//! Platform split for handle-relative, symlink/reparse-bounded traversal
//! (P0-49 wave-11 hardening, extended to Windows by audits 30/53/54).
//!
//! The unix implementation is an `openat(2)` walk anchored on the workspace
//! root's directory fd; the windows implementation is a `NtCreateFile` walk
//! anchored on the workspace root's directory HANDLE, with explicit
//! reparse-point validation. Both never re-resolve a path string after the
//! walk starts, and both are reached through the same small surface
//! ([`open_no_follow_walk`] + [`OpenKind`]) so `lib.rs` has one traversal
//! call site per operation.
//!
//! Platforms with neither primitive (wasm and friends) keep the
//! canonicalize-then-open fallback compiled in `lib.rs`; there is no
//! silently-degraded Windows surface anymore.
//!
//! `windows.rs` is also compiled under `cfg(test)` on unix hosts so its
//! platform-independent path-hazard validators and reparse parser are
//! exercised by the test suite; the Win32 walk itself stays `cfg(windows)`.

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use faktor_core::error::Error;

#[cfg(unix)]
mod unix;
#[cfg(all(unix, test))]
pub(crate) use unix::{clear_walk_seam, install_walk_seam};

#[cfg(any(windows, test))]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    canonicalize_within, lexical_check, open_no_follow_walk, opened_is_path, read_reparse_link,
    windows_list_dir_raw, windows_open_child_dir, windows_reparse_class, RawDirEntry, ReparseClass,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
};

/// What the final component of a walk must be openable as. Intermediate
/// components are always directories.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenKind {
    /// The final entry may be a file or a directory (both are openable).
    Read,
    /// The final entry must be a directory (parent re-verification before a
    /// rename).
    Directory,
    /// Windows: the final entry is opened without following; a reparse point
    /// there is REFUSED instead of being followed (strict no-follow read).
    #[cfg(windows)]
    NoFollow,
    /// Windows: like [`OpenKind::NoFollow`] and the final entry must be a
    /// directory (strict no-follow enumeration).
    #[cfg(windows)]
    NoFollowDirectory,
    /// Windows: the final entry is opened with `FILE_OPEN_REPARSE_POINT` and
    /// the HANDLE is returned without following, so the caller can inspect
    /// the reparse point itself.
    #[cfg(windows)]
    ReparsePoint,
}

#[cfg(unix)]
pub(crate) fn open_no_follow_walk(
    root: &Path,
    rel: &Path,
    kind: OpenKind,
) -> Result<std::os::unix::io::OwnedFd, Error> {
    let final_flags = match kind {
        OpenKind::Read => libc::O_RDONLY,
        OpenKind::Directory => libc::O_RDONLY | libc::O_DIRECTORY,
    };
    unix::open_no_follow_walk(root, rel, final_flags)
}
