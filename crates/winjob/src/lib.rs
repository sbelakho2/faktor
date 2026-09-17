//! Windows Job Objects (audit: taskkill /T is not equivalent to OS-enforced
//! kill-on-close). One job per owner with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when the owning process/handle dies,
//! the OS itself terminates every assigned child tree — the guarantee does
//! not depend on Faktor staying alive long enough to call taskkill. The
//! ConPTY backend (`faktor-pty`) and the process supervisor
//! (`faktor-terminal`) are the owners.
//!
//! Additive resource limits (P1 terminal budgets): a job can ALSO carry
//! `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (a member exceeding its committed
//! memory limit is terminated by the kernel) and
//! `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (the job refuses to create a new
//! process past the limit). [`JobGuard::create_with_limits_strict`] applies
//! them at job creation, before any process is assigned — the assignment
//! path (suspended child → strict assign → membership verify → resume) is
//! unchanged, so limits are in force before the tree can execute.
//! [`policy`] is the alloc-free, host-independent flag mapping, compiled
//! and adversarially tested on every host.
//!
//! Certification status: code is `cargo check`-verified against
//! `x86_64-pc-windows-msvc`; runtime certification requires a Windows
//! runner (declared platform blocker on this host).

/// The requested Job Object limits. `kill_on_close` is the containment
/// guarantee every supervised Windows child carries; the memory / active
/// process limits are the optional authorization budgets. `None` means the
/// limit is NOT requested (never "unlimited by accident": an absent limit is
/// simply absent from the job's limit flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` value (committed bytes per process).
    pub process_memory_bytes: Option<u64>,
    /// `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` value (live processes in the job).
    pub active_processes: Option<u32>,
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (containment; default true).
    pub kill_on_close: bool,
}

impl Default for JobLimits {
    fn default() -> Self {
        Self {
            process_memory_bytes: None,
            active_processes: None,
            kill_on_close: true,
        }
    }
}

impl JobLimits {
    /// A memory-limited job (plus kill-on-close containment).
    pub fn memory(process_memory_bytes: u64) -> Self {
        Self {
            process_memory_bytes: Some(process_memory_bytes),
            ..Self::default()
        }
    }

    /// A process-count-limited job (plus kill-on-close containment).
    pub fn active_processes(active_processes: u32) -> Self {
        Self {
            active_processes: Some(active_processes),
            ..Self::default()
        }
    }

    /// Whether any optional resource limit is requested.
    pub fn has_resource_limits(&self) -> bool {
        self.process_memory_bytes.is_some() || self.active_processes.is_some()
    }
}

/// Pure, host-independent Job Object limit mapping (winnt.h frozen ABI
/// values). Compiled on every host so the flag policy is adversarially
/// testable where no Windows runtime exists.
#[cfg(any(windows, test))]
pub mod policy {
    use super::JobLimits;

    /// winnt.h `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`.
    pub const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: u32 = 0x0000_0008;
    /// winnt.h `JOB_OBJECT_LIMIT_PROCESS_MEMORY`.
    pub const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
    /// winnt.h `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    pub const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    /// The exact `LimitFlags` value of a [`JobLimits`]: a flag is set IFF
    /// the corresponding limit was REQUESTED, plus kill-on-close when the
    /// caller asked for containment. No flag is ever silently dropped and no
    /// unrequested limit is ever smuggled in.
    pub fn limit_flags(limits: &JobLimits) -> u32 {
        let mut flags = 0;
        if limits.kill_on_close {
            flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if limits.process_memory_bytes.is_some() {
            flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        }
        if limits.active_processes.is_some() {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        }
        flags
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_HANDLE, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    use super::{policy, JobLimits};

    pub struct JobGuard {
        handle: HANDLE,
    }

    // SAFETY: assignment is thread-safe; Drop closes the handle.
    unsafe impl Send for JobGuard {}
    unsafe impl Sync for JobGuard {}

    impl JobGuard {
        /// Strict creation WITH resource limits, BEFORE any process is
        /// assigned: `JOB_OBJECT_LIMIT_PROCESS_MEMORY` /
        /// `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` are set on the job object, so
        /// the first assigned child is bounded from its first instruction.
        /// The raw win32 error is preserved so a containment/budget-critical
        /// caller can refuse the spawn typed instead of exposing an
        /// unbudgeted or uncontained child.
        pub fn create_with_limits_strict(limits: JobLimits) -> Result<Self, u32> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(GetLastError());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = policy::limit_flags(&limits);
                if let Some(bytes) = limits.process_memory_bytes {
                    info.ProcessMemoryLimit = bytes as usize;
                }
                if let Some(active) = limits.active_processes {
                    info.BasicLimitInformation.ActiveProcessLimit = active;
                }
                let ok = SetInformationJobObject(
                    handle,
                    9, // JobObjectExtendedLimitInformation
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    let code = GetLastError();
                    CloseHandle(handle);
                    return Err(code);
                }
                Ok(Self { handle })
            }
        }

        /// Strict creation with the default limits (kill-on-close only).
        pub fn create_strict() -> Result<Self, u32> {
            Self::create_with_limits_strict(JobLimits::default())
        }

        pub fn create() -> Option<Self> {
            Self::create_strict().ok()
        }

        /// Best-effort creation with resource limits.
        pub fn create_with_limits(limits: JobLimits) -> Option<Self> {
            Self::create_with_limits_strict(limits).ok()
        }

        /// No-op guard for when job creation failed at startup.
        pub fn null() -> Self {
            Self {
                handle: std::ptr::null_mut(),
            }
        }

        /// Assign a child pid (best-effort; a failed assignment is logged —
        /// the supervisor's own kill paths still apply). Containment-critical
        /// callers use [`JobGuard::assign_strict`] and fail instead.
        pub fn assign(&self, pid: u32) {
            if let Err(code) = self.assign_strict(pid) {
                tracing::warn!("AssignProcessToJobObject({pid}) failed (win32 error {code})");
            }
        }

        /// Strict assignment: `Ok` is the containment guarantee, `Err` is the
        /// raw win32 error (null guard → `ERROR_INVALID_HANDLE`). Used by the
        /// ConPTY path to assign a `CREATE_SUSPENDED` child before it runs:
        /// a failed assignment must refuse the spawn, never resume an
        /// uncontained child.
        pub fn assign_strict(&self, pid: u32) -> Result<(), u32> {
            if self.handle.is_null() {
                return Err(ERROR_INVALID_HANDLE);
            }
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    return Err(GetLastError());
                }
                let r = AssignProcessToJobObject(self.handle, process);
                let code = if r == 0 { GetLastError() } else { 0 };
                CloseHandle(process);
                if code == 0 {
                    Ok(())
                } else {
                    Err(code)
                }
            }
        }

        /// Is `pid` currently a member of THIS job? Certification/diagnostic
        /// seam (the ConPTY spawn test proves the child is contained before
        /// `Pty::spawn` returns). Best-effort `false` when the process cannot
        /// be opened.
        pub fn contains(&self, pid: u32) -> bool {
            if self.handle.is_null() {
                return false;
            }
            unsafe {
                let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if process.is_null() {
                    return false;
                }
                let mut result = 0;
                let ok = IsProcessInJob(process, self.handle, &mut result);
                CloseHandle(process);
                ok != 0 && result != 0
            }
        }

        /// Terminate EVERY process in the job (children of job members are
        /// members too, so this is the whole tree). OS-enforced equivalent of
        /// `taskkill /PID <leader> /T /F` with no pid-tree walk and no race on
        /// membership: the kernel iterates the job's process list.
        pub fn terminate(&self) {
            if !self.handle.is_null() {
                unsafe {
                    TerminateJobObject(self.handle, 1);
                }
            }
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe {
                    CloseHandle(self.handle);
                }
            }
        }
    }
}

