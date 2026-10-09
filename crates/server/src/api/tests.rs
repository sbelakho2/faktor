//! `api::tests`: shared fixtures for the api test modules.

pub(crate) use super::*;

pub(crate) use crate::native::terminal_authority::TerminalService;

pub(crate) use faktor_core::capability::PermissionDecision;

pub(crate) use faktor_core::id::{SessionId, WorkspaceId};

pub(crate) use faktor_core::model::ModelCapabilities;

pub(crate) use faktor_evidence::store::EvidenceStore as _;

pub(crate) use faktor_provider::FakeProvider;

pub(crate) use faktor_session::BudgetAuthority;

pub(crate) use std::time::Duration;

/// Test-only default-allow transport: every mock endpoint below is a
/// loopback server, and production construction injects the daemon's
/// policy-checked transport instead.
pub(crate) fn permissive_transport() -> Arc<dyn faktor_provider::egress::HttpTransport> {
    Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::permissive())
}

// ------------------------------------------------- poisoned authorities

/// The runtime + executor pair every test `ServerDeps` carries (the
/// real orchestrator over the test store — the endpoints under test
/// drive exactly what production drives).
pub(crate) fn orch_pair(
    session: Arc<SessionManager>,
    agent: Arc<AgentRuntime>,
) -> (
    Arc<faktor_orchestrator::runtime::OrchestratorRuntime>,
    Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
) {
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
    let tasks =
        faktor_orchestrator::runtime::task_executor::TaskExecutor::new_owner_direct_for_test_harness(
            &orchestrator,
            session,
            agent,
        );
    (orchestrator, tasks)
}

/// A daemon whose wire snapshot surface is wired to the real native
/// store: same store + CAS the session manager opened, plus a file
/// service. Returns the deps, the checkpoint store used to record edits,
/// and the file service.
pub(crate) fn wire_snapshot_deps(
    root: &std::path::Path,
) -> (
    ServerDeps,
    Arc<faktor_snapshot::CheckpointStore>,
    Arc<faktor_fs::WorkspaceFileService>,
) {
    let deps = test_deps(root);
    let fs = faktor_fs::WorkspaceFileService::new();
    let snapshots = Arc::new(faktor_snapshot::CheckpointStore::new(
        deps.session.cas(),
        deps.session.store(),
    ));
    let deps = deps.with_snapshots(fs.clone(), snapshots.clone());
    (deps, snapshots, fs)
}

