//! macOS OS-level spawn confinement via the system sandbox engine.
//!
//! The backend generates a SBPL (Sandbox Profile Language) profile per spawn
//! and applies it with `/usr/bin/sandbox-exec -p <profile> -- <command>`.
//! `sandbox-exec` is part of the base OS; it applies the profile to itself
//! and then `execvp`s the requested command, so the supervised child keeps
//! the pid/process group the supervisor and the daemon-death guardian
//! already own.
//!
//! The generated profile is restrictive by construction:
//!
//! * `(deny default)` — only the rules below are ever reachable.
//! * network is denied for `NetworkIsolation::DenyAll` (loopback included:
//!   the macOS counterpart of the empty Linux network namespace), and left
//!   to the daemon namespace only when the policy explicitly inherited it.
//! * reads are granted for read-only system roots, the agent's real home
//!   (needed by toolchains) and the workspace/temp roots; every real-home
//!   credential store is then explicitly DENIED (a sandbox deny wins over an
//!   allow), including `~/.ssh`, `~/.aws`, gcloud, netrc, git credentials,
//!   cloud/kube/docker configs, cargo credentials and the keychain.
//! * writes are granted ONLY beneath the workspace roots (read-only mode
//!   grants only the explicit build/scratch roots), the process temp roots,
//!   the agent-home scratch HOME when the exact child environment carries
//!   one, and the null/tty devices. Everything else is write-denied by
//!   default, including `..` traversal and symlinks that resolve outside a
//!   granted root — the sandbox evaluates the resolved vnode path.
//!
//! `sandbox-exec` missing, not executable, or refusing the generated profile
//! is a typed [`SANDBOX_BACKEND_UNAVAILABLE`] permission refusal BEFORE the
//! target command runs. A `BestEffort` filesystem demand may honestly fall
//! back to application-policy-only execution when the backend is unusable;
//! a `Required` demand never does.
//!
//! Every profile is probe-verified once per process (with `/usr/bin/true`)
//! before the real spawn: a profile the engine rejects can therefore never
//! surface later as an ordinary child exit code. Probes are cached by
//! profile hash in a bounded set; failures are never cached.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};

use faktor_core::error::Error;

use crate::{FilesystemIsolation, NetworkIsolation};

/// Stable typed code for a missing or refusing macOS sandbox backend.
pub const SANDBOX_BACKEND_UNAVAILABLE: &str = "sandbox_backend_unavailable";

/// The system sandbox launcher. Never resolved through `PATH`: a hostile
/// `PATH` entry must not be able to substitute the enforcement binary.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The probe target used to verify a generated profile before a real spawn.
const PROBE_TARGET: &str = "/usr/bin/true";

/// Read-only system roots every confined child may inspect (binaries,
/// libraries, configuration, devices, temp and dynamic-loader metadata).
/// All are read/execute grants only; writes are separately bounded.
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/System",
    "/Library",
    "/Applications",
    "/opt",
    "/usr/local",
    "/etc",
    "/var",
    "/private/etc",
    "/private/var/db",
    "/private/var/select",
    "/private/var/run",
    "/private/var/folders",
    "/private/tmp",
    "/tmp",
    "/dev",
    "/cores",
    "/Users/Shared",
];

/// Additional secret-bearing home locations denied explicitly. The core
/// crate list (`CREDENTIAL_HOME_SUBPATHS`) is always included; these cover
/// token caches and OS key material that also live under `$HOME`.
const EXTRA_SECRET_SUBPATHS: &[&str] = &[
    ".gnupg",
    ".kube",
    ".docker/config.json",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/gh",
    ".config/op",
    ".config/1password",
    ".password-store",
    "Library/Keychains",
];

/// A fully built sandbox-exec invocation: the launcher, its complete argv
/// (including the original command and args), and the profile evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub profile: String,
}

/// The confinement demand one spawn carries (a borrow-only view so the
/// supervisor and the PTY authority share the exact same generator).
pub(crate) struct Demand<'a> {
    pub network: NetworkIsolation,
    pub filesystem: &'a FilesystemIsolation,
    pub cwd: &'a Path,
    pub cmd: &'a str,
    pub args: &'a [String],
    /// The exact child environment: its HOME/XDG/TMPDIR anchors are the
    /// SANCTIONED child roots. A scratch HOME (agent-home policy) is granted
    /// read/write; the daemon's real HOME stays read-only and its credentials
    /// stay denied.
    pub env: &'a crate::EnvSpec,
}

