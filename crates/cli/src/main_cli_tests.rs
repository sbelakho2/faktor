//! `main_cli_tests`: out-of-line slice of the CLI test module.

use super::*;

use faktor_core::model::ModelCapabilities;

use faktor_core::state::AgentState;

use faktor_provider::testing::{MockAction, MockServer};

use faktor_provider::{
    FakeProvider, GenericAgentRequest, ProviderChunk, ProviderError, ScriptedResponse,
};

use std::pin::Pin;

/// Test-only default-allow transport: every mock endpoint below is a
/// loopback server, and production construction injects the daemon's
/// policy-checked transport instead.
pub(crate) fn permissive_transport() -> Arc<dyn HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::permissive())
}

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
        efficiency: Default::default(),
    };
    AgentRuntime::new(deps).unwrap()
}

/// The daemon's ONE execution facade over a test graph: the same
/// construction the production entries use (one OrchestratorRuntime +
/// TaskExecutor over the same session/agent, wrapped once in the
/// PromptExecutionService every adapter calls).
pub(crate) fn test_prompts(
    session: &Arc<SessionManager>,
    agent: &Arc<AgentRuntime>,
) -> Arc<faktor_server::native::PromptExecutionService> {
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
    let tasks =
        faktor_orchestrator::runtime::task_executor::TaskExecutor::new_owner_direct_for_test_harness(
            &orchestrator,
            session.clone(),
            agent.clone(),
        );
    faktor_server::native::PromptExecutionService::new(tasks, session.clone())
}

/// Every model request's system prompt, captured for wire assertions.
pub(crate) struct CapturingProvider {
    pub(crate) inner: Arc<dyn Provider>,
    pub(crate) systems: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Provider for CapturingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }

    fn supports_embeddings(&self, model: &str) -> bool {
        self.inner.supports_embeddings(model)
    }

    fn embed(
        &self,
        req: faktor_provider::EmbeddingRequest,
    ) -> Result<faktor_provider::EmbeddingResponse, ProviderError> {
        self.inner.embed(req)
    }

    fn stream(&self, req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        self.systems.lock().unwrap().push(req.system.clone());
        self.inner.stream(req)
    }
}

/// A chat model that answers every stream identically (repeated evidence
/// turns must never run out of script).
pub(crate) struct AlwaysOk;

impl Provider for AlwaysOk {
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
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderChunk::Text { text: "ok".into() }),
            Ok(ProviderChunk::Done),
        ]))
    }
}

/// The real daemon AgentDeps with an injected evidence provider (the
/// semantic E2E needs `RepoEvidence` carrying a configured embedder).
pub(crate) fn test_agent_with_evidence(
    session: Arc<SessionManager>,
    registry: ProviderRegistry,
    evidence: Arc<dyn faktor_agent::EvidenceProvider>,
) -> Arc<AgentRuntime> {
    let deps = AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence,
        tools: Arc::new(ToolRegistry::new()),
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
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        efficiency: Default::default(),
    };
    AgentRuntime::new(deps).unwrap()
}

/// E2E (the required turn-level fence): an ORDINARY turn whose
/// `[embeddings]` section selects the Ollama provider resolves through
/// `Config::semantic_embedder` into the REAL `OllamaProvider` talking
/// `/api/embed`, and the captured model request fuses the
/// semantically-matched evidence (a file no lexical/symbol/exact search
/// can find for "quantum zebra"). The SAME turn with no `[embeddings]`
/// section stays neutral: zero embed calls and no evidence block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_ollama_embedder_fuses_semantic_evidence_into_an_ordinary_turn() {
    async fn captured_turn(configured: bool) -> (String, usize) {
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
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();

        // The Ollama mock: every `/api/embed` call answers keyword-axis
        // vectors in request order — query calls then the two-candidate
        // call (twice: once per concept).
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/embed",
            MockAction::Sequence {
                actions: vec![
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"embeddings":[[1.0,0.0]]}"#.into(),
                    },
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"embeddings":[[1.0,0.0],[0.0,1.0]]}"#.into(),
                    },
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"embeddings":[[0.0,1.0]]}"#.into(),
                    },
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"embeddings":[[1.0,0.0],[0.0,1.0]]}"#.into(),
                    },
                ],
            },
        );
        let base = server.base_url().await;
        let ollama = faktor_ollama::OllamaProvider::new(
            faktor_ollama::OllamaConfig::new(Some(base)),
            permissive_transport(),
        );

        let systems = Arc::new(std::sync::Mutex::new(Vec::new()));
        let chat = Arc::new(CapturingProvider {
            inner: Arc::new(AlwaysOk),
            systems: systems.clone(),
        });
        let mut registry = ProviderRegistry::new();
        registry
            .try_register(faktor_provider::InstanceProvider::wrap(
                chat as Arc<dyn Provider>,
                "fake",
            ))
            .unwrap();
        registry.try_register(ollama as Arc<dyn Provider>).unwrap();

        let mut config = crate::config::Config::default();
        if configured {
            config.embeddings = Some(crate::config::EmbeddingCfg {
                provider: "ollama".into(),
                model: "nomic-embed-text".into(),
                policy: crate::config::EmbeddingPolicy::BestEffort,
            });
        }
        let embedder = config
            .semantic_embedder(&registry, &faktor_core::retry::RetryPolicy::default())
            .unwrap();
        assert_eq!(
            embedder.is_some(),
            configured,
            "the section must resolve exactly when configured"
        );
        // Index generation BUILDS would drive the same provider with a
        // two-input chunk batch (`parse_expr` + `reconcile_accounts`),
        // racing the scripted one-input query calls below. Wrap the
        // resolved embedder so only query-time retrieval uses it: this
        // E2E is about semantic fusion into an ordinary turn, and the
        // script then has the deterministic one-input/one-candidate
        // cadence per concept. Persisted-vector retrieval is covered by
        // the search and evidence crate tests.
        struct QueryOnlyEmbedder(Arc<dyn faktor_search::Embedder>);
        impl faktor_search::Embedder for QueryOnlyEmbedder {
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                faktor_search::Embedder::embed(self.0.as_ref(), texts)
            }
            fn try_embed(
                &self,
                texts: &[String],
            ) -> Result<Vec<Vec<f32>>, faktor_core::error::Error> {
                faktor_search::Embedder::try_embed(self.0.as_ref(), texts)
            }
        }
        let embedder: Option<Arc<dyn faktor_search::Embedder>> =
            embedder.map(|e| Arc::new(QueryOnlyEmbedder(e)) as Arc<dyn faktor_search::Embedder>);

        let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
        let sid = session.create_session(ws, "idx", "fake", "m").unwrap().id();
        let evidence = Arc::new(RepoEvidence::new(session.clone(), embedder));
        let runtime = test_agent_with_evidence(session.clone(), registry, evidence);
        // Ordinary turns only: the runtime hosts its own IndexService
        // lazily, so the first turns legitimately serve while the
        // generation builds; later turns serve the Ready generation.
        // Poll by running ordinary turns (never by reaching into the
        // private service), bounded.
        let mut last_system = String::new();
        let attempts = if configured { 120 } else { 6 };
        for attempt in 0..attempts {
            runtime.run_turn(sid, "quantum zebra", &[]).await.unwrap();
            last_system = systems.lock().unwrap().last().cloned().unwrap_or_default();
            if configured && last_system.contains("## Retrieved evidence") {
                break;
            }
            if attempt + 1 < attempts {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
        }
        (last_system, server.request_count())
    }

    let (system, embed_calls) = captured_turn(true).await;
    assert!(
        embed_calls >= 2,
        "the configured Ollama embedder must actually be called: {embed_calls}"
    );
    let evidence = system
        .split("## Retrieved evidence")
        .nth(1)
        .unwrap_or_default();
    assert!(
        evidence.contains("src/ledger.rs"),
        "the semantically-matched file must reach the captured request: {system}"
    );

    let (neutral, neutral_calls) = captured_turn(false).await;
    assert_eq!(neutral_calls, 0, "no embedder, no embed call");
    assert!(
        !neutral.contains("## Retrieved evidence"),
        "without an embedder no semantic evidence may be invented: {neutral}"
    );
}

