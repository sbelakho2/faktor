//! Chat Completions lowering tests: the cacheable system head must reach
//! the wire as the FIRST message (byte-exact), no internal field may leak,
//! and explicit system turns follow the prefix in order. Split out of
//! `lib.rs` to keep that grandfathered source from growing.

use super::*;
use faktor_core::cancellation::CancellationToken;
use faktor_core::id::{OpId, SessionId};
use faktor_provider::testing::{sse_body, MockAction, MockServer};
use faktor_provider::{RequestMeta, ToolSpec};
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
                // The cacheable system prefix is the FIRST message.
                assert_eq!(body["messages"][0]["role"], "system");
                assert_eq!(body["messages"][0]["content"], "sys");
                assert_eq!(body["messages"][1]["role"], "user");
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
    let provider =
        OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, Some("sk-test".into())));
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

/// The cacheable system prefix — retrieval evidence included — reaches
/// the Chat Completions wire byte-exact as the FIRST message with the
/// documented `system` role, and the conversation messages that follow
/// keep their ordered content parts untouched. An empty prefix never
/// fabricates a system message.
#[test]
fn chat_system_prompt_is_the_first_message_byte_exact() {
    let evidence = "## Retrieved evidence\n### src/relay.rs\ngamma_relay_symbol() -> u64";
    let mut r = req("m1");
    r.system = evidence.into();
    r.messages[0].content = vec![
        ContentPart::text("explain the gamma subsystem"),
        ContentPart::text("second part stays in order"),
    ];
    let body = chat_completions_body(&r, &OpenAiQuirks::default());
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "system head + one conversation turn");
    assert_eq!(
        messages[0],
        serde_json::json!({ "role": "system", "content": evidence }),
        "the system prompt must be the first message, verbatim and with the system role"
    );
    assert_eq!(
        messages[1],
        serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "explain the gamma subsystem" },
                { "type": "text", "text": "second part stays in order" },
            ]
        }),
        "conversation content parts stay ordered and unchanged"
    );
    // The prefix is refused as a top-level field: the documented chat
    // carrier is the message list only.
    assert!(body.get("system").is_none());
    assert!(body.get("instructions").is_none());

    // No prefix: no message is fabricated.
    let mut bare = req("m1");
    bare.system = String::new();
    let body = chat_completions_body(&bare, &OpenAiQuirks::default());
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
}

/// An explicit `Role::System` conversation turn (distinct from the
/// cacheable prefix) still lowers after the prefix, in order.
#[test]
fn chat_explicit_system_turns_follow_the_cacheable_prefix() {
    let mut r = req("m1");
    r.system = "PREFIX".into();
    r.messages.insert(
        0,
        RequestMessage {
            role: Role::System,
            content: vec![ContentPart::text("SYSTEM TURN")],
        },
    );
    let body = chat_completions_body(&r, &OpenAiQuirks::default());
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "PREFIX");
    assert_eq!(messages[1]["role"], "system");
    assert_eq!(
        messages[1]["content"],
        serde_json::json!([{ "type": "text", "text": "SYSTEM TURN" }])
    );
    assert_eq!(messages[2]["role"], "user");
}
