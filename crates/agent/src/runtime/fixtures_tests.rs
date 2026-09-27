//! `runtime::fixtures_tests`: shared test fixtures (part 2).

#![allow(unused_imports)]

use super::*;
use crate::runtime::tests::*;
use crate::*;

// ---- stall vs progress, runtime wiring (spec §28): a provider stream
// that goes silent past the stall budget is stopped with a stall verdict;
// a stream that keeps emitting output (progress evidence) for several
// times the budget never stalls.

/// Provider whose stream is FED BY THE TEST over an unbounded channel:
/// chunks exist only when the test sends them, so chunk timing is under
/// the test's control (paired with the runtime's injected skew clock).
/// The watchdog's real-time poll cadence then can never race chunk
/// delivery: silence is measured on the skew clock, and the skew clock
/// only advances when the test says so.
#[derive(Debug)]
pub(crate) struct FedProvider {
    pub(crate) tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<ProviderChunk>>>,
}

impl FedProvider {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            tx: std::sync::Mutex::new(None),
        })
    }

    /// The handle for this provider's CURRENT stream, created when the
    /// drive calls `stream()` — which happens just AFTER the durable
    /// Streaming journal hop, so tests must wait for it (bounded) before
    /// feeding.
    pub(crate) fn sender_available(&self) -> bool {
        self.tx.lock().unwrap().is_some()
    }

    pub(crate) fn sender(&self) -> tokio::sync::mpsc::UnboundedSender<ProviderChunk> {
        self.tx
            .lock()
            .unwrap()
            .clone()
            .expect("stream() must be called before feeding")
    }
}

/// Bounded wait until the provider's stream handle exists (the drive
/// opens the stream just after journaling Streaming).
pub(crate) async fn wait_fed_sender(fed: &FedProvider) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    while !fed.sender_available() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the drive never opened the fed stream"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

impl faktor_provider::Provider for FedProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: false,
            ..Default::default()
        }
    }

    fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ProviderChunk>();
        *self.tx.lock().unwrap() = Some(tx);
        // Chunks appear ONLY when the test sends them; the stream ends
        // when the channel closes after the final Done.
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|chunk| (Ok::<_, faktor_provider::ProviderError>(chunk), rx))
        }))
    }
}

/// The skew-clock deps twin of `deps_with`: same env, injectable clock.
/// Chunk delivery and watchdog polls no longer race each other: the
/// tracker reads the test clock, which advances only between chunks.
pub(crate) fn deps_with_skew(
    provider: Arc<dyn faktor_provider::Provider>,
) -> (AgentDeps, tempfile::TempDir, faktor_core::time::TestClock) {
    let (mut deps, dir) = deps_with(provider, vec![]);
    let clock = faktor_core::time::TestClock::new(10_000);
    deps.clock = Arc::new(clock.clone());
    (deps, dir, clock)
}

/// Wait (bounded) until the session's progress record shows output at
/// exactly `skew_now` — i.e. the drive has CONSUMED the chunk the test
/// just fed, so evidence and clock advance can never interleave wrongly.
pub(crate) async fn wait_output_at(
    runtime: &AgentRuntime,
    session: SessionId,
    skew_now: i64,
    what: &str,
) {
    // Environmental margin (documented bound): waits for the DRIVE task
    // under full-suite load; never a timing assertion.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let view = runtime.progress_view(session);
        let at = view
            .and_then(|v| v.get("lastOutputAt").and_then(|v| v.as_i64()))
            .unwrap_or(-1);
        if at >= skew_now {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "drive never consumed the fed output ({what}); lastOutputAt={at} want={skew_now}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Panics on ANY poll: the adversarial proof that the legacy scan is
/// unreachable on the hosted index path (its boxed future is only ever
/// polled when the IndexService could not be hosted at all).
pub(crate) struct PanicEvidence;

impl EvidenceProvider for PanicEvidence {
    fn evidence_for(
        &self,
        _s: SessionId,
        _q: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        Box::pin(async move {
            panic!("the legacy bounded scan must never run while the IndexService is hosted")
        })
    }
}

/// Embedder for the semantic-evidence E2E: the prompt concepts
/// (`quantum`, `zebra`) are semantically near `ledger`/`reconcile` — none
/// of those words appear in the corpus, so lexical/symbol/exact search
/// can never find the file; only the semantic leg can.
pub(crate) struct ConceptAxisEmbedder;

impl faktor_search::Embedder for ConceptAxisEmbedder {
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|t| {
                let l = t.to_lowercase();
                vec![
                    if l.contains("quantum") || l.contains("ledger") || l.contains("reconcile") {
                        1.0
                    } else {
                        0.0
                    },
                    if l.contains("zebra") || l.contains("parser") {
                        1.0
                    } else {
                        0.0
                    },
                ]
            })
            .collect()
    }
}

/// Evidence provider whose ONLY job is to expose a configured embedder
/// through the production seam (the legacy scan never runs on the
/// hosted-index path).
pub(crate) struct EmbedderEvidence {
    pub(crate) embedder: Option<Arc<dyn faktor_search::Embedder>>,
}

impl EvidenceProvider for EmbedderEvidence {
    fn evidence_for(
        &self,
        _s: SessionId,
        _q: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn embedder(&self) -> Option<Arc<dyn faktor_search::Embedder>> {
        self.embedder.clone()
    }
}

/// One full turn over a two-file corpus with a Ready index generation,
/// producing the CAPTURED wire request's system prompt.
pub(crate) async fn captured_system_for_prompt(
    embedder: Option<Arc<dyn faktor_search::Embedder>>,
    prompt: &str,
) -> String {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src").join("ledger.rs"),
        "pub fn reconcile_accounts() -> u32 { 7 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src").join("parser.rs"),
        "pub fn parse_expr() -> u32 { 1 }\n",
    )
    .unwrap();
    let provider = scripted_provider(vec![
        ScriptedResponse::Text("ok".into()),
        ScriptedResponse::End,
    ]);
    let fake = Arc::new(provider.clone());
    let (mut adeps, _adir) = deps_with(
        Arc::new(provider) as Arc<dyn faktor_provider::Provider>,
        vec![],
    );
    adeps.evidence = Arc::new(EmbedderEvidence { embedder });
    let ws = adeps
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let sid = adeps
        .session
        .create_session(ws, "idx", "fake", "m")
        .unwrap()
        .id();
    let seen = Arc::new(std::sync::Mutex::new(None::<String>));
    let hook = {
        let seen = seen.clone();
        move |_n: usize, req: &GenericAgentRequest| -> Result<(), String> {
            *seen.lock().unwrap() = Some(req.system.clone());
            Ok(())
        }
    };
    let inspected = Arc::new(InspectingProvider::new(fake, hook));
    let mut registry = ProviderRegistry::new();
    registry.try_register(inspected).unwrap();
    adeps.providers = Arc::new(registry);
    let runtime = AgentRuntime::new(adeps).unwrap();
    let svc = runtime.index_service().expect("index service hosted");
    svc.attach(ws).unwrap();
    svc.ensure_ready(ws, std::time::Instant::now() + Duration::from_secs(20))
        .expect("generation 1 builds");
    runtime.run_turn(sid, prompt, &[]).await.unwrap();
    let captured = seen.lock().unwrap().clone().expect("request sent");
    captured
}

