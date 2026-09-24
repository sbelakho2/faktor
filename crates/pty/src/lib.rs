//! Interactive terminal support for Faktor, on both unix and Windows.
//!
//! Unix: `Pty::spawn` creates a real pseudo-terminal (posix_openpt/
//! grantpt/unlockpt/ptsname), attaches the child's stdio to the slave side
//! with a controlling terminal (setsid + TIOCSCTTY), and exposes the master
//! side: write stdin, resize the window, snapshot/drain output, close.
//!
//! Windows: the same API is backed by a real ConPTY session
//! (`CreatePseudoConsole` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`); see
//! [`windows`] for the exact construction. Every ConPTY child is assigned to
//! a `faktor-winjob` Job created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! before `Pty::spawn` returns (spawn suspended → assign → resume), so
//! closing the Pty (or the session/daemon owning it) terminates the whole
//! child tree through the OS; no taskkill-based guarantee exists here.
//!
//! Both backends share: a bounded output ring (drop-oldest bytes; a child
//! can never deadlock on a full pipe and memory stays bounded regardless of
//! output volume), a background reader thread, and pre-spawn config
//! validation ([`validation`]) that rejects hostile config (NUL bytes,
//! oversized fields) identically on every platform before any OS call.
//!
//! Unix zero-orphans hardening: because a PTY child `setsid()`s out of the
//! daemon's process group, a daemon crash would otherwise leave its whole
//! session alive. Every unix `Pty` therefore forks a [`guardian`] process
//! holding a control pipe plus the child's start-time-verified identity: on
//! daemon death (pipe EOF) it SIGKILLs the recorded group, refuses when the
//! pid was recycled, and exits without killing when the group was already
//! reaped; the durable [`guardian::TerminalLedger`] lets a restarted daemon
//! reconcile lost terminals typed as [`guardian::TerminalLost`] instead of
//! trusting a pid. On Linux the child also sets `PR_SET_PDEATHSIG` as
//! defense-in-depth (in the unix backend's pre-exec hook).

mod ring;

mod validation;

/// THE child-environment authority shared with the supervised spawn paths
/// ([`faktor_core::command::EnvSpec`]): [`PtyConfig::env`] always clears the
/// inherited environment and applies the resolved spec — no PTY child ever
/// inherits the daemon's full environment, and the deny-set removes
/// configured secret names on every backend.
pub use faktor_core::command::EnvSpec;

// Spawn confinement: the spawn AUTHORITY (the terminal execution policy)
// decides what OS-level confinement a PTY child runs under; this layer only
// transports it onto the child command. See `SpawnConfinement` and
// `Pty::spawn_confined` — on unix the authority's hook is installed before
// exec; on Windows it is refused typed (never faked).

/// Pure Win32 mapping helpers (error tables, COORD geometry bounds, command
/// line quoting, env block layout). Windows-only by nature, but compiled on
/// unix in the test build so the adversarial tests in it run everywhere.
#[cfg(any(windows, test))]
mod win_common;

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

/// Daemon-death guardian + durable terminal-identity ledger (Unix): a forked
/// tiny process holds the PTY child's process group id and a start-time
/// verified identity, SIGKILLs the group if the daemon dies, and refuses when
/// the recorded pid was recycled. See the module docs for the exact EOF
/// protocol and exit codes.
#[cfg(unix)]
pub mod guardian;

#[cfg(unix)]
pub use unix::Pty;

#[cfg(windows)]
pub use windows::Pty;

/// A caller-supplied, OS-level confinement plan for one PTY child (the
/// spawn-authority seam; audit P0-39). The AUTHORITY decides what
/// confinement exists and supplies the install hook ([`PtyConfig`] stays
/// policy-free); this layer only installs it on the exact command it
/// spawns, via [`Pty::spawn_confined`]. The hook receives the built child
/// command before spawn and installs platform confinement on it (e.g. a
/// pre-exec network-namespace hook); a failure it causes refuses the spawn
/// before exec, so a child NEVER execs unconfined. On platforms whose
/// process API cannot install pre-exec hooks (Windows) a confinement is
/// refused typed, never silently dropped.
#[derive(Clone)]
pub struct SpawnConfinement {
    install: std::sync::Arc<dyn Fn(&mut std::process::Command) + Send + Sync>,
}

impl SpawnConfinement {
    /// Wrap one install hook. It runs on the spawning thread and receives
    /// the child command; the confinement it installs is enforced by the
    /// OS in the forked child before exec.
    pub fn new(install: std::sync::Arc<dyn Fn(&mut std::process::Command) + Send + Sync>) -> Self {
        Self { install }
    }

    pub(crate) fn install(&self, cmd: &mut std::process::Command) {
        (self.install)(cmd)
    }
}

impl std::fmt::Debug for SpawnConfinement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnConfinement").finish_non_exhaustive()
    }
}

/// Spawn configuration for [`Pty`].
///
/// Bounds (enforced pre-spawn on every platform by `validation`): the
/// command must be non-empty and free of NUL bytes; individual fields and
/// the assembled command line are capped; env entries must be NUL-free.
/// `rows`/`cols` are the initial terminal size — on Windows they must fit
/// the ConPTY `COORD` range (1..=32767) and are validated before any Win32
/// call, so a config that cannot be honored errors at spawn instead of
/// silently truncating.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PtyConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// THE environment authority ([`EnvSpec`]): every backend env-clears and
    /// applies the resolved spec — the identical builder the supervisor
    /// uses. The default is the safe platform baseline (PATH/HOME/platform
    /// bits), never the daemon's full environment.
    pub env: EnvSpec,
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: vec![],
            cwd: None,
            env: EnvSpec::default_baseline(),
            rows: 24,
            cols: 80,
        }
    }
}
