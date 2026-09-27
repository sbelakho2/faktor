//! faktor-terminal — process supervision (spec §22, §23).
//!
//! No orphans: every child process has a runtime owner; kill targets the
//! whole process group (Unix) or Job Object (Windows). Output is bounded:
//! a 200-line ring buffer live, with overflow spilling to a CAS artifact —
//! a 300MB log never becomes a 300MB RAM object. Blocking pipe reads live
//! on dedicated reader threads so they can never stall the async loop.
//!
//! Network isolation is per-spawn and fail-closed (audit 4/28/35-39):
//! [`NetworkIsolation::DenyAll`] on a [`SpawnConfig`] demands OS-level
//! network denial — on Linux the child is forked into a FRESH network
//! namespace (`unshare(CLONE_NEWNET)` pre-exec; loopback is left DOWN —
//! an empty netns is adequate, nothing is brought up), and ANY failure to
//! produce that isolated child refuses the spawn with a typed permission
//! error; it NEVER warns and runs unenforced. Platforms without the
//! backend (macOS/windows) refuse a DenyAll request BEFORE spawn. The
//! policy layer DECIDES the requirement
//! (`faktor-sandbox::SandboxGuarantee::Required` →
//! [`NetworkIsolationRequirement::DenyAll`] → `DenyAll` here); this crate
//! ENFORCES it, so there is no preflight platform guessing anywhere.
//! [`platform_network_enforcement`] reports the honest spawn-backend state
//! for diagnostics (Linux is `AppLevel` until one DenyAll spawn proves the
//! unshare path at spawn).
//!
//! This crate owns THE process supervisor for the whole workspace (audit
//! P0-40): git, lsp, mcp, hooks and the CLI daemon all spawn children
//! through [`ProcessSupervisor`]. A bounded live-child ceiling refuses
//! oversize spawns with a typed `Oversized` error before any process
//! exists; dropping the last reference (daemon shutdown) kills every live
//! child. [`ProcessSupervisor::run_sync`] gives synchronous callers (the
//! hook lifecycle) the same deadline/group-kill/bounded-head semantics as
//! [`ProcessSupervisor::run`] without a tokio context.
//!
//! Pid-reuse discipline (the terminal twin of faktor-pty's guarded
//! `signal_group`): every child has exactly one reaper, and that reaper
//! publishes the reap under a per-child serial the instant `wait` consumed
//! the child. From then on the kernel may recycle the pid, so every kill
//! path holds the same serial across its `[observe reaped → signal]` pair
//! and signals ONLY while the child is provably unreaped. The unreaped
//! child is the only identity proof that is portable: while it exists the
//! kernel cannot recycle its pid, so `kill(-pid, …)` reaches exactly the
//! owned process group. Once the reaper consumed the child, ALL guarded
//! signals are refused — a surviving process group with that pgid can no
//! longer be distinguished from an unrelated recycled group (a live group
//! only proves the pgid is allocated, not whose it is), so post-consumption
//! signals are suppressed even when a descendant may still exist. Reap,
//! Drop and kill are mutually exclusive through that serial, so no signal
//! can be issued after a `wait` consumed the child and no signal can race a
//! concurrent kill.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(all(test, unix))]
use std::sync::atomic::AtomicU64;

use faktor_core::cancellation::CancellationToken;
use faktor_core::error::Error;
use faktor_core::id::{SessionId, WorkspaceId};

/// The one environment authority for every child (see
/// [`faktor_core::command::EnvSpec`]): process creation ALWAYS clears the
/// inherited environment and applies the resolved spec.
pub use faktor_core::command::EnvSpec;

/// Typed command form + shell selection (see
/// [`faktor_core::command::CommandSpec`]).
pub use faktor_core::command::{CommandSpec, ShellKind};

/// What the sandbox policy demands of the spawn layer. The terminal crate
/// ENFORCES it: [`NetworkIsolation::from`] maps it to the concrete mode and
/// a `DenyAll` spawn either isolates the child or fails closed typed.
pub use faktor_core::command::NetworkIsolationRequirement;

/// The daemon names a supervised Chromium child may inherit (values copied
/// when set): executable resolution plus locale/timezone. Provider keys,
/// tokens, API secrets, `FAKTOR_SERVER_PASSWORD` and proxy passwords are
/// absent by construction (spec §9).
pub const BROWSER_ENV_ALLOWLIST: &[&str] = &["PATH", "LANG", "LC_ALL", "TZ"];

/// THE browser child environment authority (spec §9): an
/// [`EnvSpec::Explicit`] built from [`BROWSER_ENV_ALLOWLIST`] (daemon values
/// copied) plus the caller's exact entries (scratch HOME/TMPDIR/XDG dirs,
/// proxy address). Every name — allowlisted or caller-supplied — is checked
/// against the universal secret deny-set BEFORE spawn: a secret-shaped name
/// is a typed refusal, never a silent drop, so a caller can never believe a
/// value was applied while the deny-set stripped it. `EnvSpec::resolve`
/// applies the deny-set again on the resolved view (defense in depth).
pub fn browser_env_spec(
    exact: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<EnvSpec, Error> {
    use std::ffi::{OsStr, OsString};
    for name in BROWSER_ENV_ALLOWLIST {
        if faktor_core::command::env_name_is_denied(OsStr::new(name)) {
            return Err(Error::permission(format!(
                "browser env allowlist names a denied variable: {name}"
            )));
        }
    }
    let mut entries: Vec<(OsString, OsString)> = BROWSER_ENV_ALLOWLIST
        .iter()
        .map(|name| (OsString::from(*name), OsString::new()))
        .collect();
    for (name, value) in exact {
        if faktor_core::command::env_name_is_denied(&name) {
            return Err(Error::permission(format!(
                "browser env entry names a denied variable: {}",
                name.to_string_lossy()
            )));
        }
        entries.push((name, value));
    }
    Ok(EnvSpec::Explicit(entries))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOwner {
    Session(SessionId),
    /// One verification-check child of a session (the supervisor-backed
    /// check executor): separately killable (`kill_all_for`) without
    /// touching the session's other children, so a session whose turn died
    /// mid-check can reap exactly its verification tree.
    Verification(SessionId),
    /// One supervised Chromium child (faktor-browser, spec §9): scoped by
    /// the connector `source` and the browser `profile` so one profile's
    /// browser tree is separately killable (`kill_all_for`) without
    /// touching any session/workspace child. `profile` is the validated
    /// profile NAME (never a path); `source` is the connector's source id.
    Browser {
        source: String,
        profile: String,
    },
    /// One cold-evidence git/ripgrep child of the index crate's pre-Ready
    /// fallback provider (audit 14/26): separately killable so a session or
    /// workspace teardown never needs to reap (or spare) the whole
    /// workspace's other children. `operation` scopes one cold retrieval
    /// (0 = the whole ladder of the workspace; reserved for per-turn kill
    /// scopes at higher layers).
    IndexCold {
        workspace: WorkspaceId,
        operation: u64,
    },
    Workspace(WorkspaceId),
    Daemon,
}

/// Network isolation requested for one spawned child (audit 4/28/35-39).
/// The policy seam (`faktor-sandbox`) maps a `Required` network guarantee
/// to [`NetworkIsolation::DenyAll`] through
/// [`NetworkIsolationRequirement`]; this crate ENFORCES it or refuses the
/// spawn — never warns and runs unenforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkIsolation {
    /// The child shares the daemon's network namespace (no OS-level
    /// network isolation is requested or applied).
    #[default]
    Inherit,
    /// The child must run with NO network access: on Linux it is placed in
    /// a FRESH network namespace before exec (`unshare(CLONE_NEWNET)`; an
    /// empty netns is adequate — loopback exists but is left DOWN by the
    /// kernel and nothing is brought up, so no TCP/UDP egress can leave
    /// the child). If the kernel/user-namespace setup refuses the unshare,
    /// the spawn FAILS typed — never a warn-and-run downgrade. Platforms
    /// with no backend (macOS/windows) refuse a DenyAll request BEFORE
    /// spawn.
    DenyAll,
    /// The child may reach EXACTLY the given broker endpoint and nothing
    /// else (audit item 8: the Chromium process must not be able to open
    /// raw sockets that bypass the broker). The endpoint must be a
    /// non-zero loopback literal; the browser authority passes the live
    /// broker's address.
    ///
    /// On Linux the child is placed in a dedicated network namespace
    /// before exec: loopback is brought UP and a namespace-side listener on
    /// the endpoint relays every connection across the namespace boundary
    /// to the real broker (and back, for the host-initiated DevTools
    /// control channel). The namespace has no interface and no route other
    /// than loopback, so:
    ///
    /// * `bind()`/`connect()` on `127.0.0.0/8`/`::1` inside the sandbox
    ///   work normally (bind/connect semantics are namespace-local);
    /// * the broker endpoint is reachable because the relay bridge answers
    ///   it and forwards to the daemon-side broker;
    /// * every other destination — including the daemon's own loopback —
    ///   is unreachable, and a raw socket cannot leave the namespace.
    ///
    /// If the namespace cannot be created (EPERM without `CAP_SYS_ADMIN`,
    /// no unprivileged user namespaces, ...), the spawn FAILS typed BEFORE
    /// exec: the child is never run proxy-only-but-unconfined. Platforms
    /// with no BrokerOnly backend (macOS/windows) refuse the request typed
    /// BEFORE spawn; those platforms keep proxy flags as application
    /// configuration only and report the honest app-level state.
    BrokerOnly {
        /// The broker endpoint (loopback literal) the confined child may
        /// reach.
        endpoint: std::net::SocketAddr,
    },
}

impl NetworkIsolation {
    /// True when this is the broker-only confinement mode.
    pub const fn is_broker_only(self) -> bool {
        matches!(self, NetworkIsolation::BrokerOnly { .. })
    }

    /// The stable snake_case tag of this mode (`inherit` | `deny_all` |
    /// `broker_only`): the exact spelling recorded as durable spawn
    /// evidence (the terminal authority's effective execution profile).
    pub const fn as_tag(self) -> &'static str {
        match self {
            NetworkIsolation::Inherit => "inherit",
            NetworkIsolation::DenyAll => "deny_all",
            NetworkIsolation::BrokerOnly { .. } => "broker_only",
        }
    }
}

impl From<NetworkIsolationRequirement> for NetworkIsolation {
    /// The enforcement-side mapping: a policy that requires DenyAll gets a
    /// DenyAll spawn, everything else inherits. There is no third state and
    /// no silent downgrade.
    fn from(requirement: NetworkIsolationRequirement) -> Self {
        match requirement {
            NetworkIsolationRequirement::DenyAll => NetworkIsolation::DenyAll,
            NetworkIsolationRequirement::Inherit => NetworkIsolation::Inherit,
        }
    }
}

/// Honest diagnostic answer from the SPAWN layer to "is OS-level network
/// denial actually applied here?". The sandbox policy carries NO platform
/// probe (capability existence is not enforcement): the only authority on
/// enforcement is this spawn layer, and a `DenyAll` spawn either isolates
/// the child or refuses typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkEnforcement {
    /// Only app-level gates exist in practice: a permitted shell could
    /// still open its own sockets. Linux reports this until the
    /// `unshare(CLONE_NEWNET)` DenyAll or BrokerOnly path has proven itself
    /// active at spawn.
    #[default]
    AppLevel,
    /// The DenyAll backend has PROVEN itself active at spawn: at least one
    /// `NetworkIsolation::DenyAll` spawn succeeded in this process, so the
    /// pre-exec netns path demonstrably works here.
    OsLevel,
    /// The BrokerOnly backend has PROVEN itself active at spawn (at least
    /// one `NetworkIsolation::BrokerOnly` spawn succeeded) and the
    /// stronger full DenyAll path has not: the sandbox namespace confines
    /// the child to the relayed broker endpoint with no external route.
    OsLevelBrokerOnly,
    /// No per-process network-isolation backend exists on this platform
    /// (macOS/windows); a DenyAll or BrokerOnly request fails closed before
    /// spawn.
    Unavailable,
}

impl NetworkEnforcement {
    /// The stable snake_case tag of this enforcement verdict (doctor/health
    /// surfaces print this exact spelling).
    pub const fn as_tag(self) -> &'static str {
        match self {
            NetworkEnforcement::AppLevel => "app_level",
            NetworkEnforcement::OsLevel => "os_level",
            NetworkEnforcement::OsLevelBrokerOnly => "os_level_broker_only",
            NetworkEnforcement::Unavailable => "unavailable",
        }
    }
}

impl std::fmt::Display for NetworkEnforcement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkEnforcement::AppLevel => write!(
                f,
                "app-level only: no spawn backend has proven OS-level network isolation \
                 active at spawn"
            ),
            NetworkEnforcement::OsLevel => write!(
                f,
                "OS-level: the unshare(CLONE_NEWNET) DenyAll backend proved itself active \
                 at spawn"
            ),
            NetworkEnforcement::OsLevelBrokerOnly => write!(
                f,
                "OS-level (broker-only): the BrokerOnly sandbox namespace proved itself \
                 active at spawn — the child reaches only the relayed broker endpoint and \
                 has no external route"
            ),
            NetworkEnforcement::Unavailable => write!(
                f,
                "unavailable: this platform has no per-process network-isolation backend; \
                 DenyAll spawns fail closed"
            ),
        }
    }
}

/// Set once a `NetworkIsolation::DenyAll` spawn has succeeded in this
/// process: the unshare pre-exec path ran without error, so the backend is
/// PROVEN active at spawn. This is the ONLY thing that may move the Linux
/// report off [`NetworkEnforcement::AppLevel`].
#[cfg(target_os = "linux")]
static DENY_ALL_PROVEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set once a `NetworkIsolation::BrokerOnly` spawn has succeeded: the
/// sandbox namespace + relay bridge demonstrably work here. Reported as
/// [`NetworkEnforcement::OsLevelBrokerOnly`] only while the stronger full
/// DenyAll path has not proven itself.
#[cfg(target_os = "linux")]
static BROKER_ONLY_PROVEN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// True when this BUILD has a BrokerOnly backend at all (Linux). This is a
/// compile-time platform fact only, never an enforcement claim: the
/// runtime can still refuse a BrokerOnly spawn typed (e.g. no
/// `CAP_SYS_ADMIN`), and that refusal is never converted into a silent
/// proxy-only downgrade. Callers use it to SELECT the mode; only the spawn
/// layer decides whether it is actually applied.
pub const fn broker_only_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Probe override used by adversarial tests to force the enforcement
/// verdict (the real probe is otherwise read-only; forcing NEVER changes
/// what the spawn code applies — see the forced-lie test).
#[cfg(test)]
static NET_PROBE_OVERRIDE: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

/// What the spawn path of THIS crate actually enforces.
///
/// - **linux**: [`NetworkEnforcement::AppLevel`] until the unshare path is
///   proven active at spawn (one successful [`NetworkIsolation::DenyAll`]
///   spawn flips it to [`NetworkEnforcement::OsLevel`]). Capability
///   existence is NOT proof (audit 4/28/35-39).
/// - **macos/windows**: [`NetworkEnforcement::Unavailable`] — no backend
///   is implemented; DenyAll requests are refused before spawn.
pub fn platform_network_enforcement() -> NetworkEnforcement {
    #[cfg(test)]
    {
        match NET_PROBE_OVERRIDE.load(std::sync::atomic::Ordering::SeqCst) {
            1 => return NetworkEnforcement::AppLevel,
            2 => return NetworkEnforcement::OsLevel,
            3 => return NetworkEnforcement::Unavailable,
            4 => return NetworkEnforcement::OsLevelBrokerOnly,
            _ => {}
        }
    }
    real_platform_network_enforcement()
}

#[cfg(target_os = "linux")]
fn real_platform_network_enforcement() -> NetworkEnforcement {
    if DENY_ALL_PROVEN.load(std::sync::atomic::Ordering::SeqCst) {
        NetworkEnforcement::OsLevel
    } else if BROKER_ONLY_PROVEN.load(std::sync::atomic::Ordering::SeqCst) {
        NetworkEnforcement::OsLevelBrokerOnly
    } else {
        NetworkEnforcement::AppLevel
    }
}

#[cfg(not(target_os = "linux"))]
fn real_platform_network_enforcement() -> NetworkEnforcement {
    NetworkEnforcement::Unavailable
}

/// Set the enforcement probe for tests. `None` restores the real probe.
/// Forcing exists only where backend-absent platforms can test the
/// never-downgrade invariant (linux test builds exercise the REAL backend
/// and never force).
#[cfg(all(test, not(target_os = "linux")))]
fn override_network_probe(v: Option<NetworkEnforcement>) {
    use std::sync::atomic::Ordering;
    NET_PROBE_OVERRIDE.store(
        match v {
            None => -1,
            Some(NetworkEnforcement::AppLevel) => 1,
            Some(NetworkEnforcement::OsLevel) => 2,
            Some(NetworkEnforcement::Unavailable) => 3,
            Some(NetworkEnforcement::OsLevelBrokerOnly) => 4,
        },
        Ordering::SeqCst,
    );
}

/// The linux unshare backend lives behind this module
/// (`crates/terminal/src/sandbox/linux.rs`); every other platform has no
/// backend module at all (DenyAll and BrokerOnly are refused before spawn
/// there).
#[cfg(target_os = "linux")]
#[path = "sandbox/linux.rs"]
mod sandbox;

