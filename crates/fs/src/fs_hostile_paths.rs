//! Hostile path corpus through the rooted-directory authority (unix).
//!
//! Widens `rooted::tests` with a table-driven corpus that exercises every
//! public [`RootedDir`] entry point for each hostile input. Traversal,
//! absolute/prefix paths and NUL bytes must be refused typed BEFORE any
//! syscall; Windows-shaped names (drive letters, UNC, device paths, ADS,
//! reserved device names, trailing dots/spaces, case variants) are ordinary
//! unix names and must be treated LITERALLY under the root, never
//! re-interpreted. Every case asserts one outcome with its own message, and
//! every case verifies the outside sentinel directory was not touched.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use faktor_core::error::ErrorKind;

use crate::rooted::{RootedDir, WalkBudget};

/// One corpus row: a hostile input and what the authority must do with it.
struct Case {
    label: &'static str,
    path: PathBuf,
    expect: Expect,
}

#[derive(PartialEq, Eq)]
enum Expect {
    /// The path is rejected before any syscall with this kind.
    Reject(ErrorKind),
    /// The path is a legal unix name and must act literally under the root.
    Legal,
    /// The path is refused by the OS (name/path length) with any typed error.
    RejectAny,
}

fn raw(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

fn kind(err: &faktor_core::error::Error) -> ErrorKind {
    err.kind.clone()
}

fn corpus() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut push = |label: &'static str, path: PathBuf, expect: Expect| {
        cases.push(Case {
            label,
            path,
            expect,
        })
    };

    // --- traversal / rooted / NUL: refused before any syscall ------------
    push(
        "traversal-parent",
        raw(b"../escape"),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "traversal-deep",
        raw(b"a/b/../../../etc/passwd"),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "traversal-tail",
        raw(b"a/b/.."),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "traversal-only-dots",
        raw(b".."),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "traversal-empty-segment",
        raw(b"a//../../b"),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "traversal-curdir-then-up",
        raw(b"./a/../b"),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "traversal-dotdot-prefix-name",
        raw(b"..hidden"),
        Expect::Legal,
    );
    push(
        "absolute-etc",
        raw(b"/etc/passwd"),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "absolute-double-slash",
        raw(b"//server/share"),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "absolute-root-only",
        raw(b"/"),
        Expect::Reject(ErrorKind::Permission),
    );
    push(
        "nul-middle",
        raw(b"a\0b"),
        Expect::Reject(ErrorKind::Malformed),
    );
    push(
        "nul-final",
        raw(b"name\0"),
        Expect::Reject(ErrorKind::Malformed),
    );
    push("nul-only", raw(b"\0"), Expect::Reject(ErrorKind::Malformed));
    push(
        "component-overflow",
        PathBuf::from(vec!["a"; 4098].join("/")),
        Expect::Reject(ErrorKind::Oversized),
    );

    // --- Windows-shaped names on unix: literal, contained -----------------
    push("drive-relative", raw(b"C:foo"), Expect::Legal);
    push(
        "drive-absolute",
        raw(b"C:\\Windows\\System32"),
        Expect::Legal,
    );
    push("backslash-name", raw(b"a\\b\\c"), Expect::Legal);
    push("unc-share", raw(b"\\\\server\\share\\file"), Expect::Legal);
    push("verbatim-device", raw(b"\\\\?\\C:\\x"), Expect::Legal);
    push(
        "device-namespace",
        raw(b"\\\\.\\PhysicalDrive0"),
        Expect::Legal,
    );
    push(
        "globalroot",
        raw(b"\\\\?\\GLOBALROOT\\Device"),
        Expect::Legal,
    );
    push("reserved-con", raw(b"CON"), Expect::Legal);
    push("reserved-nul", raw(b"NUL"), Expect::Legal);
    push("reserved-aux", raw(b"aux.txt"), Expect::Legal);
    push("reserved-com1", raw(b"COM1"), Expect::Legal);
    push("reserved-lpt1", raw(b"LPT1.log"), Expect::Legal);
    push("ads-stream", raw(b"file.txt:stream"), Expect::Legal);
    push("ads-default-stream", raw(b"file.txt::$DATA"), Expect::Legal);
    push("trailing-dot", raw(b"name."), Expect::Legal);
    push("trailing-space", raw(b"name "), Expect::Legal);
    push("trailing-dots-spaces", raw(b"name. . . "), Expect::Legal);
    push("case-upper", raw(b"README"), Expect::Legal);

    // --- unicode separators and normalization forms ------------------------
    push("u2028-separator", "\u{2028}a".into(), Expect::Legal);
    push("u2215-division-slash", "a\u{2215}b".into(), Expect::Legal);
    push("uff0f-fullwidth-slash", "a\u{ff0f}b".into(), Expect::Legal);
    push("nfc-precomposed", "caf\u{e9}".into(), Expect::Legal);
    push("nfd-decomposed", "cafe\u{301}".into(), Expect::Legal);
    push("rtl-override", "\u{202e}evil.txt".into(), Expect::Legal);
    push("bidi-isolate", "a\u{2066}b\u{2069}".into(), Expect::Legal);
    push("zero-width-joiner", "a\u{200d}b".into(), Expect::Legal);

    // --- control characters, raw bytes, shell metacharacters ---------------
    push("newline-name", raw(b"line\nbreak"), Expect::Legal);
    push("tab-name", raw(b"tab\there"), Expect::Legal);
    push("cr-name", raw(b"cr\rhere"), Expect::Legal);
    push("esc-name", raw(b"\x1b[31mred"), Expect::Legal);
    push("invalid-utf8", raw(b"\xff\xfe-note"), Expect::Legal);
    push("dash-rf", raw(b"-rf"), Expect::Legal);
    push("glob-star", raw(b"*"), Expect::Legal);
    push("glob-question", raw(b"?"), Expect::Legal);
    push("pipe-name", raw(b"a|b"), Expect::Legal);
    push("redirect-name", raw(b"a>b<c"), Expect::Legal);
    push("dollar-name", raw(b"$HOME"), Expect::Legal);
    push("percent-name", raw(b"%TEMP%"), Expect::Legal);
    push("tilde-name", raw(b"~root"), Expect::Legal);
    push("brace-name", raw(b"{a,b}"), Expect::Legal);

    // --- length boundaries --------------------------------------------------
    push(
        "name-255-bytes",
        PathBuf::from("a".repeat(255)),
        Expect::Legal,
    );
    push(
        "name-256-bytes",
        PathBuf::from("b".repeat(256)),
        Expect::RejectAny,
    );
    push(
        "name-4095-bytes",
        PathBuf::from("c".repeat(4095)),
        Expect::RejectAny,
    );

    // --- dot-segment normalization that must be accepted ---------------------
    push("curdir-mixed", raw(b"a/./b"), Expect::Legal);
    push("doubled-slash", raw(b"a//b"), Expect::Legal);
    push("leading-curdir", raw(b"./a"), Expect::Legal);

    cases
}

