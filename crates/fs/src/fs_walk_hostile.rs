//! Adversarial boundaries of the bounded handle-relative walker.
//!
//! Table-driven charge boundaries for every [`WalkBudget`] axis, saturation,
//! depth checks, structural visitor outcomes (Stop/SkipDir/skip_dirs), and
//! the no-follow classification of symlinks and special entries. Each row
//! asserts its exact outcome and counter state with its own message.

#![cfg(unix)]

use std::path::Path;

use faktor_core::error::ErrorKind;

use super::{RootedDir, RootedEntryKind, WalkBudget, WalkStep};

fn budget(
    max_entries: usize,
    max_dirs: usize,
    max_depth: usize,
    max_file: u64,
    max_read: u64,
) -> WalkBudget {
    WalkBudget::new(max_entries, max_dirs, max_depth, max_file, max_read)
}

#[derive(Clone, Copy, Debug)]
enum Charge {
    Entry,
    Directory,
    FileBytes,
    ReadBytes,
}

fn charge(
    budget: &mut WalkBudget,
    what: Charge,
    amount: u64,
    depth: usize,
) -> Result<(), faktor_core::error::Error> {
    match what {
        Charge::Entry => budget.charge_entry(depth),
        Charge::Directory => budget.charge_directory(depth),
        Charge::FileBytes => budget.charge_file_bytes(amount),
        Charge::ReadBytes => budget.charge_read_bytes(amount),
    }
}

/// A charge table row: the cap, how many charges land, and the 1-based index
/// of the charge that must fail (`None` = all succeed).
struct ChargeRow {
    label: &'static str,
    what: Charge,
    cap: u64,
    max_depth: usize,
    charges: u64,
    fail_at: Option<u64>,
    depth: usize,
}

fn charge_rows() -> Vec<ChargeRow> {
    let mut rows = Vec::new();
    let mut push = |label, what, cap, max_depth, charges, fail_at, depth| {
        rows.push(ChargeRow {
            label,
            what,
            cap,
            max_depth,
            charges,
            fail_at,
            depth,
        })
    };
    // Entry budget boundaries.
    push("entry-cap-0-one-charge", Charge::Entry, 0, 8, 1, Some(1), 0);
    push("entry-cap-1-first", Charge::Entry, 1, 8, 1, None, 0);
    push("entry-cap-1-second", Charge::Entry, 1, 8, 2, Some(2), 0);
    push("entry-cap-3-exact", Charge::Entry, 3, 8, 3, None, 0);
    push("entry-cap-3-over", Charge::Entry, 3, 8, 4, Some(4), 0);
    push(
        "entry-cap-max-single",
        Charge::Entry,
        u64::MAX,
        8,
        1,
        None,
        0,
    );
    // Directory budget boundaries.
    push("dir-cap-0-root", Charge::Directory, 0, 8, 1, Some(1), 0);
    push("dir-cap-1-first", Charge::Directory, 1, 8, 1, None, 0);
    push("dir-cap-1-second", Charge::Directory, 1, 8, 2, Some(2), 0);
    push("dir-cap-2-exact", Charge::Directory, 2, 8, 2, None, 0);
    push("dir-cap-2-over", Charge::Directory, 2, 8, 3, Some(3), 0);
    // File-byte budget boundaries.
    push("file-bytes-cap-0", Charge::FileBytes, 0, 8, 1, Some(1), 0);
    push("file-bytes-exact", Charge::FileBytes, 10, 8, 10, None, 0);
    push(
        "file-bytes-over-by-one",
        Charge::FileBytes,
        10,
        8,
        11,
        Some(11),
        0,
    );
    push(
        "file-bytes-chunked-exact",
        Charge::FileBytes,
        10,
        8,
        10,
        None,
        0,
    );
    // Read-byte budget boundaries (charged by callers, same authority).
    push("read-bytes-cap-0", Charge::ReadBytes, 0, 8, 1, Some(1), 0);
    push("read-bytes-exact", Charge::ReadBytes, 64, 8, 64, None, 0);
    push("read-bytes-over", Charge::ReadBytes, 64, 8, 65, Some(65), 0);
    // Depth boundary rows: a charge at exactly max_depth passes, one past
    // fails (the budget is depth-inclusive).
    push("entry-depth-at-bound", Charge::Entry, 8, 2, 1, None, 2);
    push("entry-depth-past-bound", Charge::Entry, 8, 2, 1, Some(1), 3);
    push("dir-depth-at-bound", Charge::Directory, 8, 2, 1, None, 2);
    push(
        "dir-depth-past-bound",
        Charge::Directory,
        8,
        2,
        1,
        Some(1),
        3,
    );
    push("entry-depth-zero-cap", Charge::Entry, 8, 0, 1, None, 0);
    push("entry-depth-zero-over", Charge::Entry, 8, 0, 1, Some(1), 1);
    rows
}

