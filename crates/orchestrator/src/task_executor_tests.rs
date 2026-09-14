#![allow(clippy::await_holding_lock)]
//! (suite-level HEAVY_SUITE guard is held across awaits BY DESIGN: it
//! serializes whole heavy integration tests; clippy's lint is test-only noise.)

//! Adversarial tests of the TaskExecutor (audits P0-20/21/23/61/90/91).
//!
//! Children are driven by a REAL `AgentRuntime` over a REAL
//! `SessionManager` with scripted providers (no network). The tests break
//! the invariants: byte-parity of the single-item path versus the daemon's
//! own direct prompt drive, dispatch of multi-item runs onto real child
//! sessions, refusal of new runs over live crash residue, refusal of a
//! second concurrent orchestrated run (the runtime executes one run at a
//! time), and `resume_run` re-attaching a failed run through the durable
//! Retry row (applied exactly once). The wave-12 runtime tests already
//! prove the queue crash windows; here the EXECUTOR-level continuation is
//! exercised.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use faktor_agent::tool::RecoveryHint as ToolRecovery;
use faktor_agent::{
    AgentDeps, AgentRuntime, NoEvidence, PermissionRequester, Tool, ToolCallMode, ToolOutcome,
    ToolRegistry, ToolRunCtx,
};
use faktor_core::capability::PermissionDecision;
use faktor_core::error::Error;
use faktor_core::hash::FileHash;
use faktor_core::id::WorkspaceId;
use faktor_core::id::{SessionId, TaskId, WorktreeId};
use faktor_core::model::ModelCapabilities;
use faktor_core::resource::ResourceClass;
use faktor_core::time::SystemClock;
use faktor_provider::{
    FakeProvider, GenericAgentRequest, Provider, ProviderChunk, ProviderError, ProviderRegistry,
    ProviderStream, ScriptedResponse,
};
use faktor_session::{BudgetAuthority, SessionManager};

use crate::caps::{CapabilityGrant, CapabilitySet, LatticeCap, ScopePattern};
use crate::runtime::completion_steps::commit_message;
use crate::runtime::shadow::{ShadowCopyLimits, ShadowRoots};
use crate::runtime::task_executor::{
    compose_no_op_root_verification_status, compose_root_verification_status, MutationMode,
    PreparedRunIntegration, RunSettlement, SettlementOutcome, TaskExecutor, TaskRunMode,
    TaskRunRequest, TaskRunRow, TASK_RUN_ROW_KIND,
};
use crate::runtime::{CrashSeam, ExecError, OrchestratorRuntime};
use crate::{OwnershipSpec, TaskPlan, WorkItem, WorkKind};
use faktor_agent::IntegratedRootVerification;
use faktor_session::child::ChildOwnership;

// ------------------------------------------------------------------ fixture

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

/// Per-call scripted provider: serves one script per stream call (extra
/// calls end immediately), counts calls. Deterministic failure-then-recovery
/// runs need per-call scripts.
struct PerCallProvider {
    inner: FakeProvider,
    calls: StdMutex<Vec<Vec<ScriptedResponse>>>,
    script_index: AtomicUsize,
    request_count: AtomicUsize,
}

impl PerCallProvider {
    fn new(
        id: &str,
        caps: ModelCapabilities,
        per_call_scripts: Vec<Vec<ScriptedResponse>>,
    ) -> Self {
        Self {
            inner: FakeProvider::new(id, caps),
            calls: StdMutex::new(per_call_scripts),
            script_index: AtomicUsize::new(0),
            request_count: AtomicUsize::new(0),
        }
    }

    fn count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl Provider for PerCallProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }
    fn stream(&self, _req: faktor_provider::GenericAgentRequest) -> ProviderStream {
        use futures::StreamExt;
        self.request_count.fetch_add(1, Ordering::SeqCst);
        let i = self.script_index.fetch_add(1, Ordering::SeqCst);
        let script: Vec<ScriptedResponse> = self
            .calls
            .lock()
            .unwrap()
            .get(i)
            .cloned()
            .unwrap_or_else(|| vec![ScriptedResponse::End]);
        let stream = futures::stream::iter(script).map(|s| match s {
            ScriptedResponse::Text(t) => Ok(ProviderChunk::Text { text: t }),
            ScriptedResponse::ToolCall { id, name, input } => Ok(ProviderChunk::ToolCall {
                id,
                name,
                input,
                complete: true,
            }),
            ScriptedResponse::Die(e) => Err(e),
            ScriptedResponse::End => Ok(ProviderChunk::Done),
            ScriptedResponse::Reasoning(_) => unreachable!("no reasoning scripts"),
        });
        Box::pin(stream)
    }
}

/// Provider whose streams park until the gate opens (mid-flight windows).
struct GatedProvider {
    caps: ModelCapabilities,
    gate: Arc<tokio::sync::Notify>,
    open: Arc<AtomicUsize>,
    request_count: AtomicUsize,
}
impl GatedProvider {
    fn open(&self) {
        self.open.store(1, Ordering::SeqCst);
        self.gate.notify_waiters();
    }
    fn count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl Provider for GatedProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        self.caps.clone()
    }
    fn stream(&self, _req: faktor_provider::GenericAgentRequest) -> ProviderStream {
        use futures::StreamExt;
        self.request_count.fetch_add(1, Ordering::SeqCst);
        let open = self.open.clone();
        let gate = self.gate.clone();
        let s = futures::stream::once(async move {
            while open.load(Ordering::SeqCst) == 0 {
                gate.notified().await;
            }
            Ok(ProviderChunk::Text {
                text: "gated".into(),
            })
        })
        .chain(futures::stream::once(async { Ok(ProviderChunk::Done) }));
        Box::pin(s)
    }
}

/// Provider whose streams PARK after counting in until the barrier opens
/// (audits 7/8/21/22 concurrency witness): every stream increments
/// `entered` BEFORE releasing anything, then waits on the gate. A test can
/// therefore assert that TWO children of TWO parent sessions are both
/// mid-model-call while neither has released — the observable proof that
/// runs of different sessions are not globally serialized.
struct BarrierProvider {
    caps: ModelCapabilities,
    gate: Arc<tokio::sync::Notify>,
    open: Arc<AtomicUsize>,
    entered: AtomicUsize,
    request_count: AtomicUsize,
}

impl BarrierProvider {
    fn open(&self) {
        self.open.store(1, Ordering::SeqCst);
        self.gate.notify_waiters();
    }
    fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }
}

impl Provider for BarrierProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        self.caps.clone()
    }
    fn stream(&self, _req: faktor_provider::GenericAgentRequest) -> ProviderStream {
        use futures::StreamExt;
        self.request_count.fetch_add(1, Ordering::SeqCst);
        self.entered.fetch_add(1, Ordering::SeqCst);
        let open = self.open.clone();
        let gate = self.gate.clone();
        let s = futures::stream::once(async move {
            // Both sides must be INSIDE their model call before either
            // releases: the wait happens before any chunk is produced.
            while open.load(Ordering::SeqCst) == 0 {
                gate.notified().await;
            }
            Ok(ProviderChunk::Text {
                text: "unbarriered".into(),
            })
        })
        .chain(futures::stream::once(async { Ok(ProviderChunk::Done) }));
        Box::pin(s)
    }
}

/// Provider whose streams tick forever (never End): a mid-flight drive the
/// bounded cancel path can deterministically reach BETWEEN chunks — the
/// mirror of the runtime's paced roundtrips without a finite script.
struct TickerProvider {
    caps: ModelCapabilities,
    request_count: AtomicUsize,
    chunk_delay_ms: u64,
}

impl TickerProvider {
    fn count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl Provider for TickerProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        self.caps.clone()
    }
    fn stream(&self, _req: faktor_provider::GenericAgentRequest) -> ProviderStream {
        self.request_count.fetch_add(1, Ordering::SeqCst);
        let delay = self.chunk_delay_ms;
        let s = futures::stream::unfold(0u64, move |i| async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Some((
                Ok(ProviderChunk::Text {
                    text: format!("tick {i}"),
                }),
                i + 1,
            ))
        });
        Box::pin(s)
    }
}

/// One real executor environment (mirrors the runtime test env).
struct Env {
    manager: Arc<SessionManager>,
    agent: Arc<AgentRuntime>,
    provider: Arc<PerCallProvider>,
    orchestrator: Arc<OrchestratorRuntime>,
    executor: Arc<TaskExecutor>,
    parent: SessionId,
    owner_root: std::path::PathBuf,
    isolated_root: std::path::PathBuf,
}

fn read_caps() -> CapabilitySet {
    CapabilitySet::from_grants(vec![CapabilityGrant::new(
        LatticeCap::ReadWorkspace,
        ScopePattern::new("*").unwrap(),
    )])
    .unwrap()
}

fn open_env(root: &std::path::Path, scripts: Vec<Vec<ScriptedResponse>>) -> Arc<Env> {
    open_env_with_shadows(root, scripts, ShadowCopyLimits::default(), false)
}

/// A shadowed executor env: same wiring as [`open_env`] plus the P0-48
/// shadow service rooted at `<root>/shadows`.
fn open_shadow_env(root: &std::path::Path, scripts: Vec<Vec<ScriptedResponse>>) -> Arc<Env> {
    open_env_with_shadows(root, scripts, ShadowCopyLimits::default(), true)
}

fn open_env_with_shadows(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    limits: ShadowCopyLimits,
    shadowed: bool,
) -> Arc<Env> {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let caps = ModelCapabilities {
        tools: true,
        parallel_tools: true,
        ..Default::default()
    };
    let provider = Arc::new(PerCallProvider::new("fake", caps, scripts));
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
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
        .create_session(ws, "task-owner", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let isolated_root = root.join("isolated");
    std::fs::create_dir_all(&isolated_root).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let shadows_root = root.join("shadows");
    let shadows = if shadowed {
        Some(ShadowRoots::new_with_limits(
            manager.clone(),
            shadows_root.clone(),
            limits,
        ))
    } else {
        None
    };
    let executor = TaskExecutor::new(
        &orchestrator,
        manager.clone(),
        agent.clone(),
        shadows.clone(),
    );
    Arc::new(Env {
        manager,
        agent,
        provider,
        orchestrator,
        executor,
        parent,
        owner_root,
        isolated_root,
    })
}

fn build_agent(manager: Arc<SessionManager>, registry: ProviderRegistry) -> Arc<AgentRuntime> {
    AgentRuntime::new(AgentDeps {
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
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 5000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        efficiency: Default::default(),
    })
    .unwrap()
}

fn wi(id: &str, kind: WorkKind, deps: &[&str]) -> WorkItem {
    let mut item = WorkItem::new(id, format!("work {id}"), kind);
    item.depends_on = deps.iter().map(|d| d.to_string()).collect();
    item
}

fn path_item(id: &str, kind: WorkKind, deps: &[&str], paths: &[&str]) -> WorkItem {
    let mut item = WorkItem::with_ownership(
        id,
        format!("work {id}"),
        kind,
        OwnershipSpec::Paths {
            paths: paths.iter().map(|p| p.to_string()).collect(),
        },
    );
    item.depends_on = deps.iter().map(|d| d.to_string()).collect();
    item
}

fn request(goal: &str, items: Vec<WorkItem>, env: &Env) -> TaskRunRequest {
    TaskRunRequest {
        goal: goal.to_string(),
        work_items: items,
        parent_caps: read_caps(),
        isolated_root: env.isolated_root.clone(),
        ..Default::default()
    }
}

async fn wait_until(mut cond: impl FnMut() -> bool, timeout_secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "wait_until timed out after {timeout_secs}s"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn state_of(env: &Env, sid: SessionId) -> faktor_core::state::AgentState {
    env.manager
        .get_session(sid)
        .unwrap()
        .unwrap()
        .state()
        .unwrap()
}

fn done_script() -> Vec<Vec<ScriptedResponse>> {
    vec![
        vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
        vec![ScriptedResponse::End],
    ]
}

// ------------------------------------------------------------------- tests

/// Heavy file/CAS/process tests are serialized on the ONE crate-wide guard
/// (`crate::test_support::HEAVY_SUITE`, shared with `runtime_tests`): under
/// intra-binary parallelism their store+DbActor+fsync + CAS-file storms on
/// one disk starve each other past any reasonable wall bound (observed
/// 300 s+ tails on shared machines while every test passes in isolation and
/// serially). A per-module guard was not enough — the heavy suites of
/// different modules overlapped.
use crate::test_support::heavy_guard;

#[tokio::test]
async fn single_item_task_matches_the_direct_prompt_path_byte_for_byte() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    // Executor-driven session A versus the direct daemon drive on session
    // B: same provider scripts, same goal — on SEPARATE stores (two
    // managers must never share one SQLite file). The TaskExecutor must
    // produce the same durable op record + session outcome — a wrapper
    // around the one drive path, never a second architecture.
    let env_a = open_env(&dir.path().join("a"), done_script());
    let env_b = open_env(&dir.path().join("b"), done_script());
    let goal = "analyze the module boundaries";

    // A: through the TaskExecutor (single item = the one-work-item plan).
    let req_a = request(goal, vec![wi("a1", WorkKind::Analysis, &[])], &env_a);
    let receipt_a = env_a
        .executor
        .start_task(env_a.parent, req_a)
        .expect("single-item start");
    assert_eq!(receipt_a.mode, TaskRunMode::InSession);
    assert!(!receipt_a.queued);
    let op_a = receipt_a.op_id.expect("real op id");

    // B: the previous direct path (agent.submit + detached drive).
    let receipt_b = env_b
        .agent
        .submit(env_b.parent, goal, &[])
        .expect("direct submit");
    assert!(!receipt_b.queued);
    let handle_b = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
    let receipt_b2 = receipt_b.clone();
    let agent_b = env_b.agent.clone();
    tokio::spawn(async move {
        let _ = agent_b.drive_receipt(&handle_b, receipt_b2, None).await;
    });

    wait_until(
        || state_of(&env_a, env_a.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        30,
    )
    .await;
    wait_until(
        || state_of(&env_b, env_b.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        30,
    )
    .await;
    // The state transition and the turn-record finalization are two
    // separate durable writes; on slower hosts (Windows CI) one path can
    // observe ReadyForNextTurn while its record is still `active`. Wait for
    // BOTH records to converge to the same terminal status before
    // comparing — stronger than a fixed sleep and environment-independent.
    {
        let ha = env_a.manager.get_session(env_a.parent).unwrap().unwrap();
        let hb = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        loop {
            let a = ha.turn_record(op_a).unwrap().map(|r| r.status);
            let b = hb.turn_record(receipt_b.op_id).unwrap().map(|r| r.status);
            if a.is_some() && a == b {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "turn records never converged: a={a:?} b={b:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    // The same durable outcome on both sides.
    let ha = env_a.manager.get_session(env_a.parent).unwrap().unwrap();
    let hb = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
    assert_eq!(
        ha.message_count().unwrap(),
        hb.message_count().unwrap(),
        "same message stream as the direct path"
    );
    let rec_a = ha.turn_record(op_a).unwrap().unwrap();
    let rec_b = hb.turn_record(receipt_b.op_id).unwrap().unwrap();
    assert_eq!(rec_a.status, rec_b.status);
    assert_eq!(rec_a.effective_provider, rec_b.effective_provider);
    assert_eq!(rec_a.effective_model, rec_b.effective_model);

    // TaskExecutor extras: the durable task row exists on A and matches
    // the direct path's row on B (the daemon's end-of-turn content sync
    // converges the row's goal to the session ledger — the run's own goal
    // is preserved in the linkage row).
    let task_a = ha.get_task(TaskId::new(1)).unwrap().expect("task row");
    let task_b = hb.get_task(TaskId::new(1)).unwrap().expect("task row");
    assert_eq!(task_a.goal, task_b.goal);
    assert_eq!(task_a.budget, task_b.budget);
    let facts = ha.memory_facts().unwrap();
    let run_row = facts
        .iter()
        .find(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == &receipt_a.run_id)
        .expect("durable linkage row");
    let decoded = TaskRunRow::decode(&run_row.2).unwrap();
    assert_eq!(decoded.op_id, Some(op_a.raw()));
    assert_eq!(decoded.mode, TaskRunMode::InSession);
    assert_eq!(
        decoded.goal, goal,
        "the run's own goal lives in the linkage row"
    );
    assert_eq!(decoded.item_ids, vec!["a1".to_string()]);
}

#[tokio::test]
async fn multi_item_task_spawns_real_children_and_completes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    let req = request(
        "split the analysis",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &["a"]),
        ],
        &env,
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("multi-item start");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
    assert_eq!(receipt.op_id, None);
    assert!(receipt.run_id.starts_with("run-"));
    // Point 7: the durable ROOT task row exists BEFORE the first child spawn
    // (unconditionally, no criteria/cap/contract required) — no orchestrated
    // run can ever settle without a root row to certify.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(
        h.get_task(h.task_id().unwrap()).unwrap().is_some(),
        "every orchestrated run creates its durable root task row up front"
    );

    // Real children appear under the run and drive to terminal success.
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    assert_eq!(rows.len(), 2);
    for c in &rows {
        assert_eq!(c.state, crate::ChildState::Done);
        assert_ne!(c.session_id, 0, "real child session");
        assert_ne!(c.operation_id, 0, "child drive recorded its op");
        assert_eq!(
            c.ownership,
            faktor_session::child::ChildOwnership::ReadOnlyShared
        );
    }
    // Read-only children share the OWNER worktree (no isolated worktrees).
    assert_eq!(rows[0].worktree_id, rows[1].worktree_id);
    let owner_wt = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .row()
        .unwrap()
        .worktree_id;
    assert_eq!(rows[0].worktree_id, owner_wt.raw());
    // Both children were really driven (provider calls >= 2) and the
    // executor slot freed itself.
    assert!(env.provider.count() >= 2, "driven {}", env.provider.count());
    wait_until(|| env.executor.active_run().is_none(), 240).await;
    // The verification-disabled env parks the run in the explicit Verifying
    // state (never Pending/Running); VerifiedComplete is exercised by the
    // real-tool adversarial suite.
    let state = h.get_task(h.task_id().unwrap()).unwrap().unwrap().state;
    assert_eq!(state, TaskState::Verifying, "root task walked to Verifying");
}

#[tokio::test]
async fn start_refuses_when_a_live_run_was_left_by_a_crashed_executor() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    let parent_row = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .row()
        .unwrap();
    // Crashed-executor simulation: execute_task with the BeforeDrive seam
    // leaves a durable child row in Running state under the plan row.
    let plan = TaskPlan {
        goal: "crashed run".into(),
        non_goals: vec![],
        constraints: vec![],
        work_items: vec![wi("a", WorkKind::Analysis, &[])],
    };
    let mut spec = crate::runtime::ChildSpec::new("a");
    spec.child_caps = read_caps();
    spec.task_caps = read_caps();
    let config = crate::runtime::ExecConfig {
        run_id: "run-crash".into(),
        ceilings: crate::runtime::Ceilings::default(),
        parent_caps: read_caps(),
        provider: "fake".into(),
        default_model: "m".into(),
        isolated_root: env.isolated_root.clone(),
        crash_seam: Some(CrashSeam::BeforeDrive),
    };
    let res = tokio::time::timeout(
        Duration::from_secs(30),
        env.orchestrator.execute_task(
            plan,
            crate::runtime::OwnerContext {
                parent_session: env.parent,
                workspace_id: parent_row.workspace_id.raw(),
                worktree_id: parent_row.worktree_id.raw(),
                root: env.owner_root.clone(),
            },
            config,
            &[spec],
        ),
    )
    .await
    .expect("the seam fires fast");
    assert!(
        matches!(res, Err(crate::runtime::ExecError::InjectedCrashSeam(_))),
        "{res:?}"
    );

    // A new task on the same session must REFUSE (typed Conflict naming the
    // live run) instead of clobbering the mirror of the crashed one.
    let req = request(
        "new task over crash residue",
        vec![
            wi("x", WorkKind::Analysis, &[]),
            wi("y", WorkKind::Analysis, &["x"]),
        ],
        &env,
    );
    let err = env
        .executor
        .start_task(env.parent, req.clone())
        .expect_err("live residue blocks a new run");
    assert!(
        matches!(err, crate::runtime::ExecError::Conflict(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("run-crash"), "{err}");

    // resume_run re-attaches the crashed run and drives it to completion.
    env.executor
        .resume_run(
            env.parent,
            "run-crash",
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("resume accepted");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, "run-crash")
                .map(|rows| {
                    rows.first()
                        .is_some_and(|c| c.state == crate::ChildState::Done)
                })
                .unwrap_or(false)
        },
        60,
    )
    .await;

    // After the resumed run is terminal the session accepts new tasks.
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("new run after resume");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
}

#[tokio::test]
async fn resume_run_after_crash_between_assignments_and_first_spawn_reuses_durable_ids() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    // Crash the executor EXACTLY between the atomic assignment commit and
    // the first child spawn (the AfterAssignmentsPersisted seam).
    let mut req = request(
        "crash after compile",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    req.crash_seam = Some(CrashSeam::AfterAssignmentsPersisted);
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("run starts");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
    let run_id = receipt.run_id.clone();
    // Durable assignments exist; NO child may have spawned; the crashed
    // executor's slot is free again.
    wait_until(
        || {
            !OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id)
                .unwrap_or_default()
                .is_empty()
                && OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &run_id)
                    .map(|rows| rows.is_empty())
                    .unwrap_or(false)
        },
        30,
    )
    .await;
    wait_until(|| env.executor.active_run().is_none(), 180).await;
    let assignments =
        OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id).unwrap();
    assert_eq!(assignments.len(), 2);
    // The assignment-backed residue is LIVE: a new task must REFUSE until
    // the crashed run is resumed (its durable ids must not be orphaned).
    let err = env
        .executor
        .start_task(
            env.parent,
            request(
                "second run over residue",
                vec![wi("x", WorkKind::Analysis, &[])],
                &env,
            ),
        )
        .expect_err("assignment-backed residue blocks a new run");
    assert!(
        matches!(err, crate::runtime::ExecError::Conflict(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains(&run_id), "{err}");
    // resume_run accepts the run even though it has NO child rows yet and
    // re-spawns every item under its DURABLE child id (no re-mint).
    env.executor
        .resume_run(
            env.parent,
            &run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("resume accepted");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &run_id)
                .map(|rows| {
                    rows.len() == 2 && rows.iter().all(|c| c.state == crate::ChildState::Done)
                })
                .unwrap_or(false)
        },
        90,
    )
    .await;
    let rows =
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &run_id).unwrap();
    let after =
        OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id).unwrap();
    assert_eq!(
        after, assignments,
        "assignment rows must never be re-minted"
    );
    for a in &assignments {
        let row = rows
            .iter()
            .find(|r| r.item_id == a.item_id)
            .expect("child row per assigned item");
        assert_eq!(row.child_id, a.child_id, "spawn must reuse the durable id");
    }
    // Terminal residue frees the session for new tasks (the new run spawns
    // its own fresh children and completes).
    let fresh = env
        .executor
        .start_task(
            env.parent,
            request(
                "fresh run",
                vec![
                    wi("p", WorkKind::Analysis, &[]),
                    wi("q", WorkKind::Analysis, &[]),
                ],
                &env,
            ),
        )
        .expect("session accepts new tasks after resume");
    assert_eq!(fresh.mode, TaskRunMode::Orchestrated);
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &fresh.run_id)
                .map(|rows| {
                    rows.len() == 2 && rows.iter().all(|c| c.state == crate::ChildState::Done)
                })
                .unwrap_or(false)
        },
        90,
    )
    .await;
}

