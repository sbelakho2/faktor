//! The worker-plane deployment boundary: a SECOND listener with its OWN
//! identity, separate from the loopback-oriented native listener.
//!
//! Why a boundary exists: the worker HTTP routes
//! (`/native/workers/register`, `/native/workers/{id}/heartbeat`,
//! `/native/jobs/claim`, `/native/jobs/{id}/result`) are **remote
//! credential/protocol endpoints** — they carry worker registration tokens
//! and job payload/result material across hosts. The ordinary native
//! listener is the local IDE surface: it binds `127.0.0.1` unconditionally
//! (`api::serve`) and is not configurable at all, so it can never be
//! accidentally exposed. The worker plane is the only listener an operator
//! may bind beyond loopback, and only under the rules below.
//!
//! Rules, exactly:
//!
//! - **Separate config keys, separate construction.** The worker plane's
//!   bind/transport/auth come from the CLI's `[worker_plane]` section and
//!   this module's [`WorkerPlaneBindConfig`]; the native listener's bind is
//!   the hardcoded loopback address in [`crate::api::serve`]. Neither
//!   listener reads the other's keys.
//! - **Loopback by default.** [`DEFAULT_WORKER_PLANE_BIND`] is
//!   `127.0.0.1:8790`: a plaintext worker socket on loopback is allowed
//!   with no further acknowledgement.
//! - **TLS-or-gateway-only beyond loopback.** This workspace has NO inbound
//!   TLS stack (rustls is present only as a transitive outbound
//!   HTTP-client dependency of `reqwest`/`hyper-rustls`; no
//!   `TlsAcceptor`/server `ServerConfig` exists anywhere in the tree), so
//!   in-process TLS termination is refused typed
//!   ([`WorkerPlaneBoundaryRefusal::TlsUnavailableInProcess`]) rather than
//!   fabricated. The only supported non-loopback mode therefore is
//!   **gateway-only**: an external, trusted reverse proxy terminates TLS
//!   (and, under [`WorkerPlaneAuth::GatewayMtls`], client mTLS), and the
//!   operator records the explicit acknowledgement
//!   `trusted_gateway = true` naming this boundary. Any non-loopback bind
//!   without that acknowledgement is a typed startup refusal
//!   ([`WorkerPlaneBoundaryRefusal::BeyondLoopbackWithoutTransport`]).
//! - **Own transport credential.** The worker plane never accepts the
//!   daemon password. Every request still carries the worker registration
//!   token in its body (unchanged protocol); additionally, when `bearer` is
//!   configured, every worker-plane request must present
//!   `Authorization: Bearer <bearer>` — the gateway's transport credential —
//!   checked constant-time through
//!   [`crate::auth::check_bearer_value`].
//! - **Worker routes only.** The dedicated socket mounts ONLY the four
//!   worker routes above; the native surface (health/ready/sessions/… and
//!   the operator half of the worker plane) is unreachable there (404). The
//!   operator routes stay on the loopback native listener.
//!
//! The validated decision is a [`WorkerPlaneExposure`] witness; its
//! [`WorkerPlaneExposure::audit_line`] is logged at startup (so the gateway
//! acknowledgement is recorded in the daemon's audit log) and reported by
//! `faktor doctor --config`.
//!
//! The listener is an OWNED async task: [`WorkerPlaneHandle`] holds its
//! `JoinHandle` plus the one-shot shutdown signal, the task records its
//! terminal disposition in a daemon-queryable [`WorkerPlaneStatus`] BEFORE the
//! handle completes (a dead socket can never look alive), and
//! [`WorkerPlaneHandle::shutdown`] joins the task within a fixed bound. The
//! production serve future is never `.ok()`-discarded, and dropping the
//! handle aborts the task rather than detaching it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::header;
use axum::middleware::Next;
use axum::routing::post;
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;

use faktor_protocol::error::ApiError;

use crate::api::{AppState, ServerDeps, MAX_BODY_BYTES};
use crate::auth::check_bearer_value;
use crate::native::{
    native_job_claim, native_job_result, native_worker_heartbeat, native_worker_register,
    wire_status,
};

