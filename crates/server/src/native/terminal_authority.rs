//! Daemon session-owned terminal service — the ONE terminal authority.
//!
//! Every session-owned terminal of this daemon is governed here, and the
//! authority is the DURABLE LEDGER: each lifecycle transition is a typed
//! `terminal_*` row in the owning session's ledger (created → running →
//! exited/killed; lost/reconciled on restart). ACP
//! (`AcpServer::with_terminal_authority`) and the native HTTP surface are
//! thin adapters over this one service: they never keep a second authority.
//!
//! # Authority model
//!
//! - `TerminalService::for_manager` returns the process-wide service of one
//!   `SessionManager` (the daemon's ONE session manager), so the ACP host
//!   and the native surface share the SAME live rows and the SAME durable
//!   authority. The in-memory live map only caches the `faktor-pty` handles
//!   of THIS daemon boot — a pty handle cannot survive a restart — and is
//!   strictly rebuilt from / reconciled against the durable rows.
//! - Ownership rows carry `session_id`, `task_id`, `agent_id`,
//!   `operation_id` and the terminal UUID; every listing is scope-enforced
//!   to the requesting session. A terminal id from another session is
//!   reported exactly like an unknown one (no existence leak).
//! - Process identity is `(pid, OS start time)`: captured at spawn through
//!   an injectable probe. Recovery NEVER adopts a row by pid alone — a row
//!   whose recorded identity is unknown, gone, or different from the
//!   observed one is marked `lost` and its pid is never signalled.
//! - Kill paths route through the pty authority only (guardian-compatible:
//!   `faktor-pty` owns the process tree) and journal `terminal_killed`
//!   exactly once, under a per-row state machine, so a create/kill race
//!   yields exactly one terminal state sequence.
//! - No second PTY implementation exists here: every terminal is spawned
//!   through `faktor-pty` with the one env-clear [`EnvSpec`] authority.
//!   The legacy daemon-level `/pty/*` surface (compat) keeps its own raw
//!   PTY map in `AppState`; session-owned rows never enter it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use faktor_core::id::{OpId, SessionId, TaskId};
use faktor_pty::{EnvSpec, Pty, PtyConfig};
use faktor_session::{
    SessionHandle, SessionManager, TerminalDurableRow, TerminalEventKind, TerminalLedgerRecord,
    TERMINAL_RECONCILE_COLLECTED, TERMINAL_RECONCILE_KILLED,
};

/// Bound of one terminal command / arg / cwd (bytes; mirrors the ACP param
/// bounds and the native terminal spawn body).
pub const MAX_TERMINAL_FIELD_BYTES: usize = 4096;

/// Bound of one terminal's env name allowlist.
pub const MAX_TERMINAL_ENV_NAMES: usize = 64;

/// Bound of one env name (bytes).
pub const MAX_TERMINAL_ENV_NAME_BYTES: usize = 256;

/// Bound of one terminal input payload (bytes).
pub const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;

/// Bound of one terminal args list.
pub const MAX_TERMINAL_ARGS: usize = 256;

/// Bound on live session-owned terminal rows per daemon process (bounded
/// everything): a spawn past the ceiling is refused typed, never queued.
pub const MAX_DAEMON_TERMINALS: usize = 256;

/// Why one legacy registry operation failed. `Invalid` is the caller's
/// fault (malformed/unknown session, oversize field); `Refused` is a spawn
/// or I/O refusal; `Unavailable` is a bounded-resource refusal.
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

/// The typed failure taxonomy of the durable terminal service. `Lost` and
/// `IdentityMismatch` are the recovery refusals: an unreachable terminal
/// never accepts I/O and a recycled pid is never adopted, signalled or
/// reconciled under the wrong identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalServiceError {
    /// The request is invalid (bad ids/fields, unknown session).
    Invalid(String),
    /// The terminal id is unknown to the requesting session. A foreign row
    /// is reported EXACTLY like this: existence is never leaked.
    Unknown {
        session_id: String,
        terminal_id: String,
    },
    /// The terminal row exists under another session/task/agent scope; the
    /// denial never reveals whether the foreign row exists.
    ForeignScope {
        session_id: String,
        terminal_id: String,
    },
    /// The terminal exists but is not in a state that accepts the operation.
    State {
        terminal_id: String,
        state: &'static str,
        message: String,
    },
    /// The row has no live pty authority in this daemon boot (restart): all
    /// I/O is refused; the row can only be reconciled.
    Lost { terminal_id: String, detail: String },
    /// The observed process identity does not match the durable row's: the
    /// pid was recycled or belongs to an unrelated process.
    IdentityMismatch {
        terminal_id: String,
        expected_pid: u32,
        expected_start_time_ms: i64,
        observed_start_time_ms: i64,
    },
    /// The spawn/pty operation was refused.
    Refused(String),
    /// No capacity, or the resource is gone.
    Unavailable(String),
}

impl fmt::Display for TerminalServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalServiceError::Invalid(message) => {
                write!(f, "invalid terminal request: {message}")
            }
            TerminalServiceError::Unknown {
                session_id,
                terminal_id,
            } => write!(
                f,
                "unknown terminal {terminal_id:?} for session {session_id:?}"
            ),
            TerminalServiceError::ForeignScope {
                session_id,
                terminal_id,
            } => write!(
                f,
                "terminal {terminal_id:?} is not owned by session {session_id:?}"
            ),
            TerminalServiceError::State {
                terminal_id,
                state,
                message,
            } => write!(f, "terminal {terminal_id:?} is {state}: {message}"),
            TerminalServiceError::Lost {
                terminal_id,
                detail,
            } => write!(f, "terminal {terminal_id:?} is lost: {detail}"),
            TerminalServiceError::IdentityMismatch {
                terminal_id,
                expected_pid,
                expected_start_time_ms,
                observed_start_time_ms,
            } => write!(
                f,
                "terminal {terminal_id:?} process identity mismatch: pid {expected_pid} recorded \
                 start {expected_start_time_ms}ms, observed {observed_start_time_ms}ms (recycled \
                 pid is never adopted)"
            ),
            TerminalServiceError::Refused(message) => write!(f, "terminal refused: {message}"),
            TerminalServiceError::Unavailable(message) => {
                write!(f, "terminal unavailable: {message}")
            }
        }
    }
}

impl std::error::Error for TerminalServiceError {}

impl From<TerminalServiceError> for TerminalRegistryError {
    fn from(error: TerminalServiceError) -> Self {
        match error {
            TerminalServiceError::Invalid(message) => TerminalRegistryError::Invalid(message),
            TerminalServiceError::Unknown { terminal_id, .. } => {
                TerminalRegistryError::Invalid(format!("unknown terminal {terminal_id:?}"))
            }
            TerminalServiceError::Unavailable(message) => {
                TerminalRegistryError::Unavailable(message)
            }
            other => TerminalRegistryError::Refused(other.to_string()),
        }
    }
}

impl From<faktor_core::Error> for TerminalServiceError {
    fn from(error: faktor_core::Error) -> Self {
        TerminalServiceError::Refused(error.message)
    }
}

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

/// The OS-level identity of one terminal's child process: the pid plus the
/// process start-time marker (see [`TerminalDurableRow::start_time_ms`]).
/// `start_time_ms == 0` means the identity is unverified — such a row is
/// never adopted by pid alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time_ms: i64,
}

/// The injectable OS process-start probe: `Some(ms)` when the pid's start
/// time is observable, `None` when the process is gone or unobservable.
/// Tests inject deterministic probes; the production default reads `ps`.
pub type IdentityProbe = Arc<dyn Fn(u32) -> Option<i64> + Send + Sync>;

/// Which terminal row field state a listing projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalView {
    pub terminal_id: String,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub agent_id: Option<String>,
    pub operation_id: OpId,
    pub pid: u32,
    pub start_time_ms: i64,
    /// When the row first became durable (`terminal_created` at_ms).
    pub spawned_ms: i64,
    /// The newest durable row's at_ms.
    pub updated_ms: i64,
    pub state: TerminalEventKind,
    /// Whether a live pty handle of THIS daemon boot owns the row.
    pub alive: bool,
    pub exit_code: Option<i32>,
    /// The newest row's bounded audit detail.
    pub detail: String,
    /// The numeric daemon-level pty id of a live row (legacy cache key).
    pub pty_id: Option<u64>,
}

