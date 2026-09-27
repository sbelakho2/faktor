//! Production-wiring certification for AUTOMATIC retrieval through the
//! daemon core (`docs/architecture.md` §19/§20).
//!
//! Every test builds the REAL daemon graph through the production builder
//! (`build_daemon_core` via [`harness::build_production_graph`]), creates a
//! real workspace + session through the daemon's own session manager, and
//! runs ORDINARY agent turns (`AgentRuntime::run_turn`) — no search tool is
//! ever invoked and no evidence provider is hand-constructed. Retrieval
//! happens because the runtime derives concepts from the turn and consults
//! the daemon's index/evidence ladder, exactly as production does.
//!
//! What each test certifies:
//!
//! 1. **Positive**: an ordinary turn whose prompt names a concept finds the
//!    workspace file defining it, and the daemon's own request assembly
//!    puts the retrieved symbol into the provider-visible evidence section.
//!    The package that served it carries the NEW typed freshness metadata
//!    (`current`/`stale_while_rebuilding`/`partial`), asserted through the
//!    runtime's own hosted-index coverage snapshot for the same workspace.
//! 2. **Inverse**: a prompt matching nothing produces NO evidence section at
//!    all — the request is never decorated with fabricated evidence.
//! 3. **Semantic fusion**: with a configured semantic embedder (the
//!    `[embeddings]` selection resolved against the daemon's loopback
//!    provider), the fused retrieval (symbol + exact + lexical + semantic)
//!    reaches the request: a file that NO lexical/exact/symbol leg can match
//!    appears in the evidence section only because the semantic leg
//!    contributed to the fusion.
//!
//! The first turn of each test is the daemon's own first-prompt path: the
//! runtime attaches the workspace and the background index worker publishes
//! a Ready generation (the documented behavior: a first prompt never waits
//! for the build; the following prompt is served from the index). The test
//! waits, bounded, for the daemon's OWN observable (`serving` +
//! freshness `current` on `AgentRuntime::index_coverage_snapshot`) before
//! running the assertion turn.
//!
//! The chat side of the original certification was the Ollama-native adapter
//! because the OpenAI chat-completions family lowerer dropped
//! `GenericAgentRequest.system` entirely (the whole system head, evidence
//! section included, was absent from that wire body). That defect is FIXED:
//! the cacheable prefix now lowers to the first `role: "system"` message,
//! and `openai_chat_endpoint_observes_the_retrieved_evidence` certifies the
//! SAME ordinary-turn retrieval path end-to-end over the OpenAI-compatible
//! chat family (config `kind: "open_ai"`, `api: "chat"`, mock
//! `/chat/completions`). The Ollama-native tests below are retained.

use std::path::Path;
use std::time::{Duration, Instant};

use faktor_core::id::WorkspaceId;
use faktor_provider::testing::{sse_body, MockAction, MockServer};

use faktor_tests_production_wiring::harness::{self, Config};

/// How long the daemon's own index worker may take to publish a Ready
/// generation for the tiny test workspace.
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// The one native Ollama chat response (the adapter accepts a single JSON
/// body on a streaming request).
fn ollama_chat_ok() -> String {
    r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#.into()
}

fn config_file(dir: &Path, body: serde_json::Value) -> Config {
    let path = dir.join("wiring-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

/// One loopback Ollama-family provider with an EXPLICIT quality declaration
/// (quality-authority item 1) so ordinary pinned turns can clear the
/// Implement phase's hard 60 floor. Unprobed local models keep the
/// conservative embeddings-capable profile, so the same entry can also be
/// the strict `[embeddings]` selection.
fn ollama_config(dir: &Path, base: &str, with_embeddings: bool) -> Config {
    let mut body = serde_json::json!({
        "model": "default",
        "compact_at_usage": 1.0,
        "sandbox": {"network": [base]},
        "routing_mode": {"pinned": {"provider": "fake-ollama", "model": "default"}},
        "providers": [{
            "kind": "ollama",
            "id": "fake-ollama",
            "base_url": base,
            "allow_loopback": true,
            "quality": {"coding_reliability": 80, "context_reliability": 80},
        }],
    });
    if with_embeddings {
        body["embeddings"] = serde_json::json!({
            "provider": "fake-ollama",
            "model": "embed-model",
        });
    }
    config_file(dir, body)
}

/// One loopback OpenAI-compatible chat endpoint (`api: "chat"` — the
/// compatible-server default) with the same explicit quality declaration as
/// the Ollama family, so the pinned turn clears the Implement-phase floor.
fn openai_chat_config(dir: &Path, base: &str) -> Config {
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
                "api": "chat",
                "allow_loopback": true,
                "quality": {"coding_reliability": 80, "context_reliability": 80},
            }],
        }),
    )
}

fn write_workspace(dir: &Path, files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = dir.join("workspace");
    for (rel, body) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    root
}