/// The default worker-plane bind: loopback. Only an explicit operator
/// `[worker_plane] bind` moves it, and only under the gateway rules.
pub const DEFAULT_WORKER_PLANE_BIND: &str = "127.0.0.1:8790";
/// Bound on the worker plane's transport bearer (same bound as a worker
/// registration token).
pub const MAX_WORKER_PLANE_BEARER_BYTES: usize = 512;

/// The transport security of the worker-plane socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerPlaneTransport {
    /// Plaintext HTTP. Valid on loopback, or beyond loopback ONLY behind an
    /// explicitly acknowledged TLS-terminating gateway.
    Plaintext,
    /// In-process TLS termination. NOT AVAILABLE in this workspace (no
    /// inbound TLS stack is compiled in): [`WorkerPlaneBindConfig::validate`]
    /// refuses it typed instead of fabricating an acceptor.
    Tls,
}

/// The worker plane's client-authentication mode (its OWN transport
/// identity, never the daemon password).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerPlaneAuth {
    /// The worker registration token (body credential) is the only
    /// client credential; an optional transport `bearer` may additionally
    /// gate every request.
    WorkerTokens,
    /// The trusted gateway terminates client mTLS and authenticates itself
    /// to this socket with the configured transport `bearer`. Requires
    /// `trusted_gateway = true` (mTLS termination exists only at the
    /// boundary) and a configured `bearer`.
    GatewayMtls,
}

impl WorkerPlaneAuth {
    /// The stable config spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            WorkerPlaneAuth::WorkerTokens => "worker_tokens",
            WorkerPlaneAuth::GatewayMtls => "gateway_mtls",
        }
    }
}

/// The operator-selected worker-plane bind/transport configuration (CLI
/// `[worker_plane]` keys, resolved). Validation is explicit and typed: see
/// [`WorkerPlaneBindConfig::validate`]. `Debug` redacts the transport
/// bearer: the credential never enters a log line through this type.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkerPlaneBindConfig {
    pub bind: SocketAddr,
    pub transport: WorkerPlaneTransport,
    /// The explicit acknowledgement naming the deployment boundary: an
    /// external, trusted TLS-terminating (and, for `GatewayMtls`,
    /// client-authenticating) gateway fronts this socket.
    pub trusted_gateway: bool,
    pub auth: WorkerPlaneAuth,
    /// Optional transport bearer required on every worker-plane request (the
    /// gateway's credential in `GatewayMtls` mode).
    pub bearer: Option<String>,
}

impl std::fmt::Debug for WorkerPlaneBindConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPlaneBindConfig")
            .field("bind", &self.bind)
            .field("transport", &self.transport)
            .field("trusted_gateway", &self.trusted_gateway)
            .field("auth", &self.auth)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The validated worker-plane exposure decision (a witness: it exists only
/// for a config that passed [`WorkerPlaneBindConfig::validate`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerPlaneExposure {
    pub bind: SocketAddr,
    pub transport: WorkerPlaneTransport,
    pub auth: WorkerPlaneAuth,
    pub trusted_gateway: bool,
    /// `true` when the socket is bound beyond loopback. Only possible with
    /// the `trusted_gateway` acknowledgement.
    pub beyond_loopback: bool,
    /// `true` when a transport bearer gates every request.
    pub transport_bearer: bool,
}

impl WorkerPlaneExposure {
    /// The startup audit line recording the boundary decision (including the
    /// gateway acknowledgement when one was given).
    pub fn audit_line(&self) -> String {
        format!(
            "worker plane boundary: bind={} exposure={} transport={} auth={} trusted_gateway={}{}",
            self.bind,
            if self.beyond_loopback {
                "beyond-loopback"
            } else {
                "loopback"
            },
            match self.transport {
                WorkerPlaneTransport::Plaintext => "plaintext",
                WorkerPlaneTransport::Tls => "tls",
            },
            self.auth.as_str(),
            self.trusted_gateway,
            if self.transport_bearer {
                " transport_bearer=required"
            } else {
                ""
            },
        )
    }
}