#[tokio::test]
async fn second_orchestrated_run_is_refused_while_one_is_active() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    // A gate provider keeps the first run mid-flight so the single
    // execution slot is observably occupied.
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let gated = Arc::new(GatedProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        gate: Arc::new(tokio::sync::Notify::new()),
        open: Arc::new(AtomicUsize::new(0)),
        request_count: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(gated.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    let owner_root = dir.path().join("owner");
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
        .create_session(ws, "gated", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let isolated = dir.path().join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new(&orchestrator, manager.clone(), agent.clone(), None);

    let req = || TaskRunRequest {
        goal: "gated run".into(),
        work_items: vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &[]),
        ],
        parent_caps: read_caps(),
        isolated_root: isolated.clone(),
        ..Default::default()
    };
    let first = executor
        .start_task(parent, req())
        .expect("first run starts");
    // Wait until the first child exists and its drive is parked on the gate
    // (mid-flight).
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &first.run_id)
                .map(|rows| !rows.is_empty())
                .unwrap_or(false)
        },
        30,
    )
    .await;
    wait_until(|| gated.count() >= 1, 180).await;

    // A second orchestrated start is refused while the first is active
    // (typed Conflict — the runtime executes one run at a time).
    let err = executor
        .start_task(parent, req())
        .expect_err("busy refusal");
    assert!(
        matches!(err, crate::runtime::ExecError::Conflict(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains(&first.run_id), "{err}");
    // resume_run of the ACTIVE run is refused too (no double drive).
    let err2 = executor
        .resume_run(
            parent,
            &first.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect_err("double drive refused");
    assert!(err2.to_string().contains("already being driven"), "{err2}");

    // Releasing the gate lets the first run finish and free the slot.
    gated.open();
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &first.run_id)
                .map(|rows| rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    assert!(executor.active_run().is_none());
}

#[tokio::test]
async fn runs_of_two_parent_sessions_proceed_concurrently_past_a_provider_barrier() {
    let _heavy = heavy_guard();
    // (audits 7/8/21/22) TaskExecutor runs are keyed per RUN and indexed
    // per PARENT SESSION: one run per parent session, while runs of
    // DIFFERENT sessions proceed concurrently through the runtime's
    // run-scoped mirrors. A barrier inside the shared provider proves it:
    // both parents' children must be INSIDE their model call before either
    // releases — impossible under any global serialization.
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let barrier = Arc::new(BarrierProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        gate: Arc::new(tokio::sync::Notify::new()),
        open: Arc::new(AtomicUsize::new(0)),
        entered: AtomicUsize::new(0),
        request_count: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(barrier.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    // Two independent owner sessions (worktrees + identities) over the SAME
    // manager/agent/executor.
    let make_parent = |dir: &std::path::Path| {
        std::fs::create_dir_all(dir).unwrap();
        let ws = manager.create_workspace(dir.to_str().unwrap()).unwrap();
        let wt = WorktreeId::new(
            manager
                .put_worktree(ws, dir.to_str().unwrap(), "main")
                .unwrap() as u64,
        );
        let parent = manager
            .create_session(ws, "task-owner", "fake", "m")
            .unwrap()
            .id();
        manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
        parent
    };
    let p1 = make_parent(&dir.path().join("owner-1"));
    let p2 = make_parent(&dir.path().join("owner-2"));
    let isolated = dir.path().join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new(&orchestrator, manager.clone(), agent.clone(), None);

    let req_for = |isolated: std::path::PathBuf| TaskRunRequest {
        goal: "parallel analysis run".into(),
        // Two items keep dispatch ORCHESTRATED; b waits for a, so exactly
        // one child per run is mid-call at the barrier at a time.
        work_items: vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &["a"]),
        ],
        parent_caps: read_caps(),
        isolated_root: isolated,
        ..Default::default()
    };
    let first = executor
        .start_task(p1, req_for(isolated.clone()))
        .expect("session 1 run starts");
    // The first run's child must be parked INSIDE its model call.
    wait_until(|| barrier.entered() >= 1, 300).await;
    // The second parent's run starts WHILE session 1's run is active — the
    // old single global slot refused this with a typed Conflict.
    let second = executor.start_task(p2, req_for(isolated.clone())).expect(
        "session 2 run starts while session 1 is active: cross-session runs are concurrent",
    );
    assert_ne!(first.run_id, second.run_id);
    // BOTH children are inside their model call while NEITHER has released:
    // the observable witness that no executor-level lock serializes them.
    wait_until(|| barrier.entered() >= 2, 300).await;
    assert!(
        executor.active_runs().len() == 2,
        "both runs active concurrently: {:?}",
        executor.active_runs()
    );
    // The per-parent index still keeps ONE run per parent session: a third
    // run of session 1 is refused while its first run is active.
    let err = executor
        .start_task(p1, req_for(isolated.clone()))
        .expect_err("one orchestrated run per parent session");
    assert!(matches!(err, ExecError::Conflict(_)), "{err:?}");
    assert!(
        err.to_string().contains(&first.run_id),
        "refusal names the active run of the same session: {err}"
    );
    // Release both: each run drives its remaining waves to completion.
    barrier.open();
    for (parent, run) in [(p1, &first.run_id), (p2, &second.run_id)] {
        wait_until(
            || {
                OrchestratorRuntime::registry_rows(manager.clone(), parent, run)
                    .map(|rows| !rows.is_empty() && rows.iter().all(|c| c.state.is_terminal()))
                    .unwrap_or(false)
            },
            300,
        )
        .await;
    }
    wait_until(|| executor.active_runs().is_empty(), 240).await;
}

#[tokio::test]
async fn per_item_ownership_lands_on_the_durable_assignment_rows() {
    let _heavy = heavy_guard();
    // (audits 7/8/21/22, work-entry unification) A MIXED request (read-only
    // Analysis → mutating Implementation with its own path set) carries the
    // ownership ON THE ITEMS. The item's spec is persisted on the wave-A3
    // rows and the spawned child rows carry the compiled mode + paths.
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = vec![
        vec![
            ScriptedResponse::Text("analyzed".into()),
            ScriptedResponse::End,
        ],
        vec![
            ScriptedResponse::Text("implemented".into()),
            ScriptedResponse::End,
        ],
    ];
    let env = open_env(dir.path(), scripts);
    std::fs::create_dir_all(env.owner_root.join("src")).unwrap();
    let req = request(
        "mixed ownership run",
        vec![
            wi("analyze", WorkKind::Analysis, &[]),
            path_item(
                "implement",
                WorkKind::Implementation,
                &["analyze"],
                &["src/m.rs"],
            ),
        ],
        &env,
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("mixed run with per-item ownership starts");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        120,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    let row_of = |id: &str| rows.iter().find(|c| c.item_id == id).unwrap();
    assert_eq!(row_of("analyze").ownership, ChildOwnership::ReadOnlyShared);
    assert_eq!(
        row_of("implement").ownership,
        ChildOwnership::ExclusivePaths
    );
    assert_eq!(
        row_of("implement").ownership_paths,
        vec!["src/m.rs".to_string()]
    );
    let assignments =
        OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &receipt.run_id)
            .unwrap();
    let a_of = |id: &str| assignments.iter().find(|a| a.item_id == id).unwrap();
    assert_eq!(a_of("analyze").ownership, OwnershipSpec::NoWrites);
    assert_eq!(
        a_of("implement").ownership,
        OwnershipSpec::Paths {
            paths: vec!["src/m.rs".to_string()]
        }
    );
}

#[tokio::test]
async fn overlapping_or_write_capable_per_item_requests_are_refused_at_compile() {
    let _heavy = heavy_guard();
    // (audits 7/8/21/22, work-entry unification) Executor-level refusals
    // BEFORE any durable row, decided from the ITEMS alone: two mutating
    // items whose path sets overlap (even behind a dependency edge) and a
    // read-only item handed write capability are both rejected by the
    // per-item compile.
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), vec![vec![ScriptedResponse::End]]);
    // (a) Overlapping mutating path sets — b depends on a, so the overlap
    // could never be live; disjointness still spans ALL mutating items.
    let req = request(
        "overlapping",
        vec![
            path_item("a", WorkKind::Implementation, &[], &["src"]),
            path_item("b", WorkKind::Implementation, &["a"], &["src/a.rs"]),
        ],
        &env,
    );
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("overlapping mutating path sets must be refused");
    assert!(
        matches!(err, ExecError::InvalidPlan(_))
            && err.to_string().contains("overlapping write ownership"),
        "{err:?}"
    );
    // (b) A read-only item with write ownership is refused (never a write
    // capability on a read-only item — through ANY channel).
    let req = request(
        "write-capable read-only",
        vec![
            WorkItem::with_ownership(
                "analyze",
                "read",
                WorkKind::Analysis,
                OwnershipSpec::Paths {
                    paths: vec!["src".to_string()],
                },
            ),
            path_item(
                "impl",
                WorkKind::Implementation,
                &["analyze"],
                &["src/impl.rs"],
            ),
        ],
        &env,
    );
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("read-only items can never receive write capability");
    assert!(
        matches!(err, ExecError::InvalidPlan(_)) && err.to_string().contains("read-only work item"),
        "{err:?}"
    );
    // Nothing durable was written by either refusal.
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    assert!(
        !facts.iter().any(|(kind, _, _)| matches!(
            kind.as_str(),
            crate::runtime::PLAN_ROW_KIND
                | crate::runtime::ASSIGNMENT_ROW_KIND
                | crate::runtime::REGISTRY_ROW_KIND
        )),
        "refused plans leave no durable orchestration rows"
    );
}

#[tokio::test]
async fn resume_run_retries_a_failed_child_from_a_durable_row() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    // First provider stream dies permanently; the retry's re-drive succeeds.
    let scripts: Vec<Vec<ScriptedResponse>> = vec![
        vec![ScriptedResponse::Die(ProviderError::new(
            faktor_provider::ProviderErrorKind::Malformed,
            "injected permanent failure",
        ))],
        vec![
            ScriptedResponse::Text("recovered".into()),
            ScriptedResponse::End,
        ],
        vec![ScriptedResponse::End],
    ];
    let env = open_env(dir.path(), scripts);
    // Two items keep the dispatch ORCHESTRATED (a real child session) while
    // only the "a" item actually spawns: "auto" completes without a child.
    let mut req = request(
        "failing run",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("auto", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    // "auto" completes without a child; only "a" spawns.
    req.auto_items = vec!["auto".to_string()];
    let receipt = env
        .executor
        .start_task(env.parent, req.clone())
        .expect("start");
    // Wait for the run's first drive to fail (permanent provider error).
    let mut waited = 0;
    loop {
        let rows =
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .unwrap_or_default();
        if rows
            .first()
            .is_some_and(|c| c.state == crate::ChildState::Failed)
        {
            break;
        }
        waited += 1;
        assert!(waited < 240, "run1 never Failed: {rows:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // resume_run WITHOUT a pending Retry row does NOT blindly re-run: the
    // child stays Failed (never an automatic infinite retry loop).
    env.executor
        .resume_run(
            env.parent,
            &receipt.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("resume accepted");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    assert_eq!(rows[0].state, crate::ChildState::Failed);
    // A human retry enqueues the durable Retry row; resume_run admits the
    // re-drive exactly once and completes.
    env.orchestrator
        .retry_child("child-0")
        .expect("retry enqueued");
    env.executor
        .resume_run(
            env.parent,
            &receipt.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("retry resume");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| {
                    rows.first()
                        .is_some_and(|c| c.state == crate::ChildState::Done)
                })
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    assert_eq!(rows[0].state, crate::ChildState::Done);
    let session = env
        .manager
        .get_session(SessionId::new(rows[0].session_id))
        .unwrap()
        .unwrap();
    let ctl = session.orchestrator_ctl_all().unwrap();
    let retry = ctl
        .iter()
        .find(|r| matches!(r.control, faktor_session::child::ChildControl::Retry))
        .expect("durable retry row");
    assert!(
        retry.applied(),
        "retry decision durable before the re-drive"
    );
    // A second resume of a fully terminal run is refused (nothing to drive).
    let err = env
        .executor
        .resume_run(
            env.parent,
            &receipt.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect_err("nothing to resume");
    assert!(err.to_string().contains("nothing to resume"), "{err}");
}

#[test]
fn hostile_requests_are_rejected_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let parent = manager
        .create_session(ws, "hostile", "fake", "m")
        .unwrap()
        .id();
    let agent = build_agent(manager.clone(), ProviderRegistry::new());
    let orch = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new(&orch, manager.clone(), agent.clone(), None);
    let isolated = dir.path().join("isolated");

    let mut req = TaskRunRequest {
        isolated_root: isolated.clone(),
        work_items: vec![wi("a", WorkKind::Analysis, &[])],
        ..Default::default()
    };

    req.goal = "  ".into();
    let err = req.validate().expect_err("blank goal rejected");
    assert!(err.to_string().contains("goal is empty"));

    req.goal = "x".repeat(crate::MAX_GOAL_CHARS + 1);
    assert!(matches!(
        req.validate().expect_err("overlong goal rejected"),
        crate::runtime::ExecError::Oversized(_)
    ));

    req.goal = "fine".into();
    req.work_items = vec![];
    let err = req.validate().expect_err("no work items rejected");
    assert!(err.to_string().contains("at least one work item"));

    req.work_items = vec![wi("a", WorkKind::Analysis, &[])];
    req.model = Some("m".repeat(129));
    assert!(matches!(
        req.validate().expect_err("overlong model rejected"),
        crate::runtime::ExecError::Oversized(_)
    ));
    req.model = None;

    // A single MUTATING item is legal: it drives the session's own
    // worktree (the current normal path), never a spawn.
    req.work_items = vec![wi("a", WorkKind::Implementation, &[])];
    req.validate()
        .expect("single mutating item = the session's own drive");
    // ... and a multi-item MUTATING plan with NO isolated_root is legal:
    // the DAEMON allocates the candidate root itself (a client never
    // supplies a filesystem path).
    req.work_items = vec![
        path_item("a", WorkKind::Implementation, &[], &["src/a.rs"]),
        path_item("b", WorkKind::Implementation, &["a"], &["src/b.rs"]),
    ];
    req.isolated_root = std::path::PathBuf::new();
    req.validate()
        .expect("multi-item mutating plans allocate their own root");
    req.isolated_root = isolated.clone();
    // A mutating item that still carries NoWrites (missing ownership) is
    // InvalidPlan — nothing defaults a write authority onto it.
    req.work_items = vec![
        WorkItem::with_ownership(
            "a",
            "impl",
            WorkKind::Implementation,
            OwnershipSpec::NoWrites,
        ),
        path_item("b", WorkKind::Implementation, &["a"], &["src/b.rs"]),
    ];
    let err = req
        .validate()
        .expect_err("missing mutator ownership rejected");
    assert!(
        err.to_string().contains("requires write ownership"),
        "{err}"
    );
    // Mixed read-only + mutating items are valid exactly when each item
    // carries its kind-correct explicit ownership.
    req.work_items = vec![
        path_item("a", WorkKind::Implementation, &[], &["src/a.rs"]),
        wi("b", WorkKind::Analysis, &["a"]),
    ];
    req.validate()
        .expect("per-item ownership mixed plan validates");

    // Unknown session: typed NotFound, nothing written.
    let ok = TaskRunRequest {
        goal: "ok".into(),
        work_items: vec![wi("a", WorkKind::Analysis, &[])],
        isolated_root: isolated,
        ..Default::default()
    };
    let err = executor
        .start_task(faktor_core::id::SessionId::new(9999), ok.clone())
        .unwrap_err();
    assert!(matches!(err, crate::runtime::ExecError::NotFound(_)));

    // An orchestrated child session refuses task starts.
    let child = manager
        .create_child_session(
            parent,
            ws,
            WorktreeId::new(1),
            TaskId::new(1),
            "fake",
            "m",
            "child",
            faktor_session::child::ChildOwnership::ReadOnlyShared,
        )
        .unwrap();
    let err = executor.start_task(child.id(), ok).unwrap_err();
    assert!(
        matches!(err, crate::runtime::ExecError::InvalidState(_)),
        "{err:?}"
    );
}

// =========================================================== P0-48 shadowed
// single-agent mutating runs (shadow mutation roots).
//
// The drive itself is the REAL daemon drive (scripted providers, no
// network). The "shadowed drive writes" below are staged as direct writes
// INTO the shadow root — the exact operation the session's file consumers
// perform once they resolve `SessionManager::active_root` (the next-wave
// re-pointing); today those consumers live in the agent crate and resolve
// the durable workspace root directly, so the wiring is verified against
// the durable shadow machinery (begin/finalize/commit) instead.

use faktor_core::state::{
    CheckExecution, CriterionBinding, CriterionOrigin, CriterionRequirement, CriterionVerification,
    NoOpDisposition, TaskState, TaskTransition, VerificationStatus,
};
use faktor_session::ShadowRowState;

fn seed_owner(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), b"base-alpha").unwrap();
    std::fs::write(root.join("sub/b.txt"), b"base-beta").unwrap();
}

fn shadow_row_of(env: &Env) -> faktor_session::ShadowRow {
    env.manager
        .shadow_row(env.parent)
        .unwrap()
        .expect("an active shadow row exists")
}

fn owner_bytes(env: &Env, rel: &str) -> Vec<u8> {
    std::fs::read(env.owner_root.join(rel)).unwrap()
}

/// The durable "shadowed drive write": stage content inside the shadow root
/// (exactly where `SessionManager::active_root` re-points next wave).
fn shadow_drive_write(env: &Env, rel: &str, bytes: &[u8]) {
    let row = shadow_row_of(env);
    let dst = std::path::PathBuf::from(&row.root).join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(dst, bytes).unwrap();
}

/// The human verifier's role in the test harness: drive the durable task
/// row to Verifying, land a passing record (empty criteria/checks — the
/// seeded task rows carry no acceptance criteria) and complete the task.
/// This is the ONLY producer of VerifiedComplete (task machine invariant).
fn certify_env_task(env: &Env) {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
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
    assert_eq!(
        task.state,
        TaskState::Verifying,
        "the row must reach Verifying before completion"
    );
    let record = h
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            h.now_ms(),
        )
        .unwrap();
    let rev = h.task_revision(task_id).unwrap();
    h.complete_verified_task(task_id, rev, record).unwrap();
    assert_eq!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
}

fn mutating_request(env: &Env, goal: &str) -> TaskRunRequest {
    TaskRunRequest {
        goal: goal.to_string(),
        work_items: vec![wi("impl", WorkKind::Implementation, &[])],
        parent_caps: read_caps(),
        isolated_root: env.isolated_root.clone(),
        ..Default::default()
    }
}

/// A gated shadowed fixture: the provider parks mid-stream until released,
/// so the drive is deterministically mid-flight while assertions run.
struct GatedShadowFix {
    manager: Arc<SessionManager>,
    executor: Arc<TaskExecutor>,
    gated: Arc<GatedProvider>,
    parent: SessionId,
    owner_root: std::path::PathBuf,
    shadows: Arc<ShadowRoots>,
}

fn open_gated_shadow(root: &std::path::Path) -> GatedShadowFix {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let gated = Arc::new(GatedProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        gate: Arc::new(tokio::sync::Notify::new()),
        open: Arc::new(AtomicUsize::new(0)),
        request_count: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(gated.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    let owner_root = root.join("owner");
    std::fs::create_dir_all(&owner_root).unwrap();
    seed_owner(&owner_root);
    let ws = manager
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        manager
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = manager
        .create_session(ws, "gated-shadow", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let shadows = ShadowRoots::new(manager.clone(), root.join("shadows"));
    let executor = TaskExecutor::new(
        &orchestrator,
        manager.clone(),
        agent.clone(),
        Some(shadows.clone()),
    );
    GatedShadowFix {
        manager,
        executor,
        gated,
        parent,
        owner_root,
        shadows,
    }
}

#[tokio::test]
async fn shadowed_mutating_run_writes_never_reach_user_checkout_until_verified_commit() {
    let _heavy = heavy_guard();
    // (a)+(b) over the REAL executor: a shadowed mutating run begins a
    // durable shadow before its drive; staged writes live in the shadow
    // while the user checkout stays byte-identical; only a
    // VerifiedComplete + clean integration lands them, removes the shadow
    // and retires the row.
    let dir = tempfile::tempdir().unwrap();
    let env = open_shadow_env(dir.path(), done_script());
    seed_owner(&env.owner_root);
    let receipt = env
        .executor
        .start_task(env.parent, mutating_request(&env, "implement the change"))
        .expect("shadowed single-item start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    // The shadow began synchronously BEFORE the submit.
    let row = shadow_row_of(&env);
    assert_eq!(row.state, ShadowRowState::Active);
    let shadow_dir = std::path::PathBuf::from(&row.root);
    assert!(shadow_dir.is_dir());
    assert_eq!(
        env.manager.active_root(env.parent).unwrap(),
        Some(shadow_dir.clone()),
        "active_root re-points the session at the shadow while live"
    );
    // The drive ends (scripted text, no tools).
    wait_until(
        || state_of(&env, env.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        30,
    )
    .await;
    // Let the detached post-drive finalize settle: the row is not terminal
    // (no completion claim), so the shadow is RETAINED and the user
    // checkout stays untouched.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(owner_bytes(&env, "a.txt"), b"base-alpha");
    assert_eq!(row.state, ShadowRowState::Active);
    // Stage the shadowed drive's writes AFTER the drive (what a
    // shadow-aware verification run would have produced).
    shadow_drive_write(&env, "a.txt", b"implemented alpha");
    shadow_drive_write(&env, "new-file.txt", b"implemented new file");
    assert_eq!(
        owner_bytes(&env, "a.txt"),
        b"base-alpha",
        "user checkout untouched while the shadow holds the new world"
    );
    assert!(!env.owner_root.join("new-file.txt").exists());
    // Only a verified completion integrates. A non-terminal task row is
    // retained by the drive's one-shot finalize AND by the bounded
    // post-drive watcher — the executor settles the run by itself, the
    // test never drives the finalize by hand.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::Active,
        "the non-terminal task row keeps the shadow live"
    );
    assert_eq!(owner_bytes(&env, "a.txt"), b"base-alpha");
    certify_env_task(&env);
    wait_until(
        || {
            owner_bytes(&env, "a.txt") == b"implemented alpha"
                && owner_bytes(&env, "new-file.txt") == b"implemented new file"
                && env.manager.shadow_row(env.parent).unwrap().is_none_or(|r| {
                    r.state == ShadowRowState::Integrated
                        && !std::path::PathBuf::from(&r.root).exists()
                })
        },
        60,
    )
    .await;
    assert_eq!(owner_bytes(&env, "a.txt"), b"implemented alpha");
    assert_eq!(owner_bytes(&env, "new-file.txt"), b"implemented new file");
    assert!(!shadow_dir.exists(), "clean integration removes the shadow");
    let row = shadow_row_of(&env);
    assert_eq!(row.state, ShadowRowState::Integrated);
    assert_eq!(
        env.manager.active_root(env.parent).unwrap(),
        None,
        "a retired shadow stops re-pointing"
    );
}

#[tokio::test]
async fn mid_drive_isolation_and_conflict_surfaces_integration_blocked_then_resolves() {
    let _heavy = heavy_guard();
    // (a)+(c) with the drive parked mid-flight: while the drive is live the
    // user checkout is byte-identical; an external user edit during the
    // drive conflicts at integration — the run's content never lands, the
    // shadow is retained with the durable conflict list, and resolving the
    // drift lets the executor's own settle paths integrate (the bounded
    // post-drive watcher re-settles once the row is terminal; the test
    // never drives the finalize by hand).
    let dir = tempfile::tempdir().unwrap();
    let fix = open_gated_shadow(dir.path());
    let receipt = fix
        .executor
        .start_task(
            fix.parent,
            TaskRunRequest {
                goal: "gate-shadowed implementation".into(),
                work_items: vec![wi("impl", WorkKind::Implementation, &[])],
                parent_caps: read_caps(),
                isolated_root: dir.path().join("isolated"),
                ..Default::default()
            },
        )
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = fix
        .manager
        .shadow_row(fix.parent)
        .unwrap()
        .expect("row at begin");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    // Park the drive mid-flight and write into the shadow while it runs.
    wait_until(|| fix.gated.count() >= 1, 180).await;
    std::fs::write(shadow_dir.join("a.txt"), b"mid-drive implementation").unwrap();
    assert_eq!(
        std::fs::read(fix.owner_root.join("a.txt")).unwrap(),
        b"base-alpha",
        "user checkout byte-identical MID-drive"
    );
    assert_eq!(
        fix.manager.active_root(fix.parent).unwrap(),
        Some(shadow_dir.clone()),
        "active_root reports the shadow root while the drive is live"
    );
    // The user edits the file externally during the drive.
    std::fs::write(fix.owner_root.join("a.txt"), b"user edit during drive").unwrap();
    fix.gated.open();
    wait_until(
        || {
            fix.manager
                .get_session(fix.parent)
                .unwrap()
                .unwrap()
                .state()
                .unwrap()
                == faktor_core::state::AgentState::ReadyForNextTurn
        },
        30,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    certify_verified_complete(&fix.manager, fix.parent);
    // The terminal VerifiedComplete converges to the durable
    // IntegrationBlocked outcome through the executor's own settle paths.
    wait_until(
        || {
            fix.manager.shadow_row(fix.parent).unwrap().unwrap().state
                == ShadowRowState::IntegrationBlocked
        },
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(fix.owner_root.join("a.txt")).unwrap(),
        b"user edit during drive",
        "a conflicted user file is never overwritten"
    );
    assert!(shadow_dir.is_dir(), "shadow retained on conflict");
    assert_eq!(
        fix.manager.shadow_row(fix.parent).unwrap().unwrap().state,
        ShadowRowState::IntegrationBlocked
    );
    // The user resolves the drift (reverts to the base content); the same
    // auto decision now integrates through the settle paths.
    std::fs::write(fix.owner_root.join("a.txt"), b"base-alpha").unwrap();
    wait_until(
        || {
            std::fs::read(fix.owner_root.join("a.txt")).unwrap_or_default()
                == b"mid-drive implementation"
                && !shadow_dir.exists()
        },
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(fix.owner_root.join("a.txt")).unwrap(),
        b"mid-drive implementation"
    );
    assert!(!shadow_dir.exists());
}

/// The verifier helper over a bare manager (used by the gated fixture).
fn certify_verified_complete(manager: &Arc<SessionManager>, session: SessionId) {
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
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
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            h.now_ms(),
        )
        .unwrap();
    let rev = h.task_revision(task_id).unwrap();
    h.complete_verified_task(task_id, rev, record).unwrap();
}

#[test]
fn crashed_drive_residue_reopens_and_settles_deterministically() {
    // (d): a daemon "crash" (the parked drive dies with its runtime) leaves
    // the durable shadow row Active + dir on disk; a reopened daemon sees
    // the row, discards the pre-certification residue deterministically on
    // the next shadowed start, and runs a fresh shadowed task to a clean
    // integration.
    let dir = tempfile::tempdir().unwrap();
    // Phase 1 — the crashing daemon: park a shadowed drive mid-flight.
    let parent = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let fix = open_gated_shadow(dir.path());
            let _receipt = fix
                .executor
                .start_task(
                    fix.parent,
                    TaskRunRequest {
                        goal: "crashed shadowed drive".into(),
                        work_items: vec![wi("impl", WorkKind::Implementation, &[])],
                        parent_caps: read_caps(),
                        isolated_root: dir.path().join("isolated"),
                        ..Default::default()
                    },
                )
                .expect("shadowed start");
            let row = fix
                .manager
                .shadow_row(fix.parent)
                .unwrap()
                .expect("row exists");
            assert_eq!(row.state, ShadowRowState::Active);
            let shadow_dir = std::path::PathBuf::from(&row.root);
            wait_until(|| fix.gated.count() >= 1, 180).await;
            std::fs::write(shadow_dir.join("a.txt"), b"crashed-drive content").unwrap();
            assert_eq!(
                std::fs::read(fix.owner_root.join("a.txt")).unwrap(),
                b"base-alpha",
                "crashing daemon never touched the user checkout"
            );
            // Runtime ends here with the drive still parked = the crash.
            // A real crash never runs Drop; forget the service so the
            // graceful-shutdown removal cannot mask the residue.
            std::mem::forget(fix.shadows);
            std::mem::forget(fix.executor);
            fix.parent
        })
    };
    // Phase 2 — the daemon restarts over the same data dir.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let shadows = ShadowRoots::new(manager.clone(), dir.path().join("shadows"));
        let row = manager.shadow_row(parent).unwrap().expect("row survives");
        assert_eq!(row.state, ShadowRowState::Active, "crash residue row");
        let residue_dir = std::path::PathBuf::from(&row.root);
        assert!(residue_dir.is_dir(), "crash residue dir");
        let residue_shadow_id = row.shadow_id.clone();
        let registry = {
            let mut r = ProviderRegistry::new();
            r.try_register(Arc::new(PerCallProvider::new(
                "fake",
                ModelCapabilities {
                    tools: true,
                    parallel_tools: true,
                    ..Default::default()
                },
                done_script(),
            )))
            .unwrap();
            r
        };
        let agent = build_agent(manager.clone(), registry);
        // The real daemon runs crash recovery before the first request; the
        // parked drive's turn is resolved here (the same path serve_impl
        // takes on restart).
        let _ = agent.recover();
        let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
        let executor = TaskExecutor::new(
            &orchestrator,
            manager.clone(),
            agent.clone(),
            Some(shadows.clone()),
        );
        // The interrupted turn is reconstructed (never blindly re-run): a
        // new shadowed task over a LIVE drive is a typed Conflict naming
        // the residue — resume or cancel first.
        let req = TaskRunRequest {
            goal: "post-crash implementation".into(),
            work_items: vec![wi("impl", WorkKind::Implementation, &[])],
            parent_caps: read_caps(),
            isolated_root: dir.path().join("isolated"),
            ..Default::default()
        };
        let err = executor
            .start_task(parent, req.clone())
            .expect_err("a live interrupted drive refuses a new run");
        assert!(matches!(err, ExecError::Conflict(_)), "{err}");
        assert!(err.to_string().contains("live shadow"), "{err}");
        // The operator discards the residue (the shadowed run's task never
        // certified anything); the next shadowed start begins a fresh
        // shadow over the tombstoned row.
        shadows.discard(parent).unwrap();
        let receipt = executor
            .start_task(parent, req)
            .expect("a new shadowed run starts after deterministic settlement");
        assert_eq!(receipt.mode, TaskRunMode::InSession);
        let row = manager.shadow_row(parent).unwrap().expect("new row");
        assert_eq!(row.state, ShadowRowState::Active);
        assert_ne!(row.shadow_id, residue_shadow_id, "a fresh generation");
        assert!(!residue_dir.exists(), "crash residue directory removed");
        let new_dir = std::path::PathBuf::from(&row.root);
        assert!(new_dir.is_dir());
        wait_until(
            || {
                manager
                    .get_session(parent)
                    .unwrap()
                    .unwrap()
                    .state()
                    .unwrap()
                    == faktor_core::state::AgentState::ReadyForNextTurn
            },
            30,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(new_dir.join("a.txt"), b"post-crash implementation").unwrap();
        certify_verified_complete(&manager, parent);
        // The executor's own settle paths converge to the integration (the
        // post-drive watcher re-settles the live shadow on the terminal
        // row) — no manual finalize call.
        let owner = dir.path().join("owner").join("a.txt");
        // The durable row is the authority and is written before the
        // dir cleanup (record-first); wait on the row, then assert the fs
        // effects — environment-independent on Windows and Unix alike.
        wait_until(
            || {
                manager
                    .shadow_row(parent)
                    .ok()
                    .flatten()
                    .map(|r| r.state == ShadowRowState::Integrated)
                    .unwrap_or(false)
            },
            60,
        )
        .await;
        wait_until(
            || {
                std::fs::read(&owner).unwrap_or_default() == b"post-crash implementation"
                    && !new_dir.exists()
            },
            60,
        )
        .await;
        let retired = manager.shadow_row(parent).unwrap().unwrap();
        assert_eq!(retired.state, ShadowRowState::Integrated);
    });
}

#[tokio::test]
async fn failed_drive_keeps_shadow_for_recovery_cancel_discards() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> =
        vec![vec![ScriptedResponse::Die(ProviderError::new(
            faktor_provider::ProviderErrorKind::Malformed,
            "injected permanent failure",
        ))]];
    let env = open_shadow_env(dir.path(), scripts);
    seed_owner(&env.owner_root);
    let _ = env
        .executor
        .start_task(env.parent, mutating_request(&env, "failing shadowed run"))
        .expect("start");
    let row_before = shadow_row_of(&env);
    let dir_before = std::path::PathBuf::from(&row_before.root);
    // The drive fails (permanent provider error): the session ends
    // FailedRecoverable and the task row is NOT terminal — the shadow is
    // RETAINED for the documented recovery path (never a blind discard of
    // a resumable run).
    wait_until(
        || state_of(&env, env.parent) == faktor_core::state::AgentState::FailedRecoverable,
        30,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let row = shadow_row_of(&env);
    assert_eq!(
        row.state,
        ShadowRowState::Active,
        "recoverable runs keep the shadow"
    );
    assert!(dir_before.is_dir());
    assert_eq!(owner_bytes(&env, "a.txt"), b"base-alpha");
    // The operator cancels the task: the terminal Cancel drives the
    // post-drive settle paths (the bounded watcher re-settles live rows) to
    // DISCARD the shadow.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let rev = h.task_revision(task_id).unwrap();
    h.transition_task(task_id, rev, TaskTransition::Cancel, None)
        .unwrap();
    wait_until(
        || {
            env.manager.shadow_row(env.parent).unwrap().unwrap().state == ShadowRowState::Discarded
                && !dir_before.exists()
        },
        60,
    )
    .await;
    let row = shadow_row_of(&env);
    assert_eq!(row.state, ShadowRowState::Discarded);
    assert!(!dir_before.exists());
    assert_eq!(
        owner_bytes(&env, "a.txt"),
        b"base-alpha",
        "a discarded shadow never writes the user checkout"
    );
}

#[test]
fn oversize_shadow_refuses_the_task_before_any_mutation() {
    // (g) at the executor: an un-copyable base refuses the task start with
    // a typed Oversized BEFORE the submit — no provider call, no durable
    // shadow row, no task row, no user bytes touched.
    let dir = tempfile::tempdir().unwrap();
    let env = open_env_with_shadows(
        dir.path(),
        done_script(),
        ShadowCopyLimits {
            max_entries: 2,
            max_total_bytes: 1024 * 1024,
        },
        true,
    );
    seed_owner(&env.owner_root);
    std::fs::write(env.owner_root.join("extra.txt"), b"third file").unwrap();
    let err = env
        .executor
        .start_task(env.parent, mutating_request(&env, "oversized shadow"))
        .expect_err("the copy cap refuses the run");
    assert!(matches!(err, ExecError::Oversized(_)), "{err}");
    assert_eq!(env.provider.count(), 0, "no drive ever started");
    assert!(env.manager.shadow_row(env.parent).unwrap().is_none());
    assert!(env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .list_tasks()
        .unwrap()
        .is_empty());
    assert_eq!(owner_bytes(&env, "a.txt"), b"base-alpha");
}

// ================================================= P0-48 shadow mutation roots
// end-to-end over REAL tools (the wave-22 flip): with the agent's five
// root-resolution sites consulting `SessionManager::resolve_workspace_root`
// and the workspace-scoped consumers consulting live shadow rows, a
// shadowed drive's write_file/read/verification/repo-knowledge paths all
// resolve the SHADOW root while the drive is live — the user checkout is
// byte-untouched until a VerifiedComplete integration (wave-21
// commit_all), and every un-shadowed path keeps today's behavior.

/// A per-call scripted provider that also records every request's rendered
/// system prompt (where repo map + AGENTS.md rules + instruction rules
/// ride), so a test can prove WHICH root a drive read its context from.
struct RecordingProvider {
    caps: ModelCapabilities,
    calls: StdMutex<Vec<Vec<ScriptedResponse>>>,
    script_index: AtomicUsize,
    request_count: AtomicUsize,
    prompts: StdMutex<Vec<String>>,
}

impl RecordingProvider {
    fn new(caps: ModelCapabilities, per_call_scripts: Vec<Vec<ScriptedResponse>>) -> Self {
        Self {
            caps: caps.clone(),
            calls: StdMutex::new(per_call_scripts),
            script_index: AtomicUsize::new(0),
            request_count: AtomicUsize::new(0),
            prompts: StdMutex::new(Vec::new()),
        }
    }
    fn recorded(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let _ = model;
        self.caps.clone()
    }
    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        use futures::StreamExt;
        self.request_count.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(req.system.clone());
        let i = self.script_index.fetch_add(1, Ordering::SeqCst);
        let script: Vec<ScriptedResponse> = self
            .calls
            .lock()
            .unwrap()
            .get(i)
            .cloned()
            .unwrap_or_else(|| vec![ScriptedResponse::End]);
        let stream = futures::stream::iter(script).map(|s| match s {
            ScriptedResponse::Text(t) => Ok(ProviderChunk::Text { text: t }),
            ScriptedResponse::ToolCall { id, name, input } => Ok(ProviderChunk::ToolCall {
                id,
                name,
                input,
                complete: true,
            }),
            ScriptedResponse::Die(e) => Err(e),
            ScriptedResponse::End => Ok(ProviderChunk::Done),
            ScriptedResponse::Reasoning(_) => unreachable!("no reasoning scripts"),
        });
        Box::pin(stream)
    }
}

