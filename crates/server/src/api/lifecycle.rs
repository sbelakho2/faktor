//! `api::lifecycle`: cohesive slice of the api module.

use super::*;
use axum::routing::put;

/// The startup line the daemon prints after binding (frozen stdout
/// contract; nothing else may be printed on stdout).
pub fn startup_line(port: u16) -> String {
    format!("faktor server listening on http://127.0.0.1:{port}")
}

/// A native-server serve failure: the accept/serve loop returned an error, or
/// the owned serve task ended without a typed result (panic/abort).
#[derive(Debug, thiserror::Error)]
pub enum ServerServeError {
    /// The serve loop returned an error. The production future is never
    /// `.ok()`-discarded: the typed error is recorded in the handle's health
    /// status and returned by [`ServerHandle::shutdown`].
    #[error("native server serve {addr}: {source}")]
    Serve {
        addr: SocketAddr,
        source: std::io::Error,
    },
    /// The serve task ended without a typed serve result: it panicked, or it
    /// was aborted/cancelled out from under its owner.
    #[error("native server task {addr} ended without a typed result: {detail}")]
    Task { addr: SocketAddr, detail: String },
}

impl ServerServeError {
    /// The stable machine code of the failure (the same code recorded in
    /// [`ServerStatus::Unavailable`]).
    pub const fn code(&self) -> &'static str {
        match self {
            ServerServeError::Serve { .. } => "server_serve_failed",
            ServerServeError::Task { .. } => "server_task_failed",
        }
    }
}

/// Bound on the native server's graceful-shutdown join: the owned serve task
/// is joined within this window (a straggler is aborted), so shutdown is
/// never unbounded.
pub const SERVER_SHUTDOWN_BOUND: Duration = Duration::from_secs(5);

/// Bound on reaping the serve task after the graceful window elapsed.
pub const SERVER_ABORT_REAP_BOUND: Duration = Duration::from_secs(1);

/// A native-server shutdown failure: the owned serve task's typed result, or
/// the bounded join elapsing.
#[derive(Debug, thiserror::Error)]
pub enum ServerShutdownError {
    /// The serve task ended with a typed error (including an unexpected death
    /// that happened BEFORE the shutdown request); a clean graceful stop is
    /// the only `Ok`.
    #[error(transparent)]
    Serve(#[from] ServerServeError),
    /// The bounded graceful-shutdown join elapsed; the straggler was aborted
    /// and reaped, so shutdown was still bounded.
    #[error(
        "native server shutdown for {addr} exceeded the bounded {bound:?} join; \
         the serve task was aborted"
    )]
    ShutdownTimeout { addr: SocketAddr, bound: Duration },
}

impl ServerShutdownError {
    /// The stable machine code of the failure (the same code recorded in
    /// [`ServerStatus::Unavailable`]).
    pub const fn code(&self) -> &'static str {
        match self {
            ServerShutdownError::Serve(error) => error.code(),
            ServerShutdownError::ShutdownTimeout { .. } => "server_shutdown_timeout",
        }
    }
}

/// The shared owner state of one native-server serve task: the one-shot
/// graceful shutdown signal plus the terminal status recorded exactly once by
/// the task itself.
#[derive(Debug)]
pub(crate) struct ServerShared {
    pub(crate) shutdown: Mutex<Option<oneshot::Sender<()>>>,
    pub(crate) shutdown_requested: AtomicBool,
    pub(crate) terminal: Mutex<Option<ServerStatus>>,
    /// Test-visible signal: set by `ServerHandle::drop` when the handle was
    /// dropped while its serve task was still live (i.e. `shutdown().await`
    /// was skipped). The production drop path is the only writer; tests read
    /// it through [`ServerStatusProbe::dropped_live`] to prove a path
    /// consumed the handle.
    pub(crate) dropped_live: AtomicBool,
}

impl ServerShared {
    pub(crate) fn new(shutdown: oneshot::Sender<()>) -> Self {
        Self {
            shutdown: Mutex::new(Some(shutdown)),
            shutdown_requested: AtomicBool::new(false),
            terminal: Mutex::new(None),
            dropped_live: AtomicBool::new(false),
        }
    }

    /// Request graceful shutdown. Idempotent: the first call wins and sends
    /// the one-shot signal; every later call is a no-op. (A send failure only
    /// means the task already ended — its result is surfaced by the join.)
    pub(crate) fn request_shutdown(&self) -> bool {
        if self.shutdown_requested.swap(true, Ordering::SeqCst) {
            return false;
        }
        let sender = self
            .shutdown
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        true
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }

    /// Record the single terminal status (first writer wins; the serve task
    /// records exactly once, before its `JoinHandle` completes).
    pub(crate) fn record(&self, status: ServerStatus) {
        let mut slot = self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(status);
        }
    }

    pub(crate) fn terminal(&self) -> Option<ServerStatus> {
        self.terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Record the live-drop signal. Called from the SAME branch that emits
    /// the loud error-level diagnostic, so a test observing this flag proves
    /// the diagnostic fired.
    pub(crate) fn record_dropped_live(&self) {
        self.dropped_live.store(true, Ordering::SeqCst);
    }

    /// Read the live-drop signal (test seam; see [`ServerStatusProbe`]).
    #[cfg(test)]
    pub(crate) fn dropped_live(&self) -> bool {
        self.dropped_live.load(Ordering::SeqCst)
    }
}

