//! Agent-child HOME machinery (audit item 6), out-of-line from `lib.rs`.
//!
//! The per-session scratch HOME under the daemon data dir (`<base>/<session>/
//! home` with the conventional XDG subdirectories), created no-follow below
//! the trusted base; the [`ProcessSupervisor`] install/readback/apply
//! methods; the real-home credential-path vocabulary the Linux confinement
//! policy must never grant; and the pre-spawn confinement gate over
//! credential-overlapping roots.

use std::path::PathBuf;
#[cfg(all(test, target_os = "linux"))]
use std::sync::Mutex;

use faktor_core::error::Error;
use faktor_core::id::SessionId;

#[cfg(not(target_os = "linux"))]
use crate::NetworkIsolation;
use crate::ProcessSupervisor;
use crate::{AgentHomePolicy, EnvSpec, ProcessOwner, SpawnConfig};
#[cfg(not(target_os = "linux"))]
use crate::{DENY_ALL_REFUSAL_PREFIX, FILESYSTEM_REFUSAL_PREFIX};

/// The data-dir subdirectory holding per-session agent scratch homes:
/// `<data dir>/<AGENT_HOME_DIR>/<session>/home`.
pub const AGENT_HOME_DIR: &str = "agent-home";

/// Create (or re-verify) the per-session scratch HOME under `base` and
/// return it: `<base>/<session>/home`, plus the conventional XDG
/// subdirectories, all owner-only (`0700`) on unix. The trusted `base` is
/// resolved once (daemon paths may sit under platform symlinks such as
/// macOS `/var`), then every component BELOW it is walked
/// component-by-component with no-follow semantics: a symlink or non-
/// directory object at or under the base is a typed refusal, so a hostile
/// pre-existing link under the data dir can never redirect the agent HOME
/// onto a real credential store. Idempotent: a second call re-verifies the
/// same paths. A failure is a typed permission error and the spawn is
/// refused BEFORE any process exists.
pub fn ensure_agent_scratch_home(
    base: &std::path::Path,
    session: SessionId,
) -> Result<PathBuf, Error> {
    // The base is the daemon's own data-dir subdirectory: resolve it once
    // (ancestors of a trusted daemon path may legitimately be symlinks —
    // `/var` on macOS is one), then treat every component BELOW the resolved
    // base with no-follow semantics, so a planted link under the data dir
    // can never redirect the agent HOME.
    std::fs::create_dir_all(base).map_err(|e| {
        Error::permission(format!(
            "agent scratch base {} cannot be created: {e}",
            base.display()
        ))
    })?;
    let base = base.canonicalize().map_err(|e| {
        Error::permission(format!(
            "agent scratch base {} cannot be resolved: {e}",
            base.display()
        ))
    })?;
    let session_root = base.join(session.to_string());
    create_dirs_no_follow(&base, &session_root)?;
    let home = session_root.join("home");
    create_dirs_no_follow(&base, &home)?;
    for sub in [".config", ".cache", ".local/share", ".local/state"] {
        create_dirs_no_follow(&base, &home.join(sub))?;
    }
    Ok(home)
}

/// No-follow directory creation for `path` strictly below the trusted
/// `base`: every component between `base` (exclusive) and `path`
/// (inclusive) must be a real directory — never a symlink or any other
/// object — and missing components are created owner-only on unix. A
/// component that escapes `base` is a typed refusal.
fn create_dirs_no_follow(base: &std::path::Path, path: &std::path::Path) -> Result<(), Error> {
    let relative = path.strip_prefix(base).map_err(|_| {
        Error::permission(format!(
            "scratch home path {} escapes its base {}",
            path.display(),
            base.display()
        ))
    })?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::Normal(_) => current.push(component),
            _ => {
                return Err(Error::permission(format!(
                    "scratch home path contains a non-normal component: {}",
                    path.display()
                )));
            }
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(Error::permission(format!(
                        "scratch home component {} is a symlink; refusing to follow it",
                        current.display()
                    )));
                }
                if !meta.is_dir() {
                    return Err(Error::permission(format!(
                        "scratch home component {} is not a directory",
                        current.display()
                    )));
                }
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::permission(format!(
                    "scratch home component {} cannot be inspected: {e}",
                    current.display()
                )));
            }
        }
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&current) {
            // A concurrent creator won the race: verify what appeared.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = std::fs::symlink_metadata(&current).map_err(|e| {
                    Error::permission(format!(
                        "scratch home component {} appeared but cannot be inspected: {e}",
                        current.display()
                    ))
                })?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(Error::permission(format!(
                        "scratch home component {} was substituted during creation",
                        current.display()
                    )));
                }
            }
            Err(e) => {
                return Err(Error::permission(format!(
                    "scratch home component {} cannot be created: {e}",
                    current.display()
                )));
            }
            Ok(()) => {}
        }
    }
    Ok(())
}

