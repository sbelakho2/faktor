//! Daemon-death guardian for Unix PTY process groups (zero-orphans).
//!
//! A PTY child calls `setsid()`, so it leaves the daemon's process group and
//! no longer dies with it. `kill`/`Drop` cover the normal lifecycle, but a
//! daemon crash (SIGKILL, panic-abort, OOM kill) bypasses every destructor
//! and leaves the whole PTY session — including grandchildren — alive.
//!
//! The guardian is the cross-Unix fix: [`Pty`](crate::Pty) forks a tiny
//! syscall-only helper process that holds
//!
//! 1. the READ end of a control pipe whose WRITE end lives in the daemon,
//! 2. the PTY child's process group id, and
//! 3. a bounded identity record ([`ProcessIdentity`](crate::guardian::ProcessIdentity): pid + platform
//!    start-time marker, never a bare pid).
//!
//! The guardian blocks in `read(2)`. When the daemon dies — every copy of
//! the pipe's write end closes, including on SIGKILL — the read returns EOF
//! and the guardian verifies the recorded identity before acting:
//!
//! - identity matches (or the leader is gone while the group still has
//!   members) → `SIGKILL` the recorded process group, exit
//!   [`GUARDIAN_EXIT_KILLED`](crate::guardian::GUARDIAN_EXIT_KILLED);
//! - nothing left to kill → exit [`GUARDIAN_EXIT_NOTHING_TO_DO`](crate::guardian::GUARDIAN_EXIT_NOTHING_TO_DO) (this is
//!   the normal-shutdown path: the daemon killed and reaped the PTY before
//!   closing the pipe deliberately);
//! - the pid was recycled (start-time mismatch) → refuse and exit
//!   [`GUARDIAN_EXIT_REFUSED_RECYCLED`](crate::guardian::GUARDIAN_EXIT_REFUSED_RECYCLED); a stale identity never kills an
//!   unrelated process;
//! - the platform cannot verify start-time markers → refuse and exit
//!   [`GUARDIAN_EXIT_REFUSED_UNVERIFIABLE`](crate::guardian::GUARDIAN_EXIT_REFUSED_UNVERIFIABLE).
//!
//! Why `fork(2)` and not a re-exec of the host binary: this crate is a
//! library with no executable of its own, and a re-exec would require
//! touching the embedding binary's argv handling. The forked guardian runs
//! ONLY async-signal-safe raw syscalls (`setsid`, `close`, `open`, `dup2`,
//! `read`, `kill`, `_exit`, plus an allocation-free `/proc`/`proc_pidinfo`
//! start-time read), so it is safe to fork from the daemon's multithreaded
//! process and it can never deadlock on a lock another thread held at fork
//! time.
//!
//! [`TerminalLedger`](crate::guardian::TerminalLedger) persists the same identity records durably so a
//! restarted daemon can reconcile what it lost ([`TerminalLost`](crate::guardian::TerminalLost)) instead of
//! trusting a recycled pid. On Linux the PTY child additionally sets
//! `PR_SET_PDEATHSIG(SIGKILL)` (see `crate::unix`) as defense-in-depth; the
//! guardian is the mechanism on every Unix.

#![allow(unsafe_code)] // platform authority module: every unsafe
                       // block/function in this module carries a `// SAFETY:` justification and is
                       // enumerated by tests/static-authority.
use std::fmt;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use faktor_core::error::Error;

mod ledger;

#[cfg(all(unix, test))]
pub(crate) use ledger::REAP_MARKER_FILE;
pub use ledger::{
    Reconciliation, TerminalDisposition, TerminalLedger, TerminalLost, MAX_LEDGER_BYTES,
    MAX_LEDGER_LINES, MAX_OWNER_BYTES,
};

