//! Adversarial agent-HOME corpus (audit item 6).
//!
//! Real spawns through [`ProcessSupervisor::run_sync`] proving the two
//! halves of the fix:
//!
//! 1. `Session`/`Verification` children get a per-session SCRATCH home by
//!    default — `$HOME` in the child is the scratch directory under the
//!    configured base, never the daemon's real home — while non-agent
//!    owners and the explicit operator grant keep the environment exactly.
//! 2. The scratch path itself is hostile-input-safe: a pre-existing symlink
//!    at the session directory refuses the spawn typed BEFORE any process
//!    exists (never silently redirected onto a real credential store).
//!
//! The credential-store confinement gate (Required root covering `~/.ssh`)
//! is exercised in `filesystem_hostile` (Linux) and in the pure policy
//! tests of `faktor-sandbox` and `faktor-core`.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use faktor_core::command::AgentHomePolicy;
use faktor_core::command::EnvSpec;

use crate::{ProcessOwner, ProcessSupervisor, SpawnConfig};

fn supervisor(
    base: Option<PathBuf>,
    policy: AgentHomePolicy,
) -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    let supervisor = ProcessSupervisor::new(cas);
    if let Some(base) = base {
        supervisor.configure_agent_home(base, policy);
    }
    (dir, supervisor)
}

fn env(cfg: &mut SpawnConfig, entries: Vec<(&str, &str)>) {
    cfg.env = EnvSpec::Explicit(
        entries
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect(),
    );
}

/// `printf '%s' "$HOME"` through `/bin/sh` in the child.
fn print_home(owner: ProcessOwner, home_marker: &str) -> SpawnConfig {
    let mut cfg = SpawnConfig {
        cmd: "/bin/sh".into(),
        args: vec!["-c".into(), "printf '%s' \"$HOME\"".into()],
        cwd: std::env::temp_dir(),
        env: EnvSpec::Minimal,
        owner,
        capture: true,
        artifact_max: 64 * 1024,
        network_isolation: crate::NetworkIsolation::Inherit,
        filesystem_isolation: crate::FilesystemIsolation::Inherit,
    };
    env(&mut cfg, vec![("HOME", home_marker)]);
    cfg
}

fn run(sup: &Arc<ProcessSupervisor>, cfg: SpawnConfig) -> crate::SyncRunOutput {
    sup.run_sync(cfg, Duration::from_secs(10), 64 * 1024, 64 * 1024)
        .expect("spawn succeeds")
}

/// The default: a `Session` child never sees the marker HOME — it sees the
/// per-session scratch home, and a second spawn in the same session reuses
/// exactly that directory.
#[test]
fn session_children_get_the_per_session_scratch_home_by_default() {
    let base = tempfile::tempdir().unwrap();
    let (_d, sup) = supervisor(
        Some(base.path().join("agent-home")),
        AgentHomePolicy::Scratch,
    );
    let session = faktor_core::id::SessionId::new(7);
    let out = run(
        &sup,
        print_home(ProcessOwner::Session(session), "/real/operator/home"),
    );
    assert_eq!(out.exit_code, Some(0));
    let expected = base
        .path()
        .join("agent-home")
        .join("7")
        .join("home")
        .canonicalize()
        .expect("scratch home exists");
    assert_eq!(
        out.stdout_head.trim(),
        expected.to_string_lossy(),
        "the child HOME must be the scratch home"
    );
    assert!(
        expected.join(".config").is_dir() && expected.join(".cache").is_dir(),
        "the XDG directories are pre-created"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&expected).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the scratch home is owner-only");
    }
    // A second spawn (the verification class, same session) resolves the
    // SAME directory — one scratch home per session, not per spawn.
    let out = run(
        &sup,
        print_home(ProcessOwner::Verification(session), "/real/operator/home"),
    );
    assert_eq!(out.stdout_head.trim(), expected.to_string_lossy());
}

/// The explicit operator grant: the real HOME passes through untouched.
#[test]
fn operator_real_home_grant_passes_the_real_home() {
    let base = tempfile::tempdir().unwrap();
    let (_d, sup) = supervisor(
        Some(base.path().join("agent-home")),
        AgentHomePolicy::OperatorRealHome,
    );
    let out = run(
        &sup,
        print_home(
            ProcessOwner::Session(faktor_core::id::SessionId::new(1)),
            "/operator/real/home",
        ),
    );
    assert_eq!(out.stdout_head.trim(), "/operator/real/home");
    assert_eq!(
        sup.agent_home_state().unwrap().policy,
        AgentHomePolicy::OperatorRealHome
    );
}

/// A non-agent owner (daemon/workspace/browser children) keeps the
/// historical environment: the scratch contract is scoped to agent children.
#[test]
fn non_agent_owners_keep_the_inherited_environment() {
    let base = tempfile::tempdir().unwrap();
    let (_d, sup) = supervisor(
        Some(base.path().join("agent-home")),
        AgentHomePolicy::Scratch,
    );
    let out = run(&sup, print_home(ProcessOwner::Daemon, "/daemon/real/home"));
    assert_eq!(out.stdout_head.trim(), "/daemon/real/home");
}

