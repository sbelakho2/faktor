//! faktor-google — Gemini streaming adapter (spec §12). Adapter owns Gemini's
//! wire quirks (candidates → parts → functionCall); agent sees normalized
//! chunks only.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use faktor_core::model::ModelCapabilities;
#[cfg(test)]
use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_provider::egress::{execute_post_json, EgressError, HttpTransport};
use faktor_provider::egress::{BudgetComponent, BudgetedBody, ResponseBudget};
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

/// Documented Gemini inline-data ceiling for one image part (raw bytes;
/// base64 inflation stays inside the 20 MB request budget). Kept equal to
/// OpenAI's documented per-image bound.
pub const GOOGLE_MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Hard bound on one classified error body: `Response::text()` would buffer
/// a hostile endpoint's unbounded body into RAM, so the classifier reads at
/// most this many bytes (chunked reads, partial body dropped) and appends a
/// typed truncation note. The status-derived classification is unaffected.
pub const GOOGLE_ERROR_BODY_MAX_BYTES: usize = 64 * 1024;

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
    fut: impl std::future::Future<Output = Result<reqwest::Response, EgressError>>,
    deadlines: StreamDeadlines,
) -> Result<reqwest::Response, ProviderError> {
    let bound_ms = request_head_timeout_ms(deadlines);
    if bound_ms == 0 {
        return fut.await.map_err(ProviderError::from);
    }
    match tokio::time::timeout(std::time::Duration::from_millis(bound_ms), fut).await {
        Ok(result) => result.map_err(ProviderError::from),
        Err(_) => Err(ProviderError::new(
            ProviderErrorKind::Timeout,
            format!("google response headers exceeded the {bound_ms} ms server bound"),
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
async fn read_error_body_bounded(resp: reqwest::Response, cap: usize, bound_ms: u64) -> String {
    let budget_ms = if bound_ms == 0 {
        PROVIDER_CEILING_MS
    } else {
        bound_ms
    };
    let budget = ResponseBudget::from_millis(budget_ms, budget_ms, budget_ms, cap as u64, None);
    let read = async {
        let mut body = BudgetedBody::new(resp, budget);
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

/// Gemini adapter configuration. `api_key` is wrapped in [`SecretValue`];
/// the custom [`std::fmt::Debug`] below can never print it.
#[derive(Clone)]
pub struct GoogleConfig {
    pub base_url: String,
    pub api_key: Option<SecretValue>,
    pub model_caps: HashMap<String, ModelCapabilities>,
}

impl std::fmt::Debug for GoogleConfig {
    /// Redacting `Debug`: the API key prints `SecretValue([redacted])`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key)
            .field("model_caps", &self.model_caps)
            .finish()
    }
}

impl GoogleConfig {
    pub fn new(api_key: Option<SecretValue>) -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com".into(),
            api_key,
            model_caps: HashMap::new(),
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

pub struct GoogleProvider {
    config: GoogleConfig,
    transport: Arc<dyn HttpTransport>,
}

impl GoogleProvider {
    /// The ONLY production constructor: the egress transport is injected
    /// (the daemon passes the policy-checked one), so no production path can
    /// silently fall back to a permissive default.
    pub fn build(config: GoogleConfig, transport: Arc<dyn HttpTransport>) -> Arc<dyn Provider> {
        Arc::new(Self { config, transport })
    }

    /// Test-only default-allow constructor (production MUST inject a
    /// policy-checked transport; in-crate tests use this for mock servers).
    #[cfg(test)]
    pub fn permissive_for_tests(config: GoogleConfig) -> Arc<dyn Provider> {
        Self::build(config, Arc::new(PolicyCheckedHttpTransport::permissive()))
    }

    fn wire_body(&self, req: &GenericAgentRequest) -> serde_json::Value {
        let mut contents: Vec<serde_json::Value> = Vec::new();
        for m in &req.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "model",
                Role::System => "user",
            };
            let mut parts: Vec<serde_json::Value> = Vec::new();
            for part in &m.content {
                match &part.kind {
                    ContentKind::Text { text } => {
                        parts.push(serde_json::json!({ "text": text }));
                    }
                    ContentKind::Reasoning { text } => {
                        parts.push(serde_json::json!({ "text": text }));
                    }
                    ContentKind::Image { url } => {
                        parts.push(serde_json::json!({ "inline_data": { "mime_type": "image/png", "data": url } }));
                    }
                    ContentKind::ImageData { mime, data } => {
                        // Resolved attachment bytes: Gemini inline_data is
                        // raw base64 + the ACTUAL media type (never the old
                        // hardcoded image/png).
                        parts.push(serde_json::json!({
                            "inline_data": {
                                "mime_type": mime,
                                "data": data.to_base64(),
                            }
                        }));
                    }
                    ContentKind::FileData {
                        mime,
                        filename: _,
                        data,
                    } => {
                        // Resolved non-image DOCUMENT bytes: Gemini reads
                        // PDFs (and plain text) through the same byte-exact
                        // inline_data envelope.
                        parts.push(serde_json::json!({
                            "inline_data": {
                                "mime_type": mime,
                                "data": data.to_base64(),
                            }
                        }));
                    }
                    ContentKind::ToolCall { id, name, input } => {
                        parts.push(serde_json::json!({
                            "functionCall": { "name": name, "args": input, "id": id }
                        }));
                    }
                    ContentKind::ToolResult {
                        content: c,
                        is_error,
                    } => {
                        parts.push(serde_json::json!({
                            "functionResponse": {
                                "name": part.tool_call_id.as_deref().unwrap_or("fn"),
                                "response": { "result": c, "is_error": is_error },
                            }
                        }));
                    }
                }
            }
            contents.push(serde_json::json!({ "role": role, "parts": parts }));
        }
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "functionDeclarations": [{
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }]
                })
            })
            .collect();
        let mut body = serde_json::json!({
            "contents": contents,
            "generationConfig": { "temperature": 0.7 },
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
        if let Some(max_out) = req.max_output {
            body["generationConfig"]["maxOutputTokens"] = serde_json::json!(max_out);
        }
        body
    }
}

impl Provider for GoogleProvider {
    fn id(&self) -> &str {
        "google"
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
        if let Some(caps) = self.config.model_caps.get(model) {
            return caps.clone();
        }
        ModelCapabilities {
            context: 1_000_000,
            max_output: 8_192,
            tools: true,
            parallel_tools: false,
            thinking: false,
            vision: true,
            json_schema: false,
            streaming: true,
            embeddings: false,
            reasoning: false,
        }
    }

    fn max_image_bytes(&self) -> usize {
        GOOGLE_MAX_IMAGE_BYTES
    }

    fn document_capable(&self, _model: &str) -> bool {
        // Gemini carries documents through byte-exact `inline_data` parts.
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
        // The explicit accessor is the only way the key reaches the URL
        // (Gemini authenticates by query parameter, not header); a missing
        // key stays the keyless request it always was.
        let key = self
            .config
            .api_key
            .as_ref()
            .map(SecretValue::expose)
            .unwrap_or("");
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            self.config.base_url, req.model, key
        );
        let transport = self.transport.clone();
        let deadlines = stream_deadlines(&req);
        let cancel = req.meta.cancellation.clone();
        Box::pin(google_stream(transport, url, body, deadlines, Some(cancel)))
    }
}

pub(crate) fn google_stream(
    transport: Arc<dyn HttpTransport>,
    url: String,
    body: serde_json::Value,
    deadlines: StreamDeadlines,
    cancel: Option<faktor_core::cancellation::CancellationToken>,
) -> impl Stream<Item = Result<ProviderChunk, ProviderError>> {
    use futures::StreamExt as _;
    type LineStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

    // Registered secret scrubber for this request: Gemini authenticates by
    // query parameter, so the `key=` value is registered from the URL (plus
    // the frozen pattern policy).
    let scrubber =
        ErrorScrubber::new().with_request_credentials(&reqwest::header::HeaderMap::new(), &url);

    enum Stage {
        Fresh,
        Streaming { lines: LineStream },
        Done,
    }
    futures::stream::unfold(Stage::Fresh, move |stage| {
        let transport = transport.clone();
        let url = url.clone();
        let deadlines = deadlines;
        let cancel = cancel.clone();
        let body = body.clone();
        let scrubber = scrubber.clone();
        async move {
            let mut lines = match stage {
                Stage::Fresh => {
                    let resp = execute_with_head_timeout(
                        execute_post_json(
                            transport.as_ref(),
                            &url,
                            reqwest::header::HeaderMap::new(),
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
                                    GOOGLE_ERROR_BODY_MAX_BYTES,
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
                                    BudgetedBody::new(r, deadlines.response_budget()).into_stream(),
                                    MAX_LINE_BYTES,
                                ),
                                deadlines,
                                cancel.clone(),
                            ));
                            lines
                        }
                        Err(e) => {
                            return Some((Err(e), Stage::Done));
                        }
                    }
                }
                Stage::Streaming { lines } => lines,
                Stage::Done => return None,
            };

            loop {
                let Some(next) = lines.next().await else {
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
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    return Some((Ok(ProviderChunk::Done), Stage::Done));
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    return Some((
                        Err(ProviderError::new(
                            ProviderErrorKind::Malformed,
                            scrubber.event_diagnostic(
                                "bad gemini SSE",
                                data,
                                auth_shaped_text(data),
                            ),
                        )),
                        Stage::Done,
                    ));
                };
                match parse_gemini_chunk(&value) {
                    Ok(Some(chunk)) => {
                        return Some((Ok(chunk), Stage::Streaming { lines }));
                    }
                    Ok(None) => {}
                    // A hostile usage row (cache > prompt total, thoughts >
                    // candidate total) is a typed Malformed error — never a
                    // silent zero.
                    Err(e) => return Some((Err(e), Stage::Done)),
                }
            }
        }
    })
}

