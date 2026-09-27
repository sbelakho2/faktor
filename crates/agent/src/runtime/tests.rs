//! `runtime::tests`: shared test fixtures (part 1).

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::*;

pub(crate) use crate::tool::Tool;

pub(crate) use crate::{empty_passthrough_decision, RoutingPolicy};

pub(crate) use faktor_core::id::SessionId;

pub(crate) use faktor_core::model::ModelCapabilities;

pub(crate) use faktor_core::time::SystemClock;

pub(crate) use faktor_instructions::{InstructionResolver, WorkspaceRootProvider};

pub(crate) use faktor_memory::MemoryWriter as _;

pub(crate) use faktor_provider::{ContentKind, FakeProvider, ReportedCurrency, ScriptedResponse};

pub(crate) use faktor_router::OutcomeStore as _;

pub(crate) use faktor_session::budget::BudgetCapEvidence;

pub(crate) use faktor_session::BudgetAuthority;

pub(crate) use tempfile::tempdir;

/// Wave-16 ergonomics kept by the typed-verifier migration (P0-9/10): a
/// scripted [`crate::VerificationService`] over a legacy-style
/// command-string closure. Deterministic verdicts and asserted command
/// vectors behave byte-identically to the old `Verifier::new` injection.
pub(crate) fn fake(
    run: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static,
) -> Arc<crate::VerificationService> {
    crate::VerificationService::fake(run)
}

pub(crate) fn fake_ok() -> Arc<crate::VerificationService> {
    crate::VerificationService::fake_ok()
}

/// Test adapter: resolves roots through the REAL SessionManager
/// workspace table (the durable root the manager holds — never the
/// process CWD). Implements the instructions crate's trait over a local
/// type so no dependency cycle is created.
pub(crate) struct TestSessionRoots(pub(crate) Arc<SessionManager>);

impl WorkspaceRootProvider for TestSessionRoots {
    fn workspace_root(&self, workspace_id: u64) -> Option<std::path::PathBuf> {
        if workspace_id == 0 {
            return None;
        }
        let ws = faktor_core::id::WorkspaceId::new(workspace_id);
        self.0.workspace_root(ws).ok().flatten()
    }
}

/// A resolver over the given session manager (workspace rows are the
/// only roots it can see; unknown ids resolve to Empty).
pub(crate) fn test_resolver(session: &Arc<SessionManager>) -> Arc<InstructionResolver> {
    Arc::new(InstructionResolver::new(
        Arc::new(TestSessionRoots(session.clone())),
        32,
    ))
}

pub(crate) fn deps_with(
    provider: Arc<dyn faktor_provider::Provider>,
    tools: Vec<Tool>,
) -> (AgentDeps, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider).unwrap();
    let mut tool_registry = ToolRegistry::new();
    for t in tools {
        tool_registry.register(t);
    }
    let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let deps = AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tool_registry),
        cas: Some(Arc::new(faktor_cas::Cas::open(root.join("cas")).unwrap())),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: crate::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: test_resolver(&session),
        routing: crate::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test agent.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: crate::fallback_semantic_registry(),
        context_prior: None,
        efficiency: Default::default(),
    };
    (deps, dir)
}

pub(crate) fn deps(provider: FakeProvider, tools: Vec<Tool>) -> (AgentDeps, tempfile::TempDir) {
    deps_with(Arc::new(provider), tools)
}

/// Like [`deps_with`] but on a SHARED session manager (multi-turn tests:
/// each logical turn gets its own provider script while the durable
/// session — ledger, loop signals, queue — stays in one store).
pub(crate) fn deps_sharing_session(
    session: Arc<SessionManager>,
    provider: Arc<dyn faktor_provider::Provider>,
    tools: Vec<Tool>,
) -> (AgentDeps, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider).unwrap();
    let mut tool_registry = ToolRegistry::new();
    for t in tools {
        tool_registry.register(t);
    }
    (
        AgentDeps {
            session: session.clone(),
            providers: Arc::new(registry),
            chunk_sink: None,
            permission_requester: Arc::new(AlwaysAllow),
            evidence: Arc::new(NoEvidence),
            tools: Arc::new(tool_registry),
            cas: Some(Arc::new(
                faktor_cas::Cas::open(dir.path().join("cas")).unwrap(),
            )),
            workspaces: faktor_fs::WorkspaceFileService::new(),
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            verification: crate::VerificationService::disabled(),
            hooks: None,
            instructions_resolver: test_resolver(&session),
            routing: crate::FixedRoutingPolicy::passthrough(),
            budgets: Arc::new(faktor_session::NoopBudget),
            model: "m".into(),
            compaction_model: None,
            compact_at_usage: 0.65,
            instructions: "You are a test agent.".into(),
            clock: Arc::new(SystemClock),
            tool_call_mode: ToolCallMode::Native,
            tool_deadline_ms: 2000,
            retry_policy: faktor_core::retry::RetryPolicy::default(),
            semantic: crate::fallback_semantic_registry(),
            context_prior: None,
            efficiency: Default::default(),
        },
        dir,
    )
}

/// One session + its manager for multi-runtime tests.
pub(crate) fn shared_session(deps: &AgentDeps) -> (Arc<SessionManager>, SessionId) {
    let ws = deps.session.create_workspace("/w").unwrap();
    let sid = deps
        .session
        .create_session(ws, "t", "fake", "m")
        .unwrap()
        .id();
    (deps.session.clone(), sid)
}

/// Provider wrapper that intercepts every request before delegation:
/// the hook inspects the incoming `GenericAgentRequest` and may refuse
/// the stream with a `Malformed` provider error (the turn then fails —
/// this is how the tool-result semantic test proves the request shape).
pub(crate) type RequestHook =
    dyn Fn(usize, &GenericAgentRequest) -> Result<(), String> + Send + Sync;

pub(crate) struct InspectingProvider {
    pub(crate) inner: Arc<dyn faktor_provider::Provider>,
    pub(crate) counter: std::sync::atomic::AtomicUsize,
    pub(crate) hook: Arc<RequestHook>,
}

impl InspectingProvider {
    pub(crate) fn new(
        inner: Arc<dyn faktor_provider::Provider>,
        hook: impl Fn(usize, &GenericAgentRequest) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            counter: std::sync::atomic::AtomicUsize::new(0),
            hook: Arc::new(hook),
        }
    }
}

impl faktor_provider::Provider for InspectingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }

    fn document_capable(&self, model: &str) -> bool {
        self.inner.document_capable(model)
    }

    fn max_document_bytes(&self) -> usize {
        self.inner.max_document_bytes()
    }

    fn supports_embeddings(&self, model: &str) -> bool {
        self.inner.supports_embeddings(model)
    }

    fn embed(
        &self,
        req: faktor_provider::EmbeddingRequest,
    ) -> Result<faktor_provider::EmbeddingResponse, faktor_provider::ProviderError> {
        self.inner.embed(req)
    }

    fn stream(&self, req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Err(msg) = (self.hook)(n, &req) {
            let err = faktor_provider::ProviderError::new(
                faktor_provider::ProviderErrorKind::Malformed,
                msg,
            );
            return Box::pin(futures::stream::iter(vec![Err(err)]));
        }
        self.inner.stream(req)
    }
}