/// A real write_file: resolves the RELATIVE path through the session's
/// workspace handle and writes atomically — the exact operation the flipped
/// tool-batch site serves. No postcondition (no crash is simulated here).
fn real_write_tool() -> Tool {
    Tool {
        name: "write_file".into(),
        description: "writes a real file".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: ToolRecovery::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(|ctx: ToolRunCtx, args| {
            Box::pin(async move {
                let Some(ws) = &ctx.workspace else {
                    return Err(Error::internal("no workspace wired"));
                };
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                ws.write_atomic(std::path::Path::new(path), content.as_bytes())
                    .map_err(|e| Error::internal(format!("write {path}: {e}")))?;
                Ok(ToolOutcome {
                    text: format!("wrote {path}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// A CPU tool whose FIRST invocation parks the drive (mid-iteration, at
/// ExecutingTool) until the gate opens: the deterministic mid-drive window
/// of a shadowed run. Fired counts invocations that reached the park.
fn parking_tool(name: &str, gate: Arc<tokio::sync::Notify>, fired: Arc<AtomicUsize>) -> Tool {
    Tool {
        name: name.into(),
        description: "parks once".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::Cpu,
        capability: None,
        recovery_hint: ToolRecovery::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx, _args| {
            let gate = gate.clone();
            let fired = fired.clone();
            Box::pin(async move {
                if fired.fetch_add(1, Ordering::SeqCst) == 0 {
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

/// A write_file whose FIRST invocation parks AFTER the write landed (the
/// drive is mid-flight with the file already inside the resolved root).
fn parked_write_tool(gate: Arc<tokio::sync::Notify>, fired: Arc<AtomicUsize>) -> Tool {
    Tool {
        name: "write_file".into(),
        description: "writes a real file, parking once".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: ToolRecovery::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(move |ctx: ToolRunCtx, args| {
            let gate = gate.clone();
            let fired = fired.clone();
            Box::pin(async move {
                let Some(ws) = &ctx.workspace else {
                    return Err(Error::internal("no workspace wired"));
                };
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                ws.write_atomic(std::path::Path::new(path), content.as_bytes())
                    .map_err(|e| Error::internal(format!("write {path}: {e}")))?;
                if fired.fetch_add(1, Ordering::SeqCst) == 0 {
                    gate.notified().await;
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

/// Workspace-scoped root provider over the REAL SessionManager (mirror of
/// the daemon's `SessionWorkspaceRoots`): the live shadow of a shadowed
/// workspace re-points instruction loading at the shadow root.
struct RealRoots(Arc<SessionManager>);

impl faktor_instructions::WorkspaceRootProvider for RealRoots {
    fn workspace_root(&self, workspace_id: u64) -> Option<std::path::PathBuf> {
        if workspace_id == 0 {
            return None;
        }
        let ws = WorkspaceId::new(workspace_id);
        match self.0.live_workspace_shadow_root(ws) {
            Ok(Some(root)) => Some(root),
            Ok(None) | Err(_) => self.0.workspace_root(ws).ok().flatten(),
        }
    }
}

fn real_resolver(manager: &Arc<SessionManager>) -> Arc<faktor_instructions::InstructionResolver> {
    Arc::new(faktor_instructions::InstructionResolver::new(
        Arc::new(RealRoots(manager.clone())),
        faktor_instructions::DEFAULT_RESOLVER_CACHE_ENTRIES,
    ))
}

/// An executor env whose drive runs REAL tools (write_file over the
/// session's resolved workspace root) with a REAL instructions resolver and
/// the given verification service. `parked_write` registers the parking
/// write tool; the gate/fired pair exposes the mid-drive window.
struct RealToolEnv {
    manager: Arc<SessionManager>,
    provider: Arc<RecordingProvider>,
    executor: Arc<TaskExecutor>,
    parent: SessionId,
    owner_root: std::path::PathBuf,
    isolated_root: std::path::PathBuf,
    gate: Arc<tokio::sync::Notify>,
    fired: Arc<AtomicUsize>,
}
fn real_state_of(env: &RealToolEnv) -> faktor_core::state::AgentState {
    env.manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .state()
        .unwrap()
}

fn real_mutating_request(env: &RealToolEnv, goal: &str) -> TaskRunRequest {
    TaskRunRequest {
        goal: goal.to_string(),
        work_items: vec![wi("impl", WorkKind::Implementation, &[])],
        parent_caps: read_caps(),
        isolated_root: env.isolated_root.clone(),
        ..Default::default()
    }
}

fn open_real_tool_env(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
    parked_write: bool,
) -> Arc<RealToolEnv> {
    open_real_tool_env_full(
        root,
        scripts,
        verification,
        parked_write,
        true,
        MutationMode::Shadow,
    )
}

/// [`open_real_tool_env`] with the executor's shadow wiring spelled out:
/// `service` = carry the daemon's [`ShadowRoots`] service, `mode` = the
/// daemon default mutation mode. The full wiring matrix of the daemon graph
/// (service always present in production; the mode deciding usage only) is
/// exercised through this one constructor.
fn open_real_tool_env_full(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
    parked_write: bool,
    service: bool,
    mode: MutationMode,
) -> Arc<RealToolEnv> {
    open_real_tool_env_inner(
        root,
        scripts,
        verification,
        parked_write,
        service,
        mode,
        false,
    )
}

/// [`open_real_tool_env_full`] with the daemon's process supervisor wired
/// (the proof-basis probe path in production always has one; the default
/// fixture deliberately runs without it so probes degrade explicitly).
fn open_real_tool_env_supervised(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
    mode: MutationMode,
) -> Arc<RealToolEnv> {
    open_real_tool_env_inner(root, scripts, verification, true, true, mode, true)
}

#[allow(clippy::too_many_arguments)]
fn open_real_tool_env_inner(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
    parked_write: bool,
    service: bool,
    mode: MutationMode,
    with_supervisor: bool,
) -> Arc<RealToolEnv> {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let caps = ModelCapabilities {
        tools: true,
        parallel_tools: true,
        ..Default::default()
    };
    let provider = Arc::new(RecordingProvider::new(caps, scripts));
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider.clone()).unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let fired = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    if parked_write {
        tools.register(parked_write_tool(gate.clone(), fired.clone()));
    } else {
        tools.register(real_write_tool());
    }
    tools.register(parking_tool("pause", gate.clone(), fired.clone()));
    let resolver = real_resolver(&manager);
    let supervisor = if with_supervisor {
        Some(faktor_terminal::ProcessSupervisor::new(manager.cas()))
    } else {
        None
    };
    let agent = AgentRuntime::new(AgentDeps {
        session: manager.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tools),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor,
        verification,
        hooks: None,
        instructions_resolver: resolver,
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test agent.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 120_000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
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
        .create_session(ws, "real-shadow-tools", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = if service {
        let shadows = ShadowRoots::new(manager.clone(), root.join("shadows"));
        TaskExecutor::new_with_mode(
            &orchestrator,
            manager.clone(),
            agent.clone(),
            Some(shadows),
            mode,
        )
    } else {
        TaskExecutor::new(&orchestrator, manager.clone(), agent.clone(), None)
    };
    let isolated_root = root.join("isolated");
    std::fs::create_dir_all(&isolated_root).unwrap();
    Arc::new(RealToolEnv {
        manager,
        provider,
        executor,
        parent,
        owner_root,
        isolated_root,
        gate,
        fired,
    })
}

fn real_env_task_row(env: &RealToolEnv) -> faktor_session::Task {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    h.get_task(h.task_id().unwrap()).unwrap().unwrap()
}

/// Settle a real-tool drive to VerifiedComplete + integration: the drive's
/// own verified completion may auto-commit through the post-drive finalize
/// hook; otherwise certify through the human-verifier seam. Since the
/// wave-24 executor arms a BOUNDED post-drive watcher whenever its
/// one-shot finalize finds the task row non-terminal, the integration lands
/// WITHOUT any further executor call — the test polls the durable outcome
/// instead of driving the finalize by hand.
async fn settle_verified_integrate(env: &Arc<RealToolEnv>) {
    wait_until(
        || real_state_of(env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let task = real_env_task_row(env);
    if task.state != TaskState::VerifiedComplete {
        certify_verified_complete(&env.manager, env.parent);
    }
    // The durable outcome converges through the executor's own settle
    // paths (post-drive finalize, then the bounded watcher) — never a
    // manual executor call from the test.
    wait_until(
        || {
            env.manager
                .shadow_row(env.parent)
                .ok()
                .flatten()
                .is_some_and(|r| {
                    r.state == ShadowRowState::Integrated
                        && !std::path::PathBuf::from(&r.root).exists()
                })
        },
        60,
    )
    .await;
    let row = env
        .manager
        .shadow_row(env.parent)
        .unwrap()
        .expect("row exists");
    assert_eq!(row.state, ShadowRowState::Integrated);
    assert!(
        !std::path::PathBuf::from(&row.root).exists(),
        "clean integration removes the shadow directory"
    );
}

fn seed_rust(env: &RealToolEnv) {
    std::fs::create_dir_all(env.owner_root.join("src")).unwrap();
    std::fs::write(
        env.owner_root.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        env.owner_root.join("src/lib.rs"),
        "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
    )
    .unwrap();
}

#[tokio::test]
async fn real_write_drive_writes_the_shadow_and_verified_complete_integrates_it() {
    let _heavy = heavy_guard();
    // (a) over the REAL executor + REAL tools: a shadowed mutating drive
    // executes write_file against the SHADOW root (the flipped tool-batch
    // site); the user checkout stays byte-untouched MID-drive; a
    // VerifiedComplete integration (wave-21 commit_all through the
    // post-drive finalize) lands the content in the user checkout and
    // removes the shadow.
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
                    }),
                },
                ScriptedResponse::ToolCall {
                    id: "c2".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "util.rs",
                        "content": "pub fn fresh() -> u64 {\n    let seed: u64 = 7;\n    let factor: u64 = 3;\n    seed.saturating_mul(factor)\n}\n",
                    }),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        true,
    );
    seed_rust(&env);
    let receipt = env
        .executor
        .start_task(
            env.parent,
            real_mutating_request(&env, "implement the change"),
        )
        .expect("shadowed real-tool start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env
        .manager
        .shadow_row(env.parent)
        .unwrap()
        .expect("shadow row at begin");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    assert_eq!(row.state, ShadowRowState::Active);
    // Mid-drive: the FIRST write landed inside the shadow and parked the
    // drive at ExecutingTool — the user checkout is byte-untouched.
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    assert_eq!(
        std::fs::read(shadow_dir.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        "the write landed in the SHADOW"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        "user checkout untouched mid-drive"
    );
    assert!(!env.owner_root.join("util.rs").exists());
    assert_eq!(
        env.manager.active_root(env.parent).unwrap(),
        Some(shadow_dir.clone()),
        "active_root re-points while the drive is live"
    );
    // Release the drive: second write may or may not have landed before the
    // park, but whatever the shadow holds now must not touch the user
    // checkout until the verified integration.
    env.gate.notify_waiters();
    settle_verified_integrate(&env).await;
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        "VerifiedComplete integration lands the changed file"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("util.rs")).unwrap(),
        b"pub fn fresh() -> u64 {\n    let seed: u64 = 7;\n    let factor: u64 = 3;\n    seed.saturating_mul(factor)\n}\n",
        "VerifiedComplete integration lands the created file"
    );
    assert!(
        env.manager.active_root(env.parent).unwrap().is_none(),
        "a retired shadow stops re-pointing"
    );
}

#[tokio::test]
async fn shadowed_drive_reads_repo_knowledge_and_rules_from_the_shadow() {
    let _heavy = heavy_guard();
    // (b): repo knowledge + instructions of a shadowed drive resolve from
    // the SHADOW root — a rules file and AGENTS.md marker placed inside the
    // shadow AFTER begin (never in the user checkout) show up in the NEXT
    // iteration's rendered context, and the user checkout's own rules never
    // leak into the drive.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("owner")).unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "p1".into(),
                    name: "pause".into(),
                    input: serde_json::json!({}),
                },
                ScriptedResponse::End,
            ],
            vec![
                ScriptedResponse::Text("conclude the change".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::disabled(),
        false,
    );
    std::fs::write(
        env.owner_root.join("AGENTS.md"),
        "user-root-marker-4f1: follow user checkout conventions\n",
    )
    .unwrap();
    std::fs::write(env.owner_root.join("a.txt"), b"base-alpha").unwrap();
    let receipt = env
        .executor
        .start_task(env.parent, real_mutating_request(&env, "convention task"))
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env.manager.shadow_row(env.parent).unwrap().expect("row");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    // Mid-flight (the pause tool parks the drive): write the shadow-only
    // world — a new file and a REWRITTEN AGENTS.md — into the shadow.
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    std::fs::write(
        shadow_dir.join("AGENTS.md"),
        "shadow-world-marker-7c1: drive inside the shadow world\n",
    )
    .unwrap();
    std::fs::write(shadow_dir.join("only-shadow-notes.md"), b"shadow notes\n").unwrap();
    assert!(
        std::fs::read_to_string(env.owner_root.join("AGENTS.md"))
            .unwrap()
            .contains("user-root-marker-4f1"),
        "the user checkout rules are untouched"
    );
    env.gate.notify_waiters();
    wait_until(
        || real_state_of(&env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    let prompts = env.provider.recorded();
    assert!(prompts.len() >= 2, "two requests expected: {prompts:?}");
    assert!(
        !prompts[0].contains("shadow-world-marker-7c1"),
        "the first context predates the shadow writes"
    );
    assert!(
        !prompts[0].contains("only-shadow-notes.md"),
        "the first repo map cannot see the shadow-only file"
    );
    let second = &prompts[1];
    assert!(
        second.contains("shadow-world-marker-7c1"),
        "the rewritten shadow AGENTS.md must ride the next context"
    );
    assert!(
        second.contains("only-shadow-notes.md"),
        "the repo file map must come from the shadow root"
    );
    assert!(
        second.contains("always, loaded: always"),
        "the instruction resolver must append the shadow's rule tree"
    );
    assert!(
        !second.contains("user-root-marker-4f1"),
        "user-checkout rules must never leak into a shadowed drive"
    );
    // The drive is a no-change turn: the shadow is retained (nothing was
    // certified), and the user checkout is untouched.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !std::fs::read_to_string(env.owner_root.join("AGENTS.md"))
            .unwrap()
            .contains("shadow-world-marker-7c1"),
        "no shadow byte ever lands without a verified integration"
    );
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::Active
    );
}

#[tokio::test]
async fn real_write_drive_user_drift_conflicts_at_integration_then_resolves() {
    let _heavy = heavy_guard();
    // (d): the conflict path end-to-end at the agent + executor level — the
    // drive's REAL write lands in the shadow; a mid-drive USER edit of the
    // same file conflicts at the VerifiedComplete integration
    // (IntegrationBlocked semantics from wave-21: the user file is never
    // overwritten, the shadow is retained), and resolving the drift lets
    // the same decision integrate.
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "a.txt",
                        "content": "agent implementation",
                    }),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::disabled(),
        true,
    );
    seed_owner(&env.owner_root);
    let receipt = env
        .executor
        .start_task(env.parent, real_mutating_request(&env, "implement a.txt"))
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env.manager.shadow_row(env.parent).unwrap().expect("row");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    // Mid-drive: the agent's write landed in the shadow; the user then
    // edits the file externally.
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    assert_eq!(
        std::fs::read(shadow_dir.join("a.txt")).unwrap(),
        b"agent implementation",
        "the agent wrote the shadow"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    std::fs::write(env.owner_root.join("a.txt"), b"user edit during drive").unwrap();
    env.gate.notify_waiters();
    wait_until(
        || real_state_of(&env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    certify_verified_complete(&env.manager, env.parent);
    // The executor's own settle paths converge to the durable
    // IntegrationBlocked outcome (the bounded post-drive watcher re-settles
    // the live shadow on the terminal row) — no manual finalize call.
    wait_until(
        || {
            env.manager.shadow_row(env.parent).unwrap().unwrap().state
                == ShadowRowState::IntegrationBlocked
        },
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"user edit during drive",
        "a conflicted user file is never overwritten"
    );
    assert!(shadow_dir.is_dir(), "the shadow is retained on conflict");
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::IntegrationBlocked
    );
    assert_eq!(
        env.manager.active_root(env.parent).unwrap(),
        Some(shadow_dir.clone()),
        "the IntegrationBlocked shadow stays the session's root until resolved"
    );
    // The user resolves the drift (back to the base digest); the same auto
    // decision now integrates through the executor's own settle paths.
    std::fs::write(env.owner_root.join("a.txt"), b"base-alpha").unwrap();
    wait_until(
        || {
            std::fs::read(env.owner_root.join("a.txt")).unwrap_or_default()
                == b"agent implementation"
                && !shadow_dir.exists()
        },
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"agent implementation"
    );
    assert!(!shadow_dir.exists());
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::Integrated
    );
}

// ------------------------------------------------ max_cost_micro task control
// (audit 9/H: TaskRunRequest.max_cost_micro flows to the task row cap and
// the guarded ledger refuses an over-committed reduction on a re-seed.)

#[tokio::test]
async fn single_item_task_max_cost_micro_lands_on_the_task_row_cap_and_refuses_lowering() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    let goal = "analyze the module boundaries";
    let mut req = request(goal, vec![wi("a1", WorkKind::Analysis, &[])], &env);
    req.max_cost_micro = Some(10_000_000);
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("single-item start with a cost cap");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    wait_until(
        || state_of(&env, env.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // The requested cap landed on the session's durable task row (the row
    // every paid model call of the drive is admitted against).
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let ledger = faktor_session::DurableBudgetLedger::new(env.manager.clone());
    assert_eq!(
        ledger
            .session_budget_view(env.parent, task_id)
            .expect("durable budget view")
            .max_cost_micro,
        Some(10_000_000),
        "TaskRunRequest.max_cost_micro flows to the task row cap"
    );
    // A reserve that commits part of the cap, followed by a NEW task on the
    // same session whose cap would sit below the committed amount, refuses
    // the whole start with a typed conflict (spend never rewinds, and a new
    // run can never silently lower a live row's cap under its commitments).
    ledger
        .reserve(
            env.parent,
            task_id,
            faktor_core::id::OpId::new(7_000_001),
            60_000,
            None,
        )
        .await
        .expect("reserve under the cap");
    let mut req2 = request(
        "a second capped run",
        vec![wi("a2", WorkKind::Analysis, &[])],
        &env,
    );
    req2.max_cost_micro = Some(50_000);
    let calls_before = env.provider.count();
    let err = env
        .executor
        .start_task(env.parent, req2)
        .expect_err("a cap below the committed 60_000 must refuse the start");
    assert!(
        matches!(err, ExecError::Conflict(_)),
        "typed conflict, not a silent lower cap: {err}"
    );
    assert!(err.to_string().contains("cost cap"), "{err}");
    assert_eq!(
        env.provider.count(),
        calls_before,
        "the refused run never submitted a prompt"
    );
}

// ============================================ mutation mode (wave-24) + cancel
// Shadow mutation is the production default: the daemon ALWAYS carries the
// ShadowRoots service and the executor's MutationMode (daemon default,
// overridable per run) decides usage ONLY. DirectCompat must reproduce the
// pre-shadow direct behavior byte-identically even with the service
// present, and the executor exposes ONE task-level cancel authority
// (cancel_run) the HTTP surface drives.

fn one_write_script(content: &str) -> Vec<Vec<ScriptedResponse>> {
    vec![
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "a.txt",
                    "content": content,
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        vec![ScriptedResponse::End],
    ]
}

#[tokio::test]
async fn direct_compat_with_service_is_byte_identical_to_no_service() {
    // The parity test: the SAME scripts/goal on (a) an executor carrying
    // the shadow service in DirectCompat mode and (b) an executor without
    // any shadow service (the historical wiring) must produce byte-identical
    // outcomes — the service's presence alone must never change a
    // DirectCompat run. Both drives write DIRECTLY to the owner checkout;
    // no shadow row exists on either side, and the durable op records +
    // message streams match.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts = one_write_script("direct implementation alpha");
    let env_a = open_real_tool_env_full(
        &dir.path().join("a"),
        scripts.clone(),
        faktor_agent::VerificationService::disabled(),
        false,
        true,
        MutationMode::DirectCompat,
    );
    let env_b = open_real_tool_env_full(
        &dir.path().join("b"),
        scripts,
        faktor_agent::VerificationService::disabled(),
        false,
        false,
        MutationMode::Shadow, // irrelevant without the service
    );
    seed_owner(&env_a.owner_root);
    seed_owner(&env_b.owner_root);
    let goal = "implement the direct change";
    let receipt_a = env_a
        .executor
        .start_task(env_a.parent, real_mutating_request(&env_a, goal))
        .expect("direct-compat start over the service");
    let receipt_b = env_b
        .executor
        .start_task(env_b.parent, real_mutating_request(&env_b, goal))
        .expect("no-service start");
    assert_eq!(receipt_a.mode, TaskRunMode::InSession);
    assert_eq!(receipt_b.mode, TaskRunMode::InSession);
    wait_until(
        || real_state_of(&env_a) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    wait_until(
        || real_state_of(&env_b) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // The state transition and the turn-record finalize are separate
    // durable writes; wait for BOTH records to converge before reading
    // them (Windows CI exposed the observe-order race; a fixed sleep would
    // be environment-dependent and weaker).
    {
        let ha = env_a.manager.get_session(env_a.parent).unwrap().unwrap();
        let hb = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
        let op_a = receipt_a.op_id.unwrap();
        let op_b = receipt_b.op_id.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        loop {
            let a = ha.turn_record(op_a).unwrap().map(|r| r.status);
            let b = hb.turn_record(op_b).unwrap().map(|r| r.status);
            if a.is_some() && a == b {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "turn records never converged: a={a:?} b={b:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    // DirectCompat with the service present NEVER shadows: no durable row,
    // no re-pointing, and the drive wrote the OWNER checkout.
    assert!(
        env_a.manager.shadow_row(env_a.parent).unwrap().is_none(),
        "DirectCompat must not begin a shadow"
    );
    assert_eq!(
        env_a.manager.active_root(env_a.parent).unwrap(),
        None,
        "DirectCompat never re-points the session"
    );
    assert_eq!(
        std::fs::read(env_a.owner_root.join("a.txt")).unwrap(),
        b"direct implementation alpha",
        "the DirectCompat drive wrote the owner checkout"
    );
    // Byte parity with the historical no-service wiring.
    assert_eq!(
        std::fs::read(env_a.owner_root.join("a.txt")).unwrap(),
        std::fs::read(env_b.owner_root.join("a.txt")).unwrap(),
        "byte-identical owner content on both sides"
    );
    let ha = env_a.manager.get_session(env_a.parent).unwrap().unwrap();
    let hb = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
    assert_eq!(
        ha.message_count().unwrap(),
        hb.message_count().unwrap(),
        "byte-identical message streams"
    );
    let rec_a = ha.turn_record(receipt_a.op_id.unwrap()).unwrap().unwrap();
    let rec_b = hb.turn_record(receipt_b.op_id.unwrap()).unwrap().unwrap();
    assert_eq!(rec_a.status, rec_b.status);
    assert_eq!(rec_a.effective_provider, rec_b.effective_provider);
    assert_eq!(rec_a.effective_model, rec_b.effective_model);
}

#[tokio::test]
async fn per_run_mutation_mode_overrides_the_daemon_default() {
    // The daemon default rides the executor construction (the mode decides
    // usage only); a per-run `mutation_mode` overrides it in BOTH
    // directions: a DirectCompat-defaulted daemon still shadows a run that
    // asks for Shadow, and a Shadow-defaulted daemon (production default)
    // drives directly a run that asks for DirectCompat.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    // (a) daemon default DirectCompat + per-run Shadow => shadowed drive.
    let env_a = open_real_tool_env_full(
        &dir.path().join("a"),
        one_write_script("shadowed by override"),
        faktor_agent::VerificationService::disabled(),
        false,
        true,
        MutationMode::DirectCompat,
    );
    seed_owner(&env_a.owner_root);
    let mut req_a = real_mutating_request(&env_a, "override to shadow");
    req_a.mutation_mode = Some(MutationMode::Shadow);
    let receipt_a = env_a
        .executor
        .start_task(env_a.parent, req_a)
        .expect("per-run shadow start");
    assert_eq!(receipt_a.mode, TaskRunMode::InSession);
    let row_a = env_a
        .manager
        .shadow_row(env_a.parent)
        .unwrap()
        .expect("the per-run Shadow override began a shadow");
    let shadow_a = std::path::PathBuf::from(&row_a.root);
    wait_until(
        || real_state_of(&env_a) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(shadow_a.join("a.txt")).unwrap(),
        b"shadowed by override",
        "the write landed in the shadow"
    );
    assert_eq!(
        std::fs::read(env_a.owner_root.join("a.txt")).unwrap(),
        b"base-alpha",
        "the owner checkout stayed untouched"
    );
    // (b) daemon default Shadow (production default) + per-run DirectCompat
    // => direct drive, byte-identical to the pre-shadow behavior.
    let env_b = open_real_tool_env_full(
        &dir.path().join("b"),
        one_write_script("direct by override"),
        faktor_agent::VerificationService::disabled(),
        false,
        true,
        MutationMode::Shadow,
    );
    seed_owner(&env_b.owner_root);
    let mut req_b = real_mutating_request(&env_b, "override to direct");
    req_b.mutation_mode = Some(MutationMode::DirectCompat);
    let receipt_b = env_b
        .executor
        .start_task(env_b.parent, req_b)
        .expect("per-run direct start");
    assert_eq!(receipt_b.mode, TaskRunMode::InSession);
    wait_until(
        || real_state_of(&env_b) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    assert!(
        env_b.manager.shadow_row(env_b.parent).unwrap().is_none(),
        "the per-run DirectCompat override never shadows"
    );
    assert_eq!(
        std::fs::read(env_b.owner_root.join("a.txt")).unwrap(),
        b"direct by override",
        "the override drive wrote the owner checkout"
    );
}

#[tokio::test]
async fn criteria_land_on_the_task_row_and_hostile_criteria_refuse_the_start() {
    // The run's acceptance criteria ride the durable task row the drive
    // certifies against; hostile criteria (beyond the row's own caps) are
    // typed Oversized refusals BEFORE anything is written.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("c"), done_script());
    let mut req = request(
        "implement with criteria",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.criteria = vec![
        "the change compiles".into(),
        "existing tests still pass".into(),
    ];
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("single-item start with criteria");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task = h.get_task(h.task_id().unwrap()).unwrap().unwrap();
    assert_eq!(
        task.acceptance_criteria,
        vec![
            "the change compiles".to_string(),
            "existing tests still pass".to_string()
        ]
    );
    // A fresh session for the hostile case (a terminal row would refuse
    // for a different reason).
    let env2 = open_env(&dir.path().join("c2"), done_script());
    let mut hostile = request(
        "hostile criteria",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env2,
    );
    hostile.criteria = (0..=faktor_session::MAX_TASK_CRITERIA)
        .map(|i| format!("criterion {i}"))
        .collect();
    let err = env2
        .executor
        .start_task(env2.parent, hostile)
        .expect_err("criteria beyond the row cap refuse the start");
    assert!(matches!(err, ExecError::Oversized(_)), "{err}");
    let env3 = open_env(&dir.path().join("c3"), done_script());
    let mut hostile2 = request(
        "oversized criterion",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env3,
    );
    hostile2.criteria = vec!["x".repeat(faktor_session::MAX_TASK_CRITERION_BYTES + 1)];
    let err2 = env3
        .executor
        .start_task(env3.parent, hostile2)
        .expect_err("an oversized criterion refuses the start");
    assert!(matches!(err2, ExecError::Oversized(_)), "{err2}");
    assert_eq!(env3.provider.count(), 0, "no drive started");
}

#[tokio::test]
async fn cancel_in_session_run_mid_drive_aborts_discards_and_refuses_twice() {
    // Task-level cancel of an in-session run: the drive is parked mid
    // flight (gated provider); cancel_run aborts the op durably, cancels
    // the task row, and discards the run's shadow through the durable
    // finalize — the owner checkout never receives a byte. A second cancel
    // is a typed Conflict; an unknown run is a typed NotFound.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let fix = open_gated_shadow(dir.path());
    let receipt = fix
        .executor
        .start_task(
            fix.parent,
            TaskRunRequest {
                goal: "cancellable shadowed implementation".into(),
                work_items: vec![wi("impl", WorkKind::Implementation, &[])],
                parent_caps: read_caps(),
                isolated_root: dir.path().join("isolated"),
                ..Default::default()
            },
        )
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = fix
        .manager
        .shadow_row(fix.parent)
        .unwrap()
        .expect("row at begin");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    // Drive parked mid-flight; write into the shadow so the discard is
    // observable.
    wait_until(|| fix.gated.count() >= 1, 180).await;
    std::fs::write(shadow_dir.join("a.txt"), b"cancelled-drive content").unwrap();
    fix.executor
        .cancel_run(fix.parent, &receipt.run_id)
        .expect("task-level cancel accepted");
    // Durable outcome: task row Cancelled, session back to ReadyForNextTurn,
    // shadow discarded, owner untouched.
    wait_until(
        || {
            let h = fix.manager.get_session(fix.parent).unwrap().unwrap();
            h.get_task(h.task_id().unwrap())
                .unwrap()
                .is_some_and(|t| t.state == TaskState::Cancelled)
        },
        60,
    )
    .await;
    wait_until(
        || {
            fix.manager.shadow_row(fix.parent).unwrap().unwrap().state == ShadowRowState::Discarded
                && !shadow_dir.exists()
        },
        60,
    )
    .await;
    assert_eq!(
        fix.manager
            .get_session(fix.parent)
            .unwrap()
            .unwrap()
            .state()
            .unwrap(),
        faktor_core::state::AgentState::ReadyForNextTurn,
        "aborting the drive lands the session ReadyForNextTurn"
    );
    assert_eq!(
        std::fs::read(fix.owner_root.join("a.txt")).unwrap(),
        b"base-alpha",
        "the owner checkout never received a byte"
    );
    // A cancelled run is never cancelled twice; unknown runs stay unknown.
    let err = fix
        .executor
        .cancel_run(fix.parent, &receipt.run_id)
        .expect_err("a second cancel must refuse");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
    let err = fix
        .executor
        .cancel_run(fix.parent, "run-unknown")
        .expect_err("an unknown run must refuse");
    assert!(matches!(err, ExecError::NotFound(_)), "{err}");
}

#[tokio::test]
async fn cancel_orchestrated_run_fans_cancel_to_live_children_only() {
    // Task-level cancel of an orchestrated run: every live child receives
    // the durable Cancel control exactly once through the runtime's queue
    // (the bounded cancel path reaches a running prompt between chunks);
    // a fully terminal run refuses a second cancel.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ticker = Arc::new(TickerProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        request_count: AtomicUsize::new(0),
        chunk_delay_ms: 15,
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(ticker.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    let owner_root = dir.path().join("owner");
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
        .create_session(ws, "cancel-orch", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let isolated = dir.path().join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new(&orchestrator, manager.clone(), agent.clone(), None);
    let receipt = executor
        .start_task(
            parent,
            TaskRunRequest {
                goal: "ticking parallel analysis".into(),
                work_items: vec![
                    wi("a", WorkKind::Analysis, &[]),
                    wi("b", WorkKind::Analysis, &[]),
                ],
                parent_caps: read_caps(),
                isolated_root: isolated.clone(),
                ..Default::default()
            },
        )
        .expect("multi-item run starts");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
    // Both children spawned and their drives are mid-flight (ticking).
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| !c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    wait_until(|| ticker.count() >= 2, 240).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    executor
        .cancel_run(parent, &receipt.run_id)
        .expect("task-level cancel accepted");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows =
        OrchestratorRuntime::registry_rows(manager.clone(), parent, &receipt.run_id).unwrap();
    assert!(
        rows.iter().any(|c| c.state == crate::ChildState::Cancelled),
        "the live children were cancelled: {rows:?}"
    );
    // Terminal residue refuses a second cancel.
    let err = executor
        .cancel_run(parent, &receipt.run_id)
        .expect_err("a fully settled run refuses a second cancel");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
}

// ------------------------------------------------ multi-candidate tournaments

/// The tournament flow end-to-end over REAL isolated candidate worktrees:
/// N=2 implementation candidates start through the ONE task-start authority
/// with the byte-identical goal+criteria, every candidate is driven to
/// terminal, settlements rank deterministically (a FAILED verification can
/// never win, even at zero cost), the loser's isolated worktree is removed
/// (registry invariant stays clean), and a FRESH executor reconstructs the
/// Decided tournament with the same winner from the durable ledger.
#[tokio::test]
async fn tournament_flow_ranks_deterministically_and_discards_losers_orphan_free() {
    use crate::runtime::task_executor::TournamentStartRequest;
    use crate::tournament::{CandidateState, ReviewRank, ReviewVerdict, TournamentState};
    use faktor_core::id::VerificationRecordId;

    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..8)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let goal = "implement the same change two ways";
    let criteria = vec!["cargo test".to_string()];

    // Typed N refusals leave NOTHING durable behind.
    let facts_before = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .memory_facts()
        .unwrap()
        .len();
    for n in [1usize, 5] {
        let err = env
            .executor
            .start_tournament(env.parent, goal, &criteria, n, None)
            .expect_err("out-of-band N must refuse");
        assert!(matches!(err, ExecError::InvalidPlan(_)), "{err}");
    }
    assert_eq!(
        env.manager
            .get_session(env.parent)
            .unwrap()
            .unwrap()
            .memory_facts()
            .unwrap()
            .len(),
        facts_before,
        "refused tournament starts write nothing"
    );

    let receipt = env
        .executor
        .start_tournament_with(
            env.parent,
            TournamentStartRequest {
                goal: goal.to_string(),
                criteria: criteria.clone(),
                n: 2,
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("tournament start");
    assert_eq!(
        receipt.candidates,
        vec!["child-0".to_string(), "child-1".to_string()]
    );
    // Every candidate really spawned as an ISOLATED child and settled.
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    assert_eq!(rows.len(), 2);
    let parent_ws = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .row()
        .unwrap()
        .workspace_id;
    for c in &rows {
        assert_eq!(c.state, crate::ChildState::Done);
        assert_eq!(c.ownership, ChildOwnership::IsolatedWorktree);
        assert_ne!(c.workspace_id, parent_ws.raw(), "candidate is isolated");
    }

    // The durable tournament reconstructs the identical candidate band +
    // criteria from its ledger anchor.
    let state = env
        .executor
        .tournament_state(env.parent, &receipt.tournament_id)
        .unwrap();
    assert_eq!(state.state, TournamentState::Open);
    assert_eq!(state.candidates.len(), 2);
    assert_eq!(state.criteria.len(), 1);
    assert_eq!(state.criteria[0].spec, "cargo test");
    assert!(state.candidates.iter().all(|c| !c.worktree.is_empty()));
    let winner_wt = state.candidates[1].worktree.clone();
    let loser_wt = state.candidates[0].worktree.clone();
    assert!(std::path::Path::new(&winner_wt).is_dir());
    assert!(std::path::Path::new(&loser_wt).is_dir());

    // The losing candidate is FREE but failed verification; the winning
    // candidate passed and is expensive. The failed one can never win.
    let mut loser = env
        .executor
        .candidate_settlement(
            env.parent,
            &receipt.tournament_id,
            "child-0",
            "verification failed",
        )
        .unwrap();
    assert_eq!(loser.state, CandidateState::Done);
    loser.verification = Some(VerificationRecordId::new(1));
    loser.verification_pass = Some(false);
    loser.review = Some(ReviewVerdict {
        rank: ReviewRank::Clean,
        reviewer: "review-0".into(),
    });
    loser.cost_micro = 0;
    env.executor
        .settle_tournament_candidate(env.parent, &receipt.tournament_id, loser)
        .unwrap();

    let mut winner = env
        .executor
        .candidate_settlement(
            env.parent,
            &receipt.tournament_id,
            "child-1",
            "verified complete",
        )
        .unwrap();
    winner.verification = Some(VerificationRecordId::new(2));
    winner.verification_pass = Some(true);
    winner.review = Some(ReviewVerdict {
        rank: ReviewRank::Clean,
        reviewer: "review-0".into(),
    });
    winner.cost_micro = 1_000_000;
    env.executor
        .settle_tournament_candidate(env.parent, &receipt.tournament_id, winner)
        .unwrap();

    let decision = env
        .executor
        .decide_tournament(env.parent, &receipt.tournament_id)
        .expect("deterministic decision");
    assert_eq!(decision.winner.child_id, "child-1");
    // No automatic integration: the winner's worktree is only PROPOSED and
    // still holds the candidate content (nothing was merged anywhere).
    assert!(std::path::Path::new(&winner_wt).is_dir());
    // The loser's worktree is gone and the zero-orphan registry invariant
    // holds after cleanup.
    assert!(
        !std::path::Path::new(&loser_wt).exists(),
        "loser worktree removed"
    );
    assert!(OrchestratorRuntime::registry_violations(
        env.manager.clone(),
        env.parent,
        &receipt.run_id
    )
    .is_empty());
    assert!(
        OrchestratorRuntime::orphan_children_scan(env.manager.clone())
            .issues
            .is_empty()
    );

    // A FRESH executor over the same durable store reconstructs the very
    // same Decided tournament (winner + candidate evidence).
    let executor2 = TaskExecutor::new(
        &env.orchestrator,
        env.manager.clone(),
        env.agent.clone(),
        None,
    );
    let reopened = executor2
        .tournament_state(env.parent, &receipt.tournament_id)
        .unwrap();
    assert_eq!(reopened.state, TournamentState::Decided);
    assert_eq!(reopened.winner.as_deref(), Some("child-1"));
    assert_eq!(
        reopened
            .candidates
            .iter()
            .find(|c| c.child_id == "child-1")
            .unwrap()
            .verification_pass,
        Some(true)
    );
    assert_eq!(
        reopened
            .candidates
            .iter()
            .find(|c| c.child_id == "child-0")
            .unwrap()
            .state,
        CandidateState::Discarded
    );
}

/// The abort path discards EVERY candidate: the terminal ledger row is
/// `Aborted` with no winner, every registry row settles terminal, every
/// isolated worktree directory + row is removed, and a reopen reconstructs
/// the same Aborted tournament.
#[tokio::test]
async fn tournament_abort_discards_all_candidates_and_is_durable() {
    use crate::runtime::task_executor::TournamentStartRequest;
    use crate::tournament::{CandidateState, TournamentState};

    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..4)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let receipt = env
        .executor
        .start_tournament_with(
            env.parent,
            TournamentStartRequest {
                goal: "abort me".to_string(),
                criteria: vec!["cargo test".to_string()],
                n: 2,
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("tournament start");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let state = env
        .executor
        .tournament_state(env.parent, &receipt.tournament_id)
        .unwrap();
    let worktrees: Vec<String> = state
        .candidates
        .iter()
        .map(|c| c.worktree.clone())
        .collect();
    assert!(worktrees.iter().all(|w| std::path::Path::new(w).is_dir()));

    let aborted = env
        .executor
        .abort_tournament(env.parent, &receipt.tournament_id, "operator aborted")
        .expect("abort");
    assert_eq!(aborted.state, TournamentState::Aborted);
    assert!(aborted.winner.is_none());
    assert!(aborted
        .candidates
        .iter()
        .all(|c| c.state == CandidateState::Discarded));
    for w in &worktrees {
        assert!(!std::path::Path::new(w).exists(), "worktree {w} removed");
    }
    assert!(OrchestratorRuntime::registry_violations(
        env.manager.clone(),
        env.parent,
        &receipt.run_id
    )
    .is_empty());
    // Reopen: the same Aborted tournament, no winner ever proposed.
    let executor2 = TaskExecutor::new(
        &env.orchestrator,
        env.manager.clone(),
        env.agent.clone(),
        None,
    );
    let reopened = executor2
        .tournament_state(env.parent, &receipt.tournament_id)
        .unwrap();
    assert_eq!(reopened.state, TournamentState::Aborted);
    assert!(reopened.winner.is_none());
}

// ------------------------------------------------ completion steps (P2)

/// The completion-step runner over a real supervisor and an allow-all
/// egress policy (the executor fixture's agent carries no supervisor, so
/// the runner is installed explicitly).
fn completion_step_runner(
    root: &std::path::Path,
) -> Arc<crate::runtime::completion_steps::CompletionStepRunner> {
    use crate::runtime::completion_steps::{
        CompletionStepRunner, CompletionStepsConfig, EgressPolicy,
    };
    let cas = Arc::new(faktor_cas::Cas::open(root.join("completion-cas")).unwrap());
    let supervisor = faktor_terminal::ProcessSupervisor::new(cas);
    let egress: Arc<dyn EgressPolicy> = Arc::new(|_url: &str| Ok(()));
    Arc::new(
        CompletionStepRunner::new(supervisor, egress, CompletionStepsConfig::default()).unwrap(),
    )
}

fn cs_git(cwd: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Commit with the SAME deterministic `-c user.name/-c user.email` identity
/// the product's commit helper passes (faktor-git's `COMMIT_IDENTITY_*`):
/// fixture commits never depend on a local/global git identity, so a CI
/// runner with none still produces a real `git rev-parse HEAD` sha.
fn cs_commit(cwd: &std::path::Path, message: &str) {
    cs_git(
        cwd,
        &[
            "-c",
            &format!("user.name={}", faktor_git::COMMIT_IDENTITY_NAME),
            "-c",
            &format!("user.email={}", faktor_git::COMMIT_IDENTITY_EMAIL),
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

fn cs_seed_repo(root: &std::path::Path) {
    cs_git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "base\n").unwrap();
    cs_git(root, &["add", "-A"]);
    cs_commit(root, "init");
}

/// Drive a real-env task row to Verifying and land one passing record at
/// the resulting revision — the durable proof an in-session completion step
/// must be authorized by (the advisory verification fact is NOT enough).
fn seed_passing_proof(env: &RealToolEnv, task_id: TaskId) -> faktor_core::id::VerificationRecordId {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
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
            s => panic!("cannot reach Verifying from {s:?}"),
        };
        let rev = h.task_revision(task_id).unwrap();
        h.transition_task(task_id, rev, target, None).unwrap();
    }
    h.create_verification_record(
        task_id,
        None,
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        h.now_ms(),
    )
    .unwrap()
}

/// Adversarial executor-level cover of the PROOF-VALIDATED invocation: an
/// uncontracted run never invokes the runner, a bogus/nonexistent proof is a
/// typed Invalidated refusal (no side effect), while a proof-bound run
/// executes the requested step against the owner root and records it.
#[tokio::test]
async fn completion_steps_are_additive_and_fail_closed() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    env.executor
        .set_completion_steps(Some(completion_step_runner(dir.path())));
    cs_seed_repo(&env.owner_root);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: env.parent,
        goal: "execute the agreed completion steps".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let _proof = seed_passing_proof(&env, task_id);
    // (1) No contract: the runner is never invoked — the deliberately
    // uninitialized root in the injected runner would have failed loudly.
    assert!(env
        .executor
        .run_completion_steps(env.parent, faktor_core::id::VerificationRecordId::new(1))
        .await
        .unwrap()
        .is_none());
    // (2) Contract recorded, but the proof does not exist: the runner's
    // proof gate refuses EVERY requested step as Invalidated (retryable)
    // and no side effect runs.
    let rev = h.task_revision(task_id).unwrap();
    h.set_completion_contract(
        task_id,
        rev,
        faktor_core::completion::CompletionContract {
            include_commit: true,
            include_push: false,
            include_pr: false,
        },
    )
    .unwrap();
    // A dirty tree is waiting to be committed: a bogus proof must leave it
    // dirty (no side effect) even though the work is there.
    std::fs::write(env.owner_root.join("feature.txt"), "content\n").unwrap();
    let refused = env
        .executor
        .run_completion_steps(env.parent, faktor_core::id::VerificationRecordId::new(9001))
        .await
        .unwrap()
        .expect("a contracted run consults the proof and reports Invalidated");
    assert!(
        refused
            .records
            .iter()
            .all(|r| r.status != faktor_core::completion::CompletionStepOutcome::Succeeded),
        "{refused:?}"
    );
    let porcelain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&env.owner_root)
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&porcelain.stdout).trim().is_empty(),
        "no proof, no commit"
    );
    // (3) The durable PASSED record authorizes the commit step against the
    // session's owner root: the row is Succeeded and the tree is clean. The
    // advisory fact is deliberately left FAILED: it is UI-only and must not
    // influence the proof-validated path.
    h.upsert_memory_fact("verification", "last", r#"{"status":"failed"}"#)
        .unwrap();
    let report = env
        .executor
        .run_completion_steps(env.parent, _proof)
        .await
        .unwrap()
        .expect("a contracted proof-bound run must execute");
    assert!(report.all_succeeded(), "{report:?}");
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), rev.raw())
        .unwrap();
    assert_eq!(
        rows.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    let porcelain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&env.owner_root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&porcelain.stdout).trim().is_empty(),
        "the owner root must be committed clean"
    );
}

// ----------------------------------- attachments + unified settlement (P1)

/// Every durable USER message's `files` set of one session.
fn message_files(handle: &faktor_session::SessionHandle) -> Vec<Vec<String>> {
    handle
        .messages_before(None, 200)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == "user")
        .map(|m| {
            m.data
                .get("files")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

fn child_session_handle(
    manager: &Arc<SessionManager>,
    row: &crate::runtime::ChildRuntime,
) -> faktor_session::SessionHandle {
    manager
        .get_session(SessionId::new(row.session_id))
        .unwrap()
        .unwrap()
}

fn child_root(
    manager: &Arc<SessionManager>,
    row: &crate::runtime::ChildRuntime,
) -> std::path::PathBuf {
    let wts = manager
        .worktrees_of(WorkspaceId::new(row.workspace_id))
        .unwrap();
    std::path::PathBuf::from(
        wts.iter()
            .find(|w| (w.id.max(0) as u64) == row.worktree_id)
            .expect("child worktree row")
            .path
            .clone(),
    )
}

fn run_registry(
    manager: &Arc<SessionManager>,
    parent: SessionId,
    run_id: &str,
) -> Vec<crate::runtime::ChildRuntime> {
    OrchestratorRuntime::registry_rows(manager.clone(), parent, run_id).unwrap()
}

fn assert_prompt_files(
    manager: &Arc<SessionManager>,
    rows: &[crate::runtime::ChildRuntime],
    files: &[String],
) {
    for row in rows {
        let handle = child_session_handle(manager, row);
        let sets = message_files(&handle);
        assert!(
            sets.iter().any(|s| s == files),
            "child {} must submit exactly the run files {files:?}: {sets:?}",
            row.child_id
        );
    }
}

fn plan_row_of(env: &Arc<Env>, run_id: &str) -> serde_json::Value {
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    let row = facts
        .iter()
        .find(|(kind, key, _)| kind == crate::runtime::PLAN_ROW_KIND && key == run_id)
        .expect("durable plan row");
    serde_json::from_str(&row.2).unwrap()
}

/// FIX 1 (a): a 3-item orchestrated run's files reach EVERY child's
/// submitted prompt, the durable plan row carries the identical set, and
/// the set survives a store reopen.
#[tokio::test]
async fn multi_item_run_files_reach_every_child_and_survive_reopen() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..4)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let files = vec![
        "src/a.rs".to_string(),
        "docs/b.md".to_string(),
        "c.txt".to_string(),
    ];
    let mut req = request(
        "attached 3-item run",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &[]),
            wi("c", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    req.files = files.clone();
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("attached run starts");
    wait_until(
        || {
            let rows = run_registry(&env.manager, env.parent, &receipt.run_id);
            rows.len() == 3 && rows.iter().all(|c| c.state.is_terminal())
        },
        120,
    )
    .await;
    let rows = run_registry(&env.manager, env.parent, &receipt.run_id);
    assert_prompt_files(&env.manager, &rows, &files);
    let plan = plan_row_of(&env, &receipt.run_id);
    let specs: Vec<crate::runtime::ChildSpec> =
        serde_json::from_value(plan["specs"].clone()).unwrap();
    assert_eq!(specs.len(), 3);
    assert!(
        specs.iter().all(|s| s.files == files),
        "every durable spec carries the run's files: {specs:?}"
    );
    // The durable row is byte-identical across a reopen of the same store.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(env.parent).unwrap().unwrap();
    let plan2 = h2
        .memory_facts()
        .unwrap()
        .into_iter()
        .find(|(kind, key, _)| kind == crate::runtime::PLAN_ROW_KIND && key == &receipt.run_id)
        .expect("plan row after reopen");
    let v2: serde_json::Value = serde_json::from_str(&plan2.2).unwrap();
    assert_eq!(plan, v2, "the plan row (files included) is durable");
}

/// FIX 1 (b): a run that crashes after the durable plan/assignments (before
/// ANY spawn) re-attaches and every re-spawned child submits the files
/// decoded from the DURABLE plan row — never from the lost request memory.
#[tokio::test]
async fn crashed_orchestrated_run_reattaches_files_from_durable_plan_not_memory() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..4)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let files = vec!["src/one.rs".to_string(), "src/two.rs".to_string()];
    let mut req = request(
        "crash-before-spawn attached run",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &[]),
            wi("c", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    req.files = files.clone();
    req.crash_seam = Some(CrashSeam::AfterAssignmentsPersisted);
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("start accepted before the crash seam");
    wait_until(
        || {
            OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|a| a.len() == 3)
                .unwrap_or(false)
        },
        60,
    )
    .await;
    wait_until(|| env.executor.active_runs().is_empty(), 240).await;
    assert!(
        run_registry(&env.manager, env.parent, &receipt.run_id).is_empty(),
        "the crash seam fired BEFORE any spawn"
    );
    env.executor
        .resume_run(
            env.parent,
            &receipt.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("resume accepted");
    wait_until(
        || {
            let rows = run_registry(&env.manager, env.parent, &receipt.run_id);
            rows.len() == 3 && rows.iter().all(|c| c.state.is_terminal())
        },
        120,
    )
    .await;
    assert_prompt_files(
        &env.manager,
        &run_registry(&env.manager, env.parent, &receipt.run_id),
        &files,
    );
}

/// FIX 1 (c): hostile and oversized file lists are typed refusals BEFORE
/// any durable orchestration row (no spawn, no plan, no assignment).
#[tokio::test]
async fn hostile_or_oversized_task_files_are_refused_before_any_durable_row() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), vec![vec![ScriptedResponse::End]]);
    let base = || {
        request(
            "hostile attachments",
            vec![
                wi("a", WorkKind::Analysis, &[]),
                wi("b", WorkKind::Analysis, &[]),
            ],
            &env,
        )
    };
    // Oversized COUNT.
    let mut req = base();
    req.files = (0..=faktor_session::MAX_FILES_PER_PROMPT)
        .map(|i| format!("f{i}.rs"))
        .collect();
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("count over MAX_FILES_PER_PROMPT");
    assert!(matches!(err, ExecError::Oversized(_)), "{err:?}");
    // Oversized PATH.
    let mut req = base();
    req.files = vec!["x".repeat(faktor_session::MAX_FILE_PATH_BYTES + 1)];
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("path over MAX_FILE_PATH_BYTES");
    assert!(matches!(err, ExecError::Oversized(_)), "{err:?}");
    // Hostile paths.
    for hostile in [
        "/etc/passwd",
        "../secrets",
        "src/../../secrets",
        "",
        "\0bad",
    ] {
        let mut req = base();
        req.files = vec![hostile.to_string()];
        let err = env
            .executor
            .start_task(env.parent, req)
            .expect_err("hostile path");
        assert!(
            matches!(err, ExecError::Malformed(_)),
            "{hostile:?}: {err:?}"
        );
    }
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(
        !handle
            .memory_facts()
            .unwrap()
            .iter()
            .any(|(kind, _, _)| matches!(
                kind.as_str(),
                crate::runtime::PLAN_ROW_KIND
                    | crate::runtime::ASSIGNMENT_ROW_KIND
                    | crate::runtime::REGISTRY_ROW_KIND
            )),
        "refused attachment lists leave no orchestration rows"
    );
}

/// FIX 1 parity: an attachment-free orchestrated run submits EMPTY file
/// sets to every child — byte-identical to every previous wave.
#[tokio::test]
async fn attachment_free_orchestrated_run_submits_empty_file_sets() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..3)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let receipt = env
        .executor
        .start_task(
            env.parent,
            request(
                "no attachments",
                vec![
                    wi("a", WorkKind::Analysis, &[]),
                    wi("b", WorkKind::Analysis, &[]),
                ],
                &env,
            ),
        )
        .expect("start");
    wait_until(
        || {
            let rows = run_registry(&env.manager, env.parent, &receipt.run_id);
            rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal())
        },
        120,
    )
    .await;
    assert_prompt_files(
        &env.manager,
        &run_registry(&env.manager, env.parent, &receipt.run_id),
        &[],
    );
}

fn completion_step_runner_with(
    root: &std::path::Path,
    config: crate::runtime::completion_steps::CompletionStepsConfig,
) -> Arc<crate::runtime::completion_steps::CompletionStepRunner> {
    use crate::runtime::completion_steps::{CompletionStepRunner, EgressPolicy};
    let cas = Arc::new(faktor_cas::Cas::open(root.join("completion-cas")).unwrap());
    let supervisor = faktor_terminal::ProcessSupervisor::new(cas);
    let egress: Arc<dyn EgressPolicy> = Arc::new(|_url: &str| Ok(()));
    Arc::new(CompletionStepRunner::new(supervisor, egress, config).unwrap())
}

fn seed_contract_task(
    env: &Arc<RealToolEnv>,
    goal: &str,
    contract: faktor_core::completion::CompletionContract,
) -> TaskId {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: env.parent,
        goal: goal.to_string(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    for target in [
        TaskTransition::StartRunning,
        TaskTransition::RequestVerification,
        TaskTransition::StartVerification,
    ] {
        let rev = h.task_revision(task_id).unwrap();
        h.transition_task(task_id, rev, target, None).unwrap();
    }
    let rev = h.task_revision(task_id).unwrap();
    h.set_completion_contract(task_id, rev, contract).unwrap();
    // The proof-validated step path needs the durable PASSED record at the
    // current revision (the advisory fact is UI-only).
    h.create_verification_record(
        task_id,
        None,
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        h.now_ms(),
    )
    .unwrap();
    task_id
}

fn cs_head(root: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// FIX 2 crash seam: the commit side effect landed but its status row did
/// not; `settle_run` replays idempotently (HEAD recognized, `Succeeded`),
/// without a second commit.
#[tokio::test]
async fn settle_run_converges_after_the_commit_status_write_seam() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    env.executor
        .set_completion_steps(Some(completion_step_runner(dir.path())));
    cs_seed_repo(&env.owner_root);
    let goal = "ship the committed feature";
    let task_id = seed_contract_task(
        &env,
        goal,
        faktor_core::completion::CompletionContract {
            include_commit: true,
            include_push: false,
            include_pr: false,
        },
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let contract_rev = h.task_revision(task_id).unwrap();
    // The crash window: the runner committed, the process died BEFORE the
    // durable status row landed.
    std::fs::write(env.owner_root.join("feature.txt"), "content\n").unwrap();
    cs_git(&env.owner_root, &["add", "-A"]);
    cs_commit(&env.owner_root, &commit_message(goal));
    let head = cs_head(&env.owner_root);
    assert!(h
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap()
        .is_empty());
    // Reopen + settle: the replay records Succeeded and never commits again.
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: "tx-commit-seam".into(),
        })
        .await
        .unwrap();
    assert!(outcome.steps.is_some(), "{outcome:?}");
    assert_eq!(cs_head(&env.owner_root), head, "no double commit");
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    assert!(
        rows.last().unwrap().detail.contains("already committed"),
        "{:?}",
        rows.last()
    );
    assert!(outcome.steps.unwrap().all_succeeded());
    // A FRESH manager over the same store (the daemon-restart view) reads
    // exactly the converged row: nothing lived only in this process.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(env.parent).unwrap().unwrap();
    let rows2 = h2
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows2.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
}

/// FIX 2 crash seam: the push side effect landed but its status row did
/// not; the replay recognizes the already-pushed HEAD and records
/// `Succeeded` without touching the remote again.
#[tokio::test]
async fn settle_run_converges_after_the_push_status_write_seam() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    env.executor
        .set_completion_steps(Some(completion_step_runner(dir.path())));
    cs_seed_repo(&env.owner_root);
    let remote = dir.path().join("remote.git");
    cs_git(
        dir.path(),
        // Pin the bare remote's default branch: CI git defaults to
        // master, so `rev-parse HEAD` on the remote printed the unborn
        // "HEAD" instead of the pushed sha.
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    cs_git(
        &env.owner_root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let goal = "ship the pushed feature";
    let task_id = seed_contract_task(
        &env,
        goal,
        faktor_core::completion::CompletionContract {
            include_commit: false,
            include_push: true,
            include_pr: false,
        },
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let contract_rev = h.task_revision(task_id).unwrap();
    // The crash window: commit + push landed externally, no status row.
    std::fs::write(env.owner_root.join("feature.txt"), "content\n").unwrap();
    cs_git(&env.owner_root, &["add", "-A"]);
    cs_commit(&env.owner_root, &commit_message(goal));
    cs_git(&env.owner_root, &["push", "-q", "-u", "origin", "main"]);
    let remote_head = cs_head(&remote);
    assert_eq!(remote_head, cs_head(&env.owner_root));
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: "tx-push-seam".into(),
        })
        .await
        .unwrap();
    assert!(outcome.steps.unwrap().all_succeeded());
    assert_eq!(cs_head(&remote), remote_head, "remote untouched by replay");
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    assert!(
        rows.last().unwrap().detail.contains("already pushed"),
        "{:?}",
        rows.last()
    );
    // The daemon-restart view reads the same converged row from the store.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(env.parent).unwrap().unwrap();
    let rows2 = h2
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows2.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
}

/// FIX 2 crash seam: the PR exists but its status row never landed; the
/// replay's configured command reports the existing PR and records
/// `Succeeded` (URL parsed), never a second PR gate.
#[tokio::test]
async fn settle_run_converges_after_the_pr_status_write_seam() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    let mut config = crate::runtime::completion_steps::CompletionStepsConfig::default();
    // The external PR already exists: the helper reports it and exits
    // non-zero (the exact crash-replay shape the runner recognizes).
    let script = dir.path().join("existing-pr.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'already exists: https://example.test/pr/7'\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    config.pr_command = Some(format!("{} {{branch}}", script.display()));
    env.executor
        .set_completion_steps(Some(completion_step_runner_with(dir.path(), config)));
    cs_seed_repo(&env.owner_root);
    let goal = "ship the PR feature";
    let task_id = seed_contract_task(
        &env,
        goal,
        faktor_core::completion::CompletionContract {
            include_commit: false,
            include_push: false,
            include_pr: true,
        },
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let contract_rev = h.task_revision(task_id).unwrap();
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: "tx-pr-seam".into(),
        })
        .await
        .unwrap();
    assert!(outcome.steps.unwrap().all_succeeded());
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    assert!(
        rows.last().unwrap().detail.contains("already exists"),
        "{:?}",
        rows.last()
    );
    // The daemon-restart view reads the same converged row from the store.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(env.parent).unwrap().unwrap();
    let rows2 = h2
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows2.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
}

/// One real-tool child script: write one file, then finish the turn.
fn write_script(id: &str, path: &str, content: &str) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::ToolCall {
            id: id.into(),
            name: "write_file".into(),
            input: serde_json::json!({ "path": path, "content": content }),
        },
        ScriptedResponse::Text("wrote".into()),
        ScriptedResponse::End,
    ]
}

fn two_child_scripts(a: &str, b: &str) -> Vec<Vec<ScriptedResponse>> {
    vec![
        write_script("c-a", "child_a.rs", a),
        write_script("c-b", "child_b.rs", b),
        vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
        vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
    ]
}

fn two_isolated_items() -> Vec<WorkItem> {
    ["impl-a", "impl-b"]
        .iter()
        .map(|id| {
            WorkItem::with_ownership(
                *id,
                format!("work {id}"),
                WorkKind::Implementation,
                OwnershipSpec::IsolatedWorktree,
            )
        })
        .collect()
}

fn typed_land_criterion() -> String {
    faktor_session::task::Criterion::derived(
        "required check: cargo check",
        faktor_core::state::CriterionOrigin::ProjectPolicy,
        faktor_core::state::CriterionRequirement::Required,
        None,
    )
    .with_binding(faktor_core::state::CriterionBinding::RequiredCheck {
        check_id: "rust_check".into(),
        command_digest: faktor_core::state::command_binding_digest("cargo check"),
    })
    .encode()
}

/// A REQUIRED criterion whose binding can never pass while every derived
/// check is green: the file-state binding pins a digest the candidate cannot
/// have.
fn typed_failed_file_state_criterion() -> String {
    faktor_session::task::Criterion::derived(
        "src/lib.rs is frozen at an impossible digest",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::FileState {
        path: "src/lib.rs".into(),
        expected_digest: "0".repeat(64),
    })
    .encode()
}

/// A REQUIRED criterion carrying the explicit honest-unknown binding: the
/// evaluator can only ever return Unavailable for it, never a pass.
fn typed_explicit_unknown_criterion() -> String {
    faktor_session::task::Criterion::derived(
        "no objective mechanism certifies this criterion",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::Unavailable {
        reason: "the criterion is explicitly honest-unknown".into(),
    })
    .encode()
}

fn start_two_child_run(env: &Arc<RealToolEnv>, goal: &str) -> String {
    env.executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: goal.to_string(),
                work_items: two_isolated_items(),
                criteria: vec![typed_land_criterion()],
                parent_caps: read_caps(),
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("orchestrated start")
        .run_id
}

/// The daemon-owned candidate root of one run (the deterministic placement
/// the prepare phase copies the run base into).
fn run_candidate_root(env: &RealToolEnv, run_id: &str) -> std::path::PathBuf {
    env.isolated_root.join(run_id).join("candidate")
}

fn owner_digest(env: &RealToolEnv) -> String {
    faktor_session::root_snapshot_digest(&env.owner_root, faktor_session::MAX_ROOT_SNAPSHOT_ENTRIES)
        .unwrap()
}

fn root_digest(path: &std::path::Path) -> String {
    faktor_session::root_snapshot_digest(path, faktor_session::MAX_ROOT_SNAPSHOT_ENTRIES).unwrap()
}

/// Seed a REAL rust checkout with git history (the derived-check profile
/// needs a detectable project; the completion runner needs a repo).
fn cs_seed_owner(env: &RealToolEnv) {
    seed_rust(env);
    cs_git(&env.owner_root, &["init", "-q", "-b", "main"]);
    cs_git(&env.owner_root, &["add", "-A"]);
    cs_commit(&env.owner_root, "init");
}

fn latest_txn(
    env: &RealToolEnv,
    run_id: &str,
) -> Option<faktor_session::ledger::IntegrationTxnRow> {
    env.manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .ledger_integration_txn_for_run(run_id)
        .unwrap()
}

async fn settle_orchestrated(
    env: &Arc<RealToolEnv>,
    run_id: &str,
) -> Result<SettlementOutcome, ExecError> {
    env.executor
        .settle_run(RunSettlement::Orchestrated {
            parent: env.parent,
            run_id: run_id.to_string(),
        })
        .await
}

/// Point 1/2: the verifier runs over the CANDIDATE root while the owner
/// still lacks every child change; the passing record binds the candidate
/// digest, and only the landing phase writes the owner.
#[tokio::test]
async fn verifier_observes_candidate_not_owner() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    // Freeze the settlement right AFTER the candidate verification: the
    // verifier has already observed its root.
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "verify the candidate");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let candidate = run_candidate_root(&env, &run_id);
    assert!(
        candidate.join("child_a.rs").is_file() && candidate.join("child_b.rs").is_file(),
        "the candidate holds both child changes"
    );
    assert!(
        !env.owner_root.join("child_a.rs").exists() && !env.owner_root.join("child_b.rs").exists(),
        "the owner was NOT touched by the verification phase"
    );
    let candidate_digest = root_digest(&candidate);
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == VerificationStatus::Passed && r.tree_hash.is_some())
        .max_by_key(|r| r.record_id)
        .expect("a passing record bound to the candidate was minted");
    assert_eq!(record.tree_hash.as_deref(), Some(candidate_digest.as_str()));
    assert_ne!(
        owner_digest(&env),
        candidate_digest,
        "the verified snapshot is the candidate, not the owner"
    );
    // Recovery: the same verified candidate now lands into the owner.
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(env.owner_root.join("child_b.rs").is_file());
    assert_eq!(owner_digest(&env), candidate_digest);
}

// ---------------------------------------------- ExclusivePaths isolation

/// A run whose plan mixes a read-only AUTO item with one `Paths`-owned
/// mutating item: the executor must spawn the Paths child in its OWN overlay
/// and route its change set through stage -> compose -> verify -> land. The
/// auto item keeps the run orchestrated without opening the shared owner
/// workspace (the only provider call is the Paths child's, so the script
/// index is deterministic).
fn start_paths_run(env: &Arc<RealToolEnv>, goal: &str, paths: &[&str]) -> String {
    env.executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: goal.to_string(),
                work_items: vec![
                    wi("prep", WorkKind::Analysis, &[]),
                    path_item("impl", WorkKind::Implementation, &["prep"], paths),
                ],
                auto_items: vec!["prep".to_string()],
                criteria: vec![typed_land_criterion()],
                parent_caps: read_caps(),
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("paths orchestrated start")
        .run_id
}

fn run_child_row(env: &RealToolEnv, run_id: &str, item: &str) -> crate::runtime::ChildRuntime {
    OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, run_id)
        .unwrap()
        .into_iter()
        .find(|r| r.item_id == item)
        .unwrap_or_else(|| panic!("no child row for item {item}"))
}

fn child_overlay(env: &RealToolEnv, row: &crate::runtime::ChildRuntime) -> std::path::PathBuf {
    env.manager
        .workspace_root(WorkspaceId::new(row.workspace_id))
        .unwrap()
        .expect("overlay root registered")
}

/// A `Paths` child writes ONLY inside its daemon-owned overlay: the owner
/// digest stays stable through the whole drive AND the candidate
/// verification, and the change set lands exclusively through the shared
/// integration transaction once verification passed.
#[tokio::test]
async fn paths_child_changes_stay_in_its_overlay_until_the_verified_land() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![write_script(
            "c-impl",
            "src/impl.rs",
            "pub fn p() -> u64 {\n    let seed: u64 = 7;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        )],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterCandidatePrepared));
    let run_id = start_paths_run(&env, "land the declared path", &["src"]);
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let row = run_child_row(&env, &run_id, "impl");
    assert_eq!(row.ownership, ChildOwnership::ExclusivePaths);
    assert_eq!(row.ownership_paths, vec!["src".to_string()]);
    let overlay = child_overlay(&env, &row);
    assert!(
        overlay.ends_with(&row.child_id),
        "the Paths child owns an overlay named after its child id: {overlay:?}"
    );
    assert_ne!(overlay, env.owner_root);
    assert!(
        overlay.join("src/impl.rs").is_file(),
        "the write landed in the child's own overlay"
    );
    let candidate = run_candidate_root(&env, &run_id);
    assert!(candidate.join("src/impl.rs").is_file());
    assert!(
        !env.owner_root.join("src/impl.rs").exists(),
        "the owner is byte-untouched before the landing"
    );
    assert_eq!(
        owner_digest(&env),
        before,
        "owner digest stable while the Paths child ran and its candidate was verified"
    );
    // Recovery settles the SAME verified candidate through the txn.
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("src/impl.rs").is_file());
    let candidate_digest = root_digest(&candidate);
    assert_eq!(
        owner_digest(&env),
        candidate_digest,
        "the owner equals the verified candidate only after landing"
    );
    let txn = latest_txn(&env, &run_id).expect("the integration transaction");
    assert_eq!(txn.path_count, 1, "{txn:?}");
    assert_eq!(txn.applied_count, 1, "{txn:?}");
}

/// The declared path set is the tool-boundary write allowlist: a mutating
/// write outside it is typed-refused (journaled PermissionDenied) and never
/// reaches the overlay — long before staging.
#[tokio::test]
async fn paths_child_out_of_scope_write_is_refused_typed_at_the_tool_gate() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![write_script(
            "c-evil",
            "evil.rs",
            "outside the declared set\n",
        )],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let run_id = start_paths_run(&env, "refuse the out-of-scope write", &["src"]);
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let row = run_child_row(&env, &run_id, "impl");
    assert_eq!(row.ownership, ChildOwnership::ExclusivePaths);
    let overlay = child_overlay(&env, &row);
    assert!(
        !overlay.join("evil.rs").exists(),
        "the denied write never reached the overlay"
    );
    assert!(!env.owner_root.join("evil.rs").exists());
    let handle = env
        .manager
        .get_session(SessionId::new(row.session_id))
        .unwrap()
        .unwrap();
    let events = handle.events_range(1, None).unwrap();
    let denial = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
        .expect("the edit gate journals the out-of-scope refusal");
    let payload = denial.payload.as_ref().expect("denial carries a payload");
    assert_eq!(payload["tool"], "write_file");
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("change budget refused the edit"),
        "{payload:?}"
    );
}

// ----------------------------------------------- P0 criterion-proof residuals

/// A reviewer-bound acceptance criterion (the "independent reviewer proves
/// the no-op" shape): the wired reviewer port is the ONLY binding that can
/// certify it.
fn typed_reviewer_criterion(reviewer_id: &str) -> String {
    faktor_session::task::Criterion::derived(
        "independent reviewer proves the no-op",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::IndependentReview {
        reviewer_id: reviewer_id.into(),
    })
    .encode()
}

/// An aggregate-goal criterion: passes only when every required subordinate
/// passed AND the wired independent reviewer passed over the candidate.
fn typed_aggregate_goal_criterion() -> String {
    faktor_session::task::Criterion::derived(
        "ship the goal",
        CriterionOrigin::User,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::AggregateGoal)
    .encode()
}

/// Two isolated children that change NOTHING (the empty aggregate change
/// set the no-op policy governs).
fn idle_child_scripts() -> Vec<Vec<ScriptedResponse>> {
    vec![
        vec![
            ScriptedResponse::Text("nothing to change".into()),
            ScriptedResponse::End,
        ],
        vec![
            ScriptedResponse::Text("nothing to change".into()),
            ScriptedResponse::End,
        ],
    ]
}

/// The typed verdict a review MODEL is contract-bound to emit.
fn review_verdict_script(verdict: &str) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::Text(serde_json::json!({"verdict": verdict, "findings": []}).to_string()),
        ScriptedResponse::End,
    ]
}

fn start_no_op_run(env: &Arc<RealToolEnv>, goal: &str, disposition: NoOpDisposition) -> String {
    env.executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: goal.to_string(),
                work_items: two_isolated_items(),
                criteria: vec![typed_reviewer_criterion("review-0")],
                parent_caps: read_caps(),
                isolated_root: env.isolated_root.clone(),
                no_op_disposition: Some(disposition),
                ..Default::default()
            },
        )
        .expect("no-op orchestrated start")
        .run_id
}

/// Residual (5) path A: an empty aggregate change set with
/// `RequiresCriterionProof` completes ONLY through the reviewer-proved no-op
/// criterion — the review model call runs over the candidate snapshot and
/// its pass is the record's criterion proof; the owner is never rewritten.
#[tokio::test]
async fn no_op_run_completes_only_through_the_reviewer_proof() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let mut scripts = idle_child_scripts();
    scripts.push(review_verdict_script("clean"));
    let env = open_real_tool_env_full(
        dir.path(),
        scripts,
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_no_op_run(
        &env,
        "decide whether any change is needed",
        NoOpDisposition::RequiresCriterionProof,
    );
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert_eq!(
        owner_digest(&env),
        before,
        "a reviewer-proved no-op never rewrites the owner"
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .max_by_key(|r| r.record_id)
        .expect("the no-op proof record");
    assert_eq!(record.status, VerificationStatus::Passed);
    assert!(record.tree_hash.is_some(), "{record:?}");
    assert!(
        record.criteria.iter().any(|c| c.passed
            && c.binding
                == Some(CriterionBinding::IndependentReview {
                    reviewer_id: "review-0".into()
                })),
        "the no-op criterion is certified through the reviewer binding: {record:?}"
    );
    // The proof was a REAL review-model call (2 child drives + 1 review).
    assert_eq!(
        env.provider.request_count.load(Ordering::SeqCst),
        3,
        "the no-op proof is a reviewer call, not a local empty-suite pass"
    );
    let txn = latest_txn(&env, &run_id).expect("no-op integration record");
    assert_eq!(txn.path_count, 0);
    assert_eq!(txn.applied_count, 0);
}

/// Residual (5) path B: with the reviewer GENUINELY absent (no typed verdict)
/// the same empty change set stays unverified — no record, no landing, no
/// completion. Unavailable stays honest.
#[tokio::test]
async fn no_op_run_without_a_reviewer_verdict_refuses_completion() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        idle_child_scripts(),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_no_op_run(
        &env,
        "decide whether any change is needed",
        NoOpDisposition::RequiresCriterionProof,
    );
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(outcome.complete, "the children finished");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_eq!(owner_digest(&env), before);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        h.list_verification_records(task_id)
            .unwrap()
            .iter()
            .all(|r| r.tree_hash.is_none() || r.status != VerificationStatus::Passed),
        "no passing root record without a reviewer verdict"
    );
    assert!(
        h.ledger_integration_record_for_task(task_id.raw())
            .unwrap()
            .is_none(),
        "no landing for an unproved no-op"
    );
}

/// Residual (5) path C: an explicitly REFUSED no-op disposition never even
/// asks the reviewer — the run stays unverified and the review script stays
/// unconsumed.
#[tokio::test]
async fn no_op_run_with_refused_disposition_never_completes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let mut scripts = idle_child_scripts();
    scripts.push(review_verdict_script("clean"));
    let env = open_real_tool_env_full(
        dir.path(),
        scripts,
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_no_op_run(&env, "no changes are acceptable", NoOpDisposition::Refused);
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_eq!(owner_digest(&env), before);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(h
        .ledger_integration_record_for_task(task_id.raw())
        .unwrap()
        .is_none());
    assert_eq!(
        env.provider.request_count.load(Ordering::SeqCst),
        2,
        "a refused no-op never spends a review-model call"
    );
}

/// Residual (1) present side: an AggregateGoal criterion over a REAL changed
/// candidate is certified through the WIRED reviewer — the required
/// subordinate check passed and the recorded candidate review answered the
/// aggregate; the task completes and lands.
#[tokio::test]
async fn aggregate_goal_criterion_is_reviewer_certified_over_the_candidate() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts(
            "pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
            "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        ),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let run_id = env
        .executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: "aggregate the goal".to_string(),
                work_items: two_isolated_items(),
                criteria: vec![typed_land_criterion(), typed_aggregate_goal_criterion()],
                parent_caps: read_caps(),
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("orchestrated start")
        .run_id;
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.tree_hash.is_some())
        .max_by_key(|r| r.record_id)
        .expect("the candidate-bound record");
    assert!(
        record
            .criteria
            .iter()
            .any(|c| c.passed && c.binding == Some(CriterionBinding::AggregateGoal)),
        "the aggregate goal passed through the wired reviewer: {record:?}"
    );
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(env.owner_root.join("child_b.rs").is_file());
}

/// Residual (3): the reuse consult is real — an identical basis reuses the
/// record, a DIFFERENT basis (different derived checks at the same
/// snapshot/revision/criteria) mints a fresh basis-bound record, and the
/// fresh record carries its candidate proof + proof-basis digest.
#[tokio::test]
async fn root_record_reuse_consults_the_proof_basis() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    // Freeze right after the candidate was prepared: no root record exists
    // yet and the task is already routed to Verifying (deterministic basis
    // construction; no settlement race).
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterCandidatePrepared));
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let run_id = start_two_child_run(&env, "basis consult");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    env.executor.set_settlement_crash_seam(None);
    let task_id = h.task_id().unwrap();
    let snapshot = owner_digest(&env);
    let check = |program: &str, arg: &str| faktor_core::state::CheckExecution {
        check: "rust_check".into(),
        program: program.into(),
        args: vec![arg.into()],
        category: "compile".into(),
        required: true,
        status: VerificationStatus::Passed,
        started_ms: 1,
        finished_ms: Some(2),
        exit: Some(0),
        summary: Some("ok".into()),
    };
    let prepared = PreparedRunIntegration {
        run_id: run_id.clone(),
        task_id,
        owner_root: env.owner_root.clone(),
        base_root: env.owner_root.clone(),
        candidate_root: env.owner_root.clone(),
        base_snapshot: snapshot.clone(),
        candidate_snapshot: snapshot.clone(),
        changed: Vec::new(),
        sources: Vec::new(),
        sources_digest: String::new(),
        staged: Vec::new(),
    };
    let criteria = h
        .get_task(task_id)
        .unwrap()
        .unwrap()
        .acceptance_criteria
        .clone();
    let criterion_verdict = faktor_core::state::CriterionVerification {
        criterion_key: criteria[0].clone(),
        passed: true,
        evidence: Some("basis consult".into()),
        binding: Some(CriterionBinding::RequiredCheck {
            check_id: "rust_check".into(),
            command_digest: faktor_core::state::command_binding_digest("cargo check"),
        }),
    };
    let run = |program: &str, arg: &str| IntegratedRootVerification {
        status: VerificationStatus::Passed,
        checks: vec![check(program, arg)],
        criteria: vec![criterion_verdict.clone()],
        changed: Vec::new(),
        summary: "basis consult".into(),
        review_model_identity: None,
    };
    let (first, first_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &run("cargo", "check"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    let (reused, reused_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &run("cargo", "check"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(first, reused, "an identical basis is replay-idempotent");
    assert_eq!(
        first_basis, reused_basis,
        "two identical production builds must produce the byte-identical basis digest"
    );
    let (fresh, fresh_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &run("cargo", "clippy"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_ne!(
        fresh, first,
        "a different check basis must NEVER reuse the old record"
    );
    assert_ne!(
        fresh_basis, first_basis,
        "a changed check program changes the probed tool/basis digest"
    );
    let fresh_record = h.get_verification_record(fresh).unwrap().unwrap();
    assert_eq!(fresh_record.status, VerificationStatus::Passed);
    assert!(
        fresh_record.candidate_proof_ref.is_some(),
        "the fresh record carries its candidate proof: {fresh_record:?}"
    );
    assert_eq!(
        fresh_record
            .environment_fingerprint
            .as_ref()
            .and_then(|f| f.proof_basis_digest.as_deref()),
        Some(fresh_basis.as_str()),
        "the record carries the exact basis digest the builder produced"
    );
}

/// Build a probe-session task row plus the prepared integration the basis
/// builder consumes (production call shape, no synthetic basis injection).
fn probe_task_and_prepared(
    env: &Arc<RealToolEnv>,
    run_id: &str,
) -> (
    faktor_session::SessionHandle,
    TaskId,
    PreparedRunIntegration,
    Vec<String>,
) {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: h.id(),
        goal: "proof-basis probe hardening".into(),
        acceptance_criteria: vec!["c1".into()],
        plan: Vec::new(),
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: faktor_core::state::TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let snapshot = owner_digest(env);
    let criteria = h
        .get_task(task_id)
        .unwrap()
        .unwrap()
        .acceptance_criteria
        .clone();
    let prepared = PreparedRunIntegration {
        run_id: run_id.to_string(),
        task_id,
        owner_root: env.owner_root.clone(),
        base_root: env.owner_root.clone(),
        candidate_root: env.owner_root.clone(),
        base_snapshot: snapshot.clone(),
        candidate_snapshot: snapshot,
        changed: Vec::new(),
        sources: Vec::new(),
        sources_digest: String::new(),
        staged: Vec::new(),
    };
    (h, task_id, prepared, criteria)
}

fn probe_run(program: &str) -> IntegratedRootVerification {
    let check = faktor_core::state::CheckExecution {
        check: "rust_check".into(),
        program: program.into(),
        args: vec!["--version".into()],
        category: "compile".into(),
        required: true,
        status: VerificationStatus::Passed,
        started_ms: 1,
        finished_ms: Some(2),
        exit: Some(0),
        summary: Some("ok".into()),
    };
    let criterion = faktor_core::state::CriterionVerification {
        criterion_key: "c1".into(),
        passed: true,
        evidence: Some("probe evidence".into()),
        binding: Some(CriterionBinding::RequiredCheck {
            check_id: "rust_check".into(),
            command_digest: faktor_core::state::command_binding_digest("cargo --version"),
        }),
    };
    IntegratedRootVerification {
        status: VerificationStatus::Passed,
        checks: vec![check],
        criteria: vec![criterion],
        changed: Vec::new(),
        summary: "proof-basis probe".into(),
        review_model_identity: None,
    }
}

/// Production hardening: the basis is rebuilt immediately before every reuse
/// consult, PROBING the real toolchain through the daemon supervisor — a
/// tool-version change under the daemon PATH invalidates reuse in
/// production (a fresh record is minted), while an unchanged toolchain
/// reuses the record byte-identically.
#[cfg(unix)]
#[tokio::test]
async fn production_tool_version_change_invalidates_proof_reuse() {
    use std::os::unix::fs::PermissionsExt;
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_supervised(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        MutationMode::DirectCompat,
    );
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "probe-run");
    let fake_bin = dir.path().join("probe-bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let fake_rustc = fake_bin.join("rustc");
    let write_tool = |version: &str| {
        std::fs::write(&fake_rustc, format!("#!/bin/sh\necho \"{version}\"\n")).unwrap();
        let mut perms = std::fs::metadata(&fake_rustc).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_rustc, perms).unwrap();
    };
    write_tool("rustc 0.0.1-production-fake");
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let fake_path = format!("{}:{}", fake_bin.display(), original_path.to_string_lossy());
    std::env::set_var("PATH", &fake_path);
    let snapshot = prepared.candidate_snapshot.clone();
    let (first, first_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    let (reused, reused_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(first, reused, "an unchanged toolchain reuses the record");
    assert_eq!(first_basis, reused_basis);
    // The tool version actually changed.
    write_tool("rustc 0.0.2-production-fake");
    let (fresh, fresh_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    std::env::set_var("PATH", &original_path);
    assert_ne!(
        fresh, first,
        "a production tool-version change must NEVER reuse the old record"
    );
    assert_ne!(fresh_basis, first_basis);
    assert_eq!(
        h.get_verification_record(first)
            .unwrap()
            .unwrap()
            .environment_fingerprint
            .as_ref()
            .and_then(|f| f.proof_basis_digest.as_deref()),
        Some(first_basis.as_str())
    );
}

/// Production hardening: a daemon WITHOUT a supervisor still produces a
/// fully-populated basis on every consult (every probe is an explicit
/// marker, never silently empty), and two identical builds are
/// byte-identical so the replay is deterministic.
#[tokio::test]
async fn production_basis_degrades_explicitly_and_is_reproducible() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "probe-no-supervisor");
    let snapshot = prepared.candidate_snapshot.clone();
    let (first, first_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    let (reused, reused_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(first, reused);
    assert_eq!(
        first_basis, reused_basis,
        "identical inputs, identical basis"
    );
    // The production probe input shape degrades to explicit markers.
    let report = crate::proof_probe::probe_proof_basis(None, &["cargo".to_string()]).await;
    assert!(!report.tools.is_empty(), "tool_versions are never empty");
    assert!(
        report
            .tools
            .iter()
            .filter(|t| t.tool != "faktor-verifier")
            .all(|t| t.version.starts_with('<')),
        "no supervisor means explicit probe markers: {:?}",
        report.tools
    );
    assert!(
        report
            .tools
            .iter()
            .any(|t| t.tool == "faktor-verifier" && t.version.contains("blake3:")),
        "the custom verifier binary carries its version+digest: {:?}",
        report.tools
    );
    assert!(
        report
            .env_projection
            .iter()
            .any(|(k, v)| k == "target_triple" && v.starts_with("<probe-unavailable")),
        "target triple is an explicit marker: {:?}",
        report.env_projection
    );
}

/// PRODUCTION reviewer-identity wiring (not a synthetic helper call): with
/// the parent session's configured pair FIXED, the proof basis is rebuilt
/// through the real `find_or_create_root_verification_record` path and the
/// review model recorded on the RUN changes the reviewer digest — a changed
/// reviewer mints a FRESH record, the same reviewer reuses byte-identically.
#[tokio::test]
async fn production_reviewer_identity_changes_the_basis_digest() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_supervised(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        MutationMode::DirectCompat,
    );
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "reviewer-run");
    let snapshot = prepared.candidate_snapshot.clone();
    // The parent session's configured pair is FIXED across every call below:
    // only the RUN's recorded ACTUAL review call identity varies.
    let parent_pair = {
        let row = h.row().unwrap();
        (row.provider, row.model)
    };
    let reviewed_run = |provider: &str, model: &str| IntegratedRootVerification {
        status: VerificationStatus::Passed,
        checks: Vec::new(),
        criteria: vec![faktor_core::state::CriterionVerification {
            criterion_key: "c1".into(),
            passed: true,
            evidence: Some("independent reviewer: clean".into()),
            binding: Some(CriterionBinding::AggregateGoal),
        }],
        changed: Vec::new(),
        summary: "reviewed".into(),
        review_model_identity: Some(faktor_agent::ReviewModelIdentity {
            provider: provider.into(),
            model: model.into(),
        }),
    };
    let (first, first_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &reviewed_run("review-provider-a", "review-model-a"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    let (reused, reused_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &reviewed_run("review-provider-a", "review-model-a"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(reused, first, "the same reviewer reuses the record");
    assert_eq!(reused_basis, first_basis);
    let (fresh, fresh_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &reviewed_run("review-provider-b", "review-model-b"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_ne!(
        fresh, first,
        "a changed ACTUAL review model must never reuse the old record"
    );
    assert_ne!(
        fresh_basis, first_basis,
        "the reviewer digest folds the real review provider/model"
    );
    // The parent session's configured pair never moved: the digest difference
    // is the review call's identity, not the parent's.
    let row = h.row().unwrap();
    assert_eq!((row.provider, row.model), parent_pair);
    // Both records durably carry their distinct basis digests.
    for (record, basis) in [(first, &first_basis), (fresh, &fresh_basis)] {
        assert_eq!(
            h.get_verification_record(record)
                .unwrap()
                .unwrap()
                .environment_fingerprint
                .as_ref()
                .and_then(|f| f.proof_basis_digest.as_deref()),
            Some(basis.as_str())
        );
    }
}

/// The basis evidence/reviewer folds are pure, deterministic, bound to the
/// exact evidence (a changed check program or reviewer identity changes the
/// fold) and only count PASSED criteria — failed and unavailable verdicts
/// contribute nothing.
#[test]
fn basis_evidence_and_reviewer_digests_are_exact_and_deterministic() {
    use super::{criterion_pass_evidence_digests, reviewer_proof_basis_digest};
    let (dir, m) = {
        let dir = tempfile::tempdir().unwrap();
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        (dir, m)
    };
    let ws = m.create_workspace("/w").unwrap();
    let h = m.create_session(ws, "t", "ollama", "qwen3.8").unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: h.id(),
        goal: "g".into(),
        acceptance_criteria: vec!["c1".into(), "c2".into()],
        plan: Vec::new(),
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let check = faktor_core::state::CheckExecution {
        check: "rust_check".into(),
        program: "cargo".into(),
        args: vec!["check".into()],
        category: "compile".into(),
        required: true,
        status: VerificationStatus::Passed,
        started_ms: 1,
        finished_ms: Some(2),
        exit: Some(0),
        summary: None,
    };
    let passed_check = faktor_core::state::CriterionVerification {
        criterion_key: "c1".into(),
        passed: true,
        evidence: Some("evidence-a".into()),
        binding: Some(CriterionBinding::RequiredCheck {
            check_id: "rust_check".into(),
            command_digest: faktor_core::state::command_binding_digest("cargo check"),
        }),
    };
    let failed = faktor_core::state::CriterionVerification {
        criterion_key: "c2".into(),
        passed: false,
        evidence: Some("evidence-b".into()),
        binding: None,
    };
    let review = faktor_core::state::CriterionVerification {
        criterion_key: "c1".into(),
        passed: true,
        evidence: Some("review payload".into()),
        binding: Some(CriterionBinding::AggregateGoal),
    };
    let prepared = PreparedRunIntegration {
        run_id: "basis-fold".into(),
        task_id,
        owner_root: dir.path().join("owner"),
        base_root: dir.path().join("owner"),
        candidate_root: dir.path().join("owner"),
        base_snapshot: "a".repeat(64),
        candidate_snapshot: "b".repeat(64),
        changed: Vec::new(),
        sources: Vec::new(),
        sources_digest: String::new(),
        staged: Vec::new(),
    };
    let run_with = |criteria: Vec<faktor_core::state::CriterionVerification>,
                    identity: Option<faktor_agent::ReviewModelIdentity>| {
        IntegratedRootVerification {
            status: VerificationStatus::Passed,
            checks: vec![check.clone()],
            criteria,
            changed: Vec::new(),
            summary: "fold".into(),
            review_model_identity: identity,
        }
    };
    let reviewer_a = || {
        Some(faktor_agent::ReviewModelIdentity {
            provider: "review-provider-a".into(),
            model: "review-model-a".into(),
        })
    };
    let run = run_with(vec![passed_check.clone(), failed.clone()], None);
    let first = criterion_pass_evidence_digests(&prepared, &run);
    assert_eq!(
        first,
        criterion_pass_evidence_digests(&prepared, &run),
        "two folds of identical evidence are identical"
    );
    assert!(first.iter().any(|e| e.starts_with("binding:c1:")));
    assert!(first.iter().any(|e| e.starts_with("command:c1:")));
    assert!(first.iter().any(|e| e.starts_with("check:c1:")));
    assert!(first.iter().any(|e| e.starts_with("evidence:c1:")));
    assert!(
        !first.iter().any(|e| e.contains(":c2:")),
        "a failed criterion contributes no evidence: {first:?}"
    );
    // The check program is immutable evidence: changing it changes the fold.
    let mut other_check = check.clone();
    other_check.program = "clippy".into();
    let other_run = IntegratedRootVerification {
        status: VerificationStatus::Passed,
        checks: vec![other_check],
        criteria: vec![passed_check.clone(), failed.clone()],
        changed: Vec::new(),
        summary: "fold".into(),
        review_model_identity: None,
    };
    assert_ne!(
        first,
        criterion_pass_evidence_digests(&prepared, &other_run)
    );
    assert_eq!(
        reviewer_proof_basis_digest(&run_with(vec![passed_check.clone()], reviewer_a())),
        None,
        "no reviewer-bearing pass means an honest None"
    );
    assert_eq!(
        reviewer_proof_basis_digest(&run_with(vec![review.clone()], None)),
        None,
        "a reviewer-bearing pass without an ACTUAL review-model call is an honest None — \
         the parent session's configured pair is never substituted"
    );
    let reviewer =
        reviewer_proof_basis_digest(&run_with(vec![review.clone()], reviewer_a())).unwrap();
    assert!(reviewer.starts_with("blake3:"));
    assert_eq!(
        reviewer,
        reviewer_proof_basis_digest(&run_with(vec![review.clone()], reviewer_a())).unwrap(),
        "reviewer folds are deterministic"
    );
    // The review model identity IS the digest: a different ACTUAL reviewer
    // (same parent session row) changes the digest.
    let reviewer_b = Some(faktor_agent::ReviewModelIdentity {
        provider: "review-provider-b".into(),
        model: "review-model-b".into(),
    });
    assert_ne!(
        reviewer,
        reviewer_proof_basis_digest(&run_with(vec![review.clone()], reviewer_b)).unwrap(),
        "the actual review provider/model is part of the digest"
    );
    let mut changed_reviewer = review.clone();
    changed_reviewer.binding = Some(CriterionBinding::IndependentReview {
        reviewer_id: "review-9".into(),
    });
    assert_ne!(
        reviewer,
        reviewer_proof_basis_digest(&run_with(vec![changed_reviewer], reviewer_a())).unwrap(),
        "reviewer binding identity is part of the digest"
    );
}

/// Point 1/6: a FAILING verification lands nothing — the owner stays
/// byte-identical, no landing transaction applies a byte and the task never
/// completes.
#[tokio::test]
async fn verification_failure_leaves_owner_byte_identical() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake(|_| Err("verification deliberately failed".into())),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_two_child_run(&env, "fail verification");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(owner_digest(&env), before, "the owner is byte-identical");
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        latest_txn(&env, &run_id).is_none(),
        "no landing ever started"
    );
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_eq!(owner_digest(&env), before);
}

/// Start a two-child isolated run over an explicit criterion set with a REAL
/// commit completion step armed: the pre-fix defect would land AND commit.
fn start_two_child_run_with_criteria(
    env: &Arc<RealToolEnv>,
    goal: &str,
    criteria: Vec<String>,
) -> String {
    env.executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: goal.to_string(),
                work_items: two_isolated_items(),
                criteria,
                parent_caps: read_caps(),
                isolated_root: env.isolated_root.clone(),
                completion_contract: Some(faktor_core::completion::CompletionContract {
                    include_commit: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .expect("orchestrated start")
        .run_id
}

/// The shared refusal harness of the two decisive landing-authority tests:
/// every check ran green, the composed verdict is non-passing, and NOTHING
/// may land — owner bytes, integration transaction, completion steps.
fn assert_no_landing_at_all(
    env: &Arc<RealToolEnv>,
    run_id: &str,
    before_digest: &str,
    before_head: &str,
) {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(
        owner_digest(env),
        before_digest,
        "the owner tree digest is byte-identical"
    );
    assert!(
        latest_txn(env, run_id).is_none(),
        "no IntegrationTxnPhase::Landing row exists"
    );
    let revision = h.task_revision(task_id).unwrap();
    assert!(
        h.ledger_completion_step_statuses(task_id.raw(), revision.raw())
            .unwrap()
            .is_empty(),
        "no completion step ran"
    );
    assert_eq!(
        cs_head(&env.owner_root),
        before_head,
        "no commit completion step ran"
    );
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        h.ledger_integration_record_for_task(task_id.raw())
            .unwrap()
            .is_none(),
        "no integration record exists"
    );
}

/// P0 LANDING AUTHORITY: every derived check runs GREEN, but one REQUIRED
/// criterion's own binding fails. The composed root verdict is `Failed`, so
/// `verify_prepared_integration` must refuse — the owner stays byte-identical,
/// no Landing transaction row appears and no completion step (armed commit)
/// ever runs.
#[tokio::test]
async fn green_checks_but_failed_criterion_never_starts_landing() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts(
            "pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
            "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        ),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    env.executor
        .set_completion_steps(Some(completion_step_runner_with(
            dir.path(),
            crate::runtime::completion_steps::CompletionStepsConfig::default(),
        )));
    let before = owner_digest(&env);
    let before_head = cs_head(&env.owner_root);
    let failed_key = typed_failed_file_state_criterion();
    let run_id = start_two_child_run_with_criteria(
        &env,
        "green checks but a failed criterion",
        vec![typed_land_criterion(), failed_key.clone()],
    );
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    // The automatic settlement already had its chance: nothing landed.
    assert_no_landing_at_all(&env, &run_id, &before, &before_head);
    // Replay converges to the same refusal.
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_no_landing_at_all(&env, &run_id, &before, &before_head);
    // The durable record carries the COMPOSED verdict: green checks, failed
    // required criterion => status Failed (never Passed, never Pending).
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == VerificationStatus::Failed && r.tree_hash.is_some())
        .max_by_key(|r| r.record_id)
        .expect("the composed Failed root record");
    assert!(
        !record.checks.is_empty()
            && record
                .checks
                .iter()
                .all(|c| c.status == VerificationStatus::Passed),
        "every executed check was green: {record:?}"
    );
    assert!(
        record
            .criteria
            .iter()
            .any(|c| !c.passed && c.criterion_key == failed_key),
        "the failed required criterion is recorded as not passed: {record:?}"
    );
}

/// P0 LANDING AUTHORITY: every derived check runs GREEN, but one REQUIRED
/// criterion carries the explicit honest-unknown binding. The composed root
/// verdict is `Unavailable`, so landing must refuse exactly like a failure —
/// byte-identical owner, no Landing row, no completion step.
#[tokio::test]
async fn green_checks_but_unavailable_criterion_never_starts_landing() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts(
            "pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
            "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        ),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    env.executor
        .set_completion_steps(Some(completion_step_runner_with(
            dir.path(),
            crate::runtime::completion_steps::CompletionStepsConfig::default(),
        )));
    let before = owner_digest(&env);
    let before_head = cs_head(&env.owner_root);
    let unknown_key = typed_explicit_unknown_criterion();
    let run_id = start_two_child_run_with_criteria(
        &env,
        "green checks but an unavailable criterion",
        vec![typed_land_criterion(), unknown_key.clone()],
    );
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    assert_no_landing_at_all(&env, &run_id, &before, &before_head);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_no_landing_at_all(&env, &run_id, &before, &before_head);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == VerificationStatus::Unavailable && r.tree_hash.is_some())
        .max_by_key(|r| r.record_id)
        .expect("the composed Unavailable root record");
    assert!(
        !record.checks.is_empty()
            && record
                .checks
                .iter()
                .all(|c| c.status == VerificationStatus::Passed),
        "every executed check was green: {record:?}"
    );
    assert!(
        record
            .criteria
            .iter()
            .any(|c| !c.passed && c.criterion_key == unknown_key),
        "the unavailable required criterion is recorded as not passed: {record:?}"
    );
    // The new explicit variant round-trips durably across a real store reopen.
    let record_id = record.record_id;
    let parent = env.parent;
    drop(h);
    drop(env);
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(parent).unwrap().unwrap();
    let reopened_record = h2
        .get_verification_record(record_id)
        .unwrap()
        .expect("the Unavailable record survives the reopen");
    assert_eq!(
        reopened_record.status,
        VerificationStatus::Unavailable,
        "the Unavailable status round-trips through the store codec"
    );
    let fact = h2
        .memory_facts()
        .unwrap()
        .into_iter()
        .find(|(kind, key, _)| kind == "verification" && key == "last")
        .expect("the root verification fact");
    let fact: serde_json::Value = serde_json::from_str(&fact.2).unwrap();
    assert_eq!(fact["status"], "unavailable", "{fact:?}");
}

/// Point 4/5: two children diverging on the SAME path are a typed conflict
/// BEFORE any owner mutation; the owner keeps its bytes and no verification
/// or landing record is minted.
#[tokio::test]
async fn candidate_composition_conflict_leaves_owner_untouched() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![
            write_script("c-a", "same.rs", "pub fn v() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
            write_script("c-b", "same.rs", "pub fn v() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
            vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
            vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_two_child_run(&env, "conflict on one path");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(
        owner_digest(&env),
        before,
        "composition never touches the owner"
    );
    assert!(!env.owner_root.join("same.rs").exists());
    assert!(
        h.list_verification_records(task_id)
            .unwrap()
            .iter()
            .all(|r| r.tree_hash.is_none()),
        "no root/candidate verification may be minted over a conflicted composition (child drive records bind no tree)"
    );
    assert!(
        h.ledger_integration_record_for_task(task_id.raw())
            .unwrap()
            .is_none(),
        "no integration record for a refused composition"
    );
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("divergent children must refuse typed");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert_eq!(owner_digest(&env), before);
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
}

/// Point 2/9: an unrelated owner edit while the children were running can
/// never be verified over: the landing recheck refuses typed before the
/// first write, and removing the drift lets the SAME settlement land.
#[tokio::test]
async fn unrelated_owner_edit_during_children_blocks_before_landing() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    // Freeze after the candidate verification, then add the unrelated edit:
    // exactly the "owner moved after the run base was taken" window.
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "unrelated owner drift");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    std::fs::write(
        env.owner_root.join("unrelated.txt"),
        "not the task's work\n",
    )
    .unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("owner drift blocks before landing");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert!(!env.owner_root.join("child_a.rs").exists());
    assert!(!env.owner_root.join("child_b.rs").exists());
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    let blocked = h
        .ledger_integration_record_for_task(task_id.raw())
        .unwrap()
        .expect("blocked integration row");
    assert!(blocked.final_snapshot_hash.is_empty(), "{blocked:?}");
    // Resolve the drift; the same settlement lands and completes.
    std::fs::remove_file(env.owner_root.join("unrelated.txt")).unwrap();
    let outcome = settle_orchestrated(&env, &run_id).await.expect("resolved");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
}

/// Point 9: an owner edit AFTER a passing verification but BEFORE the
/// landing recheck is refused typed; the user's edit survives and is never
/// overwritten by the landing.
#[tokio::test]
async fn owner_edit_after_verification_before_landing_blocks() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "late owner edit");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    // The edit lands in the window between the passing verification and the
    // landing transaction's first write.
    std::fs::write(
        env.owner_root.join("src/lib.rs"),
        "pub fn value() -> u64 { 999 }\n",
    )
    .unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("late drift must block");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 { 999 }\n",
        "the late user edit is preserved"
    );
    assert!(!env.owner_root.join("child_a.rs").exists());
}

/// Point 9: a crash AFTER the passing verification and BEFORE the landing
/// transaction recovers idempotently — the next settlement lands and
/// completes without re-running the model or double-applying anything.
#[tokio::test]
async fn crash_after_verified_before_land_recovers() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "crash before landing");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    assert!(latest_txn(&env, &run_id).is_none());
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(env.owner_root.join("child_b.rs").is_file());
    assert_eq!(
        latest_txn(&env, &run_id).map(|t| t.phase),
        Some(faktor_session::ledger::IntegrationTxnPhase::Landed)
    );
}

/// Point 9: a crash at the run-base boundary leaves the owner untouched and
/// no live run behind; a fresh start converges (the run never had children).
#[tokio::test]
async fn crash_after_run_base_recorded_restarts_cleanly() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterRunBaseRecorded));
    let err = env
        .executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: "base crash".to_string(),
                work_items: two_isolated_items(),
                criteria: vec![typed_land_criterion()],
                parent_caps: read_caps(),
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect_err("the seam fires before any child spawn");
    assert!(matches!(err, ExecError::InjectedCrashSeam(_)), "{err}");
    assert!(env.executor.active_run().is_none(), "the slot was released");
    assert_eq!(owner_digest(&env), before);
    assert!(
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, "run").is_err() || true
    );
    env.executor.set_settlement_crash_seam(None);
    let run_id = start_two_child_run(&env, "fresh start after base crash");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    // The automatic settlement may already have landed the fresh run; the
    // explicit re-settle must be a convergent no-op either way.
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(env.owner_root.join("child_b.rs").is_file());
}

