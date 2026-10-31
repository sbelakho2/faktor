// TEST-SEPARATION: oracle=independent-reference isolation=isolated-data-dir
//! Deterministic durable-write failpoints (HAR-04).
//!
//! Every durable-write boundary in this crate — temp fsync, hard-link
//! publish, rename, parent fsync, adopt-fsync, rooted publish — crosses a
//! named [`trip`] point, so a fault campaign can crash the process at EVERY
//! durable boundary (the ledger's "failpoint at every durable write" row)
//! instead of at a hand-picked few. The vocabulary is enumerated in
//! [`KNOWN_POINTS`]; a source-scan drift test in this module fails the build
//! when a new `fs::rename`/`fs::hard_link` bypasses the wrapped helpers or
//! when a trip literal appears that is not registered (or a registered point
//! disappears).
//!
//! Semantics mirror the `faktor-cas` crash seam: one-shot arming at
//! `(point, ordinal)`; the armed crossing panics with a recognizable
//! `[fault-seam]` payload; the seam is INERT unless armed.
//!
//! The armed state is THREAD-LOCAL: a campaign arms the driving thread, and
//! operations that run on that same thread (every primitive in this crate is
//! synchronous) trip it. This keeps parallel test threads completely
//! isolated — an unrelated test can never trip another test's arm — at the
//! honest limit that a campaign driving writes from a spawned worker must
//! arm on the writer's thread (or use the per-instance `faktor-cas` seam for
//! CAS-level campaigns).
//!
//! Production profile: the arming API does not exist (`cfg`-gated out) and
//! [`trip`] compiles to a zero-sized, always-inlined no-op, so the call
//! sites stay uniform at zero runtime cost — exactly like the CAS seam.

/// Every durability-boundary trip point name in this crate. A trip literal
/// in the source that is missing from this list (or a listed point with no
/// trip literal) fails the drift test.
pub const KNOWN_POINTS: &[&str] = &[
    // atomic.rs — shared in-memory staged write boundary.
    "write_and_fsync.fsynced",
    // atomic.rs — caller-staged temp adoption (SQLite backup image).
    "adopt.fsynced",
    // entry_state.rs — canonical entry staged write boundary.
    "entry_stage.fsynced",
    // tree_manifest.rs — tree-copy staged write boundary.
    "manifest_stage.fsynced",
    // lib.rs copy_open_file — file-copy staged write boundary.
    "copy_stage.fsynced",
    // atomic.rs fsync_parent — every directory-entry durability boundary.
    "fsync_parent.done",
    // atomic.rs durable_rename_raw — every rename(2) commit in the crate.
    "rename.done",
    // atomic.rs durable_hard_link_raw — every hard-link commit in the crate.
    "hard_link.done",
    // atomic.rs atomic_publish_at — rooted publish, unix/windows platforms.
    "publish_at.renamed",
    "publish_at.fsynced",
];

#[cfg(any(test, feature = "fault-injection"))]
mod armed {
    use std::cell::RefCell;

    struct Armed {
        point: &'static str,
        ordinal: u64,
        crossings: u64,
    }

    thread_local! {
        static ARMED: RefCell<Option<Armed>> = const { RefCell::new(None) };
    }

    /// Arm ONE crossing on THIS thread, replacing any previous arm and
    /// resetting the crossing counter. The panic fires exactly once when
    /// `point` is crossed for the `ordinal`-th time on this thread.
    pub fn arm(point: &'static str, ordinal: u64) {
        ARMED.with(|cell| {
            *cell.borrow_mut() = Some(Armed {
                point,
                ordinal,
                crossings: 0,
            });
        });
    }

    /// Disarm this thread (a no-op when nothing is armed).
    pub fn disarm() {
        ARMED.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }

    /// Trip the seam at `point`. Panics when the armed crossing is hit.
    pub(super) fn trip(point: &'static str) {
        let hit = ARMED.with(|cell| {
            let mut guard = cell.borrow_mut();
            let Some(armed) = guard.as_mut() else {
                return false;
            };
            if armed.point != point {
                return false;
            }
            armed.crossings += 1;
            if armed.crossings - 1 != armed.ordinal {
                return false;
            }
            *guard = None;
            true
        });
        if hit {
            panic!("[fault-seam] simulated crash at fs durability boundary `{point}`");
        }
    }
}

#[cfg(any(test, feature = "fault-injection"))]
pub use armed::{arm, disarm};