impl Demand<'_> {
    fn requires_confinement(&self) -> bool {
        self.network == NetworkIsolation::DenyAll
            || !matches!(self.filesystem, FilesystemIsolation::Inherit)
    }

    fn is_required(&self) -> bool {
        self.network == NetworkIsolation::DenyAll || self.filesystem.is_required()
    }
}

/// Build the sandbox-exec plan for one demand, or `None` when the demand
/// installs no OS confinement. A `Required` demand whose backend is missing
/// or refuses the profile returns the typed
/// [`SANDBOX_BACKEND_UNAVAILABLE`] permission error before any child exists.
/// A `BestEffort` filesystem demand falls back honestly to
/// application-policy-only execution (logged, never claimed as confined).
pub(crate) fn plan_for(demand: &Demand<'_>) -> Result<Option<SandboxPlan>, Error> {
    plan_with_backend(demand, backend_path(), true)
}

/// Build the plan for one supervisor spawn from its exact configuration.
pub(crate) fn plan_for_spawn(cfg: &crate::SpawnConfig) -> Result<Option<SandboxPlan>, Error> {
    let demand = Demand {
        network: cfg.network_isolation,
        filesystem: &cfg.filesystem_isolation,
        cwd: &cfg.cwd,
        cmd: &cfg.cmd,
        args: &cfg.args,
        env: &cfg.env,
    };
    plan_for(&demand)
}

/// Gate/probe one supervisor spawn BEFORE any process exists. Discards the
/// plan; the command builder regenerates the identical one (the probe result
/// is cached).
pub(crate) fn ensure_usable(cfg: &crate::SpawnConfig) -> Result<(), Error> {
    plan_for_spawn(cfg).map(|_| ())
}

/// Build the plan for the interactive PTY authority (the same generator the
/// supervisor uses). The authority rewrites its `PtyConfig` command/args to
/// the returned launcher when a plan exists.
pub fn plan_for_pty(
    network: NetworkIsolation,
    filesystem: &FilesystemIsolation,
    cwd: &Path,
    cmd: &str,
    args: &[String],
    env: &crate::EnvSpec,
) -> Result<Option<SandboxPlan>, Error> {
    let demand = Demand {
        network,
        filesystem,
        cwd,
        cmd,
        args,
        env,
    };
    plan_for(&demand)
}

/// Testable core of [`plan_for`]: the backend path and whether probes run
/// are explicit seams. Never called with a caller-supplied path in
/// production (the fixed system path is the only backend).
fn plan_with_backend(
    demand: &Demand<'_>,
    backend: Option<PathBuf>,
    preflight: bool,
) -> Result<Option<SandboxPlan>, Error> {
    if !demand.requires_confinement() {
        return Ok(None);
    }
    let Some(profile) = profile_for(demand)? else {
        return Ok(None);
    };
    tracing::debug!(
        target: "faktor_terminal::sandbox",
        cmd = demand.cmd,
        cwd = %demand.cwd.display(),
        profile_bytes = profile.len(),
        "macOS sandbox profile generated for this spawn"
    );
    let required = demand.is_required();
    let Some(program) = backend.filter(|path| executable(path)) else {
        if required {
            return Err(unavailable(format!(
                "the macOS sandbox launcher {SANDBOX_EXEC} is missing or not executable; \
                 refusing `{}` BEFORE exec under {}",
                demand.cmd,
                demanded(demand)
            )));
        }
        tracing::warn!(
            cmd = demand.cmd,
            "macOS sandbox backend unavailable; this BestEffort filesystem demand runs \
             application-policy-only (never claimed as an OS jail)"
        );
        return Ok(None);
    };
    if preflight {
        if let Err(detail) = probe_profile(&program, &profile) {
            if required {
                return Err(unavailable(format!(
                    "the macOS sandbox launcher refused the generated profile for `{}` BEFORE \
                     exec: {detail}; never running it unconfined",
                    demand.cmd
                )));
            }
            tracing::warn!(
                cmd = demand.cmd,
                detail = %detail,
                "macOS sandbox profile refused; this BestEffort filesystem demand runs \
                 application-policy-only (never claimed as an OS jail)"
            );
            return Ok(None);
        }
    }
    let mut args = vec![
        "-p".to_string(),
        profile.clone(),
        "--".to_string(),
        demand.cmd.to_string(),
    ];
    args.extend(demand.args.iter().cloned());
    Ok(Some(SandboxPlan {
        program,
        args,
        profile,
    }))
}

