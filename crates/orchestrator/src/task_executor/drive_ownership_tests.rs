//! `task_executor::drive_ownership_tests`, split out of the parent
//! `mod.rs` by the source-size ceiling (inline-module body moved verbatim;
//! `use super::*` keeps every parent fixture in scope).
//!
//! Adversarial covers of the audited spawn-ownership gap: every detached
//! drive is owned by [`TaskDriveRegistry`], shutdown aborts/awaits within
//! bound and leaves the durable rows recoverable, and a shut-down
//! registry refuses new drives instead of orphaning them.
//!
//! (the suite-level HEAVY_SUITE guard is held across awaits BY DESIGN:
//! it serializes the file/CAS/SQLite-heavy executor fixture with the
//! other heavy suites.)
// TEST-SEPARATION: oracle=contract isolation=isolated-data-dir

#![allow(clippy::await_holding_lock)]

use super::*;
use crate::runtime::Ceilings;
use crate::test_support::heavy_guard;
use faktor_agent::{AgentDeps, NoEvidence, PermissionRequester, ToolCallMode, ToolRegistry};
use faktor_core::capability::PermissionDecision;
use faktor_core::model::ModelCapabilities;
use faktor_core::time::SystemClock;
use faktor_provider::{
    GenericAgentRequest, Provider, ProviderChunk, ProviderRegistry, ProviderStream,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

async fn wait_until(mut cond: impl FnMut() -> bool, timeout_secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "wait_until timed out after {timeout_secs}s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------- registry (unit)

/// Shutdown must abort and reap every parked drive within bound, empty
/// the registry, and refuse any later drive (no post-shutdown orphan).
#[tokio::test]
async fn drive_registry_shutdown_aborts_reaps_and_refuses_new_work() {
    let registry = TaskDriveRegistry::new();
    let entered = Arc::new(AtomicUsize::new(0));
    for i in 0..3 {
        let entered = entered.clone();
        assert!(registry.spawn(format!("run-{i}"), async move {
            entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }));
    }
    assert_eq!(registry.live_task_count(), 3);
    assert_eq!(registry.live_run_ids(), vec!["run-0", "run-1", "run-2"]);
    assert!(!registry.is_closed());
    let started = Instant::now();
    let report = registry.shutdown(Duration::from_millis(50)).await;
    assert!(started.elapsed() < Duration::from_secs(5), "bounded drain");
    assert_eq!(report.total, 3);
    assert_eq!(report.completed, 0);
    assert_eq!(report.aborted, 3);
    assert_eq!(report.unreaped, 0);
    assert!(report.all_reaped());
    assert_eq!(registry.live_task_count(), 0);
    assert!(registry.live_run_ids().is_empty());
    assert!(registry.is_closed());
    // A drive handed in after shutdown is REFUSED, never orphaned.
    let late = Arc::new(AtomicBool::new(false));
    let late_flag = late.clone();
    assert!(!registry.spawn("run-late", async move {
        late_flag.store(true, Ordering::SeqCst);
    }));
    assert_eq!(registry.live_task_count(), 0);
    tokio::task::yield_now().await;
    assert!(!late.load(Ordering::SeqCst), "refused drive never ran");
    // Idempotent: a second shutdown drains nothing.
    let second = registry.shutdown(Duration::from_millis(10)).await;
    assert_eq!(second.total, 0);
    assert!(second.all_reaped());
}

/// The graceful window is real: a drive that commits its record-first
/// marker inside the window is allowed to land it (no premature abort),
/// while a straggler is only aborted AFTER the window elapsed.
#[tokio::test]
async fn drive_registry_grace_window_is_bounded_and_aborts_only_after_it() {
    let registry = TaskDriveRegistry::new();
    let committed = Arc::new(AtomicBool::new(false));
    let committing = committed.clone();
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    assert!(registry.spawn("run-commits", async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        committing.store(true, Ordering::SeqCst); // durable marker stand-in
        let _ = held.await; // guarded effect parks
    }));
    let started = Instant::now();
    let report = registry.shutdown(Duration::from_millis(250)).await;
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "grace window honored"
    );
    assert!(
        committed.load(Ordering::SeqCst),
        "in-flight durable marker landed before the abort"
    );
    assert_eq!(report.completed, 0);
    assert_eq!(report.aborted, 1);
    assert_eq!(report.unreaped, 0);
    drop(release);
}