/// Trip the durability-boundary seam at `point` (see [`KNOWN_POINTS`]).
/// Inert unless this thread armed the seam; panics on the armed crossing.
#[cfg(any(test, feature = "fault-injection"))]
#[inline]
pub fn trip(point: &'static str) {
    armed::trip(point);
}

/// Production profile: the seam payload is compiled out; the no-op keeps
/// the surrounding call sites uniform at zero runtime cost.
#[cfg(not(any(test, feature = "fault-injection")))]
#[inline(always)]
pub fn trip(_point: &'static str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::Path;

    /// Disarm on drop so a failing assertion can never leave this thread
    /// armed for later (unrelated) tests.
    struct Disarm;
    impl Drop for Disarm {
        fn drop(&mut self) {
            disarm();
        }
    }

    fn panicked_with(point: &str, op: impl FnOnce()) -> bool {
        let out = catch_unwind(AssertUnwindSafe(op));
        let err = match out {
            Ok(()) => return false,
            Err(e) => e,
        };
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            msg.contains("[fault-seam]") && msg.contains(point),
            "panic was not the armed seam for `{point}`: {msg}"
        );
        true
    }

    #[test]
    fn unarmed_writes_never_panic() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("plain.bin");
        crate::atomic::atomic_replace(&t, b"hello").unwrap();
        assert_eq!(fs::read(&t).unwrap(), b"hello");
    }

    #[test]
    fn replace_trips_tmp_fsync_before_rename_dest_absent_temp_residue() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("f.bin");
        arm("write_and_fsync.fsynced", 0);
        assert!(panicked_with("write_and_fsync.fsynced", || {
            let _ = crate::atomic::atomic_replace(&t, b"payload");
        }));
        // Crash before the rename: the destination must NOT exist, and the
        // staged temp is the documented at-worst residue.
        assert!(!t.exists(), "the destination must be absent pre-rename");
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| crate::atomic::is_faktor_temp(n)),
            "no temp residue after a pre-rename crash: {names:?}"
        );
    }

    #[test]
    fn replace_trips_rename_after_commit_dest_has_new_content() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("f.bin");
        arm("rename.done", 0);
        assert!(panicked_with("rename.done", || {
            let _ = crate::atomic::atomic_replace(&t, b"committed");
        }));
        // The rename crossed: the destination holds the WHOLE new content
        // and the temp was consumed by the rename.
        assert_eq!(fs::read(&t).unwrap(), b"committed");
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["f.bin".to_string()], "temp leaked: {names:?}");
    }

    #[test]
    fn replace_trips_parent_fsync_after_commit() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("f.bin");
        arm("fsync_parent.done", 0);
        assert!(panicked_with("fsync_parent.done", || {
            let _ = crate::atomic::atomic_replace(&t, b"parent-fsync");
        }));
        assert_eq!(fs::read(&t).unwrap(), b"parent-fsync");
    }

    #[test]
    fn create_trips_hard_link_publish_dest_exists_tmp_residue() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("c.bin");
        arm("hard_link.done", 0);
        assert!(panicked_with("hard_link.done", || {
            let _ = crate::atomic::atomic_create(&t, b"first");
        }));
        // The link crossed (first-writer-wins committed); the temp cleanup
        // after the link never ran.
        assert_eq!(fs::read(&t).unwrap(), b"first");
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| crate::atomic::is_faktor_temp(n)),
            "no temp residue after a post-link crash: {names:?}"
        );
    }

    #[test]
    fn adopt_trips_fsync_before_rename_dest_unchanged_tmp_residue() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("pub.db");
        fs::write(&dest, b"previous").unwrap();
        let tmp = dir.path().join("pub.db.tmp-campaign");
        fs::write(&tmp, b"next-image").unwrap();
        arm("adopt.fsynced", 0);
        assert!(panicked_with("adopt.fsynced", || {
            let _ = crate::atomic::atomic_adopt(&tmp, &dest);
        }));
        assert_eq!(fs::read(&dest).unwrap(), b"previous");
        assert!(
            tmp.exists(),
            "the caller's staged temp survives an adopt crash"
        );
    }

    #[test]
    fn copy_trips_stage_fsync_target_absent_temp_residue() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        fs::write(&src, b"copy-source").unwrap();
        let f = fs::File::open(&src).unwrap();
        let target = dir.path().join("out").join("dst.bin");
        arm("copy_stage.fsynced", 0);
        assert!(panicked_with("copy_stage.fsynced", || {
            let _ = crate::copy_open_file(&f, &target, None);
        }));
        assert!(
            !target.exists(),
            "the destination must be absent pre-rename"
        );
        let names: Vec<String> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| crate::atomic::is_faktor_temp(n)),
            "no temp residue after a copy-stage crash: {names:?}"
        );
    }

    #[test]
    fn entry_trips_stage_fsync_entry_absent_temp_residue() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let rel = Path::new("e.txt");
        let next = crate::entry_state::EntryState::regular(
            crate::tree_manifest::CanonicalMode::RegularFile,
            faktor_core::hash::FileHash::from(blake3::hash(b"entry").into()),
        )
        .unwrap();
        arm("entry_stage.fsynced", 0);
        assert!(panicked_with("entry_stage.fsynced", || {
            let _ = crate::entry_state::apply_tree_entry_cas(
                root,
                rel,
                &crate::entry_state::EntryState::Absent,
                &next,
                b"entry",
            );
        }));
        let live = crate::entry_state::state_at(root, rel).unwrap();
        assert_eq!(live, crate::entry_state::EntryState::Absent);
        let names: Vec<String> = fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| crate::atomic::is_faktor_temp(n)),
            "no temp residue after an entry-stage crash: {names:?}"
        );
    }

    #[test]
    fn manifest_trips_stage_fsync_destination_absent() {
        let _g = Disarm;
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("m.txt"), b"tree-file").unwrap();
        arm("manifest_stage.fsynced", 0);
        assert!(panicked_with("manifest_stage.fsynced", || {
            let _ = crate::tree_manifest::copy_tree_manifest(
                src_dir.path(),
                dst_dir.path(),
                16,
                1 << 20,
                &[],
            );
        }));
        assert!(!dst_dir.path().join("m.txt").exists());
        let names: Vec<String> = fs::read_dir(dst_dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| crate::atomic::is_faktor_temp(n)),
            "no temp residue after a manifest-stage crash: {names:?}"
        );
    }

    #[test]
    fn publish_trips_rename_and_fsync_boundaries() {
        let _g = Disarm;
        // publish_at.renamed: the rename committed, the destination exists
        // whole, the staged temp is consumed.
        let dir = tempfile::tempdir().unwrap();
        let rooted = crate::rooted::RootedDir::open(dir.path()).unwrap();
        fs::write(dir.path().join("staged.tmp"), b"published").unwrap();
        arm("publish_at.renamed", 0);
        assert!(panicked_with("publish_at.renamed", || {
            let _ = crate::atomic::atomic_publish_at(
                &rooted,
                Path::new("staged.tmp"),
                Path::new("final.txt"),
            );
        }));
        assert_eq!(
            fs::read(dir.path().join("final.txt")).unwrap(),
            b"published"
        );
        assert!(!dir.path().join("staged.tmp").exists());

        // publish_at.fsynced: parent-fsync boundary after the same commit.
        let dir2 = tempfile::tempdir().unwrap();
        let rooted2 = crate::rooted::RootedDir::open(dir2.path()).unwrap();
        fs::write(dir2.path().join("staged.tmp"), b"published-two").unwrap();
        arm("publish_at.fsynced", 0);
        assert!(panicked_with("publish_at.fsynced", || {
            let _ = crate::atomic::atomic_publish_at(
                &rooted2,
                Path::new("staged.tmp"),
                Path::new("final.txt"),
            );
        }));
        assert_eq!(
            fs::read(dir2.path().join("final.txt")).unwrap(),
            b"published-two"
        );
    }

    #[test]
    fn ordinal_arms_the_second_crossing() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        arm("rename.done", 1);
        crate::atomic::atomic_replace(&a, b"first").unwrap();
        assert!(panicked_with("rename.done", || {
            let _ = crate::atomic::atomic_replace(&b, b"second");
        }));
        assert_eq!(fs::read(&a).unwrap(), b"first");
        // The trip fires AFTER the rename syscall commits, so the second
        // destination holds the whole new content when the crash lands.
        assert_eq!(fs::read(&b).unwrap(), b"second");
    }

    #[test]
    fn disarm_makes_an_armed_point_inert() {
        let _g = Disarm;
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("f.bin");
        arm("rename.done", 0);
        disarm();
        crate::atomic::atomic_replace(&t, b"inert").unwrap();
        assert_eq!(fs::read(&t).unwrap(), b"inert");
    }

    #[test]
    fn drift_every_trip_literal_is_registered_and_present() {
        // (file, required trip literals) — every durability boundary in the
        // file must cross its registered point.
        let required: &[(&str, &[&str])] = &[
            (
                "src/atomic.rs",
                &[
                    "write_and_fsync.fsynced",
                    "adopt.fsynced",
                    "fsync_parent.done",
                    "rename.done",
                    "hard_link.done",
                    "publish_at.renamed",
                    "publish_at.fsynced",
                ],
            ),
            ("src/entry_state.rs", &["entry_stage.fsynced"]),
            ("src/tree_manifest.rs", &["manifest_stage.fsynced"]),
            ("src/lib.rs", &["copy_stage.fsynced"]),
        ];
        let mut seen: Vec<String> = Vec::new();
        let sources: &[(&str, &str)] = &[
            ("src/atomic.rs", include_str!("../src/atomic.rs")),
            ("src/entry_state.rs", include_str!("../src/entry_state.rs")),
            (
                "src/tree_manifest.rs",
                include_str!("../src/tree_manifest.rs"),
            ),
            ("src/lib.rs", include_str!("../src/lib.rs")),
        ];
        for (file, src) in sources {
            for point in extract_trips(src) {
                seen.push(format!("{file}::{point}"));
            }
        }
        // Every trip literal in the sources is a registered point.
        for entry in &seen {
            let point = entry.split("::").nth(1).expect("entry shape");
            assert!(
                KNOWN_POINTS.contains(&point),
                "unregistered trip point `{point}` — add it to KNOWN_POINTS"
            );
        }
        // Every registered point has at least one trip literal.
        for point in KNOWN_POINTS {
            assert!(
                seen.iter().any(|e| e.ends_with(&format!("::{point}"))),
                "registered point `{point}` has no trip call site"
            );
        }
        // Every required (file, point) pair is present.
        for (file, points) in required {
            let src = sources
                .iter()
                .find(|(f, _)| f == file)
                .map(|(_, s)| *s)
                .unwrap_or_else(|| panic!("missing source {file}"));
            let trips = extract_trips(src);
            for point in *points {
                assert!(
                    trips.contains(&point.to_string()),
                    "{file} lost its trip at `{point}`"
                );
            }
        }
    }

    #[test]
    fn drift_raw_renames_and_links_only_live_in_the_wrapped_helpers() {
        // Non-test prefix of each production file (up to the LAST
        // `#[cfg(test)]`): every rename/hard-link syscall must live inside
        // the wrapped helper in atomic.rs — a new raw call site that skips
        // the helper would skip its failpoint, which is exactly the HAR-04
        // coverage regression this scan forbids.
        let files: &[(&str, &str, usize, usize)] = &[
            // (path, source, expected raw rename count, expected raw link count)
            ("src/atomic.rs", include_str!("../src/atomic.rs"), 1, 1),
            (
                "src/entry_state.rs",
                include_str!("../src/entry_state.rs"),
                0,
                0,
            ),
            (
                "src/tree_manifest.rs",
                include_str!("../src/tree_manifest.rs"),
                0,
                0,
            ),
            ("src/lib.rs", include_str!("../src/lib.rs"), 0, 0),
            ("src/rooted.rs", include_str!("../src/rooted.rs"), 0, 0),
            (
                "src/platform/unix.rs",
                include_str!("../src/platform/unix.rs"),
                0,
                0,
            ),
            (
                "src/platform/windows.rs",
                include_str!("../src/platform/windows.rs"),
                0,
                0,
            ),
        ];
        for (path, src, want_renames, want_links) in files {
            let prefix = match src.rfind("#[cfg(test)]") {
                Some(idx) => &src[..idx],
                None => src,
            };
            let renames =
                count_calls(prefix, "fs::rename(") + count_calls(prefix, "std::fs::rename(");
            let links =
                count_calls(prefix, "fs::hard_link(") + count_calls(prefix, "std::fs::hard_link(");
            assert_eq!(
                renames, *want_renames,
                "{path}: raw rename sites changed ({renames}, expected {want_renames}) — \
                 route every rename through durable_rename_raw so its failpoint fires"
            );
            assert_eq!(
                links, *want_links,
                "{path}: raw hard-link sites changed ({links}, expected {want_links}) — \
                 route every hard link through durable_hard_link_raw so its failpoint fires"
            );
        }
    }

    fn extract_trips(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = src;
        while let Some(idx) = rest.find("trip(\"") {
            let after = &rest[idx + "trip(\"".len()..];
            if let Some(end) = after.find('"') {
                out.push(after[..end].to_string());
            }
            rest = after;
        }
        out
    }

    fn count_calls(src: &str, needle: &str) -> usize {
        src.matches(needle).count()
    }
}