/// The minimal real agent every test `ServerDeps` carries: text-only
/// turns, passthrough routing over the caller's registry. `hooks` is wired
/// straight into the runtime's agent deps (the production daemon wires the
/// `FAKTOR_HOOKS` registry there too).
pub(crate) fn test_agent_over(
    session: Arc<SessionManager>,
    permissions: Arc<ChannelPermissionRequester>,
    registry: faktor_provider::ProviderRegistry,
    hooks: Option<Arc<faktor_hooks::HookRegistry>>,
) -> Arc<AgentRuntime> {
    AgentRuntime::new(faktor_agent::AgentDeps {
        session,
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: permissions,
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
        hooks,
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
    .unwrap()
}

/// The EXPLICIT user-granted shell contract the test deps inject into
/// the terminal authority: tests that exercise real session-owned PTY
/// spawns must grant it exactly like an operator would — the authority
/// default is the fail-closed `os_isolated` shape.
pub(crate) fn granted_terminal_policy() -> TerminalAuthorityPolicy {
    TerminalAuthorityPolicy::for_configured_sandbox(&faktor_sandbox::SandboxPolicy {
        execute_shell: faktor_sandbox::Rule::Allow,
        network_guarantee: faktor_sandbox::SandboxGuarantee::None,
        shell_execution: faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted,
        ..faktor_sandbox::SandboxPolicy::default()
    })
}

pub(crate) fn test_deps(root: &std::path::Path) -> ServerDeps {
    test_deps_with(root, vec![])
}

pub(crate) fn test_deps_with(
    root: &std::path::Path,
    extra_providers: Vec<Arc<dyn faktor_provider::Provider>>,
) -> ServerDeps {
    test_deps_with_hooks(root, extra_providers, None)
}

/// [`test_deps_with`] with an explicit lifecycle-hook registry on the
/// agent runtime (the SAME field the production daemon's `FAKTOR_HOOKS`
/// registry occupies).
pub(crate) fn test_deps_with_hooks(
    root: &std::path::Path,
    extra_providers: Vec<Arc<dyn faktor_provider::Provider>>,
    hooks: Option<Arc<faktor_hooks::HookRegistry>>,
) -> ServerDeps {
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
    for p in extra_providers {
        registry.try_register(p).unwrap();
    }
    let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let permissions = ChannelPermissionRequester::new(Duration::from_secs(5));
    let agent = test_agent_over(session.clone(), permissions.clone(), registry, hooks);
    let (orchestrator, tasks) = orch_pair(session.clone(), agent.clone());
    ServerDeps {
        budgets: faktor_session::DurableBudgetLedger::new(session.clone()),
        session,
        agent,
        permissions,
        orchestrator,
        tasks,
        auth_token: AuthToken::generate(),
        server_password: ServerPassword::generate(),
        directory: None,
        version: "0.1.0".into(),
        fs: None,
        snapshots: None,
        chunk_rx: None,
        simulate_not_ready: false,
        evidence: None,
        semantic: None,
        control_plane: None,
        scm: None,
        scm_webhook: None,
        billing: None,
        updater: None,
        workers: None,
        enterprise: None,
        retention: None,
        sso: None,
        worker_plane_listener: None,
        terminal_policy: granted_terminal_policy(),
    }
}

/// Removes the CWD-relative store files an empty-root fixture creates
/// (on Unix open handles do not block the unlink; any Windows leftover is
/// gitignored debris, never a gate failure).
pub(crate) struct CwdStoreFileGuard;

impl Drop for CwdStoreFileGuard {
    fn drop(&mut self) {
        for name in ["faktor-plus.db", "faktor-plus.db-wal", "faktor-plus.db-shm"] {
            let _ = std::fs::remove_file(name);
        }
    }
}

/// Serve one deps envelope's native listener and read `/native/health`
/// back (raw text + parsed JSON), then signal the listener to stop.
pub(crate) async fn health_json(
    deps: Arc<ServerDeps>,
    token: &AuthToken,
) -> (serde_json::Value, String) {
    let native = serve_arc(deps, 0).await.unwrap();
    let base = format!("http://{}", native.addr);
    let raw = reqwest::Client::new()
        .get(format!("{base}/native/health"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let _ = native.request_shutdown();
    (serde_json::from_str(&raw).unwrap(), raw)
}

/// Bounded wait for the owned native-server task to end; a task that
/// never ends fails the test instead of hanging it.
pub(crate) async fn wait_until_server_dead(handle: &ServerHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while handle.is_alive() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the native serve task must terminate within the bound"
        );
        tokio::task::yield_now().await;
    }
}

/// Bounded wait for `addr` to be rebindable. The shared one-shot signal
/// is owned by the task's shared state, so a DETACHED task would keep the
/// socket bound forever and fail this test.
pub(crate) async fn wait_until_rebindable(addr: std::net::SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
            drop(listener);
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the listener socket {addr} was never released (detached task?)"
        );
        tokio::task::yield_now().await;
    }
}

