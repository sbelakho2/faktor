//! faktor-anthropic — Anthropic Messages adapter (spec §12).
//!
//! The adapter owns Anthropic's wire quirks (stream events, tool_use
//! accumulation, input_json_delta); the agent only sees normalized chunks.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use faktor_core::model::ModelCapabilities;
use faktor_provider::config::{bearer_auth_header, ProviderConfigError};
#[cfg(test)]
use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_provider::egress::{execute_post_json, EgressError, HttpTransport};
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
    CanonicalUsage, ContentKind, GenericAgentRequest, Provider, ProviderChunk, ProviderError,
    ProviderErrorKind, ProviderStream, Role,
};

/// Documented Anthropic Messages per-image ceiling (raw bytes): the API
/// accepts at most ~5 MiB per image before base64 inflation. Matches the
/// daemon default; declared here so the adapter owns its bound.
pub const ANTHROPIC_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Hard bound on one classified error body: `Response::text()` would buffer
/// a hostile endpoint's unbounded body into RAM, so the classifier reads at
/// most this many bytes (chunked reads, partial body dropped) and appends a
/// typed truncation note. The status-derived classification is unaffected.
pub const ANTHROPIC_ERROR_BODY_MAX_BYTES: usize = 64 * 1024;

/// Await response HEADERS under the stream's existing first-byte deadline
/// (default 60 s), capped by the overall deadline when the operation set
/// one. The egress client only bounds connect, so without this a server
/// that accepts and never answers would pin the stream forever. `0` on both
/// knobs keeps the historical unbounded behavior, but [`stream_deadlines`]
/// always starts from the documented defaults.
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
            format!("anthropic response headers exceeded the {bound_ms} ms server bound"),
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

/// Anthropic adapter configuration. `api_key` is wrapped in
/// [`SecretValue`]; the custom [`std::fmt::Debug`] below can never print it.
#[derive(Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: Option<SecretValue>,
    pub model_caps: HashMap<String, ModelCapabilities>,
    pub default_caps: ModelCapabilities,
}

impl std::fmt::Debug for AnthropicConfig {
    /// Redacting `Debug`: the API key prints `SecretValue([redacted])`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key)
            .field("model_caps", &self.model_caps)
            .field("default_caps", &self.default_caps)
            .finish()
    }
}

impl AnthropicConfig {
    pub fn new(api_key: Option<SecretValue>) -> Self {
        Self {
            base_url: "https://api.anthropic.com".into(),
            api_key,
            model_caps: HashMap::new(),
            default_caps: ModelCapabilities {
                context: 200_000,
                max_output: 8_192,
                tools: true,
                parallel_tools: true,
                thinking: true,
                vision: true,
                json_schema: false,
                streaming: true,
                embeddings: false,
                reasoning: false,
            },
        }
    }

    pub fn with_model(mut self, model: &str, caps: ModelCapabilities) -> Self {
        self.model_caps.insert(model.to_string(), caps);
        self
    }

    pub fn with_base(mut self, base_url: &str) -> Self {
        self.base_url = base_url.to_string();
        self
    }
}

pub struct AnthropicProvider {
    config: AnthropicConfig,
    transport: Arc<dyn HttpTransport>,
}

impl AnthropicProvider {
    /// The ONLY production constructor: the egress transport is injected
    /// (the daemon passes the policy-checked one), so no production path can
    /// silently fall back to a permissive default.
    pub fn build(config: AnthropicConfig, transport: Arc<dyn HttpTransport>) -> Arc<dyn Provider> {
        Arc::new(Self { config, transport })
    }

    /// Test-only default-allow constructor (production MUST inject a
    /// policy-checked transport; in-crate tests use this for mock servers).
    #[cfg(test)]
    pub fn permissive_for_tests(config: AnthropicConfig) -> Arc<dyn Provider> {
        Self::build(config, Arc::new(PolicyCheckedHttpTransport::permissive()))
    }

    fn wire_body(&self, req: &GenericAgentRequest) -> serde_json::Value {
        let mut messages: Vec<serde_json::Value> = Vec::new();
        for m in &req.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                _ => "user",
            };
            let mut content: Vec<serde_json::Value> = Vec::new();
            for part in &m.content {
                match &part.kind {
                    ContentKind::Text { text } => {
                        content.push(serde_json::json!({ "type": "text", "text": text }));
                    }
                    ContentKind::Reasoning { text } => {
                        content.push(serde_json::json!({
                            "type": "reasoning",
                            "text": text
                        }));
                    }
                    ContentKind::Image { url } => {
                        content.push(serde_json::json!({
                            "type": "image",
                            "source": { "type": "url", "url": url }
                        }));
                    }
                    ContentKind::ImageData { mime, data } => {
                        // Resolved attachment bytes: Anthropic takes a
                        // base64 source with an explicit media_type.
                        content.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": mime,
                                "data": data.to_base64(),
                            }
                        }));
                    }
                    ContentKind::FileData {
                        mime,
                        filename: _,
                        data,
                    } => {
                        // Resolved non-image DOCUMENT bytes: Anthropic takes
                        // a `document` part. Plain text uses the documented
                        // `text` source (construction already proved UTF-8);
                        // PDF uses a byte-exact base64 source.
                        let source = match std::str::from_utf8(data.as_slice()) {
                            Ok(text) if mime == "text/plain" => serde_json::json!({
                                "type": "text",
                                "media_type": mime,
                                "data": text,
                            }),
                            _ => serde_json::json!({
                                "type": "base64",
                                "media_type": mime,
                                "data": data.to_base64(),
                            }),
                        };
                        content.push(serde_json::json!({ "type": "document", "source": source }));
                    }
                    ContentKind::ToolCall { id, name, input } => {
                        content.push(serde_json::json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                    ContentKind::ToolResult {
                        content: c,
                        is_error,
                    } => {
                        content.push(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": part.tool_call_id.as_deref().unwrap_or(""),
                            "content": c,
                            "is_error": is_error,
                        }));
                    }
                }
            }
            messages.push(serde_json::json!({ "role": role, "content": content }));
        }
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "max_tokens": req.max_output.unwrap_or(4096),
            "stream": true,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
        body
    }

    /// Request headers. An unencodable credential is a typed
    /// [`faktor_provider::config::ProviderConfigError`] — the request MUST
    /// fail, never silently go anonymous.
    fn headers(&self) -> Result<reqwest::header::HeaderMap, ProviderConfigError> {
        let mut h = reqwest::header::HeaderMap::new();
        if let Some(key) = &self.config.api_key {
            h.insert("authorization", bearer_auth_header(key)?);
            h.insert(
                "anthropic-version",
                reqwest::header::HeaderValue::from_static("faktor-plus/0.1"),
            );
        }
        Ok(h)
    }
}

impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn known_models(&self) -> Vec<String> {
        let mut out: Vec<String> = self.config.model_caps.keys().cloned().collect();
        if !out.contains(&"default".to_string()) {
            out.push("default".into());
        }
        out.sort();
        out
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.config
            .model_caps
            .get(model)
            .cloned()
            .unwrap_or_else(|| self.config.default_caps.clone())
    }

    fn max_image_bytes(&self) -> usize {
        ANTHROPIC_MAX_IMAGE_BYTES
    }

    fn document_capable(&self, _model: &str) -> bool {
        // The Messages API carries `document` parts (PDF base64 / plain
        // text) for every wired model.
        true
    }

    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        // Delivery gate BEFORE any wire decision: vision capability, image
        // mime allowlist and the provider's per-image byte bound.
        let caps = self.capabilities(&req.model);
        if let Err(e) =
            faktor_provider::validate_media_delivery(&req, &caps, self.max_image_bytes())
        {
            return faktor_provider::provider_error_stream(e);
        }
        // The vision-like document gate: family capability, document mime
        // allowlist, per-document and request-wide bounds.
        if let Err(e) = faktor_provider::validate_document_delivery(
            &req,
            self.document_capable(&req.model),
            self.max_document_bytes(),
        ) {
            return faktor_provider::provider_error_stream(e);
        }
        let body = self.wire_body(&req);
        let url = format!("{}/v1/messages", self.config.base_url);
        let transport = self.transport.clone();
        // An unencodable credential must fail the request with a typed
        // terminal error, never silently drop the Authorization header.
        let headers = match self.headers() {
            Ok(headers) => headers,
            Err(e) => {
                return faktor_provider::provider_error_stream(ProviderError::new(
                    ProviderErrorKind::Auth,
                    e.to_string(),
                ));
            }
        };
        let deadlines = stream_deadlines(&req);
        let cancel = req.meta.cancellation.clone();
        Box::pin(anthropic_stream(
            transport,
            url,
            headers,
            body,
            deadlines,
            Some(cancel),
        ))
    }
}

