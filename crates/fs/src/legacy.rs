//! Legacy recovery-path containment proof (audit P1-F), shared by the agent
//! runtime and the session-level recovery sweep.
//!
//! A pre-P1-F recovery row recorded a raw pathname in its `RecoveryStrategy`.
//! That string is NEVER an execution capability: it is only a containment
//! CLAIM to be proven against the session's durable workspace root before it
//! may be converted to a normalized workspace-relative identity and verified
//! through the [`WorkspaceHandle`](crate::WorkspaceHandle)'s anchored,
//! symlink-bounded open with the post-open identity net.

use std::path::{Path, PathBuf};

/// One-time legacy migration proof (audit P1-F): derive the normalized
/// workspace-relative path of a legacy `VerifyHash` path, or `None` when the
/// path cannot be PROVEN to be inside the canonical workspace `root`.
/// Canonicalization is the proof: `..` climbs, symlinks pointing out of the
/// root, and different roots all fail the `strip_prefix` step. A
/// not-yet-existing tail is appended lexically to its canonicalized deepest
/// existing ancestor (a missing component cannot be a symlink; the handle
/// re-walks the relative path under its own anchored resolution at
/// verification time, so a swap after this proof can never redirect the read
/// outside). The caller must classify `None` as Unknown/NeedsUserInput —
/// never Verified. `root` must itself be canonical (a `WorkspaceHandle`'s
/// `root()` always is).
pub fn legacy_relative_path_within(root: &Path, legacy: &str) -> Option<String> {
    if legacy.is_empty() || legacy.contains('\0') {
        return None;
    }
    let raw = Path::new(legacy);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let canonical = canonicalize_lenient(&joined)?;
    let relative = canonical.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    // Canonicalization removes `.`/`..`/root components; anything else
    // surviving (a hostile dialect, a device name) is refused.
    if relative
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(relative.to_str()?.to_string())
}

/// Canonicalize `path`; when part of the tail does not exist yet,
/// canonicalize the deepest existing ancestor and append the missing
/// components lexically (a missing component cannot be a symlink). `None`
/// when no ancestor resolves or a missing component is `..`.
fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }
    // A path ending in `..` (or at the filesystem root itself) has no
    // provable lexical continuation.
    let mut tail: Vec<std::ffi::OsString> = vec![path.file_name()?.to_os_string()];
    let mut current = path.parent();
    while let Some(dir) = current {
        if let Ok(canonical) = dir.canonicalize() {
            let mut out = canonical;
            for name in tail.iter().rev() {
                if name == ".." {
                    return None;
                }
                out.push(name);
            }
            return Some(out);
        }
        // Same refusal for a `..` ancestor: canonicalization could not
        // resolve it, and lexical guessing is not a proof.
        tail.push(dir.file_name()?.to_os_string());
        current = dir.parent();
    }
    None
}

/// Pure host-side grammar for a durable postcondition's workspace-relative
/// path (write recovery): the value must be relative on EVERY host dialect.
/// Absolute forms are refused exactly like Unix `/x` — Windows drive-letter
/// (`C:\x`, `C:/x`, drive-relative `C:x`), UNC/device (`\\server\share`,
/// `//server/share`, `\\?\...`, `\x`) — together with `..` traversal
/// components and NUL bytes. Platform-independent (the separators are
/// unified for classification), so the Windows shapes are refused on Unix
/// hosts too and stay pinned by the Unix CI lanes. `None` = safe.
pub fn workspace_relative_path_rejection(rel: &str) -> Option<&'static str> {
    if rel.is_empty() {
        return Some("empty path");
    }
    if rel.contains('\0') {
        return Some("NUL byte");
    }
    // Classify on the unified spelling: a Windows absolute form must be
    // rejected even where `Path::is_absolute` cannot see one.
    let unified = rel.replace('\\', "/");
    if unified.starts_with('/') {
        return Some("absolute path (rooted, UNC or device form)");
    }
    let bytes = unified.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some("absolute path (drive-letter prefix)");
    }
    if Path::new(&unified)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Some("path traversal (..)");
    }
    None
}
