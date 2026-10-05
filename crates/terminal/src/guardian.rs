//! Daemon-death guardian for supervised process groups (Linux).
//!
//! [`ProcessSupervisor`](crate::ProcessSupervisor) children are spawned into
//! their own process group (`process_group(0)`), so a daemon that dies
//! without running a destructor (SIGKILL, panic-abort, OOM kill) cannot reap
//! them: the direct child is reparented and its descendants — the `sleep &`
//! background job of a shell — survive forever. The guardian is the
//! kernel-mediated fix:
//!
//! * at spawn the daemon `fork(2)`s a tiny guardian and creates a control
//!   pipe with `pipe2(O_CLOEXEC)`;
//! * the WRITE end lives in the daemon (the registry row); the guardian owns
//!   the READ end plus a start-time-verified [`ProcessIdentity`] of the group
//!   leader;
//! * the guardian waits in `poll(2)`. When every write end closes — the
//!   kernel does that on SIGKILL exactly like a deliberate release — it
//!   re-verifies the recorded identity and on a match SIGTERMs the process
//!   group, waits a bounded grace, then SIGKILLs whatever remains. A record
//!   whose pid was recycled (start-time mismatch) or cannot be verified is
//!   refused: an unrelated group is never signalled;
//! * while the daemon lives, a poll tick that finds the whole recorded group
//!   gone exits the guardian on its own, so guardians do not accumulate for
//!   finished commands (the row still owns the control pipe until reap).
//!
//! On Linux the direct child additionally arms `PR_SET_PDEATHSIG(SIGKILL)`
//! in its pre-exec hook ([`install_pdeathsig`]), with a `getppid` check that
//! refuses the exec if the daemon died between fork and the hook; the
//! guardian is the primary authority and covers descendants the direct
//! child may already have created.
//!
//! Reaping: the daemon stays the direct child's parent, so its reaper
//! consumes the leader; daemon death reparents the leader and init reaps it.
//! The guardian's contract is the whole-group kill, never `waitpid` on a
//! process that was never its child.
#![allow(unsafe_code)] // platform authority module: every unsafe
                       // block/function in this module carries a `// SAFETY:` justification and is
                       // enumerated by tests/static-authority.

#[cfg(target_os = "linux")]
mod imp {
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    use std::time::{Duration, Instant};

    use faktor_core::error::Error;

    /// Exit status of the guardian after a deliberate release or owner death.
    pub(crate) const GUARDIAN_EXIT_NOTHING_TO_DO: i32 = 0;
    /// The guardian SIGTERM→SIGKILLed the recorded process group.
    pub(crate) const GUARDIAN_EXIT_KILLED: i32 = 10;
    /// Refused: the recorded pid exists with a DIFFERENT start-time marker
    /// (recycled pid) — nothing was signalled.
    pub(crate) const GUARDIAN_EXIT_REFUSED_RECYCLED: i32 = 11;
    /// Refused: no start-time marker could be produced — nothing signalled.
    pub(crate) const GUARDIAN_EXIT_REFUSED_UNVERIFIABLE: i32 = 12;
    /// Refused: the record is structurally impossible (`pgid == 0`, or
    /// `pgid != pid` for a supervisor-spawned group leader) — nothing
    /// signalled.
    pub(crate) const GUARDIAN_EXIT_REFUSED_MALFORMED: i32 = 13;
    /// The guardian could not set itself up (fd plumbing); nothing signalled.
    pub(crate) const GUARDIAN_EXIT_INTERNAL: i32 = 14;

    /// Poll tick of the wait loop (ms): a wakeup that finds the whole group
    /// gone exits the guardian instead of waiting for a release that may
    /// never come.
    const GUARDIAN_POLL_MS: libc::c_int = 200;
    /// SIGTERM→SIGKILL grace of the owner-death kill (ms).
    const TERM_GRACE_MS: u64 = 500;
    /// Bound on the daemon-side release wait for one guardian.
    const RELEASE_BOUND_MS: u64 = 2000;

