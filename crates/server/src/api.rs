//! The daemon's HTTP surface.
//!
//! One surface: the **Faktor Native Protocol v1** endpoints
//! (`/session/{id}/projection`, `/models`, `/capabilities`, plus the
//! `/native/...` mounts: liveness/readiness, durable session listings,
//! usage aggregate, task runs, tournaments, terminals, attachments,
//! evidence, semantic introspection; documented in
//! `docs/native-protocol.md`) — the daemon's own contract. All endpoints
//! stay behind password auth (`FAKTOR_SERVER_PASSWORD` via
//! `Authorization: Bearer`, or `x-faktor-server-password`; the legacy
//! per-start token rides the same Bearer header). No Basic form exists.
//!
//! Layout: handler bodies live in [`crate::native`] (Faktor Native
//! Protocol v1). This module keeps router assembly plus the re-exports the
//! tests and the daemon entry points consume.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::oneshot;
use tower_http::limit::RequestBodyLimitLayer;

use faktor_agent::AgentRuntime;
use faktor_session::SessionManager;

use crate::auth::{AuthToken, ServerPassword};
use crate::permission::ChannelPermissionRequester;

pub(crate) use crate::native::*;

/// The startup line the daemon prints after binding (frozen stdout
/// contract; nothing else may be printed on stdout).
pub fn startup_line(port: u16) -> String {
    format!("faktor server listening on http://127.0.0.1:{port}")
}

/// Handle to the daemon's evidence store: an evidence store behind a
/// process-wide lock, so the server can hold it while producers (future
/// audit) insert. Constructed through [`empty_evidence_store`] in hosts
/// that do not yet run a capture path.
pub type EvidenceStoreHandle =
    Arc<std::sync::RwLock<Box<dyn faktor_evidence::store::EvidenceStore + Send + Sync>>>;

/// The default empty evidence store: an in-memory store that retains
/// bounded backing bytes. `/native/evidence/{id}` answers an honest 404
/// until a producer inserts, never a fabricated envelope.
pub fn empty_evidence_store() -> EvidenceStoreHandle {
    Arc::new(std::sync::RwLock::new(Box::new(
        faktor_evidence::store::MemoryEvidenceStore::new(4 * 1024 * 1024),
    )))
}

const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

pub struct ServerDeps {
    pub session: Arc<SessionManager>,
    pub agent: Arc<AgentRuntime>,
    pub permissions: Arc<ChannelPermissionRequester>,
    /// The orchestration runtime (audits P0-20/21/23/61): the AUTHORITATIVE
    /// executor of multi-agent tasks and the durable control surface the
    /// `/native/agents/{child}/...` endpoints drive. Non-optional in
    /// production: the CLI wires the real runtime in the daemon graph
    /// region; `ServerDeps::new` (tests) builds one over the same
    /// session+agent.
    pub orchestrator: Arc<faktor_orchestrator::runtime::OrchestratorRuntime>,
    /// The TaskExecutor (audits P0-20/21/23/61/90/91): ONE entry for native
    /// task starts — single-item tasks drive the existing session with the
    /// daemon's own prompt path, multi-item tasks spawn real children
    /// through [`ServerDeps::orchestrator`].
    pub tasks: Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
    /// The daemon's durable cost ledger (audit 12/17): the SAME ledger the
    /// graph built. Handlers that touch per-task/per-child caps use this
    /// authority instead of constructing a second ledger per request.
    pub budgets: Arc<faktor_session::DurableBudgetLedger>,
    /// Per-start bearer token (legacy clients); the frontend uses the
    /// password. Both ride `Authorization: Bearer`.
    pub auth_token: AuthToken,
    /// The password the frontend generated and passed via `FAKTOR_SERVER_PASSWORD`.
    pub server_password: ServerPassword,
    /// The workspace root carried on the server's dependency envelope.
    pub directory: Option<String>,
    pub version: String,
    /// Real workspace file service for revert/unrevert/diff (None = the wire
    /// surface refuses with an honest 409).
    pub fs: Option<Arc<faktor_fs::WorkspaceFileService>>,
    /// Real checkpoint store for revert/unrevert/diff (None = honest 409).
    pub snapshots: Option<Arc<faktor_snapshot::CheckpointStore>>,
    /// The daemon's evidence store (audit 82): scope-checked reads of
    /// captured evidence envelopes for the native evidence endpoints.
    /// `None` = no store wired in this host (the endpoints answer 503).
    pub evidence: Option<EvidenceStoreHandle>,
    /// The semantic provider registry (audit 83) surfaced by
    /// `/native/semantic/{status,capabilities}`. `None` = no provider is
    /// configured; the endpoints report the fallback registry shape.
    pub semantic: Option<Arc<faktor_semantic::registry::SemanticProviderRegistry>>,
    /// Live chunk stream from the agent (audit round 11): when present,
    /// serve() drains it into low-latency session.next.*.delta frames.
    /// Bounded (audit 41): the agent's [`faktor_agent::ChunkSink`] sender
    /// half coalesces ephemeral deltas under backpressure instead of
    /// growing memory; this receiver half stays drained eagerly into the
    /// bounded global ring.
    pub chunk_rx: Option<tokio::sync::mpsc::Receiver<faktor_agent::ChunkEvent>>,
    /// Deterministic readiness knob (audit 55; mirrors the suggested
    /// `FAKTOR_SIMULATE_NOT_READY=1` gate as a field — an env gate would
    /// race parallel tests in one process). When true, the ready flag stays
    /// false after serve() setup, so `GET /native/ready` keeps answering
    /// 503 `{"ready":false}`. Production callers never set it.
    pub simulate_not_ready: bool,
}

impl ServerDeps {
    /// Default construction (embedded hosts + tests): builds a runtime over
    /// the same session+agent. The daemon NEVER uses this path for its
    /// production surface — `serve` assembles `ServerDeps` from the named
    /// daemon graph's instances through [`ServerDeps::new_with`], so no
    /// second orchestrator/executor is ever constructed in a daemon
    /// lifetime (audit 12/17).
    pub fn new(
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        permissions: Arc<ChannelPermissionRequester>,
    ) -> Self {
        let orchestrator =
            faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
        // The embedded host carries the SAME isolation authority the daemon
        // graph does: the shadow service rooted beside the store's data dir.
        // Mutating runs therefore always execute in an isolated candidate —
        // there is no no-shadow production constructor.
        let shadows_root = session
            .store()
            .path()
            .parent()
            .map(|dir| dir.join("shadows"))
            .unwrap_or_else(|| std::env::temp_dir().join("faktor-shadows"));
        let shadows =
            faktor_orchestrator::runtime::shadow::ShadowRoots::new(session.clone(), shadows_root);
        let tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator,
            session.clone(),
            agent.clone(),
            shadows,
        );
        let budgets = faktor_session::DurableBudgetLedger::new(session.clone());
        Self::new_with(session, agent, permissions, orchestrator, tasks, budgets)
    }

    /// Assemble the server surface over GRAPH-PROVIDED runtime authorities
    /// (audit 12/17): production passes the daemon graph's orchestrator,
    /// TaskExecutor and budget ledger — the SAME instances the rest of the
    /// daemon uses — so the server never constructs a second execution or
    /// money authority.
    pub fn new_with(
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        permissions: Arc<ChannelPermissionRequester>,
        orchestrator: Arc<faktor_orchestrator::runtime::OrchestratorRuntime>,
        tasks: Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
        budgets: Arc<faktor_session::DurableBudgetLedger>,
    ) -> Self {
        Self {
            session,
            agent,
            permissions,
            orchestrator,
            tasks,
            budgets,
            auth_token: AuthToken::generate(),
            server_password: ServerPassword::from_env(),
            directory: None,
            version: faktor_core::VERSION.to_string(),
            fs: None,
            snapshots: None,
            evidence: None,
            semantic: None,
            chunk_rx: None,
            simulate_not_ready: false,
        }
    }

    /// Wire the real native snapshot store so `/session/{id}/revert`,
    /// `/unrevert` and `/diff` actually restore files. Both must be provided
    /// together; with `None` the endpoints keep their honest 409.
    pub fn with_snapshots(
        mut self,
        fs: Arc<faktor_fs::WorkspaceFileService>,
        snapshots: Arc<faktor_snapshot::CheckpointStore>,
    ) -> Self {
        self.fs = Some(fs);
        self.snapshots = Some(snapshots);
        self
    }

    /// Wire the daemon's evidence store so the native evidence endpoints can
    /// serve scope-checked reads. Production wires the graph's store here;
    /// embedded hosts leave it `None` and answer an honest 503.
    pub fn with_evidence_store(mut self, store: EvidenceStoreHandle) -> Self {
        self.evidence = Some(store);
        self
    }

    /// Wire the semantic provider registry surfaced by the native semantic
    /// introspection endpoints. `None` reports the fallback-only shape.
    pub fn with_semantic_registry(
        mut self,
        registry: Arc<faktor_semantic::registry::SemanticProviderRegistry>,
    ) -> Self {
        self.semantic = Some(registry);
        self
    }

    /// The startup line the CLI prints on stdout after binding.
    pub fn startup_line(&self, addr: SocketAddr) -> String {
        startup_line(addr.port())
    }
}

pub struct ServerHandle {
    pub addr: SocketAddr,
    pub shutdown: oneshot::Sender<()>,
    /// The startup line the CLI prints on stdout after binding.
    pub startup_line: String,
}

/// Bind (port 0 = ephemeral) and serve. Returns once listening.
pub async fn serve(mut deps: ServerDeps, port: u16) -> std::io::Result<ServerHandle> {
    // Bind first, then compute the line (needs the bound address) and
    // finally move the deps into the router.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let startup_line = deps.startup_line(addr);
    // Readiness (audit 55): the flag starts false and flips true ONLY when
    // setup completes. Recovery runs before serve in the caller (the CLI
    // opens the store and runs `agent.recover()` first), migrations applied
    // at store open are implicit, and the required runtime components are
    // non-optional `Arc`s in `ServerDeps` — so the flip at the end of setup
    // is exactly the ready moment. `simulate_not_ready` (tests) keeps it
    // false forever: /native/ready answers 503 {ready:false}.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let set_ready = !deps.simulate_not_ready;
    // Live chunk path (audit 41): the agent's bounded, coalescing chunk
    // sink must stay drained, but the native surface is durable and
    // journal-driven (clients page `/native/events` and `/native/messages`),
    // so there is no live subscriber to fan out to. The bounded channel is
    // consumed eagerly so the agent's sink never coalesces under a dead
    // consumer.
    if let Some(mut rx) = deps.chunk_rx.take() {
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
    }
    let app = Router::new()
        // Faktor Native Protocol v1 (docs/native-protocol.md): the daemon's
        // OWN surface, optimized around this runtime. These handlers speak
        // native JSON.
        .route("/session/{id}/projection", get(native_session_projection))
        .route("/models", get(native_models))
        .route("/capabilities", get(native_capabilities))
        // Native Protocol v1, audit 55-56 wiring: liveness/readiness plus
        // the durable session listings under an explicit /native prefix.
        // Every handler is auth-gated; request bodies parse with the strict
        // native DTOs (deny_unknown_fields — a typo is a 400).
        .route("/native/health", get(native_health))
        .route("/native/ready", get(native_ready))
        .route("/native/usage", get(native_usage))
        // Session bootstrap (native): create one durable session on a
        // workspace root, list the durable sessions, and run ONE ordinary
        // prompt through the daemon's executor entry.
        .route("/native/session", post(native_create_session))
        .route("/native/sessions", get(native_list_sessions))
        .route("/native/session/{id}/prompt", post(native_prompt))
        // The native durable journal SSE stream (cursor-resumable).
        .route("/native/session/{id}/events", get(native_session_events))
        .route("/native/session/{id}/turns", get(native_session_turns))
        .route("/native/session/{id}/tasks", get(native_session_tasks))
        .route(
            "/native/session/{id}/checkpoints",
            get(native_session_checkpoints),
        )
        .route(
            "/native/session/{id}/verification",
            get(native_session_verification),
        )
        .route("/native/session/{id}/agents", get(native_session_agents))
        // Presentation/attention continuity (additive): record one durable
        // foreground/background transition of a child of this session. It
        // never changes scheduling ownership, budgets or lineage.
        .route(
            "/native/session/{id}/agents/{child}/presentation",
            post(native_agent_presentation),
        )
        .route(
            "/native/session/{id}/terminal",
            get(native_session_terminal),
        )
        .route("/native/session/{id}/abort", post(native_session_abort))
        // Native coordination board (additive): one bounded newest-first
        // page of the path session's run-family board (GET) and one bounded
        // post AS that session (POST). Strict DTOs; family scoping and
        // terminal-child refusal stay in the session board authority.
        .route(
            "/native/session/{id}/board",
            get(native_session_board).post(native_session_board_post),
        )
        .route("/native/orchestrator/graph", get(native_orchestrator_graph))
        // Native agent state + control (audits P0-20/21/23/61): the real
        // child agents of the session's task runs (GET), and first-class
        // pause/resume/cancel/retry/steer/model/budget over the runtime's
        // durable child_commands queue (exactly-once applied semantics).
        .route("/native/agents", get(native_agents))
        .route("/native/agents/{child_id}/pause", post(native_agent_pause))
        .route(
            "/native/agents/{child_id}/resume",
            post(native_agent_resume),
        )
        .route(
            "/native/agents/{child_id}/cancel",
            post(native_agent_cancel),
        )
        .route("/native/agents/{child_id}/retry", post(native_agent_retry))
        .route("/native/agents/{child_id}/steer", post(native_agent_steer))
        .route("/native/agents/{child_id}/model", post(native_agent_model))
        .route(
            "/native/agents/{child_id}/budget",
            post(native_agent_budget),
        )
        // Audit P0-62/63/64 native surface (additive; strict DTOs): the
        // session-owned terminal projection (scoped listing + owned spawn +
        // bounded lifetime-event log), the durable cursor surfaces
        // (messages / journal events), the provider registry view, the
        // per-session authoritative usage read and the durable
        // verification-evidence read of one task.
        .route("/native/terminals", get(native_terminals))
        .route(
            "/native/session/{id}/terminal/events",
            get(native_terminal_events),
        )
        .route("/native/session/{id}/terminal", post(native_terminal_spawn))
        // The session-owned terminal control seam: every op routes through
        // the ONE durable TerminalService (the ledger rows are the
        // authority; a restarted daemon's Lost rows refuse I/O typed).
        .route(
            "/native/session/{id}/terminals/{terminal_id}/input",
            post(native_terminal_input),
        )
        .route(
            "/native/session/{id}/terminals/{terminal_id}/resize",
            post(native_terminal_resize),
        )
        .route(
            "/native/session/{id}/terminals/{terminal_id}/kill",
            post(native_terminal_kill),
        )
        .route(
            "/native/session/{id}/terminals/{terminal_id}/reconcile",
            post(native_terminal_reconcile),
        )
        .route(
            "/native/session/{id}/terminals/{terminal_id}/output",
            get(native_terminal_output),
        )
        // Native permission surface: the live pending set and the ONE
        // resolve path (409 on unknown/already-resolved, never a double
        // grant).
        .route("/native/permissions", get(native_permissions))
        .route("/native/permission/reply", post(native_permission_reply))
        .route("/native/messages", get(native_messages))
        .route("/native/events", get(native_events))
        .route("/native/providers", get(native_providers))
        .route("/native/session/{id}/usage", get(native_session_usage))
        .route(
            "/native/session/{id}/tasks/{task_id}/verification",
            get(native_task_verification),
        )
        // Native task runs (wave-24): the ONE HTTP surface that starts a
        // task through the daemon's TaskExecutor (POST), lists the
        // session's durable task runs with per-run state (GET), reads one
        // run's state, and cancels one run at the TASK level. Strict DTOs;
        // hostile bodies are 400s; no second start architecture exists.
        .route(
            "/native/session/{id}/task-runs",
            get(native_task_runs).post(native_task_run_start),
        )
        .route(
            "/native/session/{id}/task-runs/{run_id}",
            get(native_task_run_state),
        )
        .route(
            "/native/session/{id}/task-runs/{run_id}/cancel",
            post(native_task_run_cancel),
        )
        // Native binary attachments (additive, strict): upload ONE bounded
        // payload into the session's durable CAS-backed store, resolve its
        // typed metadata by digest, and fetch the verified bytes. IMAGES are
        // stored here and validated against the CHOSEN model's capabilities
        // (vision, deliverable mime, per-provider byte bound) at task
        // admission, where a refusal keeps the draft and bytes intact.
        .route(
            "/native/session/{id}/attachments",
            post(native_attachment_upload),
        )
        .route(
            "/native/session/{id}/attachments/{digest}",
            get(native_attachment_get),
        )
        .route(
            "/native/session/{id}/attachments/{digest}/bytes",
            get(native_attachment_bytes),
        )
        // Multi-candidate implementation tournaments (additive): start an
        // N = 2..=4 candidate tournament through the executor's ONE entry
        // (identical goal+criteria, isolated candidate worktrees) and read
        // its durable state. Strict DTOs; hostile bodies are 400s.
        .route(
            "/native/session/{id}/tournament",
            post(native_tournament_start),
        )
        .route(
            "/native/session/{id}/tournament/{tournament_id}",
            get(native_tournament_state),
        )
        // Additive durable listing of the session's tournaments (the
        // summary projection of the pinned ledger lifecycle rows).
        .route(
            "/native/session/{id}/tournaments",
            get(native_tournaments_list),
        )
        // Additive tournament control (strict DTOs): decide runs the
        // deterministic comparison (typed 404 unknown / 409 non-open or no
        // eligible winner) and abort discards every candidate with the
        // reason on the terminal durable row (typed 404/409).
        .route(
            "/native/session/{id}/tournaments/{tournament_id}/decide",
            post(native_tournament_decide),
        )
        .route(
            "/native/session/{id}/tournaments/{tournament_id}/abort",
            post(native_tournament_abort),
        )
        .route("/native/evidence/{id}", get(native_evidence_get))
        .route(
            "/native/evidence/{id}/retrieve",
            post(native_evidence_retrieve),
        )
        .route("/native/semantic/status", get(native_semantic_status))
        .route(
            "/native/semantic/capabilities",
            get(native_semantic_capabilities),
        )
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .with_state(AppState {
            deps: Arc::new(deps),
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: ready.clone(),
        });
    if set_ready {
        ready.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });
    Ok(ServerHandle {
        addr,
        shutdown: shutdown_tx,
        startup_line,
    })
}

// ------------------------------------------------------------------ state

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) deps: Arc<ServerDeps>,
    /// Runtime server-password override (`auth.set`); `None` = the startup
    /// env password (`ServerDeps.server_password`) applies.
    pub(crate) auth: Arc<std::sync::RwLock<Option<ServerPassword>>>,
    /// Bounded per-daemon log of session-owned terminal lifetime events
    /// (audit P0-62): `created` at spawn, `exited` when the swept process
    /// dies. Ring-bounded; ids ascend from 1.
    pub(crate) terminal_events:
        Arc<std::sync::Mutex<std::collections::VecDeque<(u64, serde_json::Value)>>>,
    pub(crate) next_terminal_event_id: Arc<std::sync::atomic::AtomicU64>,
    /// Readiness flag (audit 55): set true at the END of serve() setup, after
    /// the store was opened/migrated/recovered by the caller (cli runs
    /// `agent.recover()` before serve) and every required runtime component
    /// is in place (`SessionManager` is a non-optional `Arc` in
    /// `ServerDeps`, so its presence is structural). `GET /native/ready`
    /// answers 200 `{ready:true}` once it is set; 503 `{ready:false}`
    /// before/without it (`ServerDeps.simulate_not_ready`).
    pub(crate) ready: Arc<std::sync::atomic::AtomicBool>,
}

