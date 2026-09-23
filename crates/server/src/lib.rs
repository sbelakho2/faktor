//! faktor-server — the HTTP/SSE surface of the daemon, speaking the
//! Faktor-native protocol. The UI connection is disposable: turns run
//! detached from any connection and resume from the journal.
//!
//! Auth: the frontend generates `FAKTOR_SERVER_PASSWORD` and passes it via env;
//! every endpoint requires it as `Authorization: Bearer <password>` or
//! `x-faktor-server-password: <password>`, plus the legacy per-start token as
//! a Bearer. The pre-cutover Basic compatibility arm is gone — see
//! [`auth`]'s migration note.

pub mod api;
pub mod auth;
pub mod native;
pub mod permission;
/// The worker-plane deployment boundary: the dedicated second listener and
/// its own bind/transport/auth identity (see the module docs for the
/// TLS-or-gateway-only rules).
pub mod worker_plane;

pub use api::{
    drain_chunk_stream, empty_evidence_store, serve, serve_arc, EvidenceStoreHandle, ServerDeps,
    ServerDepsError, ServerHandle, ServerServeError, ServerShutdownError, ServerStatus,
};
pub use auth::{check_bearer, check_password, AuthConfigError, AuthToken, ServerPassword};
pub use permission::{ChannelPermissionRequester, PendingPermission};
pub use worker_plane::{
    serve_worker_plane, WorkerPlaneAuth, WorkerPlaneBindConfig, WorkerPlaneBoundaryRefusal,
    WorkerPlaneExposure, WorkerPlaneHandle, WorkerPlaneServeError, WorkerPlaneTransport,
    DEFAULT_WORKER_PLANE_BIND, MAX_WORKER_PLANE_BEARER_BYTES,
};

#[cfg(test)]
#[path = "worker_plane_tests.rs"]
mod worker_plane_tests;
