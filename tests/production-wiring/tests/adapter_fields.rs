//! Adapter lowering audit (production-wiring certification): every field of
//! a `GenericAgentRequest` must reach each vendor wire in its documented
//! position — or be refused typedly BEFORE any wire byte. No adapter may
//! silently drop a request field.
//!
//! The table below is one row per wired adapter family; each row's checker
//! asserts the SAME field list against that family's frozen wire shape:
//!
//! | request field | chat | responses | anthropic | google | ollama |
//! |---|---|---|---|---|---|
//! | `model` | body.model | body.model | body.model | URL path | body.model |
//! | `system` | messages[0] system | `instructions` | top-level `system` | `systemInstruction` | messages[0] system |
//! | user text | text part | input_text | text part | text part | content |
//! | resolved image | image_url data URL | input_image | image base64 | inline_data | images base64 |
//! | assistant tool call | tool_calls | top-level function_call | tool_use | functionCall | tool_calls |
//! | tool result | role tool | function_call_output | tool_result | functionResponse | role tool |
//! | resolved document | file part | input_file | document base64 | inline_data | REFUSED pre-wire |
//! | `tools` | tools[0].function | tools[0] flattened | tools[0] | functionDeclarations | tools[0].function |
//! | `max_output` | max_tokens | max_output_tokens | max_tokens | maxOutputTokens | options.num_predict |
//! | `stream` | stream | stream true | stream true | streaming endpoint | stream true |
//!
//! `temperature` is not a `GenericAgentRequest` field (nothing to lower).
//! The Ollama document row is a deliberate typed refusal: a hostile request
//! carrying document bytes must not reach that wire at all.

use std::collections::HashMap;
use std::sync::Arc;

use faktor_core::cancellation::CancellationToken;
use faktor_core::id::{OpId, SessionId};
use faktor_core::model::ModelCapabilities;
use faktor_provider::egress::{HttpTransport, PolicyCheckedHttpTransport};
use faktor_provider::testing::{sse_body, MockAction, MockServer};
use faktor_provider::{
    ContentPart, GenericAgentRequest, Provider, ProviderChunk, ProviderStream, RequestMessage,
    RequestMeta, Role, ToolSpec,
};
use futures::StreamExt;

const SYS: &str = "SYS-SENTINEL-9";
const TEXT: &str = "TEXT-SENTINEL";
const CALL_TEXT: &str = "CALL-SENTINEL";
const RESULT: &str = "RESULT-SENTINEL";
const MAX_OUT: usize = 777;
const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF";

fn transport() -> Arc<dyn HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::permissive())
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    faktor_provider::MediaBytes::new(bytes.to_vec())
        .unwrap()
        .to_data_url(mime)
}

fn b64(bytes: &[u8]) -> String {
    faktor_provider::MediaBytes::new(bytes.to_vec())
        .unwrap()
        .to_base64()
}

/// Every audited request field populated at once. `with_document` is false
/// only for the Ollama positive row (that wire has no document carrier; the
/// refusal case is asserted separately).
fn full_request(model: &str, with_document: bool) -> GenericAgentRequest {
    let mut messages = vec![
        RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text(TEXT),
                ContentPart::image_data("image/png", PNG.to_vec()).unwrap(),
            ],
        },
        RequestMessage {
            role: Role::Assistant,
            content: vec![
                ContentPart::text(CALL_TEXT),
                ContentPart::tool_call(
                    "call_1",
                    "read_file",
                    serde_json::json!({"path": "src/a.rs"}),
                ),
            ],
        },
        RequestMessage {
            role: Role::User,
            content: vec![ContentPart::tool_result(RESULT, false, "call_1")],
        },
    ];
    if with_document {
        messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart::file_data(
                "application/pdf",
                Some("spec.pdf"),
                PDF.to_vec(),
            )
            .unwrap()],
        });
    }
    GenericAgentRequest {
        model: model.into(),
        system: SYS.into(),
        messages,
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        max_output: Some(MAX_OUT),
        reasoning: None,
        stream: true,
        meta: RequestMeta {
            operation_id: OpId::new(1),
            session_id: SessionId::new(1),
            provider: "adapter-audit".into(),
            attempt: 0,
            deadline_ms: 10_000,
            cancellation: CancellationToken::new(),
        },
    }
}

/// Drain a provider stream to its terminal `Done`; any typed error fails the
/// audit loudly (a wire-level refusal on an audited field is a finding).
async fn drain(stream: ProviderStream) {
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item {
            Ok(ProviderChunk::Done) => return,
            Ok(_) => {}
            Err(e) => panic!("adapter refused an audited request: {e:?}"),
        }
    }
}

/// One provider row of the audit table: the mock path its family calls, the
/// one native response body that terminates its stream, the constructor and
/// the per-field wire checker.
struct Row {
    adapter: &'static str,
    path: &'static str,
    response: String,
    build: fn(&str) -> Arc<dyn Provider>,
    check: fn(&serde_json::Value, &str),
    model: &'static str,
    with_document: bool,
}