/// A typed worker-plane deployment-boundary refusal. Every variant NAMES the
/// boundary in its message: a refusal never silently downgrades the
/// transport, and startup never continues half-exposed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerPlaneBoundaryRefusal {
    #[error(
        "worker-plane deployment boundary refused: in-process TLS termination was requested for {bind}, \
         but this workspace has no inbound TLS stack; terminate TLS at a trusted gateway and set \
         trusted_gateway = true (the acknowledgement names the worker-plane deployment boundary)"
    )]
    TlsUnavailableInProcess { bind: SocketAddr },
    #[error(
        "worker-plane deployment boundary refused: binding {bind} exposes the remote worker \
         credential/protocol endpoints beyond loopback without transport security; either bind a \
         loopback address or terminate TLS at a trusted gateway and set trusted_gateway = true \
         (the acknowledgement names the worker-plane deployment boundary)"
    )]
    BeyondLoopbackWithoutTransport { bind: SocketAddr },
    #[error(
        "worker-plane deployment boundary refused: auth = gateway_mtls requires \
         trusted_gateway = true on {bind} (mTLS terminates only at the acknowledged boundary)"
    )]
    GatewayMtlsWithoutGateway { bind: SocketAddr },
    #[error(
        "worker-plane deployment boundary refused: auth = gateway_mtls on {bind} requires `bearer` \
         (the trusted gateway's transport credential)"
    )]
    GatewayMtlsWithoutBearer { bind: SocketAddr },
    #[error(
        "worker-plane deployment boundary refused: the transport bearer on {bind} must be \
         1..={MAX_WORKER_PLANE_BEARER_BYTES} printable ASCII bytes without whitespace"
    )]
    MalformedTransportBearer { bind: SocketAddr },
}

impl WorkerPlaneBoundaryRefusal {
    /// The stable machine code of the refusal.
    pub const fn code(&self) -> &'static str {
        "worker_plane_boundary_refused"
    }
}

impl WorkerPlaneBindConfig {
    /// Validate the boundary rules and produce the exposure witness.
    ///
    /// Order matters: an in-process TLS request is refused FIRST (it can
    /// never be honored in this build), then the gateway acknowledgement
    /// rules, then the beyond-loopback rule.
    pub fn validate(&self) -> Result<WorkerPlaneExposure, WorkerPlaneBoundaryRefusal> {
        let bind = self.bind;
        if self.transport == WorkerPlaneTransport::Tls {
            return Err(WorkerPlaneBoundaryRefusal::TlsUnavailableInProcess { bind });
        }
        if self.auth == WorkerPlaneAuth::GatewayMtls && !self.trusted_gateway {
            return Err(WorkerPlaneBoundaryRefusal::GatewayMtlsWithoutGateway { bind });
        }
        if self.auth == WorkerPlaneAuth::GatewayMtls && self.bearer.is_none() {
            return Err(WorkerPlaneBoundaryRefusal::GatewayMtlsWithoutBearer { bind });
        }
        if let Some(bearer) = &self.bearer {
            let shaped = !bearer.is_empty()
                && bearer.len() <= MAX_WORKER_PLANE_BEARER_BYTES
                && bearer.bytes().all(|b| b.is_ascii_graphic());
            if !shaped {
                return Err(WorkerPlaneBoundaryRefusal::MalformedTransportBearer { bind });
            }
        }
        let beyond_loopback = !self.bind.ip().is_loopback();
        if beyond_loopback && !self.trusted_gateway {
            return Err(WorkerPlaneBoundaryRefusal::BeyondLoopbackWithoutTransport { bind });
        }
        Ok(WorkerPlaneExposure {
            bind,
            transport: self.transport,
            auth: self.auth,
            trusted_gateway: self.trusted_gateway,
            beyond_loopback,
            transport_bearer: self.bearer.is_some(),
        })
    }
}

