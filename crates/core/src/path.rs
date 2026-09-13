//! [`NormalizedWorkspacePath`]: the ONE path vocabulary ownership analysis
//! (and any other workspace-relative write-authority analysis) is allowed to
//! consume (audit hardening: ownership paths must be canonical, pure
//! lexical, workspace-relative and platform-stable BEFORE any overlap
//! decision).
//!
//! # Constructor contract (every rejection is typed)
//!
//! A path is accepted only when it is a relative, `/`-separated, non-empty
//! component sequence:
//!
//! - empty / overlong input is rejected;
//! - absolute and rooted forms are rejected: a leading `/`, a Windows drive
//!   prefix (`C:`, `c:`), a UNC/verbatim prefix (`//server`, `\\?\...`), or
//!   any `:` anywhere (drive-relative paths and NTFS alternate data streams
//!   can otherwise re-target a write);
//! - alternate separators are rejected: `\` is NEVER accepted, on any
//!   platform. A path that means different things on unix and Windows is a
//!   silently different write authority, so the wire vocabulary has exactly
//!   one separator;
//! - `.` and `..` components are rejected (traversal aliases); so is the
//!   empty component (`a//b`, a doubled or trailing slash — a single
//!   trailing `/` is a directory marker and is normalized away);
//! - control characters (including NUL) are rejected;
//! - Windows platform aliases are rejected on EVERY platform so one durable
//!   path set cannot widen on another OS: reserved device names (`CON`,
//!   `PRN`, `AUX`, `NUL`, `COM0..9`, `LPT0..9`, with or without an
//!   extension) and components with a trailing dot or space (Windows strips
//!   both, aliasing two spellings onto one file).
//!
//! # Comparison policy (explicit)
//!
//! Comparisons are PURE LEXICAL over the canonical components — the type
//! never touches the filesystem. On unix both equality and containment are
//! byte-exact (case-sensitive). On Windows the comparison folds ASCII case
//! (NTFS is case-insensitive, so `SRC/A` and `src/a` are the SAME write
//! authority and MUST be treated as overlapping); the canonical STORED
//! spelling is never case-folded, only the comparison key is. Cross-platform
//! safety comes from rejecting `\` and drive/UNC prefixes everywhere, so a
//! path accepted on one OS can never resolve to a different file on
//! another. Filesystem canonicalization (symlinks, case of the real
//! directory entry) is deliberately NOT part of this type: callers that
//! resolve against a live root must canonicalize through the filesystem
//! first (the scheduler's `OwnershipSet::canonicalized` remains that seam).

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Hard bound on one normalized workspace path (UTF-8 bytes). Matches the
/// ownership-spec entry bound so a spec can never carry a path the durable
/// rows cannot store.
pub const MAX_NORMALIZED_WORKSPACE_PATH_BYTES: usize = 256;

/// Why one candidate path was refused. Typed, never prose-only: every
/// rejection names the exact rule and preserves the raw offending value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathViolationKind {
    /// The input is empty.
    Empty,
    /// The input exceeds [`MAX_NORMALIZED_WORKSPACE_PATH_BYTES`].
    TooLong,
    /// A leading `/` (or any absolute spelling).
    Absolute,
    /// A drive/UNC/verbatim prefix or a `:` (drive-relative / ADS alias).
    RootedPrefix,
    /// A `\` separator (never accepted, on any platform).
    AlternateSeparator,
    /// A `..` component.
    ParentComponent,
    /// A `.` component.
    CurrentComponent,
    /// An empty component (`a//b`, doubled or trailing slash beyond the
    /// single directory marker).
    EmptyComponent,
    /// A NUL or other control character.
    ControlCharacter,
    /// A Windows platform alias: reserved device name, trailing dot/space.
    PlatformAlias,
}

impl PathViolationKind {
    /// Stable machine label (matches the rule name in the docs).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::Absolute => "absolute",
            Self::RootedPrefix => "rooted_prefix",
            Self::AlternateSeparator => "alternate_separator",
            Self::ParentComponent => "parent_component",
            Self::CurrentComponent => "current_component",
            Self::EmptyComponent => "empty_component",
            Self::ControlCharacter => "control_character",
            Self::PlatformAlias => "platform_alias",
        }
    }
}

impl std::fmt::Display for PathViolationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One typed path rejection (the raw value plus the exact rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathViolation {
    pub kind: PathViolationKind,
    pub value: String,
}

impl std::fmt::Display for PathViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "workspace path {:?} refused: {}",
            self.value,
            self.kind.as_str()
        )
    }
}

impl std::error::Error for PathViolation {}

/// A canonical, workspace-relative, platform-stable path. Constructible
/// only through [`NormalizedWorkspacePath::new`] (or the validating serde
/// decode): every write-authority comparison consumes this type, never a
/// raw string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NormalizedWorkspacePath(String);