/// A minimal REAL daemon over a temp data dir: one scripted provider
/// registered under the instance id "fake" (the single registered
/// instance, so the ACP session defaults resolve to it deterministically).
pub(crate) fn acp_test_daemon(
    script: Vec<ScriptedResponse>,
) -> (tempfile::TempDir, Arc<SessionManager>, Arc<AgentRuntime>) {
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
            script,
        )))
        .unwrap();
    let agent = test_agent(session.clone(), registry);
    (dir, session, agent)
}

pub(crate) fn handle(session: &Arc<SessionManager>, sid: &str) -> faktor_session::SessionHandle {
    session
        .get_session(SessionId::new(sid.parse().unwrap()))
        .unwrap()
        .unwrap()
}

#[test]
fn expand_home_and_relative() {
    assert_eq!(expand("."), PathBuf::from("."));
    let home = expand("~");
    assert_eq!(expand("~/x"), home.join("x"));
}

// ---- durable learning prior (audits 65-69/82) ----

/// A REAL session manager with one workspace/session but an EMPTY
/// durable learning corpus (no learning rows yet).
pub(crate) fn empty_learning_session() -> (
    tempfile::TempDir,
    Arc<SessionManager>,
    faktor_core::id::SessionId,
) {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let session = manager
        .create_session(ws, "learning", "fake", "m")
        .unwrap()
        .id();
    (dir, manager, session)
}

/// Mine ONE verified recovery into the session's durable ledger corpus
/// through the REAL `SessionLearningStore` adapter — the same durable
/// rows the runtime's mining hook appends. Returns the stored learning.
pub(crate) fn mine_durable_corpus(
    manager: &Arc<SessionManager>,
    session: faktor_core::id::SessionId,
) -> faktor_learning::ProjectLearning {
    use faktor_core::id::VerificationRecordId;
    use faktor_learning::{
        ActionDescriptor, ActionFingerprint, EnvironmentFingerprint, EpisodeId, FailureDescriptor,
        FailureEpisode, FailureFingerprint, LearningService, LearningStore as _, ProjectScope,
        SessionLearningStore, TaskClass, DEFAULT_MEMORY_CAPACITY,
    };

    let handle = manager.get_session(session).unwrap().unwrap();
    let scope = ProjectScope::new(handle.row().unwrap().workspace_id, "w").unwrap();
    let episode = FailureEpisode::new(
        EpisodeId::new(1),
        TaskClass::new("bugfix").unwrap(),
        EnvironmentFingerprint::new(scope, "linux", "rustc", None).unwrap(),
        ActionFingerprint::of(
            &ActionDescriptor::new("edit", "src/lib.rs", Some("parse"), "attempt").unwrap(),
        ),
        FailureFingerprint::of(
            &FailureDescriptor::new("test_failure", None, "assertion failed").unwrap(),
        ),
    )
    .with_recovery_actions(vec![ActionFingerprint::of(
        &ActionDescriptor::new("edit", "src/lib.rs", Some("parse"), "guard").unwrap(),
    )])
    .unwrap()
    .verified(VerificationRecordId::new(11));
    let store = SessionLearningStore::open(handle, DEFAULT_MEMORY_CAPACITY).unwrap();
    let mut service = LearningService::new(store);
    service.mine_and_store(&[episode]).unwrap();
    service.store().all()[0].clone()
}