// -------------------------------------------------------- prefix fill
// (audits 65-66): the usage-settlement site records a byte-truth prefix
// observation per completed provider call.

/// A legacy evidence provider that PARKS FOREVER on its first poll: its
/// future can never complete (a plain `std::sync::mpsc` receive — never
/// a tokio timer, so the future outlives the drop of the turn's runtime
/// without touching its timer driver). `started` flips synchronously
/// when `evidence_for` is CALLED (on the polling thread), so the test
/// can prove the hostile provider really ran: if the runtime ever
/// awaited this future to completion the turn could not return at all,
/// and any return with `started` set is structurally the wall budget
/// having cut the wait — no wall-clock race with the provider's own
/// lifetime.
pub(crate) struct ParkedEvidence {
    pub(crate) started: Arc<std::sync::atomic::AtomicBool>,
}

impl EvidenceProvider for ParkedEvidence {
    fn evidence_for(
        &self,
        _s: SessionId,
        _q: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            let (_tx, rx) = std::sync::mpsc::channel::<()>();
            let _ = rx.recv();
            Ok(vec![])
        })
    }
}

/// A legacy evidence provider that PANICS on its first poll.
pub(crate) struct PanickingEvidence {
    pub(crate) started: Arc<std::sync::atomic::AtomicBool>,
}

impl EvidenceProvider for PanickingEvidence {
    fn evidence_for(
        &self,
        _s: SessionId,
        _q: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { panic!("adversarial: this provider panics on every poll") })
    }
}

// ------------------------------- durable typed evidence-poll status
//
// The advisory poll used to hand the turn only its package, so a failed
// retrieval and an honest "no evidence" answer were the SAME durable
// fact. These tests drive the real loop with a BLOCKED index root (the
// documented degrade to `deps.evidence`) and read every typed status back
// from the durable evidence authority: each outcome is distinct, bounded
// and survives a store reopen, while the advisory policy leaves the turn
// itself untouched.

/// Legacy evidence provider that always fails with the given error.
pub(crate) struct FailingEvidence {
    pub(crate) error: Error,
}

impl EvidenceProvider for FailingEvidence {
    fn evidence_for(
        &self,
        _s: SessionId,
        _q: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
}

/// Legacy evidence provider that honestly answers "nothing matched".
pub(crate) struct EmptyEvidence;

impl EvidenceProvider for EmptyEvidence {
    fn evidence_for(
        &self,
        _s: SessionId,
        _q: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

/// Legacy evidence provider that SERVES the given package byte-for-byte.
pub(crate) struct ServingEvidence {
    pub(crate) package: Vec<Evidence>,
}

impl EvidenceProvider for ServingEvidence {
    fn evidence_for(
        &self,
        _s: SessionId,
        _q: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        let package = self.package.clone();
        Box::pin(async move { Ok(package) })
    }
}

/// Deps for one legacy-poll drive: the given model provider, a BLOCKED
/// index root (so every turn degrades to `deps.evidence`), and one
/// workspace/session whose prompt changes no files (the cold path yields
/// nothing, so the legacy poll is the turn's only evidence provider).
pub(crate) fn poll_drive_deps(
    provider: Arc<dyn faktor_provider::Provider>,
    evidence: Arc<dyn EvidenceProvider>,
) -> (AgentDeps, tempfile::TempDir, SessionId, WorkspaceId, TaskId) {
    let (mut deps, dir) = deps_with(provider, vec![]);
    std::fs::write(
        dir.path().join("store").join("index_data"),
        b"not a directory",
    )
    .unwrap();
    let ws_root = dir.path().join("repo");
    std::fs::create_dir_all(ws_root.join("src")).unwrap();
    std::fs::write(
        ws_root.join("src").join("lib.rs"),
        "pub fn balance_account() -> i64 { 42 }\n",
    )
    .unwrap();
    let ws = deps
        .session
        .create_workspace(ws_root.to_str().unwrap())
        .unwrap();
    let sid = deps
        .session
        .create_session(ws, "ev-poll", "fake", "m")
        .unwrap()
        .id();
    deps.evidence = evidence;
    let task_id = deps
        .session
        .get_session(sid)
        .unwrap()
        .unwrap()
        .task_id()
        .unwrap();
    (deps, dir, sid, ws, task_id)
}

/// Every durable typed poll status archived for the task scope, decoded
/// through the canonical encoding.
pub(crate) fn archived_poll_statuses(
    authority: &DurableEvidenceAuthority,
    sid: SessionId,
    ws: WorkspaceId,
    task_id: TaskId,
) -> Vec<crate::EvidencePollStatus> {
    let ctx = faktor_context::compiler::EvidenceAccessContext::new(
        sid.raw(),
        ws.raw(),
        Some(task_id.raw()),
    );
    authority
        .list_scoped_envelopes(&ctx, 64)
        .unwrap()
        .into_iter()
        .filter(|env| {
            env.kind == EvidenceKind::GenericText
                && env
                    .source_revision
                    .as_deref()
                    .is_some_and(|revision| revision.starts_with("evidence-poll:"))
        })
        .map(|env| {
            decode_evidence_poll_status(&env.compact.body)
                .expect("a durable poll status must decode")
        })
        .collect()
}

pub(crate) fn one_text_turn() -> Vec<ScriptedResponse> {
    vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End]
}

/// Spy routing policy (v19 end-to-end): records every `TurnPrefix`
/// history the runtime hands the cache-economics consult and defers to
/// the session's configured provider/model (passthrough).
pub(crate) struct SpyPrefixRouting {
    pub(crate) seen: std::sync::Mutex<Vec<Vec<faktor_router::stability::TurnPrefix>>>,
}

impl crate::RoutingPolicy for SpyPrefixRouting {
    fn route(
        &self,
        _req: &faktor_router::RouteRequest,
    ) -> Result<RouteDecision, crate::RouteFailure> {
        Ok(crate::empty_passthrough_decision())
    }