fn rows() -> Vec<Row> {
    vec![
        Row {
            adapter: "openai-chat",
            path: "/chat/completions",
            response: sse_body(&[serde_json::json!({
                "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]
            })]),
            build: |base| {
                faktor_openai::OpenAiProvider::build(
                    faktor_openai::OpenAiConfig::chat(base, None),
                    transport(),
                )
            },
            check: check_openai_chat,
            model: "m1",
            with_document: true,
        },
        Row {
            adapter: "openai-responses",
            path: "/responses",
            response: sse_body(&[
                serde_json::json!({"type": "response.output_text.delta", "delta": "ok"}),
                serde_json::json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
            ]),
            build: |base| {
                faktor_openai::OpenAiProvider::build(
                    faktor_openai::OpenAiConfig::responses(base, None),
                    transport(),
                )
            },
            check: check_openai_responses,
            model: "m1",
            with_document: true,
        },
        Row {
            adapter: "anthropic",
            path: "/v1/messages",
            response: "data: {\"type\":\"message_stop\"}\n\ndata: [DONE]\n\n".into(),
            build: |base| {
                faktor_anthropic::AnthropicProvider::build(
                    faktor_anthropic::AnthropicConfig::new(None).with_base(base),
                    transport(),
                )
            },
            check: check_anthropic,
            model: "claude-x",
            with_document: true,
        },
        Row {
            adapter: "google",
            path: "/v1beta/models/gemini-x:streamGenerateContent",
            response: "data: {}\n\n".into(),
            build: |base| {
                faktor_google::GoogleProvider::build(
                    faktor_google::GoogleConfig::new(None).with_base(base),
                    transport(),
                )
            },
            check: check_google,
            model: "gemini-x",
            with_document: true,
        },
        Row {
            adapter: "ollama",
            path: "/api/chat",
            response: r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#.into(),
            build: |base| {
                let mut caps = ModelCapabilities::small_local();
                caps.vision = true;
                faktor_ollama::OllamaProvider::build(
                    faktor_ollama::OllamaConfig {
                        base_url: base.to_string(),
                        keep_alive: None,
                        model_overrides: HashMap::from([("qwen-audit".to_string(), caps)]),
                    },
                    transport(),
                )
            },
            check: check_ollama,
            model: "qwen-audit",
            with_document: false,
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_adapter_drops_a_request_field_silently() {
    for row in rows() {
        let server = MockServer::new();
        server.route(
            "POST",
            row.path,
            MockAction::Respond {
                status: 200,
                body: row.response.clone(),
            },
        );
        let base = server.base_url().await;
        let provider = (row.build)(&base);
        drain(provider.stream(full_request(row.model, row.with_document))).await;
        assert_eq!(
            server.request_count(),
            1,
            "{}: exactly one wire request",
            row.adapter
        );
        let (_, path, raw) = server.last_request().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&raw).expect("lowered request body must be JSON");
        (row.check)(&body, &path);
    }
}

/// The Ollama row's document half: the wire has no document carrier, so a
/// request carrying one is refused typedly BEFORE any byte (never silently
/// dropped).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ollama_refuses_documents_pre_wire() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/api/chat",
        MockAction::Respond {
            status: 200,
            body: r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#.into(),
        },
    );
    let base = server.base_url().await;
    let mut caps = ModelCapabilities::small_local();
    caps.vision = true;
    let provider = faktor_ollama::OllamaProvider::build(
        faktor_ollama::OllamaConfig {
            base_url: base,
            keep_alive: None,
            model_overrides: HashMap::from([("qwen-audit".to_string(), caps)]),
        },
        transport(),
    );
    let err = provider
        .stream(full_request("qwen-audit", true))
        .next()
        .await
        .expect("one refusal frame")
        .expect_err("a document-bearing request must be refused");
    assert!(
        err.message.contains("document"),
        "the refusal must name the document gate: {err:?}"
    );
    assert_eq!(server.request_count(), 0, "refusal happens pre-wire");
}

fn check_openai_chat(body: &serde_json::Value, path: &str) {
    assert_eq!(path, "/chat/completions");
    assert_eq!(body["model"], "m1");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(
        messages[0],
        serde_json::json!({"role": "system", "content": SYS}),
        "system must be the first chat message, byte-exact"
    );
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(
        messages[1]["content"],
        serde_json::json!([
            {"type": "text", "text": TEXT},
            {"type": "image_url", "image_url": {"url": data_url("image/png", PNG)}},
        ])
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(
        messages[2]["content"],
        serde_json::json!([{"type": "text", "text": CALL_TEXT}])
    );
    assert_eq!(
        messages[2]["tool_calls"],
        serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"src/a.rs\"}"},
        }])
    );
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    assert_eq!(messages[3]["content"], RESULT);
    assert_eq!(messages[4]["role"], "user");
    assert_eq!(
        messages[4]["content"],
        serde_json::json!([{
            "type": "file",
            "file": {"filename": "spec.pdf", "file_data": data_url("application/pdf", PDF)},
        }])
    );
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(body["max_tokens"], MAX_OUT);
    assert_eq!(body["stream"], true);
}