fn run_reject_case(dir: &RootedDir, outside: &Path, case: &Case, want_kind: Option<ErrorKind>) {
    let path = case.path.as_path();
    let assert_refused = |result: Result<(), faktor_core::error::Error>, op: &str| match &want_kind
    {
        Some(want) => {
            let err = match result {
                Err(err) => err,
                Ok(()) => panic!(
                    "case {:?}: {op} must be refused (want {:?})",
                    case.label, want
                ),
            };
            assert_eq!(
                kind(&err),
                want.clone(),
                "case {:?}: {op} refused with the wrong kind: {err}",
                case.label
            );
        }
        None => match result {
            Err(_) => {}
            Ok(()) => panic!(
                "case {:?}: {op} must be refused with a typed error",
                case.label
            ),
        },
    };
    assert_refused(dir.create_dir_all(path), "create_dir_all");
    assert_refused(dir.open_create_new(path).map(|_| ()), "open_create_new");
    assert_refused(dir.read(path, 64).map(|_| ()), "read");
    assert_refused(dir.read_link(path).map(|_| ()), "read_link");
    assert_refused(dir.entry_meta(path).map(|_| ()), "entry_meta");
    assert_refused(dir.list_entries(path, 8).map(|_| ()), "list_entries");
    assert_refused(dir.remove_file(path), "remove_file");
    assert_refused(dir.remove_tree(path), "remove_tree");
    assert_refused(
        dir.create_symlink(Path::new("target"), path),
        "create_symlink",
    );
    // The publish path checks same-directory BEFORE walking the parent, so
    // an over-long component count is reported as the (equally typed)
    // same-directory malformed refusal there; any typed refusal is correct.
    let assert_any = |result: Result<(), faktor_core::error::Error>, op: &str| {
        if let Ok(()) = result {
            panic!(
                "case {:?}: {op} must be refused with a typed error",
                case.label
            );
        }
    };
    assert_any(
        dir.atomic_publish(path, Path::new("dest")),
        "atomic_publish-source",
    );
    assert_any(
        dir.atomic_publish(Path::new("tmp"), path),
        "atomic_publish-dest",
    );
    assert_refused(dir.sync_dir(path), "sync_dir");
    assert_refused(dir.restrict_owner_only(path), "restrict_owner_only");
    let mut budget = WalkBudget::new(64, 64, 8, 1 << 20, 1 << 20);
    assert_refused(
        dir.walk_bounded(path, &mut budget, &[], &mut |_, _, _| {
            Ok(crate::rooted::WalkStep::Continue)
        }),
        "walk_bounded",
    );
    assert!(
        !dir.exists(path),
        "case {:?}: exists() must be false for a refused path",
        case.label
    );
    // The outside sentinel must survive every refusal untransformed.
    assert_eq!(
        std::fs::read(outside.join("marker")).unwrap(),
        b"outside-marker",
        "case {:?}: outside sentinel was modified",
        case.label
    );
    assert_eq!(
        std::fs::read_dir(outside).unwrap().count(),
        1,
        "case {:?}: an entry appeared outside the root",
        case.label
    );
}

