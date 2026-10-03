//! Adversarial transactional edges of the shared atomic-write authority.
//!
//! Table-driven CAS-state matrices, a racing-writer matrix, crash/permission
//! seams and parent-swap confinement around the single `atomic_*` module
//! every writer in the runtime shares. Every row asserts one outcome with its
//! own message and checks the two invariants every crash path must keep: the
//! destination is either the old or the new WHOLE content (never partial),
//! and no internal temp survives.

use std::path::Path;

use faktor_core::error::{Error, ErrorKind};
use faktor_core::hash::FileHash;

use super::*;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

fn digest(bytes: &[u8]) -> FileHash {
    FileHash::from(blake3::hash(bytes).into())
}

/// One CAS matrix row: a constructed expected state, a mutation applied
/// before the CAS, and whether the CAS must win.
struct CasRow {
    label: &'static str,
    expect: FileState,
    mutate: Option<Vec<u8>>,
    removed_before: bool,
    should_win: bool,
}

fn cas_rows(base: &[u8]) -> Vec<CasRow> {
    let size = base.len() as u64;
    let digest = blake3::hash(base);
    let exact = FileState {
        exists: true,
        size: Some(size),
        digest: Some(FileHash::from(digest.into())),
        modified_ms: None,
    };
    let exists_only = FileState {
        exists: true,
        size: None,
        digest: None,
        modified_ms: None,
    };
    let size_only = FileState {
        exists: true,
        size: Some(size),
        digest: None,
        modified_ms: None,
    };
    let stale_digest = FileState {
        exists: true,
        size: Some(size),
        digest: Some(FileHash::from(blake3::hash(b"different").into())),
        modified_ms: None,
    };
    let wrong_size = FileState {
        exists: true,
        size: Some(size + 1),
        digest: None,
        modified_ms: None,
    };
    let absent = FileState::absent();
    let stale_mtime = FileState {
        exists: true,
        size: None,
        digest: None,
        modified_ms: Some(1),
    };
    let mut rows = Vec::new();
    let mut push = |label: &'static str,
                    expect: FileState,
                    mutate: Option<Vec<u8>>,
                    removed_before: bool,
                    should_win: bool| {
        rows.push(CasRow {
            label,
            expect,
            mutate,
            removed_before,
            should_win,
        });
    };

    // Exact state against each actual mutation.
    push("exact-vs-unchanged", exact.clone(), None, false, true);
    push(
        "exact-vs-same-size-different-bytes",
        exact.clone(),
        Some(b"base-mutated-00".to_vec()),
        false,
        false,
    );
    push(
        "exact-vs-shorter",
        exact.clone(),
        Some(b"tiny".to_vec()),
        false,
        false,
    );
    push("exact-vs-removed", exact.clone(), None, true, false);
    push(
        "exists-only-vs-unchanged",
        exists_only.clone(),
        None,
        false,
        true,
    );
    push(
        "exists-only-vs-mutated",
        exists_only.clone(),
        Some(b"base-mutated-00".to_vec()),
        false,
        true,
    );
    push(
        "exists-only-vs-removed",
        exists_only.clone(),
        None,
        true,
        false,
    );
    push(
        "size-only-vs-unchanged",
        size_only.clone(),
        None,
        false,
        true,
    );
    push("size-only-vs-removed", size_only.clone(), None, true, false);
    push(
        "stale-digest-vs-unchanged",
        stale_digest.clone(),
        None,
        false,
        false,
    );
    push(
        "wrong-size-vs-unchanged",
        wrong_size.clone(),
        None,
        false,
        false,
    );
    push("absent-vs-removed", absent.clone(), None, true, true);
    push("absent-vs-present", absent.clone(), None, false, false);
    push(
        "stale-mtime-vs-unchanged",
        stale_mtime.clone(),
        None,
        false,
        false,
    );
    // A same-bytes rewrite pins every captured axis again: the inode is
    // deliberately NOT a CAS axis (the digest is the identity), so the CAS
    // wins — a rewrite with the same bytes is not a clobber.
    push(
        "exact-vs-same-bytes-rewrite",
        exact.clone(),
        Some(base.to_vec()),
        false,
        true,
    );
    rows
}

