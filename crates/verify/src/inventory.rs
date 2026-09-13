//! Bounded repository discovery with an EXPLICIT completeness verdict
//! (audit: verification must not depend on a partial view of the repo).
//!
//! Repository profiling decides which checks are REQUIRED. A bounded walk
//! that silently stops at an entry cap, truncates a directory listing,
//! skips an unreadable subtree or refuses a symlink can therefore derive a
//! SMALLER passing suite than the repository actually demands. This module
//! makes the walk's limits part of the result: [`RepoInventory`] carries the
//! discovered files AND a typed [`InventoryCompleteness`], and ANY value
//! other than `Complete` prohibits a `Passed` verification verdict — the
//! caller must classify the attempt `Unavailable` with the typed reason.
//!
//! The walk is deterministic (sorted per directory), bounded (files,
//! per-directory page, depth), and symlink-safe: a symlink entry is never
//! followed (so cyclic/hostile links can neither loop nor hide a subtree),
//! and its presence makes the inventory non-Complete (`Unreadable` — the
//! tree could not be vouched for at that path).
//!
//! Caps mirror the integrated-root discovery path: 500 files, 200 entries
//! per directory page, depth 6. Caps are REFUSALS, never truncation.

use std::collections::VecDeque;
use std::path::Path;

/// Hard cap on the files one inventory may certify. A walk that would
/// exceed it reports [`InventoryCompleteness::EntryLimitExceeded`].
pub const MAX_INVENTORY_FILES: usize = 500;
/// Hard cap on one directory's listed children. A directory with more
/// children reports [`InventoryCompleteness::DirectoryPageTruncated`].
pub const MAX_INVENTORY_DIR_PAGE: usize = 200;
/// Hard cap on the scanned depth (the root is depth 0). Any entry beyond
/// this depth reports [`InventoryCompleteness::DepthLimitExceeded`].
pub const MAX_INVENTORY_DEPTH: usize = 6;
/// Internal per-directory iteration bound: a hostile directory with
/// millions of entries is abandoned as a truncated page once this many
/// entries were observed (memory/cpu stay bounded).
const MAX_DIR_SCAN_ENTRIES: usize = 4096;

/// Directories never walked (vcs metadata and dependency/build trees) —
/// the same fixed skip set the bounded evidence/verification walks use.
pub const INVENTORY_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".venv",
    "dist",
];

/// Whether a discovery walk certified the WHOLE repository under its caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryCompleteness {
    /// Every entry within the caps was read; the file list is the whole
    /// bounded view and profiling may proceed.
    Complete,
    /// The file cap was exceeded: the walk saw more files than
    /// [`MAX_INVENTORY_FILES`].
    EntryLimitExceeded,
    /// An entry existed beyond [`MAX_INVENTORY_DEPTH`].
    DepthLimitExceeded,
    /// A directory carried more children than [`MAX_INVENTORY_DIR_PAGE`]
    /// (or more than the internal scan bound).
    DirectoryPageTruncated,
    /// One path could not be read as a plain file/directory entry (a
    /// permission failure, an I/O error, or a symlink the walk refuses to
    /// follow).
    Unreadable { path: String },
}

impl InventoryCompleteness {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// The typed, human-readable reason of a non-Complete verdict.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Complete => None,
            Self::EntryLimitExceeded => Some(format!(
                "repository inventory exceeded the {MAX_INVENTORY_FILES}-file cap; refusing to derive a smaller check suite"
            )),
            Self::DepthLimitExceeded => Some(format!(
                "repository inventory exceeded the depth-{MAX_INVENTORY_DEPTH} cap; a manifest or source beyond the frontier may be missing"
            )),
            Self::DirectoryPageTruncated => Some(format!(
                "repository inventory truncated a directory listing at the {MAX_INVENTORY_DIR_PAGE}-entry page cap; the bounded view is not the whole tree"
            )),
            Self::Unreadable { path } => Some(format!(
                "repository inventory could not read {path:?} (unreadable path or a symlink the walk refuses to follow)"
            )),
        }
    }
}

/// The bounded discovery result: the sorted workspace-relative file paths
/// plus the COMPLETENESS of the walk that produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInventory {
    pub files: Vec<String>,
    pub completeness: InventoryCompleteness,
}