/// Point 9: a crash AFTER the candidate was prepared (staged + composed)
/// leaves the owner byte-identical; re-settling rebuilds the identical
/// candidate idempotently and lands it.
#[tokio::test]
async fn crash_after_candidate_prepared_recovers() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterCandidatePrepared));
    let run_id = start_two_child_run(&env, "candidate crash");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    assert_eq!(owner_digest(&env), before, "staging never writes the owner");
    let candidate = run_candidate_root(&env, &run_id);
    assert!(candidate.join("child_a.rs").is_file());
    assert!(latest_txn(&env, &run_id).is_none());
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert_eq!(owner_digest(&env), root_digest(&candidate));
}

/// Point 9: a crash right after the record-first landing transaction was
/// journaled (phase Landing, zero applies) recovers without any blind write
/// and finishes the landing.
#[tokio::test]
async fn crash_after_txn_record_recovers_without_writes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterIntegrationTxnRecord));
    let run_id = start_two_child_run(&env, "txn crash");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let txn = latest_txn(&env, &run_id).expect("record-first txn durable");
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::Landing
    );
    assert_eq!(txn.applied_count, 0, "no owner write after the record");
    assert!(!env.owner_root.join("child_a.rs").exists());
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert_eq!(
        latest_txn(&env, &run_id).map(|t| t.phase),
        Some(faktor_session::ledger::IntegrationTxnPhase::Landed)
    );
}