/// The typed refusal constructor: `ErrorKind::Permission` plus the stable
/// machine code every caller and test can match.
pub(crate) fn unavailable(detail: impl Into<String>) -> Error {
    Error::permission(format!("{SANDBOX_BACKEND_UNAVAILABLE}: {}", detail.into()))
}

fn backend_path() -> Option<PathBuf> {
    let path = PathBuf::from(SANDBOX_EXEC);
    executable(&path).then_some(path)
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn demanded(demand: &Demand<'_>) -> String {
    let mut parts = Vec::new();
    if demand.network == NetworkIsolation::DenyAll {
        parts.push("NetworkIsolation::DenyAll".to_string());
    }
    if !matches!(demand.filesystem, FilesystemIsolation::Inherit) {
        parts.push(format!(
            "FilesystemIsolation::{}",
            demand.filesystem.as_tag()
        ));
    }
    if parts.is_empty() {
        "the requested confinement".to_string()
    } else {
        parts.join(" + ")
    }
}

// ------------------------------------------------------------------ profile

/// Generate the SBPL profile for one demand, or `None` when there is no
/// confinement to install. Pure except for reading the daemon's own HOME
/// (the credential deny list) and canonicalizing existing roots.
fn profile_for(demand: &Demand<'_>) -> Result<Option<String>, Error> {
    if !demand.requires_confinement() {
        return Ok(None);
    }
    let credentials = credential_paths();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let temp = std::env::temp_dir();

    // The sandbox matches a `subpath` filter against the path AS THE CHILD
    // SPELLS IT (a `/tmp/...` write is not covered by a `/private/tmp/...`
    // grant, even though both name the same vnode). Grant and deny BOTH the
    // lexical and canonical form of every root so symlinked platform paths
    // (`/tmp`, `/var`, `$TMPDIR`) are confined exactly as intended instead
    // of failing closed for the wrong path spelling.
    // The read-only workspace roots and their explicit writable exceptions
    // are kept apart from the environment-derived write roots: after the
    // broad write grants below they are re-asserted as a final deny (then
    // the explicit exceptions re-allowed) so a workspace that happens to
    // live inside a granted temp/home root can never become writable.
    let mut readonly_roots: Vec<PathBuf> = Vec::new();
    let mut readonly_writable: Vec<PathBuf> = Vec::new();
    let (read_roots, write_roots): (Vec<PathBuf>, Vec<PathBuf>) = match demand.filesystem {
        FilesystemIsolation::Inherit => {
            // Network-only confinement: the command's own cwd is its data
            // root (the policy decided that cwd); no other write is granted.
            let cwd = path_aliases(demand.cwd);
            (cwd.clone(), cwd)
        }
        FilesystemIsolation::Workspace { roots, .. } => {
            let roots = safe_roots(roots, &credentials);
            (roots.clone(), roots)
        }
        FilesystemIsolation::WorkspaceReadOnly {
            roots,
            writable_roots,
            ..
        } => {
            readonly_roots = safe_roots(roots, &credentials);
            readonly_writable = safe_roots(writable_roots, &credentials);
            (readonly_roots.clone(), readonly_writable.clone())
        }
    };

    let (env_read, env_write) = sanctioned_env_roots(demand.env, &credentials);
    let mut read_roots = read_roots;
    read_roots.extend(env_read);
    let mut write_roots = write_roots;
    write_roots.extend(env_write);

    let mut profile = String::with_capacity(1024);
    profile.push_str("(version 1)\n(deny default)\n");
    // Process lifecycle only; no blanket `process*` (which would grant
    // signalling arbitrary processes).
    profile.push_str("(allow process-exec)\n(allow process-fork)\n");
    profile.push_str("(allow process-info*)\n(allow signal (target same-sandbox))\n");
    profile.push_str("(allow sysctl-read)\n");

    // Reads: root metadata (required by libc/launchd even before exec),
    // read-only system roots, the real home (toolchains), the workspace
    // roots and the process temp dir.
    // `realpath(3)` (and therefore Rust's `fs::canonicalize`) stats every
    // ancestor of an ABSOLUTE path. Grants for `/private/tmp` or
    // `/private/var` do not authorize the `/private` component itself, so a
    // confined Cargo build could not canonicalize its own absolute workspace
    // root and failed every dep-info translation with EPERM. The literal
    // grant is metadata/search for `/private` only, never a read of its
    // contents.
    profile.push_str("(allow file-read* (literal \"/\") (literal \"/private\")");
    for root in SYSTEM_READ_ROOTS.iter().map(PathBuf::from) {
        for path in path_aliases(&root) {
            push_subpath(&mut profile, &path)?;
        }
    }
    if let Some(home) = &home {
        for path in toolchain_read_paths(home) {
            for alias in path_aliases(&path) {
                push_subpath(&mut profile, &alias)?;
            }
        }
        // The rest of the home is readable so tool discovery/caches work;
        // the explicit credential denies below remove the secret stores.
        for alias in path_aliases(home) {
            push_subpath(&mut profile, &alias)?;
        }
    }
    for path in read_roots.iter() {
        push_subpath(&mut profile, path)?;
    }
    for path in path_aliases(&temp) {
        push_subpath(&mut profile, &path)?;
    }
    profile.push_str(")\n");

    // Credential stores are denied for BOTH read and write, after every
    // allow rule, so a workspace or home grant can never cover them.
    for credential in &credentials {
        for alias in path_aliases(credential) {
            profile.push_str("(deny file-read*");
            push_subpath(&mut profile, &alias)?;
            profile.push_str(")\n");
            profile.push_str("(deny file-write*");
            push_subpath(&mut profile, &alias)?;
            profile.push_str(")\n");
        }
    }

    // Network: DenyAll denies every family/direction (loopback included).
    // Inherit/BrokerOnly never reaches this backend for network confinement;
    // a filesystem-only profile keeps the daemon's inherited network.
    if demand.network == NetworkIsolation::DenyAll {
        profile.push_str("(deny network*)\n");
    } else {
        profile.push_str("(allow network*)\n");
    }

    // Writes: the null/tty devices plus the workspace/scratch roots and the
    // exact temp roots the child environment sanctions. There is no global
    // `/tmp` grant: a workspace symlink or `..` path into the shared temp
    // directory is just another outside write and stays denied.
    profile.push_str("(allow file-write*");
    for device in [
        "/dev/null",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/tty",
        "/dev/dtracehelper",
    ] {
        push_literal(&mut profile, Path::new(device))?;
    }
    let writable = write_roots;
    for path in &writable {
        push_subpath(&mut profile, path)?;
    }
    profile.push_str(")\n");

    // Read-only workspaces are physically unmodifiable even when they live
    // beneath a root that is otherwise writable (the process temp dir, a
    // scratch home, or an inherited cwd): deny after every general grant,
    // then re-allow only the caller's explicit build/scratch roots.
    if !readonly_roots.is_empty() {
        profile.push_str("(deny file-write*");
        for path in &readonly_roots {
            for alias in path_aliases(path) {
                push_subpath(&mut profile, &alias)?;
            }
        }
        profile.push_str(")\n");
        profile.push_str("(allow file-write*");
        for path in &readonly_writable {
            for alias in path_aliases(path) {
                // Dirs use a subpath grant; a FILE exception (e.g. the
                // derived Cargo.lock, which may not exist yet) must also be
                // a literal: SBPL `subpath` matches directory prefixes, not
                // the file itself.
                push_subpath(&mut profile, &alias)?;
                push_literal(&mut profile, &alias)?;
            }
        }
        profile.push_str(")\n");
    }

    Ok(Some(profile))
}

/// Toolchain/cache locations inside a real home that must remain readable
/// even if the home itself is otherwise narrowed in a future revision. They
/// are ordinary read grants; the credential deny list still wins.
fn toolchain_read_paths(home: &Path) -> Vec<PathBuf> {
    [
        ".cargo",
        ".rustup",
        ".nvm",
        ".asdf",
        ".local",
        ".cache",
        ".config",
        "go",
        "Library/Caches",
    ]
    .iter()
    .map(|sub| home.join(sub))
    .collect()
}

/// Both spellings a path may be addressed by: the lexical path and its
/// symlink-resolved canonical form. Sandbox `subpath` filters must carry both
/// (see [`profile_for`]); duplicates are removed.
fn path_aliases(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if let Ok(canonical) = std::fs::canonicalize(path) {
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    out
}

/// Roots that do not overlap a credential store. Required demands whose root
/// covers a credential are refused earlier by the supervisor's isolation
/// gate; for BestEffort the overlapping root is simply not granted (the
/// credential deny stays in force).
fn safe_roots(roots: &[PathBuf], credentials: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        let aliases = path_aliases(root);
        if aliases.iter().any(|candidate| {
            credentials
                .iter()
                .any(|credential| faktor_core::command::paths_overlap(candidate, credential))
        }) {
            continue;
        }
        for alias in aliases {
            if !out.contains(&alias) {
                out.push(alias);
            }
        }
    }
    out
}

fn credential_paths() -> Vec<PathBuf> {
    let mut paths = crate::agent_home::real_home_credential_paths();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for sub in EXTRA_SECRET_SUBPATHS {
            let path = home.join(sub);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// Roots the exact child environment sanctions. A HOME different from the
/// daemon's real HOME is the agent-home scratch home (read/write); the real
/// HOME is never made writable here. TMPDIR/TEMP/TMP are the process temp
/// roots; CARGO_HOME/RUSTUP_HOME are read-only toolchain inputs. Absolute
/// values only: a relative env path never names a kernel vnode.
fn sanctioned_env_roots(
    env: &crate::EnvSpec,
    credentials: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let resolved = env.resolve();
    let value = |names: &[&str]| {
        resolved.iter().find_map(|(key, value)| {
            let key = key.to_string_lossy();
            if names.iter().any(|name| key.eq_ignore_ascii_case(name)) {
                let path = PathBuf::from(value);
                path.is_absolute().then_some(path)
            } else {
                None
            }
        })
    };
    let daemon_home = std::env::var_os("HOME").map(PathBuf::from);
    let child_home = value(&["HOME", "USERPROFILE"]);
    let mut read = Vec::new();
    let mut write = Vec::new();
    if let Some(home) = &child_home {
        read.extend(path_aliases(home));
    }
    let scratch_home = child_home.as_ref() != daemon_home.as_ref();
    if scratch_home {
        if let Some(home) = &child_home {
            // The agent scratch home is a sanctioned write root, but never a
            // credential store (a hostile caller could point HOME there).
            write.extend(safe_roots(std::slice::from_ref(home), credentials));
        }
    }
    for xdg in [
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
    ] {
        if let Some(path) = value(&[xdg]) {
            read.extend(path_aliases(&path));
            if scratch_home {
                write.extend(safe_roots(std::slice::from_ref(&path), credentials));
            }
        }
    }
    for name in ["TMPDIR", "TEMP", "TMP"] {
        if let Some(path) = value(&[name]) {
            read.extend(path_aliases(&path));
            write.extend(path_aliases(&path));
        }
    }
    for name in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(path) = value(&[name]) {
            read.extend(path_aliases(&path));
        }
    }
    read.sort();
    read.dedup();
    write.sort();
    write.dedup();
    (read, write)
}

/// Append one `(subpath "...")` filter to the currently open rule.
fn push_subpath(profile: &mut String, path: &Path) -> Result<(), Error> {
    profile.push_str(" (subpath ");
    profile.push_str(&sbpl_path(path)?);
    profile.push(')');
    Ok(())
}

/// Append one `(literal "...")` filter to the currently open rule.
fn push_literal(profile: &mut String, path: &Path) -> Result<(), Error> {
    profile.push_str(" (literal ");
    profile.push_str(&sbpl_path(path)?);
    profile.push(')');
    Ok(())
}

/// Quote one path as a SBPL string. Control characters and non-UTF-8 paths
/// are refused: a lossy encoding could silently name a different vnode, and
/// a newline could terminate the profile early.
fn sbpl_path(path: &Path) -> Result<String, Error> {
    let text = path.to_str().ok_or_else(|| {
        unavailable(format!(
            "sandbox profile path {} is not valid UTF-8; refusing to guess a vnode",
            path.display()
        ))
    })?;
    if text.chars().any(|ch| ch.is_control()) {
        return Err(unavailable(format!(
            "sandbox profile path {} contains a control character; refusing to embed it",
            path.display()
        )));
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out.push('"');
    Ok(out)
}

// ------------------------------------------------------------------ probes

/// Bounded per-process profile probe cache. Only SUCCESSFUL probes are
/// remembered: a transient refusal must never permanently disable the
/// backend.
const MAX_CACHED_PROFILES: usize = 64;

fn probed_profiles() -> &'static Mutex<Vec<(u64, String)>> {
    static CACHE: OnceLock<Mutex<Vec<(u64, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Run `/usr/bin/sandbox-exec -p <profile> -- /usr/bin/true` and require a
/// clean exit. The byte bound keeps a hostile engine error from growing
/// memory; the profile is generated from bounded fields already.
fn probe_profile(program: &Path, profile: &str) -> Result<(), String> {
    let mut hasher = DefaultHasher::new();
    program.hash(&mut hasher);
    profile.hash(&mut hasher);
    let key = hasher.finish();
    if probed_profiles()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .any(|(cached_key, cached_profile)| *cached_key == key && cached_profile == profile)
    {
        return Ok(());
    }
    let output = std::process::Command::new(program)
        .args(["-p", profile, "--", PROBE_TARGET])
        .current_dir("/")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("the probe could not be executed: {e}"))?;
    if output.status.success() {
        let mut cache = probed_profiles()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= MAX_CACHED_PROFILES {
            cache.remove(0);
        }
        if !cache
            .iter()
            .any(|(cached_key, cached_profile)| *cached_key == key && cached_profile == profile)
        {
            cache.push((key, profile.to_string()));
        }
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let bounded: String = stderr.chars().take(512).collect();
    Err(format!(
        "profile probe exited {:?}: {}",
        output.status.code(),
        bounded.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demand<'a>(
        network: NetworkIsolation,
        filesystem: &'a FilesystemIsolation,
        cwd: &'a Path,
        cmd: &'a str,
        args: &'a [String],
    ) -> Demand<'a> {
        Demand {
            network,
            filesystem,
            cwd,
            cmd,
            args,
            env: &ENV,
        }
    }

    static ENV: crate::EnvSpec = crate::EnvSpec::Minimal;

    fn workspace(roots: Vec<PathBuf>, required: bool) -> FilesystemIsolation {
        FilesystemIsolation::Workspace { roots, required }
    }

    #[test]
    fn no_demand_builds_no_plan_and_runs_no_backend() {
        let fs = FilesystemIsolation::Inherit;
        let args = Vec::new();
        let d = demand(
            NetworkIsolation::Inherit,
            &fs,
            Path::new("/tmp"),
            "/bin/true",
            &args,
        );
        assert!(plan_with_backend(&d, None, false).unwrap().is_none());
    }

    #[test]
    fn a_required_demand_with_a_missing_backend_is_typed_and_never_runs() {
        let fs = FilesystemIsolation::Inherit;
        let args = Vec::new();
        let d = demand(
            NetworkIsolation::DenyAll,
            &fs,
            Path::new("/tmp"),
            "/bin/true",
            &args,
        );
        let err = plan_with_backend(&d, None, false).unwrap_err();
        assert_eq!(err.kind, faktor_core::error::ErrorKind::Permission);
        assert!(err.message.contains(SANDBOX_BACKEND_UNAVAILABLE), "{err:?}");
        let fs = workspace(vec![PathBuf::from("/tmp/faktor-macos-ws")], true);
        let d = demand(
            NetworkIsolation::Inherit,
            &fs,
            Path::new("/tmp"),
            "/bin/true",
            &args,
        );
        let err = plan_with_backend(&d, None, false).unwrap_err();
        assert!(err.message.contains(SANDBOX_BACKEND_UNAVAILABLE), "{err:?}");
    }

    #[test]
    fn a_best_effort_demand_without_a_backend_falls_back_honestly() {
        let fs = workspace(vec![PathBuf::from("/tmp/faktor-macos-ws")], false);
        let args = Vec::new();
        let d = demand(
            NetworkIsolation::Inherit,
            &fs,
            Path::new("/tmp"),
            "/bin/true",
            &args,
        );
        assert!(plan_with_backend(&d, None, false).unwrap().is_none());
    }

    #[test]
    fn the_generated_profile_denies_network_credentials_and_outside_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let fs = workspace(vec![root.clone()], true);
        let args = vec!["-c".to_string(), "true".to_string()];
        let d = demand(NetworkIsolation::DenyAll, &fs, &root, "/bin/sh", &args);
        let profile = profile_for(&d).unwrap().expect("profile");
        assert!(profile.contains("(deny default)"), "{profile}");
        assert!(profile.contains("(deny network*)"), "{profile}");
        assert!(
            profile.contains("(deny file-read*"),
            "the credential deny list must be explicit: {profile}"
        );
        assert!(
            profile.contains("(deny file-write*"),
            "credential write denial must be explicit: {profile}"
        );
        let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        assert!(
            profile.contains(&sbpl_path(&canonical).unwrap()),
            "the workspace root is granted: {profile}"
        );
        assert!(
            profile.contains("(allow file-write*"),
            "an explicit write allowlist exists: {profile}"
        );
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for sub in [".ssh", ".aws", "Library/Keychains"] {
                let path = sbpl_path(&home.join(sub)).unwrap();
                assert!(
                    profile.contains(&format!("(deny file-read* (subpath {path}))")),
                    "the credential store {sub} must be explicitly read-denied: {profile}"
                );
                assert!(
                    profile.contains(&format!("(deny file-write* (subpath {path}))")),
                    "the credential store {sub} must be explicitly write-denied: {profile}"
                );
            }
        }
    }

    #[test]
    fn profile_paths_refuse_quotes_control_characters_and_non_utf8() {
        let quoted = sbpl_path(Path::new("/tmp/a\"b\\c")).unwrap();
        assert_eq!(quoted, "\"/tmp/a\\\"b\\\\c\"");
        let control = sbpl_path(Path::new("/tmp/a\nb")).unwrap_err();
        assert!(control.message.contains(SANDBOX_BACKEND_UNAVAILABLE));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let invalid = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff"));
            assert!(sbpl_path(invalid).is_err());
        }
    }

    #[test]
    fn a_refusing_backend_is_typed_before_exec_and_best_effort_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(vec![dir.path().to_path_buf()], true);
        let args = Vec::new();
        let d = demand(
            NetworkIsolation::DenyAll,
            &ws,
            dir.path(),
            "/bin/true",
            &args,
        );
        // `/usr/bin/false` exists and is executable, but refuses the probe:
        // the required spawn must be typed as a backend refusal, not a
        // child that later exits nonzero.
        let err = plan_with_backend(&d, Some(PathBuf::from("/usr/bin/false")), true).unwrap_err();
        assert_eq!(err.kind, faktor_core::error::ErrorKind::Permission);
        assert!(err.message.contains(SANDBOX_BACKEND_UNAVAILABLE), "{err:?}");

        let best_effort = workspace(vec![dir.path().to_path_buf()], false);
        let d = demand(
            NetworkIsolation::Inherit,
            &best_effort,
            dir.path(),
            "/bin/true",
            &args,
        );
        assert!(
            plan_with_backend(&d, Some(PathBuf::from("/usr/bin/false")), true)
                .unwrap()
                .is_none(),
            "BestEffort must fall back honestly when the backend refuses"
        );
    }

    #[test]
    fn a_workspace_profile_allows_writing_build_outputs_and_denies_outside() {
        // Regression: the live os_isolated shell could not write rustc
        // dep-info under `<workspace>/target`. The profile must grant the
        // workspace subtree (build outputs live there) while still denying
        // an unrelated sibling.
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("target/debug/deps")).unwrap();
        std::fs::write(dir.path().join("outside.txt"), "secret").unwrap();
        let fs = workspace(vec![ws.clone()], true);
        let script = format!(
            "printf x > '{}/target/debug/deps/probe.d' && cat '{}/target/debug/deps/probe.d' && printf pwn > '{}/outside.txt'",
            ws.display(),
            ws.display(),
            dir.path().display()
        );
        let args = vec!["-c".to_string(), script];
        let d = demand(NetworkIsolation::Inherit, &fs, &ws, "/bin/sh", &args);
        let plan = plan_with_backend(&d, Some(PathBuf::from(SANDBOX_EXEC)), false)
            .unwrap()
            .expect("plan");
        let output = std::process::Command::new(&plan.program)
            .args(&plan.args)
            .envs(crate::EnvSpec::Minimal.resolve())
            .output()
            .unwrap();
        assert_eq!(
            std::fs::read(ws.join("target/debug/deps/probe.d")).unwrap(),
            b"x",
            "workspace build output write must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_ne!(
            output.status.code(),
            Some(0),
            "the outside write must fail under the workspace profile"
        );
        assert_eq!(
            std::fs::read(dir.path().join("outside.txt")).unwrap(),
            b"secret",
            "the outside file is untouched"
        );
    }

    #[test]
    fn a_workspace_profile_allows_build_writes_with_a_scratch_home_and_cargo_home() {
        // The live shape: a 0700 shadow under a deep data tree, a scratch
        // HOME outside the workspace and the daemon's real CARGO_HOME
        // (read-only). Build outputs still live in the workspace.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("data/shadows/1");
        let ws = parent.join("sh-deadbeef");
        std::fs::create_dir_all(ws.join("target/debug/deps")).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let scratch_home = dir.path().join("agent-home");
        std::fs::create_dir_all(&scratch_home).unwrap();
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cargo"));
        let env = crate::EnvSpec::Explicit(vec![
            ("HOME".into(), scratch_home.clone().into_os_string()),
            ("CARGO_HOME".into(), cargo_home.into_os_string()),
            ("PATH".into(), std::env::var_os("PATH").unwrap()),
            ("TMPDIR".into(), scratch_home.join("tmp").into_os_string()),
        ]);
        let fs = workspace(vec![ws.clone()], true);
        let script = format!(
            "mkdir -p '{}/target/debug/deps' && printf x > '{}/target/debug/deps/probe.d' && cat '{}/target/debug/deps/probe.d'",
            ws.display(),
            ws.display(),
            ws.display()
        );
        let args = vec!["-c".to_string(), script];
        let d = Demand {
            network: NetworkIsolation::Inherit,
            filesystem: &fs,
            cwd: &ws,
            cmd: "/bin/sh",
            args: &args,
            env: &env,
        };
        let plan = plan_with_backend(&d, Some(PathBuf::from(SANDBOX_EXEC)), false)
            .unwrap()
            .expect("plan");
        let output = std::process::Command::new(&plan.program)
            .args(&plan.args)
            .envs(env.resolve())
            .output()
            .unwrap();
        assert_eq!(
            std::fs::read(ws.join("target/debug/deps/probe.d")).unwrap(),
            b"x",
            "workspace build output write must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn absolute_paths_under_private_canonicalize_inside_the_profile() {
        // Regression: Cargo's dep-info translation canonicalizes absolute
        // workspace paths. Without the `/private` ancestor metadata grant,
        // realpath(3) failed with EPERM even though the workspace subtree
        // itself was readable and writable.
        let dir = tempfile::Builder::new()
            .prefix("faktor-realpath-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let fs = workspace(vec![ws.clone()], true);
        let args = vec![ws.to_string_lossy().into_owned()];
        let d = demand(NetworkIsolation::Inherit, &fs, &ws, "/bin/realpath", &args);
        let plan = plan_with_backend(&d, Some(PathBuf::from(SANDBOX_EXEC)), false)
            .unwrap()
            .expect("plan");
        let output = std::process::Command::new(&plan.program)
            .args(&plan.args)
            .envs(crate::EnvSpec::Minimal.resolve())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "realpath of an absolute workspace must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(ws.to_string_lossy().as_ref()),
            "the canonical path is returned"
        );
    }

    #[test]
    fn the_plan_wraps_the_original_command_and_args_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let fs = workspace(vec![dir.path().to_path_buf()], true);
        let args = vec!["-c".to_string(), "echo hi".to_string()];
        let d = demand(NetworkIsolation::DenyAll, &fs, dir.path(), "/bin/sh", &args);
        let probe_only = plan_with_backend(&d, Some(PathBuf::from("/usr/bin/sandbox-exec")), false)
            .unwrap()
            .expect("plan");
        assert_eq!(probe_only.program, PathBuf::from(SANDBOX_EXEC));
        assert_eq!(
            &probe_only.args[..4],
            &[
                "-p".to_string(),
                probe_only.profile.clone(),
                "--".to_string(),
                "/bin/sh".to_string()
            ]
        );
        assert_eq!(&probe_only.args[4..], &args[..]);
    }
}