/// A worker-plane failure: the boundary refusal, the socket bind, the accept
/// loop, or the owned serve task ending without a result.
#[derive(Debug, thiserror::Error)]
pub enum WorkerPlaneServeError {
    #[error("{0}")]
    Boundary(#[from] WorkerPlaneBoundaryRefusal),
    #[error("worker plane bind {bind}: {source}")]
    Bind {
        bind: SocketAddr,
        source: std::io::Error,
    },
    /// The accept/serve loop returned an error (the production future is
    /// never `.ok()`-discarded: the typed error is recorded in the handle's
    /// health status and returned by [`WorkerPlaneHandle::shutdown`]).
    #[error("worker plane serve {bind}: {source}")]
    Serve {
        bind: SocketAddr,
        source: std::io::Error,
    },
    /// The serve task ended without a typed serve result: it panicked, or it
    /// was aborted/cancelled out from under its owner.
    #[error("worker plane task {bind} ended without a typed result: {detail}")]
    Task { bind: SocketAddr, detail: String },
    /// The bounded graceful-shutdown join elapsed; the straggler was aborted
    /// and reaped, so shutdown was still bounded.
    #[error(
        "worker plane shutdown for {bind} exceeded the bounded {bound:?} join; \
         the serve task was aborted"
    )]
    ShutdownTimeout {
        bind: SocketAddr,
        bound: std::time::Duration,
    },
}

impl WorkerPlaneServeError {
    /// The stable machine code of the failure (the same code recorded in
    /// [`WorkerPlaneStatus::Unavailable`]).
    pub const fn code(&self) -> &'static str {
        match self {
            WorkerPlaneServeError::Boundary(refusal) => refusal.code(),
            WorkerPlaneServeError::Bind { .. } => "worker_plane_bind_failed",
            WorkerPlaneServeError::Serve { .. } => "worker_plane_serve_failed",
            WorkerPlaneServeError::Task { .. } => "worker_plane_task_failed",
            WorkerPlaneServeError::ShutdownTimeout { .. } => "worker_plane_shutdown_timeout",
        }
    }
}

/// Bound on the worker plane's graceful-shutdown join: the owned serve task
/// is joined within this window (a straggler is aborted), so shutdown is
/// never unbounded.
pub const WORKER_PLANE_SHUTDOWN_BOUND: Duration = Duration::from_secs(5);
/// Bound on reaping the serve task after the graceful window elapsed.
pub const WORKER_PLANE_ABORT_REAP_BOUND: Duration = Duration::from_secs(1);

/// The daemon-queryable liveness snapshot of the worker plane.
///
/// The owned serve task records ONE terminal snapshot when it ends; until
/// then the plane is [`WorkerPlaneStatus::Serving`]. An unexpected end is
/// ALWAYS [`WorkerPlaneStatus::Unavailable`] carrying the typed error code —
/// a dead remote-worker socket can never masquerade as a live one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerPlaneStatus {
    /// The listener task is alive: requests are accepted.
    Serving,
    /// The task ended before any graceful-shutdown request. The plane is
    /// UNAVAILABLE; `code` is the typed [`WorkerPlaneServeError::code`] (or
    /// `worker_plane_task_died` for a panic/abort) and `message` names the
    /// cause.
    Unavailable { code: &'static str, message: String },
    /// The task ended after (and because of) a graceful-shutdown request.
    Stopped,
}

impl WorkerPlaneStatus {
    /// `true` only while the listener task is alive and accepting requests.
    pub fn is_alive(&self) -> bool {
        matches!(self, WorkerPlaneStatus::Serving)
    }

    /// `true` when the plane died unexpectedly (never for a clean stop).
    pub fn is_unavailable(&self) -> bool {
        matches!(self, WorkerPlaneStatus::Unavailable { .. })
    }