/// Every row of the CAS matrix asserts its typed outcome, the destination's
/// whole-file state, and the absence of temp residue.
#[test]
fn cas_state_matrix_matches_only_the_pinned_state() {
    let dir = tempdir();
    let base = b"base-state-0123456789abcdef";
    for (index, row) in cas_rows(base).into_iter().enumerate() {
        let target = dir.path().join(format!("cas-{index}.bin"));
        std::fs::write(&target, base).unwrap();
        if row.removed_before {
            std::fs::remove_file(&target).unwrap();
        }
        if let Some(mutation) = &row.mutate {
            std::fs::write(&target, mutation).unwrap();
        }
        let result = atomic_replace_cas(&target, &row.expect, b"winner-payload");
        if row.should_win {
            let err = result.err().map(|e| format!("{e}"));
            assert!(
                err.is_none(),
                "case {:?}: CAS pinned to the actual state must win, got {:?}",
                row.label,
                err
            );
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"winner-payload",
                "case {:?}: a winning CAS writes the whole new payload",
                row.label
            );
        } else {
            let err = result.err().unwrap_or_else(|| {
                panic!(
                    "case {:?}: CAS must refuse a state it did not pin",
                    row.label
                )
            });
            assert_eq!(
                err.kind,
                ErrorKind::Conflict,
                "case {:?}: a CAS refusal is a typed conflict: {err}",
                row.label
            );
            assert!(
                err.message.contains("cas mismatch"),
                "case {:?}: the conflict must name the CAS recheck: {err}",
                row.label
            );
            if row.removed_before {
                assert!(
                    !target.exists(),
                    "case {:?}: a refused CAS must not resurrect the file",
                    row.label
                );
            } else {
                let expected_content: &[u8] = row.mutate.as_deref().unwrap_or(base);
                assert_eq!(
                    std::fs::read(&target).unwrap(),
                    expected_content,
                    "case {:?}: the losing writer's bytes must be untouched",
                    row.label
                );
            }
        }
        let listing = names(dir.path());
        let expect_present = !row.removed_before || row.should_win;
        let expect_listing: Vec<String> = if expect_present {
            vec![format!("cas-{index}.bin")]
        } else {
            Vec::new()
        };
        assert_eq!(
            listing, expect_listing,
            "case {:?}: the directory must hold exactly the destination (never a temp) ({listing:?})",
            row.label
        );
        // Reset the fixture for the next, independent row.
        let _ = std::fs::remove_file(&target);
    }
}

/// The digest mismatch refusal carries forensics: both the expected and the
/// found digest/size render in the message (so an operator can tell a real
/// concurrent writer from corruption).
#[test]
fn cas_mismatch_reports_expected_and_found_state() {
    let dir = tempdir();
    let target = dir.path().join("forensics.bin");
    std::fs::write(&target, b"actual-bytes").unwrap();
    let expect = FileState {
        exists: true,
        size: Some(999),
        digest: Some(digest(b"expected-bytes")),
        modified_ms: None,
    };
    let err = atomic_replace_cas(&target, &expect, b"nope").unwrap_err();
    assert!(
        err.message.contains("expected exists=true"),
        "the refusal must render the expected state: {err}"
    );
    assert!(
        err.message.contains("found exists=true"),
        "the refusal must render the found state: {err}"
    );
    assert!(
        err.message.contains("size=Some(999)"),
        "the expected size must be named: {err}"
    );
    assert!(
        err.message.contains(&digest(b"expected-bytes").to_hex()),
        "the expected digest must be named: {err}"
    );
}