/// Test-only provider wrapper: delegates capabilities/streaming to
/// `inner` but reports a FIXED `runtime_context_limit` — simulating a
/// live runtime window (an ollama /api/ps allocation) far below the
/// advertised model maximum.
pub(crate) struct RuntimeLimitedProvider {
    pub(crate) inner: Arc<dyn faktor_provider::Provider>,
    pub(crate) limit: usize,
}

impl RuntimeLimitedProvider {
    pub(crate) fn new(inner: Arc<dyn faktor_provider::Provider>, limit: usize) -> Self {
        Self { inner, limit }
    }
}

impl faktor_provider::Provider for RuntimeLimitedProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }

    fn runtime_context_limit(&self, _model: &str) -> Option<usize> {
        Some(self.limit)
    }

    fn stream(&self, req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        self.inner.stream(req)
    }
}

pub(crate) struct AlwaysAllow;

impl PermissionRequester for AlwaysAllow {
    fn request(
        &self,
        _s: SessionId,
        _p: &SessionPermission,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
    > {
        Box::pin(async { Ok(PermissionDecision::Allow) })
    }
}

pub(crate) fn echo_tool() -> Tool {
    Tool {
        name: "echo".into(),
        description: "echo back".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
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
    }
}

pub(crate) fn scripted_provider(script: Vec<ScriptedResponse>) -> FakeProvider {
    FakeProvider::with_script(
        "fake",
        ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        script,
    )
}

pub(crate) fn new_session(deps: &AgentDeps) -> SessionId {
    let ws = deps.session.create_workspace("/w").unwrap();
    deps.session
        .create_session(ws, "test session", "fake", "m")
        .unwrap()
        .id()
}

// ---- lazy tool activation (docs/acquire.md §2/§4) -------------------

/// A lazy test tool with the normative Acquire trigger vocabulary.
pub(crate) fn lazy_market_tool() -> Tool {
    Tool {
        name: "source_market".into(),
        description: "d".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"op": {"enum": ["search"]}},
            "required": ["op"],
            "additionalProperties": false
        }),
        resource_class: faktor_core::resource::ResourceClass::Network,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx, _args| Box::pin(async { Ok(ToolOutcome::default()) })),
    }
}

pub(crate) fn lazy_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_lazy(
        lazy_market_tool(),
        crate::activation::ToolExposure::lazy(
            crate::activation::acquire_source_phases(),
            crate::activation::acquire_source_triggers(),
        ),
    );
    registry
}

/// A lazy `source_market` whose body counts executions; description and
/// schema carry canaries so the refusal tests can prove neither ever
/// leaks beyond the tool name the model itself used.
pub(crate) fn counting_lazy_market_tool(executions: Arc<std::sync::atomic::AtomicUsize>) -> Tool {
    Tool {
        name: "source_market".into(),
        description: "SOURCE_MARKET_DESCRIPTION_CANARY".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "op": {"enum": ["search"]},
                "canary": {"const": "SOURCE_MARKET_SCHEMA_CANARY"}
            },
            "required": ["op"],
            "additionalProperties": false
        }),
        resource_class: faktor_core::resource::ResourceClass::Network,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx, _args| {
            let executions = executions.clone();
            Box::pin(async move {
                executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutcome {
                    text: "sourced".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

pub(crate) fn counting_lazy_registry(
    executions: Arc<std::sync::atomic::AtomicUsize>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_lazy(
        counting_lazy_market_tool(executions),
        crate::activation::ToolExposure::lazy(
            crate::activation::acquire_source_phases(),
            crate::activation::acquire_source_triggers(),
        ),
    );
    registry
}

// ---- ChunkSink (audit 41): bounded channel + drop-oldest coalescing.

pub(crate) fn text_event(sid: SessionId, mid: i64, text: &str) -> ChunkEvent {
    ChunkEvent {
        session_id: sid,
        message_id: Some(mid),
        kind: "text",
        text: text.into(),
    }
}

/// A provider that reports usage anthropic-style: the canonical frame
/// carries only the uncached remainder while cache reads/writes come as
/// separate additive lines — settlement must price each line and never
/// zero the recorded call.
pub(crate) struct CacheHeavyProvider;

impl faktor_provider::Provider for CacheHeavyProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderChunk::Text { text: "hi".into() }),
            Ok(ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 0,
                cache_read_tokens: 900,
                cache_write_tokens: 100,
                output_tokens: 7,
                reasoning_tokens: 0,
                reported_cost: None,
                request_id: None,
            })),
            Ok(ProviderChunk::Done),
        ]))
    }
}

// ------------------------------------------------------------------
// Refused tool calls (gap fix): a denial is durable turn history, never
// a dangling call. Every refusal kind answers its tool call with a typed
// tool_result part (kind + bounded, secret-free reason) and the turn
// continues through the SAME legal state walk; the approved path stays
// byte-identical. A crash between the denial decision and the result
// write is repaired on the next session open (crash-resume integrity).

/// A `PermissionRequester` that denies every hop.
pub(crate) struct DenyEveryTool;

