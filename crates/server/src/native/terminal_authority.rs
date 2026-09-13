//! Daemon session-owned terminal authority (additive).
//!
//! The native HTTP terminal surface owns its rows in `AppState`; this module
//! packages the SAME authority — `faktor-pty` children plus durable
//! session/task/operation ownership — as a self-contained public type so a
//! host WITHOUT the HTTP surface (`faktor-cli acp`) can attach it to the ACP
//! server's `faktor.terminal` seam (`AcpServer::with_terminal_authority`).
//!
//! No second PTY implementation exists here: every terminal is spawned
//! through `faktor-pty` with the one env-clear [`EnvSpec`] authority (safe
//! baseline names plus the caller's name allowlist, the secret deny-set
//! always applied). Rows are session-owned exactly like the native ones: the
//! session must exist in the durable store and the row carries the session's
//! durable task identity plus a durable operation id. Live rows are bounded;
//! dead rows are swept lazily on the next create.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use faktor_core::id::{OpId, SessionId, TaskId};
use faktor_pty::{EnvSpec, Pty, PtyConfig};
use faktor_session::SessionManager;

/// Bound of one terminal command / arg / cwd (bytes; mirrors the ACP param
/// bounds and the native terminal spawn body).
pub const MAX_TERMINAL_FIELD_BYTES: usize = 4096;

/// Bound of one terminal's env name allowlist.
pub const MAX_TERMINAL_ENV_NAMES: usize = 64;

/// Bound of one env name (bytes).
pub const MAX_TERMINAL_ENV_NAME_BYTES: usize = 256;

/// Bound on live session-owned terminal rows per daemon process (bounded
/// everything): a spawn past the ceiling is refused typed, never queued.
pub const MAX_DAEMON_TERMINALS: usize = 256;

/// Why one registry operation failed. `Invalid` is the caller's fault
/// (malformed/unknown session, oversize field); `Refused` is a spawn or I/O
/// refusal; `Unavailable` is a bounded-resource refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalRegistryError {
    /// The request is invalid (bad or unknown session, hostile field).
    Invalid(String),
    /// The spawn or terminal operation was refused.
    Refused(String),
    /// The registry has no capacity, or the terminal is gone.
    Unavailable(String),
}

impl fmt::Display for TerminalRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalRegistryError::Invalid(message) => {
                write!(f, "invalid terminal request: {message}")
            }
            TerminalRegistryError::Refused(message) => write!(f, "terminal refused: {message}"),
            TerminalRegistryError::Unavailable(message) => {
                write!(f, "terminal unavailable: {message}")
            }
        }
    }
}

impl std::error::Error for TerminalRegistryError {}

/// One spawn request (the ACP `terminal/create` spec mapped onto the daemon
/// authority). `env` is an allowlist of NAMES, never values.
#[derive(Debug, Clone)]
pub struct TerminalSpawnRequest {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<String>,
    pub rows: u16,
    pub cols: u16,
}

/// Durable ownership of one session-owned terminal row: the session that
/// owns the terminal, its durable task identity, the orchestrator agent that
/// spawned it (when one did), the durable operation id, the child pid, and
/// the spawn time. Mirrors the native P0-62 ownership row.
#[derive(Debug, Clone)]
pub struct TerminalOwnership {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub agent_id: Option<String>,
    pub operation_id: OpId,
    pub pid: u32,
    pub spawned_ms: i64,
}

struct TerminalRow {
    pty: Arc<Mutex<Pty>>,
    ownership: TerminalOwnership,
}

/// The session-owned terminal rows of one daemon process.
pub struct TerminalRegistry {
    session: Arc<SessionManager>,
    next_id: AtomicU64,
    rows: Mutex<HashMap<u64, TerminalRow>>,
}

impl TerminalRegistry {
    /// Build a registry over the daemon's ONE session manager (the ACP host
    /// passes the same `Arc` its backend drives, so session ids always refer
    /// to the real durable sessions).
    pub fn new(session: Arc<SessionManager>) -> Arc<Self> {
        Arc::new(Self {
            session,
            next_id: AtomicU64::new(1),
            rows: Mutex::new(HashMap::new()),
        })
    }