/// Each row asserts the charge that fails (and only that one), the exact
/// counter state, and the typed kind of the exhaustion error.
#[test]
fn budget_charge_boundaries_are_exact() {
    for row in charge_rows() {
        let mut b = if matches!(row.what, Charge::FileBytes | Charge::ReadBytes) {
            budget(usize::MAX, usize::MAX, row.max_depth, row.cap, row.cap)
        } else {
            budget(
                row.cap as usize,
                row.cap as usize,
                row.max_depth,
                u64::MAX,
                u64::MAX,
            )
        };
        for index in 1..=row.charges {
            let result = charge(&mut b, row.what, 1, row.depth);
            match row.fail_at {
                Some(fail) if index == fail => {
                    let err = match result {
                        Err(err) => err,
                        Ok(()) => panic!(
                            "case {:?}: charge {index} must fail the {:?} budget",
                            row.label, row.what
                        ),
                    };
                    assert_eq!(
                        err.kind,
                        ErrorKind::Oversized,
                        "case {:?}: exhaustion must be typed oversized: {err}",
                        row.label
                    );
                    assert!(
                        !err.message.is_empty(),
                        "case {:?}: the exhaustion must name the budget: {err}",
                        row.label
                    );
                }
                Some(_) | None => {
                    result.unwrap_or_else(|e| {
                        panic!(
                            "case {:?}: charge {index} must still fit the budget: {e}",
                            row.label
                        )
                    });
                }
            }
        }
    }
}

/// Saturating boundary rows: a charge at `u64::MAX` must never overflow, and
/// a second charge over the cap fails typed while the total stays capped.
#[test]
fn budget_counters_saturate_at_the_extremes() {
    let rows: [(&str, u64, u64, bool); 6] = [
        ("file-huge-charge-saturates", u64::MAX, u64::MAX, false),
        ("file-cap-100-exact-then-one", 100, 100, true),
        ("file-cap-100-under-then-one", 100, 99, false),
        ("file-cap-zero-then-one", 0, 0, true),
        (
            "file-near-max-saturates-past-cap",
            u64::MAX - 1,
            u64::MAX - 1,
            true,
        ),
        ("file-near-max-under-cap", u64::MAX - 1, u64::MAX - 2, false),
    ];
    for (label, cap, first_charge, second_charge_fails) in rows {
        let mut b = budget(usize::MAX, usize::MAX, 8, cap, cap);
        charge(&mut b, Charge::FileBytes, first_charge, 0)
            .unwrap_or_else(|e| panic!("case {label}: the first charge must fit the cap: {e}"));
        let before = b.total_file_bytes();
        let second = charge(&mut b, Charge::FileBytes, 1, 0);
        assert_eq!(
            second.is_err(),
            second_charge_fails,
            "case {label}: the follow-up charge outcome must match the cap"
        );
        if second_charge_fails {
            // Saturation proof: a refused charge must never move the counter
            // backwards (no overflow wrap-around).
            assert!(
                b.total_file_bytes() >= before,
                "case {label}: a refused charge must never wrap the counter below {before}"
            );
        }
        // Entry counters are usize: saturate at usize::MAX without panicking.
        let mut e = budget(usize::MAX, usize::MAX, 8, u64::MAX, u64::MAX);
        e.charge_entry(0).unwrap();
        assert_eq!(
            e.entries(),
            1,
            "case {label}: the entry counter advances once"
        );
    }
}

