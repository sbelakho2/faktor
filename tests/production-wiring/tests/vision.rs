//! Production-wiring certification for multimodal turns through the DAEMON'S
//! provider registry and agent runtime.
//!
//! Three halves, with different reach — stated exactly:
//!
//! 1. **Refusal before dispatch** runs the full daemon-core path
//!    (`build_daemon_core` -> agent -> session -> durable attachments ->
//!    request construction): a non-vision model refuses the image-bearing
//!    turn with a typed error and the loopback endpoint sees ZERO requests.
//!
//! 2. **The vision turn routes end to end** (`AgentRuntime::run_turn`):
//!    `build_daemon_core` builds the graph, two attachments are seeded
//!    through the actual session API, and the daemon's own retrieval ->
//!    context -> media injection -> routing path must hand the loopback
//!    provider the turn text + PNG + JPEG byte-exact and in durable order.
//!    The loopback endpoint is only routable because the provider entry
//!    declares its quality (`providers.*.quality`, the quality-authority
//!    declaration): without it, the conservative unknown-quality prior
//!    cannot clear the Implement phase's hard 60 floor and the turn would
//!    be refused before dispatch (the exact defect this test certifies
//!    closed).
//!
//! 3. **A transient provider error retries without duplicating media**: the
//!    runtime's state-aware retry replays the SAME planned request after a
//!    500 (nothing durable happened), so the retry must carry the same
//!    text + PNG + JPEG exactly once — never a second injected copy.
//!
//! The direct-provider test (`adapter_serializes_ordered_media_parts...`)
//! is retained as an ADAPTER-serialization contract: it calls the
//! daemon-created provider directly and proves only the wire lowering of
//! ordered media parts.
//!
//! Config is loaded through the production loader (`Config::load`) with an
//! explicit `allow_loopback` rule; no subsystem is hand-built.

use std::path::Path;

use faktor_core::retry::{RetryClass, RetryPolicy};
use faktor_core::ErrorKind;
use faktor_provider::testing::{sse_body, MockAction, MockServer};
use faktor_provider::{ContentPart, GenericAgentRequest, MediaBytes, ProviderChunk, RequestMeta};
use futures::StreamExt;

use faktor_tests_production_wiring::harness::{self, Config};

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
const JPG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 9, 8, 7];

fn config_file(dir: &Path, body: serde_json::Value) -> Config {
    let path = dir.join("wiring-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

/// A loopback OpenAI-compatible provider whose quality is EXPLICITLY
/// authorized (`providers.*.quality`, quality-authority declaration): the
/// user's declaration is what lets the conservative unknown-quality row
/// clear the Implement phase's hard 60 floor. Routing is pinned so the
/// session's turn must select exactly this endpoint.
fn vision_config(dir: &Path, base: &str) -> Config {
    config_file(
        dir,
        serde_json::json!({
            "model": "default",
            "compact_at_usage": 1.0,
            "sandbox": {"network": [base]},
            "routing_mode": {"pinned": {"provider": "fake-openai", "model": "default"}},
            "providers": [{
                "kind": "open_ai",
                "id": "fake-openai",
                "base_url": base,
                "api_key_env": null,
                "allow_loopback": true,
                "quality": {
                    "coding_reliability": 80,
                    "context_reliability": 80,
                    "tool_reliability": 80,
                    "reasoning_reliability": 80,
                },
            }],
        }),
    )
}

fn deepseek_nonvision_config(dir: &Path, base: &str) -> Config {
    config_file(
        dir,
        serde_json::json!({
            "model": "default",
            "compact_at_usage": 1.0,
            "routing_mode": {"pinned": {"provider": "local-nv", "model": "default"}},
            "providers": [{
                "kind": "deep_seek",
                "id": "local-nv",
                "profile": "compatible",
                "base_url": base,
                "api_key_env": null,
                "allow_loopback": true,
            }],
        }),
    )
}

fn workspace_for(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("README.md"), "wiring workspace\n").unwrap();
    root
}

fn sse_text(text: &str) -> String {
    sse_body(&[serde_json::json!({
        "choices": [{"delta": {"content": text}, "finish_reason": "stop"}]
    })])
}

/// The user message carrying the turn's media, with its ordered content
/// parts as the provider request body lowered them.
fn media_parts(body: &str) -> Vec<serde_json::Value> {
    let body: serde_json::Value = serde_json::from_str(body).expect("provider request JSON");
    let messages = body["messages"].as_array().expect("messages");
    messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user" && m["content"].is_array())
        .expect("the user message with media parts")["content"]
        .as_array()
        .expect("ordered content parts")
        .clone()
}

/// Assert the ordered parts are exactly text + PNG + JPEG, byte-exact.
fn assert_text_png_jpeg(parts: &[serde_json::Value], turn_text: &str) {
    assert_eq!(parts.len(), 3, "text + two images: {parts:?}");
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], turn_text);
    let expected_png = MediaBytes::new(PNG.to_vec())
        .unwrap()
        .to_data_url("image/png");
    let expected_jpg = MediaBytes::new(JPG.to_vec())
        .unwrap()
        .to_data_url("image/jpeg");
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(
        parts[1]["image_url"]["url"], expected_png,
        "the first image must be the byte-exact PNG"
    );
    assert_eq!(parts[2]["type"], "image_url");
    assert_eq!(
        parts[2]["image_url"]["url"], expected_jpg,
        "the second image must be the byte-exact JPEG (order preserved)"
    );
}