/// The live sandbox bridge of one BrokerOnly spawn: the child's dedicated
/// network namespace plus the relay that makes exactly the broker endpoint
/// reachable through it (see [`NetworkIsolation::BrokerOnly`]).
///
/// The bridge is created BEFORE the child exists and is owned by the
/// supervisor's registry row; dropping it releases the namespace and every
/// relay. `expose_loopback_port` additionally opens a host-side loopback
/// listener that relays INTO the namespace, which is how the browser
/// authority reaches Chromium's announced DevTools endpoint without
/// opening the sandbox.
pub struct BrokerOnlyBridge {
    endpoint: std::net::SocketAddr,
    #[cfg(target_os = "linux")]
    inner: sandbox::Inner,
}

impl std::fmt::Debug for BrokerOnlyBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerOnlyBridge")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl BrokerOnlyBridge {
    /// The broker endpoint the confined child may reach (and nothing else).
    pub fn endpoint(&self) -> std::net::SocketAddr {
        self.endpoint
    }

    /// Expose one in-sandbox loopback port on the host's loopback. Returns
    /// the host port the daemon dials; every connection to it is relayed to
    /// `127.0.0.1:<in_sandbox_port>` INSIDE the sandbox namespace. Used for
    /// the Chromium DevTools control channel (never for egress: the relayed
    /// port is chosen by the confined child and the direction is
    /// host-initiated).
    pub fn expose_loopback_port(&self, in_sandbox_port: u16) -> Result<u16, Error> {
        #[cfg(target_os = "linux")]
        {
            self.inner
                .expose_loopback_port(in_sandbox_port)
                .map_err(|e| {
                    Error::internal(format!(
                        "BrokerOnly DevTools exposure of in-sandbox port {in_sandbox_port} \
                         failed: {e}"
                    ))
                })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = in_sandbox_port;
            Err(Error::permission(format!(
                "{BROKER_ONLY_REFUSAL_PREFIX}: this platform has no BrokerOnly backend"
            )))
        }
    }

    /// The namespace fd the pre-exec `setns` hook captures (Linux only; the
    /// bridge keeps the fd open for its whole lifetime).
    #[cfg(target_os = "linux")]
    fn ns_fd(&self) -> i32 {
        self.inner.ns_fd()
    }
}

/// Process-tree resource budgets: the authorization side of the terminal
/// authority's `TerminalBudgets`. Linux cgroup v2 (with `prlimit`
/// fallback), Windows Job Object limits, unix pre-exec rlimits and the wall
/// watchdog — each limit's EFFECTIVE state is reported (`Enforced` /
/// `Degraded` / `Unsupported` / `NotRequested`), never silently claimed.
pub mod budget;

/// The budget types the terminal authority persists and enforces.
pub use budget::{
    BudgetEnforcement, BudgetPlatform, BudgetRefusal, LimitState, TreeBudgetGuard, TreeBudgets,
    WallWatchdog,
};

#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// THE child-environment authority ([`EnvSpec`]): process creation
    /// always `env_clear()`s and applies this resolved spec. The default is
    /// the safe platform baseline (PATH/HOME/platform bits) — never the
    /// daemon's full environment.
    pub env: EnvSpec,
    pub owner: ProcessOwner,
    /// Capture stdout+stderr into the ring buffer / artifact.
    pub capture: bool,
    /// Durable artifact cap in bytes (default 100MB, clamped to the global
    /// 300MB ceiling).
    pub artifact_max: usize,
    /// OS-level network isolation requested for this child
    /// ([`NetworkIsolation::Inherit`] by default; see the enum for the
    /// fail-closed `DenyAll` semantics). Derive it from the policy with
    /// [`NetworkIsolation::from(NetworkIsolationRequirement::from(guarantee))`].
    pub network_isolation: NetworkIsolation,
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            cmd: String::new(),
            args: vec![],
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            env: EnvSpec::default_baseline(),
            owner: ProcessOwner::Daemon,
            capture: true,
            artifact_max: 100 * 1024 * 1024,
            network_isolation: NetworkIsolation::Inherit,
        }
    }
}

/// A spawned process whose pipes are handed to the caller.
pub struct SpawnedProcess {
    pub child_pid: u32,
    pub stdin: std::process::ChildStdin,
    pub stdout: std::process::ChildStdout,
    pub stderr: std::process::ChildStderr,
    /// The live sandbox bridge when the child was spawned under
    /// [`NetworkIsolation::BrokerOnly`] (Linux); `None` for every other
    /// mode. The supervisor's registry row also owns it — this handle only
    /// adds the reverse exposure API (DevTools control channel).
    pub network_bridge: Option<Arc<BrokerOnlyBridge>>,
}

/// Bounded-head result of one synchronous supervised run
/// ([`ProcessSupervisor::run_sync`]): per-stream heads capped at the
/// requested byte caps with truncation flags, the exit code, and a
/// `timed_out` flag — a timed-out run's partial output is forensics only
/// (the caller's failure policy decides, never partial stdout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRunOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_head: String,
    pub stderr_head: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildHandle {
    pub id: u64,
    pub pid: u32,
    pub owner: ProcessOwner,
    pub started_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaped {
    pub id: u64,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub owner: ProcessOwner,
}

/// Bounded command output: excerpt (last 200 lines + exit code) and an
/// optional durable artifact reference for the stream. When the stream
/// exceeds the effective per-command artifact cap, the artifact holds the
/// FIRST `cap` bytes, `artifact_truncated` is set, and the ring excerpt
/// carries the readable tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub excerpt: String,
    pub exit_code: Option<i32>,
    /// CAS reference to the durable artifact (never in RAM).
    pub artifact: Option<String>,
    pub slice_hint: Option<String>,
    pub ring_lines: usize,
    /// True when the stream exceeded the effective artifact cap and tail
    /// bytes were dropped from the durable artifact.
    pub artifact_truncated: bool,
}

const RING_LINES: usize = 200;

/// Default hard ceiling on LIVE supervised children (bounded registry,
/// audit P0-40). Any spawn attempt past the ceiling is refused with a
/// typed `Oversized` error BEFORE a process exists. Exited-but-unreaped
/// entries do not count; only children that have not exited yet.
pub const DEFAULT_MAX_LIVE_CHILDREN: usize = 128;

/// Absolute ceiling on the durable artifact spool (disk), whatever
/// `SpawnConfig::artifact_max` requests: the effective per-command cap is
/// `min(artifact_max, GLOBAL_HARD_MAX)`. Past the effective cap the ring
/// keeps the tail; the artifact holds the first effective-cap bytes.
const GLOBAL_HARD_MAX: u64 = 300 * 1024 * 1024;
const MAX_EXCERPT_BYTES: usize = 64 * 1024;

/// Clamp a configured artifact cap to the global ceiling.
fn effective_artifact_max(configured: usize) -> usize {
    configured.min(GLOBAL_HARD_MAX as usize)
}

/// The materialized cmd script behind a spawn, when the configuration is
/// exactly the core lowering's `cmd.exe /d /c <reserved temp path>` form
/// (see `faktor_core::command::CMD_SCRIPT_PREFIX`). The snippet is written
/// to a direct-use temp file so cmd's `/C` quote handling cannot mangle it;
/// the supervisor — which owns the run — deletes that file once the child
/// has exited. Only the reserved prefix under the system temp dir matches,
/// so a caller-supplied `cmd /c <script>` is never removed.
fn materialized_cmd_script(cfg: &SpawnConfig) -> Option<PathBuf> {
    let program = std::path::Path::new(&cfg.cmd)
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    if program != "cmd" && program != "cmd.exe" {
        return None;
    }
    let at = cfg
        .args
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case("/c"))?;
    let path = PathBuf::from(cfg.args.get(at + 1)?.as_str());
    let name = path.file_name()?.to_string_lossy();
    if !name.starts_with(faktor_core::command::CMD_SCRIPT_PREFIX) || !name.ends_with(".cmd") {
        return None;
    }
    if path.parent() != Some(std::env::temp_dir().as_path()) {
        return None;
    }
    Some(path)
}

/// Deletes a materialized cmd script exactly once the controlling run has
/// finished. The guard drops on every return path (success, timeout,
/// cancellation, refused/failed spawn), always after the child was reaped;
/// the detached spawn paths hand the path to their reaper thread instead,
/// so the script stays readable for as long as cmd runs.
struct CmdScriptGuard(Option<PathBuf>);

impl CmdScriptGuard {
    fn disarm(mut self) -> Option<PathBuf> {
        self.0.take()
    }
}

impl Drop for CmdScriptGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Bound (ms) on the post-exit drain in [`ProcessSupervisor::run`]: the
/// existence of an unrelated descendant holding an inherited descriptor must
/// never control completion of the parent command.
const POST_EXIT_DRAIN_MS: u64 = 500;

/// Per-stream post-exit drain bound (ms) in [`ProcessSupervisor::run_sync`];
/// a descendant still holding a pipe past this bound is group-killed (a
/// pipe-holding descendant proves the owned group is alive, so the kill can
/// never hit a recycled process-group id — audit round 12).
const SYNC_DRAIN_MS: u64 = 600;

/// SIGTERM→SIGKILL grace for the run_sync group kills.
const SYNC_KILL_GRACE_MS: u64 = 1200;

struct ChildState {
    pid: u32,
    owner: ProcessOwner,
    started_ms: i64,
    exited: Option<Option<i32>>,
    /// The per-child reap/signal serial ([`ReapState`]): the single reaper
    /// publishes `reaped` under it and every kill path observes `reaped`
    /// under it, so no signal can reference a consumed pid or race a
    /// concurrent kill.
    reap: Arc<ReapState>,
    /// The per-child containment of this row (Windows: its own
    /// `KILL_ON_JOB_CLOSE` job; off Windows: the process group needs no
    /// stored authority). Read on Windows only (terminate/reap/job close);
    /// off Windows it is deliberately inert.
    #[cfg_attr(not(windows), allow(dead_code))]
    containment: ChildContainment,
    /// The live BrokerOnly sandbox bridge of this child (None for every
    /// other isolation mode). The row owns it: dropping the row (reap,
    /// supervisor teardown) releases the sandbox namespace and every relay,
    /// so the bridge can never outlive its child's registry entry.
    #[allow(dead_code)] // ownership is the point; there is no read path
    broker_bridge: Option<Arc<BrokerOnlyBridge>>,
}

/// Per-child containment: on Windows its OWN `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
/// Job Object, created BEFORE the process exists. Assignment happens while the
/// child is `CREATE_SUSPENDED`, so the child — and every descendant it will
/// ever create — is a job member before it can execute. Terminating or
/// closing this job is the OS-enumerated tree kill (no pid walk, no
/// taskkill); off Windows the process group is the tree authority and this
/// carries nothing.
#[cfg(windows)]
struct ChildContainment {
    job: JobGuard,
}

#[cfg(not(windows))]
struct ChildContainment;

impl ChildContainment {
    /// PRIMARY tree termination (Windows): the kernel iterates the job's
    /// process list, so children of members are members — the whole tree.
    #[cfg(windows)]
    fn terminate(&self) {
        self.job.terminate();
    }
}

/// Per-child reap publication + signal serialization (the terminal twin of
/// faktor-pty's guarded `signal_group`, closing the same pid-reuse class).
///
/// The single reaper for a child publishes `reaped` under
/// [`ReapState::serial`] the instant its `wait` consumed the child: from
/// that moment the kernel may recycle the pid, so a later `kill(-pid, …)`
/// could signal an unrelated process group that inherited the recycled id.
/// Every kill path takes the same serial across its `[observe reaped →
/// signal]` pair and issues a signal only while the child is provably
/// unreaped (`reaped == false`): the kernel cannot recycle the pid of a
/// child its parent has not reaped, so the signal then addresses exactly the
/// owned process group. Once the child was consumed, every guarded signal is
/// REFUSED — a still-existing group with that pgid is not identity evidence
/// (the pgid may have been recycled by an unrelated group after the whole
/// owned group died), and no portable pidfd-equivalent handle exists here
/// that could prove otherwise. The cost of the refusal is bounded: a
/// descendant that outlives a consumed leader is not signalled anymore, and
/// the caller's bounded drain/wait bounds still apply.
struct ReapState {
    /// Set by the single reaper the instant `wait` consumed the child.
    reaped: AtomicBool,
    /// Serializes the reaper's `[waitpid → publish reaped]` pair against
    /// every kill path's `[observe reaped → signal]` pair, so Drop/reap/kill
    /// are mutually exclusive and no signal can race a concurrent kill.
    serial: Mutex<()>,
    /// Test-only fault-injection counter: how many group signals this
    /// child's guarded paths actually attempted. The adversarial tests pin
    /// the `[waitpid → publish]` window and prove the counter stays frozen
    /// once the child is reaped. Never compiled into production builds.
    #[cfg(all(test, unix))]
    signal_attempts: AtomicU64,
}

impl ReapState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            reaped: AtomicBool::new(false),
            serial: Mutex::new(()),
            #[cfg(all(test, unix))]
            signal_attempts: AtomicU64::new(0),
        })
    }

    /// True once the single reaper consumed the child.
    fn reaped(&self) -> bool {
        self.reaped.load(Ordering::SeqCst)
    }

    /// Publish the reap under the signal serial: after this returns, no
    /// guarded signal may ever reference the pid again.
    fn publish_reaped(&self, pid: u32) {
        let _serial = self
            .serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        #[cfg(all(test, unix))]
        reap_injection::fire(pid);
        #[cfg(not(all(test, unix)))]
        let _ = pid;
        self.reaped.store(true, Ordering::SeqCst);
    }

    /// THE guarded signal issuance for one registered unix child: hold the
    /// serial across `[observe reaped → signal]` and refuse every signal
    /// once the reaper consumed the child (there is no identity proof left
    /// that the negative pgid still names the owned group). Never signal
    /// pid 0. Returns whether a signal was actually attempted.
    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn try_signal_group(&self, pid: u32, signal: libc::c_int) -> bool {
        if pid == 0 {
            return false;
        }
        let _serial = self
            .serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.reaped() {
            return false;
        }
        #[cfg(all(test, unix))]
        self.signal_attempts.fetch_add(1, Ordering::SeqCst);
        // SAFETY: `pid` is the group leader of a supervisor-spawned child
        // (non-zero checked above, pids fit `pid_t`), so the negative id
        // addresses exactly that process group; the serial plus the `reaped`
        // observation guarantee the child is still unreaped, so the kernel
        // cannot have recycled its pid.
        unsafe {
            libc::kill(-(pid as i32), signal);
        }
        true
    }

    /// Last-resort RAII consumption for a dropped `run()` future: SIGKILL
    /// the owned group and BLOCK until the direct child is actually consumed
    /// (`waitpid`), all under the reap serial, then publish `reaped`. This
    /// keeps the published `reaped` truthful — unlike a bare publication, no
    /// later kill path can be suppressed for a child that was never waited
    /// on, and the pid cannot be signalled by a guarded path between the
    /// kill and the consumption. `waitpid` returns immediately for a SIGKILLed
    /// child (and `ECHILD` when another authority already consumed it).
    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn consume_killed_child(&self, pid: u32) {
        let _serial = self
            .serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.reaped() {
            return;
        }
        if pid != 0 {
            #[cfg(all(test, unix))]
            self.signal_attempts.fetch_add(1, Ordering::SeqCst);
            // SAFETY: `pid` is the leader of a supervisor-spawned group
            // (unreaped here, so the kernel cannot recycle it); SIGKILL is
            // the documented last-resort for a dropped run future.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let mut status: libc::c_int = 0;
            loop {
                // SAFETY: `pid` is this process's own direct child (or was
                // already consumed elsewhere, reported as ECHILD); a
                // blocking wait on a SIGKILLed child returns immediately.
                let r = unsafe { libc::waitpid(pid as i32, &mut status, 0) };
                if r == -1 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                }
                break;
            }
        }
        self.reaped.store(true, Ordering::SeqCst);
    }

    #[cfg(all(test, unix))]
    fn attempts(&self) -> u64 {
        self.signal_attempts.load(Ordering::SeqCst)
    }
}

/// Last-resort RAII tree cleanup for one async `run()` future. Declared
/// AFTER the spawned child, so it drops BEFORE the (kill-on-drop disabled)
/// tokio child handle: a future dropped by an outer timeout/unwind has not
/// reaped its child, so this SIGKILLs the owned group WHILE the child is
/// still unreaped (its pid cannot be recycled yet), then blocks until the
/// child is consumed and publishes the reap — the published `reaped` is
/// therefore never a claim without a `waitpid`, and no later Drop/kill path
/// can ever signal the consumed pid. Every deliberate run return path reaps
/// first, so the guard is inert there.
struct RunGroupGuard {
    reap: Arc<ReapState>,
    pid: u32,
}

impl Drop for RunGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            self.reap.consume_killed_child(self.pid);
        }
        #[cfg(not(unix))]
        let _ = (&self.reap, self.pid);
    }
}

/// Test-only fault-injection seam for the reap-publication window (mirrors
/// faktor-pty's). The reaper invokes the hook registered for its child pid
/// while holding the reap serial, AFTER `wait` consumed the child and BEFORE
/// the `reaped` flag is published. A test can therefore pin the exact
/// dangerous interleaving — "the child is reaped (its pid is now
/// recyclable) but no kill path has observed it" — and prove a concurrent
/// kill or Drop blocks on the serial and never signals the stale pid. Never
/// compiled into production builds.
#[cfg(all(test, unix))]
pub(crate) mod reap_injection {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    static HOOKS: OnceLock<Mutex<HashMap<u32, Hook>>> = OnceLock::new();