fn run_legal_case(dir: &RootedDir, outside: &Path, index: usize, case: &Case) {
    let area = PathBuf::from(format!("c{index}"));
    dir.create_dir_all(&area)
        .unwrap_or_else(|e| panic!("case {:?}: area creation failed: {e}", case.label));
    let target = area.join(&case.path);
    let parent = target.parent().unwrap_or_else(|| Path::new(""));
    if !parent.as_os_str().is_empty() {
        dir.create_dir_all(parent).unwrap_or_else(|e| {
            panic!(
                "case {:?}: parent {} creation failed: {e}",
                case.label,
                parent.display()
            )
        });
    }
    // A weird-but-legal name must be creatable literally under the root.
    let mut file = dir.open_create_new(&target).unwrap_or_else(|e| {
        panic!(
            "case {:?}: legal name must be creatable literally: {e}",
            case.label
        )
    });
    use std::io::Write as _;
    file.write_all(b"payload").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let read = dir.read(&target, 64).unwrap_or_else(|e| {
        panic!(
            "case {:?}: legal name must be readable back: {e}",
            case.label
        )
    });
    assert_eq!(
        read.bytes, b"payload",
        "case {:?}: read bytes must round-trip literally",
        case.label
    );
    assert!(
        dir.entry_meta(&target).unwrap().is_some(),
        "case {:?}: entry_meta must see the literal entry",
        case.label
    );
    let final_name = target.file_name().unwrap().to_os_string();
    let listing = dir
        .list_entries(parent, 512)
        .unwrap_or_else(|e| panic!("case {:?}: enumeration failed: {e}", case.label));
    assert!(
        listing.entries.iter().any(|e| e.name == final_name),
        "case {:?}: enumeration must report the literal name {:?}",
        case.label,
        final_name
    );
    dir.remove_file(&target)
        .unwrap_or_else(|e| panic!("case {:?}: removal failed: {e}", case.label));
    assert!(
        !dir.exists(&target),
        "case {:?}: removal must be effective",
        case.label
    );
    assert_eq!(
        std::fs::read(outside.join("marker")).unwrap(),
        b"outside-marker",
        "case {:?}: outside sentinel was modified by a legal case",
        case.label
    );
    assert_eq!(
        std::fs::read_dir(outside).unwrap().count(),
        1,
        "case {:?}: a legal name leaked outside the root",
        case.label
    );
}

/// Table-driven corpus: every row asserts each public entry point's outcome
/// with a message naming the row and the operation, and every row proves the
/// outside sentinel was untouched.
#[test]
fn rooted_path_corpus_is_refused_or_contained_literally() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("marker"), b"outside-marker").unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();

    let cases = corpus();
    assert!(
        cases.len() >= 50,
        "the hostile path corpus must keep at least 50 rows, found {}",
        cases.len()
    );
    for (index, case) in cases.iter().enumerate() {
        match &case.expect {
            Expect::Reject(kind) => run_reject_case(&dir, &outside, case, Some(kind.clone())),
            Expect::RejectAny => run_reject_case(&dir, &outside, case, None),
            Expect::Legal => run_legal_case(&dir, &outside, index, case),
        }
    }
    // The sentinel itself must still be exactly one file at the end.
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        1,
        "the corpus must never touch the outside directory"
    );
}