/// Exit status of the guardian process after a deliberate release or EOF.
/// The daemon's normal shutdown reaps the guardian and a test can assert the
/// action that was taken.
pub const GUARDIAN_EXIT_NOTHING_TO_DO: i32 = 0;
/// The guardian SIGKILLed the recorded process group.
pub const GUARDIAN_EXIT_KILLED: i32 = 10;
/// The guardian refused: the recorded pid now exists with a DIFFERENT
/// start-time marker (recycled pid) — nothing was signalled.
pub const GUARDIAN_EXIT_REFUSED_RECYCLED: i32 = 11;
/// The guardian refused: the platform cannot produce start-time markers, so
/// identity can never be proven — nothing was signalled.
pub const GUARDIAN_EXIT_REFUSED_UNVERIFIABLE: i32 = 12;
/// The guardian refused: the record is structurally impossible (pgid 0, or
/// pgid != pid — a PTY child always leaders its own group after setsid) and
/// could otherwise make `kill(0, ...)`/`kill(-pgid, ...)` hit an unrelated
/// group. Nothing was signalled.
pub const GUARDIAN_EXIT_REFUSED_MALFORMED: i32 = 13;
/// The guardian could not even set itself up (fd plumbing); nothing was
/// signalled. This is a loud failure, not a silent skip.
pub const GUARDIAN_EXIT_INTERNAL: i32 = 14;

/// Bounded identity of one protected process group leader.
///
/// `start_time` is the platform start-time marker captured when the PTY
/// child was spawned: Linux `/proc/<pid>/stat` field 22 (clock ticks since
/// boot), macOS `proc_pidinfo(PROC_PIDTBSDINFO)` `pbi_start_tvsec/tvusec`
/// packed. `None` means the marker could not be captured — such a record is
/// [`IdentityVerdict::Unverifiable`] and can never authorize a kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub pgid: u32,
    pub start_time: Option<u64>,
}

impl ProcessIdentity {
    /// Capture the identity of a live (or just-exited) process group leader.
    pub fn capture(pid: u32, pgid: u32) -> Self {
        Self {
            pid,
            pgid,
            start_time: process_start_time(pid),
        }
    }

    /// Re-probe the platform marker and classify this record.
    pub fn verify(&self) -> IdentityVerdict {
        if !probe_supported() {
            return IdentityVerdict::Unverifiable;
        }
        match process_start_time(self.pid) {
            None => IdentityVerdict::Gone,
            Some(now) => match self.start_time {
                Some(recorded) if recorded == now => IdentityVerdict::Match,
                Some(_) => IdentityVerdict::Mismatch,
                None => IdentityVerdict::Unverifiable,
            },
        }
    }
}

/// Classification of a recorded identity against the live process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityVerdict {
    /// The recorded pid exists and carries the recorded start-time marker.
    Match,
    /// The pid exists with a different start-time marker: it was recycled.
    /// Never signal on this verdict.
    Mismatch,
    /// No process carries the recorded pid.
    Gone,
    /// The marker cannot be compared (platform support missing, or the
    /// record has no marker but the pid is live).
    Unverifiable,
}

/// What the guardian must do when the control pipe reaches EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardianAction {
    /// SIGKILL the recorded process group.
    Kill,
    /// Nothing left to kill; exit cleanly.
    NothingToDo,
    /// Refuse: the pid was recycled.
    RefusedRecycled,
    /// Refuse: identity cannot be verified on this platform/record.
    RefusedUnverifiable,
    /// Refuse: the record is structurally impossible (`pgid == 0` would mean
    /// "the caller's own group"; a PTY child always has `pgid == pid`).
    RefusedMalformed,
}

impl GuardianAction {
    /// The guardian process exit status for this action.
    pub fn exit_code(self) -> i32 {
        match self {
            GuardianAction::Kill => GUARDIAN_EXIT_KILLED,
            GuardianAction::NothingToDo => GUARDIAN_EXIT_NOTHING_TO_DO,
            GuardianAction::RefusedRecycled => GUARDIAN_EXIT_REFUSED_RECYCLED,
            GuardianAction::RefusedUnverifiable => GUARDIAN_EXIT_REFUSED_UNVERIFIABLE,
            GuardianAction::RefusedMalformed => GUARDIAN_EXIT_REFUSED_MALFORMED,
        }
    }
}

