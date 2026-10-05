//! Workspace path identity: the ONE lexical normal form ownership and FS
//! access agree on, so `./a`, `a/../a`, `a//b` and `a` cannot own the same
//! object twice even before the file exists (when `canonicalize` fails).

use std::path::{Path, PathBuf};

use crate::{OwnershipSet, WORKSPACE_ROOT_SENTINEL};

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

/// Canonical-combining-mark ranges dropped by the volume-identity skeleton.
/// Dropping marks makes the NFC and NFD spellings of one name collide on
/// volumes that normalize by default (macOS APFS/HFS+).
fn is_combining_mark(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe20..=0xfe2f
    )
}

/// The Latin decomposition skeleton: precomposed Latin letters map to their
/// base letter, so `é` (NFC) and `e` + U+0301 (NFD) share one identity on a
/// default-normalizing volume. Generated from Unicode canonical
/// decompositions for Latin-1 Supplement / Extended-A / Extended-B /
/// Extended Additional. This is an OVER-approximation: two genuinely
/// distinct names may serialize, which is the safe direction — the
/// scheduler must never miss an overlap the mounted filesystem treats as
/// one object.
fn latin_skeleton(c: char) -> Option<char> {
    const TABLE: &[(u32, char)] = &[
        (0x00e0, 'a'),
        (0x00e1, 'a'),
        (0x00e2, 'a'),
        (0x00e3, 'a'),
        (0x00e4, 'a'),
        (0x00e5, 'a'),
        (0x00e7, 'c'),
        (0x00e8, 'e'),
        (0x00e9, 'e'),
        (0x00ea, 'e'),
        (0x00eb, 'e'),
        (0x00ec, 'i'),
        (0x00ed, 'i'),
        (0x00ee, 'i'),
        (0x00ef, 'i'),
        (0x00f1, 'n'),
        (0x00f2, 'o'),
        (0x00f3, 'o'),
        (0x00f4, 'o'),
        (0x00f5, 'o'),
        (0x00f6, 'o'),
        (0x00f9, 'u'),
        (0x00fa, 'u'),
        (0x00fb, 'u'),
        (0x00fc, 'u'),
        (0x00fd, 'y'),
        (0x00ff, 'y'),
        (0x0101, 'a'),
        (0x0103, 'a'),
        (0x0105, 'a'),
        (0x0107, 'c'),
        (0x0109, 'c'),
        (0x010b, 'c'),
        (0x010d, 'c'),
        (0x010f, 'd'),
        (0x0113, 'e'),
        (0x0115, 'e'),
        (0x0117, 'e'),
        (0x0119, 'e'),
        (0x011b, 'e'),
        (0x011d, 'g'),
        (0x011f, 'g'),
        (0x0121, 'g'),
        (0x0123, 'g'),
        (0x0125, 'h'),
        (0x0129, 'i'),
        (0x012b, 'i'),
        (0x012d, 'i'),
        (0x012f, 'i'),
        (0x0135, 'j'),
        (0x0137, 'k'),
        (0x013a, 'l'),
        (0x013c, 'l'),
        (0x013e, 'l'),
        (0x0144, 'n'),
        (0x0146, 'n'),
        (0x0148, 'n'),
        (0x014d, 'o'),
        (0x014f, 'o'),
        (0x0151, 'o'),
        (0x0155, 'r'),
        (0x0157, 'r'),
        (0x0159, 'r'),
        (0x015b, 's'),
        (0x015d, 's'),
        (0x015f, 's'),
        (0x0161, 's'),
        (0x0163, 't'),
        (0x0165, 't'),
        (0x0169, 'u'),
        (0x016b, 'u'),
        (0x016d, 'u'),
        (0x016f, 'u'),
        (0x0171, 'u'),
        (0x0173, 'u'),
        (0x0175, 'w'),
        (0x0177, 'y'),
        (0x017a, 'z'),
        (0x017c, 'z'),
        (0x017e, 'z'),
        (0x01a1, 'o'),
        (0x01b0, 'u'),
        (0x01ce, 'a'),
        (0x01d0, 'i'),
        (0x01d2, 'o'),
        (0x01d4, 'u'),
        (0x01d6, 'ü'),
        (0x01d8, 'ü'),
        (0x01da, 'ü'),
        (0x01dc, 'ü'),
        (0x01df, 'ä'),
        (0x01e1, 'ȧ'),
        (0x01e3, 'æ'),
        (0x01e7, 'g'),
        (0x01e9, 'k'),
        (0x01eb, 'o'),
        (0x01ed, 'ǫ'),
        (0x01ef, 'ʒ'),
        (0x01f0, 'j'),
        (0x01f5, 'g'),
        (0x01f9, 'n'),
        (0x01fb, 'å'),
        (0x01fd, 'æ'),
        (0x01ff, 'ø'),
        (0x0201, 'a'),
        (0x0203, 'a'),
        (0x0205, 'e'),
        (0x0207, 'e'),
        (0x0209, 'i'),
        (0x020b, 'i'),
        (0x020d, 'o'),
        (0x020f, 'o'),
        (0x0211, 'r'),
        (0x0213, 'r'),
        (0x0215, 'u'),
        (0x0217, 'u'),
        (0x0219, 's'),
        (0x021b, 't'),
        (0x021f, 'h'),
        (0x0227, 'a'),
        (0x0229, 'e'),
        (0x022b, 'ö'),
        (0x022d, 'õ'),
        (0x022f, 'o'),
        (0x0231, 'ȯ'),
        (0x0233, 'y'),
        (0x1e01, 'a'),
        (0x1e03, 'b'),
        (0x1e05, 'b'),
        (0x1e07, 'b'),
        (0x1e09, 'ç'),
        (0x1e0b, 'd'),
        (0x1e0d, 'd'),
        (0x1e0f, 'd'),
        (0x1e11, 'd'),
        (0x1e13, 'd'),
        (0x1e15, 'ē'),
        (0x1e17, 'ē'),
        (0x1e19, 'e'),
        (0x1e1b, 'e'),
        (0x1e1d, 'ȩ'),
        (0x1e1f, 'f'),
        (0x1e21, 'g'),
        (0x1e23, 'h'),
        (0x1e25, 'h'),
        (0x1e27, 'h'),
        (0x1e29, 'h'),
        (0x1e2b, 'h'),
        (0x1e2d, 'i'),
        (0x1e2f, 'ï'),
        (0x1e31, 'k'),
        (0x1e33, 'k'),
        (0x1e35, 'k'),
        (0x1e37, 'l'),
        (0x1e39, 'ḷ'),
        (0x1e3b, 'l'),
        (0x1e3d, 'l'),
        (0x1e3f, 'm'),
        (0x1e41, 'm'),
        (0x1e43, 'm'),
        (0x1e45, 'n'),
        (0x1e47, 'n'),
        (0x1e49, 'n'),
        (0x1e4b, 'n'),
        (0x1e4d, 'õ'),
        (0x1e4f, 'õ'),
        (0x1e51, 'ō'),
        (0x1e53, 'ō'),
        (0x1e55, 'p'),
        (0x1e57, 'p'),
        (0x1e59, 'r'),
        (0x1e5b, 'r'),
        (0x1e5d, 'ṛ'),
        (0x1e5f, 'r'),
        (0x1e61, 's'),
        (0x1e63, 's'),
        (0x1e65, 'ś'),
        (0x1e67, 'š'),
        (0x1e69, 'ṣ'),
        (0x1e6b, 't'),
        (0x1e6d, 't'),
        (0x1e6f, 't'),
        (0x1e71, 't'),
        (0x1e73, 'u'),
        (0x1e75, 'u'),
        (0x1e77, 'u'),
        (0x1e79, 'ũ'),
        (0x1e7b, 'ū'),
        (0x1e7d, 'v'),
        (0x1e7f, 'v'),
        (0x1e81, 'w'),
        (0x1e83, 'w'),
        (0x1e85, 'w'),
        (0x1e87, 'w'),
        (0x1e89, 'w'),
        (0x1e8b, 'x'),
        (0x1e8d, 'x'),
        (0x1e8f, 'y'),
        (0x1e91, 'z'),
        (0x1e93, 'z'),
        (0x1e95, 'z'),
        (0x1e96, 'h'),
        (0x1e97, 't'),
        (0x1e98, 'w'),
        (0x1e99, 'y'),
        (0x1e9b, 'ſ'),
        (0x1ea1, 'a'),
        (0x1ea3, 'a'),
        (0x1ea5, 'â'),
        (0x1ea7, 'â'),
        (0x1ea9, 'â'),
        (0x1eab, 'â'),
        (0x1ead, 'ạ'),
        (0x1eaf, 'ă'),
        (0x1eb1, 'ă'),
        (0x1eb3, 'ă'),
        (0x1eb5, 'ă'),
        (0x1eb7, 'ạ'),
        (0x1eb9, 'e'),
        (0x1ebb, 'e'),
        (0x1ebd, 'e'),
        (0x1ebf, 'ê'),
        (0x1ec1, 'ê'),
        (0x1ec3, 'ê'),
        (0x1ec5, 'ê'),
        (0x1ec7, 'ẹ'),
        (0x1ec9, 'i'),
        (0x1ecb, 'i'),
        (0x1ecd, 'o'),
        (0x1ecf, 'o'),
        (0x1ed1, 'ô'),
        (0x1ed3, 'ô'),
        (0x1ed5, 'ô'),
        (0x1ed7, 'ô'),
        (0x1ed9, 'ọ'),
        (0x1edb, 'ơ'),
        (0x1edd, 'ơ'),
        (0x1edf, 'ơ'),
        (0x1ee1, 'ơ'),
        (0x1ee3, 'ơ'),
        (0x1ee5, 'u'),
        (0x1ee7, 'u'),
        (0x1ee9, 'ư'),
        (0x1eeb, 'ư'),
        (0x1eed, 'ư'),
        (0x1eef, 'ư'),
        (0x1ef1, 'ư'),
        (0x1ef3, 'y'),
        (0x1ef5, 'y'),
        (0x1ef7, 'y'),
        (0x1ef9, 'y'),
    ];
    TABLE
        .binary_search_by_key(&(c as u32), |(code, _)| *code)
        .ok()
        .map(|index| TABLE[index].1)
}