fn parse_gemini_chunk(value: &serde_json::Value) -> Result<Option<ProviderChunk>, ProviderError> {
    let candidates = value.get("candidates").and_then(|c| c.as_array());
    if let Some(content) = candidates
        .and_then(|c| c.first())
        .and_then(|f| f.get("content"))
    {
        if let Some(parts) = content.get("parts").and_then(|p| p.as_array()) {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        return Ok(Some(ProviderChunk::Text {
                            text: text.to_string(),
                        }));
                    }
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let args = fc.get("args").cloned().unwrap_or(serde_json::Value::Null);
                    let id = fc
                        .get("id")
                        .and_then(|i| i.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("gemini_call_{}", name));
                    if !name.is_empty() {
                        return Ok(Some(ProviderChunk::ToolCall {
                            id,
                            name,
                            input: args,
                            complete: true,
                        }));
                    }
                }
            }
        }
    }
    if let Some(usage) = value.get("usageMetadata") {
        // Wire semantics (audit Phase-1 item C): gemini's
        // `promptTokenCount` is the TOTAL input INCLUDING the cached
        // portion; `cachedContentTokenCount` splits the cache reads out for
        // cost attribution. `candidatesTokenCount` is the total output
        // INCLUDING thinking tokens; `thoughtTokens` reports the same
        // tokens as an informational subset — never billed a second time.
        // A row whose cache exceeds the prompt total (or whose thoughts
        // exceed the candidates) is impossible: typed Malformed. Gemini's
        // usage frames carry no provider request id.
        let total_input_tokens = usage
            .get("promptTokenCount")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let total_output_tokens = usage
            .get("candidatesTokenCount")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let cache_read_tokens = usage
            .get("cachedContentTokenCount")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let reasoning_tokens = usage
            .get("thoughtTokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        if total_input_tokens == 0
            && total_output_tokens == 0
            && cache_read_tokens == 0
            && reasoning_tokens == 0
        {
            return Ok(None);
        }
        let canonical = CanonicalUsage::from_total_including_cache(
            total_input_tokens,
            cache_read_tokens,
            0,
            total_output_tokens,
            reasoning_tokens,
        )
        .map_err(ProviderError::from)?;
        return Ok(Some(ProviderChunk::Usage(canonical)));
    }
    Ok(None)
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
            max_output: Some(1024),
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: OpId::new(1),
                session_id: SessionId::new(1),
                provider: "google".into(),
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
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::AssertThenRespond {
                status: 200,
                body: "data: {}\n\n".into(),
                assert: Arc::new(|body: &serde_json::Value| {
                    assert_eq!(body["contents"][0]["role"], "user");
                    assert!(body["tools"].is_array());
                    assert_eq!(
                        body["tools"][0]["functionDeclarations"][0]["name"],
                        "read_file"
                    );
                    assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
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
        let provider = GoogleProvider::permissive_for_tests(
            GoogleConfig::new(Some("k".into())).with_base(&base),
        );
        let mut stream = provider.stream(req("gemini-x"));
        while let Some(chunk) = stream.next().await {
            if let Ok(ProviderChunk::Done) = chunk {
                break;
            }
        }
        // The request URL must carry alt=sse (the mock records the path
        // with the query stripped; the adapter URL contains it).
        let (_, path, _) = server.last_request().unwrap();
        assert_eq!(path, "/v1beta/models/gemini-x:streamGenerateContent");
    }

    /// Resolved DOCUMENT attachments lower BYTE-EXACTLY to Gemini
    /// `inline_data` with the ACTUAL document media type and standard
    /// base64.
    #[tokio::test]
    async fn document_data_lowers_to_byte_exact_inline_data() {
        let pdf: Vec<u8> = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
        let expected_b64 = faktor_provider::MediaBytes::new(pdf.clone())
            .unwrap()
            .to_base64();
        let server = MockServer::new();
        let expected = expected_b64.clone();
        server.route(
            "POST",
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::AssertThenRespond {
                status: 200,
                body: "data: {}\n\n".into(),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["contents"][1]["parts"],
                        serde_json::json!([
                            { "text": "read" },
                            {
                                "inline_data": {
                                    "mime_type": "application/pdf",
                                    "data": expected
                                }
                            }
                        ]),
                        "Gemini document lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = GoogleProvider::permissive_for_tests(
            GoogleConfig::new(Some("k".into())).with_base(&base),
        );
        assert!(provider.document_capable("gemini-x"));
        let mut r = req("gemini-x");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("read"),
                ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if let Ok(ProviderChunk::Done) = chunk {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);
    }

    /// Resolved images lower BYTE-EXACTLY to Gemini `inline_data` with the
    /// ACTUAL media type and standard base64 (never the old hardcoded
    /// image/png + URL shape).
    #[tokio::test]
    async fn image_data_lowers_to_byte_exact_inline_data() {
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 3, 4];
        let expected_b64 = faktor_provider::MediaBytes::new(png.clone())
            .unwrap()
            .to_base64();
        let server = MockServer::new();
        let expected = expected_b64.clone();
        server.route(
            "POST",
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::AssertThenRespond {
                status: 200,
                body: "data: {}\n\n".into(),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["contents"][1]["parts"],
                        serde_json::json!([
                            { "text": "look" },
                            {
                                "inline_data": {
                                    "mime_type": "image/png",
                                    "data": expected
                                }
                            }
                        ]),
                        "Gemini image lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = GoogleProvider::permissive_for_tests(
            GoogleConfig::new(Some("k".into())).with_base(&base),
        );
        assert_eq!(provider.max_image_bytes(), GOOGLE_MAX_IMAGE_BYTES);
        let mut r = req("gemini-x");
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

    /// Vision-less media for a model whose capabilities say no is refused
    /// typedly BEFORE any wire byte.
    #[tokio::test]
    async fn image_delivery_gate_refuses_visionless_pre_wire() {
        let server = MockServer::new();
        let base = server.base_url().await;
        let provider = GoogleProvider::permissive_for_tests(
            GoogleConfig::new(Some("k".into()))
                .with_base(&base)
                .with_model(
                    "gemini-x",
                    ModelCapabilities {
                        vision: false,
                        ..Default::default()
                    },
                ),
        );
        let mut r = req("gemini-x");
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

        // Over the provider's per-image bound: typed, pre-wire.
        let provider = GoogleProvider::permissive_for_tests(
            GoogleConfig::new(Some("k".into())).with_base(&base),
        );
        let mut r = req("gemini-x");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart {
                kind: ContentKind::ImageData {
                    mime: "image/png".into(),
                    data: faktor_provider::MediaBytes::new(vec![0u8; GOOGLE_MAX_IMAGE_BYTES + 1])
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
    fn usage_metadata_splits_cache_and_thought_detail_canonically() {
        // Audit Phase-1 item C: `promptTokenCount` is the TOTAL input
        // INCLUDING the cached portion — cachedContentTokenCount splits the
        // cache reads out (priced at the cache line); the remainder is the
        // uncached input. `candidatesTokenCount` already includes thinking
        // tokens; thoughtTokens are an informational subset, never billed a
        // second time.
        let frame = serde_json::json!({
            "candidates": [{"content": {"parts": []}}],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 50,
                "cachedContentTokenCount": 40,
                "thoughtTokens": 30
            }
        });
        let chunk = parse_gemini_chunk(&frame)
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
                request_id: None,
            })
        );
        // Missing cache detail: conservative category — uncached = the
        // reported total (never invent a cheaper cache line).
        let no_cache = serde_json::json!({
            "usageMetadata": {"promptTokenCount": 1000, "candidatesTokenCount": 50}
        });
        let chunk = parse_gemini_chunk(&no_cache)
            .expect("usage chunk")
            .expect("usage frame");
        assert!(matches!(
            chunk,
            ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 1000,
                cache_read_tokens: 0,
                ..
            })
        ));
        // Hostile: thought tokens cannot exceed the candidate total they
        // ride inside — typed Malformed, never a silent zero.
        let hostile = serde_json::json!({
            "candidates": [{"content": {"parts": []}}],
            "usageMetadata": {
                "promptTokenCount": 0,
                "candidatesTokenCount": 0,
                "thoughtTokens": 3
            }
        });
        let err =
            parse_gemini_chunk(&hostile).expect_err("thoughts > candidates must be Malformed");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(!err.retryable);
        // Hostile: cached tokens exceeding the prompt total.
        let hostile_cache = serde_json::json!({
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 1,
                "cachedContentTokenCount": 600
            }
        });
        let err =
            parse_gemini_chunk(&hostile_cache).expect_err("cache > prompt total must be Malformed");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        // An all-zero envelope carries nothing: no chunk.
        let zero = serde_json::json!({"usageMetadata": {}});
        assert!(parse_gemini_chunk(&zero).unwrap().is_none());
    }

    #[tokio::test]
    async fn text_and_function_call() {
        let server = MockServer::new();
        let body = [
            r#"data: {"candidates":[{"content":{"parts":[{"text":"let me check"}]}}]}"#,
            r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"a.rs"},"id":"gc1"}}]}}]}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        server.route(
            "POST",
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let provider =
            GoogleProvider::permissive_for_tests(GoogleConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("gemini-x"));
        let mut text = String::new();
        let mut call = None;
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::ToolCall {
                    id, name, input, ..
                } => call = Some((id, name, input)),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "let me check");
        let (id, name, input) = call.expect("function call");
        assert_eq!(id, "gc1");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "a.rs");
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
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::Silent { status: 200 },
        );
        let base = server.base_url().await;
        let provider =
            GoogleProvider::permissive_for_tests(GoogleConfig::new(None).with_base(&base));
        let mut g = req("gemini-x");
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
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::Respond {
                status: 429,
                body: r#"{"error":{"message":"quota"}}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider =
            GoogleProvider::permissive_for_tests(GoogleConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("gemini-x"));
        let err = stream.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
    }

    #[tokio::test]
    async fn malformed_sse_is_loud() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::Respond {
                status: 200,
                body: "data: {broken\n\n".into(),
            },
        );
        let base = server.base_url().await;
        let provider =
            GoogleProvider::permissive_for_tests(GoogleConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("gemini-x"));
        let first = stream.next().await.unwrap();
        assert!(first.is_err());
    }

    #[tokio::test]
    async fn function_call_and_function_response_ride_the_gemini_wire() {
        // The exact request shape the agent reconstructs after a tool runs:
        // model-role functionCall then user-role functionResponse carrying
        // the call id and the tool output.
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::AssertThenRespond {
                status: 200,
                body: String::new(),
                assert: Arc::new(|body: &serde_json::Value| {
                    let contents = body["contents"].as_array().expect("contents array");
                    assert_eq!(contents.len(), 2);
                    assert_eq!(contents[0]["role"], "model");
                    assert_eq!(
                        contents[0]["parts"][0]["functionCall"]["name"], "echo",
                        "the function name must ride the functionCall"
                    );
                    assert_eq!(
                        contents[0]["parts"][0]["functionCall"]["id"], "call_1",
                        "the call id must ride the functionCall"
                    );
                    assert_eq!(
                        contents[0]["parts"][0]["functionCall"]["args"],
                        serde_json::json!({"x": 1}),
                        "the call input must ride the functionCall"
                    );
                    assert_eq!(contents[1]["role"], "user");
                    assert_eq!(
                        contents[1]["parts"][0]["functionResponse"]["name"], "call_1",
                        "the functionResponse must reference the call id"
                    );
                    assert_eq!(
                        contents[1]["parts"][0]["functionResponse"]["response"]["result"],
                        "echo: {\"x\":1}",
                        "the tool output must be on the wire verbatim"
                    );
                    assert_eq!(
                        contents[1]["parts"][0]["functionResponse"]["response"]["is_error"],
                        false
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider =
            GoogleProvider::permissive_for_tests(GoogleConfig::new(None).with_base(&base));
        let mut r = req("gemini-x");
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
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![
                    br#"data: {"candidates":[{"content":{"parts":[{"text":"hel"#.to_vec(),
                    br#"lo"}]}}]}"#.to_vec(),
                    b"\n\n".to_vec(),
                    b"data: [DONE]\n\n".to_vec(),
                ],
            },
        );
        let base = server.base_url().await;
        let provider =
            GoogleProvider::permissive_for_tests(GoogleConfig::new(None).with_base(&base));
        let mut stream = provider.stream(req("gemini-x"));
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
            r#"data: {"candidates":[{"content":{"parts":[{"text":"allowed"}]}}]}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        server.route(
            "POST",
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();

        // Allowed: the allowlisted mock streams normally.
        let provider =
            GoogleProvider::build(GoogleConfig::new(None).with_base(&base), allow_only(port));
        let mut stream = provider.stream(req("gemini-x"));
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

        // Denied: wrong-port policy fails BEFORE any network byte.
        let denied = GoogleProvider::build(
            GoogleConfig::new(None).with_base(&base),
            allow_only(port.wrapping_add(1)),
        );
        let err = first_error(denied.stream(req("gemini-x"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert!(!err.retryable, "denied destinations are never retried");
        assert_eq!(server.request_count(), 1, "deny happened before connect");

        // https-to-http mismatch against an https-only rule: pre-connect deny.
        let mismatch = GoogleProvider::build(
            GoogleConfig::new(None).with_base(&base),
            Arc::new(PolicyCheckedHttpTransport::with_policy_for_tests(
                DestinationPolicy::parse_lines([&format!("https://127.0.0.1:{port}")]).unwrap(),
            )),
        );
        let err = first_error(mismatch.stream(req("gemini-x"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert_eq!(server.request_count(), 1, "scheme mismatch: no connect");
    }

    #[tokio::test]
    async fn mock_transport_canned_sse_drives_the_parser_without_http() {
        let body = [
            r#"data: {"candidates":[{"content":{"parts":[{"text":"can"}]}}]}"#,
            r#"data: {"candidates":[{"content":{"parts":[{"text":"ned"}]}}]}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        let mock = Arc::new(MockHttpTransport::new(200, body));
        let as_transport: Arc<dyn HttpTransport> = mock.clone();
        let provider = GoogleProvider::build(
            GoogleConfig::new(None).with_base("http://mock.invalid"),
            as_transport,
        );
        let mut stream = provider.stream(req("gemini-x"));
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
                "http://mock.invalid/v1beta/models/gemini-x:streamGenerateContent?alt=sse&key="
                    .to_string()
            )]
        );
    }

    // ------------------------------------------------- canonical usage

    /// Shared canonical-usage conformance for the Gemini wire (audit
    /// Phase-1 item C): mock `usageMetadata` frames shaped exactly like
    /// real gemini SSE chunks drive the REAL provider. Gemini's usage
    /// envelope carries no provider request id, so the request-id case
    /// asserts None (nothing to preserve).
    mod canonical_usage_conformance {
        use super::*;
        use faktor_provider::canonical_usage_conformance;
        use faktor_provider::CanonicalUsage;

        /// One real-wire gemini SSE chunk carrying usageMetadata. `junk`
        /// adds unknown fields at every level (never panic).
        fn usage_chunk(
            prompt_total: u64,
            candidates: u64,
            cached: u64,
            thoughts: u64,
            junk: bool,
        ) -> String {
            let mut meta = serde_json::json!({
                "promptTokenCount": prompt_total,
                "candidatesTokenCount": candidates,
                "cachedContentTokenCount": cached,
                "thoughtTokens": thoughts,
            });
            if junk {
                meta["totally_unknown"] = serde_json::json!({"deep": [1, {"x": null}]});
                meta["promptTokensDetails"] = serde_json::json!([{"modality": "TEXT"}]);
                meta["candidatesTokensDetails"] = serde_json::json!([{"modality": "TEXT"}]);
            }
            let mut frame = serde_json::json!({
                "candidates": [{"content": {"parts": []}}],
                "usageMetadata": meta,
            });
            if junk {
                frame["modelVersion"] = serde_json::json!("gemini-2.5-pro-001");
                frame["unknown_top"] = serde_json::json!([1, 2]);
            }
            format!("data: {frame}\n\ndata: [DONE]\n\n")
        }

        fn exp(uncached: u64, cache_read: u64, output: u64, reasoning: u64) -> CanonicalUsage {
            CanonicalUsage {
                uncached_input_tokens: uncached,
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
                output_tokens: output,
                reasoning_tokens: reasoning,
                reported_cost: None,
                request_id: None,
            }
        }

        canonical_usage_conformance! {
            driver: gemini_canonical_usage_conformance,
            family: faktor_provider::usage_conformance::WireFamily::InclusiveTotal,
            label: "google gemini",
            request: || req("gemini-x"),
            provider: |base: String| GoogleProvider::permissive_for_tests(GoogleConfig::new(None).with_base(&base)),
            method: "POST",
            path: "/v1beta/models/gemini-x:streamGenerateContent",
            cases: vec![
                // promptTokenCount INCLUDES the cached portion: split out.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "total_incl_cached_split",
                    usage_chunk(1000, 50, 600, 0, false),
                    exp(400, 600, 50, 0),
                ),
                // No cache detail: conservative uncached = reported total.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "cache_detail_missing_uncached_total",
                    usage_chunk(1000, 50, 0, 0, false),
                    exp(1000, 0, 50, 0),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_cache_over_total",
                    usage_chunk(100, 50, 600, 0, false),
                ),
                // candidatesTokenCount includes thinking: thoughtTokens are
                // informational and never billed a second time.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "reasoning_subset_inside_output",
                    usage_chunk(1000, 50, 0, 30, false),
                    exp(1000, 0, 50, 30),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_reasoning_over_output",
                    usage_chunk(1000, 20, 0, 30, false),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "unknown_fields_never_panic",
                    usage_chunk(1000, 50, 0, 0, true),
                    exp(1000, 0, 50, 0),
                ),
                // Gemini's usage envelope carries no request id: preserved
                // as None, never fabricated.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "request_id_preserved",
                    usage_chunk(1000, 50, 0, 0, false),
                    exp(1000, 0, 50, 0),
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
        GoogleProvider::permissive_for_tests(GoogleConfig::new(Some("k".into())).with_base(&base))
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
            err.message.len() <= GOOGLE_ERROR_BODY_MAX_BYTES + 128,
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

    /// P0 plaintext-secret lock: the Gemini config's planted key never
    /// renders through Debug, panic formatting or serialized diagnostics.
    #[test]
    fn config_debug_never_renders_the_api_key() {
        const PLANTED: &str = "AIzaPLANTED-google-key-0123456789";
        let cfg = GoogleConfig::new(Some(SecretValue::new(PLANTED)));
        let mut rendered = vec![format!("{cfg:?}")];
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("google config: {cfg:?}")
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
            assert!(!text.contains(PLANTED), "google api key leaked: {text}");
        }
        assert!(rendered[0].contains("[redacted]"));
    }

    /// Adversarial: a provider/gateway error body that echoes request
    /// credentials must never reach the error in raw form — the Gemini
    /// `key=` query credential and pattern-shaped secrets are scrubbed,
    /// bounded diagnostics survive, and 401/403 bodies are withheld
    /// entirely.
    #[tokio::test]
    async fn error_bodies_are_scrubbed_and_auth_bodies_withheld() {
        const EXACT: &str = "exact-credential-value-9f2a";
        const PATTERN: &str = "sk-abcdefghijklmnopqrstuvwx";
        fn body(sentinel: &str) -> String {
            format!(r#"{{"error":{{"code":429,"message":"{sentinel} {EXACT} {PATTERN}"}}}}"#)
        }
        fn transport() -> Arc<dyn HttpTransport> {
            Arc::new(PolicyCheckedHttpTransport::permissive())
        }
        let url_for = |base: &str| {
            format!("{base}/v1beta/models/gemini-x:streamGenerateContent?alt=sse&key={EXACT}")
        };

        // Non-auth: scrubbed bounded diagnostic, status preserved.
        let server = MockServer::new();
        server.route(
            "POST",
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::Respond {
                status: 429,
                body: body("RATE-BODY-SENTINEL"),
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(google_stream(
            transport(),
            url_for(&base),
            serde_json::json!({"contents": []}),
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
            "/v1beta/models/gemini-x:streamGenerateContent",
            MockAction::Respond {
                status: 403,
                body: body("AUTH-BODY-SENTINEL"),
            },
        );
        let base = server.base_url().await;
        let mut stream = Box::pin(google_stream(
            transport(),
            url_for(&base),
            serde_json::json!({"contents": []}),
            StreamDeadlines::default(),
            None,
        ));
        let err = stream
            .next()
            .await
            .expect("one item")
            .expect_err("403 must fail");
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
    /// exact credential (registered from the request URL's `key=` query
    /// parameter) and the pattern secret never reach `message`, `Display`,
    /// `Debug` or the JSON-serialized message; an auth-shaped payload is
    /// withheld entirely; the diagnostic stays bounded.
    #[tokio::test]
    async fn in_stream_2xx_error_payloads_are_scrubbed_or_withheld() {
        const EXACT: &str = "exact-credential-value-9f2a";
        const PATTERN: &str = "sk-abcdefghijklmnopqrstuvwx";
        fn transport() -> Arc<dyn HttpTransport> {
            Arc::new(PolicyCheckedHttpTransport::permissive())
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
                "/v1beta/models/gemini-x:streamGenerateContent",
                MockAction::Sse {
                    status: 200,
                    events,
                },
            );
            let base = server.base_url().await;
            let mut stream = Box::pin(google_stream(
                transport(),
                format!("{base}/v1beta/models/gemini-x:streamGenerateContent?alt=sse&key={EXACT}"),
                serde_json::json!({"contents": []}),
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
            "error": {
                "code": 500,
                "message": format!("GEM-SENTINEL {EXACT} {PATTERN}"),
            },
        })
        .to_string();
        let err = stream_err(vec![format!("data: {body} trailing-garbage\n\n")]).await;
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(err.message.contains("GEM-SENTINEL"), "{}", err.message);
        assert!(
            err.message.len() <= faktor_provider::sanitize::MAX_ERROR_DIAGNOSTIC_BYTES + 128,
            "diagnostic must stay bounded: {}",
            err.message.len()
        );
        assert_no_secret(&err, &[]);

        // Auth-shaped: the upstream payload is withheld entirely.
        let body = serde_json::json!({
            "error": {
                "code": 403,
                "status": "PERMISSION_DENIED",
                "message": format!("AUTH-GEM-SENTINEL {EXACT} {PATTERN}"),
            },
        })
        .to_string();
        let err = stream_err(vec![format!("data: {body} trailing-garbage\n\n")]).await;
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(err.message.contains("withheld"), "{}", err.message);
        assert_no_secret(&err, &["AUTH-GEM-SENTINEL"]);
    }
}
