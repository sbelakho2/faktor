//! End-to-end integration tests: the daemon as a whole, adversarially.
//! Server + agent + session + provider + persistence working together:
//! crash-restart recovery, permission flow, compaction, paging, hostile
//! payloads and the durable native HTTP surface.

#![cfg_attr(
    not(test),
    allow(dead_code, unused_imports, unused_variables, unused_mut)
)] // test-harness crate: the lib view exists only for clippy
use std::sync::Arc;
use std::time::Duration;

use faktor_agent::{
    AgentDeps, AgentRuntime, NoEvidence, PermissionRequester, Tool, ToolOutcome, ToolRegistry,
};
use faktor_core::capability::PermissionDecision;
use faktor_core::id::SessionId;
use faktor_core::model::ModelCapabilities;
use faktor_core::state::AgentState;
use faktor_core::time::SystemClock;
use faktor_provider::{FakeProvider, ProviderRegistry, ScriptedResponse};
use faktor_server::permission::ChannelPermissionRequester;
use faktor_server::{serve, ServerDeps};
use faktor_session::SessionManager;
use tempfile::tempdir;

fn test_agent(
    session: Arc<SessionManager>,
    script: Vec<ScriptedResponse>,
    permissions: Arc<dyn PermissionRequester>,
) -> Arc<AgentRuntime> {
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            script,
        )))
        .unwrap();
    agent_with_registry(session, registry, permissions)
}

