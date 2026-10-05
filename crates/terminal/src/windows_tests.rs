//! Windows process-tree certification (P0-59) through the REAL Windows
//! spawn path of `lib.rs`: every child is spawned `CREATE_SUSPENDED` into
//! its own KILL_ON_JOB_CLOSE job (assign_strict + membership verification
//! before resume); cancel/kill terminate the job and dropping the
//! supervisor closes it (kill-on-close). `taskkill` is only a best-effort
//! fallback for pids the supervisor never owned. Runtime-certification
//! only on a Windows host — on unix hosts this module does not exist.
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
        filesystem_isolation: FilesystemIsolation::Inherit,
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
    let task = tokio::spawn(async move { sup2.run(cfg, Duration::from_secs(120), token2).await });

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
        filesystem_isolation: FilesystemIsolation::Inherit,
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
        filesystem_isolation: FilesystemIsolation::Inherit,
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
        filesystem_isolation: FilesystemIsolation::Inherit,
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