pub(crate) fn keyed_candidate(id: &str, keys: Vec<String>) -> faktor_context::ContextCandidate {
    faktor_context::ContextCandidate {
        id: id.into(),
        omission_keys: keys,
        ..Default::default()
    }
}

/// Audit 68 production wiring: the flag alone decides whether the
/// durable session-manager-backed adapter is installed; with no learning
/// rows the corpus index is empty and every candidate is neutral (byte
/// parity with the flag-off path), keyed candidates included.
#[test]
fn failure_learning_prior_installs_only_on_flag_and_is_neutral_when_empty() {
    let (_dir, manager, _session) = empty_learning_session();
    assert!(
        daemon_context_prior(false, &manager).is_none(),
        "flag off => no prior handle"
    );
    let prior = daemon_context_prior(true, &manager).expect("flag on => learning adapter");
    for id in ["msg:0", "src/lib.rs", ""] {
        assert_eq!(
            prior.omission_risk(&keyed_candidate(id, Vec::new())),
            1.0,
            "empty corpus => neutral risk for {id:?}"
        );
    }
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec!["0".repeat(64)])),
        1.0,
        "empty corpus => a digest-shaped omission key is neutral too"
    );
}

/// The durable adapter over a REAL mined corpus: the candidate's
/// `omission_keys` (never its render id) resolve to the learning's
/// confidence-scaled risk (`1 + ppm/1e6`); empty/hostile/oversized keys
/// can neither panic nor leave `[1, 2]`; and a fresh manager over the
/// same data dir rebuilds the identical durable index (reopen-safe).
#[test]
fn durable_prior_resolves_omission_keys_ignores_render_ids_and_survives_reopen() {
    let (dir, manager, session) = empty_learning_session();
    let stored = mine_durable_corpus(&manager, session);
    assert_eq!(stored.confidence_ppm, 400_000, "one verified sample");
    let pattern = stored.pattern_digest().to_hex();
    let failure = stored.pattern.failure.digest().to_hex();

    let prior = daemon_context_prior(true, &manager).expect("flag on => durable prior");
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec![failure.clone()])),
        1.4
    );
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec![pattern.clone()])),
        1.4
    );
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec![failure.to_uppercase()],)),
        1.4,
        "hex parsing is case-insensitive"
    );
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", Vec::new())),
        1.0
    );
    // A render id that HAPPENS to be the digest is ignored: only
    // omission_keys are lookup keys.
    let id_only = faktor_context::ContextCandidate {
        id: failure.clone(),
        ..Default::default()
    };
    assert_eq!(
        prior.omission_risk(&id_only),
        1.0,
        "lookup must key omission_keys, never candidate.id"
    );
    // The max over several keys wins; hostile keys stay neutral.
    assert_eq!(
        prior.omission_risk(&keyed_candidate(
            "msg:0",
            vec!["0".repeat(64), failure.clone(), "src/lib.rs".into()],
        )),
        1.4
    );
    for key in [
        String::new(),
        "src/lib.rs".to_string(),
        "\0\u{1f600}".to_string(),
        "0".repeat(4096),
    ] {
        let risk = prior.omission_risk(&keyed_candidate("msg:0", vec![key.clone()]));
        assert!(
            risk.is_finite() && (1.0..=2.0).contains(&risk),
            "hostile key {key:?} produced {risk}"
        );
        assert_eq!(risk, 1.0, "{key:?}");
    }

    // Reopen-safety: a fresh manager over the same data dir rebuilds the
    // same durable index from the same ledger rows.
    drop(prior);
    drop(manager);
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let prior = daemon_context_prior(true, &reopened).expect("flag on");
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec![failure])),
        1.4
    );
}

/// The loop closes in-process (no restart): a prior built while the
/// corpus was empty re-reads the durable ledger when the stamp advances
/// and protects the freshly mined learning on the next lookup.
#[test]
fn durable_prior_refreshes_without_restart_when_the_corpus_is_mined() {
    let (_dir, manager, session) = empty_learning_session();
    let prior = daemon_context_prior(true, &manager).expect("flag on");
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec!["0".repeat(64)])),
        1.0
    );
    let stored = mine_durable_corpus(&manager, session);
    let failure = stored.pattern.failure.digest().to_hex();
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec![failure])),
        1.4,
        "the ledger stamp advanced; the index must re-read without a restart"
    );
}

/// Hostile/corrupt ledger rows are LOUD but non-fatal: the prior stays
/// installed, keeps serving, and leaves the affected corpus neutral —
/// including on a stamp-advancing refresh.
#[test]
fn corrupt_learning_row_is_loud_but_leaves_the_prior_neutral_and_total() {
    let (_dir, manager, session) = empty_learning_session();
    let handle = manager.get_session(session).unwrap().unwrap();
    handle
        .ledger_learning_record(faktor_session::LEARNING_RECORD_LEARNING, "not json")
        .unwrap();
    let prior = daemon_context_prior(true, &manager).expect("flag on => prior despite corrupt row");
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec!["a".repeat(64)])),
        1.0
    );
    // Another corrupt row advances the stamp: the refresh is still
    // non-fatal and still neutral.
    handle
        .ledger_learning_record(faktor_session::LEARNING_RECORD_LEARNING, "still not json")
        .unwrap();
    assert_eq!(
        prior.omission_risk(&keyed_candidate("msg:0", vec!["a".repeat(64)])),
        1.0
    );
}