/// Decide the EOF action from a recorded identity.
///
/// `Gone` still kills when the *group* exists: a process-group id cannot be
/// recycled while any member survives, and a new group carrying this id
/// would need a live leader with `pid == pgid` (which `Gone` rules out), so
/// a surviving group under a gone leader can only be our descendants.
pub fn decide_on_eof(identity: &ProcessIdentity) -> GuardianAction {
    if !plausible(identity) {
        // `kill(0, ...)` targets the caller's own group, a pgid beyond the
        // platform pid range cannot exist, and a non-leader pgid is not a
        // PTY record this crate ever produces: refuse instead of guessing.
        return GuardianAction::RefusedMalformed;
    }
    match identity.verify() {
        IdentityVerdict::Match => GuardianAction::Kill,
        IdentityVerdict::Mismatch => GuardianAction::RefusedRecycled,
        IdentityVerdict::Unverifiable => GuardianAction::RefusedUnverifiable,
        IdentityVerdict::Gone => {
            if group_exists(identity.pgid) {
                GuardianAction::Kill
            } else {
                GuardianAction::NothingToDo
            }
        }
    }
}

/// Whether this build can produce start-time markers at all.
pub const fn probe_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

/// Structurally plausible PTY-group record: a real pid/pgid (never 0, never
/// beyond the platform's pid range) whose group leader is the process
/// itself (`setsid`). Records that fail this are refused, never signalled.
pub(crate) fn plausible(identity: &ProcessIdentity) -> bool {
    identity.pid != 0 && identity.pgid == identity.pid && identity.pgid <= i32::MAX as u32
}