    /// The stable machine code of an unavailable plane (the typed error's
    /// code); `None` while serving or after a clean stop.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            WorkerPlaneStatus::Unavailable { code, .. } => Some(code),
            WorkerPlaneStatus::Serving | WorkerPlaneStatus::Stopped => None,
        }
    }

    /// One bounded health/audit line for the daemon log.
    pub fn health_line(&self) -> String {
        match self {
            WorkerPlaneStatus::Serving => "worker plane: serving".to_string(),
            WorkerPlaneStatus::Stopped => "worker plane: stopped".to_string(),
            WorkerPlaneStatus::Unavailable { code, message } => {
                format!("worker plane: UNAVAILABLE [{code}] {message}")
            }
        }
    }
}

/// The shared owner state of one worker-plane serve task: the one-shot
/// graceful shutdown signal plus the terminal status recorded exactly once by
/// the task itself.
#[derive(Debug)]
struct WorkerPlaneShared {
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    shutdown_requested: AtomicBool,
    terminal: Mutex<Option<WorkerPlaneStatus>>,
}

impl WorkerPlaneShared {
    fn new(shutdown: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            shutdown: Mutex::new(Some(shutdown)),
            shutdown_requested: AtomicBool::new(false),
            terminal: Mutex::new(None),
        }
    }

    /// Request graceful shutdown. Idempotent: the first call wins and sends
    /// the one-shot signal; every later call is a no-op. (A send failure only
    /// means the task already ended — its result is surfaced by the join.)
    fn request_shutdown(&self) -> bool {
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

    fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst)
    }

    /// Record the single terminal status (first writer wins; the serve task
    /// records exactly once, before its `JoinHandle` completes).
    fn record(&self, status: WorkerPlaneStatus) {
        let mut slot = self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(status);
        }
    }

    fn terminal(&self) -> Option<WorkerPlaneStatus> {
        self.terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// The live handle of the worker-plane listener: it OWNS the serve task's
/// `JoinHandle<Result<(), WorkerPlaneServeError>>` (never dropped or
/// detached) plus the one-shot graceful-shutdown signal.
///
/// The task records its terminal disposition in the shared status BEFORE its
/// `JoinHandle` completes, so [`WorkerPlaneHandle::status`] always names an
/// unexpected death typed — the owning daemon can never mistake a dead
/// worker socket for a live one.
pub struct WorkerPlaneHandle {
    pub addr: SocketAddr,
    /// The validated exposure decision this listener was started with.
    pub exposure: WorkerPlaneExposure,
    task: Option<tokio::task::JoinHandle<Result<(), WorkerPlaneServeError>>>,
    shared: Arc<WorkerPlaneShared>,
}

impl std::fmt::Debug for WorkerPlaneHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPlaneHandle")
            .field("addr", &self.addr)
            .field("exposure", &self.exposure)
            .field("status", &self.status())
            .field("owns_task", &self.task.is_some())
            .finish()
    }
}

impl WorkerPlaneHandle {
    /// Request graceful shutdown of the listener. Idempotent: returns `true`
    /// exactly once (the call that initiated the shutdown) and `false` when
    /// shutdown was already requested. The owned task is joined (bounded) by
    /// [`WorkerPlaneHandle::shutdown`].
    pub fn request_shutdown(&self) -> bool {
        self.shared.request_shutdown()
    }

    /// `true` only while the owned serve task is alive and accepting
    /// requests.
    pub fn is_alive(&self) -> bool {
        self.status().is_alive()
    }

    /// The typed liveness snapshot the daemon queries for placement/health
    /// decisions (see [`WorkerPlaneStatus`]). A recorded terminal result wins;
    /// a task that ended without recording (panic/abort) is reported
    /// [`WorkerPlaneStatus::Unavailable`] with `worker_plane_task_died`, never
    /// silently as stopped.
    pub fn status(&self) -> WorkerPlaneStatus {
        if let Some(status) = self.shared.terminal() {
            return status;
        }
        let finished = match &self.task {
            Some(task) => task.is_finished(),
            None => true,
        };
        if !finished {
            return WorkerPlaneStatus::Serving;
        }
        if self.shared.shutdown_requested() {
            WorkerPlaneStatus::Stopped
        } else {
            WorkerPlaneStatus::Unavailable {
                code: "worker_plane_task_died",
                message: format!(
                    "worker plane serve task for {} ended without a typed result (panic or abort)",
                    self.addr
                ),
            }
        }
    }