/// A bare supervisor (no daemon wiring) never rewrites anything: the
/// historical byte-identical environment is preserved for library callers.
#[test]
fn unconfigured_supervisor_keeps_the_historical_environment() {
    let (_d, sup) = supervisor(None, AgentHomePolicy::Scratch);
    assert!(sup.agent_home_state().is_none());
    let out = run(
        &sup,
        print_home(
            ProcessOwner::Session(faktor_core::id::SessionId::new(1)),
            "/legacy/home",
        ),
    );
    assert_eq!(out.stdout_head.trim(), "/legacy/home");
}

/// Hostile input: a pre-existing SYMLINK at the session directory must
/// refuse the spawn typed BEFORE exec — the agent HOME can never be
/// redirected onto a real credential store by planting a link under the
/// data dir (the marker file the child would create proves no exec).
#[test]
fn symlinked_scratch_session_directory_refuses_the_spawn_before_exec() {
    let base = tempfile::tempdir().unwrap();
    let agent_home = base.path().join("agent-home");
    std::fs::create_dir_all(&agent_home).unwrap();
    let redirected = base.path().join("redirected");
    std::fs::create_dir_all(&redirected).unwrap();
    let session = faktor_core::id::SessionId::new(3);
    std::os::unix::fs::symlink(&redirected, agent_home.join("3")).unwrap();
    let marker = base.path().join("marker");
    let (_d, sup) = supervisor(Some(agent_home.clone()), AgentHomePolicy::Scratch);
    let mut cfg = SpawnConfig {
        cmd: "/bin/sh".into(),
        args: vec!["-c".into(), format!("touch {}", marker.display())],
        cwd: base.path().to_path_buf(),
        env: EnvSpec::Minimal,
        owner: ProcessOwner::Session(session),
        capture: true,
        artifact_max: 64 * 1024,
        network_isolation: crate::NetworkIsolation::Inherit,
        filesystem_isolation: crate::FilesystemIsolation::Inherit,
    };
    env(&mut cfg, vec![("HOME", "/real/home")]);
    let err = sup
        .run_sync(cfg, Duration::from_secs(10), 64 * 1024, 64 * 1024)
        .expect_err("symlinked scratch path must refuse the spawn");
    assert_eq!(err.kind, faktor_core::error::ErrorKind::Permission);
    assert!(
        err.message.contains("symlink"),
        "the refusal names the symlink: {err}"
    );
    assert!(
        !marker.exists(),
        "the refused child never ran (no marker file)"
    );
}

/// A regular FILE where a directory component is expected refuses typed too.
#[test]
fn file_blocking_the_scratch_path_refuses_the_spawn() {
    let base = tempfile::tempdir().unwrap();
    let agent_home = base.path().join("agent-home");
    std::fs::create_dir_all(&agent_home).unwrap();
    std::fs::write(agent_home.join("4"), b"not a directory").unwrap();
    let (_d, sup) = supervisor(Some(agent_home), AgentHomePolicy::Scratch);
    let err = sup
        .run_sync(
            print_home(
                ProcessOwner::Session(faktor_core::id::SessionId::new(4)),
                "/real/home",
            ),
            Duration::from_secs(10),
            64 * 1024,
            64 * 1024,
        )
        .expect_err("a file blocking the scratch path must refuse the spawn");
    assert_eq!(err.kind, faktor_core::error::ErrorKind::Permission);
}

/// The scratch rewrite must not hide an unset toolchain home: rustup needs
/// the daemon's RUSTUP_HOME (or real HOME) to resolve its default toolchain.
/// Existing explicit values always win; only real directories are added.
#[test]
fn scratch_home_preserves_approved_toolchain_homes() {
    use std::ffi::OsString;
    let real = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(real.path().join(".rustup")).unwrap();
    std::fs::create_dir_all(real.path().join(".cargo")).unwrap();
    let mut env: Vec<(OsString, OsString)> = vec![
        ("HOME".into(), real.path().as_os_str().to_os_string()),
        ("RUSTUP_HOME".into(), "/explicit/rustup".into()),
    ];
    super::preserve_toolchain_homes(&mut env);
    fn value(env: &[(std::ffi::OsString, std::ffi::OsString)], name: &str) -> Option<String> {
        env.iter()
            .find(|(key, _)| key == std::ffi::OsStr::new(name))
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }
    assert_eq!(
        value(&env, "RUSTUP_HOME").as_deref(),
        Some("/explicit/rustup")
    );
    assert_eq!(
        value(&env, "CARGO_HOME"),
        Some(real.path().join(".cargo").to_string_lossy().into_owned()),
        "an unset CARGO_HOME is anchored to the approved real toolchain home"
    );
    let scratch = real.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    faktor_core::command::apply_scratch_home(&mut env, &scratch);
    assert_eq!(
        value(&env, "HOME"),
        Some(scratch.to_string_lossy().into_owned()),
        "credentials still discover nothing under the scratch HOME"
    );
    assert_eq!(
        value(&env, "RUSTUP_HOME").as_deref(),
        Some("/explicit/rustup")
    );
    assert_eq!(
        value(&env, "CARGO_HOME"),
        Some(real.path().join(".cargo").to_string_lossy().into_owned())
    );
}