/// The installed agent-HOME contract: the scratch base directory plus the
/// typed policy. Read back by doctor/tests through
/// [`ProcessSupervisor::agent_home_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHomeSettings {
    pub base: PathBuf,
    pub policy: AgentHomePolicy,
}
impl ProcessSupervisor {
    /// Install the agent-child HOME contract (daemon assembly, once; the
    /// owner passes `<data dir>/agent-home`): every `Session`- and
    /// `Verification`-owned child then runs with a per-session scratch HOME
    /// unless the policy is the explicit operator real-home grant. The
    /// setting is last-wins and read per spawn; a bare supervisor never
    /// installed with it keeps the historical environment exactly.
    pub fn configure_agent_home(&self, base: PathBuf, policy: AgentHomePolicy) {
        *self
            .agent_home
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(AgentHomeSettings { base, policy });
    }

    /// The installed agent-HOME contract, for doctor/evidence readback.
    pub fn agent_home_state(&self) -> Option<AgentHomeSettings> {
        self.agent_home
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Apply the agent-HOME contract to one spawn BEFORE the command is
    /// built: for a `Session`/`Verification` owner under the scratch policy
    /// the per-session scratch home is created (fail closed typed on any
    /// filesystem refusal) and the resolved environment's HOME anchors are
    /// rewritten to it. Every other owner and the explicit operator grant
    /// leave the spec untouched. The rewritten spec is an `Explicit` view of
    /// the resolved entries, so the deny-set applies exactly as before.
    pub(crate) fn prepare_agent_home(&self, cfg: &mut SpawnConfig) -> Result<(), Error> {
        let settings = match self.agent_home_state() {
            Some(settings) => settings,
            None => return Ok(()),
        };
        let scratch = match (&cfg.owner, settings.policy) {
            (ProcessOwner::Session(id), AgentHomePolicy::Scratch) => *id,
            (ProcessOwner::Verification(id), AgentHomePolicy::Scratch) => *id,
            _ => return Ok(()),
        };
        let home = ensure_agent_scratch_home(&settings.base, scratch)?;
        let mut env = cfg.env.resolve();
        // Rustup resolves its default toolchain through `$RUSTUP_HOME` (or
        // the real HOME). The scratch rewrite below would otherwise point an
        // unset RUSTUP_HOME/CARGO_HOME at an empty scratch home, making every
        // derived `cargo` check fail with "no default toolchain configured".
        // Preserve the DAEMON's approved toolchain homes when the spec did
        // not already carry them; credentials are not part of this anchor.
        preserve_toolchain_homes(&mut env);
        faktor_core::command::apply_scratch_home(&mut env, &home);
        cfg.env = EnvSpec::Explicit(env);
        Ok(())
    }
}

/// Preserve the daemon's approved Rust toolchain homes across the scratch
/// HOME rewrite. Only real, existing directories are added, and an explicit
/// value in the spec always wins.
fn preserve_toolchain_homes(env: &mut Vec<(std::ffi::OsString, std::ffi::OsString)>) {
    let looks_like = |name: &std::ffi::OsStr, expected: &str| {
        name.to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(expected))
    };
    let real_home = env
        .iter()
        .find(|(name, _)| looks_like(name, "HOME"))
        .map(|(_, value)| std::path::PathBuf::from(value))
        .or_else(|| {
            env.iter()
                .find(|(name, _)| looks_like(name, "USERPROFILE"))
                .map(|(_, value)| std::path::PathBuf::from(value))
        });
    let Some(real_home) = real_home else {
        return;
    };
    for (name, subpath) in [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")] {
        if env.iter().any(|(existing, _)| looks_like(existing, name)) {
            continue;
        }
        let candidate = real_home.join(subpath);
        if candidate.is_dir() {
            env.push((name.into(), candidate.into_os_string()));
        }
    }
}

/// Test-only override of the real-home credential paths (Linux Landlock
/// tests): lets a test point the confinement policy at a hermetic temp home
/// without mutating the process environment. Never compiled into production
/// builds; the real function otherwise reads HOME/USERPROFILE directly.
#[cfg(all(test, target_os = "linux"))]
static CREDENTIAL_PATH_OVERRIDE: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);