/// Wait, bounded, for the runtime's OWN hosted index to publish a complete,
/// at-rest generation for `ws` (the new freshness fields observable through
/// the public coverage snapshot). The first ordinary turn kicked the
/// worker; this only observes the daemon's state.
async fn wait_ready(graph: &harness::DaemonGraph, ws: WorkspaceId) {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(snapshot) = graph.agent.index_coverage_snapshot(ws) {
            if snapshot.serving
                && snapshot.coverage.complete
                && snapshot.fingerprint.complete
                && snapshot.freshness == faktor_index::EvidenceFreshness::Current
            {
                return;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "the daemon's index never published a complete at-rest generation: {:?}",
                graph.agent.index_coverage_snapshot(ws)
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Bodies of the requests the mock server received on ONE path (the daemon's
/// own Ollama warm-up probes other paths and must never pollute an
/// assertion).
fn bodies_on(server: &MockServer, path: &str) -> Vec<String> {
    server
        .requests()
        .into_iter()
        .filter(|(_, p, _)| p == path)
        .map(|(_, _, body)| body)
        .collect()
}

fn last_body_on(server: &MockServer, path: &str) -> String {
    bodies_on(server, path)
        .pop()
        .unwrap_or_else(|| panic!("the provider received no request on {path}"))
}

/// Build the graph, create the workspace/session and run the daemon's own
/// first (warm-up) turn so its index worker publishes a Ready generation;
/// returns the graph, workspace and session.
async fn ready_daemon(
    dir: &Path,
    root: &Path,
    config: Config,
    provider: &str,
) -> (
    harness::DaemonGraph,
    WorkspaceId,
    faktor_core::id::SessionId,
) {
    let graph = harness::build_production_graph(dir, config).expect("production daemon graph");
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, "retrieval", provider, "default")
        .unwrap();
    graph
        .agent
        .run_turn(handle.id(), "hello", &[])
        .await
        .expect("warm-up turn");
    wait_ready(&graph, workspace).await;
    (graph, workspace, handle.id())
}

/// POSITIVE: a real workspace contains `gamma_relay_symbol`, which never
/// appears in the conversation. An ordinary turn ("fix the relay") with NO
/// search tool call must reach the provider with an evidence section that
/// contains that symbol, served from the daemon's index generation whose new
/// freshness metadata reports `current` on the runtime's own coverage
/// snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_turn_retrieves_a_symbol_absent_from_the_conversation() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/api/chat",
        MockAction::Respond {
            status: 200,
            body: ollama_chat_ok(),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = write_workspace(
        dir.path(),
        &[("src/relay.rs", "pub fn gamma_relay_symbol() -> u64 { 7 }\n")],
    );
    let (graph, workspace, session) = ready_daemon(
        dir.path(),
        &root,
        ollama_config(dir.path(), &base, false),
        "fake-ollama",
    )
    .await;

    // The new freshness fields of the package that serves this request,
    // observed on the runtime's own hosted index for the same workspace: a
    // complete, at-rest generation with a published identity.
    let snapshot = graph
        .agent
        .index_coverage_snapshot(workspace)
        .expect("the runtime hosted the index for this workspace");
    assert_eq!(
        snapshot.freshness,
        faktor_index::EvidenceFreshness::Current,
        "the served evidence came from a complete, at-rest generation"
    );
    assert!(snapshot.serving && snapshot.published_generation.is_some());
    assert!(snapshot.coverage.complete && snapshot.fingerprint.complete);

    // The assertion turn: the prompt names the concept ("gamma"), never the
    // symbol itself; the workspace symbol `gamma_relay_symbol` is absent
    // from the entire conversation.
    graph
        .agent
        .run_turn(session, "explain the gamma subsystem", &[])
        .await
        .expect("retrieval turn");

    let body = last_body_on(&server, "/api/chat");
    assert!(
        body.contains("## Retrieved evidence"),
        "the daemon's request must carry an evidence section: {body}"
    );
    assert!(
        body.contains("gamma_relay_symbol"),
        "the evidence section must contain the retrieved symbol: {body}"
    );
    assert!(
        body.contains("### src/relay.rs"),
        "the symbol must arrive as a retrieved evidence block: {body}"
    );
}

/// INVERSE: a workspace whose content shares no concept with the prompt
/// produces NO evidence section — retrieval never fabricates an evidence
/// block when nothing matched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_match_never_fabricates_an_evidence_section() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/api/chat",
        MockAction::Respond {
            status: 200,
            body: ollama_chat_ok(),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = write_workspace(
        dir.path(),
        &[("src/plain.rs", "pub fn plain_alpha() -> u64 { 1 }\n")],
    );
    let (graph, _workspace, session) = ready_daemon(
        dir.path(),
        &root,
        ollama_config(dir.path(), &base, false),
        "fake-ollama",
    )
    .await;

    // Concept tokens that appear NOWHERE in the workspace content, paths or
    // symbols: every retrieval leg must miss.
    graph
        .agent
        .run_turn(session, "wumblefrotz zqxjvbrimp", &[])
        .await
        .expect("no-match turn");

    let body = last_body_on(&server, "/api/chat");
    assert!(
        !body.contains("## Retrieved evidence"),
        "a miss must not fabricate an evidence section: {body}"
    );
    assert!(
        !body.contains("### src/plain.rs"),
        "a miss must not fabricate evidence blocks: {body}"
    );
}

/// SEMANTIC FUSION: with the daemon's configured semantic embedder, the
/// fused symbol/exact/lexical + semantic retrieval reaches the request. The
/// fake embedder maps EVERY chunk and the query to the same unit vector, so
/// the semantic leg ranks every indexed file; `src/sema.rs` shares no
/// token, path token or symbol with the prompt and can therefore appear in
/// the evidence ONLY because the semantic leg contributed to the fusion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn semantic_fusion_reaches_the_request_with_the_embedder_enabled() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/api/chat",
        MockAction::Respond {
            status: 200,
            body: ollama_chat_ok(),
        },
    );
    // First `/api/embed` call: the build embeds the two distinct chunks.
    // Every later call is a query embed (persisted vectors are scored, never
    // re-embedded): query and chunks all share the unit vector, so
    // cos(query, chunk) == 1 for both files.
    let mut actions = vec![MockAction::Respond {
        status: 200,
        body: r#"{"embeddings":[[1.0,0.0],[1.0,0.0]]}"#.into(),
    }];
    for _ in 0..64 {
        actions.push(MockAction::Respond {
            status: 200,
            body: r#"{"embeddings":[[1.0,0.0]]}"#.into(),
        });
    }
    server.route("POST", "/api/embed", MockAction::Sequence { actions });
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = write_workspace(
        dir.path(),
        &[
            ("src/lexi.rs", "pub fn zentangle() -> u64 { 1 }\n"),
            ("src/sema.rs", "pub fn wobble_flux() -> u64 { 2 }\n"),
        ],
    );
    let (graph, _workspace, session) = ready_daemon(
        dir.path(),
        &root,
        ollama_config(dir.path(), &base, true),
        "fake-ollama",
    )
    .await;

    // "zentangle" matches src/lexi.rs through the symbol/exact/lexical legs;
    // src/sema.rs shares nothing with it — only semantic fusion can surface
    // it.
    graph
        .agent
        .run_turn(session, "explain zentangle", &[])
        .await
        .expect("fusion turn");

    let body = last_body_on(&server, "/api/chat");
    assert!(
        body.contains("## Retrieved evidence"),
        "the fused retrieval must reach the request: {body}"
    );
    assert!(
        body.contains("### src/lexi.rs"),
        "the lexical/symbol match must survive the fusion: {body}"
    );
    assert!(
        body.contains("### src/sema.rs"),
        "the semantic-only match proves the semantic leg fused into the request: {body}"
    );
    // The semantic leg really consumed the configured loopback embedder.
    assert!(
        bodies_on(&server, "/api/embed").len() >= 2,
        "one build embed plus at least one query embed"
    );
}

/// POSITIVE over the OpenAI-compatible Chat Completions family — the
/// certification of the fixed defect: the SAME ordinary retrieval turn
/// reaches `POST /chat/completions` with the daemon's evidence section in
/// the FIRST message (`role: "system"`), byte-visible. Before the fix,
/// `GenericAgentRequest.system` was dropped entirely by the chat lowerer, so
/// nothing of this request (the whole system head) reached the endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_chat_endpoint_observes_the_retrieved_evidence() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/chat/completions",
        MockAction::Respond {
            status: 200,
            body: sse_body(&[serde_json::json!({
                "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]
            })]),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = write_workspace(
        dir.path(),
        &[("src/relay.rs", "pub fn gamma_relay_symbol() -> u64 { 7 }\n")],
    );
    let (graph, workspace, session) = ready_daemon(
        dir.path(),
        &root,
        openai_chat_config(dir.path(), &base),
        "fake-openai",
    )
    .await;

    // Same hosted-index observable as the Ollama-native positive test.
    let snapshot = graph
        .agent
        .index_coverage_snapshot(workspace)
        .expect("the runtime hosted the index for this workspace");
    assert_eq!(
        snapshot.freshness,
        faktor_index::EvidenceFreshness::Current,
        "the served evidence came from a complete, at-rest generation"
    );

    graph
        .agent
        .run_turn(session, "explain the gamma subsystem", &[])
        .await
        .expect("retrieval turn");

    let raw = last_body_on(&server, "/chat/completions");
    let body: serde_json::Value =
        serde_json::from_str(&raw).expect("the chat-completions body is JSON");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(
        messages[0]["role"], "system",
        "the cacheable prefix must be the FIRST chat message: {raw}"
    );
    let system = messages[0]["content"]
        .as_str()
        .expect("the system message carries the lowered prefix as a string");
    assert!(
        system.contains("## Retrieved evidence"),
        "the daemon's evidence section must be observable on the OpenAI chat wire: {raw}"
    );
    assert!(
        system.contains("gamma_relay_symbol"),
        "the retrieved symbol must be observable on the OpenAI chat wire: {raw}"
    );
    assert!(
        system.contains("### src/relay.rs"),
        "the evidence block must be observable on the OpenAI chat wire: {raw}"
    );
    // The prefix is a MESSAGE, never a top-level chat field.
    assert!(
        !body.as_object().unwrap().contains_key("system"),
        "no top-level system key on the chat wire: {raw}"
    );
}
