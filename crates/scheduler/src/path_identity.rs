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

/// Fold one path to the identity a case-insensitive, default-normalizing
/// volume uses: Unicode lowercase, full canonical decomposition (NFD), then
/// removal of every canonically-combining mark (combining class != 0).
///
/// This is the standard casefold -> NFD -> strip-marks identity. It is
/// normalization-pair complete and idempotent (both property-tested against
/// the generated Unicode corpus), unlike a hand-maintained approximation, so
/// `fold(x) == fold(y)` whenever a normalizing filesystem resolves x and y to
/// the same name.
pub(crate) fn fold_volume_identity(input: &str) -> String {
    let mut decomposed = String::with_capacity(input.len());
    for c in input.to_lowercase().chars() {
        push_decomposed(c, &mut decomposed);
    }
    decomposed
        .chars()
        .filter(|c| crate::unicode_data::combining_class(*c as u32) == 0)
        .collect()
}

/// Recursively expand one character's canonical decomposition into `out`.
fn push_decomposed(c: char, out: &mut String) {
    match crate::unicode_data::canonical_decomposition(c as u32) {
        Some(sequence) => {
            for code in sequence {
                if let Some(inner) = char::from_u32(*code) {
                    push_decomposed(inner, out);
                }
            }
        }
        None => out.push(c),
    }
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

    /// P1-SCHEDULER: the identity is normalization-pair complete over the WHOLE
    /// generated Unicode canonical-decomposition corpus: for every precomposed
    /// character and its NFD expansion, the folded identities are identical.
    #[test]
    fn normalization_corpus_folds_every_decomposition_pair() {
        let corpus = include_str!("../tests/fixtures/unicode-normalization-pairs.txt");
        let mut pairs = 0usize;
        for line in corpus.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (precomposed, decomposed) = line.split_once(';').expect("precomposed;decomposed");
            let pre: u32 = u32::from_str_radix(precomposed.trim(), 16).unwrap();
            let pre = char::from_u32(pre).expect("valid scalar");
            let decomposed: String = decomposed
                .split(',')
                .map(|code| char::from_u32(u32::from_str_radix(code.trim(), 16).unwrap()).unwrap())
                .collect();
            assert_eq!(
                fold_volume_identity(&pre.to_string()),
                fold_volume_identity(&decomposed),
                "NFC vs NFD identity for U+{:04X}",
                pre as u32
            );
            pairs += 1;
        }
        assert!(
            pairs > 2000,
            "the corpus must cover the full decomposition set"
        );
    }

    /// The fold is idempotent: folding an already-folded identity changes
    /// nothing (the previous hand table was not, because entries chained
    /// precomposed -> precomposed).
    #[test]
    fn volume_identity_is_idempotent() {
        for sample in [
            "ǖ.rs",
            "Ǖ.rs",
            "e\u{301}.rs",
            "É.rs",
            "ẞ",
            "ФАЙЛ",
            "a/b/c.rs",
            "\u{1E9B}\u{0323}",
        ] {
            let once = fold_volume_identity(sample);
            let twice = fold_volume_identity(&once);
            assert_eq!(once, twice, "fold must be idempotent for {sample:?}");
        }
        // The audit's concrete chain: ǖ (u + diaeresis + macron) and its NFD
        // form must share one identity.
        assert_eq!(
            fold_volume_identity("ǖ"),
            fold_volume_identity("u\u{308}\u{304}")
        );
    }
}