/// The full production selection loop (audits 65-69/82): a durable mined
/// corpus -> the wire planner's `learning:<digest>` evidence exposes the
/// digest through `omission_keys` -> the CLI's durable prior protects it
/// enough to flip the single evidence slot, and the exact same corpus
/// survives a manager reopen.
#[test]
fn durable_prior_flips_planner_selection_and_survives_reopen() {
    use faktor_context::assembler::Evidence;
    use faktor_context::budget::ContextBudget;
    use faktor_context::information::FailurePrior;
    use faktor_context::ledger::TaskLedger;
    use faktor_context::wire_plan::WirePlan;
    use faktor_context::TokenCache;

    fn plan(
        evidence: &[Evidence],
        budget: &ContextBudget,
        ledger: &TaskLedger,
        cache: &TokenCache,
        prior: Option<&(dyn FailurePrior + Send + Sync)>,
    ) -> WirePlan {
        faktor_agent::wire_plan::plan_wire_turn_with_prior(
            "You are a test agent.\n",
            "",
            &[],
            "",
            ledger,
            "",
            &[],
            evidence,
            budget,
            "gpt-5",
            cache,
            prior,
        )
        .unwrap()
    }

    let (dir, manager, session) = empty_learning_session();
    let stored = mine_durable_corpus(&manager, session);
    let failure = stored.pattern.failure.digest().to_hex();

    // Two equal-priced evidence blocks compete for exactly one slot;
    // only the learning-sourced one carries an omission key.
    let evidence = vec![
        Evidence {
            path: format!("learning:{failure}"),
            snippet: "x".repeat(96),
            score: 0.5,
        },
        Evidence {
            path: "src/b.rs".into(),
            snippet: "x".repeat(400),
            score: 0.6,
        },
    ];
    let budget = ContextBudget {
        system: 130,
        tools: 0,
        working: 0,
        retrieved: 0,
        recent: 0,
        output_reserve: 0,
        safety: 0,
    };
    let ledger = TaskLedger::default();
    let cache = TokenCache::new();

    let off = plan(&evidence, &budget, &ledger, &cache, None);
    assert!(
        off.system.contains("### src/b.rs") && !off.system.contains("### learning:"),
        "baseline: the 0.6 block wins the single slot; system={}",
        off.system
    );

    let prior = daemon_context_prior(true, &manager).expect("flag on");
    let on = plan(&evidence, &budget, &ledger, &cache, Some(prior.as_ref()));
    assert!(
        on.system.contains(&format!("### learning:{failure}")),
        "the mined learning's omission risk must protect its candidate"
    );
    assert!(!on.system.contains("### src/b.rs"));
    assert_eq!(
        off.cacheable_prefix().unwrap(),
        on.cacheable_prefix().unwrap(),
        "the prior may only change volatile selection"
    );

    // Durable across reopen: the same corpus rebuilds the same prior.
    drop(prior);
    drop(manager);
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let prior = daemon_context_prior(true, &reopened).expect("flag on");
    let again = plan(&evidence, &budget, &ledger, &cache, Some(prior.as_ref()));
    assert!(again.system.contains(&format!("### learning:{failure}")));
}

/// The parsed `[efficiency]` switches thread 1:1 onto the agent-side
/// flags (`failure_learning` included); the additive default is all-off.
#[test]
fn efficiency_flags_mirror_every_parsed_switch() {
    let all = config::EfficiencyCfg {
        failure_learning: true,
        ccr: true,
        typed_handoff: true,
        semantic_context: true,
        rework_routing: true,
    };
    assert_eq!(
        efficiency_flags(&all),
        faktor_agent::EfficiencyFlags {
            failure_learning: true,
            ccr: true,
            typed_handoff: true,
            semantic_context: true,
            rework_routing: true,
        }
    );
    assert!(efficiency_flags(&all).failure_learning);
    assert_eq!(
        efficiency_flags(&config::EfficiencyCfg::default()),
        faktor_agent::EfficiencyFlags::default(),
        "additive default: every flag off"
    );
}

/// Every enabled-section database open goes through
/// `enabled_section_db_path`: a resolver `None` is a TYPED config error
/// (never an `unreachable!` startup abort, never a silently created
/// fallback database). The disabled section is the only resolver path
/// that returns `None`, for all four sections.
#[test]
fn enabled_sections_resolve_typed_errors_never_panic() {
    for section in ["cloud control-plane", "cloud scm", "billing", "workers"] {
        let err = enabled_section_db_path(section, None)
            .expect_err("an enabled section with no resolved path must refuse");
        assert!(err.starts_with(&format!("{section} config:")), "{err}");
        assert!(err.contains("resolved no database path"), "{err}");
        let path = std::path::PathBuf::from("/tmp/faktor-db");
        assert_eq!(
            enabled_section_db_path(section, Some(path.clone())).unwrap(),
            path
        );
    }
    // The resolvers' None branch is exactly the disabled section for
    // every one of the four configs the call sites guard.
    let dir = std::path::Path::new("/data");
    assert_eq!(
        config::CloudCfg::default().control_plane_path(dir).unwrap(),
        None
    );
    assert_eq!(config::CloudCfg::default().scm_path(dir).unwrap(), None);
    assert_eq!(
        config::BillingCfg::default().billing_path(dir).unwrap(),
        None
    );
    assert_eq!(
        config::WorkersCfg::default().workers_path(dir).unwrap(),
        None
    );
}