    fn mode(&self) -> crate::RoutingMode {
        crate::RoutingMode::Economy
    }

    fn route_with_session_stability(
        &self,
        _req: &faktor_router::RouteRequest,
        prefix_history: Option<&[faktor_router::stability::TurnPrefix]>,
    ) -> Result<RouteDecision, crate::RouteFailure> {
        self.seen
            .lock()
            .unwrap()
            .push(prefix_history.map(|h| h.to_vec()).unwrap_or_default());
        Ok(crate::empty_passthrough_decision())
    }
}

/// A scripted provider reporting one cache-read count per completed call
/// (audit 102): turn 1 = 0, turn 2 = N, turn 3 = 0 (prefix rewritten),
/// turn 4 = 0.
#[derive(Clone)]
pub(crate) struct CacheReportingProvider {
    pub(crate) reads: Arc<std::sync::Mutex<std::collections::VecDeque<u64>>>,
    pub(crate) streams: Arc<std::sync::atomic::AtomicUsize>,
}

impl CacheReportingProvider {
    pub(crate) fn new(reads: Vec<u64>) -> Arc<Self> {
        Arc::new(Self {
            reads: Arc::new(std::sync::Mutex::new(reads.into_iter().collect())),
            streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }
}

impl faktor_provider::Provider for CacheReportingProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            streaming: true,
            context: 1_000_000,
            ..Default::default()
        }
    }

    fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        let read = self.reads.lock().unwrap().pop_front().unwrap_or(0);
        self.streams
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(futures::stream::iter(vec![
            Ok(faktor_provider::ProviderChunk::Text { text: "ok".into() }),
            Ok(faktor_provider::ProviderChunk::Usage(
                faktor_provider::CanonicalUsage {
                    uncached_input_tokens: 1_000,
                    cache_read_tokens: read,
                    cache_write_tokens: 0,
                    output_tokens: 10,
                    reasoning_tokens: 0,
                    reported_cost: None,
                    request_id: None,
                },
            )),
            Ok(faktor_provider::ProviderChunk::Done),
        ]))
    }

    fn runtime_context_limit(&self, _model: &str) -> Option<usize> {
        Some(1_000_000)
    }
}

/// One observed route consult: the request dimensions, the durable
/// prefix history handed to the policy, and the production decision's
/// cost next to the cost an OBSERVED-CacheState-fed consult would price.
pub(crate) struct ObservedCacheConsult {
    pub(crate) context_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) production_cost: u64,
    pub(crate) enriched_cost: u64,
    pub(crate) history: Vec<faktor_router::stability::TurnPrefix>,
}

/// Spy policy (audit 102): records the exact `(request, TurnPrefix)`
/// route input AND both cost estimates — the production policy's (which
/// supplies no `CacheState`) and one fed from the observed cache state
/// carried by the route input's segment observations.
pub(crate) struct ObservingCachePolicy {
    pub(crate) inner: Arc<crate::EconomicRoutingPolicy>,
    pub(crate) service: Arc<faktor_router::RouterService>,
    pub(crate) seen: Arc<std::sync::Mutex<Vec<ObservedCacheConsult>>>,
}

impl RoutingPolicy for ObservingCachePolicy {
    fn route(&self, req: &faktor_router::RouteRequest) -> Result<RouteDecision, RouteFailure> {
        self.inner.route(req)
    }

    fn mode(&self) -> RoutingMode {
        self.inner.mode()
    }

    fn route_with_session_stability(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[faktor_router::stability::TurnPrefix]>,
    ) -> Result<RouteDecision, RouteFailure> {
        let history: Vec<faktor_router::stability::TurnPrefix> =
            prefix_history.map(|h| h.to_vec()).unwrap_or_default();
        let production = self
            .inner
            .route_with_session_stability(req, prefix_history)?;
        // The observed cache state the route input carries: the last
        // durable segment observation's reported cache reads.
        let cache: Vec<faktor_router::CacheState> = history
            .last()
            .and_then(|t| t.segments.as_ref())
            .map(|seg| {
                vec![faktor_router::CacheState {
                    provider: "fake".into(),
                    model: "m".into(),
                    cached_input_tokens: seg.cache_read_tokens.min(req.context_tokens),
                    will_write_tokens: 0,
                }]
            })
            .unwrap_or_default();
        let enriched = self
            .service
            .route_with_prefix_stability(
                req,
                &cache,
                faktor_router::stability::DEFAULT_STABILITY_FLOOR,
                prefix_history,
            )
            .map_err(|_| RouteFailure::PolicyDenied)?;
        self.seen.lock().unwrap().push(ObservedCacheConsult {
            context_tokens: req.context_tokens,
            output_tokens: req.estimated_output_tokens,
            production_cost: production.estimated_cost_micro,
            enriched_cost: enriched.estimated_cost_micro,
            history,
        });
        Ok(production)
    }
}

pub(crate) fn cache_route_candidate() -> faktor_core::model::ModelDescriptor {
    faktor_core::model::ModelDescriptor {
        provider: "fake".into(),
        model: "m".into(),
        context: 1_000_000,
        max_output: 64_000,
        tools: true,
        parallel_tools: true,
        reasoning: true,
        thinking: true,
        vision: false,
        structured_output: true,
        embeddings: false,
        streaming: true,
        economics: faktor_core::model::ModelEconomics {
            input_price_per_mtok: faktor_core::model::MicroUsdPerToken::from(10),
            output_price_per_mtok: faktor_core::model::MicroUsdPerToken::from(30),
            cache_read_price_per_mtok: faktor_core::model::MicroUsdPerToken::from(1),
            cache_write_price_per_mtok: faktor_core::model::MicroUsdPerToken::from(5),
            estimated_latency_ms: 100,
            tool_reliability: 90,
            reasoning_reliability: 90,
            coding_reliability: 90,
            context_reliability: 90,
            availability: 100,
            rate_limit_state: faktor_core::model::RateLimitState::Healthy,
        },
        source: faktor_core::model::ModelSource::ProviderCatalog,
    }
}

// ============================================================ audit
// round 15: structured-diff review (P0-12/80) + independent review
// model (P0-13). The completion path replaced the head-only collector
// with the checkpoint/CAS diff package; risky changes get a REAL
// separate review-model call through the routing policy (phase Review).