/// The live handle of the native listener: it OWNS the serve task's
/// `JoinHandle<Result<(), ServerServeError>>` (never dropped or detached)
/// plus the one-shot graceful-shutdown signal. The task records its terminal
/// disposition in the shared status BEFORE its `JoinHandle` completes, so
/// [`ServerHandle::status`] always names an unexpected death typed — the
/// owning daemon can never mistake a dead native socket for a live one.
///
/// # Ownership contract
///
/// There is exactly ONE lifecycle for a daemon-critical listener: consume
/// the handle with [`ServerHandle::shutdown`] and await it. That call
/// requests the bounded graceful stop AND joins the owned serve task, so the
/// listener has terminated before the handle's ownership ends and its typed
/// result is surfaced — never `.ok()`-discarded.
///
/// `Drop` cannot await the join. A handle that falls out of scope while its
/// serve task is still live loses that guarantee: the task is aborted (the
/// synchronous last-resort fallback, so a listener can never outlive its
/// owner) and an error-level structured diagnostic is emitted — the drop is
/// a contract violation, not an equivalent stop. `#[must_use]` turns a
/// forgotten `shutdown()` into a compile-time warning, and the live-drop
/// signal on the shared owner state is test-visible so tests can prove no
/// path drops a live handle.
#[must_use = "the native serve task is owned by this handle: await \
              ServerHandle::shutdown() to join it (dropping a live handle only aborts it)"]
pub struct ServerHandle {
    pub addr: SocketAddr,
    /// The startup line the CLI prints on stdout after binding.
    pub startup_line: String,
    task: Option<JoinHandle<Result<(), ServerServeError>>>,
    shared: Arc<ServerShared>,
}

impl std::fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerHandle")
            .field("addr", &self.addr)
            .field("status", &self.status())
            .field("owns_task", &self.task.is_some())
            .finish()
    }
}

impl ServerHandle {
    /// Request graceful shutdown of the listener. Idempotent: returns `true`
    /// exactly once (the call that initiated the shutdown) and `false` when
    /// shutdown was already requested. The owned task is joined (bounded) by
    /// [`ServerHandle::shutdown`].
    pub fn request_shutdown(&self) -> bool {
        self.shared.request_shutdown()
    }

    /// `true` only while the owned serve task is alive and accepting
    /// requests.
    pub fn is_alive(&self) -> bool {
        self.status().is_alive()
    }

    /// The typed liveness snapshot the daemon queries for health decisions
    /// (see [`ServerStatus`]). A recorded terminal result wins; a task that
    /// ended without recording (panic/abort) is reported
    /// [`ServerStatus::Unavailable`] with `server_task_died`, never silently
    /// as stopped.
    pub fn status(&self) -> ServerStatus {
        if let Some(status) = self.shared.terminal() {
            return status;
        }
        let finished = match &self.task {
            Some(task) => task.is_finished(),
            None => true,
        };
        if !finished {
            return ServerStatus::Serving;
        }
        if self.shared.shutdown_requested() {
            ServerStatus::Stopped
        } else {
            ServerStatus::Unavailable {
                code: "server_task_died",
                message: format!(
                    "native server serve task for {} ended without a typed result (panic or abort)",
                    self.addr
                ),
            }
        }
    }

    /// Signal graceful shutdown and JOIN the owned serve task.
    ///
    /// This is the only lifecycle that joins the task (see the ownership
    /// contract on [`ServerHandle`]); dropping the handle while the task is
    /// live aborts it and emits an error-level diagnostic instead.
    ///
    /// - Bounded: the join waits at most [`SERVER_SHUTDOWN_BOUND`]; a
    ///   straggler is aborted and reaped within [`SERVER_ABORT_REAP_BOUND`].
    /// - Idempotent: requesting shutdown twice is a no-op (the signal is
    ///   one-shot); a handle whose task already ended joins immediately.
    /// - A clean graceful stop maps to `Ok(())`; a serve error (including an
    ///   unexpected death that happened BEFORE the request), a panicked/
    ///   aborted task, or an elapsed join bound is surfaced as the typed
    ///   [`ServerShutdownError`] — never `.ok()`-discarded.
    pub async fn shutdown(mut self) -> Result<(), ServerShutdownError> {
        let addr = self.addr;
        self.request_shutdown();
        let mut task = self
            .task
            .take()
            .expect("the native server task is owned until shutdown consumes the handle");
        match tokio::time::timeout(SERVER_SHUTDOWN_BOUND, &mut task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(ServerShutdownError::Serve(error)),
            Ok(Err(join_error)) => Err(ServerShutdownError::Serve(ServerServeError::Task {
                addr,
                detail: join_error.to_string(),
            })),
            Err(_elapsed) => {
                task.abort();
                let _ = tokio::time::timeout(SERVER_ABORT_REAP_BOUND, &mut task).await;
                Err(ServerShutdownError::ShutdownTimeout {
                    addr,
                    bound: SERVER_SHUTDOWN_BOUND,
                })
            }
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // Terminal health is read BEFORE the shutdown request so an
        // unexpected death is never masked as a requested stop.
        let status = self.status();
        let Some(task) = self.task.take() else {
            return;
        };
        if status.is_unavailable() {
            tracing::error!(addr = %self.addr, "{}", status.health_line());
        }
        let live = !task.is_finished();
        if live {
            // The caller let a LIVE handle fall out of scope: `Drop` cannot
            // await the join, so the bounded graceful stop and the typed
            // serve result are lost. This is the lifecycle violation the
            // type's ownership contract forbids (`#[must_use]`): signal it
            // loudly (structured, error-level) and abort the task so the
            // listener still cannot outlive its owner.
            self.shared.record_dropped_live();
            tracing::error!(
                addr = %self.addr,
                status = %status.health_line(),
                "ServerHandle dropped while the native serve task was still live: \
                 the bounded graceful join was skipped (call ServerHandle::shutdown().await); \
                 aborting the serve task"
            );
        }
        // Synchronous fallback so the listener task can never outlive its
        // owner: request graceful shutdown, then abort. Callers that need the
        // graceful drain call `shutdown().await`.
        self.request_shutdown();
        if live {
            task.abort();
        }
    }
}

/// Spawn the OWNED serve task: it maps the serve future's `io::Error` into
/// the typed [`ServerServeError::Serve`], records the terminal disposition in
/// the handle's shared status, and returns the typed result to its owner
/// ([`ServerHandle::shutdown`]). The production serve future is never
/// `.ok()`-discarded.
pub(crate) fn spawn_server_task<F>(
    addr: SocketAddr,
    shared: Arc<ServerShared>,
    serve: F,
) -> JoinHandle<Result<(), ServerServeError>>
where
    F: std::future::Future<Output = std::io::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let result = serve
            .await
            .map_err(|source| ServerServeError::Serve { addr, source });
        let status = match &result {
            Ok(()) if shared.shutdown_requested() => ServerStatus::Stopped,
            Ok(()) => ServerStatus::Unavailable {
                code: "server_serve_ended",
                message: format!(
                    "native server serve for {addr} completed without a shutdown request"
                ),
            },
            Err(error) => ServerStatus::Unavailable {
                code: error.code(),
                message: error.to_string(),
            },
        };
        shared.record(status);
        result
    })
}