/// Seed one parent session with a wave-12/13-shaped durable run: a
/// plan row (items b then a), two registry children (b is Done with a
/// staged merge; a is Running), one child with steering rows. Returns
/// the parent session id and the child session ids.
pub(crate) fn seed_orchestration_graph(
    manager: &Arc<SessionManager>,
) -> (SessionId, SessionId, SessionId) {
    let ws = manager.create_workspace("/orch-root").unwrap();
    let parent = manager.create_session(ws, "orch", "fake", "m").unwrap();
    let child_a = manager.create_session(ws, "child-a", "fake", "m").unwrap();
    let child_b = manager.create_session(ws, "child-b", "fake", "m").unwrap();
    let now = 1_700_000_000_000i64;
    let plan_row = serde_json::json!({
        "plan": {
            "goal": "Ship the graph",
            "non_goals": [],
            "constraints": [],
            "work_items": [
                {"id": "b", "summary": "work b", "depends_on": [], "kind": "Analysis",
                 "acceptance_checks": [], "completion": "Pending"},
                {"id": "a", "summary": "work a", "depends_on": ["b"], "kind": "Analysis",
                 "acceptance_checks": [], "completion": "Pending"},
            ]
        },
        "owner_ws": ws.raw(),
        "owner_wt": 1,
        "owner_root": "/orch-root",
        "specs": [],
        "provider": "fake",
        "default_model": "m",
        "isolated_root": "/iso",
        "created_ms": now,
    });
    parent
        .upsert_memory_fact(ORCH_PLAN_KIND, "run-1", &plan_row.to_string())
        .unwrap();
    let registry = |child_id: &str, item: &str, sid: u64, state: &str, ms: i64| {
        serde_json::json!({
            "child_id": child_id,
            "parent_session_id": parent.id().raw(),
            "run_id": "run-1",
            "item_id": item,
            "kind": "Analysis",
            "session_id": sid,
            "operation_id": 0,
            "workspace_id": ws.raw(),
            "worktree_id": sid,
            "ownership": "read_only_shared",
            "ownership_paths": [],
            "state": state,
            "budget_max_tokens": 1000,
            "permissions": [],
            "model_policy": {"model": null},
            "created_ms": ms,
            "updated_ms": ms,
        })
    };
    parent
        .upsert_memory_fact(
            ORCH_REGISTRY_KIND,
            "run-1/child-0",
            &registry("child-0", "b", child_b.id().raw(), "Done", now).to_string(),
        )
        .unwrap();
    parent
        .upsert_memory_fact(
            ORCH_REGISTRY_KIND,
            "run-1/child-1",
            &registry("child-1", "a", child_a.id().raw(), "Running", now + 10).to_string(),
        )
        .unwrap();
    // Steering history on the a-child: one applied Pause, one pending
    // Steer — seq order matters.
    let pause = child_a
        .orchestrator_ctl_enqueue(faktor_session::child::ChildControl::Pause)
        .unwrap();
    child_a
        .orchestrator_ctl_enqueue(faktor_session::child::ChildControl::Steer {
            note: "focus the api".into(),
        })
        .unwrap();
    child_a.orchestrator_ctl_ack(pause.seq).unwrap();
    // A durable merge record + parts for the Done b-child.
    let cs = "base-child-0-cs";
    let envelope = serde_json::json!({
        "seq": 1, "child_id": "child-0", "cs_id": cs,
        "status": "applied", "approved_count": 2, "rejected_count": 0,
        "merged_count": 1, "conflict_count": 1,
        "created_ms": now, "finished_ms": now + 5, "details": "x",
    });
    parent
        .upsert_memory_fact(
            ORCH_MERGE_KIND,
            &format!("run-1/child-0/merge/{cs}/1"),
            &envelope.to_string(),
        )
        .unwrap();
    for (part, value) in [
        ("merged", serde_json::json!(["keep.rs"])),
        ("rejected", serde_json::json!([])),
        (
            "conflicts",
            serde_json::json!([["src/a.rs", "parent moved past the base snapshot"]]),
        ),
    ] {
        let key = format!("run-1/child-0/merge/{cs}/1/part/{part}");
        parent
            .upsert_memory_fact(ORCH_MERGE_PART_KIND, &key, "{\"chunks\":1}")
            .unwrap();
        parent
            .upsert_memory_fact(
                ORCH_MERGE_PART_KIND,
                &format!("{key}/c001"),
                &value.to_string(),
            )
            .unwrap();
    }
    (parent.id(), child_a.id(), child_b.id())
}

/// Chunk-paced per-call provider (real-drive control windows).
pub(crate) struct PacedScriptedProvider {
    pub(crate) inner: FakeProvider,
    pub(crate) calls: std::sync::Mutex<Vec<Vec<faktor_provider::ScriptedResponse>>>,
    pub(crate) script_index: std::sync::atomic::AtomicUsize,
    pub(crate) request_count: std::sync::atomic::AtomicUsize,
    pub(crate) chunk_delay_ms: u64,
}

impl PacedScriptedProvider {
    pub(crate) fn new(
        caps: ModelCapabilities,
        per_call_scripts: Vec<Vec<faktor_provider::ScriptedResponse>>,
        chunk_delay_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: FakeProvider::new("fake", caps),
            calls: std::sync::Mutex::new(per_call_scripts),
            script_index: std::sync::atomic::AtomicUsize::new(0),
            request_count: std::sync::atomic::AtomicUsize::new(0),
            chunk_delay_ms,
        })
    }
}

impl faktor_provider::Provider for PacedScriptedProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }
    fn stream(
        &self,
        _req: faktor_provider::GenericAgentRequest,
    ) -> faktor_provider::ProviderStream {
        use futures_util::StreamExt;
        self.request_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let i = self
            .script_index
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let script: Vec<faktor_provider::ScriptedResponse> = self
            .calls
            .lock()
            .unwrap()
            .get(i)
            .cloned()
            .unwrap_or_else(|| vec![faktor_provider::ScriptedResponse::End]);
        let delay = self.chunk_delay_ms;
        let stream = futures_util::stream::iter(script).then(move |s| async move {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            match s {
                faktor_provider::ScriptedResponse::Text(t) => {
                    Ok(faktor_provider::ProviderChunk::Text { text: t })
                }
                faktor_provider::ScriptedResponse::ToolCall { id, name, input } => {
                    Ok(faktor_provider::ProviderChunk::ToolCall {
                        id,
                        name,
                        input,
                        complete: true,
                    })
                }
                faktor_provider::ScriptedResponse::Die(e) => Err(e),
                faktor_provider::ScriptedResponse::End => Ok(faktor_provider::ProviderChunk::Done),
                faktor_provider::ScriptedResponse::Reasoning(_) => unreachable!(),
            }
        });
        Box::pin(stream)
    }
}

