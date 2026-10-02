//! Workspace path identity: the ONE lexical normal form ownership and FS
//! access agree on, so `./a`, `a/../a`, `a//b` and `a` cannot own the same
//! object twice even before the file exists (when `canonicalize` fails).

use std::path::{Path, PathBuf};

use crate::OwnershipSet;

/// Normalize one workspace-relative spelling WITHOUT touching the
/// filesystem: skip `.`, collapse duplicate separators, resolve `..` inside
/// the relative path only. `None` means the spelling escapes the workspace
/// (leading `..`), which the rooted-path policy refuses upstream.
pub(crate) fn lexical_relative(rel: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in rel.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

impl OwnershipSet {
    pub fn canonicalized(&self, base: &Path) -> Self {
        let base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
        let mut v: Vec<String> = Vec::new();
        for p in &self.0 {
            let is_dir = p.ends_with('/') || p.ends_with('\\');
            let trimmed = p.trim_end_matches(['/', '\\']);
            if trimmed.is_empty() {
                continue;
            }
            // Lexical identity BEFORE any FS call: aliases of a not-yet
            // existing path must collide, not own it twice.
            let joined = match lexical_relative(trimmed) {
                Some(rel) if !rel.as_os_str().is_empty() => base.join(rel),
                Some(_) => continue,
                None => base.join(trimmed),
            };
            let canon = std::fs::canonicalize(&joined).unwrap_or(joined);
            let mut s = canon.to_string_lossy().to_string();
            if is_dir {
                s.push('/');
            }
            v.push(s);
        }
        v.sort();
        v.dedup();
        Self(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_aliases_share_one_identity_before_existence() {
        let base = tempfile::tempdir().unwrap();
        let a = OwnershipSet::new(["src/a.rs".to_string()]).canonicalized(base.path());
        let b = OwnershipSet::new(["./src/a.rs".to_string()]).canonicalized(base.path());
        let c = OwnershipSet::new(["src//x/../a.rs".to_string()]).canonicalized(base.path());
        let d = OwnershipSet::new(["src/./a.rs".to_string()]).canonicalized(base.path());
        assert!(
            a.overlaps(&b),
            "./ prefix must not change ownership identity"
        );
        assert!(a.overlaps(&c), ".. inside the workspace must normalize");
        assert!(a.overlaps(&d), "./ component must normalize");
        let escape = OwnershipSet::new(["../outside.rs".to_string()]).canonicalized(base.path());
        assert!(!a.overlaps(&escape), "a leading .. must never alias inside");
    }
}