impl TerminalView {
    /// The snake_case lifecycle state tag.
    pub fn state_tag(&self) -> &'static str {
        self.state.state_tag()
    }

    /// Whether the row is in a non-terminal state.
    pub fn is_open(&self) -> bool {
        !self.state.is_terminal()
    }
}

/// The typed disposition of one stale-row reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReconcileDisposition {
    /// Closed as killed (the recorded process was proven gone/recycled; no
    /// signal was ever sent under an unverified identity).
    Killed,
    /// Retired as collected (the unreachable row is finished).
    Collected,
}

impl TerminalReconcileDisposition {
    pub fn as_tag(self) -> &'static str {
        match self {
            TerminalReconcileDisposition::Killed => TERMINAL_RECONCILE_KILLED,
            TerminalReconcileDisposition::Collected => TERMINAL_RECONCILE_COLLECTED,
        }
    }
}

/// Outcome of one terminal spawn: the live handle plus its durable view.
pub struct TerminalCreation {
    pub handle: TerminalHandle,
    pub view: TerminalView,
}

/// What one recovery scan did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalRecoveryReport {
    /// Sessions scanned.
    pub sessions_scanned: usize,
    /// Non-terminal durable rows examined.
    pub terminals_scanned: usize,
    /// Terminal ids marked Lost by this scan (never pid-signalled).
    pub lost: Vec<String>,
    /// Rows already in a terminal state.
    pub already_terminal: usize,
    /// Rows with a live pty authority of this boot.
    pub live: usize,
}

/// One recovered durable terminal (the fold of its row stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableTerminal {
    pub row: TerminalDurableRow,
    pub state: TerminalEventKind,
    pub detail: String,
    pub exit_code: Option<i32>,
    pub seq: i64,
    pub spawned_ms: i64,
    pub updated_ms: i64,
}

impl DurableTerminal {
    /// The listing projection of one durable row.
    fn view(&self, live: Option<&LiveRow>) -> TerminalView {
        TerminalView {
            terminal_id: self.row.terminal_id.clone(),
            session_id: SessionId::new(self.row.session_id),
            task_id: TaskId::new(self.row.task_id),
            agent_id: self.row.agent_id.clone(),
            operation_id: OpId::new(self.row.operation_id),
            pid: self.row.pid,
            start_time_ms: self.row.start_time_ms,
            spawned_ms: self.spawned_ms,
            updated_ms: self.updated_ms,
            state: self.state,
            alive: live.is_some_and(|row| row.is_alive()),
            exit_code: self.exit_code,
            detail: self.detail.clone(),
            pty_id: live.map(|row| row.pty_id),
        }
    }

    fn is_open(&self) -> bool {
        !self.state.is_terminal()
    }
}

/// Durable ownership of one session-owned terminal row (legacy read surface:
/// one entry per live row of the requesting session).
#[derive(Debug, Clone)]
pub struct TerminalOwnership {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub agent_id: Option<String>,
    pub operation_id: OpId,
    pub pid: u32,
    pub spawned_ms: i64,
}

/// The per-session fold cache: strictly derived from the durable rows and
/// rebuilt from scratch by a restarted daemon (watermark = newest applied
/// seq; terminal rows are pinned across compaction, so the cursor never
/// rewinds).
#[derive(Debug, Default)]
struct SessionIndex {
    watermark: i64,
    recovered: bool,
    terminals: BTreeMap<String, DurableTerminal>,
}

impl SessionIndex {
    fn apply(&mut self, record: TerminalLedgerRecord) {
        self.watermark = self.watermark.max(record.seq);
        let entry = self
            .terminals
            .entry(record.row.terminal_id.clone())
            .or_insert_with(|| DurableTerminal {
                row: record.row.clone(),
                state: TerminalEventKind::Created,
                detail: String::new(),
                exit_code: None,
                seq: record.seq,
                spawned_ms: record.row.at_ms,
                updated_ms: record.row.at_ms,
            });
        if record.kind == TerminalEventKind::Created {
            entry.spawned_ms = record.row.at_ms;
        }
        entry.row = record.row;
        entry.state = record.kind;
        entry.detail = record.detail;
        entry.exit_code = record.exit_code;
        entry.seq = record.seq;
        entry.updated_ms = entry.row.at_ms;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveState {
    /// The pty authority owns a running child.
    Active,
    /// Exactly one terminal row was journaled for this live row.
    Terminal,
}

struct LiveRow {
    terminal_id: String,
    session_id: SessionId,
    pty_id: u64,
    pty: Arc<Mutex<Pty>>,
    row: TerminalDurableRow,
    state: Mutex<LiveState>,
}

impl LiveRow {
    fn is_alive(&self) -> bool {
        self.pty.lock().map(|pty| pty.is_alive()).unwrap_or(false)
    }
}

impl fmt::Debug for LiveRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveRow")
            .field("terminal_id", &self.terminal_id)
            .field("session_id", &self.session_id)
            .field("pty_id", &self.pty_id)
            .field("pid", &self.row.pid)
            .finish_non_exhaustive()
    }
}

/// The session-owned terminal service of one daemon process. The DURABLE
/// ledger rows are the authority; the live map is a strictly derived cache
/// of this boot's pty handles and can never disagree with a durable fold.
pub struct TerminalService {
    session: Arc<SessionManager>,
    probe: IdentityProbe,
    next_pty_id: AtomicU64,
    /// Live pty handles of THIS boot, keyed by terminal UUID.
    live: Mutex<HashMap<String, Arc<LiveRow>>>,
    /// Terminal ids whose create is mid-flight (journaled but not yet live):
    /// recovery must never mark one Lost before its live row is registered.
    inflight: Mutex<HashSet<String>>,
    /// The per-session durable fold cache.
    index: Mutex<HashMap<u64, SessionIndex>>,
    /// Serializes recovery scans (the recovered flag alone would race).
    recovery_lock: Mutex<()>,
}

/// One registry entry: the manager's address key, the manager weak (so a
/// dead manager's stale entry is pruned) and the service.
type ServiceRegistryEntry = (usize, Weak<SessionManager>, Arc<TerminalService>);

/// Process-wide service registry: ONE service per `SessionManager`, so the
/// ACP host and the native surface of the daemon share the same authority.
/// The strong `Arc` is intentional — the service owns the live terminal
/// handles for the lifetime of the session manager it is rooted at.
static SERVICES: OnceLock<Mutex<Vec<ServiceRegistryEntry>>> = OnceLock::new();