pub(crate) struct AllowAll;

impl faktor_agent::PermissionRequester for AllowAll {
    fn request(
        &self,
        _session: SessionId,
        _permission: &faktor_session::ops::PermissionRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
    > {
        Box::pin(async { Ok(PermissionDecision::Allow) })
    }
}

/// The echo tool the paced roundtrips call (a second reasoning
/// iteration gives the control boundary a real window).
pub(crate) fn echo_tool() -> faktor_agent::Tool {
    faktor_agent::Tool {
        name: "echo".into(),
        description: "echo its input".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: faktor_agent::RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(
            |_ctx: faktor_agent::ToolRunCtx, _input: serde_json::Value| {
                Box::pin(async move { Ok(faktor_agent::ToolOutcome::default()) })
            },
        ),
    }
}

/// Server deps whose ONLY provider is the paced scripted one (a
/// deterministic control window for real orchestrated drives).
pub(crate) fn paced_test_deps(
    root: &std::path::Path,
    paced: Arc<PacedScriptedProvider>,
) -> ServerDeps {
    let mut registry = faktor_provider::ProviderRegistry::new();
    registry.try_register(paced).unwrap();
    let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let permissions = ChannelPermissionRequester::new(Duration::from_secs(5));
    let mut tools = faktor_agent::ToolRegistry::new();
    tools.register(echo_tool());
    let agent = AgentRuntime::new(faktor_agent::AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AllowAll),
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
    let (orchestrator, tasks) = orch_pair(session.clone(), agent.clone());
    ServerDeps {
        budgets: faktor_session::DurableBudgetLedger::new(session.clone()),
        session,
        agent,
        permissions,
        orchestrator,
        tasks,
        auth_token: AuthToken::generate(),
        server_password: ServerPassword::generate(),
        directory: None,
        version: "0.1.0".into(),
        fs: None,
        snapshots: None,
        chunk_rx: None,
        simulate_not_ready: false,
        evidence: None,
        semantic: None,
        control_plane: None,
        scm: None,
        scm_webhook: None,
        billing: None,
        updater: None,
        workers: None,
        enterprise: None,
        retention: None,
        sso: None,
        worker_plane_listener: None,
        terminal_policy: granted_terminal_policy(),
    }
}

pub(crate) fn read_workspace_caps() -> faktor_orchestrator::caps::CapabilitySet {
    use faktor_orchestrator::caps::{CapabilityGrant, LatticeCap, ScopePattern};
    faktor_orchestrator::caps::CapabilitySet::from_grants(vec![CapabilityGrant::new(
        LatticeCap::ReadWorkspace,
        ScopePattern::new("*").unwrap(),
    )])
    .unwrap()
}

pub(crate) fn read_child_spec(item: &str) -> faktor_orchestrator::runtime::ChildSpec {
    let mut s = faktor_orchestrator::runtime::ChildSpec::new(item);
    s.child_caps = read_workspace_caps();
    s.task_caps = read_workspace_caps();
    s
}

/// A real owner session for an orchestrated run (registered worktree on
/// a real directory).
pub(crate) fn orch_owner_env(
    manager: &Arc<SessionManager>,
    root: &std::path::Path,
) -> (
    SessionId,
    faktor_orchestrator::runtime::OwnerContext,
    std::path::PathBuf,
) {
    use faktor_core::id::{TaskId, WorktreeId};
    let owner_dir = root.join("owner");
    std::fs::create_dir_all(&owner_dir).unwrap();
    let ws = manager
        .create_workspace(owner_dir.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        manager
            .put_worktree(ws, owner_dir.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = manager
        .create_session(ws, "orch-owner", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let isolated = root.join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    (
        parent,
        faktor_orchestrator::runtime::OwnerContext {
            parent_session: parent,
            workspace_id: ws.raw(),
            worktree_id: wt.raw(),
            root: owner_dir,
        },
        isolated,
    )
}

pub(crate) fn analysis_plan(items: &[&str]) -> faktor_orchestrator::TaskPlan {
    use faktor_orchestrator::{OwnershipSpec, WorkItem, WorkKind};
    faktor_orchestrator::TaskPlan {
        goal: "Ship the analysis".into(),
        non_goals: vec![],
        constraints: vec![],
        work_items: items
            .iter()
            .map(|id| WorkItem {
                id: id.to_string(),
                summary: format!("work {id}"),
                depends_on: vec![],
                kind: WorkKind::Analysis,
                ownership: OwnershipSpec::NoWrites,
                required_capabilities: faktor_orchestrator::caps::CapabilitySet::new(),
                acceptance_checks: vec![],
                completion: faktor_orchestrator::WorkState::Pending,
            })
            .collect(),
    }
}

pub(crate) async fn get_agents(base: &str, token: &str, path: &str) -> serde_json::Value {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    if resp.status() != 200 {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        panic!("{path} -> {status} body: {body}");
    }
    resp.json().await.unwrap()
}

pub(crate) async fn native_get(
    client: &reqwest::Client,
    base: &str,
    token: &AuthToken,
    path: &str,
) -> reqwest::Response {
    client
        .get(format!("{base}{path}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap()
}

/// Read one money field off the wire: monetary fields are decimal
/// strings (see `faktor_cloud::money`).
pub(crate) fn money(value: &serde_json::Value) -> u64 {
    value
        .as_str()
        .unwrap_or_else(|| panic!("money field is not a decimal string: {value}"))
        .parse()
        .expect("money field parses as u64")
}

/// Spawn a session-owned terminal through the native endpoint. Returns
/// `None` when the platform refuses PTY spawns (documented skip).
pub(crate) async fn native_spawn_terminal(
    client: &reqwest::Client,
    base: &str,
    token: &AuthToken,
    sid: &str,
    command: &str,
    args: &[&str],
) -> Option<serde_json::Value> {
    let resp = client
        .post(format!("{base}/native/session/{sid}/terminal"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({ "command": command, "args": args }))
        .send()
        .await
        .unwrap();
    if resp.status() != 200 {
        assert_eq!(resp.status(), 400, "platform refusal is a 400");
        return None;
    }
    Some(resp.json().await.unwrap())
}

/// Kill one durable session-owned terminal through the native control
/// seam; returns the status (200 on the first kill, 409 once terminal).
pub(crate) async fn native_kill_terminal(
    client: &reqwest::Client,
    base: &str,
    token: &AuthToken,
    sid: &str,
    terminal_id: &str,
) -> u16 {
    client
        .post(format!(
            "{base}/native/session/{sid}/terminals/{terminal_id}/kill"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Create a typed task row of a session (task machine semantics: only
/// creation; revision starts at 1).
pub(crate) fn seed_typed_task(
    handle: &faktor_session::SessionHandle,
    task_id: u64,
    max_tokens: Option<u64>,
    max_turns: Option<u32>,
    goal: &str,
) {
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id: faktor_core::id::TaskId::new(task_id),
            session_id: handle.id(),
            goal: goal.into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget {
                max_tokens,
                max_turns,
                spent_tokens: 0,
                spent_turns: 0,
            },
            state: faktor_core::state::TaskState::Running,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
}

/// Provider for the real-turn usage test (P0-63): a canonical usage
/// frame with zero uncached input, only cache reads/writes + output
/// (anthropic-style split semantics) plus a provider-reported USD cost.
/// The runtime settlement prices each line and folds the categories
/// into the recorded input/output totals.
#[derive(Clone)]
pub(crate) struct CacheUsageProvider;

impl faktor_provider::Provider for CacheUsageProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            ..Default::default()
        }
    }

    fn stream(
        &self,
        _req: faktor_provider::GenericAgentRequest,
    ) -> faktor_provider::ProviderStream {
        let items: Vec<Result<faktor_provider::ProviderChunk, faktor_provider::ProviderError>> = vec![
            Ok(faktor_provider::ProviderChunk::Text {
                text: "pong".into(),
            }),
            Ok(faktor_provider::ProviderChunk::Usage(
                faktor_provider::CanonicalUsage {
                    uncached_input_tokens: 0,
                    cache_read_tokens: 7,
                    cache_write_tokens: 2,
                    output_tokens: 3,
                    reasoning_tokens: 0,
                    reported_cost: Some(faktor_provider::ReportedCost {
                        micro_usd: 123,
                        currency: faktor_provider::ReportedCurrency::Usd,
                        source: faktor_provider::ReportedCostSource::ProviderUsage,
                        request_id: Some("req-cache-1".into()),
                    }),
                    request_id: Some("req-cache-1".into()),
                },
            )),
            Ok(faktor_provider::ProviderChunk::Done),
        ];
        Box::pin(futures_util::stream::iter(items))
    }
}

/// The full test deps builder with an explicit provider registry and a
/// REAL durable cost ledger wired over the same session manager (the
/// usage E2E test drives reservations through the actual runtime).
pub(crate) fn test_deps_full(
    root: &std::path::Path,
    providers: Vec<Arc<dyn faktor_provider::Provider>>,
) -> ServerDeps {
    let mut registry = faktor_provider::ProviderRegistry::new();
    for p in providers {
        registry.try_register(p).unwrap();
    }
    let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let ledger = faktor_session::DurableBudgetLedger::new(session.clone());
    let budgets: Arc<dyn faktor_session::BudgetAuthority> = ledger.clone();
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
        budgets,
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
    let (orchestrator, tasks) = orch_pair(session.clone(), agent.clone());
    ServerDeps {
        budgets: faktor_session::DurableBudgetLedger::new(session.clone()),
        session,
        agent,
        permissions,
        orchestrator,
        tasks,
        auth_token: AuthToken::generate(),
        server_password: ServerPassword::generate(),
        directory: None,
        version: "0.1.0".into(),
        fs: None,
        snapshots: None,
        chunk_rx: None,
        simulate_not_ready: false,
        evidence: None,
        semantic: None,
        control_plane: None,
        scm: None,
        scm_webhook: None,
        billing: None,
        updater: None,
        workers: None,
        enterprise: None,
        retention: None,
        sso: None,
        worker_plane_listener: None,
        terminal_policy: granted_terminal_policy(),
    }
}

/// Session-scoped workspace root provider mirroring the daemon graph:
/// the live shadow of a shadowed workspace re-points instruction
/// loading at the shadow root.
pub(crate) struct NativeRealRoots(pub(crate) Arc<SessionManager>);

impl faktor_instructions::WorkspaceRootProvider for NativeRealRoots {
    fn workspace_root(
        &self,
        workspace_id: u64,
    ) -> Result<Option<std::path::PathBuf>, faktor_core::Error> {
        use faktor_core::id::WorkspaceId;
        if workspace_id == 0 {
            return Ok(None);
        }
        let ws = WorkspaceId::new(workspace_id);
        match self.0.live_workspace_shadow_root(ws) {
            Ok(Some(root)) => Ok(Some(root)),
            Ok(None) => self.0.workspace_root(ws),
            Err(e) => Err(e),
        }
    }
}

/// A REAL write_file: writes through the session's resolved workspace
/// root (the shadow root while a shadowed drive is live). `park` parks
/// the FIRST invocation after the write landed (the deterministic
/// mid-drive window of the shadowed HTTP tests).
pub(crate) fn native_real_write_tool(
    park: Option<(
        Arc<tokio::sync::Notify>,
        Arc<std::sync::atomic::AtomicUsize>,
    )>,
) -> faktor_agent::Tool {
    use faktor_agent::tool::RecoveryHint;
    use faktor_agent::{ToolOutcome, ToolRunCtx};
    use faktor_core::resource::ResourceClass;
    let gate = park.as_ref().map(|(g, _)| g.clone());
    let fired = park.as_ref().map(|(_, f)| f.clone());
    faktor_agent::Tool {
        name: "write_file".into(),
        description: "writes a real file".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(move |ctx: ToolRunCtx, args| {
            let gate = gate.clone();
            let fired = fired.clone();
            Box::pin(async move {
                let Some(ws) = &ctx.workspace else {
                    return Err(faktor_core::error::Error::internal("no workspace wired"));
                };
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                ws.write_atomic(std::path::Path::new(path), content.as_bytes())
                    .map_err(|e| {
                        faktor_core::error::Error::internal(format!("write {path}: {e}"))
                    })?;
                if let (Some(g), Some(f)) = (gate, fired) {
                    if f.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        g.notified().await;
                    }
                }
                Ok(ToolOutcome {
                    text: format!("wrote {path}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// A CPU tool whose FIRST invocation parks the drive mid-flight (the
/// deterministic cancel window).
pub(crate) fn native_parking_tool(
    gate: Arc<tokio::sync::Notify>,
    fired: Arc<std::sync::atomic::AtomicUsize>,
) -> faktor_agent::Tool {
    use faktor_agent::tool::RecoveryHint;
    use faktor_agent::ToolOutcome;
    use faktor_core::resource::ResourceClass;
    faktor_agent::Tool {
        name: "pause".into(),
        description: "parks once".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx, _args| {
            let gate = gate.clone();
            let fired = fired.clone();
            Box::pin(async move {
                if fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    gate.notified().await;
                }
                Ok(ToolOutcome {
                    text: "parked".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// A shadowed (or direct) native rig: the real agent with a REAL write
/// tool + a real workspace, the executor carrying the ShadowRoots
/// service per `service`, and the executor's default mutation mode per
/// `mode`. `scripts` serve the drive's model calls.
pub(crate) struct NativeTaskRig {
    pub(crate) deps: ServerDeps,
    pub(crate) manager: Arc<SessionManager>,
    pub(crate) parent: SessionId,
    pub(crate) owner_root: std::path::PathBuf,
    pub(crate) gate: Arc<tokio::sync::Notify>,
    pub(crate) fired: Arc<std::sync::atomic::AtomicUsize>,
}

pub(crate) fn native_task_rig(
    root: &std::path::Path,
    scripts: Vec<Vec<faktor_provider::ScriptedResponse>>,
    parked_write: bool,
    service: bool,
) -> NativeTaskRig {
    use faktor_core::model::ModelCapabilities;
    let paced = PacedScriptedProvider::new(
        ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        scripts,
        5,
    );
    native_task_rig_with_provider(root, paced, parked_write, service)
}

/// [`native_task_rig`] with the always-pass verification seam wired, so
/// the executor's own isolation pipeline (prepare → verify → land →
/// complete → retire) runs end to end without a human certification.
pub(crate) fn native_task_rig_verified(
    root: &std::path::Path,
    scripts: Vec<Vec<faktor_provider::ScriptedResponse>>,
    parked_write: bool,
    service: bool,
) -> NativeTaskRig {
    use faktor_core::model::ModelCapabilities;
    let paced = PacedScriptedProvider::new(
        ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        scripts,
        5,
    );
    native_task_rig_with_provider_and_verification(
        root,
        paced,
        parked_write,
        service,
        faktor_agent::VerificationService::fake_ok(),
    )
}

/// The same rig with an EXPLICIT provider: the deterministic
/// failure-injection seam (a stub whose stream returns a typed error on
/// every platform, with no workspace/path/host-speed dependence).
pub(crate) fn native_task_rig_with_provider(
    root: &std::path::Path,
    provider: Arc<dyn faktor_provider::Provider>,
    parked_write: bool,
    service: bool,
) -> NativeTaskRig {
    native_task_rig_with_provider_and_verification(
        root,
        provider,
        parked_write,
        service,
        faktor_agent::VerificationService::disabled(),
    )
}

pub(crate) fn native_task_rig_with_provider_and_verification(
    root: &std::path::Path,
    provider: Arc<dyn faktor_provider::Provider>,
    parked_write: bool,
    service: bool,
    verification: Arc<faktor_agent::VerificationService>,
) -> NativeTaskRig {
    use faktor_core::id::{TaskId, WorktreeId};
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let mut registry = faktor_provider::ProviderRegistry::new();
    registry.try_register(provider).unwrap();
    let permissions = ChannelPermissionRequester::new(Duration::from_secs(5));
    let gate = Arc::new(tokio::sync::Notify::new());
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tools = faktor_agent::ToolRegistry::new();
    if parked_write {
        tools.register(native_real_write_tool(Some((gate.clone(), fired.clone()))));
    } else {
        tools.register(native_real_write_tool(None));
    }
    tools.register(native_parking_tool(gate.clone(), fired.clone()));
    let resolver = Arc::new(faktor_instructions::InstructionResolver::new(
        Arc::new(NativeRealRoots(manager.clone())),
        faktor_instructions::DEFAULT_RESOLVER_CACHE_ENTRIES,
    ));
    let agent = AgentRuntime::new(faktor_agent::AgentDeps {
        session: manager.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AllowAll),
        evidence: Arc::new(faktor_agent::NoEvidence),
        tools: Arc::new(tools),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification,
        hooks: None,
        instructions_resolver: resolver,
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test server agent.".into(),
        clock: Arc::new(faktor_core::time::SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 120_000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    })
    .unwrap();
    let owner_root = root.join("owner");
    std::fs::create_dir_all(&owner_root).unwrap();
    let ws = manager
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        manager
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = manager
        .create_session(ws, "native-task-rig", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(manager.clone(), agent.clone());
    let tasks = if service {
        let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
            manager.clone(),
            root.join("shadows"),
        )
        .unwrap();
        faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator,
            manager.clone(),
            agent.clone(),
            shadows,
        )
    } else {
        // Owner-direct low-level harness: the cfg/test-gated seam.
        // Production code can only construct the isolating executor.
        faktor_orchestrator::runtime::task_executor::TaskExecutor::new_owner_direct_for_test_harness(
            &orchestrator,
            manager.clone(),
            agent.clone(),
        )
    };
    let deps = ServerDeps {
        budgets: faktor_session::DurableBudgetLedger::new(manager.clone()),
        session: manager.clone(),
        agent,
        permissions,
        orchestrator,
        tasks,
        auth_token: AuthToken::generate(),
        server_password: ServerPassword::generate(),
        directory: None,
        version: "0.1.0".into(),
        fs: None,
        snapshots: None,
        chunk_rx: None,
        simulate_not_ready: false,
        evidence: None,
        semantic: None,
        control_plane: None,
        scm: None,
        scm_webhook: None,
        billing: None,
        updater: None,
        workers: None,
        enterprise: None,
        retention: None,
        sso: None,
        worker_plane_listener: None,
        terminal_policy: granted_terminal_policy(),
    };
    NativeTaskRig {
        deps,
        manager,
        parent,
        owner_root,
        gate,
        fired,
    }
}

pub(crate) fn seed_native_owner(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), NATIVE_OWNER_LIB_RS).unwrap();
}

/// The owner checkout's ORIGINAL content (distinct from the drive's
/// write so integration is byte-observable).
pub(crate) const NATIVE_OWNER_LIB_RS: &str = "pub fn value() -> u64 {\n    let base_amount: u64 = 40;\n    let increment: u64 = 1;\n    base_amount.saturating_add(increment)\n}\n";

pub(crate) const NATIVE_IMPL_LIB_RS: &str = "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n";

pub(crate) async fn native_wait_session_state(
    manager: &Arc<SessionManager>,
    sid: SessionId,
    want: faktor_core::state::AgentState,
) {
    for _ in 0..1500 {
        if manager.get_session(sid).unwrap().unwrap().state().unwrap() == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("session {sid} never reached {want:?}");
}

/// The human-verifier seam over a manager (the ONLY producer of
/// VerifiedComplete): drive the durable task row to Verifying, land a
/// passing record (covering every acceptance criterion of the row) and
/// complete.
pub(crate) fn certify_native_task(manager: &Arc<SessionManager>, sid: SessionId) {
    use faktor_core::state::{TaskState, TaskTransition, VerificationStatus};
    let h = manager.get_session(sid).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let criteria = h
        .get_task(task_id)
        .unwrap()
        .unwrap()
        .acceptance_criteria
        .into_iter()
        .map(|criterion_key| faktor_core::state::CriterionVerification {
            criterion_key,
            passed: true,
            evidence: None,
            binding: None,
        })
        .collect::<Vec<_>>();
    for _ in 0..8 {
        let task = h.get_task(task_id).unwrap().unwrap();
        let target = match task.state {
            TaskState::Pending => TaskTransition::StartRunning,
            TaskState::Planning => TaskTransition::PlanComplete,
            TaskState::Running => TaskTransition::RequestVerification,
            TaskState::Waiting => TaskTransition::ResumeFromWaiting,
            TaskState::Blocked => TaskTransition::Unblock,
            TaskState::NeedsVerification => TaskTransition::StartVerification,
            TaskState::Verifying => break,
            s => panic!("cannot certify a task at {s:?}"),
        };
        let rev = h.task_revision(task_id).unwrap();
        h.transition_task(task_id, rev, target, None).unwrap();
    }
    let task = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(task.state, TaskState::Verifying);
    let record = h
        .create_verification_record(
            task_id,
            None,
            criteria,
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            h.now_ms(),
        )
        .unwrap();
    let rev = h.task_revision(task_id).unwrap();
    let task = h.get_task(task_id).unwrap().unwrap();
    h.complete_verified_task(task_id, rev, record)
        .unwrap_or_else(|e| {
            panic!(
                "complete_verified_task at {rev:?} state {:?}: {e}",
                task.state
            )
        });
}

/// One complete, range-readable evidence envelope with retained backing
/// (`b"hello"`), owned by `session`/`workspace`.
pub(crate) fn evidence_envelope(
    id: u64,
    session: u64,
    workspace: u64,
    allow_ranges: bool,
) -> faktor_evidence::types::EvidenceEnvelope {
    faktor_evidence::types::EvidenceEnvelope::new(
        faktor_evidence::types::EvidenceId(id),
        faktor_evidence::types::EvidenceKind::ProcessLog,
        SessionId::new(session),
        WorkspaceId::new(workspace),
        None,
        None,
        faktor_evidence::types::ProvenanceSet::new([
            faktor_evidence::types::ProvenanceSource::Tool,
        ]),
        faktor_evidence::types::Compressibility::Reversible,
        faktor_evidence::types::CompactRepresentation {
            grammar: "log-v1".into(),
            body: "hello".into(),
        },
        Some([3u8; 32]),
        faktor_evidence::types::BackingCompleteness::Complete,
        faktor_evidence::types::CompressionRecord::identity(5),
        faktor_evidence::types::RetrievalPolicy::new(allow_ranges, true, 1024),
    )
    .unwrap()
}

pub(crate) fn evidence_handle(
    envelopes: Vec<faktor_evidence::types::EvidenceEnvelope>,
) -> EvidenceStoreHandle {
    let mut store = faktor_evidence::store::MemoryEvidenceStore::new(4096);
    for env in envelopes {
        store.insert(env, Some(b"hello".to_vec())).unwrap();
    }
    Arc::new(std::sync::RwLock::new(
        Box::new(store) as Box<dyn faktor_evidence::store::EvidenceStore + Send + Sync>
    ))
}
