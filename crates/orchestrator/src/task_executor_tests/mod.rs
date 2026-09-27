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

pub(crate) use std::sync::atomic::{AtomicUsize, Ordering};
pub(crate) use std::sync::{Arc, Mutex as StdMutex};
pub(crate) use std::time::Duration;

pub(crate) use faktor_agent::tool::RecoveryHint as ToolRecovery;
pub(crate) use faktor_agent::{
    AgentDeps, AgentRuntime, NoEvidence, PermissionRequester, Tool, ToolCallMode, ToolOutcome,
    ToolRegistry, ToolRunCtx,
};
pub(crate) use faktor_core::capability::PermissionDecision;
pub(crate) use faktor_core::error::Error;
pub(crate) use faktor_core::hash::FileHash;
pub(crate) use faktor_core::id::WorkspaceId;
pub(crate) use faktor_core::id::{SessionId, TaskId, WorktreeId};
pub(crate) use faktor_core::model::ModelCapabilities;
pub(crate) use faktor_core::resource::ResourceClass;
pub(crate) use faktor_core::time::SystemClock;
pub(crate) use faktor_provider::{
    FakeProvider, GenericAgentRequest, Provider, ProviderChunk, ProviderError, ProviderRegistry,
    ProviderStream, ScriptedResponse,
};
pub(crate) use faktor_session::{BudgetAuthority, SessionManager};

pub(crate) use crate::caps::{CapabilityGrant, CapabilitySet, LatticeCap, ScopePattern};
pub(crate) use crate::runtime::completion_steps::commit_message;
pub(crate) use crate::runtime::shadow::{ShadowCopyLimits, ShadowRoots};
pub(crate) use crate::runtime::task_executor::{
    compose_no_op_root_verification_status, compose_root_verification_status, MutationMode,
    PreparedRunIntegration, RunSettlement, SettlementOutcome, ShadowFinalizeAction, TaskExecutor,
    TaskRunMode, TaskRunRequest, TaskRunRow, TASK_RUN_ROW_KIND,
};
pub(crate) use crate::runtime::{CrashSeam, ExecError, OrchestratorRuntime};
pub(crate) use crate::test_support::heavy_guard;
pub(crate) use crate::{OwnershipSpec, TaskPlan, WorkItem, WorkKind};
pub(crate) use faktor_agent::IntegratedRootVerification;
pub(crate) use faktor_core::state::{
    CheckExecution, CriterionBinding, CriterionOrigin, CriterionRequirement, CriterionVerification,
    NoOpDisposition, TaskState, TaskTransition, VerificationStatus,
};
pub(crate) use faktor_session::child::ChildOwnership;
pub(crate) use faktor_session::ShadowRowState;

// ------------------------------------------------------------------ fixture

pub(crate) struct AlwaysAllow;
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
pub(crate) struct PerCallProvider {
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
pub(crate) struct GatedProvider {
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
pub(crate) struct BarrierProvider {
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
pub(crate) struct TickerProvider {
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
pub(crate) struct Env {
    manager: Arc<SessionManager>,
    agent: Arc<AgentRuntime>,
    provider: Arc<PerCallProvider>,
    orchestrator: Arc<OrchestratorRuntime>,
    executor: Arc<TaskExecutor>,
    parent: SessionId,
    owner_root: std::path::PathBuf,
    isolated_root: std::path::PathBuf,
}

pub(crate) fn read_caps() -> CapabilitySet {
    CapabilitySet::from_grants(vec![CapabilityGrant::new(
        LatticeCap::ReadWorkspace,
        ScopePattern::new("*").unwrap(),
    )])
    .unwrap()
}

pub(crate) fn open_env(root: &std::path::Path, scripts: Vec<Vec<ScriptedResponse>>) -> Arc<Env> {
    open_env_with_shadows(root, scripts, ShadowCopyLimits::default(), false)
}

/// A shadowed executor env: same wiring as [`open_env`] plus the P0-48
/// shadow service rooted at `<root>/shadows`.
pub(crate) fn open_shadow_env(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
) -> Arc<Env> {
    open_env_with_shadows(root, scripts, ShadowCopyLimits::default(), true)
}

pub(crate) fn open_env_with_shadows(
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
    let executor = if shadowed {
        let shadows =
            ShadowRoots::new_with_limits(manager.clone(), shadows_root.clone(), limits).unwrap();
        TaskExecutor::new(&orchestrator, manager.clone(), agent.clone(), shadows)
    } else {
        // Low-level owner-direct suite: the cfg(test) seam (no shadow
        // service). Production code cannot construct this.
        TaskExecutor::new_owner_direct_for_test_harness(
            &orchestrator,
            manager.clone(),
            agent.clone(),
        )
    };
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

pub(crate) fn build_agent(
    manager: Arc<SessionManager>,
    registry: ProviderRegistry,
) -> Arc<AgentRuntime> {
    build_agent_with_verification(
        manager,
        registry,
        faktor_agent::VerificationService::disabled(),
    )
}

pub(crate) fn build_agent_with_verification(
    manager: Arc<SessionManager>,
    registry: ProviderRegistry,
    verification: Arc<faktor_agent::VerificationService>,
) -> Arc<AgentRuntime> {
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
        verification,
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

pub(crate) fn wi(id: &str, kind: WorkKind, deps: &[&str]) -> WorkItem {
    let mut item = WorkItem::new(id, format!("work {id}"), kind);
    item.depends_on = deps.iter().map(|d| d.to_string()).collect();
    item
}

pub(crate) fn path_item(id: &str, kind: WorkKind, deps: &[&str], paths: &[&str]) -> WorkItem {
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

pub(crate) fn request(goal: &str, items: Vec<WorkItem>, env: &Env) -> TaskRunRequest {
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

pub(crate) fn state_of(env: &Env, sid: SessionId) -> faktor_core::state::AgentState {
    env.manager
        .get_session(sid)
        .unwrap()
        .unwrap()
        .state()
        .unwrap()
}

pub(crate) fn done_script() -> Vec<Vec<ScriptedResponse>> {
    vec![
        vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
        vec![ScriptedResponse::End],
    ]
}

// ------------------------------------------------------------------- tests
#[cfg(test)]
mod tests_control;
#[cfg(test)]
mod tests_core;
#[cfg(test)]
mod tests_verification;