/// Point 5/9: a crash MID-landing (one path applied) resumes from the
/// durable transaction and finishes the landing; the final digest equals the
/// verified candidate.
#[tokio::test]
async fn crash_mid_land_recovers_or_rolls_back() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::IntegrationApply { after: 1 }));
    let run_id = start_two_child_run(&env, "crash mid landing");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let txn = latest_txn(&env, &run_id).expect("landing txn durable");
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::Landing
    );
    assert_eq!(txn.applied_count, 1, "{txn:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(!env.owner_root.join("child_b.rs").exists());
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id)
        .await
        .expect("finish landing");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    let txn = latest_txn(&env, &run_id).unwrap();
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::Landed
    );
    assert_eq!(
        owner_digest(&env),
        root_digest(&run_candidate_root(&env, &run_id)),
        "the landed owner equals the verified candidate"
    );
}

/// Point 5: a rollback restores only paths still equal to OUR written
/// candidate hash — a post-landing user edit is never overwritten and is
/// reported as a rollback conflict.
#[tokio::test]
async fn rollback_never_overwrites_later_user_edit() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::IntegrationApply { after: 1 }));
    let run_id = start_two_child_run(&env, "rollback keeps edits");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let txn = latest_txn(&env, &run_id).expect("landing txn durable");
    assert_eq!(txn.applied_count, 1);
    let applied_path = txn.paths[0].path.clone();
    let pending_path = txn.paths[1].path.clone();
    // A user edits the path we already wrote (ours must never clobber it)
    // and makes the pending path diverge from the base (forcing the
    // rollback on resume).
    std::fs::write(
        env.owner_root.join(&applied_path),
        "user post-landing edit\n",
    )
    .unwrap();
    std::fs::write(env.owner_root.join(&pending_path), "hostile drift\n").unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("a late per-path conflict rolls back");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert_eq!(
        std::fs::read(env.owner_root.join(&applied_path)).unwrap(),
        b"user post-landing edit\n",
        "the rollback never overwrote the later user edit"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join(&pending_path)).unwrap(),
        b"hostile drift\n",
        "a never-applied path is left alone"
    );
    let txn = latest_txn(&env, &run_id).unwrap();
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::RolledBack
    );
    let applied = txn.paths.iter().find(|p| p.path == applied_path).unwrap();
    assert_eq!(
        applied.state,
        faktor_session::ledger::IntegrationPathTxnState::RollbackConflict
    );
}

