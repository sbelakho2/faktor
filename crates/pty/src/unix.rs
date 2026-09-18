//! Unix interactive-terminal backend.
//!
//! `Pty::spawn` creates a real pseudo-terminal (posix_openpt/grantpt/
//! unlockpt/ptsname), attaches the child's stdio to the slave side with a
//! controlling terminal (setsid + TIOCSCTTY), and exposes the master side:
//! write stdin, resize the window, snapshot/drain output, close.
//!
//! Output capture is a bounded ring (drop-oldest bytes) drained by a
//! dedicated reader thread — the child can never deadlock on a full pipe
//! and memory stays bounded regardless of output volume. The reader thread
//! BLOCKS in read(2) on its own duplicate of the master (no polling): data
//! wakes it, and EOF (every slave fd closed — i.e. the process group died)
//! wakes it at shutdown. The SAME thread is the single process reaper
//! (waitpid), so there is exactly one owner of child reaping; `kill()`
//! signals the group and then joins the reader thread, and `Drop` is the
//! emergency failsafe (immediate SIGKILL + join, no grace sleeps).
//!
//! Zero-orphans hardening (daemon death is not a destructor):
//!
//! - the reader thread is the child's PARENT: it spawns the child and then
//!   outlives it (it is also the reaper). That makes Linux's
//!   `PR_SET_PDEATHSIG(SIGKILL)` (armed in `pre_exec`) fire exactly when the
//!   daemon dies — with a pooled spawn thread as parent, PDEATHSIG would
//!   fire when that thread returned and kill a healthy terminal;
//! - immediately after the child exists, the reader thread forks a
//!   [`GuardianHandle`](crate::guardian::GuardianHandle) holding a
//!   CLOEXEC control pipe (write end in this process), the child's pgid and
//!   its start-time identity. If this process dies, the pipe reaches EOF and
//!   the guardian SIGKILLs the recorded process group unless a start-time
//!   mismatch proves the pid was recycled. On normal shutdown the group is
//!   killed and reaped FIRST, then the pipe is closed deliberately; the
//!   guardian finds nothing left to kill and exits cleanly. The guardian
//!   outlives any destructor and detaches into its own session, so it also
//!   survives a SIGKILL aimed at the daemon's process group long enough to
//!   enforce the kill;
//! - [`Pty::spawn_recorded`] additionally appends the identity to a durable
//!   [`TerminalLedger`](crate::guardian::TerminalLedger) so a restarted
//!   daemon can report the terminal as lost
//!   ([`TerminalLost`](crate::guardian::TerminalLost)) — typed, and without
//!   ever signalling a possibly recycled pid.

use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use faktor_core::error::Error;

use crate::guardian::{GuardianHandle, ProcessIdentity, TerminalLedger};
use crate::ring::{lock_ring, Ring};
use crate::validation::validate_spawn_config;
use crate::PtyConfig;

/// One live PTY. Sync API (the master side is O_NONBLOCK, reads are
/// non-blocking snapshots); a background thread owns the child (reads,
/// reaps) — dropping the handle kills the whole process group. On unix the
/// group is additionally protected by a forked daemon-death guardian.
pub struct Pty {
    master: OwnedFd,
    pid: libc::pid_t,
    shared: Arc<(Mutex<Ring>, Condvar)>,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// Daemon-death guardian for this pty's process group (`None` after
    /// teardown released it).
    guardian: Option<GuardianHandle>,
    /// Durable reconciliation row opened by [`Pty::spawn_recorded`]:
    /// `(ledger, row id)`, marked reaped once the group is gone.
    ledger: Option<(Arc<TerminalLedger>, String)>,
}

/// The optional durable-ledger plan of one spawn (see
/// [`Pty::spawn_recorded`]).
struct LedgerPlan {
    ledger: Arc<TerminalLedger>,
    owner: String,
}

/// Facts produced by the child-spawning reader thread and handed back to the
/// caller: the child pid, its guardian, and the durable row when requested.
struct ChildSpawn {
    pid: libc::pid_t,
    guardian: GuardianHandle,
    ledger: Option<(Arc<TerminalLedger>, String)>,
}