/// A checkpoint-recording write tool: whole-file replace that records
/// before/after rows exactly like the daemon's write_file (existing
/// file -> before_write + after_write; missing file -> an
/// existence-bearing Added row via record_change). Feed the review's
/// diff base.
pub(crate) fn checkpoint_write_tool() -> Tool {
    Tool {
        name: "write_file".into(),
        description: "checkpoint-recording write".into(),
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
                let Some(snaps) = &ctx.snapshots else {
                    return Err(Error::internal("no checkpoint store wired"));
                };
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                let rel = std::path::Path::new(path);
                if let Some(parent) = rel.parent() {
                    if !parent.as_os_str().is_empty() {
                        if let Ok(resolved) = ws.resolve(parent) {
                            let _ = std::fs::create_dir_all(&resolved);
                        }
                    }
                }
                let current = ws.read(rel, 16 * 1024 * 1024).ok();
                match current {
                    Some(data) => {
                        if data.bytes == content.as_bytes() {
                            return Ok(ToolOutcome {
                                text: format!("{path} unchanged"),
                                exit_code: Some(0),
                                ..Default::default()
                            });
                        }
                        let before = snaps.before_write(ctx.session_id, path, &data.bytes)?;
                        ws.write_atomic(rel, content.as_bytes())
                            .map_err(|e| Error::internal(format!("write {path}: {e}")))?;
                        let (_, after) = ws
                            .hash_file_streaming(rel, None)
                            .map_err(|e| Error::internal(format!("hash {path}: {e}")))?;
                        snaps.after_write(
                            ctx.session_id,
                            path,
                            before,
                            after,
                            0,
                            content.as_bytes(),
                        )?;
                    }
                    None => {
                        ws.write_atomic(rel, content.as_bytes())
                            .map_err(|e| Error::internal(format!("write {path}: {e}")))?;
                        let (_, after) = ws
                            .hash_file_streaming(rel, None)
                            .map_err(|e| Error::internal(format!("hash {path}: {e}")))?;
                        if let Err(e) = snaps.record_change(
                            ctx.session_id,
                            path,
                            faktor_snapshot::FileState::missing(),
                            None,
                            faktor_snapshot::FileState::existing(after),
                            Some(content.as_bytes()),
                        ) {
                            eprintln!("CHECKPOINT record_change failed for {path}: {e}");
                            return Err(Error::internal(format!("checkpoint {path}: {e}")));
                        }
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

/// A checkpoint-recording delete tool (the shape a future daemon
/// delete_file records: an existence-bearing Deleted row).
pub(crate) fn checkpoint_delete_tool() -> Tool {
    Tool {
        name: "delete_file".into(),
        description: "checkpoint-recording delete".into(),
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
                let Some(snaps) = &ctx.snapshots else {
                    return Err(Error::internal("no checkpoint store wired"));
                };
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let rel = std::path::Path::new(path);
                let current = match ws.read(rel, 16 * 1024 * 1024) {
                    Ok(data) => data,
                    Err(_) => {
                        // Nothing to delete: idempotent success.
                        return Ok(ToolOutcome {
                            text: format!("{path} already gone"),
                            exit_code: Some(0),
                            ..Default::default()
                        });
                    }
                };
                let before = snaps.before_write(ctx.session_id, path, &current.bytes)?;
                let resolved = ws
                    .resolve(rel)
                    .map_err(|e| Error::internal(format!("resolve {path}: {e}")))?;
                std::fs::remove_file(&resolved)
                    .map_err(|e| Error::internal(format!("delete {path}: {e}")))?;
                snaps.record_change(
                    ctx.session_id,
                    path,
                    faktor_snapshot::FileState::existing(before),
                    None,
                    faktor_snapshot::FileState::missing(),
                    None,
                )?;
                Ok(ToolOutcome {
                    text: format!("deleted {path}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// Provider wrapper that records every request it streams (isolation +
/// call-count assertions).
pub(crate) struct RecordingProvider {
    pub(crate) inner: Arc<dyn faktor_provider::Provider>,
    pub(crate) log: Arc<std::sync::Mutex<Vec<faktor_provider::GenericAgentRequest>>>,
}

impl RecordingProvider {
    pub(crate) fn new(inner: Arc<dyn faktor_provider::Provider>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            log: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }
    pub(crate) fn requests(&self) -> Vec<faktor_provider::GenericAgentRequest> {
        self.log.lock().unwrap().clone()
    }
}

impl faktor_provider::Provider for RecordingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }
    fn stream(&self, req: faktor_provider::GenericAgentRequest) -> faktor_provider::ProviderStream {
        self.log.lock().unwrap().push(req.clone());
        self.inner.stream(req)
    }
}

pub(crate) fn rendered_request(req: &faktor_provider::GenericAgentRequest) -> String {
    let mut out = format!("SYSTEM<<{}>>", req.system);
    for m in &req.messages {
        for part in &m.content {
            if let ContentKind::Text { text } = &part.kind {
                out.push('\n');
                out.push_str(text);
            }
        }
    }
    out
}

/// Phase-pinned routing for tests: the Review phase routes to the mock
/// review provider; every other phase keeps the session defaults
/// (passthrough). Counts Review-phase route calls.
pub(crate) struct PhasePinnedRouting {
    pub(crate) decision: RouteDecision,
    pub(crate) review_route_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl PhasePinnedRouting {
    pub(crate) fn review_to(
        provider: &str,
        model: &str,
    ) -> (Arc<dyn RoutingPolicy>, Arc<std::sync::atomic::AtomicUsize>) {
        let mut decision = empty_passthrough_decision();
        decision.provider = provider.into();
        decision.model = model.into();
        decision.reasoning = "test: review phase pinned to the mock reviewer".into();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Arc::new(Self {
                decision,
                review_route_calls: calls.clone(),
            }),
            calls,
        )
    }
}

impl RoutingPolicy for PhasePinnedRouting {
    fn route(&self, req: &faktor_router::RouteRequest) -> Result<RouteDecision, RouteFailure> {
        if req.phase == RouterPhase::Review {
            self.review_route_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(self.decision.clone());
        }
        Ok(empty_passthrough_decision())
    }
    fn mode(&self) -> RoutingMode {
        RoutingMode::Economy
    }
}

pub(crate) fn mock_review_provider(verdict_json: &str) -> Arc<dyn faktor_provider::Provider> {
    Arc::new(FakeProvider::with_script(
        "reviewmock",
        ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        vec![
            ScriptedResponse::Text(verdict_json.into()),
            ScriptedResponse::End,
        ],
    ))
}

/// A shared Rust workspace whose store/cas also back a CheckpointStore,
/// so checkpoint-recording tools feed the review's diff base.
pub(crate) fn snapshot_review_env(
    seeds: &[(&str, &str)],
) -> (
    Arc<SessionManager>,
    SessionId,
    Arc<faktor_cas::Cas>,
    Arc<faktor_snapshot::CheckpointStore>,
    tempfile::TempDir,
) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    for (path, content) in seeds {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    let snapshots = Arc::new(faktor_snapshot::CheckpointStore::new(
        cas.clone(),
        manager.store(),
    ));
    let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws, "gating task", "fake", "m")
        .unwrap()
        .id();
    (manager, session, cas, snapshots, dir)
}

/// One turn's deps on a shared snapshot-backed env: multiple providers
/// (drive fake + optional mock reviewer), the given tools and routing.
pub(crate) fn snapshot_review_deps(
    manager: &Arc<SessionManager>,
    snapshots: &Arc<faktor_snapshot::CheckpointStore>,
    cas: &Arc<faktor_cas::Cas>,
    providers: Vec<Arc<dyn faktor_provider::Provider>>,
    tools: Vec<Tool>,
    routing: Arc<dyn RoutingPolicy>,
) -> (AgentDeps, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let mut registry = ProviderRegistry::new();
    for p in providers {
        registry.try_register(p).unwrap();
    }
    let mut tool_registry = ToolRegistry::new();
    for t in tools {
        tool_registry.register(t);
    }
    let deps = AgentDeps {
        session: manager.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tool_registry),
        cas: Some(cas.clone()),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: Some(snapshots.clone()),
        sandbox: None,
        supervisor: None,
        verification: fake_ok(),
        hooks: None,
        instructions_resolver: test_resolver(manager),
        routing,
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 1.0,
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

pub(crate) fn review_evidence_structured(review: &serde_json::Value) -> serde_json::Value {
    review["evidence"]["structured"].clone()
}

/// A write tool that deliberately records NO checkpoint row: the audit
/// 16 drift fixture needs the newest durable expectation to come from
/// somewhere else than this write.
pub(crate) fn uncheckpointed_write_tool() -> Tool {
    Tool {
        name: "write_file".into(),
        description: "write without a checkpoint".into(),
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
                if let Some(parent) = std::path::Path::new(path).parent() {
                    if !parent.as_os_str().is_empty() {
                        if let Ok(resolved) = ws.resolve(parent) {
                            let _ = std::fs::create_dir_all(&resolved);
                        }
                    }
                }
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

/// Run one high-risk turn on a fresh review env whose write to
/// `src/auth.rs` is deliberately NOT check-pointed; `seed_expectation`
/// decides whether a durable edit expectation exists first.
pub(crate) async fn run_high_risk_drift_scenario(seed_expectation: bool) -> serde_json::Value {
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[]);
    if seed_expectation {
        // An out-of-band durable expectation that the disk will NOT
        // satisfy (the file lands with different bytes).
        let expected = faktor_core::hash::FileHash::from(blake3::hash(b"expected bytes").into());
        snapshots
            .record_change(
                session,
                "src/auth.rs",
                faktor_snapshot::FileState::missing(),
                None,
                faktor_snapshot::FileState::existing(expected),
                Some(b"expected bytes"),
            )
            .unwrap();
    }
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/auth.rs",
                "content": "pub fn authenticate() -> bool { false }\n",
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let (routing, _calls) = PhasePinnedRouting::review_to("reviewmock", "rev");
    let (deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![
            Arc::new(scripted_provider(script)),
            mock_review_provider(r#"{"verdict":"clean","findings":[]}"#),
        ],
        vec![uncheckpointed_write_tool()],
        routing,
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "harden the auth path", &[])
        .await
        .unwrap();
    outcome.review.expect("a risky change must run the review")
}

/// Routes Implement (and every non-Review phase) as the passthrough
/// session side, but REFUSES the Review-phase call typed — the
/// fail-closed-block shape of the fallback-deletion test.
pub(crate) struct ReviewOnlyRefusingPolicy;

impl crate::RoutingPolicy for ReviewOnlyRefusingPolicy {
    fn route(
        &self,
        req: &faktor_router::RouteRequest,
    ) -> Result<RouteDecision, crate::RouteFailure> {
        if req.phase == RouterPhase::Review {
            return Err(crate::RouteFailure::NoCapableModel);
        }
        Ok(empty_passthrough_decision())
    }

    fn mode(&self) -> crate::RoutingMode {
        crate::RoutingMode::Economy
    }
}

// ============================================================
// wave-B4 closure: attempt-keyed provider-call rows at every
// settle/uncertain site (attempt-accounting reconciliation has exactly
// the rows to join) + verified-outcome signal feeds at the deterministic
// gate sites (audit items 13/14/L).
// ============================================================

/// Provider whose FIRST stream fails before any content with a
/// retryable network error, then serves the canonical cache-split frame
/// (400 uncached + 600 cache reads + 50 output — openai-style) and ends
/// cleanly. Captures every wire request meta it saw.
#[derive(Default)]
pub(crate) struct RetryOnceThenCacheSplitProvider {
    pub(crate) calls: std::sync::atomic::AtomicUsize,
    pub(crate) seen: std::sync::Mutex<Vec<(OpId, u32)>>,
}

impl RetryOnceThenCacheSplitProvider {
    pub(crate) fn requests(&self) -> Vec<(OpId, u32)> {
        self.seen.lock().unwrap().clone()
    }
}

impl faktor_provider::Provider for RetryOnceThenCacheSplitProvider {
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
        self.seen
            .lock()
            .unwrap()
            .push((req.meta.operation_id, req.meta.attempt));
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Box::pin(futures::stream::iter(vec![Err(ProviderError {
                kind: ProviderErrorKind::Network,
                message: "first attempt fails before any content".into(),
                retryable: true,
                code: None,
            })]));
        }
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderChunk::Text {
                text: "answer".into(),
            }),
            Ok(ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 400,
                cache_read_tokens: 600,
                cache_write_tokens: 0,
                output_tokens: 50,
                reasoning_tokens: 0,
                reported_cost: None,
                request_id: Some("req-attempt-keyed-b4".into()),
            })),
            Ok(ProviderChunk::Done),
        ]))
    }
}

/// The routed settlement env with a configurable retry policy.
pub(crate) async fn routed_settlement_runtime_with_retries(
    provider: Arc<dyn faktor_provider::Provider>,
    retry_policy: faktor_core::retry::RetryPolicy,
) -> (
    Arc<AgentRuntime>,
    SessionId,
    Arc<faktor_session::DurableBudgetLedger>,
) {
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider.clone()).unwrap();
    let (mut adeps, _dir) = deps_with(provider, vec![]);
    adeps.retry_policy = retry_policy;
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
            goal: "routed retry".into(),
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

/// One economic policy whose RouterService carries `outcomes`, wired
/// like the daemon graph: candidates = the session's fake/m priced at
/// $1/$1 per million, Implement-phase consult only.
pub(crate) fn outcome_wired_policy(
    outcomes: Arc<dyn faktor_router::OutcomeStore>,
) -> Arc<dyn crate::RoutingPolicy> {
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
    crate::EconomicRoutingPolicy::new(
        Arc::new(faktor_router::RouterService::with_pricing_and_outcomes(
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
                        cache_read: faktor_core::model::MicroUsdPerMillionTokens::ZERO,
                        cache_write: faktor_core::model::MicroUsdPerMillionTokens::ZERO,
                    },
                    1,
                    "fake".to_string(),
                ),
            )]),
            outcomes,
        )),
        crate::RoutingMode::Economy,
    )
}

