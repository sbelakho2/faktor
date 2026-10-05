//! `main_acp_tests`: out-of-line slice of the CLI test module.

use super::*;

use faktor_core::model::ModelCapabilities;

use faktor_provider::{FakeProvider, GenericAgentRequest, ProviderChunk, ScriptedResponse};

use faktor_terminal::{EnvSpec, ProcessOwner, SpawnConfig};

use futures::StreamExt;

use std::pin::Pin;

/// Permission requester that never blocks on a UI (text-only turns never
/// ask, but AgentDeps requires one deterministically).
pub(crate) struct AlwaysAllow;

impl faktor_agent::PermissionRequester for AlwaysAllow {
    fn request(
        &self,
        _session: SessionId,
        _permission: &faktor_session::PermissionRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = faktor_core::Result<faktor_core::capability::PermissionDecision>,
                > + Send,
        >,
    > {
        Box::pin(async { Ok(faktor_core::capability::PermissionDecision::Allow) })
    }
}

/// Minimal REAL daemon AgentDeps over an open session manager: text-only
/// turns, no MCP/verifier/supervisor (nothing here ever runs a process).
pub(crate) fn test_agent(
    session: Arc<SessionManager>,
    registry: ProviderRegistry,
) -> Arc<AgentRuntime> {
    let cas = session.cas();
    let deps = AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(faktor_agent::NoEvidence),
        tools: Arc::new(ToolRegistry::new()),
        cas: Some(cas),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: daemon_instructions_resolver(&session),
        // Test graph: the passthrough pin (session-configured
        // provider/model win) + the REAL durable ledger over this
        // session manager (reservations ride the tempdir store).
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: faktor_session::DurableBudgetLedger::new(session.clone()),
        model: "default".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are Faktor.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    };
    AgentRuntime::new(deps).unwrap()
}

