//! `api::router`: cohesive slice of the api module.

use super::*;

/// The daemon's bounded request-body limit (`pub(crate)`: the dedicated
/// worker-plane listener applies the same bound).
pub(crate) const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// One daemon-owned slot for the dedicated worker-plane listener handle (the
/// second listener of [`crate::worker_plane`]). The native health handler
/// reads the typed [`WorkerPlaneStatus`] through it; the daemon shutdown
/// sequence takes the owned handle out of it for the bounded join, so the
/// health payload names the same socket before, during and after teardown.
pub type WorkerPlaneListener = Arc<std::sync::Mutex<WorkerPlaneListenerSlot>>;

/// The contents of a [`WorkerPlaneListener`] slot.
#[derive(Default)]
pub struct WorkerPlaneListenerSlot {
    pub(crate) handle: Option<WorkerPlaneHandle>,
    /// The bound address, remembered after the handle is consumed.
    pub(crate) bind: Option<SocketAddr>,
    /// The terminal snapshot recorded when the shutdown sequence consumed
    /// the handle; `None` until then.
    pub(crate) terminal: Option<WorkerPlaneStatus>,
}

impl WorkerPlaneListenerSlot {
    /// Install the freshly bound listener handle (daemon startup; called
    /// BEFORE the native listener starts, so a serving health route always
    /// observes the installed handle).
    pub fn install(&mut self, handle: WorkerPlaneHandle) {
        self.bind = Some(handle.addr);
        self.handle = Some(handle);
    }

    /// The typed status: live from the owned handle, or the terminal
    /// snapshot recorded by the shutdown join. `None` = enabled but never
    /// installed (or consumed without a recorded terminal).
    pub fn status(&self) -> Option<WorkerPlaneStatus> {
        if let Some(handle) = &self.handle {
            return Some(handle.status());
        }
        self.terminal.clone()
    }

    /// The bound address, known even after the handle was consumed.
    pub fn bind(&self) -> Option<SocketAddr> {
        self.handle.as_ref().map(|handle| handle.addr).or(self.bind)
    }

    /// Take the owned handle for the bounded shutdown join. The
    /// pre-shutdown status is captured FIRST (an unexpected death must be
    /// named, never masked by the stop) and recorded as the provisional
    /// terminal state; the caller may refine it with
    /// [`WorkerPlaneListenerSlot::record_terminal`] after the join.
    pub fn take_for_shutdown(&mut self) -> Option<(WorkerPlaneHandle, WorkerPlaneStatus)> {
        let handle = self.handle.take()?;
        let health = handle.status();
        self.terminal = Some(if health.is_unavailable() {
            health.clone()
        } else {
            WorkerPlaneStatus::Stopped
        });
        Some((handle, health))
    }

    /// Record the typed terminal status produced by the bounded join.
    pub fn record_terminal(&mut self, status: WorkerPlaneStatus) {
        self.terminal = Some(status);
    }
}

/// Query of [`native_index_coverage`]: exactly one session whose workspace
/// coverage is reported. Strict (`deny_unknown_fields`): a typo is a 400.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeIndexCoverageQuery {
    pub session: String,
}

/// `GET /native/index/coverage?session=<id>` — additive index coverage
/// diagnostics (audits 5/6/16). Reports the durable [`faktor_index::IndexCoverage`]
/// (files seen/indexed, bytes, `complete`, `truncated_reason`), the
/// fingerprint shard coverage and the typed freshness of the session's
/// workspace generation, so the IDE panels can display a PARTIAL generation
/// honestly. Pure read: never opens the index service and never starts a
/// build; `index_coverage: null` when the service was never hosted for this
/// daemon (never a fabricated "complete").
pub(crate) async fn native_index_coverage(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
    query: Result<
        axum::extract::Query<NativeIndexCoverageQuery>,
        axum::extract::rejection::QueryRejection,
    >,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Err(e) = authed(&headers, &state) {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(e.to_json()),
        )
            .into_response();
    }
    let axum::extract::Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let handle = match native_resolve_session(&state, &query.session) {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    let snapshot = handle
        .row()
        .ok()
        .and_then(|row| state.deps.agent.index_coverage_snapshot(row.workspace_id));
    axum::Json(serde_json::json!({
        "sessionId": query.session,
        "index_coverage": snapshot,
    }))
    .into_response()
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

#[cfg(test)]
#[path = "router_projection_tests.rs"]
mod router_projection_tests;

#[cfg(test)]
#[path = "router_task_tests.rs"]
mod router_task_tests;

#[cfg(test)]
#[path = "router_misc_tests.rs"]
mod router_misc_tests;
