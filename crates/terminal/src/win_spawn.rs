//! Windows spawn path: create the job, create the process suspended,
//! assign strictly, verify membership while it cannot run, resume LAST.
//! Any failure terminates the still-suspended child and refuses the spawn.

use std::os::windows::process::CommandExt;

use faktor_winjob::{JobGuard, JobLimits};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SUSPEND_RESUME};

use super::*;

// `NtResumeProcess` (ntdll): resume every thread of a process. The
// kernel transition needed is `ResumeThread`, but
// `std::process::Command::spawn` closes the primary-thread handle that
// `CreateProcess` returned and exposes no resume seam; process-wide
// resume is the user-mode counterpart of the WDK-documented
// `ZwResumeProcess` and takes only a process handle (opened with
// `PROCESS_SUSPEND_RESUME`). A just-created suspended process has
// exactly one thread, so this is precisely "resume the primary thread".
#[link(name = "ntdll")]
extern "system" {
    fn NtResumeProcess(process_handle: HANDLE) -> i32;
}

/// Create the per-child containment job. Fail-closed: without the job
/// there is no tree guarantee, so the spawn is refused before any
/// process exists.
pub(super) fn create_job() -> Result<JobGuard, Error> {
    JobGuard::create_strict().map_err(|code| {
        Error::internal(format!(
            "windows containment: CreateJobObject(KILL_ON_JOB_CLOSE) failed \
                 (win32 error {code}); refusing the spawn"
        ))
    })
}

/// [`create_job`] with the requested Job Object resource limits
/// (`PROCESS_MEMORY` / `ACTIVE_PROCESS`): the job carries them BEFORE
/// the suspended child is assigned, so the tree is bounded from its
/// first instruction.
pub(super) fn create_job_with_limits(limits: JobLimits) -> Result<JobGuard, Error> {
    JobGuard::create_with_limits_strict(limits).map_err(|code| {
        Error::internal(format!(
            "windows containment: CreateJobObject(memory/process limits) failed \
                 (win32 error {code}); refusing the spawn"
        ))
    })
}

/// Mark the command `CREATE_SUSPENDED` (the pure policy owns the flag).
pub(super) fn suspend_on_create(cmd: &mut std::process::Command) {
    cmd.creation_flags(win_containment_policy::creation_flags());
}

/// The assign → verify → resume core, executed while the child is STILL
/// suspended. Ordering is the containment guarantee:
/// 1. `assign_strict` — a failed assignment refuses the spawn;
/// 2. `contains` — membership is verified before any instruction runs,
///    so it cannot race a descendant;
/// 3. resume — the ONLY exposure point, gated by the pure
///    [`win_containment_policy::may_resume`] predicate.
fn assign_verify_resume(job: &JobGuard, pid: u32) -> Result<(), Error> {
    if pid == 0 {
        return Err(Error::internal(
            "windows containment: spawned child has no pid; refusing the spawn",
        ));
    }
    job.assign_strict(pid).map_err(|code| {
        Error::internal(format!(
            "windows containment: AssignProcessToJobObject(pid {pid}) failed \
                 (win32 error {code}); the suspended child is terminated, never resumed \
                 uncontained"
        ))
    })?;
    // The child is still suspended: this check cannot race a descendant.
    let verified_member = job.contains(pid);
    if !win_containment_policy::may_resume(true, verified_member) {
        return Err(Error::internal(format!(
            "windows containment: pid {pid} is not a job member after assignment; \
                 the suspended child is terminated, never resumed uncontained"
        )));
    }
    resume(pid).map_err(|code| {
        Error::internal(format!(
            "windows containment: NtResumeProcess(pid {pid}) failed (status {code}); \
                 the child is terminated"
        ))
    })
}

#[allow(unsafe_code)]
fn resume(pid: u32) -> Result<(), u32> {
    // SAFETY: `pid` names the just-created, still-suspended child; the
    // handle is closed on every path.
    unsafe {
        let process = OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid);
        if process.is_null() {
            return Err(GetLastError());
        }
        let status = NtResumeProcess(process);
        CloseHandle(process);
        if status < 0 {
            Err(status as u32)
        } else {
            Ok(())
        }
    }
}

/// Contain an already-created std child. On failure the child is killed
/// while still suspended (`TerminateProcess` works on a suspended
/// process; the primary thread never runs) and the error is returned.
pub(super) fn assign_and_resume_std(
    job: &JobGuard,
    child: &mut std::process::Child,
) -> Result<(), Error> {
    let pid = child.id();
    match assign_verify_resume(job, pid) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

/// tokio twin of [`assign_and_resume_std`].
pub(super) async fn assign_and_resume_tokio(
    job: &JobGuard,
    child: &mut tokio::process::Child,
) -> Result<(), Error> {
    let pid = child.id().unwrap_or(0);
    match assign_verify_resume(job, pid) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(e)
        }
    }
}