fn services() -> &'static Mutex<Vec<ServiceRegistryEntry>> {
    SERVICES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Bound on registered services (one per `SessionManager` ever created in
/// this process): beyond it, services with no live rows are released first
/// (their durable rows stay the authority), then the oldest entry. The
/// daemon has one manager; this bound keeps adversarial test processes (and
/// exotic hosts) from growing the registry without limit.
const MAX_REGISTERED_SERVICES: usize = 64;

/// The LEGACY name of the one daemon terminal service. Kept as an alias so
/// every existing consumer constructs the SAME authority.
pub type TerminalRegistry = TerminalService;

impl TerminalService {
    /// The daemon's ONE service for `session` (created on first use). Every
    /// caller sharing this `SessionManager` shares the service, its live
    /// rows and its durable authority.
    pub fn for_manager(session: &Arc<SessionManager>) -> Arc<Self> {
        let key = Arc::as_ptr(session) as usize;
        let mut registry = services()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.retain(|(_, weak, _)| weak.strong_count() > 0);
        if let Some((_, _, service)) = registry.iter().find(|(k, _, _)| *k == key) {
            return Arc::clone(service);
        }
        // Bounded registry: release services with no live rows first (their
        // durable rows remain the authority), then the oldest entry.
        if registry.len() >= MAX_REGISTERED_SERVICES {
            registry.retain(|(_, _, service)| service.live_rows() > 0);
        }
        if registry.len() >= MAX_REGISTERED_SERVICES {
            registry.remove(0);
        }
        let service = Arc::new(Self::with_probe(
            session.clone(),
            Arc::new(default_identity_probe),
        ));
        registry.push((key, Arc::downgrade(session), Arc::clone(&service)));
        service
    }

    /// [`Self::for_manager`] with an injected process-identity probe (the
    /// deterministic test seam; production uses the `ps`-based default).
    pub fn with_identity_probe(session: Arc<SessionManager>, probe: IdentityProbe) -> Arc<Self> {
        let key = Arc::as_ptr(&session) as usize;
        let service = Arc::new(Self::with_probe(session.clone(), probe));
        let mut registry = services()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.retain(|(_, weak, _)| weak.strong_count() > 0);
        registry.retain(|(k, _, _)| *k != key);
        registry.push((key, Arc::downgrade(&session), Arc::clone(&service)));
        service
    }

    /// An unregistered service (tests that want strict drop semantics).
    pub fn detached(session: Arc<SessionManager>, probe: IdentityProbe) -> Arc<Self> {
        Arc::new(Self::with_probe(session, probe))
    }

    fn with_probe(session: Arc<SessionManager>, probe: IdentityProbe) -> Self {
        Self {
            session,
            probe,
            next_pty_id: AtomicU64::new(1),
            live: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            index: Mutex::new(HashMap::new()),
            recovery_lock: Mutex::new(()),
        }
    }

    /// Build a service over the daemon's ONE session manager (legacy
    /// constructor: the same `for_manager` authority).
    pub fn new(session: Arc<SessionManager>) -> Arc<Self> {
        Self::for_manager(&session)
    }

    fn lock_live(&self) -> MutexGuard<'_, HashMap<String, Arc<LiveRow>>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_inflight(&self) -> MutexGuard<'_, HashSet<String>> {
        self.inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_index(&self) -> MutexGuard<'_, HashMap<u64, SessionIndex>> {
        self.index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Clone one live row out of the map (the map lock is released before
    /// any per-row state lock is taken, so kill/exit/sweep can never
    /// deadlock against each other).
    fn live_row(&self, terminal_id: &str) -> Option<Arc<LiveRow>> {
        self.lock_live().get(terminal_id).cloned()
    }

    /// Live-row count (diagnostic/test probe).
    pub fn live_rows(&self) -> usize {
        self.lock_live()
            .values()
            .filter(|row| row.is_alive())
            .count()
    }

    /// The live ownership rows of one session, sorted by terminal id. Rows
    /// of another session are never included.
    pub fn session_rows(&self, session_id: SessionId) -> Vec<(String, TerminalOwnership)> {
        let live = self.lock_live();
        let mut out: Vec<(String, TerminalOwnership)> = live
            .values()
            .filter(|row| row.session_id == session_id)
            .map(|row| {
                (
                    row.terminal_id.clone(),
                    TerminalOwnership {
                        session_id: row.session_id,
                        task_id: TaskId::new(row.row.task_id),
                        agent_id: row.row.agent_id.clone(),
                        operation_id: OpId::new(row.row.operation_id),
                        pid: row.row.pid,
                        spawned_ms: row.row.at_ms,
                    },
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    // ------------------------------------------------------------ resolution

    fn session_handle(&self, session_id: &str) -> Result<SessionHandle, TerminalServiceError> {
        let raw: u64 = session_id.parse().map_err(|_| {
            TerminalServiceError::Invalid(format!("invalid session id {session_id:?}"))
        })?;
        if raw == 0 {
            return Err(TerminalServiceError::Invalid(
                "session id cannot be 0".into(),
            ));
        }
        self.session
            .get_session(SessionId::new(raw))
            .map_err(|e| TerminalServiceError::Refused(e.message))?
            .ok_or_else(|| TerminalServiceError::Invalid(format!("unknown session {session_id:?}")))
    }

    /// Refresh one session's durable fold from the entry stream (the rows
    /// remain the authority; the index only caches the decoded fold).
    fn fold(
        &self,
        handle: &SessionHandle,
        sid: u64,
    ) -> Result<BTreeMap<String, DurableTerminal>, TerminalServiceError> {
        let mut index = self.lock_index();
        let entry = index.entry(sid).or_default();
        let records = handle
            .ledger_terminal_rows(Some(entry.watermark))
            .map_err(|e| TerminalServiceError::Refused(e.message))?;
        for record in records {
            entry.apply(record);
        }
        Ok(entry.terminals.clone())
    }

    fn apply_durable(&self, sid: u64, record: TerminalLedgerRecord) {
        self.lock_index().entry(sid).or_default().apply(record);
    }

    fn mark_recovered(&self, sid: u64) {
        self.lock_index().entry(sid).or_default().recovered = true;
    }

    /// The typed recovery decision for one non-terminal durable row that no
    /// live authority owns in this boot. The pid is probed and the recorded
    /// identity decides the audit reason; a mismatch is NEVER adopted.
    fn recovery_reason(&self, row: &TerminalDurableRow) -> String {
        match (self.probe)(row.pid) {
            None => format!(
                "no live pty authority after restart; pid {} is not observable \
                 (never adopted by pid alone)",
                row.pid
            ),
            Some(_) if row.start_time_ms == 0 => format!(
                "no live pty authority after restart; pid {} has no recorded start time \
                 (identity unverified, never adopted by pid alone)",
                row.pid
            ),
            Some(observed) if observed == row.start_time_ms => format!(
                "no live pty authority after restart; pid {} still matches the recorded start \
                 time {}ms but is unreachable (never signalled)",
                row.pid, row.start_time_ms
            ),
            Some(observed) => format!(
                "pid {} was recycled: observed start time {}ms differs from the recorded {}ms \
                 (never adopted or signalled)",
                row.pid, observed, row.start_time_ms
            ),
        }
    }

    /// Sweep live rows whose pty child was observed dead and journal exactly
    /// one `terminal_exited` row per row (state-machine guarded).
    fn sweep_live(&self) {
        let rows: Vec<Arc<LiveRow>> = self.lock_live().values().cloned().collect();
        for live in rows {
            if live.is_alive() {
                continue;
            }
            let _ = self.terminalize(&live, None, None);
        }
    }

    /// Transition one live row to a terminal state exactly once. `killed`
    /// selects the journaled kind (`terminal_killed` with a reason) vs
    /// `terminal_exited` (exit code unknown).
    fn terminalize(
        &self,
        live: &LiveRow,
        killed_reason: Option<&str>,
        exit_code: Option<i32>,
    ) -> Result<bool, TerminalServiceError> {
        let mut state = live
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *state == LiveState::Terminal {
            return Ok(false);
        }
        // A kill routes through the pty authority (guardian-compatible: the
        // pty owns the process tree); an observed exit needs no signal.
        if killed_reason.is_some() {
            kill_pty(&live.pty);
        }
        let mut row = live.row.clone();
        row.at_ms = self.session.now_ms();
        let handle = self
            .session
            .get_session(live.session_id)
            .map_err(|e| TerminalServiceError::Refused(e.message))?
            .ok_or_else(|| {
                TerminalServiceError::Invalid(format!("unknown session {}", live.session_id))
            })?;
        let journal = match killed_reason {
            Some(reason) => handle
                .ledger_terminal_killed(&row, reason)
                .map(|seq| (TerminalEventKind::Killed, reason.to_string(), seq)),
            None => handle
                .ledger_terminal_exited(&row, exit_code)
                .map(|seq| (TerminalEventKind::Exited, String::new(), seq)),
        };
        match journal {
            Ok((kind, detail, seq)) => {
                self.apply_durable(
                    live.session_id.raw(),
                    TerminalLedgerRecord {
                        seq,
                        kind,
                        row,
                        exit_code,
                        detail,
                    },
                );
            }
            Err(error) => {
                // The pty is already gone: retire the live row anyway so no
                // further I/O is served, and let a later recovery scan
                // journal the Lost row from the durable state (the session's
                // recovered flag is cleared so the scan actually reruns).
                *state = LiveState::Terminal;
                self.lock_live().remove(&live.terminal_id);
                self.lock_index()
                    .entry(live.session_id.raw())
                    .or_default()
                    .recovered = false;
                return Err(TerminalServiceError::Refused(error.message));
            }
        }
        *state = LiveState::Terminal;
        self.lock_live().remove(&live.terminal_id);
        Ok(true)
    }

    /// Mark every non-terminal durable row of one session Lost when no live
    /// pty authority of this boot owns it. Idempotent (a Lost row is
    /// terminal, so a second scan journals nothing).
    fn recover_rows(
        &self,
        handle: &SessionHandle,
        sid: u64,
    ) -> Result<usize, TerminalServiceError> {
        self.sweep_live();
        let terminals = self.fold(handle, sid)?;
        let mut lost = 0usize;
        for (terminal_id, entry) in terminals {
            if !entry.is_open() {
                continue;
            }
            if self.live_row(&terminal_id).is_some() {
                continue;
            }
            if self.lock_inflight().contains(&terminal_id) {
                continue;
            }
            let reason = self.recovery_reason(&entry.row);
            let mut row = entry.row.clone();
            row.at_ms = self.session.now_ms();
            let seq = handle
                .ledger_terminal_lost(&row, &reason)
                .map_err(|e| TerminalServiceError::Refused(e.message))?;
            self.apply_durable(
                sid,
                TerminalLedgerRecord {
                    seq,
                    kind: TerminalEventKind::Lost,
                    row,
                    exit_code: None,
                    detail: reason,
                },
            );
            lost += 1;
        }
        Ok(lost)
    }

    /// Recover ONE session: every non-terminal durable row without a live
    /// pty authority of this boot is marked Lost (typed, identity-checked,
    /// never signalled by pid). Lazy recovery runs it on the first touch of
    /// a session after a restart; callers can also force it.
    pub fn recover_session(
        &self,
        session_id: &str,
    ) -> Result<TerminalRecoveryReport, TerminalServiceError> {
        let handle = self.session_handle(session_id)?;
        let sid = handle.id().raw();
        let _guard = self
            .recovery_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let terminals = self.fold(&handle, sid)?;
        let report = self.build_recovery_report(&terminals);
        self.recover_rows(&handle, sid)?;
        self.mark_recovered(sid);
        Ok(report)
    }

    /// Recover EVERY durable session: the restart scan. Returns the aggregate
    /// report; a second call after the first journals nothing.
    pub fn recover_all(&self) -> Result<TerminalRecoveryReport, TerminalServiceError> {
        let sessions = self
            .session
            .list_sessions(None)
            .map_err(|e| TerminalServiceError::Refused(e.message))?;
        let mut aggregate = TerminalRecoveryReport::default();
        for handle in sessions {
            let sid = handle.id().raw();
            let _guard = self
                .recovery_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let terminals = self.fold(&handle, sid)?;
            let report = self.build_recovery_report(&terminals);
            let _lost = self.recover_rows(&handle, sid)?;
            self.mark_recovered(sid);
            aggregate.sessions_scanned += 1;
            aggregate.terminals_scanned += report.terminals_scanned;
            aggregate.already_terminal += report.already_terminal;
            aggregate.live += report.live;
            aggregate.lost.extend(report.lost);
        }
        Ok(aggregate)
    }

    fn build_recovery_report(
        &self,
        terminals: &BTreeMap<String, DurableTerminal>,
    ) -> TerminalRecoveryReport {
        let mut report = TerminalRecoveryReport::default();
        for (terminal_id, entry) in terminals {
            if !entry.is_open() {
                report.already_terminal += 1;
                continue;
            }
            report.terminals_scanned += 1;
            if self.live_row(terminal_id).is_some() {
                report.live += 1;
            } else {
                report.lost.push(terminal_id.clone());
            }
        }
        report
    }

    fn ensure_recovered(
        &self,
        handle: &SessionHandle,
        sid: u64,
    ) -> Result<(), TerminalServiceError> {
        if self
            .lock_index()
            .get(&sid)
            .is_some_and(|index| index.recovered)
        {
            return Ok(());
        }
        let _guard = self
            .recovery_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self
            .lock_index()
            .get(&sid)
            .is_some_and(|index| index.recovered)
        {
            return Ok(());
        }
        self.recover_rows(handle, sid)?;
        self.mark_recovered(sid);
        Ok(())
    }

    // ----------------------------------------------------------- the surface

    /// Spawn one session-owned terminal. The durable `terminal_created` row
    /// is journaled before the row is exposed; the `terminal_running` row
    /// follows once the child identity is known. A spawn/store failure kills
    /// the just-spawned pty before returning, so no child is ever orphaned.
    pub fn spawn(
        self: &Arc<Self>,
        session_id: &str,
        request: &TerminalSpawnRequest,
    ) -> Result<TerminalCreation, TerminalServiceError> {
        let handle = self.session_handle(session_id)?;
        let row = handle
            .row()
            .map_err(|e| TerminalServiceError::Refused(e.message))?;
        validate_spawn_request(request)?;
        let sid = handle.id();
        self.ensure_recovered(&handle, sid.raw())?;
        self.sweep_live();

        if self.lock_live().len() >= MAX_DAEMON_TERMINALS {
            return Err(TerminalServiceError::Unavailable(format!(
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

        let terminal_id = new_terminal_id();
        self.lock_inflight().insert(terminal_id.clone());
        let prepared = self.prepare_spawn(&handle, row.task_id, &terminal_id, cfg);
        self.lock_inflight().remove(&terminal_id);
        prepared
    }

    fn prepare_spawn(
        self: &Arc<Self>,
        handle: &SessionHandle,
        task_id: TaskId,
        terminal_id: &str,
        cfg: PtyConfig,
    ) -> Result<TerminalCreation, TerminalServiceError> {
        let pty = Pty::spawn(&cfg).map_err(|e| TerminalServiceError::Refused(e.message))?;
        let pid = pty.pid();
        let start_time_ms = (self.probe)(pid).unwrap_or(0);
        let pty = Arc::new(Mutex::new(pty));
        let mut durable = TerminalDurableRow {
            terminal_id: terminal_id.to_string(),
            session_id: handle.id().raw(),
            task_id: task_id.raw(),
            agent_id: None,
            operation_id: self.session.next_op_id().raw(),
            pid,
            start_time_ms,
            at_ms: self.session.now_ms(),
        };

        let created_row = durable.clone();
        let created_seq = match handle.ledger_terminal_created(&created_row) {
            Ok(seq) => seq,
            Err(error) => {
                kill_pty(&pty);
                return Err(TerminalServiceError::Refused(error.message));
            }
        };
        durable.at_ms = self.session.now_ms();
        let running_seq = match handle.ledger_terminal_running(&durable) {
            Ok(seq) => seq,
            Err(error) => {
                // The Created row stays durable with no live authority: the
                // next recovery scan marks it Lost. The pty is killed so no
                // process is orphaned without its journaled state.
                kill_pty(&pty);
                return Err(TerminalServiceError::Refused(error.message));
            }
        };
        self.apply_durable(
            handle.id().raw(),
            TerminalLedgerRecord {
                seq: created_seq,
                kind: TerminalEventKind::Created,
                row: created_row,
                exit_code: None,
                detail: String::new(),
            },
        );
        self.apply_durable(
            handle.id().raw(),
            TerminalLedgerRecord {
                seq: running_seq,
                kind: TerminalEventKind::Running,
                row: durable.clone(),
                exit_code: None,
                detail: String::new(),
            },
        );

        let pty_id = self.next_pty_id.fetch_add(1, Ordering::SeqCst);
        let live = Arc::new(LiveRow {
            terminal_id: terminal_id.to_string(),
            session_id: handle.id(),
            pty_id,
            pty: Arc::clone(&pty),
            row: durable.clone(),
            state: Mutex::new(LiveState::Active),
        });
        self.lock_live()
            .insert(terminal_id.to_string(), Arc::clone(&live));
        let view = DurableTerminal {
            row: durable.clone(),
            state: TerminalEventKind::Running,
            detail: String::new(),
            exit_code: None,
            seq: running_seq,
            spawned_ms: durable.at_ms,
            updated_ms: durable.at_ms,
        }
        .view(Some(&live));

        Ok(TerminalCreation {
            handle: TerminalHandle {
                service: Arc::downgrade(self),
                terminal_id: terminal_id.to_string(),
                ownership_id: durable.operation_id.to_string(),
                pid,
                session_id: handle.id(),
                task_id,
                pty,
            },
            view,
        })
    }

    /// The LEGACY constructor surface: spawn and return the handle, mapping
    /// the typed service failure onto the frozen registry taxonomy.
    pub fn create(
        self: &Arc<Self>,
        session_id: &str,
        request: &TerminalSpawnRequest,
    ) -> Result<TerminalHandle, TerminalRegistryError> {
        self.spawn(session_id, request)
            .map(|creation| creation.handle)
            .map_err(TerminalRegistryError::from)
    }

    /// Scope-enforced durable listing of one session. Rows of another
    /// session are never included.
    pub fn list(&self, session_id: &str) -> Result<Vec<TerminalView>, TerminalServiceError> {
        let handle = self.session_handle(session_id)?;
        let sid = handle.id();
        self.ensure_recovered(&handle, sid.raw())?;
        self.sweep_live();
        let terminals = self.fold(&handle, sid.raw())?;
        let live = self.lock_live();
        let mut out: Vec<TerminalView> = terminals
            .values()
            .filter(|entry| entry.row.session_id == sid.raw())
            .map(|entry| entry.view(live.get(&entry.row.terminal_id).map(|row| &**row)))
            .collect();
        out.sort_by(|a, b| a.terminal_id.cmp(&b.terminal_id));
        Ok(out)
    }

    /// The durable terminal event stream of one session, ascending by seq,
    /// strictly above `after`, bounded by `limit`.
    pub fn events(
        &self,
        session_id: &str,
        after: Option<i64>,
        limit: u64,
    ) -> Result<(Vec<TerminalLedgerRecord>, bool), TerminalServiceError> {
        let handle = self.session_handle(session_id)?;
        let limit = limit.clamp(1, 500);
        self.ensure_recovered(&handle, handle.id().raw())?;
        self.sweep_live();
        let mut records = handle
            .ledger_terminal_rows(after)
            .map_err(|e| TerminalServiceError::Refused(e.message))?;
        let has_more = records.len() as u64 > limit;
        records.truncate(limit as usize);
        Ok((records, has_more))
    }

    /// Write input to a Running terminal through the pty authority. A row
    /// with no live authority (restart) is refused with the typed `Lost`
    /// error; a terminal row refuses I/O typed; a foreign terminal id is
    /// reported exactly like an unknown one.
    pub fn input(
        &self,
        session_id: &str,
        terminal_id: &str,
        data: &[u8],
    ) -> Result<(), TerminalServiceError> {
        if data.len() > MAX_TERMINAL_INPUT_BYTES {
            return Err(TerminalServiceError::Invalid(format!(
                "terminal input of {} bytes exceeds the {MAX_TERMINAL_INPUT_BYTES}-byte bound",
                data.len()
            )));
        }
        if data.contains(&0) {
            return Err(TerminalServiceError::Invalid(
                "terminal input contains a NUL byte".into(),
            ));
        }
        let handle = self.session_handle(session_id)?;
        let sid = handle.id();
        self.ensure_recovered(&handle, sid.raw())?;
        self.sweep_live();
        let terminals = self.fold(&handle, sid.raw())?;
        let entry = terminals
            .get(terminal_id)
            .ok_or_else(|| TerminalServiceError::Unknown {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            })?;
        if entry.row.session_id != sid.raw() {
            return Err(TerminalServiceError::ForeignScope {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            });
        }
        let live = match entry.state {
            TerminalEventKind::Running => {
                self.live_row(terminal_id)
                    .ok_or_else(|| TerminalServiceError::Lost {
                        terminal_id: terminal_id.to_string(),
                        detail: "the pty authority of this daemon boot does not own the row".into(),
                    })?
            }
            TerminalEventKind::Lost => {
                return Err(TerminalServiceError::Lost {
                    terminal_id: terminal_id.to_string(),
                    detail: entry.detail.clone(),
                })
            }
            other => {
                return Err(TerminalServiceError::State {
                    terminal_id: terminal_id.to_string(),
                    state: other.state_tag(),
                    message: "only a running terminal accepts input".into(),
                })
            }
        };
        if live.session_id != sid {
            return Err(TerminalServiceError::ForeignScope {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            });
        }
        let pty = live
            .pty
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !pty.is_alive() {
            drop(pty);
            let _ = self.terminalize(&live, None, None);
            return Err(TerminalServiceError::State {
                terminal_id: terminal_id.to_string(),
                state: "exited",
                message: "the child process is gone".into(),
            });
        }
        pty.write_all(data)
            .map_err(|e| TerminalServiceError::Refused(e.message))
    }

    /// Resize a Running terminal through the pty authority.
    pub fn resize(
        &self,
        session_id: &str,
        terminal_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), TerminalServiceError> {
        if rows == 0 || cols == 0 {
            return Err(TerminalServiceError::Invalid(
                "terminal size must be non-zero".into(),
            ));
        }
        let handle = self.session_handle(session_id)?;
        let sid = handle.id();
        self.ensure_recovered(&handle, sid.raw())?;
        self.sweep_live();
        let terminals = self.fold(&handle, sid.raw())?;
        let entry = terminals
            .get(terminal_id)
            .ok_or_else(|| TerminalServiceError::Unknown {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            })?;
        if entry.row.session_id != sid.raw() {
            return Err(TerminalServiceError::ForeignScope {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            });
        }
        let live = match entry.state {
            TerminalEventKind::Running => {
                self.live_row(terminal_id)
                    .ok_or_else(|| TerminalServiceError::Lost {
                        terminal_id: terminal_id.to_string(),
                        detail: "the pty authority of this daemon boot does not own the row".into(),
                    })?
            }
            TerminalEventKind::Lost => {
                return Err(TerminalServiceError::Lost {
                    terminal_id: terminal_id.to_string(),
                    detail: entry.detail.clone(),
                })
            }
            other => {
                return Err(TerminalServiceError::State {
                    terminal_id: terminal_id.to_string(),
                    state: other.state_tag(),
                    message: "only a running terminal accepts a resize".into(),
                })
            }
        };
        if live.session_id != sid {
            return Err(TerminalServiceError::ForeignScope {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            });
        }
        let pty = live
            .pty
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !pty.is_alive() {
            drop(pty);
            let _ = self.terminalize(&live, None, None);
            return Err(TerminalServiceError::State {
                terminal_id: terminal_id.to_string(),
                state: "exited",
                message: "the child process is gone".into(),
            });
        }
        pty.resize(rows, cols)
            .map_err(|e| TerminalServiceError::Refused(e.message))
    }

    /// Kill one terminal through the pty authority. Idempotent: once a
    /// terminal row exists the call answers `Ok(false)` and journals
    /// nothing, so a create/kill race yields exactly one state sequence.
    /// Returns `Ok(true)` when THIS call journaled `terminal_killed`.
    pub fn kill(
        &self,
        session_id: &str,
        terminal_id: &str,
        reason: &str,
    ) -> Result<bool, TerminalServiceError> {
        let handle = self.session_handle(session_id)?;
        let sid = handle.id();
        self.ensure_recovered(&handle, sid.raw())?;
        self.sweep_live();
        let terminals = self.fold(&handle, sid.raw())?;
        let entry = terminals
            .get(terminal_id)
            .ok_or_else(|| TerminalServiceError::Unknown {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            })?;
        if entry.row.session_id != sid.raw() {
            return Err(TerminalServiceError::ForeignScope {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            });
        }
        if let Some(live) = self.live_row(terminal_id) {
            if live.session_id != sid {
                return Err(TerminalServiceError::ForeignScope {
                    session_id: session_id.to_string(),
                    terminal_id: terminal_id.to_string(),
                });
            }
            return self.terminalize(&live, Some(reason), None);
        }
        match entry.state {
            TerminalEventKind::Lost => Err(TerminalServiceError::Lost {
                terminal_id: terminal_id.to_string(),
                detail: entry.detail.clone(),
            }),
            // Created without a live row can only be an in-flight create.
            TerminalEventKind::Created | TerminalEventKind::Running => Err(
                TerminalServiceError::Unavailable(format!("terminal {terminal_id:?} is starting")),
            ),
            // Already terminal: idempotent no-op, no second row.
            TerminalEventKind::Exited
            | TerminalEventKind::Killed
            | TerminalEventKind::Reconciled => Ok(false),
        }
    }

    /// Finish one stale Lost row exactly once, typed. `observed` is the
    /// caller-observed process identity (probe result); a mismatch with the
    /// durable row is refused and NOTHING is journaled or signalled.
    pub fn reconcile(
        &self,
        session_id: &str,
        terminal_id: &str,
        disposition: TerminalReconcileDisposition,
        observed: Option<ProcessIdentity>,
    ) -> Result<TerminalView, TerminalServiceError> {
        let handle = self.session_handle(session_id)?;
        let sid = handle.id();
        self.ensure_recovered(&handle, sid.raw())?;
        self.sweep_live();
        let terminals = self.fold(&handle, sid.raw())?;
        let entry = terminals
            .get(terminal_id)
            .ok_or_else(|| TerminalServiceError::Unknown {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            })?;
        if entry.row.session_id != sid.raw() {
            return Err(TerminalServiceError::ForeignScope {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            });
        }
        if entry.state != TerminalEventKind::Lost {
            return Err(TerminalServiceError::State {
                terminal_id: terminal_id.to_string(),
                state: entry.state.state_tag(),
                message: "only a Lost row can be reconciled".into(),
            });
        }
        if self.live_row(terminal_id).is_some() {
            return Err(TerminalServiceError::State {
                terminal_id: terminal_id.to_string(),
                state: entry.state.state_tag(),
                message: "a live pty authority owns the row; kill it through the authority".into(),
            });
        }
        if let Some(observed) = observed {
            if observed.pid != entry.row.pid {
                return Err(TerminalServiceError::IdentityMismatch {
                    terminal_id: terminal_id.to_string(),
                    expected_pid: entry.row.pid,
                    expected_start_time_ms: entry.row.start_time_ms,
                    observed_start_time_ms: observed.start_time_ms,
                });
            }
            if entry.row.start_time_ms != 0
                && observed.start_time_ms != 0
                && observed.start_time_ms != entry.row.start_time_ms
            {
                return Err(TerminalServiceError::IdentityMismatch {
                    terminal_id: terminal_id.to_string(),
                    expected_pid: entry.row.pid,
                    expected_start_time_ms: entry.row.start_time_ms,
                    observed_start_time_ms: observed.start_time_ms,
                });
            }
        }
        let mut row = entry.row.clone();
        row.at_ms = self.session.now_ms();
        let seq = handle
            .ledger_terminal_reconciled(&row, disposition.as_tag())
            .map_err(|e| TerminalServiceError::Refused(e.message))?;
        self.apply_durable(
            sid.raw(),
            TerminalLedgerRecord {
                seq,
                kind: TerminalEventKind::Reconciled,
                row,
                exit_code: None,
                detail: disposition.as_tag().to_string(),
            },
        );
        let terminals = self.fold(&handle, sid.raw())?;
        let entry = terminals
            .get(terminal_id)
            .ok_or_else(|| TerminalServiceError::Unknown {
                session_id: session_id.to_string(),
                terminal_id: terminal_id.to_string(),
            })?;
        Ok(entry.view(None))
    }

    /// Kill every live row and journal each kill (bounded daemon shutdown).
    pub fn shutdown_all(&self) {
        let rows: Vec<Arc<LiveRow>> = self.lock_live().values().cloned().collect();
        for live in rows {
            let _ = self.terminalize(&live, Some("daemon shutdown"), None);
        }
    }
}

impl Drop for TerminalService {
    /// Emergency failsafe: dropping the last reference kills every pty tree
    /// this service still owns (the per-row `terminal_killed` rows are
    /// journaled by [`TerminalService::shutdown_all`]; a drop without it
    /// leaves the rows for the next recovery scan).
    fn drop(&mut self) {
        let live = std::mem::take(
            &mut *self
                .live
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for row in live.into_values() {
            kill_pty(&row.pty);
        }
    }
}

/// One live daemon terminal handed to the ACP seam. `kill` is idempotent and
/// journals `terminal_killed` exactly once; dropping the handle does NOT
/// kill the child (the service owns it), so a terminal survives a dropped
/// adapter exactly as its durable row says.
pub struct TerminalHandle {
    service: Weak<TerminalService>,
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
    /// The authority-unique terminal id (the durable row's UUID key).
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

    /// Write raw bytes to the terminal (state-checked by the service).
    pub fn write(&self, bytes: &[u8]) -> Result<(), TerminalRegistryError> {
        match self.service.upgrade() {
            Some(service) => service
                .write_handle(&self.terminal_id, bytes)
                .map_err(TerminalRegistryError::from),
            None => self
                .lock_pty()
                .write_all(bytes)
                .map_err(|e| TerminalRegistryError::Refused(e.message)),
        }
    }

    /// Resize the terminal window (state-checked by the service).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), TerminalRegistryError> {
        if rows == 0 || cols == 0 {
            return Err(TerminalRegistryError::Invalid(
                "terminal size must be non-zero".into(),
            ));
        }
        match self.service.upgrade() {
            Some(service) => service
                .resize_handle(&self.terminal_id, rows, cols)
                .map_err(TerminalRegistryError::from),
            None => self
                .lock_pty()
                .resize(rows, cols)
                .map_err(|e| TerminalRegistryError::Refused(e.message)),
        }
    }

    /// Drain all currently available output (bounded by the pty ring; never
    /// blocking).
    pub fn drain_output(&self) -> Vec<u8> {
        self.lock_pty().read_available()
    }

    /// Kill the whole process tree through the pty authority (idempotent;
    /// journals `terminal_killed` exactly once).
    pub fn kill(&self) -> Result<(), TerminalRegistryError> {
        match self.service.upgrade() {
            Some(service) => service
                .kill_handle(&self.terminal_id)
                .map_err(TerminalRegistryError::from),
            None => {
                self.lock_pty().kill();
                Ok(())
            }
        }
    }

    fn lock_pty(&self) -> MutexGuard<'_, Pty> {
        self.pty
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl TerminalService {
    /// Live write used by [`TerminalHandle::write`]: refuses once the row's
    /// one terminal transition was journaled.
    fn write_handle(&self, terminal_id: &str, bytes: &[u8]) -> Result<(), TerminalServiceError> {
        let live = self.live_row(terminal_id).ok_or_else(|| {
            TerminalServiceError::Unavailable(format!("terminal {terminal_id:?} is gone"))
        })?;
        let state = live
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *state == LiveState::Terminal {
            return Err(TerminalServiceError::Unavailable(format!(
                "terminal {terminal_id:?} is gone"
            )));
        }
        drop(state);
        if bytes.len() > MAX_TERMINAL_INPUT_BYTES {
            return Err(TerminalServiceError::Invalid(
                "terminal input exceeds the bound".into(),
            ));
        }
        let result = live
            .pty
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write_all(bytes)
            .map_err(|e| TerminalServiceError::Refused(e.message));
        result
    }

    fn resize_handle(
        &self,
        terminal_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), TerminalServiceError> {
        let live = self.live_row(terminal_id).ok_or_else(|| {
            TerminalServiceError::Unavailable(format!("terminal {terminal_id:?} is gone"))
        })?;
        let state = live
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *state == LiveState::Terminal {
            return Err(TerminalServiceError::Unavailable(format!(
                "terminal {terminal_id:?} is gone"
            )));
        }
        drop(state);
        let result = live
            .pty
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resize(rows, cols)
            .map_err(|e| TerminalServiceError::Refused(e.message));
        result
    }

    fn kill_handle(&self, terminal_id: &str) -> Result<(), TerminalServiceError> {
        // The handle path is the idempotent best-effort kill: a row that is
        // already terminal (or owned by another boot) has nothing to signal.
        let Some(live) = self.live_row(terminal_id) else {
            return Ok(());
        };
        self.terminalize(
            &live,
            Some("kill requested through the terminal authority"),
            None,
        )?;
        Ok(())
    }
}

/// Kill one pty through the pty authority (the emergency path).
fn kill_pty(pty: &Arc<Mutex<Pty>>) {
    if let Ok(mut pty) = pty.lock() {
        pty.kill();
    }
}

/// Mint one canonical UUID v4 from the process RNG (the terminal UUID is the
/// durable authority key; it is deliberately NOT pid-derived).
fn new_terminal_id() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    // RFC 4122 version 4 + variant bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = |range: std::ops::Range<usize>| -> String {
        bytes[range].iter().map(|b| format!("{b:02x}")).collect()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..10),
        hex(10..16)
    )
}

/// The production process-start probe: the `faktor-pty` guardian's
/// start-time marker (allocation-free platform probe; the same marker the
/// pre-spawn guardian verifies before it ever signals a group). Normalized to
/// milliseconds: epoch ms on macOS, `USER_HZ` ticks (fixed 10ms) since boot
/// on Linux — equality-comparable within one boot, which is exactly what
/// recovery compares. A pid the platform cannot observe yields `None`: the
/// row is then unverified and NEVER adopted by pid alone.
pub fn default_identity_probe(pid: u32) -> Option<i64> {
    if pid == 0 {
        return None;
    }
    #[cfg(unix)]
    {
        let marker = faktor_pty::guardian::process_start_time(pid)?;
        #[cfg(target_os = "linux")]
        {
            Some(marker.saturating_mul(10) as i64)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Some((marker / 1000) as i64)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// Defensive field validation (the ACP layer validates too; the authority
/// must not depend on a caller having done so).
fn validate_spawn_request(request: &TerminalSpawnRequest) -> Result<(), TerminalServiceError> {
    if request.command.is_empty() || request.command.len() > MAX_TERMINAL_FIELD_BYTES {
        return Err(TerminalServiceError::Invalid(
            "terminal command must be non-empty and at most 4096 bytes".into(),
        ));
    }
    if request.command.contains('\0') {
        return Err(TerminalServiceError::Invalid(
            "terminal command contains a NUL byte".into(),
        ));
    }
    if request.args.len() > MAX_TERMINAL_ARGS {
        return Err(TerminalServiceError::Invalid(
            "terminal args exceed the 256-entry bound".into(),
        ));
    }
    for arg in &request.args {
        if arg.len() > MAX_TERMINAL_FIELD_BYTES {
            return Err(TerminalServiceError::Invalid(
                "terminal arg exceeds 4096 bytes".into(),
            ));
        }
        if arg.contains('\0') {
            return Err(TerminalServiceError::Invalid(
                "terminal arg contains a NUL byte".into(),
            ));
        }
    }
    if let Some(cwd) = &request.cwd {
        if cwd.is_empty() || cwd.len() > MAX_TERMINAL_FIELD_BYTES || cwd.contains('\0') {
            return Err(TerminalServiceError::Invalid(
                "terminal cwd is invalid or oversized".into(),
            ));
        }
    }
    if request.env.len() > MAX_TERMINAL_ENV_NAMES {
        return Err(TerminalServiceError::Invalid(
            "terminal env allowlist exceeds 64 names".into(),
        ));
    }
    for name in &request.env {
        if name.len() > MAX_TERMINAL_ENV_NAME_BYTES || name.contains('\0') {
            return Err(TerminalServiceError::Invalid(
                "terminal env name is invalid or oversized".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn manager() -> (tempfile::TempDir, Arc<SessionManager>) {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        (dir, manager)
    }

    fn session(manager: &Arc<SessionManager>, title: &str) -> String {
        let ws = manager.create_workspace("/tmp").unwrap();
        manager
            .create_session(ws, title, "fake", "m")
            .unwrap()
            .id()
            .to_string()
    }

    fn spawn_request(command: &str, args: &[&str]) -> TerminalSpawnRequest {
        TerminalSpawnRequest {
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
        }
    }

    /// A probe that records the marker handed to the first spawn and returns
    /// it for every later probe of that pid (simulated live process).
    fn recording_probe() -> (IdentityProbe, Arc<Mutex<HashMap<u32, i64>>>) {
        let map = Arc::new(Mutex::new(HashMap::new()));
        let map2 = Arc::clone(&map);
        let probe: IdentityProbe = Arc::new(move |pid| {
            let mut map = map2.lock().unwrap();
            Some(*map.entry(pid).or_insert(1_700_000_000_000))
        });
        (probe, map)
    }

    fn terminal_kinds(handle: &SessionHandle) -> Vec<TerminalEventKind> {
        handle
            .ledger_terminal_rows(None)
            .unwrap()
            .into_iter()
            .map(|record| record.kind)
            .collect()
    }

    /// Spawn or skip (documented platform refusal: PTY spawn is refused on
    /// platforms without a backend, exactly like every other PTY test).
    fn spawn_or_skip(
        service: &Arc<TerminalService>,
        sid: &str,
        request: &TerminalSpawnRequest,
    ) -> Option<TerminalCreation> {
        match service.spawn(sid, request) {
            Ok(creation) => Some(creation),
            Err(TerminalServiceError::Refused(_)) | Err(TerminalServiceError::Unavailable(_)) => {
                None
            }
            Err(other) => panic!("unexpected spawn failure: {other}"),
        }
    }

    #[test]
    fn create_running_kill_round_trip_journals_one_sequence_and_kills_the_tree() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-roundtrip");
        let (probe, _map) = recording_probe();
        let service = TerminalService::with_identity_probe(manager.clone(), probe);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        assert!(creation.handle.pid() > 0);
        assert!(!creation.handle.ownership_id().is_empty());
        assert!(creation.view.start_time_ms > 0, "identity recorded");
        assert_eq!(creation.view.state_tag(), "running");

        // The durable row stream is exactly created -> running.
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal_kinds(&handle),
            vec![TerminalEventKind::Created, TerminalEventKind::Running]
        );

        // Scope-enforced listing carries the full ownership.
        let rows = service.list(&sid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].terminal_id, terminal_id);
        assert_eq!(rows[0].state_tag(), "running");
        assert!(rows[0].alive);
        assert_eq!(rows[0].task_id.raw(), 1);
        assert!(rows[0].agent_id.is_none());

        // Kill routes through the pty authority and journals once.
        assert!(service.kill(&sid, &terminal_id, "roundtrip kill").unwrap());
        assert!(!creation.handle.is_alive());
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Killed
            ]
        );
        let rows = service.list(&sid).unwrap();
        assert_eq!(rows[0].state_tag(), "killed");
        assert!(!rows[0].alive);

        // Idempotent: a second kill journals NOTHING.
        assert!(!service.kill(&sid, &terminal_id, "again").unwrap());
        assert_eq!(terminal_kinds(&handle).len(), 3);
        // The handle path journals exactly once too.
        creation.handle.kill().unwrap();
        assert_eq!(terminal_kinds(&handle).len(), 3);
    }

    #[test]
    fn foreign_scope_is_denied_without_touching_the_owned_terminal() {
        let (_dir, manager) = manager();
        let a = session(&manager, "scope-a");
        let b = session(&manager, "scope-b");
        let (probe, _map) = recording_probe();
        let service = TerminalService::with_identity_probe(manager.clone(), probe);
        let Some(creation) = spawn_or_skip(&service, &a, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();

        // B's listing never contains A's terminal.
        assert!(service.list(&b).unwrap().is_empty());

        // Every foreign operation is denied typed AND leaves the row alone.
        for denied in [
            service.input(&b, &terminal_id, b"x"),
            service.resize(&b, &terminal_id, 10, 10),
        ] {
            assert!(denied.is_err(), "foreign scope must be denied");
        }
        assert!(
            service.kill(&b, &terminal_id, "hostile").is_err(),
            "foreign kill must be denied"
        );
        let rows = service.list(&a).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state_tag(), "running");
        assert!(rows[0].alive, "the owned terminal is untouched");
        let a_handle = manager
            .get_session(SessionId::new(a.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal_kinds(&a_handle),
            vec![TerminalEventKind::Created, TerminalEventKind::Running]
        );
        let _ = service.kill(&a, &terminal_id, "cleanup");
    }

    #[test]
    fn restart_marks_unreachable_terminals_lost_and_refuses_io_typed() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-restart");
        let (probe, identities) = recording_probe();
        let service = TerminalService::with_identity_probe(manager.clone(), probe);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let pid = creation.view.pid;
        let identity = identities.lock().unwrap().get(&pid).copied().unwrap();

        // A restarted daemon: a fresh service over the same durable store
        // whose probe still observes the recorded identity (the process may
        // even still run) but which owns NO live pty handle.
        let restarted = TerminalService::detached(
            manager.clone(),
            Arc::new(move |probed: u32| (probed == pid).then_some(identity)),
        );
        let report = restarted.recover_all().unwrap();
        assert_eq!(report.lost, vec![terminal_id.clone()]);
        let rows = restarted.list(&sid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state_tag(), "lost");
        assert!(!rows[0].alive);
        assert!(rows[0].detail.contains("unreachable"), "{}", rows[0].detail);

        // I/O is refused typed (Lost), never pid-signalled.
        match restarted.input(&sid, &terminal_id, b"x") {
            Err(TerminalServiceError::Lost { .. }) => {}
            other => panic!("expected typed Lost refusal, got {other:?}"),
        }
        match restarted.resize(&sid, &terminal_id, 10, 10) {
            Err(TerminalServiceError::Lost { .. }) => {}
            other => panic!("expected typed Lost refusal, got {other:?}"),
        }
        match restarted.kill(&sid, &terminal_id, "hostile") {
            Err(TerminalServiceError::Lost { .. }) => {}
            other => panic!("expected typed Lost refusal, got {other:?}"),
        }

        // The stale row is reconciled exactly once, typed.
        let view = restarted
            .reconcile(
                &sid,
                &terminal_id,
                TerminalReconcileDisposition::Collected,
                None,
            )
            .unwrap();
        assert_eq!(view.state_tag(), "reconciled");
        assert!(matches!(
            restarted.reconcile(
                &sid,
                &terminal_id,
                TerminalReconcileDisposition::Killed,
                None
            ),
            Err(TerminalServiceError::State { .. })
        ));
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Lost,
                TerminalEventKind::Reconciled
            ]
        );
    }

    #[test]
    fn recycled_pid_identity_mismatch_is_refused_and_never_adopted() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-recycled");
        let (probe, identities) = recording_probe();
        let service = TerminalService::with_identity_probe(manager.clone(), probe);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let pid = creation.view.pid;
        let recorded = identities.lock().unwrap().get(&pid).copied().unwrap();

        // The restarted daemon probes the same pid with a DIFFERENT start
        // marker: the pid was recycled by an unrelated process.
        let recycled_marker = recorded + 1_000_000;
        let restarted = TerminalService::detached(
            manager.clone(),
            Arc::new(move |probed: u32| (probed == pid).then_some(recycled_marker)),
        );
        let report = restarted.recover_all().unwrap();
        assert_eq!(report.lost, vec![terminal_id.clone()]);
        let rows = restarted.list(&sid).unwrap();
        assert_eq!(rows[0].state_tag(), "lost");
        assert!(rows[0].detail.contains("recycled"), "{}", rows[0].detail);

        let before = rows.len();
        let refused = restarted.reconcile(
            &sid,
            &terminal_id,
            TerminalReconcileDisposition::Killed,
            Some(ProcessIdentity {
                pid,
                start_time_ms: recycled_marker,
            }),
        );
        match refused {
            Err(TerminalServiceError::IdentityMismatch { .. }) => {}
            other => panic!("expected IdentityMismatch refusal, got {other:?}"),
        }
        // Nothing was journaled: the row is still Lost.
        assert_eq!(restarted.list(&sid).unwrap().len(), before);
        assert_eq!(restarted.list(&sid).unwrap()[0].state_tag(), "lost");
    }

    #[test]
    fn concurrent_kill_race_journals_exactly_one_terminal_sequence() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-race");
        let (probe, _map) = recording_probe();
        let service = TerminalService::with_identity_probe(manager.clone(), probe);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();

        let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shared_handle = Arc::new(creation.handle);
        let mut threads = Vec::new();
        for index in 0..8 {
            let service = Arc::clone(&service);
            let sid = sid.clone();
            let terminal_id = terminal_id.clone();
            let rounds = Arc::clone(&rounds);
            let shared_handle = Arc::clone(&shared_handle);
            threads.push(std::thread::spawn(move || {
                // Both the service path and the handle path race the same
                // row: exactly one terminal transition may survive.
                if index % 2 == 0 {
                    let _ = service.kill(&sid, &terminal_id, "race");
                } else {
                    let _ = shared_handle.kill();
                }
                rounds.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(rounds.load(Ordering::SeqCst), 8);
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Killed
            ],
            "exactly one terminal transition survives the race"
        );
    }

    #[test]
    fn restart_equality_no_in_memory_cache_can_diverge_from_the_durable_rows() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-equality");
        let (probe, _map) = recording_probe();
        let service = TerminalService::with_identity_probe(manager.clone(), probe);
        let Some(first) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let Some(second) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let first_id = first.handle.terminal_id().to_string();
        let second_id = second.handle.terminal_id().to_string();
        assert!(service.kill(&sid, &first_id, "equality").unwrap());

        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let raw_fold = handle.ledger_terminal_rows(None).unwrap();
        let live_views = service.list(&sid).unwrap();
        assert_eq!(live_views.len(), 2);
        // The cache equals the durable fold, row by row.
        for view in &live_views {
            let last = raw_fold
                .iter()
                .rfind(|record| record.row.terminal_id == view.terminal_id)
                .expect("durable row");
            let first = raw_fold
                .iter()
                .find(|record| record.row.terminal_id == view.terminal_id)
                .expect("durable row");
            assert_eq!(view.state, last.kind);
            assert_eq!(view.pid, last.row.pid);
            assert_eq!(view.start_time_ms, last.row.start_time_ms);
            assert_eq!(view.updated_ms, last.row.at_ms);
            assert_eq!(view.spawned_ms, first.row.at_ms);
        }

        // A fresh service (restart) rebuilds the same fold; only the
        // running -> lost recovery transition may differ.
        let restarted = TerminalService::detached(manager.clone(), Arc::new(|_pid: u32| None));
        restarted.recover_all().unwrap();
        let after = restarted.list(&sid).unwrap();
        assert_eq!(after.len(), 2);
        for view in &after {
            if view.terminal_id == second_id {
                assert_eq!(view.state_tag(), "lost");
            } else {
                assert_eq!(view.state_tag(), "killed");
            }
        }
        // Idempotent: a second recovery scan journals nothing new.
        let raw_after = handle.ledger_terminal_rows(None).unwrap().len();
        restarted.recover_all().unwrap();
        assert_eq!(handle.ledger_terminal_rows(None).unwrap().len(), raw_after);
        assert_eq!(restarted.list(&sid).unwrap(), after);
    }

    #[test]
    fn reconciliation_of_a_live_row_is_refused_typed() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-reconcile-live");
        let (probe, _map) = recording_probe();
        let service = TerminalService::with_identity_probe(manager.clone(), probe);
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let refused = service.reconcile(
            &sid,
            &terminal_id,
            TerminalReconcileDisposition::Killed,
            None,
        );
        assert!(matches!(refused, Err(TerminalServiceError::State { .. })));
        let _ = service.kill(&sid, &terminal_id, "cleanup");
    }

    #[test]
    fn unverified_identity_is_never_adopted_by_pid_alone() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-unverified");
        // The spawn probe observes nothing: the durable row records no
        // verifiable identity.
        let service =
            TerminalService::with_identity_probe(manager.clone(), Arc::new(|_pid: u32| None));
        let Some(creation) = spawn_or_skip(&service, &sid, &spawn_request("/bin/sleep", &["30"]))
        else {
            return;
        };
        assert_eq!(creation.view.start_time_ms, 0);
        // A restarted daemon whose probe WOULD observe the pid must still
        // refuse to adopt the row: no recorded identity = unverifiable.
        let seen = Arc::new(AtomicBool::new(false));
        let seen2 = Arc::clone(&seen);
        let pid = creation.view.pid;
        let restarted = TerminalService::detached(
            manager.clone(),
            Arc::new(move |probed: u32| {
                if probed == pid {
                    seen2.store(true, Ordering::SeqCst);
                    Some(42)
                } else {
                    None
                }
            }),
        );
        restarted.recover_all().unwrap();
        assert!(seen.load(Ordering::SeqCst), "the pid was probed");
        let rows = restarted.list(&sid).unwrap();
        assert_eq!(rows[0].state_tag(), "lost");
        assert!(rows[0].detail.contains("unverified"), "{}", rows[0].detail);
    }

    #[test]
    fn exited_terminal_is_swept_and_journals_exited_exactly_once() {
        let (_dir, manager) = manager();
        let sid = session(&manager, "terminal-exit");
        let (probe, _map) = recording_probe();
        let service = TerminalService::with_identity_probe(manager.clone(), probe);
        let Some(creation) =
            spawn_or_skip(&service, &sid, &spawn_request("/bin/sh", &["-c", "exit 0"]))
        else {
            return;
        };
        let terminal_id = creation.handle.terminal_id().to_string();
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let rows = service.list(&sid).unwrap();
            if rows[0].state_tag() == "exited" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the exited child was never swept: {rows:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!creation.handle.is_alive());
        assert_eq!(
            terminal_kinds(&handle),
            vec![
                TerminalEventKind::Created,
                TerminalEventKind::Running,
                TerminalEventKind::Exited
            ],
            "exactly one exited row"
        );
        // A second sweep journals nothing.
        let _ = service.list(&sid).unwrap();
        assert_eq!(terminal_kinds(&handle).len(), 3);
        // I/O on an exited row is a typed state refusal.
        match service.input(&sid, &terminal_id, b"x") {
            Err(TerminalServiceError::State { .. }) => {}
            other => panic!("expected typed state refusal, got {other:?}"),
        }
        // An exited row is already terminal: a kill is an idempotent no-op.
        assert!(!service.kill(&sid, &terminal_id, "late kill").unwrap());
        assert_eq!(terminal_kinds(&handle).len(), 3);
    }
}
