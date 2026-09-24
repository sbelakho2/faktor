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
            #[allow(unused_mut)]
            let mut child = cmd.spawn().map_err(|e| spawn_failure(&cfg, e))?;
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
            #[allow(unused_mut)]
            let mut child = cmd.spawn().map_err(|e| spawn_failure(&cfg, e))?;
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
mod tests {
    use super::*;
    use faktor_core::error::ErrorKind;
    use std::ffi::OsString;
    use tempfile::tempdir;

    fn supervisor() -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        (dir, ProcessSupervisor::new(cas))
    }

    fn supervisor_with_limit(limit: usize) -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        (dir, ProcessSupervisor::with_limit(cas, limit))
    }

    #[allow(unsafe_code)]
    fn pid_is_gone(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // SAFETY: test helper; `pid` comes from a spawned child (non-zero,
            // fits `pid_t`) and signal 0 only probes liveness — no signal is
            // delivered, so a recycled id can at worst extend a test poll.
            let r = unsafe { libc::kill(pid as i32, 0) };
            if r == -1 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    return true;
                }
            }
            false
        }
        #[cfg(not(unix))]
        {
            !ps_alive(pid)
        }
    }

    /// The browser env allowlist path (spec §9): only the allowlisted daemon
    /// names resolve (plus the universal `GIT_TERMINAL_PROMPT=0`), a
    /// secret-shaped caller entry is a typed refusal (never a silent drop),
    /// and the allowlist itself can never name a denied variable.
    #[test]
    fn browser_env_spec_is_allowlisted_and_refuses_denied_names() {
        for name in BROWSER_ENV_ALLOWLIST {
            assert!(
                !faktor_core::command::env_name_is_denied(std::ffi::OsStr::new(name)),
                "the browser allowlist must never name a denied variable: {name}"
            );
        }
        std::env::set_var("FAKTOR_TEST_BROWSER_PATH", "/tmp/faktor-ci-browser-bin");
        let spec = browser_env_spec(vec![(
            "HOME".into(),
            OsString::from("/tmp/faktor-ci-browser-home"),
        )])
        .unwrap();
        let resolved = spec.resolve();
        let names: Vec<String> = resolved
            .iter()
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "PATH"), "{names:?}");
        assert!(names.iter().any(|n| n == "HOME"), "{names:?}");
        assert!(
            !names.iter().any(|n| n == "FAKTOR_TEST_BROWSER_PATH"),
            "a non-allowlisted daemon name must never resolve: {names:?}"
        );
        assert!(
            resolved
                .iter()
                .any(|(k, v)| k == "HOME"
                    && v == std::ffi::OsStr::new("/tmp/faktor-ci-browser-home")),
            "the exact scratch HOME must win"
        );
        std::env::remove_var("FAKTOR_TEST_BROWSER_PATH");

        for denied in ["OPENAI_API_KEY", "FAKTOR_SERVER_PASSWORD", "PROXY_PASSWORD"] {
            let err = browser_env_spec(vec![(denied.into(), OsString::from("x"))])
                .expect_err("a secret-shaped exact entry must be refused typed");
            assert_eq!(err.kind, ErrorKind::Permission, "{err}");
        }
    }

    #[test]
    fn run_sync_reports_exit_code_and_bounded_heads() {
        let (_d, sup) = supervisor();
        let out = sup
            .run_sync(
                sh("echo out-line; echo err-line >&2; exit 3"),
                Duration::from_secs(10),
                4096,
                4096,
            )
            .unwrap();
        assert!(!out.timed_out);
        assert_eq!(out.exit_code, Some(3));
        assert!(
            out.stdout_head.contains("out-line"),
            "{:?}",
            out.stdout_head
        );
        assert!(
            out.stderr_head.contains("err-line"),
            "{:?}",
            out.stderr_head
        );
        assert!(!out.stdout_truncated);
        assert!(!out.stderr_truncated);
        // The exit is marked exactly once; reap collects the single entry.
        for _ in 0..40 {
            if !sup.reap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(sup.registered(), 0, "reap collects the run_sync child");
    }

    #[test]
    fn cmd_shell_lowering_materializes_the_script_off_the_command_line() {
        // Pure lowering (no spawn): `echo "a b"` through a raw
        // `cmd.exe /C <script>` command line is mangled by cmd's quote
        // stripping. The lowered argv must carry a materialized `.cmd` path
        // and never the raw embedded-quote script.
        let script = "echo \"a b\"";
        let resolved = CommandSpec::shell(script, ShellKind::Cmd).lower().unwrap();
        assert_eq!(resolved.program.to_string_lossy(), "cmd.exe");
        let args: Vec<String> = resolved
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "/d");
        assert_eq!(args[1], "/c");
        assert_eq!(args.len(), 3);
        let path = PathBuf::from(&resolved.args[2]);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("faktor-cmd-") && name.ends_with(".cmd"),
            "the cmd form must name its materialized script: {args:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), script);
        assert!(
            !args.iter().any(|a| a.contains(script)),
            "no raw embedded-quote script on the command line: {args:?}"
        );
        // Unix platform default stays `/bin/sh -c` with the snippet as one
        // argv element: only cmd materializes.
        let sh = CommandSpec::shell(script, ShellKind::PlatformDefault)
            .lower()
            .unwrap();
        assert_eq!(sh.program.to_string_lossy(), "/bin/sh");
        assert!(
            !sh.args
                .iter()
                .any(|a| a.to_string_lossy().starts_with("faktor-cmd-")),
            "only cmd materializes: {:?}",
            sh.args
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cmd_script_guard_deletes_only_the_reserved_materialized_file() {
        // The supervisor cleanup contract: a reserved-prefix script under
        // the system temp dir is deleted by the run guard; a user-named
        // script (even in the temp dir) is never matched, so it survives.
        let reserved =
            std::env::temp_dir().join(format!("faktor-cmd-guard-{}.cmd", uuid::Uuid::new_v4()));
        std::fs::write(&reserved, b"echo reserved").unwrap();
        let cfg = SpawnConfig {
            cmd: "cmd.exe".into(),
            args: vec![
                "/d".into(),
                "/c".into(),
                reserved.to_string_lossy().into_owned(),
            ],
            ..Default::default()
        };
        assert_eq!(materialized_cmd_script(&cfg), Some(reserved.clone()));
        drop(CmdScriptGuard(materialized_cmd_script(&cfg)));
        assert!(!reserved.exists(), "the run guard deletes the script");

        let user = std::env::temp_dir().join(format!("faktor-user-{}.cmd", uuid::Uuid::new_v4()));
        std::fs::write(&user, b"echo user").unwrap();
        let cfg = SpawnConfig {
            cmd: "cmd.exe".into(),
            args: vec!["/c".into(), user.to_string_lossy().into_owned()],
            ..Default::default()
        };
        assert_eq!(
            materialized_cmd_script(&cfg),
            None,
            "a caller-supplied script is never claimed by the guard"
        );
        assert!(user.exists(), "user scripts are never deleted");
        let _ = std::fs::remove_file(user);
    }

    #[test]
    fn run_sync_deadline_kills_the_whole_tree() {
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let pf = dir.path().join("gc.pid");
        let mut cfg = sh(&format!("sleep 30 & echo $! > '{}'; wait", pf.display()));
        cfg.owner = ProcessOwner::Daemon;
        let t0 = std::time::Instant::now();
        let out = sup
            .run_sync(cfg, Duration::from_millis(400), 4096, 4096)
            .unwrap();
        assert!(out.timed_out, "deadline must dominate");
        assert_eq!(out.exit_code, None, "the tree was killed, not exited");
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "deadline kill must be prompt"
        );
        // The grandchild died with the group — no orphan survives the kill.
        let gc: u32 = std::fs::read_to_string(&pf)
            .unwrap()
            .trim()
            .parse()
            .expect("grandchild pid file");
        let mut gone = false;
        for _ in 0..100 {
            if pid_is_gone(gc) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gone, "the hook grandchild must die with the group kill");
        assert!(sup.alive().is_empty(), "no live child after the kill");
    }

    #[test]
    fn run_sync_env_clear_and_is_exact() {
        let (_d, sup) = supervisor();
        std::env::set_var("FAKTOR_HOSTILE", "sekrit");
        // Cleared base: the hostile daemon var and HOME (not allowlisted)
        // must be absent; the explicit entry must be present. Even a
        // Minimal spec keeps only the universal GIT_TERMINAL_PROMPT=0.
        let mut cfg = sh(
            "test -z \"$FAKTOR_HOSTILE\" && test \"$VISIBLE\" = 1 && test -z \"$HOME\" && echo exact",
        );
        cfg.env = EnvSpec::Explicit(vec![("VISIBLE".into(), "1".into())]);
        let out = sup
            .run_sync(cfg, Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        assert!(out.stdout_head.contains("exact"));
        std::env::remove_var("FAKTOR_HOSTILE");
        // Empty-value-inherit: benign keys and the daemon's own value for
        // an explicitly-listed key arrive; the hostile var still does not.
        std::env::set_var("FAKTOR_HOSTILE", "sekrit");
        std::env::set_var("FAKTOR_TEST_DAEMON_ONLY", "xyz");
        let mut cfg = sh(
            "test -n \"$PATH\" && test -n \"$HOME\" && test \"$FAKTOR_TEST_DAEMON_ONLY\" = xyz && test -z \"$FAKTOR_HOSTILE\" && echo benign",
        );
        cfg.env = EnvSpec::Explicit(vec![
            ("PATH".into(), OsString::new()),
            ("HOME".into(), OsString::new()),
            ("FAKTOR_TEST_DAEMON_ONLY".into(), OsString::new()),
        ]);
        let out = sup
            .run_sync(cfg, Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        std::env::remove_var("FAKTOR_HOSTILE");
        std::env::remove_var("FAKTOR_TEST_DAEMON_ONLY");
    }

    #[test]
    fn child_env_toolchain_allowlist_present_and_secret_names_absent() {
        // One environment authority end-to-end: PATH and the approved
        // toolchain vars arrive; configured secret-shaped names set in the
        // parent never cross, even when the spec would otherwise copy them,
        // and an undeclared daemon var never arrives.
        //
        // Env mutation is process-global: the values are UNIQUE per test
        // (tempdir-backed) so parallel cargo-test processes can never read
        // each other's paths, and a drop guard restores the prior values so
        // a panicking assertion cannot leak them into later tests.
        struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                for (name, prior) in &self.0 {
                    match prior {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
        let (_d, sup) = supervisor();
        let home = tempfile::tempdir().unwrap();
        let cargo_home = home.path().join("cargo-home");
        let rustup_home = home.path().join("rustup-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        std::fs::create_dir_all(&rustup_home).unwrap();
        let cargo_home = cargo_home.display().to_string();
        let rustup_home = rustup_home.display().to_string();
        let names = [
            "CARGO_HOME",
            "RUSTUP_HOME",
            "FAKTOR_SERVER_PASSWORD",
            "OPENAI_API_KEY",
            "TEST_PRIVATE_SECRET",
            "FAKTOR_TEST_UNDECLARED_DAEMON_VAR",
        ];
        let _restore = RestoreEnv(
            names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        );
        std::env::set_var("CARGO_HOME", &cargo_home);
        std::env::set_var("RUSTUP_HOME", &rustup_home);
        std::env::set_var("FAKTOR_SERVER_PASSWORD", "hunter2");
        std::env::set_var("OPENAI_API_KEY", "sk-test-secret");
        std::env::set_var("TEST_PRIVATE_SECRET", "private");
        std::env::set_var("FAKTOR_TEST_UNDECLARED_DAEMON_VAR", "must-not-arrive");
        // The child PRINTS its environment; the assertions run on the
        // printed set (not on a hand-written probe).
        let mut cfg = sh("env");
        cfg.env = EnvSpec::toolchain();
        let out = sup
            .run_sync(cfg, Duration::from_secs(10), 8192, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        assert!(
            out.stdout_head
                .lines()
                .any(|l| l.starts_with("PATH=") && l.len() > "PATH=".len()),
            "PATH must be present and non-empty: {:?}",
            out.stdout_head
        );
        assert!(out
            .stdout_head
            .contains(&format!("CARGO_HOME={cargo_home}")));
        assert!(out
            .stdout_head
            .contains(&format!("RUSTUP_HOME={rustup_home}")));
        for secret in [
            "FAKTOR_SERVER_PASSWORD",
            "OPENAI_API_KEY",
            "TEST_PRIVATE_SECRET",
            "FAKTOR_TEST_UNDECLARED_DAEMON_VAR",
        ] {
            assert!(
                !out.stdout_head.contains(secret),
                "{secret} must never cross: {:?}",
                out.stdout_head
            );
        }
        // Even an Explicit spec cannot smuggle the denied names.
        let mut cfg = sh("test -z \"$OPENAI_API_KEY\" && test -z \"$TEST_PRIVATE_SECRET\" && echo explicit-exact");
        cfg.env = EnvSpec::Explicit(vec![
            ("OPENAI_API_KEY".into(), "leak".into()),
            ("TEST_PRIVATE_SECRET".into(), "leak".into()),
        ]);
        let out = sup
            .run_sync(cfg, Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        assert!(out.stdout_head.contains("explicit-exact"));
    }

    #[test]
    fn spawn_isolation_maps_the_policy_requirement_one_to_one() {
        // The enforcement-side mapping: the sandbox's DenyAll requirement
        // becomes DenyAll here, everything else Inherit. There is no third
        // state and no downgrade.
        assert_eq!(
            NetworkIsolation::from(NetworkIsolationRequirement::DenyAll),
            NetworkIsolation::DenyAll
        );
        assert_eq!(
            NetworkIsolation::from(NetworkIsolationRequirement::Inherit),
            NetworkIsolation::Inherit
        );
        assert_eq!(
            SpawnConfig::default().network_isolation,
            NetworkIsolation::Inherit
        );
    }

    #[test]
    fn run_sync_heads_are_capped_and_truncation_reported() {
        let (_d, sup) = supervisor();
        let out = sup
            .run_sync(
                sh("dd if=/dev/zero bs=1048576 count=2 2>/dev/null | tr '\\0' 'x'"),
                Duration::from_secs(30),
                128,
                128,
            )
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout_truncated, "2MB over a 128-byte cap truncates");
        assert!(out.stdout_head.len() <= 128, "head is bounded");
        // A flood must complete (drained, not deadlocked) within the call.
        let t0 = std::time::Instant::now();
        assert!(t0.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn run_sync_ends_promptly_when_a_descendant_holds_the_pipe() {
        let (_d, sup) = supervisor();
        let t0 = std::time::Instant::now();
        let out = sup
            .run_sync(
                sh("(sleep 30) & echo done"),
                Duration::from_secs(30),
                4096,
                4096,
            )
            .unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
        assert!(out.stdout_head.contains("done"), "{:?}", out.stdout_head);
        assert!(
            elapsed < Duration::from_secs(5),
            "a pipe-holding descendant must never own the caller: {elapsed:?}"
        );
    }

    #[test]
    fn try_shared_returns_the_process_wide_singleton() {
        let a = ProcessSupervisor::try_shared().expect("try_shared");
        let b = ProcessSupervisor::try_shared().expect("try_shared");
        assert!(Arc::ptr_eq(&a, &b));
        // The shared supervisor actually runs env-cleared children.
        let mut cfg = sh("echo shared-ok");
        cfg.env = EnvSpec::Minimal;
        let out = a
            .run_sync(cfg, Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout_head.contains("shared-ok"));
    }

    /// The standalone root is a reserved-prefix temp dir, owner-only on
    /// unix, and OWNED by the supervisor (it outlives every child and is
    /// removed only when the last reference drops).
    #[test]
    fn try_shared_root_is_owner_only_and_supervisor_owned() {
        let sup = ProcessSupervisor::try_shared().expect("try_shared");
        let root = sup
            ._standalone_root
            .as_ref()
            .expect("try_shared roots itself in an owned temp dir")
            .path()
            .to_path_buf();
        assert!(
            root.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(FAKTOR_SUPERVISOR_PREFIX)),
            "reserved prefix: {}",
            root.display()
        );
        assert!(root.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o700,
                "owner-only root: {:o} at {}",
                mode,
                root.display()
            );
        }
        assert!(
            root.join("cas").is_dir(),
            "the CAS root exists under the owned temp dir"
        );
    }

    #[test]
    fn live_ceiling_refuses_oversize_before_any_child_exists() {
        let (_d, sup) = supervisor_with_limit(3);
        let mut held = Vec::new();
        for _ in 0..3 {
            let h = sup.spawn(sh("sleep 30")).unwrap();
            held.push(h);
        }
        let err = sup.spawn(sh("true")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Oversized, "{err:?}");
        assert_eq!(sup.alive().len(), 3, "the refused spawn never existed");
        for h in &held {
            assert!(sup.kill(h.id, 500).is_ok());
        }
        for _ in 0..60 {
            if sup.alive().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sup.alive().is_empty());
    }

    #[test]
    fn drop_of_the_last_reference_kills_live_children() {
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let pid = {
            let sup = ProcessSupervisor::new(cas);
            let h = sup.spawn(sh("sleep 30")).unwrap();
            h.pid
        };
        let mut gone = false;
        for _ in 0..100 {
            if pid_is_gone(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gone, "daemon-shutdown Drop must kill live children");
    }

    // ============ pid-reuse discipline: reap serial + guarded signals =====

    /// Wait until the owned process group has no member left.
    fn wait_group_gone(pid: u32, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !group_gone(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: group {pid} must be gone"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Pin the single reaper inside its `[wait consumed the child → publish
    /// reaped]` window (the exact instant the pid becomes recyclable). The
    /// hook runs while the reaper holds the child's reap serial, so every
    /// kill path must block behind it. Returns the "entered" receiver and
    /// the release sender; the caller must release and clear.
    fn pin_reap_window(pid: u32) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        reap_injection::register(
            pid,
            Arc::new(move || {
                let _ = entered_tx.send(());
                let _ = release_rx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv();
            }),
        );
        (entered_rx, release_tx)
    }

    /// ADVERSARIAL PID-REUSE BOUNDARY (deterministic fault injection): while
    /// the single reaper sits in `[wait consumed the child → publish
    /// reaped]`, the kernel may already recycle the pid, so ANY signal would
    /// be able to hit an unrelated group. A concurrent `kill` must block on
    /// the reap serial and must never attempt a signal — neither while
    /// blocked nor after the reap is published. The blocking assertion is
    /// load-bearing: the unguarded code signalled (or returned) immediately.
    #[test]
    fn concurrent_kill_blocks_on_the_reap_window_and_never_signals() {
        let (_d, sup) = supervisor();
        let h = sup.spawn(sh("sleep 0.3")).unwrap();
        let reap = sup.reap_state(h.id).expect("registered row");
        let (entered_rx, release_tx) = pin_reap_window(h.pid);
        assert!(
            entered_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the reaper must reach its [wait → publish] window"
        );
        // The single reaper consumed the child: the pid is recyclable now.
        assert!(pid_is_gone(h.pid), "the reaper consumed the child");
        let before = reap.attempts();
        let sup2 = Arc::clone(&sup);
        let killer = std::thread::spawn(move || sup2.kill(h.id, 200));
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !killer.is_finished(),
            "LOAD-BEARING: a kill must serialize behind the reap publication, not \
             signal a recyclable pid"
        );
        assert_eq!(
            reap.attempts(),
            before,
            "no signal may be attempted while the reap is unpublished"
        );
        release_tx.send(()).unwrap();
        killer.join().unwrap().unwrap();
        assert_eq!(
            reap.attempts(),
            before,
            "no signal may ever reference a pid the reaper consumed"
        );
        reap_injection::clear(h.pid);
    }

    /// ADVERSARIAL PID-REUSE BOUNDARY (Drop twin): dropping the last
    /// supervisor reference while a reaper sits in the publication window
    /// must block on the serial and never signal the consumed pid. This is
    /// the exact daemon-shutdown race that used to signal a recycled pid.
    #[test]
    fn supervisor_drop_blocks_on_the_reap_window_and_never_signals() {
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let h = sup.spawn(sh("sleep 0.3")).unwrap();
        let reap = sup.reap_state(h.id).expect("registered row");
        let (entered_rx, release_tx) = pin_reap_window(h.pid);
        assert!(
            entered_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the reaper must reach its [wait → publish] window"
        );
        let before = reap.attempts();
        let dropper = std::thread::spawn(move || drop(sup));
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !dropper.is_finished(),
            "LOAD-BEARING: Drop must serialize behind the reap publication, not \
             signal a recyclable pid"
        );
        assert_eq!(
            reap.attempts(),
            before,
            "Drop must not signal while the reap is unpublished"
        );
        release_tx.send(()).unwrap();
        dropper.join().unwrap();
        assert_eq!(
            reap.attempts(),
            before,
            "Drop must never signal a pid the reaper consumed"
        );
        reap_injection::clear(h.pid);
    }

    /// The raw-pid path takes the same serial and never signals a consumed
    /// pid; an unowned pid is refused typed without any signal at all.
    #[test]
    fn kill_child_pid_is_guarded_and_refuses_unowned_pids() {
        let (_d, sup) = supervisor();
        let h = sup.spawn(sh("sleep 0.3")).unwrap();
        let reap = sup.reap_state(h.id).expect("registered row");
        let (entered_rx, release_tx) = pin_reap_window(h.pid);
        assert!(
            entered_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the reaper must reach its [wait → publish] window"
        );
        let before = reap.attempts();
        let sup2 = Arc::clone(&sup);
        let pid = h.pid;
        let killer = std::thread::spawn(move || sup2.kill_child_pid(pid, 200));
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !killer.is_finished(),
            "the raw-pid path must take the same reap serial"
        );
        assert_eq!(
            reap.attempts(),
            before,
            "no raw signal while the reap is unpublished"
        );
        release_tx.send(()).unwrap();
        assert!(
            killer.join().unwrap().is_ok(),
            "the consumed child's guarded kill is an idempotent no-op"
        );
        assert_eq!(
            reap.attempts(),
            before,
            "the raw-pid path must never signal a consumed pid"
        );
        reap_injection::clear(h.pid);
        // A pid this supervisor never owned is refused typed — never a blind
        // raw signal.
        let err = sup.kill_child_pid(std::process::id(), 100).unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound, "{err:?}");
        assert!(sup.pid_alive(std::process::id()));
    }

    /// The normal order still works: a live child is signalled by an explicit
    /// kill and by the supervisor's daemon-shutdown Drop.
    #[test]
    fn live_children_are_still_signalled_and_killed_by_kill_and_drop() {
        let (_d, sup) = supervisor();
        let h = sup.spawn(sh("sleep 30")).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let reap = sup.reap_state(h.id).expect("registered row");
        let before = reap.attempts();
        sup.kill(h.id, 500).unwrap();
        assert!(
            reap.attempts() > before,
            "an unreaped live child must be signalled"
        );
        wait_group_gone(h.pid, "explicit kill of a live child");
        let h2 = sup.spawn(sh("sleep 30")).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        drop(sup);
        wait_group_gone(h2.pid, "supervisor Drop of a live child");
    }

    /// Budget enforcement is a kill path too: the wall budget must still take
    /// a live tree through the guarded deadline kill.
    #[test]
    fn budget_wall_deadline_still_kills_a_live_child() {
        let (_d, sup) = supervisor();
        let budgets = TreeBudgets {
            wall_time_ms: 300,
            ..TreeBudgets::disabled()
        };
        let (out, enforcement) = sup
            .run_sync_with_budgets(sh("sleep 30"), budgets, Duration::from_secs(30), 4096, 4096)
            .unwrap();
        assert!(out.timed_out, "the wall budget must dominate");
        assert_eq!(out.exit_code, None, "the tree was killed, not exited");
        assert_eq!(enforcement.wall, LimitState::Enforced);
        assert!(
            sup.alive().is_empty(),
            "no live child after the budget kill"
        );
        let mut collected = Vec::new();
        for _ in 0..40 {
            collected.extend(sup.reap());
            if !collected.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(collected.len(), 1, "exactly one collectible child");
        assert_eq!(sup.registered(), 0, "the budgeted child is collected");
    }

    /// ADVERSARIAL (identity is not provable after consumption): the leader
    /// is consumed by its reaper while a detached descendant keeps the owned
    /// group (and the leader's pipe) alive. The old "a live group keeps its
    /// pgid allocated" proof would have signalled here — but a live group
    /// with that pgid can be an unrelated RECYCLED group once the owned
    /// group fully died, so every guarded path must emit NO signal after the
    /// reap. The run still ends on the bounded drain and returns the bounded
    /// head the reader had already published; the test owns its fixture and
    /// cleans it with the raw test-only helper.
    #[test]
    fn consumed_leader_with_surviving_descendants_emits_no_stale_signal() {
        let (_d, sup) = supervisor();
        let t0 = std::time::Instant::now();
        let out = sup
            .run_sync(
                sh("(sleep 30) & echo done"),
                Duration::from_secs(30),
                4096,
                4096,
            )
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(
            out.stdout_head.contains("done"),
            "the bounded head read before the descendant held the pipe is preserved: {:?}",
            out.stdout_head
        );
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the caller is bounded by the drain window, never by the descendant"
        );
        let timeline = sup.recent_spawns();
        let (op_id, pid) = (timeline[0].op_id, timeline[0].pid);
        let reap = sup.reap_state(op_id).expect("run_sync row until reap()");
        assert!(
            reap.reaped(),
            "the leader was consumed by its single reaper"
        );
        // The consumed pid may not be signalled again — the old live-group
        // probe cannot prove the group is OURS. Load-bearing: the descendant
        // is still alive, so the old code would have attempted a signal.
        assert!(
            !group_gone(pid),
            "the descendant still holds (and keeps alive) a group with this pgid"
        );
        assert_eq!(
            reap.attempts(),
            0,
            "a consumed pid must emit no signal even while a group with its pgid is alive"
        );
        sup.kill(op_id, 200).unwrap();
        assert_eq!(
            reap.attempts(),
            0,
            "the guarded kill refuses once the child was consumed"
        );
        // Fixture cleanup (test-only raw helper; the audit forbids production
        // raw-pgid kills, not a test owning its own unreaped descendant).
        kill_group(pid, 1000).unwrap();
        wait_group_gone(pid, "test-owned descendant cleanup");
    }

    /// ADVERSARIAL, DETERMINISTIC (consumed-then-recycled injection): pin the
    /// exact state the guard exists for — the child is consumed (its pid is
    /// recyclable) while the negative-pgid probe reports a LIVE group with
    /// that pgid, exactly what an unrelated recycled process group looks
    /// like. Every guarded path must refuse: no signal may be attempted
    /// through the consumed pid.
    #[test]
    fn consumed_then_recycled_group_is_never_signalled() {
        let (_d, sup) = supervisor();
        let h = sup.spawn(sh("sleep 0.3")).unwrap();
        let reap = sup.reap_state(h.id).expect("registered row");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !reap.reaped() {
            assert!(
                std::time::Instant::now() < deadline,
                "the single reaper must consume the child"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // Inject the recycled-group state: a live group with this pgid.
        group_probe_injection::register(h.pid, false);
        assert!(
            !group_gone(h.pid),
            "the injection simulates the recycled group the probe cannot distinguish"
        );
        let before = reap.attempts();
        // Every production kill path for the row refuses.
        sup.kill(h.id, 200).unwrap();
        assert_eq!(
            reap.attempts(),
            before,
            "no signal may reference a consumed pid, even with a live (recycled) group"
        );
        assert!(
            !reap.try_signal_group(h.pid, libc::SIGKILL),
            "the guarded issuance itself refuses once reaped"
        );
        assert_eq!(reap.attempts(), before);
        group_probe_injection::clear(h.pid);
    }

    /// ADVERSARIAL: `RunGroupGuard::drop` must actually CONSUME the child it
    /// killed before it publishes `reaped`. The old Drop could publish
    /// `reaped` without a `waitpid`, falsifying the field's invariant. The
    /// test's own `waitpid(WNOHANG)` after the guard drops reports ECHILD,
    /// proving the guard (not some other party) consumed the child.
    #[test]
    #[allow(unsafe_code)]
    fn run_group_guard_drop_consumes_the_child_it_killed() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh");
        child.args(["-c", "sleep 30"]).process_group(0);
        let child = child.spawn().unwrap();
        let pid = child.id();
        let reap = ReapState::new();
        {
            let _guard = RunGroupGuard {
                reap: reap.clone(),
                pid,
            };
        }
        assert!(
            reap.reaped(),
            "the guard published the reap only after consuming"
        );
        assert!(pid_is_gone(pid), "the killed child was actually reaped");
        let mut status: libc::c_int = 0;
        // SAFETY: `pid` was this test's own child; this wait can at worst
        // report ECHILD (already consumed by the guard).
        let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        assert_eq!(r, -1, "no unreaped child may remain after the guard");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD),
            "the guard is the consumer"
        );
        drop(child);
    }

    /// ADVERSARIAL twin: when the parent never reaped and the guard's group
    /// kill fails (ESRCH — the pid names no group of ours), the guard must
    /// still resolve the child through `waitpid` before publishing `reaped`;
    /// an already-consumed (ECHILD) child is the documented alternative and
    /// `reaped` is then truthful — there is no unreaped child to signal.
    #[test]
    #[allow(unsafe_code)]
    fn run_group_guard_drop_is_truthful_when_the_kill_cannot_reach_a_group() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh");
        // Its own process group (like a supervised child), but already
        // consumed by the test: `kill(-pid, …)` is ESRCH (the group died
        // with the child), never a signal to another group.
        child.args(["-c", "sleep 0.2"]).process_group(0);
        let mut child = child.spawn().unwrap();
        let pid = child.id();
        let _ = child.wait();
        let reap = ReapState::new();
        {
            let _guard = RunGroupGuard {
                reap: reap.clone(),
                pid,
            };
        }
        assert!(
            reap.reaped(),
            "reaped is published after the child is provably consumed/absent"
        );
        let mut status: libc::c_int = 0;
        // SAFETY: test-owned child, already waited above.
        let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        assert_eq!(r, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        drop(child);
    }

    /// A `run()` future dropped by an outer timeout/unwind still takes the
    /// whole owned group with it (the RAII guard SIGKILLs while the child is
    /// unreaped) and publishes the reap, so the later supervisor Drop cannot
    /// signal the consumed pid.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_run_future_kills_the_group_and_suppresses_later_signals() {
        let (_d, sup) = supervisor();
        let sup2 = Arc::clone(&sup);
        let token = CancellationToken::new();
        let task = tokio::spawn(async move {
            sup2.run(
                sh("(sleep 30) & echo started; sleep 30"),
                Duration::from_secs(120),
                token,
            )
            .await
        });
        let (op_id, pid) = loop {
            let recent = sup.recent_spawns();
            if let Some(t) = recent.first() {
                if t.pid > 0 {
                    break (t.op_id, t.pid);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let reap = sup.reap_state(op_id).expect("registered row");
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();
        let _ = task.await;
        wait_group_gone(pid, "RAII group guard on a dropped run future");
        let before = reap.attempts();
        assert!(before > 0, "the drop guard must SIGKILL the owned group");
        sup.kill(op_id, 200).unwrap();
        assert_eq!(
            reap.attempts(),
            before,
            "nothing may signal a pid the drop guard consumed"
        );
    }

    #[test]
    fn run_sync_refusal_at_the_ceiling_is_typed_oversized() {
        let (_d, sup) = supervisor_with_limit(1);
        let h = sup.spawn(sh("sleep 30")).unwrap();
        let err = sup
            .run_sync(sh("true"), Duration::from_secs(5), 1024, 1024)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Oversized, "{err:?}");
        assert!(sup.kill(h.id, 500).is_ok());
    }

    fn sh(cmd: &str) -> SpawnConfig {
        SpawnConfig {
            cmd: "/bin/sh".into(),
            args: vec!["-c".into(), cmd.into()],
            cwd: std::env::temp_dir(),
            ..Default::default()
        }
    }

    fn ps_alive(pid: u32) -> bool {
        let out = std::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string()])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        // <defunct> still counts as a live entry until reaped.
        text.contains(&pid.to_string())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ring_buffer_caps_at_200_lines() {
        let (_d, sup) = supervisor();
        let out = sup
            .run(
                sh("i=0; while [ $i -lt 10000 ]; do echo line$i; i=$((i+1)); done"),
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.ring_lines <= 200);
        assert!(out.excerpt.contains("line9999"));
        assert!(
            !out.excerpt.contains("line1\nline2\n"),
            "ring must drop the head"
        );
        assert!(out.excerpt.len() < 64 * 1024);
    }

    /// Backdate a path (file or directory) so an age-gated sweep can prove
    /// it stale.
    fn backdate_path(p: &std::path::Path, age: Duration) {
        let old = std::time::SystemTime::now() - age;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .or_else(|_| std::fs::File::open(p))
            .unwrap();
        f.set_modified(old).unwrap();
    }

    /// Phase F item 24: the writers mint Faktor names, while recognition
    /// accepts BOTH spellings so crash residue from an older release is never
    /// mistaken for user temp content.
    #[test]
    fn internal_spill_and_supervisor_names_recognize_both_spellings() {
        for name in [
            "faktor-spill-123-456",
            "kp-spill-123-456",
            "faktor-supervisor-shared-123",
            "kp-supervisor-shared-123",
        ] {
            assert!(
                is_internal_spill_name(name) || is_internal_supervisor_name(name),
                "{name}"
            );
        }
        assert!(!is_internal_spill_name("my-faktor-spill-1"));
        assert!(!is_internal_supervisor_name("my-kp-supervisor-shared-1"));
        assert!(!is_internal_spill_name("faktor.txt"));
        assert_eq!(supervisor_root_pid("faktor-supervisor-shared-42"), Some(42));
        assert_eq!(supervisor_root_pid("kp-supervisor-shared-42"), Some(42));
        assert_eq!(supervisor_root_pid("faktor-supervisor-shared-x"), None);
        assert_eq!(supervisor_root_pid("faktor-supervisor-notes"), None);
    }

    /// Cleanup accepts BOTH spellings: stale residue (regular files /
    /// owner-dead dirs) is removed; fresh files, live owners, symlinks and
    /// non-matching names survive.
    #[test]
    fn stale_spill_and_supervisor_residue_of_both_spellings_is_swept() {
        let temp = std::env::temp_dir();
        let nonce = uuid::Uuid::new_v4();
        let stale_age = Duration::from_secs(48 * 60 * 60);

        let stale_faktor_spill = temp.join(format!("{FAKTOR_SPILL_PREFIX}{nonce}-stale"));
        let stale_legacy_spill = temp.join(format!("{LEGACY_KP_SPILL_PREFIX}{nonce}-stale"));
        let fresh_spill = temp.join(format!("{FAKTOR_SPILL_PREFIX}{nonce}-fresh"));
        let sacred = temp.join(format!("faktor-ci-reviewer-sacred-{nonce}.bin"));
        let spill_link = temp.join(format!("{LEGACY_KP_SPILL_PREFIX}{nonce}-link"));
        for p in [&stale_faktor_spill, &stale_legacy_spill] {
            std::fs::write(p, b"torn spill").unwrap();
            backdate_path(p, stale_age);
        }
        std::fs::write(&fresh_spill, b"live spool").unwrap();
        std::fs::write(&sacred, b"SACRED-BYTES").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&sacred, &spill_link).unwrap();

        let stale_faktor_sup = temp.join(format!("{FAKTOR_SUPERVISOR_PREFIX}shared-0"));
        // A pid far above any real pid_t: provably gone, never recycled.
        let stale_legacy_sup = temp.join(format!("{LEGACY_KP_SUPERVISOR_PREFIX}shared-2147483647"));
        let fresh_sup = temp.join(format!("{LEGACY_KP_SUPERVISOR_PREFIX}shared-0"));
        let live_sup = temp.join(format!(
            "{FAKTOR_SUPERVISOR_PREFIX}shared-{}",
            std::process::id()
        ));
        for p in [&stale_faktor_sup, &stale_legacy_sup, &live_sup] {
            std::fs::create_dir_all(p).unwrap();
            backdate_path(p, stale_age);
        }
        std::fs::create_dir_all(&fresh_sup).unwrap();

        sweep_stale_spill_files();
        sweep_stale_supervisor_roots();

        assert!(!stale_faktor_spill.exists(), "Faktor spill residue swept");
        assert!(!stale_legacy_spill.exists(), "legacy spill residue swept");
        assert!(
            fresh_spill.exists(),
            "a fresh spill belongs to a live writer"
        );
        assert!(sacred.exists(), "a symlink target must never be touched");
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(&spill_link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "a symlink named like a spill is never followed or removed"
        );
        assert!(
            !stale_faktor_sup.exists(),
            "Faktor dead supervisor root swept"
        );
        assert!(
            !stale_legacy_sup.exists(),
            "legacy dead supervisor root swept"
        );
        assert!(
            fresh_sup.exists(),
            "a fresh supervisor root survives the age gate"
        );
        assert!(live_sup.exists(), "a live owner keeps its supervisor root");

        for p in [&fresh_spill, &sacred, &spill_link, &fresh_sup, &live_sup] {
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(p)
            } else {
                std::fs::remove_file(p)
            };
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn huge_output_spills_to_cas_ram_bounded() {
        let (_d, sup) = supervisor();
        let mut cfg = sh("yes 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' | head -n 500000");
        cfg.artifact_max = 1024 * 1024; // small cap so the spill triggers fast
        let out = sup
            .run(cfg, Duration::from_secs(60), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.excerpt.len() < 64 * 1024, "excerpt bounded");
        assert!(out.artifact.is_some(), "overflow must spill to the CAS");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_terminates_process_group() {
        let (_d, sup) = supervisor();
        let handle = sup.spawn(sh("sleep 30 & wait")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(ps_alive(handle.pid), "child must be alive before kill");
        sup.kill(handle.id, 500).unwrap();
        // Give the reaper a moment.
        for _ in 0..40 {
            if !ps_alive(handle.pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!ps_alive(handle.pid), "group kill must take the whole tree");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deadline_kills_group_and_returns_timeout() {
        let (_d, sup) = supervisor();
        let err = sup
            .run(
                sh("sleep 30"),
                Duration::from_millis(300),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(err.kind == ErrorKind::Timeout);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_kills_group_and_returns_cancelled() {
        let (_d, sup) = supervisor();
        let token = CancellationToken::new();
        let t = token.clone();
        let sup2 = sup.clone();
        let task =
            tokio::spawn(async move { sup2.run(sh("sleep 30"), Duration::from_secs(60), t).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
        let err = task.await.unwrap().unwrap_err();
        assert!(err.kind == ErrorKind::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reader_does_not_spin_after_one_pipe_eofs() {
        // Audit round 51: stdout closes EARLY while stderr stays open for
        // ~1s. Once stdout_eof is set the stdout arm must be removed from
        // the reader's select — an EOF-ready pipe would resolve Ok(0) on
        // every poll and spin the loop at 100% CPU until stderr closes.
        // Behavior must be identical: both streams still captured, run()
        // completes promptly after the second pipe closes.
        let (_d, sup) = supervisor();
        let t0 = std::time::Instant::now();
        let out = sup
            .run(
                sh("echo out-first; exec 1>&-; sleep 1; echo err-tail >&2"),
                Duration::from_secs(10),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.excerpt.contains("out-first"), "{:?}", out.excerpt);
        assert!(out.excerpt.contains("err-tail"), "{:?}", out.excerpt);
        assert!(
            elapsed >= Duration::from_millis(500),
            "the reader must wait on the still-open pipe, not spin: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "the reader must complete promptly once the second pipe closes: {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deadline_timeout_aborts_reader_and_supervisor_stays_clean() {
        // Audit round 17: on the terminal timeout path the reader task must
        // be terminated exactly once (joined via the post-exit drain, or
        // aborted) and never leak into the next command. The child ignores
        // SIGTERM and keeps writing, so the reader is mid-read at the
        // deadline; only the SIGKILL escalation closes the pipes.
        let (_d, sup) = supervisor();
        let t0 = std::time::Instant::now();
        let err = sup
            .run(
                sh("trap '' TERM; i=0; while true; do echo stuck-line-$i; i=$((i+1)); done"),
                Duration::from_millis(300),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(err.kind == ErrorKind::Timeout, "{err:?}");
        assert!(
            t0.elapsed() < Duration::from_secs(6),
            "timeout must escalate SIGKILL and return: {:?}",
            t0.elapsed()
        );
        // Exactly-once reaping: the timed-out child is marked exited and
        // collected by the next reap() (one entry, no exit code), leaving
        // the registry clean; a subsequent run on the same supervisor is
        // unaffected by any leaked reader task.
        assert!(sup.alive().is_empty(), "no live children after timeout");
        let reaped = sup.reap();
        assert_eq!(reaped.len(), 1, "exactly one collectible child");
        assert_eq!(reaped[0].exit_code, None);
        assert_eq!(sup.registered(), 0, "registry must drain after reap");
        let out = sup
            .run(
                sh("echo after-timeout"),
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.excerpt.contains("after-timeout"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reap_collects_exit_codes_and_no_zombies() {
        let (_d, sup) = supervisor();
        let mut ids = Vec::new();
        for _ in 0..6 {
            let h = sup.spawn(sh("exit 3")).unwrap();
            ids.push(h.id);
        }
        let mut reaped = Vec::new();
        for _ in 0..40 {
            reaped.extend(sup.reap());
            if reaped.len() == 6 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(reaped.len(), 6);
        for r in &reaped {
            assert_eq!(r.exit_code, Some(3));
        }
        assert!(sup.alive().is_empty());
        assert_eq!(sup.registered(), 0, "no zombies left registered");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_all_for_session_kills_children() {
        let (_d, sup) = supervisor();
        let owner = ProcessOwner::Session(SessionId::new(9));
        let mut cfg = sh("sleep 30");
        cfg.owner = owner.clone();
        let h1 = sup.spawn(cfg.clone()).unwrap();
        let h2 = sup.spawn(cfg).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let killed = sup.kill_all_for(owner);
        assert_eq!(killed.len(), 2);
        for _ in 0..40 {
            if !ps_alive(h1.pid) && !ps_alive(h2.pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!ps_alive(h1.pid));
        assert!(!ps_alive(h2.pid));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transfer_changes_owner_and_survives() {
        let (_d, sup) = supervisor();
        let mut cfg = sh("sleep 1");
        cfg.owner = ProcessOwner::Session(SessionId::new(1));
        let h = sup.spawn(cfg).unwrap();
        sup.transfer(h.id, ProcessOwner::Daemon).unwrap();
        let killed = sup.kill_all_for(ProcessOwner::Session(SessionId::new(1)));
        assert!(killed.is_empty(), "transferred child must survive");
        sup.kill(h.id, 300).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_id_operations_are_not_found() {
        let (_d, sup) = supervisor();
        assert!(sup.kill(999, 10).is_err());
        assert!(sup.transfer(999, ProcessOwner::Daemon).is_err());
        assert!(sup.reap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exit_code_propagation() {
        let (_d, sup) = supervisor();
        assert_eq!(
            sup.run(sh("true"), Duration::from_secs(5), CancellationToken::new())
                .await
                .unwrap()
                .exit_code,
            Some(0)
        );
        assert_eq!(
            sup.run(
                sh("false"),
                Duration::from_secs(5),
                CancellationToken::new()
            )
            .await
            .unwrap()
            .exit_code,
            Some(1)
        );
        assert_eq!(
            sup.run(
                sh("exit 42"),
                Duration::from_secs(5),
                CancellationToken::new()
            )
            .await
            .unwrap()
            .exit_code,
            Some(42)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malicious_command_vector_stays_literal() {
        let (_d, sup) = supervisor();
        let out = sup
            .run(
                SpawnConfig {
                    cmd: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        "printf '%s' \"$1\"".into(),
                        "x".into(),
                        "; rm -rf /tmp/faktor-ci-evil".into(),
                    ],
                    cwd: std::env::temp_dir(),
                    ..Default::default()
                },
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.excerpt.contains("; rm -rf /tmp/faktor-ci-evil"));
        assert!(!std::path::Path::new("/tmp/faktor-ci-evil").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_command_is_not_found() {
        let (_d, sup) = supervisor();
        let err = sup
            .run(
                SpawnConfig {
                    cmd: "/nonexistent-binary-xyz".into(),
                    ..Default::default()
                },
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(err.kind == ErrorKind::NotFound, "{err:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dbg_50_spawns() {
        let (_d, sup) = supervisor();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..50 {
            let h = sup.spawn(sh("true")).unwrap();
            ids.insert(h.id);
        }
        eprintln!("dbg: spawned 50, registered={}", sup.registered());
        for i in 0..80 {
            std::thread::sleep(Duration::from_millis(50));
            let r = sup.reap();
            if !r.is_empty() {
                eprintln!("dbg: first reap at iter {i}, count={}", r.len());
            }
            if r.len() >= 50 {
                break;
            }
        }
        eprintln!(
            "dbg: final reaped={} registered={}",
            sup.reap().len(),
            sup.registered()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_before_run_races_unique_ids() {
        let (_d, sup) = supervisor();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..50 {
            let h = sup.spawn(sh("true")).unwrap();
            assert!(ids.insert(h.id));
        }
        assert_eq!(sup.registered(), 50);
        // reap() drains: accumulate across polls until all 50 are collected.
        let mut collected = Vec::new();
        for _ in 0..80 {
            collected.extend(sup.reap());
            if collected.len() == 50 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(collected.len(), 50);
        assert_eq!(sup.registered(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stderr_and_stdout_both_captured() {
        let (_d, sup) = supervisor();
        let out = sup
            .run(
                sh("echo out1; echo err1 >&2; echo out2"),
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.excerpt.contains("out1"));
        assert!(out.excerpt.contains("out2"));
        assert!(out.excerpt.contains("err1"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn artifact_roundtrip_via_cas() {
        let (_d, sup) = supervisor();
        let mut cfg = sh("i=0; while [ $i -lt 200000 ]; do echo overflow-$i; i=$((i+1)); done");
        // ~3MB stream; the cap is honored now, so size it above the stream to
        // keep the whole (untruncated) artifact reachable.
        cfg.artifact_max = 8 * 1024 * 1024;
        let out = sup
            .run(cfg, Duration::from_secs(60), CancellationToken::new())
            .await
            .unwrap();
        assert!(!out.artifact_truncated, "stream fits under the cap");
        assert!(out.artifact.is_some());
        let hash = out
            .artifact
            .as_ref()
            .and_then(|a| a.strip_prefix("artifact://"))
            .and_then(faktor_core::hash::FileHash::from_hex)
            .unwrap();
        let blob = sup.cas.get_verified_now(hash).unwrap();
        assert!(String::from_utf8_lossy(&blob).contains("overflow-199999"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn artifact_cap_enforced_and_truncation_reported() {
        // Audit round 10: `artifact_max = 1MB` must actually cap the spool.
        // 5MB of output over a 1MB cap ⇒ artifact holds exactly the first
        // 1MB (first bytes of the stream), the ring keeps the tail, and the
        // outcome reports the truncation explicitly.
        let (_d, sup) = supervisor();
        let cap = 1024 * 1024;
        let mut cfg = sh(
            "printf 'BEGIN-MARKER-0\\n'; yes 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' | head -c 5000000",
        );
        cfg.artifact_max = cap;
        let out = sup
            .run(cfg, Duration::from_secs(60), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.artifact_truncated, "5MB over a 1MB cap must truncate");
        let artifact = out
            .artifact
            .as_ref()
            .expect("overflow must still produce an artifact");
        let hash = artifact
            .strip_prefix("artifact://")
            .and_then(faktor_core::hash::FileHash::from_hex)
            .unwrap();
        let blob = sup.cas.get_verified_now(hash).unwrap();
        assert!(
            blob.len() <= cap,
            "artifact {} bytes exceeds the {cap}-byte cap",
            blob.len()
        );
        assert_eq!(blob.len(), cap, "cap must be reached exactly");
        assert!(
            blob.starts_with(b"BEGIN-MARKER-0\n"),
            "artifact keeps the FIRST bytes of the stream"
        );
        assert!(out.excerpt.len() < 64 * 1024, "excerpt stays bounded");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn descendant_holding_capture_pipe_is_killed_on_exit() {
        // A backgrounded descendant that keeps stdout open AFTER the direct
        // child exits must be group-killed on the Exited path (audit round
        // 11: otherwise the capture reader never sees EOF and run() hangs).
        let (_d, sup) = supervisor();
        // `sh -c '(sleep 300; echo late) & echo done; exit 0'` — sh exits
        // immediately but the background subshell holds the pipe for 300s.
        let cfg = sh("(sleep 300; echo late) & echo done; exit 0");
        let t0 = std::time::Instant::now();
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            sup.run(cfg, Duration::from_secs(30), CancellationToken::new()),
        )
        .await
        .expect("run must return promptly after the direct child exits")
        .unwrap();
        assert!(t0.elapsed() < Duration::from_secs(8));
        assert_eq!(out.exit_code, Some(0));
        assert!(out.excerpt.contains("done"));
        assert!(!out.excerpt.contains("late"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn artifact_within_cap_untouched_and_exact() {
        // 200KB of output under a 1MB cap: the artifact is the complete
        // stream, byte-exact, and reports no truncation.
        let (_d, sup) = supervisor();
        // Bounded producer: `yes | head` leaves an eternal writer that can
        // starve the runtime under capture (audit round 11); dd|tr|fold
        // emits the same ~200 KB payload with every process exiting.
        let mut cfg = sh("dd if=/dev/zero bs=204800 count=1 2>/dev/null | tr '\\0' 'x'");
        cfg.artifact_max = 1024 * 1024;
        let out = sup
            .run(cfg, Duration::from_secs(30), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.artifact_truncated, "stream under the cap is intact");
        let artifact = out.artifact.as_ref().expect("artifact must exist");
        let hash = artifact
            .strip_prefix("artifact://")
            .and_then(faktor_core::hash::FileHash::from_hex)
            .unwrap();
        let blob = sup.cas.get_verified_now(hash).unwrap();
        let expected = "x".repeat(204_800);
        assert_eq!(expected.len(), 204_800);
        assert_eq!(blob.len(), 204_800, "artifact must be byte-exact");
        assert_eq!(blob, expected.as_bytes(), "artifact content must be intact");
    }

    #[test]
    fn artifact_cap_default_and_global_ceiling() {
        assert_eq!(SpawnConfig::default().artifact_max, 100 * 1024 * 1024);
        assert_eq!(effective_artifact_max(1), 1);
        assert_eq!(
            effective_artifact_max(usize::MAX),
            GLOBAL_HARD_MAX as usize,
            "configured caps must never exceed the global ceiling"
        );
        assert_eq!(
            effective_artifact_max(GLOBAL_HARD_MAX as usize),
            GLOBAL_HARD_MAX as usize
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn descendant_holding_pipe_cannot_hang_run() {
        // A backgrounded descendant keeps the stdout pipe open for 5s after
        // the shell exits. run() must not wait for that descendant: the
        // post-exit drain is bounded.
        let (_d, sup) = supervisor();
        let t0 = std::time::Instant::now();
        let out = sup
            .run(
                sh("(sleep 5 &) ; echo done"),
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.excerpt.contains("done"), "{:?}", out.excerpt);
        assert!(
            elapsed < Duration::from_millis(1500),
            "run() must not wait for a descendant holding the pipe: {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_exit_drain_is_bounded_even_when_pipe_never_closes() {
        // `(sleep 30) &` keeps the pipe open for 30s; the drain bound must
        // cap run() at ~500ms after the shell exits.
        let (_d, sup) = supervisor();
        let t0 = std::time::Instant::now();
        let out = sup
            .run(
                sh("(sleep 30) & echo x"),
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.excerpt.contains("x"), "{:?}", out.excerpt);
        assert!(
            elapsed < Duration::from_secs(3),
            "post-exit drain must be bounded: {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_group_async_returns_immediately_on_fast_exit() {
        let (_d, sup) = supervisor();
        let h = sup.spawn(sh("sleep 30")).unwrap();
        let t0 = std::time::Instant::now();
        kill_group_async(h.pid, 2000).await.unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "kill_group_async must return as soon as the group is gone: {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_group_async_escalates_to_sigkill_at_grace() {
        // sh ignores SIGTERM; only the SIGKILL at the grace deadline can
        // take it down. The elapsed time must reflect the grace, not less.
        let (_d, sup) = supervisor();
        let h = sup.spawn(sh("trap '' TERM; sleep 30")).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let t0 = std::time::Instant::now();
        kill_group_async(h.pid, 300).await.unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(2),
            "SIGKILL escalation must happen at the grace deadline: {elapsed:?}"
        );
        for _ in 0..40 {
            if !ps_alive(h.pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!ps_alive(h.pid), "SIGKILL must take the whole group");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_kill_group_exits_early_too() {
        let (_d, sup) = supervisor();
        let h = sup.spawn(sh("sleep 30")).unwrap();
        let t0 = std::time::Instant::now();
        sup.kill(h.id, 2000).unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "sync kill must not hold the full grace when the group exits early: {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reader_abort_does_not_lose_exit_code() {
        // A descendant holds the pipe open past the drain bound, so the
        // reader is aborted mid-drain; the exit code and the drained excerpt
        // must still survive.
        let (_d, sup) = supervisor();
        let out = sup
            .run(
                sh("(sleep 30) & echo exit42; exit 42"),
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(42));
        assert!(out.excerpt.contains("exit42"), "{:?}", out.excerpt);
    }

    #[test]
    fn ring_buffer_unit() {
        let mut r = RingBuffer::new(3);
        r.push("a".into());
        r.push("b".into());
        r.push("c".into());
        r.push("d".into());
        assert_eq!(r.len(), 3);
        assert_eq!(r.excerpt(), "b\nc\nd\n");
        assert!(r.error_lines().is_empty());
        r.push("error: boom".into());
        assert_eq!(r.error_lines(), vec!["error: boom"]);
    }

    #[tokio::test]
    async fn artifact_contains_beginning_and_end_of_large_output() {
        // Audit round 5: with full-stream spooling the CAS artifact holds the
        // stream from the FIRST byte; the ring holds the tail. Both ends must
        // be recoverable.
        let (_d, sup) = supervisor();
        let mut cfg =
            sh("i=0; while [ $i -lt 200000 ]; do echo beginning-check-$i; i=$((i+1)); done");
        cfg.artifact_max = 1024 * 1024;
        let out = sup
            .run(cfg, Duration::from_secs(60), CancellationToken::new())
            .await
            .unwrap();
        assert!(
            out.artifact.is_some(),
            "full-stream spooling must produce an artifact"
        );
        let hash = out
            .artifact
            .as_ref()
            .and_then(|a| a.strip_prefix("artifact://"))
            .and_then(faktor_core::hash::FileHash::from_hex)
            .unwrap();
        let blob = sup.cas.get_verified_now(hash).unwrap();
        let text = String::from_utf8_lossy(&blob);
        // The artifact begins at the stream's first line...
        assert!(
            text.contains("beginning-check-0"),
            "artifact must contain the stream beginning"
        );
        // ...and the excerpt holds the very end.
        assert!(out.excerpt.contains("beginning-check-199999"));
    }

    #[tokio::test]
    async fn stubborn_descendant_forces_sigkill_escalation() {
        // Audit round 5: leader-gone is not group-gone. A child that ignores
        // SIGTERM must keep the group alive until the SIGKILL escalation.
        // The async probe must NOT return as soon as the leader dies.
        let (_d, sup) = supervisor();
        // sh dies at SIGTERM; the trapped sleep 10 ignores it.
        let cfg = sh("trap '' TERM; sleep 10 & trap '' TERM; wait");
        let h = sup.spawn(cfg).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let t0 = std::time::Instant::now();
        sup.kill_child_pid(h.pid, 800).unwrap();
        let elapsed = t0.elapsed();
        // The escalation must have happened (the stubborn member is gone),
        // and the kill must not return before the grace deadline.
        assert!(
            elapsed >= Duration::from_millis(500),
            "must wait for the group (including stubborn members), took {elapsed:?}"
        );
        // The reaper thread needs a moment to reap the leader zombie.
        for _ in 0..40 {
            if !sup.pid_alive(h.pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!sup.pid_alive(h.pid), "the group must be fully gone");
        let _ = sup.reap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_timeline_records_pid_argv_and_exit() {
        // Audit round 10 instrumentation: every child is visible with
        // op/pid/argv/spawn/exit timestamps — the Linux git hang will show
        // up in this ring instead of vanishing.
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let cfg = SpawnConfig {
            cmd: "sh".into(),
            args: vec!["-c".into(), "exit 3".into()],
            cwd: dir.path().into(),
            env: EnvSpec::default_baseline(),
            owner: ProcessOwner::Daemon,
            capture: true,
            artifact_max: 1024 * 1024,
            network_isolation: NetworkIsolation::Inherit,
        };
        let out = sup
            .run(
                cfg,
                std::time::Duration::from_secs(10),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
        let spawns = sup.recent_spawns();
        let rec = spawns
            .iter()
            .find(|t| t.argv.contains("exit 3"))
            .expect("timeline records the spawned argv");
        assert!(rec.pid > 0);
        assert_eq!(rec.exit_code, Some(3));
        assert!(rec.exited_ms.is_some());
        assert!(rec.exited_ms.unwrap() >= rec.started_ms);
        assert_eq!(rec.owner, "Daemon");
        // A handful of extra spawns still land in the timeline.
        for _ in 0..3 {
            let _ = sup
                .run(
                    SpawnConfig {
                        cmd: "sh".into(),
                        args: vec!["-c".into(), "true".into()],
                        cwd: dir.path().into(),
                        env: EnvSpec::default_baseline(),
                        owner: ProcessOwner::Daemon,
                        capture: true,
                        artifact_max: 1024,
                        network_isolation: NetworkIsolation::Inherit,
                    },
                    std::time::Duration::from_secs(5),
                    CancellationToken::new(),
                )
                .await;
        }
        assert!(
            sup.recent_spawns().len() >= 4,
            "timeline records every spawn"
        );
    }

    #[ignore = "[perf] 300 sequential spawns — ring must stay bounded; run explicitly"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeline_ring_stays_bounded_under_300_spawns() {
        // Bounded ring under pressure: 300 sequential spawns never grow the
        // timeline past its cap.
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        for _ in 0..300 {
            let _ = sup
                .run(
                    SpawnConfig {
                        cmd: "sh".into(),
                        args: vec!["-c".into(), "true".into()],
                        cwd: dir.path().into(),
                        env: EnvSpec::default_baseline(),
                        owner: ProcessOwner::Daemon,
                        capture: true,
                        artifact_max: 1024,
                        network_isolation: NetworkIsolation::Inherit,
                    },
                    std::time::Duration::from_secs(5),
                    CancellationToken::new(),
                )
                .await;
        }
        assert!(
            sup.recent_spawns().len() >= 4,
            "timeline records every spawn"
        );
    }

    // ================= network-isolation honesty (audit 4/28/35-39) =====
    //
    // The Linux spawn backend (pre-exec unshare(CLONE_NEWNET) under
    // NetworkIsolation::DenyAll) must either isolate the child or refuse
    // the spawn TYPED — never warn-and-run unenforced. Platforms without
    // the backend refuse BEFORE spawn. The cfg(test) probe hook only
    // changes the REPORT; spawn code never consults it (a forced "backend
    // proven" claim must never downgrade a refusal into an unenforced
    // run).

    fn assert_isolation_refusal(err: &Error) {
        assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
        assert!(
            err.message.contains("sandbox unavailable") && err.message.contains("DenyAll"),
            "the typed refusal must name the sandbox and the isolation mode: {err:?}"
        );
    }

    #[test]
    fn default_isolation_is_inherit_and_ordinary_spawns_still_run() {
        assert_eq!(
            SpawnConfig::default().network_isolation,
            NetworkIsolation::Inherit,
            "the additive isolation field must default to Inherit"
        );
        let (_d, sup) = supervisor();
        let out = sup
            .run_sync(sh("echo inherit-ok"), Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout_head.contains("inherit-ok"));
    }

    #[cfg(not(target_os = "linux"))]
    fn deny_all(sh_cmd: &str) -> SpawnConfig {
        SpawnConfig {
            cmd: "/bin/sh".into(),
            args: vec!["-c".into(), sh_cmd.into()],
            cwd: std::env::temp_dir(),
            network_isolation: NetworkIsolation::DenyAll,
            ..Default::default()
        }
    }
    // The probe hook is process-global: forced-state tests serialize
    // through this lock so they never race each other.
    #[cfg(not(target_os = "linux"))]
    static NET_PROBE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    #[cfg(not(target_os = "linux"))]
    fn net_probe_lock() -> std::sync::MutexGuard<'static, ()> {
        NET_PROBE_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Read the REAL probe under the force-serialization lock: a parallel
    /// forced-state test must never race this read.
    #[cfg(not(target_os = "linux"))]
    fn real_probe_locked() -> NetworkEnforcement {
        let _lock = net_probe_lock();
        platform_network_enforcement()
    }

    // The guard's field exists ONLY for its Drop side (probe restore +
    // lock release); it is intentionally never read.
    #[cfg(not(target_os = "linux"))]
    #[allow(dead_code)]
    struct NetProbeGuard(std::sync::MutexGuard<'static, ()>);
    #[cfg(not(target_os = "linux"))]
    impl NetProbeGuard {
        fn force(v: NetworkEnforcement) -> NetProbeGuard {
            let lock = net_probe_lock();
            override_network_probe(Some(v));
            NetProbeGuard(lock)
        }
    }
    #[cfg(not(target_os = "linux"))]
    impl Drop for NetProbeGuard {
        fn drop(&mut self) {
            override_network_probe(None);
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn deny_all_is_refused_before_spawn_without_a_backend() {
        // macOS/windows: no backend exists, so every DenyAll request
        // refuses BEFORE spawn with the typed error on every entry point,
        // while the same supervisor still runs ordinary computation — the
        // refusal is isolation-specific, not a broken spawn layer.
        let (_d, sup) = supervisor();
        assert_eq!(
            real_probe_locked(),
            NetworkEnforcement::Unavailable,
            "no backend is implemented on this platform"
        );
        let err = sup.spawn(deny_all("true")).unwrap_err();
        assert_isolation_refusal(&err);
        assert!(sup.alive().is_empty(), "the refused spawn never existed");
        let err = sup
            .run_sync(deny_all("true"), Duration::from_secs(10), 4096, 4096)
            .unwrap_err();
        assert_isolation_refusal(&err);
        let err = sup
            .spawn_detached_with_pipes(deny_all("true"))
            .err()
            .expect("DenyAll must be refused before spawn without a backend");
        assert_isolation_refusal(&err);
        assert!(sup.alive().is_empty());
        let out = sup
            .run_sync(sh("echo control-ok"), Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        assert!(out.stdout_head.contains("control-ok"));
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deny_all_is_refused_before_spawn_on_the_async_path() {
        let (_d, sup) = supervisor();
        let err = sup
            .run(
                deny_all("true"),
                Duration::from_secs(10),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_isolation_refusal(&err);
        let out = sup
            .run(
                sh("echo async-ok"),
                Duration::from_secs(10),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.excerpt.contains("async-ok"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_forced_backend_claim_never_downgrades_a_refusal_into_a_run() {
        // The forced states round-trip and restore the real probe (the
        // hook is a test seam only; the guard serializes + restores).
        for v in [
            NetworkEnforcement::AppLevel,
            NetworkEnforcement::OsLevel,
            NetworkEnforcement::Unavailable,
        ] {
            let _g = NetProbeGuard::force(v);
            assert_eq!(platform_network_enforcement(), v);
        }
        assert_eq!(
            real_probe_locked(),
            NetworkEnforcement::Unavailable,
            "the real probe is restored after every forced state"
        );
        // Now the LIE: the cfg(test) probe claims the backend is proven.
        // Spawn code never consults the probe: on this platform the
        // backend does not exist, so the DenyAll request still refuses
        // typed before spawn — a false claim never yields an unenforced
        // child.
        let _guard = NetProbeGuard::force(NetworkEnforcement::OsLevel);
        assert_eq!(
            platform_network_enforcement(),
            NetworkEnforcement::OsLevel,
            "the probe hook must take effect for the lie to be meaningful"
        );
        let (_d, sup) = supervisor();
        let err = sup.spawn(deny_all("true")).unwrap_err();
        assert_isolation_refusal(&err);
        assert!(sup.alive().is_empty());
        let out = sup
            .run_sync(sh("echo control-ok"), Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
    }

    // ------------- BrokerOnly (audit item 8) mode semantics --------------

    #[test]
    fn broker_only_is_a_distinct_copyable_mode_with_a_loopback_endpoint() {
        let endpoint: std::net::SocketAddr = "127.0.0.1:43123".parse().unwrap();
        let mode = NetworkIsolation::BrokerOnly { endpoint };
        assert!(mode.is_broker_only());
        assert_eq!(mode, NetworkIsolation::BrokerOnly { endpoint });
        assert_ne!(mode, NetworkIsolation::Inherit);
        assert_ne!(mode, NetworkIsolation::DenyAll);
        // The isolation field still defaults to Inherit.
        assert_eq!(
            SpawnConfig::default().network_isolation,
            NetworkIsolation::Inherit
        );
        // The compile-time backend presence matches the platform (the
        // RUNTIME may still refuse typed — never a silent downgrade).
        assert_eq!(broker_only_supported(), cfg!(target_os = "linux"));
    }

    #[cfg(not(target_os = "linux"))]
    fn broker_only(sh_cmd: &str) -> SpawnConfig {
        SpawnConfig {
            cmd: "/bin/sh".into(),
            args: vec!["-c".into(), sh_cmd.into()],
            cwd: std::env::temp_dir(),
            network_isolation: NetworkIsolation::BrokerOnly {
                endpoint: "127.0.0.1:43123".parse().unwrap(),
            },
            ..Default::default()
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn broker_only_is_refused_before_spawn_without_a_backend() {
        // macOS/windows: the proxy flags stay application configuration;
        // an OS-confinement request is refused typed on every entry point
        // and no child is ever forked. This is the honest app-level state:
        // the daemon must report it, never claim confinement.
        let (_d, sup) = supervisor();
        assert!(!broker_only_supported());
        let err = sup.spawn(broker_only("true")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission);
        assert!(
            err.message.contains("NetworkIsolation::BrokerOnly")
                && err.message.contains("no BrokerOnly"),
            "the refusal must name the mode and the platform reason: {err:?}"
        );
        assert!(sup.alive().is_empty(), "the refused spawn never existed");
        let err = sup
            .run_sync(broker_only("true"), Duration::from_secs(10), 4096, 4096)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission);
        let err = sup
            .spawn_detached_with_pipes(broker_only("true"))
            .err()
            .expect("BrokerOnly must be refused before spawn without a backend");
        assert_eq!(err.kind, ErrorKind::Permission);
        assert!(sup.alive().is_empty());
        // The same supervisor still runs ordinary computation: the refusal
        // is isolation-specific.
        let out = sup
            .run_sync(sh("echo control-ok"), Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        assert!(out.stdout_head.contains("control-ok"));
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn broker_only_is_refused_before_spawn_on_the_async_path() {
        let (_d, sup) = supervisor();
        let err = sup
            .run(
                broker_only("true"),
                Duration::from_secs(10),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission);
        assert!(
            err.message.contains("NetworkIsolation::BrokerOnly"),
            "{err:?}"
        );
        assert!(sup.alive().is_empty());
    }

    // --------------------------- test env names (Phase F item 24) -------

    /// Test env readers accept BOTH the current `FAKTOR_TEST_*` spelling and
    /// the legacy `KP_*` one during the naming migration. The Faktor
    /// spelling wins when both are set; the legacy spelling emits a warning
    /// and reports itself so tests can assert the fallback.
    fn read_test_env(suffix: &str) -> Option<(String, bool)> {
        let current = format!("FAKTOR_TEST_{suffix}");
        if let Ok(value) = std::env::var(&current) {
            return Some((value, false));
        }
        let legacy = format!("KP_{suffix}");
        if let Ok(value) = std::env::var(&legacy) {
            tracing::warn!(
                legacy = %legacy,
                current = %current,
                "legacy test env spelling accepted during the naming migration; \
                 writers must use the Faktor name"
            );
            return Some((value, true));
        }
        None
    }

    /// Readers accept both spellings during the migration; the Faktor
    /// spelling wins and the legacy one still resolves (with a warning).
    #[test]
    fn test_env_reader_falls_back_to_the_legacy_spelling() {
        std::env::remove_var("FAKTOR_TEST_FALLBACK_PROBE");
        std::env::remove_var("KP_FALLBACK_PROBE");
        assert_eq!(read_test_env("FALLBACK_PROBE"), None);
        std::env::set_var("KP_FALLBACK_PROBE", "legacy-value");
        assert_eq!(
            read_test_env("FALLBACK_PROBE"),
            Some(("legacy-value".to_string(), true))
        );
        std::env::set_var("FAKTOR_TEST_FALLBACK_PROBE", "faktor-value");
        assert_eq!(
            read_test_env("FALLBACK_PROBE"),
            Some(("faktor-value".to_string(), false)),
            "the Faktor spelling must win when both are present"
        );
        std::env::remove_var("KP_FALLBACK_PROBE");
        assert_eq!(
            read_test_env("FALLBACK_PROBE"),
            Some(("faktor-value".to_string(), false))
        );
        std::env::remove_var("FAKTOR_TEST_FALLBACK_PROBE");
    }

    // ------------------------------ linux: the real unshare backend -----

    #[cfg(target_os = "linux")]
    const NET_PROBE_ENV: &str = "FAKTOR_TEST_TERMINAL_NET_PROBE";

    /// The probe child's env reader (Faktor spelling first, legacy fallback).
    #[cfg(target_os = "linux")]
    fn net_probe_env(suffix: &str) -> Option<String> {
        read_test_env(&format!("NET_{suffix}")).map(|(value, _legacy)| value)
    }

    #[cfg(target_os = "linux")]
    fn netns_inode() -> Option<u64> {
        // readlink("/proc/self/ns/net") -> "net:[4026532008]"
        let link = std::fs::read_link("/proc/self/ns/net").ok()?;
        let text = link.to_string_lossy();
        let inner = text.strip_prefix("net:[")?.strip_suffix(']')?;
        inner.parse().ok()
    }

    #[cfg(target_os = "linux")]
    fn net_probe_child_main() -> ! {
        // Runs INSIDE the spawned child (parent set NET_PROBE_ENV). Writes
        // a machine-readable report; the parent interprets it. A child
        // that cannot even write its report exits 3 (the parent then fails
        // on the missing file).
        let port: u16 = net_probe_env("TCP_PORT").unwrap().parse().unwrap();
        let udp_port: u16 = net_probe_env("UDP_PORT").unwrap().parse().unwrap();
        let uds = net_probe_env("UDS").unwrap();
        let report = net_probe_env("REPORT").unwrap();
        let compute = net_probe_env("COMPUTE").unwrap();
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!(
            "netns={}",
            netns_inode()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "unreadable".into())
        ));
        let tcp = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(3),
        );
        lines.push(format!(
            "tcp={}",
            if tcp.is_ok() { "connected" } else { "failed" }
        ));
        // Optional BrokerOnly probes: a second parent-side loopback port
        // (must be unreachable through the sandbox loopback relay) and a
        // non-loopback destination (must be unreachable for lack of any
        // route).
        if let Some(other) = net_probe_env("OTHER_TCP_PORT").and_then(|p| p.parse::<u16>().ok()) {
            let connect = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], other)),
                Duration::from_secs(1),
            );
            lines.push(format!(
                "other_tcp={}",
                if connect.is_ok() {
                    "connected"
                } else {
                    "failed"
                }
            ));
        }
        if let Some(external) = net_probe_env("EXTERNAL") {
            let connect = external
                .parse::<std::net::SocketAddr>()
                .ok()
                .and_then(|addr| {
                    std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).ok()
                });
            lines.push(format!(
                "external={}",
                if connect.is_some() {
                    "connected"
                } else {
                    "failed"
                }
            ));
        }
        let udp = (|| -> std::io::Result<usize> {
            let s = std::net::UdpSocket::bind("127.0.0.1:0")?;
            s.send_to(
                b"probe",
                std::net::SocketAddr::from(([127, 0, 0, 1], udp_port)),
            )
        })();
        lines.push(format!(
            "udp={}",
            if udp.is_ok() { "delivered" } else { "failed" }
        ));
        let unix = std::os::unix::net::UnixStream::connect(&uds);
        lines.push(format!(
            "unix={}",
            if unix.is_ok() { "connected" } else { "failed" }
        ));
        let computed = format!("computed-{}", 6 * 7);
        let compute_ok = std::fs::write(&compute, &computed).is_ok();
        lines.push(format!(
            "compute={}",
            if compute_ok { "ok" } else { "failed" }
        ));
        let report_ok = std::fs::write(&report, lines.join("\n")).is_ok();
        std::process::exit(if report_ok { 0 } else { 3 });
    }

    /// Everything the probe child needs to reach its targets and report
    /// back (linux-only test scaffolding).
    #[cfg(target_os = "linux")]
    struct NetProbeTargets<'a> {
        tcp_port: u16,
        udp_port: u16,
        uds: &'a std::path::Path,
        report: &'a std::path::Path,
        compute: &'a std::path::Path,
    }

    #[cfg(target_os = "linux")]
    fn spawn_probe_child(
        sup: &Arc<ProcessSupervisor>,
        self_exe: &std::path::Path,
        isolation: NetworkIsolation,
        targets: &NetProbeTargets<'_>,
    ) -> Result<SyncRunOutput, Error> {
        // Re-exec THIS test binary with an exact filter: only the probe
        // test runs, its first statement detects the child mode and exits
        // after writing the report. The probe vars ride an explicit
        // EnvSpec — no daemon environment is inherited.
        std::env::set_var(NET_PROBE_ENV, "1");
        std::env::set_var("FAKTOR_TEST_NET_TCP_PORT", targets.tcp_port.to_string());
        std::env::set_var("FAKTOR_TEST_NET_UDP_PORT", targets.udp_port.to_string());
        std::env::set_var(
            "FAKTOR_TEST_NET_UDS",
            targets.uds.to_string_lossy().into_owned(),
        );
        std::env::set_var(
            "FAKTOR_TEST_NET_REPORT",
            targets.report.to_string_lossy().into_owned(),
        );
        std::env::set_var(
            "FAKTOR_TEST_NET_COMPUTE",
            targets.compute.to_string_lossy().into_owned(),
        );
        let probe_env = EnvSpec::Explicit(vec![
            (NET_PROBE_ENV.into(), "1".into()),
            (
                "FAKTOR_TEST_NET_TCP_PORT".into(),
                targets.tcp_port.to_string().into(),
            ),
            (
                "FAKTOR_TEST_NET_UDP_PORT".into(),
                targets.udp_port.to_string().into(),
            ),
            (
                "FAKTOR_TEST_NET_UDS".into(),
                targets.uds.to_string_lossy().into_owned().into(),
            ),
            (
                "FAKTOR_TEST_NET_REPORT".into(),
                targets.report.to_string_lossy().into_owned().into(),
            ),
            (
                "FAKTOR_TEST_NET_COMPUTE".into(),
                targets.compute.to_string_lossy().into_owned().into(),
            ),
        ]);
        let cfg = SpawnConfig {
            cmd: self_exe.to_string_lossy().into_owned(),
            args: vec![
                "--exact".into(),
                "tests::deny_all_spawn_isolates_the_child_or_refuses_typed".into(),
            ],
            cwd: std::env::temp_dir(),
            env: probe_env,
            owner: ProcessOwner::Daemon,
            capture: true,
            artifact_max: 1024 * 1024,
            network_isolation: isolation,
        };
        sup.run_sync(cfg, Duration::from_secs(60), 64 * 1024, 64 * 1024)
    }

    /// Serializes the DenyAll spawn tests: the forced-unshare hook is
    /// process-global, so the real-backend test and the refusal test must
    /// never overlap (each asserts the global proof state).
    #[cfg(target_os = "linux")]
    static DENY_ALL_SPAWN_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    #[cfg(target_os = "linux")]
    fn deny_all_spawn_lock() -> std::sync::MutexGuard<'static, ()> {
        DENY_ALL_SPAWN_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deny_all_spawn_isolates_the_child_or_refuses_typed() {
        // Child mode: the parent spawned THIS test binary again under
        // NetworkIsolation::DenyAll (or Inherit for the control) with the
        // probe env set. Exit before touching any parent-side state.
        if std::env::var_os(NET_PROBE_ENV).is_some() {
            net_probe_child_main();
        }
        let _serial = deny_all_spawn_lock();
        // Parent mode. Host endpoints live in the PARENT netns: an
        // isolated child must NOT reach the TCP/UDP ones, while the unix
        // socket (not namespaced) must STAY reachable — a failure limited
        // to inet sockets is a network-namespace effect, not a blanket
        // syscall deny.
        let (_d, sup) = supervisor();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_port = tcp.local_addr().unwrap().port();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_port = udp.local_addr().unwrap().port();
        let uds_path = _d.path().join("probe.sock");
        let _uds = std::os::unix::net::UnixListener::bind(&uds_path).unwrap();
        let parent_netns = netns_inode().expect("parent /proc/self/ns/net readable");
        let self_exe = std::env::current_exe().unwrap();
        // Pre-spawn proof state: no DENY-ALL spawn has succeeded in this
        // process, so the report must not claim OsLevel — capability
        // existence (this test may even run as root) proves nothing by
        // itself. A proven BrokerOnly backend is legitimate here: it is a
        // different (weaker) proof that never implies the unshare path.
        assert_ne!(
            platform_network_enforcement(),
            NetworkEnforcement::OsLevel,
            "the DenyAll unshare path has not proven itself active at spawn yet"
        );
        // CONTROL under Inherit: the same probe must reach every endpoint
        // and report the PARENT netns inode — when it fails under DenyAll
        // below, the failure is caused by isolation, not by the probe.
        let control_report = _d.path().join("report-control.txt");
        let control_compute = _d.path().join("compute-control.txt");
        let out = spawn_probe_child(
            &sup,
            &self_exe,
            NetworkIsolation::Inherit,
            &NetProbeTargets {
                tcp_port,
                udp_port,
                uds: &uds_path,
                report: &control_report,
                compute: &control_compute,
            },
        )
        .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        let report = std::fs::read_to_string(&control_report).expect("control probe report");
        assert!(
            report.contains(&format!("netns={parent_netns}")),
            "the inherit child shares the parent netns: {report}"
        );
        assert!(report.contains("tcp=connected"), "{report}");
        assert!(report.contains("udp=delivered"), "{report}");
        assert!(report.contains("unix=connected"), "{report}");
        assert!(report.contains("compute=ok"), "{report}");
        assert_eq!(
            std::fs::read_to_string(&control_compute).unwrap(),
            "computed-42"
        );
        // DENY-ALL: adaptive to the host's permission state. A host that
        // grants the netns unshare yields an ISOLATED child (report
        // proves it); a host whose kernel/user-namespace policy refuses it
        // yields a TYPED spawn refusal. Both are fail-closed — no branch
        // ever runs the child unisolated under DenyAll.
        let deny_report = _d.path().join("report-deny.txt");
        let deny_compute = _d.path().join("compute-deny.txt");
        match spawn_probe_child(
            &sup,
            &self_exe,
            NetworkIsolation::DenyAll,
            &NetProbeTargets {
                tcp_port,
                udp_port,
                uds: &uds_path,
                report: &deny_report,
                compute: &deny_compute,
            },
        ) {
            Ok(out) => {
                assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
                let report = std::fs::read_to_string(&deny_report).expect("isolated probe report");
                let child_netns: u64 = report
                    .lines()
                    .find_map(|l| l.strip_prefix("netns="))
                    .expect("netns line")
                    .parse()
                    .expect("netns inode parses");
                assert_ne!(
                    child_netns, parent_netns,
                    "a DenyAll child must sit in a FRESH network namespace: {report}"
                );
                assert!(
                    report.contains("tcp=failed"),
                    "an empty netns must refuse the TCP connect to the parent listener: {report}"
                );
                assert!(
                    report.contains("udp=failed"),
                    "an empty netns must refuse the UDP send to the parent socket: {report}"
                );
                assert!(
                    report.contains("unix=connected"),
                    "unix sockets are not network-namespaced — the denial must be \
                     network-scoped, not a blanket syscall deny: {report}"
                );
                assert!(
                    report.contains("compute=ok"),
                    "ordinary non-network computation must succeed in the isolated child: {report}"
                );
                assert_eq!(
                    std::fs::read_to_string(&deny_compute).unwrap(),
                    "computed-42",
                    "the isolated child's non-network filesystem work must land intact"
                );
                // This successful DenyAll spawn PROVES the unshare path
                // active at spawn: the report may now claim OsLevel.
                assert_eq!(
                    platform_network_enforcement(),
                    NetworkEnforcement::OsLevel,
                    "a successful DenyAll spawn is the proof"
                );
            }
            Err(err) => {
                assert_isolation_refusal(&err);
                assert!(
                    !deny_report.exists(),
                    "the refused DenyAll child never exec'd (its probe never ran): {err:?}"
                );
                assert!(
                    sup.alive().is_empty(),
                    "no process may exist after the typed refusal: {err:?}"
                );
                assert_ne!(
                    platform_network_enforcement(),
                    NetworkEnforcement::OsLevel,
                    "a refused unshare must not prove the DenyAll backend; Required keeps \
                     failing closed: {err:?}"
                );
            }
        }
        // The supervisor stays healthy and ordinary computation still runs
        // after either branch.
        let out = sup
            .run_sync(sh("echo tail-ok"), Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forced_unshare_failure_refuses_required_and_the_program_body_never_runs() {
        // Kernel/user-namespace refusal simulation: the pre-exec
        // unshare(CLONE_NEWNET) fails with EPERM. The DenyAll spawn must
        // refuse typed BEFORE exec — the program body (a marker write) must
        // never run, no process may exist, and the same supervisor must
        // recover for ordinary spawns afterwards. This is the spawn-layer
        // half of "Required never becomes a warn-and-run downgrade".
        let _serial = deny_all_spawn_lock();
        let (_d, sup) = supervisor();
        let marker = _d.path().join("program-body-ran.txt");
        let mut cfg = sh(&format!("echo ran > '{}'", marker.display()));
        cfg.network_isolation = NetworkIsolation::from(NetworkIsolationRequirement::DenyAll);
        super::sandbox::force_unshare_failure_for_tests(true);
        let result = sup.run_sync(cfg, Duration::from_secs(10), 4096, 4096);
        super::sandbox::force_unshare_failure_for_tests(false);
        let err = result.expect_err("a failed unshare must refuse the spawn");
        assert_isolation_refusal(&err);
        assert!(
            !marker.exists(),
            "the refused child never exec'd: no program body may run"
        );
        assert!(sup.alive().is_empty(), "no process may exist after refusal");
        let out = sup
            .run_sync(sh("echo recovered"), Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        assert!(out.stdout_head.contains("recovered"));
    }

    // ------------------------- linux: the real BrokerOnly backend --------

    /// Child-mode marker for the BrokerOnly confinement probe. Deliberately
    /// NOT set process-wide by the parent (unlike the DenyAll probe env):
    /// it rides only the explicit child `EnvSpec`, so parallel tests can
    /// never mistake themselves for the probe child.
    #[cfg(target_os = "linux")]
    const BROKER_ONLY_PROBE_ENV: &str = "FAKTOR_TEST_BROKER_ONLY_PROBE";

    /// Child-mode marker for the BrokerOnly DevTools-exposure probe.
    #[cfg(target_os = "linux")]
    const BROKER_ONLY_EXPOSE_ENV: &str = "FAKTOR_TEST_BROKER_ONLY_EXPOSE";

    /// True when a BrokerOnly refusal is the documented unprivileged-host
    /// outcome (no CAP_SYS_ADMIN / no unprivileged network namespaces). The
    /// test then SKIPS with an explicit typed line — never a silent pass —
    /// and never falls back to running the child unconfined.
    #[cfg(target_os = "linux")]
    fn broker_only_skip_if_unprivileged(err: &Error) -> bool {
        let unprivileged = err.kind == ErrorKind::Permission
            && (err.message.contains("CAP_SYS_ADMIN")
                || err.message.contains("Operation not permitted")
                || err.message.contains("Permission denied"));
        if unprivileged {
            eprintln!(
                "SKIP (typed, unprivileged host): BrokerOnly namespace backend refused \
                 fail-closed and no child ran: {err}"
            );
        }
        unprivileged
    }

    #[cfg(target_os = "linux")]
    #[allow(clippy::too_many_arguments)]
    fn spawn_broker_only_probe_child(
        sup: &Arc<ProcessSupervisor>,
        self_exe: &std::path::Path,
        endpoint: std::net::SocketAddr,
        tcp_port: u16,
        udp_port: u16,
        other_tcp_port: u16,
        uds: &std::path::Path,
        report: &std::path::Path,
        compute: &std::path::Path,
    ) -> Result<SyncRunOutput, Error> {
        let probe_env = EnvSpec::Explicit(vec![
            (BROKER_ONLY_PROBE_ENV.into(), "1".into()),
            (
                "FAKTOR_TEST_NET_TCP_PORT".into(),
                tcp_port.to_string().into(),
            ),
            (
                "FAKTOR_TEST_NET_UDP_PORT".into(),
                udp_port.to_string().into(),
            ),
            (
                "FAKTOR_TEST_NET_OTHER_TCP_PORT".into(),
                other_tcp_port.to_string().into(),
            ),
            // TEST-NET-3: routed nowhere, unreachable without an external
            // route (the sandbox namespace has none).
            ("FAKTOR_TEST_NET_EXTERNAL".into(), "203.0.113.7:9".into()),
            (
                "FAKTOR_TEST_NET_UDS".into(),
                uds.to_string_lossy().into_owned().into(),
            ),
            (
                "FAKTOR_TEST_NET_REPORT".into(),
                report.to_string_lossy().into_owned().into(),
            ),
            (
                "FAKTOR_TEST_NET_COMPUTE".into(),
                compute.to_string_lossy().into_owned().into(),
            ),
        ]);
        let cfg = SpawnConfig {
            cmd: self_exe.to_string_lossy().into_owned(),
            args: vec![
                "--exact".into(),
                "tests::broker_only_spawn_reaches_only_the_broker_endpoint".into(),
            ],
            cwd: std::env::temp_dir(),
            env: probe_env,
            owner: ProcessOwner::Daemon,
            capture: true,
            artifact_max: 1024 * 1024,
            network_isolation: NetworkIsolation::BrokerOnly { endpoint },
        };
        sup.run_sync(cfg, Duration::from_secs(60), 64 * 1024, 64 * 1024)
    }

    /// The audit item 8 scenario: a child under `BrokerOnly` reaches the
    /// broker endpoint through the sandbox relay, cannot reach a DIFFERENT
    /// parent-side loopback port, cannot reach any external destination, and
    /// sits in a fresh network namespace. A host that cannot create the
    /// namespace refuses typed (skipped loudly here, never silently passed
    /// and never run unconfined).
    #[cfg(target_os = "linux")]
    #[test]
    fn broker_only_spawn_reaches_only_the_broker_endpoint() {
        // Child mode: the parent re-executed THIS test binary under
        // BrokerOnly with the probe env; report and exit.
        if std::env::var_os(BROKER_ONLY_PROBE_ENV).is_some() {
            net_probe_child_main();
        }
        let (_d, sup) = supervisor();
        // The "real broker": a parent-side loopback listener at the exact
        // endpoint the child may reach.
        let broker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = broker.local_addr().unwrap();
        // A second parent-side loopback listener: must stay unreachable —
        // the sandbox loopback is private, only the relayed endpoint works.
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other_port = other.local_addr().unwrap().port();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_port = udp.local_addr().unwrap().port();
        let uds_path = _d.path().join("broker-only-probe.sock");
        let _uds = std::os::unix::net::UnixListener::bind(&uds_path).unwrap();
        let report = _d.path().join("report-broker-only.txt");
        let compute = _d.path().join("compute-broker-only.txt");
        let parent_netns = netns_inode().expect("parent /proc/self/ns/net readable");
        let self_exe = std::env::current_exe().unwrap();
        let out = match spawn_broker_only_probe_child(
            &sup,
            &self_exe,
            endpoint,
            endpoint.port(),
            udp_port,
            other_port,
            &uds_path,
            &report,
            &compute,
        ) {
            Ok(out) => out,
            Err(err) => {
                if broker_only_skip_if_unprivileged(&err) {
                    return;
                }
                panic!("BrokerOnly spawn must either confine or refuse typed: {err:?}");
            }
        };
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
        let report = std::fs::read_to_string(&report).expect("broker-only probe report");
        let child_netns: u64 = report
            .lines()
            .find_map(|l| l.strip_prefix("netns="))
            .expect("netns line")
            .parse()
            .expect("netns inode parses");
        assert_ne!(
            child_netns, parent_netns,
            "a BrokerOnly child must sit in its own network namespace: {report}"
        );
        assert!(
            report.contains("tcp=connected"),
            "the broker endpoint must be reachable through the sandbox relay: {report}"
        );
        assert!(
            report.contains("other_tcp=failed"),
            "a second parent-side loopback port must NOT be reachable: {report}"
        );
        assert!(
            report.contains("external=failed"),
            "no external destination may be reachable from the sandbox: {report}"
        );
        assert!(
            report.contains("unix=connected"),
            "AF_UNIX is not network-namespaced; the denial must be network-scoped: {report}"
        );
        assert!(
            report.contains("compute=ok"),
            "ordinary computation must succeed in the confined child: {report}"
        );
        assert_eq!(std::fs::read_to_string(&compute).unwrap(), "computed-42");
        // The host broker really received the relayed connection.
        broker.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut accepted = false;
        while std::time::Instant::now() < deadline && !accepted {
            match broker.accept() {
                Ok(_) => accepted = true,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("broker accept failed: {e}"),
            }
        }
        assert!(
            accepted,
            "the confined child's broker connection must reach the host broker"
        );
        // A successful BrokerOnly spawn proves its backend active at spawn
        // (the stronger DenyAll proof may already have been recorded by a
        // parallel test in this process).
        let probe = platform_network_enforcement();
        assert!(
            matches!(
                probe,
                NetworkEnforcement::OsLevel | NetworkEnforcement::OsLevelBrokerOnly
            ),
            "a successful BrokerOnly spawn is the proof: {probe:?}"
        );
        let out = sup
            .run_sync(sh("echo tail-ok"), Duration::from_secs(10), 4096, 4096)
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
    }

    /// Child mode of the DevTools-exposure probe: bind an in-sandbox
    /// loopback listener, report its port to a FILE (libtest captures
    /// stdout), accept exactly one relayed connection and answer `PONG`.
    #[cfg(target_os = "linux")]
    fn broker_only_expose_child_main() -> ! {
        use std::io::{BufRead, BufReader, Write};
        let report = std::env::var("FAKTOR_TEST_BROKER_ONLY_EXPOSE_REPORT").expect("report path");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("in-sandbox bind");
        let port = listener.local_addr().unwrap().port();
        std::fs::write(&report, format!("PORT={port}\n")).expect("port report");
        let (mut conn, _) = listener.accept().expect("relayed connection");
        let mut line = String::new();
        let _ = BufReader::new(conn.try_clone().unwrap()).read_line(&mut line);
        let ok = line.trim() == "PING" && conn.write_all(b"PONG\n").is_ok();
        std::process::exit(if ok { 0 } else { 4 });
    }

    /// The reverse direction of the bridge (the browser DevTools control
    /// channel): a host-side loopback listener exposed for one in-sandbox
    /// port relays INTO the namespace.
    #[cfg(target_os = "linux")]
    #[test]
    fn broker_only_devtools_exposure_relays_into_the_sandbox() {
        use std::io::{BufRead, BufReader, Write};
        if std::env::var_os(BROKER_ONLY_EXPOSE_ENV).is_some() {
            broker_only_expose_child_main();
        }
        let (_d, sup) = supervisor();
        let report = _d.path().join("expose-port.txt");
        let self_exe = std::env::current_exe().unwrap();
        let cfg = SpawnConfig {
            cmd: self_exe.to_string_lossy().into_owned(),
            args: vec![
                "--exact".into(),
                "tests::broker_only_devtools_exposure_relays_into_the_sandbox".into(),
            ],
            cwd: std::env::temp_dir(),
            env: EnvSpec::Explicit(vec![
                (BROKER_ONLY_EXPOSE_ENV.into(), "1".into()),
                (
                    "FAKTOR_TEST_BROKER_ONLY_EXPOSE_REPORT".into(),
                    report.to_string_lossy().into_owned().into(),
                ),
            ]),
            owner: ProcessOwner::Daemon,
            capture: false,
            artifact_max: 1024,
            // The broker endpoint is irrelevant here (the child never dials
            // it); a high loopback port is required so the unprivileged
            // namespace thread can bind it.
            network_isolation: NetworkIsolation::BrokerOnly {
                endpoint: "127.0.0.1:45999".parse().unwrap(),
            },
        };
        let spawned = match sup.spawn_detached_with_pipes(cfg) {
            Ok(spawned) => spawned,
            Err(err) => {
                if broker_only_skip_if_unprivileged(&err) {
                    return;
                }
                panic!("BrokerOnly spawn must either confine or refuse typed: {err:?}");
            }
        };
        let pid = spawned.child_pid;
        // Wait for the child's in-sandbox listener to report its port.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !report.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = std::fs::read_to_string(&report).expect("child port report");
        let port: u16 = text
            .trim()
            .strip_prefix("PORT=")
            .expect("PORT= line")
            .parse()
            .expect("port parses");
        let bridge = spawned
            .network_bridge
            .clone()
            .expect("a BrokerOnly spawn carries its bridge");
        let host_port = bridge
            .expose_loopback_port(port)
            .expect("exposure of the in-sandbox port");
        let mut conn = std::net::TcpStream::connect(("127.0.0.1", host_port))
            .expect("host-side exposed port accepts");
        conn.write_all(b"PING\n").unwrap();
        let mut reply = String::new();
        BufReader::new(conn).read_line(&mut reply).unwrap();
        assert_eq!(
            reply.trim(),
            "PONG",
            "the exposed port must relay into the sandbox"
        );
        let _ = sup.kill_child_pid(pid, 500);
        let _ = sup.reap();
        assert!(
            !sup.alive().iter().any(|child| child.pid == pid),
            "the exposure probe child must be reaped"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broker_only_non_loopback_endpoint_is_refused_typed() {
        // Endpoint validation happens BEFORE the namespace is created, so
        // this is testable without privileges and must never fork.
        let (_d, sup) = supervisor();
        for endpoint in ["192.0.2.1:9999", "127.0.0.1:0"] {
            let mut cfg = sh("echo body-must-not-run");
            cfg.network_isolation = NetworkIsolation::BrokerOnly {
                endpoint: endpoint.parse().unwrap(),
            };
            let err = sup
                .run_sync(cfg, Duration::from_secs(10), 4096, 4096)
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::Permission, "{endpoint}: {err:?}");
            assert!(
                err.message.contains("NetworkIsolation::BrokerOnly"),
                "{endpoint}: {err:?}"
            );
            assert!(
                !err.message.contains("body-must-not-run"),
                "the refusal must be typed before exec: {err:?}"
            );
            assert!(sup.alive().is_empty(), "no process may exist: {err:?}");
        }
        assert!(
            broker_only_supported(),
            "this test module only exists on the BrokerOnly platform"
        );
    }
}

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
