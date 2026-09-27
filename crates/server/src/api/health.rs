//! `api::health`: cohesive slice of the api module.

use super::*;

/// Bound of the worker-plane health message surfaced by `/native/health`: a
/// hostile IO/join error string must never balloon the health payload.
pub const MAX_WORKER_PLANE_HEALTH_MESSAGE_BYTES: usize = 512;

/// Truncate a typed status message on a UTF-8 char boundary. The message is
/// derived from the typed [`crate::worker_plane::WorkerPlaneServeError`] only
/// (bind + IO/join cause) — the configured transport bearer is never part of
/// it, so no secret can reach the health payload through this path.
pub(crate) fn bounded_worker_plane_health_message(message: &str) -> String {
    if message.len() <= MAX_WORKER_PLANE_HEALTH_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut end = MAX_WORKER_PLANE_HEALTH_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

/// The additive health view of the dedicated worker-plane listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerPlaneHealth {
    /// `[worker_plane]` is disabled (the default): no second socket exists.
    Disabled,
    /// The dedicated listener is wired. `status` is `None` only in the
    /// startup window before the handle is installed and with no recorded
    /// terminal state (the native listener starts after installation, so a
    /// serving health route cannot observe that window in production).
    Enabled {
        bind: Option<SocketAddr>,
        status: Option<WorkerPlaneStatus>,
    },
}

impl WorkerPlaneHealth {
    /// The stable machine spelling of the worker-plane state.
    pub fn state(&self) -> &'static str {
        match self {
            WorkerPlaneHealth::Disabled => "disabled",
            WorkerPlaneHealth::Enabled { status, .. } => match status {
                Some(WorkerPlaneStatus::Serving) => "serving",
                Some(WorkerPlaneStatus::Unavailable { .. }) => "unavailable",
                Some(WorkerPlaneStatus::Stopped) | None => "stopped",
            },
        }
    }

    /// The additive, secret-free JSON object for `/native/health`: `state`
    /// always; `bind` when known; `code` + `message` only for an unavailable
    /// plane (the typed error's stable code and bounded message).
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::Map::new();
        value.insert("state".into(), self.state().into());
        if let WorkerPlaneHealth::Enabled { bind, status } = self {
            if let Some(bind) = bind {
                value.insert("bind".into(), bind.to_string().into());
            }
            if let Some(WorkerPlaneStatus::Unavailable { code, message }) = status {
                value.insert("code".into(), (*code).into());
                value.insert(
                    "message".into(),
                    bounded_worker_plane_health_message(message).into(),
                );
            }
        }
        serde_json::Value::Object(value)
    }
}

/// The daemon-queryable liveness snapshot of the native listener.
///
/// The owned serve task records ONE terminal snapshot when it ends; until
/// then the listener is [`ServerStatus::Serving`]. An unexpected end is
/// ALWAYS [`ServerStatus::Unavailable`] carrying the typed error code — a
/// dead native socket can never masquerade as a live one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    /// The listener task is alive: requests are accepted.
    Serving,
    /// The task ended before any graceful-shutdown request. The listener is
    /// UNAVAILABLE; `code` is the typed [`ServerServeError::code`] (or
    /// `server_task_died` for a panic/abort) and `message` names the cause.
    Unavailable { code: &'static str, message: String },
    /// The task ended after (and because of) a graceful-shutdown request.
    Stopped,
}

impl ServerStatus {
    /// `true` only while the listener task is alive and accepting requests.
    pub fn is_alive(&self) -> bool {
        matches!(self, ServerStatus::Serving)
    }

    /// `true` when the listener died unexpectedly (never for a clean stop).
    pub fn is_unavailable(&self) -> bool {
        matches!(self, ServerStatus::Unavailable { .. })
    }

    /// The stable machine code of an unavailable listener (the typed error's
    /// code); `None` while serving or after a clean stop.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            ServerStatus::Unavailable { code, .. } => Some(code),
            ServerStatus::Serving | ServerStatus::Stopped => None,
        }
    }

    /// One bounded health/audit line for the daemon log.
    pub fn health_line(&self) -> String {
        match self {
            ServerStatus::Serving => "native server: serving".to_string(),
            ServerStatus::Stopped => "native server: stopped".to_string(),
            ServerStatus::Unavailable { code, message } => {
                format!("native server: UNAVAILABLE [{code}] {message}")
            }
        }
    }
}

/// Test-only detached view of a handle's shared owner state.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ServerStatusProbe(pub(crate) Arc<ServerShared>);

#[cfg(test)]
impl ServerStatusProbe {
    pub(crate) fn terminal(&self) -> Option<ServerStatus> {
        self.0.terminal()
    }

    /// The test-visible live-drop signal: `true` once the handle was dropped
    /// while its serve task was still live (the loud diagnostic fired). An
    /// explicitly awaited [`ServerHandle::shutdown`] never sets it.
    pub(crate) fn dropped_live(&self) -> bool {
        self.0.dropped_live()
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod health_tests;
