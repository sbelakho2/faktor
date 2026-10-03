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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tower_http::limit::RequestBodyLimitLayer;

use faktor_agent::AgentRuntime;
use faktor_session::SessionManager;

use crate::auth::{AuthToken, ServerPassword};
use crate::native::terminal_authority::TerminalAuthorityPolicy;
use crate::permission::ChannelPermissionRequester;
use crate::worker_plane::{WorkerPlaneHandle, WorkerPlaneStatus};

pub(crate) use crate::native::*;

// ------------------------------------------------------------------ handlers

#[cfg(test)]
#[path = "control_plane_tests.rs"]
mod control_plane_tests;

#[cfg(test)]
#[path = "enterprise_tests.rs"]
mod enterprise_tests;

#[cfg(test)]
#[path = "updater_tests.rs"]
mod updater_tests;

#[cfg(test)]
#[path = "adversarial_auth_tests.rs"]
mod adversarial_auth_tests;

#[cfg(test)]
#[path = "adversarial_dto_tests.rs"]
mod adversarial_dto_tests;

#[cfg(test)]
#[path = "adversarial_sse_bounds_tests.rs"]
mod adversarial_sse_bounds_tests;

#[cfg(test)]
#[path = "adversarial_control_plane_tests.rs"]
mod adversarial_control_plane_tests;

mod deps;
pub use deps::*;
mod lifecycle;
pub use lifecycle::*;
mod router;
pub use router::*;
mod health;
pub use health::*;

#[cfg(test)]
pub(crate) use deps::tests;
