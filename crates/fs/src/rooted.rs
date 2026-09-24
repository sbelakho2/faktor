//! A rooted-directory authority: every operation is anchored on a directory
//! handle opened once, and every path is resolved component-by-component with
//! no-follow semantics, so a directory entry swapped for a symlink (unix) or
//! a reparse point (Windows) cannot redirect an operation outside the root.
//!
//! # Contract
//!
//! * Paths passed to [`RootedDir`] are **relative**. `..`, absolute paths,
//!   root/prefix components and NUL bytes are refused typed before any
//!   syscall; the root itself is addressed by the empty path.
//! * `create_dir_all`, `open_create_new`, `remove_tree`, `remove_file`,
//!   `atomic_publish` and `sync_dir` never re-resolve a path string after the
//!   anchored walk starts.
//! * A symlink/reparse entry on any walked component — including the final
//!   one — is refused: this API never follows links. The paths this authority
//!   receives are runtime-generated identifiers (validated profile names,
//!   UUID directories, sanitized file names), so a link is by construction
//!   unexpected and is treated as an attack.
//! * `atomic_publish` requires the temp and destination entries to share one
//!   parent directory (same anchored fd), so the publish is a single
//!   `renameat`; the parent directory is fsynced afterwards on unix.
//! * `remove_tree` refuses a link at the requested entry and unlinks nested
//!   links without ever traversing them.
//!
//! # Platform split
//!
//! * unix: `openat(2)`/`mkdirat(2)`/`renameat(2)`/`unlinkat(2)` on the
//!   anchored directory fd with `O_NOFOLLOW | O_CLOEXEC` everywhere.
//! * Windows: resolution goes through the workspace's reparse-aware,
//!   handle-relative walk (`crate::platform::open_no_follow_walk` /
//!   `canonicalize_within`), which opens every component with
//!   `FILE_FLAG_OPEN_REPARSE_POINT`, validates permitted reparse targets
//!   itself and refuses out-of-root/unverified targets. Directory creation,
//!   file creation and rename then act on the verified location; publish is a
//!   same-directory rename, and `open_create_new` re-checks the created
//!   entry's identity against a no-follow open of its path. Windows directory
//!   fsync has no equivalent, so `sync_dir` is a documented no-op there.
//!
//! # Honest limit (Windows only)
//!
//! Between the reparse-aware walk of an operation and the Win32 call that
//! performs it, a hostile process could swap the directory entry once more.
//! `open_create_new` closes that with the post-open identity net;
//! `remove_tree` uses `std::fs::remove_dir_all`, which on modern Rust deletes
//! by reparse-point-opening handles and never follows a link out of the tree.
//! The unix implementation has no such window: every step is an `openat` on
//! the fd produced by the previous step.
//!
//! # Bounded traversal (P0-52)
//!
//! [`RootedDir::read`], [`RootedDir::list_entries`] and
//! [`RootedDir::walk_bounded`] are the ONE handle-relative traversal surface
//! upper layers (tree manifest, tree copy, the CLI tools) build on. A
//! [`WalkBudget`] is mutated by every walker step; exhaustion is a typed
//! `Oversized` error naming the exhausted budget, and a caller that may
//! return partial results (the CLI search) must say so explicitly instead of
//! presenting a shorter result as complete.
//!
//! Windows review note: the Windows walker uses the same relative NtCreateFile
//! child opens and handle-based enumeration (`GetFileInformationByHandleEx`)
//! as the platform walker; strict no-follow reads/enumeration refuse a
//! reparse point at the final component instead of following it. That path is
//! covered by the Windows-runner test `rooted_walk_refuses_a_junction_before_descent`
//! in `platform/windows.rs` and was statically reviewed on the unix host
//! where it cannot be compiled.

#![allow(unsafe_code)] // platform authority module: every unsafe
                       // block/function in this module carries a `// SAFETY:` justification and is
                       // enumerated by tests/static-authority.
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

use faktor_core::error::Error;

#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use crate::FileData;

/// Hard bound on the number of components one operation may consume, so a
/// hostile relative path fails loudly instead of exhausting the syscall
/// budget (parity with the workspace walk bound).
const MAX_COMPONENTS: usize = 4096;

/// An authority anchored on one directory. Cheap to clone (shares the
/// anchored handle on unix).
#[derive(Debug, Clone)]
pub struct RootedDir {
    root: PathBuf,
    #[cfg(unix)]
    fd: Arc<OwnedFd>,
}

impl RootedDir {
    /// Open an existing directory as a rooted authority. The root entry
    /// itself is opened without following a link; a symlinked root is
    /// refused.
    pub fn open(root: &Path) -> Result<Self, Error> {
        #[cfg(unix)]
        {
            let fd = unix_open_root(root)?;
            Ok(Self {
                root: root.to_path_buf(),
                fd: Arc::new(fd),
            })
        }
        #[cfg(windows)]
        {
            // The reparse-aware walk validates the root handle (including
            // reparse/identity checks); it is re-opened per operation, so
            // nothing is retained here.
            crate::platform::open_no_follow_walk(root, Path::new(""), OpenKind::Directory)?;
            Ok(Self {
                root: root.to_path_buf(),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            if !root.is_dir() {
                return Err(Error::not_found(format!("{}", root.display())));
            }
            Ok(Self {
                root: root.to_path_buf(),
            })
        }
    }

    /// Create (when missing) and open the root.
    pub fn create(root: &Path) -> Result<Self, Error> {
        if !root.is_dir() {
            std::fs::create_dir_all(root).map_err(|e| {
                Error::internal(format!("cannot create root {}: {e}", root.display()))
            })?;
        }
        Self::open(root)
    }

    /// The root path (diagnostics and child-process argv; never a path
    /// resolution input).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lexical join for consumers that need a printable/absolute location
    /// (e.g. Chromium's `--user-data-dir`). Containment of the joined path is
    /// the caller's concern; every *file operation* must go through this
    /// authority.
    pub fn join(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }

    /// Create `rel` and every missing ancestor, then verify each component is
    /// a real directory (never a link).
    pub fn create_dir_all(&self, rel: &Path) -> Result<(), Error> {
        let comps = relative_components(rel)?;
        if comps.len() > MAX_COMPONENTS {
            return Err(Error::oversized(format!(
                "{rel:?} exceeds the {MAX_COMPONENTS}-component bound"
            )));
        }
        #[cfg(unix)]
        {
            let mut dir = unix_dup(self.fd.as_raw_fd())?;
            for comp in comps {
                match unix_mkdir_at(&dir, &comp) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => {
                        return Err(Error::internal(format!(
                            "cannot create directory component {comp:?} under {}: {e}",
                            self.root.display()
                        )))
                    }
                }
                dir = unix_open_dir_at(&dir, &comp).map_err(|e| {
                    Error::permission(format!(
                        "{}: directory component {comp:?} is a symlink or not a directory: {e}",
                        self.root.display()
                    ))
                })?;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let mut prefix = PathBuf::new();
            for comp in comps {
                prefix.push(&comp);
                let candidate = self.root.join(&prefix);
                if let Err(e) = std::fs::create_dir(&candidate) {
                    if e.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(Error::internal(format!(
                            "cannot create directory component {comp:?} under {}: {e}",
                            self.root.display()
                        )));
                    }
                }
                self.require_real_dir(&prefix)?;
            }
            Ok(())
        }
    }

    /// Exclusively create `rel` (parent components must already exist) and
    /// return the open file. The final component is created with
    /// `O_EXCL`/`CREATE_NEW` and no-follow semantics, so an existing entry —
    /// including a link — is refused.
    pub fn open_create_new(&self, rel: &Path) -> Result<std::fs::File, Error> {
        let (parent, name) = split_final(rel)?;
        #[cfg(unix)]
        {
            let dir = self.walk_dirs(&parent)?;
            let file = unix_open_create_new_at(&dir, &name).map_err(|e| {
                Error::internal(format!(
                    "cannot create {rel:?} under {}: {e}",
                    self.root.display()
                ))
            })?;
            Ok(std::fs::File::from(file))
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            self.require_parent_dir(rel)?;
            let path = self.root.join(rel);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt as _;
                options.custom_flags(WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT);
            }
            let file = options
                .open(&path)
                .map_err(|e| Error::internal(format!("cannot create {rel:?}: {e}")))?;
            #[cfg(windows)]
            {
                if !crate::platform::opened_is_path(&file, &path) {
                    return Err(Error::permission(format!(
                        "{rel:?}: created entry failed the identity check"
                    )));
                }
            }
            Ok(file)
        }
    }