    /// Signal graceful shutdown and JOIN the owned serve task.
    ///
    /// - Bounded: the join waits at most [`WORKER_PLANE_SHUTDOWN_BOUND`]; a
    ///   straggler is aborted and reaped within [`WORKER_PLANE_ABORT_REAP_BOUND`].
    /// - Idempotent: requesting shutdown twice is a no-op (the signal is
    ///   one-shot); a handle whose task already ended joins immediately.
    /// - A clean graceful stop maps to `Ok(())`; a bind/serve error (including
    ///   an unexpected death that happened BEFORE the request), a panicked/
    ///   aborted task, or an elapsed join bound is surfaced as the typed
    ///   [`WorkerPlaneServeError`] — never `.ok()`-discarded.
    pub async fn shutdown(mut self) -> Result<(), WorkerPlaneServeError> {
        let bind = self.addr;
        self.request_shutdown();
        let mut task = self
            .task
            .take()
            .expect("the worker-plane task is owned until shutdown consumes the handle");
        match tokio::time::timeout(WORKER_PLANE_SHUTDOWN_BOUND, &mut task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(join_error)) => Err(WorkerPlaneServeError::Task {
                bind,
                detail: join_error.to_string(),
            }),
            Err(_elapsed) => {
                task.abort();
                let _ = tokio::time::timeout(WORKER_PLANE_ABORT_REAP_BOUND, &mut task).await;
                Err(WorkerPlaneServeError::ShutdownTimeout {
                    bind,
                    bound: WORKER_PLANE_SHUTDOWN_BOUND,
                })
            }
        }
    }
}

impl Drop for WorkerPlaneHandle {
    fn drop(&mut self) {
        // Terminal health is read BEFORE the shutdown request so an
        // unexpected death is never masked as a requested stop.
        let status = self.status();
        let Some(task) = self.task.take() else {
            return;
        };
        if status.is_unavailable() {
            tracing::error!("{}", status.health_line());
        }
        // Synchronous fallback so the listener task can never outlive its
        // owner: request graceful shutdown, then abort. Callers that need the
        // graceful drain call `shutdown().await`.
        self.request_shutdown();
        if !task.is_finished() {
            task.abort();
        }
    }
}

/// Spawn the OWNED serve task: it maps the serve future's `io::Error` into
/// the typed [`WorkerPlaneServeError::Serve`], records the terminal
/// disposition in the handle's shared status, and returns the typed result to
/// its owner (`WorkerPlaneHandle::shutdown`). The production serve future is
/// never `.ok()`-discarded.
fn spawn_worker_plane_task<F>(
    bind: SocketAddr,
    shared: Arc<WorkerPlaneShared>,
    serve: F,
) -> tokio::task::JoinHandle<Result<(), WorkerPlaneServeError>>
where
    F: std::future::Future<Output = std::io::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let result = serve
            .await
            .map_err(|source| WorkerPlaneServeError::Serve { bind, source });
        let status = match &result {
            Ok(()) if shared.shutdown_requested() => WorkerPlaneStatus::Stopped,
            Ok(()) => WorkerPlaneStatus::Unavailable {
                code: "worker_plane_serve_ended",
                message: format!(
                    "worker plane serve for {bind} completed without a shutdown request"
                ),
            },
            Err(error) => WorkerPlaneStatus::Unavailable {
                code: error.code(),
                message: error.to_string(),
            },
        };
        shared.record(status);
        result
    })
}