/// Cooperative racing writers: every round has N writers that all captured
/// the same base; exactly one wins, every loser gets a typed conflict, the
/// final file is one whole payload, and no temp survives.
#[test]
fn racing_cas_writers_never_clobber_or_tear() {
    use std::sync::{Arc, Barrier};

    let dir = tempdir();
    for round in 0..8usize {
        let target = dir.path().join(format!("race-{round}.bin"));
        std::fs::write(&target, b"round-base").unwrap();
        let expected = FileState::now_with_digest(&target).unwrap();
        let writers = 6usize;
        let barrier = Arc::new(Barrier::new(writers));
        let mut handles = Vec::new();
        for writer in 0..writers {
            let target = target.clone();
            let expected = expected.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let payload = format!("writer-{writer}-{}", "p".repeat(64));
                match atomic_replace_cas(&target, &expected, payload.as_bytes()) {
                    Ok(_) => Some(payload),
                    Err(e) => {
                        assert_eq!(
                            e.kind,
                            ErrorKind::Conflict,
                            "round {round} writer {writer}: a loser must be a typed conflict: {e}"
                        );
                        assert!(
                            e.message.contains("cas mismatch"),
                            "round {round} writer {writer}: the loser must name the recheck: {e}"
                        );
                        None
                    }
                }
            }));
        }
        let winners: Vec<String> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "round {round}: exactly one writer may win, got {winners:?}"
        );
        let final_bytes = std::fs::read(&target).unwrap();
        assert_eq!(
            final_bytes,
            winners[0].as_bytes(),
            "round {round}: the file must hold exactly the winning payload"
        );
        let listing = names(dir.path());
        assert_eq!(
            listing,
            vec![format!("race-{round}.bin")],
            "round {round}: no temp may survive the race ({listing:?})"
        );
        std::fs::remove_file(&target).unwrap();
    }
}

/// Crash seams: each stage of the replace pipeline is poisoned from outside
/// (missing parent, destination swapped for a directory, verify refusal).
/// Every outcome is typed, the previous state is preserved whole, and no
/// temp is left behind.
#[test]
fn crash_seams_preserve_whole_previous_content_and_leave_no_temp() {
    // 1. Parent removed between staging and rename: the temp write cannot
    //    even be created.
    let dir = tempdir();
    let parent = dir.path().join("gone");
    std::fs::create_dir_all(&parent).unwrap();
    let target = parent.join("f.bin");
    std::fs::write(&target, b"previous").unwrap();
    let expected = FileState::now_with_digest(&target).unwrap();
    std::fs::remove_dir_all(&parent).unwrap();
    let err = atomic_replace_cas(&target, &expected, b"new").unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Conflict,
        "case remove-parent: a vanished file is refused by the cheap precheck: {err}"
    );
    assert!(
        err.message.contains("cas mismatch"),
        "case remove-parent: the vanished-file refusal names the recheck: {err}"
    );
    assert!(
        !target.exists(),
        "case remove-parent: nothing may be written under a vanished parent"
    );

    // 2. Destination swapped for a DIRECTORY between staging and rename: the
    //    rename refuses, the directory survives, no temp remains.
    let dir2 = tempdir();
    let target2 = dir2.path().join("swap.bin");
    std::fs::write(&target2, b"previous").unwrap();
    let expected2 = FileState::now_with_digest(&target2).unwrap();
    std::fs::remove_file(&target2).unwrap();
    std::fs::create_dir(&target2).unwrap();
    std::fs::write(target2.join("inner"), b"keep").unwrap();
    let err = atomic_replace_cas(&target2, &expected2, b"new").unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Internal,
        "case dest-dir: the strong digest recheck refuses a directory destination: {err}"
    );
    assert!(
        err.message.contains("Is a directory") || err.message.contains("read"),
        "case dest-dir: the refusal must name the unreadable destination: {err}"
    );
    assert!(
        target2.join("inner").exists(),
        "case dest-dir: the directory must survive untouched"
    );
    assert_eq!(
        names(dir2.path()),
        vec!["swap.bin".to_string()],
        "case dest-dir: no temp may survive"
    );

    // 3. Destination replaced by an unrelated file AFTER the precheck but
    //    BEFORE the rename, through the guarded verify seam.
    let dir3 = tempdir();
    let target3 = dir3.path().join("verify.bin");
    std::fs::write(&target3, b"previous").unwrap();
    let expected3 = FileState::now_with_digest(&target3).unwrap();
    let err = atomic_replace_cas_guarded(&target3, &expected3, b"attacker", &|path| {
        // Simulate a parent-swap detected by the caller's re-verification.
        std::fs::write(path, b"raced-writer").unwrap();
        Err(Error::permission("parent re-verification failed"))
    })
    .unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Permission,
        "case verify-refusal: the verify error must surface typed: {err}"
    );
    assert_eq!(
        std::fs::read(&target3).unwrap(),
        b"raced-writer",
        "case verify-refusal: the raced destination is never clobbered"
    );
    assert_eq!(
        names(dir3.path()),
        vec!["verify.bin".to_string()],
        "case verify-refusal: the staged temp must be removed"
    );

    // 4. The unguarded replace with a verify that refuses, destination
    //    absent: an absent destination must stay absent.
    let dir4 = tempdir();
    let target4 = dir4.path().join("absent.bin");
    let err =
        atomic_replace_guarded(&target4, b"new", &|_| Err(Error::conflict("nope"))).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Conflict,
        "case absent-verify: typed refusal expected: {err}"
    );
    assert!(
        !target4.exists(),
        "case absent-verify: a refused write must not create the destination"
    );
    assert_eq!(
        names(dir4.path()),
        Vec::<String>::new(),
        "case absent-verify: no temp may survive"
    );
}

