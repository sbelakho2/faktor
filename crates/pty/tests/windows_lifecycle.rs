//! P0-59 ConPTY lifecycle certification (Windows only): exercises the REAL
//! wave-13 ConPTY backend of faktor-pty (`CreatePseudoConsole` +
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`) against live processes.
//!
//! Four guarantees under test:
//! 1. `Pty::kill` ends a live sleeper attached to the session: the OS
//!    terminates the attached client when the pseudoconsole closes, with
//!    the bounded `TerminateProcess` fallback.
//! 2. Dropping the `Pty` (the daemon-crash equivalent for a session) kills
//!    every process attached to the session — a `Start-Process
//!    -NoNewWindow` grandchild shares the pseudoconsole console, so
//!    session close must take it too, not just the direct child.
//! 3. JOB containment, not console semantics: the child is assigned to a
//!    `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` job BEFORE `Pty::spawn` returns
//!    (create suspended → assign → resume), so `kill`/drop kills the whole
//!    tree even for a grandchild with its OWN console (`-WindowStyle
//!    Hidden`), which `ClosePseudoConsole` alone cannot reach.
//! 4. Spawn-time argument delivery: `-Command <script>` actually executes.
//!    A command line whose argv[0] is not the full resolved module path
//!    (e.g. a truncated `powershell.ex`) starts PowerShell interactively and
//!    silently drops the switches, so this is the end-to-end proof that the
//!    per-argument MSVCRT command line reaches the child intact.
//!
//! Tree shape (deterministic on CI): powershell.exe is the ConPTY client;
//! it starts a ping.exe grandchild (-NoNewWindow => same console session,
//! NOT a detached console) that writes its pid to a file and sleeps
//! ~60 s, then sleeps 60 s itself.
//!
//! Every liveness check is a bounded poll (10 s ceiling, 100 ms steps) so a
//! slow CI reaper can never hang the suite; ping self-terminates after
//! ~60 s, so even a failed assertion cannot leave an eternal sleeper.

#![allow(unsafe_code)]
// platform authority module: every unsafe
// block/function in this module carries a `// SAFETY:` justification and is
// enumerated by tests/static-authority.
#![cfg(windows)]

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

use faktor_pty::{Pty, PtyConfig};

static PIDFILE_SEQ: AtomicU32 = AtomicU32::new(0);

/// Direct Win32 liveness probe; see crates/winjob/tests/windows_tree.rs for
/// the semantics (an open process handle stays valid after termination).
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

static CONPTY_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pid_file_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "faktor-pty-lifecycle-{}-{}.pid",
        std::process::id(),
        PIDFILE_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// powershell sleeps 60 s; the ping grandchild is created ~1.5 s in (once
/// powershell is a settled ConPTY client), runs -NoNewWindow so it shares
/// the pseudoconsole console, writes its pid, and sleeps ~60 s.
fn sleeper_tree_script(pid_file: &Path) -> String {
    // Robust on CI: absolute system ping path (no PATH reliance), and the
    // pid is written with Set-Content ascii to avoid any encoding quirk.
    format!(
        "Start-Sleep -Milliseconds 1500; \
         $ping = Join-Path $env:SystemRoot 'System32\\ping.exe'; \
         $p = Start-Process -FilePath $ping -ArgumentList '-n','60','127.0.0.1' \
             -NoNewWindow -PassThru; \
         Set-Content -Path '{}' -Value ([string]$p.Id) -Encoding ascii; \
         Start-Sleep -Seconds 60",
        pid_file.display()
    )
}

fn pty_config(script: &str) -> PtyConfig {
    PtyConfig {
        command: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.into(),
        ],
        ..Default::default()
    }
}

fn read_grandchild_pid(pid_file: &Path) -> u32 {
    let pid: u32 = std::fs::read_to_string(pid_file)
        .expect("grandchild pid file readable")
        .trim()
        .parse()
        .expect("grandchild pid file holds a pid");
    assert_ne!(pid, 0);
    pid
}

/// Bounded wait for the grandchild's pid file, dumping the pseudoconsole ring
/// on timeout so a CI failure names PowerShell's own error.
fn wait_for_grandchild_pid(pty: &Pty, pid_file: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !pid_file.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out after 60s waiting for grandchild pid file; pty output: {}",
            String::from_utf8_lossy(&pty.snapshot())
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    read_grandchild_pid(pid_file)
}