/// An agent over an already-populated provider registry. The registry's
/// instance id decides the session provider name that resolves (the native
/// tests create sessions with provider "fake").
fn agent_with_registry(
    session: Arc<SessionManager>,
    registry: ProviderRegistry,
    permissions: Arc<dyn PermissionRequester>,
) -> Arc<AgentRuntime> {
    let mut tools = ToolRegistry::new();
    tools.register(Tool {
        name: "echo".into(),
        description: "d".into(),
        input_schema: serde_json::json!({}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: faktor_agent::RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx, args| {
            Box::pin(async move {
                Ok(ToolOutcome {
                    text: format!("echo: {args}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    });
    AgentRuntime::new(AgentDeps {
        session,
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: permissions,
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tools),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: faktor_instructions::no_roots_resolver(),
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test agent.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        efficiency: Default::default(),
    })
    .unwrap()
}

/// Daemon restart: reopen the same store, run recovery, verify no state
/// loss and no double tool execution.
#[tokio::test]
async fn daemon_restart_recovers_without_state_loss() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    #[derive(Clone)]
    struct AlwaysAllow;
    impl PermissionRequester for AlwaysAllow {
        fn request(
            &self,
            _s: SessionId,
            _p: &faktor_session::ops::PermissionRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
        > {
            Box::pin(async { Ok(PermissionDecision::Allow) })
        }
    }
    let (session, agent) = {
        let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
        let agent = test_agent(
            session.clone(),
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"x": 1}),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            Arc::new(AlwaysAllow),
        );
        (session, agent)
    };
    let ws = session.create_workspace("/w").unwrap();
    let row = session
        .create_session(ws, "restart test", "fake", "m")
        .unwrap();
    let outcome = agent.run_turn(row.id(), "use echo", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    drop(session);
    drop(agent);

    // CRASH: reopen the daemon store.
    let session2 = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let perm2 = Arc::new(AlwaysAllow);
    let agent2 = test_agent(
        session2.clone(),
        vec![ScriptedResponse::End], // fresh provider: NO tool calls this time
        perm2,
    );
    // Recovery finds no pending runs (the tool finished before the crash)
    // and the session is intact.
    let reports = agent2.recover().unwrap();
    let pending_anywhere: usize = reports.iter().map(|r| r.crashed_ops.len()).sum();
    assert_eq!(
        pending_anywhere, 0,
        "no unfinished ops after a clean finish: {reports:?}"
    );
    let handle = session2.get_session(row.id()).unwrap().unwrap();
    let page = handle.messages_page(None, 100).unwrap();
    let texts: Vec<&String> = page
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            faktor_protocol::native::Part::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("done")),
        "assistant text survived the restart"
    );
    let has_tool_result = page
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .any(|p| matches!(p, faktor_protocol::native::Part::ToolResult { .. }));
    assert!(has_tool_result, "tool result survived the restart");
}

/// Hostile HTTP: oversized body, malformed ids, unknown sessions — all
/// clean typed 4xx on the native surface, and the daemon keeps serving.
#[tokio::test]
async fn hostile_http_is_clean_4xx() {
    let dir = tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let perm = ChannelPermissionRequester::new(Duration::from_secs(5));
    let agent = test_agent(session.clone(), vec![ScriptedResponse::End], perm.clone());
    let deps = ServerDeps::new(session.clone(), agent, perm).unwrap();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // Unauthenticated requests are rejected before any handler.
    let resp = client
        .get(format!("{base}/native/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Oversized body (>10MB) → 413 (or an early-close connection error;
    // either way the daemon must survive and keep serving).
    let big = serde_json::json!({"session_id": "1", "op_id": "x".repeat(11 * 1024 * 1024)});
    match client
        .post(format!("{base}/native/session/1/abort"))
        .bearer_auth(token.as_str())
        .json(&big)
        .send()
        .await
    {
        Ok(resp) => assert_eq!(resp.status(), 413),
        Err(_e) => { /* early close is acceptable; daemon liveness below */ }
    }
    // The daemon is still alive and serving.
    let resp = client
        .get(format!("{base}/native/health"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Bad ids → 400; missing sessions → 404.
    let resp = client
        .get(format!("{base}/native/messages?session=not-a-number"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .get(format!("{base}/native/messages?session=999999"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .get(format!("{base}/session/abc/projection"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .get(format!("{base}/session/999999/projection"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    // Hostile unknown query fields are strict DTO drift, never ignored.
    let resp = client
        .get(format!("{base}/native/events?session=1&smuggled=1"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let _ = handle.shutdown.send(());
}

/// Permission flow end-to-end: the agent blocks on the durable permission,
/// the in-process resolver decides it, the tool runs exactly once.
#[tokio::test]
async fn permission_flow_end_to_end() {
    let dir = tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let perm = ChannelPermissionRequester::new(Duration::from_secs(10));
    let agent = test_agent(
        session.clone(),
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::End,
        ],
        perm.clone(),
    );
    let ws = session.create_workspace("/w").unwrap();
    let row = session.create_session(ws, "perm", "fake", "m").unwrap();

    let turn = tokio::spawn({
        let agent = agent.clone();
        let id = row.id();
        async move { agent.run_turn(id, "use echo", &[]).await }
    });

    // The turn blocks on permission; resolve it through the requester.
    let mut resolved = false;
    // Deadline-based (the fixed 100x20ms loop is host-speed dependent and
    // Windows CI exposed it); the failure message stays the same.
    let perm_deadline = std::time::Instant::now() + Duration::from_secs(90);
    while std::time::Instant::now() < perm_deadline {
        if let Some(pid) = perm.pending_ids().first().copied() {
            assert!(
                perm.resolve(pid, PermissionDecision::Allow)
                    .expect("permission authority is not poisoned"),
                "resolve once"
            );
            resolved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(resolved, "permission must surface");

    let outcome = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    // The tool ran exactly once.
    let sh = session.get_session(row.id()).unwrap().unwrap();
    assert!(sh.pending_tool_runs().unwrap().is_empty());
}

/// Compaction under a growing session never exceeds the budget and cannot
/// loop (death-spiral guard at the daemon level).
#[tokio::test]
async fn compaction_under_load_never_spirals() {
    let dir = tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let perm = ChannelPermissionRequester::new(Duration::from_secs(5));
    let mut deps = test_agent_deps(session.clone(), perm.clone());
    deps.compact_at_usage = 0.01; // aggressive trigger
    deps.instructions = "You are a test agent.".into();
    let agent = AgentRuntime::new(deps).unwrap();
    let ws = session.create_workspace("/w").unwrap();
    let row = session.create_session(ws, "compact", "fake", "m").unwrap();

    // 30 turns with growing responses.
    for i in 0..30 {
        let script = vec![
            ScriptedResponse::Text(format!("turn {i} {}", "x".repeat(2000))),
            ScriptedResponse::End,
        ];
        // Rebuild the provider per turn (FakeProvider scripts are single-use).
        let mut registry = ProviderRegistry::new();
        registry
            .try_register(Arc::new(FakeProvider::with_script(
                "fake",
                ModelCapabilities {
                    tools: true,
                    ..Default::default()
                },
                script,
            )))
            .unwrap();
        let agent2 = {
            let mut deps = test_agent_deps(session.clone(), perm.clone());
            deps.compact_at_usage = 0.01;
            deps.instructions = "You are a test agent.".into();
            deps.providers = Arc::new(registry);
            AgentRuntime::new(deps).unwrap()
        };
        let outcome = agent2
            .run_turn(row.id(), &format!("turn {i}"), &[])
            .await
            .unwrap();
        assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    }
    // The session is still coherent and paged.
    let handle = session.get_session(row.id()).unwrap().unwrap();
    let page = handle.messages_page(None, 5).unwrap();
    assert!(page.messages.len() <= 5);
    assert!(page.has_more);
    let _ = agent;
}

fn test_agent_deps(
    session: Arc<SessionManager>,
    permissions: Arc<ChannelPermissionRequester>,
) -> AgentDeps {
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![ScriptedResponse::End],
        )))
        .unwrap();
    AgentDeps {
        session,
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: permissions,
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(ToolRegistry::new()),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: faktor_instructions::no_roots_resolver(),
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "i".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        efficiency: Default::default(),
    }
}

/// 100k-message session loads in constant time (paging is fundamental).
#[tokio::test]
async fn hundred_thousand_message_session_loads_constantly() {
    let dir = tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = session.create_workspace("/w").unwrap();
    let row = session.create_session(ws, "big", "fake", "m").unwrap();
    let big_handle = session.get_session(row.id()).unwrap().unwrap();
    for i in 1..=100_000 {
        big_handle
            .put_message(i, "user", serde_json::json!({"text": format!("m{i}")}))
            .unwrap();
    }
    let handle = session.get_session(row.id()).unwrap().unwrap();
    let t0 = std::time::Instant::now();
    let page = handle.messages_page(None, 100).unwrap();
    let load_ms = t0.elapsed();
    assert_eq!(page.messages.len(), 100);
    assert!(page.has_more);
    // 100k messages: the initial page must not depend on history size.
    assert!(
        load_ms < Duration::from_millis(500),
        "page load took {load_ms:?}"
    );
    // A second page via the cursor.
    let page2 = handle
        .messages_page(Some(page.next_before.unwrap()), 100)
        .unwrap();
    assert_eq!(page2.messages.len(), 100);
    assert!(page2.messages[0].seq < page.messages[0].seq);
}

/// The synthetic fixture repository indexes correctly: symbols extracted
/// from real files (spec §19/§44 fixtures/repositories).
#[test]
fn fixture_repository_indexes() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/repositories/parser-demo");
    let mut idx = faktor_index::WorkspaceIndex::new();
    let ws = faktor_core::id::WorkspaceId::new(1);
    let mut files = 0usize;
    for entry in std::fs::read_dir(root.join("src")).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let bytes = std::fs::read(&path).unwrap();
            idx.index_file(ws, &path, &bytes, 1).unwrap();
            files += 1;
        }
    }
    assert!(files >= 3, "fixture repo must have rust sources");
    // Symbols from the fixture: Lexer struct, Parser struct, new, parse,
    // next_token, lexer_advances test, parses_identifiers test.
    let all: Vec<_> = ["lexer.rs", "parser.rs"]
        .iter()
        .flat_map(|f| idx.symbols_in(ws, &root.join("src").join(f)))
        .collect();
    let names: std::collections::HashSet<&str> = all.iter().map(|s| s.name.as_str()).collect();
    for expected in [
        "Lexer",
        "Parser",
        "next_token",
        "parse",
        "lexer_advances",
        "parses_identifiers",
    ] {
        assert!(
            names.contains(expected),
            "missing symbol {expected}: {names:?}"
        );
    }
    // Tests are classified as Test symbols.
    let tests = all
        .iter()
        .filter(|s| s.kind == faktor_index::SymbolKind::Test)
        .count();
    assert_eq!(tests, 2);
    // Lexical search finds a token from the fixture.
    assert!(!idx.files_for_token(ws, "lexer", 10).is_empty());
}