/// `atomic_create` exclusive semantics under a racing matrix: exactly one of
/// N contenders wins, the losers are typed conflicts, and the winner's bytes
/// are the whole file.
#[test]
fn atomic_create_racing_contenders_have_exactly_one_winner() {
    use std::sync::{Arc, Barrier};
    let dir = tempdir();
    for round in 0..6usize {
        let target = dir.path().join(format!("create-{round}.bin"));
        let contenders = 5usize;
        let barrier = Arc::new(Barrier::new(contenders));
        let mut handles = Vec::new();
        for contender in 0..contenders {
            let target = target.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let payload = format!("contender-{contender}");
                atomic_create(&target, payload.as_bytes())
                    .map(|_| payload)
                    .map_err(|e| (e.kind, e.message))
            }));
        }
        let mut win = None;
        for (i, handle) in handles.into_iter().enumerate() {
            match handle.join().unwrap() {
                Ok(payload) => {
                    assert!(
                        win.is_none(),
                        "round {round}: contender {i} won but {:?} already won",
                        win
                    );
                    win = Some(payload);
                }
                Err((kind, msg)) => {
                    assert_eq!(
                        kind,
                        ErrorKind::Conflict,
                        "round {round} contender {i}: a loser must be a typed conflict: {msg}"
                    );
                }
            }
        }
        let winner = win.expect("exactly one contender must win");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            winner.as_bytes(),
            "round {round}: the winner's whole payload must be the file"
        );
        assert_eq!(
            names(dir.path()),
            vec![format!("create-{round}.bin")],
            "round {round}: no temp may survive"
        );
        std::fs::remove_file(&target).unwrap();
    }
}

/// Parent-symlink swap confinement for the adopt/guarded paths: when the
/// caller's re-verification detects the swap, the staged temp stays inside
/// the original directory and the outside tree is never written.
#[test]
fn parent_swap_confines_guarded_writes_to_the_original_directory() {
    let dir = tempdir();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("marker"), b"outside-marker").unwrap();
    let parent = dir.path().join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    let target = parent.join("f.bin");
    std::fs::write(&target, b"previous").unwrap();

    // Swap the parent for an outside symlink inside the verify hook, then
    // refuse: a caller that merely verifies must never follow the link.
    let moved = dir.path().join("parent-moved");
    std::fs::rename(&parent, &moved).unwrap();
    std::os::unix::fs::symlink(&outside, &parent).unwrap();
    let err = atomic_replace_guarded(&target, b"new", &|_| {
        Err(Error::permission("parent swapped for a symlink"))
    })
    .unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Permission,
        "the swapped parent must surface the caller's typed refusal: {err}"
    );
    assert_eq!(
        std::fs::read(outside.join("marker")).unwrap(),
        b"outside-marker",
        "the outside sentinel must be untouched"
    );
    assert_eq!(
        names(&outside),
        vec!["marker".to_string()],
        "no temp or destination may appear outside the original parent"
    );
    assert_eq!(
        std::fs::read(moved.join("f.bin")).unwrap(),
        b"previous",
        "the moved original must stay byte-identical"
    );
    assert_eq!(
        names(&moved),
        vec!["f.bin".to_string()],
        "the staged temp must have been cleaned from the original parent"
    );
}

