use crate::api::tests::*;
use crate::api::*;

#[tokio::test]
async fn native_projection_idle_session_shape_auth_and_errors() {
    // GET /session/{id}/projection on a session that never ran a turn:
    // the row-backed projection is honest (idle, no task data), every
    // native endpoint demands auth, unknown sessions 404 and malformed
    // ids 400.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let ws = deps.session.create_workspace("/tmp").unwrap();
    let created = deps
        .session
        .create_session(ws, "t-proj", "fake", "m")
        .unwrap();
    let sid = created.id().to_string();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // Native endpoints are auth-required like every daemon route.
    for path in [
        format!("/session/{sid}/projection"),
        "/models".to_string(),
        "/capabilities".to_string(),
    ] {
        let resp = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(resp.status(), 401, "{path}");
    }

    // Runtime tool capability metadata (audit 14): the readOnlyShell
    // availability rides /capabilities and mirrors the build fact.
    let resp = client
        .get(format!("{base}/capabilities"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["readOnlyShell"]["available"],
        faktor_sandbox::filesystem_backend_available(),
        "readOnlyShell.available must mirror the sandbox build fact: {body}"
    );
    if !faktor_sandbox::filesystem_backend_available() {
        assert_eq!(
            body["readOnlyShell"]["reason"],
            "os_filesystem_confinement_unavailable"
        );
    }

    // Idle projection: row state, no task data yet.
    let resp = client
        .get(format!("{base}/session/{sid}/projection"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["session"]["id"], sid);
    assert_eq!(body["session"]["provider"], "fake");
    assert_eq!(body["session"]["model"], "m");
    assert_eq!(body["session"]["lifecycle"], "open");
    assert_eq!(body["state"]["machine"], "idle");
    assert_eq!(body["state"]["label"], "idle");
    assert_eq!(body["state"]["active"], false);
    assert_eq!(body["state"]["terminal"], false);
    assert!(
        body["activeModel"].is_null(),
        "no turn record before the first turn: {body}"
    );
    assert!(body["activeTool"].is_null(), "nothing running: {body}");
    assert!(body["progress"].is_null());
    assert_eq!(body["filesChanged"], serde_json::json!([]));
    assert_eq!(body["verification"], serde_json::json!([]));
    assert!(
        body["lastCheckpoint"].is_null(),
        "no checkpoint service wired in tests"
    );
    assert!(body["contextUsage"].is_null());
    assert_eq!(body["queued"], 0);
    assert!(
        body["prefixStability"].is_null(),
        "no prefix observation before any driven turn: {body}"
    );

    // Unknown session → 404; non-numeric id → 400.
    let resp = client
        .get(format!("{base}/session/999999/projection"))
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
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_projection_after_driven_turn_reports_ledger_files() {
    // Drive a real turn whose tool call changes a file (write_file →
    // durable ledger changed_files), then assert the projection maps
    // the durable state: ledger files, turn-record model envelope and
    // terminal machine state.
    let dir = tempfile::tempdir().unwrap();
    let mut registry = faktor_provider::ProviderRegistry::new();
    registry
        .try_register(Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![
                faktor_provider::ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({"path": "src/a.txt"}),
                },
                faktor_provider::ScriptedResponse::End,
            ],
        )))
        .unwrap();
    let mut tools = faktor_agent::ToolRegistry::new();
    tools.register(faktor_agent::Tool {
        name: "write_file".into(),
        description: "write a file".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        }),
        resource_class: faktor_core::resource::ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: faktor_agent::RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(|_ctx, _args| {
            Box::pin(async move {
                Ok(faktor_agent::ToolOutcome {
                    text: "wrote src/a.txt".into(),
                    exit_code: Some(0),
                    effect_status: faktor_core::op::EffectStatus::Applied,
                    ..Default::default()
                })
            })
        }),
    });
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let permissions = ChannelPermissionRequester::new(Duration::from_secs(5));
    let agent = AgentRuntime::new(faktor_agent::AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: permissions.clone(),
        evidence: Arc::new(faktor_agent::NoEvidence),
        tools: Arc::new(tools),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test server agent.".into(),
        hooks: None,
        instructions_resolver: faktor_instructions::no_roots_resolver(),
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        clock: Arc::new(faktor_core::time::SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    })
    .unwrap();
    let manager = session.clone();
    let driver = agent.clone();
    let deps = ServerDeps::new(session, agent, permissions.clone()).unwrap();
    let token = deps.auth_token.clone();
    let ws = manager.create_workspace("/tmp").unwrap();
    let created = manager.create_session(ws, "t-drive", "fake", "m").unwrap();
    let sid = created.id().to_string();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // The fake provider makes one write_file tool call; the turn blocks
    // on the permission hop until the in-process requester resolves it.
    let drive_id = created.id();
    let drive =
        tokio::spawn(async move { driver.run_turn(drive_id, "change src/a.txt", &[]).await });
    let resolve = async {
        for _ in 0..100 {
            if let Some(pid) = permissions.pending_ids().first().copied() {
                assert!(
                    permissions
                        .resolve(drive_id, pid, PermissionDecision::Allow)
                        .expect("the permission belongs to the driving session"),
                    "the permission hop resolves once"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("tool permission never surfaced");
    };
    let (drive_result, ()) = tokio::join!(drive, resolve);
    drive_result.unwrap().unwrap();

    // Wait for the machine to land on its terminal turn state.
    let mut body = serde_json::Value::Null;
    for _ in 0..100 {
        let resp = client
            .get(format!("{base}/session/{sid}/projection"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        body = resp.json().await.unwrap();
        if body["state"]["machine"] == "ready_for_next_turn" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        body["state"]["machine"], "ready_for_next_turn",
        "turn must complete: {body}"
    );
    // The durable ledger's changed file appears in the projection...
    let files = body["filesChanged"].as_array().unwrap();
    assert!(
        files.iter().any(|f| f == "src/a.txt"),
        "ledger changed files must surface: {files:?}"
    );
    // ...the turn record's effective envelope is the activeModel...
    assert_eq!(body["activeModel"]["provider"], "fake");
    assert_eq!(body["activeModel"]["model"], "m");
    // ...and nothing is left running or queued.
    assert!(body["activeTool"].is_null(), "{body}");
    assert_eq!(body["verification"], serde_json::json!([]));
    assert_eq!(body["queued"], 0);
    // The driven turn settled provider calls, so the additive prefix
    // stability aggregate is present (its exact value is asserted in
    // the dedicated text-only test below — a tool turn rewrites the
    // head between its two calls, so only presence is pinned here).
    assert!(
        body["prefixStability"].is_object(),
        "prefix stability must surface after a driven turn: {body}"
    );
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_projection_prefix_stability_reflects_recorded_observations() {
    // v13 fill-site projection: null before any provider call settled
    // (asserted in the idle-shape test), then the durable aggregate of
    // the recorded per-call prefix observations — a single text-only
    // turn records exactly one observation with per-row stability 1.0
    // (nothing preceded it), so the projected aggregate must reflect
    // that recorded value: observations 1, mean 1.0, stdDev 0.0.
    let dir = tempfile::tempdir().unwrap();
    let mut registry = faktor_provider::ProviderRegistry::new();
    registry
        .try_register(Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![
                faktor_provider::ScriptedResponse::Text("pong".into()),
                faktor_provider::ScriptedResponse::End,
            ],
        )))
        .unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let permissions = ChannelPermissionRequester::new(Duration::from_secs(5));
    let agent = AgentRuntime::new(faktor_agent::AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: permissions.clone(),
        evidence: Arc::new(faktor_agent::NoEvidence),
        tools: Arc::new(faktor_agent::ToolRegistry::new()),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test server agent.".into(),
        hooks: None,
        instructions_resolver: faktor_instructions::no_roots_resolver(),
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        clock: Arc::new(faktor_core::time::SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    })
    .unwrap();
    let driver = agent.clone();
    let deps = ServerDeps::new(session.clone(), agent, permissions.clone()).unwrap();
    let token = deps.auth_token.clone();
    let ws = session.create_workspace("/tmp").unwrap();
    let created = session.create_session(ws, "t-prefix", "fake", "m").unwrap();
    let sid = created.id().to_string();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let projection = || async {
        client
            .get(format!("{base}/session/{sid}/projection"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    };

    driver.run_turn(created.id(), "hi", &[]).await.unwrap();

    // Wait for the terminal machine state, then read the projection.
    let mut body = serde_json::Value::Null;
    for _ in 0..200 {
        body = projection().await;
        if body["state"]["machine"] == "ready_for_next_turn" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(body["state"]["machine"], "ready_for_next_turn", "{body}");
    let ps = &body["prefixStability"];
    assert_eq!(ps["observations"], 1, "one settled provider call: {body}");
    assert_eq!(ps["mean"], 1.0, "first observation is stable by definition");
    assert_eq!(ps["stdDev"], 0.0);
    // The projection reflects the DURABLE recorded value: the store's
    // single observation row carries stability 1.0 (the aggregate mean
    // is computed over exactly that row).
    let rows = session
        .store()
        .provider_call_prefix_rows(SessionId::new(sid.as_str().parse::<u64>().unwrap()))
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one prefix observation row");
    assert!(rows[0].prompt_tokens > 0, "tokens recorded: {rows:?}");
    assert_ne!(rows[0].prompt_prefix_hash, [0u8; 32], "hash recorded");
    assert_eq!(rows[0].prefix_stability, Some(1.0));
    let agg = session
        .store()
        .session_stored_prefix_stability(SessionId::new(sid.as_str().parse::<u64>().unwrap()));
    assert_eq!(
        agg.unwrap().unwrap().mean,
        ps["mean"].as_f64().unwrap(),
        "projection must reflect the store aggregate"
    );
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_models_and_capabilities_serve_registered_models() {
    // GET /models and GET /capabilities must enumerate what the daemon
    // can ACTUALLY serve: an adapter registered with two configured
    // models appears in both surfaces with their real capabilities
    // (mirroring the provider/list introspection).
    let dir = tempfile::tempdir().unwrap();
    let mut caps = std::collections::HashMap::new();
    caps.insert(
        "gpt-x".to_string(),
        ModelCapabilities {
            context: 128_000,
            max_output: 16_384,
            tools: true,
            ..Default::default()
        },
    );
    caps.insert(
        "gpt-y".to_string(),
        ModelCapabilities {
            context: 64_000,
            max_output: 8_192,
            reasoning: true,
            ..Default::default()
        },
    );
    let openai = faktor_openai::OpenAiProvider::build(
        faktor_openai::OpenAiConfig {
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            family: faktor_openai::OpenAiFamily::Chat,
            models: caps,
        },
        permissive_transport(),
    );
    let deps = test_deps_with(dir.path(), vec![openai]);
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // /models: flat, deterministic, one entry per provider x model.
    let resp = client
        .get(format!("{base}/models"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let list: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(
        list.iter().any(|m| m["provider"] == "openai"
            && m["model"] == "gpt-x"
            && m["context"] == 128_000
            && m["maxOutput"] == 16_384
            && m["tools"] == true
            && m["source"] == "providerCatalog"),
        "gpt-x with real capabilities: {list:?}"
    );
    assert!(
        list.iter()
            .any(|m| m["provider"] == "openai" && m["model"] == "gpt-y" && m["reasoning"] == true),
        "gpt-y reasoning flag: {list:?}"
    );
    assert!(
        list.iter()
            .any(|m| m["provider"] == "fake" && m["model"] == "default"),
        "registered fake provider still catalogued: {list:?}"
    );
    // The advertised attachment contract rides every /models entry (the
    // ONE Rust source of truth): the OpenAI family is document-capable
    // with its image/document allowlists and the daemon's HTTP ceilings.
    let gpt_x = list
        .iter()
        .find(|m| m["provider"] == "openai" && m["model"] == "gpt-x")
        .expect("gpt-x entry present");
    assert_eq!(gpt_x["documentCapable"], true);
    let limits = &gpt_x["attachmentLimits"];
    assert_eq!(limits["document"]["capable"], true);
    assert_eq!(limits["document"]["mimes"][0]["mime"], "application/pdf");
    assert_eq!(limits["document"]["mimes"][1]["mime"], "text/plain");
    assert_eq!(limits["image"]["mimes"][0]["mime"], "image/png");
    assert_eq!(limits["maxUploadBytes"], MAX_ATTACHMENT_UPLOAD_BYTES as u64);
    assert_eq!(limits["maxRequestBytes"], MAX_BODY_BYTES as u64);
    assert!(
        limits["image"]["mimes"][0]["maxBytes"].as_u64().unwrap() > 0,
        "every advertised per-MIME bound is positive: {limits:?}"
    );
    // Deterministic ordering: sorted by provider then model.
    let keys: Vec<(&str, &str)> = list
        .iter()
        .map(|m| {
            (
                m["provider"].as_str().unwrap_or(""),
                m["model"].as_str().unwrap_or(""),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "catalog must be deterministically ordered");

    // /capabilities: map provider -> {models, runtimeContextLimitSupported}.
    let resp = client
        .get(format!("{base}/capabilities"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let openai_entry = body.get("openai").expect("openai provider key present");
    let models = openai_entry["models"].as_array().unwrap();
    assert!(
        models
            .iter()
            .any(|m| m["id"] == "gpt-x" && m["capabilities"]["context"] == 128_000),
        "gpt-x capabilities: {models:?}"
    );
    assert!(
        models
            .iter()
            .any(|m| m["id"] == "gpt-y" && m["capabilities"]["max_output"] == 8_192),
        "gpt-y capabilities: {models:?}"
    );
    assert!(
        models.iter().any(|m| m["id"] == "gpt-x"
            && m["documentCapable"] == true
            && m["attachmentLimits"]["document"]["capable"] == true),
        "gpt-x advertised attachment contract: {models:?}"
    );
    assert_eq!(openai_entry["runtimeContextLimitSupported"], false);
    let fake_entry = body.get("fake").expect("fake provider key present");
    assert_eq!(fake_entry["runtimeContextLimitSupported"], false);
    let _ = handle.request_shutdown();
}

// --------------------------------------------------- native v1: audits 55-56
// /native/health + /native/ready semantics, the durable session
// listings, the strict abort DTO and the cross-session usage aggregate.

#[tokio::test]
async fn native_turns_lists_a_driven_turn_and_hostile_ids_are_loud() {
    // Drive a REAL turn through the HTTP surface (FakeProvider pong),
    // then read it back from /native/session/{id}/turns as a completed
    // durable turn record with its envelope.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let driver = deps.agent.clone();
    let ws = deps.session.create_workspace("/tmp").unwrap();
    let created = deps
        .session
        .create_session(ws, "t-turns", "fake", "m")
        .unwrap();
    let sid = created.id().to_string();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let drive_id = created.id();
    tokio::spawn(async move {
        let _ = driver.run_turn(drive_id, "hi", &[]).await;
    });

    // Poll the native turns listing until the durable record lands.
    let mut body = serde_json::Value::Null;
    for _ in 0..200 {
        let resp = client
            .get(format!("{base}/native/session/{sid}/turns"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        body = resp.json().await.unwrap();
        let done = body
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["status"] == "completed");
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let turns = body.as_array().unwrap();
    let last = turns.first().expect("at least one completed turn");
    assert_eq!(last["status"], "completed");
    assert_eq!(last["provider"], "fake");
    assert_eq!(last["model"], "m");
    assert!(last["opId"].as_str().unwrap().parse::<u64>().is_ok());
    assert!(last["startedAt"].as_i64().unwrap_or(0) > 0);

    // Unauth 401; hostile ids: 0 and non-numeric → 400, unknown → 404.
    let resp = client
        .get(format!("{base}/native/session/{sid}/turns"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    for hostile in ["0", "abc", "184467440737095516150"] {
        let resp = client
            .get(format!("{base}/native/session/{hostile}/turns"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "hostile id {hostile}");
    }
    let resp = client
        .get(format!("{base}/native/session/999999/turns"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_checkpoints_reflect_written_rows_when_service_wired() {
    // With the real checkpoint service wired, recorded file changes
    // surface as checkpoint rows (newest first); without rows the
    // listing is empty but live.
    let dir = tempfile::tempdir().unwrap();
    let (deps, snapshots, _fs) = wire_snapshot_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/tmp").unwrap();
    let session = manager.create_session(ws, "t-cp", "fake", "m").unwrap();
    let sid = session.id().to_string();

    // Empty before any write.
    let resp = client
        .get(format!("{base}/native/session/{sid}/checkpoints"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );

    // Two real checkpoint rows, exactly like the edit engine records.
    let before = snapshots
        .before_write(session.id(), "notes.txt", b"original\n")
        .unwrap();
    let after = snapshots
        .before_write(session.id(), "notes.txt", b"edited\n")
        .unwrap();
    snapshots
        .after_write(session.id(), "notes.txt", before, after, 0, b"edited\n")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let before2 = snapshots
        .before_write(session.id(), "a.rs", b"one")
        .unwrap();
    let after2 = snapshots
        .before_write(session.id(), "a.rs", b"two")
        .unwrap();
    snapshots
        .after_write(session.id(), "a.rs", before2, after2, 0, b"two")
        .unwrap();

    let resp = client
        .get(format!("{base}/native/session/{sid}/checkpoints"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let rows: serde_json::Value = resp.json().await.unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // Newest first (higher sequence first).
    assert_eq!(rows[0]["path"], "a.rs");
    assert_eq!(rows[1]["path"], "notes.txt");
    assert_eq!(rows[1]["beforeHash"], before.to_hex());
    assert_eq!(rows[1]["afterHash"], after.to_hex());
    assert!(rows[0]["createdMs"].as_i64().unwrap_or(0) > 0);
    assert!(rows[0]["restoredMs"].is_null());
    let _ = handle.request_shutdown();
}

/// The canonical projection is ONE function: the JSON graph surface and
/// the agent/task-run aggregation derive byte-identical states for the
/// same registry rows, and a Blocked child's durable blocker truth rides
/// both surfaces (the old "non-terminal means Running" conversion is
/// gone).
#[tokio::test]
async fn native_graph_and_agents_share_the_canonical_child_projection() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (parent, _ca, _cb) = seed_orchestration_graph(&manager);
    // Rewrite child-1 (item a) into a Blocked child carrying durable
    // blocker truth.
    let parent_handle = manager.get_session(parent).unwrap().unwrap();
    let mut raw = None;
    let mut after = None;
    loop {
        let page = parent_handle
            .memory_facts_page(after.as_ref(), 200)
            .unwrap();
        for (kind, key, value) in &page.facts {
            if kind == ORCH_REGISTRY_KIND && key == "run-1/child-1" {
                raw = Some(value.clone());
            }
        }
        match page.cursor {
            Some(c) => after = Some(c),
            None => break,
        }
    }
    let mut row: faktor_orchestrator::runtime::ChildRuntime =
        serde_json::from_str(&raw.expect("child-1 registry row")).unwrap();
    row.set_blocker(&faktor_orchestrator::runtime::ChildBlocker {
        kind: faktor_orchestrator::runtime::BlockerKind::Permission,
        reason: "waiting for a pending permission decision".into(),
        dependency: None,
        resolution: Some("resolve the pending permission request".into()),
        last_progress_ms: Some(42),
    })
    .unwrap();
    parent_handle
        .upsert_memory_fact(
            ORCH_REGISTRY_KIND,
            "run-1/child-1",
            &serde_json::to_string(&row).unwrap(),
        )
        .unwrap();
    // Surface 1: the JSON operation graph.
    let g: serde_json::Value = client
        .get(format!("{base}/native/orchestrator/graph?session={parent}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(g["work_items"][1]["state"], "Blocked");
    assert_eq!(g["state"], "Blocked");
    let graph_child = g["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["child_id"] == "child-1")
        .expect("child-1 graph node");
    assert_eq!(graph_child["state"], "Blocked");
    assert_eq!(graph_child["blocker"]["kind"], "permission");
    assert_eq!(graph_child["blocker"]["last_progress_ms"], 42);
    // Surface 2: the agent listing (the task-run projection delegates
    // to the SAME body).
    let entries: serde_json::Value = client
        .get(format!("{base}/native/agents?session={parent}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let self_entry = entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "self")
        .expect("self entry");
    let child = entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["agent_id"] == "child-1")
        .expect("child entry");
    assert_eq!(
        self_entry["state"], g["state"],
        "root state must be byte-identical across surfaces"
    );
    assert_eq!(
        child["state"], graph_child["state"],
        "child state must be byte-identical across surfaces"
    );
    assert_eq!(child["blocker"]["kind"], "permission");
    assert_eq!(child["blocker"]["last_progress_ms"], 42);
    let runs: serde_json::Value = client
        .get(format!("{base}/native/session/{parent}/task-runs"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run = runs
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == "run-1")
        .expect("task run entry");
    assert_eq!(
        run["state"], g["state"],
        "task-run state must use the same projection"
    );
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_messages_cursor_paging_no_dup_gap_and_isolation() {
    // P0-64a: cursor pages over the durable message rows. A 100-row
    // fixture pages with hasMore/nextBefore semantics — every row
    // appears exactly once (no duplicate, no gap); hostile session ids,
    // oversized limits and unknown query fields are rejected; session B
    // never sees A's rows.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/msg-root").unwrap();
    let a = manager.create_session(ws, "t-msg-a", "fake", "m").unwrap();
    let b = manager.create_session(ws, "t-msg-b", "fake", "m").unwrap();
    let a_sid = a.id().to_string();
    let b_sid = b.id().to_string();
    let ha = manager.get_session(a.id()).unwrap().unwrap();
    let hb = manager.get_session(b.id()).unwrap().unwrap();
    // 100 durable rows under A (seq 1..=100), one with a text part.
    for seq in 1..=100i64 {
        let mid = ha
            .put_message(
                seq,
                if seq % 2 == 0 { "assistant" } else { "user" },
                serde_json::json!({"text": format!("m{seq}")}),
            )
            .unwrap();
        if seq == 50 {
            ha.put_text_part(mid, "part-of-50").unwrap();
        }
    }
    for seq in 1..=3i64 {
        hb.put_message(seq, "user", serde_json::json!({"text": format!("b{seq}")}))
            .unwrap();
    }

    // Page across the whole 100-row fixture.
    let mut seen: Vec<i64> = Vec::new();
    let mut before: Option<i64> = None;
    let mut pages = 0;
    loop {
        let cursor = before.map(|b| format!("&before={b}")).unwrap_or_default();
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/messages?session={a_sid}&limit=30{cursor}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["sessionId"], a_sid);
        let msgs = body["messages"].as_array().unwrap();
        pages += 1;
        assert!(!msgs.is_empty());
        assert!(msgs.len() <= 30);
        for m in msgs {
            let seq = m["seq"].as_i64().unwrap();
            assert!(seen.last().map(|s| seq < *s).unwrap_or(true), "descending");
            assert!(seen.iter().all(|s| *s != seq), "no duplicate {seq}");
            seen.push(seq);
            assert!(m["role"].is_string());
            assert!(m["createdMs"].as_i64().unwrap_or(0) > 0);
            assert_eq!(m["data"]["text"], format!("m{seq}"));
        }
        let has_more = body["hasMore"].as_bool().unwrap();
        before = body["nextBefore"].as_i64();
        if !has_more {
            assert!(before.is_none());
            break;
        }
        assert!(before.is_some(), "next page cursor present");
        assert!(pages < 10, "paging must terminate");
    }
    assert_eq!(seen.len(), 100, "every row exactly once");
    assert_eq!(*seen.first().unwrap(), 100);
    assert_eq!(*seen.last().unwrap(), 1);

    // The part row came through with its part.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/messages?session={a_sid}&limit=200&before=51"),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    let msgs = body["messages"].as_array().unwrap();
    let m50 = msgs.iter().find(|m| m["seq"] == 50).unwrap();
    assert_eq!(m50["parts"][0]["kind"], "text");
    assert_eq!(m50["parts"][0]["data"]["text"], "part-of-50");

    // Isolation: B's pages contain only B's rows.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/messages?session={b_sid}&limit=10"),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert!(msgs
        .iter()
        .all(|m| m["data"]["text"].as_str().unwrap().starts_with('b')));

    // Hostile ids, oversized limits, strict DTO, auth.
    for path in [
        "/native/messages?session=0".to_string(),
        "/native/messages?session=abc".to_string(),
        "/native/messages?session=1&limit=0".to_string(),
        "/native/messages?session=1&limit=201".to_string(),
        "/native/messages?session=1&before=0".to_string(),
        "/native/messages?session=1&before=-3".to_string(),
        "/native/messages?session=1&limt=5".to_string(),
    ] {
        let resp = native_get(&client, &base, &token, &path).await;
        assert_eq!(resp.status(), 400, "{path}");
    }
    let resp = native_get(&client, &base, &token, "/native/messages?session=999999").await;
    assert_eq!(resp.status(), 404);
    let resp = client
        .get(format!("{base}/native/messages?session={a_sid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_events_journal_page_no_dup_gap_and_bounds() {
    // P0-64b: the native twin of the journal stream pages the durable
    // event rows with seq > after ascending; a 300-event fixture pages
    // without a duplicate or a gap.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/ev-root").unwrap();
    let a = manager.create_session(ws, "t-ev-a", "fake", "m").unwrap();
    let b = manager.create_session(ws, "t-ev-b", "fake", "m").unwrap();
    let a_sid = a.id().to_string();
    let b_sid = b.id().to_string();
    for _ in 0..300 {
        a.force_append_event(
            faktor_core::event::EventKind::PhaseChanged,
            faktor_core::state::AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
    }
    // Session B gets a small independent journal.
    b.force_append_event(
        faktor_core::event::EventKind::PhaseChanged,
        faktor_core::state::AgentState::WaitingForModel,
        None,
        None,
    )
    .unwrap();

    let mut all: Vec<u64> = Vec::new();
    let mut after: u64 = 0;
    let mut pages = 0;
    loop {
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/events?session={a_sid}&after={after}&limit=100"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["sessionId"], a_sid);
        let events = body["events"].as_array().unwrap();
        pages += 1;
        assert!(!events.is_empty());
        assert!(events.len() <= 100);
        for e in events {
            let seq = e["seq"].as_u64().unwrap();
            assert!(all.last().map(|s| seq > *s).unwrap_or(true), "ascending");
            assert!(all.iter().all(|s| *s != seq), "no duplicate {seq}");
            all.push(seq);
            if seq == 1 {
                assert_eq!(e["kind"], "session_created");
                assert_eq!(e["state"], "idle");
            } else {
                assert_eq!(e["kind"], "phase_changed");
                assert_eq!(e["state"], "waiting_for_model");
            }
            assert!(e["opId"].is_null());
            assert!(e["tsMs"].as_i64().unwrap_or(0) > 0);
        }
        let has_more = body["hasMore"].as_bool().unwrap();
        if has_more {
            after = body["nextCursor"].as_u64().unwrap();
        } else {
            assert!(body["nextCursor"].is_null());
            break;
        }
        assert!(pages < 10, "paging must terminate");
    }
    assert_eq!(all.len(), 301, "session_created + 300 forced events");
    assert_eq!(all[0], 1);
    assert_eq!(*all.last().unwrap(), 301);
    assert!(
        all.windows(2).all(|w| w[1] == w[0] + 1),
        "gapless journal paging"
    );

    // Isolation: B's journal pages only its own events.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/events?session={b_sid}&limit=10"),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "{body}");

    // Bounds, hostile ids, unknown query fields, auth.
    for path in [
        "/native/events?session=0".to_string(),
        "/native/events?session=abc".to_string(),
        "/native/events?session=1&limit=0".to_string(),
        "/native/events?session=1&limit=257".to_string(),
        "/native/events?session=1&aftr=3".to_string(),
    ] {
        let resp = native_get(&client, &base, &token, &path).await;
        assert_eq!(resp.status(), 400, "{path}");
    }
    let resp = native_get(&client, &base, &token, "/native/events?session=999999").await;
    assert_eq!(resp.status(), 404);
    let resp = client
        .get(format!("{base}/native/events?session={a_sid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_child_payload_projects_the_durable_execution_phase() {
    // The coarse durable drive phase is an additive projection: it rides
    // both native child payloads (the agents listing and the graph) and
    // is read from the child's durable drive-state row — lifecycle
    // untouched.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let orch = deps.orchestrator.clone();
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let (parent, owner, isolated) = orch_owner_env(&manager, dir.path());
    let config = faktor_orchestrator::runtime::ExecConfig {
        run_id: "run-phase".into(),
        ceilings: faktor_orchestrator::runtime::Ceilings::default(),
        parent_caps: read_workspace_caps(),
        provider: "fake".into(),
        default_model: "m".into(),
        isolated_root: isolated.clone(),
        crash_seam: Some(faktor_orchestrator::runtime::CrashSeam::BeforeDrive),
    };
    let res = orch
        .execute_task(
            analysis_plan(&["a"]),
            owner,
            config,
            &[read_child_spec("a")],
        )
        .await
        .expect_err("the seam must fire");
    assert!(
        matches!(
            res,
            faktor_orchestrator::runtime::ExecError::InjectedCrashSeam(_)
        ),
        "{res:?}"
    );
    let child = orch.child("child-0").unwrap().unwrap();
    let child_session = manager
        .get_session(SessionId::new(child.session_id))
        .unwrap()
        .unwrap();
    let state_before = child_session.state().unwrap();
    child_session
        .set_execution_phase(faktor_core::blocker::ExecutionPhase::Coding)
        .unwrap();
    assert_eq!(
        child_session.state().unwrap(),
        state_before,
        "phase writes never move lifecycle"
    );
    // Agents listing.
    let agents = get_agents(
        &base,
        token.as_str(),
        &format!("/native/agents?session={parent}"),
    )
    .await;
    let entry = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["agent_id"] == "child-0")
        .expect("child listed");
    assert_eq!(entry["execution_phase"], "coding");
    // Graph payload carries the same derived phase.
    let resp = reqwest::Client::new()
        .get(format!("{base}/native/orchestrator/graph?session={parent}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let graph: serde_json::Value = resp.json().await.unwrap();
    let graph_child = graph["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["child_id"] == "child-0")
        .expect("graph child listed");
    assert_eq!(graph_child["execution_phase"], "coding");
    let _ = handle.request_shutdown();
}

// ------------------------------------------------ split invariants