    fn hooks() -> &'static Mutex<HashMap<u32, Hook>> {
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn register(pid: u32, hook: Hook) {
        hooks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pid, hook);
    }

    pub(crate) fn clear(pid: u32) {
        hooks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&pid);
    }

    pub(crate) fn fire(pid: u32) {
        let hook = hooks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&pid);
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Test-only fault-injection seam for the negative-pgid group probe
/// (`group_gone`). It simulates the exact state the guarded refusal exists
/// for: our child was consumed (its pid is recyclable) while the probe STILL
/// reports a live group with that pgid — the recycled-group state that makes
/// the old "a live group keeps its pgid allocated" proof unsound. A test
/// registers `true`/`false` as the simulated "group gone" answer for one
/// pgid. Never compiled into production builds.
#[cfg(all(test, unix))]
pub(crate) mod group_probe_injection {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static OVERRIDES: OnceLock<Mutex<HashMap<u32, bool>>> = OnceLock::new();

    fn overrides() -> &'static Mutex<HashMap<u32, bool>> {
        OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn register(pgid: u32, group_gone: bool) {
        overrides()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pgid, group_gone);
    }

    pub(crate) fn clear(pgid: u32) {
        overrides()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&pgid);
    }

    pub(crate) fn probe(pgid: u32) -> Option<bool> {
        overrides()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&pgid)
            .copied()
    }
}

/// Prefix of the capture spill files the CURRENT writer mints:
/// `faktor-spill-<pid>-<uuid>` in the system temp dir.
pub const FAKTOR_SPILL_PREFIX: &str = "faktor-spill-";

/// Prefix of pre-migration spill residue (`kp-spill-*`). Recognized and
/// cleaned for compatibility, never written.
pub const LEGACY_KP_SPILL_PREFIX: &str = "kp-spill-";

/// Prefix of the current per-process supervisor roots:
/// `faktor-supervisor-shared-<pid>`.
pub const FAKTOR_SUPERVISOR_PREFIX: &str = "faktor-supervisor-";

/// Prefix of pre-migration supervisor roots (`kp-supervisor-*`). Recognized
/// and cleaned for compatibility, never written.
pub const LEGACY_KP_SUPERVISOR_PREFIX: &str = "kp-supervisor-";

/// True when `name` is an internal capture spill file under either spelling.
///
/// Compatibility intent: cleanup must accept the legacy spelling so crash
/// residue from an older release in the shared system temp dir is never
/// mistaken for user files. Retirement note: [`LEGACY_KP_SPILL_PREFIX`] can
/// be deleted once no supported installation can still leave `kp-spill-*`
/// residue.
pub fn is_internal_spill_name(name: &str) -> bool {
    name.starts_with(FAKTOR_SPILL_PREFIX) || name.starts_with(LEGACY_KP_SPILL_PREFIX)
}

/// True when `name` is an internal supervisor root under either spelling.
/// Same compatibility/retirement contract as [`is_internal_spill_name`].
pub fn is_internal_supervisor_name(name: &str) -> bool {
    name.starts_with(FAKTOR_SUPERVISOR_PREFIX) || name.starts_with(LEGACY_KP_SUPERVISOR_PREFIX)
}

/// The pid of a `*-supervisor-shared-<pid>` root, when the name carries one.
fn supervisor_root_pid(name: &str) -> Option<u32> {
    let rest = name
        .strip_prefix(FAKTOR_SUPERVISOR_PREFIX)
        .or_else(|| name.strip_prefix(LEGACY_KP_SUPERVISOR_PREFIX))?;
    rest.strip_prefix("shared-")?.parse().ok()
}