// SAFETY: every field is itself `Send + Sync` (owned fds and pid values,
// `Arc`-shared mutex/condvar state, atomic stop flag, a join handle, the
// guardian handle, and the ledger tuple), and all interior mutation goes
// through `Mutex`/atomics or the owned file descriptors. `Pty` has no
// thread-affine state, so sharing/moving it across threads cannot race: the
// reader thread owns the child reads, and teardown is serialized through the
// shared stop flag and the fd's own close semantics.
unsafe impl Send for Pty {}
// SAFETY: shared `&Pty` access only reaches `Mutex`/atomic state and
// `&self` syscalls on owned descriptors (e.g. non-blocking master reads),
// which are valid from any thread; no `&self` method mutates borrowed state
// without the mutex or atomics.
unsafe impl Sync for Pty {}

impl fmt::Debug for Pty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pty")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl Pty {
    /// Create the pty and spawn the child with its stdio on the slave.
    ///
    /// On unix the child's process group is additionally protected by a
    /// forked daemon-death guardian (see [`crate::guardian`]): if this
    /// process dies without reaping the child, the guardian SIGKILLs the
    /// whole group.
    pub fn spawn(cfg: &PtyConfig) -> Result<Self, Error> {
        Self::spawn_inner(cfg, None)
    }

    /// Like [`Pty::spawn`], plus a durable [`TerminalLedger`] row carrying
    /// the child's start-time-verified identity, so a RESTARTED daemon can
    /// reconcile this terminal as [`crate::guardian::TerminalLost`] instead
    /// of trusting a bare pid. A normal shutdown marks the row reaped; a
    /// crash leaves it live for the next `reconcile()`.
    pub fn spawn_recorded(
        cfg: &PtyConfig,
        ledger: &Arc<TerminalLedger>,
        owner: &str,
    ) -> Result<Self, Error> {
        Self::spawn_inner(
            cfg,
            Some(LedgerPlan {
                ledger: ledger.clone(),
                owner: owner.to_string(),
            }),
        )
    }

    fn spawn_inner(cfg: &PtyConfig, ledger_plan: Option<LedgerPlan>) -> Result<Self, Error> {
        validate_spawn_config(cfg)?;
        // 1. Open the master; grant + unlock + resolve the slave path.
        let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master_fd < 0 {
            return Err(Error::internal("posix_openpt failed"));
        }
        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        if unsafe { libc::grantpt(master_fd) } != 0 {
            return Err(Error::internal("grantpt failed"));
        }
        if unsafe { libc::unlockpt(master_fd) } != 0 {
            return Err(Error::internal("unlockpt failed"));
        }
        // The slave path must stay a CStr until libc::open: converting to a
        // Rust String and passing String::as_ptr() to open(2) reads past the
        // logical buffer (the String has no trailing NUL). ptsname's pointer
        // is a libc-owned static buffer valid until the next ptsname call on
        // this thread; we open before any further ptsname call.
        let slave_path = unsafe {
            let p = libc::ptsname(master_fd);
            if p.is_null() {
                return Err(Error::internal("ptsname failed"));
            }
            std::ffi::CStr::from_ptr(p)
        };
        let slave = unsafe { libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if slave < 0 {
            return Err(Error::internal("open slave failed"));
        }
        let slave_fd = unsafe { OwnedFd::from_raw_fd(slave) };

        // 2. Window size on the master.
        let mut ws = libc::winsize {
            ws_row: cfg.rows,
            ws_col: cfg.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(master_fd, libc::TIOCSWINSZ, &mut ws);
        }
        // Non-blocking master so snapshots never block.
        let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
        unsafe {
            libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // 3. The reader's private BLOCKING duplicate of the master, made
        //    BEFORE any process exists: data wakes read(2), EOF (every slave
        //    fd closed — the group died) wakes it at shutdown, and a failure
        //    here can never leak a process.
        let reader_master = unsafe { libc::dup(master.as_raw_fd()) };
        if reader_master < 0 {
            return Err(Error::internal("dup master failed"));
        }
        let reader_master = unsafe { OwnedFd::from_raw_fd(reader_master) };
        let f = unsafe { libc::fcntl(reader_master.as_raw_fd(), libc::F_GETFL) };
        unsafe {
            libc::fcntl(
                reader_master.as_raw_fd(),
                libc::F_SETFL,
                f & !libc::O_NONBLOCK,
            );
        }

        // 4. Spawn the child ON the reader thread, which then stays alive as
        //    its single reaper. Two deliberate consequences: (a) the child's
        //    parent thread outlives the child, so Linux PR_SET_PDEATHSIG
        //    (armed in pre_exec) fires on DAEMON death and never on a pooled
        //    spawn thread returning; (b) the guardian fork happens on the
        //    same thread immediately after spawn — the unguarded window is
        //    the few syscalls between them.
        let shared = Arc::new((Mutex::new(Ring::new()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<ChildSpawn, Error>>();
        let reader = {
            let shared = shared.clone();
            let stop = stop.clone();
            let cfg = cfg.clone();
            std::thread::spawn(move || {
                let spawned = match spawn_child(&cfg, slave_fd, ledger_plan) {
                    Ok(spawned) => spawned,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let pid = spawned.pid;
                let _ = tx.send(Ok(spawned));
                reader_loop(reader_master, pid, shared, stop);
            })
        };
        let spawned = match rx.recv() {
            Ok(Ok(spawned)) => spawned,
            Ok(Err(e)) => {
                let _ = reader.join();
                return Err(e);
            }
            Err(_) => {
                let _ = reader.join();
                return Err(Error::internal("pty spawn thread died"));
            }
        };

        Ok(Self {
            master,
            pid: spawned.pid,
            shared,
            stop,
            reader: Some(reader),
            guardian: Some(spawned.guardian),
            ledger: spawned.ledger,
        })
    }

    /// The child pid (0 when unsupported).
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// Write raw bytes to the pty's stdin (master side).
    pub fn write_all(&self, bytes: &[u8]) -> Result<(), Error> {
        let mut written = 0usize;
        while written < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.master.as_raw_fd(),
                    bytes[written..].as_ptr().cast(),
                    bytes.len() - written,
                )
            };
            if n > 0 {
                written += n as usize;
            } else {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EAGAIN) => {
                        // The tty input buffer is full: wait for POLLOUT
                        // with a bounded timeout instead of a 2 ms busy
                        // sleep. A slow consumer costs one wakeup per poll
                        // interval at most, never a hot loop.
                        let mut pfd = libc::pollfd {
                            fd: self.master.as_raw_fd(),
                            events: libc::POLLOUT,
                            revents: 0,
                        };
                        let r = unsafe { libc::poll(&mut pfd, 1, 100) };
                        if r < 0 {
                            let e2 = std::io::Error::last_os_error();
                            if e2.raw_os_error() == Some(libc::EINTR) {
                                continue;
                            }
                            return Err(Error::internal(format!("pty poll: {e2}")));
                        }
                        if r == 0 {
                            return Err(Error::internal("pty write stalled (POLLOUT timeout)"));
                        }
                    }
                    Some(libc::EINTR) => {}
                    _ => return Err(Error::internal(format!("pty write: {err}"))),
                }
            }
        }
        Ok(())
    }

    /// Write a line (the pty's line discipline handles CR/echo).
    pub fn write_line(&self, line: &str) -> Result<(), Error> {
        let mut b = line.as_bytes().to_vec();
        b.push(b'\n');
        self.write_all(&b)
    }

    /// Resize the terminal window.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), Error> {
        if rows == 0 || cols == 0 {
            return Err(Error::malformed("pty size must be non-zero"));
        }
        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let r = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &mut ws) };
        if r != 0 {
            return Err(Error::internal("TIOCSWINSZ failed"));
        }
        Ok(())
    }

    /// Current window size from the kernel.
    pub fn size(&self) -> (u16, u16) {
        let mut ws = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCGWINSZ, &mut ws);
        }
        (ws.ws_row, ws.ws_col)
    }

    /// Drain all currently available output.
    pub fn read_available(&self) -> Vec<u8> {
        lock_ring(&self.shared.0).drain()
    }

    /// Snapshot the current output WITHOUT draining.
    pub fn snapshot(&self) -> Vec<u8> {
        lock_ring(&self.shared.0).snapshot()
    }

    /// Total bytes ever read from the master.
    pub fn total_bytes(&self) -> u64 {
        lock_ring(&self.shared.0).total()
    }

    /// Block until `needle` appears in the accumulated output or `timeout`
    /// elapses (test/consumer helper).
    pub fn wait_for_contains(&self, needle: &str, timeout: std::time::Duration) -> bool {
        let (ring, cv) = &*self.shared;
        let mut guard = lock_ring(ring);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let snap = guard.snapshot();
            let text = String::from_utf8_lossy(&snap);
            if text.contains(needle) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            let (g, _t) = cv
                .wait_timeout(guard, std::time::Duration::from_millis(50))
                .unwrap();
            guard = g;
        }
    }

    /// Is the child still running? False once the reader thread has reaped
    /// it (a zombie reports false, matching the single-reaper design).
    pub fn is_alive(&self) -> bool {
        if self.pid <= 0 {
            return false;
        }
        let r = unsafe { libc::kill(self.pid, 0) };
        r == 0
    }

    /// Graceful shutdown: SIGTERM the process group, a short grace period,
    /// SIGKILL, then join the reader/reaper thread (bounded). This is the
    /// normal lifecycle for live objects; [`Drop`] is the emergency path.
    pub fn shutdown(&mut self) {
        if self.pid > 0 {
            unsafe {
                libc::kill(-self.pid, libc::SIGTERM);
            }
            // Give the group a short grace, watching for exit.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
            loop {
                if !self.is_alive() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if self.is_alive() {
                unsafe {
                    libc::kill(-self.pid, libc::SIGKILL);
                }
            }
        }
        self.settle();
    }

    /// Kill the process group (SIGTERM, short grace, SIGKILL), reap the
    /// child, release the guardian and settle the durable row. Idempotent;
    /// kept for compatibility with `kill()`.
    pub fn kill(&mut self) {
        self.shutdown();
    }

    /// Teardown (idempotent): reap the child on the reader thread, then
    /// release the guardian DELIBERATELY — the pipe closes after the group
    /// was killed and reaped, so the guardian finds nothing to kill and
    /// exits without signalling (a descendant that outlived the leader is
    /// SIGKILLed here). Finally the durable ledger row is marked reaped.
    fn settle(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        if let Some(mut guardian) = self.guardian.take() {
            guardian.release();
        }
        if let Some((ledger, row)) = self.ledger.take() {
            if let Err(e) = ledger.mark_reaped(&row) {
                // The durable row stays live under the bounded retry marker
                // that the next reconcile consumes; surfaced here, never
                // silently discarded.
                tracing::error!(
                    row = %row,
                    "terminal ledger reaped transition could not be appended: {}",
                    e.message
                );
            }
        }
    }
}