    /// Bounded identity of one protected process group leader.
    ///
    /// `start_time` is Linux `/proc/<pid>/stat` field 22 (clock ticks since
    /// boot), captured while the leader is unreaped (a zombie still has a
    /// `/proc` entry). `None` means the marker could not be captured; such a
    /// record verifies [`IdentityVerdict::Unverifiable`] and can never
    /// authorize a kill.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct ProcessIdentity {
        pub pid: u32,
        pub pgid: u32,
        pub start_time: Option<u64>,
    }

    impl ProcessIdentity {
        /// Capture the identity of a just-spawned group leader
        /// (`process_group(0)` ⇒ `pgid == pid`).
        pub(crate) fn capture(pid: u32, pgid: u32) -> Self {
            Self {
                pid,
                pgid,
                start_time: process_start_time(pid),
            }
        }

        /// Re-probe the start-time marker and classify this record.
        pub(crate) fn verify(&self) -> IdentityVerdict {
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
    pub(crate) enum IdentityVerdict {
        Match,
        Mismatch,
        Gone,
        Unverifiable,
    }

    /// What the guardian does when the control pipe reaches EOF.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum GuardianAction {
        Kill,
        NothingToDo,
        RefusedRecycled,
        RefusedUnverifiable,
        RefusedMalformed,
    }

    impl GuardianAction {
        fn exit_code(self) -> i32 {
            match self {
                GuardianAction::Kill => GUARDIAN_EXIT_KILLED,
                GuardianAction::NothingToDo => GUARDIAN_EXIT_NOTHING_TO_DO,
                GuardianAction::RefusedRecycled => GUARDIAN_EXIT_REFUSED_RECYCLED,
                GuardianAction::RefusedUnverifiable => GUARDIAN_EXIT_REFUSED_UNVERIFIABLE,
                GuardianAction::RefusedMalformed => GUARDIAN_EXIT_REFUSED_MALFORMED,
            }
        }
    }

    /// Decide the EOF action. `Gone` still kills when the GROUP exists: a
    /// process-group id cannot be recycled while a member survives, and a
    /// recycled group would need a live leader with `pid == pgid` (which
    /// `Gone` rules out), so a surviving group under a gone leader can only
    /// be our reparented descendants.
    fn decide_on_eof(identity: &ProcessIdentity) -> GuardianAction {
        if !plausible(identity) {
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

    /// Structurally plausible supervised-group record: a real pid whose
    /// group leader is the process itself (never 0, never beyond `pid_t`).
    fn plausible(identity: &ProcessIdentity) -> bool {
        identity.pid != 0 && identity.pgid == identity.pid && identity.pgid <= i32::MAX as u32
    }

    /// The Linux start-time marker of `pid`, or `None` when the process does
    /// not exist. Allocation-free: safe to call in the forked guardian.
    pub(crate) fn process_start_time(pid: u32) -> Option<u64> {
        if pid == 0 {
            return None;
        }
        let mut path = [0u8; 32];
        let prefix = b"/proc/";
        path[..prefix.len()].copy_from_slice(prefix);
        let mut at = prefix.len();
        let mut digits = [0u8; 10];
        let mut n = 0usize;
        let mut v = pid;
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
        // NUL) and it outlives the call; `O_CLOEXEC` keeps the fd out of any
        // exec, and a negative return is handled below.
        let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let mut buf = [0u8; 1024];
        // SAFETY: `fd` is a valid open descriptor (checked above) and `buf`
        // is a live stack array of exactly `buf.len()` bytes, so the kernel
        // write stays inside the allocation; a non-positive return is
        // rejected below.
        let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        // SAFETY: `fd` is owned by this function (returned by `open` above
        // and never used again), so closing it exactly once cannot
        // double-close another thread's descriptor.
        unsafe {
            libc::close(fd);
        }
        if read <= 0 {
            return None;
        }
        let data = &buf[..read as usize];
        // `comm` may contain spaces and parentheses: parse from the LAST
        // ')' (the kernel's documented rule); starttime is the 20th token
        // after it.
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

    /// Does any process (live or zombie) still belong to `pgid`? `EPERM`
    /// counts as existing; 0 and out-of-range ids are never probed.
    fn group_exists(pgid: u32) -> bool {
        if pgid == 0 || pgid > i32::MAX as u32 {
            return false;
        }
        // SAFETY: `pgid` was validated above (non-zero, within `pid_t`
        // range), so the negation cannot overflow; signal 0 only probes
        // group existence and never delivers a signal.
        let r = unsafe { libc::kill(-(pgid as libc::pid_t), 0) };
        if r == 0 {
            return true;
        }
        raw_errno() == libc::EPERM
    }

    fn raw_errno() -> i32 {
        // SAFETY: `__errno_location` returns this thread's live errno slot
        // pointer for the whole thread lifetime; the immediate read cannot
        // race another thread (errno is thread-local) and the pointer is
        // non-null.
        unsafe { *libc::__errno_location() }
    }

    /// Install `PR_SET_PDEATHSIG(SIGKILL)` on a supervisor child before
    /// `exec` (defense in depth behind the guardian). The pre-exec `getppid`
    /// check closes the fork→prctl race: a parent that died in that window
    /// can never deliver the signal, so the child refuses to exec instead of
    /// running unguarded.
    pub(crate) fn install_pdeathsig(cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: the closure runs post-fork/pre-exec and calls only
        // async-signal-safe syscalls (`getppid`, `prctl`); it allocates
        // nothing and takes no locks, so it cannot deadlock the multi-
        // threaded daemon at fork time.
        unsafe {
            cmd.pre_exec(move || {
                if libc::getppid() != parent {
                    return Err(std::io::Error::other(
                        "supervisor exited before exec; refusing an unguarded child",
                    ));
                }
                // Best-effort: a kernel/seccomp refusal is covered by the
                // forked guardian, so it must not fail the spawn.
                let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
                Ok(())
            });
        }
    }

    /// The daemon-side handle of ONE guardian: owns the control pipe's write
    /// end (closing it is the death signal) and the guardian pid. Dropping
    /// the handle releases the guardian (close + bounded reap), so a row
    /// that is collected can never leak a guardian zombie.
    pub(crate) struct GuardianHandle {
        pid: libc::pid_t,
        control: Option<OwnedFd>,
        reaped: bool,
    }

    impl std::fmt::Debug for GuardianHandle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GuardianHandle")
                .field("pid", &self.pid)
                .field("released", &self.control.is_none())
                .finish_non_exhaustive()
        }
    }

    impl GuardianHandle {
        /// Fork the daemon-death guardian for the group led by `pid`.
        ///
        /// Returns `Ok(None)` only where no backend exists (never on Linux);
        /// `Err` means the spawn must be refused: the caller kills and reaps
        /// the child rather than exposing it unguarded.
        pub(crate) fn try_spawn(pid: u32) -> Result<Option<Self>, Error> {
            if pid == 0 {
                return Err(Error::internal(
                    "guardian: refusing to guard a zero child pid",
                ));
            }
            let identity = ProcessIdentity::capture(pid, pid);
            if !plausible(&identity) {
                return Err(Error::internal(format!(
                    "guardian: child {pid} is not its own process group leader; refusing to \
                     expose it unguarded"
                )));
            }
            if identity.start_time.is_none() {
                // The leader was already consumed by a racing reaper before
                // the marker could be captured: the guardian still exists,
                // but it can only refuse on EOF (it must never signal an
                // unverified group). Recorded loudly, never silently claimed.
                tracing::warn!(
                    pid,
                    "guardian: start-time marker unavailable at spawn; the daemon-death kill \
                     for this child cannot be identity-verified"
                );
            }
            Ok(Some(Self::spawn_with_identity(identity)?))
        }

        /// Fork a guardian over an explicit identity (tests exercise the
        /// refusal verdicts through this seam).
        pub(crate) fn spawn_with_identity(identity: ProcessIdentity) -> Result<Self, Error> {
            if !plausible(&identity) {
                return Err(Error::internal(
                    "guardian: refusing a structurally impossible identity record",
                ));
            }
            let mut fds = [0 as RawFd; 2];
            // SAFETY: `fds` is a live two-element array; `pipe2` writes both
            // descriptors on success and returns non-zero on failure (checked
            // before any read); `O_CLOEXEC` is applied atomically by the
            // kernel, so no concurrent fork/exec can inherit either end.
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                return Err(Error::internal(format!(
                    "guardian control pipe2 failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            // SAFETY: called from the daemon with no other guard required:
            // the child branch (pid == 0) touches only the raw syscalls of
            // `guardian_main` — no allocation, locks, or Rust runtime state
            // — then `_exit`s, so the classic fork+threads hazards do not
            // apply; a negative return is handled below.
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                // SAFETY: both descriptors belong to this failed spawn (the
                // fork did not happen), so each is closed exactly once.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(Error::internal(format!(
                    "guardian fork failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if pid == 0 {
                // SAFETY: in the child, `fds[1]` (the write end) is a valid
                // inherited descriptor owned by this process; closing it is
                // async-signal-safe and required so the guardian cannot keep
                // its own control pipe alive.
                unsafe {
                    libc::close(fds[1]);
                }
                guardian_main(fds[0], identity);
            }
            // SAFETY: in the parent, `fds[0]` (the read end) is a valid
            // descriptor owned by this process; it is closed exactly once
            // and never used again (the child inherited its own copy).
            unsafe {
                libc::close(fds[0]);
            }
            Ok(Self {
                pid,
                // SAFETY: `fds[1]` is the pipe write end created above,
                // still open and owned by nothing else (the child closed its
                // inherited copy in its own address space); `OwnedFd` takes
                // sole ownership and closes it exactly once on drop.
                control: Some(unsafe { OwnedFd::from_raw_fd(fds[1]) }),
                reaped: false,
            })
        }

        /// The guardian process id (diagnostics/tests).
        #[cfg(test)]
        pub(crate) fn pid(&self) -> u32 {
            self.pid as u32
        }

        /// Deliberate release: close the control pipe (EOF — the guardian
        /// verifies identity and kills what remains), then make sure the
        /// guardian is gone within a bounded window. Returns the guardian's
        /// exit code when it was reaped as our child; `None` when it had
        /// already been reparented (init reaps it) or had to be SIGKILLed.
        /// Idempotent. Dropping the handle calls this.
        pub(crate) fn release(&mut self) -> Option<i32> {
            self.control.take();
            if self.reaped || self.pid <= 0 {
                return None;
            }
            let deadline = Instant::now() + Duration::from_millis(RELEASE_BOUND_MS);
            loop {
                let mut status: libc::c_int = 0;
                // SAFETY: `self.pid` is our own forked child (fork returned
                // it and no wait has consumed it: `reaped` guards re-entry),
                // and `status` is a live stack slot; WNOHANG never blocks.
                let r = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
                if r == self.pid {
                    self.reaped = true;
                    return exit_code_of(status);
                }
                if r < 0 && raw_errno() == libc::ECHILD {
                    // Reparented (the forking thread exited): waitpid can
                    // never observe it again. Poll liveness only, NEVER
                    // signal: a live pid here could in principle be a
                    // recycled id.
                    self.reaped = true;
                    self.poll_reparented_gone();
                    return None;
                }
                if Instant::now() >= deadline {
                    // SAFETY: the preceding waitpid did not report ECHILD,
                    // so `self.pid` is still our child (not a recycled pid);
                    // SIGKILL to a real child cannot affect any other
                    // process. The blocking wait reaps it so no zombie is
                    // left behind.
                    unsafe {
                        libc::kill(self.pid, libc::SIGKILL);
                        let mut status: libc::c_int = 0;
                        let _ = libc::waitpid(self.pid, &mut status, 0);
                    }
                    self.reaped = true;
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// Bounded liveness poll for a reparented guardian; never signals.
        fn poll_reparented_gone(&self) {
            let deadline = Instant::now() + Duration::from_millis(RELEASE_BOUND_MS);
            // SAFETY: signal 0 only probes liveness and never delivers a
            // signal; a zero return just re-polls until the bound.
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

    fn exit_code_of(status: libc::c_int) -> Option<i32> {
        if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else {
            None
        }
    }

    /// The guardian process body. MUST NOT return: it is reached only
    /// through `fork()` from a possibly multithreaded process, so it calls
    /// raw syscalls only (no allocation, no locks, no Rust runtime).
    fn guardian_main(read_fd: RawFd, identity: ProcessIdentity) -> ! {
        // SAFETY: (single raw-syscall block) this body is reachable ONLY in
        // the post-fork child, which inherits no locks and performs no
        // allocation, so no syscall here can deadlock on shared Rust state.
        // `read_fd` is the control pipe's read end inherited across fork
        // (valid by construction); every other argument is a checked
        // constant, and every result is checked — failure paths `_exit`
        // instead of unwinding.
        unsafe {
            // Detach from the daemon's session/process group: a group signal
            // aimed at the daemon must not take the guardian down before it
            // can enforce the kill.
            libc::setsid();

            // Keep the control fd above stdio and re-point stdio at
            // /dev/null so the guardian never holds a daemon pipe open.
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
            // Close every other inherited descriptor. Fork (not exec) means
            // the kernel does not do this for us; a leaked write end of
            // ANOTHER child's control pipe would mask that child's death.
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

            // Wait for the control pipe. A poll tick that finds the whole
            // recorded group gone exits on its own (no guardian accumulates
            // for a finished command); EOF — daemon SIGKILL, crash or
            // deliberate release — is the authority path.
            loop {
                let mut pfd = libc::pollfd {
                    fd: control,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let r = libc::poll(&mut pfd, 1, GUARDIAN_POLL_MS);
                if r < 0 {
                    if raw_errno() == libc::EINTR {
                        continue;
                    }
                    break;
                }
                if r == 0 {
                    if !group_exists(identity.pgid) {
                        libc::_exit(GUARDIAN_EXIT_NOTHING_TO_DO);
                    }
                    continue;
                }
                let mut buf = [0u8; 16];
                let n = loop {
                    let n = libc::read(control, buf.as_mut_ptr().cast(), buf.len());
                    if n < 0 && raw_errno() == libc::EINTR {
                        continue;
                    }
                    break n;
                };
                if n <= 0 {
                    break; // EOF: the owner is gone (or deliberately released)
                }
                // Stray bytes are ignored: the protocol is EOF.
            }
            libc::close(control);

            let action = decide_on_eof(&identity);
            if action == GuardianAction::Kill {
                kill_group_term_then_kill(identity.pgid);
            }
            libc::_exit(action.exit_code());
        }
    }

    /// SIGTERM the recorded group, wait the bounded grace, then SIGKILL
    /// whatever remains. `pgid` was validated by [`plausible`] before the
    /// guardian was forked.
    fn kill_group_term_then_kill(pgid: u32) {
        // SAFETY: `pgid` is a plausible, non-zero, in-range group id of the
        // group this guardian was forked to protect, so the negated id
        // cannot overflow and addresses exactly that group; `nanosleep` is
        // async-signal-safe and writes to no memory through a null remainder
        // pointer.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGTERM);
        }
        let step = libc::timespec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        };
        let mut waited_ms: u64 = 0;
        while waited_ms < TERM_GRACE_MS {
            if !group_exists(pgid) {
                return;
            }
            // SAFETY: `step` is a live initialized `timespec`; the null
            // remainder pointer is explicitly allowed by `nanosleep`.
            unsafe {
                libc::nanosleep(&step, std::ptr::null_mut());
            }
            waited_ms += 10;
        }
        // SAFETY: same validated group id as the SIGTERM above; SIGKILL is
        // the documented bounded escalation.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::{Command, Stdio};

        /// Capture→verify round trip on a live process, and the forged
        /// marker half of the recycled-pid refusal.
        #[test]
        fn capture_matches_self_and_forged_marker_mismatches() {
            let identity = ProcessIdentity::capture(std::process::id(), std::process::id());
            assert_eq!(identity.verify(), IdentityVerdict::Match);
            let forged = ProcessIdentity {
                start_time: identity.start_time.map(|t| t.wrapping_add(1)),
                ..identity
            };
            assert_eq!(forged.verify(), IdentityVerdict::Mismatch);
            assert_eq!(
                decide_on_eof(&forged).exit_code(),
                GUARDIAN_EXIT_REFUSED_RECYCLED
            );
        }

        /// A record without a marker can never authorize a kill, and a
        /// structurally impossible record is refused before any fork.
        #[test]
        fn unverifiable_and_malformed_records_are_refused() {
            let no_marker = ProcessIdentity {
                pid: std::process::id(),
                pgid: std::process::id(),
                start_time: None,
            };
            assert_eq!(no_marker.verify(), IdentityVerdict::Unverifiable);
            assert_eq!(
                decide_on_eof(&no_marker).exit_code(),
                GUARDIAN_EXIT_REFUSED_UNVERIFIABLE
            );
            for malformed in [
                ProcessIdentity {
                    pid: 1,
                    pgid: 0,
                    start_time: Some(1),
                },
                ProcessIdentity {
                    pid: 0,
                    pgid: 0,
                    start_time: None,
                },
                ProcessIdentity {
                    pid: 1,
                    pgid: 2,
                    start_time: Some(1),
                },
            ] {
                assert!(
                    GuardianHandle::spawn_with_identity(malformed).is_err(),
                    "malformed identity forks nothing: {malformed:?}"
                );
                assert_eq!(
                    decide_on_eof(&malformed).exit_code(),
                    GUARDIAN_EXIT_REFUSED_MALFORMED
                );
            }
        }

        /// `Gone` + live group is a kill (reparented descendants); `Gone` +
        /// empty group is nothing to do.
        #[test]
        fn gone_leader_decides_on_the_group_state() {
            use std::os::unix::process::CommandExt;
            let mut child = Command::new("/bin/sh")
                .args(["-c", "sleep 0.2 & true"])
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("fixture tree");
            let pid = child.id();
            let identity = ProcessIdentity::capture(pid, pid);
            let _ = child.wait();
            // The background `sleep 0.2` keeps the group alive briefly.
            if group_exists(pid) {
                assert_eq!(decide_on_eof(&identity), GuardianAction::Kill);
                kill_group_term_then_kill(pid);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while group_exists(pid) {
                assert!(Instant::now() < deadline, "fixture group must die");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(decide_on_eof(&identity), GuardianAction::NothingToDo);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use faktor_core::error::Error;

    /// Inert handle where no guardian backend exists (Linux-first): spawns
    /// proceed with the platform's other containment (Windows job objects)
    /// or the existing in-process lifecycle (other unixes).
    #[derive(Debug)]
    pub(crate) struct GuardianHandle;

    impl GuardianHandle {
        pub(crate) fn try_spawn(_pid: u32) -> Result<Option<Self>, Error> {
            Ok(None)
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use imp::install_pdeathsig;
pub(crate) use imp::GuardianHandle;
#[cfg(all(test, target_os = "linux"))]
pub(crate) use imp::{ProcessIdentity, GUARDIAN_EXIT_REFUSED_RECYCLED};