impl PermissionRequester for DenyEveryTool {
    fn request(
        &self,
        _s: SessionId,
        _p: &SessionPermission,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
    > {
        Box::pin(async { Ok(PermissionDecision::Deny) })
    }
}

/// An echo tool that counts executions (a refused call must never reach
/// the tool body).
pub(crate) fn counting_echo_tool(executions: Arc<std::sync::atomic::AtomicUsize>) -> Tool {
    Tool {
        name: "echo".into(),
        description: "echo back".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx, args| {
            let executions = executions.clone();
            Box::pin(async move {
                executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutcome {
                    text: format!("echo: {args}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// A DiskWrite tool that counts executions.
pub(crate) fn counting_write_tool(executions: Arc<std::sync::atomic::AtomicUsize>) -> Tool {
    Tool {
        name: "write_file".into(),
        description: "writes a file".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(move |_ctx, _args| {
            let executions = executions.clone();
            Box::pin(async move {
                executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutcome {
                    text: "wrote".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// The durable tool_result answering `call_id`, if any: (excerpt,
/// exit_code).
pub(crate) fn tool_result_for(
    handle: &faktor_session::SessionHandle,
    call_id: &str,
) -> Option<(String, Option<i64>)> {
    for m in handle.messages_before(None, 100).unwrap() {
        for p in handle.parts_of(m.id).unwrap() {
            if p.kind == "tool_result"
                && p.data.get("tool_call_id").and_then(|v| v.as_str()) == Some(call_id)
            {
                return Some((
                    p.data
                        .get("excerpt")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    p.data.get("exit_code").and_then(|v| v.as_i64()),
                ));
            }
        }
    }
    None
}

/// Every wire-visible tool call (the states `history_messages` carries)
/// with NO answering result part.
pub(crate) fn dangling_tool_calls(handle: &faktor_session::SessionHandle) -> Vec<String> {
    let mut calls = Vec::new();
    let mut answered = std::collections::HashSet::new();
    for m in handle.messages_before(None, 200).unwrap() {
        for p in handle.parts_of(m.id).unwrap() {
            let Some(id) = p.data.get("tool_call_id").and_then(|v| v.as_str()) else {
                continue;
            };
            match p.kind.as_str() {
                "tool_call"
                    if matches!(
                        p.data.get("state").and_then(|v| v.as_str()),
                        Some("completed") | Some("error")
                    ) =>
                {
                    calls.push(id.to_string())
                }
                "tool_result" => {
                    answered.insert(id.to_string());
                }
                _ => {}
            }
        }
    }
    calls
        .into_iter()
        .filter(|c| !answered.contains(c))
        .collect()
}

pub(crate) fn ledger_known_failures(handle: &faktor_session::SessionHandle) -> Vec<String> {
    let raw = handle.get_task_ledger().unwrap().expect("ledger row");
    let ledger: faktor_context::ledger::TaskLedger = serde_json::from_value(raw).unwrap();
    ledger.known_failures
}

/// Shared assertions for ONE refused call: the typed denial result is
/// present (error-flagged, bounded, tagged), nothing executed, no run
/// row exists, the call is not dangling, and the refusal is durable
/// ledger + journal history.
pub(crate) fn assert_refusal_answered(
    runtime: &AgentRuntime,
    session: SessionId,
    call_id: &str,
    kind_tag: &str,
    reason_fragment: &str,
    executions: &Arc<std::sync::atomic::AtomicUsize>,
) {
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a refused call must never execute"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    let events = handle.events_range(1, None).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::ToolStarted),
        "no run may start for a refused call"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::PermissionDenied),
        "the refusal must be journaled"
    );
    let (excerpt, exit) =
        tool_result_for(&handle, call_id).unwrap_or_else(|| panic!("{call_id} unanswered"));
    assert_eq!(exit, Some(1), "a denial is an error result: {excerpt}");
    assert!(
        excerpt.contains(&format!("tool call denied ({kind_tag})")),
        "typed denial tag missing: {excerpt}"
    );
    assert!(
        excerpt.contains(reason_fragment),
        "the refusal reason must reach the model: {excerpt}"
    );
    assert!(excerpt.len() <= 2000, "bounded excerpt: {}", excerpt.len());
    let dangling = dangling_tool_calls(&handle);
    assert!(dangling.is_empty(), "dangling calls: {dangling:?}");
    let failures = ledger_known_failures(&handle);
    assert!(
        failures.iter().any(|f| f.contains(kind_tag)),
        "the refusal is durable ledger history: {failures:?}"
    );
}

// ------------------------------------------------------------------
// Mixed PERMISSION batch (the last state-machine hole): a permission
// DENIED call plus an APPROVED sibling in the SAME batch. The denied
// call's result is written only after the whole batch resolves, so at
// deny time the sibling is durably pending (its tool_call part has no
// result). Landing `ReadyForNextTurn` there claimed the batch was
// finished and the approved sibling's next hop (`ToolRequested`,
// `ToolStarted`, `FileChanged`) died with
// `InvalidState{ReadyForNextTurn -> ExecutingTool}`. The denial now
// lands on the batch-execution edge (`ExecutingTool`) while a sibling
// is pending; the deny-only batch keeps its `ReadyForNextTurn` landing.

/// A `PermissionRequester` that denies exactly the listed tool names (a
/// tool without an explicit capability requests `ExecuteShell { command:
/// <tool name> }`) and allows every sibling.
pub(crate) struct DenyToolNames(pub(crate) &'static [&'static str]);

impl PermissionRequester for DenyToolNames {
    fn request(
        &self,
        _s: SessionId,
        p: &SessionPermission,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
    > {
        let denied = matches!(
            &p.capability,
            faktor_core::capability::Capability::ExecuteShell { command }
                if self.0.contains(&command.as_str())
        );
        Box::pin(async move {
            Ok(if denied {
                PermissionDecision::Deny
            } else {
                PermissionDecision::Allow
            })
        })
    }
}

/// One refused call's durable result: error-flagged, typed
/// `permission_denied`, naming the tool, answered (never dangling).
pub(crate) fn assert_permission_denial(
    handle: &faktor_session::SessionHandle,
    call_id: &str,
    tool: &str,
) {
    let (excerpt, exit) =
        tool_result_for(handle, call_id).unwrap_or_else(|| panic!("{call_id} unanswered"));
    assert_eq!(exit, Some(1), "a denial is an error result: {excerpt}");
    assert!(
        excerpt.contains("tool call denied (permission_denied)"),
        "{excerpt}"
    );
    assert!(
        excerpt.contains(&format!("permission denied: {tool}")),
        "{excerpt}"
    );
}

/// The wire-visible tool results of the recorded second request:
/// (call_id, is_error), sorted.
pub(crate) fn wire_tool_results(recorder: &RecordingProvider) -> Vec<(String, bool)> {
    let requests = recorder.requests();
    let mut seen: Vec<(String, bool)> = Vec::new();
    for m in &requests[1].messages {
        for p in &m.content {
            if let ContentKind::ToolResult { is_error, .. } = &p.kind {
                seen.push((p.tool_call_id.clone().unwrap_or_default(), *is_error));
            }
        }
    }
    seen.sort();
    seen
}

/// Build the mixed-batch runtime: `calls` is the model's batch, the
/// permission requester denies the listed tool names, the trailing text
/// continues the turn.
pub(crate) fn mixed_permission_batch(
    calls: Vec<(String, String, serde_json::Value)>,
    denied: &'static [&'static str],
    tools: Vec<Tool>,
) -> (
    Arc<AgentRuntime>,
    Arc<RecordingProvider>,
    SessionId,
    tempfile::TempDir,
) {
    let mut script: Vec<ScriptedResponse> = calls
        .into_iter()
        .map(|(id, name, input)| ScriptedResponse::ToolCall { id, name, input })
        .collect();
    script.push(ScriptedResponse::Text("done".into()));
    script.push(ScriptedResponse::End);
    let recorder = RecordingProvider::new(Arc::new(scripted_provider(script)));
    let (mut deps, dir) = deps_with(recorder.clone(), tools);
    deps.permission_requester = Arc::new(DenyToolNames(denied));
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    (runtime, recorder, session, dir)
}

/// A tool whose runnable returns a typed error: the scheduler marks the
/// op `Failed` — an ORDINARY per-tool failure, distinct from a scheduler
/// infrastructure failure.
pub(crate) fn failing_tool() -> Tool {
    Tool {
        name: "explode".into(),
        execute: Arc::new(|_ctx, _args| {
            Box::pin(async move { Err(Error::new(ErrorKind::Internal, "tool blew up")) })
        }),
        ..echo_tool()
    }
}

/// The ToolCompleted journal rows of one session, `(op_id, status)`.
pub(crate) fn tool_completed_statuses(
    handle: &faktor_session::SessionHandle,
) -> Vec<(Option<OpId>, String)> {
    handle
        .events_range(1, None)
        .unwrap()
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::ToolCompleted)
        .map(|e| {
            let status = e
                .payload
                .as_ref()
                .and_then(|p| p.get("status"))
                .and_then(|s| s.as_str())
                .unwrap_or("?")
                .to_string();
            (e.op_id, status)
        })
        .collect()
}

/// Wait until the LIVE queue runner of `session` has STARTED `passes`
/// bounded passes (`0` when no runner is live).
pub(crate) async fn wait_for_runner_passes(
    runtime: &AgentRuntime,
    session: SessionId,
    passes: u64,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = runtime
            .runners
            .lock()
            .unwrap()
            .get(&session)
            .map(|gate| gate.passes)
            .unwrap_or(0);
        if current >= passes {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the queue runner never reached pass {passes} (at {current})"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---- end-of-turn verification (audit: verification must not depend on
// the model's discretion) ----

/// A REAL Rust workspace (Cargo.toml + src/lib.rs on disk) with a
/// session and a write tool, so the end-of-turn repo map resolves and
/// the engine derives Rust checks from the model's OWN changed files.
pub(crate) fn verified_rust_env(
    script: Vec<ScriptedResponse>,
    verification: Option<Arc<crate::VerificationService>>,
) -> (AgentDeps, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    let write_tool = Tool {
        name: "write_file".into(),
        description: "w".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(|_ctx, args| {
            Box::pin(async move {
                let _ = args;
                Ok(ToolOutcome {
                    text: "wrote".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    };
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(scripted_provider(script)))
        .unwrap();
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(write_tool);
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let deps = AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tool_registry),
        cas: Some(Arc::new(
            faktor_cas::Cas::open(dir.path().join("cas")).unwrap(),
        )),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: verification.unwrap_or_else(crate::VerificationService::disabled),
        hooks: None,
        instructions_resolver: test_resolver(&session),
        routing: crate::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test agent.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: crate::fallback_semantic_registry(),
        context_prior: None,
        efficiency: Default::default(),
    };
    (deps, dir, root)
}

pub(crate) fn session_in_workspace(deps: &AgentDeps, root: &std::path::Path) -> SessionId {
    let ws = deps
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    deps.session
        .create_session(ws, "verified", "fake", "m")
        .unwrap()
        .id()
}

// ---- durable background verification jobs (audit P0-5/26) ----
// The three adversarial flows below drive REAL make processes through
// THE supervisor executor: a Full-category check must never hold its
// turn inline, must survive restarts honestly and must never complete a
// task from a vanished or failed process.

/// One real-Make workspace on a fresh manager: `make -j` (the Unit
/// build check) runs INLINE and passes (default target), and the
/// required Full checks (`make test` / `make check` when the Makefile
/// defines the targets) run as durable background jobs.
pub(crate) fn make_background_env(
    test_recipe: &str,
    check_recipe: Option<&str>,
) -> (Arc<SessionManager>, SessionId, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let mut makefile = String::from("all:\n\t@true\n");
    makefile.push_str("test:\n");
    makefile.push_str(test_recipe);
    if let Some(check) = check_recipe {
        makefile.push_str("check:\n");
        makefile.push_str(check);
    }
    std::fs::write(root.join("Makefile"), makefile).unwrap();
    std::fs::write(
        root.join("src/main.c"),
        "int main(void) {\n    int base = 40;\n    printf(\"%d\\n\", base);\n    return 0;\n}\n",
    )
    .unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws, "background verification", "fake", "m")
        .unwrap()
        .id();
    (manager, session, dir)
}

pub(crate) fn real_background_verifier() -> Arc<crate::VerificationService> {
    crate::VerificationService::new(
        Arc::new(
            faktor_verify::exec::AsyncCheckExecutor::try_shared().expect("standalone supervisor"),
        ),
        faktor_verify::exec::VerificationPolicy::default(),
    )
}

// ---- completion gating (audits 4/6/7: VerifiedComplete / Unverified /
// BlockedVerification / FailedVerification at the genuine turn end) ----

/// Multi-turn Rust workspace environment: ONE real workspace on disk
/// bound to a session on a SHARED session manager, so later runtimes on
/// the same manager keep the ledger, workspace and memory rows durable
/// across logical turns.
pub(crate) fn verified_shared_env() -> (Arc<SessionManager>, SessionId, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws, "gating task", "fake", "m")
        .unwrap()
        .id();
    (manager, session, dir)
}

/// One logical turn's deps on a SHARED verified workspace: a real write
/// tool (files land on disk so the review reads real heads), the given
/// verification service and compaction trigger.
pub(crate) fn verified_turn_deps(
    manager: &Arc<SessionManager>,
    script: Vec<ScriptedResponse>,
    verification: Arc<crate::VerificationService>,
    compact_at_usage: f64,
) -> (AgentDeps, tempfile::TempDir) {
    let (mut deps, dir) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(script)),
        vec![real_write_tool()],
    );
    deps.verification = verification;
    deps.compact_at_usage = compact_at_usage;
    (deps, dir)
}

/// Run the REAL runtime mining path end-to-end on one shared session: a
/// failed verification turn records an unverified episode, the next
/// verified turn mines the recovery into the durable ledger corpus.
/// Returns the manager, session, data dir and the stored learning.
pub(crate) async fn mined_corpus_via_runtime() -> (
    Arc<SessionManager>,
    SessionId,
    tempfile::TempDir,
    faktor_learning::ProjectLearning,
) {
    use faktor_learning::{
        LearningService, LearningStore as _, SessionLearningStore, DEFAULT_MEMORY_CAPACITY,
    };

    let (manager, session, dir) = verified_shared_env();
    let (mut turn1, _d1) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/broken.rs",
                    "content": "pub fn broken() -> u32 { 1 }\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake(|_cmd: &str| Err("type error".to_string())),
        0.65,
    );
    turn1.efficiency.failure_learning = true;
    AgentRuntime::new(turn1)
        .unwrap()
        .run_turn(session, "write broken.rs", &[])
        .await
        .unwrap();
    let (mut turn2, _d2) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/fixed.rs",
                    "content": "pub fn fixed() -> u32 {\n    let base: u32 = 41;\n    let step: u32 = 1;\n    base.saturating_add(step).saturating_mul(2).saturating_add(1)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake_ok(),
        0.65,
    );
    turn2.efficiency.failure_learning = true;
    AgentRuntime::new(turn2)
        .unwrap()
        .run_turn(session, "write fixed.rs", &[])
        .await
        .unwrap();
    let handle = manager.get_session(session).unwrap().unwrap();
    let service =
        LearningService::new(SessionLearningStore::open(handle, DEFAULT_MEMORY_CAPACITY).unwrap());
    assert_eq!(
        service.len(),
        1,
        "the verified recovery must mine one learning"
    );
    let stored = service.store().all()[0].clone();
    (manager, session, dir, stored)
}

// ---- first-class durable Task rows (audit 25: gates write the typed
// row, restarts restore it into the facts, budgets gate completion)

/// Provider whose stream NEVER yields (a hung wire call): the drive
/// parks inside its stream until the future is aborted — the crash.
pub(crate) struct PendingProvider;

impl faktor_provider::Provider for PendingProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        Box::pin(futures::stream::pending::<
            Result<ProviderChunk, ProviderError>,
        >())
    }
}

pub(crate) fn task_snapshot(h: &faktor_session::SessionHandle) -> serde_json::Value {
    let t = h.list_tasks().unwrap().into_iter().next().unwrap();
    serde_json::json!({
        "goal": t.goal,
        "acceptance_criteria": t.acceptance_criteria,
        "state": serde_json::to_value(t.state).unwrap(),
        "created_ms": t.created_ms,
    })
}

pub(crate) fn criteria_fact(h: &faktor_session::SessionHandle) -> Option<String> {
    h.memory_facts()
        .unwrap()
        .into_iter()
        .find(|(k, key, _)| k == "criteria" && key == "0")
        .map(|(_, _, v)| v)
}

/// A tool whose execution takes a known 25 ms (guarantees the slice
/// budget expires before the next iteration boundary).
pub(crate) fn slow_tool() -> Tool {
    Tool {
        name: "slow_echo".into(),
        description: "echo after a pause".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx, args| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(ToolOutcome {
                    text: format!("slow echo: {args}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// Provider that yields ONE tool call per request, forever: without the
/// turn-budget slice an agent loop on this provider never ends.
pub(crate) struct InfiniteToolProvider;

impl faktor_provider::Provider for InfiniteToolProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            ..Default::default()
        }
    }

    fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        use futures::stream;
        Box::pin(stream::iter(vec![Ok(ProviderChunk::ToolCall {
            id: "c_loop".into(),
            name: "slow_echo".into(),
            input: serde_json::json!({}),
            complete: true,
        })]))
    }
}

// ---- completion review at genuine turn ends (audit round 14) ----

/// A REAL write tool: mirrors the production write_file by resolving the
/// relative path through the session workspace handle and persisting the
/// content — the review must then read the head back from disk.
pub(crate) fn real_write_tool() -> Tool {
    Tool {
        name: "write_file".into(),
        description: "writes a real file".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(|ctx, args| {
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
                    text: "wrote".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

pub(crate) fn review_env(
    script: Vec<ScriptedResponse>,
) -> (AgentDeps, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    let mut registry = ProviderRegistry::new();
    registry
        .try_register(Arc::new(scripted_provider(script)))
        .unwrap();
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(real_write_tool());
    let session =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let deps = AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tool_registry),
        cas: Some(Arc::new(
            faktor_cas::Cas::open(dir.path().join("cas")).unwrap(),
        )),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: fake_ok(),
        hooks: None,
        instructions_resolver: test_resolver(&session),
        routing: crate::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test agent.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: crate::fallback_semantic_registry(),
        context_prior: None,
        efficiency: Default::default(),
    };
    (deps, dir, root)
}

// ----------------------------------------------------------- secret /
// provenance gate tests (audit round 16: enforcement at the runtime
// boundaries, adversarial-only)

pub(crate) const SK_SAMPLE: &str = "sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

pub(crate) const GHP_SAMPLE: &str = "ghp_0123456789abcdefghijklmnopqrstuv";

pub(crate) const AKIA_SAMPLE: &str = "AKIA0123456789ABCDEF";

/// Grow a REAL shared-session history of `count` long turns (assistant
/// replies ~`text_len` chars each) so the next turn's context triggers
/// compaction and the deterministic fallback fits under the hard cap
/// with margin.
pub(crate) async fn seed_long_history(
    manager: &Arc<SessionManager>,
    session: SessionId,
    count: usize,
    text_len: usize,
) {
    let caps = ModelCapabilities {
        tools: true,
        context: 200_000,
        ..Default::default()
    };
    for i in 0..count {
        let (turn_deps, _dir) = deps_sharing_session(
            manager.clone(),
            Arc::new(FakeProvider::with_script(
                "fake",
                caps.clone(),
                vec![
                    ScriptedResponse::Text(format!("turn seed{i} {}", "z".repeat(text_len))),
                    ScriptedResponse::End,
                ],
            )),
            vec![],
        );
        let runtime = AgentRuntime::new(turn_deps).unwrap();
        runtime
            .run_turn(session, &format!("prompt seed {i}"), &[])
            .await
            .unwrap();
    }
}

/// Concatenated text of every message in a provider request — the wire
/// history that rides the next request after compaction is where a
/// leaked partial summary would land.
pub(crate) fn request_text(req: &GenericAgentRequest) -> String {
    let mut out = String::new();
    for m in &req.messages {
        for c in &m.content {
            if let ContentKind::Text { text } = &c.kind {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

/// Channel-gated provider double for compaction-summary tests:
/// `stream()` records the request's cancellation token (the same test
/// hook FakeProvider offers) and returns a stream that yields EXACTLY
/// the chunks pushed through [`GatedStreamProvider::push`]. While no
/// chunk is pushed the stream parks indefinitely — a provider that
/// stalls without erroring or ending — until the summarizer gives up
/// (deadline or turn cancellation) and drops the stream. Dropping the
/// stream closes the channel, so a later push reports false.
pub(crate) struct GatedStreamProvider {
    pub(crate) caps: ModelCapabilities,
    pub(crate) recorded: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    pub(crate) feed: Arc<std::sync::Mutex<Option<GatedFeed>>>,
}

/// One live stream's chunk channel (see [`GatedStreamProvider`]).
pub(crate) type GatedFeed =
    tokio::sync::mpsc::UnboundedSender<Result<ProviderChunk, ProviderError>>;

impl GatedStreamProvider {
    pub(crate) fn new() -> Self {
        Self {
            caps: ModelCapabilities {
                streaming: true,
                context: 64_000,
                ..Default::default()
            },
            recorded: Arc::new(std::sync::Mutex::new(None)),
            feed: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// The cancellation token of the last request this provider was
    /// asked to stream (`None` when nothing was streamed yet).
    pub(crate) fn recorded(&self) -> Option<CancellationToken> {
        self.recorded.lock().unwrap().clone()
    }

    /// Push one chunk to the CURRENT open stream. Returns false once
    /// the summarizer terminated the stream (receiver dropped).
    pub(crate) fn push(&self, chunk: Result<ProviderChunk, ProviderError>) -> bool {
        self.feed
            .lock()
            .unwrap()
            .as_ref()
            .map(|tx| tx.send(chunk).is_ok())
            .unwrap_or(false)
    }
}

impl faktor_provider::Provider for GatedStreamProvider {
    fn id(&self) -> &str {
        "gated"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        self.caps.clone()
    }

    fn stream(&self, req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        *self.recorded.lock().unwrap() = Some(req.meta.cancellation.clone());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.feed.lock().unwrap() = Some(tx);
        Box::pin(futures::stream::unfold(rx, |rx| async move {
            let mut rx = rx;
            let chunk = rx.recv().await?;
            Some((chunk, rx))
        }))
    }
}

// ============================================================
// P0 recovery invariants (turn records, idempotent replay,
// workspace-aware write postconditions).
// ============================================================

pub(crate) use faktor_core::id::{TaskId, WorkspaceId, WorktreeId};

pub(crate) use faktor_core::op::OpMeta;

pub(crate) use faktor_core::time::Deadline;

pub(crate) use std::sync::atomic::{AtomicUsize, Ordering};

/// A counting tool (execution observable for exactly-once assertions).
pub(crate) fn counting_tool(name: &str, hint: RecoveryHint, counter: Arc<AtomicUsize>) -> Tool {
    let name_owned = name.to_string();
    Tool {
        name: name.to_string(),
        description: "counting".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: hint,
        path_args: vec![],
        execute: Arc::new(move |_ctx, args| {
            let counter = counter.clone();
            let name = name_owned.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutcome {
                    text: format!("ran {name}:{args}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

pub(crate) fn op_meta(m: &Arc<SessionManager>, s: SessionId, recovery: RecoveryStrategy) -> OpMeta {
    let op = m.try_next_op_id().unwrap();
    OpMeta::new(
        op,
        s,
        Deadline::at(m.now_ms() + 60_000),
        faktor_core::retry::RetryPolicy::default(),
        CancellationToken::new(),
        recovery,
        m.now_ms(),
    )
}

/// Journal the machine chain exactly the way the runtime does before a
/// tool batch (from the freshly-admitted Preparing state).
pub(crate) fn chain_to_streaming(handle: &faktor_session::SessionHandle, turn_op: OpId) {
    assert_eq!(handle.state().unwrap(), AgentState::Preparing);
    handle
        .append_event(
            faktor_core::event::EventKind::ContextPrepared,
            AgentState::BuildingContext,
            Some(turn_op),
            None,
        )
        .unwrap();
    handle
        .append_event(
            faktor_core::event::EventKind::ModelStarted,
            AgentState::WaitingForModel,
            Some(turn_op),
            None,
        )
        .unwrap();
    handle
        .append_event(
            faktor_core::event::EventKind::ModelChunkReceived,
            AgentState::Streaming,
            Some(turn_op),
            None,
        )
        .unwrap();
}

/// Start ONE durable tool run the way run_tool_calls does (permission
/// hop + the model's tool_call part + ToolStarted) and leave it running
/// — the residue of a crash mid-tool-batch. May be called repeatedly on
/// the same turn (parallel batch).
pub(crate) fn crash_tool_start(
    handle: &faktor_session::SessionHandle,
    turn_op: OpId,
    tool: &str,
    args: serde_json::Value,
    call_id: &str,
    meta: OpMeta,
) {
    let perm = handle
        .request_permission(turn_op, &Capability::ReadWorkspace { path: ".".into() })
        .unwrap();
    handle
        .resolve_permission(perm.id, PermissionDecision::Allow)
        .unwrap();
    let seq = handle.proposed_message_seq().unwrap();
    let mid = handle
        .put_message(seq, "assistant", serde_json::json!({ "parts": [] }))
        .unwrap();
    handle
        .put_tool_call_part(mid, call_id, tool, args.clone(), "completed")
        .unwrap();
    handle.start_tool_run(meta, tool, args).unwrap();
    assert_eq!(handle.state().unwrap(), AgentState::ExecutingTool);
}

/// A fresh manager+runtime over the same durable dir (daemon restart).
pub(crate) fn reopen_runtime(
    dir: &tempfile::TempDir,
    provider: Arc<dyn faktor_provider::Provider>,
    tools: Vec<Tool>,
) -> (AgentDeps, tempfile::TempDir) {
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    deps_sharing_session(manager, provider, tools)
}

pub(crate) fn fresh_store_dir() -> tempfile::TempDir {
    tempdir().unwrap()
}

// ---- workspace-aware write postconditions (requirement 3) ----

pub(crate) fn workspace_env(
    dir: &tempfile::TempDir,
) -> (
    Arc<SessionManager>,
    WorkspaceId,
    SessionId,
    std::path::PathBuf,
) {
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws_id = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let handle = manager.create_session(ws_id, "t", "fake", "m").unwrap();
    (manager, ws_id, handle.id(), root)
}

// ---- transactional recovery terminalization (row+event atomicity) ----

/// Build the deterministic op-active residue the seam campaign sweeps: a
/// real session driven to `ExecutingTool` with two running `MarkUnknown`
/// tool runs. The fixture is file-free, so every row classifies as
/// failed/unknown and the batch lands `FailedRecoverable`.
pub(crate) fn recovery_seam_fixture(
    dir: &std::path::Path,
) -> (Arc<SessionManager>, SessionId, Vec<OpId>) {
    let manager = SessionManager::open(dir.join("store"), dir.join("cas"), true).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let session = handle.id();
    let receipt = handle.submit_prompt("crash", &[]).unwrap();
    chain_to_streaming(&handle, receipt.op_id);
    let mut ops = Vec::new();
    for i in 0..2 {
        let meta = op_meta(&manager, session, RecoveryStrategy::MarkUnknown);
        ops.push(meta.operation_id);
        crash_tool_start(
            &handle,
            receipt.op_id,
            "read_file",
            serde_json::json!({ "row": i }),
            &format!("call_{i}"),
            meta,
        );
    }
    (manager, session, ops)
}

/// Reopen the crashed fixture's store: the in-process op/turn registry is
/// gone, so the sweep no longer sees a live driver (the same restart
/// condition the daemon has). Returns the running ops in row order.
pub(crate) fn recovery_seam_reopen(
    dir: &std::path::Path,
    session: SessionId,
) -> (Arc<SessionManager>, Vec<OpId>) {
    let manager = SessionManager::open(dir.join("store"), dir.join("cas"), true).unwrap();
    let handle = manager.get_session(session).unwrap().unwrap();
    let ops = handle
        .pending_tool_runs()
        .unwrap()
        .into_iter()
        .map(|r| r.op_id)
        .collect();
    (manager, ops)
}

pub(crate) fn recovery_seam_runtime(
    manager: Arc<SessionManager>,
) -> (Arc<AgentRuntime>, tempfile::TempDir) {
    let (deps, keep) = deps_sharing_session(manager, Arc::new(scripted_provider(vec![])), vec![]);
    (AgentRuntime::new(deps).unwrap(), keep)
}

/// The normalized durable world of the recovery scope: session state, per
/// row running/terminal, and each `RecoveryApplied` event's state+payload
/// (the store-global `op_id` is replaced by the deterministic row index;
/// `CrashDetected` annotations are excluded exactly like the fault
/// campaign's dumps).
pub(crate) fn recovery_seam_world(
    runtime: &AgentRuntime,
    session: SessionId,
    ops: &[OpId],
) -> Vec<String> {
    let handle = runtime
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let mut lines = vec![format!("state:{:?}", handle.state().unwrap())];
    let running: Vec<OpId> = handle
        .pending_tool_runs()
        .unwrap()
        .into_iter()
        .map(|r| r.op_id)
        .collect();
    for (i, op) in ops.iter().enumerate() {
        lines.push(format!(
            "row:{i}:{}",
            if running.contains(op) {
                "running"
            } else {
                "terminal"
            }
        ));
    }
    for e in handle.events_range(1, None).unwrap() {
        if e.kind != faktor_core::event::EventKind::RecoveryApplied {
            continue;
        }
        let idx = e
            .op_id
            .and_then(|o| ops.iter().position(|x| *x == o))
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".into());
        let payload = e
            .payload
            .map(|mut p| {
                if let Some(obj) = p.as_object_mut() {
                    obj.remove("op_id");
                }
                p.to_string()
            })
            .unwrap_or_default();
        lines.push(format!("ev:{idx}:{:?}:{payload}", e.state));
    }
    lines
}

// ---- completion review (audit round 14: independent skepticism) ----

pub(crate) fn signals_for(heads: &[(&str, &str)]) -> serde_json::Value {
    let changed: Vec<String> = heads.iter().map(|(p, _)| p.to_string()).collect();
    let snapshot: Vec<(String, String)> = heads
        .iter()
        .map(|(p, h)| (p.to_string(), h.to_string()))
        .collect();
    review_signals(&changed, &snapshot)
}

pub(crate) fn blocking_of(v: &serde_json::Value) -> Vec<String> {
    v.get("blocking")
        .and_then(|b| b.as_array())
        .map(|b| {
            b.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn suspects_of(v: &serde_json::Value) -> Vec<String> {
    v.get("suspects")
        .and_then(|s| s.as_array())
        .map(|s| {
            s.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---- review gate x quality (audit 92): the skeptical-review gate's
// bar. Normal keeps today's semantics byte-for-byte; Strict (the
// mutating-turn default) is fail-closed for mislabeled verdicts and
// advisory findings — a "weakened" review must never clear the gate.

pub(crate) fn review_json(
    verdict: &str,
    blocking: &[&str],
    suspects: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "verdict": verdict,
        "blocking": blocking,
        "suspects": suspects,
    })
}

// -------------------------------------------------------- lifecycle hooks

/// A hook that exits non-zero WITHOUT emitting a verdict: under
/// FailClosed the registry resolves that to a Deny — the adversarial
/// shape for "post-hoc verdicts must be audit-only".
#[cfg(unix)]
pub(crate) fn failing_closed_hook(
    id: &str,
    event: faktor_hooks::HookEvent,
) -> faktor_hooks::HookSpec {
    faktor_hooks::HookSpec {
        id: id.into(),
        events: vec![event],
        command: "sh".into(),
        args: vec!["-c".into(), "exit 1".into()],
        failure_policy: faktor_hooks::FailurePolicy::FailClosed,
        ..Default::default()
    }
}

/// A hook that dumps the FAKTOR_HOOK_INPUT json it received into `out`
/// (the env var is set by the registry for every run).
#[cfg(unix)]
pub(crate) fn file_writing_hook(
    id: &str,
    event: faktor_hooks::HookEvent,
    out: &std::path::Path,
) -> faktor_hooks::HookSpec {
    let out = out.display().to_string();
    faktor_hooks::HookSpec {
        id: id.into(),
        events: vec![event],
        command: "sh".into(),
        args: vec![
            "-c".into(),
            format!("printf %s \"$FAKTOR_HOOK_INPUT\" > \"{out}\""),
        ],
        ..Default::default()
    }
}

/// Drive one turn whose ONLY tool call genuinely errors (execute ->
/// Err). The session layer journals a failed tool finish as
/// FailedRecoverable, so the live drive reports an error and the
/// session lands promptable — with or without any hook. Returns the
/// tempdir (store lifetime), the runtime, the session, and the hook
/// audit (empty when no registry was wired).
#[allow(clippy::type_complexity)]
pub(crate) async fn drive_failing_tool_turn(
    hooks: Option<Arc<faktor_hooks::HookRegistry>>,
) -> (
    tempfile::TempDir,
    Arc<AgentRuntime>,
    SessionId,
    Vec<faktor_hooks::HookAuditRecord>,
) {
    let boom = Tool {
        name: "boom".into(),
        description: "always fails".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx, _args| {
            Box::pin(async move { Err(Error::new(ErrorKind::Internal, "exploded")) })
        }),
    };
    let (mut deps, dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "boom".into(),
                input: serde_json::json!({}),
            },
            ScriptedResponse::Text("after the failure".into()),
            ScriptedResponse::End,
        ]),
        vec![boom],
    );
    deps.hooks = hooks.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let _ = runtime.run_turn(session, "break it", &[]).await;
    let audit = hooks.map(|h| h.audit()).unwrap_or_default();
    (dir, runtime, session, audit)
}

/// Spy routing policy: records EVERY route request it sees (the
/// audit-D witness) and answers passthrough.
#[derive(Clone)]
pub(crate) struct SpyRouting {
    pub(crate) requests: Arc<std::sync::Mutex<Vec<faktor_router::RouteRequest>>>,
}

impl crate::RoutingPolicy for SpyRouting {
    fn route(
        &self,
        req: &faktor_router::RouteRequest,
    ) -> Result<RouteDecision, crate::RouteFailure> {
        self.requests.lock().unwrap().push(req.clone());
        Ok(empty_passthrough_decision())
    }

    fn mode(&self) -> crate::RoutingMode {
        crate::RoutingMode::Economy
    }
}

/// A provider whose `stream` PANICS when invoked — the tripwire proving
/// a typed route refusal never reaches the provider. The panic would
/// poison the test task, so a single invocation is a loud failure.
#[derive(Clone)]
pub(crate) struct CountingPanicProvider {
    pub(crate) inner: Arc<faktor_provider::FakeProvider>,
    pub(crate) calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingPanicProvider {
    pub(crate) fn new(caps: ModelCapabilities) -> Self {
        Self {
            inner: Arc::new(faktor_provider::FakeProvider::with_script(
                "fake",
                caps,
                vec![faktor_provider::ScriptedResponse::Text("never".into())],
            )),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl faktor_provider::Provider for CountingPanicProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }
    fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!(
            "tripwire provider streamed: the route refused this call before any provider contact"
        )
    }
    fn runtime_context_limit(&self, model: &str) -> Option<usize> {
        self.inner.runtime_context_limit(model)
    }
}

/// Provider that reports a per-call provider-reported cost on its usage
/// frame (usage settlement persists it into the durable reservation).
/// The per-call script also decides whether the stream opens with a
/// tool call (to force an interior hop that would need a SECOND stream).
/// One scripted stream: (provider-reported cost, open-with-tool-call?).
pub(crate) type CostStep = (Option<u64>, bool);

#[derive(Clone)]
pub(crate) struct CostReportingProvider {
    pub(crate) steps: Arc<std::sync::Mutex<std::collections::VecDeque<CostStep>>>,
    pub(crate) streams: Arc<std::sync::atomic::AtomicUsize>,
}

impl CostReportingProvider {
    pub(crate) fn new(steps: Vec<(Option<u64>, bool)>) -> Arc<Self> {
        Arc::new(Self {
            steps: Arc::new(std::sync::Mutex::new(steps.into_iter().collect())),
            streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    pub(crate) fn stream_count(&self) -> usize {
        self.streams.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl faktor_provider::Provider for CostReportingProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            streaming: true,
            ..Default::default()
        }
    }

    fn stream(&self, req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        self.streams
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (reported, tool_call) = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((None, false));
        let mut chunks: Vec<Result<ProviderChunk, ProviderError>> = Vec::new();
        if tool_call {
            chunks.push(Ok(ProviderChunk::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({}),
                complete: true,
            }));
        } else {
            chunks.push(Ok(ProviderChunk::Text {
                text: "costly answer".into(),
            }));
        }
        chunks.push(Ok(ProviderChunk::Usage(CanonicalUsage {
            uncached_input_tokens: 40,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 9,
            reasoning_tokens: 0,
            // The scripted cost is an authoritative USD provider report
            // (the only currency the runtime accepts as an override).
            reported_cost: reported.map(|micro| {
                ReportedCost::usd(micro, faktor_provider::ReportedCostSource::ProviderUsage)
            }),
            request_id: None,
        })));
        chunks.push(Ok(ProviderChunk::Done));
        let _ = req;
        Box::pin(futures::stream::iter(chunks))
    }
}

/// Read-failing budget authority (budget-read fail-safe policy): the
/// READ fails with the configured cap evidence. Writes stay harmless and
/// are counted: a hard-capped unavailable read must issue NO reserve at
/// all, while an explicitly uncapped one proceeds (one reserve + one
/// provider stream), because the durable reserve transaction remains the
/// real admission check even when the read surface cannot paint the
/// picture.
pub(crate) struct ReadFailingBudget {
    pub(crate) cap: BudgetCapEvidence,
    pub(crate) reserve_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl ReadFailingBudget {
    pub(crate) fn new(cap: BudgetCapEvidence) -> Arc<Self> {
        Arc::new(Self {
            cap,
            reserve_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    pub(crate) fn reserve_calls(&self) -> usize {
        self.reserve_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl BudgetAuthority for ReadFailingBudget {
    fn reserve(
        &self,
        _s: SessionId,
        _t: TaskId,
        _op: OpId,
        _pred: u64,
        _snap: Option<PricingSnapshot>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<faktor_session::ReservationId, SessionBudgetError>,
                > + Send,
        >,
    > {
        let calls = self.reserve_calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(faktor_session::ReservationId::NOOP)
        })
    }
    fn reserve_attempt(
        &self,
        _s: SessionId,
        _t: TaskId,
        _a: ModelCallAttempt,
        _pred: u64,
        _snap: Option<PricingSnapshot>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<faktor_session::ReservationId, SessionBudgetError>,
                > + Send,
        >,
    > {
        let calls = self.reserve_calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(faktor_session::ReservationId::NOOP)
        })
    }
    fn mark_dispatched(
        &self,
        _s: SessionId,
        _r: faktor_session::ReservationId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SessionBudgetError>> + Send>>
    {
        Box::pin(async { Ok(()) })
    }
    fn mark_uncertain(
        &self,
        _s: SessionId,
        _r: faktor_session::ReservationId,
        _reason: String,
        _request_id: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SessionBudgetError>> + Send>>
    {
        Box::pin(async { Ok(()) })
    }
    fn settle_usage(
        &self,
        _s: SessionId,
        _r: faktor_session::ReservationId,
        _a: u64,
        _b: u64,
        _c: u64,
        _d: u64,
        _e: Option<u64>,
        _f: Option<String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<u64>, SessionBudgetError>> + Send>,
    > {
        Box::pin(async { Ok(None) })
    }
    fn refund(
        &self,
        _s: SessionId,
        _r: faktor_session::ReservationId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SessionBudgetError>> + Send>>
    {
        Box::pin(async { Ok(()) })
    }
    fn session_budget_view(
        &self,
        _s: SessionId,
        _t: TaskId,
    ) -> Result<faktor_session::BudgetView, SessionBudgetError> {
        Err(SessionBudgetError::Unavailable {
            cap: self.cap,
            reason: "injected budget read failure".into(),
        })
    }
    fn recover_after_restart(&self) {}
    fn reconcile_uncertain(
        &self,
        _s: SessionId,
        _t: TaskId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<faktor_store::CostReconcileReport, SessionBudgetError>,
                > + Send,
        >,
    > {
        Box::pin(async { Ok(Default::default()) })
    }
    fn finalize_uncertain(
        &self,
        _s: SessionId,
        _t: TaskId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<faktor_store::CostFinalizeReport, SessionBudgetError>,
                > + Send,
        >,
    > {
        Box::pin(async { Ok(Default::default()) })
    }
}

/// Provider for the canonical-split settlement tests: ONE canonical
/// usage frame whose cache reads were already split off the uncached
/// counter at the adapter boundary (openai-style wire: total 1000
/// incl. 600 cached -> 400 uncached + 600 cache reads), plus an
/// optional provider-reported cost.
#[derive(Clone)]
pub(crate) struct CacheSplitProvider {
    pub(crate) reported: Option<ReportedCost>,
}

impl faktor_provider::Provider for CacheSplitProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            streaming: true,
            ..Default::default()
        }
    }

    fn stream(&self, req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        let _ = req;
        let usage = CanonicalUsage {
            uncached_input_tokens: 400,
            cache_read_tokens: 600,
            cache_write_tokens: 0,
            output_tokens: 50,
            reasoning_tokens: 0,
            reported_cost: self.reported.clone(),
            request_id: Some("req-cache-split".into()),
        };
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderChunk::Text {
                text: "answer".into(),
            }),
            Ok(ProviderChunk::Usage(usage)),
            Ok(ProviderChunk::Done),
        ]))
    }
}

/// A routed runtime whose single candidate prices at $1/M input,
/// $0.5/M cache read, $1/M output, against a fresh durable ledger.
pub(crate) async fn routed_settlement_runtime(
    provider: Arc<dyn faktor_provider::Provider>,
) -> (
    Arc<AgentRuntime>,
    faktor_core::id::SessionId,
    Arc<faktor_session::DurableBudgetLedger>,
) {
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider.clone()).unwrap();
    let (mut adeps, _dir) = deps_with(provider, vec![]);
    adeps.providers = Arc::new(registry);
    let ledger = faktor_session::DurableBudgetLedger::new(adeps.session.clone());
    let budgets: Arc<dyn faktor_session::BudgetAuthority> = ledger.clone();
    adeps.budgets = budgets;
    let candidate = faktor_core::model::ModelDescriptor {
        provider: "fake".into(),
        model: "m".into(),
        context: 512_000,
        max_output: 16_000,
        tools: true,
        parallel_tools: true,
        reasoning: false,
        thinking: false,
        vision: false,
        structured_output: false,
        embeddings: false,
        streaming: true,
        economics: faktor_core::model::ModelEconomics {
            input_price_per_mtok: faktor_core::model::MicroUsdPerToken::from_dollars_per_million(1),
            output_price_per_mtok: faktor_core::model::MicroUsdPerToken::from_dollars_per_million(
                1,
            ),
            coding_reliability: 90,
            tool_reliability: 90,
            ..Default::default()
        },
        source: faktor_core::model::ModelSource::ProviderCatalog,
    };
    adeps.routing = crate::EconomicRoutingPolicy::new(
        Arc::new(faktor_router::RouterService::with_pricing(
            vec![candidate.clone()],
            std::collections::HashMap::from([(
                (candidate.provider.clone(), candidate.model.clone()),
                faktor_core::model::PricingSnapshot::exact(
                    faktor_core::model::PriceQuote {
                        input:
                            faktor_core::model::MicroUsdPerMillionTokens::from_dollars_per_million(
                                1,
                            ),
                        output:
                            faktor_core::model::MicroUsdPerMillionTokens::from_dollars_per_million(
                                1,
                            ),
                        cache_read: faktor_core::model::MicroUsdPerMillionTokens(500_000),
                        cache_write: faktor_core::model::MicroUsdPerMillionTokens(1_250_000),
                    },
                    1,
                    "fake".to_string(),
                ),
            )]),
        )),
        crate::RoutingMode::Economy,
    );
    let runtime = AgentRuntime::new(adeps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id: handle.task_id().unwrap(),
            session_id: session,
            goal: "routed".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    (runtime, session, ledger)
}