/// The platform start-time marker of `pid`, or `None` when the process does
/// not exist / the marker is unavailable. Allocation-free: safe to call in
/// the forked guardian.
pub fn process_start_time(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        linux_start_time(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_start_time(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Does any process still belong to `pgid`? `EPERM` counts as existing
/// (the signal would be refused, but the group is real). Out-of-range and 0
/// ids are never signalled (a `pgid` beyond the pid range must not wrap into
/// a negative/own-group signal).
pub fn group_exists(pgid: u32) -> bool {
    if pgid == 0 || pgid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: `pgid` was validated above (non-zero, within `pid_t` range),
    // so the negation cannot overflow; signal 0 only probes group existence
    // and never delivers a signal.
    let r = unsafe { libc::kill(-(pgid as libc::pid_t), 0) };
    if r == 0 {
        return true;
    }
    raw_errno() == libc::EPERM
}

#[cfg(target_os = "linux")]
fn linux_start_time(pid: u32) -> Option<u64> {
    // "/proc/<pid>/stat" with field 22 = starttime. `comm` may contain
    // spaces and parentheses, so parse from the LAST ')' (the kernel's own
    // documented parsing rule) and count whitespace-separated tokens from
    // there: state is field 3, starttime is the 20th token after ')'.
    let mut path = [0u8; 32];
    let prefix = b"/proc/";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut at = prefix.len();
    let mut digits = [0u8; 10];
    let mut n = 0usize;
    let mut v = pid;
    if v == 0 {
        digits[0] = b'0';
        n = 1;
    }
    while v > 0 {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        path[at] = digits[n];
        at += 1;
    }
    path[at..at + 5].copy_from_slice(b"/stat");

    // SAFETY: `path` is a NUL-terminated byte buffer built above from the
    // fixed `/proc/` prefix, decimal pid digits and `/stat` (no interior
    // NUL), and it outlives the call. `O_CLOEXEC` keeps the fd out of any
    // exec; a negative return is handled by the caller.
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 1024];
    // SAFETY: `fd` is a valid open descriptor (checked above) and `buf` is a
    // live stack array for exactly `buf.len()` bytes, so the kernel writes
    // stay inside the allocation; a non-positive return is rejected below.
    let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    // SAFETY: `fd` is owned by this function (returned by `open` above and
    // not used again), so closing it exactly once is required and cannot
    // double-close another thread's descriptor.
    unsafe {
        libc::close(fd);
    }
    if read <= 0 {
        return None;
    }
    let data = &buf[..read as usize];
    let close = data.iter().rposition(|&b| b == b')')?;
    let rest = &data[close + 1..];
    let mut idx = 0usize;
    let mut token = 0usize;
    while idx < rest.len() {
        while idx < rest.len() && rest[idx].is_ascii_whitespace() {
            idx += 1;
        }
        let start = idx;
        while idx < rest.len() && !rest[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx > start {
            token += 1;
            if token == 20 {
                let mut value: u64 = 0;
                for &b in &rest[start..idx] {
                    if !b.is_ascii_digit() {
                        return None;
                    }
                    value = value.wrapping_mul(10).wrapping_add((b - b'0') as u64);
                }
                return Some(value);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_start_time(pid: u32) -> Option<u64> {
    // SAFETY: `proc_bsdinfo` is a plain C POD struct whose all-zero bit
    // pattern is a valid initial value; the kernel fills it via
    // `proc_pidinfo` before any field is read (and the size check below
    // rejects a partial fill).
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a live, correctly sized `proc_bsdinfo` and `size`
    // reports exactly that size, so the kernel writes stay inside the
    // struct; `pid` is only a scalar argument that the kernel validates.
    let r = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if r < size {
        return None;
    }
    Some(
        info.pbi_start_tvsec
            .wrapping_mul(1_000_000)
            .wrapping_add(info.pbi_start_tvusec),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn raw_errno() -> i32 {
    #[cfg(target_os = "linux")]
    // SAFETY: `__errno_location` returns this thread's live errno slot
    // pointer for the whole thread lifetime; the immediate read cannot race
    // another thread (errno is thread-local), and the pointer is non-null.
    unsafe {
        *libc::__errno_location()
    }
    #[cfg(target_os = "macos")]
    // SAFETY: `__error` returns this thread's live errno slot pointer for the
    // whole thread lifetime; the immediate read cannot race another thread
    // (errno is thread-local), and the pointer is non-null.
    unsafe {
        *libc::__error()
    }
}

/// The daemon-side handle of ONE guardian process: owns the control pipe's
/// write end (closing it is the death signal) and the guardian pid.
///
/// Dropping the handle releases the guardian (close + bounded reap), so a
/// `Pty` that is dropped without an explicit shutdown can never leak a
/// guardian zombie.
pub struct GuardianHandle {
    pid: libc::pid_t,
    control: Option<OwnedFd>,
    reaped: bool,
}

impl fmt::Debug for GuardianHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardianHandle")
            .field("pid", &self.pid)
            .field("released", &self.control.is_none())
            .finish_non_exhaustive()
    }
}

/// Bound on the guardian reap after the control pipe closes: the guardian
/// performs a handful of syscalls, so anything past this means it is wedged
/// (SIGKILL + final blocking wait keeps shutdown bounded and zombie-free).
const GUARDIAN_REAP_MS: u64 = 500;

impl GuardianHandle {
    /// Create the control pipe and fork the guardian for `identity`.
    ///
    /// SAFETY (fork-only discipline): the child never touches heap, locks,
    /// or any Rust runtime state — it closes its inherited descriptors and
    /// runs `guardian_main` with raw syscalls only, then `_exit`s. Both pipe
    /// ends are `FD_CLOEXEC`, so no exec'd process (the PTY child, other
    /// supervised children) can inherit the pipe and mask daemon death.
    pub fn spawn(identity: ProcessIdentity) -> Result<Self, Error> {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a live two-element array; `pipe` writes exactly
        // both descriptors on success, and any non-zero return is refused
        // before the values are read.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(Error::internal("guardian pipe failed"));
        }
        for fd in fds {
            // SAFETY: `fd` is a descriptor returned by the successful `pipe`
            // above; `F_SETFD` with `FD_CLOEXEC` cannot invalidate it, and
            // the (ignored) failure only means another close-on-exec race we
            // do not rely on — fork below does not exec.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        // SAFETY: called from the runtime with no other guard: the child
        // branch (pid == 0) touches only the raw syscalls of `guardian_main`
        // — no allocation, locks, or Rust runtime state — then `_exit`s, so
        // the classic fork+threads hazards (deadlocks in malloc/at-fork
        // handlers) do not apply. A negative return is handled below.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            for fd in fds {
                // SAFETY: both descriptors belong to this failed spawn (the
                // fork did not happen), so each is closed exactly once here.
                unsafe {
                    libc::close(fd);
                }
            }
            return Err(Error::internal("guardian fork failed"));
        }
        if pid == 0 {
            // Guardian child: never runs another line of shared Rust state.
            // SAFETY: in the child, `fds[1]` (the write end) is a valid
            // inherited descriptor owned by this process; closing it is
            // async-signal-safe and required so the guardian cannot keep the
            // control pipe alive.
            unsafe {
                libc::close(fds[1]);
            }
            guardian_main(fds[0], identity);
        }
        // SAFETY: in the parent, `fds[0]` (the read end) is a valid
        // descriptor owned by this process; it is closed exactly once and
        // never used again (the child inherited its own copy).
        unsafe {
            libc::close(fds[0]);
        }
        Ok(Self {
            pid,
            // SAFETY: `fds[1]` is the pipe write end created above, still
            // open and owned by nothing else (the child closed its inherited
            // copy in its own address space); `OwnedFd` takes sole ownership
            // and closes it exactly once on drop.
            control: Some(unsafe { OwnedFd::from_raw_fd(fds[1]) }),
            reaped: false,
        })
    }

    /// The guardian process id (diagnostics/tests).
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// Has the control pipe already been closed (guardian released)?
    pub fn is_released(&self) -> bool {
        self.control.is_none()
    }

    /// Deliberate release: close the control pipe (the guardian probes and
    /// exits without killing whatever is already reaped), then make sure the
    /// guardian is gone within a bounded window. Returns the guardian's exit
    /// code when it was reaped as our child; `None` when it had already been
    /// reparented (its parent thread exited — init reaps it) or had to be
    /// SIGKILLed. Idempotent.
    pub fn release(&mut self) -> Option<i32> {
        self.control.take();
        if self.reaped || self.pid <= 0 {
            return None;
        }
        let deadline = Instant::now() + Duration::from_millis(GUARDIAN_REAP_MS);
        loop {
            let mut status = 0;
            // SAFETY: `self.pid` is our own forked child (fork returned it
            // and no wait has consumed it: `reaped` guards re-entry), and
            // `status` is a live stack slot. WNOHANG never blocks.
            let r = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if r == self.pid {
                self.reaped = true;
                return exit_code_of(status);
            }
            if r < 0 && raw_errno() == libc::ECHILD {
                // Reparented (the forking thread exited): waitpid can never
                // observe it again. Poll liveness instead, but NEVER signal:
                // once it is not our child, a live pid here could in
                // principle be a recycled id.
                self.reaped = true;
                self.poll_reparented_gone();
                return None;
            }
            if Instant::now() >= deadline {
                // Still our child after the bound: a wedged guardian is
                // killed and reaped (safe — waitpid proved it is ours).
                // SAFETY: the preceding waitpid did not report ECHILD, so
                // `self.pid` is still our child (not a recycled pid); SIGKILL
                // to a real child cannot affect any other process.
                unsafe {
                    libc::kill(self.pid, libc::SIGKILL);
                }
                let mut status = 0;
                // SAFETY: `self.pid` is still our child (just signaled) and
                // `status` is a live stack slot; the blocking wait reaps it
                // so no zombie is left behind.
                let _ = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                self.reaped = true;
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Bounded liveness poll for a reparented guardian; never signals.
    fn poll_reparented_gone(&self) {
        let deadline = Instant::now() + Duration::from_millis(GUARDIAN_REAP_MS);
        // SAFETY: signal 0 only probes liveness and never delivers a signal;
        // a zero return means the (possibly recycled) id exists, which only
        // extends the bounded poll — no kill is ever issued here.
        while unsafe { libc::kill(self.pid, 0) } == 0 {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for GuardianHandle {
    fn drop(&mut self) {
        self.release();
    }
}

fn exit_code_of(status: i32) -> Option<i32> {
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        None
    }
}

/// The guardian process body. MUST NOT return: it is reached only through
/// `fork()` from a possibly multithreaded process, so it may call raw
/// syscalls only (no allocation, no locks, no Rust runtime).
fn guardian_main(read_fd: RawFd, identity: ProcessIdentity) -> ! {
    // SAFETY (single raw-syscall block): this body is reachable ONLY in the
    // post-fork child, which inherits no locks and uses no allocation, so the
    // process is effectively single-threaded and no syscall here can
    // deadlock on shared Rust state. `read_fd` is the control pipe's read end
    // inherited across fork (valid by construction); every other fd/literal
    // argument is either produced by a checked syscall inside this block or a
    // constant, and every result is checked before use — failure paths
    // `_exit` instead of unwinding.
    // SAFETY: the arguments were validated by the caller per this function's documented contract and the call has no additional aliasing or lifetime requirements.
    unsafe {
        // Detach from the daemon's session/process group: a group signal
        // aimed at the daemon must not take the guardian down before it can
        // enforce the kill.
        libc::setsid();

        // Keep the control fd above stdio (it may have been fd 0..2 if the
        // daemon closed its own stdio) and re-point stdio at /dev/null so
        // the guardian never holds a daemon pipe open.
        let control = if read_fd <= 2 {
            let moved = libc::fcntl(read_fd, libc::F_DUPFD, 3);
            if moved < 0 {
                libc::_exit(GUARDIAN_EXIT_INTERNAL);
            }
            libc::close(read_fd);
            moved
        } else {
            read_fd
        };
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
        // Close every other inherited descriptor. Fork (not exec) means the
        // kernel does not do this for us; a leaked write end of ANOTHER
        // pty's control pipe would mask that daemon's death.
        let open_max = libc::sysconf(libc::_SC_OPEN_MAX);
        let max: libc::c_int = if open_max > 0 {
            open_max.min(65_536) as libc::c_int
        } else {
            4096
        };
        let mut fd: libc::c_int = 3;
        while fd < max {
            if fd != control {
                libc::close(fd);
            }
            fd += 1;
        }

        // Block until the daemon dies (all write ends closed) or releases
        // deliberately. Stray bytes are ignored: the protocol is EOF.
        let mut buf = [0u8; 16];
        loop {
            let n = libc::read(control, buf.as_mut_ptr().cast(), buf.len());
            if n < 0 && raw_errno() == libc::EINTR {
                continue;
            }
            break;
        }
        libc::close(control);

        // EOF: verify identity before ever signalling, then act.
        let action = decide_on_eof(&identity);
        if action == GuardianAction::Kill {
            libc::kill(-(identity.pgid as libc::pid_t), libc::SIGKILL);
        }
        libc::_exit(action.exit_code());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    fn group_alive(pgid: u32) -> bool {
        group_exists(pgid)
    }

    /// A single-process process group (`process_group(0)` ⇒ pgid == pid).
    fn spawn_group_sleeper() -> (Child, ProcessIdentity) {
        let child = Command::new("sh")
            .args(["-c", "sleep 300"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleeper");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid, pid);
        (child, identity)
    }

    fn wait_group_gone(pgid: u32, bound: Duration) {
        let deadline = Instant::now() + bound;
        while group_alive(pgid) {
            assert!(
                Instant::now() < deadline,
                "group {pgid} must die within {bound:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn probe_finds_this_process_and_recomputes_the_same_marker() {
        if !probe_supported() {
            return;
        }
        let pid = std::process::id();
        let first = process_start_time(pid).expect("own start time");
        let second = process_start_time(pid).expect("own start time (second probe)");
        assert_eq!(first, second, "start marker is stable for a live process");
        let identity = ProcessIdentity::capture(pid, pid);
        assert_eq!(identity.verify(), IdentityVerdict::Match);
    }

    #[test]
    fn probe_of_a_reaped_process_is_gone_and_a_bogus_marker_mismatches() {
        if !probe_supported() {
            return;
        }
        let mut child = Command::new("sh")
            .args(["-c", "sleep 300"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleeper");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid, pid);
        assert!(identity.start_time.is_some(), "captured while alive");
        // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        child.wait().expect("reap sleeper");
        // Reaped: the pid may be reused by another process in principle, so
        // accept Gone or a same-pid different-marker Mismatch. Never Match.
        let verdict = identity.verify();
        assert_ne!(verdict, IdentityVerdict::Match);
    }

    #[test]
    fn decide_refuses_a_recycled_pid_record() {
        if !probe_supported() {
            return;
        }
        let pid = std::process::id();
        let live = ProcessIdentity::capture(pid, pid);
        let stale = ProcessIdentity {
            pid: live.pid,
            pgid: live.pgid,
            start_time: live.start_time.map(|t| t.wrapping_add(1)),
        };
        assert_eq!(stale.verify(), IdentityVerdict::Mismatch);
        assert_eq!(decide_on_eof(&stale), GuardianAction::RefusedRecycled);
        assert_eq!(
            decide_on_eof(&stale).exit_code(),
            GUARDIAN_EXIT_REFUSED_RECYCLED
        );
    }

    #[test]
    fn decide_refuses_a_record_without_a_marker_on_a_supported_platform() {
        if !probe_supported() {
            return;
        }
        let pid = std::process::id();
        let no_marker = ProcessIdentity {
            pid,
            pgid: pid,
            start_time: None,
        };
        assert_eq!(no_marker.verify(), IdentityVerdict::Unverifiable);
        assert_eq!(
            decide_on_eof(&no_marker),
            GuardianAction::RefusedUnverifiable
        );
    }

    #[test]
    fn decide_reports_nothing_to_do_when_the_group_is_gone() {
        if !probe_supported() {
            return;
        }
        let mut child = Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid, pid);
        child.wait().expect("reap true");
        std::thread::sleep(Duration::from_millis(20));
        assert!(!group_alive(pid), "no members survive a reaped leader");
        assert_eq!(decide_on_eof(&identity), GuardianAction::NothingToDo);
    }

    #[test]
    fn eof_kills_the_recorded_group_and_reports_killed() {
        let (mut child, identity) = spawn_group_sleeper();
        assert_eq!(identity.verify(), IdentityVerdict::Match);
        let mut guardian = GuardianHandle::spawn(identity).expect("fork guardian");
        let gpid = guardian.pid();
        let code = guardian.release();
        assert_eq!(
            code,
            Some(GUARDIAN_EXIT_KILLED),
            "guardian must SIGKILL the group at EOF"
        );
        let _ = child.wait();
        wait_group_gone(identity.pgid, Duration::from_secs(5));
        let mut status = 0;
        // SAFETY: the pid is this module's own child (or already reaped, reported as ECHILD) and `status` is a live stack local.
        unsafe {
            assert_eq!(libc::waitpid(gpid as libc::pid_t, &mut status, 0), -1);
            assert_eq!(raw_errno(), libc::ECHILD, "guardian was reaped");
        }
    }

    #[test]
    fn eof_kills_surviving_descendants_when_the_leader_is_already_reaped() {
        // The leader exits immediately; the background `sleep` stays in the
        // leader's process group. The identity says "gone" but the group is
        // alive: the guardian must kill the remaining members.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 300 & exit 0"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        let identity = ProcessIdentity::capture(pid, pid);
        assert!(identity.start_time.is_some());
        child.wait().expect("reap the leader");
        assert!(group_alive(pid), "the grandchild keeps the group alive");
        let mut guardian = GuardianHandle::spawn(identity).expect("fork guardian");
        assert_eq!(guardian.release(), Some(GUARDIAN_EXIT_KILLED));
        wait_group_gone(pid, Duration::from_secs(5));
    }

    #[test]
    fn eof_refuses_a_recycled_pid_and_leaves_the_process_alive() {
        let (mut child, identity) = spawn_group_sleeper();
        let stale = ProcessIdentity {
            pid: identity.pid,
            pgid: identity.pgid,
            start_time: identity.start_time.map(|t| t.wrapping_add(1)),
        };
        let mut guardian = GuardianHandle::spawn(stale).expect("fork guardian");
        assert_eq!(guardian.release(), Some(GUARDIAN_EXIT_REFUSED_RECYCLED));
        assert!(
            group_alive(identity.pgid),
            "a recycled-pid record must never authorize a kill"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn deliberate_release_after_reap_exits_without_killing() {
        // Normal shutdown: the daemon already killed and reaped the child,
        // THEN closes the pipe. The guardian must exit cleanly (code 0).
        let (mut child, identity) = spawn_group_sleeper();
        let mut guardian = GuardianHandle::spawn(identity).expect("fork guardian");
        // SAFETY: the pid/pgid was validated non-zero and is owned by this module (or signal 0 only probes existence); no signal is sent to an unproven target.
        unsafe {
            libc::kill(-(identity.pgid as libc::pid_t), libc::SIGKILL);
        }
        child.wait().expect("reap the killed child");
        assert!(!group_alive(identity.pgid));
        assert_eq!(
            guardian.release(),
            Some(GUARDIAN_EXIT_NOTHING_TO_DO),
            "deliberate close after the group was reaped kills nothing"
        );
        assert!(guardian.is_released());
    }

    #[test]
    fn double_release_is_idempotent_and_never_waits_twice() {
        let (mut child, identity) = spawn_group_sleeper();
        let mut guardian = GuardianHandle::spawn(identity).expect("fork guardian");
        let first = guardian.release();
        let second = guardian.release();
        assert_eq!(first, Some(GUARDIAN_EXIT_KILLED));
        assert_eq!(second, None);
        let _ = child.wait();
    }

    #[test]
    fn hostile_identity_never_signals_group_zero() {
        // pgid 0 would mean "the caller's own group": a corrupt/hostile
        // record must never be able to authorize it.
        let hostile = ProcessIdentity {
            pid: std::process::id(),
            pgid: 0,
            start_time: process_start_time(std::process::id()),
        };
        assert!(!group_exists(0));
        assert_eq!(decide_on_eof(&hostile), GuardianAction::RefusedMalformed);
        // A pid/pgid split (not a setsid leader) is refused too.
        let split = ProcessIdentity {
            pid: std::process::id(),
            pgid: std::process::id() + 1,
            start_time: process_start_time(std::process::id()),
        };
        assert_eq!(decide_on_eof(&split), GuardianAction::RefusedMalformed);
        // An out-of-range pgid must never wrap into a negative (own-group)
        // signal, and group_exists must not negate i32::MIN.
        let huge = ProcessIdentity {
            pid: u32::MAX,
            pgid: u32::MAX,
            start_time: Some(1),
        };
        assert_eq!(decide_on_eof(&huge), GuardianAction::RefusedMalformed);
        assert!(!group_exists(u32::MAX));
        assert!(!group_exists(0));
    }
}