/// Test seams for the ownership/health contract. These exercise the SAME
/// recording wrapper the production task uses (`spawn_server_task`), never a
/// parallel code path.
#[cfg(test)]
impl ServerHandle {
    /// A handle whose serve task fails immediately with `source`: unexpected
    /// termination must be recorded typed and surfaced by `shutdown`.
    pub(crate) fn failing_serve_for_test(addr: SocketAddr, source: std::io::Error) -> ServerHandle {
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
        let shared = Arc::new(ServerShared::new(shutdown_tx));
        let task = spawn_server_task(addr, Arc::clone(&shared), async move { Err(source) });
        ServerHandle {
            addr,
            startup_line: startup_line(addr.port()),
            task: Some(task),
            shared,
        }
    }

    /// A handle whose serve task completes cleanly WITHOUT a shutdown
    /// request: that is an unexpected end (the socket stopped accepting), not
    /// a requested stop, and must be recorded typed.
    pub(crate) fn ended_serve_for_test(addr: SocketAddr) -> ServerHandle {
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
        let shared = Arc::new(ServerShared::new(shutdown_tx));
        let task = spawn_server_task(addr, Arc::clone(&shared), async move { Ok(()) });
        ServerHandle {
            addr,
            startup_line: startup_line(addr.port()),
            task: Some(task),
            shared,
        }
    }