/// Finished drives are reaped on the next observation: the live count is
/// never a completed high-water mark.
#[tokio::test]
async fn drive_registry_reaps_finished_drives() {
    let registry = TaskDriveRegistry::new();
    assert!(registry.spawn("run-quick", async {}));
    for _ in 0..1000 {
        if registry.live_task_count() == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(registry.live_task_count(), 0);
    assert!(registry.live_run_ids().is_empty());
}

// --------------------------------------------- executor (integration)

struct AlwaysAllow;
impl PermissionRequester for AlwaysAllow {
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

/// Provider whose model calls PARK until the test opens the gate: the
/// drive stays deterministically in flight across shutdown, then serves
/// a complete turn to the resumed drive.
struct GateProvider {
    caps: ModelCapabilities,
    opened: tokio::sync::watch::Sender<bool>,
    entered: Arc<AtomicUsize>,
}

impl Provider for GateProvider {
    fn id(&self) -> &str {
        "gate"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        self.caps.clone()
    }
    fn capability_resolution(&self, model: &str) -> faktor_core::model::CapabilityResolution {
        // A test double constructed with its exact profile is
        // model-specific by construction.
        faktor_core::model::CapabilityResolution::known(self.capabilities(model))
    }

    fn stream(&self, _req: GenericAgentRequest) -> ProviderStream {
        use futures::StreamExt;
        self.entered.fetch_add(1, Ordering::SeqCst);
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

struct GateEnv {
    manager: Arc<SessionManager>,
    agent: Arc<AgentRuntime>,
    orchestrator: Arc<OrchestratorRuntime>,
    executor: Arc<TaskExecutor>,
    parent: SessionId,
    provider: Arc<GateProvider>,
}

fn read_caps() -> CapabilitySet {
    CapabilitySet::from_grants([CapabilityGrant::new(
        LatticeCap::ReadWorkspace,
        ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
    )])
    .expect("wildcard grants are sane")
}

fn gate_provider() -> Arc<GateProvider> {
    let (opened, _opened_rx) = tokio::sync::watch::channel(false);
    Arc::new(GateProvider {
        caps: ModelCapabilities {
            tools: true,
            parallel_tools: true,
            ..Default::default()
        },
        opened,
        entered: Arc::new(AtomicUsize::new(0)),
    })
}

/// Build the full daemon-side stack over a store root WITHOUT creating a
/// session: a reopen of the same root after a "crash" reconstructs the
/// graph over the SAME durable rows (the only way the in-memory op
/// registry is truly fresh, exactly like a process restart).
fn build_gate_stack(
    root: &std::path::Path,
    provider: Arc<GateProvider>,
) -> (
    Arc<SessionManager>,
    Arc<AgentRuntime>,
    Arc<OrchestratorRuntime>,
    Arc<TaskExecutor>,
) {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider).unwrap();
    let agent = AgentRuntime::new(AgentDeps {
        session: manager.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
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
        instructions: "You are a test agent.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 5000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
        receipt_sink: None,
    })
    .unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new_owner_direct_for_test_harness(
        &orchestrator,
        manager.clone(),
        agent.clone(),
    );
    (manager, agent, orchestrator, executor)
}

fn open_gate_env(root: &std::path::Path) -> GateEnv {
    let provider = gate_provider();
    let (manager, agent, orchestrator, executor) = build_gate_stack(root, provider.clone());
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
        .create_session(ws, "gate-owner", "gate", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    GateEnv {
        manager,
        agent,
        orchestrator,
        executor,
        parent,
        provider,
    }
}

/// The audited end-to-end invariant: a live detached drive is owned by
/// the executor; shutdown aborts/awaits it within bound with an empty
/// registry; the durable task/run rows survive the abort; and the
/// documented crash-recovery entry (`agent.recover()` +
/// `agent.continue_turn()`) resumes the SAME recorded turn to a genuine
/// end — the abort is crash-equivalent, never durable loss.
#[tokio::test]
async fn executor_shutdown_aborts_owned_drive_and_crash_recovery_still_resumes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_gate_env(dir.path());
    let req = TaskRunRequest {
        goal: "parked single-item run".to_string(),
        work_items: vec![WorkItem::new("a", "work a", WorkKind::Analysis)],
        parent_caps: read_caps(),
        ..Default::default()
    };
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("run starts");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let run_id = receipt.run_id.clone();
    let parent = env.parent;
    wait_until(|| env.executor.live_drive_count() == 1, 30).await;
    assert_eq!(env.executor.live_drive_runs(), vec![run_id.clone()]);
    // The provider parks INSIDE the model call: the drive cannot finish.
    wait_until(|| env.provider.entered.load(Ordering::SeqCst) >= 1, 30).await;
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = handle.task_id().unwrap();
    let task_before = handle.get_task(task_id).unwrap().expect("durable task row");
    assert!(!task_before.state.is_terminal());

    let started = Instant::now();
    let report = env
        .executor
        .shutdown_drives(Duration::from_millis(100))
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "shutdown drain stayed bounded"
    );
    assert!(report.total >= 1, "the live drive was owned: {report:?}");
    assert!(
        report.aborted >= 1,
        "the parked drive was aborted: {report:?}"
    );
    assert_eq!(report.unreaped, 0, "every drive reaped: {report:?}");
    assert_eq!(env.executor.live_drive_count(), 0);
    assert!(env.executor.drives_shutdown());

    // No durable state loss: the task row and the receipt-named linkage
    // row are still exactly there after the abort.
    let task = handle
        .get_task(task_id)
        .unwrap()
        .expect("task row survives");
    assert!(!task.state.is_terminal());
    assert!(
        handle
            .memory_facts()
            .unwrap()
            .iter()
            .any(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == &run_id),
        "the durable run linkage row survives the abort"
    );

    // CRASH: the graph is dropped and REOPENED over the same store —
    // exactly a daemon restart. (The in-process op registry dies with the
    // graph, so the reopened runtime sees a genuine interrupted turn, not
    // a "live in-process driver" fingerprint.)
    let provider = env.provider.clone();
    drop(env);
    let (manager2, agent2, _orchestrator2, _executor2) =
        build_gate_stack(dir.path(), provider.clone());
    let _ = provider.opened.send_replace(true);
    agent2.recover().expect("boot recovery pass");
    let handle2 = manager2.get_session(parent).unwrap().unwrap();
    assert!(
        handle2
            .events_range(0, Some(400))
            .unwrap()
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::CrashDetected),
        "the interrupted turn is crash residue"
    );

    // Recovery still resumes: the SAME recorded turn continues to a
    // genuine end once the provider gate opens.
    agent2
        .continue_turn(parent)
        .await
        .expect("the interrupted turn resumes");
    assert!(
        matches!(
            handle2.state().unwrap(),
            faktor_core::state::AgentState::ReadyForNextTurn
                | faktor_core::state::AgentState::Completed
        ),
        "the resumed turn reached a genuine end"
    );
    assert!(
        handle2
            .get_task(task_id)
            .unwrap()
            .is_some_and(|t| !t.goal.is_empty()),
        "the durable task row is intact after recovery"
    );
}

/// Orchestrated counterpart: the run's detached drive is registry-owned;
/// after shutdown aborts it, the durable plan + assignment rows survive
/// and a FRESH executor's `resume_run` re-attaches (the recovery entry
/// point) — the run was never silently dropped.
#[tokio::test]
async fn executor_shutdown_keeps_orchestrated_run_resumable() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_gate_env(dir.path());
    let req = TaskRunRequest {
        goal: "two-item parked run".to_string(),
        work_items: vec![
            WorkItem::new("a", "work a", WorkKind::Analysis),
            WorkItem::new("b", "work b", WorkKind::Analysis),
        ],
        parent_caps: read_caps(),
        ..Default::default()
    };
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("run starts");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
    let run_id = receipt.run_id.clone();
    wait_until(|| env.executor.live_drive_count() == 1, 30).await;
    assert_eq!(env.executor.live_drive_runs(), vec![run_id.clone()]);
    // Durable plan + assignment rows land BEFORE the first child spawn:
    // they are the recovery entry point `resume_run` re-attaches to.
    wait_until(
        || {
            !OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id)
                .unwrap_or_default()
                .is_empty()
        },
        30,
    )
    .await;
    let report = env
        .executor
        .shutdown_drives(Duration::from_millis(100))
        .await;
    assert!(report.total >= 1, "the live drive was owned: {report:?}");
    assert!(report.aborted >= 1, "{report:?}");
    assert_eq!(report.unreaped, 0, "{report:?}");
    assert_eq!(env.executor.live_drive_count(), 0);
    // The abort cut the drive, NOT the durable markers.
    assert!(
        !OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id)
            .unwrap()
            .is_empty(),
        "durable assignments survive the abort"
    );
    // A fresh executor re-attaches the same durable run.
    let executor2 = TaskExecutor::new_owner_direct_for_test_harness(
        &env.orchestrator,
        env.manager.clone(),
        env.agent.clone(),
    );
    executor2
        .resume_run(env.parent, &run_id, Ceilings::default(), read_caps(), None)
        .expect("re-attach accepted from the surviving durable rows");
    let _ = executor2.shutdown_drives(Duration::from_millis(50)).await;
    assert_eq!(executor2.live_drive_count(), 0);
}