/// Set (or clear) the credential-path test override.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn override_credential_paths_for_tests(paths: Option<Vec<PathBuf>>) {
    *CREDENTIAL_PATH_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = paths;
}

/// The daemon's real-home credential stores (audit item 6): the concrete
/// paths the confinement policy must never transitively grant, derived from
/// the daemon's own HOME/USERPROFILE. Empty when the daemon carries no home
/// (then no store can be reached through a home path). Pure environment
/// read; the child's environment may point somewhere else entirely. Used by
/// the Linux Landlock seam (the only platform with a confinement backend).
#[cfg(target_os = "linux")]
pub(crate) fn real_home_credential_paths() -> Vec<PathBuf> {
    #[cfg(all(test, target_os = "linux"))]
    if let Some(paths) = CREDENTIAL_PATH_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return paths;
    }
    let mut homes: Vec<PathBuf> = Vec::new();
    for name in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(name) {
            if !value.is_empty() {
                let path = PathBuf::from(value);
                if !homes.contains(&path) {
                    homes.push(path);
                }
            }
        }
    }
    let mut credentials = Vec::new();
    for home in homes {
        for path in faktor_core::command::credential_paths_for_home(&home) {
            if !credentials.contains(&path) {
                credentials.push(path);
            }
        }
    }
    credentials
}

/// Pre-spawn fail-closed gate for OS-confinement requests. On Linux the
/// real backends exist (pre-exec `unshare(CLONE_NEWNET)` and/or Landlock)
/// and are ALWAYS attempted — capability heuristics never pre-judge them,
/// only the actual syscalls prove or refuse; a failure refuses the spawn
/// typed via [`spawn_failure`]. Every other platform has no filesystem
/// backend at all, so a `Required` workspace demand is refused BEFORE
/// spawn, typed, and no process is forked; `BestEffort` falls back to the
/// honest application-policy-only execution (the projection never claimed
/// otherwise).
///
/// On every platform this gate ALSO refuses a `Required` confinement whose
/// configured roots cover a real-home credential store (audit item 6): the
/// kernel can only grant a whole subtree, so a root covering `~/.ssh` (for
/// example a workspace rooted at the home directory) can never be confined
/// without granting that store — the spawn is refused typed, never run
/// while silently granting credentials. `BestEffort` demands continue and
/// the rule builder drops the overlapping root, keeping the store denied.
#[cfg(target_os = "linux")]
pub(crate) fn isolation_gate(cfg: &SpawnConfig) -> Result<(), Error> {
    if !cfg.filesystem_isolation.is_required() {
        return Ok(());
    }
    let credentials = real_home_credential_paths();
    for root in cfg
        .filesystem_isolation
        .roots()
        .iter()
        .chain(cfg.filesystem_isolation.writable_roots())
    {
        for credential in &credentials {
            if faktor_core::command::paths_overlap(root, credential) {
                return Err(Error::permission(format!(
                    "sandbox unavailable: refusing spawn under \
                     FilesystemIsolation::Workspace(required) of `{}` BEFORE spawn: confinement \
                     root {} covers the home credential store {}; the confinement policy never \
                     grants home credentials (move the workspace out of the home directory or \
                     use an explicit external-cwd grant)",
                    cfg.cmd,
                    root.display(),
                    credential.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn isolation_gate(cfg: &SpawnConfig) -> Result<(), Error> {
    if cfg.network_isolation == NetworkIsolation::DenyAll {
        return Err(Error::permission(format!(
            "{DENY_ALL_REFUSAL_PREFIX} of `{}` BEFORE spawn: this platform provides no \
             per-process network-isolation backend; never running the child unenforced",
            cfg.cmd
        )));
    }
    if cfg.filesystem_isolation.is_required() {
        return Err(Error::permission(format!(
            "{FILESYSTEM_REFUSAL_PREFIX} of `{}` BEFORE spawn: this platform provides no \
             per-process filesystem-confinement backend; never running the child while the \
             workspace guarantee claims enforcement",
            cfg.cmd
        )));
    }
    Ok(())
}

/// The agent scratch-HOME corpus (audit item 6): real spawns proving the
/// per-session scratch home, the explicit real-home grant, the refusal of
/// symlinked scratch paths and the confinement gate over credential roots.
#[cfg(test)]
#[path = "agent_home_tests.rs"]
mod agent_home_tests;