#[test]
fn semantic_section_is_additive_strict_and_empty_by_default() {
    // The `[semantic]` section (audits 48-54/58/79) is additive: absent
    // keeps the fallback-only registry; a present section parses under
    // the SAME strict document rules as every other config key.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("serve.json");
    let (_cfg, semantic) = serve_config_and_semantic(None).unwrap();
    assert_eq!(semantic, graph::SemanticCfg::default());
    std::fs::write(&path, r#"{"config_version": 1, "model": "m"}"#).unwrap();
    let (cfg, semantic) = serve_config_and_semantic(Some(path.clone())).unwrap();
    assert_eq!(cfg.model, "m");
    assert_eq!(semantic, graph::SemanticCfg::default(), "empty default");
    std::fs::write(
        &path,
        r#"{"config_version": 1, "model": "m", "semantic": {
                "max_payload_bytes": 2048, "max_entity_refs": 64
            }}"#,
    )
    .unwrap();
    let (cfg, semantic) = serve_config_and_semantic(Some(path.clone())).unwrap();
    assert_eq!(cfg.model, "m");
    assert_eq!(semantic.max_payload_bytes, Some(2048));
    assert_eq!(semantic.max_entity_refs, Some(64));
    let supervisor = ProcessSupervisor::new(Arc::new(faktor_cas::Cas::new(dir.path().join("cas"))));
    let transport: Arc<dyn HttpTransport> = permissive_transport();
    let registry = graph::semantic_registry(&semantic, &supervisor, &transport).unwrap();
    assert!(
        registry.providers().is_empty(),
        "caps-only section registers no provider"
    );
    // A configured external provider builds through the daemon's own
    // authorities (registration itself spawns nothing).
    std::fs::write(
            &path,
            r#"{"config_version": 1, "model": "m", "semantic": {"providers": [
                {"kind": "process", "id": "local-proc", "command": "/bin/true", "timeout_ms": 1000},
                {"kind": "http", "id": "remote", "endpoint": "http://provider.example/semantic", "timeout_ms": 1000}
            ]}}"#,
        )
        .unwrap();
    let (_cfg, semantic) = serve_config_and_semantic(Some(path.clone())).unwrap();
    let registry = graph::semantic_registry(&semantic, &supervisor, &transport).unwrap();
    let ids: Vec<String> = registry
        .providers()
        .iter()
        .map(|p| p.id().as_str().to_string())
        .collect();
    assert_eq!(ids, vec!["local-proc".to_string(), "remote".to_string()]);
    // Duplicate provider ids are refused at strict load.
    std::fs::write(
        &path,
        r#"{"model": "m", "semantic": {"providers": [
                {"kind": "process", "id": "dup", "command": "/bin/true", "timeout_ms": 1000},
                {"kind": "http", "id": "dup", "endpoint": "http://x", "timeout_ms": 1000}
            ]}}"#,
    )
    .unwrap();
    let e = serve_config_and_semantic(Some(path.clone())).expect_err("duplicate ids");
    assert!(e.contains("[semantic]") && e.contains("dup"), "{e}");
    // Hostile provider values (zero timeout, non-http endpoint) refuse
    // startup here, before any graph authority exists.
    for bad in [
        r#"{"model": "m", "semantic": {"providers": [
                {"kind": "process", "id": "p", "command": "/bin/true", "timeout_ms": 0}
            ]}}"#,
        r#"{"model": "m", "semantic": {"providers": [
                {"kind": "http", "id": "h", "endpoint": "ftp://x", "timeout_ms": 1000}
            ]}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        let e = serve_config_and_semantic(Some(path.clone())).expect_err("hostile provider");
        assert!(e.contains("[semantic]"), "{e}");
    }
    // Strict inside the section: a typo'd key fails startup.
    std::fs::write(&path, r#"{"model": "m", "semantic": {"surprise": true}}"#).unwrap();
    let e = serve_config_and_semantic(Some(path.clone())).expect_err("strict section");
    assert!(e.contains("[semantic]"), "{e}");
    // Unknown fields OUTSIDE the section still fail exactly as before.
    std::fs::write(&path, r#"{"model": "m", "surprise": 1}"#).unwrap();
    assert!(serve_config_and_semantic(Some(path)).is_err());
}

/// F10 (adversarial): a store read failure during the startup
/// verification sweep is TYPED and counted — never a silent `continue`
/// that looks like "nothing to recover"; the durable rows stay for the
/// next boot.
#[test]
fn startup_verification_recovery_reports_unreadable_rows() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap(),
    );
    let ws = manager.create_workspace("/w").unwrap();
    let _sid = manager
        .create_session(ws, "recover", "fake", "m")
        .unwrap()
        .id();
    // Healthy baseline: a typed zero, no failure.
    let healthy = recover_verification_jobs_at_startup(&manager);
    assert_eq!(healthy, VerificationRecoverySummary::default());
    assert!(!healthy.scan_failed);
    // Corrupt the durable verification store under the live manager:
    // the sweep must report the unreadable session instead of skipping.
    {
        let conn =
            rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db")).unwrap();
        conn.execute_batch("DROP TABLE verification_job").unwrap();
    }
    let summary = recover_verification_jobs_at_startup(&manager);
    assert_eq!(summary.unreadable, 1, "{summary:?}");
    assert!(!summary.scan_failed, "{summary:?}");
    assert_eq!(summary.requeued, 0, "{summary:?}");
}