impl NormalizedWorkspacePath {
    /// Validate and canonicalize one path. See the module docs for the exact
    /// rules; a single trailing `/` directory marker is normalized away.
    pub fn new(raw: &str) -> Result<Self, PathViolation> {
        let refuse = |kind: PathViolationKind| {
            Err(PathViolation {
                kind,
                value: raw.to_string(),
            })
        };
        if raw.is_empty() {
            return refuse(PathViolationKind::Empty);
        }
        if raw.len() > MAX_NORMALIZED_WORKSPACE_PATH_BYTES {
            return refuse(PathViolationKind::TooLong);
        }
        if raw.chars().any(|c| c.is_control()) {
            return refuse(PathViolationKind::ControlCharacter);
        }
        if raw.starts_with('/') {
            return refuse(PathViolationKind::Absolute);
        }
        if raw.contains('\\') {
            return refuse(PathViolationKind::AlternateSeparator);
        }
        if raw.contains(':') {
            return refuse(PathViolationKind::RootedPrefix);
        }
        // Exactly one trailing `/` is the directory marker; more means an
        // empty component.
        let body = match raw.strip_suffix('/') {
            Some(rest) => {
                if rest.ends_with('/') {
                    return refuse(PathViolationKind::EmptyComponent);
                }
                rest
            }
            None => raw,
        };
        let mut canonical = String::with_capacity(body.len());
        for component in body.split('/') {
            if component.is_empty() {
                return refuse(PathViolationKind::EmptyComponent);
            }
            if component == "." {
                return refuse(PathViolationKind::CurrentComponent);
            }
            if component == ".." {
                return refuse(PathViolationKind::ParentComponent);
            }
            if component.ends_with('.') || component.ends_with(' ') {
                return refuse(PathViolationKind::PlatformAlias);
            }
            if is_reserved_device_name(component) {
                return refuse(PathViolationKind::PlatformAlias);
            }
            if !canonical.is_empty() {
                canonical.push('/');
            }
            canonical.push_str(component);
        }
        if canonical.is_empty() {
            return refuse(PathViolationKind::Empty);
        }
        Ok(Self(canonical))
    }

    /// The canonical path text (no trailing slash, `/`-separated).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// The `/`-separated components of the canonical path.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The comparison key: byte-exact on unix, ASCII-case-folded on Windows
    /// (NTFS is case-insensitive; two spellings are one file).
    fn cmp_key(&self) -> std::borrow::Cow<'_, str> {
        if cfg!(windows) {
            std::borrow::Cow::Owned(self.0.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(self.0.as_str())
        }
    }

    /// Whether `self` is `other` or a proper directory ancestor of it, at a
    /// component boundary. Directory semantics are implicit: ownership
    /// paths always cover their subtree.
    pub fn covers(&self, other: &Self) -> bool {
        let (a, b) = (self.cmp_key(), other.cmp_key());
        if a == b {
            return true;
        }
        b.starts_with(a.as_ref()) && b.as_bytes().get(a.len()) == Some(&b'/')
    }

    /// Whether the two paths touch at a component boundary in either
    /// direction.
    pub fn overlaps(&self, other: &Self) -> bool {
        self.covers(other) || other.covers(self)
    }
}

impl std::fmt::Display for NormalizedWorkspacePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for NormalizedWorkspacePath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for NormalizedWorkspacePath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NormalizedWorkspacePath {
    /// Decode is validating: a hostile durable row carrying a traversal or
    /// alias spelling is a loud deserialization error, never a silently
    /// accepted ownership widening.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

/// Windows reserves these device names (case-insensitively, with or without
/// an extension) in every directory. Rejected on every platform so a durable
/// path set cannot widen on Windows.
fn is_reserved_device_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let Some(suffix) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    // `COM0..COM9` / `LPT0..LPT9` are reserved; `COM10` is not.
    suffix.len() == 1 && suffix.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(raw: &str) -> NormalizedWorkspacePath {
        NormalizedWorkspacePath::new(raw)
            .unwrap_or_else(|e| panic!("{raw:?} must be accepted: {e}"))
    }

    fn reject(raw: &str, kind: PathViolationKind) {
        let got = NormalizedWorkspacePath::new(raw).expect_err("must be refused");
        assert_eq!(got.kind, kind, "raw {raw:?}: {got:?}");
        assert_eq!(got.value, raw);
    }

    #[test]
    fn canonical_relative_paths_are_accepted_and_normalized() {
        assert_eq!(accept("src").as_str(), "src");
        assert_eq!(accept("src/a.rs").as_str(), "src/a.rs");
        assert_eq!(accept("src/").as_str(), "src");
        assert_eq!(accept("a/b/c").components().count(), 3);
        // Non-ASCII file names are legal on unix and stay byte-exact.
        assert_eq!(
            accept("crates/\u{6f22}\u{5b57}.rs").as_str(),
            "crates/\u{6f22}\u{5b57}.rs"
        );
    }

