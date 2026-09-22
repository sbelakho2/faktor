//! Browser profiles (spec §9): persistent profiles live under
//! `<data-dir>/commerce/profiles/<name>` with restrictive permissions;
//! incognito sessions use temporary contexts. Cookies are confined to the
//! profile directory: this crate never reads cookie values, never puts them
//! in CAS, and never exposes them to a model.

use std::path::{Path, PathBuf};

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

/// The persistent profile store rooted at `<data-dir>/commerce/profiles`.
pub struct ProfileStore {
    root: PathBuf,
}

impl ProfileStore {
    /// Open (creating when missing) the profile root with restrictive
    /// permissions.
    pub fn open(root: PathBuf) -> Result<Self, BrowserError> {
        std::fs::create_dir_all(&root).map_err(|e| {
            BrowserError::profile(format!("cannot create profile root {root:?}: {e}"))
        })?;
        restrict_dir(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory for a validated profile name, created 0700 on demand.
    pub fn profile_dir(&self, name: &str) -> Result<PathBuf, BrowserError> {
        validate_profile_name(name)?;
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir)
            .map_err(|e| BrowserError::profile(format!("cannot create profile {dir:?}: {e}")))?;
        restrict_dir(&dir)?;
        Ok(dir)
    }

    pub fn exists(&self, name: &str) -> bool {
        validate_profile_name(name).is_ok() && self.root.join(name).is_dir()
    }

    /// A scratch directory inside the profile, used for the child's
    /// HOME/TMPDIR/XDG homes so Chromium never touches the operator's real
    /// home. Created 0700.
    pub fn scratch_dir(&self, name: &str) -> Result<PathBuf, BrowserError> {
        let profile = self.profile_dir(name)?;
        let scratch = profile.join("scratch");
        std::fs::create_dir_all(&scratch).map_err(|e| {
            BrowserError::profile(format!("cannot create profile scratch {scratch:?}: {e}"))
        })?;
        restrict_dir(&scratch)?;
        Ok(scratch)
    }

    /// Cookie store location (diagnostics only). The crate never reads its
    /// contents: cookies stay inside the profile boundary, are never sent to
    /// CAS, and are never model-visible.
    pub fn cookie_store_path(&self, name: &str) -> Result<PathBuf, BrowserError> {
        Ok(self.profile_dir(name)?.join("Default").join("Cookies"))
    }

    /// Remove one profile's data (operator action). The name is validated
    /// first, so this can only ever delete inside the store root.
    pub fn wipe(&self, name: &str) -> Result<(), BrowserError> {
        let dir = self.profile_dir(name)?;
        if !dir.starts_with(&self.root) || dir == self.root {
            return Err(BrowserError::profile(format!(
                "refusing to wipe outside the profile root: {dir:?}"
            )));
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|e| BrowserError::profile(format!("cannot wipe profile {dir:?}: {e}")))?;
        Ok(())
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
        let dir = self
            .root
            .join(".incognito")
            .join(format!("{hint}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).map_err(|e| {
            BrowserError::profile(format!("cannot create incognito profile {dir:?}: {e}"))
        })?;
        restrict_dir(&dir)?;
        Ok(IncognitoProfile { dir, keep: false })
    }
}

/// A temporary incognito profile directory; deleted on drop unless
/// `keep()` is called (forensics are never silently retained).
pub struct IncognitoProfile {
    dir: PathBuf,
    keep: bool,
}

impl IncognitoProfile {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn keep(mut self) -> PathBuf {
        self.keep = true;
        self.dir.clone()
    }
}

impl Drop for IncognitoProfile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

impl std::fmt::Debug for IncognitoProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncognitoProfile")
            .field("dir", &self.dir)
            .finish()
    }
}

/// Restrict a directory to owner-only access (0700) on unix. On platforms
/// without POSIX modes this is a no-op and the honest isolation note in the
/// crate docs applies.
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
            let dir = incognito.dir().to_path_buf();
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
}