/// snapshot_review_deps with a caller-chosen verification service and a
/// caller-chosen routing policy (the gate-feed tests need a scripted
/// FAILING verification and an outcome-wired economic policy).
pub(crate) fn snapshot_review_deps_full(
    manager: &Arc<SessionManager>,
    snapshots: &Arc<faktor_snapshot::CheckpointStore>,
    cas: &Arc<faktor_cas::Cas>,
    providers: Vec<Arc<dyn faktor_provider::Provider>>,
    tools: Vec<Tool>,
    routing: Arc<dyn RoutingPolicy>,
    verification: Arc<crate::VerificationService>,
) -> (AgentDeps, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let mut registry = ProviderRegistry::new();
    for p in providers {
        registry.try_register(p).unwrap();
    }
    let mut tool_registry = ToolRegistry::new();
    for t in tools {
        tool_registry.register(t);
    }
    let deps = AgentDeps {
        session: manager.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tool_registry),
        cas: Some(cas.clone()),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: Some(snapshots.clone()),
        sandbox: None,
        supervisor: None,
        verification,
        hooks: None,
        instructions_resolver: test_resolver(manager),
        routing,
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 1.0,
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

// ----------------- semantic-provider wiring (audits 54/58/77/79/118/119)

use faktor_semantic::{
    AffectedSet, GenericSemanticFallback, SemanticEnvelope, SemanticError, SemanticProvider,
    SemanticProviderId, SemanticProviderRegistry,
};

/// A scripted semantic provider: one `affected` answer per kind.
#[derive(Clone)]
pub(crate) enum FakeSemanticKind {
    /// `paths.len()` affected entities (drives blast radius).
    Affected(Vec<String>),
    /// Degraded answer (never trustworthy — conservative Unknown).
    Degraded,
    /// Hostile instruction prose in the entity ID (must stay DATA).
    Hostile(&'static str),
    /// Panics inside the provider future (guard must catch it).
    Panic,
}

#[derive(Clone)]
pub(crate) struct FakeSemanticProvider {
    pub(crate) id: &'static str,
    pub(crate) capabilities: faktor_semantic::SemanticCapabilities,
    pub(crate) kind: FakeSemanticKind,
}

impl FakeSemanticProvider {
    pub(crate) fn affected(paths: Vec<String>) -> Self {
        Self {
            id: "fake-semantic",
            capabilities: faktor_semantic::SemanticCapabilities::AFFECTED,
            kind: FakeSemanticKind::Affected(paths),
        }
    }

    pub(crate) fn with_kind(kind: FakeSemanticKind) -> Self {
        Self {
            id: "fake-semantic",
            capabilities: faktor_semantic::SemanticCapabilities::AFFECTED,
            kind,
        }
    }
}

impl SemanticProvider for FakeSemanticProvider {
    fn id(&self) -> SemanticProviderId {
        SemanticProviderId::parse(self.id).unwrap()
    }

    fn version(&self) -> u32 {
        7
    }

    fn capabilities(&self) -> faktor_semantic::SemanticCapabilities {
        self.capabilities
    }

    fn affected(
        &self,
        request: faktor_semantic::AffectedRequest,
    ) -> faktor_semantic::BoxFuture<'_, Result<SemanticEnvelope<AffectedSet>, SemanticError>> {
        let workspace = request.workspace;
        let snapshot_id = request.snapshot_id;
        let provider = SemanticProviderId::parse(self.id).unwrap();
        let kind = self.kind.clone();
        Box::pin(async move {
            let payload = match kind {
                FakeSemanticKind::Affected(paths) => {
                    let affected = paths
                        .iter()
                        .map(|p| {
                            faktor_semantic::SemanticEntityRef::new(
                                workspace,
                                faktor_semantic::WorkspacePath::parse(p).expect("test path"),
                                faktor_semantic::SemanticEntityId::parse(p).expect("test id"),
                            )
                        })
                        .collect();
                    AffectedSet {
                        affected,
                        tests: vec![],
                        degraded: false,
                    }
                }
                FakeSemanticKind::Degraded => AffectedSet {
                    affected: vec![],
                    tests: vec![],
                    degraded: true,
                },
                FakeSemanticKind::Hostile(text) => AffectedSet {
                    affected: vec![faktor_semantic::SemanticEntityRef::new(
                        workspace,
                        faktor_semantic::WorkspacePath::parse("src/evil.rs").unwrap(),
                        faktor_semantic::SemanticEntityId::parse(text).unwrap(),
                    )],
                    tests: vec![],
                    degraded: false,
                },
                FakeSemanticKind::Panic => panic!("hostile semantic provider panicked"),
            };
            Ok(SemanticEnvelope::new(
                provider,
                7,
                workspace,
                snapshot_id,
                0,
                payload,
            ))
        })
    }
}

pub(crate) fn semantic_registry_with(
    provider: FakeSemanticProvider,
) -> Arc<SemanticProviderRegistry> {
    let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
    registry.register(Arc::new(provider));
    Arc::new(registry)
}

/// High-blast-radius affected set: 25 valid entities >= High.
pub(crate) fn high_risk_paths() -> Vec<String> {
    (0..25).map(|i| format!("src/change_{i:02}.rs")).collect()
}

#[derive(Clone)]
pub(crate) struct ReviewSpyRouting {
    pub(crate) requests: Arc<std::sync::Mutex<Vec<faktor_router::RouteRequest>>>,
    pub(crate) decision: RouteDecision,
}

impl ReviewSpyRouting {
    pub(crate) fn pinned(provider: &str, model: &str) -> Self {
        let mut decision = empty_passthrough_decision();
        decision.provider = provider.into();
        decision.model = model.into();
        Self {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            decision,
        }
    }
}

impl RoutingPolicy for ReviewSpyRouting {
    fn route(&self, req: &faktor_router::RouteRequest) -> Result<RouteDecision, RouteFailure> {
        self.requests.lock().unwrap().push(req.clone());
        if req.phase == RouterPhase::Review {
            return Ok(self.decision.clone());
        }
        Ok(empty_passthrough_decision())
    }

    fn mode(&self) -> RoutingMode {
        RoutingMode::Economy
    }
}

// ==================================================================
// Production tripwires (efficiency audit): the ModelCallIntent route
// conversion, the tagged DB read pool, and the typed child handoff.
// ==================================================================

/// The production-recording router: every `RouteRequest` the runtime
/// converts from a `ModelCallIntent` is captured; the decision stays the
/// documented passthrough (session-configured side) unless a phase pin
/// was configured.
pub(crate) struct RecordingRouter {
    pub(crate) seen: Arc<std::sync::Mutex<Vec<faktor_router::RouteRequest>>>,
    pub(crate) pin: Option<(RouterPhase, RouteDecision)>,
}

impl RecordingRouter {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            pin: None,
        })
    }

    pub(crate) fn pin_review(provider: &str, model: &str) -> Arc<Self> {
        let mut decision = empty_passthrough_decision();
        decision.provider = provider.into();
        decision.model = model.into();
        decision.reasoning = "test: review pinned with a recording router".into();
        Arc::new(Self {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            pin: Some((RouterPhase::Review, decision)),
        })
    }

    pub(crate) fn requests(&self) -> Vec<faktor_router::RouteRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl RoutingPolicy for RecordingRouter {
    fn route(&self, req: &faktor_router::RouteRequest) -> Result<RouteDecision, RouteFailure> {
        self.seen.lock().unwrap().push(req.clone());
        if let Some((phase, decision)) = &self.pin {
            if req.phase == *phase {
                return Ok(decision.clone());
            }
        }
        Ok(empty_passthrough_decision())
    }

    fn mode(&self) -> RoutingMode {
        RoutingMode::Economy
    }
}