pub(crate) fn anthropic_stream(
    transport: Arc<dyn HttpTransport>,
    url: String,
    headers: reqwest::header::HeaderMap,
    body: serde_json::Value,
    deadlines: StreamDeadlines,
    cancel: Option<faktor_core::cancellation::CancellationToken>,
) -> impl Stream<Item = Result<ProviderChunk, ProviderError>> {
    use futures::StreamExt as _;
    type LineStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

    // Registered secret scrubber for this request: credentials the request
    // actually carries (authorization header, credential-named query
    // parameters) plus the frozen pattern policy.
    let scrubber = ErrorScrubber::new().with_request_credentials(&headers, &url);

    enum Stage {
        Fresh,
        Streaming {
            lines: LineStream,
            tool_id: Option<String>,
            tool_name: Option<String>,
            tool_args: String,
        },
        Done,
    }
    futures::stream::unfold(Stage::Fresh, move |stage| {
        let transport = transport.clone();
        let url = url.clone();
        let deadlines = deadlines;
        let cancel = cancel.clone();
        let headers = headers.clone();
        let body = body.clone();
        let scrubber = scrubber.clone();
        async move {
            let (mut lines, mut tool_id, mut tool_name, mut tool_args) = match stage {
                Stage::Fresh => {
                    let resp = execute_with_head_timeout(
                        execute_post_json(transport.as_ref(), &url, headers, &body),
                        deadlines,
                    )
                    .await;
                    match resp {
                        Ok(r) => {
                            let status = r.status();
                            if !status.is_success() {
                                let text = read_error_body_bounded(
                                    r,
                                    ANTHROPIC_ERROR_BODY_MAX_BYTES,
                                    request_head_timeout_ms(deadlines),
                                )
                                .await;
                                let kind = match status.as_u16() {
                                    401 | 403 => ProviderErrorKind::Auth,
                                    429 => ProviderErrorKind::RateLimited,
                                    408 | 504 => ProviderErrorKind::Timeout,
                                    500..=599 => ProviderErrorKind::Server,
                                    _ => ProviderErrorKind::BadRequest,
                                };
                                let code = status.as_u16();
                                return Some((
                                    Err(ProviderError::with_code(
                                        kind,
                                        code.to_string(),
                                        scrubber.diagnostic(code, &text),
                                    )),
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
                            (lines, None, None, String::new())
                        }
                        Err(e) => {
                            return Some((Err(e), Stage::Done));
                        }
                    }
                }
                Stage::Streaming {
                    lines,
                    tool_id,
                    tool_name,
                    tool_args,
                } => (lines, tool_id, tool_name, tool_args),
                Stage::Done => return None,
            };

            loop {
                let Some(next) = lines.next().await else {
                    if let (Some(id), Some(name)) = (tool_id, tool_name) {
                        let input =
                            serde_json::from_str(&tool_args).unwrap_or(serde_json::Value::Null);
                        return Some((
                            Ok(ProviderChunk::ToolCall {
                                id,
                                name,
                                input,
                                complete: true,
                            }),
                            Stage::Done,
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
                    if let (Some(id), Some(name)) = (tool_id, tool_name) {
                        let input =
                            serde_json::from_str(&tool_args).unwrap_or(serde_json::Value::Null);
                        return Some((
                            Ok(ProviderChunk::ToolCall {
                                id,
                                name,
                                input,
                                complete: true,
                            }),
                            Stage::Done,
                        ));
                    }
                    return Some((Ok(ProviderChunk::Done), Stage::Done));
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    return Some((
                        Err(ProviderError::new(
                            ProviderErrorKind::Malformed,
                            scrubber.event_diagnostic(
                                "bad anthropic SSE",
                                data,
                                auth_shaped_text(data),
                            ),
                        )),
                        Stage::Done,
                    ));
                };
                let event = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match event {
                    "content_block_start" => {
                        if let Some(block) = value.get("content_block") {
                            let block_type = block.get("type").and_then(|t| t.as_str());
                            if block_type == Some("tool_use") {
                                tool_id = block
                                    .get("id")
                                    .and_then(|i| i.as_str())
                                    .map(|s| s.to_string());
                                tool_name = block
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.to_string());
                                tool_args.clear();
                            } else if block_type == Some("text") {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    if !text.is_empty() {
                                        return Some((
                                            Ok(ProviderChunk::Text {
                                                text: text.to_string(),
                                            }),
                                            Stage::Streaming {
                                                lines,
                                                tool_id,
                                                tool_name,
                                                tool_args,
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    "content_block_delta" => {
                        let delta = value.get("delta");
                        if let Some(text) =
                            delta.and_then(|d| d.get("text")).and_then(|t| t.as_str())
                        {
                            if !text.is_empty() {
                                return Some((
                                    Ok(ProviderChunk::Text {
                                        text: text.to_string(),
                                    }),
                                    Stage::Streaming {
                                        lines,
                                        tool_id,
                                        tool_name,
                                        tool_args,
                                    },
                                ));
                            }
                        }
                        if let Some(args) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|a| a.as_str())
                        {
                            tool_args.push_str(args);
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = value.get("usage") {
                            // Wire semantics (audit Phase-1 item C):
                            // anthropic `input_tokens` is the input total
                            // EXCLUDING cache READS — cache reads arrive as
                            // `cache_read_input_tokens` — but INCLUDING the
                            // tokens written to cache this call, which also
                            // ride `cache_creation_input_tokens` (the cache
                            // WRITE line is priced separately from input).
                            // `output_tokens` includes thinking tokens;
                            // anthropic reports no separate reasoning count
                            // (no separately-priced reasoning line), so the
                            // informational subset stays zero and can never
                            // be double-billed. A row whose cache writes
                            // exceed the input total they must be a subset
                            // of is impossible: typed Malformed.
                            let input_total = usage
                                .get("input_tokens")
                                .and_then(|t| t.as_u64())
                                .unwrap_or(0);
                            let output_tokens = usage
                                .get("output_tokens")
                                .and_then(|t| t.as_u64())
                                .unwrap_or(0);
                            let cache_read_tokens = usage
                                .get("cache_read_input_tokens")
                                .and_then(|t| t.as_u64())
                                .unwrap_or(0);
                            let cache_write_tokens = usage
                                .get("cache_creation_input_tokens")
                                .and_then(|t| t.as_u64())
                                .unwrap_or(0);
                            let canonical: Option<CanonicalUsage> = if input_total == 0
                                && output_tokens == 0
                                && cache_read_tokens == 0
                                && cache_write_tokens == 0
                            {
                                // An all-zero usage envelope carries
                                // nothing: keep reading, no chunk.
                                None
                            } else {
                                let Some(uncached_input_tokens) =
                                    input_total.checked_sub(cache_write_tokens)
                                else {
                                    return Some((
                                        Err(ProviderError::new(
                                            ProviderErrorKind::Malformed,
                                            format!(
                                                "hostile usage frame: cache write tokens \
                                                 {cache_write_tokens} exceed the reported input \
                                                 total {input_total}"
                                            ),
                                        )),
                                        Stage::Done,
                                    ));
                                };
                                Some(CanonicalUsage {
                                    uncached_input_tokens,
                                    cache_read_tokens,
                                    cache_write_tokens,
                                    output_tokens,
                                    reasoning_tokens: 0,
                                    reported_cost: None,
                                    request_id: None,
                                })
                            };
                            if let Some(usage) = canonical {
                                let stage = Stage::Streaming {
                                    lines,
                                    tool_id,
                                    tool_name,
                                    tool_args,
                                };
                                return Some((Ok(ProviderChunk::Usage(usage)), stage));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::id::{OpId, SessionId};
    use faktor_provider::egress::MockHttpTransport;
    use faktor_provider::testing::{MockAction, MockServer};
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
            max_output: Some(512),
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: OpId::new(1),
                session_id: SessionId::new(1),
                provider: "anthropic".into(),
                attempt: 0,
                deadline_ms: 5000,
                cancellation: CancellationToken::new(),
            },
        }
    }

    #[tokio::test]
    async fn wire_shape_is_clean() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::AssertThenRespond {
                status: 200,
                body: "data: {\"type\":\"message_stop\"}\n\ndata: [DONE]\n\n".into(),
                assert: Arc::new(|body: &serde_json::Value| {
                    assert_eq!(body["model"], "claude-x");
                    assert_eq!(body["max_tokens"], 512);
                    assert_eq!(body["tools"][0]["name"], "read_file");
                    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
                    for leaked in [
                        "operation_id",
                        "session_id",
                        "attempt",
                        "deadline_ms",
                        "cancellation",
                        "system",
                    ] {
                        assert!(
                            !body.as_object().unwrap().contains_key(leaked),
                            "{leaked} leaked!"
                        );
                    }
                }),
            },
        );
        let base = server.base_url().await;
        let provider = AnthropicProvider::permissive_for_tests(
            AnthropicConfig::new(Some("sk".into())).with_base(&base),
        );
        let mut stream = provider.stream(req("claude-x"));
        while let Some(chunk) = stream.next().await {
            if let Ok(ProviderChunk::Done) = chunk {
                break;
            }
        }
    }

    /// Resolved images lower BYTE-EXACTLY to an Anthropic base64 source
    /// carrying the ACTUAL media type (never the old url/hardcoded shape).
    #[tokio::test]
    async fn image_data_lowers_to_byte_exact_base64_source() {
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 5, 6];
        let expected_b64 = faktor_provider::MediaBytes::new(png.clone())
            .unwrap()
            .to_base64();
        let server = MockServer::new();
        let expected = expected_b64.clone();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::AssertThenRespond {
                status: 200,
                body: "data: {\"type\":\"message_stop\"}\n\ndata: [DONE]\n\n".into(),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["messages"][1]["content"],
                        serde_json::json!([
                            { "type": "text", "text": "look" },
                            {
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": "image/png",
                                    "data": expected
                                }
                            }
                        ]),
                        "Anthropic image lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        assert_eq!(provider.max_image_bytes(), ANTHROPIC_MAX_IMAGE_BYTES);
        let mut r = req("claude-x");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("look"),
                ContentPart::image_data("image/png", png).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk, Ok(ProviderChunk::Done)) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);
    }

    /// Resolved DOCUMENT attachments lower BYTE-EXACTLY: PDF via a base64
    /// `document` source, plain text via the documented `text` source —
    /// never a fabricated extraction and never a lossy re-encode.
    #[tokio::test]
    async fn document_data_lowers_to_byte_exact_document_parts() {
        let pdf: Vec<u8> = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
        let expected_b64 = faktor_provider::MediaBytes::new(pdf.clone())
            .unwrap()
            .to_base64();
        let server = MockServer::new();
        let expected = expected_b64.clone();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::AssertThenRespond {
                status: 200,
                body: "data: {\"type\":\"message_stop\"}\n\ndata: [DONE]\n\n".into(),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["messages"][1]["content"],
                        serde_json::json!([
                            { "type": "text", "text": "read" },
                            {
                                "type": "document",
                                "source": {
                                    "type": "base64",
                                    "media_type": "application/pdf",
                                    "data": expected
                                }
                            },
                            {
                                "type": "document",
                                "source": {
                                    "type": "text",
                                    "media_type": "text/plain",
                                    "data": "plain notes\n"
                                }
                            }
                        ]),
                        "Anthropic document lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        assert!(provider.document_capable("claude-x"));
        let mut r = req("claude-x");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("read"),
                ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf).unwrap(),
                ContentPart::file_data("text/plain", Some("notes.txt"), b"plain notes\n".to_vec())
                    .unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk, Ok(ProviderChunk::Done)) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);
    }

    /// Vision-less and over-bound media are refused typedly BEFORE any wire
    /// byte (the Anthropic per-image bound is 5 MiB).
    #[tokio::test]
    async fn image_delivery_gate_is_typed_and_pre_wire() {
        let server = MockServer::new();
        let base = server.base_url().await;
        let visionless = AnthropicConfig::new(None).with_base(&base).with_model(
            "claude-x",
            ModelCapabilities {
                vision: false,
                ..Default::default()
            },
        );
        let provider = AnthropicProvider::permissive_for_tests(visionless);
        let mut r = req("claude-x");
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

        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut r = req("claude-x");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart {
                kind: ContentKind::ImageData {
                    mime: "image/png".into(),
                    data: faktor_provider::MediaBytes::new(vec![
                        0u8;
                        ANTHROPIC_MAX_IMAGE_BYTES + 1
                    ])
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

    #[tokio::test]
    async fn text_and_tool_use_stream() {
        let server = MockServer::new();
        let body = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hello"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
            r#"data: {"type":"message_stop"}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        server.route(
            "POST",
            "/v1/messages",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("claude-x"));
        let mut text = String::new();
        let mut call = None;
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::ToolCall {
                    id, name, input, ..
                } => {
                    call = Some((id, name, input));
                }
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "hello world");
        let (id, name, input) = call.expect("tool call");
        assert_eq!(id, "toolu_1");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "a.rs");
    }

    #[tokio::test]
    async fn usage_message_delta_maps_cache_detail_canonically() {
        // Audit Phase-1 item C: message_delta usage reports `input_tokens`
        // EXCLUDING cache reads but INCLUDING the tokens written to cache
        // (`cache_creation_input_tokens` ride both counters — the write
        // line is priced separately). The canonical frame must therefore be
        // uncached = input − creation, with reads and writes as additive
        // lines — never double-billed.
        let server = MockServer::new();
        let body = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hi"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":1600,"output_tokens":50,"cache_creation_input_tokens":1500,"cache_read_input_tokens":900}}"#,
            r#"data: {"type":"message_stop"}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        server.route(
            "POST",
            "/v1/messages",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("claude-x"));
        let mut usage = None;
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Usage(usage_chunk) => usage = Some(usage_chunk),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        let usage = usage.expect("usage frame");
        assert_eq!(
            usage,
            CanonicalUsage {
                uncached_input_tokens: 100,
                cache_read_tokens: 900,
                cache_write_tokens: 1500,
                output_tokens: 50,
                reasoning_tokens: 0,
                reported_cost: None,
                request_id: None,
            }
        );
    }

    #[tokio::test]
    async fn hostile_cache_write_over_input_fails_malformed() {
        // A message_delta row whose cache creation tokens EXCEED the input
        // total they must be a subset of is impossible: the stream must
        // fail typed Malformed, never silently settle a lie.
        let server = MockServer::new();
        let body = [
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":1500}}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        server.route(
            "POST",
            "/v1/messages",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("claude-x"));
        let mut items = Vec::new();
        while let Some(item) = stream.next().await {
            items.push(item);
        }
        let errs: Vec<_> = items.iter().filter_map(|i| i.as_ref().err()).collect();
        assert_eq!(errs.len(), 1, "{items:?}");
        assert_eq!(errs[0].kind, ProviderErrorKind::Malformed);
        assert!(!errs[0].retryable);
    }

    #[tokio::test]
    async fn request_meta_deadline_bounds_silent_stream() {
        // Audit round 15: `RequestMeta::deadline_ms` is the operation
        // deadline. A silent server with meta.deadline_ms = 1200 must error
        // Timeout at the overall bound (~1.2s, well inside 2.5s) instead of
        // waiting out the 60s first-byte default.
        let server = MockServer::new();
        server.route("POST", "/v1/messages", MockAction::Silent { status: 200 });
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut g = req("claude-x");
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
    async fn rate_limit_mapped() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::Respond {
                status: 429,
                body: r#"{"error":{"message":"slow down"}}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("claude-x"));
        let err = stream.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
    }

    #[tokio::test]
    async fn malformed_sse_is_loud() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::Respond {
                status: 200,
                body: "data: {garbage\n\ndata: [DONE]\n\n".into(),
            },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("claude-x"));
        let first = stream.next().await.unwrap();
        assert!(first.is_err());
    }

    #[test]
    fn caps_default_and_override() {
        let p = AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None));
        assert!(p.capabilities("any").tools);
        assert_eq!(p.capabilities("any").context, 200_000);
    }

    #[tokio::test]
    async fn tool_use_and_tool_result_ride_the_anthropic_wire() {
        // The exact request shape the agent reconstructs after a tool runs:
        // assistant tool_use then user tool_result bound via tool_use_id.
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::AssertThenRespond {
                status: 200,
                body: "data: {\"type\":\"message_stop\"}\n\ndata: [DONE]\n\n".into(),
                assert: Arc::new(|body: &serde_json::Value| {
                    let msgs = body["messages"].as_array().expect("messages array");
                    assert_eq!(msgs.len(), 2);
                    assert_eq!(msgs[0]["role"], "assistant");
                    assert_eq!(msgs[0]["content"][0]["type"], "tool_use");
                    assert_eq!(msgs[0]["content"][0]["id"], "call_1");
                    assert_eq!(msgs[0]["content"][0]["name"], "echo");
                    assert_eq!(
                        msgs[0]["content"][0]["input"],
                        serde_json::json!({"x": 1}),
                        "the call input must ride the tool_use block"
                    );
                    assert_eq!(msgs[1]["role"], "user");
                    assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
                    assert_eq!(
                        msgs[1]["content"][0]["tool_use_id"], "call_1",
                        "the tool_result must name the tool_use it answers"
                    );
                    assert_eq!(
                        msgs[1]["content"][0]["content"], "echo: {\"x\":1}",
                        "the tool output must be on the wire verbatim"
                    );
                    assert_eq!(msgs[1]["content"][0]["is_error"], false);
                }),
            },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut r = req("claude-x");
        r.messages = vec![
            RequestMessage {
                role: Role::Assistant,
                content: vec![ContentPart::tool_call(
                    "call_1",
                    "echo",
                    serde_json::json!({"x": 1}),
                )],
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
    async fn sse_frame_split_across_http_chunks_assembles() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![
                    br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"#.to_vec(),
                    br#"lo"}}"#.to_vec(),
                    b"\n\n".to_vec(),
                    b"data: [DONE]\n\n".to_vec(),
                ],
            },
        );
        let base = server.base_url().await;
        let provider =
            AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("m"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(ProviderChunk::Text { text: t }) => text.push_str(&t),
                Ok(ProviderChunk::Done) => break,
                Ok(_) => {}
                Err(e) => panic!("fragmented SSE must assemble, got {e:?}"),
            }
        }
        assert_eq!(text, "hello");
    }

    // ------------------------------------------------------- egress (P0-36)

    fn allow_only(port: u16) -> Arc<dyn HttpTransport> {
        Arc::new(PolicyCheckedHttpTransport::with_policy_for_tests(
            DestinationPolicy::parse_lines([&format!("http://127.0.0.1:{port}")]).unwrap(),
        ))
    }

    async fn first_error(mut stream: ProviderStream) -> ProviderError {
        stream
            .next()
            .await
            .expect("an item")
            .expect_err("the first item must be the deny error")
    }

    #[tokio::test]
    async fn egress_allowlist_gates_the_streaming_path_before_connect() {
        let server = MockServer::new();
        let body = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"allowed"}}"#,
            r#"data: {"type":"message_stop"}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        server.route(
            "POST",
            "/v1/messages",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();

        // Allowed: the allowlisted mock streams normally through the
        // injected transport.
        let provider = AnthropicProvider::build(
            AnthropicConfig::new(None).with_base(&base),
            allow_only(port),
        );
        let mut stream = provider.stream(req("claude-x"));
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

        // Denied: a second instance whose policy allows a different port
        // fails BEFORE any network byte (the mock counter stays put).
        let denied = AnthropicProvider::build(
            AnthropicConfig::new(None).with_base(&base),
            allow_only(port.wrapping_add(1)),
        );
        let err = first_error(denied.stream(req("claude-x"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert!(!err.retryable, "denied destinations are never retried");
        assert_eq!(server.request_count(), 1, "deny happened before connect");

        // https-to-http mismatch against an https-only rule: denied before
        // connect as well.
        let mismatch = AnthropicProvider::build(
            AnthropicConfig::new(None).with_base(&base),
            Arc::new(PolicyCheckedHttpTransport::with_policy_for_tests(
                DestinationPolicy::parse_lines([&format!("https://127.0.0.1:{port}")]).unwrap(),
            )),
        );
        let err = first_error(mismatch.stream(req("claude-x"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert_eq!(server.request_count(), 1, "scheme mismatch: no connect");
    }

    #[tokio::test]
    async fn mock_transport_canned_sse_drives_the_parser_without_http() {
        // No server exists: the canned SSE body drives the anthropic parser
        // through the injected transport (unit tests no longer need HTTP).
        let body = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"can"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ned"}}"#,
            r#"data: {"type":"message_stop"}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        let mock = Arc::new(MockHttpTransport::new(200, body));
        let as_transport: Arc<dyn HttpTransport> = mock.clone();
        let provider = AnthropicProvider::build(
            AnthropicConfig::new(None).with_base("http://mock.invalid"),
            as_transport,
        );
        let mut stream = provider.stream(req("claude-x"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "canned");
        assert_eq!(mock.request_count(), 1);
        assert_eq!(
            mock.requests(),
            vec![(
                "POST".to_string(),
                "http://mock.invalid/v1/messages".to_string()
            )]
        );
    }

    // ------------------------------------------------- canonical usage

    /// Shared canonical-usage conformance for the Anthropic Messages wire
    /// (audit Phase-1 item C): mock message_delta frames shaped exactly
    /// like real usage events drive the REAL provider.
    mod canonical_usage_conformance {
        use super::*;
        use faktor_provider::canonical_usage_conformance;
        use faktor_provider::CanonicalUsage;

        /// One real-wire message_delta usage event body. `junk` adds
        /// unknown fields at every level (unknown fields never panic).
        fn delta_frame(
            input: u64,
            output: u64,
            cache_read: u64,
            cache_creation: u64,
            junk: bool,
        ) -> String {
            let mut usage = serde_json::json!({
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": cache_read,
                "cache_creation_input_tokens": cache_creation,
            });
            if junk {
                usage["server_tool_use"] = serde_json::json!({"tool_use_ids": []});
                usage["unknown_usage_field"] = serde_json::json!({"n": [1, 2]});
                usage["input_tokens_details"] = serde_json::json!({"extra": true});
            }
            let mut frame = serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": usage,
            });
            if junk {
                frame["unknown_top"] = serde_json::json!("x");
            }
            format!("data: {frame}\n\ndata: [DONE]\n\n")
        }

        fn exp(uncached: u64, cache_read: u64, cache_write: u64, output: u64) -> CanonicalUsage {
            CanonicalUsage {
                uncached_input_tokens: uncached,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
                output_tokens: output,
                reasoning_tokens: 0,
                reported_cost: None,
                request_id: None,
            }
        }

        canonical_usage_conformance! {
            driver: messages_canonical_usage_conformance,
            family: faktor_provider::usage_conformance::WireFamily::SplitCache,
            label: "anthropic messages",
            request: || req("claude-x"),
            provider: |base: String| AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base)),
            method: "POST",
            path: "/v1/messages",
            cases: vec![
                // A wire already reporting input EXCLUDING cache reads
                // (400 uncached + 600 cache reads) maps identically — the
                // audit's pre-split identity case.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "split_input_cache_identity",
                    delta_frame(400, 50, 600, 0, false),
                    exp(400, 600, 0, 50),
                ),
                // input_tokens INCLUDES the tokens written to cache this
                // call; they also ride cache_creation_input_tokens and are
                // priced at the write line — uncached = input - creation,
                // never billed twice.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "cache_write_inside_input_total_split",
                    delta_frame(1600, 50, 0, 1500, false),
                    exp(100, 0, 1500, 50),
                ),
                // Hostile: cache creation tokens exceeding the input total
                // they must be a subset of -> typed Malformed.
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_cache_write_over_input",
                    delta_frame(100, 50, 0, 1500, false),
                ),
                // No cache detail on the wire: conservative category —
                // uncached = the reported input total.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "cache_detail_missing_uncached_total",
                    delta_frame(1000, 50, 0, 0, false),
                    exp(1000, 0, 0, 50),
                ),
                // A full-cache-hit row (zero fresh input, reads only) is
                // legal on this wire and must still emit a usage frame.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "cache_only_frame_no_input",
                    delta_frame(0, 0, 600, 0, false),
                    exp(0, 600, 0, 0),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "unknown_fields_never_panic",
                    delta_frame(1000, 50, 0, 0, true),
                    exp(1000, 0, 0, 50),
                ),
            ]
        }
    }

    /// A raw HTTP/1.1 server that reads the request head, writes `head`
    /// (empty = never answer headers), streams `body_prefix` bytes, then
    /// stalls with the connection open — proving reads are wall-clock and
    /// byte bounded instead of hanging or buffering.
    async fn stalling_http_server(head: &'static str, body_prefix: usize) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            if !head.is_empty() {
                let _ = socket.write_all(head.as_bytes()).await;
            }
            if body_prefix > 0 {
                let chunk = vec![b'x'; body_prefix];
                let _ = socket.write_all(&chunk).await;
                let _ = socket.flush().await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        format!("http://{addr}")
    }

    fn error_path_provider(base: String) -> Arc<dyn Provider> {
        AnthropicProvider::permissive_for_tests(AnthropicConfig::new(None).with_base(&base))
    }

    #[tokio::test]
    async fn oversize_error_body_is_capped_and_the_status_still_classifies() {
        let head = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 1048576\r\n\r\n";
        let base = stalling_http_server(head, 256 * 1024).await;
        let mut stream = error_path_provider(base).stream(req("m"));
        let started = std::time::Instant::now();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("a bounded error-body read must not hang")
            .expect("exactly one error item");
        let err = item.expect_err("a 500 must surface as an error");
        assert_eq!(err.kind, ProviderErrorKind::Server, "{err:?}");
        assert!(err.message.contains("truncated"), "{}", err.message);
        assert!(
            err.message.len() <= ANTHROPIC_ERROR_BODY_MAX_BYTES + 128,
            "error body kept {} bytes past the cap",
            err.message.len()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
    }

    #[tokio::test]
    async fn stalled_error_body_is_bounded_by_the_wall_clock_read_bound() {
        let head = "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 999\r\n\r\n";
        let base = stalling_http_server(head, 0).await;
        let mut request = req("m");
        request.meta.deadline_ms = 250;
        let mut stream = error_path_provider(base).stream(request);
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .expect("a stalled error body must not hang")
            .expect("one item")
            .expect_err("429 must surface");
        assert_eq!(err.kind, ProviderErrorKind::RateLimited, "{err:?}");
        assert!(err.message.contains("exceeded"), "{}", err.message);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_server_that_never_answers_headers_is_a_typed_timeout() {
        let base = stalling_http_server("", 0).await;
        let mut request = req("m");
        request.meta.deadline_ms = 250;
        let mut stream = error_path_provider(base).stream(request);
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .expect("a header stall must not hang")
            .expect("one item")
            .expect_err("no response headers is an error");
        assert_eq!(err.kind, ProviderErrorKind::Timeout, "{err:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    /// P0 plaintext-secret lock: the planted key never renders and an
    /// unencodable key fails the header build AND the request typed —
    /// never a silently anonymous Messages call.
    #[tokio::test]
    async fn config_redacts_key_and_invalid_key_fails_typed() {
        const PLANTED: &str = "sk-ant-PLANTED-key-0123456789abcdef";
        let cfg = AnthropicConfig::new(Some(SecretValue::new(PLANTED)));
        let mut rendered = vec![format!("{cfg:?}")];
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("anthropic config: {cfg:?}")
        }))
        .expect_err("must panic");
        if let Some(message) = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        {
            rendered.push(message);
        }
        rendered.push(serde_json::to_string(&format!("{cfg:?}")).unwrap());
        for text in &rendered {
            assert!(!text.contains(PLANTED), "anthropic key leaked: {text}");
        }
        assert!(rendered[0].contains("[redacted]"));

        // `headers()` refuses the unencodable credential typed.
        let bad = AnthropicProvider {
            config: AnthropicConfig::new(Some(SecretValue::new("sk-bad\r\nX-Injected: yes"))),
            transport: Arc::new(PolicyCheckedHttpTransport::permissive()),
        };
        let err = bad.headers().unwrap_err();
        for text in [format!("{err}"), format!("{err:?}")] {
            assert!(!text.contains("Injected"), "credential leaked: {text}");
        }
        // The request path yields the typed Auth error, not an anonymous
        // request.
        let provider = AnthropicProvider::build(
            bad.config.clone(),
            Arc::new(PolicyCheckedHttpTransport::permissive()),
        );
        let mut stream = provider.stream(req("m"));
        let err = stream
            .next()
            .await
            .expect("one item")
            .expect_err("invalid key must fail typed");
        assert_eq!(err.kind, ProviderErrorKind::Auth, "{err:?}");
    }

    /// Adversarial: a provider/gateway error body that echoes request
    /// credentials must never reach the error in raw form — exact
    /// registered credentials and pattern-shaped secrets are scrubbed,
    /// bounded diagnostics survive, and 401/403 bodies are withheld
    /// entirely.
    #[tokio::test]
    async fn error_bodies_are_scrubbed_and_auth_bodies_withheld() {
        const EXACT: &str = "exact-credential-value-9f2a";
        const PATTERN: &str = "sk-abcdefghijklmnopqrstuvwx";
        fn body(sentinel: &str) -> String {
            format!(
                r#"{{"error":{{"type":"overloaded_error","message":"{sentinel} {EXACT} {PATTERN}"}}}}"#
            )
        }
        fn transport() -> Arc<dyn HttpTransport> {
            Arc::new(PolicyCheckedHttpTransport::permissive())
        }
        fn headers() -> reqwest::header::HeaderMap {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("authorization", format!("Bearer {EXACT}").parse().unwrap());
            h
        }

        // Non-auth: scrubbed bounded diagnostic, status preserved.
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::Respond {
                status: 429,
                body: body("RATE-BODY-SENTINEL"),
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(anthropic_stream(
            transport(),
            format!("{base}/v1/messages"),
            headers(),
            serde_json::json!({"model": "m"}),
            StreamDeadlines::default(),
            None,
        ));
        let err = stream
            .next()
            .await
            .expect("one item")
            .expect_err("429 must fail");
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
        assert_eq!(err.code.as_deref(), Some("429"));
        let serialized = serde_json::to_string(&err.message).expect("serialize");
        let rendered = format!("{err}|{err:?}|{serialized}");
        for secret in [EXACT, PATTERN] {
            assert!(!rendered.contains(secret), "secret leaked: {rendered}");
        }
        assert!(err.message.contains("HTTP 429"), "{}", err.message);
        assert!(
            err.message.len() <= faktor_provider::sanitize::MAX_ERROR_DIAGNOSTIC_BYTES + 128,
            "diagnostic must stay bounded: {}",
            err.message.len()
        );

        // Auth: the arbitrary upstream body is not preserved at all.
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1/messages",
            MockAction::Respond {
                status: 401,
                body: body("AUTH-BODY-SENTINEL"),
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(anthropic_stream(
            transport(),
            format!("{base}/v1/messages"),
            headers(),
            serde_json::json!({"model": "m"}),
            StreamDeadlines::default(),
            None,
        ));
        let err = stream
            .next()
            .await
            .expect("one item")
            .expect_err("401 must fail");
        assert_eq!(err.kind, ProviderErrorKind::Auth);
        let rendered = format!(
            "{err}|{err:?}|{}",
            serde_json::to_string(&err.message).unwrap()
        );
        for leaked in ["AUTH-BODY-SENTINEL", EXACT, PATTERN] {
            assert!(!rendered.contains(leaked), "auth body leaked: {rendered}");
        }
        assert!(err.message.contains("withheld"), "{}", err.message);
    }

    /// Adversarial: an in-stream error payload under an HTTP 2xx (a data
    /// line the SSE parser cannot parse) is hostile text too. The planted
    /// exact credential (registered from the request's Authorization
    /// header) and the pattern secret never reach `message`, `Display`,
    /// `Debug` or the JSON-serialized message; an auth-shaped payload is
    /// withheld entirely; the diagnostic stays bounded.
    #[tokio::test]
    async fn in_stream_2xx_error_payloads_are_scrubbed_or_withheld() {
        const EXACT: &str = "exact-credential-value-9f2a";
        const PATTERN: &str = "sk-abcdefghijklmnopqrstuvwx";
        fn transport() -> Arc<dyn HttpTransport> {
            Arc::new(PolicyCheckedHttpTransport::permissive())
        }
        fn headers() -> reqwest::header::HeaderMap {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("authorization", format!("Bearer {EXACT}").parse().unwrap());
            h
        }
        fn assert_no_secret(err: &ProviderError, sentinels: &[&str]) {
            let rendered = format!(
                "{err}|{err:?}|{}|{:?}",
                serde_json::to_string(&err.message).expect("serialize message"),
                serde_json::to_string(&err.code).expect("serialize code"),
            );
            for secret in [EXACT, PATTERN].iter().chain(sentinels) {
                assert!(!rendered.contains(secret), "secret leaked: {rendered}");
            }
        }
        async fn stream_err(events: Vec<String>) -> ProviderError {
            let server = MockServer::new();
            server.route(
                "POST",
                "/v1/messages",
                MockAction::Sse {
                    status: 200,
                    events,
                },
            );
            let base = server.base_url().await;
            let mut stream = Box::pin(anthropic_stream(
                transport(),
                format!("{base}/v1/messages"),
                headers(),
                serde_json::json!({"model": "m"}),
                StreamDeadlines::default(),
                None,
            ));
            stream
                .next()
                .await
                .expect("one item")
                .expect_err("malformed 2xx payload must fail the stream")
        }

        // Non-auth: scrubbed and bounded, plain non-secret text visible.
        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "overloaded_error",
                "message": format!("ANTH-SENTINEL {EXACT} {PATTERN}"),
            },
        })
        .to_string();
        let err = stream_err(vec![format!("data: {body} trailing-garbage\n\n")]).await;
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(err.message.contains("ANTH-SENTINEL"), "{}", err.message);
        assert!(
            err.message.len() <= faktor_provider::sanitize::MAX_ERROR_DIAGNOSTIC_BYTES + 128,
            "diagnostic must stay bounded: {}",
            err.message.len()
        );
        assert_no_secret(&err, &[]);

        // Auth-shaped: the upstream payload is withheld entirely.
        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": format!("AUTH-ANTH-SENTINEL {EXACT} {PATTERN}"),
            },
        })
        .to_string();
        let err = stream_err(vec![format!("data: {body} trailing-garbage\n\n")]).await;
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(err.message.contains("withheld"), "{}", err.message);
        assert_no_secret(&err, &["AUTH-ANTH-SENTINEL"]);
    }
}
