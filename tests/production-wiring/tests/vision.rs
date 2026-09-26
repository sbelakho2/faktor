//! Production-wiring certification for multimodal turns through the DAEMON'S
//! provider registry and agent runtime.
//!
//! Two halves, with different reach — stated exactly:
//!
//! 1. **Refusal before dispatch** runs the full daemon-core path
//!    (`build_daemon_core` -> agent -> session -> durable attachments ->
//!    request construction): a non-vision model refuses the image-bearing
//!    turn with a typed error and the loopback endpoint sees ZERO requests.
//!
//! 2. **Ordered image parts reach the provider** is certified at the
//!    daemon's PROVIDER boundary: the provider registry, its configured
//!    loopback transport and the media bytes resolved from the daemon's own
//!    session store are production wiring, and the request carries the
//!    ordered parts (text, then images in attachment order) that the
//!    runtime's media injection produces. A full agent *turn* against a
//!    non-official endpoint is NOT routable in the current production
//!    policy — the router's Implement/Review quality floor is a HARD 60 and
//!    every custom/local endpoint carries the conservative prior 50 (only
//!    official-endpoint catalog rows clear the floor) — so that half is
//!    reported as a residual instead of being weakened silently.
//!
//! Config is loaded through the production loader (`Config::load`) with an
//! explicit `allow_loopback` rule; no subsystem is hand-built.

use std::path::Path;

use faktor_core::ErrorKind;
use faktor_provider::testing::{sse_body, MockAction, MockServer};
use faktor_provider::{ContentPart, GenericAgentRequest, MediaBytes, ProviderChunk, RequestMeta};
use futures::StreamExt;

use faktor_tests_production_wiring::wiring::{self, Config};

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
const JPG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 9, 8, 7];

fn config_file(dir: &Path, body: serde_json::Value) -> Config {
    let path = dir.join("wiring-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

fn openai_config(dir: &Path, base: &str) -> Config {
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

/// The daemon's provider registry hands an image-bearing request to the
/// configured loopback endpoint with ordered content parts, byte-exact and
/// in attachment order; the media bytes are resolved from the daemon's own
/// session store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_parts_reach_the_vision_provider_in_order() {
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
    let graph = wiring::build_production_graph(dir.path(), openai_config(dir.path(), &base))
        .expect("production daemon graph");
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, "vision wiring", "fake-openai", "default")
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
    let body: serde_json::Value = serde_json::from_str(body).expect("provider request JSON");
    let messages = body["messages"].as_array().expect("messages");
    let user = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user" && m["content"].is_array())
        .expect("the user message with media parts");
    let parts = user["content"].as_array().expect("ordered content parts");
    assert_eq!(parts.len(), 3, "text + two images: {parts:?}");
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "describe both images");
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
        wiring::build_production_graph(dir.path(), deepseek_nonvision_config(dir.path(), &base))
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