/// Test board authority: resolves the calling session and delegates to
/// the REAL session board API (same shape the daemon injects).
pub(crate) struct TestBoardGateway(pub(crate) Arc<SessionManager>);

impl crate::BoardToolGateway for TestBoardGateway {
    fn board_post(
        &self,
        session: SessionId,
        subject: &str,
        body: &str,
        refs: &[String],
    ) -> Result<serde_json::Value, Error> {
        let handle = self
            .0
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("board session {session}")))?;
        Ok(serde_json::to_value(handle.board_post(subject, body, refs)?).unwrap())
    }

    fn board_read(
        &self,
        session: SessionId,
        since_revision: Option<u64>,
        limit: usize,
        exclude_self: bool,
    ) -> Result<serde_json::Value, Error> {
        let handle = self
            .0
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("board session {session}")))?;
        Ok(serde_json::to_value(handle.board_read_posts(
            None,
            since_revision,
            limit,
            exclude_self,
        )?)
        .unwrap())
    }
}

// ------------------------------------------------ provider-attempt debits
//
// The Wave 5 residual's agent-side call site: a managed attempt opens a
// durable credit hold BEFORE the provider stream (record-before-call), a
// settled call consumes it at the reservation ledger's actual, a
// pre-dispatch failure refunds it, a BYOK model never debits, and a
// daemon without an installed authority is byte-identical (no calls).
// The ordering proof is the shared event log: the provider's own
// `stream()` invocation records itself into the SAME recorder the debit
// authority writes to.