/// Case-fold handling: unix is byte-exact, so `README` and `readme` are two
/// distinct entries (no implicit case folding, no collision).
#[test]
fn case_variants_are_distinct_entries_and_never_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = RootedDir::create(&tmp.path().join("root")).unwrap();
    use std::io::Write as _;
    for (label, name) in [("upper", "README"), ("lower", "readme")] {
        let mut f = dir
            .open_create_new(Path::new(name))
            .unwrap_or_else(|e| panic!("{label}: create failed: {e}"));
        f.write_all(label.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
    let listing = dir.list_entries(Path::new(""), 16).unwrap();
    assert_eq!(
        listing.entries.len(),
        2,
        "case folding must not collapse README/readme on unix"
    );
    assert_eq!(dir.read(Path::new("README"), 16).unwrap().bytes, b"upper");
    assert_eq!(dir.read(Path::new("readme"), 16).unwrap().bytes, b"lower");
}

/// NFC/NFD are distinct byte sequences for the unix authority (Linux
/// filesystems are byte-based): both must be creatable and readable, and the
/// enumeration must not silently fold one into the other.
#[test]
#[cfg(target_os = "linux")]
fn nfc_and_nfd_names_are_distinct_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = RootedDir::create(&tmp.path().join("root")).unwrap();
    use std::io::Write as _;
    for (label, name) in [("nfc", "caf\u{e9}"), ("nfd", "cafe\u{301}")] {
        let mut f = dir
            .open_create_new(Path::new(name))
            .unwrap_or_else(|e| panic!("{label}: create failed: {e}"));
        f.write_all(label.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
    assert_eq!(
        dir.read(Path::new("caf\u{e9}"), 16).unwrap().bytes,
        b"nfc",
        "the precomposed form must read its own bytes"
    );
    assert_eq!(
        dir.read(Path::new("cafe\u{301}"), 16).unwrap().bytes,
        b"nfd",
        "the decomposed form must read its own bytes"
    );
    let listing = dir.list_entries(Path::new(""), 16).unwrap();
    assert_eq!(
        listing.entries.len(),
        2,
        "NFC and NFD must be two distinct entries on a byte-based filesystem"
    );
}

/// Windows reserved device names are plain unix file names: creating `CON`
/// must not consult any device namespace and must land under the root.
#[test]
fn windows_reserved_device_names_are_ordinary_unix_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();
    use std::io::Write as _;
    for name in ["CON", "PRN", "AUX", "NUL", "COM1", "LPT1"] {
        let mut f = dir
            .open_create_new(Path::new(name))
            .unwrap_or_else(|e| panic!("{name}: reserved name must be a legal unix file: {e}"));
        f.write_all(name.as_bytes()).unwrap();
        f.sync_all().unwrap();
        assert_eq!(
            dir.read(Path::new(name), 32).unwrap().bytes,
            name.as_bytes(),
            "{name}: literal content must round-trip"
        );
        assert!(
            root.join(name).is_file(),
            "{name}: the entry must live under the root"
        );
    }
}

/// The rooted root itself can never be addressed by a relative path that
/// still names it: the empty path addresses the root, while `.` normalizes
/// to it, and `..` is refused even as the only component.
#[test]
fn root_addressing_is_explicit_and_traversal_is_never_normalized() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();
    assert!(dir.exists(Path::new("")), "empty path addresses the root");
    assert!(dir.exists(Path::new(".")), "curdir addresses the root");
    assert!(dir.create_dir_all(Path::new(".")).is_ok());
    let err = dir.entry_meta(Path::new("")).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Malformed,
        "empty entry_meta must be refused typed: {err}"
    );
    let err = crate::rooted::split_final(Path::new("..")).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Permission,
        "'..' must be refused before normalization: {err}"
    );
    // A trailing curdir normalizes back onto the directory it names.
    dir.create_dir_all(Path::new("a")).unwrap();
    let meta = dir.entry_meta(Path::new("a/.")).unwrap();
    assert!(
        matches!(meta, Some(m) if m.kind == crate::rooted::RootedEntryKind::Directory),
        "a/. must normalize to the directory a: {meta:?}"
    );
}