    /// Live-row count (diagnostic/test probe).
    pub fn live_rows(&self) -> usize {
        let rows = self.lock_rows();
        rows.values()
            .filter(|row| row.pty.lock().map(|p| p.is_alive()).unwrap_or(true))
            .count()
    }

    /// The ownership rows of one session, sorted by terminal id. Rows of
    /// another session are never included (the session-scoped projection
    /// invariant of the native P0-62 view).
    pub fn session_rows(&self, session_id: SessionId) -> Vec<(String, TerminalOwnership)> {
        let rows = self.lock_rows();
        let mut out: Vec<(String, TerminalOwnership)> = rows
            .iter()
            .filter(|(_, row)| row.ownership.session_id == session_id)
            .map(|(id, row)| (id.to_string(), row.ownership.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Spawn one session-owned terminal. The session must exist in the
    /// durable store; the row records its durable task identity and a fresh
    /// durable operation id. Hostile fields are refused before any OS call;
    /// dead rows are swept and the live ceiling is enforced before spawn.
    pub fn create(
        &self,
        session_id: &str,
        request: &TerminalSpawnRequest,
    ) -> Result<TerminalHandle, TerminalRegistryError> {
        let raw: u64 = session_id.parse().map_err(|_| {
            TerminalRegistryError::Invalid(format!("invalid session id {session_id:?}"))
        })?;
        if raw == 0 {
            return Err(TerminalRegistryError::Invalid(
                "session id cannot be 0".into(),
            ));
        }
        let sid = SessionId::new(raw);
        let session_handle = self
            .session
            .get_session(sid)
            .map_err(|e| TerminalRegistryError::Refused(e.message))?
            .ok_or_else(|| {
                TerminalRegistryError::Invalid(format!("unknown session {session_id:?}"))
            })?;
        let row = session_handle
            .row()
            .map_err(|e| TerminalRegistryError::Refused(e.message))?;

        validate_spawn_request(request)?;

        // Sweep dead rows and enforce the ceiling BEFORE the spawn: a
        // refused create never touches a PTY.
        let mut rows = self.lock_rows();
        rows.retain(|_, row| row.pty.lock().map(|pty| pty.is_alive()).unwrap_or(false));
        if rows.len() >= MAX_DAEMON_TERMINALS {
            return Err(TerminalRegistryError::Unavailable(format!(
                "terminal capacity exhausted ({MAX_DAEMON_TERMINALS} live rows)"
            )));
        }

        let env = if request.env.is_empty() {
            EnvSpec::default_baseline()
        } else {
            // The name allowlist rides the ONE env authority: values never
            // cross, and the secret deny-set applies inside `resolve()`.
            EnvSpec::Allowlisted(request.env.clone())
        };
        let cfg = PtyConfig {
            command: request.command.clone(),
            args: request.args.clone(),
            cwd: request.cwd.clone(),
            env,
            rows: request.rows.max(1),
            cols: request.cols.max(1),
        };
        let pty = Pty::spawn(&cfg).map_err(|e| TerminalRegistryError::Refused(e.message))?;
        let pty_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let pid = pty.pid();
        let ownership = TerminalOwnership {
            session_id: sid,
            task_id: row.task_id,
            agent_id: None,
            operation_id: self.session.next_op_id(),
            pid,
            spawned_ms: self.session.now_ms(),
        };
        let handle = TerminalHandle {
            terminal_id: pty_id.to_string(),
            ownership_id: ownership.operation_id.to_string(),
            pid,
            session_id: sid,
            task_id: ownership.task_id,
            pty: Arc::new(Mutex::new(pty)),
        };
        rows.insert(
            pty_id,
            TerminalRow {
                pty: Arc::clone(&handle.pty),
                ownership,
            },
        );
        Ok(handle)
    }

    /// Poison-tolerant row lock (a panicked holder must not make the daemon
    /// terminal surface unusable).
    fn lock_rows(&self) -> std::sync::MutexGuard<'_, HashMap<u64, TerminalRow>> {
        self.rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One live daemon terminal. `kill` is idempotent and terminates the whole
/// tree through `faktor-pty` (ConPTY job close on Windows, session close +
/// process-group kill on unix); dropping the last handle kills it as the
/// final failsafe.
pub struct TerminalHandle {
    terminal_id: String,
    ownership_id: String,
    pid: u32,
    session_id: SessionId,
    task_id: TaskId,
    pty: Arc<Mutex<Pty>>,
}

impl fmt::Debug for TerminalHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerminalHandle")
            .field("terminal_id", &self.terminal_id)
            .field("pid", &self.pid)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl TerminalHandle {
    /// The authority-unique terminal id (the ownership row's key).
    pub fn terminal_id(&self) -> &str {
        &self.terminal_id
    }

    /// The child pid at spawn.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The owning session.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// The owning session's durable task identity.
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// The authority's ownership id for this row (the durable operation id).
    pub fn ownership_id(&self) -> &str {
        &self.ownership_id
    }

    /// Whether the child process tree is still alive.
    pub fn is_alive(&self) -> bool {
        self.lock_pty().is_alive()
    }

    /// Write raw bytes to the terminal.
    pub fn write(&self, bytes: &[u8]) -> Result<(), TerminalRegistryError> {
        self.lock_pty()
            .write_all(bytes)
            .map_err(|e| TerminalRegistryError::Refused(e.message))
    }

    /// Resize the terminal window.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), TerminalRegistryError> {
        if rows == 0 || cols == 0 {
            return Err(TerminalRegistryError::Invalid(
                "terminal size must be non-zero".into(),
            ));
        }
        self.lock_pty()
            .resize(rows, cols)
            .map_err(|e| TerminalRegistryError::Refused(e.message))
    }

    /// Drain all currently available output (bounded by the pty ring; never
    /// blocking).
    pub fn drain_output(&self) -> Vec<u8> {
        self.lock_pty().read_available()
    }

    /// Kill the whole process tree (idempotent).
    pub fn kill(&self) -> Result<(), TerminalRegistryError> {
        self.lock_pty().kill();
        Ok(())
    }

    fn lock_pty(&self) -> std::sync::MutexGuard<'_, Pty> {
        self.pty
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Defensive field validation (the ACP layer validates too; the authority
/// must not depend on a caller having done so).
fn validate_spawn_request(request: &TerminalSpawnRequest) -> Result<(), TerminalRegistryError> {
    if request.command.is_empty() || request.command.len() > MAX_TERMINAL_FIELD_BYTES {
        return Err(TerminalRegistryError::Invalid(
            "terminal command must be non-empty and at most 4096 bytes".into(),
        ));
    }
    if request.command.contains('\0') {
        return Err(TerminalRegistryError::Invalid(
            "terminal command contains a NUL byte".into(),
        ));
    }
    if request.args.len() > 256 {
        return Err(TerminalRegistryError::Invalid(
            "terminal args exceed the 256-entry bound".into(),
        ));
    }
    for arg in &request.args {
        if arg.len() > MAX_TERMINAL_FIELD_BYTES {
            return Err(TerminalRegistryError::Invalid(
                "terminal arg exceeds 4096 bytes".into(),
            ));
        }
        if arg.contains('\0') {
            return Err(TerminalRegistryError::Invalid(
                "terminal arg contains a NUL byte".into(),
            ));
        }
    }
    if let Some(cwd) = &request.cwd {
        if cwd.is_empty() || cwd.len() > MAX_TERMINAL_FIELD_BYTES || cwd.contains('\0') {
            return Err(TerminalRegistryError::Invalid(
                "terminal cwd is invalid or oversized".into(),
            ));
        }
    }
    if request.env.len() > MAX_TERMINAL_ENV_NAMES {
        return Err(TerminalRegistryError::Invalid(
            "terminal env allowlist exceeds 64 names".into(),
        ));
    }
    for name in &request.env {
        if name.len() > MAX_TERMINAL_ENV_NAME_BYTES || name.contains('\0') {
            return Err(TerminalRegistryError::Invalid(
                "terminal env name is invalid or oversized".into(),
            ));
        }
    }
    Ok(())
}