/// Boundary payloads through the whole replace pipeline: empty bytes, a
/// one-byte payload, a payload at the 64 KiB internal read buffer boundary
/// and one just past it, plus a 4 MiB payload. Each is asserted as a whole
/// round-trip with its digest.
#[test]
fn replace_payload_boundaries_round_trip_whole() {
    let dir = tempdir();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one-byte", vec![0x00]),
        ("63k", vec![b'a'; 63 * 1024]),
        ("64k", vec![b'b'; 64 * 1024]),
        ("64k-plus-one", vec![b'c'; 64 * 1024 + 1]),
        ("four-mib", vec![b'd'; 4 * 1024 * 1024]),
    ];
    for (label, payload) in cases {
        let target = dir.path().join("boundary.bin");
        let hash = atomic_replace(&target, &payload)
            .unwrap_or_else(|e| panic!("case {label}: replace failed: {e}"));
        assert_eq!(
            hash,
            digest(&payload),
            "case {label}: the returned hash must be the whole payload's"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            payload,
            "case {label}: the stored bytes must equal the payload"
        );
        assert_eq!(
            names(dir.path()),
            vec!["boundary.bin".to_string()],
            "case {label}: no temp may survive"
        );
        std::fs::remove_file(&target).unwrap();
    }
}

/// `atomic_adopt` and `atomic_adopt_dir` refusal matrix: missing temp,
/// missing parent, non-directory temp for the dir variant, and a directory
/// temp for the file variant — every refusal typed, destination untouched.
#[test]
fn adopt_refusals_are_typed_and_destination_preserving() {
    #[derive(Clone, Copy)]
    enum Adopt {
        File,
        Dir,
    }
    let rows: [(Adopt, &str, &str); 5] = [
        (Adopt::File, "missing-temp", "ghost"),
        (Adopt::Dir, "missing-dir-temp", "ghost"),
        (Adopt::File, "dir-as-file-temp", "dir-temp"),
        (Adopt::Dir, "file-as-dir-temp", "file-temp"),
        (Adopt::Dir, "missing", "ghost-dir"),
    ];
    for (arm, label, temp_name) in rows {
        let dir = tempdir();
        let dest = dir.path().join("dest");
        std::fs::write(&dest, b"previous").unwrap();
        let tmp = dir.path().join(temp_name);
        match label {
            "dir-temp" => std::fs::create_dir(&tmp).unwrap(),
            "file-temp" => std::fs::write(&tmp, b"file").unwrap(),
            _ => {}
        }
        let result = match arm {
            Adopt::File => atomic_adopt(&tmp, &dest).map(|_| ()),
            Adopt::Dir => atomic_adopt_dir(&tmp, &dest),
        };
        assert!(
            result.is_err(),
            "case {label}: the adoption must be refused typed"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"previous",
            "case {label}: a refused adoption must keep the destination whole"
        );
    }
}

/// `atomic_replace_cas` against a destination whose parent is a symlink to an
/// outside directory: the path-based CAS (used by store/bootstrap writers
/// that do not hold a directory handle) writes through the link by design —
/// this test documents the seam so callers that need confinement must use
/// the guarded/rooted forms (asserted here as the contrast).
#[test]
fn unguarded_path_cas_follows_a_symlinked_parent_but_guarded_forms_do_not_hide_it() {
    let dir = tempdir();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("marker"), b"outside-marker").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let target = link.join("f.bin");
    // The unguarded primitive operates on the pathname it was given: the
    // write lands in the link target. This is the documented reason callers
    // that must stay rooted use `WorkspaceHandle`/`RootedDir` instead.
    atomic_replace(&target, b"through-link").unwrap();
    assert_eq!(
        std::fs::read(outside.join("f.bin")).unwrap(),
        b"through-link",
        "the unguarded path primitive writes through a symlinked parent"
    );
    // The guarded form lets the caller re-verify the parent fd-relative and
    // refuse; the outside marker is untouched by the refusal.
    let guarded_target = link.join("g.bin");
    let err = atomic_replace_guarded(&guarded_target, b"guarded", &|_| {
        Err(Error::permission("parent is not workspace content"))
    })
    .unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Permission,
        "the guarded form must surface the caller's re-verification: {err}"
    );
    assert!(
        !outside.join("g.bin").exists(),
        "a refused guarded write must not land outside"
    );
    assert_eq!(
        std::fs::read(outside.join("marker")).unwrap(),
        b"outside-marker"
    );
}