impl Default for RepoInventory {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            completeness: InventoryCompleteness::Complete,
        }
    }
}

impl RepoInventory {
    pub fn is_complete(&self) -> bool {
        self.completeness.is_complete()
    }

    /// `None` when the walk was Complete; otherwise the typed reason a
    /// verification verdict must surface instead of `Passed`.
    pub fn refusal_reason(&self) -> Option<String> {
        self.completeness.reason()
    }
}

fn posix_rel(parts: &[String]) -> String {
    parts.join("/")
}

/// Discover a repository's bounded file inventory. Deterministic
/// (breadth-first, sorted per directory), symlink-safe, and honest about
/// every cap: the first non-Complete condition encountered in walk order is
/// reported (walk order is stable, so the verdict is stable).
pub fn discover_repo_inventory(root: &Path) -> RepoInventory {
    let mut files: Vec<String> = Vec::new();
    let mut completeness = InventoryCompleteness::Complete;
    let mut queue: VecDeque<(usize, Vec<String>)> = VecDeque::new();
    queue.push_back((0, Vec::new()));
    'walk: while let Some((depth, dir_parts)) = queue.pop_front() {
        let abs = if dir_parts.is_empty() {
            root.to_path_buf()
        } else {
            root.join(posix_rel(&dir_parts))
        };
        let entries = match std::fs::read_dir(&abs) {
            Ok(entries) => entries,
            Err(_) => {
                completeness = InventoryCompleteness::Unreadable {
                    path: posix_rel(&dir_parts),
                };
                break 'walk;
            }
        };
        let mut names: Vec<(String, std::fs::FileType)> = Vec::new();
        let mut seen = 0usize;
        for entry in entries {
            seen += 1;
            if seen > MAX_DIR_SCAN_ENTRIES {
                completeness = InventoryCompleteness::DirectoryPageTruncated;
                break 'walk;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    completeness = InventoryCompleteness::Unreadable {
                        path: posix_rel(&dir_parts),
                    };
                    break 'walk;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            // symlink_metadata-based classification: a symlink stays a
            // symlink (never followed), so cyclic links cannot loop and a
            // linked subtree can never hide from the completeness verdict.
            let file_type = match std::fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta.file_type(),
                Err(_) => {
                    let mut path = dir_parts.clone();
                    path.push(name);
                    completeness = InventoryCompleteness::Unreadable {
                        path: posix_rel(&path),
                    };
                    break 'walk;
                }
            };
            names.push((name, file_type));
        }
        if names.len() > MAX_INVENTORY_DIR_PAGE {
            completeness = InventoryCompleteness::DirectoryPageTruncated;
            break 'walk;
        }
        names.sort_by_key(|(name, _)| name.clone());
        for (name, file_type) in names {
            let mut parts = dir_parts.clone();
            parts.push(name.clone());
            if file_type.is_symlink() {
                completeness = InventoryCompleteness::Unreadable {
                    path: posix_rel(&parts),
                };
                break 'walk;
            }
            if file_type.is_dir() {
                if INVENTORY_SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if depth + 1 > MAX_INVENTORY_DEPTH {
                    completeness = InventoryCompleteness::DepthLimitExceeded;
                    break 'walk;
                }
                queue.push_back((depth + 1, parts));
            } else {
                files.push(posix_rel(&parts));
                if files.len() > MAX_INVENTORY_FILES {
                    completeness = InventoryCompleteness::EntryLimitExceeded;
                    break 'walk;
                }
            }
        }
    }
    files.sort();
    files.dedup();
    RepoInventory {
        files,
        completeness,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn complete_small_repo_is_complete_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        write(dir.path(), "Cargo.toml");
        write(dir.path(), "src/deep/mod.rs");
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(
            inv.files,
            vec![
                "Cargo.toml".to_string(),
                "src/deep/mod.rs".to_string(),
                "src/lib.rs".to_string()
            ]
        );
        assert!(inv.refusal_reason().is_none());
    }

    #[test]
    fn skip_dirs_are_not_walked_and_never_poison_completeness() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".git/objects/x");
        write(dir.path(), "target/debug/x");
        write(dir.path(), "src/lib.rs");
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(inv.files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn five_hundred_one_files_is_an_entry_limit_refusal() {
        let dir = tempfile::tempdir().unwrap();
        // Spread across directories so no single page exceeds the
        // directory cap: the FILE cap is the condition under test.
        for i in 0..=MAX_INVENTORY_FILES {
            write(dir.path(), &format!("d{}/f{i:04}.rs", i % 40));
        }
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::EntryLimitExceeded,
            "{inv:?}"
        );
        assert!(inv.refusal_reason().unwrap().contains("file cap"));
    }

    #[test]
    fn five_hundred_files_is_complete_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_INVENTORY_FILES {
            write(dir.path(), &format!("d{}/f{i:04}.rs", i % 40));
        }
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(inv.files.len(), MAX_INVENTORY_FILES);
    }

    #[test]
    fn over_two_hundred_directory_children_is_a_page_truncation_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide");
        fs::create_dir_all(&wide).unwrap();
        for i in 0..=MAX_INVENTORY_DIR_PAGE {
            fs::write(wide.join(format!("c{i:03}")), b"x").unwrap();
        }
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::DirectoryPageTruncated,
            "{inv:?}"
        );
        assert!(inv.refusal_reason().unwrap().contains("page cap"));
    }

    #[test]
    fn exactly_two_hundred_children_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide");
        fs::create_dir_all(&wide).unwrap();
        for i in 0..MAX_INVENTORY_DIR_PAGE {
            fs::write(wide.join(format!("c{i:03}")), b"x").unwrap();
        }
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert_eq!(inv.files.len(), MAX_INVENTORY_DIR_PAGE);
    }

    #[test]
    fn manifest_beyond_depth_six_is_a_depth_refusal() {
        let dir = tempfile::tempdir().unwrap();
        // depth 1..=6 directories, manifest at depth 7: beyond the cap.
        write(dir.path(), "a/b/c/d/e/f/g/Cargo.toml");
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::DepthLimitExceeded,
            "{inv:?}"
        );
        assert!(inv.refusal_reason().unwrap().contains("depth"));
    }

    #[test]
    fn manifest_at_depth_six_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a/b/c/d/e/f/Cargo.toml");
        let inv = discover_repo_inventory(dir.path());
        assert_eq!(inv.completeness, InventoryCompleteness::Complete);
        assert!(inv.files.iter().any(|f| f.ends_with("Cargo.toml")));
    }

    #[test]
    fn unreadable_subdirectory_is_reported_and_never_silently_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        let locked = dir.path().join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("hidden.rs"), b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            let readable = std::fs::read_dir(&locked).is_ok();
            let inv = discover_repo_inventory(dir.path());
            // Root (CI containers) can read through mode 0; the test then
            // has no unreadable directory to assert and says so loudly.
            if readable {
                fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
                eprintln!("skipping unreadable-dir assertion: running with permission bypass");
                return;
            }
            assert!(
                matches!(
                    &inv.completeness,
                    InventoryCompleteness::Unreadable { path } if path == "locked"
                ),
                "{inv:?}"
            );
            assert!(inv.refusal_reason().unwrap().contains("locked"));
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn cyclic_and_hostile_symlinks_terminate_and_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs");
        let loop_dir = dir.path().join("loops");
        fs::create_dir_all(&loop_dir).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&loop_dir, loop_dir.join("self")).unwrap();
            std::os::unix::fs::symlink(dir.path(), loop_dir.join("up")).unwrap();
            // A dangling symlink is still a symlink: refused, never followed.
            std::os::unix::fs::symlink("/nonexistent/faktor", loop_dir.join("dangling")).unwrap();
            let inv = discover_repo_inventory(dir.path());
            assert!(
                matches!(
                    &inv.completeness,
                    InventoryCompleteness::Unreadable { path } if path.starts_with("loops")
                ),
                "a symlink must make the inventory non-Complete: {inv:?}"
            );
        }
    }

    #[test]
    fn unreadable_root_is_reported_not_empty_complete() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let inv = discover_repo_inventory(&missing);
        assert_eq!(
            inv.completeness,
            InventoryCompleteness::Unreadable {
                path: String::new()
            }
        );
        assert!(inv.files.is_empty());
        assert!(inv.refusal_reason().is_some());
    }
}