/// Fold one path to the identity a case-insensitive, default-normalizing
/// volume uses: Unicode lowercase, dropped combining marks, and the Latin
/// skeleton. Only called on Windows/macOS; Linux/case-sensitive volumes keep
/// byte identity (the rooted layer resolves real objects via `canonicalize`).
pub(crate) fn fold_volume_identity(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.to_lowercase().chars() {
        if is_combining_mark(c) {
            continue;
        }
        out.push(latin_skeleton(c).unwrap_or(c));
    }
    out
}

impl OwnershipSet {
    pub fn canonicalized(&self, base: &Path) -> Self {
        let base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
        let mut v: Vec<String> = Vec::new();
        for p in &self.0 {
            // The full-workspace policy token is NOT a filesystem path: it
            // must survive canonicalization byte-identically (P0-2, a generic
            // shell owns every workspace object). The runtime also returns it
            // before the normalizer; this keeps the normalizer total so no
            // caller can ever turn the sentinel into one particular path.
            if p == WORKSPACE_ROOT_SENTINEL {
                v.push(p.clone());
                continue;
            }
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

    #[test]
    fn workspace_root_ownership_overlaps_every_path() {
        let root = OwnershipSet::workspace_root();
        let a = OwnershipSet::new(["src/a.rs".to_string()]);
        assert!(
            root.overlaps(&a)
                && a.overlaps(&root)
                && root.overlaps(&OwnershipSet::new(["docs/".to_string()]))
        );
        assert!(!a.overlaps(&OwnershipSet::new(["src2/a.rs".to_string()])));
    }

    #[test]
    fn workspace_root_sentinel_survives_canonicalization() {
        // The shell's full-workspace ownership is a POLICY token, not a path:
        // canonicalizing it must leave it byte-identical, or the sentinel
        // would degrade to one directory that no longer overlaps every writer.
        let base = tempfile::tempdir().unwrap();
        let root = OwnershipSet::workspace_root().canonicalized(base.path());
        assert_eq!(
            root.entries(),
            &[WORKSPACE_ROOT_SENTINEL.to_string()],
            "canonicalization must never turn `**` into a path"
        );
        let any = OwnershipSet::new(["src/deep/file.rs".to_string()]).canonicalized(base.path());
        assert!(
            root.overlaps(&any),
            "the canonicalized sentinel must still overlap every workspace path"
        );
    }

    /// P1-SCHEDULER: the volume identity fold is Unicode-aware (not ASCII):
    /// case variants and NFC/NFD normalization variants of one name must
    /// fold together, on every platform, because the scheduler conservative
    /// identity must never be weaker than the mounted volume's.
    #[test]
    fn volume_identity_folds_unicode_case_and_normalization() {
        assert_eq!(
            fold_volume_identity("SRC/É.rs"),
            fold_volume_identity("src/é.rs")
        );
        // NFC `é` vs NFD `e` + U+0301 are the same name on a normalizing volume.
        assert_eq!(
            fold_volume_identity("é.rs"),
            fold_volume_identity("e\u{301}.rs")
        );
        // Case folding is Unicode, not ASCII: ẞ (U+1E9E) lowers to ß.
        assert_eq!(fold_volume_identity("ẞ"), fold_volume_identity("ß"));
        // Non-Latin scripts still case-fold.
        assert_eq!(fold_volume_identity("ФАЙЛ"), fold_volume_identity("файл"));
    }

    /// On a case-insensitive, default-normalizing CI volume, REAL aliases that
    /// address the same on-disk object must share one scheduler identity.
    /// (Runs on the macOS/Windows axes; Linux is case-sensitive by design.)
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn real_case_and_normalization_aliases_overlap_on_this_volume() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("Ä.txt"), b"x").unwrap();
        let upper = OwnershipSet::new(["Ä.txt".to_string()]).canonicalized(base.path());
        let lower = OwnershipSet::new(["ä.txt".to_string()]).canonicalized(base.path());
        assert!(
            upper.overlaps(&lower),
            "a real case alias must share one identity"
        );
        std::fs::write(base.path().join("é.txt"), b"x").unwrap();
        let nfc = OwnershipSet::new(["é.txt".to_string()]).canonicalized(base.path());
        let nfd = OwnershipSet::new(["e\u{301}.txt".to_string()]).canonicalized(base.path());
        assert!(
            nfc.overlaps(&nfd),
            "a real normalization alias must share one identity"
        );
    }

    /// On a case-SENSITIVE volume (Linux), distinct cases stay distinct, so
    /// the fold must not over-serialize... stronger: the scheduler must not
    /// claim two different files are one when the volume keeps them apart.
    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn case_distinct_names_stay_distinct_on_case_sensitive_volumes() {
        let base = tempfile::tempdir().unwrap();
        let upper = OwnershipSet::new(["SRC/a.rs".to_string()]).canonicalized(base.path());
        let lower = OwnershipSet::new(["src/a.rs".to_string()]).canonicalized(base.path());
        assert!(!upper.overlaps(&lower));
    }
}