/// Structural visitor outcomes through `walk_bounded`: Stop truncates the
/// walk successfully, SkipDir prevents descent into exactly one directory,
/// `skip_dirs` never visits a statically skipped directory, and exhausted
/// budgets abort with a typed error naming the axis.
#[test]
fn walk_visitor_and_skip_matrix_behaves_exactly() {
    #[derive(Clone, Copy)]
    enum Action {
        ContinueAll,
        StopAt(&'static str),
        SkipDir(&'static str),
        FailOnFile,
    }
    struct Row {
        label: &'static str,
        action: Action,
        skip_dirs: &'static [&'static str],
        max_entries: usize,
        expect_error: bool,
        expect_error_kind: Option<ErrorKind>,
    }
    let rows = [
        Row {
            label: "continue-all",
            action: Action::ContinueAll,
            skip_dirs: &[],
            max_entries: 1024,
            expect_error: false,
            expect_error_kind: None,
        },
        Row {
            label: "stop-at-b",
            action: Action::StopAt("b.txt"),
            skip_dirs: &[],
            max_entries: 1024,
            expect_error: false,
            expect_error_kind: None,
        },
        Row {
            label: "skip-dir-d",
            action: Action::SkipDir("d"),
            skip_dirs: &[],
            max_entries: 1024,
            expect_error: false,
            expect_error_kind: None,
        },
        Row {
            label: "static-skip-d",
            action: Action::ContinueAll,
            skip_dirs: &["d"],
            max_entries: 1024,
            expect_error: false,
            expect_error_kind: None,
        },
        Row {
            label: "entry-budget-exhausted",
            action: Action::ContinueAll,
            skip_dirs: &[],
            max_entries: 2,
            expect_error: true,
            expect_error_kind: Some(ErrorKind::Oversized),
        },
        Row {
            label: "zero-entry-budget",
            action: Action::ContinueAll,
            skip_dirs: &[],
            max_entries: 0,
            expect_error: true,
            expect_error_kind: Some(ErrorKind::Oversized),
        },
        Row {
            label: "visitor-error-propagates",
            action: Action::FailOnFile,
            skip_dirs: &[],
            max_entries: 1024,
            expect_error: true,
            expect_error_kind: Some(ErrorKind::Internal),
        },
    ];
    for row in rows {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let dir = RootedDir::create(&root).unwrap();
        dir.create_dir_all(Path::new("d/sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("b.txt"), b"b").unwrap();
        std::fs::write(root.join("c.txt"), b"c").unwrap();
        std::fs::write(root.join("d/x.txt"), b"x").unwrap();
        std::fs::write(root.join("d/sub/y.txt"), b"y").unwrap();
        let mut budget = budget(row.max_entries, 1024, 32, 1 << 20, 1 << 20);
        let mut visited: Vec<String> = Vec::new();
        let result = dir.walk_bounded(
            Path::new(""),
            &mut budget,
            row.skip_dirs,
            &mut |entry, _, _| {
                visited.push(entry.rel.to_string_lossy().into_owned());
                match row.action {
                    Action::ContinueAll => Ok(WalkStep::Continue),
                    Action::StopAt(name) if entry.rel == Path::new(name) => Ok(WalkStep::Stop),
                    Action::SkipDir(name) if entry.rel == Path::new(name) => Ok(WalkStep::SkipDir),
                    Action::FailOnFile if entry.is_file() => {
                        Err(faktor_core::error::Error::internal("visitor refused"))
                    }
                    _ => Ok(WalkStep::Continue),
                }
            },
        );
        if row.expect_error {
            let err = result.expect_err(&format!("case {}: walk must fail", row.label));
            if let Some(kind) = row.expect_error_kind {
                assert_eq!(
                    err.kind, kind,
                    "case {}: the failure must carry the expected kind: {err}",
                    row.label
                );
            }
        } else {
            result.unwrap_or_else(|e| panic!("case {}: walk must succeed: {e}", row.label));
        }
        match row.action {
            Action::StopAt(name) => {
                assert_eq!(
                    visited.last().map(String::as_str),
                    Some(name),
                    "case {}: Stop must end the walk exactly at the named entry",
                    row.label
                );
                assert!(
                    !visited.iter().any(|v| v == "d/sub/y.txt"),
                    "case {}: nothing after Stop may be visited",
                    row.label
                );
            }
            Action::SkipDir("d") => {
                assert!(
                    visited.iter().any(|v| v == "d"),
                    "case {}: the skipped directory itself is still visited",
                    row.label
                );
                assert!(
                    !visited.iter().any(|v| v == "d/x.txt" || v == "d/sub/y.txt"),
                    "case {}: SkipDir must prevent descent (visited {visited:?})",
                    row.label
                );
            }
            Action::ContinueAll if row.skip_dirs.len() == 1 && row.skip_dirs[0] == "d" => {
                assert!(
                    !visited.iter().any(|v| v == "d" || v.starts_with("d/")),
                    "case {}: a statically skipped directory is never visited or walked ({visited:?})",
                    row.label
                );
            }
            _ => {}
        }
    }
}

/// A symlinked directory entry is classified and visited but NEVER descended:
/// the walk stays inside the root and the outside tree keeps exactly its own
/// marker. A FIFO entry is classified `Other` and never opened.
#[test]
fn walk_never_traverses_symlink_directories_or_opens_special_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"outside-secret").unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();
    dir.create_dir_all(Path::new("real")).unwrap();
    std::fs::write(root.join("real/in.txt"), b"inside").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let fifo = root.join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo utility must exist on unix CI hosts");
    assert!(
        status.success(),
        "fixture fifo creation must succeed: {status:?}"
    );

    let mut budget = budget(64, 64, 16, 1 << 20, 1 << 20);
    let mut by_rel: std::collections::HashMap<String, RootedEntryKind> = Default::default();
    let mut opened: Vec<String> = Vec::new();
    dir.walk_bounded(Path::new(""), &mut budget, &[], &mut |entry, _, _| {
        by_rel.insert(entry.rel.to_string_lossy().into_owned(), entry.kind);
        if entry.is_file() {
            opened.push(entry.rel.to_string_lossy().into_owned());
        }
        Ok(WalkStep::Continue)
    })
    .unwrap();

    assert_eq!(
        by_rel.get("link"),
        Some(&RootedEntryKind::Symlink),
        "a symlinked directory entry must be classified as a symlink"
    );
    assert_eq!(
        by_rel.get("pipe"),
        Some(&RootedEntryKind::Other),
        "a FIFO must be classified as Other, never opened"
    );
    assert!(
        !by_rel.keys().any(|k| k.starts_with("link/")),
        "the walk must never descend through a symlinked directory ({by_rel:?})"
    );
    assert_eq!(
        opened,
        vec!["real/in.txt".to_string()],
        "only real in-root files are opened"
    );
    assert_eq!(
        std::fs::read(outside.join("secret.txt")).unwrap(),
        b"outside-secret",
        "the outside tree must be untouched"
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        1,
        "no entry may appear outside the root"
    );
}

/// `list_entries` cap matrix: 0, 1, exact, one under and one over the
/// directory size; `overflowed` must be true exactly when more entries
/// existed than the cap, and the returned prefix must be the sorted head.
#[test]
fn list_entries_cap_matrix_reports_overflow_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();
    for name in ["d", "b", "a", "c"] {
        std::fs::write(root.join(name), b"x").unwrap();
    }
    let rows: [(&str, usize, bool, usize); 5] = [
        ("cap-zero", 0, true, 0),
        ("cap-one", 1, true, 1),
        ("cap-three", 3, true, 3),
        ("cap-four-exact", 4, false, 4),
        ("cap-five-over", 5, false, 4),
    ];
    for (label, cap, overflowed, count) in rows {
        let listing = dir.list_entries(Path::new(""), cap).unwrap();
        assert_eq!(
            listing.overflowed, overflowed,
            "case {label}: overflow flag must reflect cap {cap} against 4 entries"
        );
        assert_eq!(
            listing.entries.len(),
            count,
            "case {label}: the returned prefix length must be min(cap, entries)"
        );
        let names: Vec<String> = listing
            .entries
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "case {label}: the returned prefix must be the sorted head"
        );
    }
}