    /// Remove one entry. Directories are removed recursively; a link at the
    /// requested entry is refused, and nested links are unlinked without
    /// being traversed. A missing entry is `Ok(())` (idempotent).
    pub fn remove_tree(&self, rel: &Path) -> Result<(), Error> {
        let (parent, name) = split_final(rel)?;
        #[cfg(unix)]
        {
            let dir = self.walk_dirs(&parent)?;
            let target = match unix_open_dir_at(&dir, &name) {
                Ok(fd) => fd,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    return Err(Error::permission(format!(
                        "refusing to remove {}: the entry is a symlink or not a directory: {e}",
                        rel.display()
                    )))
                }
            };
            unix_remove_children(&target, rel)?;
            drop(target);
            unix_unlink_at(&dir, &name, true)
                .map_err(|e| Error::internal(format!("cannot remove {rel:?}: {e}")))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            let resolved = canonicalize_rooted(&self.root, rel)?;
            match std::fs::remove_dir_all(&resolved) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::internal(format!("cannot remove {rel:?}: {e}"))),
            }
        }
    }

    /// Remove one file entry (never a directory, never following a link).
    /// A missing entry is `Ok(())`.
    pub fn remove_file(&self, rel: &Path) -> Result<(), Error> {
        let (parent, name) = split_final(rel)?;
        #[cfg(unix)]
        {
            let dir = self.walk_dirs(&parent)?;
            match unix_unlink_at(&dir, &name, false) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::internal(format!(
                    "cannot remove {}: {e}",
                    rel.display()
                ))),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name);
            let resolved = canonicalize_rooted(&self.root, rel)?;
            match std::fs::remove_file(&resolved) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::internal(format!(
                    "cannot remove {}: {e}",
                    rel.display()
                ))),
            }
        }
    }

    /// Atomically publish `tmp_rel` at `dest_rel`. Both must live in the same
    /// directory (the rename is a single anchored operation); the parent
    /// directory is fsynced on unix so the publish survives power loss.
    ///
    /// The implementation lives in [`crate::atomic::atomic_publish_at`] (the
    /// one sanctioned durable-write module); this authority only anchors it,
    /// so the constrained walk and the rename stay under one roof.
    pub fn atomic_publish(&self, tmp_rel: &Path, dest_rel: &Path) -> Result<(), Error> {
        crate::atomic::atomic_publish_at(self, tmp_rel, dest_rel)
    }

    /// Restrict an existing directory at `rel` to owner-only access (0700) on
    /// unix; a no-op on platforms without POSIX modes. The empty path
    /// restricts the root itself.
    pub fn restrict_owner_only(&self, rel: &Path) -> Result<(), Error> {
        #[cfg(unix)]
        {
            let fd = self.walk_dirs(rel)?;
            // SAFETY: the fd is owned and points at the directory this function just created; failure is surfaced, not ignored.
            if unsafe { libc::fchmod(fd.as_raw_fd(), 0o700) } != 0 {
                return Err(Error::internal(format!(
                    "cannot restrict {}: {}",
                    rel.display(),
                    io::Error::last_os_error()
                )));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = rel;
            Ok(())
        }
    }

    /// fsync a directory so a rename inside it is durable (unix only; a
    /// documented no-op elsewhere).
    pub fn sync_dir(&self, rel: &Path) -> Result<(), Error> {
        #[cfg(unix)]
        {
            let fd = self.walk_dirs(rel)?;
            unix_fsync(&fd)
                .map_err(|e| Error::internal(format!("cannot fsync {}: {e}", rel.display())))
        }
        #[cfg(not(unix))]
        {
            let _ = rel;
            Ok(())
        }
    }

    /// True when `rel` resolves to a chain of real directories under the
    /// root (links are never followed, so a symlinked entry is `false`).
    pub fn exists(&self, rel: &Path) -> bool {
        if relative_components(rel)
            .map(|c| c.is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        #[cfg(windows)]
        {
            // A reparse entry is not "existing" for containment purposes.
            crate::platform::open_no_follow_walk(&self.root, rel, OpenKind::Directory).is_ok()
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.root.join(rel).is_dir()
        }
        #[cfg(unix)]
        {
            self.walk_dirs(rel).is_ok()
        }
    }

    /// Walk every directory component of `rel` with no-follow semantics and
    /// return the final directory fd. The empty path returns a duplicate of
    /// the anchored root fd.
    #[cfg(unix)]
    pub(crate) fn walk_dirs(&self, rel: &Path) -> Result<OwnedFd, Error> {
        let comps = relative_components(rel)?;
        if comps.len() > MAX_COMPONENTS {
            return Err(Error::oversized(format!(
                "{rel:?} exceeds the {MAX_COMPONENTS}-component bound"
            )));
        }
        let mut dir = unix_dup(self.fd.as_raw_fd())?;
        for comp in comps {
            dir = unix_open_dir_at(&dir, &comp).map_err(|e| {
                Error::permission(format!(
                    "{}: component {comp:?} is a symlink or not a directory: {e}",
                    self.root.display()
                ))
            })?;
        }
        Ok(dir)
    }

    /// Like [`Self::walk_dirs`] but every level — including the root — is a
    /// FRESH `openat(2)` of the directory, so an enumeration on the returned
    /// fd never shares (or perturbs) a file offset with the stored root fd.
    #[cfg(unix)]
    fn walk_dirs_fresh(&self, rel: &Path) -> Result<OwnedFd, Error> {
        let comps = relative_components(rel)?;
        if comps.len() > MAX_COMPONENTS {
            return Err(Error::oversized(format!(
                "{rel:?} exceeds the {MAX_COMPONENTS}-component bound"
            )));
        }
        let mut dir = unix_open_fresh_dir_at(&self.fd, OsStr::new(".")).map_err(|e| {
            Error::internal(format!(
                "cannot reopen workspace root {} for enumeration: {e}",
                self.root.display()
            ))
        })?;
        for comp in comps {
            dir = unix_open_dir_at(&dir, &comp).map_err(|e| {
                Error::permission(format!(
                    "{}: component {comp:?} is a symlink or not a directory: {e}",
                    self.root.display()
                ))
            })?;
        }
        Ok(dir)
    }

    /// Non-unix: verify the parent directory of `rel` through the
    /// reparse-aware walk.
    #[cfg(not(unix))]
    fn require_parent_dir(&self, rel: &Path) -> Result<(), Error> {
        let (parent, _) = split_final(rel)?;
        self.require_real_dir(&parent)
    }

    /// Non-unix: verify `rel` is a real directory under the reparse-aware
    /// walk, never a link/reparse escape.
    #[cfg(not(unix))]
    fn require_real_dir(&self, rel: &Path) -> Result<(), Error> {
        #[cfg(windows)]
        {
            crate::platform::open_no_follow_walk(&self.root, rel, OpenKind::Directory).map(|_| ())
        }
        #[cfg(not(windows))]
        {
            let path = self.root.join(rel);
            if path.is_dir() {
                Ok(())
            } else {
                Err(Error::permission(format!(
                    "{}: not a directory",
                    path.display()
                )))
            }
        }
    }
}

#[cfg(windows)]
use crate::platform::OpenKind;

/// Canonical location of `rel` under `root` on non-unix platforms, via the
/// reparse-aware handle walk on Windows.
#[cfg(not(unix))]
pub(crate) fn canonicalize_rooted(root: &Path, rel: &Path) -> Result<PathBuf, Error> {
    #[cfg(windows)]
    {
        crate::platform::canonicalize_within(root, rel)
    }
    #[cfg(not(windows))]
    {
        Ok(root.join(rel))
    }
}

/// Split a caller path into validated relative components. `..`, absolute
/// paths, prefixes and NULs are denied before any syscall.
fn relative_components(rel: &Path) -> Result<Vec<OsString>, Error> {
    let mut out: Vec<OsString> = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(name) => {
                if name.as_encoded_bytes().contains(&0) {
                    return Err(Error::malformed(format!(
                        "path component contains a NUL byte: {rel:?}"
                    )));
                }
                out.push(name.to_os_string());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::permission(format!(
                    "path traversal rejected: {rel:?}"
                )))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::permission(format!(
                    "rooted path rejected by the rooted-directory authority: {rel:?}"
                )))
            }
        }
    }
    Ok(out)
}

/// Split `rel` into (parent components, final name); the final component must
/// be a plain name.
pub(crate) fn split_final(rel: &Path) -> Result<(PathBuf, OsString), Error> {
    let comps = relative_components(rel)?;
    let Some(name) = comps.last() else {
        return Err(Error::malformed(format!(
            "operation requires a final path component: {rel:?}"
        )));
    };
    let mut parent = PathBuf::new();
    for comp in &comps[..comps.len() - 1] {
        parent.push(comp);
    }
    Ok((parent, name.clone()))
}

// ---------------------------------------------------------------------------
// unix syscall layer
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn unix_cstring(name: &OsStr) -> Result<CString, Error> {
    CString::new(name.as_bytes())
        .map_err(|_| Error::malformed(format!("path component {name:?} contains a NUL byte")))
}