/// Bind and serve the dedicated worker-plane listener over the SAME
/// [`ServerDeps`] authority the native listener uses (ONE worker plane, two
/// listeners). The boundary config is validated BEFORE the socket is bound;
/// a refusal means no worker socket exists at all.
pub async fn serve_worker_plane(
    deps: Arc<ServerDeps>,
    config: WorkerPlaneBindConfig,
) -> Result<WorkerPlaneHandle, WorkerPlaneServeError> {
    let exposure = config.validate()?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|source| WorkerPlaneServeError::Bind {
            bind: config.bind,
            source,
        })?;
    let addr = listener
        .local_addr()
        .map_err(|source| WorkerPlaneServeError::Bind {
            bind: config.bind,
            source,
        })?;
    let app = worker_plane_router(deps, config.bearer.clone());
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shared = Arc::new(WorkerPlaneShared::new(shutdown_tx));
    // The handle OWNS this task: its result is recorded in `shared` and
    // returned to `shutdown()`; nothing is discarded here.
    let task = spawn_worker_plane_task(addr, Arc::clone(&shared), async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });
    Ok(WorkerPlaneHandle {
        addr,
        exposure,
        task: Some(task),
        shared,
    })
}

/// Test seams for the ownership/health contract. These exercise the SAME
/// recording wrapper the production task uses (`spawn_worker_plane_task`),
/// never a parallel code path.
#[cfg(test)]
impl WorkerPlaneHandle {
    /// A handle whose serve task fails immediately with `source`: unexpected
    /// termination must be recorded typed and surfaced by `shutdown`.
    pub(crate) fn failing_serve_for_test(
        addr: SocketAddr,
        exposure: WorkerPlaneExposure,
        source: std::io::Error,
    ) -> WorkerPlaneHandle {
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let shared = Arc::new(WorkerPlaneShared::new(shutdown_tx));
        let task = spawn_worker_plane_task(addr, Arc::clone(&shared), async move { Err(source) });
        WorkerPlaneHandle {
            addr,
            exposure,
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
    pub(crate) fn probe_for_test(&self) -> WorkerPlaneStatusProbe {
        WorkerPlaneStatusProbe(Arc::clone(&self.shared))
    }
}

/// Test-only detached view of a handle's shared owner state.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct WorkerPlaneStatusProbe(Arc<WorkerPlaneShared>);

#[cfg(test)]
impl WorkerPlaneStatusProbe {
    pub(crate) fn terminal(&self) -> Option<WorkerPlaneStatus> {
        self.0.terminal()
    }
}

/// The dedicated worker-plane router: the four worker-token routes ONLY.
/// Every native route (health/ready, sessions, operator worker routes, SSO,
/// …) is absent here and answers 404 — the socket's whole purpose is the
/// remote worker contract, nothing else.
fn worker_plane_router(deps: Arc<ServerDeps>, bearer: Option<String>) -> Router {
    let next_terminal_event_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let state = AppState {
        deps,
        // The daemon-password override (`auth.set`) is a native-listener
        // concern and deliberately NOT mounted here: the worker plane has
        // its own identity and never accepts the daemon password.
        auth: Arc::new(std::sync::RwLock::new(None)),
        terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        next_terminal_event_id,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let router = Router::new()
        .route("/native/workers/register", post(native_worker_register))
        .route(
            "/native/workers/{id}/heartbeat",
            post(native_worker_heartbeat),
        )
        .route("/native/jobs/claim", post(native_job_claim))
        .route("/native/jobs/{id}/result", post(native_job_result))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .with_state(state);
    match bearer {
        None => router,
        Some(expected) => {
            // The gateway transport bearer: constant-time extraction is
            // bounded (`check_bearer_value` refuses oversized headers before
            // comparing) and the expected value never enters a log line.
            router.layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: Next| {
                    let expected = expected.clone();
                    async move {
                        let presented = request
                            .headers()
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok());
                        if check_bearer_value(&expected, presented) {
                            next.run(request).await
                        } else {
                            wire_status(unauthorized())
                        }
                    }
                },
            ))
        }
    }
}

fn unauthorized() -> ApiError {
    ApiError {
        code: "unauthorized",
        message: "missing or invalid worker-plane transport bearer".into(),
        http_status: 401,
        retryable: false,
    }
}