    /// Abort the owned serve task out from under the handle (an external
    /// kill), so the health snapshot must name the death.
    pub(crate) fn abort_task_for_test(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    /// A detached probe of the shared owner state, so a test can observe the
    /// recorded terminal snapshot AFTER `shutdown` consumed the handle.
    pub(crate) fn probe_for_test(&self) -> ServerStatusProbe {
        ServerStatusProbe(Arc::clone(&self.shared))
    }
}

/// Bind (port 0 = ephemeral) and serve. Returns once listening.
pub async fn serve(mut deps: ServerDeps, port: u16) -> std::io::Result<ServerHandle> {
    drain_chunk_stream(&mut deps);
    serve_arc(Arc::new(deps), port).await
}

/// Take and eagerly drain the bounded live chunk stream of one dependency
/// envelope (idempotent: a second call is a no-op). The native surface is
/// durable and journal-driven (clients page `/native/events` and
/// `/native/messages`), so there is no live subscriber to fan out to; the
/// bounded channel is consumed eagerly so the agent's coalescing sink never
/// blocks on a dead consumer. Hosts that start a SECOND listener over the
/// same deps (the worker plane) call this once before sharing the `Arc`.
pub fn drain_chunk_stream(deps: &mut ServerDeps) {
    if let Some(mut rx) = deps.chunk_rx.take() {
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
    }
}

/// Arc-taking twin of [`serve`]: lets a host run the dedicated worker-plane
/// listener over the SAME [`ServerDeps`] authority (ONE session/worker graph,
/// two listeners) without constructing a second dependency envelope. The
/// caller must have drained the chunk stream already (see
/// [`drain_chunk_stream`]); [`serve`] does that itself.
/// A method with no handler on a known route: typed JSON, never a bare 405.
async fn native_method_not_allowed() -> axum::response::Response {
    wire_status(faktor_protocol::error::ApiError {
        code: "method_not_allowed",
        message: "method not allowed".into(),
        http_status: 405,
        retryable: false,
    })
}

/// An unknown native path: typed JSON, never a bare empty 404.
async fn native_not_found() -> axum::response::Response {
    wire_status(faktor_protocol::error::ApiError {
        code: "not_found",
        message: "no such native route".into(),
        http_status: 404,
        retryable: false,
    })
}

/// Serve one axum router with HTTP/1 half-close support enabled.
///
/// `axum::serve` builds hyper's auto-connection builder with hyper's default
/// `half_close = false`: a client that sends a COMPLETE request and then
/// half-closes its write side (FIN — a normal HTTP/1.1 pattern; all bytes
/// are sent, only the read side stays open) races the response, and if the
/// dispatcher observes the EOF before the handler completes it aborts the
/// in-flight request and closes the connection WITHOUT a response. Clients
/// then see a silent empty body instead of the typed refusal this surface
/// contracts on (the adversarial body-bounds corpus pins exactly that: a
/// declared/oversized body must answer a typed 413/400 under a bounded
/// deadline, never an empty close). `axum::serve` exposes no configuration
/// hook, so the daemon drives the same auto builder itself with half-close
/// enabled for HTTP/1; protocol detection, upgrades (websockets), the
/// graceful-shutdown signal, the accept-error retry policy and the
/// completion-after-drain semantics stay identical to `axum::serve`'s loop.
pub(crate) async fn serve_with_half_close<F>(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use futures_util::FutureExt as _;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use hyper_util::service::TowerToHyperService;
    use tokio::sync::watch;
    use tower::ServiceExt as _;

    let mut builder = Builder::new(TokioExecutor::new());
    builder.http1().half_close(true);
    let service = app.map_request(|req: axum::extract::Request<hyper::body::Incoming>| {
        req.map(axum::body::Body::new)
    });

    let (signal_tx, signal_rx) = watch::channel(());
    tokio::spawn(async move {
        shutdown.await;
        drop(signal_rx);
    });
    let (close_tx, close_rx) = watch::channel(());

    loop {
        let accepted = tokio::select! {
            conn = listener.accept() => conn,
            _ = signal_tx.closed() => break,
        };
        let (io, remote_addr) = match accepted {
            Ok(pair) => pair,
            Err(err) => {
                // axum's listener contract: a transient accept error never
                // ends the serve loop; drain for a moment and retry.
                tracing::error!(error = %err, "native listener accept failed; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        // SSE frames are small writes: Nagle would coalesce them behind a
        // delayed ACK (up to ~40 ms of avoidable event->client latency).
        // `nodelay` keeps every emitted chunk on the wire immediately; the
        // daemon already batches frames per chunk above this layer.
        let _ = io.set_nodelay(true);
        tracing::trace!(?remote_addr, "connection accepted");
        let builder = builder.clone();
        let service = service.clone();
        let signal_tx = signal_tx.clone();
        let close_rx = close_rx.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(io);
            let hyper_service = TowerToHyperService::new(service);
            let mut conn =
                std::pin::pin!(builder.serve_connection_with_upgrades(io, hyper_service));
            let mut signal_closed = std::pin::pin!(signal_tx.closed().fuse());
            loop {
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(err) = result {
                            tracing::debug!(error = %err, "native connection ended");
                        }
                        break;
                    }
                    _ = &mut signal_closed => {
                        conn.as_mut().graceful_shutdown();
                    }
                }
            }
            drop(close_rx);
        });
    }

    drop(close_rx);
    drop(listener);
    close_tx.closed().await;
    Ok(())
}

fn native_core_routes(router: Router<AppState>) -> Router<AppState> {
    router
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
        // Engineering Outcome Ledger (economic audit item 7, additive):
        // durable completed-run metrics aggregates plus one bounded run page.
        .route("/native/outcomes", get(native_outcomes))
        // Wave 3 commercial metering (additive): the per-organization usage
        // fold + cursor page (the `?org=` branch of /native/usage above),
        // the derived entitlement snapshot, and the admin-only credit grant.
        // Without the `[billing]` section every one answers 409.
        .route("/native/entitlements", get(native_entitlements))
        .route("/native/credits/grant", post(native_credits_grant))
        // Value-centric paid entitlements (additive): the plan-mapped,
        // inference-independent value report (value features, governance
        // capability flags, one typed decision per entitlement). Without the
        // `[billing]` section it answers the same typed 409.
        .route("/native/value-entitlements", get(native_value_entitlements))
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
}