/// Best-effort bounded cleanup of stale spill files (both spellings) left in
/// the system temp dir by crashed commands. Only regular files older than a
/// day are removed — a live spool is written continuously by its command and
/// can never be that old; symlinks and directories are never followed or
/// removed. Runs at most once per capture, when a new spill starts.
fn sweep_stale_spill_files() {
    const MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
    const MAX_ENTRIES: usize = 4096;
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    // The scan bound must be far above the candidate cap: a temp dir full
    // of unrelated entries must not starve residue cleanup, so the entry
    // cap counts matched candidates, not foreign files.
    const SCAN_ENTRY_CAP: usize = 262_144;
    let mut candidates = 0usize;
    for (scanned, entry) in entries.flatten().enumerate() {
        if scanned >= SCAN_ENTRY_CAP {
            break;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_internal_spill_name(&name) {
            continue;
        }
        candidates += 1;
        if candidates > MAX_ENTRIES {
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age >= MIN_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Best-effort bounded cleanup of supervisor roots left in the system temp
/// dir by crashed runs (both spellings). Only directories named exactly
/// `*-supervisor-shared-<pid>`, older than an hour, whose owning pid is
/// provably gone are removed; a live owner (pid reuse included) keeps its
/// root. Symlinks are never followed or removed.
fn sweep_stale_supervisor_roots() {
    const MIN_AGE: Duration = Duration::from_secs(60 * 60);
    const MAX_ENTRIES: usize = 4096;
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    // The scan bound must be far above the candidate cap: a temp dir full
    // of unrelated entries must not starve residue cleanup, so the entry
    // cap counts matched candidates, not foreign files.
    const SCAN_ENTRY_CAP: usize = 262_144;
    let mut candidates = 0usize;
    for (scanned, entry) in entries.flatten().enumerate() {
        if scanned >= SCAN_ENTRY_CAP {
            break;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(pid) = supervisor_root_pid(&name) else {
            continue;
        };
        candidates += 1;
        if candidates > MAX_ENTRIES {
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age >= MIN_AGE);
        if stale && !process_alive(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// A bounded head (lossy text, truncated?) published by one stream reader.
type HeadSlot = Arc<Mutex<(String, bool)>>;

/// The FINAL bounded head a reader thread sends once its stream reaches EOF.
type HeadFinal = std::sync::mpsc::Receiver<(String, bool)>;

/// The bounded capture state; mutated only by the reader task.
struct SharedCapture {
    ring: RingBuffer,
    total: usize,
    /// Effective per-command spool cap (configured value clamped to the
    /// global ceiling); the artifact never holds more than this.
    artifact_max: usize,
    spooled: usize,
    /// True once the stream exceeded the effective cap and tail bytes were
    /// dropped from the artifact.
    artifact_truncated: bool,
    /// Overflow spills to a temp file on disk (never RAM); stored into the
    /// CAS once the command finishes.
    spill: Option<std::fs::File>,
    spill_path: Option<PathBuf>,
    artifact: Option<String>,
    cas: Arc<faktor_cas::Cas>,
}

impl SharedCapture {
    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len();
        let text = String::from_utf8_lossy(bytes);
        for line in text.split('\n') {
            self.ring.push(line.trim_end_matches('\r').to_string());
        }
        // Full-stream spooling (audit round 5), bounded by the effective
        // per-command cap (audit round 10): the artifact is the command
        // stream from the FIRST byte up to `artifact_max`, spooled to a temp
        // file — RAM stays bounded by the ring; disk grows only to the cap.
        // Once the cap is reached, further bytes are dropped from the
        // artifact (the ring keeps the tail) and the drop is recorded in
        // `artifact_truncated`. Finalization streams the file into the CAS
        // without ever materializing it in memory.
        if self.spill.is_none() && self.artifact_max > 0 {
            sweep_stale_spill_files();
            let dir = std::env::temp_dir();
            let path = dir.join(format!(
                "{FAKTOR_SPILL_PREFIX}{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            if let Ok(f) = std::fs::File::create(&path) {
                self.spill = Some(f);
                self.spill_path = Some(path);
            }
        }
        if let Some(f) = self.spill.as_mut() {
            let n = self
                .artifact_max
                .saturating_sub(self.spooled)
                .min(bytes.len());
            if n > 0 && std::io::Write::write_all(&mut *f, &bytes[..n]).is_ok() {
                self.spooled += n;
            }
            // A byte is lost only when the stream offered to the spool runs
            // past the cap; `spooled` counts what was actually stored.
            if self.spooled + (bytes.len() - n) > self.artifact_max {
                self.artifact_truncated = true;
            }
        }
    }

    /// Stream the spill file into the CAS (put_reader — never read-whole),
    /// then clean up the temp file.
    fn finalize_artifact(&mut self) {
        if let Some(path) = self.spill_path.take() {
            if let Ok(f) = std::fs::File::open(&path) {
                if let Ok(hash) = self.cas.put_reader_bounded(f, self.artifact_max) {
                    self.artifact = Some(format!("artifact://{}", hash.to_hex()));
                }
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Diagnostic timeline of one supervised child (audit round 10: the Linux
/// git/worktree hang investigation needs per-child op/pid/argv/timestamps).
#[derive(Debug, Clone)]
pub struct SpawnTimeline {
    pub op_id: u64,
    pub pid: u32,
    /// The command + args as spawned (space-joined, truncated for display).
    pub argv: String,
    pub owner: String,
    pub started_ms: i64,
    pub exited_ms: Option<i64>,
    pub exit_code: Option<i32>,
}

pub struct ProcessSupervisor {
    registry: Arc<Mutex<HashMap<u64, ChildState>>>,
    cas: Arc<faktor_cas::Cas>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    /// Bounded ring of recently spawned children (diagnostics).
    timeline: Arc<Mutex<VecDeque<SpawnTimeline>>>,
    /// Hard ceiling on live (not-yet-exited) children.
    max_live: usize,
    /// Serializes [admit → spawn → register] so the live ceiling is exact
    /// even under a spawn race (100 concurrent spawners never overshoot).
    spawn_serial: Mutex<()>,
    /// The ephemeral CAS root [`ProcessSupervisor::try_shared`] created for
    /// this supervisor, OWNED for the supervisor's whole lifetime (dropped
    /// together with the last `Arc`, so the temp dir can never be deleted
    /// under a live child or another `try_shared()` clone). `None` for
    /// every injected supervisor: daemon-owned supervisors are rooted by
    /// their constructor (the daemon CAS), never by a global temp root.
    _standalone_root: Option<tempfile::TempDir>,
}

impl std::fmt::Debug for ProcessSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessSupervisor")
            .field("registered", &self.registered())
            .field("max_live", &self.max_live)
            .finish()
    }
}

impl Drop for ProcessSupervisor {
    /// Daemon-shutdown scope (spec §22 / commandment 8): when the LAST
    /// reference to the supervisor drops, every still-live child is
    /// killed. No child outlives its runtime owner. On Windows each child's
    /// own kill-on-close job is the primary authority (terminated here, and
    /// closed by dropping the registry right after — the OS takes every
    /// still-running member even if the terminate call were skipped).
    fn drop(&mut self) {
        let targets: Vec<(u64, u32)> = {
            let reg = self.registry.lock().unwrap();
            reg.iter()
                .filter(|(_, s)| s.exited.is_none())
                .map(|(id, s)| (*id, s.pid))
                .collect()
        };
        for (id, pid) in targets {
            let _ = self.terminate_registered_sync(id, pid, 300);
        }
    }
}

impl ProcessSupervisor {
    pub fn new(cas: Arc<faktor_cas::Cas>) -> Arc<Self> {
        Self::with_limit(cas, DEFAULT_MAX_LIVE_CHILDREN)
    }

    /// Like [`ProcessSupervisor::new`] with an explicit live-child ceiling
    /// (bounded registry; spawns past the ceiling fail `Oversized`).
    pub fn with_limit(cas: Arc<faktor_cas::Cas>, max_live: usize) -> Arc<Self> {
        Self::with_limit_and_root(cas, max_live, None)
    }

    fn with_limit_and_root(
        cas: Arc<faktor_cas::Cas>,
        max_live: usize,
        standalone_root: Option<tempfile::TempDir>,
    ) -> Arc<Self> {
        let max_live = max_live.max(1);
        Arc::new(Self {
            registry: Arc::new(Mutex::new(HashMap::new())),
            cas,
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            timeline: Arc::new(Mutex::new(VecDeque::new())),
            max_live,
            spawn_serial: Mutex::new(()),
            _standalone_root: standalone_root,
        })
    }

    /// The process-wide standalone supervisor, for callers that genuinely
    /// cannot receive a daemon-rooted supervisor through their constructor
    /// (env-var hook registry, crate-level tests, one-shot CLI helpers).
    ///
    /// Daemon-owned subsystems MUST receive `Arc<ProcessSupervisor>` by
    /// injection instead of calling this: index, hooks and verification all
    /// take the daemon's ONE supervisor from their constructors, so the
    /// normal graph has no global fallback.
    ///
    /// Failure to create the process authority is a typed initialization
    /// failure (`Error`), NEVER a panic: the ephemeral root is a
    /// `tempfile::Builder` temp dir with the reserved `faktor-supervisor-`
    /// prefix, explicitly set to and VERIFIED at owner-only (`0700`) mode on
    /// Unix before the supervisor is built. The temp dir is OWNED by the
    /// returned supervisor for its whole lifetime (dropped with the last
    /// `Arc`), so the root cannot disappear under a live child or a clone.
    pub fn try_shared() -> Result<Arc<Self>, Error> {
        static SHARED: std::sync::OnceLock<Result<Arc<ProcessSupervisor>, Error>> =
            std::sync::OnceLock::new();
        SHARED.get_or_init(Self::init_shared).clone()
    }

    fn init_shared() -> Result<Arc<ProcessSupervisor>, Error> {
        // Legacy sweep (best effort, age- and liveness-gated): the old
        // pid-named shared roots a crashed process may have left behind.
        sweep_stale_supervisor_roots();
        let dir = tempfile::Builder::new()
            .prefix(FAKTOR_SUPERVISOR_PREFIX)
            .tempdir()
            .map_err(|e| {
                Error::internal(format!(
                    "process supervisor initialization failed: cannot create its ephemeral \
                     root: {e}"
                ))
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    Error::internal(format!(
                        "process supervisor initialization failed: cannot set owner-only \
                         permissions on {}: {e}",
                        dir.path().display()
                    ))
                },
            )?;
            let mode = std::fs::metadata(dir.path())
                .map_err(|e| {
                    Error::permission(format!(
                        "process supervisor initialization failed: cannot verify the mode of \
                         {}: {e}",
                        dir.path().display()
                    ))
                })?
                .permissions()
                .mode()
                & 0o777;
            if mode != 0o700 {
                return Err(Error::permission(format!(
                    "process supervisor initialization failed: ephemeral root {} is not \
                     owner-only ({mode:o}); refusing to root a supervisor there",
                    dir.path().display()
                )));
            }
        }
        let cas = faktor_cas::Cas::open(dir.path().join("cas")).map_err(|e| {
            Error::internal(format!(
                "process supervisor initialization failed: cannot open its ephemeral CAS root \
                 at {}: {e}",
                dir.path().display()
            ))
        })?;
        Ok(Self::with_limit_and_root(
            Arc::new(cas),
            DEFAULT_MAX_LIVE_CHILDREN,
            Some(dir),
        ))
    }

    fn alloc_id(&self) -> u64 {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if id == 0 {
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        } else {
            id
        }
    }

    /// Args/cwd/process-group base, NO env applied: [`ProcessSupervisor::command`]
    /// layers the exact [`EnvSpec`] policy on top. A `DenyAll`
    /// network-isolation request installs its pre-exec hook here, so EVERY
    /// spawn entry point (async, sync, detached) carries the backend or
    /// none.
    #[allow(unsafe_code)]
    fn command_base(&self, cfg: &SpawnConfig) -> std::process::Command {
        let mut cmd = std::process::Command::new(&cfg.cmd);
        cmd.args(&cfg.args).current_dir(&cfg.cwd);
        // Own process group so kills target the whole tree.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(target_os = "linux")]
        if cfg.network_isolation == NetworkIsolation::DenyAll {
            // SAFETY: `apply_deny_all_isolation` installs the documented
            // allocation-free unshare pre-exec hook (post-fork, pre-exec).
            unsafe {
                sandbox::apply_deny_all_isolation(&mut cmd);
            }
        }
        cmd
    }

    /// THE env policy: `env_clear()` then the resolved [`EnvSpec`]. No
    /// implicit daemon environment and no legacy PATH/HOME injection —
    /// every spawn entry point (async, sync, detached) carries exactly the
    /// spec's environment.
    fn command(&self, cfg: &SpawnConfig) -> std::process::Command {
        let mut cmd = self.command_base(cfg);
        cfg.env.apply(&mut cmd);
        cmd
    }

    /// [`Self::command`] plus the pre-exec budget plan: on unix the
    /// requested rlimits are installed AFTER the environment is applied and
    /// BEFORE `exec`, so the whole tree inherits them (failures are
    /// per-resource non-fatal; the effective report says which resources the
    /// platform honors).
    fn command_with_budgets(
        &self,
        cfg: &SpawnConfig,
        budgets: Option<&TreeBudgets>,
    ) -> std::process::Command {
        let mut cmd = self.command(cfg);
        #[cfg(unix)]
        if let Some(budgets) = budgets {
            budget::install_child_rlimits(&mut cmd, budgets);
        }
        #[cfg(not(unix))]
        let _ = budgets;
        cmd
    }

    /// The EFFECTIVE budget report of one spawn, applied post-spawn on top
    /// of the pre-exec plan: Linux cgroup v2 (tree-wide memory/process
    /// limits, race-free thanks to `cgroup.freeze`) when the unified
    /// hierarchy is writable. The returned guard owns the cgroup cleanup and
    /// must live exactly as long as the tree does.
    fn apply_spawn_budgets(
        &self,
        pid: u32,
        budgets: Option<&TreeBudgets>,
    ) -> (BudgetEnforcement, Option<TreeBudgetGuard>) {
        let Some(budgets) = budgets else {
            return (BudgetEnforcement::default(), None);
        };
        let mut enforcement = budget::spawn_path_enforcement(budgets);
        if budgets.wall_time_ms > 0 {
            enforcement.wall = LimitState::Enforced;
            enforcement.note(&format!(
                "wall: the run deadline (min(caller deadline, {}ms wall budget)) kills the \
                 whole group",
                budgets.wall_time_ms
            ));
        }
        if budgets.is_disabled() {
            return (enforcement, None);
        }
        #[cfg(target_os = "linux")]
        {
            match budget::enforce_tree_budgets(pid, budgets) {
                Ok(guard) => {
                    enforcement = enforcement.merge_best(guard.enforcement().clone());
                    (enforcement, Some(guard))
                }
                Err(error) => {
                    enforcement.note(&format!("post-spawn budget guard refused: {error}"));
                    (enforcement, None)
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            (enforcement, None)
        }
    }

    /// Refuse the spawn when the live-child ceiling is reached: the caller
    /// holds `spawn_serial`, so admit→spawn→register is atomic and a spawn
    /// race can never overshoot the ceiling.
    fn admit(&self) -> Result<(), Error> {
        let live = self
            .registry
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.exited.is_none())
            .count();
        if live >= self.max_live {
            return Err(Error::oversized(format!(
                "live child ceiling reached: {live} live children registered, ceiling is {}; \
                 refusing the spawn",
                self.max_live
            )));
        }
        Ok(())
    }

    fn register(
        &self,
        pid: u32,
        owner: ProcessOwner,
        started_ms: i64,
        containment: ChildContainment,
        reap: Arc<ReapState>,
        broker_bridge: Option<Arc<BrokerOnlyBridge>>,
    ) -> u64 {
        let id = self.alloc_id();
        self.registry.lock().unwrap().insert(
            id,
            ChildState {
                pid,
                owner,
                started_ms,
                exited: None,
                reap,
                containment,
                broker_bridge,
            },
        );
        id
    }

    /// The row's reap/signal serial. `None` once the row was collected by
    /// [`ProcessSupervisor::reap`]: a collected row can never be signalled.
    fn reap_state(&self, id: u64) -> Option<Arc<ReapState>> {
        self.registry
            .lock()
            .unwrap()
            .get(&id)
            .map(|state| state.reap.clone())
    }

    /// PRIMARY tree termination of one registered child (async paths): on
    /// Windows the child's kill-on-close job (the kernel enumerates
    /// membership — children of members are members, so this is the whole
    /// tree with no pid walk and no taskkill); on unix the owned process
    /// group with SIGTERM→SIGKILL grace, issued ONLY through the child's
    /// guarded [`ReapState::try_signal_group`], which refuses everything
    /// once the reaper consumed the child. Async so the unix grace wait
    /// never blocks the runtime; the Windows job kill is immediate.
    async fn terminate_registered(&self, id: u64, pid: u32, grace_ms: u64) {
        #[cfg(windows)]
        {
            let _ = (pid, grace_ms);
            if self.reap_state(id).is_some() {
                self.terminate_containment(id);
            }
        }
        #[cfg(not(windows))]
        {
            let Some(reap) = self.reap_state(id) else {
                return;
            };
            if !reap.try_signal_group(pid, libc::SIGTERM) {
                return;
            }
            let deadline = tokio::time::Instant::now() + Duration::from_millis(grace_ms);
            loop {
                // Once the single reaper consumed the child, no further
                // signal may reference the pid: stop the grace loop instead
                // of polling a possibly-recycled group id.
                if reap.reaped() || group_gone(pid) {
                    return;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            // A stubborn member survived SIGTERM: escalate. Identity is
            // re-verified under the reap serial by the guarded issuance —
            // it signals only while the child is still provably unreaped
            // (and refuses if the reaper consumed it during the grace).
            let _ = reap.try_signal_group(pid, libc::SIGKILL);
        }
    }

    /// Sync twin of [`Self::terminate_registered`] for the sync, Drop and
    /// public kill paths.
    fn terminate_registered_sync(&self, id: u64, pid: u32, grace_ms: u64) -> Result<(), Error> {
        #[cfg(windows)]
        {
            let _ = (pid, grace_ms);
            if self.reap_state(id).is_none() {
                return Err(Error::not_found(format!("child {id}")));
            }
            self.terminate_containment(id);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let Some(reap) = self.reap_state(id) else {
                return Err(Error::not_found(format!("child {id}")));
            };
            if !reap.try_signal_group(pid, libc::SIGTERM) {
                return Ok(());
            }
            let deadline = std::time::Instant::now() + Duration::from_millis(grace_ms);
            while std::time::Instant::now() < deadline {
                if reap.reaped() || group_gone(pid) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // A stubborn member survived SIGTERM: escalate (see the async
            // twin for the identity rule).
            let _ = reap.try_signal_group(pid, libc::SIGKILL);
            Ok(())
        }
    }

    /// Terminate the job of one registered child. The job handle stays in
    /// the registry row: the row's own kill-on-close closes it when the row
    /// is dropped (reap/Drop), the OS failsafe for anything still running.
    #[cfg(windows)]
    fn terminate_containment(&self, id: u64) {
        let reg = self.registry.lock().unwrap();
        if let Some(state) = reg.get(&id) {
            state.containment.terminate();
        }
    }

    /// Recent spawns, newest first (bounded; see [`SpawnTimeline`]).
    pub fn recent_spawns(&self) -> Vec<SpawnTimeline> {
        self.timeline.lock().unwrap().iter().cloned().collect()
    }

    fn timeline_spawn(&self, op_id: u64, pid: u32, argv: String, owner: &ProcessOwner) {
        let mut tl = self.timeline.lock().unwrap();
        tl.push_front(SpawnTimeline {
            op_id,
            pid,
            argv,
            owner: format!("{owner:?}"),
            started_ms: now_ms(),
            exited_ms: None,
            exit_code: None,
        });
        tl.truncate(256);
    }

    /// Run to completion: bounded capture (ring + CAS spill), deadline,
    /// cancellation. Uses tokio's async process pipes so reads never block
    /// the runtime; the reader task owns the bounded ring.
    pub async fn run(
        &self,
        cfg: SpawnConfig,
        deadline: Duration,
        token: CancellationToken,
    ) -> Result<CommandOutput, Error> {
        self.run_inner(cfg, None, deadline, token)
            .await
            .map(|(output, _enforcement)| output)
    }

    /// [`Self::run`] under an explicit process-tree budget (see
    /// [`Self::run_sync_with_budgets`] for the enforcement semantics): the
    /// requested limits are installed before `exec` (unix rlimits) and
    /// upgraded post-spawn where the platform can do better (Linux cgroup
    /// v2); the deadline is tightened to the requested wall budget. The
    /// returned [`BudgetEnforcement`] is the EFFECTIVE per-limit state.
    pub async fn run_with_budgets(
        &self,
        cfg: SpawnConfig,
        budgets: TreeBudgets,
        deadline: Duration,
        token: CancellationToken,
    ) -> Result<(CommandOutput, BudgetEnforcement), Error> {
        self.run_inner(cfg, Some(budgets), deadline, token).await
    }

    async fn run_inner(
        &self,
        cfg: SpawnConfig,
        budgets: Option<TreeBudgets>,
        deadline: Duration,
        token: CancellationToken,
    ) -> Result<(CommandOutput, BudgetEnforcement), Error> {
        // The run OWNS the materialized cmd script (when lowering produced
        // one): deleted on every return path after the child is gone.
        let _cmd_script = CmdScriptGuard(materialized_cmd_script(&cfg));
        if cfg.network_isolation == NetworkIsolation::DenyAll {
            isolation_gate(&cfg)?;
        }
        let bridge = prepare_network_isolation(&cfg)?;
        use tokio::io::AsyncReadExt;
        use tokio::process::Command as TokioCommand;

        let effective_deadline = budgets
            .map(|budgets| budget::deadline_with_wall(deadline, &budgets))
            .unwrap_or(deadline);
        let mut std_cmd = contain_on_create(self.command_with_budgets(&cfg, budgets.as_ref()));
        install_network_isolation(&mut std_cmd, bridge.as_ref());
        if cfg.capture {
            std_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            std_cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        // Windows containment (before any process exists): the per-child
        // kill-on-close job, carrying the requested budget limits when
        // present. The process is spawned CREATE_SUSPENDED, is assigned
        // strictly, has membership verified while it cannot run, and is
        // resumed LAST — an unbudgeted, uncontained child is never exposed.
        let containment = prepare_containment_with_limits(budgets.as_ref())?;
        let started_ms = now_ms();
        let mut cmd = TokioCommand::from(std_cmd);
        // Audit round 11: a DROPPED run() future (outer timeout/unwind) must
        // never leak the child. `kill_on_drop` is deliberately DISABLED: the
        // RAII guard below is the single kill+reap authority for a dropped
        // future (group SIGKILL while the child is unreaped, then a blocking
        // `waitpid`), so tokio's own drop would be a second, raw-pid signal
        // of an already-consumed (possibly recycled) pid. The guard is
        // constructed immediately after spawn, before any fallible step, so
        // no path can drop the child unguarded.
        cmd.kill_on_drop(false);
        let mut child = cmd.spawn().map_err(|e| spawn_failure(&cfg, e))?;
        let pid = child.id().unwrap_or(0);
        let reap = ReapState::new();
        // Declared AFTER `child` so it drops BEFORE the child handle: the
        // outer-timeout/unwind path SIGKILLs the owned group while the child
        // is still unreaped, blocks until the child is consumed, then
        // publishes the reap (see [`RunGroupGuard`]).
        let _run_group_guard = RunGroupGuard {
            reap: reap.clone(),
            pid,
        };
        #[cfg(windows)]
        win_spawn::assign_and_resume_tokio(&containment.job, &mut child).await?;
        #[cfg(target_os = "linux")]
        mark_network_isolation_proven(&cfg);
        let id = self.register(
            pid,
            cfg.owner.clone(),
            started_ms,
            containment,
            reap.clone(),
            bridge.clone(),
        );
        self.timeline_spawn(
            id,
            pid,
            format!("{} {}", cfg.cmd, cfg.args.join(" "))
                .chars()
                .take(300)
                .collect(),
            &cfg.owner,
        );
        let (enforcement, _post_guard) = self.apply_spawn_budgets(pid, budgets.as_ref());

        let shared: Arc<Mutex<SharedCapture>> = Arc::new(Mutex::new(SharedCapture {
            ring: RingBuffer::new(RING_LINES),
            total: 0,
            artifact_max: effective_artifact_max(cfg.artifact_max),
            spooled: 0,
            artifact_truncated: false,
            spill: None,
            spill_path: None,
            artifact: None,
            cas: self.cas.clone(),
        }));

        // Async reader task: drains both streams into the bounded ring.
        // Audit round 51: a pipe whose EOF was observed is REMOVED from
        // future selects (arm precondition) — an EOF-ready read would
        // resolve `Ok(0)` instantly on every poll and spin the loop while
        // the other pipe stays open. Cancellation is wake-driven
        // (`CancellationToken::cancelled`, std-waker registration), so the
        // reader also stops promptly on cancel without a 5 ms poll timer.
        let reader = if cfg.capture {
            let mut stdout = child.stdout.take();
            let mut stderr = child.stderr.take();
            let shared2 = shared.clone();
            let token2 = token.clone();
            Some(tokio::spawn(async move {
                let mut out_buf = [0u8; 8192];
                let mut err_buf = [0u8; 8192];
                let mut stdout_eof = stdout.is_none();
                let mut stderr_eof = stderr.is_none();
                loop {
                    if stdout_eof && stderr_eof {
                        break;
                    }
                    tokio::select! {
                        _ = token2.cancelled() => break,
                        r = async {
                            match stdout.as_mut() {
                                Some(s) => s.read(&mut out_buf).await,
                                None => Ok(0),
                            }
                        }, if !stdout_eof => {
                            match r {
                                Ok(0) => { stdout_eof = true; stdout = None; }
                                Ok(n) => { shared2.lock().unwrap().push(&out_buf[..n]); }
                                Err(_) => { stdout_eof = true; stdout = None; }
                            }
                        }
                        r = async {
                            match stderr.as_mut() {
                                Some(s) => s.read(&mut err_buf).await,
                                None => Ok(0),
                            }
                        }, if !stderr_eof => {
                            match r {
                                Ok(0) => { stderr_eof = true; stderr = None; }
                                Ok(n) => { shared2.lock().unwrap().push(&err_buf[..n]); }
                                Err(_) => { stderr_eof = true; stderr = None; }
                            }
                        }
                    }
                }
            }))
        } else {
            None
        };

        // Poll loop: cancellation + exit status. The deadline is the overall
        // bound; the child's group is killed on timeout (async: no blocking
        // sleep inside the runtime).
        enum RunOutcome {
            Exited(Option<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }
        // The deadline is an ABSOLUTE instant: `select!` rebuilds its arm
        // futures on every poll, so a relative `sleep(deadline)` against
        // the hot 5ms cancellation arm would slide forward forever under
        // load and never fire (audit round 11 hang). `sleep_until` pins the
        // deadline.
        // Process-lifetime discipline (audit round 12 — the P0): ONE
        // reaping authority per child (this future owns child.wait), the
        // leader's pgid is used for tree signalling ONLY while the group
        // provably still exists, and a normally-exited command is NOT
        // group-killed (that killed unrelated trees via PID/PGID reuse).
        // Sequence: child exit -> bounded pipe drain -> EOF? finish :
        // descendant still owns the pipe -> terminate the OWNED tree (a
        // descendant holding our pipe means the group is alive, so the
        // pgid cannot have been recycled) -> bounded final drain -> finish.
        let deadline_at = tokio::time::Instant::now() + effective_deadline;
        let outcome = tokio::select! {
            s = child.wait() => {
                // The single reaper consumed the child: publish the reap
                // under the serial before anything else can observe it.
                reap.publish_reaped(pid);
                RunOutcome::Exited(s.ok())
            }
            _ = tokio::time::sleep_until(deadline_at) => {
                self.terminate_registered(id, pid, 2000).await;
                let _ = child.kill().await;
                let _ = child.wait().await;
                reap.publish_reaped(pid);
                // The child is gone WITH no exit code: mark it exited so
                // reap() can collect the registry entry exactly once. The
                // old `None` left the child permanently "alive" in the
                // registry (audit round 17).
                self.mark_exited(id, Some(None));
                RunOutcome::TimedOut
            }
            _ = token.cancelled() => {
                self.terminate_registered(id, pid, 500).await;
                let _ = child.kill().await;
                let _ = child.wait().await;
                reap.publish_reaped(pid);
                self.mark_exited(id, Some(None));
                RunOutcome::Cancelled
            }
        };

        // The direct child is gone. Drain its remaining output for the
        // bounded window; a reader that is STILL alive after the window
        // means a descendant keeps one of our pipes open (EOF can never
        // arrive). The owned tree is then asked to terminate through the
        // guarded path — which is a no-op once the reaper consumed the
        // leader (no identity proof for the negative pgid remains; a
        // recycled group must never be signalled). The drain is bounded
        // either way, so this never owns the caller.
        let mut reader = reader;
        let drain_done = {
            let mut r = reader.take();
            let done = tokio::time::timeout(Duration::from_millis(POST_EXIT_DRAIN_MS), async {
                if let Some(r) = r.as_mut() {
                    let _ = r.await;
                }
            })
            .await
            .is_ok();
            if r.is_some() && !done {
                // Descendant still owns the pipe: guarded tree termination
                // (refused if the leader was already consumed).
                self.terminate_registered(id, pid, 1500).await;
                if let Some(r) = r {
                    r.abort();
                    let _ = r.await;
                }
            }
            done
        };
        let _ = drain_done;

        let exit_code = match outcome {
            RunOutcome::Exited(status) => status.and_then(|s| s.code()),
            RunOutcome::TimedOut => {
                return Err(Error::timeout(format!(
                    "command {} exceeded its {}ms deadline",
                    cfg.cmd,
                    effective_deadline.as_millis()
                )));
            }
            RunOutcome::Cancelled => {
                return Err(Error::cancelled());
            }
        };
        self.mark_exited(id, Some(exit_code));

        let mut slice_hint = None;
        let (excerpt, artifact, artifact_truncated) = {
            let mut g = shared.lock().unwrap();
            g.finalize_artifact();
            let mut excerpt = g.ring.excerpt();
            let artifact = g.artifact.clone();
            let artifact_truncated = g.artifact_truncated;
            excerpt.push_str(&format!("[exit code: {}]\n", exit_code.unwrap_or(-1)));
            if excerpt.len() > MAX_EXCERPT_BYTES {
                excerpt.truncate(MAX_EXCERPT_BYTES);
            }
            (excerpt, artifact, artifact_truncated)
        };
        if let Some(a) = &artifact {
            slice_hint = Some(format!("{a}?slice=0&len=1024"));
        }
        Ok((
            CommandOutput {
                excerpt,
                exit_code,
                artifact,
                slice_hint,
                ring_lines: RING_LINES,
                artifact_truncated,
            },
            enforcement,
        ))
    }

    /// Spawn one bounded-head reader thread for a child stream (or an empty
    /// head when the stream is not piped). The returned receiver yields the
    /// FINAL head at EOF; the shared snapshot is updated after every read,
    /// so a caller that gives up on a descendant-held pipe still gets every
    /// byte the reader had already consumed.
    fn spawn_head_reader(
        pipe: Option<impl Read + Send + 'static>,
        cap: usize,
    ) -> (HeadFinal, HeadSlot) {
        let (tx, rx) = std::sync::mpsc::channel();
        let progress: Arc<Mutex<(String, bool)>> = Arc::new(Mutex::new((String::new(), false)));
        let progress2 = progress.clone();
        std::thread::spawn(move || {
            let res = match pipe {
                Some(p) => Self::read_bounded_head(Box::new(p), cap, Some(&progress2)),
                None => (String::new(), false),
            };
            *progress2
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = res.clone();
            let _ = tx.send(res);
        });
        (rx, progress)
    }

    /// The bounded head published so far by one reader thread.
    fn head_snapshot(slot: &HeadSlot) -> (String, bool) {
        slot.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Stream a child pipe into a bounded head: read incrementally, keep at
    /// most `cap` bytes, then DRAIN the remainder in fixed chunks so a
    /// hostile 10 MB / infinite producer never grows memory past the cap
    /// (the child is only ever blocked briefly per kernel pipe buffer,
    /// never on us). When `progress` is given, the bounded head so far is
    /// published after every read. Returns (lossy head, truncated?).
    fn read_bounded_head(
        pipe: Box<dyn Read + Send>,
        cap: usize,
        progress: Option<&HeadSlot>,
    ) -> (String, bool) {
        const CHUNK: usize = 8192;
        let mut head: Vec<u8> = Vec::with_capacity(cap.min(CHUNK));
        let mut scratch = [0u8; CHUNK];
        let mut truncated = false;
        let mut pipe = pipe;
        loop {
            let n = match pipe.read(&mut scratch) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if head.len() < cap {
                let take = (cap - head.len()).min(n);
                head.extend_from_slice(&scratch[..take]);
                if take < n {
                    truncated = true;
                }
            } else {
                truncated = true;
            }
            if let Some(slot) = progress {
                *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    (String::from_utf8_lossy(&head).into_owned(), truncated);
            }
        }
        (String::from_utf8_lossy(&head).into_owned(), truncated)
    }

    /// The SYNCHRONOUS bounded run (audit P0-40; the hook lifecycle runs
    /// from synchronous contexts and cannot await [`Self::run`]).
    ///
    /// Semantics mirror `run()` without a tokio context:
    /// - the child env comes EXCLUSIVELY from [`SpawnConfig::env`]
    ///   ([`EnvSpec`]) — no implicit daemon environment, not even PATH;
    /// - the child runs in its own process group; the deadline DOMINATES —
    ///   on expiry the OWNED tree is killed (guarded against signalling a
    ///   consumed/recycled id) and partial output is reported as forensics
    ///   with `timed_out: true` (the caller's policy decides, never the
    ///   partial stdout);
    /// - stdout/stderr are read on dedicated threads into bounded heads
    ///   (remainder drained, never buffered);
    /// - after the direct child exits, a descendant still holding a pipe
    ///   past the drain bound gets the guarded tree kill — which refuses to
    ///   signal once the leader was consumed (the negative pgid then has no
    ///   portable identity proof), so neither the caller nor a reader
    ///   thread is ever owned by a grandchild: the bounded published head
    ///   is returned and the run ends on the drain bound;
    /// - the run is registered in the registry (owner row, timeline) and
    ///   marked exited exactly once; `reap()` collects the entry.
    ///
    /// Wall time is bounded by `deadline` + a small constant (kill grace
    /// and bounded drains), never by the child's behavior.
    pub fn run_sync(
        &self,
        cfg: SpawnConfig,
        deadline: Duration,
        stdout_cap: usize,
        stderr_cap: usize,
    ) -> Result<SyncRunOutput, Error> {
        self.run_sync_inner(cfg, None, deadline, stdout_cap, stderr_cap)
            .map(|(output, _enforcement)| output)
    }

    /// [`Self::run_sync`] under an explicit process-tree budget: the
    /// requested limits are installed BEFORE `exec` (unix rlimits where the
    /// platform honors them) and upgraded where the platform can do better
    /// (Linux cgroup v2 when the unified hierarchy is writable); the run
    /// deadline is tightened to the requested wall budget. The returned
    /// [`BudgetEnforcement`] is the EFFECTIVE per-limit state — `Enforced`,
    /// `Degraded`, `Unsupported` (typed) or `NotRequested`, never a silent
    /// claim.
    pub fn run_sync_with_budgets(
        &self,
        cfg: SpawnConfig,
        budgets: TreeBudgets,
        deadline: Duration,
        stdout_cap: usize,
        stderr_cap: usize,
    ) -> Result<(SyncRunOutput, BudgetEnforcement), Error> {
        self.run_sync_inner(cfg, Some(budgets), deadline, stdout_cap, stderr_cap)
    }

    fn run_sync_inner(
        &self,
        cfg: SpawnConfig,
        budgets: Option<TreeBudgets>,
        deadline: Duration,
        stdout_cap: usize,
        stderr_cap: usize,
    ) -> Result<(SyncRunOutput, BudgetEnforcement), Error> {
        // The run OWNS the materialized cmd script (when lowering produced
        // one): deleted on every return path after the child is gone.
        let _cmd_script = CmdScriptGuard(materialized_cmd_script(&cfg));
        if cfg.network_isolation == NetworkIsolation::DenyAll {
            isolation_gate(&cfg)?;
        }
        let bridge = prepare_network_isolation(&cfg)?;
        let effective_deadline = budgets
            .map(|budgets| budget::deadline_with_wall(deadline, &budgets))
            .unwrap_or(deadline);
        let argv = format!("{} {}", cfg.cmd, cfg.args.join(" "))
            .chars()
            .take(300)
            .collect();
        let mut cmd = contain_on_create(self.command_with_budgets(&cfg, budgets.as_ref()));
        install_network_isolation(&mut cmd, bridge.as_ref());
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started_ms = now_ms();
        let (mut child, pid, id, reap) = {
            let _serial = self.spawn_serial.lock().unwrap();
            self.admit()?;
            // Windows containment: job BEFORE process, spawn suspended,
            // assign strictly, verify membership, resume LAST. Budgets are
            // part of the job, so limits exist before the child can run.
            let containment = prepare_containment_with_limits(budgets.as_ref())?;
            let child = cmd.spawn().map_err(|e| spawn_failure(&cfg, e))?;
            #[cfg(windows)]
            let mut child = child;
            #[cfg(windows)]
            win_spawn::assign_and_resume_std(&containment.job, &mut child)?;
            #[cfg(target_os = "linux")]
            mark_network_isolation_proven(&cfg);
            let pid = child.id();
            let reap = ReapState::new();
            let id = self.register(
                pid,
                cfg.owner.clone(),
                started_ms,
                containment,
                reap.clone(),
                bridge.clone(),
            );
            self.timeline_spawn(id, pid, argv, &cfg.owner);
            (child, pid, id, reap)
        };
        let (enforcement, _post_guard) = self.apply_spawn_budgets(pid, budgets.as_ref());
        // Dedicated reader threads: bounded head per stream, remainder
        // drained, then ONE send. Reads can never block the caller past the
        // bounded settles below; each thread also publishes its bounded head
        // so far, so a reader still blocked on a descendant-held pipe does
        // not forfeit the bytes it already read.
        let (out_rx, out_progress) = Self::spawn_head_reader(child.stdout.take(), stdout_cap);
        let (err_rx, err_progress) = Self::spawn_head_reader(child.stderr.take(), stderr_cap);
        // The waiter owns reaping; the caller enforces the deadline. The
        // reap publication under the child serial is what guards every kill:
        // once the waiter consumed the child, no kill path may signal the
        // (now recyclable) pid.
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        {
            let reap = reap.clone();
            std::thread::spawn(move || {
                let code = child.wait().ok().and_then(|s| s.code());
                reap.publish_reaped(pid);
                let _ = exit_tx.send(code);
            });
        }
        let (exit_code, timed_out) = match exit_rx.recv_timeout(effective_deadline) {
            Ok(code) => (code, false),
            Err(_) => {
                // Deadline fired: guarded kill of the OWNED tree — if the
                // waiter consumed the child first, the guard turns this into
                // a no-op instead of a stale signal.
                let _ = self.terminate_registered_sync(id, pid, 500);
                let code = exit_rx
                    .recv_timeout(Duration::from_millis(500))
                    .ok()
                    .flatten();
                (code, true)
            }
        };
        // Exactly-once exit marking (registry + timeline); a timed-out tree
        // was killed, so its exit code is None unless it raced out cleanly.
        self.mark_exited(id, Some(exit_code));
        // Bounded settle: each reader finishes at pipe EOF. A grandchild
        // that inherited the pipe delays it — never the caller, never the
        // reader forever: past the drain bound the guarded tree kill is
        // attempted (a no-op once the leader was consumed by its reaper;
        // the negative pgid then has no portable identity proof) and the
        // bounded head each reader has PUBLISHED so far is collected.
        let settle = Duration::from_millis(SYNC_DRAIN_MS);
        let mut out_head = out_rx.recv_timeout(settle).ok();
        let mut err_head = err_rx.recv_timeout(settle).ok();
        if out_head.is_none() || err_head.is_none() {
            let _ = self.terminate_registered_sync(id, pid, SYNC_KILL_GRACE_MS);
            let grace = Duration::from_millis(500);
            if out_head.is_none() {
                out_head = out_rx.recv_timeout(grace).ok();
            }
            if err_head.is_none() {
                err_head = err_rx.recv_timeout(grace).ok();
            }
        }
        let (stdout_head, stdout_truncated) =
            out_head.unwrap_or_else(|| Self::head_snapshot(&out_progress));
        let (stderr_head, stderr_truncated) =
            err_head.unwrap_or_else(|| Self::head_snapshot(&err_progress));
        Ok((
            SyncRunOutput {
                exit_code,
                timed_out,
                stdout_head,
                stderr_head,
                stdout_truncated,
                stderr_truncated,
            },
            enforcement,
        ))
    }

    /// Spawn with piped stdin/stdout/stderr (for MCP/LSP style servers).
    /// The caller owns the pipes; a reaper thread still reaps the child.
    pub fn spawn_detached_with_pipes(&self, mut cfg: SpawnConfig) -> Result<SpawnedProcess, Error> {
        // The reaper thread owns the materialized cmd script (when lowering
        // produced one): cmd reads the batch file while it runs, so it is
        // deleted only after the child exits.
        let cmd_script = CmdScriptGuard(materialized_cmd_script(&cfg));
        if cfg.network_isolation == NetworkIsolation::DenyAll {
            isolation_gate(&cfg)?;
        }
        let bridge = prepare_network_isolation(&cfg)?;
        cfg.capture = false;
        let mut cmd = contain_on_create(self.command(&cfg));
        install_network_isolation(&mut cmd, bridge.as_ref());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let containment = prepare_containment()?;
        let started_ms = now_ms();
        let mut child = cmd.spawn().map_err(|e| spawn_failure(&cfg, e))?;
        #[cfg(windows)]
        win_spawn::assign_and_resume_std(&containment.job, &mut child)?;
        #[cfg(target_os = "linux")]
        mark_network_isolation_proven(&cfg);
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::internal("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::internal("no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::internal("no stderr"))?;
        let reap = ReapState::new();
        let id = self.register(
            pid,
            cfg.owner.clone(),
            started_ms,
            containment,
            reap.clone(),
            bridge.clone(),
        );
        self.timeline_spawn(
            id,
            pid,
            format!("{} {}", cfg.cmd, cfg.args.join(" "))
                .chars()
                .take(300)
                .collect(),
            &cfg.owner,
        );
        // Reaper thread (no zombies); the caller keeps the pipes. It also
        // deletes the materialized cmd script once the child has exited, and
        // publishes the reap BEFORE anything else can observe the pid.
        let registry = self.registry.clone();
        let script = cmd_script.disarm();
        std::thread::spawn(move || {
            let status = child.wait().ok();
            reap.publish_reaped(pid);
            if let Some(path) = script {
                let _ = std::fs::remove_file(path);
            }
            let code = status.and_then(|s| s.code());
            let mut reg = registry.lock().unwrap();
            if let Some(state) = reg.get_mut(&id) {
                state.exited = Some(code);
            }
        });
        Ok(SpawnedProcess {
            child_pid: pid,
            stdin,
            stdout,
            stderr,
            network_bridge: bridge.clone(),
        })
    }

    /// Spawn detached with a reaper thread (no zombies); the caller owns the
    /// child and must kill/transfer deliberately.
    pub fn spawn(&self, cfg: SpawnConfig) -> Result<ChildHandle, Error> {
        // The reaper thread owns the materialized cmd script (when lowering
        // produced one): deleted only after the child exits.
        let cmd_script = CmdScriptGuard(materialized_cmd_script(&cfg));
        if cfg.network_isolation == NetworkIsolation::DenyAll {
            isolation_gate(&cfg)?;
        }
        let bridge = prepare_network_isolation(&cfg)?;
        let mut cmd = contain_on_create(self.command(&cfg));
        install_network_isolation(&mut cmd, bridge.as_ref());
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let started_ms = now_ms();
        let (child, pid, id, reap) = {
            let _serial = self.spawn_serial.lock().unwrap();
            self.admit()?;
            let containment = prepare_containment()?;
            let child = cmd.spawn().map_err(|e| spawn_failure(&cfg, e))?;
            #[cfg(windows)]
            let mut child = child;
            #[cfg(windows)]
            win_spawn::assign_and_resume_std(&containment.job, &mut child)?;
            #[cfg(target_os = "linux")]
            mark_network_isolation_proven(&cfg);
            let pid = child.id();
            let reap = ReapState::new();
            let id = self.register(
                pid,
                cfg.owner.clone(),
                started_ms,
                containment,
                reap.clone(),
                bridge.clone(),
            );
            self.timeline_spawn(
                id,
                pid,
                format!("{} {}", cfg.cmd, cfg.args.join(" "))
                    .chars()
                    .take(300)
                    .collect(),
                &cfg.owner,
            );
            (child, pid, id, reap)
        };
        // Reaper thread: waitpid is the only way to avoid zombies. It also
        // deletes the materialized cmd script once the child has exited, and
        // publishes the reap BEFORE anything else can observe the pid.
        let registry = self.registry.clone();
        let script = cmd_script.disarm();
        std::thread::spawn(move || {
            let status = child.wait_with_output().map(|o| o.status).ok();
            reap.publish_reaped(pid);
            if let Some(path) = script {
                let _ = std::fs::remove_file(path);
            }
            let code = status.and_then(|s| s.code());
            let mut reg = registry.lock().unwrap();
            if let Some(state) = reg.get_mut(&id) {
                state.exited = Some(code);
            }
        });
        Ok(ChildHandle {
            id,
            pid,
            owner: cfg.owner,
            started_ms,
        })
    }

    pub fn kill(&self, id: u64, grace_ms: u64) -> Result<(), Error> {
        let pid = self
            .registry
            .lock()
            .unwrap()
            .get(&id)
            .map(|c| c.pid)
            .ok_or_else(|| Error::not_found(format!("child {id}")))?;
        self.terminate_registered_sync(id, pid, grace_ms)
    }

    /// Kill a process by raw pid (process-group aware); used by MCP/LSP
    /// clients that own their own child lifecycle. The pid MUST belong to a
    /// registered, still-unreaped child: it is then terminated through the
    /// same guarded authority as [`ProcessSupervisor::kill`] (the
    /// containment job on Windows, the reap-serialized process group on
    /// unix). A pid whose child was already consumed by its reaper — or one
    /// this supervisor never owned — is refused typed: a raw pid may be
    /// recycled at any moment, so it is NEVER signalled blindly.
    pub fn kill_child_pid(&self, pid: u32, grace_ms: u64) -> Result<(), Error> {
        if pid == 0 {
            return Err(Error::not_found("pid 0"));
        }
        let (live_id, known) = {
            let reg = self.registry.lock().unwrap();
            (
                reg.iter()
                    .find(|(_, s)| s.pid == pid && s.exited.is_none())
                    .map(|(id, _)| *id),
                reg.values().any(|s| s.pid == pid),
            )
        };
        match (live_id, known) {
            // The row's serial is the authority: if its reaper consumed the
            // child in the meantime, the guarded kill becomes a no-op.
            (Some(id), _) => self.terminate_registered_sync(id, pid, grace_ms),
            (None, true) => Err(Error::not_found(format!(
                "child pid {pid} was already consumed by its reaper; refusing a stale signal"
            ))),
            (None, false) => Err(Error::not_found(format!(
                "pid {pid} is not owned by this supervisor; raw pids are never signalled"
            ))),
        }
    }

    /// Is a raw pid still alive (used by MCP/LSP clients)?
    pub fn pid_alive(&self, pid: u32) -> bool {
        process_alive(pid)
    }

    /// Collect exited children (no zombies).
    pub fn reap(&self) -> Vec<Reaped> {
        let mut out = Vec::new();
        let mut reg = self.registry.lock().unwrap();
        let ids: Vec<u64> = reg.keys().copied().collect();
        for id in ids {
            let state = reg.get(&id).unwrap();
            if let Some(code) = state.exited {
                out.push(Reaped {
                    id,
                    pid: state.pid,
                    exit_code: code,
                    owner: state.owner.clone(),
                });
                reg.remove(&id);
            }
        }
        out
    }

    pub fn alive(&self) -> Vec<ChildHandle> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.exited.is_none())
            .map(|(id, s)| ChildHandle {
                id: *id,
                pid: s.pid,
                owner: s.owner.clone(),
                started_ms: s.started_ms,
            })
            .collect()
    }

    /// Deliberate ownership transfer (spec §22).
    pub fn transfer(&self, id: u64, new_owner: ProcessOwner) -> Result<(), Error> {
        let mut reg = self.registry.lock().unwrap();
        let state = reg
            .get_mut(&id)
            .ok_or_else(|| Error::not_found(format!("child {id}")))?;
        state.owner = new_owner;
        Ok(())
    }

    /// Session death ⇒ its children die (unless transferred first).
    pub fn kill_all_for(&self, owner: ProcessOwner) -> Vec<u64> {
        // Snapshot the targets, terminate outside the registry lock (the
        // Windows terminate path re-locks the registry; the unix grace wait
        // must never serialize other callers), then mark exactly the
        // snapshotted rows exited.
        let targets: Vec<(u64, u32)> = {
            let reg = self.registry.lock().unwrap();
            reg.iter()
                .filter(|(_, s)| s.owner == owner && s.exited.is_none())
                .map(|(id, s)| (*id, s.pid))
                .collect()
        };
        let mut killed = Vec::with_capacity(targets.len());
        for (id, pid) in &targets {
            let _ = self.terminate_registered_sync(*id, *pid, 2000);
            killed.push(*id);
        }
        let mut reg = self.registry.lock().unwrap();
        for (id, _) in &targets {
            if let Some(s) = reg.get_mut(id) {
                if s.exited.is_none() {
                    s.exited = Some(None);
                }
            }
        }
        killed
    }

    pub fn registered(&self) -> usize {
        self.registry.lock().unwrap().len()
    }

    fn mark_exited(&self, id: u64, code: Option<Option<i32>>) {
        let mut reg = self.registry.lock().unwrap();
        if let Some(s) = reg.get_mut(&id) {
            s.exited = code;
            let found = {
                let tl = self.timeline.lock().unwrap();
                tl.iter().any(|t| t.op_id == id)
            };
            if found {
                let mut tl = self.timeline.lock().unwrap();
                if let Some(t) = tl.iter_mut().find(|t| t.op_id == id) {
                    t.exited_ms = Some(now_ms());
                    t.exit_code = code.flatten();
                }
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Canonical refusal wording for DenyAll isolation failures (mirrors the
/// sandbox crate's "sandbox unavailable" phrasing so daemon error text
/// stays consistent).
const DENY_ALL_REFUSAL_PREFIX: &str = "sandbox unavailable: refusing spawn under \
                                       NetworkIsolation::DenyAll";

/// Canonical refusal wording for BrokerOnly isolation failures (same
/// "sandbox unavailable" phrasing, the mode named explicitly). A BrokerOnly
/// request NEVER degrades to a proxy-only child: either the sandbox
/// namespace exists and the child is `setns`'d into it, or the spawn is
/// refused typed.
const BROKER_ONLY_REFUSAL_PREFIX: &str = "sandbox unavailable: refusing spawn under \
                                         NetworkIsolation::BrokerOnly";

/// Prepare the requested per-spawn network isolation BEFORE any process
/// exists. `Inherit`/`DenyAll` need nothing here (DenyAll's pre-exec hook is
/// installed at command build; its failure is classified by
/// [`spawn_failure`]); `BrokerOnly` creates the sandbox namespace + relay
/// bridge, and any failure — no backend on this platform, a non-loopback
/// endpoint, or a refused `unshare` — is a typed permission refusal with no
/// process forked and no downgrade to proxy-only.
fn prepare_network_isolation(
    cfg: &SpawnConfig,
) -> Result<Option<std::sync::Arc<BrokerOnlyBridge>>, Error> {
    match cfg.network_isolation {
        NetworkIsolation::BrokerOnly { endpoint } => {
            #[cfg(target_os = "linux")]
            {
                sandbox::validate_endpoint(endpoint).map_err(|e| {
                    Error::permission(format!(
                        "{BROKER_ONLY_REFUSAL_PREFIX} of `{}` BEFORE spawn: {e}; never running \
                         it unconfined",
                        cfg.cmd
                    ))
                })?;
                let inner = sandbox::Inner::start(endpoint).map_err(|e| {
                    Error::permission(format!(
                        "{BROKER_ONLY_REFUSAL_PREFIX} of `{}` BEFORE spawn: the sandbox \
                         network namespace could not be created ({e}); the BrokerOnly backend \
                         needs CAP_SYS_ADMIN and an empty-network-namespace capable kernel — \
                         never running it unconfined",
                        cfg.cmd
                    ))
                })?;
                Ok(Some(std::sync::Arc::new(BrokerOnlyBridge {
                    endpoint,
                    inner,
                })))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = endpoint;
                Err(Error::permission(format!(
                    "{BROKER_ONLY_REFUSAL_PREFIX} of `{}` BEFORE spawn: this platform \
                     provides no BrokerOnly per-process network-isolation backend (the proxy \
                     flags are application configuration, not confinement); never running it \
                     unconfined",
                    cfg.cmd
                )))
            }
        }
        NetworkIsolation::Inherit | NetworkIsolation::DenyAll => Ok(None),
    }
}

/// Install the BrokerOnly pre-exec `setns` hook on a built command. Called on
/// every spawn entry point; a `None` bridge is the `Inherit`/`DenyAll` path
/// and installs nothing (DenyAll's hook is installed by `command_base`).
fn install_network_isolation(
    cmd: &mut std::process::Command,
    bridge: Option<&std::sync::Arc<BrokerOnlyBridge>>,
) {
    #[cfg(target_os = "linux")]
    if let Some(bridge) = bridge {
        sandbox::install_broker_only_isolation(cmd, bridge.ns_fd());
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (cmd, bridge);
}

/// Pre-spawn fail-closed gate for a DenyAll request. On linux the real
/// backend exists (pre-exec `unshare(CLONE_NEWNET)`) and is ALWAYS
/// attempted — capability heuristics never pre-judge it, only the actual
/// syscall proves or refuses; its failure refuses the spawn typed via
/// [`spawn_failure`]. Every other platform has no backend at all, so the
/// request is refused BEFORE spawn, typed, and no process is forked.
#[cfg(target_os = "linux")]
fn isolation_gate(_cfg: &SpawnConfig) -> Result<(), Error> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn isolation_gate(cfg: &SpawnConfig) -> Result<(), Error> {
    Err(Error::permission(format!(
        "{DENY_ALL_REFUSAL_PREFIX} of `{}` BEFORE spawn: this platform provides no \
         per-process network-isolation backend; never running the child unenforced",
        cfg.cmd
    )))
}

/// Map one spawn() io error. Under a DenyAll request ANY failure to bring
/// the child up ISOLATED is a typed permission refusal (audit 4/28/35-39:
/// never warn-and-run unenforced); `e` carries the OS error — for a
/// pre-exec unshare refusal std transports the raw errno, whose OS message
/// names the kernel/user-namespace denial. All other configs keep the
/// historic not_found mapping.
fn spawn_failure(cfg: &SpawnConfig, e: std::io::Error) -> Error {
    match cfg.network_isolation {
        NetworkIsolation::DenyAll => Error::permission(format!(
            "{DENY_ALL_REFUSAL_PREFIX} of `{}`: the isolated child could not be created \
             ({e}); never running it unenforced",
            cfg.cmd
        )),
        NetworkIsolation::BrokerOnly { .. } => Error::permission(format!(
            "{BROKER_ONLY_REFUSAL_PREFIX} of `{}`: the confined child could not enter its \
             sandbox network namespace ({e}); never running it unconfined",
            cfg.cmd
        )),
        NetworkIsolation::Inherit => Error::not_found(format!("spawn {}: {e}", cfg.cmd)),
    }
}

/// A successful isolated spawn proves its backend active at spawn: the
/// enforcement report may then claim OsLevel (DenyAll) or OsLevelBrokerOnly
/// (BrokerOnly) — see [`platform_network_enforcement`].
#[cfg(target_os = "linux")]
fn mark_network_isolation_proven(cfg: &SpawnConfig) {
    match cfg.network_isolation {
        NetworkIsolation::DenyAll => {
            DENY_ALL_PROVEN.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        NetworkIsolation::BrokerOnly { .. } => {
            BROKER_ONLY_PROVEN.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        NetworkIsolation::Inherit => {}
    }
}

/// A reusable OS-level spawn-confinement hook for command builders outside
/// this crate's supervisor: it installs its confinement on the given command
/// (post-fork, pre-exec). [`deny_all_spawn_confinement`] returns the one
/// this crate ships.
pub type SpawnConfinementHook = Arc<dyn Fn(&mut std::process::Command) + Send + Sync>;

/// The DenyAll spawn-confinement seam for spawn authorities that build
/// their own child command (the interactive PTY launcher): the EXACT
/// `unshare(CLONE_NEWNET)` pre-exec hook the supervisor installs for
/// [`NetworkIsolation::DenyAll`] — one backend, two spawn seams, never a
/// second isolation implementation. The returned hook installs its
/// confinement on the given command (the hook itself runs post-fork,
/// pre-exec).
///
/// `Some` only where a backend exists (Linux). `None` is platform truth —
/// never a silent no-op: a caller holding a `DenyAll` requirement must
/// refuse the spawn typed before any child exists.
#[allow(unsafe_code)]
pub fn deny_all_spawn_confinement() -> Option<SpawnConfinementHook> {
    #[cfg(target_os = "linux")]
    {
        Some(Arc::new(|cmd: &mut std::process::Command| {
            // SAFETY: `apply_deny_all_isolation` installs the documented
            // allocation-free unshare pre-exec hook on `cmd` (post-fork,
            // pre-exec) — the identical hook the supervisor's own DenyAll
            // spawn path installs.
            unsafe {
                sandbox::apply_deny_all_isolation(cmd);
            }
        }))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Record that the DenyAll backend proved itself active at spawn through a
/// seam OUTSIDE this crate's supervisor (the PTY confinement hook): the
/// pre-exec unshare ran on a child that exec'd successfully. The
/// supervisor's own DenyAll spawn paths record this automatically; every
/// external seam that reused [`deny_all_spawn_confinement`] must call this
/// after a successful spawn so [`platform_network_enforcement`] reports the
/// honest state.
pub fn record_deny_all_isolation_proven() {
    #[cfg(target_os = "linux")]
    DENY_ALL_PROVEN.store(true, std::sync::atomic::Ordering::SeqCst);
}

// ------------------------------------------------------------------ windows
// The Windows spawn containment protocol (spec §22, commandment 8): every
// supervised child gets its OWN `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` job,
// is created `CREATE_SUSPENDED`, assigned strictly while it cannot execute,
// has its membership verified before it could fork, and is resumed LAST.
// Tree death is OS-enumerated job termination (explicit kills) plus
// kill-on-close (daemon death) — never a `taskkill` pid walk.

/// Apply the platform's spawn-time containment preparation to a base
/// command: on Windows the process is marked `CREATE_SUSPENDED` (it must
/// not execute one instruction before the containment job owns it); unix
/// needs nothing beyond the process group already applied by
/// [`ProcessSupervisor::command_base`].
#[cfg(windows)]
fn contain_on_create(mut cmd: std::process::Command) -> std::process::Command {
    win_spawn::suspend_on_create(&mut cmd);
    cmd
}

#[cfg(not(windows))]
fn contain_on_create(cmd: std::process::Command) -> std::process::Command {
    cmd
}

/// Create the per-child containment for one spawn, BEFORE any process
/// exists. Windows: its own kill-on-close job (a creation failure refuses
/// the spawn — without the job there is no tree guarantee). Off Windows:
/// nothing to carry (the process group is the authority).
#[cfg(windows)]
fn prepare_containment() -> Result<ChildContainment, Error> {
    prepare_containment_with_limits(None)
}

#[cfg(not(windows))]
fn prepare_containment() -> Result<ChildContainment, Error> {
    Ok(ChildContainment)
}

/// [`prepare_containment`] with the requested tree budgets folded into the
/// Windows Job Object (`PROCESS_MEMORY` / `ACTIVE_PROCESS`) so the limits
/// exist BEFORE the suspended child is resumed. Off Windows the budgets are
/// installed by the pre-exec rlimit plan instead.
#[cfg(windows)]
fn prepare_containment_with_limits(
    budgets: Option<&TreeBudgets>,
) -> Result<ChildContainment, Error> {
    let limits = budgets.map(budget::job_limits_for).unwrap_or_default();
    Ok(ChildContainment {
        job: win_spawn::create_job_with_limits(limits)?,
    })
}

#[cfg(not(windows))]
fn prepare_containment_with_limits(
    _budgets: Option<&TreeBudgets>,
) -> Result<ChildContainment, Error> {
    Ok(ChildContainment)
}

/// Pure Windows containment policy (compiled on every host so the ordering
/// invariants are adversarially testable where no Windows runtime exists —
/// the same pattern as `faktor-pty::win_common`).
#[cfg(any(windows, test))]
mod win_containment_policy {
    /// `CREATE_SUSPENDED` (winbase.h frozen ABI value): the process is
    /// created with its primary thread suspended, so it can neither execute
    /// nor fork a descendant before the job owns it.
    pub(crate) const CREATE_SUSPENDED: u32 = 0x0000_0004;

    /// The creation flags every supervised Windows child is created with:
    /// suspended until the containment job owns it. Nothing else may be
    /// added here without re-justifying the ordering.
    pub(crate) fn creation_flags() -> u32 {
        CREATE_SUSPENDED
    }

    /// The exposure gate: resume a child (and therefore expose it) ONLY
    /// after the strict assignment succeeded AND job membership was
    /// verified while the child was still suspended. Any other combination
    /// refuses the spawn and terminates the still-suspended child.
    pub(crate) fn may_resume(assigned: bool, verified_member: bool) -> bool {
        assigned && verified_member
    }
}

/// The Windows spawn path: create the job, create the process suspended,
/// assign strictly, verify membership while it cannot run, resume LAST.
/// Any failure terminates the still-suspended child and refuses the spawn.
#[cfg(windows)]
mod win_spawn {
    use std::os::windows::process::CommandExt;

    use faktor_winjob::{JobGuard, JobLimits};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SUSPEND_RESUME};

    use super::*;

    // `NtResumeProcess` (ntdll): resume every thread of a process. The
    // kernel transition needed is `ResumeThread`, but
    // `std::process::Command::spawn` closes the primary-thread handle that
    // `CreateProcess` returned and exposes no resume seam; process-wide
    // resume is the user-mode counterpart of the WDK-documented
    // `ZwResumeProcess` and takes only a process handle (opened with
    // `PROCESS_SUSPEND_RESUME`). A just-created suspended process has
    // exactly one thread, so this is precisely "resume the primary thread".
    #[link(name = "ntdll")]
    extern "system" {
        fn NtResumeProcess(process_handle: HANDLE) -> i32;
    }

    /// Create the per-child containment job. Fail-closed: without the job
    /// there is no tree guarantee, so the spawn is refused before any
    /// process exists.
    pub(super) fn create_job() -> Result<JobGuard, Error> {
        JobGuard::create_strict().map_err(|code| {
            Error::internal(format!(
                "windows containment: CreateJobObject(KILL_ON_JOB_CLOSE) failed \
                 (win32 error {code}); refusing the spawn"
            ))
        })
    }

    /// [`create_job`] with the requested Job Object resource limits
    /// (`PROCESS_MEMORY` / `ACTIVE_PROCESS`): the job carries them BEFORE
    /// the suspended child is assigned, so the tree is bounded from its
    /// first instruction.
    pub(super) fn create_job_with_limits(limits: JobLimits) -> Result<JobGuard, Error> {
        JobGuard::create_with_limits_strict(limits).map_err(|code| {
            Error::internal(format!(
                "windows containment: CreateJobObject(memory/process limits) failed \
                 (win32 error {code}); refusing the spawn"
            ))
        })
    }

    /// Mark the command `CREATE_SUSPENDED` (the pure policy owns the flag).
    pub(super) fn suspend_on_create(cmd: &mut std::process::Command) {
        cmd.creation_flags(win_containment_policy::creation_flags());
    }

    /// The assign → verify → resume core, executed while the child is STILL
    /// suspended. Ordering is the containment guarantee:
    /// 1. `assign_strict` — a failed assignment refuses the spawn;
    /// 2. `contains` — membership is verified before any instruction runs,
    ///    so it cannot race a descendant;
    /// 3. resume — the ONLY exposure point, gated by the pure
    ///    [`win_containment_policy::may_resume`] predicate.
    fn assign_verify_resume(job: &JobGuard, pid: u32) -> Result<(), Error> {
        if pid == 0 {
            return Err(Error::internal(
                "windows containment: spawned child has no pid; refusing the spawn",
            ));
        }
        job.assign_strict(pid).map_err(|code| {
            Error::internal(format!(
                "windows containment: AssignProcessToJobObject(pid {pid}) failed \
                 (win32 error {code}); the suspended child is terminated, never resumed \
                 uncontained"
            ))
        })?;
        // The child is still suspended: this check cannot race a descendant.
        let verified_member = job.contains(pid);
        if !win_containment_policy::may_resume(true, verified_member) {
            return Err(Error::internal(format!(
                "windows containment: pid {pid} is not a job member after assignment; \
                 the suspended child is terminated, never resumed uncontained"
            )));
        }
        resume(pid).map_err(|code| {
            Error::internal(format!(
                "windows containment: NtResumeProcess(pid {pid}) failed (status {code}); \
                 the child is terminated"
            ))
        })
    }

    #[allow(unsafe_code)]
    fn resume(pid: u32) -> Result<(), u32> {
        // SAFETY: `pid` names the just-created, still-suspended child; the
        // handle is closed on every path.
        unsafe {
            let process = OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid);
            if process.is_null() {
                return Err(GetLastError());
            }
            let status = NtResumeProcess(process);
            CloseHandle(process);
            if status < 0 {
                Err(status as u32)
            } else {
                Ok(())
            }
        }
    }

    /// Contain an already-created std child. On failure the child is killed
    /// while still suspended (`TerminateProcess` works on a suspended
    /// process; the primary thread never runs) and the error is returned.
    pub(super) fn assign_and_resume_std(
        job: &JobGuard,
        child: &mut std::process::Child,
    ) -> Result<(), Error> {
        let pid = child.id();
        match assign_verify_resume(job, pid) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    /// tokio twin of [`assign_and_resume_std`].
    pub(super) async fn assign_and_resume_tokio(
        job: &JobGuard,
        child: &mut tokio::process::Child,
    ) -> Result<(), Error> {
        let pid = child.id().unwrap_or(0);
        match assign_verify_resume(job, pid) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Err(e)
            }
        }
    }
}

/// Is the pid still alive (zombies do not count)?
#[cfg(not(unix))]
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // kill(pid, 0) probes existence natively (no /bin/ps); zombies count as
    // existing (they are reaped by the caller's waitpid/child.wait).
    // SAFETY: `pid` was checked non-zero above and fits `i32` (kernel pids
    // are `pid_t`); signal 0 only probes existence and never delivers a
    // signal, so a recycled id can at worst report "alive".
    let r = unsafe { libc::kill(pid as i32, 0) };
    if r == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return false;
        }
    }
    true
}

/// Wait (bounded) for process group `pgid` to be gone. The terminal crate is
/// the process-lifecycle authority, so the bounded wait belongs here (callers
/// like the cold-runner tests must not grow their own polling sleeps). A
/// SIGKILLed group can stay observable for a scheduling quantum while its
/// members are consumed by the single reaper; the wait observes extinction,
/// it does not force it.
#[cfg(unix)]
pub fn wait_for_group_gone(pgid: u32, timeout: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        if group_gone(pgid) {
            return true;
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return group_gone(pgid);
        }
        std::thread::sleep(std::time::Duration::from_millis(20).min(timeout - elapsed));
    }
}

/// True when the `kill(-pgid, 0)` probe reports no member of the process
/// group. The probe NEVER reaps (audit round 12: one reaping authority per
/// child), so zombie members keep it reporting "not gone" until the single
/// reaper consumes them — that can only delay an early return, never cause a
/// wrong signal. A stubborn child that survived SIGTERM keeps the group
/// alive and forces the SIGKILL escalation (audit round 5: leader-gone !=
/// group-gone). Note the probe is liveness evidence only: a "not gone"
/// answer does NOT identify the group as ours once the leader was consumed
/// (the pgid may have been recycled), which is why it is used solely for
/// early return AND why every signal is refused after the reap.
#[cfg(unix)]
#[allow(unsafe_code)]
fn group_gone(pgid: u32) -> bool {
    if pgid == 0 {
        return true;
    }
    #[cfg(all(test, unix))]
    if let Some(simulated) = group_probe_injection::probe(pgid) {
        return simulated;
    }
    // SAFETY: `pgid` was validated non-zero above, so the negation cannot be
    // 0 or overflow; signal 0 only probes group existence and never delivers
    // a signal (an out-of-range group id can at worst report "not found").
    let r = unsafe { libc::kill(-(pgid as i32), 0) };
    if r == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return true; // no members at all
        }
        // EPERM: the group exists but belongs to another user — treat as alive.
    }
    false
}

/// Best-effort process-tree kill for a pid this supervisor does NOT own (no
/// containment row exists, so there is no job to close): `taskkill /T`.
/// Deliberately an afterthought — it is never the primary guarantee and
/// every supervised Windows child is terminated through its own
/// kill-on-close Job Object instead (OS-enumerated membership, no pid walk,
/// no window where a recycled pid could be hit).
#[cfg(not(unix))]
fn taskkill_best_effort(pid: u32) {
    if pid == 0 {
        return;
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Kill the whole process group on unix: SIGTERM, grace, SIGKILL. The
/// grace wait exits early: the moment the group leader is gone the function
/// returns instead of sleeping the full grace.
///
/// TEST-ONLY raw helper: production kill paths for REGISTERED children must
/// go through [`ReapState::try_signal_group`], which holds the reap serial
/// across the observation/signal pair; this raw form has no reap state and
/// is exercised only by the budget tests, which own their unreaped fixture
/// children directly. On Windows the primary tree kill is the containment
/// job ([`ChildContainment::terminate`]).
#[allow(unsafe_code)]
#[cfg(all(test, unix))]
fn kill_group(pid: u32, grace_ms: u64) -> Result<(), Error> {
    #[cfg(unix)]
    {
        // SAFETY: `pid` is the group leader of a supervisor-spawned `setsid`
        // child (pids fit `pid_t`), so the negative id addresses exactly that
        // process group; SIGTERM is the cooperative first move and a stale
        // group id can at worst fail with ESRCH, which the caller handles.
        let sigterm = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        if sigterm != 0 {
            return Err(Error::internal(format!("kill TERM {pid}")));
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(grace_ms);
        while std::time::Instant::now() < deadline {
            if group_gone(pid) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // A stubborn descendant survived SIGTERM: SIGKILL the whole group.
        // SAFETY: same group id as the SIGTERM above (established by the
        // supervisor's setsid spawn); SIGKILL is the documented escalation
        // after the grace window, and the ignored error can only be ESRCH.
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    {
        let _ = grace_ms;
        taskkill_best_effort(pid);
    }
    Ok(())
}

/// Async kill of the whole process group on unix: SIGTERM, then poll for
/// exit every 25ms (no blocking sleep inside the runtime); at the grace
/// deadline SIGKILL is sent. On Windows the primary tree kill is the
/// containment job; this async form delegates to the best-effort
/// [`taskkill_best_effort`].
///
/// TEST-ONLY raw helper (audit: this was public production API raw-signalling
/// an arbitrary negative pgid): it has no reap state, so it must never be
/// reachable from production code. The only callers are this crate's own
/// tests, which own their unreaped fixture children; every production kill
/// path for a registered child goes through the guarded
/// [`ReapState::try_signal_group`] (or the child's containment job).
#[allow(unsafe_code)]
#[cfg(all(test, unix))]
async fn kill_group_async(pid: u32, grace_ms: u64) -> Result<(), Error> {
    #[cfg(unix)]
    {
        // SAFETY: `pid` is the group leader of a supervisor-spawned `setsid`
        // child (pids fit `pid_t`), so the negative id addresses exactly that
        // process group; SIGTERM is the cooperative first move and a stale
        // group id can at worst fail with ESRCH, which the caller handles.
        let sigterm = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        if sigterm != 0 {
            return Err(Error::internal(format!("kill TERM {pid}")));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(grace_ms);
        loop {
            if group_gone(pid) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // A stubborn descendant survived SIGTERM: SIGKILL the whole group.
        // SAFETY: same group id as the SIGTERM above (established by the
        // supervisor's setsid spawn); SIGKILL is the documented escalation
        // after the grace window, and the ignored error can only be ESRCH.
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    {
        let _ = grace_ms;
        taskkill_best_effort(pid);
    }
    Ok(())
}

/// 200-line ring buffer (spec §23).
#[derive(Debug, Clone)]
pub struct RingBuffer {
    lines: std::collections::VecDeque<String>,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, line: String) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn excerpt(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            out.push_str(l);
            out.push('\n');
        }
        out
    }

    /// Error-ish lines (bounded) for the excerpt.
    pub fn error_lines(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter(|l| {
                let lower = l.to_ascii_lowercase();
                lower.contains("error")
                    || lower.contains("panic")
                    || lower.contains("failed")
                    || lower.contains("warning:")
            })
            .take(20)
            .cloned()
            .collect()
    }
}

// Every test below drives the process-group/signal machinery (setsid
// children, /bin/sh scripts, SIGTERM grace, /bin/ps probes) and only runs
// on unix hosts; the Windows certification suite lives in `windows_tests`.
#[cfg(all(test, unix))]
#[path = "tests.rs"]
mod tests;

// ================================================================ windows
/// On Windows every supervised child gets its OWN faktor-winjob JobGuard
/// (`KILL_ON_JOB_CLOSE`), created before the process and assigned while the
/// child is `CREATE_SUSPENDED` — so the OS owns the whole tree before the
/// child can execute. Daemon death closes the jobs (kill-on-close); explicit
/// kills terminate the job (OS-enumerated membership). macOS/Linux keep
/// process groups + signals.
#[cfg(windows)]
pub use faktor_winjob::JobGuard;

// ================================================================ windows tests
// P0-59 process-tree certification through the REAL windows spawn path of
// this crate: every child is spawned CREATE_SUSPENDED into its own
// KILL_ON_JOB_CLOSE job (assign_strict + membership verification before
// resume); cancel/kill terminate the job and dropping the supervisor closes
// it (kill-on-close). taskkill is only a best-effort fallback for pids the
// supervisor never owned. Runtime-certification only on a windows host — on
// unix hosts this module does not exist.
#[cfg(all(test, windows))]
mod windows_tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    use super::*;
    use faktor_core::error::ErrorKind;

    #[allow(unsafe_code)]
    fn pid_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        // SAFETY: Win32: every handle/pointer passed here is live, initialized, and owned by this function per the documented call contract; results are checked and owned handles closed exactly once.
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let running = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
            CloseHandle(handle);
            running
        }
    }

    fn wait_until<F: FnMut() -> bool>(what: &str, limit: Duration, mut cond: F) {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out after {limit:?} waiting for {what}");
    }

    /// powershell (direct child) sleeps 60 s; the ping grandchild is born
    /// ~1.5 s in — after spawn/resume returned, and since the direct child
    /// was assigned to its job WHILE SUSPENDED, the grandchild lands in the
    /// job by descent — writes its pid, and sleeps ~60 s. `ping -n 60` is a
    /// deterministic ~60 s sleeper even on a network-blocked runner (ICMP
    /// failure still paces the retries).
    fn sleeper_tree_script(pid_file: &Path) -> String {
        // Proven-correct on CI (mirrors the pty lifecycle suite): absolute
        // system ping path (no PATH reliance under a hidden window) and an
        // ascii Set-Content write.
        format!(
            "Start-Sleep -Milliseconds 1500; \
             $ping = Join-Path $env:SystemRoot 'System32\\ping.exe'; \
             $p = Start-Process -FilePath $ping -ArgumentList '-n','60','127.0.0.1' \
                 -WindowStyle Hidden -PassThru; \
             Set-Content -Path '{}' -Value ([string]$p.Id) -Encoding ascii; \
             Start-Sleep -Seconds 60",
            pid_file.display()
        )
    }

    fn tree_cfg(pid_file: &Path) -> SpawnConfig {
        SpawnConfig {
            cmd: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                sleeper_tree_script(pid_file).into(),
            ],
            cwd: std::env::temp_dir(),
            env: EnvSpec::default_baseline(),
            owner: ProcessOwner::Daemon,
            capture: false, // no pipe drama: the tree is killed, not drained
            artifact_max: 1024 * 1024,
            network_isolation: NetworkIsolation::Inherit,
        }
    }

    fn supervisor_with_tree(
        dir: &tempfile::TempDir,
        pid_file: &Path,
    ) -> (Arc<ProcessSupervisor>, SpawnConfig) {
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        (sup, tree_cfg(pid_file))
    }

    fn wait_for_grandchild(pid_file: &Path) -> u32 {
        wait_until("grandchild pid file", Duration::from_secs(60), || {
            pid_file.exists()
        });
        std::fs::read_to_string(pid_file)
            .expect("grandchild pid file readable")
            .trim()
            .parse()
            .expect("grandchild pid file holds a pid")
    }

    /// The task-cancellation path (run + CancellationToken) must kill the
    /// whole supervised tree: direct powershell child AND ping grandchild,
    /// through the child's kill-on-close job (OS-enumerated membership).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_cancellation_kills_the_whole_supervised_tree() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("gc.pid");
        let (sup, cfg) = supervisor_with_tree(&dir, &pid_file);
        let token = CancellationToken::new();

        let sup2 = sup.clone();
        let token2 = token.clone();
        let task =
            tokio::spawn(async move { sup2.run(cfg, Duration::from_secs(120), token2).await });

        // The direct child pid is registered + timeline-logged once run()
        // spawns; poll the timeline instead of guessing.
        let direct = wait_for_direct_pid(&sup, Duration::from_secs(20));
        let grandchild = wait_for_grandchild(&pid_file);
        assert!(
            pid_alive(direct) && pid_alive(grandchild),
            "parent + grandchild must be alive before cancellation"
        );

        token.cancel();
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");

        wait_until("cancelled tree death", Duration::from_secs(10), || {
            !pid_alive(direct) && !pid_alive(grandchild)
        });
    }

    fn wait_for_direct_pid(sup: &ProcessSupervisor, limit: Duration) -> u32 {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(t) = sup.recent_spawns().first() {
                if t.pid > 0 {
                    return t.pid;
                }
            }
            assert!(Instant::now() < deadline, "run() must spawn the child");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Daemon-crash semantics end-to-end: dropping the LAST supervisor
    /// reference kills the live tree — the per-child job is terminated on
    /// the drop path and its handle closes right after (kill-on-close), so
    /// the OS takes every remaining member with no taskkill involved.
    #[test]
    fn dropping_the_supervisor_kills_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("gc2.pid");
        let (direct, grandchild) = {
            let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
            let sup = ProcessSupervisor::new(cas);
            let cfg = tree_cfg(&pid_file);
            let handle = sup.spawn(cfg).expect("supervised spawn");
            let grandchild = wait_for_grandchild(&pid_file);
            assert!(pid_alive(grandchild), "grandchild must be alive pre-drop");
            (handle.pid, grandchild) // sup drops here: daemon crash
        };

        wait_until("drop-killed tree death", Duration::from_secs(10), || {
            !pid_alive(direct) && !pid_alive(grandchild)
        });
    }

    /// Suspended-assign ordering + descendant membership before resume: the
    /// script starts a `-WindowStyle Hidden` ping as its FIRST action (the
    /// hidden window gives ping its OWN console, so ONLY job membership can
    /// reach it) and writes the pid. `CREATE_SUSPENDED` → assign_strict →
    /// membership check → resume makes the direct child a job member before
    /// it executes one instruction, so the grandchild is contained by
    /// descent; a spawn-then-assign race lets it escape. `sup.kill`
    /// terminates that job and BOTH die (no taskkill pid walk).
    #[test]
    fn immediate_detached_grandchild_is_a_job_member_before_resume() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("imm.pid");
        let (sup, mut cfg) = supervisor_with_tree(&dir, &pid_file);
        cfg.args[3] = format!(
            "$ping = Join-Path $env:SystemRoot 'System32\\ping.exe'; \
             $p = Start-Process -FilePath $ping -ArgumentList '-n','60','127.0.0.1' \
                 -WindowStyle Hidden -PassThru; \
             Set-Content -Path '{}' -Value ([string]$p.Id) -Encoding ascii; \
             Start-Sleep -Seconds 60",
            pid_file.display()
        );
        let handle = sup.spawn(cfg).expect("supervised spawn");
        let grandchild = wait_for_grandchild(&pid_file);
        assert!(
            pid_alive(handle.pid) && pid_alive(grandchild),
            "direct child + immediate detached grandchild must be alive before the kill"
        );
        sup.kill(handle.id, 500).expect("containment kill");
        wait_until("job-terminated tree death", Duration::from_secs(10), || {
            !pid_alive(handle.pid) && !pid_alive(grandchild)
        });
    }

    /// One containment job per CHILD (never one shared job): killing one
    /// supervised tree terminates exactly that child's job — the other
    /// tree, detached grandchild included, keeps running until its own job
    /// is terminated.
    #[test]
    fn kill_takes_only_the_target_childs_job() {
        let dir = tempfile::tempdir().unwrap();
        let pid_a = dir.path().join("a.pid");
        let pid_b = dir.path().join("b.pid");
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let a = sup.spawn(tree_cfg(&pid_a)).expect("tree a");
        let b = sup.spawn(tree_cfg(&pid_b)).expect("tree b");
        let ga = wait_for_grandchild(&pid_a);
        let gb = wait_for_grandchild(&pid_b);
        assert!(pid_alive(a.pid) && pid_alive(ga) && pid_alive(b.pid) && pid_alive(gb));

        sup.kill(a.id, 500).expect("kill tree a");
        wait_until("target tree death", Duration::from_secs(10), || {
            !pid_alive(a.pid) && !pid_alive(ga)
        });
        assert!(
            pid_alive(b.pid) && pid_alive(gb),
            "killing one child's job must never touch another child's tree"
        );
        sup.kill(b.id, 500).expect("cleanup tree b");
    }

    /// Exited-leader containment: the direct child starts a detached
    /// grandchild and exits immediately. `reap()` drops the leader's row —
    /// and with it the job handle — so kill-on-close takes the descendant.
    /// A taskkill/pid-walk kill cannot do this: the tree's root pid is
    /// already gone when the descendant must die.
    #[test]
    fn reap_kills_the_descendants_of_an_exited_leader() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("orphan.pid");
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let mut cfg = tree_cfg(&pid_file);
        cfg.args[3] = format!(
            "$ping = Join-Path $env:SystemRoot 'System32\\ping.exe'; \
             $p = Start-Process -FilePath $ping -ArgumentList '-n','60','127.0.0.1' \
                 -WindowStyle Hidden -PassThru; \
             Set-Content -Path '{}' -Value ([string]$p.Id) -Encoding ascii",
            pid_file.display()
        );
        let handle = sup.spawn(cfg).expect("supervised spawn");
        let grandchild = wait_for_grandchild(&pid_file);
        assert!(
            pid_alive(grandchild),
            "detached descendant alive before reap"
        );
        wait_until(
            "exited leader is collectible",
            Duration::from_secs(20),
            || !sup.reap().is_empty(),
        );
        assert!(!pid_alive(handle.pid), "leader exited");
        wait_until(
            "kill-on-close descendant death after reap",
            Duration::from_secs(10),
            || !pid_alive(grandchild),
        );
    }

    // --------------- platform-default shell through the supervisor -------

    /// Lower a user/model snippet through the typed command authority and
    /// run it through the real supervisor. On Windows
    /// [`ShellKind::PlatformDefault`] must resolve to cmd.exe — never a
    /// Git-Bash `sh`.
    fn shell_cfg(script: &str) -> SpawnConfig {
        let resolved = CommandSpec::shell(script, ShellKind::PlatformDefault)
            .lower()
            .expect("platform-default shell must resolve");
        SpawnConfig {
            cmd: resolved.program.to_string_lossy().into_owned(),
            args: resolved
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            cwd: std::env::temp_dir(),
            env: EnvSpec::default_baseline(),
            owner: ProcessOwner::Daemon,
            capture: true,
            artifact_max: 1024 * 1024,
            network_isolation: NetworkIsolation::Inherit,
        }
    }

    fn shell_supervisor() -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        (dir, ProcessSupervisor::new(cas))
    }

    /// Every shell round-trip assertion names its case and dumps both
    /// bounded heads (tails): a Windows CI failure must be root-causable
    /// from the message alone — exit code, timeout flag, stdout/stderr.
    fn case_tails(out: &SyncRunOutput) -> String {
        let tail = |s: &str| {
            let bytes = s.as_bytes();
            let start = bytes.len().saturating_sub(400);
            String::from_utf8_lossy(&bytes[start..]).into_owned()
        };
        format!(
            "exit_code={:?} timed_out={} stdout_truncated={} stderr_truncated={} \
             stdout_tail={:?} stderr_tail={:?}",
            out.exit_code,
            out.timed_out,
            out.stdout_truncated,
            out.stderr_truncated,
            tail(&out.stdout_head),
            tail(&out.stderr_head),
        )
    }

    /// Run one shell case and assert the success contract with the case
    /// label + heads attached to the failure.
    fn shell_case(
        sup: &ProcessSupervisor,
        label: &str,
        cfg: SpawnConfig,
        deadline: Duration,
    ) -> SyncRunOutput {
        let out = sup
            .run_sync(cfg, deadline, 64 * 1024, 64 * 1024)
            .unwrap_or_else(|e| panic!("[{label}] run failed: {e:?}"));
        let tails = case_tails(&out);
        assert!(!out.timed_out, "[{label}] must not time out: {tails}");
        assert_eq!(
            out.exit_code,
            Some(0),
            "[{label}] expected success: {tails}"
        );
        out
    }

    #[test]
    fn platform_default_shell_is_cmd_exe_not_git_bash() {
        let resolved = CommandSpec::shell("echo hi", ShellKind::PlatformDefault)
            .lower()
            .unwrap();
        assert_eq!(resolved.program, std::ffi::OsString::from("cmd.exe"));
        assert!(
            !resolved
                .program
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("bash"),
            "the default shell must never be Git Bash: {:?}",
            resolved.program
        );
        assert_eq!(resolved.args[0], std::ffi::OsString::from("/d"));
        assert_eq!(resolved.args[1], std::ffi::OsString::from("/c"));
        // The script is MATERIALIZED: cmd is handed a path, never the raw
        // embedded-quote snippet (whose /C quote stripping mangled
        // `echo "a b"` into exit 1 / empty stdout).
        let script = std::path::PathBuf::from(&resolved.args[2]);
        assert!(
            script
                .file_name()
                .map(|n| n.to_string_lossy().starts_with("faktor-cmd-"))
                .unwrap_or(false),
            "the cmd form must hand over a materialized script path: {:?}",
            resolved.args
        );
        assert!(
            !resolved
                .args
                .iter()
                .any(|a| a.to_string_lossy().contains("echo hi")),
            "the raw script must never ride on the cmd command line: {:?}",
            resolved.args
        );
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn materialized_cmd_script_is_deleted_after_the_run() {
        // The runner owns the materialized script: after the supervised run
        // the temp `.cmd` file must be gone (cmd reads it while executing,
        // so deletion happens only once the child has exited).
        let (_dir, sup) = shell_supervisor();
        let resolved = CommandSpec::shell("echo cleanup-check", ShellKind::PlatformDefault)
            .lower()
            .unwrap();
        let script = std::path::PathBuf::from(&resolved.args[2]);
        assert!(script.exists(), "lowering materializes the script");
        let cfg = SpawnConfig {
            cmd: resolved.program.to_string_lossy().into_owned(),
            args: resolved
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            cwd: std::env::temp_dir(),
            env: EnvSpec::default_baseline(),
            owner: ProcessOwner::Daemon,
            capture: true,
            artifact_max: 1024 * 1024,
            network_isolation: NetworkIsolation::Inherit,
        };
        let out = sup
            .run_sync(cfg, Duration::from_secs(20), 64 * 1024, 64 * 1024)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stderr_head);
        assert!(
            out.stdout_head.contains("cleanup-check"),
            "{:?}",
            out.stdout_head
        );
        assert!(
            !script.exists(),
            "the supervisor must delete the run's materialized cmd script"
        );
    }

    #[test]
    fn shell_echo_quoted_spaces_unicode_and_exit_codes_round_trip() {
        let (_dir, sup) = shell_supervisor();
        let out = shell_case(
            &sup,
            "cmd echo",
            shell_cfg("echo hello-from-cmd"),
            Duration::from_secs(20),
        );
        assert!(
            out.stdout_head.contains("hello-from-cmd"),
            "[cmd echo] stdout must carry the echo: {}",
            case_tails(&out)
        );

        let out = shell_case(
            &sup,
            "cmd echo quoted spaces",
            shell_cfg("echo \"a b\""),
            Duration::from_secs(20),
        );
        assert!(
            out.stdout_head.contains("a b"),
            "[cmd echo quoted spaces] stdout must carry the quoted text: {}",
            case_tails(&out)
        );

        // Unicode through PowerShell: the lowered `-Command` script carries
        // the terminal-wide UTF-8 prelude (see
        // `faktor_core::command::POWERSHELL_UTF8_PRELUDE`), so the captured
        // bytes decode as UTF-8. A loaded Windows runner can take tens of
        // seconds to cold-start PowerShell under the parallel tree tests
        // (the pty/fault suites budget 60 s), so this case uses the same
        // proven budget.
        let resolved = CommandSpec::shell("Write-Output '日本語'", ShellKind::PowerShell)
            .lower()
            .unwrap();
        let cfg = SpawnConfig {
            cmd: resolved.program.to_string_lossy().into_owned(),
            args: resolved
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            cwd: std::env::temp_dir(),
            env: EnvSpec::default_baseline(),
            owner: ProcessOwner::Daemon,
            capture: true,
            artifact_max: 1024 * 1024,
            network_isolation: NetworkIsolation::Inherit,
        };
        let out = shell_case(
            &sup,
            "powershell unicode UTF-8",
            cfg,
            Duration::from_secs(60),
        );
        assert!(
            out.stdout_head.contains("日本語"),
            "[powershell unicode UTF-8] the UTF-8 prelude must make 日本語 \
             round-trip (only real UTF-8 bytes decode to it): {}",
            case_tails(&out)
        );

        // `exit /b 7` inside the materialized script must propagate exactly
        // through `cmd.exe /d /c <path>` (the batch's exit code becomes
        // cmd.exe's exit code).
        let out = sup
            .run_sync(
                shell_cfg("exit /b 7"),
                Duration::from_secs(20),
                64 * 1024,
                64 * 1024,
            )
            .unwrap_or_else(|e| panic!("[cmd exit /b 7] run failed: {e:?}"));
        assert!(
            !out.timed_out,
            "[cmd exit /b 7] must not time out: {}",
            case_tails(&out)
        );
        assert_eq!(
            out.exit_code,
            Some(7),
            "[cmd exit /b 7] the batch exit code must propagate exactly: {}",
            case_tails(&out)
        );
    }

    #[test]
    fn deadline_kills_the_windows_shell_tree() {
        let (_dir, sup) = shell_supervisor();
        let out = sup
            .run_sync(
                shell_cfg("ping -n 60 127.0.0.1"),
                Duration::from_millis(500),
                64 * 1024,
                64 * 1024,
            )
            .unwrap();
        assert!(out.timed_out, "the deadline must dominate: {out:?}");
        assert!(
            sup.alive().is_empty(),
            "no live child after the timeout kill"
        );
    }
}

// ============================================== containment policy (portable)
// The Windows containment ordering policy and the supervisor's kill-path
// source contract, testable on EVERY host (the runtime rows above are
// windows-only). Pure policy functions mirror the values the Windows spawn
// path applies; the source scans lock the protocol order and the removal of
// taskkill from the primary kill paths.
#[cfg(test)]
mod containment_policy_tests {
    use super::*;

    /// Extract one item's `{...}` body by brace matching. Format-string
    /// braces are balanced, so the scan is exact for these items.
    fn body<'a>(src: &'a str, header: &str) -> &'a str {
        let start = src
            .find(header)
            .unwrap_or_else(|| panic!("source has no {header}"));
        let open = start + src[start..].find('{').expect("item body");
        let mut depth = 0usize;
        for (i, b) in src.as_bytes()[open..].iter().enumerate() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &src[open..open + i + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("unclosed item {header}");
    }

    /// Windows creation is suspended until the job owns the child: the pure
    /// flag function is the ONLY source of the creation flags, and it is
    /// exactly CREATE_SUSPENDED (0x4, winbase.h).
    #[test]
    fn windows_creation_is_suspended_until_assigned() {
        assert_eq!(
            win_containment_policy::creation_flags(),
            win_containment_policy::CREATE_SUSPENDED
        );
        assert_eq!(win_containment_policy::CREATE_SUSPENDED, 0x0000_0004);
    }

    /// Exposure is legal only with BOTH a successful strict assignment and a
    /// verified membership; every other combination refuses the spawn.
    #[test]
    fn exposure_requires_assignment_and_verified_membership() {
        assert!(win_containment_policy::may_resume(true, true));
        assert!(!win_containment_policy::may_resume(false, true));
        assert!(!win_containment_policy::may_resume(true, false));
        assert!(!win_containment_policy::may_resume(false, false));
    }

    /// Source contract of the Windows spawn path: `assign_verify_resume`
    /// executes the one legal order — assign_strict → membership check →
    /// resume gate → resume — and every spawn path suspends on create and
    /// prepares the job before spawning.
    #[test]
    fn windows_spawn_source_orders_assign_verify_resume() {
        let src = include_str!("lib.rs");
        let inner = body(src, "fn assign_verify_resume(");
        let i_assign = inner.find("assign_strict(").expect("strict assignment");
        let i_member = inner
            .find("contains(pid)")
            .expect("membership verification while suspended");
        let i_gate = inner.find("may_resume(").expect("exposure gate");
        let i_resume = inner.find("resume(pid)").expect("resume");
        assert!(
            i_assign < i_member,
            "membership must be verified after assignment"
        );
        assert!(
            i_member < i_gate,
            "the resume gate must follow membership verification"
        );
        assert!(i_gate < i_resume, "resume must be the last step");
        // The suspension hook is the job-preparation path's child, applied
        // before spawn on every entry point. (Scan the production source
        // only: this test's own literals must not count.)
        let production = src
            .split("mod containment_policy_tests")
            .next()
            .expect("test module split");
        let contain = body(production, "fn contain_on_create(mut cmd:");
        assert!(contain.contains("suspend_on_create"));
        // Every spawn entry point suspends on create: run/run_sync through
        // the budget-aware command builder, spawn/detached through the plain
        // env builder. The budget-aware form must still be contain_on_create.
        let contain_calls = production
            .matches("contain_on_create(self.command(&cfg))")
            .count()
            + production
                .matches("contain_on_create(self.command_with_budgets(&cfg, budgets.as_ref()))")
                .count();
        assert_eq!(
            contain_calls, 4,
            "every spawn entry point (run/run_sync/spawn/detached) must suspend on create"
        );
        // Every spawn entry point prepares the containment job (with budget
        // limits where requested) before spawning.
        let prepare_calls = production.matches("prepare_containment()?").count()
            + production
                .matches("prepare_containment_with_limits(budgets.as_ref())?")
                .count();
        assert_eq!(
            prepare_calls, 4,
            "every spawn entry point must prepare the containment job before spawning"
        );
    }

    /// Kill-path contract: no supervisor kill path references taskkill. The
    /// primary authority is the per-child containment job (Windows) or the
    /// process group (unix); taskkill survives exactly once, as the
    /// documented best-effort helper for pids the supervisor never owned.
    #[test]
    fn supervisor_kill_paths_do_not_depend_on_taskkill() {
        let src = include_str!("lib.rs");
        let production = src
            .split("mod containment_policy_tests")
            .next()
            .expect("test module split");
        for header in [
            "async fn terminate_registered(",
            "fn terminate_registered_sync(",
            "pub fn kill(",
            "pub fn kill_all_for(",
            "impl Drop for ProcessSupervisor {",
        ] {
            let inner = body(production, header);
            assert!(
                !inner.contains("taskkill"),
                "{header} must not fall back to taskkill"
            );
            assert!(
                inner.contains("terminate_containment")
                    || inner.contains("terminate_registered")
                    || inner.contains("kill_group_async"),
                "{header} must terminate through the containment authority"
            );
        }
        assert_eq!(
            production.matches("Command::new(\"taskkill\")").count(),
            1,
            "taskkill must survive only as the single best-effort helper"
        );
        assert!(body(production, "fn taskkill_best_effort(").contains("taskkill"));
        // kill_child_pid routes a registered pid through the guarded kill;
        // an unowned or already-consumed pid is refused typed and never
        // raw-signalled.
        let kcp = body(production, "pub fn kill_child_pid(");
        assert!(kcp.contains("terminate_registered_sync"));
        assert!(!kcp.contains("kill_group"));
        assert!(!kcp.contains("libc::kill"));
        assert!(body(production, "fn kill_group(").contains("taskkill_best_effort"));
    }

    /// Pid-reuse guard contract (portable source scan): the unix signal
    /// issuance locks the reap serial BEFORE observing `reaped` and only
    /// calls the kernel after both, and no supervisor kill path raw-signals
    /// a stored pid anymore.
    #[test]
    fn unix_signals_are_reap_serialized_and_kill_paths_are_guarded() {
        let src = include_str!("lib.rs");
        let production = src
            .split("mod containment_policy_tests")
            .next()
            .expect("test module split");
        let issue = body(production, "fn try_signal_group(");
        let i_serial = issue.find(".serial").expect("reap serial lock");
        let i_reaped = issue.find("self.reaped()").expect("reaped observation");
        let i_kill = issue.find("libc::kill").expect("guarded signal");
        assert!(
            i_serial < i_reaped && i_reaped < i_kill,
            "the reap serial must cover [observe reaped → signal]"
        );
        for header in [
            "async fn terminate_registered(",
            "fn terminate_registered_sync(",
            "pub fn kill_child_pid(",
            "pub fn kill_all_for(",
            "impl Drop for ProcessSupervisor {",
        ] {
            let inner = body(production, header);
            assert!(
                !inner.contains("libc::kill"),
                "{header} must signal only through the guarded issuance"
            );
            assert!(
                !inner.contains("kill_group(") && !inner.contains("kill_group_async("),
                "{header} must not raw-signal a stored pid"
            );
        }
        // The raw unix group helpers are test-only: production registered
        // kills route through ReapState, so a raw negative pgid is never
        // signalled by production API.
        assert!(production.contains("#[cfg(all(test, unix))]\nfn kill_group("));
        assert!(production.contains("#[cfg(all(test, unix))]\nasync fn kill_group_async("));
    }
}