/// END-TO-END: `graph.agent.run_turn` against the daemon-built loopback
/// vision provider. The request that reaches the endpoint must be the
/// runtime's own assembled turn: text + PNG + JPEG, byte-exact, in the
/// durable attachment order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vision_turn_routes_through_the_agent_and_sends_ordered_media() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/chat/completions",
        MockAction::Respond {
            status: 200,
            body: sse_text("seen both"),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = workspace_for(dir.path());
    let graph = harness::build_production_graph(dir.path(), vision_config(dir.path(), &base))
        .expect("production daemon graph");
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, "vision e2e", "fake-openai", "default")
        .unwrap();
    // Two attachments through the actual session API, in durable order.
    let png = handle
        .put_attachment("image/png", Some("a.png"), PNG)
        .unwrap();
    let jpg = handle
        .put_attachment("image/jpeg", Some("b.jpg"), JPG)
        .unwrap();
    graph
        .agent
        .seed_task_attachments(handle.id(), &[png.clone(), jpg.clone()])
        .expect("durable attachment rows");

    let outcome = graph
        .agent
        .run_turn(handle.id(), "describe both images", &[])
        .await
        .expect("the vision turn must route to the quality-authorized loopback provider");
    assert!(!outcome.queued, "an unqueued turn ran: {outcome:?}");

    assert_eq!(
        server.request_count(),
        1,
        "exactly one provider call for the whole turn"
    );
    let (_, path, body) = &server.requests()[0];
    assert_eq!(path, "/chat/completions");
    assert_text_png_jpeg(&media_parts(body), "describe both images");

    // The durable rows are unchanged by request construction: the media
    // rides the request only, never the session state.
    assert_eq!(handle.list_attachments(16).unwrap(), vec![png, jpg]);
}

/// A transient provider failure (500 on the first physical attempt) retries
/// the SAME planned request: the second request must carry the identical
/// text + PNG + JPEG once each — the retry never re-injects or duplicates
/// media. The attempt loop only retries when NOTHING durable happened, so
/// the request bodies are byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_provider_error_retries_without_duplicating_media() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/chat/completions",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 500,
                    body: "transient upstream failure".into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: sse_text("recovered"),
                },
            ],
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = workspace_for(dir.path());
    let graph = harness::build_production_graph(dir.path(), vision_config(dir.path(), &base))
        .expect("production daemon graph");
    // The daemon builder hardcodes the one-attempt default; the certification
    // installs the multi-attempt policy through the additive seam so the
    // retry path itself is exercised.
    graph.agent.set_retry_policy(RetryPolicy {
        max_attempts: 2,
        base_delay_ms: 1,
        max_delay_ms: 1,
        jitter: 0.0,
        class: RetryClass::ServerError,
    });
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, "vision retry", "fake-openai", "default")
        .unwrap();
    let png = handle
        .put_attachment("image/png", Some("a.png"), PNG)
        .unwrap();
    let jpg = handle
        .put_attachment("image/jpeg", Some("b.jpg"), JPG)
        .unwrap();
    graph
        .agent
        .seed_task_attachments(handle.id(), &[png, jpg])
        .expect("durable attachment rows");

    let outcome = graph
        .agent
        .run_turn(handle.id(), "describe both images", &[])
        .await
        .expect("the retry must recover on the scripted second response");
    assert!(!outcome.queued, "an unqueued turn ran: {outcome:?}");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "one failed attempt then one retry: {requests:?}"
    );
    assert_text_png_jpeg(&media_parts(&requests[0].2), "describe both images");
    assert_text_png_jpeg(&media_parts(&requests[1].2), "describe both images");
    assert_eq!(
        requests[0].2, requests[1].2,
        "the retry replays the SAME planned request byte-for-byte (no duplicated media)"
    );
}