    #[test]
    fn absolute_root_and_prefix_paths_are_refused() {
        reject("/etc/passwd", PathViolationKind::Absolute);
        reject("/", PathViolationKind::Absolute);
        reject("C:/windows", PathViolationKind::RootedPrefix);
        reject("C:windows", PathViolationKind::RootedPrefix);
        reject("c:src/a", PathViolationKind::RootedPrefix);
        reject("//server/share", PathViolationKind::Absolute);
        reject("\\\\?\\C:\\x", PathViolationKind::AlternateSeparator);
        reject("\\\\server\\share", PathViolationKind::AlternateSeparator);
        reject("a:b", PathViolationKind::RootedPrefix);
        // NTFS alternate data stream spelling.
        reject("src/a.rs:stream", PathViolationKind::RootedPrefix);
    }

    #[test]
    fn dot_and_parent_components_are_refused() {
        reject("..", PathViolationKind::ParentComponent);
        reject("../src", PathViolationKind::ParentComponent);
        reject("src/..", PathViolationKind::ParentComponent);
        reject("src/../a", PathViolationKind::ParentComponent);
        reject(".", PathViolationKind::CurrentComponent);
        reject("./src", PathViolationKind::CurrentComponent);
        reject("src/.", PathViolationKind::CurrentComponent);
    }

    #[test]
    fn empty_components_and_empty_inputs_are_refused() {
        reject("", PathViolationKind::Empty);
        reject("a//b", PathViolationKind::EmptyComponent);
        reject("a//", PathViolationKind::EmptyComponent);
        reject("//", PathViolationKind::Absolute);
        reject("a/ /b", PathViolationKind::PlatformAlias);
    }

    #[test]
    fn alternate_separators_are_refused_on_every_platform() {
        reject("src\\a.rs", PathViolationKind::AlternateSeparator);
        reject("\\src", PathViolationKind::AlternateSeparator);
        reject("src\\..\\a", PathViolationKind::AlternateSeparator);
    }

    #[test]
    fn control_and_nul_characters_are_refused() {
        reject("a\0b", PathViolationKind::ControlCharacter);
        reject("a\nb", PathViolationKind::ControlCharacter);
        reject("\u{7f}", PathViolationKind::ControlCharacter);
    }

    #[test]
    fn platform_aliases_are_refused_on_every_platform() {
        reject("CON", PathViolationKind::PlatformAlias);
        reject("con.rs", PathViolationKind::PlatformAlias);
        reject("src/NUL", PathViolationKind::PlatformAlias);
        reject("src/COM1.txt", PathViolationKind::PlatformAlias);
        reject("src/LPT9", PathViolationKind::PlatformAlias);
        reject("src/a.", PathViolationKind::PlatformAlias);
        reject("src/a ", PathViolationKind::PlatformAlias);
        // Non-reserved lookalikes stay accepted.
        assert_eq!(accept("src/CONSOLE").as_str(), "src/CONSOLE");
        assert_eq!(accept("src/COM10").as_str(), "src/COM10");
    }

    #[test]
    fn overlength_paths_are_refused() {
        let long = "a".repeat(MAX_NORMALIZED_WORKSPACE_PATH_BYTES + 1);
        reject(&long, PathViolationKind::TooLong);
        let at_cap = "a".repeat(MAX_NORMALIZED_WORKSPACE_PATH_BYTES);
        assert!(NormalizedWorkspacePath::new(&at_cap).is_ok());
    }

    #[test]
    fn overlap_uses_canonical_component_boundaries() {
        let a = accept("src");
        let b = accept("src/");
        let deep = accept("src/a.rs");
        let sibling = accept("src2/a.rs");
        assert_eq!(a, b, "trailing slash is a normalized-away directory marker");
        assert!(a.overlaps(&deep));
        assert!(deep.overlaps(&a), "overlap is symmetric");
        assert!(a.overlaps(&b));
        assert!(
            !a.overlaps(&sibling),
            "component boundary, not string prefix"
        );
        assert!(!deep.overlaps(&accept("src/a.rs2")));
        assert!(deep.overlaps(&accept("src/a.rs")));
        assert!(accept("src/a").overlaps(&accept("src/a/b")));
    }

    #[test]
    fn serde_is_validating_and_round_trips() {
        let path = accept("src/lib.rs");
        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(json, "\"src/lib.rs\"");
        let back: NormalizedWorkspacePath = serde_json::from_str(&json).unwrap();
        assert_eq!(back, path);
        // Hostile rows fail decode loudly instead of smuggling traversal.
        for hostile in ["\"../etc\"", "\"/abs\"", "\"a\\\\b\"", "\"\"", "\"src//a\""] {
            assert!(
                serde_json::from_str::<NormalizedWorkspacePath>(hostile).is_err(),
                "{hostile} must not decode"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_comparison_folds_ascii_case() {
        let upper = accept("SRC/A.RS");
        let lower = accept("src/a.rs");
        assert_ne!(upper, lower, "stored spelling is preserved");
        assert!(
            upper.overlaps(&lower),
            "NTFS folds case for write authority"
        );
        assert!(accept("SRC").covers(&lower));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_comparison_is_byte_exact() {
        let upper = accept("SRC/A.RS");
        let lower = accept("src/a.rs");
        assert!(!upper.overlaps(&lower), "unix is case-sensitive");
        assert!(!accept("SRC").covers(&lower));
    }
}