/// F10 (adversarial): the startup queue-head sweep never classifies an
/// unreadable/unopenable session as "already drained" — it counts it
/// typed (an issue the operator can see) instead of silently skipping.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_queue_recovery_reports_unopenable_rows_instead_of_silence() {
    let dir = tempfile::tempdir().unwrap();
    let graph = build_daemon(dir.path(), None).unwrap();
    let ws = graph.session.create_workspace("/w").unwrap();
    let handle = graph.session.create_session(ws, "t", "fake", "m").unwrap();
    let op = graph.session.try_next_op_id().unwrap();
    graph
        .session
        .store()
        .enqueue_prompt(handle.id(), op, "queued", &[], None, None, None, 1)
        .unwrap();
    let healthy = recover_pending_queues_at_startup(&graph);
    assert_eq!(
        (healthy.candidates, healthy.runnable, healthy.unreadable),
        (1, 1, 0),
        "{healthy:?}"
    );
    assert!(!healthy.scan_failed, "{healthy:?}");
    // A durable queue row whose session does not exist (foreign row,
    // written with FK enforcement off): the sweep lists it as a candidate
    // but cannot open it — that is reported, never silently "drained".
    {
        let conn =
            rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "INSERT INTO prompt_queue(session_id, seq, op_id, prompt, status, requested_at) \
                 VALUES (999999, 1, 999999, 'foreign', 'pending', 1)",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    }
    let summary = recover_pending_queues_at_startup(&graph);
    assert_eq!(summary.candidates, 2, "{summary:?}");
    assert_eq!(
        summary.unreadable, 1,
        "the unopenable foreign row must be reported, not silent: {summary:?}"
    );
    assert!(!summary.scan_failed, "{summary:?}");
}

#[test]
fn parse_hooks_env_skips_malformed_entries_and_bounds_the_count() {
    // Hostile/malformed env: garbage entries are skipped, never a panic
    // and never a registered hook. Bounded to MAX_ENV_HOOKS entries.
    let raw = "  ; no_colon_here ; pre_tool: ; nope:true; bogus_event:/bin/true";
    let specs = parse_hooks_env(raw);
    assert!(specs.is_empty(), "every entry is malformed: {specs:?}");

    let ok = "task_complete:/bin/true";
    let specs = parse_hooks_env(ok);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].id, "env-0");
    assert_eq!(specs[0].events, vec![faktor_hooks::HookEvent::TaskComplete]);
    assert_eq!(specs[0].command, "/bin/true");
    assert!(specs[0].args.is_empty());
    assert!(specs[0].env_allowlist, "env allowlist is mandatory");
    assert_eq!(
        specs[0].failure_policy,
        faktor_hooks::FailurePolicy::FailClosed,
        "FailClosed is the default failure policy"
    );

    // Malformed entries between valid ones are skipped; ids stay
    // contiguous over the parsed (not the raw) positions.
    let mixed = "pre_tool:/bin/a arg1;garbage;post_tool:/bin/b";
    let specs = parse_hooks_env(mixed);
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].id, "env-0");
    assert_eq!(specs[0].events, vec![faktor_hooks::HookEvent::PreTool]);
    assert_eq!(specs[0].args, vec!["arg1".to_string()]);
    assert_eq!(specs[1].id, "env-1");
    assert_eq!(specs[1].events, vec![faktor_hooks::HookEvent::PostTool]);

    // A hostile env with more than MAX_ENV_HOOKS entries is capped.
    let many = (0..100)
        .map(|i| format!("pre_tool:/bin/true {i}"))
        .collect::<Vec<_>>()
        .join(";");
    let specs = parse_hooks_env(&many);
    assert_eq!(specs.len(), MAX_ENV_HOOKS);
}

#[test]
fn parse_hooks_env_recognizes_every_hook_event_name() {
    // Every snake_case event name the runtime can fire must parse, so an
    // env entry never silently drops a supported event.
    let names = [
        ("session_start", faktor_hooks::HookEvent::SessionStart),
        ("session_resume", faktor_hooks::HookEvent::SessionResume),
        ("task_start", faktor_hooks::HookEvent::TaskStart),
        ("pre_model", faktor_hooks::HookEvent::PreModel),
        ("post_model", faktor_hooks::HookEvent::PostModel),
        ("pre_tool", faktor_hooks::HookEvent::PreTool),
        ("post_tool", faktor_hooks::HookEvent::PostTool),
        ("tool_error", faktor_hooks::HookEvent::ToolError),
        ("pre_edit", faktor_hooks::HookEvent::PreEdit),
        ("post_edit", faktor_hooks::HookEvent::PostEdit),
        ("pre_commit", faktor_hooks::HookEvent::PreCommit),
        ("subagent_start", faktor_hooks::HookEvent::SubagentStart),
        ("subagent_stop", faktor_hooks::HookEvent::SubagentStop),
        ("agent_error", faktor_hooks::HookEvent::AgentError),
        ("agent_stop", faktor_hooks::HookEvent::AgentStop),
        ("task_complete", faktor_hooks::HookEvent::TaskComplete),
        ("session_end", faktor_hooks::HookEvent::SessionEnd),
    ];
    for (name, event) in names {
        let specs = parse_hooks_env(&format!("{name}:/bin/true"));
        assert_eq!(specs.len(), 1, "event {name} must parse");
        assert_eq!(specs[0].events, vec![event], "event {name}");
    }
}

#[test]
fn parsed_env_hook_registers_and_fires_on_a_real_registry() {
    // End-to-end shape of the daemon wiring: a parsed spec registers on
    // a real HookRegistry and the hook FIRES for its event (the audit
    // log gains the record). /bin/echo exists on the CI platforms.
    let specs = parse_hooks_env("post_tool:/bin/echo hook-fired");
    assert_eq!(specs.len(), 1);
    let registry = Arc::new(faktor_hooks::HookRegistry::with_supervisor(
        ProcessSupervisor::try_shared().expect("standalone supervisor"),
        faktor_core::CapabilitySet::ALL,
    ));
    for spec in specs {
        registry.register(spec).unwrap();
    }
    let verdict = registry.run(
        faktor_hooks::HookEvent::PostTool,
        &faktor_hooks::HookInput {
            event: faktor_hooks::HookEvent::PostTool,
            ..Default::default()
        },
    );
    assert_eq!(verdict, faktor_hooks::HookVerdict::Allow);
    let audit = registry.audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].hook_id, "env-0");
    assert_eq!(audit[0].event, faktor_hooks::HookEvent::PostTool);
}

