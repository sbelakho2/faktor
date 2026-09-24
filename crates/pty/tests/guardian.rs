//! End-to-end daemon-death tests (unix): a simulated daemon process spawns a
//! real PTY whose shell forks a grandchild, reports the pids, and SIGKILLs
//! itself — exactly the crash that used to leave the detached session alive.
//! The forked guardian must SIGKILL the whole process group within a bounded
//! window (no surviving member, no zombie), and a restarted daemon must
//! reconcile the durable ledger row as a typed `TerminalLost`.
//!
//! The simulated daemon is THIS test binary re-exec'd with
//! `--exact simulated_daemon_helper`: the helper test does the PTY work when
//! `FAKTOR_PTY_SIM_DAEMON_FD` names the inherited report pipe, and is an
//! inert pass in the ordinary suite otherwise. That keeps the forking out of
//! the libtest process entirely (the helper is a fresh, exec'd process).

#![allow(unsafe_code)]
// platform authority module: every unsafe
// block/function in this module carries a `// SAFETY:` justification and is
// enumerated by tests/static-authority.
#![cfg(unix)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use faktor_pty::guardian::{TerminalDisposition, TerminalLedger};
use faktor_pty::{EnvSpec, PtyConfig};

const HELPER_TEST: &str = "simulated_daemon_helper";
const REPORT_FD_ENV: &str = "FAKTOR_PTY_SIM_DAEMON_FD";
const LEDGER_DIR_ENV: &str = "FAKTOR_PTY_SIM_LEDGER_DIR";

/// The in-child script: the shell traps SIGHUP (a closing PTY master would
/// otherwise hang the group up all by itself — the test must measure the
/// guardian, not the tty hangup), forks a grandchild that ignores SIGHUP
/// too, prints its pid, and waits.
const GRANDCHILD_SCRIPT: &str = "trap '' HUP; \
     sh -c 'trap \"\" HUP; while :; do sleep 1; done' & \
     echo grandchild-$!; wait";

/// Runs inside the re-exec'd test binary. Without the report fd this is an
/// inert pass (ordinary `cargo test`).
#[test]
fn simulated_daemon_helper() {
    let Ok(report_fd) = std::env::var(REPORT_FD_ENV) else {
        return;
    };
    let report_fd: RawFd = report_fd.parse().expect("report fd");
    let cfg = PtyConfig {
        command: "sh".into(),
        args: vec!["-c".into(), GRANDCHILD_SCRIPT.into()],
        cwd: None,
        env: EnvSpec::default_baseline(),
        rows: 24,
        cols: 80,
    };
    let pty = if let Ok(dir) = std::env::var(LEDGER_DIR_ENV) {
        let ledger = std::sync::Arc::new(
            TerminalLedger::open(std::path::Path::new(&dir)).expect("ledger open"),
        );
        faktor_pty::Pty::spawn_recorded(&cfg, &ledger, "sim-session:1/task:1").expect("recorded")
    } else {
        faktor_pty::Pty::spawn(&cfg).expect("pty spawn")
    };
    assert!(
        pty.wait_for_contains("grandchild-", Duration::from_secs(15)),
        "grandchild must be forked and named: {:?}",
        String::from_utf8_lossy(&pty.snapshot())
    );
    let text = String::from_utf8_lossy(&pty.snapshot()).into_owned();
    let grandchild: i32 = text
        .split("grandchild-")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|pid| pid.parse().ok())
        .expect("grandchild pid in the pty output");
    let msg = format!("{} {}\n", pty.pid(), grandchild);
    // SAFETY: the fd is owned/open on this path and the buffer is a live bounded slice whose length is passed exactly.
    unsafe {
        assert_eq!(
            libc::write(report_fd, msg.as_ptr().cast(), msg.len()),
            msg.len() as isize,
            "report pids"
        );
    }
    // The parent cannot prove "the group was alive at crash time" by looking
    // after the fact: the guardian can settle the group before the test
    // thread is scheduled again. The proof is taken HERE, synchronously,
    // immediately before the simulated crash: both the leader and the
    // grandchild must be signallable right now. On failure the helper does
    // not SIGKILL itself, so the test's daemon-was-SIGKILLed assertion fails.
    // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
    unsafe {
        assert_eq!(
            libc::kill(pty.pid() as i32, 0),
            0,
            "the pty leader must be alive at crash time"
        );
        assert_eq!(
            libc::kill(grandchild, 0),
            0,
            "the grandchild must be alive at crash time"
        );
    }
    if let Ok(hold_ms) = std::env::var("FAKTOR_PTY_SIM_HOLD_MS") {
        // Control mode: stay alive (holding the control pipe) until the
        // parent SIGKILLs this process.
        let hold_ms: u64 = hold_ms.parse().expect("hold ms");
        std::thread::sleep(Duration::from_millis(hold_ms));
        return;
    }
    // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
    unsafe {
        // Simulated daemon crash: no Drop, no shutdown, no pipe close
        // performed by us — only the process death closes the control pipe.
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    unreachable!("SIGKILL cannot return");
}