// ------------------------------------------------------------------ handlers

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::capability::PermissionDecision;
    use faktor_core::id::{SessionId, WorkspaceId};
    use faktor_core::model::ModelCapabilities;
    use faktor_evidence::store::EvidenceStore as _;
    use faktor_provider::FakeProvider;
    use faktor_session::BudgetAuthority;
    use std::time::Duration;

    /// Test-only default-allow transport: every mock endpoint below is a
    /// loopback server, and production construction injects the daemon's
    /// policy-checked transport instead.
    fn permissive_transport() -> Arc<dyn faktor_provider::egress::HttpTransport> {
        Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::permissive())
    }

    // ------------------------------------------------- poisoned authorities

    /// A poisoned auth-override lock is authority-bearing: the next auth
    /// check must return the typed internal refusal instead of panicking the
    /// whole daemon surface.
    #[test]
    fn poisoned_auth_override_lock_refuses_with_typed_internal_error() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let state = AppState {
            deps: Arc::new(deps),
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let auth = Arc::clone(&state.auth);
        let poisoner = std::thread::spawn(move || {
            let _guard = auth.write().unwrap();
            panic!("poison the auth override");
        });
        assert!(poisoner.join().is_err());
        assert!(state.auth.is_poisoned());

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer deadbeef".parse().unwrap(),
        );
        let err = crate::native::authed(&headers, &state)
            .expect_err("poisoned auth must refuse, never panic");
        assert_eq!(err.code, "internal");
        assert_eq!(err.http_status, 500);
        assert!(!err.retryable);
        assert!(err.message.contains("poison"), "{}", err.message);
    }

    /// The terminal-event ring is a strictly derived bounded cache (never an
    /// authority): a poisoned ring recovers on the next read instead of
    /// killing the endpoint.
    #[tokio::test]
    async fn poisoned_terminal_event_cache_recovers_on_next_read() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let session = deps.session.clone();
        let ws = session.create_workspace("/tmp").unwrap();
        let sid = session
            .create_session(ws, "poisoned-terminal-events", "fake", "m")
            .unwrap()
            .id()
            .to_string();
        let state = AppState {
            deps: Arc::new(deps),
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let ring = Arc::clone(&state.terminal_events);
        let poisoner = std::thread::spawn(move || {
            let _guard = ring.lock().unwrap();
            panic!("poison the terminal event cache");
        });
        assert!(poisoner.join().is_err());
        assert!(state.terminal_events.is_poisoned());

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", token.as_str()).parse().unwrap(),
        );
        let query: NativeTerminalEventsQuery =
            serde_json::from_value(serde_json::json!({})).unwrap();
        let response = native_terminal_events(
            axum::extract::State(state.clone()),
            headers,
            axum::extract::Path(sid.clone()),
            axum::extract::Query(query),
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::OK,
            "the derived cache must recover under poison"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["sessionId"], sid);
        assert!(body["events"].as_array().unwrap().is_empty());
    }

    /// The runtime + executor pair every test `ServerDeps` carries (the
    /// real orchestrator over the test store — the endpoints under test
    /// drive exactly what production drives).
    fn orch_pair(
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
    fn wire_snapshot_deps(
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

    fn test_deps(root: &std::path::Path) -> ServerDeps {
        test_deps_with(root, vec![])
    }

    fn test_deps_with(
        root: &std::path::Path,
        extra_providers: Vec<Arc<dyn faktor_provider::Provider>>,
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
        }
    }

    // ------------------------------------------------------------------
    // P0 round: the added operations (status aliases, fork,
    // summarize, delete, deleteMessage, question/network over the permission
    // machinery, config update/warnings/overlay, pty rejection, dispose,
    // auth rotation) each do real work and refuse loudly where the runtime
    // cannot honor them.

    /// Drop the ids that differ by construction between a session and its
    /// fork (info.sessionID and the row createdMs) for equality checks.

    // ---------------------------------------------------------------- native v1

    #[tokio::test]
    async fn native_bootstrap_strict_dtos_and_cursor_stream() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);

        // Strict create DTO: an unknown field or empty provider is a 400.
        for body in [
            serde_json::json!({"provider": "fake", "model": "m", "smuggled": 1}),
            serde_json::json!({"provider": "", "model": "m"}),
        ] {
            let resp = client
                .post(format!("{base}/native/session"))
                .bearer_auth(token.as_str())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "{body}");
        }
        // One durable session on the workspace root.
        let resp = client
            .post(format!("{base}/native/session"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "provider": "fake", "model": "m", "workspace": "/tmp", "title": "t-bootstrap",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let created: serde_json::Value = resp.json().await.unwrap();
        let sid = created["id"].as_str().unwrap().to_string();
        assert_eq!(created["title"], "t-bootstrap");
        assert!(created["created_ms"].as_i64().unwrap() > 0);

        // The durable listing carries it.
        let listing: serde_json::Value = client
            .get(format!("{base}/native/sessions"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(listing["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == sid && s["title"] == "t-bootstrap"));

        // Prompt strictness: id mismatch, empty prompt, unknown session.
        let resp = client
            .post(format!("{base}/native/session/{sid}/prompt"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": "999", "prompt": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = client
            .post(format!("{base}/native/session/{sid}/prompt"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": sid, "prompt": "   "}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = client
            .post(format!("{base}/native/session/999999/prompt"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": "999999", "prompt": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        // A real prompt is accepted and lands on the ONE executor entry.
        let resp = client
            .post(format!("{base}/native/session/{sid}/prompt"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": sid, "prompt": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let receipt: serde_json::Value = resp.json().await.unwrap();
        assert!(!receipt["op_id"].as_str().unwrap().is_empty());
        assert!(receipt["accepted"].as_bool().unwrap());

        // Native permission surface: the live set is empty, ids/decisions
        // are strict, and an unknown resolve is a typed 409 (never a
        // double grant).
        let listing: serde_json::Value = client
            .get(format!("{base}/native/permissions?session={sid}"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listing["permissions"], serde_json::json!([]));
        let resp = client
            .get(format!("{base}/native/permissions?session=abc"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = client
            .get(format!("{base}/native/permissions?sessionId={sid}"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "unknown query fields stay strict");
        for (body, expect) in [
            (
                serde_json::json!({"permission_id": "1", "decision": "maybe"}),
                400,
            ),
            (
                serde_json::json!({"permission_id": "0", "decision": "allow"}),
                400,
            ),
            (
                serde_json::json!({"permission_id": "999", "decision": "allow", "x": 1}),
                400,
            ),
        ] {
            let resp = client
                .post(format!("{base}/native/permission/reply"))
                .bearer_auth(token.as_str())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), expect, "{body}");
        }
        // An unknown id is the requester's documented pre-resolution: the
        // first reply lands, the second is a typed 409 (never a double
        // grant).
        let body = serde_json::json!({"permission_id": "999", "decision": "allow"});
        let resp = client
            .post(format!("{base}/native/permission/reply"))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{body}");
        let resp = client
            .post(format!("{base}/native/permission/reply"))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "double resolve is a typed conflict");

        // The native SSE stream replays the durable journal from cursor 0.
        let resp = client
            .get(format!("{base}/native/session/{sid}/events?after=0"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        use futures_util::StreamExt;
        let mut body = resp.bytes_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), body.next())
            .await
            .expect("stream must not hang")
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&first);
        assert!(text.contains("id: "), "frame carries a cursor id: {text}");
        assert!(
            text.contains("event: "),
            "frame carries an event kind: {text}"
        );
        assert!(text.contains("data: "), "frame carries data: {text}");
        // Unknown query fields stay strict 400s.
        let resp = client
            .get(format!("{base}/native/session/{sid}/events?aftr=1"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let _ = handle.shutdown.send(());
    }

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
        let _ = handle.shutdown.send(());
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
            efficiency: Default::default(),
        })
        .unwrap();
        let manager = session.clone();
        let driver = agent.clone();
        let deps = ServerDeps::new(session, agent, permissions.clone());
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
                        permissions.resolve(pid, PermissionDecision::Allow),
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
        let _ = handle.shutdown.send(());
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
            efficiency: Default::default(),
        })
        .unwrap();
        let driver = agent.clone();
        let deps = ServerDeps::new(session.clone(), agent, permissions.clone());
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
        let _ = handle.shutdown.send(());
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
            list.iter().any(|m| m["provider"] == "openai"
                && m["model"] == "gpt-y"
                && m["reasoning"] == true),
            "gpt-y reasoning flag: {list:?}"
        );
        assert!(
            list.iter()
                .any(|m| m["provider"] == "fake" && m["model"] == "default"),
            "registered fake provider still catalogued: {list:?}"
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
        assert_eq!(openai_entry["runtimeContextLimitSupported"], false);
        let fake_entry = body.get("fake").expect("fake provider key present");
        assert_eq!(fake_entry["runtimeContextLimitSupported"], false);
        let _ = handle.shutdown.send(());
    }

    // --------------------------------------------------- native v1: audits 55-56
    // /native/health + /native/ready semantics, the durable session
    // listings, the strict abort DTO and the cross-session usage aggregate.

    #[tokio::test]
    async fn native_health_and_ready_semantics() {
        // health answers 200 whenever the process responds; ready answers
        // 200 ONLY after serve() setup completed (recovery ran, migrations
        // applied, components in place) — and 503 before that moment, which
        // the simulate_not_ready knob keeps observable in tests.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);

        // Both are auth-gated like every daemon route.
        let resp = client
            .get(format!("{base}/native/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let resp = client
            .get(format!("{base}/native/ready"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Post-serve: health = liveness {ok, version}, ready = 200
        // {ready:true} (the flag flips at the very end of serve() setup, so
        // a test can only observe true after serve returns).
        let resp = client
            .get(format!("{base}/native/health"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert!(body["version"].is_string());
        let resp = client
            .get(format!("{base}/native/ready"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ready"], true);
        let _ = handle.shutdown.send(());

        // The not-ready window (deterministic test knob): with
        // simulate_not_ready the flag never flips, so ready is 503
        // {ready:false} even after serve returned — health stays 200.
        let deps = {
            let mut d = test_deps(dir.path());
            d.simulate_not_ready = true;
            d
        };
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let base2 = format!("http://{}", handle.addr);
        let resp = client
            .get(format!("{base2}/native/ready"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ready"], false);
        let resp = client
            .get(format!("{base2}/native/health"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = handle.shutdown.send(());
    }

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
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_tasks_verification_agents_and_terminal_reflect_durable_rows() {
        // The listings are row-backed: an injected durable ledger, an
        // injected verification fact, no turn records, no PTYs. Reading
        // them back must be exact; hostile ids are loud.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/tmp").unwrap();
        let session = manager.create_session(ws, "t-rows", "fake", "m").unwrap();
        let sid = session.id().to_string();
        let h = manager.get_session(session.id()).unwrap().unwrap();
        h.put_task_ledger(serde_json::json!({
            "goal": "implement the native surface",
            "constraints": ["rust"],
            "completed_steps": ["mount routes"],
            "open_steps": ["wire abort", "aggregate usage"],
            "decisions": ["strict DTOs"],
            "known_failures": [],
            "changed_files": ["crates/server/src/api.rs"],
            "tests_run": ["cargo check"],
            "tests_failed": [],
            "user_preferences": [],
        }))
        .unwrap();
        h.upsert_memory_fact("verification", "fmt", "failed:make fmt")
            .unwrap();

        // tasks: exactly one entry, typed from the ledger + the fact.
        let resp = client
            .get(format!("{base}/native/session/{sid}/tasks"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let tasks = body.as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{body}");
        assert_eq!(tasks[0]["goal"], "implement the native surface");
        assert_eq!(tasks[0]["state"], "in_progress");
        assert_eq!(
            tasks[0]["milestones"]["open"],
            serde_json::json!(["wire abort", "aggregate usage"])
        );
        assert_eq!(
            tasks[0]["milestones"]["completed"],
            serde_json::json!(["mount routes"])
        );
        assert_eq!(
            tasks[0]["changedFiles"],
            serde_json::json!(["crates/server/src/api.rs"])
        );
        assert_eq!(
            tasks[0]["verification"],
            serde_json::json!([{"id": "fmt", "detail": "failed:make fmt", "status": "failed"}])
        );

        // verification: the durable fact is owed-failed; no pending runs.
        let resp = client
            .get(format!("{base}/native/session/{sid}/verification"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["owed"], serde_json::json!([]));
        assert_eq!(body["failedChecks"][0]["id"], "fmt");
        assert_eq!(body["failedChecks"][0]["detail"], "failed:make fmt");

        // agents: no background agents yet (orchestration not landed).
        let resp = client
            .get(format!("{base}/native/session/{sid}/agents"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!([])
        );

        // terminal: no PTYs exist on this daemon → the empty view.
        let resp = client
            .get(format!("{base}/native/session/{sid}/terminal"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!([])
        );

        // turns: no turn ever ran → empty. checkpoints: no service wired
        // in test_deps → empty.
        let resp = client
            .get(format!("{base}/native/session/{sid}/turns"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!([])
        );
        let resp = client
            .get(format!("{base}/native/session/{sid}/checkpoints"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!([])
        );

        // The same hostile/unknown treatment applies to every listing.
        for path in [
            "/native/session/0/tasks".to_string(),
            "/native/session/abc/verification".to_string(),
            "/native/session/0/agents".to_string(),
            "/native/session/abc/terminal".to_string(),
        ] {
            let resp = client
                .get(format!("{base}{path}"))
                .bearer_auth(token.as_str())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "{path}");
        }
        for path in [
            "/native/session/999999/tasks".to_string(),
            "/native/session/999999/checkpoints".to_string(),
            "/native/session/999999/verification".to_string(),
            "/native/session/999999/agents".to_string(),
            "/native/session/999999/terminal".to_string(),
        ] {
            let resp = client
                .get(format!("{base}{path}"))
                .bearer_auth(token.as_str())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 404, "{path}");
        }
        let _ = handle.shutdown.send(());
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
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_abort_strict_dto_and_targeted_kill() {
        // The native abort is sdk_abort semantics behind the STRICT native
        // DTO (audit 56): any unknown body field — a typo included — is a
        // 400 before anything runs; a valid body kills exactly the targeted
        // queued op and leaves the machine untouched.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/tmp").unwrap();
        let session = manager
            .create_session(ws, "t-abort-native", "fake", "m")
            .unwrap();
        let session_id = session.id().to_string();
        let _ = session.submit_prompt("first", &[]).unwrap();
        let second = session.submit_prompt("second", &[]).unwrap();
        assert!(second.queued, "second prompt must queue behind Preparing");
        let op_id = second.op_id.to_string();

        // Strict DTO rejections: unknown field, realistic typo, missing
        // session_id, unparseable op_id, path/body mismatch, hostile path.
        for evil in [
            format!(r#"{{"session_id":"{session_id}","bogus":1}}"#),
            format!(r#"{{"session_id":"{session_id}","hardBudegt":true}}"#),
        ] {
            let resp = client
                .post(format!("{base}/native/session/{session_id}/abort"))
                .bearer_auth(token.as_str())
                .header("content-type", "application/json")
                .body(evil.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "{evil}");
        }
        let resp = client
            .post(format!("{base}/native/session/{session_id}/abort"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"op_id": "1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "missing session_id");
        let resp = client
            .post(format!("{base}/native/session/{session_id}/abort"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": session_id, "op_id": "not-a-number"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "unparseable op_id");
        let resp = client
            .post(format!("{base}/native/session/999999/abort"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": session_id}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "path/body session mismatch");
        let resp = client
            .post(format!("{base}/native/session/abc/abort"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": session_id}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "hostile path id");

        // Unauth with a VALID body → 401 (auth gate runs in the handler).
        let resp = client
            .post(format!("{base}/native/session/{session_id}/abort"))
            .json(&serde_json::json!({"session_id": session_id, "op_id": op_id}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Valid targeted abort of the queued prompt.
        let resp = client
            .post(format!("{base}/native/session/{session_id}/abort"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": session_id, "op_id": op_id}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let aborted: serde_json::Value = resp.json().await.unwrap();
        assert!(
            aborted["aborted"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o.as_str() == Some(op_id.as_str())),
            "{aborted}"
        );
        assert_eq!(
            session.state().unwrap(),
            faktor_core::state::AgentState::Preparing,
            "a queued-prompt kill must not touch the state machine"
        );
        assert_eq!(session.queued_prompt_count().unwrap(), 0);

        // Unknown session → 404 for a valid body.
        let resp = client
            .post(format!("{base}/native/session/999999/abort"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"session_id": "999999"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_terminal_lists_live_ptys_with_id_pid_alive() {
        // A live PTY on the daemon appears in /native/session/{id}/terminal
        // (the session id is validated, but PTYs have no session binding
        // yet — all daemon PTYs are listed). Platforms that refuse PTY
        // spawns must still serve the empty listing honestly.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/tmp").unwrap();
        let session = manager.create_session(ws, "t-pty", "fake", "m").unwrap();
        let sid = session.id().to_string();

        let Some(created) =
            native_spawn_terminal(&client, &base, &token, &sid, "/bin/sleep", &["30"]).await
        else {
            // Platform refusal (documented): the session terminal view stays
            // empty and honest.
            let resp = client
                .get(format!("{base}/native/session/{sid}/terminal"))
                .bearer_auth(token.as_str())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(
                resp.json::<serde_json::Value>().await.unwrap(),
                serde_json::json!([])
            );
            let _ = handle.shutdown.send(());
            return;
        };
        let pty_id = created["terminalId"].as_str().unwrap().to_string();
        let pid = created["pid"].as_u64().unwrap_or(0);
        assert!(pid > 0);

        // The live pty lists with its id, pid and aliveness.
        let resp = client
            .get(format!("{base}/native/session/{sid}/terminal"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let entries = body.as_array().unwrap();
        let mine = entries
            .iter()
            .find(|e| e["id"] == pty_id)
            .expect("the live pty must be listed");
        assert_eq!(mine["pid"], pid);
        assert_eq!(mine["alive"], true);

        // The bounded output snapshot reads through the ONE terminal
        // authority; unknown terminals are typed 404s, never fabricated
        // bytes.
        let resp = client
            .get(format!(
                "{base}/native/session/{sid}/terminals/{pty_id}/output"
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let out: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["alive"], true);
        let resp = client
            .get(format!(
                "{base}/native/session/{sid}/terminals/not-a-terminal/output"
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        // Killing it is terminal: the durable row stays in the session view
        // with state killed and alive false.
        assert_eq!(
            native_kill_terminal(&client, &base, &token, &sid, &pty_id).await,
            200
        );
        let resp = client
            .get(format!("{base}/native/session/{sid}/terminal"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        let mine = body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == pty_id)
            .expect("the killed terminal row stays durable");
        assert_eq!(mine["alive"], false);
        assert_eq!(mine["state"], "killed");
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_usage_aggregates_budget_and_spent_across_sessions() {
        // /native/usage sums the durable usage facts (kind "usage", keys
        // budget/spent) across sessions; hostile non-numeric values are
        // skipped, sessions without usage facts are counted but not listed.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/tmp").unwrap();
        let s1 = manager.create_session(ws, "t-u1", "fake", "m").unwrap();
        let ws2 = manager.create_workspace("/tmp2").unwrap();
        let s2 = manager.create_session(ws2, "t-u2", "fake", "m").unwrap();
        let ws3 = manager.create_workspace("/tmp3").unwrap();
        let _s3 = manager.create_session(ws3, "t-u3", "fake", "m").unwrap();
        // Real usage facts...
        s1.upsert_memory_fact("usage", "budget", "90000").unwrap();
        s1.upsert_memory_fact("usage", "spent", "1234").unwrap();
        s2.upsert_memory_fact("usage", "budget", "10000").unwrap();
        s2.upsert_memory_fact("usage", "spent", "42").unwrap();
        // ...hostile rows (non-numeric / other kinds / other keys) never
        // break the aggregate.
        s2.upsert_memory_fact("usage", "budget", "not-a-number")
            .unwrap();
        s2.upsert_memory_fact("usage", "rogue", "900000").unwrap();
        s2.upsert_memory_fact("preference", "budget", "700000")
            .unwrap();

        let resp = client
            .get(format!("{base}/native/usage"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let resp = client
            .get(format!("{base}/native/usage"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["sessions"], 3);
        // The later hostile budget upsert REPLACED s2's numeric budget
        // (upsert semantics) with a non-numeric value, which is skipped:
        // totals carry only the numeric facts.
        assert_eq!(body["totals"]["budget"], 90000);
        assert_eq!(body["totals"]["spent"], 1276);
        let per = body["perSession"].as_array().unwrap();
        assert_eq!(per.len(), 2, "fact-less sessions are counted, not listed");
        assert!(per.iter().any(|e| {
            e["sessionId"] == s1.id().to_string() && e["budget"] == 90000 && e["spent"] == 1234
        }));
        assert!(per.iter().any(|e| {
            e["sessionId"] == s2.id().to_string() && e["budget"].is_null() && e["spent"] == 42
        }));
        let _ = handle.shutdown.send(());
    }

    // ------------------------------------ orchestration graph (audit 93)

    /// Seed one parent session with a wave-12/13-shaped durable run: a
    /// plan row (items b then a), two registry children (b is Done with a
    /// staged merge; a is Running), one child with steering rows. Returns
    /// the parent session id and the child session ids.
    fn seed_orchestration_graph(
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

    #[tokio::test]
    async fn native_presentation_continuity_is_durable_scoped_and_typed() {
        // Foreground -> background -> foreground continuity: the durable
        // presentation fold of the child session, surfaced in the agent
        // listing + typed graph, never a scheduling or lineage change.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let manager = deps.session.clone();
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        // child-0 = Done row, child-1 = Running row.
        let (parent, child_a, child_b) = seed_orchestration_graph(&manager);
        let presentation_path = |session: &str, child: &str| {
            format!("/native/session/{session}/agents/{child}/presentation")
        };

        // Unauthenticated is 401 before anything else.
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-1")
            ))
            .json(&serde_json::json!({"state": "background"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Strict DTO: every hostile body is a plain 400, never a 422 and
        // never a silent default.
        for body in [
            serde_json::json!({}),
            serde_json::json!({"state": "paused"}),
            serde_json::json!({"state": "BACKGROUND"}),
            serde_json::json!({"state": 1}),
            serde_json::json!({"state": null}),
            serde_json::json!({"state": "background", "extra": true}),
            serde_json::json!("background"),
        ] {
            let resp = client
                .post(format!(
                    "{base}{}",
                    presentation_path(&parent.to_string(), "child-1")
                ))
                .bearer_auth(token.as_str())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "hostile body must 400: {body}");
        }
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-1")
            ))
            .bearer_auth(token.as_str())
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // Hostile/unknown/foreign ids are typed 404s scoped to the path
        // session (a child id of another session is never resolved).
        let other = manager
            .create_session(
                manager.create_workspace("/other").unwrap(),
                "other",
                "fake",
                "m",
            )
            .unwrap()
            .id();
        for (session, child) in [
            (parent.to_string(), "child-9".to_string()),
            (parent.to_string(), "..".to_string()),
            ("999999".to_string(), "child-1".to_string()),
            (other.to_string(), "child-1".to_string()),
        ] {
            let resp = client
                .post(format!("{base}{}", presentation_path(&session, &child)))
                .bearer_auth(token.as_str())
                .json(&serde_json::json!({"state": "background"}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 404, "session {session} child {child}");
        }
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path("not-a-number", "child-1")
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"state": "background"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // A running child flips to background durably; the same-state set is
        // an idempotent no-op that writes nothing.
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-1")
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"state": "background"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["child_id"], "child-1");
        assert_eq!(ack["presentation"], "background");
        assert_eq!(ack["changed"], true);
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-1")
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"state": "background"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["changed"], false);
        // Durable truth: the child session's ledger fold holds Background.
        let ca = manager.get_session(child_a).unwrap().unwrap();
        assert_eq!(
            ca.child_presentation("child-1").unwrap(),
            faktor_session::child::PresentationState::Background
        );

        // The native agent listing and the typed graph both carry the field;
        // an untouched child stays foreground.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/agents?session={parent}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let listing: serde_json::Value = resp.json().await.unwrap();
        let c1 = listing
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["agent_id"] == "child-1")
            .expect("child-1 listed");
        assert_eq!(c1["presentation"], "background");
        let c0 = listing
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["agent_id"] == "child-0")
            .expect("child-0 listed");
        assert_eq!(c0["presentation"], "foreground");
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/orchestrator/graph?session={parent}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let graph: serde_json::Value = resp.json().await.unwrap();
        let g1 = graph["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["child_id"] == "child-1")
            .expect("child-1 graphed");
        assert_eq!(g1["presentation"], "background");

        // Background -> foreground: the SAME ChildId/session continues and
        // the state folds back.
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-1")
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"state": "foreground"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["presentation"], "foreground");
        assert_eq!(ack["changed"], true);
        assert_eq!(c1["session_id"], g1["session_id"]);

        // Terminal children are Background-only: child-0's durable row is
        // Done. The currently-foreground no-op stays legal, the background
        // flip is accepted, and the foreground revival is a typed 409 that
        // writes nothing.
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-0")
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"state": "foreground"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap()["changed"],
            false
        );
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-0")
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"state": "background"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp = client
            .post(format!(
                "{base}{}",
                presentation_path(&parent.to_string(), "child-0")
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"state": "foreground"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "terminal child refuses foreground revival"
        );
        let cb = manager.get_session(child_b).unwrap().unwrap();
        assert_eq!(
            cb.child_presentation("child-0").unwrap(),
            faktor_session::child::PresentationState::Background,
            "the refused revival wrote nothing"
        );
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_orchestrator_graph_projects_the_durable_run() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let manager = deps.session.clone();
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let (parent, _ca, cb) = seed_orchestration_graph(&manager);
        // Unauthenticated is 401 like every /native handler.
        let resp = client
            .get(format!(
                "{base}/native/orchestrator/graph?session={}",
                parent
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        // Authenticated: the full graph JSON.
        let resp = client
            .get(format!(
                "{base}/native/orchestrator/graph?session={}",
                parent
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let g: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(g["plan_id"], "run-1");
        assert_eq!(g["goal"], "Ship the graph");
        // b is Done (durable child row), a is still Pending behind... no:
        // a has a durable Running child, so the step is Running; root is
        // Running (not all Done).
        let steps: Vec<&str> = g["work_items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["item_id"].as_str().unwrap())
            .collect();
        assert_eq!(steps, vec!["b", "a"]);
        assert_eq!(g["work_items"][0]["state"], "Done");
        assert_eq!(g["work_items"][1]["state"], "Running");
        assert_eq!(g["state"], "Running");
        // Children ordered by plan step (b child first even though its
        // created_ms is smaller anyway; verify the linkage values).
        let children = g["children"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0]["child_id"], "child-0");
        assert_eq!(children[0]["plan_step_index"], 0);
        assert_eq!(children[0]["session_id"], cb.raw());
        assert_eq!(children[0]["worktree_id"], cb.raw());
        assert_eq!(children[0]["ownership"], "read_only_shared");
        assert_eq!(children[0]["state"], "Done");
        assert_eq!(children[0]["budget"], 1000);
        assert!(children[0]["capabilities"].is_array());
        // The merge record of the Done child.
        let m = &children[0]["merge"];
        assert_eq!(m["change_set_id"], "base-child-0-cs");
        assert_eq!(m["merged"], serde_json::json!(["keep.rs"]));
        assert_eq!(m["rejected"], serde_json::json!([]));
        assert_eq!(m["conflicts"][0][0], "src/a.rs");
        assert_eq!(children[1]["child_id"], "child-1");
        assert_eq!(children[1]["plan_step_index"], 1);
        assert_eq!(children[1]["state"], "Running");
        assert!(children[1]["merge"].is_null());
        // Steering history of the a-child: applied pause first, pending
        // steer second, seq order preserved.
        let events = children[1]["steer_events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["kind"]["kind"], "pause");
        assert!(!events[0]["applied_ms"].is_null());
        assert_eq!(events[1]["kind"]["kind"], "steer");
        assert_eq!(events[1]["kind"]["note"], "focus the api");
        assert!(events[1]["applied_ms"].is_null());
        assert!(events[0]["seq"].as_u64().unwrap() < events[1]["seq"].as_u64().unwrap());
        let _ = handle.shutdown.send(());
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
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_orchestrator_graph_hostile_ids_404_and_corrupt_rows_are_loud() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let manager = deps.session.clone();
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let (parent, _ca, _cb) = seed_orchestration_graph(&manager);
        // Hostile session ids: non-numeric, zero, negative, absurd, and a
        // session that exists but holds no orchestration run — all 404
        // (never a phantom graph, never a 200).
        for hostile in [
            "abc".to_string(),
            "0".to_string(),
            "-1".to_string(),
            "999999999".to_string(),
            "1;drop".to_string(),
            "%2e%2e".to_string(),
        ] {
            let resp = client
                .get(format!(
                    "{base}/native/orchestrator/graph?session={hostile}"
                ))
                .bearer_auth(token.as_str())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 404, "hostile session {hostile:?} must 404");
        }
        // A real session with no orchestration rows: 404.
        let ws = manager.create_workspace("/plain").unwrap();
        let plain = manager.create_session(ws, "plain", "fake", "m").unwrap();
        let resp = client
            .get(format!(
                "{base}/native/orchestrator/graph?session={}",
                plain.id()
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // A second run under the PARENT: the graph is ambiguous — loud 409
        // naming both runs.
        let plan_row = serde_json::json!({
            "plan": {"goal": "second", "non_goals": [], "constraints": [],
                     "work_items": []},
            "created_ms": 1,
        });
        manager
            .get_session(parent)
            .unwrap()
            .unwrap()
            .upsert_memory_fact(ORCH_PLAN_KIND, "run-2", &plan_row.to_string())
            .unwrap();
        let resp = client
            .get(format!(
                "{base}/native/orchestrator/graph?session={}",
                parent
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("run-1"));
        assert!(body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("run-2"));
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_orchestrator_graph_tampered_registry_row_is_a_loud_500() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let manager = deps.session.clone();
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let (parent, _ca, _cb) = seed_orchestration_graph(&manager);
        // Corrupt one registry row: the projection refuses loudly (500)
        // instead of serving a silently partial graph.
        let parent_handle = manager.get_session(parent).unwrap().unwrap();
        parent_handle
            .upsert_memory_fact(ORCH_REGISTRY_KIND, "run-1/child-1", "{corrupt")
            .unwrap();
        let resp = client
            .get(format!(
                "{base}/native/orchestrator/graph?session={}",
                parent
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 500);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("run-1/child-1"),
            "{body}"
        );
        let _ = handle.shutdown.send(());
    }

    // ------------------------------------------------- native agents + control

    /// Chunk-paced per-call provider (real-drive control windows).
    struct PacedScriptedProvider {
        inner: FakeProvider,
        calls: std::sync::Mutex<Vec<Vec<faktor_provider::ScriptedResponse>>>,
        script_index: std::sync::atomic::AtomicUsize,
        request_count: std::sync::atomic::AtomicUsize,
        chunk_delay_ms: u64,
    }

    impl PacedScriptedProvider {
        fn new(
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
                    faktor_provider::ScriptedResponse::End => {
                        Ok(faktor_provider::ProviderChunk::Done)
                    }
                    faktor_provider::ScriptedResponse::Reasoning(_) => unreachable!(),
                }
            });
            Box::pin(stream)
        }
    }

    struct AllowAll;
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
    fn echo_tool() -> faktor_agent::Tool {
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
    fn paced_test_deps(root: &std::path::Path, paced: Arc<PacedScriptedProvider>) -> ServerDeps {
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
        }
    }

    fn read_workspace_caps() -> faktor_orchestrator::caps::CapabilitySet {
        use faktor_orchestrator::caps::{CapabilityGrant, LatticeCap, ScopePattern};
        faktor_orchestrator::caps::CapabilitySet::from_grants(vec![CapabilityGrant::new(
            LatticeCap::ReadWorkspace,
            ScopePattern::new("*").unwrap(),
        )])
        .unwrap()
    }

    fn read_child_spec(item: &str) -> faktor_orchestrator::runtime::ChildSpec {
        let mut s = faktor_orchestrator::runtime::ChildSpec::new(item);
        s.child_caps = read_workspace_caps();
        s.task_caps = read_workspace_caps();
        s
    }

    /// A real owner session for an orchestrated run (registered worktree on
    /// a real directory).
    fn orch_owner_env(
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

    fn analysis_plan(items: &[&str]) -> faktor_orchestrator::TaskPlan {
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

    async fn get_agents(base: &str, token: &str, path: &str) -> serde_json::Value {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_agents_lists_and_controls_real_children_mid_flight() {
        let dir = tempfile::tempdir().unwrap();
        // Real children over the server's runtime: paced two-iteration
        // roundtrips keep the drives mid-flight long enough for HTTP
        // controls to land deterministically.
        let paced = PacedScriptedProvider::new(
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![
                vec![
                    faktor_provider::ScriptedResponse::Text("analyzing".into()),
                    faktor_provider::ScriptedResponse::ToolCall {
                        id: "c1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"text": "hello"}),
                    },
                    faktor_provider::ScriptedResponse::End,
                ],
                vec![
                    faktor_provider::ScriptedResponse::Text("done".into()),
                    faktor_provider::ScriptedResponse::End,
                ],
                vec![faktor_provider::ScriptedResponse::End],
            ],
            4000,
        );
        let deps = paced_test_deps(dir.path(), paced);
        let orch = deps.orchestrator.clone();
        let manager = deps.session.clone();
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        let (parent, owner, isolated) = orch_owner_env(&manager, dir.path());

        let config = faktor_orchestrator::runtime::ExecConfig {
            run_id: "run-http".into(),
            ceilings: faktor_orchestrator::runtime::Ceilings::default(),
            parent_caps: read_workspace_caps(),
            provider: "fake".into(),
            default_model: "m".into(),
            isolated_root: isolated.clone(),
            crash_seam: None,
        };
        let plan = analysis_plan(&["a", "b"]);
        let specs = vec![read_child_spec("a"), read_child_spec("b")];
        let run = tokio::spawn(async move {
            orch.execute_task(plan, owner, config, &specs)
                .await
                .unwrap()
        });

        // The agent listing shows the parent's own run + both children.
        let client = reqwest::Client::new();
        let mut entries = Vec::new();
        for _ in 0..200 {
            let resp = client
                .get(format!("{base}/native/agents?session={parent}"))
                .bearer_auth(token.as_str())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let v: serde_json::Value = resp.json().await.unwrap();
            let kids: Vec<&serde_json::Value> = v
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["kind"] == "child")
                .collect();
            if kids.len() >= 2 {
                entries = v.as_array().unwrap().clone();
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(entries.len(), 3, "self + two children: {entries:?}");
        assert_eq!(entries[0]["kind"], "self");
        assert_eq!(entries[0]["run_id"], "run-http");
        assert_eq!(entries[0]["ownership"], "self");
        assert_eq!(entries[0]["state"], "Running");
        assert_eq!(entries[0]["goal"], "Ship the analysis");
        assert!(entries[1]["item_id"] == "a" || entries[2]["item_id"] == "a");
        // Children carry real session ids, worktree identity, live model
        // and progress while their drives are in flight. The provider is the
        // child session's OWN durable row (the catalog join key), so it must
        // match the run's provider even when another provider serves the same
        // model id.
        for e in entries.iter().filter(|e| e["kind"] == "child") {
            assert_eq!(e["run_id"], "run-http");
            assert_ne!(e["session_id"].as_u64().unwrap_or(0), 0);
            assert_eq!(e["ownership"], "read_only_shared");
            assert_eq!(e["state"], "Running");
            assert_eq!(e["model"], "m");
            assert_eq!(e["provider"], "fake");
        }

        // Mid-flight budget change: applied synchronously and durably
        // visible on the child row through the listing.
        let resp = client
            .post(format!("{base}/native/agents/child-0/budget"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"max_tokens": 4321}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["queuedSeq"], 1);
        assert_eq!(ack["applied"], true);
        // Steer + model enqueue durably with the exact wire shape and are
        // applied at the drive's next safe reasoning boundary (the pause
        // boundary machine itself is the wave-12 harness's contract; this
        // endpoint test freezes the queue + ack wire and the durable
        // application visible on the child's drive-state row).
        for (path, seq, body) in [
            (
                "steer",
                2,
                Some(serde_json::json!({"text": "focus the api surface"})),
            ),
            ("model", 3, Some(serde_json::json!({"model": "default"}))),
        ] {
            let mut rb = client
                .post(format!("{base}/native/agents/child-0/{path}"))
                .bearer_auth(token.as_str());
            if let Some(b) = body {
                rb = rb.json(&b);
            }
            let resp = rb.send().await.unwrap();
            assert_eq!(resp.status(), 200, "{path}");
            let ack: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(ack["queuedSeq"], seq, "{path}");
            assert!(ack["applied"].is_null(), "{path}");
        }

        // The run completes naturally; both children are Done. The queue
        // wire for steer/model is frozen above; their exactly-once
        // application at a reasoning boundary is the wave-12 runtime
        // harness's contract (pause/steer/cancel/budget drives), exercised
        // in this crate's own suite.
        let outcome = tokio::time::timeout(Duration::from_secs(180), run)
            .await
            .expect("run must settle")
            .expect("executor drive panicked");
        assert!(outcome.complete, "{outcome:?}");

        // The terminal listing reflects the durable rows: the budget patch
        // sits on the child row and both children are Done.
        let resp = client
            .get(format!("{base}/native/agents?session={parent}"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        let v: serde_json::Value = resp.json().await.unwrap();
        let entries = v.as_array().unwrap();
        assert_eq!(entries.len(), 3);
        let c0 = entries
            .iter()
            .find(|e| e["agent_id"] == "child-0")
            .expect("done child listed");
        assert_eq!(c0["state"], "Done");
        assert_eq!(c0["budget"], 4321, "budget change visible on the child row");
        let c1 = entries
            .iter()
            .find(|e| e["agent_id"] == "child-1")
            .expect("done child listed");
        assert_eq!(c1["state"], "Done");
        let root = entries
            .iter()
            .find(|e| e["kind"] == "self")
            .expect("self entry");
        assert_eq!(root["state"], "Done");
        // Pause after the run is a typed terminal refusal once the mirror
        // settled (409, never a silent no-op).
        let resp = client
            .post(format!("{base}/native/agents/child-0/pause"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "terminal children refuse pause");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_agents_lists_insession_task_runs_and_empty_only_without_runs() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let tasks = deps.tasks.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/plain").unwrap();
        let sid = manager
            .create_session(ws, "plain", "fake", "m")
            .unwrap()
            .id();

        // Genuinely no task run -> the empty array (never a phantom).
        let v = get_agents(
            &base,
            token.as_str(),
            &format!("/native/session/{sid}/agents"),
        )
        .await;
        assert_eq!(v, serde_json::json!([]));

        // A TaskExecutor single-item task (the one-work-item case of the
        // SAME executor that spawns orchestrated children) drives this
        // session through the daemon's own prompt path and shows up as the
        // parent's own task run.
        let req = faktor_orchestrator::runtime::task_executor::TaskRunRequest {
            goal: "analyze the module boundaries".into(),
            work_items: vec![faktor_orchestrator::WorkItem::new(
                "a1",
                "analyze the module boundaries",
                faktor_orchestrator::WorkKind::Analysis,
            )],
            ..Default::default()
        };
        let receipt = tasks.start_task(sid, req).expect("single-item start");
        assert_eq!(
            receipt.mode,
            faktor_orchestrator::runtime::task_executor::TaskRunMode::InSession
        );
        assert!(receipt.run_id.starts_with("tx-"));

        // The listing polls to the run's terminal state and shows the
        // session's own run with the session's worktree identity.
        let mut seen = None;
        for _ in 0..200 {
            let v = get_agents(
                &base,
                token.as_str(),
                &format!("/native/agents?session={sid}"),
            )
            .await;
            let entries = v.as_array().unwrap();
            if entries.is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            seen = Some(entries.clone());
            if entries[0]["state"] == "Done" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let entries = seen.expect("the in-session run must appear");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e["kind"], "self");
        assert_eq!(e["run_id"], receipt.run_id);
        assert_eq!(e["session_id"], sid.raw());
        assert_eq!(e["state"], "Done");
        assert_eq!(e["goal"], "analyze the module boundaries");
        assert_eq!(e["item_ids"], serde_json::json!(["a1"]));
        assert_eq!(e["ownership"], "self");
        assert!(e["budget"].is_null());
        let session_row = manager.get_session(sid).unwrap().unwrap().row().unwrap();
        assert_eq!(e["worktree_id"], session_row.worktree_id.raw());
        // The path-id form lists the same truth (progress ticks between
        // polls, so the live fields are compared individually).
        let v2 = get_agents(
            &base,
            token.as_str(),
            &format!("/native/session/{sid}/agents"),
        )
        .await;
        let e2 = &v2.as_array().unwrap()[0];
        for key in [
            "agent_id",
            "kind",
            "run_id",
            "session_id",
            "worktree_id",
            "goal",
            "state",
            "model",
            "budget",
            "ownership",
        ] {
            assert_eq!(e2.get(key), e.get(key), "{key}");
        }
        assert_eq!(e2["item_ids"], e["item_ids"]);
        // Hostile session ids are typed 404 on the query endpoint.
        for hostile in ["abc", "0", "-1", "999999999", "1;drop"] {
            let client = reqwest::Client::new();
            let resp = client
                .get(format!("{base}/native/agents?session={hostile}"))
                .bearer_auth(token.as_str())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 404, "hostile {hostile:?}");
        }
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_agent_control_guards_and_hostile_inputs_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let orch = deps.orchestrator.clone();
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        let (parent, owner, isolated) = orch_owner_env(&manager, dir.path());

        // A run whose executor crashed right after the child was created
        // (BeforeDrive seam): the durable child row is live (Running) and
        // the mirror is parked — a deterministic control window without a
        // racing drive.
        let config = faktor_orchestrator::runtime::ExecConfig {
            run_id: "run-seam".into(),
            ceilings: faktor_orchestrator::runtime::Ceilings::default(),
            parent_caps: read_workspace_caps(),
            provider: "fake".into(),
            default_model: "m".into(),
            isolated_root: isolated.clone(),
            crash_seam: Some(faktor_orchestrator::runtime::CrashSeam::BeforeDrive),
        };
        let plan = analysis_plan(&["a"]);
        let specs = vec![read_child_spec("a")];
        let res = orch
            .execute_task(plan, owner, config, &specs)
            .await
            .expect_err("the seam must fire");
        assert!(
            matches!(
                res,
                faktor_orchestrator::runtime::ExecError::InjectedCrashSeam(_)
            ),
            "{res:?}"
        );

        // Hostile child ids and bodies: typed 404/400, never a panic.
        let client = reqwest::Client::new();
        let post = |path: &str, body: Option<serde_json::Value>| {
            let mut rb = client
                .post(format!("{base}{path}"))
                .bearer_auth(token.as_str());
            if let Some(b) = body {
                rb = rb.json(&b);
            }
            rb.send()
        };
        for path in [
            "/native/agents/child-9/pause",
            "/native/agents/nope/cancel",
            "/native/agents/child-0/../../pause",
        ] {
            let resp = post(path, None).await.unwrap();
            assert_eq!(resp.status(), 404, "{path}");
        }
        let resp = post("/native/agents/child-0/budget", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        // A cost budget change is a synchronous durable effect (audit 9/H):
        // the child's task-row `max_cost_micro` is patched and its budget
        // scope is enrolled under the run root. No queue row exists for it
        // (queuedSeq null) and the token axis is untouched.
        let resp = post(
            "/native/agents/child-0/budget",
            Some(serde_json::json!({"max_cost_micro": 500})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert!(ack["queuedSeq"].is_null(), "{ack}");
        assert_eq!(ack["applied"], true);
        let v = get_agents(
            &base,
            token.as_str(),
            &format!("/native/agents?session={parent}"),
        )
        .await;
        let child_session = v
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["agent_id"] == "child-0")
            .and_then(|e| e["session_id"].as_u64())
            .expect("child-0 listed with its session");
        let ledger = faktor_session::DurableBudgetLedger::new(manager.clone());
        let cost_view = ledger
            .session_budget_view(
                SessionId::new(child_session),
                faktor_core::id::TaskId::new(1),
            )
            .expect("durable budget view");
        assert_eq!(
            cost_view.max_cost_micro,
            Some(500),
            "the cost change landed on the child's durable task-row cap"
        );
        assert_eq!(
            ledger
                .scope_of(SessionId::new(child_session))
                .unwrap()
                .map(|s| s.child_id),
            Some("child-0".to_string()),
            "the child is enrolled under its run root"
        );
        // Zero on either axis is ambiguous (the store reads 0 as unlimited)
        // and refuses typed on both axes.
        let resp = post(
            "/native/agents/child-0/budget",
            Some(serde_json::json!({"max_cost_micro": 0})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = post(
            "/native/agents/child-0/budget",
            Some(serde_json::json!({"max_tokens": 0})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = post(
            "/native/agents/child-0/budget",
            Some(serde_json::json!({"max_tokens": 5, "max_cost_micro": 5})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = post(
            "/native/agents/child-0/steer",
            Some(serde_json::json!({"text": "x".repeat(501)})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 400, "steering notes are bounded");
        let resp = post(
            "/native/agents/child-0/model",
            Some(serde_json::json!({"model": "gpt-99"})),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 404, "model not in the provider registry");
        let resp = post("/native/agents/child-0/retry", None).await.unwrap();
        assert_eq!(resp.status(), 409, "only Failed children retry");
        let resp = post("/native/agents/child-0/resume", None).await.unwrap();
        assert_eq!(resp.status(), 200);

        // Valid controls enqueue durably with the exactly-once ack shape.
        let resp = post("/native/agents/child-0/pause", None).await.unwrap();
        assert_eq!(resp.status(), 200);
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["queuedSeq"], 2, "the earlier resume took seq 1");
        assert!(ack["applied"].is_null());
        let resp = post(
            "/native/agents/child-0/steer",
            Some(serde_json::json!({"text": "look at the seam"})),
        )
        .await
        .unwrap();
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["queuedSeq"], 3);
        assert!(ack["applied"].is_null());
        let resp = post(
            "/native/agents/child-0/model",
            Some(serde_json::json!({"model": "default"})),
        )
        .await
        .unwrap();
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["queuedSeq"], 4);
        assert!(ack["applied"].is_null());
        let resp = post(
            "/native/agents/child-0/budget",
            Some(serde_json::json!({"max_tokens": 99})),
        )
        .await
        .unwrap();
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["queuedSeq"], 5);
        assert_eq!(ack["applied"], true);
        let resp = post("/native/agents/child-0/cancel", None).await.unwrap();
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["queuedSeq"], 6);
        assert_eq!(ack["applied"], true);

        // The listing reflects the durable rows (budget patch on the child
        // row; the run's own entry derives from the children).
        let v = get_agents(
            &base,
            token.as_str(),
            &format!("/native/agents?session={}", parent),
        )
        .await;
        let entries = v.as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let child = entries
            .iter()
            .find(|e| e["agent_id"] == "child-0")
            .expect("child listed");
        assert_eq!(
            child["budget"], 99,
            "budget change visible on the child row"
        );
        assert_eq!(child["run_id"], "run-seam");
        let root = entries
            .iter()
            .find(|e| e["kind"] == "self")
            .expect("self entry");
        assert_eq!(root["run_id"], "run-seam");
        assert_eq!(root["state"], "Running");
        let _ = handle.shutdown.send(());
    }

    // ------------------------------------------------- audits P0-62/63/64

    async fn native_get(
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

    /// Spawn a session-owned terminal through the native endpoint. Returns
    /// `None` when the platform refuses PTY spawns (documented skip).
    async fn native_spawn_terminal(
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
    async fn native_kill_terminal(
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
    fn seed_typed_task(
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

    #[tokio::test]
    async fn native_terminal_ownership_scope_isolation_and_lifetime_events() {
        // The terminal unification: terminals spawned under session A carry
        // durable {session_id, task_id, agent_id, operation_id} + terminal
        // UUID ownership; the session-scoped view of B never shows A's
        // terminal even when the daemon owns both, and the lifetime log is
        // the durable row stream (created -> running -> killed), session-
        // scoped and strictly above the cursor.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        // The execution authority resolves the default terminal cwd to the
        // session's candidate root, so this fixture root must be a real
        // directory.
        let root = dir.path().join("nt-root");
        std::fs::create_dir_all(&root).unwrap();
        let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
        let a = manager.create_session(ws, "t-pty-a", "fake", "m").unwrap();
        let b = manager.create_session(ws, "t-pty-b", "fake", "m").unwrap();
        let a_sid = a.id().to_string();
        let b_sid = b.id().to_string();

        // Hostile/unknown sessions and strict DTO checks first.
        for path in [
            "/native/terminals?session=0",
            "/native/terminals?session=abc",
        ] {
            let resp = native_get(&client, &base, &token, path).await;
            assert_eq!(resp.status(), 400, "{path}");
        }
        let resp = native_get(&client, &base, &token, "/native/terminals?session=999999").await;
        assert_eq!(resp.status(), 404);
        let resp = native_get(&client, &base, &token, "/native/terminals?session=1&limt=2").await;
        assert_eq!(resp.status(), 400, "unknown query field is a 400");

        // Spawn owned terminals under A and B through the ONE durable
        // service. The response carries the terminal UUID plus the legacy
        // pty alias, the durable ownership and the process identity.
        let Some(pty_a) =
            native_spawn_terminal(&client, &base, &token, &a_sid, "/bin/sleep", &["60"]).await
        else {
            // Platform refusal: the session-scoped view stays empty + the
            // documented unowned/note shape still serves.
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/native/terminals?session={a_sid}"),
            )
            .await;
            assert_eq!(resp.status(), 200);
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["terminals"], serde_json::json!([]));
            assert_eq!(body["unowned"], 0);
            let _ = handle.shutdown.send(());
            return;
        };
        let pty_a_id = pty_a["terminalId"].as_str().unwrap().to_string();
        assert_eq!(pty_a["ptyId"], pty_a_id, "legacy alias is the UUID");
        let pid_a = pty_a["pid"].as_u64().unwrap();
        assert!(pid_a > 0);
        assert_eq!(pty_a["state"], "running");
        assert_eq!(pty_a["sessionId"], a_sid);
        assert_eq!(pty_a["taskId"], "1", "standalone session task identity");
        assert!(pty_a["agentId"].is_null());
        assert!(pty_a["startTimeMs"].as_i64().unwrap_or(0) > 0);
        let op_a = pty_a["operationId"].as_str().unwrap().to_string();
        assert!(!op_a.is_empty());
        // The durable effective execution profile rides the spawn response:
        // the cwd is the session's candidate root and the budgets/profiles
        // are the admitted ones (never just the owner).
        let profile = &pty_a["executionProfile"];
        assert!(profile.is_object(), "{pty_a}");
        assert_eq!(profile["sessionId"], a.id().raw());
        assert_eq!(
            profile["cwd"],
            root.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(profile["filesystem"], "workspace+external:ask-ask");
        assert_eq!(profile["network"], "none");
        assert!(profile["budgets"]["maxProcesses"].as_u64().unwrap_or(0) > 0);

        let Some(pty_b) =
            native_spawn_terminal(&client, &base, &token, &b_sid, "/bin/sleep", &["60"]).await
        else {
            let _ = native_kill_terminal(&client, &base, &token, &a_sid, &pty_a_id).await;
            let _ = handle.shutdown.send(());
            return;
        };
        let pty_b_id = pty_b["terminalId"].as_str().unwrap().to_string();

        // A's scoped view: ONLY A's terminal with full ownership.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/terminals?session={a_sid}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["sessionId"], a_sid);
        assert_eq!(body["unowned"], 0);
        assert_eq!(body["note"], "");
        let rows = body["terminals"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{body}");
        assert_eq!(rows[0]["id"], pty_a_id);
        assert_eq!(rows[0]["terminalId"], pty_a_id);
        assert_eq!(rows[0]["pid"], pid_a);
        assert_eq!(rows[0]["alive"], true);
        assert_eq!(rows[0]["sessionId"], a_sid);
        assert_eq!(rows[0]["taskId"], "1");
        assert_eq!(rows[0]["operationId"], op_a);
        assert_eq!(rows[0]["state"], "running");
        assert!(rows[0]["spawnedMs"].as_i64().unwrap_or(0) > 0);
        assert!(rows[0]["agentId"].is_null());

        // B's scoped view NEVER contains A's terminal although the daemon
        // owns both: B sees exactly its own row.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/terminals?session={b_sid}"),
        )
        .await;
        let body: serde_json::Value = resp.json().await.unwrap();
        let rows = body["terminals"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "B sees only its own terminal: {body}");
        assert_eq!(rows[0]["terminalId"], pty_b_id);
        assert!(
            rows.iter().all(|r| r["terminalId"] != pty_a_id),
            "A's terminal must never surface in B's view: {body}"
        );

        // Cross-session control is denied typed: B can never drive A's
        // terminal through the IDs it can name.
        let denied = client
            .post(format!(
                "{base}/native/session/{b_sid}/terminals/{pty_a_id}/input"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"data": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 404, "foreign scope is denied");
        let denied = client
            .post(format!(
                "{base}/native/session/{b_sid}/terminals/{pty_a_id}/kill"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 404, "foreign kill is denied");

        // A's terminal is still alive and untouched.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/terminals?session={a_sid}"),
        )
        .await;
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["terminals"][0]["state"], "running");
        assert_eq!(body["terminals"][0]["alive"], true);

        // The legacy daemon-level view lists both (durable rows + the
        // retired compat PTY rows).
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{a_sid}/terminal"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let rows: serde_json::Value = resp.json().await.unwrap();
        let rows = rows.as_array().unwrap();
        let mine = rows
            .iter()
            .find(|r| r["id"] == pty_a_id)
            .expect("A's terminal in the daemon view");
        assert_eq!(mine["sessionId"], a_sid, "ownership annotated: {mine}");
        let other = rows
            .iter()
            .find(|r| r["id"] == pty_b_id)
            .expect("B's terminal in the daemon view");
        assert_eq!(other["sessionId"], b_sid);

        // Session-scoped durable lifetime events: created + running frames
        // with the terminal ownership; B's log never contains A's frames.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{a_sid}/terminal/events"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["sessionId"], a_sid);
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 2, "created + running: {body}");
        assert_eq!(events[0]["type"], "terminal_created");
        assert_eq!(events[1]["type"], "terminal_running");
        assert_eq!(events[0]["terminalId"], pty_a_id);
        assert_eq!(events[0]["ptyId"], pty_a_id);
        assert_eq!(events[0]["pid"], pid_a);
        assert!(events[0]["id"].as_u64().unwrap() > 0);
        assert_eq!(body["hasMore"], false);
        assert!(body["nextCursor"].is_null());
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{b_sid}/terminal/events"),
        )
        .await;
        let body: serde_json::Value = resp.json().await.unwrap();
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["terminalId"], pty_b_id,
            "no cross-session frames: {body}"
        );

        // Kill A through the pty authority: the Killed row is journaled
        // immediately (no lazy sweep needed) and the child tree dies.
        let kill = native_kill_terminal(&client, &base, &token, &a_sid, &pty_a_id).await;
        assert_eq!(kill, 200);
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{a_sid}/terminal/events"),
        )
        .await;
        let body: serde_json::Value = resp.json().await.unwrap();
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 3, "created + running + killed: {body}");
        assert_eq!(events[2]["type"], "terminal_killed");
        assert_eq!(events[2]["terminalId"], pty_a_id);
        assert_eq!(events[2]["pid"], pid_a, "kill event keeps the spawn pid");
        assert_eq!(
            body["hasMore"], false,
            "the whole log fits one page: {body}"
        );
        assert!(body["nextCursor"].is_null());
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/terminals?session={a_sid}"),
        )
        .await;
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["terminals"][0]["state"], "killed", "{body}");
        assert_eq!(body["terminals"][0]["alive"], false, "{body}");

        // Input on a terminal row is a typed state refusal (409).
        let refused = client
            .post(format!(
                "{base}/native/session/{a_sid}/terminals/{pty_a_id}/input"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"data": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 409, "killed terminal refuses I/O");

        // Bounds + hostile ids + strict DTO on the event log and spawn.
        for path in [
            format!("/native/session/{a_sid}/terminal/events?limit=0"),
            format!("/native/session/{a_sid}/terminal/events?limit=201"),
            format!("/native/session/{a_sid}/terminal/events?limt=5"),
            "/native/session/abc/terminal/events".to_string(),
            "/native/session/0/terminal/events".to_string(),
        ] {
            let resp = native_get(&client, &base, &token, &path).await;
            assert_eq!(resp.status(), 400, "{path}");
        }
        let resp = native_get(
            &client,
            &base,
            &token,
            "/native/session/999999/terminal/events",
        )
        .await;
        assert_eq!(resp.status(), 404);
        // Unknown spawn session / hostile body / unknown body field.
        let resp = client
            .post(format!("{base}/native/session/999999/terminal"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"command": "/bin/sleep", "args": ["1"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = client
            .post(format!("{base}/native/session/{a_sid}/terminal"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"command": "/bin/sleep", "bogus": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "unknown body field is a 400");
        let resp = client
            .post(format!("{base}/native/session/{a_sid}/terminal"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"args": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "missing command");
        let resp = client
            .post(format!("{base}/native/session/{a_sid}/terminal"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"command": "x".repeat(5000)}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "oversized command rejected");
        let resp = client
            .post(format!("{base}/native/session/{a_sid}/terminal"))
            .json(&serde_json::json!({"command": "/bin/sleep", "args": ["1"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Cleanup: kill B's terminal so no test child outlives the test.
        assert_eq!(
            native_kill_terminal(&client, &base, &token, &b_sid, &pty_b_id).await,
            200
        );
        let _ = handle.shutdown.send(());
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
        let _ = handle.shutdown.send(());
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
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_providers_registry_and_health_snapshot() {
        // P0-64c: the registry view carries provider identity, models with
        // real capabilities, source provenance and the honest health
        // snapshot; secrets never leak.
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
        let openai = faktor_openai::OpenAiProvider::build(
            faktor_openai::OpenAiConfig {
                base_url: "http://127.0.0.1:1/v1".into(),
                api_key: Some("sk-super-secret".into()),
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

        let resp = native_get(&client, &base, &token, "/native/providers").await;
        assert_eq!(resp.status(), 200);
        let list: serde_json::Value = resp.json().await.unwrap();
        let entries = list.as_array().unwrap();
        assert!(entries.len() >= 2, "fake + openai registered: {list}");
        // Deterministic order by instance id.
        let ids: Vec<&str> = entries
            .iter()
            .map(|e| e["instanceId"].as_str().unwrap())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        let fake = entries.iter().find(|e| e["instanceId"] == "fake").unwrap();
        assert_eq!(fake["family"], "fake");
        assert_eq!(fake["health"]["status"], "registered");
        assert!(fake["health"]["note"]
            .as_str()
            .unwrap()
            .contains("adapter-private"));
        assert!(!fake["models"].as_array().unwrap().is_empty());
        let oai = entries
            .iter()
            .find(|e| e["instanceId"] == "openai")
            .unwrap();
        let models = oai["models"].as_array().unwrap();
        let gpt_x = models.iter().find(|m| m["model"] == "gpt-x").unwrap();
        assert_eq!(gpt_x["context"], 128_000);
        assert_eq!(gpt_x["source"], "providerCatalog");
        assert_eq!(oai["runtimeContextLimitSupported"], false);
        // Secrets never reach this surface.
        let raw = list.to_string().to_lowercase();
        assert!(!raw.contains("sk-super-secret"), "api keys never leak");
        assert!(!raw.contains("api_key"));

        let resp = client
            .get(format!("{base}/native/providers"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_usage_reads_durable_rows_exactly_and_survives_reopen() {
        // P0-63: the usage endpoints read the DURABLE rows — provider_call
        // token columns (incl. prefix observations) and the per-task
        // cost_reservation rows with their route decisions. Numbers match
        // the stored rows exactly, sessions stay isolated, and a reopened
        // store (brand-new manager + server over the same data root) serves
        // the identical JSON.
        let dir = tempfile::tempdir().unwrap();
        let (expected_a, expected_global, sid_a) = {
            let deps = test_deps(dir.path());
            let token = deps.auth_token.clone();
            let manager = deps.session.clone();
            let handle = serve(deps, 0).await.unwrap();
            let client = reqwest::Client::new();
            let base = format!("http://{}", handle.addr);
            let ws_a = manager.create_workspace("/usage-a").unwrap();
            let ws_b = manager.create_workspace("/usage-b").unwrap();
            let a = manager
                .create_session(ws_a, "t-usage-a", "fake", "m")
                .unwrap();
            let b = manager
                .create_session(ws_b, "t-usage-b", "fake", "m")
                .unwrap();
            let ha = manager.get_session(a.id()).unwrap().unwrap();
            let hb = manager.get_session(b.id()).unwrap().unwrap();
            // Typed task rows: A owns task 1 (capped) and an untouched task
            // 2 (null-safe pre-first-reservation view); B owns task 1.
            seed_typed_task(&ha, 1, Some(5000), Some(3), "usage-a");
            seed_typed_task(&ha, 2, Some(1000), None, "usage-a-extra");
            seed_typed_task(&hb, 1, Some(100), None, "usage-b");
            let store = manager.store();
            let ta1 = faktor_core::id::TaskId::new(1);
            let tb1 = faktor_core::id::TaskId::new(1);
            store
                .cost_task_cap_set(a.id(), ta1, Some(1_000_000))
                .unwrap();
            store.cost_task_cap_set(b.id(), tb1, Some(50_000)).unwrap();
            let now = manager.now_ms();
            // A's provider calls: two completed with prefix observations
            // (cacheable-prefix token columns) + one failed row (NULL
            // counters never count).
            let op1 = manager.next_op_id();
            let op2 = manager.next_op_id();
            let op3 = manager.next_op_id();
            ha.settle_usage_with_prefix(
                op1,
                "fake",
                "m",
                "completed",
                Some(900),
                Some(100),
                None,
                Some([7u8; 32]),
                Some(400),
            )
            .unwrap();
            ha.settle_usage_with_prefix(
                op2,
                "fake",
                "m",
                "completed",
                Some(500),
                Some(50),
                None,
                Some([9u8; 32]),
                Some(460),
            )
            .unwrap();
            ha.record_provider_call(op3, "fake", "m", "failed", None, None, Some("boom"))
                .unwrap();
            // B's call: completed WITHOUT a prefix observation.
            let opb = manager.next_op_id();
            hb.settle_usage_with_prefix(
                opb,
                "fake",
                "m",
                "completed",
                Some(7),
                Some(3),
                None,
                None,
                None,
            )
            .unwrap();
            // A's reservations: one settled (with route JSON), one refunded,
            // one left open.
            let faktor_store::CostReserveOutcome::Granted(r1) =
                store.cost_reserve(a.id(), ta1, op1, 5000, now).unwrap()
            else {
                panic!("reserve r1 must be granted");
            };
            store
                .cost_settle(
                    r1,
                    1000,
                    Some(1000),
                    Some(990),
                    Some(
                        r#"{"provider":"fake","model":"m","estimated_cost_micro":90,"estimated_latency_ms":1,"reasoning":"passthrough","considered":1,"source":"configured"}"#,
                    ),
                    now + 1,
                )
                .unwrap();
            let faktor_store::CostReserveOutcome::Granted(r2) =
                store.cost_reserve(a.id(), ta1, op2, 3000, now).unwrap()
            else {
                panic!("reserve r2 must be granted");
            };
            store.cost_refund(r2, now + 2).unwrap();
            let faktor_store::CostReserveOutcome::Granted(_r3) = store
                .cost_reserve(a.id(), ta1, manager.next_op_id(), 200, now)
                .unwrap()
            else {
                panic!("reserve r3 must be granted");
            };
            // B's reservation: one settled with NO provider report.
            let faktor_store::CostReserveOutcome::Granted(rb) =
                store.cost_reserve(b.id(), tb1, opb, 4000, now).unwrap()
            else {
                panic!("reserve rb must be granted");
            };
            store
                .cost_settle(rb, 40, Some(40), None, None, now + 1)
                .unwrap();

            // ---- per-session authoritative usage of A
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/native/session/{}/usage", a.id()),
            )
            .await;
            assert_eq!(resp.status(), 200);
            let ua: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(ua["sessionId"], a.id().to_string());
            // provider-call tokens exactly the stored input+output columns.
            assert_eq!(ua["providerCalls"]["tokens"], 1550);
            // prefix observations mirror the store rows.
            let prefix = store.provider_call_prefix_rows(a.id()).unwrap();
            let observed = ua["providerCalls"]["prefixObservations"]
                .as_array()
                .unwrap();
            assert_eq!(observed.len(), prefix.len());
            for (row, o) in prefix.iter().zip(observed) {
                assert_eq!(o["rowId"], row.row_id);
                assert_eq!(o["promptTokens"], row.prompt_tokens as u64);
                assert_eq!(
                    o["stability"],
                    row.prefix_stability
                        .map(|v| serde_json::json!(v))
                        .unwrap_or(serde_json::Value::Null)
                );
            }
            // The stability aggregate equals the store's aggregate (f64
            // equality through the JSON wire is checked within one ulp: the
            // client-side serde_json parser is not the round-trip-exact
            // parser, so a long decimal can land one ulp off the stored
            // double; the endpoint itself serves the store's exact value).
            let agg = store
                .session_stored_prefix_stability(a.id())
                .unwrap()
                .unwrap();
            let ps = &ua["prefixStability"];
            assert_eq!(ps["observations"], agg.observations);
            let near = |x: f64, y: f64| {
                (x - y).abs() <= 2.0 * f64::EPSILON * x.abs().max(y.abs()).max(1.0)
            };
            assert!(
                near(ps["mean"].as_f64().unwrap(), agg.mean),
                "mean differs by more than 1 ulp: {:?} vs {:?}",
                ps["mean"],
                agg.mean
            );
            assert!(
                near(ps["stdDev"].as_f64().unwrap(), agg.std_dev),
                "stdDev differs: {:?} vs {:?}",
                ps["stdDev"],
                agg.std_dev
            );
            // Task entries: durable budget envelope + reservation rows.
            let tasks = ua["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 2, "{ua}");
            let t1 = &tasks[0];
            assert_eq!(t1["taskId"], "1");
            assert_eq!(t1["budget"]["maxTokens"], 5000);
            assert_eq!(t1["budget"]["maxTurns"], 3);
            assert_eq!(t1["budget"]["spentTokens"], 0);
            assert_eq!(t1["budget"]["spentCostMicro"], 1000);
            assert_eq!(t1["budget"]["maxCostMicro"], 1_000_000);
            assert_eq!(t1["budget"]["openReservedMicro"], 200);
            let res = &t1["reservations"];
            assert_eq!(res["open"]["count"], 1);
            assert_eq!(res["open"]["predictedMicro"], 200);
            assert_eq!(res["settled"]["count"], 1);
            assert_eq!(res["settled"]["predictedMicro"], 5000);
            assert_eq!(res["settled"]["spentMicro"], 1000);
            assert_eq!(res["settled"]["providerReportedMicro"], 990);
            assert_eq!(res["refunded"]["count"], 1);
            assert_eq!(res["refunded"]["predictedMicro"], 3000);
            assert_eq!(res["uncertain"]["count"], 0);
            let routes = res["routeDecisions"].as_array().unwrap();
            assert_eq!(routes.len(), 1);
            assert_eq!(routes[0]["reservationId"], r1);
            assert_eq!(routes[0]["spentMicro"], 1000);
            assert_eq!(routes[0]["decision"]["provider"], "fake");
            assert_eq!(routes[0]["decision"]["estimated_cost_micro"], 90);
            // Untouched task 2: null-safe pre-first-reservation budget.
            let t2 = &tasks[1];
            assert_eq!(t2["taskId"], "2");
            assert_eq!(t2["budget"]["maxTokens"], 1000);
            assert_eq!(t2["budget"]["maxCostMicro"], serde_json::Value::Null);
            assert_eq!(t2["budget"]["spentCostMicro"], 0);
            assert_eq!(t2["budget"]["openReservedMicro"], 0);
            assert_eq!(t2["reservations"]["settled"]["count"], 0);

            // ---- isolation: B's usage never carries A's rows.
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/native/session/{}/usage", b.id()),
            )
            .await;
            let ub: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(ub["providerCalls"]["tokens"], 10, "B only sees its call");
            assert_eq!(
                ub["providerCalls"]["prefixObservations"],
                serde_json::json!([]),
                "B recorded no prefix"
            );
            assert_eq!(ub["tasks"][0]["taskId"], "1");
            assert_eq!(ub["tasks"][0]["budget"]["spentCostMicro"], 40);
            assert_eq!(ub["tasks"][0]["reservations"]["settled"]["count"], 1);
            assert_eq!(ub["tasks"][0]["reservations"]["settled"]["spentMicro"], 40);
            assert_eq!(ub["tasks"].as_array().unwrap().len(), 1);

            // ---- global aggregate: durable numbers over every session.
            let resp = native_get(&client, &base, &token, "/native/usage").await;
            assert_eq!(resp.status(), 200);
            let gu: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(gu["sessions"], 2);
            assert_eq!(gu["durable"]["providerCalls"]["tokens"], 1560);
            assert_eq!(gu["durable"]["providerCalls"]["prefixObservations"], 2);
            assert_eq!(gu["durable"]["providerCalls"]["prefixTokens"], 860);
            assert_eq!(
                gu["durable"]["providerCalls"]["prefixStabilityObservations"],
                2
            );
            assert_eq!(gu["durable"]["taskSpend"]["settledCostMicro"], 1040);
            let res = &gu["durable"]["reservations"];
            assert_eq!(res["settled"]["count"], 2);
            assert_eq!(res["settled"]["spentMicro"], 1040);
            assert_eq!(res["settled"]["providerReportedMicro"], 990);
            assert_eq!(res["refunded"]["count"], 1);
            assert_eq!(res["refunded"]["predictedMicro"], 3000);
            assert_eq!(res["open"]["count"], 1);
            assert_eq!(res["open"]["predictedMicro"], 200);
            assert_eq!(res["uncertain"]["count"], 0);

            // ---- crash-recovery semantics (schema v17+): a crash closes
            // every surviving in-flight reservation split on the durable
            // dispatch marker — this one never left the process
            // (`reserved`, marker NULL), so recovery REFUNDS it (never
            // spent, its prediction released); only dispatched-marker rows
            // go UNCERTAIN. The aggregate follows.
            store.cost_abandon_open_reservations(now + 5).unwrap();
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/native/session/{}/usage", a.id()),
            )
            .await;
            let ua_after: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(ua_after["tasks"][0]["budget"]["openReservedMicro"], 0);
            assert_eq!(ua_after["tasks"][0]["budget"]["spentCostMicro"], 1000);
            assert_eq!(ua_after["tasks"][0]["reservations"]["open"]["count"], 0);
            assert_eq!(
                ua_after["tasks"][0]["reservations"]["uncertain"]["count"],
                0
            );
            assert_eq!(ua_after["tasks"][0]["reservations"]["refunded"]["count"], 2);
            assert_eq!(
                ua_after["tasks"][0]["reservations"]["refunded"]["predictedMicro"],
                3200
            );
            let resp = native_get(&client, &base, &token, "/native/usage").await;
            let gu_after: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(gu_after["durable"]["reservations"]["refunded"]["count"], 2);
            assert_eq!(gu_after["durable"]["reservations"]["uncertain"]["count"], 0);

            // Capture the authoritative snapshots for the reopen check.
            let expected_a = ua_after;
            let expected_global = gu_after;
            let _ = handle.shutdown.send(());
            (expected_a, expected_global, a.id())
        };
        // ---- reopen durability: a brand-new manager (and server) over the
        // same data root serves the IDENTICAL usage JSON.
        let deps2 = test_deps(dir.path());
        let token2 = deps2.auth_token.clone();
        let handle2 = serve(deps2, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base2 = format!("http://{}", handle2.addr);
        let resp = native_get(
            &client,
            &base2,
            &token2,
            &format!("/native/session/{sid_a}/usage"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let reopened: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(reopened, expected_a, "usage survives a reopen exactly");
        let resp = native_get(&client, &base2, &token2, "/native/usage").await;
        let reopened_global: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(reopened_global, expected_global);
        let _ = handle2.shutdown.send(());
    }

    /// Provider for the real-turn usage test (P0-63): a canonical usage
    /// frame with zero uncached input, only cache reads/writes + output
    /// (anthropic-style split semantics) plus a provider-reported USD cost.
    /// The runtime settlement prices each line and folds the categories
    /// into the recorded input/output totals.
    #[derive(Clone)]
    struct CacheUsageProvider;

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
    fn test_deps_full(
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
        }
    }

    #[tokio::test]
    async fn native_usage_real_turn_with_cache_and_reasoning_tokens() {
        // P0-63 end-to-end: a REAL turn through the wire surface against a
        // provider whose usage frame carries only cache reads/writes +
        // reasoning tokens and a provider-reported cost. The runtime
        // settlement persists the folded totals and the reservation rows;
        // /native/session/{id}/usage then reports EXACTLY the stored rows.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps_full(dir.path(), vec![Arc::new(CacheUsageProvider)]);
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let tasks = deps.tasks.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/usage-e2e").unwrap();
        let s = manager
            .create_session(ws, "t-usage-e2e", "fake", "m")
            .unwrap();
        let sid = s.id().to_string();
        seed_typed_task(&s, 1, None, None, "usage-e2e");
        // The session row task identity drives the reserve (task 1 row).
        assert_eq!(s.task_id().unwrap().raw(), 1);

        // Drive the turn through the ONE executor entry ordinary prompts
        // use (the same edge the HTTP handler reaches).
        let service = crate::native::PromptExecutionService::new(tasks, manager.clone());
        service
            .prompt(
                s.id(),
                crate::native::PromptRequest {
                    prompt: "hi".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut body = serde_json::Value::Null;
        for _ in 0..300 {
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/session/{sid}/projection"),
            )
            .await;
            assert_eq!(resp.status(), 200);
            body = resp.json().await.unwrap();
            if body["state"]["machine"] == "ready_for_next_turn" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(body["state"]["machine"], "ready_for_next_turn", "{body}");

        // The stored rows: folded cache/reasoning totals, prefix observation,
        // one settled reservation with the reported cost.
        let store = manager.store();
        let tokens = store.session_usage_tokens(s.id()).unwrap();
        assert_eq!(
            tokens, 12,
            "cache reads 7 + writes 2 + reasoning 3 (in=9,out=3)"
        );
        let prefix = store.provider_call_prefix_rows(s.id()).unwrap();
        assert_eq!(
            prefix.len(),
            1,
            "the completed call recorded its prefix: {prefix:?}"
        );
        let cost = store
            .cost_task_row(s.id(), faktor_core::id::TaskId::new(1))
            .unwrap()
            .unwrap();
        assert_eq!(cost.spent_cost_micro, 123, "provider-reported cost wins");
        let reservations = store
            .cost_reservations_of(s.id(), faktor_core::id::TaskId::new(1), 10)
            .unwrap();
        assert_eq!(reservations.len(), 1);
        assert_eq!(reservations[0].status, "settled");
        // The passthrough test policy consulted NO pricing authority (its
        // decision snapshot is None), so there is no honest locally
        // calculated amount: the provider-reported cost is the ONLY amount
        // and wins both the v18 canonical columns (`settled_cost_micro`,
        // `provider_reported_cost_micro`) and the folded task spend. The
        // pre-B2 "tokens x 1 microUSD local estimate" was abolished — a
        // fabricated number never lands next to a real report.
        assert_eq!(reservations[0].provider_reported_cost_micro, Some(123));
        assert_eq!(reservations[0].provider_reported_micro, Some(123));
        assert_eq!(reservations[0].settled_cost_micro, Some(123));
        assert_eq!(reservations[0].provider_cost_micro, None);
        assert_eq!(
            reservations[0].cost_basis.as_deref(),
            Some(faktor_store::COST_BASIS_PROVIDER_REPORTED)
        );

        // The endpoint reports exactly the stored rows.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/usage"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let u: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(u["providerCalls"]["tokens"], tokens);
        let obs = u["providerCalls"]["prefixObservations"].as_array().unwrap();
        assert_eq!(obs.len(), prefix.len());
        assert_eq!(obs[0]["promptTokens"], prefix[0].prompt_tokens as u64);
        assert_eq!(
            obs[0]["stability"],
            prefix[0]
                .prefix_stability
                .map(|v| serde_json::json!(v))
                .unwrap_or(serde_json::Value::Null)
        );
        let tasks = u["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["budget"]["spentCostMicro"], cost.spent_cost_micro);
        assert_eq!(tasks[0]["budget"]["maxCostMicro"], serde_json::Value::Null);
        assert_eq!(tasks[0]["budget"]["openReservedMicro"], 0);
        let res = &tasks[0]["reservations"];
        assert_eq!(res["settled"]["count"], 1);
        assert_eq!(
            res["settled"]["spentMicro"], 123,
            "the folded actual (provider-reported) is what was spent"
        );
        assert_eq!(res["settled"]["providerReportedMicro"], 123);
        let routes = res["routeDecisions"].as_array().unwrap();
        assert!(!routes.is_empty(), "the routed call records its decision");
        assert_eq!(routes[0]["providerReportedMicro"], 123);
        assert_eq!(routes[0]["spentMicro"], 123);
        assert!(
            routes[0]["decision"]["provider"] == "fake" || routes[0]["decision"].is_object(),
            "{routes:?}"
        );

        // Usage of a never-used sibling session is empty, never A's rows.
        let ws2 = manager.create_workspace("/usage-e2e-b").unwrap();
        let b = manager
            .create_session(ws2, "t-usage-e2e-b", "fake", "m")
            .unwrap();
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/usage", b.id()),
        )
        .await;
        let ub: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ub["providerCalls"]["tokens"], 0);
        assert_eq!(ub["tasks"], serde_json::json!([]));
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_verification_evidence_scoped_to_session_and_task() {
        // P0-64e: /native/session/{id}/tasks/{task_id}/verification returns
        // the durable VerificationRecord rows (checks/criteria/changed
        // files) of the session's OWN task only. Another session querying
        // the same numeric task id gets a typed 404 or an empty list when a
        // different workspace holds records under that id — evidence never
        // crosses sessions.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws_a = manager.create_workspace("/ver-a").unwrap();
        let ws_b = manager.create_workspace("/ver-b").unwrap();
        let a = manager
            .create_session(ws_a, "t-ver-a", "fake", "m")
            .unwrap();
        let b = manager
            .create_session(ws_b, "t-ver-b", "fake", "m")
            .unwrap();
        // C shares B's workspace and (for the cross-workspace guard) adopts
        // the SAME numeric task id A owns — yet C must never see A's
        // evidence rows, which certify A's workspace.
        let c = manager
            .create_session(ws_b, "t-ver-c", "fake", "m")
            .unwrap();
        manager
            .adopt_identity(
                a.id(),
                faktor_core::WorktreeId::new(3),
                faktor_core::id::TaskId::new(7),
            )
            .unwrap();
        manager
            .adopt_identity(
                b.id(),
                faktor_core::WorktreeId::new(4),
                faktor_core::id::TaskId::new(9),
            )
            .unwrap();
        manager
            .adopt_identity(
                c.id(),
                faktor_core::WorktreeId::new(5),
                faktor_core::id::TaskId::new(7),
            )
            .unwrap();
        let store = manager.store();
        let row_a = a.row().unwrap();
        let row_b = b.row().unwrap();
        let rev = faktor_core::id::TaskRevision::new(1);
        // A's record carries the full v20 candidate-proof evidence; B's is a
        // legacy NULL-evidence row (both must project honestly).
        let candidate = faktor_core::state::CandidateProofRef {
            task_revision: rev,
            base_manifest_hash: "11".repeat(32),
            candidate_manifest_hash: "22".repeat(32),
            source_diff_evidence: None,
            risk_report_evidence: None,
            accounting_snapshot_digest: "accounting:v1:feedfacefeedface".into(),
            run_id: Some("run-ver-a".into()),
            run_base_snapshot: Some("33".repeat(32)),
            candidate_snapshot: Some("44".repeat(32)),
            sources_digest: Some("55".repeat(32)),
            changed_files_digest: Some("66".repeat(32)),
        };
        let candidate_json = serde_json::to_string(&candidate).unwrap();
        let put =
            |row: &faktor_store::VerificationRecordRow| store.verification_record_put(row).unwrap();
        let put_with_evidence = |row: &faktor_store::VerificationRecordRow| {
            store
                .verification_record_put_with_evidence(row, None, Some(&candidate_json))
                .unwrap()
        };
        let rec_for = |session_row: &faktor_store::SessionRow, task_id: u64, check: &str| {
            faktor_store::VerificationRecordRow {
                id: faktor_core::id::VerificationRecordId::new(1),
                task_id: faktor_core::id::TaskId::new(task_id),
                revision: rev,
                workspace_id: session_row.workspace_id,
                worktree_id: session_row.worktree_id,
                tree_hash: Some("ab".repeat(32)),
                criteria: vec![faktor_core::state::CriterionVerification {
                    criterion_key: "tests pass".into(),
                    passed: true,
                    evidence: Some("ran".into()),
                    binding: None,
                }],
                checks: vec![faktor_core::state::CheckExecution {
                    check: check.into(),
                    program: "cargo".into(),
                    args: vec!["test".into()],
                    category: "required".into(),
                    required: true,
                    status: faktor_core::state::VerificationStatus::Passed,
                    started_ms: 1,
                    finished_ms: Some(2),
                    exit: Some(0),
                    summary: Some("ok".into()),
                }],
                changed_files: vec![faktor_core::state::FileStateEvidence {
                    path: "crates/server/src/api.rs".into(),
                    digest_hex: "cd".repeat(32),
                    size: 42,
                }],
                unrelated_changes: vec!["README.md".into()],
                reviewer: None,
                status: faktor_core::state::VerificationStatus::Passed,
                started_ms: 1,
                completed_ms: Some(2),
            }
        };
        let ra = put_with_evidence(&rec_for(&row_a, 7, "cargo test -p faktor-session"));
        let rb = put(&rec_for(&row_b, 9, "cargo test -p faktor-server"));
        // A's integration record: 3 aggregate sources landing the candidate
        // snapshot (the proof payload's source count + landed snapshot).
        a.ledger_integration_record_set(&faktor_session::ledger::IntegrationRecordRow {
            run_id: "run-ver-a".into(),
            task_id: 7,
            base_revision: None,
            base_snapshot: Some("33".repeat(32)),
            run_base_snapshot: Some("33".repeat(32)),
            candidate_snapshot: Some("44".repeat(32)),
            landed_snapshot: Some("44".repeat(32)),
            proof_basis_digest: None,
            integration_txn_id: None,
            final_root: "/ver-a".into(),
            final_snapshot_hash: "44".repeat(32),
            integrated_files: vec!["src/lib.rs".into()],
            integrated_file_count: 1,
            integrated_files_digest: "77".repeat(32),
            conflicts: Vec::new(),
            conflict_count: 0,
            sources: Vec::new(),
            source_count: 3,
            sources_digest: "55".repeat(32),
            at_ms: 3,
        })
        .unwrap();

        // A's task-7 evidence: checks/criteria/changed files all present.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/tasks/7/verification", a.id()),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["sessionId"], a.id().to_string());
        assert_eq!(body["taskId"], "7");
        let records = body["records"].as_array().unwrap();
        assert_eq!(records.len(), 1, "{body}");
        let rec = &records[0];
        assert_eq!(rec["recordId"], ra.to_string());
        assert_eq!(rec["revision"], "1");
        assert_eq!(rec["status"], "passed");
        assert_eq!(rec["criteria"][0]["criterionKey"], "tests pass");
        assert_eq!(rec["criteria"][0]["passed"], true);
        assert_eq!(rec["checks"][0]["check"], "cargo test -p faktor-session");
        assert_eq!(rec["checks"][0]["program"], "cargo");
        assert_eq!(rec["checks"][0]["args"], serde_json::json!(["test"]));
        assert_eq!(rec["checks"][0]["status"], "passed");
        assert_eq!(rec["checks"][0]["exit"], 0);
        assert_eq!(rec["changedFiles"][0]["path"], "crates/server/src/api.rs");
        assert_eq!(rec["changedFiles"][0]["size"], 42);
        assert_eq!(rec["unrelatedChanges"], serde_json::json!(["README.md"]));
        assert_eq!(rec["completedMs"], 2);
        // P0 proof payload (additive strict fields): the CandidateProofRef,
        // the verified candidate snapshot, the run base it was based on, the
        // integration source count and the landed final snapshot.
        assert_eq!(rec["candidateProof"]["taskRevision"], "1");
        assert_eq!(rec["candidateProof"]["runId"], "run-ver-a");
        assert_eq!(rec["candidateProof"]["candidateSnapshot"], "44".repeat(32));
        assert_eq!(rec["candidateProof"]["runBaseSnapshot"], "33".repeat(32));
        assert_eq!(rec["candidateProof"]["sourcesDigest"], "55".repeat(32));
        assert_eq!(rec["candidateProof"]["changedFilesDigest"], "66".repeat(32));
        assert_eq!(rec["verifiedSnapshot"], "44".repeat(32));
        assert_eq!(rec["basedOnSnapshot"], "33".repeat(32));
        assert_eq!(rec["sourceCount"], 3);
        assert_eq!(rec["landedSnapshot"], "44".repeat(32));

        // B never sees A's task-7 evidence: 7 is not B's task → typed 404.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/tasks/7/verification", b.id()),
        )
        .await;
        assert_eq!(resp.status(), 404);
        let err: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(err["error"]["code"], "not_found");
        // A cannot read B's task-9 records either.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/tasks/9/verification", a.id()),
        )
        .await;
        assert_eq!(resp.status(), 404);
        // B's OWN task 9 serves its record.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/tasks/9/verification", b.id()),
        )
        .await;
        let body: serde_json::Value = resp.json().await.unwrap();
        let records = body["records"].as_array().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["recordId"], rb.to_string());
        assert_eq!(
            records[0]["checks"][0]["check"],
            "cargo test -p faktor-server"
        );
        // Legacy NULL-evidence row: the P0 fields are honest absences (the
        // verified snapshot falls back to the stored tree hash; no
        // integration exists for task 9).
        assert_eq!(records[0]["candidateProof"], serde_json::Value::Null);
        assert_eq!(records[0]["verifiedSnapshot"], "ab".repeat(32));
        assert_eq!(records[0]["basedOnSnapshot"], serde_json::Value::Null);
        assert_eq!(records[0]["sourceCount"], serde_json::Value::Null);
        assert_eq!(records[0]["landedSnapshot"], serde_json::Value::Null);
        // C (another workspace, SAME numeric task id 7) cannot reach A's
        // record: records certify A's workspace, so C's view is the honest
        // empty list — evidence never crosses sessions or workspaces.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/tasks/7/verification", c.id()),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["records"],
            serde_json::json!([]),
            "records of another workspace never surface: {body}"
        );

        // Hostile ids, unknown sessions, auth.
        for path in [
            format!("/native/session/{}/tasks/0/verification", a.id()),
            format!("/native/session/{}/tasks/abc/verification", a.id()),
            "/native/session/abc/tasks/7/verification".to_string(),
            "/native/session/0/tasks/7/verification".to_string(),
        ] {
            let resp = native_get(&client, &base, &token, &path).await;
            assert_eq!(resp.status(), 400, "{path}");
        }
        let resp = native_get(
            &client,
            &base,
            &token,
            "/native/session/999999/tasks/7/verification",
        )
        .await;
        assert_eq!(resp.status(), 404);
        let resp = client
            .get(format!(
                "{base}/native/session/{}/tasks/7/verification",
                a.id()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_verification_serves_criterion_binding_origin_requirement_and_verdict() {
        // P0 UI proof blockers: every criterion row of the task-verification
        // projection additively carries the typed binding (kind + members),
        // the flat binding kind, the three-way verdict
        // (pass|fail|unavailable) alongside the recorded `passed` boolean,
        // and — when the session's typed task row holds the criterion — its
        // origin and requirement. All SEVEN binding kinds render; an
        // unbound (legacy) row is `unavailable`, which is distinct from
        // both `fail` and `pass`.
        use faktor_core::state::{
            CriterionBinding, CriterionOrigin, CriterionRequirement, TaskState,
        };
        use faktor_session::task::{encode_criteria, Criterion};

        struct Spec {
            key: &'static str,
            binding: CriterionBinding,
            passed: bool,
            origin: CriterionOrigin,
            requirement: CriterionRequirement,
        }

        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/ver-proof").unwrap();
        let s = manager
            .create_session(ws, "t-ver-proof", "fake", "m")
            .unwrap();
        let task_id = s.task_id().unwrap();

        let specs = [
            Spec {
                key: "required check proof",
                binding: CriterionBinding::RequiredCheck {
                    check_id: "rust_check".into(),
                    command_digest: "digest-check".into(),
                },
                passed: true,
                origin: CriterionOrigin::User,
                requirement: CriterionRequirement::Required,
            },
            Spec {
                key: "integration coverage proof",
                binding: CriterionBinding::IntegrationCoverage {
                    required_work_items: vec!["impl-a".into(), "impl-b".into()],
                },
                passed: true,
                origin: CriterionOrigin::ProjectPolicy,
                requirement: CriterionRequirement::Required,
            },
            Spec {
                key: "file state proof",
                binding: CriterionBinding::FileState {
                    path: "src/a.rs".into(),
                    expected_digest: "digest-file".into(),
                },
                passed: true,
                origin: CriterionOrigin::VerificationPolicy,
                requirement: CriterionRequirement::Required,
            },
            Spec {
                key: "evidence proof",
                binding: CriterionBinding::Evidence {
                    evidence_id: "41".into(),
                    evidence_digest: "digest-evidence".into(),
                },
                passed: true,
                origin: CriterionOrigin::SemanticProvider,
                requirement: CriterionRequirement::Preferred,
            },
            Spec {
                key: "independent review proof",
                binding: CriterionBinding::IndependentReview {
                    reviewer_id: "reviewer-1".into(),
                },
                passed: true,
                origin: CriterionOrigin::ProjectPolicy,
                requirement: CriterionRequirement::Required,
            },
            Spec {
                key: "aggregate goal proof",
                binding: CriterionBinding::AggregateGoal,
                passed: false,
                origin: CriterionOrigin::VerificationPolicy,
                requirement: CriterionRequirement::Preferred,
            },
            Spec {
                key: "explicitly unavailable proof",
                binding: CriterionBinding::Unavailable {
                    reason: "no objective mechanism".into(),
                },
                passed: true,
                origin: CriterionOrigin::User,
                requirement: CriterionRequirement::Required,
            },
        ];
        let typed: Vec<Criterion> = specs
            .iter()
            .map(|spec| {
                Criterion::derived(spec.key, spec.origin, spec.requirement, None)
                    .with_binding(spec.binding.clone())
            })
            .collect();
        let now = s.now_ms();
        s.create_task(faktor_session::Task {
            task_id,
            session_id: s.id(),
            goal: "prove every binding kind".into(),
            acceptance_criteria: encode_criteria(&typed),
            plan: Vec::new(),
            attachments: Vec::new(),
            budget: Default::default(),
            state: TaskState::Running,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();

        let row = s.row().unwrap();
        let mut criteria: Vec<faktor_core::state::CriterionVerification> = specs
            .iter()
            .map(|spec| faktor_core::state::CriterionVerification {
                criterion_key: spec.key.into(),
                passed: spec.passed,
                evidence: Some(format!("evidence for {}", spec.key)),
                binding: Some(spec.binding.clone()),
            })
            .collect();
        // A legacy unbound row: the recorded boolean contract survives, the
        // new three-way verdict must honestly say `unavailable`.
        criteria.push(faktor_core::state::CriterionVerification {
            criterion_key: "legacy unbound proof".into(),
            passed: true,
            evidence: None,
            binding: None,
        });
        let record = faktor_store::VerificationRecordRow {
            id: faktor_core::id::VerificationRecordId::new(1),
            task_id,
            revision: faktor_core::id::TaskRevision::new(1),
            workspace_id: row.workspace_id,
            worktree_id: row.worktree_id,
            tree_hash: Some("ab".repeat(32)),
            criteria,
            checks: Vec::new(),
            changed_files: Vec::new(),
            unrelated_changes: Vec::new(),
            reviewer: None,
            status: faktor_core::state::VerificationStatus::Passed,
            started_ms: 11,
            completed_ms: Some(22),
        };
        manager.store().verification_record_put(&record).unwrap();

        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/tasks/{}/verification", s.id(), task_id),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let records = body["records"].as_array().unwrap();
        assert_eq!(records.len(), 1, "{body}");
        let rec = &records[0];
        assert_eq!(rec["startedMs"], 11);
        assert_eq!(rec["completedMs"], 22);
        let rows = rec["criteria"].as_array().unwrap();
        assert_eq!(rows.len(), specs.len() + 1);
        let find = |key: &str| -> serde_json::Value {
            rows.iter()
                .find(|r| r["criterionKey"] == key)
                .unwrap_or_else(|| panic!("criterion {key} must be served"))
                .clone()
        };

        // All seven binding kinds with their served origin/requirement and
        // the three-way verdict.
        let expected: [(&str, &str, &str, &str, &str); 7] = [
            (
                "required check proof",
                "required_check",
                "user",
                "required",
                "pass",
            ),
            (
                "integration coverage proof",
                "integration_coverage",
                "project_policy",
                "required",
                "pass",
            ),
            (
                "file state proof",
                "file_state",
                "verification_policy",
                "required",
                "pass",
            ),
            (
                "evidence proof",
                "evidence",
                "semantic_provider",
                "preferred",
                "pass",
            ),
            (
                "independent review proof",
                "independent_review",
                "project_policy",
                "required",
                "pass",
            ),
            (
                "aggregate goal proof",
                "aggregate_goal",
                "verification_policy",
                "preferred",
                "fail",
            ),
            (
                "explicitly unavailable proof",
                "unavailable",
                "user",
                "required",
                "unavailable",
            ),
        ];
        for (key, kind, origin, requirement, verdict) in expected {
            let row = find(key);
            assert_eq!(row["bindingKind"], kind, "{row}");
            assert_eq!(row["origin"], origin, "{row}");
            assert_eq!(row["requirement"], requirement, "{row}");
            assert_eq!(row["verdict"], verdict, "{row}");
            assert_eq!(row["binding"]["kind"], kind, "{row}");
        }
        // The typed binding members ride the wire in the serde shape.
        assert_eq!(
            find("required check proof")["binding"]["check_id"],
            "rust_check"
        );
        assert_eq!(
            find("required check proof")["binding"]["command_digest"],
            "digest-check"
        );
        assert_eq!(
            find("integration coverage proof")["binding"]["required_work_items"],
            serde_json::json!(["impl-a", "impl-b"])
        );
        assert_eq!(find("file state proof")["binding"]["path"], "src/a.rs");
        assert_eq!(find("evidence proof")["binding"]["evidence_id"], "41");
        assert_eq!(
            find("independent review proof")["binding"]["reviewer_id"],
            "reviewer-1"
        );
        assert_eq!(
            find("explicitly unavailable proof")["binding"]["reason"],
            "no objective mechanism"
        );
        // A verdict without a binding (legacy row) is unavailable, never a
        // pass — the recorded `passed` boolean stays alongside it.
        let legacy = find("legacy unbound proof");
        assert_eq!(legacy["passed"], true);
        assert_eq!(legacy["binding"], serde_json::Value::Null);
        assert_eq!(legacy["bindingKind"], "unavailable");
        assert_eq!(legacy["verdict"], "unavailable");
        assert_ne!(legacy["verdict"], "pass");
        // The three-way verdict distinguishes fail from unavailable AND
        // pass from unavailable.
        assert_ne!(find("explicitly unavailable proof")["verdict"], "fail");
        assert_ne!(find("explicitly unavailable proof")["verdict"], "pass");
        assert_ne!(find("aggregate goal proof")["verdict"], "unavailable");
        assert_ne!(find("aggregate goal proof")["verdict"], "pass");
        // No typed task criterion matches the legacy row: honest nulls.
        assert_eq!(legacy["origin"], serde_json::Value::Null);
        assert_eq!(legacy["requirement"], serde_json::Value::Null);

        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_tasks_carry_progress_and_durable_budget() {
        // P0-64d: the /native/session/{id}/tasks entry additively carries
        // `progress` (the live bounded progress record, null when nothing
        // ran) and `budget` — the DURABLE budget envelope of the typed task
        // row (token + cost-ledger columns and the open reservation sum).
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/task-budget").unwrap();
        let s = manager
            .create_session(ws, "t-task-budget", "fake", "m")
            .unwrap();
        let sid = s.id().to_string();
        let h = manager.get_session(s.id()).unwrap().unwrap();
        // Durable task ledger (the tasks endpoint's base) + a typed row with
        // a budget + one open and one settled reservation.
        h.put_task_ledger(serde_json::json!({
            "goal": "wire durable budgets",
            "completed_steps": ["mount endpoints"],
            "open_steps": ["ship"],
            "changed_files": ["crates/server/src/api.rs"],
        }))
        .unwrap();
        seed_typed_task(&h, 1, Some(8000), Some(4), "wire durable budgets");
        let store = manager.store();
        store
            .cost_task_cap_set(s.id(), faktor_core::id::TaskId::new(1), Some(250_000))
            .unwrap();
        let now = manager.now_ms();
        store
            .cost_reserve(
                s.id(),
                faktor_core::id::TaskId::new(1),
                manager.next_op_id(),
                60,
                now,
            )
            .unwrap();
        let faktor_store::CostReserveOutcome::Granted(settled_id) = store
            .cost_reserve(
                s.id(),
                faktor_core::id::TaskId::new(1),
                manager.next_op_id(),
                500,
                now,
            )
            .unwrap()
        else {
            panic!("settled reserve granted");
        };
        store
            .cost_settle(settled_id, 100, Some(100), None, None, now + 1)
            .unwrap();

        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/tasks"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let tasks = body.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        let entry = &tasks[0];
        assert!(
            entry.get("progress").is_some(),
            "progress key present: {entry}"
        );
        assert_eq!(entry["budget"]["maxTokens"], 8000);
        assert_eq!(entry["budget"]["maxTurns"], 4);
        assert_eq!(entry["budget"]["spentTokens"], 0);
        assert_eq!(entry["budget"]["spentTurns"], 0);
        assert_eq!(entry["budget"]["maxCostMicro"], 250_000);
        assert_eq!(entry["budget"]["spentCostMicro"], 100);
        assert_eq!(entry["budget"]["openReservedMicro"], 60);
        let _ = handle.shutdown.send(());
    }

    // ---------------------------------------- max_cost_micro task control E2E
    // (audit 9/H: TaskRunRequest.max_cost_micro flows to the task row cap and a
    // REAL drive whose first model-call reserve exceeds the cap fails with the
    // typed budget refusal — nothing is reserved, nothing is spent.)

    #[tokio::test]
    async fn native_single_item_task_max_cost_micro_caps_the_real_drive() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps_full(dir.path(), vec![Arc::new(CacheUsageProvider)]);
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let tasks = deps.tasks.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/cap-e2e").unwrap();
        let s = manager
            .create_session(ws, "t-cap-e2e", "fake", "m")
            .unwrap();
        let sid = s.id();
        // A 1-micro cap: every real reserve of the drive (the route estimate is
        // far larger) is refused at admission — the refusal writes NOTHING.
        let req = faktor_orchestrator::runtime::task_executor::TaskRunRequest {
            goal: "spend against the cost cap".into(),
            work_items: vec![faktor_orchestrator::WorkItem::new(
                "a1",
                "spend against the cost cap",
                faktor_orchestrator::WorkKind::Analysis,
            )],
            max_cost_micro: Some(1),
            ..Default::default()
        };
        let receipt = tasks.start_task(sid, req).expect("single-item start");
        assert_eq!(
            receipt.mode,
            faktor_orchestrator::runtime::task_executor::TaskRunMode::InSession
        );
        // The cap was durable before the detached drive ran its first call...
        let h = manager.get_session(sid).unwrap().unwrap();
        let task_id = h.task_id().unwrap();
        let ledger = faktor_session::DurableBudgetLedger::new(manager.clone());
        assert_eq!(
            ledger
                .session_budget_view(sid, task_id)
                .expect("durable budget view")
                .max_cost_micro,
            Some(1),
            "the request cap landed on the task row"
        );
        // ...and the drive ends FailedRecoverable (budget exceeded): the spy
        // provider was never billed — no reservation row, zero spent.
        let mut seen = None;
        for _ in 0..240 {
            let state = h.state().unwrap();
            if state == faktor_core::state::AgentState::FailedRecoverable {
                seen = Some(state);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            seen,
            Some(faktor_core::state::AgentState::FailedRecoverable),
            "the capped drive must fail recoverable, never silently spend"
        );
        let view = ledger
            .session_budget_view(sid, task_id)
            .expect("durable budget view");
        assert_eq!(view.max_cost_micro, Some(1));
        assert_eq!(view.spent_cost_micro, 0, "a refused reserve spends nothing");
        assert_eq!(view.open_reservations, 0, "a refused reserve writes no row");
        assert_eq!(view.uncertain_reservations, 0);
        // The native agent listing reflects the failed run.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/agents?session={sid}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let entries: serde_json::Value = resp.json().await.unwrap();
        let e = &entries.as_array().unwrap()[0];
        assert_eq!(e["kind"], "self");
        assert_eq!(e["state"], "Failed");
        let _ = handle.shutdown.send(());
    }

    // =========================================== native task runs (wave-24)
    // The native task-start surface: POST /native/session/{id}/task-runs is
    // the ONE HTTP edge into TaskExecutor::start_task (shadow mutation is
    // the production default; DirectCompat keeps the byte-identical direct
    // behavior), GET list/state read the durable runs, and the task-level
    // cancel is the executor's single cancel authority. Tests below drive
    // REAL shadowed worktrees through the HTTP layer, attack the strict
    // DTO, freeze the parity of DirectCompat, and source-scan the crate for
    // any second task-start edge.

    /// Session-scoped workspace root provider mirroring the daemon graph:
    /// the live shadow of a shadowed workspace re-points instruction
    /// loading at the shadow root.
    struct NativeRealRoots(Arc<SessionManager>);
    impl faktor_instructions::WorkspaceRootProvider for NativeRealRoots {
        fn workspace_root(&self, workspace_id: u64) -> Option<std::path::PathBuf> {
            use faktor_core::id::WorkspaceId;
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

    /// A REAL write_file: writes through the session's resolved workspace
    /// root (the shadow root while a shadowed drive is live). `park` parks
    /// the FIRST invocation after the write landed (the deterministic
    /// mid-drive window of the shadowed HTTP tests).
    fn native_real_write_tool(
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
    fn native_parking_tool(
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
    struct NativeTaskRig {
        deps: ServerDeps,
        manager: Arc<SessionManager>,
        parent: SessionId,
        owner_root: std::path::PathBuf,
        gate: Arc<tokio::sync::Notify>,
        fired: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn native_task_rig(
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
    fn native_task_rig_verified(
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
    fn native_task_rig_with_provider(
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

    fn native_task_rig_with_provider_and_verification(
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
            );
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

    fn seed_native_owner(root: &std::path::Path) {
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
    const NATIVE_OWNER_LIB_RS: &str = "pub fn value() -> u64 {\n    let base_amount: u64 = 40;\n    let increment: u64 = 1;\n    base_amount.saturating_add(increment)\n}\n";
    const NATIVE_IMPL_LIB_RS: &str = "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n";

    async fn native_wait_session_state(
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
    fn certify_native_task(manager: &Arc<SessionManager>, sid: SessionId) {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_task_run_start_shadowed_drive_keeps_checkout_then_integrates() {
        // The wave-24 E2E through the HTTP layer: POST starts a mutating
        // single-item task (production default: shadowed) that really
        // drives; the run's write lands in the SHADOW while the owner
        // checkout stays byte-untouched MID-drive; the run listing/state
        // endpoints reflect it; a verified completion integrates the owner
        // checkout through the executor's own settle paths (never a manual
        // finalize call from the test).
        let dir = tempfile::tempdir().unwrap();
        let rig = native_task_rig_verified(
            dir.path(),
            vec![
                vec![
                    faktor_provider::ScriptedResponse::ToolCall {
                        id: "c1".into(),
                        name: "write_file".into(),
                        input: serde_json::json!({
                            "path": "src/lib.rs",
                            "content": NATIVE_IMPL_LIB_RS,
                        }),
                    },
                    faktor_provider::ScriptedResponse::Text("done".into()),
                    faktor_provider::ScriptedResponse::End,
                ],
                vec![faktor_provider::ScriptedResponse::End],
            ],
            true,
            true,
        );
        seed_native_owner(&rig.owner_root);
        let NativeTaskRig {
            deps,
            manager,
            parent: sid,
            owner_root,
            gate,
            fired,
        } = rig;
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);

        // POST the strict native start (goal only: absent work_items = one
        // MUTATING main item; absent mutation_mode = the daemon default
        // Shadow).
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"goal": "implement the change"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let start: serde_json::Value = resp.json().await.unwrap();
        let run_id = start["run_id"].as_str().unwrap().to_string();
        assert!(run_id.starts_with("tx-"), "{start}");
        assert_eq!(start["task_id"], 1);
        let row = manager
            .shadow_row(sid)
            .unwrap()
            .expect("shadow row at begin");
        let shadow_dir = std::path::PathBuf::from(&row.root);
        assert_eq!(row.state, faktor_session::ShadowRowState::Active);

        // Mid-drive: the first write landed inside the SHADOW and parked
        // the drive; the user checkout is byte-untouched.
        for _ in 0..3000 {
            if fired.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(shadow_dir.join("src/lib.rs")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "the write landed in the SHADOW"
        );
        assert_eq!(
            std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
            NATIVE_OWNER_LIB_RS.as_bytes(),
            "the owner checkout is byte-untouched MID-drive"
        );
        assert_eq!(
            manager.active_root(sid).unwrap(),
            Some(shadow_dir.clone()),
            "the live shadow re-points the session"
        );
        // The task-runs list reflects the live run with its state.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let list: serde_json::Value = resp.json().await.unwrap();
        let entry = list
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["run_id"] == run_id)
            .expect("the live run is listed");
        assert_eq!(entry["task_id"], 1);
        assert_eq!(entry["mode"], "in_session");
        assert_eq!(entry["goal"], "implement the change");
        assert_eq!(entry["item_ids"], serde_json::json!(["main"]));
        // The per-run state read matches the list entry.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs/{run_id}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let one: serde_json::Value = resp.json().await.unwrap();
        for key in ["task_id", "run_id", "mode", "goal", "item_ids"] {
            assert_eq!(one.get(key), entry.get(key), "{key}");
        }

        // Release the drive; the executor's own isolation pipeline
        // (prepare the candidate from the shadow run base → verify the
        // CANDIDATE with the configured verifier → land the owner
        // transactionally → complete the manifest-bound task → retire the
        // shadow) runs through its settle paths — no finalize endpoint, no
        // manual certification.
        gate.notify_waiters();
        for _ in 0..600 {
            let ok = std::fs::read(owner_root.join("src/lib.rs"))
                .map(|b| b == NATIVE_IMPL_LIB_RS.as_bytes())
                .unwrap_or(false)
                && !shadow_dir.exists();
            if ok {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "VerifiedComplete integration lands the changed file"
        );
        assert!(!shadow_dir.exists(), "clean integration removes the shadow");
        assert_eq!(
            manager.shadow_row(sid).unwrap().unwrap().state,
            faktor_session::ShadowRowState::Integrated
        );
        // The run's terminal state reads Done on the task-runs surface.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs/{run_id}"),
        )
        .await;
        let done: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(done["state"], "Done", "{done}");
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_shadow_run_via_extension_client_session_survives_worktree_id_shift() {
        // Shadow 409 root cause, end to end through the EXACT extension
        // client path: `POST /session/create` (workspace root) followed by
        // `POST /native/session/{id}/task-runs` with the shadow default.
        // The regression: a session created over HTTP carries the
        // standalone default worktree 1. When its workspace ALREADY holds
        // an owner worktree row with another id (any worktree row from an
        // earlier project on the same daemon), the executor's adoption
        // early-returned on "workspace has worktrees" and the shadowed run
        // refused with a typed 409 "no registered worktree row". The daemon
        // must register the session's own workspace/worktree at creation
        // and self-heal older sessions at task-run start.
        let dir = tempfile::tempdir().unwrap();
        let rig = native_task_rig(
            dir.path(),
            vec![
                vec![
                    faktor_provider::ScriptedResponse::ToolCall {
                        id: "c1".into(),
                        name: "write_file".into(),
                        input: serde_json::json!({
                            "path": "src/lib.rs",
                            "content": NATIVE_IMPL_LIB_RS,
                        }),
                    },
                    faktor_provider::ScriptedResponse::Text("done".into()),
                    faktor_provider::ScriptedResponse::End,
                ],
                vec![faktor_provider::ScriptedResponse::End],
            ],
            false,
            true,
        );
        seed_native_owner(&rig.owner_root);
        let NativeTaskRig {
            deps,
            manager,
            owner_root,
            ..
        } = rig;
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);

        // Force the regression's precondition: the owner workspace's
        // worktree row is NOT id 1 (a decoy workspace registered first,
        // then the owner row recreated), so the standalone default 1 names
        // no row of this workspace. The rig's own session is irrelevant.
        let ws = manager
            .create_workspace(owner_root.to_str().unwrap())
            .unwrap();
        manager
            .remove_worktree(owner_root.to_str().unwrap())
            .unwrap();
        let decoy = manager
            .create_workspace(dir.path().join("decoy").to_str().unwrap())
            .unwrap();
        let decoy_wt = manager
            .put_worktree(decoy, dir.path().join("decoy").to_str().unwrap(), "main")
            .unwrap();
        assert_eq!(
            decoy_wt, 1,
            "the standalone default id is taken by the decoy"
        );
        let owner_wt = manager
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap();
        assert_ne!(
            owner_wt, 1,
            "the owner row id shifted away from the default"
        );

        // The native session creation path leaves the standalone default
        // worktree; the self-heal below forces the regression precondition
        // (a session naming no row of its workspace) before the task start.
        let sid = manager
            .create_session(ws, "extension session", "fake", "m")
            .unwrap()
            .id();

        // Adversarial self-heal probe: an OLDER session (or one created
        // before creation-time registration existed) still holds the
        // standalone default. The task-run start must re-register it
        // instead of refusing the shadowed run with a 409.
        manager
            .adopt_identity(
                sid,
                faktor_core::id::WorktreeId::new(1),
                faktor_core::id::TaskId::new(1),
            )
            .unwrap();

        // The shadowed mutating run via the extension client path: goal
        // only, mutation_mode omitted = daemon default (Shadow).
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"goal": "implement the change"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "an unregistered session must be adopted at task-run start, never 409ed: {:?}",
            resp.text().await
        );
        native_wait_session_state(
            &manager,
            sid,
            faktor_core::state::AgentState::ReadyForNextTurn,
        )
        .await;
        // The run really worked in a daemon-owned shadow; the owner
        // checkout stayed byte-untouched.
        let shadow = manager
            .shadow_row(sid)
            .unwrap()
            .expect("an ordinary native mutating prompt must begin a shadow");
        assert_eq!(shadow.state, faktor_session::ShadowRowState::Active);
        assert_eq!(
            std::fs::read(std::path::Path::new(&shadow.root).join("src/lib.rs")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "the edit landed in the shadow"
        );
        assert_eq!(
            std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
            NATIVE_OWNER_LIB_RS.as_bytes(),
            "the owner checkout is byte-untouched"
        );
        assert_eq!(
            manager
                .get_session(sid)
                .unwrap()
                .unwrap()
                .row()
                .unwrap()
                .worktree_id,
            faktor_core::id::WorktreeId::new(owner_wt as u64),
            "the self-heal re-adopted the workspace owner row"
        );
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_mutating_multi_agent_task_isolated_then_explicit_integration_and_restart() {
        // The audit E2E: POST one 3-stage analysis -> implementation ->
        // review plan. Ownership is EXPLICIT per item (read-only items hold
        // NoWrites, the mutating item owns an IsolatedWorktree); the DAEMON
        // allocates the candidate root itself (the DTO carries no path).
        // Asserts: accepted, durable assignments, real child sessions,
        // implementation inside the daemon-owned candidate root, the owner
        // checkout untouched, an EXPLICIT integration that makes the
        // candidate visible to review, and a restart that preserves child
        // ids + run.
        use faktor_orchestrator::runtime::OrchestratorRuntime;

        let dir = tempfile::tempdir().unwrap();
        let rig = native_task_rig(
            dir.path(),
            vec![
                // child-0 analysis (read-only).
                vec![
                    faktor_provider::ScriptedResponse::Text("analysis done".into()),
                    faktor_provider::ScriptedResponse::End,
                ],
                // child-1 implementation: a REAL write inside its isolated
                // worktree. (The candidate starts empty; the write creates
                // `candidate.txt` at its root.)
                vec![
                    faktor_provider::ScriptedResponse::ToolCall {
                        id: "w1".into(),
                        name: "write_file".into(),
                        input: serde_json::json!({
                            "path": "candidate.txt",
                            "content": NATIVE_IMPL_LIB_RS,
                        }),
                    },
                    faktor_provider::ScriptedResponse::Text("implemented".into()),
                    faktor_provider::ScriptedResponse::End,
                ],
                // child-2 review (read-only).
                vec![
                    faktor_provider::ScriptedResponse::Text("review ok".into()),
                    faktor_provider::ScriptedResponse::End,
                ],
            ],
            false,
            false,
        );
        seed_native_owner(&rig.owner_root);
        let NativeTaskRig {
            deps,
            manager,
            parent: sid,
            owner_root,
            ..
        } = rig;
        let orchestrator = deps.orchestrator.clone();
        let tasks = deps.tasks.clone();
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);

        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "goal": "3-stage change",
                "work_items": [
                    {"id": "analyze", "kind": "Analysis", "ownership": "no_writes"},
                    {
                        "id": "implement",
                        "kind": "Implementation",
                        "depends_on": ["analyze"],
                        "ownership": "isolated_worktree",
                    },
                    {
                        "id": "review",
                        "kind": "Review",
                        "depends_on": ["implement"],
                        "ownership": "no_writes",
                    },
                ],
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let start: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(status, 200, "{start}");
        assert_eq!(start["task_id"], 1);
        let run_id = start["run_id"].as_str().unwrap().to_string();
        // The list surface reports the orchestrated mode (the POST receipt
        // predates the detached plan row).
        let mut modes = Vec::new();
        // Environment-independent wait (the fixed 200x25ms loop was
        // load-sensitive); the assertions and break condition are unchanged.
        let wait_deadline = std::time::Instant::now() + Duration::from_secs(240);
        while std::time::Instant::now() < wait_deadline {
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/native/session/{sid}/task-runs"),
            )
            .await;
            assert_eq!(resp.status(), 200);
            let list: serde_json::Value = resp.json().await.unwrap();
            modes = list
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["run_id"] == run_id.as_str())
                .map(|e| e["mode"].clone())
                .collect();
            if !modes.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(modes, vec![serde_json::json!("orchestrated")], "{start}");

        // Durable assignments exist BEFORE/while the children drive.
        let assignments =
            OrchestratorRuntime::assignment_rows(manager.clone(), sid, &run_id).unwrap();
        assert_eq!(assignments.len(), 3, "one durable assignment per item");
        let a_of = |id: &str| assignments.iter().find(|a| a.item_id == id).unwrap();
        assert_eq!(
            a_of("analyze").ownership,
            faktor_core::state::OwnershipSpec::NoWrites
        );
        assert_eq!(
            a_of("implement").ownership,
            faktor_core::state::OwnershipSpec::IsolatedWorktree
        );
        assert_eq!(
            a_of("review").ownership,
            faktor_core::state::OwnershipSpec::NoWrites
        );

        // Wait for the whole run to reach its terminal item states.
        for _ in 0..600 {
            let rows = OrchestratorRuntime::registry_rows(manager.clone(), sid, &run_id).unwrap();
            if rows.len() == 3 && rows.iter().all(|c| c.state.is_terminal()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let rows = OrchestratorRuntime::registry_rows(manager.clone(), sid, &run_id).unwrap();
        assert_eq!(rows.len(), 3, "three real child rows");
        let row_of = |id: &str| rows.iter().find(|r| r.item_id == id).unwrap();
        for id in ["analyze", "implement", "review"] {
            assert_ne!(row_of(id).session_id, 0, "child {id} has a real session");
            assert_ne!(row_of(id).operation_id, 0, "child {id} was really driven");
            assert_eq!(row_of(id).state, faktor_orchestrator::ChildState::Done);
        }
        // Child sessions are visible on the native agents surface.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/agents?session={sid}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let entries: serde_json::Value = resp.json().await.unwrap();
        let children: Vec<&serde_json::Value> = entries
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "child")
            .collect();
        assert_eq!(children.len(), 3, "children visible: {entries}");

        // The implementation ran in the DAEMON-allocated candidate root
        // (never a client path): its workspace lives under
        // `<run_roots.root()>/s<sid>/<run_id>`.
        let impl_row = row_of("implement");
        assert_eq!(
            impl_row.ownership,
            faktor_session::child::ChildOwnership::IsolatedWorktree
        );
        let impl_root = manager
            .workspace_root(WorkspaceId::new(impl_row.workspace_id))
            .unwrap()
            .expect("implementation workspace root");
        let expected = tasks
            .run_roots()
            .root()
            .join(format!("s{}", sid.raw()))
            .join(&run_id);
        assert!(
            std::path::Path::new(&impl_root).starts_with(&expected),
            "the implementation lives under the daemon-allocated candidate root \
             ({impl_root:?} vs {expected:?})"
        );
        assert!(
            std::path::Path::new(&impl_root).ends_with("child-1"),
            "the implementation child owns its own isolated child root"
        );
        assert_eq!(
            std::fs::read(std::path::Path::new(&impl_root).join("candidate.txt")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "the implementation wrote inside its candidate"
        );
        // Owner checkout unchanged until an EXPLICIT integration.
        assert_eq!(
            std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
            NATIVE_OWNER_LIB_RS.as_bytes(),
            "the owner checkout is byte-untouched before integration"
        );
        assert!(
            !owner_root.join("candidate.txt").exists(),
            "nothing of the candidate leaked into the owner checkout"
        );

        // Explicit integration of the candidate into the owner checkout.
        let cs = orchestrator
            .stage_child_changes(&impl_row.child_id)
            .unwrap();
        assert!(
            cs.files.iter().any(|f| f.path.ends_with("candidate.txt")),
            "the staged change set holds the candidate write: {:?}",
            cs.files
        );
        let approved: Vec<std::path::PathBuf> = cs
            .files
            .iter()
            .filter(|f| f.child_hash.is_some())
            .map(|f| f.path.clone())
            .collect();
        let outcome = orchestrator
            .approve_and_merge(&impl_row.child_id, &cs.id(), &approved, &[])
            .unwrap();
        assert!(
            outcome.merged.iter().any(|p| p.ends_with("candidate.txt")),
            "the candidate merged explicitly: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(owner_root.join("candidate.txt")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "integration lands the candidate in the owner checkout"
        );
        // Review now SEES the candidate: a reviewer tree copied from the
        // current parent state contains the integrated bytes.
        let reviewer = orchestrator.spawn_reviewer(&impl_row.child_id).unwrap();
        let reviewer_root = manager
            .workspace_root(WorkspaceId::new(reviewer.workspace_id))
            .unwrap()
            .expect("reviewer workspace root");
        assert_eq!(
            std::fs::read(std::path::Path::new(&reviewer_root).join("candidate.txt")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "review sees the integrated candidate"
        );
        let _ = handle.shutdown.send(());
        drop(client);
        drop(orchestrator);
        drop(tasks);

        // Restart on the same data dir: the run + child ids survive.
        drop(manager);
        let reopened =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let assignments2 =
            OrchestratorRuntime::assignment_rows(reopened.clone(), sid, &run_id).unwrap();
        assert_eq!(assignments2, assignments, "assignments survive a restart");
        let rows2 = OrchestratorRuntime::registry_rows(reopened, sid, &run_id).unwrap();
        assert_eq!(
            rows2.len(),
            4,
            "the three plan children + the reviewer survive"
        );
        let ids: Vec<&str> = rows2.iter().map(|r| r.child_id.as_str()).collect();
        for id in ["child-0", "child-1", "child-2"] {
            assert!(ids.contains(&id), "child id {id} survives: {ids:?}");
        }
        for r in &rows2 {
            assert_ne!(r.session_id, 0, "child session ids survive a restart");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_ordinary_prompt_uses_the_shadow_executor_and_keeps_the_owner_untouched() {
        // The NATIVE ordinary prompt (no explicit work items) keeps the
        // daemon default shadow mutation: its write lands in the daemon
        // shadow and the owner checkout stays byte-untouched until a
        // verified integration. (The moved coverage of the pre-regression
        // the native surface is where shadowing is the promise; the
        // retired compatibility surfaces were direct.)
        let dir = tempfile::tempdir().unwrap();
        let rig = native_task_rig(
            dir.path(),
            vec![vec![
                faktor_provider::ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": NATIVE_IMPL_LIB_RS,
                    }),
                },
                faktor_provider::ScriptedResponse::Text("done".into()),
                faktor_provider::ScriptedResponse::End,
            ]],
            false,
            true,
        );
        seed_native_owner(&rig.owner_root);
        let NativeTaskRig {
            deps,
            manager,
            parent: sid,
            owner_root,
            ..
        } = rig;
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);

        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"goal": "implement the change"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        native_wait_session_state(
            &manager,
            sid,
            faktor_core::state::AgentState::ReadyForNextTurn,
        )
        .await;
        // The write stayed in the shadow; the owner checkout is untouched
        // until a verified integration.
        let shadow = manager
            .shadow_row(sid)
            .unwrap()
            .expect("an ordinary native mutating prompt must begin a shadow");
        assert_eq!(shadow.state, faktor_session::ShadowRowState::Active);
        assert_eq!(
            std::fs::read(std::path::Path::new(&shadow.root).join("src/lib.rs")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "the edit landed in the shadow"
        );
        assert_eq!(
            std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
            NATIVE_OWNER_LIB_RS.as_bytes(),
            "the owner checkout is byte-untouched"
        );
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_plan_global_ownership_converts_once_at_the_native_dto_boundary() {
        // A legacy client posts ONE plan-global ownership with two mutating
        // items that carry none of their own: the DTO boundary converts it
        // ONCE onto the items (the runtime never sees the plan-global
        // value), and the run is accepted with explicit per-item ownership
        // on the durable assignment rows.
        use faktor_orchestrator::runtime::OrchestratorRuntime;
        let dir = tempfile::tempdir().unwrap();
        let rig = native_task_rig(dir.path(), vec![], false, false);
        seed_native_owner(&rig.owner_root);
        let NativeTaskRig {
            deps,
            manager,
            parent: sid,
            ..
        } = rig;
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);

        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "goal": "legacy ownership",
                "ownership": "IsolatedWorktree",
                "work_items": [
                    {"id": "a", "kind": "Implementation"},
                    {"id": "b", "kind": "Implementation", "depends_on": ["a"]},
                ],
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let start: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(status, 200, "{start}");
        let run_id = start["run_id"].as_str().unwrap().to_string();
        let mut assignments =
            OrchestratorRuntime::assignment_rows(manager.clone(), sid, &run_id).unwrap();
        for _ in 0..400 {
            if !assignments.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            assignments =
                OrchestratorRuntime::assignment_rows(manager.clone(), sid, &run_id).unwrap();
        }
        assert_eq!(assignments.len(), 2);
        for a in &assignments {
            assert_eq!(
                a.ownership,
                faktor_core::state::OwnershipSpec::IsolatedWorktree,
                "the legacy plan-global value converted onto item {}",
                a.item_id
            );
        }
        for _ in 0..600 {
            let rows = OrchestratorRuntime::registry_rows(manager.clone(), sid, &run_id).unwrap();
            if rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_task_run_start_always_isolates_and_direct_compat_is_a_400() {
        // P0 isolation over the HTTP edge: (a) the removed `direct_compat`
        // value is a strict DTO rejection (400) before any drive or durable
        // run row; (b) the same start under the (wire-only) shadow policy
        // begins the isolated candidate — the owner checkout stays
        // byte-untouched MID-drive, and the write lands in the candidate.
        let dir = tempfile::tempdir().unwrap();
        let rig = native_task_rig(
            dir.path(),
            vec![
                vec![
                    faktor_provider::ScriptedResponse::ToolCall {
                        id: "c1".into(),
                        name: "write_file".into(),
                        input: serde_json::json!({
                            "path": "src/lib.rs",
                            "content": NATIVE_IMPL_LIB_RS,
                        }),
                    },
                    faktor_provider::ScriptedResponse::Text("done".into()),
                    faktor_provider::ScriptedResponse::End,
                ],
                vec![faktor_provider::ScriptedResponse::End],
            ],
            true,
            true,
        );
        seed_native_owner(&rig.owner_root);
        let NativeTaskRig {
            deps,
            manager,
            parent: sid,
            owner_root,
            gate,
            fired,
            ..
        } = rig;
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);

        // (a) the removed escape hatch is a strict 400 and never drives.
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "goal": "implement the change",
                "mutation_mode": "direct_compat",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "direct_compat must stay a strict DTO 400"
        );
        assert_eq!(
            manager
                .get_session(sid)
                .unwrap()
                .unwrap()
                .message_count()
                .unwrap(),
            0,
            "a refused DTO never drives"
        );
        assert!(
            manager.shadow_row(sid).unwrap().is_none(),
            "a refused DTO never begins a shadow"
        );

        // (b) the same start under the only decodable policy isolates.
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "goal": "implement the change",
                "mutation_mode": "shadow",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let row = manager
            .shadow_row(sid)
            .unwrap()
            .expect("a mutating task-run must isolate");
        assert_eq!(row.state, faktor_session::ShadowRowState::Active);
        // The parked write fired inside the candidate and parked the drive;
        // the owner checkout is STILL byte-untouched mid-drive.
        for _ in 0..3000 {
            if fired.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(std::path::PathBuf::from(&row.root).join("src/lib.rs")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "the write landed in the isolated candidate"
        );
        assert_eq!(
            std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
            NATIVE_OWNER_LIB_RS.as_bytes(),
            "the owner checkout is byte-untouched mid-drive"
        );
        gate.notify_waiters();
        native_wait_session_state(
            &manager,
            sid,
            faktor_core::state::AgentState::ReadyForNextTurn,
        )
        .await;
        assert_eq!(
            std::fs::read(std::path::PathBuf::from(&row.root).join("src/lib.rs")).unwrap(),
            NATIVE_IMPL_LIB_RS.as_bytes(),
            "the write stays in the isolated candidate"
        );
        assert_eq!(
            std::fs::read(owner_root.join("src/lib.rs")).unwrap(),
            NATIVE_OWNER_LIB_RS.as_bytes(),
            "the owner checkout stayed byte-untouched"
        );
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_task_run_start_hostile_dtos_are_typed_400s() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/plain").unwrap();
        let sid = manager
            .create_session(ws, "hostile", "fake", "m")
            .unwrap()
            .id();

        // Unauthenticated is 401 before anything else.
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .json(&serde_json::json!({"goal": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let oversized_goal = "x".repeat(2001);
        let hostile_bodies: Vec<serde_json::Value> = vec![
            serde_json::json!({}),
            serde_json::json!({"goal": ""}),
            serde_json::json!({"goal": "x", "bogus": 1}),
            serde_json::json!({"goal": "x", "mutation_mode": "nonsense"}),
            // The removed direct-owner mode stays a strict DTO 400.
            serde_json::json!({"goal": "x", "mutation_mode": "direct_compat"}),
            serde_json::json!({"goal": "x", "mutation_mode": "Shadow"}),
            serde_json::json!({"goal": "x", "routing_mode": "economy"}),
            serde_json::json!({"goal": oversized_goal}),
            serde_json::json!({"goal": "x", "max_tokens": "many"}),
            serde_json::json!({"goal": "x", "criteria": (0..=faktor_session::MAX_TASK_CRITERIA).map(|i| format!("criterion {i}")).collect::<Vec<_>>()}),
            serde_json::json!({"goal": "x", "criteria": vec!["c".repeat(faktor_session::MAX_TASK_CRITERION_BYTES + 1)]}),
            serde_json::json!({"goal": "x", "work_items": [{"id": "a", "kind": "Implementation"}, {"id": "b", "kind": "Implementation"}]}),
            serde_json::json!({"goal": "x", "work_items": [{"id": "a a/..", "kind": "Analysis"}]}),
            serde_json::json!({"goal": "x", "work_items": [{"id": "a", "kind": "Analysis"}, {"id": "a", "kind": "Analysis"}]}),
            serde_json::json!({"goal": "x", "work_items": [{"id": "a", "kind": "NoSuchKind"}]}),
            serde_json::json!({"goal": "x", "work_items": [{"id": "a", "kind": "Analysis", "extra": 1}]}),
            serde_json::json!({"goal": "x", "work_items": "not-an-array"}),
        ];
        for body in hostile_bodies {
            let resp = client
                .post(format!("{base}/native/session/{sid}/task-runs"))
                .bearer_auth(token.as_str())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "hostile body must 400: {body}");
        }
        // Non-JSON bodies are plain 400s; unknown sessions are 404s.
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = client
            .post(format!("{base}/native/session/999999/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"goal": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // No provider call ever happened on the hostile attempts.
        let h = manager.get_session(sid).unwrap().unwrap();
        assert_eq!(h.message_count().unwrap(), 0, "hostile starts never drive");
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_tournament_start_hostile_dtos_are_typed_400s() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/plain").unwrap();
        let sid = manager
            .create_session(ws, "hostile-tournament", "fake", "m")
            .unwrap()
            .id();

        // Unauthenticated is 401 before anything else.
        let resp = client
            .post(format!("{base}/native/session/{sid}/tournament"))
            .json(&serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let oversized_goal = "x".repeat(513);
        let hostile_bodies: Vec<serde_json::Value> = vec![
            serde_json::json!({}),
            serde_json::json!({"goal": "x", "criteria": ["c"]}),
            serde_json::json!({"goal": "", "criteria": ["c"], "n": 2}),
            serde_json::json!({"goal": "x", "criteria": [], "n": 2}),
            serde_json::json!({"goal": "x", "criteria": ["c"], "n": 0}),
            serde_json::json!({"goal": "x", "criteria": ["c"], "n": 1}),
            serde_json::json!({"goal": "x", "criteria": ["c"], "n": 5}),
            serde_json::json!({"goal": "x", "criteria": ["c"], "n": "two"}),
            serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2, "bogus": 1}),
            serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2, "mutation_mode": "nonsense"}),
            serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2, "mutation_mode": "direct_compat"}),
            serde_json::json!({"goal": oversized_goal, "criteria": ["c"], "n": 2}),
            serde_json::json!({"goal": "x", "criteria": ["c".repeat(600)], "n": 2}),
        ];
        for body in hostile_bodies {
            let resp = client
                .post(format!("{base}/native/session/{sid}/tournament"))
                .bearer_auth(token.as_str())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                400,
                "hostile tournament body must 400: {body}"
            );
        }
        // Non-JSON bodies are plain 400s; unknown sessions are 404s; an
        // unknown tournament id is a 404 (never a phantom tournament).
        let resp = client
            .post(format!("{base}/native/session/{sid}/tournament"))
            .bearer_auth(token.as_str())
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = client
            .post(format!("{base}/native/session/999999/tournament"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"goal": "x", "criteria": ["c"], "n": 2}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/tournament/does-not-exist"),
        )
        .await;
        assert_eq!(resp.status(), 404);
        // No provider call ever happened on the hostile attempts.
        let h = manager.get_session(sid).unwrap().unwrap();
        assert_eq!(h.message_count().unwrap(), 0, "hostile starts never drive");
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_tournaments_list_summarizes_the_durable_fold() {
        // The additive listing folds the pinned tournament lifecycle rows:
        // id, state, candidate count, winner and the decision timestamp.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let manager = deps.session.clone();
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/plain").unwrap();
        let s = manager
            .create_session(ws, "tour-list", "fake", "m")
            .unwrap();
        let sid = s.id();

        // Unauthenticated is 401.
        let resp = client
            .get(format!("{base}/native/session/{sid}/tournaments"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        // A session without tournaments is an empty list, never a phantom.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/tournaments"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!([])
        );
        // Hostile session ids are typed (malformed 400 / unknown 404).
        let resp = native_get(&client, &base, &token, "/native/session/nope/tournaments").await;
        assert_eq!(resp.status(), 400);
        let resp = native_get(&client, &base, &token, "/native/session/999999/tournaments").await;
        assert_eq!(resp.status(), 404);

        // Seed one OPEN and one DECIDED tournament through the typed ledger.
        let criteria = vec![faktor_session::ledger::TournamentCriterionRow {
            id: "c1".into(),
            spec: "cargo test".into(),
        }];
        let candidates: Vec<faktor_session::ledger::TournamentCandidateRow> = (0..2)
            .map(|i| faktor_session::ledger::TournamentCandidateRow {
                child_id: format!("child-{i}"),
                worktree: String::new(),
                base_revision: String::new(),
            })
            .collect();
        s.ledger_tournament_started("tour-open", "run-open", "open goal", &criteria, &candidates)
            .unwrap();
        s.ledger_tournament_started("tour-done", "run-done", "done goal", &criteria, &candidates)
            .unwrap();
        s.ledger_tournament_decided(
            "tour-done",
            Some("child-1"),
            faktor_session::TOURNAMENT_OUTCOME_DECIDED,
            "winner child-1",
        )
        .unwrap();

        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/tournaments"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let list: serde_json::Value = resp.json().await.unwrap();
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 2);
        let open = list.iter().find(|e| e["id"] == "tour-open").unwrap();
        assert_eq!(open["state"], "open");
        assert_eq!(open["candidate_count"], 2);
        assert!(open["winner"].is_null());
        assert!(open["decided_ms"].is_null());
        let done = list.iter().find(|e| e["id"] == "tour-done").unwrap();
        assert_eq!(done["state"], "decided");
        assert_eq!(done["candidate_count"], 2);
        assert_eq!(done["winner"], "child-1");
        assert!(done["decided_ms"].as_i64().unwrap_or(0) > 0);
        // The listing is a durable fold: it survives a ledger compaction
        // (tournament rows are pinned).
        s.compact_typed_ledger().unwrap();
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/tournaments"),
        )
        .await;
        let list: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 2);
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_tournament_decide_and_abort_are_strict_and_engine_gated() {
        // The additive decide/abort routes over the typed ledger: happy
        // decide (deterministic winner + losers discarded), happy abort
        // (terminal row with the reason), and every refusal boundary —
        // unknown ids 404, non-open/no-eligible-winner 409, hostile bodies
        // and oversized reasons 400, missing auth 401. The engine stays the
        // ONE authority: the routes never mutate the fold directly.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let manager = deps.session.clone();
        let token = deps.auth_token.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/plain").unwrap();
        let s = manager
            .create_session(ws, "tour-control", "fake", "m")
            .unwrap();
        let sid = s.id();

        let criteria = vec![faktor_session::ledger::TournamentCriterionRow {
            id: "c1".into(),
            spec: "cargo test".into(),
        }];
        let candidates: Vec<faktor_session::ledger::TournamentCandidateRow> = (0..2)
            .map(|i| faktor_session::ledger::TournamentCandidateRow {
                child_id: format!("child-{i}"),
                worktree: String::new(),
                base_revision: String::new(),
            })
            .collect();
        let derived = faktor_orchestrator::tournament::derive_check_specs(&[
            faktor_orchestrator::tournament::Criterion {
                id: "c1".into(),
                spec: "cargo test".into(),
            },
        ]);
        let settlement = |child_id: &str, rank: &str, cost: u64| {
            faktor_session::ledger::TournamentSettlementRow {
                child_id: child_id.into(),
                worktree: String::new(),
                base_revision: String::new(),
                state: "done".into(),
                verification: Some(7),
                verification_pass: Some(true),
                checks: derived.clone(),
                review: Some(rank.into()),
                reviewer: Some("review-0".into()),
                cost_micro: cost,
                wall_ms: 100,
                reason: "settled".into(),
            }
        };

        // Unauthenticated is 401 before anything else.
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-x/decide"
            ))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // A tournament with no eligible candidate refuses decide as a typed
        // 409 (nothing was persisted; a second decide reads the same state).
        s.ledger_tournament_started("tour-bare", "run-bare", "bare goal", &criteria, &candidates)
            .unwrap();
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-bare/decide"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/tournament/tour-bare"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap()["state"],
            "open"
        );

        // Happy decide: clean review outranks the cheaper concern reviewer.
        s.ledger_tournament_started(
            "tour-happy",
            "run-happy",
            "happy goal",
            &criteria,
            &candidates,
        )
        .unwrap();
        s.ledger_candidate_settled("tour-happy", &settlement("child-0", "clean", 500))
            .unwrap();
        s.ledger_candidate_settled("tour-happy", &settlement("child-1", "concern", 1))
            .unwrap();
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-happy/decide"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let decided: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(decided["tournament_id"], "tour-happy");
        assert_eq!(decided["winner"], "child-0");
        assert!(decided["rationale"]
            .as_str()
            .unwrap_or("")
            .contains("child-0"));
        let discarded = decided["discarded"].as_array().unwrap();
        assert_eq!(discarded.len(), 1);
        assert_eq!(discarded[0]["child_id"], "child-1");
        // Wrong state: deciding or aborting the decided tournament is 409.
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-happy/decide"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-happy/abort"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"reason": "too late"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/tournament/tour-happy"),
        )
        .await;
        let state: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(state["state"], "decided");
        assert_eq!(state["winner"], "child-0");
        assert_eq!(state["candidates"][1]["state"], "discarded");

        // Happy abort: the terminal row carries the reason and every
        // candidate is discarded; a second abort is a typed 409.
        s.ledger_tournament_started(
            "tour-abort",
            "run-abort",
            "abort goal",
            &criteria,
            &candidates,
        )
        .unwrap();
        s.ledger_candidate_settled("tour-abort", &settlement("child-0", "clean", 10))
            .unwrap();
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-abort/abort"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"reason": "operator stopped it"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let aborted: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(aborted["id"], "tour-abort");
        assert_eq!(aborted["state"], "aborted");
        assert!(aborted["winner"].is_null());
        for candidate in aborted["candidates"].as_array().unwrap() {
            assert_eq!(candidate["state"], "discarded");
        }
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-abort/abort"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);

        // Hostile boundaries: unknown tournament 404, unknown session 404,
        // strict bodies 400 (unknown member / non-JSON / missing body /
        // oversized reason).
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/nope/decide"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/nope/abort"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = client
            .post(format!(
                "{base}/native/session/999999/tournaments/nope/decide"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        for body in [
            serde_json::json!({"bogus": 1}),
            serde_json::json!({"reason": "decide takes no reason"}),
        ] {
            let resp = client
                .post(format!(
                    "{base}/native/session/{sid}/tournaments/tour-bare/decide"
                ))
                .bearer_auth(token.as_str())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "hostile decide body: {body}");
        }
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-bare/decide"
            ))
            .bearer_auth(token.as_str())
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-bare/decide"
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/tournaments/tour-bare/abort"
            ))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"reason": "x".repeat(600)}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_task_run_list_state_and_cancel_reflect_the_durable_run() {
        // List/state/cancel over HTTP on a real session: a completed
        // read-only run reads Done on both surfaces and refuses cancel; a
        // mid-flight run is cancelled at the task level (durable row
        // Cancelled, drive aborted) and stays Cancelled; hostile ids stay
        // typed.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/plain").unwrap();
        let sid = manager
            .create_session(ws, "list-cancel", "fake", "m")
            .unwrap()
            .id();

        // Fresh session: an empty task-run list and typed 404 per-run reads.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!([])
        );
        for hostile in ["tx-1", "run-x", "..", "a/b"] {
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/native/session/{sid}/task-runs/{hostile}"),
            )
            .await;
            assert_eq!(resp.status(), 404, "hostile run id {hostile:?}");
        }

        // A completed read-only run (explicit Analysis item + criteria):
        // the list/state reflect the terminal run; cancelling it is a 409.
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "goal": "analyze the module boundaries",
                "criteria": ["the analysis names the seams"],
                "work_items": [{"id": "a1", "kind": "Analysis"}],
                "max_tokens": 100_000,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let start: serde_json::Value = resp.json().await.unwrap();
        let run_id = start["run_id"].as_str().unwrap().to_string();
        assert_eq!(start["task_id"], 1);
        // Criteria rode the durable task row.
        let h = manager.get_session(sid).unwrap().unwrap();
        assert_eq!(
            h.get_task(faktor_core::id::TaskId::new(1))
                .unwrap()
                .unwrap()
                .acceptance_criteria,
            vec!["the analysis names the seams".to_string()]
        );
        let mut state = None;
        for _ in 0..300 {
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/native/session/{sid}/task-runs/{run_id}"),
            )
            .await;
            let v: serde_json::Value = resp.json().await.unwrap();
            state = Some(v.clone());
            if v["state"] == "Done" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let entry = state.expect("the run must settle");
        assert_eq!(entry["state"], "Done");
        assert_eq!(entry["run_id"], run_id);
        assert_eq!(entry["mode"], "in_session");
        assert_eq!(entry["goal"], "analyze the module boundaries");
        assert_eq!(entry["item_ids"], serde_json::json!(["a1"]));
        // The list carries the same projection.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{sid}/task-runs"),
        )
        .await;
        let list: serde_json::Value = resp.json().await.unwrap();
        let list_entry = list
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["run_id"] == run_id)
            .expect("settled run listed");
        assert_eq!(list_entry["state"], "Done");
        // A run whose task row is durably TERMINAL (verified complete)
        // refuses cancel — a typed 409, never a silent no-op.
        certify_native_task(&manager, sid);
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/task-runs/{run_id}/cancel"
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409, "terminal runs refuse cancel");
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs/nope/cancel"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "unknown runs are typed 404s");
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_task_run_cancel_aborts_a_mid_flight_in_session_run() {
        // A mid-flight in-session drive (paced text-only provider, no
        // tools) is cancelled at the TASK level: the drive is aborted
        // durably, the task row turns Cancelled, and the task-runs surface
        // reads Cancelled.
        let dir = tempfile::tempdir().unwrap();
        let paced = PacedScriptedProvider::new(
            faktor_core::model::ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![
                (0..400)
                    .map(|i| faktor_provider::ScriptedResponse::Text(format!("tick {i}")))
                    .chain(std::iter::once(faktor_provider::ScriptedResponse::End))
                    .collect(),
                vec![faktor_provider::ScriptedResponse::End],
            ],
            10,
        );
        let deps = paced_test_deps(dir.path(), paced);
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws = manager.create_workspace("/plain").unwrap();
        let sid = manager
            .create_session(ws, "cancel-mid", "fake", "m")
            .unwrap()
            .id();
        let resp = client
            .post(format!("{base}/native/session/{sid}/task-runs"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({
                "goal": "long-running analysis",
                "work_items": [{"id": "a1", "kind": "Analysis"}],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let start: serde_json::Value = resp.json().await.unwrap();
        let run_id = start["run_id"].as_str().unwrap().to_string();
        // Wait for the drive to be mid-flight (the session is actively
        // working — anything but parked/terminal), then cancel at the task
        // level.
        let wait_deadline = std::time::Instant::now() + Duration::from_secs(240);
        while std::time::Instant::now() < wait_deadline {
            let st = manager.get_session(sid).unwrap().unwrap().state().unwrap();
            if !st.is_terminal()
                && !matches!(
                    st,
                    faktor_core::state::AgentState::ReadyForNextTurn
                        | faktor_core::state::AgentState::Idle
                        | faktor_core::state::AgentState::Suspended
                )
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/task-runs/{run_id}/cancel"
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["run_id"], run_id);
        assert_eq!(ack["cancelled"], true);
        // The durable outcome: task row Cancelled, session parked, task-runs
        // state Cancelled; a second cancel is a typed 409.
        let wait_deadline = std::time::Instant::now() + Duration::from_secs(240);
        while std::time::Instant::now() < wait_deadline {
            let resp = native_get(
                &client,
                &base,
                &token,
                &format!("/native/session/{sid}/task-runs/{run_id}"),
            )
            .await;
            let v: serde_json::Value = resp.json().await.unwrap();
            if v["state"] == "Cancelled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let h = manager.get_session(sid).unwrap().unwrap();
        let task = h
            .get_task(faktor_core::id::TaskId::new(1))
            .unwrap()
            .unwrap();
        assert_eq!(
            task.state,
            faktor_core::state::TaskState::Cancelled,
            "the task row is durably Cancelled"
        );
        let resp = client
            .post(format!(
                "{base}/native/session/{sid}/task-runs/{run_id}/cancel"
            ))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "a cancelled run is never cancelled twice"
        );
        let _ = handle.shutdown.send(());
    }

    #[test]
    fn server_reaches_task_start_only_through_the_executor() {
        // The single-authority source scan: in the NON-TEST server code the
        // ONLY TaskExecutor start edge is the native start handler, the
        // ONLY TaskRunRequest construction lives in that same handler. The
        // scan spans every server source file of the audit 81-83/94 split
        // (api router assembly + native/*).
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let api_src = std::fs::read_to_string(root.join("api.rs")).expect("api.rs source");
        let mut src = api_src
            .split_once("mod tests {")
            .expect("the tests module marker exists")
            .0
            .to_string();
        for module in [
            "native/mod.rs",
            "native/session.rs",
            "native/task.rs",
            "native/agents.rs",
            "native/evidence.rs",
            "native/verification.rs",
            "native/terminal.rs",
            "native/usage.rs",
            "native/semantic.rs",
            "native/models.rs",
        ] {
            src.push_str(
                &std::fs::read_to_string(root.join(module)).expect("server module source"),
            );
            src.push('\n');
        }
        let lines: Vec<&str> = src.lines().collect();
        let is_decl_start = |l: &str| {
            l.starts_with("async fn ")
                || l.starts_with("fn ")
                || l.starts_with("pub(crate) async fn ")
                || l.starts_with("pub(crate) fn ")
        };
        let in_handler = |i: usize| {
            let Some(start) = lines
                .iter()
                .position(|l| l.contains("async fn native_task_run_start("))
            else {
                return false;
            };
            let end = lines
                .iter()
                .enumerate()
                .skip(start + 1)
                .find(|(_, l)| is_decl_start(l))
                .map(|(j, _)| j)
                .expect("a declaration follows the start handler");
            i > start && i < end
        };
        // Exactly one `.start_task(` call, inside the native start handler.
        let starts: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(".start_task("))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(starts.len(), 1, "one task-start call site: {starts:?}");
        assert!(
            in_handler(starts[0]),
            "the start call must live in native_task_run_start, at line {}",
            starts[0]
        );
        assert!(
            lines[starts[0]].contains("prompts.start_task"),
            "the native handler reaches the executor ONLY through the \
             PromptExecutionService: {}",
            lines[starts[0]]
        );
        // Exactly one TaskRunRequest construction, in the same handler.
        let requests: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains("task_executor::TaskRunRequest {"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(requests.len(), 1, "one TaskRunRequest site: {requests:?}");
        assert!(in_handler(requests[0]), "line {}", requests[0]);
        // (work-entry unification) NO non-test server code drives the agent
        // directly: every ordinary prompt and every explicit task start goes
        // through the PromptExecutionService.
        let drives: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.contains(".run_session_queue(")
                    || l.contains(".drive_receipt(")
                    || l.contains("agent.submit(")
            })
            .map(|(i, _)| i)
            .collect();
        assert!(
            drives.is_empty(),
            "no direct AgentRuntime drive may remain in server production code: {drives:?}"
        );
        // ... and the ONE start edge is the prompt service, which itself
        // wraps the executor (never the agent).
        let service_src = std::fs::read_to_string(root.join("native/prompt.rs"))
            .expect("native/prompt.rs source");
        assert!(
            service_src.contains("self.tasks.start_task("),
            "PromptExecutionService must start runs through the TaskExecutor"
        );
        for (i, l) in service_src.lines().enumerate() {
            assert!(
                !l.contains(".run_session_queue(")
                    && !l.contains(".drive_receipt(")
                    && !l.contains("agent.submit("),
                "prompt service line {} drives the agent directly: {l}",
                i + 1
            );
        }
    }

    // ------------------------------------------------ native evidence (audit 82)

    /// One complete, range-readable evidence envelope with retained backing
    /// (`b"hello"`), owned by `session`/`workspace`.
    fn evidence_envelope(
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

    fn evidence_handle(
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

    #[tokio::test]
    async fn native_evidence_foreign_scope_is_typed_denial() {
        // Knowing a valid evidence id is not authorization (audit 82): the
        // same id read under a foreign session's scope is a typed 403.
        let dir = tempfile::tempdir().unwrap();
        let mut deps = test_deps(dir.path());
        let ws = deps.session.create_workspace("/tmp").unwrap();
        let owner = deps
            .session
            .create_session(ws, "owner", "fake", "m")
            .unwrap();
        let foreign = deps
            .session
            .create_session(ws, "foreign", "fake", "m")
            .unwrap();
        let owner_row = owner.row().unwrap();
        let foreign_row = foreign.row().unwrap();
        deps.evidence = Some(evidence_handle(vec![evidence_envelope(
            7,
            owner_row.id.raw(),
            owner_row.workspace_id.raw(),
            true,
        )]));
        let pw = deps.server_password.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        // The owning session reads its own evidence.
        let resp = client
            .get(format!(
                "{base}/native/evidence/7?session={}",
                owner_row.id.raw()
            ))
            .header("x-faktor-server-password", pw.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["id"], 7);
        assert_eq!(body["sessionId"], owner_row.id.raw());
        assert_eq!(body["backingRetained"], true);
        // A foreign session that knows the id gets no bytes and no oracle:
        // 403 with the typed denial code.
        let resp = client
            .get(format!(
                "{base}/native/evidence/7?session={}",
                foreign_row.id.raw()
            ))
            .header("x-faktor-server-password", pw.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "foreign scope must be denied");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "evidence_access_denied");
        // The retrieve path enforces the same scope.
        let resp = client
            .post(format!(
                "{base}/native/evidence/7/retrieve?session={}",
                foreign_row.id.raw()
            ))
            .header("x-faktor-server-password", pw.as_str())
            .json(&serde_json::json!({"selector": "all"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "evidence_access_denied");
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_evidence_unknown_and_oversized_are_honest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut deps = test_deps(dir.path());
        let ws = deps.session.create_workspace("/tmp").unwrap();
        let session = deps.session.create_session(ws, "t", "fake", "m").unwrap();
        let row = session.row().unwrap();
        deps.evidence = Some(evidence_handle(vec![evidence_envelope(
            7,
            row.id.raw(),
            row.workspace_id.raw(),
            true,
        )]));
        let pw = deps.server_password.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        // Unknown id: 404, never a scope oracle.
        let resp = client
            .get(format!(
                "{base}/native/evidence/999?session={}",
                row.id.raw()
            ))
            .header("x-faktor-server-password", pw.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // Oversized search selector: the bound is enforced before the store.
        let oversized = "x".repeat(5000);
        let resp = client
            .post(format!(
                "{base}/native/evidence/7/retrieve?session={}",
                row.id.raw()
            ))
            .header("x-faktor-server-password", pw.as_str())
            .json(&serde_json::json!({
                "selector": "search",
                "query": oversized,
                "max_hits": 1,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "oversized selector is a loud 400");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "evidence_oversized");
        // Oversized item selector: same bound.
        let ids: Vec<u64> = (1..=300).collect();
        let resp = client
            .post(format!(
                "{base}/native/evidence/7/retrieve?session={}",
                row.id.raw()
            ))
            .header("x-faktor-server-password", pw.as_str())
            .json(&serde_json::json!({"selector": "items", "ids": ids}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        // A bounded selector on owned evidence actually retrieves.
        let resp = client
            .post(format!(
                "{base}/native/evidence/7/retrieve?session={}",
                row.id.raw()
            ))
            .header("x-faktor-server-password", pw.as_str())
            .json(&serde_json::json!({"selector": "all"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["bytesBase64"], "aGVsbG8=");
        assert_eq!(body["byteLen"], 5);
        let _ = handle.shutdown.send(());
    }

    // ---------------------------------------------- native semantic (audit 83)

    #[tokio::test]
    async fn native_semantic_status_falls_back_without_provider() {
        // No registry configured is a 200 fallback shape, never a 500.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let pw = deps.server_password.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let resp = client
            .get(format!("{base}/native/semantic/status"))
            .header("x-faktor-server-password", pw.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["configured"], false);
        assert!(body["providers"].as_array().unwrap().is_empty());
        assert!(!body["fallback"]["id"].as_str().unwrap().is_empty());
        assert!(body["fallback"]["version"].is_u64());
        assert!(body["snapshotState"]["fallback"].is_boolean());
        // Capabilities mirror the same fallback-only registry.
        let resp = client
            .get(format!("{base}/native/semantic/capabilities"))
            .header("x-faktor-server-password", pw.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["providers"].as_array().unwrap().is_empty());
        assert!(body["union"]["operations"]["snapshot"].is_boolean());
        assert!(!body["fallback"]["capabilities"]
            .as_object()
            .unwrap()
            .is_empty());
        let _ = handle.shutdown.send(());
    }

    #[tokio::test]
    async fn native_board_endpoints_are_scoped_strict_and_hostile_inputs_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let manager = deps.session.clone();
        let ws = manager.create_workspace("/tmp").unwrap();
        let root = manager
            .create_session(ws, "board-root", "fake", "m")
            .unwrap();
        let child = manager
            .create_child_session(
                root.id(),
                ws,
                faktor_core::id::WorktreeId::new(1),
                faktor_core::id::TaskId::new(1),
                "fake",
                "m",
                "board-child",
                faktor_session::child::ChildOwnership::ReadOnlyShared,
            )
            .unwrap();
        // A second family: its board must never leak into the first's.
        let foreign = manager
            .create_session(ws, "foreign-root", "fake", "m")
            .unwrap();
        let root_post = root.board_post("root post", "root body", &[]).unwrap();
        let child_post = child.board_post("child post", "child body", &[]).unwrap();
        foreign
            .board_post("foreign post", "foreign body", &[])
            .unwrap();
        let pw = deps.server_password.clone();
        let handle = serve(deps, 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        let client = reqwest::Client::new();
        let get = |path: &str, auth: Option<&str>| {
            let mut rb = client.get(format!("{base}{path}"));
            if let Some(pw) = auth {
                rb = rb.header("x-faktor-server-password", pw);
            }
            rb.send()
        };
        let post = |path: &str, body: serde_json::Value| {
            client
                .post(format!("{base}{path}"))
                .header("x-faktor-server-password", pw.as_str())
                .json(&body)
                .send()
        };

        // Auth is required for both verbs.
        assert_eq!(
            get(&format!("/native/session/{}/board", root.id().raw()), None)
                .await
                .unwrap()
                .status(),
            401
        );

        // The root reads its family board: both posts, newest first, and the
        // board identity is the family root — there is no board-id input to
        // forge.
        let resp = get(
            &format!("/native/session/{}/board", root.id().raw()),
            Some(pw.as_str()),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        let page: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(page["board_id"], root.id().raw());
        assert_eq!(page["revision"], child_post.revision);
        assert_eq!(page["posts"].as_array().unwrap().len(), 2);
        assert_eq!(page["posts"][0]["id"], child_post.id.raw());
        assert_eq!(page["posts"][1]["id"], root_post.id.raw());
        assert_eq!(page["posts"][0]["author_session"], child.id().raw());
        assert_eq!(page["has_more"], false);
        assert!(page["next_before_revision"].is_null());

        // The child reads the SAME family board (its own member view).
        let resp = get(
            &format!("/native/session/{}/board", child.id().raw()),
            Some(pw.as_str()),
        )
        .await
        .unwrap();
        let child_page: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(child_page["board_id"], root.id().raw());
        assert_eq!(child_page["posts"].as_array().unwrap().len(), 2);

        // The foreign family sees ONLY its own single-post board: knowing the
        // first family's session ids grants no board access.
        let resp = get(
            &format!("/native/session/{}/board", foreign.id().raw()),
            Some(pw.as_str()),
        )
        .await
        .unwrap();
        let foreign_page: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(foreign_page["board_id"], foreign.id().raw());
        assert_eq!(foreign_page["posts"].as_array().unwrap().len(), 1);
        assert_eq!(foreign_page["posts"][0]["subject"], "foreign post");

        // Cursor paging: limit=1 yields the newest post + the exclusive
        // older cursor; `since` then returns the older page.
        let resp = get(
            &format!("/native/session/{}/board?limit=1", root.id().raw()),
            Some(pw.as_str()),
        )
        .await
        .unwrap();
        let first: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(first["posts"].as_array().unwrap().len(), 1);
        assert_eq!(first["posts"][0]["id"], child_post.id.raw());
        assert_eq!(first["has_more"], true);
        let cursor = first["next_before_revision"].as_u64().unwrap();
        let resp = get(
            &format!(
                "/native/session/{}/board?since={cursor}&limit=1",
                root.id().raw()
            ),
            Some(pw.as_str()),
        )
        .await
        .unwrap();
        let second: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(second["posts"].as_array().unwrap().len(), 1);
        assert_eq!(second["posts"][0]["id"], root_post.id.raw());

        // Hostile/malformed queries are typed 400s, never a silent default.
        for query in [
            "?since=0",
            "?limit=0",
            "?limit=101",
            "?limit=abc",
            "?since=-1",
            "?bogus=1",
            "?since=1&limit=1&bogus=1",
        ] {
            let resp = get(
                &format!("/native/session/{}/board{query}", root.id().raw()),
                Some(pw.as_str()),
            )
            .await
            .unwrap();
            assert_eq!(resp.status(), 400, "{query}");
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["error"]["code"], "malformed", "{query}");
        }

        // Session path hostility: malformed id 400, unknown id 404 (no
        // phantom empty board for a session that does not exist).
        for (path, status) in [
            ("/native/session/nope/board", 400),
            ("/native/session/0/board", 400),
            ("/native/session/999999/board", 404),
        ] {
            assert_eq!(
                get(path, Some(pw.as_str())).await.unwrap().status(),
                status,
                "{path}"
            );
        }

        // Hostile bodies: strict DTOs and the session bounds both hold.
        for (body, status) in [
            (
                serde_json::json!({"subject": "s", "body": "b", "extra": 1}),
                400,
            ),
            (serde_json::json!({"subject": "s"}), 400),
            (serde_json::json!({"subject": "", "body": "b"}), 400),
            (
                serde_json::json!({"subject": "x".repeat(513), "body": "b"}),
                413,
            ),
            (
                serde_json::json!({"subject": "s", "body": "x".repeat(16 * 1024 + 1)}),
                413,
            ),
            (
                serde_json::json!({"subject": "s", "body": "b", "refs": "nope"}),
                400,
            ),
        ] {
            let resp = post(
                &format!("/native/session/{}/board", root.id().raw()),
                body.clone(),
            )
            .await
            .unwrap();
            assert_eq!(resp.status(), status, "{body}");
        }

        // A valid post is durable and immediately visible as the newest row.
        let resp = post(
            &format!("/native/session/{}/board", child.id().raw()),
            serde_json::json!({"subject": "native post", "body": "from the child", "refs": ["evidence://1"]}),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 201);
        let created: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(created["author_session"], child.id().raw());
        assert_eq!(created["subject"], "native post");
        let resp = get(
            &format!("/native/session/{}/board?limit=1", root.id().raw()),
            Some(pw.as_str()),
        )
        .await
        .unwrap();
        let newest: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            newest["posts"][0]["id"].as_u64().unwrap(),
            created["id"].as_u64().unwrap()
        );

        // A terminal child cannot post: the session-layer lifecycle rule
        // refuses BEFORE any durable write (typed 403).
        child
            .orchestrator_child_runtime_put(&faktor_session::child::ChildRuntimeBlockerRow {
                child_id: "board-child".into(),
                state: "cancelled".into(),
                blocker: None,
                updated_ms: 1,
            })
            .unwrap();
        let before = child.board_read_posts(None, None, 10, false).unwrap();
        let resp = post(
            &format!("/native/session/{}/board", child.id().raw()),
            serde_json::json!({"subject": "late", "body": "should refuse"}),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 403);
        let after = child.board_read_posts(None, None, 10, false).unwrap();
        assert_eq!(
            before.posts.len(),
            after.posts.len(),
            "a refused terminal post writes nothing"
        );
        let _ = handle.shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_budget_ack_failure_is_typed_retriable_and_never_claimed_applied() {
        // ACK == durable at the wire: an authoritative control-ack failure
        // must surface as a typed RETRIABLE failure (503 persistence_failed),
        // never a silent 200 applied:false and never a permanent 500 — and
        // the child mirror must not claim the change.
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let orch = deps.orchestrator.clone();
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        let (_parent, owner, isolated) = orch_owner_env(&manager, dir.path());
        let config = faktor_orchestrator::runtime::ExecConfig {
            run_id: "run-ack-wire".into(),
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
        // The control-row ACK (an UPDATE of the orchestrator_ctl fact) fails
        // while its INSERT succeeds.
        manager
            .store()
            .sql_execute(
                "CREATE TRIGGER fail_ctl_ack BEFORE UPDATE ON memory_fact \
                 WHEN NEW.kind = 'orchestrator_ctl' \
                 BEGIN SELECT RAISE(ABORT, 'injected ack failure'); END;",
            )
            .unwrap();
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/native/agents/child-0/budget"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"max_tokens": 5_000}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 503, "a failed ack is a retriable failure");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "persistence_failed");
        assert_eq!(body["error"]["retryable"], true);
        // Nothing claimed applied: the mirror keeps the pre-change budget and
        // the durable control row is still pending.
        let child = orch.child("child-0").unwrap().unwrap();
        assert_ne!(child.budget_max_tokens, Some(5_000));
        let child_session = manager
            .get_session(SessionId::new(child.session_id))
            .unwrap()
            .unwrap();
        let pending = child_session.orchestrator_ctl_pending().unwrap();
        assert!(
            pending.iter().any(|r| matches!(
                r.control,
                faktor_session::child::ChildControl::ChangeBudget { max_tokens: 5_000 }
            )),
            "the failed change left its durable row unapplied"
        );
        // Remove the seam: the retried call is idempotent and reports applied.
        manager
            .store()
            .sql_execute("DROP TRIGGER fail_ctl_ack;")
            .unwrap();
        let resp = client
            .post(format!("{base}/native/agents/child-0/budget"))
            .bearer_auth(token.as_str())
            .json(&serde_json::json!({"max_tokens": 5_000}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ack: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ack["applied"], true);
        let _ = handle.shutdown.send(());
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
        let _ = handle.shutdown.send(());
    }

    // ------------------------------------------------ split invariants
}