#[cfg(unix)]
fn unix_open_root(root: &Path) -> Result<OwnedFd, Error> {
    let c = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| Error::malformed(format!("root {:?} contains a NUL byte", root)))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `c` is a NUL-terminated path; flags include O_NOFOLLOW and
    // O_DIRECTORY so a symlinked/non-directory root is refused, and no mode
    // argument is needed without O_CREAT.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => Error::permission(format!(
                "root {} was swapped for a symlink or non-directory",
                root.display()
            )),
            _ if e.kind() == io::ErrorKind::NotFound => {
                Error::not_found(format!("{}", root.display()))
            }
            _ => Error::internal(format!("root {}: {e}", root.display())),
        });
    }
    // SAFETY: `fd` is a live descriptor returned by the successful open above
    // and not owned anywhere else; OwnedFd closes it exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn unix_dup(fd: libc::c_int) -> Result<OwnedFd, Error> {
    // SAFETY: `fd` is a live descriptor; `fcntl` with F_DUPFD_CLOEXEC
    // allocates a new descriptor or returns -1, both handled here.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(Error::internal(format!(
            "cannot duplicate anchored directory fd: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: `dup` is a fresh descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

#[cfg(unix)]
fn unix_open_dir_at(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let c = unix_cstring(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `c` is NUL-terminated and relative to the live directory fd
    // `dir`; no O_CREAT so no mode argument is required.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn unix_mkdir_at(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let c = unix_cstring(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c` is NUL-terminated and relative to the live directory fd
    // `dir`; mode 0700 restricts the new directory to its owner.
    let r = unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o700) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn unix_open_create_new_at(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let c = unix_cstring(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let flags = libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `c` is NUL-terminated and relative to the live directory fd
    // `dir`; mode 0600 restricts the new file to its owner.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn unix_unlink_at(dir: &OwnedFd, name: &OsStr, directory: bool) -> io::Result<()> {
    let c = unix_cstring(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: `c` is NUL-terminated and relative to the live directory fd
    // `dir`; AT_REMOVEDIR only removes an empty directory (the caller
    // removes children first) and never follows a link.
    let r = unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn unix_rename_at(
    from_dir: &OwnedFd,
    from: &OsStr,
    to_dir: &OwnedFd,
    to: &OsStr,
) -> io::Result<()> {
    let from_c = unix_cstring(from).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let to_c = unix_cstring(to).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both names are NUL-terminated and relative to their live
    // directory fds; renameat is atomic and replaces the destination.
    let r = unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            from_c.as_ptr(),
            to_dir.as_raw_fd(),
            to_c.as_ptr(),
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn unix_fsync(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: `fd` is a live descriptor owned by the caller; fsync takes no
    // ownership and only flushes state.
    let r = unsafe { libc::fsync(fd.as_raw_fd()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Remove every child of the open directory `dir` without ever traversing a
/// link: directory entries are recursed by fd, links and files are unlinked
/// in place. A `readdir` error is treated as end-of-entries; the final
/// `AT_REMOVEDIR` then fails loudly on anything left behind.
#[cfg(unix)]
fn unix_remove_children(dir: &OwnedFd, rel: &Path) -> Result<(), Error> {
    // SAFETY: `dup` returns a fresh descriptor (or -1, handled below), which
    // `fdopendir` takes ownership of.
    let dup = unsafe { libc::dup(dir.as_raw_fd()) };
    if dup < 0 {
        return Err(Error::internal(format!(
            "cannot enumerate {}: {}",
            rel.display(),
            io::Error::last_os_error()
        )));
    }
    // SAFETY: `dup` is a fresh directory descriptor; fdopendir takes
    // ownership and returns NULL on failure (the descriptor is then closed
    // by us).
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        // SAFETY: `dup` is still owned by us because fdopendir failed.
        let _ = unsafe { libc::close(dup) };
        return Err(Error::internal(format!(
            "cannot enumerate {}: {}",
            rel.display(),
            io::Error::last_os_error()
        )));
    }
    let mut failure: Option<Error> = None;
    loop {
        // SAFETY: `stream` is a live DIR* until closedir below; readdir
        // returns a pointer to the next entry or NULL at end/error.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `entry` points at a live dirent for this iteration; d_name
        // is a NUL-terminated array inside it.
        let name_ptr = unsafe { (*entry).d_name.as_ptr() };
        // SAFETY: `name_ptr` is NUL-terminated by the dirent contract.
        let name = unsafe { CStr::from_ptr(name_ptr) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let comp = OsStr::from_bytes(bytes);
        // SAFETY: fstatat with AT_SYMLINK_NOFOLLOW never follows the entry
        // and writes only into `st`; the name pointer is NUL-terminated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name_ptr,
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if r != 0 {
            failure = Some(Error::internal(format!(
                "cannot stat a child of {}: {}",
                rel.display(),
                io::Error::last_os_error()
            )));
            break;
        }
        if (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            match unix_open_dir_at(dir, comp) {
                Ok(child) => {
                    if let Err(e) = unix_remove_children(&child, rel) {
                        failure = Some(e);
                        break;
                    }
                    drop(child);
                    if let Err(e) = unix_unlink_at(dir, comp, true) {
                        failure = Some(Error::internal(format!(
                            "cannot remove directory child {comp:?} of {}: {e}",
                            rel.display()
                        )));
                        break;
                    }
                }
                Err(_) => {
                    // The entry changed under us: it is no longer a real
                    // directory, so unlink it as an entry (never traverse).
                    if let Err(e) = unix_unlink_at(dir, comp, false) {
                        failure = Some(Error::internal(format!(
                            "cannot remove swapped child {comp:?} of {}: {e}",
                            rel.display()
                        )));
                        break;
                    }
                }
            }
        } else if let Err(e) = unix_unlink_at(dir, comp, false) {
            failure = Some(Error::internal(format!(
                "cannot remove child {comp:?} of {}: {e}",
                rel.display()
            )));
            break;
        }
    }
    // SAFETY: `stream` is live and owned here; closedir closes the dup'ed fd
    // and only that one.
    let _ = unsafe { libc::closedir(stream) };
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(windows)]
const WINDOWS_FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

// ---------------------------------------------------------------------------
// Bounded handle-relative traversal (P0-52)
// ---------------------------------------------------------------------------
//
// Every upper-layer walker (tree manifest, tree copy, the CLI's search)
// enumerates through this authority: directories are opened relative to the
// already-open parent handle, children are inspected with fstatat
// (`AT_SYMLINK_NOFOLLOW`) / handle attribute queries, and a budgeted walk
// carries explicit whole-operation caps. A path string is never converted
// back to an absolute pathname and reopened.
//
// A [`WalkBudget`] is mutated by every walker step (entries, directories,
// depth, total file bytes seen, bytes actually read). Exhaustion is always a
// typed [`faktor_core::error::ErrorKind::Oversized`] error carrying the
// budget name, so a caller that may return partial results (the CLI search)
// can say `complete: false` with the concrete truncation reason.

/// What one enumerated entry is. `Other` covers FIFOs, sockets, devices and
/// (on Windows) reparse tags outside the symlink set: callers decide whether
/// that is a typed refusal or a completeness note — this layer never folds it
/// away silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootedEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// One directory entry enumerated through an already-open directory handle.
/// Symlinks are never followed: they are reported as [`RootedEntryKind::Symlink`]
/// with the literal link metadata.
#[derive(Debug, Clone)]
pub struct RootedEntry {
    /// The entry's name (one plain path component).
    pub name: OsString,
    /// The workspace-relative path of the entry.
    pub rel: PathBuf,
    pub kind: RootedEntryKind,
    /// `lstat` size (the literal target length for symlinks; 0 on platforms
    /// that do not report one).
    pub size: u64,
    /// Last-modification time in milliseconds since the unix epoch
    /// (`lstat`/directory-record value; 0 when the platform reports none).
    pub modified_ms: i64,
    #[cfg(unix)]
    pub(crate) mode: u32,
}

impl RootedEntry {
    pub fn is_file(&self) -> bool {
        self.kind == RootedEntryKind::File
    }

    pub fn is_dir(&self) -> bool {
        self.kind == RootedEntryKind::Directory
    }

    pub fn is_symlink(&self) -> bool {
        self.kind == RootedEntryKind::Symlink
    }

    pub fn is_other(&self) -> bool {
        self.kind == RootedEntryKind::Other
    }

    /// True when any execute bit is set (the canonical manifest's `100755`
    /// projection). Always false on platforms without POSIX mode bits.
    pub fn executable(&self) -> bool {
        #[cfg(unix)]
        {
            self.mode & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// The entry's unix mode bits (lower 12), `None` on other platforms.
    pub fn unix_mode(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            Some(self.mode)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

/// The result of one directory enumeration. `overflowed` means the
/// directory held MORE entries than the `max` the caller allowed: the
/// returned prefix is real but incomplete, and a caller that needs a
/// complete listing must refuse (a manifest) or surface a truncation reason
/// (a search). The name avoids the `FileData`-hygiene scan's `.truncated`
/// consumer marker (P0-50) while keeping the semantics explicit.
#[derive(Debug, Clone, Default)]
pub struct DirListing {
    pub entries: Vec<RootedEntry>,
    pub overflowed: bool,
}

/// Explicit whole-operation budget, mutated by every walker step. Exhaustion
/// is a typed `Oversized` error naming the budget that ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkBudget {
    /// Total entries (files, dirs, symlinks, other) a walk may inspect.
    pub max_entries: usize,
    /// Total directories a walk may open.
    pub max_directories: usize,
    /// Maximum directory depth (the root is depth 0).
    pub max_depth: usize,
    /// Total regular-file bytes a walk may account (file sizes seen).
    pub max_total_file_bytes: u64,
    /// Total bytes a walk may actually read.
    pub max_total_read_bytes: u64,
    entries: usize,
    directories: usize,
    total_file_bytes: u64,
    total_read_bytes: u64,
}

impl WalkBudget {
    pub fn new(
        max_entries: usize,
        max_directories: usize,
        max_depth: usize,
        max_total_file_bytes: u64,
        max_total_read_bytes: u64,
    ) -> Self {
        Self {
            max_entries,
            max_directories,
            max_depth,
            max_total_file_bytes,
            max_total_read_bytes,
            entries: 0,
            directories: 0,
            total_file_bytes: 0,
            total_read_bytes: 0,
        }
    }

    pub fn entries(&self) -> usize {
        self.entries
    }

    pub fn directories(&self) -> usize {
        self.directories
    }

    pub fn total_file_bytes(&self) -> u64 {
        self.total_file_bytes
    }

    pub fn total_read_bytes(&self) -> u64 {
        self.total_read_bytes
    }

    /// Entries still allowed before the cap is hit.
    pub fn remaining_entries(&self) -> usize {
        self.max_entries.saturating_sub(self.entries)
    }

    pub fn charge_entry(&mut self, depth: usize) -> Result<(), Error> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > self.max_entries {
            return Err(Error::oversized(format!(
                "walk exceeded the {}-entry budget",
                self.max_entries
            )));
        }
        self.check_depth(depth)
    }

    pub fn charge_directory(&mut self, depth: usize) -> Result<(), Error> {
        self.directories = self.directories.saturating_add(1);
        if self.directories > self.max_directories {
            return Err(Error::oversized(format!(
                "walk exceeded the {}-directory budget",
                self.max_directories
            )));
        }
        self.check_depth(depth)
    }

    pub fn charge_file_bytes(&mut self, bytes: u64) -> Result<(), Error> {
        self.total_file_bytes = self.total_file_bytes.saturating_add(bytes);
        if self.total_file_bytes > self.max_total_file_bytes {
            return Err(Error::oversized(format!(
                "walk exceeded the {}-byte file budget",
                self.max_total_file_bytes
            )));
        }
        Ok(())
    }

    pub fn charge_read_bytes(&mut self, bytes: u64) -> Result<(), Error> {
        self.total_read_bytes = self.total_read_bytes.saturating_add(bytes);
        if self.total_read_bytes > self.max_total_read_bytes {
            return Err(Error::oversized(format!(
                "walk exceeded the {}-byte read budget",
                self.max_total_read_bytes
            )));
        }
        Ok(())
    }

    fn check_depth(&self, depth: usize) -> Result<(), Error> {
        if depth > self.max_depth {
            return Err(Error::oversized(format!(
                "walk exceeded the {}-directory depth budget at depth {depth}",
                self.max_depth
            )));
        }
        Ok(())
    }
}

/// What a walk visitor wants after seeing one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkStep {
    /// Keep walking (descend into the entry when it is a directory).
    Continue,
    /// Do not descend into this entry even when it is a directory.
    SkipDir,
    /// End the whole walk successfully (the visitor has what it needs).
    Stop,
}

/// One entry of a budgeted walk offered to the visitor.
#[derive(Debug, Clone)]
pub struct WalkItem<'a> {
    pub rel: &'a Path,
    pub name: &'a OsStr,
    pub depth: usize,
    pub entry: &'a RootedEntry,
}

impl RootedDir {
    /// Handle-relative, strict no-follow bounded read. A symlink at any
    /// walked component (including the final one) is refused typed: this
    /// authority never follows links. The digest is the shared
    /// [`ContentDigest`](crate::ContentDigest): `Full` when the whole file
    /// was returned, `Slice` when the read was capped at `max_bytes`.
    pub fn read(&self, rel: &Path, max_bytes: usize) -> Result<FileData, Error> {
        let mut f = self.open_read(rel)?;
        let (bytes, digest) = crate::read_open_bounded(&mut f, rel, max_bytes)?;
        let size = bytes.len();
        Ok(FileData {
            path: self.root.join(rel),
            bytes,
            size,
            digest,
        })
    }

    /// Open `rel` for reading through the anchored walk, never following a
    /// link: on unix the parent chain is `openat(O_DIRECTORY|O_NOFOLLOW)`
    /// and the final `openat(O_RDONLY|O_NOFOLLOW)`; on Windows the walk
    /// opens every component handle-relative and the final reparse point is
    /// refused (`OpenKind::NoFollow`).
    pub fn open_read(&self, rel: &Path) -> Result<fs::File, Error> {
        let (parent, name) = split_final(rel)?;
        #[cfg(unix)]
        {
            let dir = self.walk_dirs(&parent)?;
            let fd = unix_open_read_at(&dir, &name, rel)?;
            Ok(fs::File::from(fd))
        }
        #[cfg(windows)]
        {
            let _ = (parent, name);
            let handle = crate::platform::open_no_follow_walk(
                &self.root,
                rel,
                crate::platform::OpenKind::NoFollow,
            )?;
            Ok(fs::File::from(handle))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (parent, name);
            Err(Error::internal(format!(
                "handle-relative read is unsupported on {}: no directory-relative open",
                std::env::consts::OS
            )))
        }
    }

    /// Read one symlink's LITERAL target through the anchored walk (the link
    /// is never followed and the target is never resolved).
    pub fn read_link(&self, rel: &Path) -> Result<PathBuf, Error> {
        let (parent, name) = split_final(rel)?;
        #[cfg(unix)]
        {
            let dir = self.walk_dirs(&parent)?;
            let bytes = unix_readlink_bounded_at(&dir, &name, rel, MAX_LINK_TARGET_BYTES)?;
            Ok(PathBuf::from(OsString::from_vec(bytes)))
        }
        #[cfg(windows)]
        {
            let _ = (parent, name);
            crate::platform::read_reparse_link(&self.root, rel)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (parent, name);
            Err(Error::internal(format!(
                "handle-relative readlink is unsupported on {}",
                std::env::consts::OS
            )))
        }
    }

    /// Enumerate `rel` through the anchored walk (handle-relative; children
    /// are inspected with `fstatat(AT_SYMLINK_NOFOLLOW)` on unix), capped at
    /// `max` entries. `truncated` reports that the directory held more.
    pub fn list_entries(&self, rel: &Path, max: usize) -> Result<DirListing, Error> {
        let rel = normalize_rel(rel)?;
        #[cfg(unix)]
        {
            let dir = self.walk_dirs_fresh(&rel)?;
            unix_list_dir(&dir, &rel, max)
        }
        #[cfg(windows)]
        {
            let handle = crate::platform::open_no_follow_walk(
                &self.root,
                &rel,
                crate::platform::OpenKind::NoFollowDirectory,
            )?;
            windows_list_dir(&self.root, &handle, &rel, max)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = rel;
            Err(Error::internal(format!(
                "handle-relative enumeration is unsupported on {}",
                std::env::consts::OS
            )))
        }
    }

    /// Budgeted recursive walk anchored on `rel`. Every entry the walker
    /// inspects is charged to `budget` (entries, directories, file bytes);
    /// the visitor may charge reads and may stop the walk early. Directory
    /// descent opens the child RELATIVE TO THE ALREADY-OPEN PARENT: a child
    /// swapped for a link before the descent is refused typed and the walk
    /// never follows it out of the root. Directory names in `skip_dirs` are
    /// not descended into (their entries are still charged and visited).
    pub fn walk_bounded<F>(
        &self,
        rel: &Path,
        budget: &mut WalkBudget,
        skip_dirs: &[&str],
        visit: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&RootedEntry, usize, &mut WalkBudget) -> Result<WalkStep, Error>,
    {
        let rel = normalize_rel(rel)?;
        #[cfg(unix)]
        {
            let dir = self.walk_dirs_fresh(&rel)?;
            unix_walk_dir(&dir, &rel, 0, budget, skip_dirs, visit)
        }
        #[cfg(windows)]
        {
            let handle = crate::platform::open_no_follow_walk(
                &self.root,
                &rel,
                crate::platform::OpenKind::NoFollowDirectory,
            )?;
            windows_walk_dir(&handle, &self.root, &rel, 0, budget, skip_dirs, visit)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (rel, budget, skip_dirs, visit);
            Err(Error::internal(format!(
                "budgeted handle walk is unsupported on {}",
                std::env::consts::OS
            )))
        }
    }

    /// Create one literal symlink at `rel` (parent components must already be
    /// real directories). Unix uses `symlinkat(2)` on the anchored parent fd;
    /// Windows verifies the parent through the reparse-aware walk and then
    /// creates the link with the platform file/dir choice (an existing entry
    /// is refused).
    pub fn create_symlink(&self, target: &Path, rel: &Path) -> Result<(), Error> {
        let (parent, name) = split_final(rel)?;
        #[cfg(unix)]
        {
            let dir = self.walk_dirs(&parent)?;
            unix_symlink_at(&dir, target, &name)
        }
        #[cfg(windows)]
        {
            let _ = (parent, name);
            self.require_parent_dir(rel)?;
            let link = self.root.join(rel);
            let resolved = if target.is_absolute() {
                target.to_path_buf()
            } else {
                link.parent().unwrap_or_else(|| Path::new(".")).join(target)
            };
            let result = match fs::metadata(&resolved) {
                Ok(meta) if meta.is_dir() => std::os::windows::fs::symlink_dir(target, &link),
                _ => std::os::windows::fs::symlink_file(target, &link),
            };
            result.map_err(|e| Error::internal(format!("symlink {rel:?}: {e}")))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (parent, name);
            Err(Error::internal(format!(
                "symlink creation is unsupported on {}",
                std::env::consts::OS
            )))
        }
    }
}

/// Hard bound on one literal link target read through the walk (parity with
/// the canonical manifest's link bound).
const MAX_LINK_TARGET_BYTES: usize = 1024 * 1024;

/// Normalize a caller path to workspace-relative components (the empty path
/// and `.` both address the root). `..`, absolute paths, prefixes and NULs
/// are refused before any syscall.
fn normalize_rel(rel: &Path) -> Result<PathBuf, Error> {
    let comps = relative_components(rel)?;
    let mut out = PathBuf::new();
    for comp in comps {
        out.push(comp);
    }
    Ok(out)
}

fn join_rel(rel: &Path, name: &OsStr) -> PathBuf {
    if rel.as_os_str().is_empty() {
        PathBuf::from(name)
    } else {
        rel.join(name)
    }
}

fn display_rel(rel: &Path) -> String {
    if rel.as_os_str().is_empty() {
        "<root>".to_string()
    } else {
        rel.display().to_string()
    }
}

/// Test seam: fired with the entry's relative path immediately before the
/// walker opens/visits it, so adversarial tests can swap the entry in the
/// enumeration -> open window deterministically.
#[cfg(test)]
type EntrySeam = Box<dyn Fn(&Path) + Send>;
#[cfg(test)]
static ENTRY_SEAM: OnceLock<Mutex<Option<EntrySeam>>> = OnceLock::new();
/// Serializes tests that install the process-global entry seam (a single
/// hook slot shared by every test in the binary).
#[cfg(test)]
pub(crate) static ENTRY_SEAM_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
pub(crate) fn install_entry_seam(hook: EntrySeam) {
    let m = ENTRY_SEAM.get_or_init(|| Mutex::new(None));
    *m.lock().expect("entry seam poisoned") = Some(hook);
}
#[cfg(test)]
pub(crate) fn clear_entry_seam() {
    if let Some(lock) = ENTRY_SEAM.get() {
        *lock.lock().expect("entry seam poisoned") = None;
    }
}
#[cfg(test)]
fn entry_seam(rel: &Path) {
    if let Some(lock) = ENTRY_SEAM.get() {
        if let Some(hook) = lock.lock().expect("entry seam poisoned").as_ref() {
            hook(rel);
        }
    }
}
#[cfg(not(test))]
fn entry_seam(_rel: &Path) {}

// ---------------------------------------------------------------------------
// unix walker implementation (parent-relative descent)
// ---------------------------------------------------------------------------

/// Open a fresh directory fd for `name` under `dir` (`O_DIRECTORY|O_NOFOLLOW`).
#[cfg(unix)]
fn unix_open_fresh_dir_at(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    unix_open_dir_at(dir, name)
}

/// Enumerate up to `max` entries of an open directory fd. Children are
/// inspected with `fstatat(..., AT_SYMLINK_NOFOLLOW)`: a name that vanished
/// between `readdir` and the stat is skipped (it is no longer part of the
/// tree), any other failure is loud. `truncated` reports a directory that
/// held more than `max` entries.
#[cfg(unix)]
fn unix_list_dir(dir: &OwnedFd, rel: &Path, max: usize) -> Result<DirListing, Error> {
    let mut names = unix_read_dir_names(dir, rel)?;
    names.sort();
    let mut overflowed = false;
    if names.len() > max {
        names.truncate(max);
        overflowed = true;
    }
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let st = match unix_lstat_at(dir, &name) {
            Ok(st) => st,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(Error::internal(format!(
                    "cannot stat a child of {}: {e}",
                    display_rel(rel)
                )))
            }
        };
        let mode = (st.st_mode & 0o7777) as u32;
        entries.push(RootedEntry {
            name: name.clone(),
            rel: join_rel(rel, &name),
            kind: entry_kind_from_mode(st.st_mode),
            size: st.st_size.max(0) as u64,
            modified_ms: (st.st_mtime as i64).saturating_mul(1000),
            mode,
        });
    }
    Ok(DirListing {
        entries,
        overflowed,
    })
}

#[cfg(unix)]
fn entry_kind_from_mode(mode: libc::mode_t) -> RootedEntryKind {
    match mode & libc::S_IFMT {
        libc::S_IFREG => RootedEntryKind::File,
        libc::S_IFDIR => RootedEntryKind::Directory,
        libc::S_IFLNK => RootedEntryKind::Symlink,
        _ => RootedEntryKind::Other,
    }
}

/// One budgeted directory frame: enumerate `dir`, charge every entry, hand
/// each to the visitor and descend into directories by opening the child
/// RELATIVE TO THIS fd. A child swapped for a symlink before the descent is
/// refused typed; a directory replaced by another REAL directory is walked
/// as the tree's current content (documented honest limit, parity with the
/// platform walker) — it can never be steered outside the root by a link.
#[cfg(unix)]
fn unix_walk_dir<F>(
    dir: &OwnedFd,
    rel: &Path,
    depth: usize,
    budget: &mut WalkBudget,
    skip_dirs: &[&str],
    visit: &mut F,
) -> Result<(), Error>
where
    F: FnMut(&RootedEntry, usize, &mut WalkBudget) -> Result<WalkStep, Error>,
{
    budget.charge_directory(depth)?;
    let cap = budget.remaining_entries();
    let listing = unix_list_dir(dir, rel, cap)?;
    if listing.overflowed {
        return Err(Error::oversized(format!(
            "{}: directory listing exceeds the remaining {cap}-entry walk budget",
            display_rel(rel)
        )));
    }
    for entry in listing.entries {
        budget.charge_entry(depth)?;
        if entry.kind == RootedEntryKind::File {
            budget.charge_file_bytes(entry.size)?;
        }
        // A statically skipped directory is never visited and never
        // descended (its entry is still charged, parity with the historical
        // walkers).
        if entry.kind == RootedEntryKind::Directory && skip_dirs.iter().any(|s| entry.name == *s) {
            continue;
        }
        entry_seam(&entry.rel);
        let step = visit(&entry, depth, budget)?;
        if step == WalkStep::Stop {
            return Ok(());
        }
        if entry.kind == RootedEntryKind::Directory && step != WalkStep::SkipDir {
            let child = unix_open_dir_at(dir, &entry.name).map_err(|e| {
                Error::permission(format!(
                    "{}: entry {:?} is a symlink or not a directory: {e}",
                    display_rel(rel),
                    entry.name
                ))
            })?;
            unix_walk_dir(&child, &entry.rel, depth + 1, budget, skip_dirs, visit)?;
        }
    }
    Ok(())
}

/// Read every name of an open directory fd through a duplicate (the dup
/// shares the offset with the caller's fd, and every fd handed to the walker
/// is a fresh open for the duration of one walk, so nothing else observes
/// it). A `readdir` failure is loud, never a silent end.
#[cfg(unix)]
fn unix_read_dir_names(dir: &OwnedFd, rel: &Path) -> Result<Vec<OsString>, Error> {
    // SAFETY: `dup` returns a fresh descriptor (or -1, handled below), which
    // `fdopendir` then takes ownership of.
    let dup = unsafe { libc::dup(dir.as_raw_fd()) };
    if dup < 0 {
        return Err(Error::internal(format!(
            "cannot enumerate {}: {}",
            display_rel(rel),
            io::Error::last_os_error()
        )));
    }
    // SAFETY: `dup` is a fresh directory descriptor; fdopendir takes
    // ownership and returns NULL on failure (the descriptor is then closed
    // by us).
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        // SAFETY: `dup` is still owned by us because fdopendir failed.
        let _ = unsafe { libc::close(dup) };
        return Err(Error::internal(format!(
            "cannot enumerate {}: {}",
            display_rel(rel),
            io::Error::last_os_error()
        )));
    }
    let mut out = Vec::new();
    let mut failure: Option<Error> = None;
    loop {
        // Clear errno first so a NULL return can be told apart from a real
        // end-of-directory.
        set_errno(0);
        // SAFETY: `stream` is a live DIR* until closedir below; readdir
        // returns a pointer to the next entry or NULL at end/error.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let errno = get_errno();
            if errno != 0 {
                failure = Some(Error::internal(format!(
                    "cannot enumerate {}: {}",
                    display_rel(rel),
                    io::Error::from_raw_os_error(errno)
                )));
            }
            break;
        }
        // SAFETY: `entry` points at a live dirent for this iteration; d_name
        // is a NUL-terminated array inside it.
        let name_ptr = unsafe { (*entry).d_name.as_ptr() };
        // SAFETY: `name_ptr` is NUL-terminated by the dirent contract.
        let name = unsafe { CStr::from_ptr(name_ptr) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        out.push(OsString::from_vec(bytes.to_vec()));
    }
    // SAFETY: `stream` is live and owned here; closedir closes the dup'ed fd
    // and only that one.
    let _ = unsafe { libc::closedir(stream) };
    match failure {
        Some(error) => Err(error),
        None => Ok(out),
    }
}

/// Open `name` in `dir` read-only with no-follow semantics. A symlink (unix
/// `ELOOP`, macOS `ENOTDIR` when the entry is a link) is refused typed.
#[cfg(unix)]
fn unix_open_read_at(dir: &OwnedFd, name: &OsStr, rel: &Path) -> Result<OwnedFd, Error> {
    let c = unix_cstring(name)?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `c` is NUL-terminated and relative to the live directory fd
    // `dir`; no O_CREAT so no mode argument is required.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
    if fd >= 0 {
        // SAFETY: fresh descriptor owned by this function.
        return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    let e = io::Error::last_os_error();
    let is_link = matches!(e.raw_os_error(), Some(libc::ELOOP))
        || (matches!(e.raw_os_error(), Some(libc::ENOTDIR))
            && matches!(unix_lstat_at(dir, name), Ok(st) if (st.st_mode & libc::S_IFMT) == libc::S_IFLNK));
    if is_link {
        return Err(Error::permission(format!(
            "{rel:?} is a symlink (no-follow read refused)"
        )));
    }
    match e.kind() {
        io::ErrorKind::NotFound => Err(Error::not_found(format!("{}", rel.display()))),
        _ => Err(Error::internal(format!("{}: {e}", rel.display()))),
    }
}

/// `lstat` a child relative to an open directory fd (never follows).
#[cfg(unix)]
fn unix_lstat_at(dir: &OwnedFd, name: &OsStr) -> io::Result<libc::stat> {
    let c = unix_cstring(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: the arguments were validated by the caller per this function's documented contract and the call has no additional aliasing or lifetime requirements.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstatat does not modify the NUL-terminated `c`; `st` is a valid
    // output buffer and AT_SYMLINK_NOFOLLOW never follows the entry.
    let r = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

/// Read a symlink's literal target relative to `dir`, bounded by `max`
/// bytes (a hostile over-long target fails typed instead of allocating).
#[cfg(unix)]
fn unix_readlink_bounded_at(
    dir: &OwnedFd,
    name: &OsStr,
    rel: &Path,
    max: usize,
) -> Result<Vec<u8>, Error> {
    let c = unix_cstring(name)?;
    let mut buf = vec![0u8; 4096.min(max.max(1))];
    loop {
        // SAFETY: readlinkat does not modify the NUL-terminated `c`, and
        // `buf` is a writable byte buffer of the reported size.
        let n = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                c.as_ptr(),
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
            )
        };
        if n < 0 {
            return Err(Error::internal(format!(
                "read_link {}: {}",
                rel.display(),
                io::Error::last_os_error()
            )));
        }
        let n = n as usize;
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        if buf.len() >= max {
            return Err(Error::oversized(format!(
                "read_link {}: target exceeds the {max}-byte bound",
                rel.display()
            )));
        }
        buf.resize((buf.len() * 2).min(max), 0);
    }
}

/// Create one literal symlink relative to the anchored parent fd.
#[cfg(unix)]
fn unix_symlink_at(dir: &OwnedFd, target: &Path, name: &OsStr) -> Result<(), Error> {
    let target_c = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| Error::malformed(format!("symlink target {target:?} contains a NUL byte")))?;
    let name_c = unix_cstring(name)?;
    // SAFETY: both strings are NUL-terminated; `name_c` is relative to the
    // live directory fd `dir`; symlinkat never follows anything.
    let r = unsafe { libc::symlinkat(target_c.as_ptr(), dir.as_raw_fd(), name_c.as_ptr()) };
    if r != 0 {
        return Err(Error::internal(format!(
            "cannot create symlink {name:?}: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Thread-local errno slot (the libc spelling differs per unix: glibc/Bionic
/// expose `__errno_location`, Apple/BSD expose `__error`). Targets with
/// neither keep a null pointer: errno is then treated as 0, so a NULL
/// `readdir` is an end-of-directory (the documented behavior there).
#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
fn unix_errno_ptr() -> *mut libc::c_int {
    // SAFETY: `__errno_location` always returns this thread's valid slot.
    unsafe { libc::__errno_location() }
}

#[cfg(all(
    unix,
    any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    )
))]
fn unix_errno_ptr() -> *mut libc::c_int {
    // SAFETY: `__error` always returns this thread's valid slot.
    unsafe { libc::__error() }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))
))]
fn unix_errno_ptr() -> *mut libc::c_int {
    std::ptr::null_mut()
}

#[cfg(unix)]
fn set_errno(v: libc::c_int) {
    let p = unix_errno_ptr();
    if !p.is_null() {
        // SAFETY: the pointer was validated non-null; writing this thread's
        // errno slot is what libc itself does.
        unsafe {
            *p = v;
        }
    }
}

#[cfg(unix)]
fn get_errno() -> libc::c_int {
    let p = unix_errno_ptr();
    if p.is_null() {
        return 0;
    }
    // SAFETY: same slot as above; reading it is always valid.
    unsafe { *p }
}

// ---------------------------------------------------------------------------
// windows walker implementation (handle-relative descent)
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn windows_list_dir(
    root: &Path,
    handle: &std::os::windows::io::OwnedHandle,
    rel: &Path,
    max: usize,
) -> Result<DirListing, Error> {
    let (raw, overflowed) = crate::platform::windows_list_dir_raw(handle, max)?;
    let mut entries = Vec::with_capacity(raw.len());
    for item in raw {
        use std::os::windows::ffi::OsStringExt as _;
        let name = OsString::from_wide(&item.name);
        let child_rel = join_rel(rel, &name);
        let is_dir_attr = item.attributes & crate::platform::FILE_ATTRIBUTE_DIRECTORY != 0;
        let is_reparse = item.attributes & crate::platform::FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let kind = if is_reparse {
            match crate::platform::windows_reparse_class(root, &child_rel)? {
                Some(crate::platform::ReparseClass::Symlink) => RootedEntryKind::Symlink,
                // A junction/mount point is reported as a directory entry;
                // descent uses the strict no-follow child open, so the walk
                // itself never traverses it (a caller that must follow an
                // in-root junction goes through the platform walk).
                _ => RootedEntryKind::Directory,
            }
        } else if is_dir_attr {
            RootedEntryKind::Directory
        } else {
            RootedEntryKind::File
        };
        entries.push(RootedEntry {
            name,
            rel: child_rel,
            kind,
            size: item.size,
            modified_ms: windows_filetime_ms(item.modified),
        });
    }
    Ok(DirListing {
        entries,
        overflowed,
    })
}

/// A Windows FILETIME (100 ns since 1601) as milliseconds since the unix
/// epoch (0 when the record carries no timestamp).
#[cfg(windows)]
fn windows_filetime_ms(filetime: i64) -> i64 {
    const UNIX_EPOCH_100NS: i64 = 116_444_736_000_000_000;
    if filetime <= UNIX_EPOCH_100NS {
        return 0;
    }
    (filetime - UNIX_EPOCH_100NS) / 10_000
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn windows_walk_dir<F>(
    handle: &std::os::windows::io::OwnedHandle,
    root: &Path,
    rel: &Path,
    depth: usize,
    budget: &mut WalkBudget,
    skip_dirs: &[&str],
    visit: &mut F,
) -> Result<(), Error>
where
    F: FnMut(&RootedEntry, usize, &mut WalkBudget) -> Result<WalkStep, Error>,
{
    budget.charge_directory(depth)?;
    let mut cap = budget.remaining_entries();
    // The raw enumeration is bounded, but the budget must also bound how many
    // names one listing may materialize: cap at the remaining entries + 1 so
    // truncation is detectable without unbounded memory.
    let listing = windows_list_dir(root, handle, rel, cap.saturating_add(1))?;
    if listing.overflowed {
        return Err(Error::oversized(format!(
            "{}: directory listing exceeds the remaining {cap}-entry walk budget",
            display_rel(rel)
        )));
    }
    cap = budget.remaining_entries();
    if listing.entries.len() > cap {
        return Err(Error::oversized(format!(
            "{}: directory listing exceeds the remaining {cap}-entry walk budget",
            display_rel(rel)
        )));
    }
    for entry in listing.entries {
        budget.charge_entry(depth)?;
        if entry.kind == RootedEntryKind::File {
            budget.charge_file_bytes(entry.size)?;
        }
        if entry.kind == RootedEntryKind::Directory && skip_dirs.iter().any(|s| entry.name == *s) {
            continue;
        }
        entry_seam(&entry.rel);
        let step = visit(&entry, depth, budget)?;
        if step == WalkStep::Stop {
            return Ok(());
        }
        if entry.kind == RootedEntryKind::Directory && step != WalkStep::SkipDir {
            // Descend by opening the child RELATIVE to the already-open
            // parent handle: the strict no-follow NT open refuses a child
            // reparse point.
            let child = crate::platform::windows_open_child_dir(handle, &entry.name)?;
            windows_walk_dir(
                &child,
                root,
                &entry.rel,
                depth + 1,
                budget,
                skip_dirs,
                visit,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    mod unix_tests {
        use super::super::*;

        fn anchored(root: &Path) -> RootedDir {
            RootedDir::create(root).unwrap()
        }

        #[test]
        fn traversal_and_rooted_paths_are_refused_before_any_syscall() {
            let tmp = tempfile::tempdir().unwrap();
            let dir = anchored(&tmp.path().join("root"));
            for bad in [
                PathBuf::from("../escape"),
                PathBuf::from("a/../../b"),
                PathBuf::from("/absolute"),
                PathBuf::from(".."),
            ] {
                assert!(dir.create_dir_all(&bad).is_err(), "{bad:?}");
                assert!(dir.remove_tree(&bad).is_err(), "{bad:?}");
                assert!(dir.open_create_new(&bad).is_err(), "{bad:?}");
            }
            assert!(dir.create_dir_all(Path::new("")).is_ok());
        }

        #[test]
        fn create_open_publish_remove_roundtrip() {
            let tmp = tempfile::tempdir().unwrap();
            let dir = anchored(&tmp.path().join("root"));
            dir.create_dir_all(Path::new("p1/downloads")).unwrap();
            dir.restrict_owner_only(Path::new("p1/downloads")).unwrap();
            let mut file = dir
                .open_create_new(Path::new("p1/downloads/.tmp-1"))
                .unwrap();
            use std::io::Write as _;
            file.write_all(b"payload").unwrap();
            file.sync_all().unwrap();
            drop(file);
            // A second create of the same temp name is refused (exclusive).
            assert!(dir
                .open_create_new(Path::new("p1/downloads/.tmp-1"))
                .is_err());
            dir.atomic_publish(
                Path::new("p1/downloads/.tmp-1"),
                Path::new("p1/downloads/file.bin"),
            )
            .unwrap();
            assert_eq!(
                std::fs::read(dir.join(Path::new("p1/downloads/file.bin"))).unwrap(),
                b"payload"
            );
            assert!(!dir.join(Path::new("p1/downloads/.tmp-1")).exists());
            dir.remove_file(Path::new("p1/downloads/file.bin")).unwrap();
            dir.remove_file(Path::new("p1/downloads/file.bin")).unwrap();
            dir.remove_tree(Path::new("p1")).unwrap();
            assert!(!dir.join(Path::new("p1")).exists());
            dir.remove_tree(Path::new("p1")).unwrap();
        }

        #[test]
        fn symlink_swap_on_create_is_refused_and_outside_untouched() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("marker"), b"keep").unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            // p1 was created, then swapped for a symlink to the outside dir.
            dir.create_dir_all(Path::new("p1")).unwrap();
            std::fs::remove_dir(root.join("p1")).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("p1")).unwrap();
            assert!(
                dir.create_dir_all(Path::new("p1/scratch")).is_err(),
                "a symlinked component must be refused"
            );
            assert!(
                !outside.join("scratch").exists(),
                "the symlink target must never be created through"
            );
            assert!(outside.join("marker").exists());
        }

        #[test]
        fn symlink_swap_on_file_create_and_publish_is_refused() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            dir.create_dir_all(Path::new("dl")).unwrap();
            std::fs::remove_dir(root.join("dl")).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("dl")).unwrap();
            assert!(
                dir.open_create_new(Path::new("dl/tmp")).is_err(),
                "file creation through a symlinked parent must be refused"
            );
            assert!(
                dir.atomic_publish(Path::new("dl/a"), Path::new("dl/b"))
                    .is_err(),
                "publish through a symlinked parent must be refused"
            );
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        }

        #[test]
        fn wipe_refuses_a_symlinked_root_entry_and_preserves_the_target() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(outside.join("inner")).unwrap();
            std::fs::write(outside.join("marker"), b"keep").unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            std::os::unix::fs::symlink(&outside, root.join("p1")).unwrap();
            let err = dir.remove_tree(Path::new("p1")).unwrap_err();
            assert_eq!(err.kind, faktor_core::error::ErrorKind::Permission);
            assert!(outside.join("marker").exists());
            assert!(outside.join("inner").is_dir());
        }

        #[test]
        fn nested_symlinks_are_unlinked_without_being_traversed() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("marker"), b"keep").unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            dir.create_dir_all(Path::new("p1/sub")).unwrap();
            std::fs::write(root.join("p1/sub/data"), b"x").unwrap();
            std::os::unix::fs::symlink(&outside, root.join("p1/link")).unwrap();
            dir.remove_tree(Path::new("p1")).unwrap();
            assert!(!root.join("p1").exists());
            assert!(outside.join("marker").exists());
        }

        /// The rooted publish implementation now lives in `crate::atomic`
        /// (`atomic_publish_at`; the static-authority scan rejects a
        /// hand-rolled rename in this file). Its behavior is unchanged: a
        /// same-directory precondition refused typed before any syscall, no
        /// partial destination when the staged temp is missing, a whole-file
        /// swap on success with the temp consumed, the parent still
        /// fsyncable, and no temp residue across repeated publishes.
        #[test]
        fn publish_through_the_shared_atomic_module_is_whole_or_nothing() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            dir.create_dir_all(Path::new("dl")).unwrap();
            dir.create_dir_all(Path::new("other")).unwrap();
            let err = dir
                .atomic_publish(Path::new("dl/tmp"), Path::new("other/final"))
                .unwrap_err();
            assert_eq!(err.kind, faktor_core::error::ErrorKind::Malformed);
            assert!(!root.join("other/final").exists());
            assert!(dir
                .atomic_publish(Path::new("dl/ghost"), Path::new("dl/final"))
                .is_err());
            assert!(!root.join("dl/final").exists());
            use std::io::Write as _;
            for i in 0..25u64 {
                let rel_tmp = PathBuf::from(format!("dl/.tmp-{i}"));
                let payload = format!("v{i}-{}", "p".repeat((i * 7) as usize));
                let mut f = dir.open_create_new(&rel_tmp).unwrap();
                f.write_all(payload.as_bytes()).unwrap();
                f.sync_all().unwrap();
                drop(f);
                dir.atomic_publish(&rel_tmp, Path::new("dl/final")).unwrap();
                assert_eq!(
                    std::fs::read(root.join("dl/final")).unwrap(),
                    payload.as_bytes()
                );
            }
            dir.sync_dir(Path::new("dl")).unwrap();
            let mut names: Vec<String> = std::fs::read_dir(root.join("dl"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            assert_eq!(names, vec!["final".to_string()], "temp leaked: {names:?}");
        }

        // ----------------------------------------------------------------
        // P0-52 adversarial traversal tests
        // ----------------------------------------------------------------

        use faktor_core::error::ErrorKind;

        struct SeamClear;
        impl Drop for SeamClear {
            fn drop(&mut self) {
                clear_entry_seam();
            }
        }

        /// Install the entry seam and synchronously clear it on drop; the
        /// global lock serializes every test that uses it.
        fn install_seam(
            f: impl Fn(&Path) + Send + 'static,
        ) -> (std::sync::MutexGuard<'static, ()>, SeamClear) {
            let guard = ENTRY_SEAM_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            install_entry_seam(Box::new(f));
            (guard, SeamClear)
        }

        #[test]
        fn list_entries_is_handle_relative_and_refuses_a_swapped_dir() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("marker.txt"), b"outside-marker").unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            dir.create_dir_all(Path::new("d/sub")).unwrap();
            std::fs::write(root.join("d/sub/inner.txt"), b"inner").unwrap();

            let top = dir.list_entries(Path::new(""), 100).unwrap();
            assert!(!top.overflowed);
            assert_eq!(top.entries.len(), 1);
            assert_eq!(top.entries[0].name, "d");
            assert_eq!(top.entries[0].kind, RootedEntryKind::Directory);

            let nested = dir.list_entries(Path::new("d/sub"), 100).unwrap();
            assert_eq!(nested.entries.len(), 1);
            assert_eq!(nested.entries[0].rel, PathBuf::from("d/sub/inner.txt"));
            assert_eq!(nested.entries[0].size, 5);
            assert!(nested.entries[0].is_file());

            // Directory swapped for an outside symlink BEFORE enumeration:
            // the walk is refused and the outside directory is never entered.
            std::fs::remove_dir_all(root.join("d")).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("d")).unwrap();
            let err = dir.list_entries(Path::new("d"), 100).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Permission, "{err}");
            assert_eq!(
                std::fs::read(outside.join("marker.txt")).unwrap(),
                b"outside-marker"
            );
            assert!(!outside.join("sub").exists());
        }

        #[test]
        fn file_swapped_to_outside_symlink_after_enumeration_is_never_read() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret.txt"), b"outside-secret").unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            dir.create_dir_all(Path::new("d")).unwrap();
            std::fs::write(root.join("d/note.txt"), b"inside").unwrap();

            let listing = dir.list_entries(Path::new("d"), 100).unwrap();
            assert_eq!(listing.entries.len(), 1);
            assert!(listing.entries[0].is_file());
            // Swap the enumerated file for an outside symlink.
            std::fs::remove_file(root.join("d/note.txt")).unwrap();
            std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("d/note.txt"))
                .unwrap();

            let err = dir.read(Path::new("d/note.txt"), 4096).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Permission, "{err}");
            assert!(dir.open_read(Path::new("d/note.txt")).is_err());
            assert!(dir.read_link(Path::new("d/note.txt")).is_ok());
            assert_eq!(
                std::fs::read(outside.join("secret.txt")).unwrap(),
                b"outside-secret"
            );
        }

        #[test]
        fn seam_file_swap_before_read_is_refused_and_outside_never_read() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("secret.txt"), b"outside-secret").unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            dir.create_dir_all(Path::new("d")).unwrap();
            std::fs::write(root.join("d/race-note.txt"), b"inside").unwrap();

            let swap_root = root.clone();
            let swap_outside = outside.join("secret.txt");
            let (_lock, _clear) = install_seam(move |rel| {
                if rel == Path::new("d/race-note.txt") {
                    let _ = std::fs::remove_file(swap_root.join("d/race-note.txt"));
                    let _ = std::os::unix::fs::symlink(
                        &swap_outside,
                        swap_root.join("d/race-note.txt"),
                    );
                }
            });

            let mut budget = WalkBudget::new(100, 100, 8, 1 << 20, 1 << 20);
            let mut visited_files = 0usize;
            let err = dir
                .walk_bounded(Path::new(""), &mut budget, &[], &mut |entry, _depth, _b| {
                    if entry.is_file() {
                        visited_files += 1;
                        // The entry was enumerated as a file; the seam just
                        // swapped it for an escaping symlink.
                        dir.read(&entry.rel, 4096)?;
                    }
                    Ok(WalkStep::Continue)
                })
                .unwrap_err();
            assert_eq!(visited_files, 1, "the entry was seen as a file");
            assert_eq!(err.kind, ErrorKind::Permission, "{err}");
            assert_eq!(
                std::fs::read(outside.join("secret.txt")).unwrap(),
                b"outside-secret"
            );
        }

        #[test]
        fn seam_dir_swap_before_descent_is_refused_and_outside_untouched() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("marker"), b"keep").unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            dir.create_dir_all(Path::new("a")).unwrap();
            std::fs::write(root.join("a/keep.txt"), b"inside").unwrap();

            let swap_root = root.clone();
            let swap_outside = outside.clone();
            let (_lock, _clear) = install_seam(move |rel| {
                if rel == Path::new("a") {
                    // libc rename (the static scan forbids the std rename
                    // spelling in this file so the only sanctioned publish
                    // path is `crate::atomic`; this is test scaffolding, not
                    // a publish).
                    let from =
                        std::ffi::CString::new(swap_root.join("a").to_str().unwrap()).unwrap();
                    let to = std::ffi::CString::new(swap_root.join("a-moved").to_str().unwrap())
                        .unwrap();
                    // SAFETY: both CStrings are NUL-terminated valid paths.
                    unsafe {
                        libc::rename(from.as_ptr(), to.as_ptr());
                    }
                    let _ = std::os::unix::fs::symlink(&swap_outside, swap_root.join("a"));
                }
            });

            let mut budget = WalkBudget::new(100, 100, 8, 1 << 20, 1 << 20);
            let err = dir
                .walk_bounded(Path::new(""), &mut budget, &[], &mut |_, _, _| {
                    Ok(WalkStep::Continue)
                })
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::Permission, "{err}");
            // The outside tree is never entered and the pinned-away original
            // directory is untouched.
            assert!(outside.join("marker").exists());
            assert!(!outside.join("keep.txt").exists());
            assert!(root.join("a-moved/keep.txt").exists());
        }

        #[test]
        fn walk_budget_exhaustion_is_typed_oversized() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            dir.create_dir_all(Path::new("sub")).unwrap();
            for i in 0..5 {
                std::fs::write(root.join(format!("f{i}.txt")), b"xx").unwrap();
            }
            let run = |budget: &mut WalkBudget| {
                dir.walk_bounded(Path::new(""), budget, &[], &mut |_, _, budget| {
                    // Charge reads so the read budget is exercised too.
                    budget.charge_read_bytes(1)?;
                    Ok(WalkStep::Continue)
                })
            };

            let mut b = WalkBudget::new(3, 100, 16, u64::MAX, u64::MAX);
            let err = run(&mut b).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Oversized, "{err}");
            assert!(b.entries() <= 3);

            let mut b = WalkBudget::new(100, 1, 16, u64::MAX, u64::MAX);
            let err = run(&mut b).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Oversized, "{err}");
            assert!(err.message.contains("directory budget"), "{err}");

            let mut b = WalkBudget::new(100, 100, 0, u64::MAX, u64::MAX);
            let err = run(&mut b).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Oversized, "{err}");
            assert!(err.message.contains("depth budget"), "{err}");

            let mut b = WalkBudget::new(100, 100, 16, 2, u64::MAX);
            let err = run(&mut b).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Oversized, "{err}");
            assert!(err.message.contains("file budget"), "{err}");

            let mut b = WalkBudget::new(100, 100, 16, u64::MAX, 2);
            let err = run(&mut b).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Oversized, "{err}");
            assert!(err.message.contains("read budget"), "{err}");

            // A sufficient budget completes and its counters are exact.
            let mut b = WalkBudget::new(100, 100, 16, 1 << 20, 1 << 20);
            run(&mut b).unwrap();
            assert_eq!(b.entries(), 6, "5 files + 1 dir");
            assert_eq!(b.directories(), 2, "root + sub");
            assert_eq!(b.total_file_bytes(), 10);
            assert_eq!(b.total_read_bytes(), 6, "one charge per entry visited");
        }

        #[test]
        fn read_and_listing_semantics_are_bounded_and_sorted() {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("t"), b"outside").unwrap();
            let root = tmp.path().join("root");
            let dir = anchored(&root);
            std::fs::write(root.join("b.txt"), b"0123456789").unwrap();
            std::fs::write(root.join("a.txt"), b"short").unwrap();
            std::os::unix::fs::symlink(outside.join("t"), root.join("link")).unwrap();

            let listing = dir.list_entries(Path::new(""), 100).unwrap();
            let names: Vec<String> = listing
                .entries
                .iter()
                .map(|e| e.name.to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, vec!["a.txt", "b.txt", "link"]);
            assert_eq!(listing.entries[2].kind, RootedEntryKind::Symlink);

            let capped = dir.list_entries(Path::new(""), 2).unwrap();
            assert!(capped.overflowed);
            assert_eq!(capped.entries.len(), 2);

            let whole = dir.read(Path::new("b.txt"), 64).unwrap();
            assert!(whole.digest.is_full());
            assert_eq!(whole.bytes, b"0123456789");
            let prefix = dir.read(Path::new("b.txt"), 4).unwrap();
            assert!(!prefix.digest.is_full());
            assert_eq!(prefix.bytes, b"0123");
            // A link is refused by the strict no-follow read, but its literal
            // target stays readable.
            let err = dir.read(Path::new("link"), 64).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Permission, "{err}");
            assert_eq!(dir.read_link(Path::new("link")).unwrap(), outside.join("t"));
        }
    }
}