fn parse_report(line: &str) -> (i32, i32) {
    let mut parts = line.split_whitespace();
    let leader: i32 = parts.next().expect("leader pid").parse().expect("pid");
    let grandchild: i32 = parts.next().expect("grandchild pid").parse().expect("pid");
    (leader, grandchild)
}

/// Is the recorded process group still a settle-relevant entity?
///
/// `kill(-pgid, 0)` semantics: `0` means a member exists; `EPERM` ALSO means
/// a member exists — on Darwin a group holding only unreaped zombies answers
/// `EPERM`, so treating that as "gone" would let a waiter observe a settled
/// group while a member is still unreaped (and reconciliation would then
/// classify the survivor as `StillAlive`). Only `ESRCH` proves the group has
/// no member left, live or zombie.
fn group_alive(pgid: i32) -> bool {
    // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Same probe semantics for one pid: `EPERM` is "exists, not ours to signal"
/// — a live target, never a settled one.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Wait until the process group has NO member left — live or unreaped zombie.
/// The only observable that authorizes "settled" is `ESRCH`; the bound is an
/// explicit generous deadline for a wedged guardian, not a sleep to outlast.
fn wait_group_settled(leader: i32, bound: Duration, what: &str) {
    let deadline = Instant::now() + bound;
    while group_alive(leader) {
        assert!(Instant::now() < deadline, "{what} within {bound:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Bounded read of one `\n`-terminated line from `fd` (poll + read; never
/// blocks the suite past `timeout`).
fn read_line(fd: RawFd, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 128];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a live stack `pollfd` with the validated master fd; the timeout bounds the call.
        let r = unsafe {
            libc::poll(
                &mut pfd,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if r <= 0 {
            return None;
        }
        // SAFETY: the fd is owned/open on this path and the buffer is a live bounded slice whose length is passed exactly.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            return None;
        }
        out.extend_from_slice(&buf[..n as usize]);
        if let Some(pos) = out.iter().position(|&b| b == b'\n') {
            return Some(String::from_utf8_lossy(&out[..pos]).into_owned());
        }
    }
}

/// Spawn the simulated daemon (this test binary, helper test only) with the
/// write end of a fresh report pipe inherited via env.
fn spawn_simulated_daemon(
    ledger_dir: Option<&std::path::Path>,
    hold_ms: Option<u64>,
    own_group: bool,
) -> (OwnedFd, std::process::Child) {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a live 2-element stack array the kernel fills; the return value is checked.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let (read_fd, write_fd) = (fds[0], fds[1]);
    let exe = std::env::current_exe().expect("current exe");
    let mut cmd = Command::new(exe);
    cmd.arg("--exact")
        .arg(HELPER_TEST)
        .arg("--nocapture")
        .env(REPORT_FD_ENV, write_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = ledger_dir {
        cmd.env(LEDGER_DIR_ENV, dir);
    }
    if let Some(ms) = hold_ms {
        cmd.env("FAKTOR_PTY_SIM_HOLD_MS", ms.to_string());
    }
    if own_group {
        // The simulated daemon leaders its own group, so the test can
        // SIGKILL that WHOLE group (the guardian must have detached into
        // its own session to survive it).
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().expect("spawn simulated daemon");
    // SAFETY: the fd/handle was just produced by the preceding call on this path and its ownership transfers here exactly once (failure paths close it explicitly).
    unsafe {
        libc::close(write_fd);
    }
    // SAFETY: the fd/handle was just produced by the preceding call on this path and its ownership transfers here exactly once (failure paths close it explicitly).
    (unsafe { OwnedFd::from_raw_fd(read_fd) }, child)
}

#[test]
fn daemon_crash_kills_the_whole_pty_group_and_leaves_no_zombie() {
    let (report, mut daemon) = spawn_simulated_daemon(None, None, false);
    let line = read_line(report.as_raw_fd(), Duration::from_secs(30))
        .expect("the simulated daemon reports its pids");
    let (leader, grandchild) = parse_report(&line);
    assert!(leader > 0 && grandchild > 0);
    // Liveness at crash time is proven inside the helper, immediately before
    // it SIGKILLs itself (the parent cannot observe that instant reliably).
    let status = daemon.wait().expect("reap the simulated daemon");
    assert_eq!(status.signal(), Some(libc::SIGKILL), "daemon was SIGKILLed");
    // The daemon is gone; the detached session must not outlive it. The
    // guardian SIGKILLs the recorded group at control-pipe EOF. Settled =
    // ESRCH for the whole group (no live and no unreaped member).
    wait_group_settled(
        leader,
        Duration::from_secs(10),
        "guardian must kill the whole pty group",
    );
    // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
    unsafe {
        assert_eq!(libc::kill(-leader, 0), -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(libc::kill(grandchild, 0), -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[test]
fn restart_reconciliation_reports_the_lost_terminal_typed_and_never_kills() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (report, mut daemon) = spawn_simulated_daemon(Some(dir.path()), None, false);
    let line = read_line(report.as_raw_fd(), Duration::from_secs(30))
        .expect("the simulated daemon reports its pids");
    let (leader, _grandchild) = parse_report(&line);
    let status = daemon.wait().expect("reap the simulated daemon");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    // Wait for the guardian to settle the group, so the restart sees a
    // terminal that is gone (never a recycled pid): the group probe must
    // reach ESRCH — zombies included — before reconciliation is meaningful.
    wait_group_settled(
        leader,
        Duration::from_secs(10),
        "guardian settles the group",
    );
    // Restarted daemon: open the same durable ledger and reconcile.
    let ledger = TerminalLedger::open(dir.path()).expect("restart ledger open");
    let report = ledger.reconcile().expect("reconcile");
    assert_eq!(
        report.lost.len(),
        1,
        "the crashed terminal is reported lost"
    );
    let lost = &report.lost[0];
    assert_eq!(lost.owner, "sim-session:1/task:1");
    assert_eq!(lost.pid, leader as u32);
    assert_eq!(lost.pgid, leader as u32);
    assert_eq!(lost.disposition, TerminalDisposition::Gone);
    assert!(
        lost.start_time.is_some(),
        "durable identity carries a marker"
    );
    // Exactly once: a second restart reports nothing.
    assert_eq!(ledger.reconcile().expect("second reconcile").lost.len(), 0);
}

#[test]
fn a_healthy_daemon_does_not_trigger_the_guardian() {
    // Control for the crash test: while the daemon lives (control pipe
    // open), the PTY group must stay alive well past the window in which
    // the crash test expects it to die.
    let (report, mut daemon) = spawn_simulated_daemon(None, Some(60_000), false);
    let line = read_line(report.as_raw_fd(), Duration::from_secs(30)).expect("pids reported");
    let (leader, grandchild) = parse_report(&line);
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        group_alive(leader),
        "guardian must not kill while the daemon lives"
    );
    assert!(pid_alive(grandchild));
    // Clean up: SIGKILL the daemon (crash path), then the guardian settles
    // the group; reap the daemon.
    // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
    unsafe {
        libc::kill(daemon.id() as i32, libc::SIGKILL);
    }
    let _ = daemon.wait();
    wait_group_settled(
        leader,
        Duration::from_secs(10),
        "guardian settles after the crash",
    );
}

#[test]
fn daemon_group_kill_does_not_take_the_guardian_with_it() {
    // The guardian detaches into its own session BEFORE blocking: a SIGKILL
    // aimed at the daemon's whole process group must not kill the guardian
    // before it can enforce the PTY group kill.
    let (report, mut daemon) = spawn_simulated_daemon(None, None, true);
    let line = read_line(report.as_raw_fd(), Duration::from_secs(30))
        .expect("the simulated daemon reports its pids");
    let (leader, _grandchild) = parse_report(&line);
    let daemon_pgid = daemon.id() as i32;
    // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
    unsafe {
        libc::kill(-daemon_pgid, libc::SIGKILL);
    }
    let status = daemon.wait().expect("reap the simulated daemon");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    wait_group_settled(
        leader,
        Duration::from_secs(10),
        "the detached guardian must still kill the pty group",
    );
    // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
    unsafe {
        assert_eq!(libc::kill(-leader, 0), -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
