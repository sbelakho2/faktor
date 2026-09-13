//! Windows Job Objects (audit: taskkill /T is not equivalent to OS-enforced
//! kill-on-close). One job per owner with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when the owning process/handle dies,
//! the OS itself terminates every assigned child tree — the guarantee does
//! not depend on Faktor staying alive long enough to call taskkill. The
//! ConPTY backend (`faktor-pty`) and the process supervisor
//! (`faktor-terminal`) are the owners.
//!
//! Certification status: code is `cargo check`-verified against
//! `x86_64-pc-windows-msvc`; runtime certification requires a Windows
//! runner (declared platform blocker on this host).

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_HANDLE, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    pub struct JobGuard {
        handle: HANDLE,
    }

    // SAFETY: assignment is thread-safe; Drop closes the handle.
    unsafe impl Send for JobGuard {}
    unsafe impl Sync for JobGuard {}

    impl JobGuard {
        /// Strict creation: the raw win32 error is preserved so a
        /// containment-critical caller (the ConPTY spawn path) can refuse
        /// the spawn typed instead of exposing an uncontained child.
        /// [`JobGuard::create`] stays the convenience form.
        pub fn create_strict() -> Result<Self, u32> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(GetLastError());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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

        pub fn create() -> Option<Self> {
            Self::create_strict().ok()
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
    pub fn create_strict() -> Result<Self, u32> {
        Err(1)
    }
    pub fn create() -> Option<Self> {
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