fn check_openai_responses(body: &serde_json::Value, path: &str) {
    assert_eq!(path, "/responses");
    assert_eq!(body["model"], "m1");
    assert_eq!(
        body["instructions"], SYS,
        "system rides top-level instructions"
    );
    let input = body["input"].as_array().expect("input items");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(
        input[0]["content"],
        serde_json::json!([
            {"type": "input_text", "text": TEXT},
            {"type": "input_image", "image_url": data_url("image/png", PNG)},
        ])
    );
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(
        input[1]["content"],
        serde_json::json!([{"type": "output_text", "text": CALL_TEXT}])
    );
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[2]["name"], "read_file");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_1");
    assert_eq!(input[3]["output"], RESULT);
    assert_eq!(input[4]["role"], "user");
    assert_eq!(
        input[4]["content"],
        serde_json::json!([{
            "type": "input_file",
            "filename": "spec.pdf",
            "file_data": data_url("application/pdf", PDF),
        }])
    );
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert!(body["tools"][0].get("function").is_none());
    assert_eq!(body["max_output_tokens"], MAX_OUT);
    assert_eq!(body["stream"], true);
}

fn check_anthropic(body: &serde_json::Value, path: &str) {
    assert_eq!(path, "/v1/messages");
    assert_eq!(body["model"], "claude-x");
    assert_eq!(body["system"], SYS, "system rides the top-level field");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(
        messages[0]["content"],
        serde_json::json!([
            {"type": "text", "text": TEXT},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": b64(PNG)}},
        ])
    );
    assert_eq!(
        messages[1]["content"],
        serde_json::json!([
            {"type": "text", "text": CALL_TEXT},
            {"type": "tool_use", "id": "call_1", "name": "read_file", "input": {"path": "src/a.rs"}},
        ])
    );
    assert_eq!(
        messages[2]["content"],
        serde_json::json!([
            {"type": "tool_result", "tool_use_id": "call_1", "content": RESULT, "is_error": false},
        ])
    );
    assert_eq!(
        messages[3]["content"],
        serde_json::json!([
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": b64(PDF)}},
        ])
    );
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["max_tokens"], MAX_OUT);
    assert_eq!(body["stream"], true);
}

fn check_google(body: &serde_json::Value, path: &str) {
    assert!(
        path.contains("models/gemini-x:streamGenerateContent"),
        "model must lower into the URL: {path}"
    );
    assert_eq!(
        body["systemInstruction"],
        serde_json::json!({"parts": [{"text": SYS}]}),
        "system rides the top-level systemInstruction field"
    );
    let contents = body["contents"].as_array().expect("contents");
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(
        contents[0]["parts"],
        serde_json::json!([
            {"text": TEXT},
            {"inline_data": {"mime_type": "image/png", "data": b64(PNG)}},
        ])
    );
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(
        contents[1]["parts"],
        serde_json::json!([
            {"text": CALL_TEXT},
            {"functionCall": {"name": "read_file", "args": {"path": "src/a.rs"}, "id": "call_1"}},
        ])
    );
    assert_eq!(
        contents[2]["parts"],
        serde_json::json!([
            {"functionResponse": {"name": "call_1", "response": {"result": RESULT, "is_error": false}}},
        ])
    );
    assert_eq!(
        contents[3]["parts"],
        serde_json::json!([
            {"inline_data": {"mime_type": "application/pdf", "data": b64(PDF)}},
        ])
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "read_file"
    );
    assert_eq!(body["generationConfig"]["maxOutputTokens"], MAX_OUT);
}

fn check_ollama(body: &serde_json::Value, path: &str) {
    assert_eq!(path, "/api/chat");
    assert_eq!(body["model"], "qwen-audit");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(
        messages[0],
        serde_json::json!({"role": "system", "content": SYS}),
        "system must be the first native message, byte-exact"
    );
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], TEXT);
    assert_eq!(
        messages[1]["images"],
        serde_json::json!([b64(PNG)]),
        "images ride raw base64"
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], CALL_TEXT);
    assert_eq!(
        messages[2]["tool_calls"],
        serde_json::json!([{"function": {"name": "read_file", "arguments": {"path": "src/a.rs"}}}]),
        "the native tool call carries the ordered function object"
    );
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["content"], RESULT);
    assert_eq!(messages[3]["tool_name"], "read_file");
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(body["options"]["num_predict"], MAX_OUT);
    assert_eq!(body["stream"], true);
}