/// `Pty::kill` must terminate a live ConPTY child promptly (session close +
/// bounded TerminateProcess fallback), and the pty must report it dead.
#[test]
fn kill_terminates_the_live_conpty_child() {
    // ConPTY cold-start on a loaded CI runner is the flake source: the
    // three lifecycle tests serialize (documented; no product behavior
    // change) and waits are generous but bounded.
    let _serial = CONPTY_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let script = "Start-Sleep -Seconds 30".to_string();
    let mut pty = Pty::spawn(&pty_config(&script)).expect("ConPTY spawn on CI");
    let child = pty.pid();
    assert!(
        pid_alive(child),
        "powershell must be alive right after spawn"
    );

    std::thread::sleep(Duration::from_millis(800));
    assert!(pty.is_alive(), "powershell must survive its engine startup");

    pty.kill();

    assert!(!pty.is_alive(), "kill must mark the session closed");
    wait_until("conpty child death", Duration::from_secs(10), || {
        !pid_alive(child)
    });
}

/// Dropping the Pty (session owner gone = the pty-side crash) must kill the
/// direct child AND the -NoNewWindow grandchild sharing the session.
#[test]
fn dropping_the_pty_kills_the_whole_session_tree() {
    // ConPTY cold-start on a loaded CI runner is the flake source: the
    // three lifecycle tests serialize (documented; no product behavior
    // change) and waits are generous but bounded.
    let _serial = CONPTY_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let pid_file = pid_file_path();
    let script = sleeper_tree_script(&pid_file);
    let pty = Pty::spawn(&pty_config(&script)).expect("ConPTY spawn on CI");
    let child = pty.pid();
    // Self-diagnosing wait: a timeout dumps the pseudoconsole ring so the
    // CI failure names PowerShell's own error instead of just 'timed out'.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !pid_file.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out after 20s waiting for grandchild pid file; pty output: {}",
            String::from_utf8_lossy(&pty.snapshot())
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let grandchild: u32 = read_grandchild_pid(&pid_file);
    assert_ne!(grandchild, 0);

    assert!(
        pid_alive(child) && pid_alive(grandchild),
        "powershell + ping grandchild must be alive pre-drop"
    );

    drop(pty); // job close (kill-on-close) takes the whole contained tree

    wait_until(
        "whole-session death after drop",
        Duration::from_secs(10),
        || !pid_alive(child) && !pid_alive(grandchild),
    );
    let _ = std::fs::remove_file(&pid_file);
}

/// `Pty::kill` must terminate the direct child AND a grandchild sharing the
/// session: the job's kill-on-close is the tree authority, so a
/// child-only `TerminateProcess` is a regression.
#[test]
fn kill_terminates_the_whole_conpty_tree() {
    // ConPTY cold-start on a loaded CI runner is the flake source: the
    // lifecycle tests serialize (documented; no product behavior change)
    // and waits are generous but bounded.
    let _serial = CONPTY_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let pid_file = pid_file_path();
    let script = sleeper_tree_script(&pid_file);
    let mut pty = Pty::spawn(&pty_config(&script)).expect("ConPTY spawn on CI");
    let child = pty.pid();
    let grandchild = wait_for_grandchild_pid(&pty, &pid_file);
    assert!(
        pid_alive(child) && pid_alive(grandchild),
        "powershell + ping grandchild must be alive pre-kill"
    );

    pty.kill();

    assert!(!pty.is_alive(), "kill must mark the session closed");
    wait_until(
        "whole-tree death after kill",
        Duration::from_secs(10),
        || !pid_alive(child) && !pid_alive(grandchild),
    );
    let _ = std::fs::remove_file(&pid_file);
}

/// JOB containment, not console semantics: `-WindowStyle Hidden` gives the
/// ping grandchild its OWN console, so `ClosePseudoConsole` cannot reach it —
/// only Job-Object membership (inherited at creation) can. The grandchild is
/// started IMMEDIATELY (no settle delay), so this also covers the
/// create-suspended → assign → resume order: a child still in `CREATE_SUSPENDED`
/// cannot spawn a descendant before assignment.
#[test]
fn kill_on_close_takes_a_console_detached_grandchild() {
    let _serial = CONPTY_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let pid_file = pid_file_path();
    let script = format!(
        "$ping = Join-Path $env:SystemRoot 'System32\\ping.exe'; \
         $p = Start-Process -FilePath $ping -ArgumentList '-n','60','127.0.0.1' \
             -WindowStyle Hidden -PassThru; \
         Set-Content -Path '{}' -Value ([string]$p.Id) -Encoding ascii; \
         Start-Sleep -Seconds 60",
        pid_file.display()
    );
    let pty = Pty::spawn(&pty_config(&script)).expect("ConPTY spawn on CI");
    let child = pty.pid();
    let grandchild = wait_for_grandchild_pid(&pty, &pid_file);
    assert!(
        pid_alive(child) && pid_alive(grandchild),
        "powershell + own-console ping grandchild must be alive pre-drop"
    );

    drop(pty); // kill-on-close: the OS terminates every job member

    wait_until(
        "job-only grandchild death after drop",
        Duration::from_secs(10),
        || !pid_alive(child) && !pid_alive(grandchild),
    );
    let _ = std::fs::remove_file(&pid_file);
}