/// A path whose joined absolute spelling exceeds PATH_MAX is still reachable
/// through the dirfd-relative authority: names are opened component-by-
/// component, never by re-resolving one long string. The outside sentinel is
/// untouched and the tree is removed through the same authority.
#[test]
#[cfg(target_os = "linux")]
fn path_beyond_path_max_is_handled_component_relative() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("marker"), b"outside-marker").unwrap();
    let root = tmp.path().join("root");
    let dir = RootedDir::create(&root).unwrap();

    let component = "z".repeat(64);
    let depth = 100usize;
    let deep: PathBuf = std::iter::repeat_n(component.as_str(), depth)
        .collect::<Vec<_>>()
        .join("/")
        .into();
    let joined = root.join(&deep);
    assert!(
        joined.to_string_lossy().len() > 4096,
        "fixture must exceed PATH_MAX, got {} chars",
        joined.to_string_lossy().len()
    );
    dir.create_dir_all(&deep)
        .unwrap_or_else(|e| panic!("dirfd-relative creation must not be limited by PATH_MAX: {e}"));
    let file_rel = deep.join("leaf.txt");
    let mut f = dir
        .open_create_new(&file_rel)
        .unwrap_or_else(|e| panic!("deep file creation through the authority must succeed: {e}"));
    use std::io::Write as _;
    f.write_all(b"deep-payload").unwrap();
    f.sync_all().unwrap();
    drop(f);
    assert_eq!(
        dir.read(&file_rel, 64).unwrap().bytes,
        b"deep-payload",
        "deep content must round-trip through the authority"
    );
    assert!(
        std::fs::metadata(&joined).is_err(),
        "the std pathname API cannot reach beyond PATH_MAX; the authority must"
    );
    assert_eq!(
        std::fs::read(outside.join("marker")).unwrap(),
        b"outside-marker",
        "the outside sentinel must be untouched by deep path handling"
    );
    let top = "z".repeat(64);
    dir.remove_tree(Path::new(&top))
        .expect("the authority must remove the deep tree it created");
    assert!(
        dir.list_entries(Path::new(""), 8)
            .unwrap()
            .entries
            .is_empty(),
        "the deep tree must be fully removed"
    );
}

/// Symlink/entry swap races through the existing entry seam: an entry
/// enumerated by `walk_bounded` is swapped before descent. Every swap shape
/// must produce a typed refusal and leave the outside tree byte-identical.
#[test]
fn seam_swaps_of_every_entry_shape_are_refused_typed() {
    #[derive(Debug, Clone, Copy)]
    enum Swap {
        DirToSymlink,
        DirToFile,
        DirToFifo,
        NestedDirToSymlink,
    }
    let shapes = [
        ("dir-to-symlink", Swap::DirToSymlink),
        ("dir-to-file", Swap::DirToFile),
        ("dir-to-fifo", Swap::DirToFifo),
        ("nested-dir-to-symlink", Swap::NestedDirToSymlink),
    ];
    for (label, swap) in shapes {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("marker"), b"outside-marker").unwrap();
        let root = tmp.path().join("root");
        let dir = RootedDir::create(&root).unwrap();
        dir.create_dir_all(Path::new("victim")).unwrap();
        std::fs::write(root.join("victim/keep.txt"), b"inside").unwrap();
        if matches!(swap, Swap::NestedDirToSymlink) {
            dir.create_dir_all(Path::new("victim/child")).unwrap();
            std::fs::write(root.join("victim/child/inner.txt"), b"inner").unwrap();
        }

        let seam_root = root.clone();
        let seam_outside = outside.clone();
        let target: &str = if matches!(swap, Swap::NestedDirToSymlink) {
            "victim/child"
        } else {
            "victim"
        };
        let guard = crate::rooted::ENTRY_SEAM_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::rooted::install_entry_seam(Box::new({
            let target = target.to_string();
            move |rel: &Path| {
                if rel == Path::new(&target) {
                    let absolute = seam_root.join(&target);
                    let _ = std::fs::remove_dir_all(&absolute);
                    match swap {
                        Swap::DirToSymlink => {
                            let _ = std::os::unix::fs::symlink(&seam_outside, &absolute);
                        }
                        Swap::DirToFile => {
                            let _ = std::fs::write(&absolute, b"swapped");
                        }
                        Swap::DirToFifo => {
                            // No unsafe: create the FIFO through the standard
                            // `mkfifo` utility available on every unix CI host.
                            let _ = std::process::Command::new("mkfifo").arg(&absolute).status();
                        }
                        Swap::NestedDirToSymlink => {
                            let _ = std::os::unix::fs::symlink(&seam_outside, &absolute);
                        }
                    }
                }
            }
        }));

        let mut budget = WalkBudget::new(100, 100, 16, 1 << 20, 1 << 20);
        let result = dir.walk_bounded(Path::new(""), &mut budget, &[], &mut |_, _, _| {
            Ok(crate::rooted::WalkStep::Continue)
        });
        crate::rooted::clear_entry_seam();
        drop(guard);

        let err = result.expect_err(&format!("case {label}: swapped entry must be refused"));
        assert_eq!(
            err.kind,
            ErrorKind::Permission,
            "case {label}: the refusal must be a typed permission error: {err}"
        );
        assert_eq!(
            std::fs::read(outside.join("marker")).unwrap(),
            b"outside-marker",
            "case {label}: the outside sentinel must be untouched"
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            1,
            "case {label}: no entry may appear outside the root"
        );
    }
}
