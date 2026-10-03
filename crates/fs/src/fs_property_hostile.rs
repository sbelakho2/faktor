//! Deterministic property tests over a hostile path-byte corpora generator.
//!
//! A fixed-seed LCG builds arbitrary byte paths from an alphabet that
//! includes traversal dots, both separators, NUL, colons, backslashes and
//! multi-byte UTF-8 fragments. For every generated path the rooted authority
//! must either succeed or refuse with a typed error — never panic, never
//! touch the outside sentinel, and never leak a pathname outside the root.
//! Each seed/iteration pair asserts with its own message.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use super::{RootedDir, WalkBudget};

/// Deterministic xorshift64*: same seed → same corpus on every platform.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

const ALPHABET: &[u8] = &[
    b'a', b'b', b'/', b'/', b'.', b'.', b'.', b'\\', b':', b' ', b'\t', 0x00, 0xC3, 0xA9, b'-',
    b'~',
];

fn generate(seed: u64, len: usize) -> PathBuf {
    let mut rng = Lcg(seed | 1);
    let mut bytes = Vec::with_capacity(len);
    for _ in 0..len {
        let index = (rng.next() % ALPHABET.len() as u64) as usize;
        bytes.push(ALPHABET[index]);
    }
    PathBuf::from(OsString::from_vec(bytes))
}

/// Every generated path is either accepted under the root or refused typed;
/// the outside sentinel is byte-identical after every attempt.
#[test]
fn generated_paths_are_typed_and_confined() {
    let mut cases = 0usize;
    for seed in [1u64, 7, 42, 0xDEAD_BEEF] {
        for iteration in 0..16usize {
            cases += 1;
            let label = format!("seed={seed} iter={iteration}");
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("marker"), b"outside-marker").unwrap();
            let root = tmp.path().join("root");
            let dir = RootedDir::create(&root).unwrap();
            let path = generate(
                seed.wrapping_add(iteration as u64 * 7919),
                1 + iteration % 12,
            );

            // Every entry point must return Ok or a typed Error (no panic).
            let ops: Vec<(&str, Result<(), faktor_core::error::Error>)> = vec![
                ("create_dir_all", dir.create_dir_all(&path).map(|_| ())),
                ("open_create_new", dir.open_create_new(&path).map(|_| ())),
                ("read", dir.read(&path, 8).map(|_| ())),
                ("entry_meta", dir.entry_meta(&path).map(|_| ())),
                ("remove_tree", dir.remove_tree(&path)),
                ("sync_dir", dir.sync_dir(&path)),
                ("exists", {
                    let _ = dir.exists(&path);
                    Ok(())
                }),
            ];
            for (op, result) in ops {
                if let Err(err) = result {
                    assert!(
                        !err.message.is_empty(),
                        "case {label}: {op} must refuse with a typed message, got {err:?}"
                    );
                    assert!(
                        matches!(
                            err.kind,
                            faktor_core::error::ErrorKind::Malformed
                                | faktor_core::error::ErrorKind::Permission
                                | faktor_core::error::ErrorKind::Oversized
                                | faktor_core::error::ErrorKind::NotFound
                                | faktor_core::error::ErrorKind::Internal
                        ),
                        "case {label}: {op} refused with an undeclared kind: {err}"
                    );
                }
            }
            // A bounded walk over the generated subtree must not panic either.
            let mut budget = WalkBudget::new(64, 64, 16, 1 << 20, 1 << 20);
            let _ = dir.walk_bounded(&path, &mut budget, &[], &mut |_, _, _| {
                Ok(super::WalkStep::Continue)
            });
            assert_eq!(
                std::fs::read(outside.join("marker")).unwrap(),
                b"outside-marker",
                "case {label}: the outside sentinel must be untouched after {path:?}"
            );
            assert_eq!(
                std::fs::read_dir(&outside).unwrap().count(),
                1,
                "case {label}: no entry may appear outside the root after {path:?}"
            );
        }
    }
    assert!(
        cases >= 64,
        "the generated corpus must keep at least 64 cases, found {cases}"
    );
}

/// Determinism: the same seed regenerates the same bytes (the corpus is
/// reproducible evidence, not schedule-dependent fuzz).
#[test]
fn generator_is_deterministic() {
    for seed in [1u64, 7, 42] {
        let a = generate(seed, 32);
        let b = generate(seed, 32);
        assert_eq!(
            a, b,
            "seed {seed}: the generated corpus must be byte-identical across runs"
        );
        assert!(
            a.as_os_str().len() <= 32 * 2,
            "seed {seed}: generated length must stay bounded"
        );
    }
}

/// Traversal is impossible by construction: for any accepted generated path,
/// the resolved on-disk entry lives under the root (checked through a scan
/// that only uses the authority itself).
#[test]
fn accepted_generated_paths_stay_under_root() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("marker"), b"outside-marker").unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();

    let mut accepted = 0usize;
    for seed in [3u64, 11, 99, 1234] {
        for iteration in 0..16usize {
            let path = generate(seed.wrapping_add(iteration as u64 * 104_729), 4);
            if dir.create_dir_all(&path).is_ok() {
                accepted += 1;
                // The authority's own listing must be able to see it; an
                // absolute std join must either agree or be unreachable
                // (deep/odd bytes), never point outside.
                let joined = dir.join(&path);
                let under_root = joined.starts_with(&root);
                assert!(
                    under_root,
                    "case seed={seed} iter={iteration}: accepted path {path:?} joined outside the root: {}",
                    joined.display()
                );
            }
        }
    }
    assert!(
        accepted > 0,
        "the corpus must exercise at least one accepted path"
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        1,
        "no accepted path may leak outside"
    );
}