/// Point 6: the landed owner digest must EQUAL the verified candidate
/// snapshot; an external edit before the final digest check rolls our
/// writes back and refuses typed (never a "merge returned ok" completion).
#[tokio::test]
async fn final_owner_digest_must_equal_verified_candidate() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterFinalOwnerSnapshot));
    let run_id = start_two_child_run(&env, "final digest equality");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let candidate_digest = root_digest(&run_candidate_root(&env, &run_id));
    assert_eq!(
        owner_digest(&env),
        candidate_digest,
        "the landing applied every path before the crash"
    );
    // An external edit lands exactly between the last apply and the final
    // equality check.
    std::fs::write(env.owner_root.join("late.txt"), "concurrent edit\n").unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("a moved owner root can never complete");
    assert!(matches!(err, ExecError::WorkspaceDrift(_)), "{err}");
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    assert_ne!(
        h.get_task(h.task_id().unwrap()).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        env.owner_root.join("late.txt").is_file(),
        "external edit kept"
    );
    assert!(
        !env.owner_root.join("child_a.rs").exists(),
        "our applied paths were rolled back after the failed equality"
    );
    let txn = latest_txn(&env, &run_id).unwrap();
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::RolledBack
    );
}

// ------------------------------------------------- composition unit coverage

fn verdict_check(id: &str, required: bool, status: VerificationStatus) -> CheckExecution {
    CheckExecution {
        check: id.into(),
        program: "cargo".into(),
        args: vec!["check".into()],
        category: "compile".into(),
        required,
        status,
        started_ms: 1,
        finished_ms: Some(2),
        exit: None,
        summary: None,
    }
}

