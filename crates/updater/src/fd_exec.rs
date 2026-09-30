//! The fd-identity exec seam: replace this process with the EXACT open file
//! descriptor whose bytes the launcher verified.
//!
//! This module is compiled only where the `libc` crate declares POSIX
//! `fexecve` (Linux/Android and FreeBSD/DragonFly). The kernel resolves no
//! version-directory pathname for the exec, so a writer that swaps the
//! directory entry — or the whole release directory — between the digest
//! check and the exec cannot redirect the launch: the executed file IS the
//! descriptor that was hashed.
//!
//! Platform gating: Darwin has no `fexecve` symbol (checked against Apple's
//! `unistd.h`); the launcher there execs the `/dev/fd/<n>` magic link on the
//! same descriptor instead (see `release::exec_path_fallback`). Every other
//! unix platform and non-unix platforms keep the path-based exec with the
//! documented gap.
//!
//! Honest limit: an fd exec binds the exec to the verified INODE, not to a
//! frozen snapshot of its content. A same-owner (or root) writer can still
//! rewrite the bytes in place after the hash; that writer owns the install
//! tree and is outside the trust boundary by construction (the release
//! ancestry check refuses group/other-writable paths, so the in-place case
//! needs the operator's own uid).

use std::ffi::{CString, OsString};
use std::fs::File;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::Path;

use crate::error::UpdateError;

/// Execute `file` with the exact argv/envp a `Command` would have used.
/// On success this never returns; a failure is a typed launch refusal.
pub(crate) fn exec_verified_fd(
    file: &File,
    argv: &[OsString],
    env: &[(OsString, OsString)],
    program: &Path,
) -> Result<i32, UpdateError> {
    let argv = argv
        .iter()
        .map(|value| cstring(value.as_os_str().as_bytes(), "argument"))
        .collect::<Result<Vec<_>, _>>()?;
    let envp = env
        .iter()
        .map(|(key, value)| {
            let mut entry = key.as_os_str().as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_os_str().as_bytes());
            cstring(&entry, "environment entry")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|entry| entry.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());
    // SAFETY: `file` is a live descriptor whose full content was hashed by
    // the caller immediately before this call; the argv/envp arrays are
    // NUL-terminated C strings kept alive across the call. fexecve either
    // replaces this process (and never returns) or returns -1 with errno set,
    // which is read immediately below.
    #[allow(unsafe_code)]
    let outcome =
        unsafe { libc::fexecve(file.as_raw_fd(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
    if outcome == -1 {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "exec verified fd for {}: {}",
                program.display(),
                std::io::Error::last_os_error()
            ),
        });
    }
    unreachable!("a successful fexecve never returns to the caller")
}

fn cstring(bytes: &[u8], what: &str) -> Result<CString, UpdateError> {
    CString::new(bytes).map_err(|_| UpdateError::LaunchRefused {
        detail: format!("exec {what} contains a NUL byte; refusing the launch"),
    })
}
