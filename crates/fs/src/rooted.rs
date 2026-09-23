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

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use faktor_core::error::Error;

#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::sync::Arc;

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
    }
}