#[test]
fn agent_info_names_faktor_and_lists_registered_provider_families() {
    let (_dir, session, agent) = acp_test_daemon(vec![]);
    let prompts = test_prompts(&session, &agent);
    let backend = DaemonAcpBackend::new(session, agent, prompts);
    let info = backend.agent_info();
    assert_eq!(info["name"], "Faktor");
    assert_eq!(info["version"], faktor_core::VERSION);
    assert_eq!(
        info["providerFamilies"],
        json!(["fake"]),
        "the scripted provider's family id must surface"
    );
}

#[test]
fn create_session_applies_defaults_and_list_sessions_sees_it() {
    let (_dir, session, agent) = acp_test_daemon(vec![]);
    let prompts = test_prompts(&session, &agent);
    let backend = DaemonAcpBackend::new(session.clone(), agent, prompts);

    // Defaults: workspace "/", title "acp", daemon model, single
    // registered provider.
    let sid = backend.create_session(&json!({})).unwrap();
    assert!(!sid.is_empty());
    let row = handle(&session, &sid).row().unwrap();
    assert_eq!(row.title, "acp");
    assert_eq!(row.provider, "fake");
    assert_eq!(row.model, "default");
    assert!(backend.list_sessions().contains(&sid));

    // Explicit params override every default and stay listable.
    let sid2 = backend
        .create_session(&json!({
            "workspace": "/elsewhere",
            "title": "zed-import",
            "provider": "fake",
            "model": "m",
        }))
        .unwrap();
    let row2 = handle(&session, &sid2).row().unwrap();
    assert_eq!(row2.title, "zed-import");
    assert_eq!(row2.model, "m");
    assert_eq!(
        session
            .store()
            .workspace_root(row2.workspace_id)
            .unwrap()
            .as_deref(),
        Some("/elsewhere")
    );
    assert!(backend.list_sessions().contains(&sid2));
    assert_eq!(backend.list_sessions().len(), 2);

    // No registered provider: refusing loudly beats a phantom session.
    let (_d2, session3, agent3) = {
        let dir2 = tempfile::tempdir().unwrap();
        let s =
            SessionManager::open(dir2.path().join("store"), dir2.path().join("cas"), true).unwrap();
        let a = test_agent(s.clone(), ProviderRegistry::new());
        (dir2, s, a)
    };
    let backend3 = DaemonAcpBackend::new(
        session3.clone(),
        agent3.clone(),
        test_prompts(&session3, &agent3),
    );
    let err = backend3.create_session(&json!({})).unwrap_err();
    assert!(err.contains("no providers"), "{err}");
}