/// Bounded read cap matrix: at/under/over the file size; only a cap smaller
/// than the file yields a Slice digest, everything else is Full and complete.
#[test]
fn read_cap_matrix_digest_shape_is_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();
    std::fs::write(root.join("f.bin"), b"0123456789").unwrap();
    let rows: [(&str, usize, usize, bool); 6] = [
        ("cap-zero", 0, 0, false),
        ("cap-one", 1, 1, false),
        ("cap-nine", 9, 9, false),
        ("cap-ten-exact", 10, 10, true),
        ("cap-eleven-over", 11, 10, true),
        ("cap-huge", usize::MAX, 10, true),
    ];
    for (label, cap, byte_count, full) in rows {
        let data = dir.read(Path::new("f.bin"), cap).unwrap();
        assert_eq!(
            data.bytes.len(),
            byte_count,
            "case {label}: the returned byte count must be min(cap, size)"
        );
        assert_eq!(
            data.digest.is_full(),
            full,
            "case {label}: digest fullness must report whole-file coverage exactly"
        );
        assert_eq!(
            data.size, byte_count,
            "case {label}: the reported size must match the returned bytes"
        );
    }
    // An empty file is always Full, even at cap zero.
    std::fs::write(root.join("empty.bin"), b"").unwrap();
    let empty = dir.read(Path::new("empty.bin"), 0).unwrap();
    assert!(
        empty.digest.is_full(),
        "an empty file read at cap zero covers the whole (empty) file"
    );
    assert!(empty.bytes.is_empty());
}

/// A walk whose directory holds more entries than the remaining budget
/// aborts with a typed listing-overflow error and never returns a partial
/// listing as complete.
#[test]
fn walk_listing_overflow_is_typed_and_never_partial() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();
    for i in 0..10 {
        std::fs::write(root.join(format!("f{i:02}.txt")), b"x").unwrap();
    }
    // 3 remaining entries but 10 children: the listing itself overflows.
    let mut b = budget(3, 16, 8, 1 << 20, 1 << 20);
    let mut visited = 0usize;
    let err = dir
        .walk_bounded(Path::new(""), &mut b, &[], &mut |_, _, _| {
            visited += 1;
            Ok(WalkStep::Continue)
        })
        .unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Oversized,
        "a listing over the remaining budget must be typed oversized: {err}"
    );
    assert!(
        err.message.contains("remaining") || err.message.contains("listing"),
        "the refusal must name the listing budget: {err}"
    );
    assert_eq!(
        visited, 0,
        "no entry may be visited when the listing itself overflows"
    );
}