fn native_foundation_routes(router: Router<AppState>) -> Router<AppState> {
    router
        // ---- Edition 5 foundation (settings/runtime contract) ----
        .route(
            "/native/config/descriptors",
            get(foundation::native_config_descriptors),
        )
        .route(
            "/native/config/snapshot",
            get(foundation::native_config_snapshot),
        )
        .route(
            "/native/config/preview",
            post(foundation::native_config_preview),
        )
        .route(
            "/native/config/commit",
            post(foundation::native_config_commit),
        )
        .route(
            "/native/config/rollback",
            post(foundation::native_config_rollback),
        )
        .route(
            "/native/config/runtime-ack",
            post(foundation::native_config_runtime_ack),
        )
        .route(
            "/native/config/search",
            get(foundation::native_config_search),
        )
        .route(
            "/native/config/export",
            get(foundation::native_config_export),
        )
        .route(
            "/native/config/import",
            post(foundation::native_config_import),
        )
        .route(
            "/native/providers/connections",
            get(foundation::native_provider_connections)
                .post(foundation::native_provider_connection_save),
        )
        .route(
            "/native/providers/connections/{id}",
            delete(foundation::native_provider_connection_delete),
        )
        .route(
            "/native/providers/connections/{id}/test",
            post(foundation::native_provider_connection_test),
        )
        .route(
            "/native/providers/connections/{id}/health",
            get(foundation::native_provider_connection_health),
        )
        .route(
            "/native/providers/credentials",
            get(foundation::native_provider_credentials)
                .post(foundation::native_provider_credential_store),
        )
        .route(
            "/native/providers/credentials/rotate",
            post(foundation::native_provider_credential_rotate),
        )
        .route(
            "/native/providers/credentials/{handle}",
            delete(foundation::native_provider_credential_delete),
        )
        .route(
            "/native/model-parameters",
            get(foundation::native_model_parameters),
        )
        .route(
            "/native/model-parameters/compile",
            post(foundation::native_model_compile),
        )
        .route(
            "/native/call-receipts",
            get(foundation::native_call_receipts),
        )
        .route(
            "/native/session/{id}/context/preview",
            post(foundation::native_context_preview),
        )
        .route(
            "/native/session/{id}/context/manifests",
            post(foundation::native_context_capture),
        )
        .route(
            "/native/context/manifest",
            get(foundation::native_context_manifest),
        )
        .route(
            "/native/context/manifests",
            get(foundation::native_context_manifests),
        )
        .route(
            "/native/session/{id}/context/compact",
            post(foundation::native_context_compact),
        )
        // Rev8 durable compaction operations (S10/K-03/K-04): explicit op id,
        // durable progress/cancel and atomic checkpoint+event commit.
        .route(
            "/native/session/{id}/context/compaction",
            post(longtask::native_compaction_start).get(longtask::native_compaction_latest),
        )
        .route(
            "/native/session/{id}/context/compaction/{operation}",
            get(longtask::native_compaction_status),
        )
        .route(
            "/native/session/{id}/context/compaction/{operation}/cancel",
            post(longtask::native_compaction_cancel),
        )
        .route(
            "/native/permissions/policy",
            get(foundation::native_permissions_policy),
        )
        .route(
            "/native/permissions/rules",
            post(foundation::native_permission_rule_save),
        )
        .route(
            "/native/permissions/rules/{id}",
            delete(foundation::native_permission_rule_delete),
        )
        .route(
            "/native/permissions/preview",
            post(foundation::native_permission_preview),
        )
        .route(
            "/native/permissions/audit",
            get(foundation::native_permission_audit),
        )
        .route(
            "/native/diagnostics/preview",
            get(foundation::native_diagnostics_preview),
        )
        .route(
            "/native/diagnostics/export",
            post(foundation::native_diagnostics_export),
        )
        // ---- Edition 5 lifecycle surfaces ----
        .route(
            "/native/sessions/parallel",
            get(foundation_ext::native_sessions_parallel),
        )
        .route(
            "/native/sessions/{id}/meta",
            get(foundation_ext::native_session_meta)
                .post(foundation_ext::native_session_meta_update),
        )
        .route(
            "/native/transcript/search",
            get(foundation_ext::native_transcript_search),
        )
        .route(
            "/native/memory",
            get(foundation_ext::native_memory_list).post(foundation_ext::native_memory_write),
        )
        // Rev8 memory V2 management (S12/S66/K-10/K-11): provenance,
        // optimistic versions, tombstone/restore, dedupe, bounded retention.
        .route(
            "/native/memory/records",
            get(longtask::native_memory_records_list).post(longtask::native_memory_records_create),
        )
        .route(
            "/native/memory/records/retention",
            post(longtask::native_memory_records_retention),
        )
        .route(
            "/native/memory/records/{id}",
            put(longtask::native_memory_records_update)
                .delete(longtask::native_memory_records_delete),
        )
        .route(
            "/native/memory/records/{id}/restore",
            post(longtask::native_memory_records_restore),
        )
        .route(
            "/native/memory/policy",
            get(longtask::native_memory_policy_get).put(longtask::native_memory_policy_put),
        )
        // Rev8 instruction inspection/editing (S11/S69/I-11).
        .route(
            "/native/instructions",
            get(instructions::native_instructions_list),
        )
        .route(
            "/native/instructions/preview",
            get(instructions::native_instructions_preview),
        )
        .route(
            "/native/instructions/source",
            put(instructions::native_instructions_source_put),
        )
        .route(
            "/native/memory/retention",
            post(foundation_ext::native_memory_retention),
        )
        .route(
            "/native/memory/{kind}/{key}",
            delete(foundation_ext::native_memory_delete),
        )
        .route(
            "/native/mcp/servers",
            get(foundation_ext::native_mcp_servers).post(foundation_ext::native_mcp_server_save),
        )
        .route(
            "/native/mcp/servers/{id}",
            delete(foundation_ext::native_mcp_server_delete),
        )
        .route(
            "/native/mcp/servers/{id}/start",
            post(foundation_ext::native_mcp_server_start),
        )
        .route(
            "/native/mcp/servers/{id}/stop",
            post(foundation_ext::native_mcp_server_stop),
        )
        .route(
            "/native/mcp/servers/{id}/tools",
            get(foundation_ext::native_mcp_server_tools),
        )
        .route(
            "/native/mcp/servers/{id}/connect",
            post(foundation_ext::native_mcp_server_connect),
        )
        .route(
            "/native/mcp/servers/{id}/resources",
            get(foundation_ext::native_mcp_server_resources),
        )
        .route(
            "/native/mcp/servers/{id}/auth",
            get(foundation_ext::native_mcp_server_auth_get)
                .post(foundation_ext::native_mcp_server_auth),
        )
        .route(
            "/native/mcp/servers/{id}/oauth/revoke",
            post(foundation_ext::native_mcp_server_oauth_revoke),
        )
        .route("/native/skills", get(foundation_ext::native_skills))
        .route(
            "/native/skills/install",
            post(foundation_ext::native_skill_install),
        )
        .route(
            "/native/skills/{id}",
            get(foundation_ext::native_skill_get).delete(foundation_ext::native_skill_delete),
        )
        .route(
            "/native/skills/{id}/trust",
            post(foundation_ext::native_skill_trust),
        )
        .route(
            "/native/skills/{id}/invocation",
            post(foundation_ext::native_skill_invocation),
        )
        .route(
            "/native/skills/{id}/invoke",
            post(foundation_ext::native_skill_invoke),
        )
        .route(
            "/native/hooks",
            get(foundation_ext::native_hooks).post(foundation_ext::native_hook_save),
        )
        .route(
            "/native/hooks/audit",
            get(foundation_ext::native_hook_audit),
        )
        .route(
            "/native/hooks/{id}",
            delete(foundation_ext::native_hook_delete),
        )
        .route(
            "/native/hooks/{id}/test",
            post(foundation_ext::native_hook_test),
        )
        .route("/native/tools", get(foundation_ext::native_tools))
        .route(
            "/native/browser/status",
            get(foundation_ext::native_browser_status),
        )
        .route(
            "/native/browser/validate-url",
            post(foundation_ext::native_browser_validate_url),
        )
}