impl Drop for Pty {
    /// Emergency failsafe ONLY: immediate SIGKILL of the group (no grace
    /// sleep on the caller's thread) then reap + deliberate guardian
    /// release. Live objects should use [`Pty::shutdown`] for the graceful
    /// SIGTERM path.
    fn drop(&mut self) {
        if self.pid > 0 {
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
        self.settle();
    }
}

/// Spawn the child (stdio on the slave) and its guardian. Runs ON the
/// reader thread: it is the child's parent and stays alive until the child
/// is reaped, so Linux `PR_SET_PDEATHSIG` (armed in the pre-exec hook) is a
/// DAEMON-death signal, never a spawn-thread-exit signal.
fn spawn_child(
    cfg: &PtyConfig,
    slave: OwnedFd,
    ledger_plan: Option<LedgerPlan>,
) -> Result<ChildSpawn, Error> {
    use std::os::unix::process::CommandExt;

    let mut cmd = std::process::Command::new(&cfg.command);
    cmd.args(&cfg.args);
    if let Some(cwd) = &cfg.cwd {
        cmd.current_dir(cwd);
    }
    // THE identical environment authority the supervised spawn path uses:
    // env_clear, then the resolved EnvSpec (deny-set filtered,
    // GIT_TERMINAL_PROMPT safety default). PTY children never inherit the
    // daemon environment implicitly.
    cfg.env.apply(&mut cmd);
    let slave_fd = slave.into_raw_fd();
    cmd.stdin(unsafe { std::process::Stdio::from_raw_fd(slave_fd) });
    let dup = |fd: RawFd| unsafe { libc::dup(fd) };
    let err1 = dup(slave_fd);
    let err2 = dup(slave_fd);
    if err1 < 0 || err2 < 0 {
        unsafe {
            if err1 >= 0 {
                libc::close(err1);
            }
            if err2 >= 0 {
                libc::close(err2);
            }
        }
        return Err(Error::internal("dup slave failed"));
    }
    cmd.stdout(unsafe { std::process::Stdio::from_raw_fd(err1) });
    cmd.stderr(unsafe { std::process::Stdio::from_raw_fd(err2) });
    unsafe {
        cmd.pre_exec(move || {
            // New session + controlling terminal on the slave.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let r = libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0);
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                // Defense-in-depth only (the guardian is the cross-Unix
                // mechanism): when the daemon dies this child is SIGKILLed
                // by the kernel even in the window before the guardian
                // exists. The parent is the reader thread, which outlives
                // the child, so a pooled spawn thread returning can never
                // fire it. Best-effort: a kernel/seccomp refusal must not
                // fail the spawn, because the guardian still covers it.
                let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| Error::not_found(format!("spawn {}: {e}", cfg.command)))?;
    let pid = child.id() as libc::pid_t;
    // The Child handle is intentionally dropped after spawn: the reader
    // thread is the SINGLE reaper (waitpid). Keeping a second std Child
    // whose Drop/wait could race the reader's waitpid would create two
    // reapers (audit P0-56).
    drop(child);
    // NOTE: the slave fd was moved into the child's stdio above; it must
    // NOT be closed here (double close aborts under Rust's IO safety
    // checks).

    // Guardian immediately (the smallest possible unguarded window). On
    // failure the just-spawned group is killed and reaped rather than
    // exposed unguarded.
    let identity = ProcessIdentity::capture(pid as u32, pid as u32);
    let mut guardian = match GuardianHandle::spawn(identity) {
        Ok(guardian) => guardian,
        Err(e) => {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            reap_blocking(pid);
            return Err(e);
        }
    };
    let ledger = match ledger_plan {
        None => None,
        Some(plan) => match plan.ledger.record_spawn(&plan.owner, &identity) {
            Ok(row) => Some((plan.ledger, row)),
            Err(e) => {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                reap_blocking(pid);
                guardian.release();
                return Err(e);
            }
        },
    };
    Ok(ChildSpawn {
        pid,
        guardian,
        ledger,
    })
}

fn reap_blocking(pid: libc::pid_t) {
    let mut status = 0;
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
}

/// The reader + single reaper loop: blocking reads into the bounded ring,
/// then exactly one waitpid for the child. Owns its private blocking master
/// duplicate (closed when this returns, on every path).
fn reader_loop(
    mfd: OwnedFd,
    pid: libc::pid_t,
    shared: Arc<(Mutex<Ring>, Condvar)>,
    stop: Arc<AtomicBool>,
) {
    let mfd_raw = mfd.as_raw_fd();
    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe { libc::read(mfd_raw, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            let (ring, cv) = &*shared;
            lock_ring(ring).push(&buf[..n as usize]);
            cv.notify_all();
        } else if n == 0 {
            break; // EOF: every slave fd closed
        } else {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => {}
                _ => break, // real error: nothing more to read
            }
        }
    }
    // Reap the child — the ONLY waitpid in this crate. Normal children are
    // zombies by now (EOF after group death or natural exit) so WNOHANG
    // succeeds immediately. A child that outlives its stdio (daemonized) is
    // polled at a low cadence until the kill path sets `stop`, then one
    // final blocking wait (the group is being SIGKILLed).
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            break;
        }
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            break;
        }
        if stop.load(Ordering::SeqCst) {
            reap_blocking(pid);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The private master duplicate closes here via Drop, on every path.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::REAP_MARKER_FILE;
    use crate::ring::RING_MAX_BYTES;
    use crate::EnvSpec;
    use faktor_core::error::ErrorKind;

    fn sh_cfg(script: &str) -> PtyConfig {
        PtyConfig {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            rows: 24,
            cols: 80,
            ..Default::default()
        }
    }

    fn group_alive(pid: libc::pid_t) -> bool {
        (unsafe { libc::kill(pid, 0) }) == 0
    }

    #[test]
    fn interactive_round_trip_through_a_real_tty() {
        // read a line, echo it back; echo disabled so we assert OUR bytes.
        let cfg = sh_cfg("stty -echo; read x; echo out:$x; exit 0");
        let mut pty = Pty::spawn(&cfg).unwrap();
        pty.write_line("hello pty").unwrap();
        assert!(
            pty.wait_for_contains("out:hello pty", std::time::Duration::from_secs(10)),
            "the child must read our line through the pty: {:?}",
            String::from_utf8_lossy(&pty.snapshot())
        );
        pty.kill();
    }

    #[test]
    fn resize_reaches_the_kernel_and_the_shell() {
        // `stty size` prints the live rows/cols AFTER we resize: the child
        // waits on a line of input first, so the test is not a startup race
        // (Linux CI exposed the child winning it).
        let cfg = sh_cfg("read x; stty size");
        let mut pty = Pty::spawn(&cfg).unwrap();
        pty.resize(33, 121).unwrap();
        pty.write_line("go").unwrap();
        assert_eq!(pty.size(), (33, 121));
        assert!(
            pty.wait_for_contains("33 121", std::time::Duration::from_secs(10)),
            "TIOCSWINSZ must reach the child: {:?}",
            String::from_utf8_lossy(&pty.snapshot())
        );
        pty.kill();
    }

    #[test]
    fn huge_output_stays_bounded_and_never_deadlocks() {
        // seq 1..200000 through a pty: the reader drains continuously (the
        // child never blocks) and RAM stays bounded by the ring.
        let cfg = sh_cfg("seq 1 200000; exit 0");
        let mut pty = Pty::spawn(&cfg).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while pty.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = pty.read_available();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pty output must not deadlock the child"
        );
        assert!(pty.total_bytes() >= 200_000, "all output was drained");
        assert!(pty.snapshot().len() <= RING_MAX_BYTES, "ring stays bounded");
        pty.kill();
    }