use crate::credits::{
    DebitDecision, DebitError, DebitHold, ProviderAttemptDebit, ProviderAttemptDebits,
};

pub(crate) use faktor_session::BudgetError;

pub(crate) use std::pin::Pin;

/// Recording debit authority. The event log is the ordering proof:
/// `begin:<id>:<estimate>` / `stream` / `settle:<actual>` /
/// `refund:<reason>` appear in real call order.
pub(crate) struct DebitRecorder {
    pub(crate) managed: bool,
    pub(crate) refuse_begin: Option<String>,
    pub(crate) events: std::sync::Mutex<Vec<String>>,
    pub(crate) begins: std::sync::atomic::AtomicUsize,
    pub(crate) settles: std::sync::Mutex<Vec<u64>>,
    pub(crate) refunds: std::sync::Mutex<Vec<String>>,
}

impl DebitRecorder {
    pub(crate) fn new(managed: bool) -> Arc<Self> {
        Arc::new(Self {
            managed,
            refuse_begin: None,
            events: std::sync::Mutex::new(Vec::new()),
            begins: std::sync::atomic::AtomicUsize::new(0),
            settles: std::sync::Mutex::new(Vec::new()),
            refunds: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn refusing(reason: &str) -> Arc<Self> {
        Arc::new(Self {
            managed: true,
            refuse_begin: Some(reason.to_string()),
            events: std::sync::Mutex::new(Vec::new()),
            begins: std::sync::atomic::AtomicUsize::new(0),
            settles: std::sync::Mutex::new(Vec::new()),
            refunds: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    pub(crate) fn push(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }
}

impl ProviderAttemptDebits for DebitRecorder {
    fn is_managed(&self, _provider: &str) -> bool {
        self.managed
    }

    fn begin(&self, attempt: &ProviderAttemptDebit) -> Result<DebitDecision, DebitError> {
        self.begins
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.push(format!(
            "begin:{}:{}",
            attempt.attempt_id, attempt.estimate_micro
        ));
        if let Some(reason) = &self.refuse_begin {
            return Err(DebitError::Refused {
                reason: reason.clone(),
            });
        }
        if self.managed {
            Ok(DebitDecision::Hold(
                DebitHold::new(
                    format!("hold:{}", attempt.attempt_id),
                    attempt.estimate_micro,
                )
                .expect("hold id"),
            ))
        } else {
            Ok(DebitDecision::Byok)
        }
    }

    fn settle(
        &self,
        _attempt: &ProviderAttemptDebit,
        _hold: &DebitHold,
        actual_micro: u64,
    ) -> Result<(), DebitError> {
        self.push(format!("settle:{actual_micro}"));
        self.settles.lock().unwrap().push(actual_micro);
        Ok(())
    }

    fn refund(
        &self,
        _attempt: &ProviderAttemptDebit,
        _hold: &DebitHold,
        reason: &str,
    ) -> Result<(), DebitError> {
        self.push(format!("refund:{reason}"));
        self.refunds.lock().unwrap().push(reason.to_string());
        Ok(())
    }
}

/// The reservation ledger double: `settle_actual` is the settlement
/// truth the debit hold must follow (`None` = documented Unknown spend);
/// `fail_dispatch_marker` models a durable dispatch-marker write failure
/// (the provider provably never contacted).
pub(crate) struct DebitBudget {
    pub(crate) settle_actual: Option<u64>,
    pub(crate) fail_dispatch_marker: bool,
    pub(crate) refunds: std::sync::atomic::AtomicUsize,
    pub(crate) uncertain: std::sync::atomic::AtomicUsize,
    pub(crate) settles: std::sync::atomic::AtomicUsize,
}

impl DebitBudget {
    pub(crate) fn new(settle_actual: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            settle_actual,
            fail_dispatch_marker: false,
            refunds: std::sync::atomic::AtomicUsize::new(0),
            uncertain: std::sync::atomic::AtomicUsize::new(0),
            settles: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn refusing_dispatch_marker(settle_actual: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            settle_actual,
            fail_dispatch_marker: true,
            refunds: std::sync::atomic::AtomicUsize::new(0),
            uncertain: std::sync::atomic::AtomicUsize::new(0),
            settles: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

pub(crate) type BudgetFut<T> =
    Pin<Box<dyn std::future::Future<Output = Result<T, BudgetError>> + Send>>;

impl BudgetAuthority for DebitBudget {
    fn reserve(
        &self,
        _s: SessionId,
        _t: TaskId,
        _op: faktor_core::id::OpId,
        _pred: u64,
        _snap: Option<faktor_core::model::PricingSnapshot>,
    ) -> BudgetFut<faktor_session::ReservationId> {
        Box::pin(async { Ok(faktor_session::ReservationId::NOOP) })
    }

    fn reserve_attempt(
        &self,
        _s: SessionId,
        _t: TaskId,
        _a: faktor_core::op::ModelCallAttempt,
        _pred: u64,
        _snap: Option<faktor_core::model::PricingSnapshot>,
    ) -> BudgetFut<faktor_session::ReservationId> {
        Box::pin(async { Ok(faktor_session::ReservationId::NOOP) })
    }

    fn mark_dispatched(&self, _s: SessionId, _r: faktor_session::ReservationId) -> BudgetFut<()> {
        let fail = self.fail_dispatch_marker;
        Box::pin(async move {
            if fail {
                return Err(BudgetError::Malformed(
                    "test: dispatch marker write failed".into(),
                ));
            }
            Ok(())
        })
    }

    fn mark_uncertain(
        &self,
        _s: SessionId,
        _r: faktor_session::ReservationId,
        _reason: String,
        _request_id: Option<String>,
    ) -> BudgetFut<()> {
        self.uncertain
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    ) -> BudgetFut<Option<u64>> {
        self.settles
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let actual = self.settle_actual;
        Box::pin(async move { Ok(actual) })
    }

    fn refund(&self, _s: SessionId, _r: faktor_session::ReservationId) -> BudgetFut<()> {
        self.refunds
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn session_budget_view(
        &self,
        _s: SessionId,
        _t: TaskId,
    ) -> Result<faktor_session::BudgetView, BudgetError> {
        Ok(faktor_session::BudgetView {
            max_cost_micro: None,
            spent_cost_micro: 0,
            open_reserved_micro: 0,
            open_reservations: 0,
            uncertain_reserved_micro: 0,
            uncertain_reservations: 0,
            settled_count: 0,
        })
    }

    fn recover_after_restart(&self) {}

    fn reconcile_uncertain(
        &self,
        _s: SessionId,
        _t: TaskId,
    ) -> BudgetFut<faktor_store::CostReconcileReport> {
        Box::pin(async { Ok(Default::default()) })
    }

    fn finalize_uncertain(
        &self,
        _s: SessionId,
        _t: TaskId,
    ) -> BudgetFut<faktor_store::CostFinalizeReport> {
        Box::pin(async { Ok(Default::default()) })
    }
}

/// The provider's own `stream()` invocation records itself into the
/// debit recorder, so the shared log proves the hold exists BEFORE the
/// request is built/dispatched.
pub(crate) fn debit_aware_provider(
    recorder: &Arc<DebitRecorder>,
) -> Arc<dyn faktor_provider::Provider> {
    let recorder = recorder.clone();
    Arc::new(InspectingProvider::new(
        Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                context: 200_000,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        )),
        move |_n: usize, _req: &GenericAgentRequest| -> Result<(), String> {
            recorder.push("stream");
            Ok(())
        },
    ))
}

pub(crate) async fn run_turn_with_debits(
    recorder: &Arc<DebitRecorder>,
    budget: Arc<DebitBudget>,
) -> (TurnOutcome, Arc<AgentRuntime>) {
    let (mut deps, _dir) = deps_with(debit_aware_provider(recorder), vec![]);
    deps.budgets = budget;
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime
        .set_provider_debits(Some(recorder.clone() as Arc<dyn ProviderAttemptDebits>))
        .expect("install debit authority");
    let (manager, session) = shared_session(runtime.deps());
    let _ = manager;
    let outcome = runtime
        .run_turn(session, "do the thing", &[])
        .await
        .expect("turn");
    (outcome, runtime)
}

// ---------------------------------------------- durable-write guard tests

/// The marker files of one store root, deterministic order.
pub(crate) fn marker_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let dir = root.join(DURABLE_WRITE_MARKER_DIR);
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

pub(crate) fn marker_sites(root: &std::path::Path) -> Vec<String> {
    marker_files(root)
        .iter()
        .map(|path| {
            let marker: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            marker
                .get("site")
                .and_then(|site| site.as_str())
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

pub(crate) fn marker_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Write one raw marker file (tests fabricate markers exactly like the
/// production writer does).
pub(crate) fn write_raw_marker(dir: &std::path::Path, name: &str, value: &serde_json::Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(name),
        serde_json::to_vec(value).expect("marker serializes"),
    )
    .unwrap();
}

pub(crate) fn reopen_manager(dir: &tempfile::TempDir) -> Arc<SessionManager> {
    SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap()
}
