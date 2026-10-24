//! Independent final-wave probes (written by the no-gap verifier, not the
//! feature owner).
//!
//! * macOS: a Required `DenyAll` + workspace demand must deny loopback
//!   network, home writes, credential-store reads and parent escapes while
//!   allowing workspace writes — or refuse typed when the backend is
//!   unusable, never run unconfined.
//! * Unix: closing the daemon-side guardian channel (exactly what a daemon
//!   SIGKILL does) must extinguish the whole supervised group — leader,
//!   descendant and guardian — within one bounded window.

use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_core::cancellation::CancellationToken;

use crate::{ProcessSupervisor, SpawnConfig};

fn supervisor() -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    (dir, ProcessSupervisor::new(cas))
}

fn wait_until(what: &str, bound: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + bound;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[allow(unsafe_code)]
fn pid_gone(pid: u32) -> bool {
    // SAFETY: signal 0 only probes existence and delivers nothing; a
    // non-zero result with ESRCH is the only "gone" verdict (a zombie still
    // answers 0, so a leaked zombie fails the bound instead of hiding).
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[allow(unsafe_code)]
fn group_gone(pgid: u32) -> bool {
    // SAFETY: signal 0 to the negated group only probes existence; `pgid`
    // is a live positive pid from this test's own spawn.
    unsafe { libc::kill(-(pgid as libc::pid_t), 0) != 0 }
}

fn read_pid(path: &std::path::Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "no pid in {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Locate the Rust `faktor-ci-tools` tcp probe when the workspace built it.
/// Any member of this workspace can run the probe directly; the terminal
/// crate's own test binary is the fallback so an isolated crate test never
/// depends on a sibling binary that cargo did not build.
#[cfg(target_os = "macos")]
fn rust_tcp_probe() -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("FAKTOR_CI_TOOLS") {
        candidates.push(std::path::PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(target) = exe.parent().and_then(std::path::Path::parent) {
            candidates.push(target.join("faktor-ci-tools"));
        }
    }
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        let target = std::path::PathBuf::from(target);
        candidates.push(target.join("debug/faktor-ci-tools"));
        candidates.push(target.join("release/faktor-ci-tools"));
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for profile in ["debug", "release"] {
        candidates.push(
            manifest
                .join("../..")
                .join("target")
                .join(profile)
                .join("faktor-ci-tools"),
        );
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(target_os = "macos")]
fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// A Rust TCP-connect probe executed INSIDE the confined child. The macOS
/// step used to shell out to an interpreter one-liner; the repository has no
/// Python tooling, so the probe is either the `faktor-ci-tools tcp-connect`
/// helper or this same test binary re-executed with `--ignored --exact`.
#[cfg(target_os = "macos")]
fn rust_tcp_probe_command(port: u16) -> String {
    if let Some(tool) = rust_tcp_probe() {
        return format!(
            "{} tcp-connect --host 127.0.0.1 --port {port} >probe.out 2>&1; echo net=$?; cat probe.out",
            shell_quote(&tool)
        );
    }
    let exe = std::env::current_exe().expect("the test binary has a path");
    format!(
        "FAKTOR_FINAL_PROBE_PORT={port} {} --exact final_surface_probes::final_probe_tcp_child \
         --ignored --nocapture >probe.out 2>&1; echo net=$?; cat probe.out",
        shell_quote(&exe)
    )
}

/// Child-side Rust probe: no fixture, no interpreter. Invoked only by the
/// macOS confinement test through `rust_tcp_probe_command`; absent the env
/// marker it is a no-op so an ordinary ignored run never touches a port.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "[security] spawned by final_probe_macos_confinement_denies_network_home_and_allows_workspace as its Rust TCP probe"]
fn final_probe_tcp_child() {
    let Ok(port) = std::env::var("FAKTOR_FINAL_PROBE_PORT") else {
        return;
    };
    let port: u16 = port.parse().expect("FAKTOR_FINAL_PROBE_PORT is a port");
    match std::net::TcpStream::connect(("127.0.0.1", port)) {
        Ok(_) => {
            println!("tcp-connect-allowed");
            std::process::exit(0);
        }
        Err(error) => {
            println!("tcp-connect-denied: {error}");
            std::process::exit(1);
        }
    }
}

/// macOS: live Required confinement through the real supervisor. A local
/// loopback listener is the network target, so the probe never depends on
/// external connectivity.
#[cfg(target_os = "macos")]
#[test]
fn final_probe_macos_confinement_denies_network_home_and_allows_workspace() {
    use crate::{FilesystemIsolation, NetworkIsolation};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();

    let (_dir, sup) = supervisor();
    let workspace = tempfile::Builder::new()
        .prefix("faktor-final-probe-")
        .tempdir_in("/Users/Shared")
        .expect("/Users/Shared is writable");
    let root = workspace.path().to_path_buf();
    let escape = root
        .parent()
        .unwrap()
        .join(format!("faktor-final-probe-parent-{}", std::process::id()));
    let home_escape = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(format!("faktor-final-probe-home-{}", std::process::id()));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let ssh_exists = home.as_ref().is_some_and(|h| h.join(".ssh").is_dir());
    let aws_exists = home.as_ref().is_some_and(|h| h.join(".aws").is_dir());
    let keychain = home
        .as_ref()
        .map(|h| h.join("Library/Keychains"))
        .filter(|path| path.is_dir());
    // A workspace symlink into a directory outside the granted root: the
    // sandbox resolves vnode paths, so writing through it must be denied.
    let outside = root.parent().unwrap().join(format!(
        "faktor-final-probe-outside-{}-{}",
        std::process::id(),
        port
    ));
    std::fs::create_dir_all(&outside).unwrap();
    let link = root.join("outside-link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let tmp_escape = format!(
        "/tmp/faktor-final-probe-tmp-{}-{}",
        std::process::id(),
        port
    );

    let net_probe = rust_tcp_probe_command(port);
    let script = format!(
        "printf ok > allowed.txt; echo ws=$?\n\
         {net_probe}\n\
         printf x > '{home}' 2>/dev/null; echo home=$?\n\
         printf x > '{escape}' 2>/dev/null; echo parent=$?\n\
         printf x > '{tmp_escape}' 2>/dev/null; echo tmp=$?\n\
         printf x > outside-link/escaped.txt 2>/dev/null; echo link=$?\n\
         ls ~/.ssh >/dev/null 2>ssh.err; echo ssh=$?; head -1 ssh.err\n\
         ls ~/.aws >/dev/null 2>aws.err; echo aws=$?; head -1 aws.err\n\
         ls '{keychain}' >/dev/null 2>kc.err; echo keychain=$?; head -1 kc.err\n\
         cat allowed.txt\n",
        home = home_escape.display(),
        escape = escape.display(),
        tmp_escape = tmp_escape,
        keychain = keychain
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "/nonexistent-keychain".into()),
    );
    let cfg = SpawnConfig {
        cmd: "/bin/sh".into(),
        args: vec!["-c".into(), script],
        cwd: root.clone(),
        network_isolation: NetworkIsolation::DenyAll,
        filesystem_isolation: FilesystemIsolation::Workspace {
            roots: vec![root.clone()],
            required: true,
        },
        ..SpawnConfig::default()
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    match rt.block_on(sup.run(cfg, Duration::from_secs(30), CancellationToken::new())) {
        Ok(out) => {
            assert_eq!(out.exit_code, Some(0), "{}", out.excerpt);
            let report = out.excerpt.as_str();
            assert!(
                report.contains("ws=0"),
                "workspace write must succeed: {report}"
            );
            assert!(
                !report.contains("net=0"),
                "loopback network must be denied: {report}"
            );
            assert!(
                report.contains("tcp-connect-denied") || report.contains("tcp-connect:"),
                "the Rust TCP probe must actually run inside the child: {report}"
            );
            assert!(
                report.contains("home=1"),
                "home write must be denied: {report}"
            );
            assert!(
                report.contains("parent=1"),
                "parent escape must be denied: {report}"
            );
            if ssh_exists {
                assert!(
                    report.contains("ssh=1") && report.contains("Operation not permitted"),
                    "credential-store read must be denied by the sandbox: {report}"
                );
            }
            if aws_exists {
                assert!(
                    report.contains("aws=1") && report.contains("Operation not permitted"),
                    "the AWS credential store must be denied: {report}"
                );
            }
            if keychain.is_some() {
                assert!(
                    report.contains("keychain=1") && report.contains("Operation not permitted"),
                    "the macOS keychain must be denied: {report}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(root.join("allowed.txt")).unwrap(),
                "ok"
            );
            assert_eq!(
                report.matches("tmp=1").count(),
                1,
                "a direct /tmp escape must be denied: {report}"
            );
            assert_eq!(
                report.matches("link=1").count(),
                1,
                "a symlink escape must be denied: {report}"
            );
            assert!(!home_escape.exists(), "no home escape file may exist");
            assert!(!escape.exists(), "no parent escape file may exist");
            assert!(
                !std::path::Path::new(&tmp_escape).exists(),
                "no /tmp escape file may exist"
            );
            assert!(
                !outside.join("escaped.txt").exists(),
                "no symlink escape file may exist"
            );
            assert!(
                listener.accept().is_err(),
                "the denied child must never reach the host listener"
            );
        }
        Err(err) => {
            assert_eq!(
                err.kind,
                faktor_core::error::ErrorKind::Permission,
                "{err:?}"
            );
            assert!(
                err.message.contains(crate::SANDBOX_BACKEND_UNAVAILABLE),
                "only the typed backend refusal may replace the live run: {err:?}"
            );
        }
    }
}

/// Unix: the daemon-side channel close is the SIGKILL equivalent. The
/// guardian must kill the leader's whole group and exit, leaving no
/// descendant and no guardian behind.
#[cfg(unix)]
#[test]
fn final_probe_guardian_release_reaps_group_within_bound() {
    let (dir, sup) = supervisor();
    let marker = format!("faktor-final-guardian-{}", std::process::id());
    let pidfile = dir.path().join("leaf.pid");
    let script = format!(
        "sh -c 'sleep 300' & echo $! > '{}'; wait # {marker}",
        pidfile.display()
    );
    let handle = sup
        .spawn(SpawnConfig {
            cmd: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            cwd: dir.path().to_path_buf(),
            ..SpawnConfig::default()
        })
        .expect("supervised tree");
    let leaf = read_pid(&pidfile);
    let guardian = sup
        .guardian_pid_for_tests(handle.id)
        .expect("a guarded child");
    assert!(!pid_gone(handle.pid) && !pid_gone(leaf) && !pid_gone(guardian));
    assert!(
        sup.release_guardian_for_tests(handle.id),
        "the row owns a live guardian"
    );
    wait_until("daemon-death group kill", Duration::from_secs(10), || {
        group_gone(handle.pid) && pid_gone(handle.pid) && pid_gone(leaf) && pid_gone(guardian)
    });
    let _ = sup.reap();
    assert_eq!(sup.registered(), 0, "no row may survive the guarded kill");
}