// The daemon-turn tests each drive a real SessionManager actor from a
// multi-threaded runtime. Two of them running concurrently on a loaded
// runner has livelocked the actor handshake (test thread <-> db actor
// futex ping-pong with zero I/O, 30+ minutes, while each test passes in
// isolation). They take this process-wide guard so at most one heavy
// turn test runs at a time; assertions and coverage are unchanged.
pub(crate) static HEAVY_TURN_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn serial_turn_test() -> std::sync::MutexGuard<'static, ()> {
    HEAVY_TURN_TEST.lock().unwrap_or_else(|p| p.into_inner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_runs_a_real_turn_and_reports_the_final_state() {
    let _serial = serial_turn_test();
    let (_dir, session, agent) = acp_test_daemon(vec![
        ScriptedResponse::Text("pong".into()),
        ScriptedResponse::End,
    ]);
    let prompts = test_prompts(&session, &agent);
    let backend = DaemonAcpBackend::new(session, agent, prompts);
    let sid = backend.create_session(&json!({})).unwrap();

    let result = backend.prompt(&sid, "ping").unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["finalState"], "ready_for_next_turn");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_on_unknown_session_errors() {
    let _serial = serial_turn_test();
    let (_dir, session, agent) = acp_test_daemon(vec![
        ScriptedResponse::Text("pong".into()),
        ScriptedResponse::End,
    ]);
    let prompts = test_prompts(&session, &agent);
    let backend = DaemonAcpBackend::new(session, agent, prompts);
    let unknown = format!("{}", u64::MAX - 1);
    let err = backend.prompt(&unknown, "hi").unwrap_err();
    assert!(err.contains(&unknown), "{err}");
    assert!(backend.abort(&unknown).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_cancels_a_live_turn_and_keeps_the_session_usable() {
    let _serial = serial_turn_test();
    let (_dir, session, agent) = acp_test_daemon(vec![
        ScriptedResponse::Text("pong".into()),
        ScriptedResponse::End,
    ]);
    let prompts = test_prompts(&session, &agent);
    let backend = DaemonAcpBackend::new(session.clone(), agent.clone(), prompts);

    // A: the turn is durably ACTIVE (Preparing, live op registered, never
    // driven) — abort must land the machine ReadyForNextTurn.
    let sid_a = backend
        .create_session(&json!({ "title": "mid-flight" }))
        .unwrap();
    agent
        .submit(SessionId::new(sid_a.parse().unwrap()), "stop me", &[])
        .unwrap();
    assert!(handle(&session, &sid_a).state().unwrap().is_active());
    assert!(backend.abort(&sid_a).is_ok());
    assert_eq!(
        handle(&session, &sid_a).state().unwrap(),
        AgentState::ReadyForNextTurn
    );

    // B: an idle abort (Stop cancels the turn, never the session) keeps
    // the session promptable — a real turn still completes afterwards.
    let sid_b = backend.create_session(&json!({})).unwrap();
    assert!(backend.abort(&sid_b).is_ok());
    let result = backend.prompt(&sid_b, "ping").unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["finalState"], "ready_for_next_turn");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_inputs_are_errs_never_panics() {
    let _serial = serial_turn_test();
    let (_dir, session, agent) = acp_test_daemon(vec![
        ScriptedResponse::Text("pong".into()),
        ScriptedResponse::End,
    ]);
    let prompts = test_prompts(&session, &agent);
    let backend = DaemonAcpBackend::new(session.clone(), agent, prompts);
    let sid = backend.create_session(&json!({})).unwrap();

    // Empty and whitespace prompts are refused before the runtime.
    assert!(backend.prompt(&sid, "").is_err());
    assert!(backend.prompt(&sid, "   \n\t ").is_err());

    // Oversized prompts are refused by the daemon bound (never a panic,
    // never an unbounded journal write).
    let huge = "x".repeat(5 * 1024 * 1024);
    let err = backend.prompt(&sid, &huge).unwrap_err();
    assert!(
        err.contains("exceeds") && err.contains("bound"),
        "oversized prompt must be refused, got: {err}"
    );

    // Hostile session ids: non-numeric, zero, overflowing u64.
    for bad in ["abc", "0", "18446744073709551616", "-1", ""] {
        assert!(backend.prompt(bad, "hi").is_err(), "prompt {bad:?} refused");
        assert!(backend.abort(bad).is_err(), "abort {bad:?} refused");
    }

    // A deleted (Closed) session refuses prompts and aborts loudly.
    let dead = backend.create_session(&json!({})).unwrap();
    session
        .delete_session(SessionId::new(dead.parse().unwrap()))
        .unwrap();
    assert!(backend.prompt(&dead, "hi").is_err());
    assert!(backend.abort(&dead).is_err());

    // None of the hostile inputs touched the live session or crashed the
    // daemon: a real turn still completes.
    let result = backend.prompt(&sid, "still alive").unwrap();
    assert_eq!(result["finalState"], "ready_for_next_turn");
}

#[test]
fn verification_config_unknown_fields_fail_and_zero_disables_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.json");
    // Sane values parse (strictly) and drive the daemon service: the
    // derived per-check budget is the configured cap.
    std::fs::write(
        &path,
        r#"{"verification": {"quick_max_s": 30, "unit_max_s": 120, "full_as_background": false}}"#,
    )
    .unwrap();
    let cfg = serve_config(Some(path.clone())).expect("explicit sane config");
    let supervisor = ProcessSupervisor::try_shared().expect("standalone supervisor");
    let service = daemon_verification(&cfg.verification, &supervisor);
    assert!(!service.is_disabled());
    let policy = service.policy();
    assert_eq!(policy.quick_max, std::time::Duration::from_secs(30));
    assert_eq!(policy.unit_max, std::time::Duration::from_secs(120));
    assert!(!policy.full_as_background);
    // Budget probe through the service (same decision the genuine-end
    // site applies per check category).
    let quick = faktor_verify::exec::CheckSpec::new(
        "q",
        faktor_verify::exec::CheckKind::Compile,
        faktor_verify::exec::CheckCategory::Quick,
        "cargo",
        ["check"],
        true,
    );
    assert_eq!(
        service.budget_for(&quick),
        faktor_verify::exec::BudgetDecision::RunInline(std::time::Duration::from_secs(30))
    );
    // An explicit unknown field inside [verification] fails startup.
    std::fs::write(&path, r#"{"verification": {"bogus": 1}}"#).unwrap();
    let e = serve_config(Some(path.clone())).expect_err("unknown field fails startup");
    assert!(e.contains("unknown field"), "{e}");
    // quick_max_s = 0 yields the DISABLED service: fail closed.
    std::fs::write(
        &path,
        r#"{"verification": {"quick_max_s": 0, "unit_max_s": 0}}"#,
    )
    .unwrap();
    let cfg = serve_config(Some(path)).expect("zero quick budget is a valid config");
    let supervisor = ProcessSupervisor::try_shared().expect("standalone supervisor");
    let service = daemon_verification(&cfg.verification, &supervisor);
    assert!(
        service.is_disabled(),
        "quick_max_s = 0 must disable verification (fail closed)"
    );
}

#[test]
fn the_build_report_names_the_running_artifact_honestly() {
    // Directly started (no bootstrap env): the version is reported, the
    // self hash is optional and no release identity is claimed.
    let report = build_report_json(true);
    assert_eq!(report["schema"], "faktor-build-report/v1");
    assert_eq!(report["version"], faktor_core::VERSION);
    assert!(report["exe"].is_string());
    let self_sha = report["self_sha256"].as_str().unwrap();
    assert_eq!(self_sha.len(), 64);
    // No release environment in this test process (no other test sets
    // it): the release fields are honest absences, and the folded health
    // version keeps the plain package version.
    assert!(report["release_id"].is_null());
    assert!(report["release_digest"].is_null());
    assert!(report["self_digest_matches_release"].is_null());
    // The cheap daemon-startup form skips the full-binary hash pass.
    let cheap = build_report_json(false);
    assert!(cheap["self_sha256"].is_null());
    assert_eq!(cheap["version"], faktor_core::VERSION);
}
