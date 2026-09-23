//! Browser profiles (spec §9): persistent profiles live under
//! `<data-dir>/commerce/profiles/<name>` with restrictive permissions;
//! incognito sessions use temporary contexts. Cookies are confined to the
//! profile directory: this crate never reads cookie values, never puts them
//! in CAS, and never exposes them to a model.
//!
//! Every profile path is created, removed and restricted through
//! [`faktor_fs::RootedDir`]: names are validated, the directory authority is
//! anchored on filesystem handles, and a profile (or scratch, or download)
//! entry swapped for a symlink is refused typed instead of followed.

use std::path::{Path, PathBuf};

use faktor_fs::RootedDir;

use crate::error::BrowserError;

/// Maximum profile name length.
pub const MAX_PROFILE_NAME_BYTES: usize = 64;
/// Maximum account label length (identity component; never a path).
pub const MAX_ACCOUNT_BYTES: usize = 128;

/// Profile names are strict identifiers: lowercase alphanumerics, `-` and
/// `_`, 1..=64 bytes. Anything else (including path separators, dots and
/// traversal) is refused typed before a path is ever built.
pub fn validate_profile_name(name: &str) -> Result<(), BrowserError> {
    if name.is_empty() || name.len() > MAX_PROFILE_NAME_BYTES {
        return Err(BrowserError::invalid_config(format!(
            "profile name must be 1..={MAX_PROFILE_NAME_BYTES} bytes"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(BrowserError::invalid_config(format!(
            "profile name {name:?} must match [a-z0-9_-]+"
        )));
    }
    Ok(())
}

fn profile_err(context: &str, error: faktor_core::error::Error) -> BrowserError {
    BrowserError::Profile {
        detail: format!("{context}: {error}"),
    }
}

/// The persistent profile store rooted at `<data-dir>/commerce/profiles`.
pub struct ProfileStore {
    root: RootedDir,
}

impl ProfileStore {
    /// Open (creating when missing) the profile root with restrictive
    /// permissions.
    pub fn open(root: PathBuf) -> Result<Self, BrowserError> {
        let rooted = RootedDir::create(&root)
            .map_err(|e| profile_err(&format!("cannot open profile root {root:?}"), e))?;
        rooted
            .restrict_owner_only(Path::new(""))
            .map_err(|e| profile_err("cannot restrict the profile root", e))?;
        Ok(Self { root: rooted })
    }

    pub fn root(&self) -> &Path {
        self.root.root()
    }

    /// The anchored directory authority every profile path operation uses.
    pub fn rooted(&self) -> &RootedDir {
        &self.root
    }

    /// The directory for a validated profile name, created 0700 on demand.
    pub fn profile_dir(&self, name: &str) -> Result<PathBuf, BrowserError> {
        validate_profile_name(name)?;
        let rel = Path::new(name);
        self.root
            .create_dir_all(rel)
            .map_err(|e| profile_err(&format!("cannot create profile {name:?}"), e))?;
        self.root
            .restrict_owner_only(rel)
            .map_err(|e| profile_err(&format!("cannot restrict profile {name:?}"), e))?;
        Ok(self.root.join(rel))
    }

    pub fn exists(&self, name: &str) -> bool {
        validate_profile_name(name).is_ok() && self.root.exists(Path::new(name))
    }

    /// A scratch directory inside the profile, used for the child's
    /// HOME/TMPDIR/XDG homes so Chromium never touches the operator's real
    /// home. Created 0700 through the anchored authority.
    pub fn scratch_dir(&self, name: &str) -> Result<PathBuf, BrowserError> {
        validate_profile_name(name)?;
        let rel = Path::new(name).join("scratch");
        self.root
            .create_dir_all(&rel)
            .map_err(|e| profile_err(&format!("cannot create scratch for {name:?}"), e))?;
        self.root
            .restrict_owner_only(&rel)
            .map_err(|e| profile_err(&format!("cannot restrict scratch for {name:?}"), e))?;
        Ok(self.root.join(&rel))
    }

    /// Cookie store location (diagnostics only). The crate never reads its
    /// contents: cookies stay inside the profile boundary, are never sent to
    /// CAS, and are never model-visible.
    pub fn cookie_store_path(&self, name: &str) -> Result<PathBuf, BrowserError> {
        Ok(self.profile_dir(name)?.join("Default").join("Cookies"))
    }

    /// Remove one profile's data (operator action). The name is validated
    /// first and the removal walks the anchored handle without ever following
    /// a link, so this can only ever delete inside the store root.
    pub fn wipe(&self, name: &str) -> Result<(), BrowserError> {
        validate_profile_name(name)?;
        self.root
            .remove_tree(Path::new(name))
            .map_err(|e| profile_err(&format!("cannot wipe profile {name:?}"), e))
    }

    /// An incognito (temporary) profile context: a unique directory under
    /// `<root>/.incognito`, removed on drop. Never shared with a persistent
    /// profile.
    pub fn incognito(&self, hint: &str) -> Result<IncognitoProfile, BrowserError> {
        let hint: String = hint
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
            .take(24)
            .collect();
        let container = Path::new(".incognito");
        self.root
            .create_dir_all(container)
            .map_err(|e| profile_err("cannot create the incognito container", e))?;
        self.root
            .restrict_owner_only(container)
            .map_err(|e| profile_err("cannot restrict the incognito container", e))?;
        let rel = container.join(format!("{hint}-{}", uuid::Uuid::new_v4().simple()));
        self.root
            .create_dir_all(&rel)
            .map_err(|e| profile_err("cannot create the incognito profile", e))?;
        self.root
            .restrict_owner_only(&rel)
            .map_err(|e| profile_err("cannot restrict the incognito profile", e))?;
        Ok(IncognitoProfile {
            root: self.root.clone(),
            rel,
            keep: false,
        })
    }
}

/// A temporary incognito profile directory; deleted on drop unless
/// `keep()` is called (forensics are never silently retained).
pub struct IncognitoProfile {
    root: RootedDir,
    rel: PathBuf,
    keep: bool,
}

impl IncognitoProfile {
    /// The profile-relative path of this incognito instance.
    pub fn rel(&self) -> &Path {
        &self.rel
    }

    /// The absolute path (Chromium's `--user-data-dir`).
    pub fn dir(&self) -> PathBuf {
        self.root.join(&self.rel)
    }

    pub fn keep(mut self) -> PathBuf {
        self.keep = true;
        self.dir()
    }

    /// Force removal (used by rollback and shutdown paths that do not go
    /// through `Drop`).
    pub fn remove(&self) {
        let _ = self.root.remove_tree(&self.rel);
    }
}

impl Drop for IncognitoProfile {
    fn drop(&mut self) {
        if !self.keep {
            // Anchored, link-refusing removal: a swapped entry is refused,
            // never traversed.
            let _ = self.root.remove_tree(&self.rel);
        }
    }
}

impl std::fmt::Debug for IncognitoProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncognitoProfile")
            .field("dir", &self.dir())
            .finish()
    }
}

/// Restrict a directory to owner-only access (0700) on unix. On platforms
/// without POSIX modes this is a no-op and the honest isolation note in the
/// crate docs applies. Path-based; the anchored authority is preferred for
/// anything under the profile root.
pub fn restrict_dir(path: &Path) -> Result<(), BrowserError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            BrowserError::profile(format!("cannot restrict permissions on {path:?}: {e}"))
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Restrict a file to owner-only access (0600) on unix.
pub fn restrict_file(path: &Path) -> Result<(), BrowserError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            BrowserError::profile(format!("cannot restrict permissions on {path:?}: {e}"))
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_refuse_traversal_and_separators() {
        for bad in [
            "",
            "../escape",
            "a/b",
            "a\\b",
            "A-Upper",
            "with space",
            "with.dot",
            "..",
        ] {
            assert!(
                validate_profile_name(bad).is_err(),
                "profile name {bad:?} must be refused"
            );
        }
        for good in ["procurement-cn", "a", "p_1", &"x".repeat(64)] {
            assert!(validate_profile_name(good).is_ok(), "{good:?}");
        }
        assert!(validate_profile_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn wipe_can_only_target_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(tmp.path().join("profiles")).unwrap();
        let dir = store.profile_dir("p1").unwrap();
        assert!(dir.starts_with(store.root()));
        std::fs::write(dir.join("marker"), b"x").unwrap();
        store.wipe("p1").unwrap();
        assert!(!dir.exists());
        // Traversal is refused before any path operation.
        assert!(store.wipe("../..").is_err());
        assert!(store.wipe("p1/../../x").is_err());
    }

    #[test]
    fn incognito_dirs_are_removed_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(tmp.path().join("profiles")).unwrap();
        let dir = {
            let incognito = store.incognito("probe").unwrap();
            let dir = incognito.dir();
            assert!(dir.exists());
            dir
        };
        assert!(!dir.exists(), "incognito dir must be removed on drop");
        let kept = store.incognito("probe").unwrap().keep();
        assert!(kept.exists());
        let _ = std::fs::remove_dir_all(&kept);
    }

    #[cfg(unix)]
    #[test]
    fn profile_dirs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(tmp.path().join("profiles")).unwrap();
        let dir = store.profile_dir("p1").unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "profile dir must be 0700");
        let scratch = store.scratch_dir("p1").unwrap();
        let mode = std::fs::metadata(&scratch).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "scratch dir must be 0700");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_swapped_profile_entries_are_refused_and_targets_untouched() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("marker"), b"keep").unwrap();
        let root = tmp.path().join("profiles");
        let store = ProfileStore::open(root.clone()).unwrap();

        // profile creation: p1 swapped for a symlink to the outside dir
        symlink(&outside, root.join("p1")).unwrap();
        assert!(
            store.profile_dir("p1").is_err(),
            "a symlinked profile entry must be refused"
        );
        assert!(
            store.scratch_dir("p1").is_err(),
            "scratch creation through a symlinked profile must be refused"
        );
        assert!(!outside.join("scratch").exists());
        assert!(outside.join("marker").exists());

        // wipe: the symlinked entry is refused, the target is preserved
        assert!(store.wipe("p1").is_err());
        assert!(outside.join("marker").exists());

        // scratch swap inside a real profile
        std::fs::remove_file(root.join("p1")).unwrap();
        let dir = store.profile_dir("p1").unwrap();
        assert!(dir.is_dir());
        symlink(&outside, dir.join("scratch")).unwrap();
        assert!(
            store.scratch_dir("p1").is_err(),
            "a symlinked scratch entry must be refused"
        );
        assert!(!outside.join("tmp").exists());
        assert!(outside.join("marker").exists());
    }
}
