//! The stable bootstrap launcher entry.
//!
//! The install layout keeps ONE `launcher` file that is copied once and
//! never replaced by an update. Everything that starts the daemon through an
//! install layout starts THIS file: it resolves the activated release through
//! the authenticated pointer (signature against `trusted-keys.json` + digest
//! re-hash of `versions/<release-id>/faktor`), and then execs the exact
//! verified bytes. A pointer swap can therefore only change which
//! AUTHENTICATED release runs — never the launcher itself, and never an
//! unsigned binary.
//!
//! Invocation contract:
//!
//! - `install/launcher <args...>` (the file name IS the mode): the install
//!   root is the launcher's own directory;
//! - `FAKTOR_LAUNCHER_INSTALL_ROOT=<root> <faktor-cli> <args...>`: the
//!   explicit mode (scripts/tests) for a binary that is not named `launcher`;
//! - `faktor-cli bootstrap --install-root <root> -- <args...>`: the clap
//!   entry point.
//!
//! Refusals are loud and NEVER fall back to a different binary (exit code
//! [`EXIT_LAUNCH_REFUSED`]). On unix the exec replaces the bootstrap process,
//! so the supervisor observes one live process running the release.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use faktor_updater::{LaunchInputs, TrustFile};

/// Exit code of a refused launch (distinct from the daemon's exit 1).
pub const EXIT_LAUNCH_REFUSED: i32 = 3;

/// Detect the bootstrap mode BEFORE clap parses: the executable is named
/// `launcher` and a version layout sits beside it, or the explicit
/// `FAKTOR_LAUNCHER_INSTALL_ROOT` override names one. Returns the install
/// root to bootstrap from.
pub fn launcher_invocation() -> Option<PathBuf> {
    if let Ok(root) = std::env::var(faktor_updater::LAUNCHER_ROOT_ENV) {
        if !root.is_empty() {
            return Some(PathBuf::from(root));
        }
    }
    launcher_invocation_at(&std::env::current_exe().ok()?)
}

/// The pure detection over one executable path: the file name must be
/// `launcher` AND an install layout must sit beside it (a `current` pointer
/// or a `versions/` directory). Anything else returns `None`: the caller
/// then falls back to the legacy binary resolution, never to a hard failure.
pub fn launcher_invocation_at(exe: &Path) -> Option<PathBuf> {
    if exe.file_stem()?.to_str()? != faktor_updater::install::LAUNCHER_FILE_NAME {
        return None;
    }
    let root = exe.parent()?.to_path_buf();
    if root.join("current").is_file() || root.join("versions").is_dir() {
        Some(root)
    } else {
        None
    }
}

/// Resolve and exec the activated release. Only returns (via process exit)
/// when the launch is refused; on unix a successful call never returns.
pub fn run_launcher(install_root: PathBuf, args: Vec<OsString>) -> ! {
    let inputs = LaunchInputs::new(install_root);
    let keys = match TrustFile::read(&inputs.trust_path) {
        Ok(keys) => keys,
        Err(e) => refuse(&e.to_string()),
    };
    let target = match faktor_updater::resolve_launch(&inputs, &keys) {
        Ok(target) => target,
        Err(e) => refuse(&e.to_string()),
    };
    eprintln!(
        "launcher: activating release {} (version {}, digest {})",
        target.release_id, target.version, target.digest
    );
    match faktor_updater::launch(&target, &inputs.install_root, &args) {
        Ok(code) => std::process::exit(code),
        Err(e) => refuse(&e.to_string()),
    }
}

fn refuse(detail: &str) -> ! {
    eprintln!("launcher: {detail}");
    std::process::exit(EXIT_LAUNCH_REFUSED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_launcher_beside_an_install_layout_is_a_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("launcher");
        std::fs::write(&launcher, b"#!/bin/sh\n").unwrap();
        // No layout beside it: legacy resolution, never a bootstrap.
        assert!(launcher_invocation_at(&launcher).is_none());
        // A `current` pointer makes it a bootstrap root.
        std::fs::write(dir.path().join("current"), b"{}").unwrap();
        assert_eq!(
            launcher_invocation_at(&launcher),
            Some(dir.path().to_path_buf())
        );
        // A differently named binary is never the launcher, even with a
        // full layout present.
        let other = dir.path().join("faktor-cli");
        std::fs::write(&other, b"x").unwrap();
        assert!(launcher_invocation_at(&other).is_none());
        // A bare `versions/` directory is enough too.
        let dir2 = tempfile::tempdir().unwrap();
        let launcher2 = dir2.path().join("launcher");
        std::fs::write(&launcher2, b"x").unwrap();
        std::fs::create_dir_all(dir2.path().join("versions")).unwrap();
        assert_eq!(
            launcher_invocation_at(&launcher2),
            Some(dir2.path().to_path_buf())
        );
    }
}