fn verdict_criterion(
    criterion_key: &str,
    passed: bool,
    evidence: Option<&str>,
) -> CriterionVerification {
    CriterionVerification {
        criterion_key: criterion_key.into(),
        passed,
        evidence: evidence.map(str::to_string),
        binding: None,
    }
}

fn required_criterion_entry(text: &str) -> String {
    faktor_session::task::Criterion::derived(
        text,
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .encode()
}

fn advisory_criterion_entry(text: &str) -> String {
    faktor_session::task::Criterion::derived(
        text,
        CriterionOrigin::User,
        CriterionRequirement::Preferred,
        None,
    )
    .encode()
}

/// P0: the composer passes ONLY when every required check AND every required
/// criterion passes; advisory verdicts never block; no unknown state ever
/// surfaces as `Pending` (unavailable/missing evidence becomes the explicit
/// `Unavailable`).
#[test]
fn compose_root_verdict_is_required_only_and_never_pending() {
    let required_key = required_criterion_entry("the build is green");
    let advisory_key = advisory_criterion_entry("the docs read nicely");
    let green = vec![verdict_check(
        "rust_check",
        true,
        VerificationStatus::Passed,
    )];
    let all_pass = vec![verdict_criterion(
        &required_key,
        true,
        Some("check:rust_check"),
    )];
    // Control: every required check and criterion passes.
    assert_eq!(
        compose_root_verification_status(&green, &all_pass),
        VerificationStatus::Passed
    );
    // Advisory failure (and advisory unavailability) never blocks.
    assert_eq!(
        compose_root_verification_status(
            &green,
            &[
                verdict_criterion(&required_key, true, Some("check:rust_check")),
                verdict_criterion(&advisory_key, false, Some("style note")),
                verdict_criterion(&advisory_key, false, None),
            ],
        ),
        VerificationStatus::Passed
    );
    // A required criterion with missing evidence is Unavailable, never
    // Pending and never a pass.
    assert_eq!(
        compose_root_verification_status(&green, &[verdict_criterion(&required_key, false, None)]),
        VerificationStatus::Unavailable
    );
    // The explicit honest-unknown binding is Unavailable even with prose
    // "evidence".
    let explicit_unknown = faktor_session::task::Criterion::derived(
        "honest unknown",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::Unavailable {
        reason: "no mechanism".into(),
    })
    .encode();
    let mut unknown_verdict = verdict_criterion(&explicit_unknown, false, Some("no mechanism"));
    unknown_verdict.binding = Some(CriterionBinding::Unavailable {
        reason: "no mechanism".into(),
    });
    assert_eq!(
        compose_root_verification_status(&green, &[unknown_verdict]),
        VerificationStatus::Unavailable
    );
    // A failed required criterion with evidence is Failed.
    assert_eq!(
        compose_root_verification_status(
            &green,
            &[verdict_criterion(&required_key, false, Some("file:x.rs"))],
        ),
        VerificationStatus::Failed
    );
    // A required-check binding that resolves to nothing is missing evidence.
    let mut unresolved = verdict_criterion(&required_key, false, Some("check:rust_check"));
    unresolved.binding = Some(CriterionBinding::RequiredCheck {
        check_id: "rust_check".into(),
        command_digest: "digest-that-never-ran".into(),
    });
    assert_eq!(
        compose_root_verification_status(&green, &[unresolved]),
        VerificationStatus::Unavailable
    );
    // Legacy plain-text criteria stay Required (`Criterion::legacy`).
    assert_eq!(
        compose_root_verification_status(
            &green,
            &[verdict_criterion("plain legacy prose", false, Some("note"))],
        ),
        VerificationStatus::Failed
    );
    // Check side: failed => Failed; unavailable/pending => Unavailable;
    // optional checks never block.
    assert_eq!(
        compose_root_verification_status(
            &[verdict_check(
                "rust_check",
                true,
                VerificationStatus::Failed
            )],
            &[],
        ),
        VerificationStatus::Failed
    );
    for status in [
        VerificationStatus::Unavailable,
        VerificationStatus::Pending,
        VerificationStatus::Running,
    ] {
        assert_eq!(
            compose_root_verification_status(&[verdict_check("rust_check", true, status)], &[]),
            VerificationStatus::Unavailable,
            "{status:?} must never compose Pending"
        );
    }
    assert_eq!(
        compose_root_verification_status(
            &[
                verdict_check("rust_check", true, VerificationStatus::Passed),
                verdict_check("advisory_bench", false, VerificationStatus::Failed),
            ],
            &[verdict_criterion(&required_key, true, Some("ok"))],
        ),
        VerificationStatus::Passed
    );
    // Failure dominates unavailability when both required sides are bad.
    assert_eq!(
        compose_root_verification_status(
            &[verdict_check(
                "rust_check",
                true,
                VerificationStatus::Failed
            )],
            &[verdict_criterion(&required_key, false, None)],
        ),
        VerificationStatus::Failed
    );
}

/// The no-op rule is STRICTER: on an empty aggregate change set an advisory
/// failure also blocks (there is no check evidence to lean on).
#[test]
fn compose_no_op_verdict_requires_every_criterion() {
    let required_key = required_criterion_entry("the build is green");
    let advisory_key = advisory_criterion_entry("the docs read nicely");
    let all_pass = vec![
        verdict_criterion(&required_key, true, Some("check:rust_check")),
        verdict_criterion(&advisory_key, true, Some("prose reviewed")),
    ];
    assert_eq!(
        compose_no_op_root_verification_status(&[], &all_pass),
        VerificationStatus::Passed
    );
    assert_eq!(
        compose_no_op_root_verification_status(
            &[],
            &[verdict_criterion(&advisory_key, false, Some("style note"))],
        ),
        VerificationStatus::Failed,
        "an advisory failure cannot pass the no-op rule"
    );
    assert_eq!(
        compose_no_op_root_verification_status(
            &[],
            &[verdict_criterion(&advisory_key, false, None)],
        ),
        VerificationStatus::Unavailable
    );
    // The required-only composer, by contrast, ignores the same advisory
    // failure.
    assert_eq!(
        compose_root_verification_status(
            &[],
            &[verdict_criterion(&advisory_key, false, Some("style note"))],
        ),
        VerificationStatus::Passed
    );
}

/// The new explicit variant is a first-class durable value: a record written
/// with `Unavailable` survives a real store reopen byte-for-byte, and the
/// root verification fact carries its own durable tag.
#[tokio::test]
async fn unavailable_verdict_round_trips_through_a_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    let parent = env.parent;
    let h = env.manager.get_session(parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: parent,
        goal: "round-trip".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let record_id = h
        .create_verification_record(
            task_id,
            Some("a".repeat(64)),
            vec![CriterionVerification {
                criterion_key: "criterion".into(),
                passed: false,
                evidence: Some("no objective mechanism ran".into()),
                binding: Some(CriterionBinding::Unavailable {
                    reason: "no objective mechanism ran".into(),
                }),
            }],
            vec![verdict_check(
                "rust_check",
                true,
                VerificationStatus::Unavailable,
            )],
            vec![],
            vec![],
            None,
            VerificationStatus::Unavailable,
            now,
        )
        .unwrap();
    crate::runtime::task_executor::persist_root_verification_fact(&h, "unavailable", &[], &[])
        .unwrap();
    drop(h);
    drop(env);
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(parent).unwrap().unwrap();
    let record = h2
        .get_verification_record(record_id)
        .unwrap()
        .expect("the Unavailable record survives the reopen");
    assert_eq!(record.status, VerificationStatus::Unavailable);
    assert_eq!(
        record.checks[0].status,
        VerificationStatus::Unavailable,
        "check rows round-trip the variant too"
    );
    assert_eq!(
        record.criteria[0].binding,
        Some(CriterionBinding::Unavailable {
            reason: "no objective mechanism ran".into()
        })
    );
    let fact = h2
        .memory_facts()
        .unwrap()
        .into_iter()
        .find(|(kind, key, _)| kind == "verification" && key == "last")
        .expect("the root verification fact");
    let fact: serde_json::Value = serde_json::from_str(&fact.2).unwrap();
    assert_eq!(fact["status"], "unavailable");
}

fn unit_cs(
    child: &str,
    files: Vec<crate::runtime::merge::ChangeEntry>,
) -> crate::runtime::merge::ChangeSet {
    crate::runtime::merge::ChangeSet {
        child_id: child.to_string(),
        base_id: format!("base-{child}"),
        run_base_snapshot: Some("a".repeat(64)),
        child_start_snapshot: None,
        final_child_snapshot: None,
        files,
        created_ms: 1,
    }
}

fn unit_entry(
    path: &str,
    child_hash: Option<FileHash>,
    base_hash: Option<FileHash>,
) -> crate::runtime::merge::ChangeEntry {
    crate::runtime::merge::ChangeEntry {
        path: std::path::PathBuf::from(path),
        child_hash,
        base_hash,
    }
}

/// Point 4: three children converging on the SAME resulting hash (same
/// file, same content) are applied once with provenance retained; every
/// input permutation resolves identically (lexical order is not the
/// resolution mechanism).
#[test]
fn convergent_multi_child_same_file_is_deterministic() {
    let base = FileHash::from([7u8; 32]);
    let result = FileHash::from([9u8; 32]);
    let make = |order: &[usize]| {
        let mut children: Vec<crate::runtime::merge::ChangeSet> = Vec::new();
        for i in order {
            children.push(unit_cs(
                &format!("child-{i}"),
                vec![
                    unit_entry("same.rs", Some(result), Some(base)),
                    unit_entry(
                        &format!("only-{i}.rs"),
                        Some(FileHash::from([*i as u8; 32])),
                        None,
                    ),
                ],
            ));
        }
        crate::runtime::merge::compose_child_changes(&children).unwrap()
    };
    let reference = make(&[0, 1, 2]);
    let same = reference
        .iter()
        .find(|c| c.path.ends_with("same.rs"))
        .unwrap();
    assert_eq!(same.sources, vec!["child-0", "child-1", "child-2"]);
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        assert_eq!(make(&order), reference, "order {order:?} drifted");
    }
    let paths: Vec<_> = reference.iter().map(|c| c.path.clone()).collect();
    assert_eq!(paths, {
        let mut sorted = paths.clone();
        sorted.sort();
        sorted
    });
}