    #[test]
    fn drop_is_emergency_sigkill_and_fast() {
        let cfg = sh_cfg("sleep 300");
        let (pid, elapsed) = {
            let pty = Pty::spawn(&cfg).unwrap();
            assert!(pty.is_alive());
            let start = std::time::Instant::now();
            drop(pty);
            (0, start.elapsed())
        };
        let _ = pid;
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "Drop must not block the caller with grace sleeps: {elapsed:?}"
        );
    }

    #[test]
    fn drop_kills_the_process_group() {
        let cfg = sh_cfg("sleep 300");
        let pid = {
            let pty = Pty::spawn(&cfg).unwrap();
            assert!(pty.is_alive());
            pty.pid()
        };
        // Dropped: the child group must be dead shortly after.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !group_alive(pid as libc::pid_t) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "dropped pty must kill its child group"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn naturally_exited_child_is_reaped_by_the_reader_thread() {
        // The reader thread is the single reaper: after a natural exit the
        // child must be reaped (no zombie) — is_alive() turns false.
        let cfg = sh_cfg("echo done; exit 0");
        let mut pty = Pty::spawn(&cfg).unwrap();
        assert!(
            pty.wait_for_contains("done", std::time::Duration::from_secs(10)),
            "child output arrives"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pty.is_alive() {
            assert!(
                std::time::Instant::now() < deadline,
                "the reader thread must reap the exited child"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        pty.kill();
    }

    #[test]
    fn sigterm_resisting_child_is_escalated_and_reaped() {
        // The child traps SIGTERM: shutdown() must escalate to SIGKILL and
        // the single reaper must reap it (no zombie, join succeeds).
        let cfg = sh_cfg("trap '' TERM; echo armed; sleep 30");
        let mut pty = Pty::spawn(&cfg).unwrap();
        assert!(
            pty.wait_for_contains("armed", std::time::Duration::from_secs(10)),
            "child armed"
        );
        let pid = pty.pid();
        let start = std::time::Instant::now();
        pty.shutdown();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "shutdown must escalate within the bound"
        );
        assert!(
            !group_alive(pid as libc::pid_t),
            "SIGKILL must have taken the group"
        );
    }

    #[test]
    fn idle_pty_reader_does_not_busy_poll_and_wakes_on_output() {
        // The reader blocks in read(2): an idle pty performs no periodic
        // wakeups. The child sleeps 1s then writes; the output must arrive
        // promptly after the write (a 2 ms-polling reader would also pass,
        // so the structural guarantee is asserted via shutdown promptness:
        // a blocking reader cannot be interrupted by polling, yet kill()
        // returns quickly because EOF wakes it).
        let cfg = sh_cfg("sleep 1; echo late; sleep 30");
        let mut pty = Pty::spawn(&cfg).unwrap();
        let start = std::time::Instant::now();
        // Idle for 600 ms (no reads from our side): nothing should stall.
        std::thread::sleep(std::time::Duration::from_millis(600));
        // Bounded, environment-independent: this only guards against a
        // pathological stall in the 600 ms idle window (the sleep itself is
        // the workload); 15 s cannot flake under CI/certificate load.
        assert!(start.elapsed() < std::time::Duration::from_secs(15));
        assert!(
            pty.wait_for_contains("late", std::time::Duration::from_secs(10)),
            "blocking reader wakes on data"
        );
        let t0 = std::time::Instant::now();
        pty.shutdown();
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(15),
            "kill path must wake the blocking reader via group EOF"
        );
    }

    #[test]
    fn spawn_errors_are_loud() {
        let mut cfg = sh_cfg("true");
        cfg.command = "/nonexistent-binary".into();
        assert!(Pty::spawn(&cfg).is_err());
        let err = Pty::spawn(&PtyConfig::default()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed);
    }

    /// Serializes process-environment mutation: libtest runs tests on
    /// parallel threads, so a test that installs authority inputs must be
    /// the only one touching them while its children spawn.
    static ENV_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Installs the named values and RESTORES the process environment on
    /// Drop: a panicking assertion must never leak test values into other
    /// tests or into the daemon's own environment.
    struct EnvOverride(Vec<(String, Option<std::ffi::OsString>)>);

    impl EnvOverride {
        fn new(entries: &[(&str, &str)]) -> Self {
            let saved = entries
                .iter()
                .map(|(name, _)| ((*name).to_string(), std::env::var_os(name)))
                .collect::<Vec<_>>();
            for (name, value) in entries {
                std::env::set_var(name, value);
            }
            Self(saved)
        }
    }

    impl Drop for EnvOverride {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn pty_env_uses_the_identical_authority_and_no_daemon_var_leaks() {
        // The exact terminal-side assertion, repeated through a REAL PTY:
        // PATH + the approved toolchain vars arrive; configured secret
        // names set in the parent never cross (even allowlisted explicitly);
        // an undeclared daemon var never arrives. Host-independent: the
        // approved values are UNIQUE per test (tempdir-backed, so parallel
        // cargo-test processes cannot collide), installed under the env
        // serial lock, and restored on Drop instead of relying on the
        // runner image.
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let cargo_home = home.path().join("cargo-home");
        let rustup_home = home.path().join("rustup-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        std::fs::create_dir_all(&rustup_home).unwrap();
        let cargo_home = cargo_home.display().to_string();
        let rustup_home = rustup_home.display().to_string();
        let _env = EnvOverride::new(&[
            ("CARGO_HOME", cargo_home.as_str()),
            ("RUSTUP_HOME", rustup_home.as_str()),
            ("FAKTOR_SERVER_PASSWORD", "hunter2"),
            ("OPENAI_API_KEY", "sk-pty-secret"),
            ("TEST_PRIVATE_SECRET", "private"),
            ("KP_PTY_UNDECLARED", "must-not-arrive"),
        ]);
        // The child PRINTS its env through the PTY: the assertions run on
        // the bytes the terminal actually delivered. The sentinel is the
        // last line, so waiting for it makes the snapshot complete — no
        // assertion can race the reader for a variable 'env' has not
        // delivered yet.
        let mut cfg = sh_cfg("env; echo __KP_PTY_ENV_END__");
        cfg.env = EnvSpec::toolchain();
        let mut pty = Pty::spawn(&cfg).unwrap();
        assert!(
            pty.wait_for_contains("__KP_PTY_ENV_END__", std::time::Duration::from_secs(10)),
            "PATH/toolchain vars must arrive: {:?}",
            String::from_utf8_lossy(&pty.snapshot())
        );
        let printed = String::from_utf8_lossy(&pty.snapshot()).into_owned();
        assert!(
            printed
                .lines()
                .any(|l| l.trim_end().starts_with("PATH=") && l.len() > "PATH=".len()),
            "PATH must be present and non-empty: {printed:?}"
        );
        for approved in [
            format!("CARGO_HOME={cargo_home}"),
            format!("RUSTUP_HOME={rustup_home}"),
        ] {
            assert!(
                printed.contains(&approved),
                "the authority must forward the approved name {approved}: {printed:?}"
            );
        }
        for secret in [
            "FAKTOR_SERVER_PASSWORD",
            "OPENAI_API_KEY",
            "TEST_PRIVATE_SECRET",
            "KP_PTY_UNDECLARED",
        ] {
            assert!(!printed.contains(secret), "{secret} leaked through the pty");
        }
        pty.kill();
        // Explicit entries cannot smuggle denied names either.
        let mut cfg = sh_cfg(
            "test -z \"$OPENAI_API_KEY\" && test -z \"$TEST_PRIVATE_SECRET\" \
             && echo pty-explicit-exact",
        );
        cfg.env = EnvSpec::Explicit(vec![
            ("OPENAI_API_KEY".into(), "leak".into()),
            ("TEST_PRIVATE_SECRET".into(), "leak".into()),
        ]);
        let mut pty = Pty::spawn(&cfg).unwrap();
        assert!(
            pty.wait_for_contains("pty-explicit-exact", std::time::Duration::from_secs(10)),
            "{:?}",
            String::from_utf8_lossy(&pty.snapshot())
        );
        pty.kill();
    }

    #[test]
    fn hostile_environment_variables_do_not_break_spawn() {
        // Hostile env specs never panic the spawn path: NUL-bearing explicit
        // entries are rejected pre-spawn as Malformed; a huge allowlist name
        // is dropped by resolve (no daemon value exists).
        let mut cfg = sh_cfg("echo ok");
        cfg.env = EnvSpec::Explicit(vec![("K\0EY".into(), "v".into())]);
        let err = Pty::spawn(&cfg).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed);
        let mut cfg = sh_cfg("echo ok");
        cfg.env = EnvSpec::Allowlisted(vec!["K\0EY".into()]);
        let err = Pty::spawn(&cfg).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed);
        let mut cfg = sh_cfg("echo ok");
        cfg.env = EnvSpec::Allowlisted(vec!["NOPE_NOT_SET".into()]);
        let mut pty = Pty::spawn(&cfg).unwrap();
        assert!(
            pty.wait_for_contains("ok", std::time::Duration::from_secs(10)),
            "child runs with a custom env"
        );
        pty.kill();
    }

    #[test]
    fn resize_with_zero_dimensions_is_rejected_before_any_ioctl() {
        let cfg = sh_cfg("true");
        let mut pty = Pty::spawn(&cfg).unwrap();
        assert_eq!(
            Pty::resize(&pty, 0, 80).unwrap_err().kind,
            ErrorKind::Malformed
        );
        assert_eq!(
            Pty::resize(&pty, 24, 0).unwrap_err().kind,
            ErrorKind::Malformed
        );
        assert_eq!(pty.size(), (24, 80));
        pty.kill();
    }

    #[test]
    fn nul_bytes_in_args_fail_validation_before_spawn() {
        // Previously this surfaced as a late io::Error mapped to NotFound;
        // pre-spawn validation must reject it as Malformed.
        let mut cfg = sh_cfg("true");
        cfg.args = vec!["-c".into(), "echo\0owned".into()];
        let err = Pty::spawn(&cfg).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed);
    }

    #[test]
    fn pty_child_outlives_a_transient_spawning_thread() {
        // Regression guard for Linux PR_SET_PDEATHSIG: the child is spawned
        // by the reader thread, which outlives it, so a short-lived CALLER
        // (a pooled spawn_blocking thread) exiting must never be mistaken
        // for daemon death and kill a healthy terminal.
        let cfg = sh_cfg("sleep 30");
        let mut pty = std::thread::spawn(move || Pty::spawn(&cfg).unwrap())
            .join()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(
            pty.is_alive(),
            "the spawning thread's exit is not daemon death"
        );
        pty.shutdown();
    }

    #[test]
    fn spawn_holds_a_guardian_released_only_after_teardown() {
        let cfg = sh_cfg("sleep 30");
        let mut pty = Pty::spawn(&cfg).unwrap();
        let guardian_pid = pty.guardian.as_ref().expect("guardian forked").pid();
        assert!(guardian_pid > 0);
        assert_eq!(
            unsafe { libc::kill(guardian_pid as libc::pid_t, 0) },
            0,
            "guardian is alive while the pty is"
        );
        pty.shutdown();
        assert!(pty.guardian.is_none(), "teardown releases the guardian");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while unsafe { libc::kill(guardian_pid as libc::pid_t, 0) } == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "guardian exits after the deliberate release"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn spawn_recorded_settles_the_durable_row_on_normal_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(TerminalLedger::open(dir.path()).unwrap());
        let cfg = sh_cfg("echo recorded; exit 0");
        let mut pty = Pty::spawn_recorded(&cfg, &ledger, "session:5/task:1").unwrap();
        assert!(pty.wait_for_contains("recorded", std::time::Duration::from_secs(10)));
        pty.shutdown();
        let report = ledger.reconcile().unwrap();
        assert_eq!(
            report.lost,
            vec![],
            "a cleanly reaped terminal is never reported lost"
        );
    }

    #[test]
    fn lost_reap_marker_from_teardown_is_consumed_by_reconcile() {
        // Adversarial (injected append failure): teardown's `mark_reaped`
        // fails, so the transition must live in the durable retry marker
        // until the next reconcile consumes it — never a silent discard and
        // never a false TerminalLost on the next daemon start.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(TerminalLedger::open(dir.path()).unwrap());
        let cfg = sh_cfg("sleep 30");
        let mut pty = Pty::spawn_recorded(&cfg, &ledger, "session:6/task:2").unwrap();
        ledger.fail_next_mark_reaped();
        pty.shutdown();
        let marker = ledger.path().with_file_name(REAP_MARKER_FILE);
        assert!(
            marker.exists(),
            "the lost reaped transition is compensated durably"
        );
        let report = ledger.reconcile().unwrap();
        assert_eq!(
            report.lost,
            vec![],
            "a compensated reap is never reported lost"
        );
        assert!(!marker.exists(), "the consumed marker is removed");
    }
}