fn native_r8_routes(router: Router<AppState>) -> Router<AppState> {
    router
        // ---- Revision 8 daemon contract layer (additive) ----
        // Config provenance/conflicts (S60/S64) and single-scope reset.
        .route(
            "/native/config/provenance",
            get(r8::native_config_provenance),
        )
        .route("/native/config/reset", post(r8::native_config_reset))
        // Durable instruction/rule records, previews, imports and captured
        // bundles (S11/S69/I-01..I-11). Distinct from the file-tree
        // inspection routes at /native/instructions above.
        .route(
            "/native/instructions/records",
            get(r8::native_instruction_records).post(r8::native_instruction_upsert),
        )
        .route(
            "/native/instructions/records/preview",
            post(r8::native_instruction_preview),
        )
        .route(
            "/native/instructions/records/{id}",
            get(r8::native_instruction_get).delete(r8::native_instruction_delete),
        )
        .route(
            "/native/instructions/import",
            post(r8::native_instruction_import),
        )
        .route(
            "/native/instructions/bindings",
            get(r8::native_instruction_bindings).post(r8::native_instruction_bind),
        )
        // Versioned agent presets (S06/S07/S33/A-01..A-12).
        .route("/native/presets/binding", get(r8::native_preset_binding))
        .route(
            "/native/presets",
            get(r8::native_presets).post(r8::native_preset_upsert),
        )
        .route(
            "/native/presets/{id}",
            get(r8::native_preset_get).delete(r8::native_preset_delete),
        )
        .route(
            "/native/presets/{id}/envelope",
            get(r8::native_preset_envelope),
        )
        .route(
            "/native/presets/{id}/roles",
            get(r8::native_preset_roles_get).post(r8::native_preset_roles_set),
        )
        .route(
            "/native/presets/{id}/fallback",
            get(r8::native_preset_fallback_get).post(r8::native_preset_fallback_set),
        )
        // Bounded live catalog probe (M-08).
        .route(
            "/native/catalog/probe",
            post(foundation::native_catalog_probe),
        )
        // Runtime executable trust (S34/S35/S56, O-04/O-05) and status.
        .route(
            "/native/runtime/trust",
            get(r8::native_runtime_trust_get).post(r8::native_runtime_trust_confirm),
        )
        .route(
            "/native/runtime/trust/check",
            post(r8::native_runtime_trust_check),
        )
        .route("/native/runtime/status", get(r8::native_runtime_status))
        // Browser adapter/session lifecycle, bounded observations and
        // content-addressed captures (S20/X-06..X-09). There is deliberately
        // no route that marks a session connected or fabricates an event.
        .route("/native/browser/adapters", get(r8::native_browser_adapters))
        .route(
            "/native/browser/sessions",
            get(r8::native_browser_sessions).post(r8::native_browser_session_request),
        )
        .route(
            "/native/browser/sessions/{id}/observations",
            get(r8::native_browser_observations),
        )
        .route(
            "/native/browser/sessions/{id}/screenshot",
            post(r8::native_browser_screenshot),
        )
        .route(
            "/native/browser/artifacts/{id}",
            get(r8::native_browser_artifact),
        )
        .route(
            "/native/browser/artifacts/{id}/bytes",
            get(r8::native_browser_artifact_bytes),
        )
        // Context pin/exclude decisions and explicit export (S08/S62/C-07/C-14).
        .route(
            "/native/session/{id}/context/pins",
            get(r8::native_context_pins)
                .post(r8::native_context_pin_set)
                .delete(r8::native_context_pin_delete),
        )
        .route("/native/context/export", get(r8::native_context_export))
        // Measured efficiency effects (S27/S65/K-12) and budget revision
        // (S41/B-05).
        .route(
            "/native/session/{id}/efficiency",
            get(r8::native_session_efficiency),
        )
        .route(
            "/native/session/{id}/budget",
            get(r8::native_task_budget_get).put(r8::native_task_budget_put),
        )
        .route(
            "/native/session/{id}/fork",
            post(foundation_ext::native_session_fork),
        )
        .route(
            "/native/session/{id}/checkpoints/{sequence}/restore",
            post(foundation_ext::native_checkpoint_restore),
        )
        .route(
            "/native/session/{id}/checkpoints/{sequence}/restore/preview",
            post(foundation_ext::native_checkpoint_restore_preview),
        )
        // Review comments (V-02) and per-file change acceptance (V-03).
        .route(
            "/native/session/{id}/review/comments",
            get(foundation_ext::native_review_comments_list)
                .post(foundation_ext::native_review_comment_create),
        )
        .route(
            "/native/session/{id}/review/comments/{comment}",
            axum::routing::patch(foundation_ext::native_review_comment_update)
                .delete(foundation_ext::native_review_comment_delete),
        )
        .route(
            "/native/session/{id}/changes/accept",
            post(foundation_ext::native_change_accept),
        )
        // Session delete/export (D-06) and selectable session settings
        // (K-02/B-09).
        .route(
            "/native/session/{id}",
            axum::routing::delete(foundation_ext::native_session_delete),
        )
        .route(
            "/native/session/{id}/export",
            post(foundation_ext::native_session_export),
        )
        .route(
            "/native/session/{id}/settings",
            get(foundation_ext::native_session_settings_get)
                .put(foundation_ext::native_session_settings_put),
        )
        .route(
            "/native/agents/{child_id}/stage",
            post(foundation_ext::native_agent_stage),
        )
        .route(
            "/native/agents/{child_id}/merge",
            post(foundation_ext::native_agent_merge),
        )
        .route("/native/session/{id}/usage", get(native_session_usage))
        .route(
            "/native/session/{id}/tasks/{task_id}/verification",
            get(native_task_verification),
        )
        // Additive proof surfacing (read-only, session-scoped via the
        // REQUIRED ?session= query): the full VERIFIED story of one task in
        // one strict payload, and the durable completion-step status/report
        // read. Store errors and present-but-corrupt rows fail closed; the
        // payload distinguishes missing / unavailable / corrupt explicitly.
        .route("/native/tasks/{id}/proof", get(native_task_proof))
        .route(
            "/native/tasks/{id}/completion-steps",
            get(native_task_completion_steps),
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
        // payload into the session's durable CAS-backed store, resolve ONE
        // reference by its stable `ref_id` (metadata and THAT reference's
        // MIME-tagged bytes), resolve one blob's metadata by digest (exactly
        // one reference; several references are a typed 409 that lists the
        // candidate ref ids), and fetch one blob's raw octet-stream bytes.
        // The route grammar is strictly separated: `ref/{ref_id}` is
        // canonical-decimal id-addressed, `blob/{digest}` is 64-hex
        // digest-addressed, so a digest can never collide with a ref id.
        // IMAGES are stored here and validated against the CHOSEN model's
        // capabilities (vision, deliverable mime, per-provider byte bound) at
        // task admission, where a refusal keeps the draft and bytes intact.
        .route(
            "/native/session/{id}/attachments",
            post(native_attachment_upload),
        )
        .route(
            "/native/session/{id}/attachments/blob/{digest}",
            get(native_attachment_blob_get),
        )
        .route(
            "/native/session/{id}/attachments/blob/{digest}/bytes",
            get(native_attachment_blob_bytes),
        )
        .route(
            "/native/session/{id}/attachments/ref/{ref_id}",
            get(native_attachment_get),
        )
        .route(
            "/native/session/{id}/attachments/ref/{ref_id}/bytes",
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
        // Index coverage diagnostics (audits 5/6, additive): durable
        // per-workspace coverage/fingerprint coverage/freshness; pure read.
        .route("/native/index/coverage", get(native_index_coverage))
}

fn native_control_routes(router: Router<AppState>) -> Router<AppState> {
    router
        // Control-plane surface (additive; disabled by default): identity,
        // organizations, members, synced repositories and approvals. Strict
        // DTOs, cursor pagination, idempotency keys and tenant isolation are
        // enforced in `native::control_plane`; with no control plane wired
        // every route answers a typed 409 `cloud_disabled`.
        .route("/native/identity", get(native_identity))
        .route(
            "/native/orgs",
            get(native_orgs_list).post(native_orgs_create),
        )
        .route(
            "/native/orgs/{id}/members",
            get(native_org_members_list).post(native_org_members_invite),
        )
        // Invitation acceptance (the single-use redemption end of the
        // invite flow): `accept_invitation` was previously unreachable over
        // the wire. Rides the daemon password + a control-plane principal;
        // the body carries the single-use token and the idempotency key.
        .route("/native/invitations/accept", post(native_invitation_accept))
        .route("/native/repositories", get(native_repositories))
        // GitHub App webhook ingress (additive; disabled by default): the
        // ONLY route without the daemon password — the HMAC signature is the
        // credential, and the wired sink verifies + claims the delivery
        // before scheduling its idempotent re-sync. Without a sink the route
        // answers a typed 409 `scm_webhook_disabled`.
        .route("/native/scm/webhook", post(native_scm_webhook))
        .route(
            "/native/approvals",
            get(native_approvals_list).post(native_approvals_create),
        )
        .route(
            "/native/approvals/{id}/decide",
            post(native_approvals_decide),
        )
        // Signed updater surface (additive; disabled by default): status,
        // check, stage, apply, rollback and the explicitly authorized
        // downgrade below the anti-rollback floor. Every route needs a
        // control-plane principal (viewer for status/check, member for
        // stage, ADMIN for apply/rollback/downgrade); with no `[updater]`
        // section wired every route answers a typed 409 `updater_disabled`.
        .route("/native/updater/status", get(native_updater_status))
        .route("/native/updater/check", post(native_updater_check))
        .route("/native/updater/stage", post(native_updater_stage))
        .route("/native/updater/apply", post(native_updater_apply))
        .route("/native/updater/rollback", post(native_updater_rollback))
        .route("/native/updater/downgrade", post(native_updater_downgrade))
        // Remote/VPC worker plane (additive; disabled by default). Worker
        // routes authenticate with a registration token (no daemon
        // password); operator routes ride the daemon password + a
        // control-plane principal. Without the `[workers]` section every
        // route answers 409 `workers_disabled`.
        .route("/native/workers/register", post(native_worker_register))
        .route("/native/workers", get(native_workers_list))
        .route("/native/workers/tokens", post(native_worker_token_mint))
        .route(
            "/native/workers/{id}/heartbeat",
            post(native_worker_heartbeat),
        )
        .route("/native/workers/{id}/revoke", post(native_worker_revoke))
        .route("/native/jobs/claim", post(native_job_claim))
        .route("/native/jobs/{id}", get(native_job_status))
        .route("/native/jobs/{id}/result", post(native_job_result))
        // SSO login surface (additive; disabled by default): start mints an
        // authorization URL for the organization's IdP, callback consumes the
        // single-use state and mints ONE control-plane session. Both routes
        // ride the daemon password; with no SSO authority wired every route
        // answers a typed 409 `sso_disabled`.
        .route("/native/sso/start", post(native_sso_start))
        .route("/native/sso/callback", post(native_sso_callback))
        // Logout (additive): durably revoke the presented control-plane
        // session. Rides the daemon password (like start/callback) PLUS the
        // session's own token in `x-faktor-control-token`; a foreign session
        // id is the same typed 404 a missing one answers, and a second
        // logout is the typed idempotent replay.
        .route("/native/sso/logout", post(native_sso_logout))
        // Enterprise plane (additive; disabled by default): retention
        // artifacts + guarded GC, the audit-ledger cursor export, deletion
        // jobs, admin settings and the effective-config attestation. Every
        // route needs a control-plane principal; with no `[enterprise]`
        // service wired every route answers a typed 409
        // `enterprise_disabled`.
        .route("/native/enterprise/status", get(native_enterprise_status))
        .route("/native/enterprise/audit", get(native_enterprise_audit))
        .route(
            "/native/enterprise/settings",
            get(native_enterprise_settings_get).put(native_enterprise_settings_put),
        )
        .route(
            "/native/enterprise/artifacts",
            get(native_enterprise_artifacts_list).post(native_enterprise_artifacts_register),
        )
        .route(
            "/native/enterprise/artifacts/{id}/eligible",
            post(native_enterprise_artifact_eligible),
        )
        .route(
            "/native/enterprise/retention/gc",
            post(native_enterprise_gc),
        )
        .route(
            "/native/enterprise/deletion-jobs",
            get(native_enterprise_deletion_jobs_list).post(native_enterprise_deletion_jobs_create),
        )
        .route(
            "/native/enterprise/deletion-jobs/{id}",
            get(native_enterprise_deletion_job_get),
        )
        .route(
            "/native/enterprise/deletion-jobs/{id}/advance",
            post(native_enterprise_deletion_job_advance),
        )
        .route(
            "/native/enterprise/tombstones/{scope_key}",
            get(native_enterprise_tombstone),
        )
        .route(
            "/native/enterprise/effective-config",
            post(native_enterprise_effective_config),
        )
}

pub async fn serve_arc(deps: Arc<ServerDeps>, port: u16) -> std::io::Result<ServerHandle> {
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
    let app = native_core_routes(Router::new());
    let app = native_foundation_routes(app);
    let app = native_r8_routes(app);
    let app = native_control_routes(app);
    let app = app
        // Every rejection is the frozen typed envelope, never an empty body.
        .method_not_allowed_fallback(native_method_not_allowed)
        .fallback(native_not_found)
        // The tower limit alone leaves axum's 2 MiB Json-extractor default in
        // force: a 2 MiB..10 MiB body was a misleading `400 malformed`.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .with_state(AppState {
            deps,
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: ready.clone(),
        });
    if set_ready {
        ready.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shared = Arc::new(ServerShared::new(shutdown_tx));
    // The handle OWNS this task: its result is recorded in `shared` and
    // returned to `shutdown()`; nothing is discarded here.
    let ready_on_shutdown = Arc::clone(&ready);
    let task = spawn_server_task(addr, Arc::clone(&shared), async move {
        serve_with_half_close(listener, app, async move {
            let _ = shutdown_rx.await;
            // Long-lived SSE streams observe this bounded flag (one fallback
            // tick) and end, so graceful shutdown never waits on a client
            // that already dropped its body.
            ready_on_shutdown.store(false, std::sync::atomic::Ordering::SeqCst);
        })
        .await
    });
    Ok(ServerHandle {
        addr,
        startup_line,
        task: Some(task),
        shared,
    })
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