/// The daemon's real write_file shape for ACP shadow tests: writes
/// through the session's resolved workspace (the live shadow while a
/// shadowed drive is running).
pub(crate) fn acp_write_tool() -> faktor_agent::Tool {
    use faktor_agent::tool::RecoveryHint;
    use faktor_agent::{ToolOutcome, ToolRunCtx};
    use faktor_core::resource::ResourceClass;
    faktor_agent::Tool {
        name: "write_file".into(),
        description: "writes a real file".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(move |ctx: ToolRunCtx, args| {
            Box::pin(async move {
                let ws = ctx
                    .workspace
                    .ok_or_else(|| faktor_core::error::Error::internal("no workspace wired"))?;
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                ws.write_atomic(std::path::Path::new(path), content.as_bytes())
                    .map_err(|e| {
                        faktor_core::error::Error::internal(format!("write {path}: {e}"))
                    })?;
                Ok(ToolOutcome {
                    text: format!("wrote {path}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// An ACP host over the FULL execution wiring: one session/agent, one
/// shadow-carrying TaskExecutor, one PromptExecutionService the backend
/// was constructed with. `owner` is the real checkout the ACP session
/// points at.
pub(crate) struct AcpShadowRig {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) session: Arc<SessionManager>,
    #[allow(dead_code)]
    pub(crate) agent: Arc<AgentRuntime>,
    pub(crate) service: Arc<faktor_server::native::PromptExecutionService>,
    pub(crate) backend: DaemonAcpBackend,
}

pub(crate) fn acp_shadow_rig(scripts: Vec<ScriptedResponse>) -> AcpShadowRig {
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            scripts,
        )))
        .unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(acp_write_tool());
    let deps = AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(faktor_agent::NoEvidence),
        tools: Arc::new(tools),
        cas: Some(session.cas()),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: daemon_instructions_resolver(&session),
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: faktor_session::DurableBudgetLedger::new(session.clone()),
        model: "default".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are Faktor.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 60_000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    };
    let agent = AgentRuntime::new(deps).unwrap();
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
    let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
        session.clone(),
        dir.path().join("shadows"),
    )
    .unwrap();
    let tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
        &orchestrator,
        session.clone(),
        agent.clone(),
        shadows,
    );
    let service = faktor_server::native::PromptExecutionService::new(tasks, session.clone());
    let backend = DaemonAcpBackend::new(session.clone(), agent.clone(), service.clone());
    AcpShadowRig {
        dir,
        session,
        agent,
        service,
        backend,
    }
}

/// The EXPLICIT user-granted shell contract tests inject into the
/// daemon terminal authority when they exercise real PTY spawns — the
/// authority default is the fail-closed `os_isolated` shape, so a test
/// that wants a live terminal grants it exactly like an operator would.
pub(crate) fn granted_terminal_policy(
) -> faktor_server::native::terminal_authority::TerminalAuthorityPolicy {
    faktor_server::native::terminal_authority::TerminalAuthorityPolicy::explicit_user_granted_shell(
    )
}

/// The ACP turn-settle wait is BOUNDED: a machine that never leaves a
/// busy state returns a typed timeout on a short deadline instead of
/// polling forever; a settling machine returns its terminal state; a
/// durable read error propagates (never a silent success).
#[tokio::test]
async fn acp_turn_settle_wait_is_bounded_and_typed() {
    let t0 = std::time::Instant::now();
    let err = await_settled(
        || Ok(faktor_core::state::AgentState::Streaming),
        std::time::Duration::from_millis(40),
    )
    .await
    .expect_err("a never-settling turn must time out");
    assert!(err.contains("did not settle"), "{err}");
    assert!(err.contains("Streaming"), "{err}");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(5),
        "the wait must be bounded by the deadline, not by the poll"
    );

    let mut calls = 0u32;
    let state = await_settled(
        || {
            calls += 1;
            Ok(if calls < 3 {
                faktor_core::state::AgentState::ExecutingTool
            } else {
                faktor_core::state::AgentState::ReadyForNextTurn
            })
        },
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(state, faktor_core::state::AgentState::ReadyForNextTurn);

    let err = await_settled(
        || Err("durable read failed".to_string()),
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert!(err.contains("durable read failed"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_session_prompt_uses_the_shadow_and_keeps_the_owner_untouched() {
    // Ordinary chat through ACP `session/prompt` goes through the SAME
    // PromptExecutionService as Native and SDK compat: its write lands
    // in the daemon-owned shadow of the session workspace; the owner
    // checkout stays byte-untouched until a verified integration.
    const OWNER: &str = "pub fn value() -> u64 {\n    let base_amount: u64 = 40;\n    let increment: u64 = 1;\n    base_amount.saturating_add(increment)\n}\n";
    const IMPL: &str = "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n";
    let rig = acp_shadow_rig(vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: json!({"path": "src/lib.rs", "content": IMPL}),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let owner = rig.dir.path().join("owner");
    std::fs::create_dir_all(owner.join("src")).unwrap();
    std::fs::write(owner.join("src/lib.rs"), OWNER).unwrap();
    let sid = rig
        .backend
        .create_session(&json!({"workspace": owner.to_str().unwrap()}))
        .unwrap();

    let result = rig.backend.prompt(&sid, "implement the change").unwrap();
    assert_eq!(result["status"], "completed", "{result}");
    let sid = SessionId::new(sid.parse().unwrap());
    let shadow = rig
        .session
        .shadow_row(sid)
        .unwrap()
        .expect("an ordinary mutating ACP prompt must begin a shadow");
    assert_eq!(shadow.state, faktor_session::ShadowRowState::Active);
    assert_eq!(
        std::fs::read(std::path::Path::new(&shadow.root).join("src/lib.rs")).unwrap(),
        IMPL.as_bytes(),
        "the edit landed in the shadow"
    );
    assert_eq!(
        std::fs::read(owner.join("src/lib.rs")).unwrap(),
        OWNER.as_bytes(),
        "the owner checkout is byte-untouched"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_and_acp_prompts_hit_the_same_execution_service() {
    // Identity spy: ACP drives the exact PromptExecutionService instance
    // it was constructed with, and the service method the SDK compat
    // surface calls executes on the SAME task/session authorities.
    use faktor_server::native::{set_prompt_observer, PromptCallKind};
    use std::sync::Mutex;
    let rig = acp_shadow_rig(vec![
        ScriptedResponse::Text("pong".into()),
        ScriptedResponse::End,
    ]);
    let tasks_ptr = Arc::as_ptr(rig.service.tasks()) as usize;
    let sessions_ptr = Arc::as_ptr(rig.service.sessions()) as usize;
    let seen: Arc<Mutex<Vec<(PromptCallKind, usize, usize)>>> = Arc::new(Mutex::new(vec![]));
    let sink = seen.clone();
    set_prompt_observer(Some(Arc::new(move |call| {
        if call.tasks_ptr == tasks_ptr {
            sink.lock()
                .unwrap()
                .push((call.kind, call.tasks_ptr, call.sessions_ptr));
        }
    })));
    let owner = rig.dir.path().join("owner");
    std::fs::create_dir_all(&owner).unwrap();
    let sid = rig
        .backend
        .create_session(&json!({"workspace": owner.to_str().unwrap()}))
        .unwrap();
    // ACP session/prompt.
    rig.backend.prompt(&sid, "ping").unwrap();
    // Durable-point synchronization (load flake): the ACP prompt returns
    // when the turn MACHINE settles, but the drive's durable turn record
    // can still be ACTIVE for a moment (the detached drive resolves it on
    // its way out). A prompt issued in that window is correctly refused by
    // the executor's interrupted-run guard ("a live shadow ... and an
    // active drive"), so wait on the SAME durable point the guard reads
    // (the session's active turn record) before issuing the SDK call. The
    // retained shadow row is expected — a later prompt settles it. Bounded:
    // a run that never settles fails loudly here.
    {
        let sid = SessionId::new(sid.parse().unwrap());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let mid_turn = rig
                .session
                .get_session(sid)
                .unwrap()
                .expect("the session exists")
                .active_turn_record()
                .unwrap()
                .is_some();
            if !mid_turn {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first ACP run's drive never settled (active turn record \
                     still durable)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    // The SDK compat surface's exact service call (the server builds
    // its facade over the same deps and calls `prompt`).
    let request = faktor_server::native::PromptRequest {
        prompt: "sdk ping".into(),
        ..Default::default()
    };
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(
            rig.service
                .prompt(SessionId::new(sid.parse().unwrap()), request),
        )
    })
    .unwrap();
    set_prompt_observer(None);
    let calls = seen.lock().unwrap().clone();
    assert!(
        calls.len() >= 2,
        "ACP and the SDK service call must both be observed: {calls:?}"
    );
    for (kind, tasks, sessions) in &calls {
        assert_eq!(*tasks, tasks_ptr, "call {kind:?} ran on another executor");
        assert_eq!(
            *sessions, sessions_ptr,
            "call {kind:?} ran on another store"
        );
    }
    // The backend's field is the same Arc the caller constructed.
    assert!(Arc::ptr_eq(&rig.backend.prompts, &rig.service));
}

#[test]
fn acp_production_never_drives_the_agent_directly() {
    // Static scan (work-entry unification): the ACP host's backend may
    // only translate wire prompts into PromptExecutionService calls —
    // no direct AgentRuntime drive entry may remain in its body. The
    // backend impl moved with the daemon serve module (audit 9/23/24).
    let src = include_str!("daemon/serve.rs");
    let lines: Vec<&str> = src.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.contains("impl AcpBackend for DaemonAcpBackend {"))
        .expect("the ACP backend impl exists");
    // The trait impl runs until the first column-0 closing brace after
    // its header (it contains no nested column-0 items).
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.starts_with('}'))
        .map(|(j, _)| j)
        .expect("the ACP backend impl closes");
    for (i, l) in lines[start..end].iter().enumerate() {
        for token in [
            ".run_session_queue(",
            ".drive_receipt(",
            "agent.submit(",
            ".run_turn(",
        ] {
            assert!(
                !l.contains(token),
                "DaemonAcpBackend:{}: direct agent drive {token:?}: {l}",
                start + i + 1
            );
        }
    }
    assert!(
        lines[start..end]
            .iter()
            .any(|l| l.contains("service.prompt(")),
        "The ACP prompt must be delegated to PromptExecutionService::prompt"
    );
}

/// The residual: the REAL daemon shutdown sequence — the exact function
/// serve invokes when its shutdown channel fires — must reap live
/// orchestrated drives within the documented bound, leave the drive
/// registry empty, and keep the aborted run recoverable from its durable
/// rows (the task_executor abort->resume path, driven from the serve
/// integration level over the production shadow-carrying executor).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_shutdown_reaps_live_orchestrated_drives_and_keeps_them_resumable() {
    use faktor_core::id::{TaskId as CoreTaskId, WorktreeId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Bounded wait for the drive to reach the provider call. Under
    /// full-suite CPU starvation the OS schedules the drive's progress
    /// arbitrarily late, so this is a CONDITION wait with an explicit
    /// generous deadline — never a fixed sleep, never a bound sized for
    /// an idle machine.
    const DRIVE_PARK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
    /// Wall-clock load allowance for observing the drain: the product
    /// bound (`SERVE_DRIVE_SHUTDOWN_GRACE + ABORT_REAP_GRACE`) is
    /// enforced by the registry's own deadlines, and scheduler
    /// starvation stretches wall-clock timers. The starvation-
    /// independent teeth stay strict: the parked drive is aborted (never
    /// completed), every drive is reaped, the registry is closed, and
    /// the durable rows resume.
    const SHUTDOWN_DRAIN_LOAD_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(60);

    /// Provider whose model calls PARK until the gate opens: the
    /// orchestrated child drives stay deterministically in flight across
    /// the shutdown window, so the drain MUST abort (never just await)
    /// them.
    struct GateProvider {
        caps: ModelCapabilities,
        opened: tokio::sync::watch::Sender<bool>,
        entered: Arc<AtomicUsize>,
        /// Wakes a condition waiter whenever `entered` advances; a
        /// `notify_one` permit is stored, so a call that entered before
        /// the wait began can never be missed.
        entered_notify: tokio::sync::Notify,
    }

    impl GateProvider {
        /// Wait, bounded by the explicit `deadline`, until at least
        /// `at_least` provider calls have entered. Returns `false` only
        /// when the deadline elapsed.
        async fn wait_entered(&self, at_least: usize, deadline: std::time::Duration) -> bool {
            let wait = async {
                while self.entered.load(Ordering::SeqCst) < at_least {
                    self.entered_notify.notified().await;
                }
            };
            tokio::time::timeout(deadline, wait).await.is_ok()
        }
    }

    impl Provider for GateProvider {
        fn id(&self) -> &str {
            "gate"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            self.caps.clone()
        }

        fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.entered_notify.notify_one();
            let mut rx = self.opened.subscribe();
            let s = futures::stream::once(async move {
                while !*rx.borrow_and_update() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                Ok(ProviderChunk::Text {
                    text: "done".into(),
                })
            })
            .chain(futures::stream::once(async { Ok(ProviderChunk::Done) }));
            Box::pin(s)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (opened, _opened_rx) = tokio::sync::watch::channel(false);
    let provider = Arc::new(GateProvider {
        caps: ModelCapabilities {
            tools: true,
            parallel_tools: true,
            ..Default::default()
        },
        opened,
        entered: Arc::new(AtomicUsize::new(0)),
        entered_notify: tokio::sync::Notify::new(),
    });

    // A real, shadow-carrying executor over a real store: the SAME
    // authority set the serve daemon graph builds.
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider.clone()).unwrap();
    let agent = test_agent(session.clone(), registry);
    let owner_root = dir.path().join("owner");
    std::fs::create_dir_all(&owner_root).unwrap();
    std::fs::write(owner_root.join("a.txt"), b"base").unwrap();
    let ws = session
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        session
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = session
        .create_session(ws, "serve-shutdown", "gate", "m")
        .unwrap()
        .id();
    session
        .adopt_identity(parent, wt, CoreTaskId::new(1))
        .unwrap();
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
    let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
        session.clone(),
        dir.path().join("shadows"),
    )
    .unwrap();
    let tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
        &orchestrator,
        session.clone(),
        agent.clone(),
        shadows,
    );

    // Two work items => a real orchestrated run whose child drives park
    // inside the provider call.
    let receipt = tasks
        .start_task(
            parent,
            faktor_orchestrator::runtime::task_executor::TaskRunRequest {
                goal: "two-item shutdown run".into(),
                work_items: vec![
                    faktor_orchestrator::WorkItem::new(
                        "a",
                        "work a",
                        faktor_orchestrator::WorkKind::Analysis,
                    ),
                    faktor_orchestrator::WorkItem::new(
                        "b",
                        "work b",
                        faktor_orchestrator::WorkKind::Analysis,
                    ),
                ],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        receipt.mode,
        faktor_orchestrator::runtime::task_executor::TaskRunMode::Orchestrated
    );
    let run_id = receipt.run_id.clone();
    // The drive is live AND parked in the provider: it cannot finish on
    // its own before shutdown. Wait on the observable park condition
    // (the provider call entered) with the explicit deadline above —
    // under full-suite CPU starvation this can take far longer than on
    // an idle machine, and a fixed 30s window was the last known flake.
    assert!(
        provider.wait_entered(1, DRIVE_PARK_DEADLINE).await,
        "the orchestrated drive never parked within {DRIVE_PARK_DEADLINE:?}"
    );
    assert_eq!(
        tasks.live_drive_count(),
        1,
        "the parked drive is owned by the executor registry"
    );
    // The durable assignments (the resume entry point) exist BEFORE the
    // abort.
    let assignments = faktor_orchestrator::runtime::OrchestratorRuntime::assignment_rows(
        session.clone(),
        parent,
        &run_id,
    )
    .expect("assignment rows readable");
    assert!(
        !assignments.is_empty(),
        "durable assignments before shutdown"
    );

    // The EXACT serve shutdown sequence (the post-ready loop handles are
    // dummies; the daemon's own loops have dedicated tests). The drain
    // itself must resolve within the documented product bound plus the
    // explicit load allowance; a timeout here is a real unbounded-
    // shutdown failure, never an unbounded test wait.
    let drain_bound = SERVE_DRIVE_SHUTDOWN_GRACE
        + faktor_orchestrator::runtime::task_executor::TaskDriveRegistry::ABORT_REAP_GRACE
        + SHUTDOWN_DRAIN_LOAD_ALLOWANCE;
    let report = tokio::time::timeout(
        drain_bound,
        shutdown_serving_daemon(
            &tasks,
            None,
            None,
            None,
            None,
            tokio::spawn(std::future::pending::<()>()),
            None,
            None,
            tokio::spawn(async {}),
        ),
    )
    .await
    .expect("the drive drain must stay within grace + abort/reap + load allowance");
    assert!(report.total >= 1, "the live drive was owned: {report:?}");
    assert_eq!(
        report.completed, 0,
        "the parked drive must not complete during the grace: {report:?}"
    );
    assert!(
        report.aborted >= 1,
        "the parked drive was aborted: {report:?}"
    );
    assert_eq!(report.unreaped, 0, "every drive reaped: {report:?}");
    assert_eq!(tasks.live_drive_count(), 0, "registry empty");
    assert!(tasks.live_drive_runs().is_empty(), "registry empty");
    assert!(tasks.drives_shutdown(), "the registry is closed");

    // Durable state intact: the assignment rows survive the abort.
    let after = faktor_orchestrator::runtime::OrchestratorRuntime::assignment_rows(
        session.clone(),
        parent,
        &run_id,
    )
    .expect("assignment rows still readable");
    assert!(!after.is_empty(), "durable assignments survive the abort");

    // CRASH: drop the whole daemon stack and reopen the same data dir —
    // exactly a daemon restart. The aborted run re-attaches from its
    // durable rows through the documented recovery entry.
    drop(tasks);
    drop(orchestrator);
    drop(agent);
    drop(session);
    let session2 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let mut registry2 = ProviderRegistry::new();
    registry2.try_register(provider.clone()).unwrap();
    let agent2 = test_agent(session2.clone(), registry2);
    agent2.recover().expect("fresh daemon recovery pass");
    let orchestrator2 =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(session2.clone(), agent2.clone());
    let shadows2 = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
        session2.clone(),
        dir.path().join("shadows"),
    )
    .unwrap();
    let tasks2 = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
        &orchestrator2,
        session2.clone(),
        agent2.clone(),
        shadows2,
    );
    let resumed = tasks2
        .resume_run(
            parent,
            &run_id,
            faktor_orchestrator::runtime::Ceilings::default(),
            faktor_orchestrator::caps::CapabilitySet::new(),
            None,
        )
        .expect("the aborted run re-attaches from its durable rows");
    assert_eq!(resumed.run_id, run_id);
    // Re-attachment is only proven when the re-driven child observably
    // reaches the provider again: the resumed drive re-parks on the same
    // closed gate, so it stays owned by the fresh registry.
    assert!(
        provider.wait_entered(2, DRIVE_PARK_DEADLINE).await,
        "the re-attached run never re-parked within {DRIVE_PARK_DEADLINE:?}"
    );
    assert_eq!(
        tasks2.live_drive_count(),
        1,
        "the re-attached drive is owned by the fresh registry"
    );
    // Reap the re-attached drive too: the second drain owns, aborts and
    // reaps it exactly like the first.
    let report2 = tasks2
        .shutdown_drives(std::time::Duration::from_millis(50))
        .await;
    assert!(
        report2.total >= 1,
        "the re-attached drive was owned: {report2:?}"
    );
    assert_eq!(
        report2.completed, 0,
        "the re-parked drive must not complete during the grace: {report2:?}"
    );
    assert!(
        report2.aborted >= 1,
        "the re-parked drive was aborted: {report2:?}"
    );
    assert_eq!(
        report2.unreaped, 0,
        "every re-attached drive reaped: {report2:?}"
    );
    assert_eq!(tasks2.live_drive_count(), 0);
}

/// The construction seam both daemon entry points use resolves the
/// INTERACTIVE terminal class: unset selects the honest user grant
/// (never the agent class's secure default), explicit `os_isolated`
/// selects isolation/typed refusal, an explicit grant selects the
/// granted shape, and an invalid pairing is refused here — never
/// silently reinterpreted. The planted fixture pins the invariant that
/// the terminal grant can NEVER widen the agent shell-tool class.
#[test]
fn terminal_authority_policy_resolves_the_interactive_class_and_never_widens_the_agent_class() {
    // Unset: the interactive class default is the user grant; the agent
    // shell-tool class keeps `os_isolated`/`required` (the PLANTED
    // FIXTURE: an implicitly granted terminal class does not widen the
    // agent class).
    let default_cfg = config::Config::default();
    let terminals = terminal_authority_policy(&default_cfg).expect("the default config resolves");
    let state = terminals.shell_execution_state();
    assert_eq!(
        state.mode,
        faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted
    );
    assert_eq!(
        state.network_guarantee,
        faktor_sandbox::SandboxGuarantee::None
    );
    let agent = default_cfg
        .sandbox_policy()
        .expect("the agent policy resolves");
    assert_eq!(
        agent.shell_execution,
        faktor_sandbox::ShellExecutionMode::OsIsolated,
        "no terminal grant may widen the agent shell-tool class"
    );
    assert_eq!(
        agent.network_guarantee,
        faktor_sandbox::SandboxGuarantee::Required
    );

    // Explicit OS isolation: both classes isolate (terminals refuse
    // typed where no OS backend exists; the isolation demand is real).
    let os_config = config::Config {
        sandbox: config::SandboxCfg {
            network_guarantee: faktor_sandbox::SandboxGuarantee::Required,
            shell: Some(faktor_sandbox::ShellExecutionMode::OsIsolated),
            ..config::SandboxCfg::default()
        },
        ..config::Config::default()
    };
    let terminals = terminal_authority_policy(&os_config).expect("explicit isolation resolves");
    let state = terminals.shell_execution_state();
    assert_eq!(state.mode, faktor_sandbox::ShellExecutionMode::OsIsolated);
    assert_eq!(
        state.network_guarantee,
        faktor_sandbox::SandboxGuarantee::Required
    );
    assert_eq!(
        os_config
            .sandbox_policy()
            .expect("the agent policy resolves")
            .shell_execution,
        faktor_sandbox::ShellExecutionMode::OsIsolated
    );

    // Explicit grant: both classes carry the grant and the configured
    // non-required guarantee.
    let grant = config::Config {
        sandbox: config::SandboxCfg {
            network_guarantee: faktor_sandbox::SandboxGuarantee::None,
            shell: Some(faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted),
            ..config::SandboxCfg::default()
        },
        ..config::Config::default()
    };
    for state in [
        terminal_authority_policy(&grant)
            .expect("an explicit terminal grant resolves")
            .shell_execution_state(),
        grant
            .sandbox_policy()
            .expect("the agent policy resolves")
            .shell_execution_state(),
    ] {
        assert_eq!(
            state.mode,
            faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted
        );
        assert_eq!(
            state.network_guarantee,
            faktor_sandbox::SandboxGuarantee::None
        );
    }

    // A user-granted shell paired with the OS-isolation requirement is
    // the invalid pairing `SandboxPolicy::validate` refuses (the daemon
    // would refuse the config); the seam must refuse it too.
    let invalid = config::Config {
        sandbox: config::SandboxCfg {
            network_guarantee: faktor_sandbox::SandboxGuarantee::Required,
            shell: Some(faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted),
            ..config::SandboxCfg::default()
        },
        ..config::Config::default()
    };
    let refused = terminal_authority_policy(&invalid)
        .expect_err("an invalid shell/guarantee pairing must be refused");
    assert!(
        refused.contains("network_capable_user_granted"),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_hook_and_terminal_children_share_one_daemon_supervisor() {
    // (a) The daemon owns EXACTLY ONE supervisor. Two MCP servers, a
    // long hook and a long terminal run CONCURRENTLY all admit into the
    // SAME bounded registry: while the hook and the terminal are live,
    // the single supervisor accounts 4 live children (2 mcp + hook +
    // terminal) — the old per-server supervisors would have split them
    // across registries.
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 missing; skipping");
        return;
    }
    let fixture = format!("{}/tests/fixtures/mcp_mock.py", env!("CARGO_MANIFEST_DIR"));
    assert!(
        std::path::Path::new(&fixture).exists(),
        "mcp fixture missing at {fixture}"
    );
    let mcp_entries = vec![
        config::McpEntry {
            name: "mock-a".into(),
            command: "python3".into(),
            args: vec![fixture.clone()],
        },
        config::McpEntry {
            name: "mock-b".into(),
            command: "python3".into(),
            args: vec![fixture.clone()],
        },
    ];
    let cfg = config::Config {
        mcp: mcp_entries,
        ..Default::default()
    };
    // The env hook path registers NO long hook here (the env format
    // cannot carry an unquoted `sleep 3` argument vector), so the long
    // hook is registered through the SAME cli helper `serve` uses
    // (env_hook_registry delegates here) onto the daemon's supervisor.
    let dir = tempfile::tempdir().unwrap();
    let graph = build_daemon_with_mcp(dir.path(), Some(cfg))
        .await
        .expect("daemon with two mcp servers builds");
    let supervisor = graph
        .agent
        .deps()
        .supervisor
        .clone()
        .expect("daemon supervisor wired");
    let registry = hook_registry(
        &supervisor,
        vec![faktor_hooks::HookSpec {
            id: "env-0".into(),
            events: vec![faktor_hooks::HookEvent::PreTool],
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 3".into()],
            env_allowlist: true,
            deadline_ms: 20_000,
            failure_policy: faktor_hooks::FailurePolicy::FailClosed,
            ..Default::default()
        }],
    )
    .expect("long hook registered");
    let hooks = registry;
    // Both MCP children live in the daemon supervisor's registry.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if supervisor.alive().len() == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "two mcp children never appeared: {:?}",
            supervisor.alive()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(graph.mcp_servers.iter().all(|s| s.is_alive()));
    // A long hook AND a long terminal child concurrently with the MCP
    // servers: one registry counts all four.
    let registry = hooks.clone();
    let hook_thread = std::thread::spawn(move || {
        registry.run(
            faktor_hooks::HookEvent::PreTool,
            &faktor_hooks::HookInput::default(),
        )
    });
    let sup = supervisor.clone();
    let terminal_thread = std::thread::spawn(move || {
        sup.run_sync(
            SpawnConfig {
                cmd: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 3".into()],
                cwd: std::env::temp_dir(),
                env: EnvSpec::Minimal,
                owner: ProcessOwner::Daemon,
                ..Default::default()
            },
            std::time::Duration::from_secs(30),
            64 * 1024,
            64 * 1024,
        )
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if supervisor.alive().len() == 4 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected 4 live children in the ONE registry, saw {:?}",
            supervisor.alive()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let hook_verdict = hook_thread.join().expect("hook thread panicked");
    assert_eq!(
        hook_verdict,
        faktor_hooks::HookVerdict::Allow,
        "the long hook exits cleanly within its deadline"
    );
    terminal_thread
        .join()
        .expect("terminal thread panicked")
        .expect("the terminal child must complete");
    // Both finished: the MCP children remain, counted by the SAME
    // supervisor the hooks and terminal ran through.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if supervisor.alive().len() == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "children leaked after the runs: {:?}",
            supervisor.alive()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    drop(graph);
}

// ------------- daemon ACP terminal round-trip (the REAL authority) -----

/// Byte-level ACP client over the client half of an in-memory duplex.
#[cfg(unix)]
pub(crate) struct AcpWire {
    pub(crate) read: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    pub(crate) write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    pub(crate) buf: Vec<u8>,
    pub(crate) next_id: u64,
}

#[cfg(unix)]
impl AcpWire {
    pub(crate) async fn expect_message(&mut self) -> Value {
        use tokio::io::AsyncReadExt;
        loop {
            match faktor_acp::protocol::parse_frame(&self.buf) {
                Ok(Some((consumed, value))) => {
                    self.buf.drain(..consumed);
                    return value;
                }
                Ok(None) => {
                    let mut chunk = [0u8; 8192];
                    let n = tokio::time::timeout(
                        std::time::Duration::from_secs(20),
                        self.read.read(&mut chunk),
                    )
                    .await
                    .expect("server answers within the test bound")
                    .expect("server read");
                    assert!(n > 0, "server closed the pipe before answering");
                    self.buf.extend_from_slice(&chunk[..n]);
                }
                Err(e) => panic!("client framing error: {e}"),
            }
        }
    }

    pub(crate) async fn send(&mut self, method: &str, params: Value) -> u64 {
        use tokio::io::AsyncWriteExt;
        let id = self.next_id;
        self.next_id += 1;
        let bytes = faktor_acp::protocol::frame(method.to_string(), id, params);
        self.write.write_all(&bytes).await.expect("client write");
        self.write.flush().await.expect("client flush");
        id
    }

    pub(crate) async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        loop {
            let msg = self.expect_message().await;
            let is_response = msg.get("result").is_some() || msg.get("error").is_some();
            if msg["id"] == json!(id) && is_response {
                return msg;
            }
        }
    }

    /// Send one request and read until BOTH its response and a
    /// `terminalOutput` frame whose data contains `needle` have arrived
    /// (the two race, and neither may be dropped).
    pub(crate) async fn request_until_output(
        &mut self,
        method: &str,
        params: Value,
        terminal_id: &str,
        needle: &str,
    ) -> (Value, Vec<String>) {
        let id = self.send(method, params).await;
        let mut updates: Vec<String> = Vec::new();
        let mut response: Option<Value> = None;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for terminal output {needle:?}"
            );
            let msg = self.expect_message().await;
            if msg.get("method").and_then(Value::as_str) == Some("session/update") {
                let update = &msg["params"]["update"];
                if update["kind"] == "terminalOutput" && update["terminalId"] == json!(terminal_id)
                {
                    updates.push(update["data"].as_str().unwrap_or_default().to_string());
                }
            } else if msg["id"] == json!(id) {
                response = Some(msg);
            }
            if updates.iter().any(|data| data.contains(needle)) {
                if let Some(response) = response {
                    return (response, updates);
                }
            }
        }
    }
}

/// Spawn the ACP server over a duplex; returns the client plus the serve
/// task. `authority` is attached when present (the production wiring).
#[cfg(unix)]
pub(crate) fn start_daemon_terminal_server(
    backend: DaemonAcpBackend,
    authority: Option<Arc<dyn faktor_acp::TerminalAuthority>>,
) -> (AcpWire, tokio::task::JoinHandle<Result<(), String>>) {
    let server = match authority {
        Some(authority) => AcpServer::new(backend).with_terminal_authority(authority),
        None => AcpServer::new(backend),
    };
    let (server_side, client_side) = tokio::io::duplex(1024 * 1024);
    let (server_r, server_w) = tokio::io::split(server_side);
    let (client_r, client_w) = tokio::io::split(client_side);
    let task = tokio::spawn(async move { server.serve_connection(server_r, server_w).await });
    (
        AcpWire {
            read: client_r,
            write: client_w,
            buf: Vec::new(),
            next_id: 1,
        },
        task,
    )
}

/// Create one REAL durable session through the ACP wire, returning its id.
#[cfg(unix)]
pub(crate) async fn acp_new_real_session(
    client: &mut AcpWire,
    workspace: &std::path::Path,
) -> String {
    std::fs::create_dir_all(workspace).unwrap();
    let new = client
        .request(
            "session/new",
            json!({ "workspace": workspace.to_str().unwrap() }),
        )
        .await;
    assert_eq!(new["error"], Value::Null, "session/new failed: {new}");
    new["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

/// Native liveness probe of one raw pid. `kill(pid, 0) == 0` means the
/// pid exists (a live child OR an unreaped zombie); `EPERM` also means it
/// exists — the pid is real but not ours to signal, and on Darwin a group
/// of unreaped zombies answers `EPERM`. Only `ESRCH` proves the pid is
/// gone. No shell subprocess: a transient spawn failure under full-suite
/// load (`EAGAIN`/`ENOMEM`) can never be misread as "the child died", and
/// the probe matches the wait's observable state exactly.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn pid_probe(pid: u32) -> Result<(), i32> {
    if pid == 0 {
        return Err(libc::ESRCH);
    }
    // SAFETY: signal 0 only probes existence and never delivers a signal;
    // the pid fits `pid_t` and a recycled/invalid id can at worst report
    // the wrong liveness, never crash the probe.
    let r = unsafe { libc::kill(pid as i32, 0) };
    if r == 0 {
        return Ok(());
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    if errno == libc::EPERM {
        Ok(())
    } else {
        Err(errno)
    }
}

#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    pid_probe(pid).is_ok()
}

/// Pins the probe contract the round-trip wait depends on: an UNREAPED
/// zombie still owns its pid (the kernel answers `0`/`EPERM`, never
/// `ESRCH`), and only the parent's reap turns the pid into `ESRCH`. A
/// shell wrapper that merely exited non-zero can never satisfy this.
#[cfg(unix)]
#[test]
fn pid_liveness_probe_requires_esrch_not_a_shell_verdict() {
    let mut child = std::process::Command::new("sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn the probe fixture");
    let pid = child.id();
    assert!(pid_alive(pid), "a running child owns its pid");
    // Observe the exit WITHOUT reaping it: `ps` state `Z` is a zombie.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let out = std::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps the fixture");
        if String::from_utf8_lossy(&out.stdout)
            .trim_start()
            .starts_with('Z')
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fixture child must exit"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        pid_alive(pid),
        "an unreaped zombie still exists: only ESRCH means gone"
    );
    child.wait().expect("reap the fixture");
    assert_eq!(
        pid_probe(pid),
        Err(libc::ESRCH),
        "after the parent reaps it, the kernel reports ESRCH"
    );
}

/// The production wiring end-to-end: an ACP client negotiates
/// `faktor.terminal` and drives create → input → output → kill against
/// the daemon's REAL session-owned authority (faktor-pty rows over the
/// SAME session manager the backend uses). The terminal output must
/// round-trip through a real PTY; the kill must take the real child.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_terminal_round_trip_runs_on_the_daemon_authority() {
    let rig = acp_shadow_rig(Vec::new());
    let authority_impl = Arc::new(DaemonTerminalAuthority::new(
        rig.session.clone(),
        granted_terminal_policy(),
    ));
    let registry = authority_impl.registry.clone();
    let authority: Arc<dyn faktor_acp::TerminalAuthority> = authority_impl.clone();
    let (mut client, task) = start_daemon_terminal_server(rig.backend, Some(authority));

    let init = client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    assert!(
        init["result"]["extensions"]
            .as_array()
            .map(|names| names.iter().any(|n| n == "faktor.terminal"))
            .unwrap_or(false),
        "the attached authority must negotiate faktor.terminal: {init}"
    );

    let workspace = rig.dir.path().join("ws");
    let sid = acp_new_real_session(&mut client, &workspace).await;

    let created = client
        .request(
            "terminal/create",
            json!({
                "sessionId": sid,
                "command": "sh",
                "args": ["-c", "stty -echo; read x; echo got:$x; sleep 30"],
                "env": ["PATH"],
                "rows": 24,
                "cols": 80,
            }),
        )
        .await;
    assert_eq!(created["error"], Value::Null, "create failed: {created}");
    let tid = created["result"]["terminalId"]
        .as_str()
        .expect("terminalId")
        .to_string();
    let pid = created["result"]["pid"].as_u64().expect("pid") as u32;
    let ownership = created["result"]["ownershipId"]
        .as_str()
        .expect("ownershipId")
        .to_string();
    assert!(pid > 0, "a real pty pid");
    assert_eq!(created["result"]["sessionId"], json!(sid));

    // OWNERSHIP FIRST: the create-response contract is that the terminal
    // is already session-owned when the response arrives. Observe that
    // ownership condition with a bounded wait instead of assuming it; a
    // response that ever raced its registration fails HERE naming the
    // exact missing condition.
    let sid_typed = SessionId::new(sid.parse().unwrap());
    let ownership_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let rows = loop {
        let rows = registry.session_rows(sid_typed);
        if !rows.is_empty() {
            break rows;
        }
        assert!(
            std::time::Instant::now() < ownership_deadline,
            "terminal/create returned before its session-owned row was registered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    // The row is session-owned in the daemon registry: the durable
    // session id, the operation id and the pid all match the wire.
    assert_eq!(rows.len(), 1, "one session-owned row: {rows:?}");
    assert_eq!(rows[0].0, tid);
    assert_eq!(rows[0].1.pid, pid);
    assert_eq!(rows[0].1.operation_id.to_string(), ownership);
    assert_eq!(rows[0].1.session_id.to_string(), sid);

    // LIVENESS of the OWNED row, with a bounded wait and never a single
    // instant sample: only ESRCH (the pid was reaped by the single
    // reader/reaper) proves the child is gone — 0 and EPERM both mean
    // the pid still exists (Darwin answers EPERM for unreaped zombies).
    let alive_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match pid_probe(pid) {
            Ok(()) => break,
            Err(libc::ESRCH) => {
                let rows = registry.session_rows(sid_typed);
                assert!(
                    std::time::Instant::now() < alive_deadline,
                    "the owned pty child (pid {pid}) was reaped right after create \
                         (session rows: {rows:?})"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(errno) => panic!(
                "pid {pid} liveness probe failed with errno {errno}; the terminal \
                     cannot be observed"
            ),
        }
    }

    // Input → real tty → terminalOutput frames.
    let (response, updates) = client
        .request_until_output(
            "terminal/input",
            json!({ "sessionId": sid, "terminalId": tid, "data": "hello\n" }),
            &tid,
            "got:hello",
        )
        .await;
    assert_eq!(response["result"], json!({}), "{response}");
    assert!(
        !updates.is_empty(),
        "the real tty output must arrive as terminalOutput frames"
    );

    // Kill takes the real child; the bounded wait leaves ONLY when the
    // kernel reports ESRCH (the pid was reaped), never on a probe-side
    // verdict. EPERM keeps waiting: the pid still exists.
    let killed = client
        .request(
            "terminal/kill",
            json!({ "sessionId": sid, "terminalId": tid }),
        )
        .await;
    assert_eq!(killed["result"], json!({}), "{killed}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match pid_probe(pid) {
            Err(libc::ESRCH) => break,
            Ok(()) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "terminal/kill must terminate the real child (pid {pid} still exists)"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(errno) => panic!(
                "pid {pid} liveness probe failed with errno {errno}; the kill cannot be observed"
            ),
        }
    }

    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
}

/// The negative contract on the wire: without a negotiated capability
/// (or without an attached authority) every `terminal/*` method keeps
/// the official `-32601` — never a partial success.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_terminal_methods_are_method_not_found_when_not_negotiated() {
    // (a) Authority attached, extension NOT negotiated.
    let rig = acp_shadow_rig(Vec::new());
    let authority: Arc<dyn faktor_acp::TerminalAuthority> = Arc::new(DaemonTerminalAuthority::new(
        rig.session.clone(),
        granted_terminal_policy(),
    ));
    let (mut client, task) = start_daemon_terminal_server(rig.backend, Some(authority));
    client
        .request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    let workspace = rig.dir.path().join("ws-no-neg");
    let sid = acp_new_real_session(&mut client, &workspace).await;
    let msg = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh", "args": ["-c", "true"] }),
        )
        .await;
    assert_eq!(
        msg["error"]["code"],
        json!(faktor_acp::METHOD_NOT_FOUND),
        "unnegotiated terminal/create must stay -32601: {msg}"
    );
    let msg = client
        .request("terminal/list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(
        msg["error"]["code"],
        json!(faktor_acp::METHOD_NOT_FOUND),
        "unnegotiated terminal/list must stay -32601: {msg}"
    );
    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

    // (b) Extension negotiated, but NO authority attached.
    let rig = acp_shadow_rig(Vec::new());
    let (mut client, task) = start_daemon_terminal_server(rig.backend, None);
    let init = client
        .request(
            "initialize",
            json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
        )
        .await;
    assert!(
        init["result"].get("extensions").is_none()
            || !init["result"]["extensions"]
                .as_array()
                .map(|names| names.iter().any(|n| n == "faktor.terminal"))
                .unwrap_or(false),
        "without an authority the extension is not negotiated: {init}"
    );
    let workspace = rig.dir.path().join("ws-no-authority");
    let sid = acp_new_real_session(&mut client, &workspace).await;
    let msg = client
        .request(
            "terminal/create",
            json!({ "sessionId": sid, "command": "sh", "args": ["-c", "true"] }),
        )
        .await;
    assert_eq!(
        msg["error"]["code"],
        json!(faktor_acp::METHOD_NOT_FOUND),
        "without an authority terminal/create must stay -32601: {msg}"
    );
    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
}