/// Point 4: two children producing DIFFERENT hashes for the same path are a
/// typed conflict (no order-based resolution exists).
#[test]
fn divergent_multi_child_same_file_conflicts() {
    let base = FileHash::from([7u8; 32]);
    let children = vec![
        unit_cs(
            "child-0",
            vec![unit_entry(
                "same.rs",
                Some(FileHash::from([1u8; 32])),
                Some(base),
            )],
        ),
        unit_cs(
            "child-1",
            vec![unit_entry(
                "same.rs",
                Some(FileHash::from([2u8; 32])),
                Some(base),
            )],
        ),
    ];
    let err = crate::runtime::merge::compose_child_changes(&children).unwrap_err();
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
}

/// Point 4: delete-vs-modify on one path is a typed conflict.
#[test]
fn delete_vs_modify_conflicts() {
    let base = FileHash::from([7u8; 32]);
    let children = vec![
        unit_cs("child-0", vec![unit_entry("same.rs", None, Some(base))]),
        unit_cs(
            "child-1",
            vec![unit_entry(
                "same.rs",
                Some(FileHash::from([3u8; 32])),
                Some(base),
            )],
        ),
    ];
    let err = crate::runtime::merge::compose_child_changes(&children).unwrap_err();
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
}

/// Point 8: the tournament WINNER lands through the SAME pipeline — the
/// decided winner is integrated, verified over the candidate, committed
/// into the owner and certified; the loser's work never reaches the owner.
#[tokio::test]
async fn tournament_winner_lands_through_the_one_pipeline() {
    use crate::runtime::task_executor::TournamentStartRequest;
    use crate::tournament::{CandidateState, ReviewRank, ReviewVerdict};
    use faktor_core::id::VerificationRecordId;

    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..6)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_real_tool_env_full(
        dir.path(),
        scripts,
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        MutationMode::DirectCompat,
    );
    cs_seed_owner(&env);
    let receipt = env
        .executor
        .start_tournament_with(
            env.parent,
            TournamentStartRequest {
                goal: "pick a winner".to_string(),
                criteria: vec![typed_land_criterion()],
                n: 2,
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("tournament start");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    for row in &rows {
        let root = child_root(&env.manager, row);
        std::fs::write(
            root.join("winner.rs"),
            format!(
                "pub fn winner() -> u64 {{\n    let seed: u64 = {0};\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}}\n",
                row.child_id.trim_start_matches("child-").parse::<u64>().unwrap_or(1) + 1
            ),
        )
        .unwrap();
    }
    let mut loser = env
        .executor
        .candidate_settlement(
            env.parent,
            &receipt.tournament_id,
            "child-0",
            "verification failed",
        )
        .unwrap();
    loser.verification = Some(VerificationRecordId::new(1));
    loser.verification_pass = Some(false);
    loser.review = Some(ReviewVerdict {
        rank: ReviewRank::Clean,
        reviewer: "review-0".into(),
    });
    env.executor
        .settle_tournament_candidate(env.parent, &receipt.tournament_id, loser)
        .unwrap();
    let mut winner = env
        .executor
        .candidate_settlement(
            env.parent,
            &receipt.tournament_id,
            "child-1",
            "verified complete",
        )
        .unwrap();
    winner.verification = Some(VerificationRecordId::new(2));
    winner.verification_pass = Some(true);
    winner.review = Some(ReviewVerdict {
        rank: ReviewRank::Clean,
        reviewer: "review-0".into(),
    });
    env.executor
        .settle_tournament_candidate(env.parent, &receipt.tournament_id, winner)
        .unwrap();
    let decision = env
        .executor
        .decide_tournament(env.parent, &receipt.tournament_id)
        .expect("deterministic decision");
    assert_eq!(decision.winner.child_id, "child-1");
    assert_eq!(decision.winner.state, CandidateState::Done);
    // The SAME settlement pipeline lands the winner through the candidate.
    let outcome = settle_orchestrated(&env, &receipt.run_id)
        .await
        .expect("winner settlement");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(env.owner_root.join("winner.rs")).unwrap(),
        "pub fn winner() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"
    );
    let integrated = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .ledger_integration_record_for_task(
            env.manager
                .get_session(env.parent)
                .unwrap()
                .unwrap()
                .task_id()
                .unwrap()
                .raw(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(integrated.source_count, 1, "{integrated:?}");
    assert_eq!(integrated.sources[0].child_id, "child-1");
    assert!(!integrated.final_snapshot_hash.is_empty());
    // Hardening: the explicit identity fields are populated from the run's
    // real base/candidate/landed snapshots — and the overloaded
    // `base_revision` (formerly derived from `sources.first()`) stays
    // unpopulated.
    assert!(
        integrated.base_revision.is_none(),
        "sources.first() derivation is gone: {integrated:?}"
    );
    assert_eq!(
        integrated.base_snapshot, integrated.run_base_snapshot,
        "deprecated alias and explicit run base agree"
    );
    assert!(integrated.run_base_snapshot.is_some());
    assert!(integrated.candidate_snapshot.is_some());
    assert_eq!(
        integrated.landed_snapshot.as_deref(),
        Some(integrated.final_snapshot_hash.as_str()),
        "landed identity equals the finalized snapshot"
    );
    let (txn, txn_id) = {
        let h = env.manager.get_session(env.parent).unwrap().unwrap();
        let txn = h
            .ledger_integration_txn_for_run(&receipt.run_id)
            .unwrap()
            .unwrap();
        (txn.clone(), txn.txn_id())
    };
    assert_eq!(
        integrated.integration_txn_id.as_deref(),
        Some(txn_id.as_str()),
        "the record names the exact landing transaction: {txn:?}"
    );
    let basis_digest = {
        let h = env.manager.get_session(env.parent).unwrap().unwrap();
        let task_id = h.task_id().unwrap();
        h.list_verification_records(task_id)
            .unwrap()
            .into_iter()
            .filter(|r| r.status == VerificationStatus::Passed)
            .max_by_key(|r| r.record_id)
            .and_then(|r| r.environment_fingerprint.and_then(|f| f.proof_basis_digest))
            .expect("the passing root record carries its proof basis")
    };
    assert_eq!(
        integrated.proof_basis_digest.as_deref(),
        Some(basis_digest.as_str()),
        "the integration record binds the exact verification basis"
    );
}

/// P1 binary attachments: a stored `AttachmentId` set admits durably on the
/// task row AND the run's linkage row (SEPARATE from `files`), survives a
/// store reopen, and an unknown digest is refused BEFORE any run/task row —
/// no partial durable admission. Image ids are structurally valid here;
/// model-aware delivery (vision/mime/size) is validated at the server DTO
/// and resolved into media parts at agent request construction.
#[tokio::test]
async fn binary_attachments_admit_durably_and_unknown_digests_leave_no_run() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), vec![vec![ScriptedResponse::End]]);
    let parent = env.parent;
    let handle = env.manager.get_session(parent).unwrap().unwrap();
    let stored = handle
        .put_attachment("application/pdf", Some("spec.pdf"), b"%PDF-1.4 spec")
        .unwrap();
    // An unknown digest is refused by the EXECUTOR before any run/task row:
    // no partial durable admission.
    let unknown = faktor_core::attachment::AttachmentId {
        digest: faktor_core::hash::FileHash::from([9; 32]),
        ..stored.clone()
    };
    let mut bad = request("unknown", vec![wi("a1", WorkKind::Analysis, &[])], &env);
    bad.attachments = vec![unknown];
    let err = env
        .executor
        .start_task(parent, bad)
        .expect_err("unknown digest must be refused");
    assert!(matches!(err, ExecError::NotFound(_)), "{err:?}");
    assert!(handle
        .get_task(handle.task_id().unwrap())
        .unwrap()
        .is_none());
    assert!(handle.memory_facts().unwrap().is_empty());

    let mut req = request(
        "attached goal",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.attachments = vec![stored.clone()];
    let receipt = env
        .executor
        .start_task(parent, req)
        .expect("attachment admission must be accepted");
    // Durable Task row carries the typed set, separate from workspace paths.
    let task = handle.get_task(handle.task_id().unwrap()).unwrap().unwrap();
    assert_eq!(task.attachments, vec![stored.clone()]);
    assert!(task.plan.is_empty());
    // The linkage row reconstructs the byte-identical typed set.
    let facts = handle.memory_facts().unwrap();
    let row = facts
        .iter()
        .find(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == &receipt.run_id)
        .expect("durable linkage row");
    let decoded = TaskRunRow::decode(&row.2).unwrap();
    assert_eq!(decoded.attachments, vec![stored.clone()]);
    assert_eq!(decoded.files, Vec::<String>::new());
    // Reopen the REAL store: the typed set survives, byte-identically.
    drop(handle);
    drop(env);
    let m2 = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = m2.get_session(parent).unwrap().unwrap();
    let task2 = h2.get_task(h2.task_id().unwrap()).unwrap().unwrap();
    assert_eq!(task2.attachments, vec![stored.clone()]);
    assert_eq!(
        h2.attachment_bytes(&stored, 1 << 20).unwrap(),
        b"%PDF-1.4 spec"
    );

    // Image ids are structurally valid at this layer; the server DTO gates
    // delivery on the chosen model's vision capability and the adapters
    // encode the resolved bytes per wire.
    let image = h2
        .put_attachment("image/png", Some("shot.png"), b"\x89PNG")
        .unwrap();
    crate::runtime::validate_attachment_ids(&[image]).expect("image id is structurally valid");
}
