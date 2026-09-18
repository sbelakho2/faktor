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

use std::net::SocketAddr;
use std::sync::Arc;

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

/// A worker-plane startup failure: the boundary refusal or the socket bind.
#[derive(Debug, thiserror::Error)]
pub enum WorkerPlaneServeError {
    #[error("{0}")]
    Boundary(#[from] WorkerPlaneBoundaryRefusal),
    #[error("worker plane bind {bind}: {source}")]
    Bind {
        bind: SocketAddr,
        source: std::io::Error,
    },
}

/// The live handle of the worker-plane listener.
#[derive(Debug)]
pub struct WorkerPlaneHandle {
    pub addr: SocketAddr,
    /// Dropping (or sending on) this ends the worker-plane listener.
    pub shutdown: tokio::sync::oneshot::Sender<()>,
    /// The validated exposure decision this listener was started with.
    pub exposure: WorkerPlaneExposure,
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
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });
    Ok(WorkerPlaneHandle {
        addr,
        shutdown: shutdown_tx,
        exposure,
    })
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