/// Session/daemon shutdown mirror (the server owns `HashMap<pty_id, Pty>`
/// and drops entries at session teardown): removing ONE session's pty kills
/// exactly its tree — per-Pty jobs are not shared — while dropping the whole
/// map (daemon shutdown) takes every remaining tree via job-close.
#[test]
fn session_shutdown_drops_only_its_own_tree_and_daemon_drop_takes_all() {
    let _serial = CONPTY_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let mut ptys: std::collections::HashMap<u64, Pty> = std::collections::HashMap::new();
    let mut trees: Vec<(std::path::PathBuf, u32, u32)> = Vec::new();
    for id in 0..2u64 {
        let pid_file = pid_file_path();
        let pty = Pty::spawn(&pty_config(&sleeper_tree_script(&pid_file))).expect("ConPTY spawn");
        let child = pty.pid();
        let grandchild = wait_for_grandchild_pid(&pty, &pid_file);
        ptys.insert(id, pty);
        trees.push((pid_file, child, grandchild));
    }

    // Session teardown for terminal 0 only.
    let (_, child0, grandchild0) = &trees[0];
    drop(ptys.remove(&0).expect("owned pty"));
    wait_until("session-0 tree death", Duration::from_secs(10), || {
        !pid_alive(*child0) && !pid_alive(*grandchild0)
    });

    // Terminal 1 is untouched: one job per Pty, never a shared job.
    let (_, child1, grandchild1) = &trees[1];
    assert!(
        pid_alive(*child1) && pid_alive(*grandchild1),
        "dropping one session's pty must not kill another session's tree"
    );

    // Daemon shutdown: the remaining map drops; its tree follows.
    drop(ptys);
    wait_until("daemon-drop tree death", Duration::from_secs(10), || {
        !pid_alive(*child1) && !pid_alive(*grandchild1)
    });
    for (pid_file, _, _) in &trees {
        let _ = std::fs::remove_file(pid_file);
    }
}

/// Spawn-time argument delivery proof: the `-Command` script must actually
/// run and write the marker. A pty whose command line truncates argv[0]
/// (e.g. `powershell.ex`) starts PowerShell interactive, so the script never
/// executes; the bounded wait fails here and dumps the captured pty output
/// so CI names the real cause.
#[test]
fn command_argument_runs_the_script_to_completion() {
    // ConPTY cold-start on a loaded CI runner is the flake source: the
    // three lifecycle tests serialize (documented; no product behavior
    // change) and waits are generous but bounded.
    let _serial = CONPTY_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let marker = std::env::temp_dir().join(format!(
        "faktor-pty-command-{}-{}.txt",
        std::process::id(),
        PIDFILE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let script = format!(
        "Set-Content -Path '{}' -Value ok -Encoding ascii",
        marker.display()
    );
    let pty = Pty::spawn(&pty_config(&script)).expect("ConPTY spawn on CI");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out after 20s waiting for command marker {}; pty output: {}",
            marker.display(),
            String::from_utf8_lossy(&pty.snapshot())
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let content = std::fs::read_to_string(&marker).expect("marker readable");
    let _ = std::fs::remove_file(&marker);
    assert_eq!(content.trim(), "ok", "script must write the marker content");
}

/// Diagnostics contract for the CI lane: when `CreateProcessW` itself fails,
/// the typed error MUST name the failing step and carry the exact win32
/// code. A generic "spawn failed" message is a regression because the
/// Windows runner has no other way to root-cause the failure.
///
/// The bogus module is an existing file with valid-looking path syntax but
/// no PE image, so resolution succeeds and `CreateProcessW` itself fails
/// with ERROR_BAD_EXE_FORMAT (193).
#[test]
fn create_process_failure_names_the_step_and_win32_code() {
    let bogus = std::env::temp_dir().join(format!(
        "faktor-pty-not-a-pe-{}-{}.bin",
        std::process::id(),
        PIDFILE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&bogus, b"this file is not a PE image").expect("write bogus module");

    let cfg = PtyConfig {
        command: bogus.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let err = Pty::spawn(&cfg).unwrap_err();
    let msg = err.to_string();
    let _ = std::fs::remove_file(&bogus);

    assert!(
        msg.contains("CreateProcessW"),
        "error must name the failing step, got: {msg}"
    );
    assert!(
        msg.contains("193"),
        "error must carry ERROR_BAD_EXE_FORMAT (193), got: {msg}"
    );
}