/// ADAPTER SERIALIZATION CONTRACT (demoted from the former principal
/// positive test): the daemon-created provider registry, its configured
/// loopback transport and the media bytes resolved from the daemon's own
/// session store lower to the exact ordered OpenAI wire parts (text, then
/// images in attachment order). This proves provider serialization and the
/// registry/transport wiring — NOT the agent turn (that is certified by
/// `vision_turn_routes_through_the_agent_and_sends_ordered_media`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapter_serializes_ordered_media_parts_byte_exact() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/chat/completions",
        MockAction::Respond {
            status: 200,
            body: sse_text("seen"),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = workspace_for(dir.path());
    let graph = harness::build_production_graph(dir.path(), vision_config(dir.path(), &base))
        .expect("production daemon graph");
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, "vision adapter", "fake-openai", "default")
        .unwrap();
    let png = handle
        .put_attachment("image/png", Some("a.png"), PNG)
        .unwrap();
    let jpg = handle
        .put_attachment("image/jpeg", Some("b.jpg"), JPG)
        .unwrap();

    // The media bytes come back out of the daemon's own durable store; the
    // parts are the ordered shape the runtime's media injection emits (the
    // turn text first, then images in attachment order).
    let png_bytes = handle.attachment_bytes(&png, 1 << 20).unwrap();
    let jpg_bytes = handle.attachment_bytes(&jpg, 1 << 20).unwrap();
    let provider = graph
        .providers
        .get("fake-openai")
        .expect("the daemon registered the configured provider");
    let request = GenericAgentRequest {
        model: "default".into(),
        messages: vec![faktor_provider::RequestMessage {
            role: faktor_provider::Role::User,
            content: vec![
                ContentPart::text("describe both images"),
                ContentPart::image_data("image/png", png_bytes).unwrap(),
                ContentPart::image_data("image/jpeg", jpg_bytes).unwrap(),
            ],
        }],
        tools: Vec::new(),
        system: String::new(),
        max_output: Some(64),
        reasoning: None,
        stream: true,
        meta: RequestMeta {
            operation_id: faktor_core::OpId::new(1),
            session_id: handle.id(),
            provider: "fake-openai".into(),
            attempt: 0,
            deadline_ms: 10_000,
            cancellation: faktor_core::cancellation::CancellationToken::new(),
        },
    };
    let mut stream = provider.stream(request);
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item.expect("the daemon transport must dispatch") {
            ProviderChunk::Text { text: t } => text.push_str(&t),
            ProviderChunk::Done => break,
            _ => {}
        }
    }
    assert_eq!(text, "seen");
    assert_eq!(server.request_count(), 1, "exactly one provider call");

    let requests = server.requests();
    let (_, path, body) = &requests[0];
    assert_eq!(path, "/chat/completions");
    assert_text_png_jpeg(&media_parts(body), "describe both images");
}

/// A model whose capabilities have no vision refuses an image-bearing
/// daemon turn typed BEFORE any dispatch: the loopback endpoint sees zero
/// requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_vision_model_refuses_before_dispatch() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/chat/completions",
        MockAction::Respond {
            status: 200,
            body: sse_text("should never run"),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = workspace_for(dir.path());
    let graph =
        harness::build_production_graph(dir.path(), deepseek_nonvision_config(dir.path(), &base))
            .expect("production daemon graph");
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, "non-vision wiring", "local-nv", "default")
        .unwrap();
    let session = handle.id();
    let png = handle
        .put_attachment("image/png", Some("a.png"), PNG)
        .unwrap();
    graph
        .agent
        .seed_task_attachments(session, std::slice::from_ref(&png))
        .expect("durable image attachment");

    let error = graph
        .agent
        .run_turn(session, "describe the image", &[])
        .await
        .expect_err("a non-vision model must refuse the image turn");
    assert_eq!(error.kind, ErrorKind::Malformed, "{error:?}");
    assert!(
        error.message.contains("does not support vision"),
        "{error:?}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "the refusal must happen before any provider dispatch"
    );
}
