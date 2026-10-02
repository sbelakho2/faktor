//! Terminal tests (mechanically split from `lib`): unix process-group,
//! signal and ring-buffer certification.

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
            .any(|(k, v)| k == "HOME" && v == std::ffi::OsStr::new("/tmp/faktor-ci-browser-home")),
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
    let mut cfg = sh(
        "test -z \"$OPENAI_API_KEY\" && test -z \"$TEST_PRIVATE_SECRET\" && echo explicit-exact",
    );
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
    let mut cfg = sh(
        "yes 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' | head -n 500000",
    );
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
    let mut cfg = sh("i=0; while [ $i -lt 200000 ]; do echo beginning-check-$i; i=$((i+1)); done");
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

/// A typed DenyAll refusal is a skippable environment outcome ONLY when this
/// host cannot run the isolation setup at all. On a host that CAN
/// `unshare(CLONE_NEWNET)` and run the privilege drop (the privileged lane),
/// a refusal is a regression and must fail the test instead of hiding behind
/// an adaptive branch.
#[cfg(target_os = "linux")]
fn assert_isolation_refusal_is_environment_gated(err: &Error) {
    assert!(
        !super::sandbox::isolation_must_succeed_for_tests(),
        "this host supports unshare(CLONE_NEWNET) and the privilege drop, so an \
         isolated spawn must succeed; it was refused instead: {err:?}"
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
    // Sandbox credential posture (Linux /proc): the parent asserts these
    // only on the isolation Ok branches; a pass-through child never reaches
    // this code under a different confinement.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for field in ["CapInh", "CapPrm", "CapEff", "CapBnd", "NoNewPrivs"] {
            let prefix = format!("{field}:");
            if let Some(line) = status.lines().find(|l| l.starts_with(&prefix)) {
                lines.push(format!(
                    "{}={}",
                    field.to_ascii_lowercase(),
                    line[prefix.len()..].trim()
                ));
            }
        }
    }
    let escaped = !super::sandbox::setns_escape_denied_for_tests();
    lines.push(format!(
        "setns_escape={}",
        if escaped { "succeeded" } else { "denied" }
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
static DENY_ALL_SPAWN_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

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
            // Namespace membership is not confinement: the child must hold
            // no capabilities at all, must be under no-new-privs, and must
            // NOT be able to setns back into the parent namespace.
            assert!(
                report.contains("capprm=0000000000000000"),
                "the isolated child must have no permitted capabilities: {report}"
            );
            assert!(
                report.contains("capeff=0000000000000000"),
                "the isolated child must have no effective capabilities: {report}"
            );
            assert!(
                report.contains("capbnd=0000000000000000"),
                "the isolated child's capability bounding set must be empty: {report}"
            );
            assert!(
                report.contains("nonewprivs=1"),
                "the isolated child must run under PR_SET_NO_NEW_PRIVS: {report}"
            );
            assert!(
                report.contains("setns_escape=denied"),
                "the isolated child must not be able to setns back to the host \
                 network namespace: {report}"
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
            assert_isolation_refusal_is_environment_gated(&err);
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

/// DenyAll must not break ordinary command execution: when the host permits
/// the namespace setup, a normal shell command still runs under the
/// capability-dropped child and its output is captured intact. A host
/// that refuses the setup takes the typed-refusal skip (never a silent
/// pass, and never an unenforced run).
#[cfg(target_os = "linux")]
#[test]
fn deny_all_spawn_runs_and_captures_output_when_permitted() {
    let _serial = deny_all_spawn_lock();
    let (_d, sup) = supervisor();
    let mut cfg = sh("echo pass-through-ok; id -u");
    cfg.network_isolation = NetworkIsolation::DenyAll;
    match sup.run_sync(cfg, Duration::from_secs(10), 4096, 4096) {
        Ok(out) => {
            assert_eq!(out.exit_code, Some(0), "{:?}", out.stdout_head);
            assert!(
                out.stdout_head.contains("pass-through-ok"),
                "a normal command must still run under DenyAll and be captured: {:?}",
                out.stdout_head
            );
        }
        Err(err) => {
            assert_isolation_refusal(&err);
            assert_isolation_refusal_is_environment_gated(&err);
            eprintln!(
                "SKIP (typed, unprivileged host): DenyAll refused fail-closed and no \
                 command ran: {err}"
            );
        }
    }
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
        assert!(
            !super::sandbox::isolation_must_succeed_for_tests(),
            "this host supports unshare(CLONE_NEWNET) and the privilege drop, so a \
             BrokerOnly spawn must succeed; it was refused instead: {err:?}"
        );
        eprintln!(
            "SKIP (typed, unprivileged host): BrokerOnly namespace backend refused \
                 fail-closed and no child ran: {err}"
        );
    }
    unprivileged
}

/// The REAL privilege drop every isolated child runs must succeed (and leave
/// no capability behind) on a host that can run it: credentials first (while
/// CAP_SETGID/CAP_SETUID are effective), capabilities last. The original
/// ordering cleared the capability sets before
/// `setgroups`/`setresgid`/`setresuid`, so EVERY isolated spawn on the
/// privileged lane refused EPERM. On hosts that cannot run the drop the
/// probe refuses typed and this test skips explicitly.
#[cfg(target_os = "linux")]
#[test]
fn privilege_drop_succeeds_with_empty_caps_on_a_capable_host() {
    let _serial = deny_all_spawn_lock();
    match super::sandbox::probe_privilege_drop_for_tests() {
        super::sandbox::DropProbe::Succeeded {
            no_new_privs,
            caps_empty,
        } => {
            assert!(no_new_privs, "PR_SET_NO_NEW_PRIVS must hold after the drop");
            assert!(caps_empty, "the drop must leave no capability behind");
        }
        super::sandbox::DropProbe::Refused(errno) => {
            assert!(
                !super::sandbox::privilege_drop_must_succeed_for_tests(),
                "this host runs the drop (root, setgroups permitted), so it must not \
                 refuse: errno {errno}"
            );
            eprintln!("SKIP (typed, unprivileged host): privilege drop refused errno {errno}");
        }
    }
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