#[cfg(windows)]
pub use imp::JobGuard;

/// Off-Windows stub: job objects do not exist, so every strict form fails
/// with `ERROR_INVALID_FUNCTION` (1, frozen winerror.h ABI value) and every
/// best-effort form is a no-op — the same shape Windows callers compile
/// against without a `cfg` fork.
#[cfg(not(windows))]
pub struct JobGuard;

#[cfg(not(windows))]
impl JobGuard {
    /// `ERROR_INVALID_FUNCTION`: no job objects off Windows.
    pub fn create_with_limits_strict(_limits: JobLimits) -> Result<Self, u32> {
        Err(1)
    }
    /// `ERROR_INVALID_FUNCTION`: no job objects off Windows.
    pub fn create_strict() -> Result<Self, u32> {
        Err(1)
    }
    pub fn create() -> Option<Self> {
        None
    }
    pub fn create_with_limits(_limits: JobLimits) -> Option<Self> {
        None
    }
    pub fn null() -> Self {
        Self
    }
    pub fn assign(&self, _pid: u32) {}
    pub fn assign_strict(&self, _pid: u32) -> Result<(), u32> {
        Err(1)
    }
    pub fn contains(&self, _pid: u32) -> bool {
        false
    }
    pub fn terminate(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_carry_only_the_containment_flag() {
        let limits = JobLimits::default();
        assert!(limits.kill_on_close);
        assert!(!limits.has_resource_limits());
        assert_eq!(
            policy::limit_flags(&limits),
            policy::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        );
    }

    #[test]
    fn requested_limits_set_exactly_their_own_flags() {
        let memory = JobLimits::memory(64 * 1024 * 1024);
        assert_eq!(
            policy::limit_flags(&memory),
            policy::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | policy::JOB_OBJECT_LIMIT_PROCESS_MEMORY
        );
        let processes = JobLimits::active_processes(8);
        assert_eq!(
            policy::limit_flags(&processes),
            policy::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | policy::JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        );
        let both = JobLimits {
            process_memory_bytes: Some(1),
            active_processes: Some(1),
            kill_on_close: true,
        };
        assert_eq!(
            policy::limit_flags(&both),
            policy::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | policy::JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | policy::JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        );
    }

    #[test]
    fn an_absent_request_never_smuggles_a_flag_in() {
        // Adversarial: a caller that asks for no resource limit (and even
        // drops containment) gets NO flag — no silent memory/process cap is
        // ever invented, and no containment is ever claimed.
        let none = JobLimits {
            process_memory_bytes: None,
            active_processes: None,
            kill_on_close: false,
        };
        assert_eq!(policy::limit_flags(&none), 0);
        assert!(!none.has_resource_limits());
    }

    #[test]
    fn frozen_winnt_abi_values() {
        assert_eq!(policy::JOB_OBJECT_LIMIT_ACTIVE_PROCESS, 0x8);
        assert_eq!(policy::JOB_OBJECT_LIMIT_PROCESS_MEMORY, 0x100);
        assert_eq!(policy::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, 0x2000);
    }

    #[cfg(windows)]
    #[test]
    fn policy_flags_agree_with_windows_sys_constants() {
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        };
        assert_eq!(
            policy::JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        );
        assert_eq!(
            policy::JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY
        );
        assert_eq!(
            policy::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        );
    }
}
