// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
use super::*;
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::fs::symlink;

/// Wall deadline for watcher registration in tests. The production
/// default (10s) is a *pathological-root* guard: under full-suite
/// contention (8 concurrent fs test binaries) FSEvents registration
/// legitimately exceeded it, `open` degraded to watcher-less, and both
/// watcher tests failed on a loaded host. Tests that assert
/// attachment/delivery therefore give registration a generous deadline —
/// the assertion keeps its teeth (a root that truly cannot register still
/// fails loudly, and the event wait below has its own bound), while the
/// host scheduler can no longer turn a successful registration into a
/// false negative.

#[test]
fn ephemeral_candidate_roots_open_alongside_a_durable_workspace() {
    let _guard = lock_watch_tests();
    let live = tempfile::tempdir().unwrap();
    let candidate = tempfile::tempdir().unwrap();
    let service = WorkspaceFileService::default();
    let live_handle = service
        .open(WorkspaceId::new(1), live.path().to_path_buf())
        .unwrap();
    // A durable id may be open at its real root AND at a live shadow
    // root simultaneously (the shadowed drive reads the world it
    // mutates); each (id, root) pair is idempotent.
    let shadow_handle = service
        .open(WorkspaceId::new(1), candidate.path().to_path_buf())
        .unwrap();
    assert_eq!(shadow_handle.workspace_id(), live_handle.workspace_id());
    assert_ne!(shadow_handle.root(), live_handle.root());
    assert_eq!(service.open_count(), 2);
    let again = service
        .open(WorkspaceId::new(1), candidate.path().to_path_buf())
        .unwrap();
    assert_eq!(again.root(), shadow_handle.root());
    assert_eq!(service.open_count(), 2, "same (id, root) is idempotent");
    let ws = service
        .open_ephemeral(candidate.path().to_path_buf())
        .unwrap();
    assert!(ws.workspace_id().raw() >= crate::workspace_service::EPHEMERAL_WORKSPACE_ID_BASE);
    assert_ne!(ws.workspace_id(), live_handle.workspace_id());
    assert_eq!(ws.root(), candidate.path().canonicalize().unwrap());
    // The ephemeral handle is NOT cached: the durable map is untouched.
    assert_eq!(service.open_count(), 2);
}
const TEST_WATCH_REGISTRATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(240);

/// Serializes every test that depends on the PROCESS-WIDE watcher
/// worker state (spawn count, wedge seams, overdue overrides).
static WATCH_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_watch_tests() -> std::sync::MutexGuard<'static, ()> {
    WATCH_TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture_unlocked() -> (
    tempfile::TempDir,
    Arc<WorkspaceFileService>,
    WorkspaceHandle,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    fs::create_dir_all(&root).unwrap();
    let service =
        WorkspaceFileService::with_watch_registration_deadline(TEST_WATCH_REGISTRATION_DEADLINE);
    let handle = service.open(WorkspaceId::new(1), root.clone()).unwrap();
    (dir, service, handle)
}

/// The default fixture. Watcher registration is process-global (one
/// bounded singleton worker, a spawn cap, and the wedge/overdue test
/// seams), so the `open` here is serialized against every other
/// watcher-state test; the lock spans only the registration, never an
/// event wait. Tests that already hold [`WATCH_TEST_SERIAL`] use
/// [`fixture_unlocked`] to avoid re-entering it.
fn fixture() -> (
    tempfile::TempDir,
    Arc<WorkspaceFileService>,
    WorkspaceHandle,
) {
    let _guard = lock_watch_tests();
    fixture_unlocked()
}

#[test]
fn filesystem_root_opens_degraded_instead_of_walking_the_world() {
    // Fault case that wedged the trusted linux lane: a session whose
    // workspace defaults to `/` used to start an unbounded recursive
    // inotify walk and never answer the attach. The root must open
    // immediately, loudly watcher-less.
    let service = WorkspaceFileService::new();
    let _guard = lock_watch_tests();
    let handle = service
        .open(WorkspaceId::new(9), PathBuf::from("/"))
        .expect("the filesystem root opens in degraded mode");
    assert!(
        !handle.watcher_attached(),
        "the filesystem root must never get a recursive watcher"
    );
    service.close(WorkspaceId::new(9));
}

#[test]
fn ordinary_workspace_keeps_its_watcher() {
    let (_d, _service, handle) = fixture();
    assert!(
        handle.watcher_attached(),
        "a normal workspace root must register its watcher"
    );
}

#[test]
fn watcher_registration_uses_exactly_one_worker_thread() {
    let _guard = lock_watch_tests();
    // Warm the singleton first: the COUNTED window starts once a worker
    // exists, so what is asserted below is REUSE — a fresh process is
    // allowed exactly one first spawn, and no later registration may
    // grow the thread count.
    let (_warm_dir, _warm_service, warm) = fixture_unlocked();
    assert!(warm.watcher_attached());
    let workers_before = crate::workspace_service::watch_registration_worker_spawns();
    // P2-FS: every registration (including deadline-degraded ones) funnels
    // through one bounded singleton worker, so live registration threads
    // cannot grow per root.
    let (_d, _service, handle) = fixture_unlocked();
    assert!(handle.watcher_attached());
    let mut handles = Vec::new();
    let mut roots = Vec::new();
    for index in 0..4u64 {
        let dir = tempfile::tempdir().unwrap();
        let service = WorkspaceFileService::new();
        handles.push(
            service
                .open(WorkspaceId::new(100 + index), dir.path().to_path_buf())
                .unwrap(),
        );
        roots.push(dir);
    }
    assert!(handles.iter().all(|handle| handle.watcher_attached()));
    assert_eq!(
        crate::workspace_service::watch_registration_worker_spawns(),
        workers_before,
        "watch registration must never spawn an additional worker"
    );
    drop(handles);
    drop(roots);
}

#[test]
fn bounded_join_degrades_once_the_deadline_elapses() {
    let started = std::time::Instant::now();
    let slow = bounded_join(
        "faktor-test-slow-registration",
        std::time::Duration::from_millis(40),
        || {
            std::thread::sleep(std::time::Duration::from_millis(600));
            7u8
        },
    );
    assert!(slow.is_none(), "a slow producer must time out, not block");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "the wait must end at the deadline, not at the producer"
    );
    let fast = bounded_join(
        "faktor-test-fast-registration",
        std::time::Duration::from_secs(5),
        || 9u8,
    );
    assert_eq!(fast, Some(9));
}

#[test]
fn poisoned_workspace_registry_is_recovered_not_propagated() {
    let (_d, service, _handle) = fixture();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = service.workspaces.lock().unwrap();
        panic!("holder poisoned the workspace registry");
    }));
    assert!(service.workspaces.is_poisoned());
    // Derived cache => recover (poison cleared, reconciled against the
    // durable workspace rows): later open/close/open_count never panic.
    let dir = tempfile::tempdir().unwrap();
    let extra = dir.path().join("extra");
    fs::create_dir_all(&extra).unwrap();
    {
        let _guard = lock_watch_tests();
        service.open(WorkspaceId::new(2), extra.clone()).unwrap();
    }
    assert_eq!(service.open_count(), 2);
    service.close(WorkspaceId::new(2));
    assert_eq!(service.open_count(), 1);
    assert!(!service.workspaces.is_poisoned());
}

#[test]
fn traversal_escape_rejected() {
    let (_d, _s, h) = fixture();
    for evil in ["../x", "a/../../b", "/etc/passwd", "..", "a/.."] {
        let r = h.resolve(Path::new(evil));
        assert!(r.is_err(), "{evil} must be rejected");
    }
    assert!(h.resolve(Path::new("ok.txt")).is_ok());
}

#[cfg(unix)]
#[test]
fn symlink_escape_rejected() {
    let (_d, _s, h) = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "s").unwrap();
    symlink(outside.path(), h.root().join("link")).unwrap();
    // Both the escape THROUGH the link and the link itself are typed
    // permission refusals, and the outside bytes are never addressed.
    for evil in [Path::new("link/secret"), Path::new("link")] {
        let err = h.resolve(evil).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission, "{evil:?}: {err:?}");
    }
    // Inside symlink is fine.
    fs::write(h.root().join("real"), "x").unwrap();
    symlink(h.root().join("real"), h.root().join("alias")).unwrap();
    let resolved = h.resolve(Path::new("alias")).unwrap();
    assert!(
        resolved.ends_with("real"),
        "an inside symlink resolves to its target: {resolved:?}"
    );
    assert!(
        resolved.starts_with(h.root()),
        "resolution must stay inside the workspace root"
    );
}

#[cfg(unix)]
#[test]
fn symlink_hop_chain_is_bounded_at_eight_and_reads_nothing_past_it() {
    // Fails if the walk forgets to charge hops: a 9-link chain would be
    // followed (an unbounded loop risk) instead of a typed refusal.
    let (_d, _s, h) = fixture();
    fs::write(h.root().join("real"), b"secret").unwrap();
    symlink("real", h.root().join("l0")).unwrap();
    for i in 1..9 {
        symlink(format!("l{}", i - 1), h.root().join(format!("l{i}"))).unwrap();
    }
    assert_eq!(h.read(Path::new("l7"), 64).unwrap().bytes, b"secret");
    let err = h.read(Path::new("l8"), 64).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    assert!(
        err.message.contains("8-hop bound"),
        "the refusal names the bound: {err:?}"
    );
}

/// P1-C: the delete is rooted, not resolve-then-`std::fs::remove_file`.
/// The old two-step (`resolve` to an absolute path, then unlink that
/// path string) followed a parent directory swapped for an outside
/// symlink AFTER the resolve and deleted the outside file. The rooted
/// authority walks the parent from the fd anchored at open time, so the
/// swap is refused and the outside marker survives.
#[cfg(unix)]
#[test]
fn remove_file_is_rooted_and_refuses_a_parent_swapped_after_resolve() {
    let (_d, _s, h) = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("precious"), b"outside-marker").unwrap();
    fs::create_dir_all(h.root().join("d")).unwrap();
    fs::write(h.root().join("d/f"), b"inside").unwrap();
    // The old dangerous sequence: resolve FIRST (the path string is now
    // the only authority), THEN swap the parent for an outside symlink.
    let _resolved = h.resolve(Path::new("d/f")).unwrap();
    fs::remove_dir_all(h.root().join("d")).unwrap();
    symlink(outside.path(), h.root().join("d")).unwrap();
    let err = h.remove_file(Path::new("d/f")).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Permission,
        "the anchored walk must refuse the swapped parent: {err:?}"
    );
    assert_eq!(
        fs::read(outside.path().join("precious")).unwrap(),
        b"outside-marker",
        "the outside file must never be deleted"
    );
    // In-workspace delete works, and a missing entry is idempotent.
    fs::write(h.root().join("ok.txt"), b"x").unwrap();
    h.remove_file(Path::new("ok.txt")).unwrap();
    assert!(!h.root().join("ok.txt").exists());
    h.remove_file(Path::new("ok.txt")).unwrap();
    // Traversal is refused before any syscall; the outside tree is
    // still untouched.
    assert!(h.remove_file(Path::new("../precious")).is_err());
    assert_eq!(
        fs::read(outside.path().join("precious")).unwrap(),
        b"outside-marker"
    );
}

/// P1-C: the handle anchors its rooted authority ONCE at open. Renaming
/// the root path away and replacing it with an outside symlink does not
/// redirect the retained authority's operations — they keep addressing
/// the original directory.
#[cfg(unix)]
#[test]
fn retained_root_authority_is_immune_to_a_root_path_swap() {
    let (dir, _s, h) = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("marker"), b"keep").unwrap();
    fs::write(h.root().join("f.txt"), b"one").unwrap();
    let moved = dir.path().join("ws-moved");
    fs::rename(h.root(), &moved).unwrap();
    symlink(outside.path(), h.root()).unwrap();
    // Deletes and walks still address the original directory through the
    // retained fd; the symlinked root path is irrelevant.
    h.remove_file(Path::new("f.txt")).unwrap();
    assert!(
        !moved.join("f.txt").exists(),
        "the anchored root was addressed"
    );
    assert!(outside.path().join("marker").exists(), "outside untouched");
    fs::write(moved.join("g.txt"), b"two").unwrap();
    let mut budget = crate::rooted::WalkBudget::new(16, 16, 8, 1 << 20, 1 << 20);
    let mut seen = Vec::new();
    h.walk_bounded(Path::new(""), &mut budget, &[], &mut |entry, _depth, _b| {
        seen.push(entry.rel.display().to_string());
        Ok(crate::rooted::WalkStep::Continue)
    })
    .unwrap();
    assert_eq!(seen, vec!["g.txt".to_string()], "retained walk: {seen:?}");
}

#[test]
fn read_bounded_sets_slice_or_full_digest() {
    let (_d, _s, h) = fixture();
    fs::write(h.root().join("big.bin"), vec![7u8; 10_000]).unwrap();
    let data = h.read(Path::new("big.bin"), 1000).unwrap();
    assert!(data.full_hash().is_none(), "capped read must not be Full");
    assert_eq!(
        data.digest,
        ContentDigest::Slice {
            hash: FileHash::from(blake3::hash(&data.bytes).into()),
            offset: 0,
            len: 1000,
        }
    );
    assert_eq!(data.bytes.len(), 1000);
    let data = h.read(Path::new("big.bin"), 20_000).unwrap();
    assert_eq!(
        data.full_hash(),
        Some(FileHash::from(blake3::hash(&vec![7u8; 10_000]).into())),
        "whole read must carry the whole-file hash"
    );
    assert_eq!(data.bytes.len(), 10_000);
}

#[test]
fn bounded_read_never_exceeds_the_cap_under_concurrent_growth() {
    use std::io::Write as _;
    let (_d, _s, h) = fixture();
    let path = h.root().join("grow.bin");
    fs::write(&path, vec![1u8; 16]).unwrap();
    let cap = 512usize;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let writer_stop = stop.clone();
    let writer_start = start.clone();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&writer_path)
            .unwrap();
        writer_start.wait();
        let chunk = vec![2u8; 4096];
        while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
            if f.write_all(&chunk).is_err() {
                break;
            }
            let _ = f.flush();
            std::thread::yield_now();
        }
    });
    start.wait();
    let mut saw_slice = false;
    for i in 0..500 {
        let data = h.read(Path::new("grow.bin"), cap).unwrap();
        assert!(
            data.bytes.len() <= cap,
            "read {i} returned {} bytes (cap {cap}) while the file grew",
            data.bytes.len()
        );
        match &data.digest {
            ContentDigest::Slice { len, .. } => {
                assert_eq!(*len as usize, data.bytes.len());
                saw_slice = true;
            }
            ContentDigest::Full(hash) => {
                assert_eq!(
                    *hash,
                    FileHash::from(blake3::hash(&data.bytes).into()),
                    "a Full digest must hash exactly the bytes returned"
                );
            }
        }
    }
    assert!(
        saw_slice,
        "the concurrent grower must have produced at least one capped read"
    );
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
}

#[test]
fn read_slice_pages_whole_file() {
    let (_d, _s, h) = fixture();
    let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
    fs::write(h.root().join("paged.bin"), &content).unwrap();
    let mut assembled = Vec::new();
    let mut offset = 0u64;
    loop {
        let part = h.read_slice(Path::new("paged.bin"), offset, 777).unwrap();
        if part.bytes.is_empty() {
            break;
        }
        // Paging digests are always Slices: no page ever claims to be
        // the whole-file identity (full_hash is structurally None).
        assert!(
            part.full_hash().is_none(),
            "a paged read must never look like a whole-file hash"
        );
        assembled.extend_from_slice(&part.bytes);
        offset += part.bytes.len() as u64;
    }
    assert_eq!(assembled, content);
}

#[test]
fn write_atomic_replaces_and_hashes() {
    let (_d, _s, h) = fixture();
    let h1 = h.write_atomic(Path::new("a.txt"), b"one").unwrap();
    let h2 = h.write_atomic(Path::new("a.txt"), b"two").unwrap();
    assert_ne!(h1, h2);
    let data = h.read(Path::new("a.txt"), 100).unwrap();
    assert_eq!(data.bytes, b"two");
    assert_eq!(data.full_hash(), Some(h2));
    // No temp files left behind.
    let names: Vec<_> = fs::read_dir(h.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !names.iter().any(|n| atomic::is_internal_temp_name(n)),
        "temp leaked: {names:?}"
    );
}

#[tokio::test]
async fn watcher_delivers_events_with_workspace_id() {
    let (_d, _s, h) = fixture();
    assert!(
        h.watcher_attached(),
        "registration must have succeeded before delivery is asserted"
    );
    fs::write(h.root().join("w.txt"), "x").unwrap();
    let mut saw = false;
    // FSEvents delivery on a loaded host can lag well past any fixed
    // iteration count; the adversarial assertion (the event MUST arrive
    // with the workspace id) is unchanged, while the bound is a
    // deadline and the path is re-touched while waiting so a coalesced
    // first create can never strand the wait.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    let mut last_touch = std::time::Instant::now();
    loop {
        if let Ok(ev) = h.events().lock().unwrap().try_recv() {
            if ev.workspace_id == WorkspaceId::new(1) && ev.path.ends_with("w.txt") {
                saw = true;
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        if last_touch.elapsed() >= std::time::Duration::from_secs(1) {
            fs::write(h.root().join("w.txt"), "x").unwrap();
            last_touch = std::time::Instant::now();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(saw, "watcher must deliver the create event");
}

#[test]
fn missing_file_not_found() {
    let (_d, _s, h) = fixture();
    assert!(h.read(Path::new("nope.rs"), 100).is_err());
    assert!(h.stat(Path::new("nope.rs")).is_err());
    assert!(!h.exists(Path::new("nope.rs")));
}

#[tokio::test]
async fn concurrent_atomic_writes_never_partial() {
    let (_d, _s, h) = fixture();
    let mut handles = Vec::new();
    for t in 0..8 {
        let h = h.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..50 {
                let payload = format!("thread-{t}-{i}-{}", "x".repeat(100));
                h.write_atomic(Path::new("shared.txt"), payload.as_bytes())
                    .unwrap();
                let data = h.read(Path::new("shared.txt"), 10_000).unwrap();
                let text = String::from_utf8_lossy(&data.bytes);
                assert!(
                    text.starts_with("thread-") && text.contains("-x"),
                    "partial write observed: {text}"
                );
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

#[test]
fn list_bounded_and_sorted() {
    let (_d, _s, h) = fixture();
    for i in 0..30 {
        fs::write(h.root().join(format!("f{i:02}.rs")), "x").unwrap();
    }
    let list = h.list(Path::new("."), 10).unwrap();
    assert_eq!(list.len(), 10);
    assert!(list.windows(2).all(|w| w[0].path <= w[1].path));
}

#[test]
fn unicode_and_binary_paths() {
    let (_d, _s, h) = fixture();
    fs::write(h.root().join("é😀.rs"), b"\x00\x01\x02").unwrap();
    let data = h.read(Path::new("é😀.rs"), 100).unwrap();
    assert_eq!(data.bytes, vec![0, 1, 2]);
    assert!(h.exists(Path::new("é😀.rs")));
}

#[test]
fn identity_verification_blocks_cross_workspace() {
    let (_d, _s, h) = fixture();
    let wrong = WorkspaceIdentity::new(
        WorkspaceId::new(99),
        faktor_core::WorktreeId::new(1),
        faktor_core::TaskId::new(1),
    );
    assert!(h.verify_identity(&wrong).is_err());
    let right = WorkspaceIdentity::new(
        WorkspaceId::new(1),
        faktor_core::WorktreeId::new(1),
        faktor_core::TaskId::new(1),
    );
    assert!(h.verify_identity(&right).is_ok());
}

#[test]
fn open_is_idempotent_and_close_unloads() {
    let _guard = lock_watch_tests();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    fs::create_dir_all(&root).unwrap();
    let service = WorkspaceFileService::new();
    let h1 = service.open(WorkspaceId::new(1), root.clone()).unwrap();
    let h2 = service.open(WorkspaceId::new(1), root.clone()).unwrap();
    assert_eq!(h1.root(), h2.root());
    assert_eq!(service.open_count(), 1);
    // Same id, different root → a SECOND live root (real + live shadow
    // root of a shadowed drive); identity stays the id, and close()
    // unloads every root of the id.
    let other = dir.path().join("other");
    fs::create_dir_all(&other).unwrap();
    let shadow = service.open(WorkspaceId::new(1), other).unwrap();
    assert_eq!(shadow.workspace_id(), h1.workspace_id());
    assert_ne!(shadow.root(), h1.root());
    assert_eq!(service.open_count(), 2);
    service.close(WorkspaceId::new(1));
    assert_eq!(service.open_count(), 0);
}

#[test]
fn missing_root_not_found() {
    let _guard = lock_watch_tests();
    let service = WorkspaceFileService::new();
    assert!(service
        .open(WorkspaceId::new(1), "/definitely/not/here".into())
        .is_err());
}

#[test]
fn truncated_read_produces_slice_digest_full_read_produces_full() {
    let (_d, _s, h) = fixture();
    let content: Vec<u8> = (0..100_000).map(|i| (i % 253) as u8).collect();
    fs::write(h.root().join("dig.bin"), &content).unwrap();
    // Whole file: Full digest, equal to the plain read()'s digest.
    let (bytes, digest) = h.read_hashed(Path::new("dig.bin"), 200_000).unwrap();
    assert_eq!(bytes, content);
    let ContentDigest::Full(full_hash) = digest else {
        panic!("unbounded read must be Full");
    };
    let data = h.read(Path::new("dig.bin"), 200_000).unwrap();
    assert_eq!(data.full_hash(), Some(full_hash));
    // Capped read: Slice digest of exactly the prefix, never Full.
    let (bytes, digest) = h.read_hashed(Path::new("dig.bin"), 10_000).unwrap();
    assert_eq!(bytes.len(), 10_000);
    let ContentDigest::Slice {
        hash: slice_hash,
        offset: 0,
        len,
    } = digest
    else {
        panic!("truncated read must be Slice");
    };
    assert_eq!(len, 10_000);
    assert_eq!(
        slice_hash,
        FileHash::from(blake3::hash(&content[..10_000]).into()),
        "slice hash must be the hash of the returned prefix"
    );
    // Type-level separation: a Slice over the same bytes is never equal
    // to the Full digest of the file.
    assert_ne!(
        digest,
        ContentDigest::Full(FileHash::from(blake3::hash(&content).into()))
    );
    let data = h.read(Path::new("dig.bin"), 10_000).unwrap();
    assert_eq!(
        data.digest,
        ContentDigest::Slice {
            hash: slice_hash,
            offset: 0,
            len: 10_000
        },
        "read() keeps its digest honest: a capped read is a Slice"
    );
}

/// P0-50 structural adversarial test: a truncated read CANNOT be
/// mistaken for whole-file identity. The compiler is the first line of
/// defense (FileData exposes only `digest`), so this test proves the
/// runtime half: `full_hash()` is None exactly when the read was capped,
/// and the whole-file identity paths (the optimistic-concurrency CAS
/// expected-hash the edit engine feeds) must handle that case — here by
/// the same typed Oversized refusal the engine applies, both when the
/// caller insists on whole identity and when a whole read is requested
/// at a sufficient bound.
#[test]
fn capped_read_never_masquerades_as_whole_identity() {
    let (_d, _s, h) = fixture();
    let content: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
    fs::write(h.root().join("whole.bin"), &content).unwrap();
    // A cap below the file size: Slice digest -> full_hash() is None.
    let capped = h.read(Path::new("whole.bin"), 10_000).unwrap();
    let ContentDigest::Slice { .. } = capped.digest else {
        panic!("a capped read of a bigger file must be a Slice");
    };
    assert!(capped.full_hash().is_none());
    // A cap above the file size: Full digest -> whole-file identity.
    let whole = h.read(Path::new("whole.bin"), 200_000).unwrap();
    let ContentDigest::Full(full) = whole.digest else {
        panic!("an uncapped read must be Full");
    };
    assert_eq!(whole.full_hash(), Some(full));
    assert_eq!(whole.bytes.len(), content.len());

    // The edit/CAS expected-hash discipline (mirrors stage_one in
    // crates/edit): a caller that needs whole-file identity MUST
    // request a whole read; when it only has a Slice it must refuse
    // loudly instead of comparing a prefix hash. Both branches:
    let edit_bound = 16 * 1024 * 1024;
    let oversized_file: Vec<u8> = (0..(edit_bound + 1)).map(|i| (i % 7) as u8).collect();
    fs::write(h.root().join("too-big.bin"), &oversized_file).unwrap();
    let big = h.read(Path::new("too-big.bin"), edit_bound).unwrap();
    assert!(big.full_hash().is_none(), "over-bound read is a Slice");
    assert!(matches!(big.digest, ContentDigest::Slice { .. }));
    // Branch 1: honest refusal at read classification (the edit engine
    // raises Oversized when the digest is Slice).
    let oversized_refusal = big
        .full_hash()
        .map(|_| ())
        .ok_or_else(|| Error::oversized("exceeds the bound"));
    assert!(matches!(oversized_refusal, Err(e) if e.kind == ErrorKind::Oversized));
    // Branch 2: a whole read of the same file succeeds and the CAS
    // preimage is the true whole-file digest.
    let whole = h.read(Path::new("too-big.bin"), edit_bound + 1).unwrap();
    let cas_expected = whole.full_hash().expect("whole read must give identity");
    assert_eq!(
        cas_expected,
        FileHash::from(blake3::hash(&oversized_file).into())
    );
}

/// P0-50 structural guarantee at the type level: `FileData` is exactly
/// {path, bytes, size, digest}. This destructuring pattern has NO `..`,
/// so it only compiles while the struct exposes exactly these four
/// public fields — a re-introduced `hash` or `truncated` breaks the
/// build, keeping the ambiguity impossible by construction.
#[test]
fn filedata_shape_is_exactly_path_bytes_size_digest() {
    let (_d, _s, h) = fixture();
    fs::write(h.root().join("shape.txt"), b"shape").unwrap();
    let FileData {
        path,
        bytes,
        size,
        digest,
    } = h.read(Path::new("shape.txt"), 100).unwrap();
    assert_eq!(path.file_name().unwrap(), "shape.txt");
    assert_eq!(bytes, b"shape");
    assert_eq!(size, 5);
    assert_eq!(
        digest,
        ContentDigest::Full(FileHash::from(blake3::hash(b"shape").into()))
    );
}

/// P0-50 caller-hygiene scan: the crates that consume `FileData` from
/// bounded `WorkspaceHandle::read` must not touch a `.hash`/`.truncated`
/// field anymore (compile-driven migration proof for in-tree callers —
/// a field-access test cannot fail to compile in-tree, so this bounded
/// source scan over the consumer files is the regression net).
#[test]
fn filedata_consumers_never_access_hash_or_truncated_fields() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for rel in ["../edit/src/lib.rs", "../cli/src/tools.rs"] {
        let source = match fs::read_to_string(manifest_dir.join(rel)) {
            Ok(s) => s,
            Err(_) => continue, // crate absent (non-workspace checkout)
        };
        for (idx, line) in source.lines().enumerate() {
            let line_no = idx + 1;
            let mut rest = line;
            while let Some(pos) = rest.find(".hash") {
                let after = rest[pos + 5..].chars().next();
                let field_access = matches!(
                    after,
                    None | Some(' ')
                        | Some(',')
                        | Some('=')
                        | Some('!')
                        | Some(')')
                        | Some('?')
                        | Some(';')
                        | Some('.')
                        | Some('\t')
                        | Some('"')
                        | Some('}')
                );
                if field_access {
                    offenders.push(format!("{rel}:{line_no}: {line}"));
                }
                rest = &rest[pos + 5..];
            }
            if line.contains(".truncated") {
                offenders.push(format!("{rel}:{line_no}: {line}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "FileData consumers still access removed hash/truncated fields:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn content_digest_full_vs_slice_never_equal() {
    let h = FileHash::from([7; 32]);
    assert_ne!(
        ContentDigest::Full(h),
        ContentDigest::Slice {
            hash: h,
            offset: 0,
            len: 10,
        },
        "Full(same-hash) must never equal a Slice: a prefix is not the file"
    );
    assert!(ContentDigest::Full(h).is_full());
    assert!(!ContentDigest::Slice {
        hash: h,
        offset: 0,
        len: 10
    }
    .is_full());
}

#[test]
fn hash_file_streaming_matches_direct_read_and_caps() {
    let (_d, _s, h) = fixture();
    // 5 MB patterned control file: the streaming hash must equal a
    // direct full read.
    let content: Vec<u8> = (0..(5 * 1024 * 1024))
        .map(|i| ((i * 31 + 7) % 256) as u8)
        .collect();
    fs::write(h.root().join("stream.bin"), &content).unwrap();
    let (n, hash) = h
        .hash_file_streaming(Path::new("stream.bin"), None)
        .unwrap();
    assert_eq!(n, content.len() as u64);
    assert_eq!(hash, FileHash::from(blake3::hash(&content).into()));
    // Capped: hashes exactly the prefix, reports the byte count.
    let (n, hash) = h
        .hash_file_streaming(Path::new("stream.bin"), Some(12_345))
        .unwrap();
    assert_eq!(n, 12_345);
    assert_eq!(
        hash,
        FileHash::from(blake3::hash(&content[..12_345]).into())
    );
    // Empty file: zero bytes, empty-content hash.
    fs::write(h.root().join("empty.bin"), b"").unwrap();
    let (n, hash) = h.hash_file_streaming(Path::new("empty.bin"), None).unwrap();
    assert_eq!(n, 0);
    assert_eq!(hash, FileHash::from(blake3::hash(b"").into()));
}

#[test]
fn write_atomic_delegates_to_shared_helper_no_temps_after_storm() {
    let (_d, _s, h) = fixture();
    fs::create_dir_all(h.root().join("deep")).unwrap();
    for i in 0..25 {
        let payload = format!("storm-{i}-{}", "x".repeat(i * 100));
        let hash = h
            .write_atomic(Path::new("deep/f.bin"), payload.as_bytes())
            .unwrap();
        assert_eq!(
            hash,
            FileHash::from(blake3::hash(payload.as_bytes()).into())
        );
    }
    // Crash simulation (behavioral): the destination directory holds
    // exactly the target after the storm — replacement visible, zero
    // temp leftovers, and the parent was fsynced after each rename.
    let names: Vec<String> = fs::read_dir(h.root().join("deep"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["f.bin"], "temp leaked: {names:?}");
    let expected = format!("storm-24-{}", "x".repeat(2400));
    assert_eq!(
        fs::read(h.root().join("deep/f.bin")).unwrap(),
        expected.as_bytes()
    );
}

// ---------------------------------------------------------- audit 46/47

#[test]
fn write_atomic_cas_rejects_stale_writes_and_never_clobbers() {
    let (_d, _s, h) = fixture();
    fs::write(h.root().join("cas.txt"), b"original-0123456789").unwrap();
    let first = h.read(Path::new("cas.txt"), 100).unwrap();
    let first_hash = first.full_hash().expect("small file read whole");
    // Same state: succeeds.
    let new_hash = h
        .write_atomic_cas(Path::new("cas.txt"), first_hash, b"edited-by-tool")
        .unwrap();
    let after = h.read(Path::new("cas.txt"), 100).unwrap();
    assert_eq!(after.bytes, b"edited-by-tool");
    assert_eq!(after.full_hash(), Some(new_hash));
    // Stale expected hash: the file moved on (same size, different
    // digest) — CAS must reject, not clobber.
    let stale = first_hash;
    h.write_atomic(Path::new("cas.txt"), b"other-writer-0987654321")
        .unwrap();
    let err = h
        .write_atomic_cas(Path::new("cas.txt"), stale, b"late-edit-should-fail")
        .unwrap_err();
    assert!(
        err.message.contains("cas mismatch") || err.message.contains("changed"),
        "{err:?}"
    );
    assert_eq!(
        fs::read(h.root().join("cas.txt")).unwrap(),
        b"other-writer-0987654321"
    );
    // Deleted underneath: never silently recreated from a stale patch.
    fs::remove_file(h.root().join("cas.txt")).unwrap();
    let err = h
        .write_atomic_cas(Path::new("cas.txt"), stale, b"zombie")
        .unwrap_err();
    assert!(
        err.kind == ErrorKind::Conflict || err.kind == ErrorKind::NotFound,
        "{err:?}"
    );
    assert!(!h.root().join("cas.txt").exists());
}

#[cfg(unix)]
#[test]
fn read_after_directory_entry_swap_is_detected_via_open_identity() {
    let (_d, _s, h) = fixture();
    fs::write(h.root().join("swap.bin"), b"aaaaaaaaaaaaaaaaaaaa").unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"sssh").unwrap();

    let install = |hook: Box<dyn Fn(&Path) + Send>| {
        let m = READ_RACE_SEAM.get_or_init(|| std::sync::Mutex::new(None));
        *m.lock().expect("seam poisoned") = Some(hook);
    };

    // Seam fires between the OPEN and the identity check: the directory
    // entry is swapped for a symlink pointing outside the workspace. The
    // opened fd belongs to the original file; the path now names a
    // symlink — (dev, ino) must differ and the read must be rejected.
    // This is the canonical canonicalize-then-open TOCTOU, deterministic.
    let root = h.root().to_path_buf();
    let root2 = root.clone();
    install(Box::new(move |rel: &Path| {
        if rel == Path::new("swap.bin") {
            let t = root2.join("swap.bin");
            let _ = fs::remove_file(&t);
            #[cfg(unix)]
            std::os::unix::fs::symlink(outside.path().join("secret"), &t).unwrap();
        }
    }));
    let r = h.read(Path::new("swap.bin"), 100);
    assert!(r.is_err(), "symlink-swapped open must be rejected");
    assert!(r.clone().unwrap_err().message.contains("TOCTOU"), "{r:?}");
    // Cleanup so the symlink cannot leak into the next scenario.
    let _ = fs::remove_file(root.join("swap.bin"));

    // Second scenario: the directory entry is RENAMED over by a
    // different regular file mid-read (inode changes under the fd).
    fs::write(root.join("swap.bin"), b"aaaaaaaaaaaaaaaaaaaa").unwrap();
    let alt = root.join("alt.bin");
    let root3 = root.clone();
    fs::write(&alt, b"bbbbbbbbbbbbbbbbbbbb").unwrap();
    install(Box::new(move |rel: &Path| {
        if rel == Path::new("swap.bin") {
            let _ = fs::rename(&alt, root3.join("swap.bin"));
        }
    }));
    let r = h.read(Path::new("swap.bin"), 100);
    assert!(r.is_err(), "renamed-over open must be rejected");
    assert!(r.clone().unwrap_err().message.contains("TOCTOU"), "{r:?}");
    // Cleanup: swap.bin now contains alt's content; restore shape.
    let _ = fs::remove_file(root.join("swap.bin"));
    fs::write(root.join("swap.bin"), b"aaaaaaaaaaaaaaaaaaaa").unwrap();
    let ok = h.read(Path::new("swap.bin"), 100).unwrap();
    assert_eq!(ok.bytes, b"aaaaaaaaaaaaaaaaaaaa");
}

#[test]
fn benign_read_has_no_seam_effect() {
    let (_d, _s, h) = fixture();
    fs::write(h.root().join("ok.bin"), b"hello world").unwrap();
    let data = h.read(Path::new("ok.bin"), 100).unwrap();
    assert_eq!(data.bytes, b"hello world");
}

// ------------------------------------------------ wave-13 merge/snapshot

fn tree_fixture(root: &Path) {
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::write(root.join("a.txt"), b"alpha").unwrap();
    fs::write(root.join("src/b.rs"), b"mod b;").unwrap();
    fs::write(root.join("src/deep/c.rs"), b"fn c() {}").unwrap();
}

#[test]
fn snapshot_hashes_whole_files_sorted_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    fs::create_dir_all(&root).unwrap();
    tree_fixture(&root);
    let snap = snapshot_tree(&root, 100).unwrap();
    let paths: Vec<PathBuf> = snap.iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            PathBuf::from("a.txt"),
            PathBuf::from("src/b.rs"),
            PathBuf::from("src/deep/c.rs"),
        ]
    );
    for e in &snap {
        let whole = fs::read(root.join(&e.path)).unwrap();
        assert_eq!(
            e.hash,
            FileHash::from(blake3::hash(&whole).into()),
            "hash of {} must cover the whole file",
            e.path.display()
        );
        assert_eq!(e.size, whole.len() as u64);
    }
    // Entry cap: typed Oversized, never a silent truncation.
    let err = snapshot_tree(&root, 2).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Oversized);
    assert!(err.message.contains("cap"), "{err:?}");
}

#[cfg(unix)]
#[test]
fn snapshot_refuses_symlink_escape_and_unsupported_entries_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"s").unwrap();
    fs::write(root.join("ok.txt"), b"ok").unwrap();
    // File symlink pointing OUTSIDE the tree: the snapshot must fail
    // loudly — never silently skip the escaping file.
    symlink(outside.join("secret.txt"), root.join("evil-link")).unwrap();
    let err = snapshot_tree(&root, 100).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    assert!(err.message.contains("escape"), "{err:?}");
    fs::remove_file(root.join("evil-link")).unwrap();
    // A FIFO is not a file: loud, never skipped.
    #[cfg(unix)]
    {
        let fifo = root.join("fifo");
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        let err = snapshot_tree(&root, 100).unwrap_err();
        assert!(err.message.contains("unsupported"), "{err:?}");
        fs::remove_file(&fifo).unwrap();
    }
    // Inside-root file symlink is fine (same discipline as resolve()).
    symlink(root.join("ok.txt"), root.join("alias")).unwrap();
    let snap = snapshot_tree(&root, 100).unwrap();
    let paths: Vec<PathBuf> = snap.iter().map(|e| e.path.clone()).collect();
    assert_eq!(paths, vec![PathBuf::from("alias"), PathBuf::from("ok.txt")]);
    assert_eq!(snap[0].hash, snap[1].hash, "alias hashes the target");
}

#[test]
fn copy_tree_replicates_content_and_respects_both_caps() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    tree_fixture(&src);
    let manifest = copy_tree(&src, &dst, 100, 1_000_000).unwrap();
    assert_eq!(manifest.len(), 3);
    let src_snap = snapshot_tree(&src, 100).unwrap();
    assert_eq!(
        manifest, src_snap,
        "copy manifest equals the source snapshot"
    );
    let dst_snap = snapshot_tree(&dst, 100).unwrap();
    assert_eq!(dst_snap, manifest, "destination is byte-identical");
    for e in &manifest {
        assert_eq!(
            fs::read(src.join(&e.path)).unwrap(),
            fs::read(dst.join(&e.path)).unwrap()
        );
    }
    // Entry cap and byte cap are loud Oversized.
    let err = copy_tree(&src, &dst, 2, 1_000_000).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Oversized);
    assert!(err.message.contains("entry"), "{err:?}");
    let err = copy_tree(&src, &dst, 100, 3).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Oversized);
    assert!(err.message.contains("byte"), "{err:?}");
    // Same tree is refused; a missing root is not found.
    assert_eq!(
        copy_tree(&src, &src, 100, 1_000_000).unwrap_err().kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        copy_tree(&dir.path().join("ghost"), &dst, 100, 1_000_000)
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

#[test]
fn copy_under_concurrent_atomic_writers_never_sees_a_torn_file() {
    // A writer repeatedly replaces two files with write_atomic_cas
    // payloads A/B while copy_tree runs: every copied file must be a
    // WHOLE A or a WHOLE B image — never a mix — and the copy must
    // succeed (per-file atomic snapshots are the contract, cross-file
    // mixing is allowed).
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let payload_a = format!("AAAAAAAA-{}", "x".repeat(4096));
    let payload_b = format!("BBBBBBBB-{}", "y".repeat(4096));
    let service = WorkspaceFileService::new();
    let h = {
        let _guard = lock_watch_tests();
        service.open(WorkspaceId::new(1), src.clone()).unwrap()
    };
    h.write_atomic(Path::new("f1.bin"), payload_a.as_bytes())
        .unwrap();
    h.write_atomic(Path::new("f2.bin"), payload_a.as_bytes())
        .unwrap();
    for round in 0..6 {
        let dst = dir.path().join(format!("dst{round}"));
        fs::create_dir_all(&dst).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h1 = h.clone();
        let s1 = stop.clone();
        let pa = payload_a.clone();
        let pb = payload_b.clone();
        let writer = std::thread::spawn(move || {
            let mut turn = 0usize;
            while !s1.load(std::sync::atomic::Ordering::SeqCst) {
                let (first, second) = if turn.is_multiple_of(2) {
                    (pa.as_bytes(), pb.as_bytes())
                } else {
                    (pb.as_bytes(), pa.as_bytes())
                };
                for (name, content) in [("f1.bin", first), ("f2.bin", second)] {
                    let _ = h1.read(Path::new(name), 1_000_000).map(|d| {
                        let _ = h1.write_atomic_cas(
                            Path::new(name),
                            d.full_hash().expect("payloads fit the read bound"),
                            content,
                        );
                    });
                }
                turn += 1;
            }
        });
        let result = copy_tree(&src, &dst, 100, 1_000_000);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        writer.join().unwrap();
        let manifest = result.expect("copy must succeed under the writer");
        assert_eq!(manifest.len(), 2);
        for e in &manifest {
            let content = fs::read(dst.join(&e.path)).unwrap();
            let text = String::from_utf8(content).unwrap();
            assert!(
                text == payload_a || text == payload_b,
                "round {round}: torn copy of {}: {text:?}",
                e.path.display()
            );
            assert_eq!(
                e.hash,
                FileHash::from(blake3::hash(text.as_bytes()).into()),
                "manifest hash must match the copied bytes"
            );
        }
    }
}

#[test]
fn merge_apply_classifies_conflict_already_current_and_applied() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let child = dir.path().join("child");
    fs::create_dir_all(&parent).unwrap();
    fs::create_dir_all(&child).unwrap();
    // Parent file at the base snapshot.
    fs::write(parent.join("f.rs"), b"base-content").unwrap();
    let base_hash = FileHash::from(blake3::hash(b"base-content").into());
    // Child modified it.
    fs::write(child.join("f.rs"), b"child-edit").unwrap();
    let child_hash = FileHash::from(blake3::hash(b"child-edit").into());
    // 1. Parent unchanged since base: applied, digest preserved.
    let r = merge_apply_content(
        &parent,
        Path::new("f.rs"),
        &child,
        Path::new("f.rs"),
        child_hash,
        Some(base_hash),
    )
    .unwrap();
    assert_eq!(r, CasMergeResult::Applied);
    assert_eq!(fs::read(parent.join("f.rs")).unwrap(), b"child-edit");
    // 2. Replay: already merged content -> AlreadyCurrent, idempotent.
    let r = merge_apply_content(
        &parent,
        Path::new("f.rs"),
        &child,
        Path::new("f.rs"),
        child_hash,
        Some(base_hash),
    )
    .unwrap();
    assert_eq!(r, CasMergeResult::AlreadyCurrent);
    assert_eq!(fs::read(parent.join("f.rs")).unwrap(), b"child-edit");
    // 3. Parent moved on (different content): conflict, parent intact.
    fs::write(parent.join("f.rs"), b"parent-edit-after-base").unwrap();
    let err = merge_apply_content(
        &parent,
        Path::new("f.rs"),
        &child,
        Path::new("f.rs"),
        child_hash,
        Some(base_hash),
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    assert_eq!(
        fs::read(parent.join("f.rs")).unwrap(),
        b"parent-edit-after-base",
        "the conflicting parent file must stay byte-identical"
    );
    // 4. Parent file vanished: never recreated from a stale patch.
    fs::remove_file(parent.join("f.rs")).unwrap();
    let err = merge_apply_content(
        &parent,
        Path::new("f.rs"),
        &child,
        Path::new("f.rs"),
        child_hash,
        Some(base_hash),
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    assert!(!parent.join("f.rs").exists());
    // 5. Child content drifted since staging: conflict, nothing merged.
    fs::write(parent.join("f.rs"), b"base-content").unwrap();
    fs::write(child.join("f.rs"), b"child-changed-again").unwrap();
    let err = merge_apply_content(
        &parent,
        Path::new("f.rs"),
        &child,
        Path::new("f.rs"),
        child_hash,
        Some(base_hash),
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    assert!(err.message.contains("drifted"), "{err:?}");
    assert_eq!(fs::read(parent.join("f.rs")).unwrap(), b"base-content");
}

#[test]
fn merge_create_is_exclusive_and_replay_safe() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let child = dir.path().join("child");
    fs::create_dir_all(&parent).unwrap();
    fs::create_dir_all(&child).unwrap();
    fs::write(child.join("new.txt"), b"child-new-file").unwrap();
    let child_hash = FileHash::from(blake3::hash(b"child-new-file").into());
    // Base had no such file (expected None): exclusive create.
    let r = merge_apply_content(
        &parent,
        Path::new("new.txt"),
        &child,
        Path::new("new.txt"),
        child_hash,
        None,
    )
    .unwrap();
    assert_eq!(r, CasMergeResult::Applied);
    assert_eq!(fs::read(parent.join("new.txt")).unwrap(), b"child-new-file");
    // Replay: file already holds the child digest -> AlreadyCurrent.
    let r = merge_apply_content(
        &parent,
        Path::new("new.txt"),
        &child,
        Path::new("new.txt"),
        child_hash,
        None,
    )
    .unwrap();
    assert_eq!(r, CasMergeResult::AlreadyCurrent);
    // A parent-side file appeared since the base: conflict, never an
    // overwrite of a file the merge did not create.
    fs::remove_file(parent.join("new.txt")).unwrap();
    fs::write(parent.join("new.txt"), b"parent-made-this").unwrap();
    let err = merge_apply_content(
        &parent,
        Path::new("new.txt"),
        &child,
        Path::new("new.txt"),
        child_hash,
        None,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    assert_eq!(
        fs::read(parent.join("new.txt")).unwrap(),
        b"parent-made-this"
    );
}

#[test]
fn merge_create_materializes_missing_parent_dirs_inside_the_root_only() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let child = dir.path().join("child");
    fs::create_dir_all(&parent).unwrap();
    fs::create_dir_all(&child).unwrap();
    // The child created a nested file whose parent directories never
    // existed in the parent tree: the merge creates them.
    fs::create_dir_all(child.join("sub/deep")).unwrap();
    fs::write(child.join("sub/deep/new.rs"), b"child-nested").unwrap();
    let child_hash = FileHash::from(blake3::hash(b"child-nested").into());
    let r = merge_apply_content(
        &parent,
        Path::new("sub/deep/new.rs"),
        &child,
        Path::new("sub/deep/new.rs"),
        child_hash,
        None,
    )
    .unwrap();
    assert_eq!(r, CasMergeResult::Applied);
    assert_eq!(
        fs::read(parent.join("sub/deep/new.rs")).unwrap(),
        b"child-nested"
    );
    // Replay after the dirs exist: AlreadyCurrent, dirs not duplicated.
    let r = merge_apply_content(
        &parent,
        Path::new("sub/deep/new.rs"),
        &child,
        Path::new("sub/deep/new.rs"),
        child_hash,
        None,
    )
    .unwrap();
    assert_eq!(r, CasMergeResult::AlreadyCurrent);
    // A hostile create that must walk THROUGH an existing file for an
    // intermediate component is a conflict, never a clobber.
    fs::write(parent.join("blocker"), b"i-am-a-file").unwrap();
    fs::create_dir_all(child.join("blocker/x")).unwrap();
    fs::write(child.join("blocker/x/y.rs"), b"z").unwrap();
    let hz = FileHash::from(blake3::hash(b"z").into());
    let err = merge_apply_content(
        &parent,
        Path::new("blocker/x/y.rs"),
        &child,
        Path::new("blocker/x/y.rs"),
        hz,
        None,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict, "{err:?}");
    assert_eq!(fs::read(parent.join("blocker")).unwrap(), b"i-am-a-file");
}

#[test]
fn merge_delete_removes_only_the_base_content() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    fs::create_dir_all(&parent).unwrap();
    fs::write(parent.join("gone.rs"), b"base-content").unwrap();
    let base_hash = FileHash::from(blake3::hash(b"base-content").into());
    assert_eq!(
        merge_delete(&parent, Path::new("gone.rs"), base_hash).unwrap(),
        CasMergeResult::Applied
    );
    assert!(!parent.join("gone.rs").exists());
    // Replay after the crashed run deleted it: end state (absent) holds.
    assert_eq!(
        merge_delete(&parent, Path::new("gone.rs"), base_hash).unwrap(),
        CasMergeResult::AlreadyCurrent
    );
    // Parent content moved on: the deletion is refused, file intact.
    fs::write(parent.join("gone.rs"), b"base-content").unwrap();
    fs::write(parent.join("gone.rs"), b"parent-recreated").unwrap();
    let err = merge_delete(&parent, Path::new("gone.rs"), base_hash).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    assert_eq!(
        fs::read(parent.join("gone.rs")).unwrap(),
        b"parent-recreated"
    );
}

#[cfg(unix)]
#[test]
fn merge_paths_are_resolved_traversal_and_escape_safe() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let child = dir.path().join("child");
    fs::create_dir_all(&parent).unwrap();
    fs::create_dir_all(&child).unwrap();
    fs::write(child.join("x.txt"), b"x").unwrap();
    let h = FileHash::from(blake3::hash(b"x").into());
    for evil in ["../x.txt", "a/../../x.txt", "/etc/x.txt", ".."] {
        let err = merge_apply_content(
            &parent,
            Path::new(evil),
            &child,
            Path::new("x.txt"),
            h,
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission, "{evil} must be refused");
    }
    let err = merge_apply_content(
        &parent,
        Path::new("x.txt"),
        &child,
        Path::new("x.txt"),
        h,
        None,
    )
    .unwrap();
    assert_eq!(err, CasMergeResult::Applied);
    // An inside-root symlink destination that escapes the parent root
    // must never be followed for the write.
    let outside = dir.path().join("outside-root");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("victim.txt"), b"v").unwrap();
    fs::create_dir_all(parent.join("sub")).unwrap();
    symlink(&outside, parent.join("sub/link-out")).unwrap();
    fs::write(child.join("payload.txt"), b"p").unwrap();
    let hp = FileHash::from(blake3::hash(b"p").into());
    let err = merge_apply_content(
        &parent,
        Path::new("sub/link-out/victim.txt"),
        &child,
        Path::new("payload.txt"),
        hp,
        None,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    assert_eq!(fs::read(outside.join("victim.txt")).unwrap(), b"v");
    assert_eq!(
        merge_delete(&parent, Path::new("../x"), h)
            .unwrap_err()
            .kind,
        ErrorKind::Permission
    );
}

// ================================================= wave-11 fd-relative
// traversal (P0-49): per-component openat walk + bounded readlink
// re-anchoring. The seams swap directory entries BETWEEN walk steps,
// deterministically, and the assertions are always "original content or
// loud rejection — never the attacker's file".

/// Seam tests share ONE global walk seam, so they must run serially and
/// must always clean up (also on panic): the guard holds a process-wide
/// mutex for the whole test and clears the seam when dropped.
#[cfg(unix)]
struct SeamTest {
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl SeamTest {
    fn new() -> Self {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        Self { _serial }
    }
}

#[cfg(unix)]
impl Drop for SeamTest {
    fn drop(&mut self) {
        platform::clear_walk_seam();
    }
}

/// Install a walk seam that fires at most ONCE, only on the installing
/// thread (tests on other threads must never trip a seam) and only for
/// the given component name. The hook runs to completion between two
/// walk steps — the walker cannot observe a state between the hook's
/// own syscalls.
#[cfg(unix)]
fn swap_seam(root: &Path, target: &str, hook: impl Fn(&Path) + Send + 'static) {
    let me = std::thread::current().id();
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let root = root.to_path_buf();
    let target: std::ffi::OsString = target.into();
    platform::install_walk_seam(Box::new(move |comp: &OsStr| {
        if std::thread::current().id() == me
            && *comp == *target
            && !fired.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            hook(&root);
        }
    }));
}

// ============================================ rooted-write TOCTOU seams
// The writers below resolve a destination ONCE and then mutate by
// pathname; these tests swap the parent for an outside symlink in the
// seam fired by the pre-mutation parent re-verification and assert the
// call refuses typed BEFORE mutating, with the outside file untouched.

/// A canonical `root` with `root/sub` materialized plus an OUTSIDE victim dir.
#[cfg(unix)]
fn swap_fixture(
    root_dir: &tempfile::TempDir,
    outside_dir: &tempfile::TempDir,
) -> (PathBuf, PathBuf) {
    let root = root_dir.path().canonicalize().unwrap();
    let outside = outside_dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(outside.join("victim.txt"), b"OUTSIDE-VICTIM").unwrap();
    (root, outside)
}

/// Swap `root/sub` for a symlink to `outside` inside the walk seam.
#[cfg(unix)]
fn swap_sub_for_outside(root: &Path, outside: &Path) {
    swap_seam(root, "sub", {
        let outside = outside.to_path_buf();
        move |root| {
            fs::rename(root.join("sub"), root.join("sub-moved")).unwrap();
            symlink(&outside, root.join("sub")).unwrap();
        }
    });
}

/// `merge_apply_content` replace + exclusive-create must refuse a parent
/// swapped to an outside symlink; the outside file (the rename's would-be
/// victim) and the absent create target stay untouched.
#[cfg(unix)]
#[test]
fn merge_apply_content_refuses_a_parent_swapped_to_an_outside_symlink() {
    let _seam = SeamTest::new();
    let root_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    let (root, outside) = swap_fixture(&root_dir, &outside_dir);
    let child = child_dir.path().canonicalize().unwrap();
    fs::write(root.join("sub/f.txt"), b"base-content").unwrap();
    fs::write(outside.join("f.txt"), b"OUTSIDE-VICTIM").unwrap();
    fs::write(child.join("f.txt"), b"child-edit").unwrap();
    let base = FileHash::from(blake3::hash(b"base-content").into());
    let landed = FileHash::from(blake3::hash(b"child-edit").into());
    swap_sub_for_outside(&root, &outside);
    let err = merge_apply_content(
        &root,
        Path::new("sub/f.txt"),
        &child,
        Path::new("f.txt"),
        landed,
        Some(base),
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert_eq!(
        fs::read(outside.join("f.txt")).unwrap(),
        b"OUTSIDE-VICTIM",
        "the swap redirected the merge outside the root"
    );
    assert_eq!(
        fs::read(outside.join("victim.txt")).unwrap(),
        b"OUTSIDE-VICTIM"
    );
    // Refused BEFORE the mutation: the destination (moved aside by the seam hook) still holds its base bytes.
    assert_eq!(
        fs::read(root.join("sub-moved/f.txt")).unwrap(),
        b"base-content",
        "the merge mutated the destination before refusing"
    );

    // Exclusive create (expected None): without the parent re-verification
    // the hard link/rename would publish the child's file outside.
    let root_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let (root, outside) = swap_fixture(&root_dir, &outside_dir);
    fs::write(child.join("new.txt"), b"child-new").unwrap();
    let landed = FileHash::from(blake3::hash(b"child-new").into());
    swap_sub_for_outside(&root, &outside);
    let err = merge_apply_content(
        &root,
        Path::new("sub/new.txt"),
        &child,
        Path::new("new.txt"),
        landed,
        None,
    )
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(
        !outside.join("new.txt").exists(),
        "the exclusive create landed outside the root"
    );
    assert!(
        !root.join("sub-moved/new.txt").exists(),
        "the exclusive create mutated inside the root before refusing"
    );
}

/// `merge_delete` must refuse a parent swapped to an outside symlink: the
/// outside namesake the unlink would have removed stays present.
#[cfg(unix)]
#[test]
fn merge_delete_refuses_a_parent_swapped_to_an_outside_symlink() {
    let _seam = SeamTest::new();
    let root_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let (root, outside) = swap_fixture(&root_dir, &outside_dir);
    fs::write(root.join("sub/gone.rs"), b"base-content").unwrap();
    fs::write(outside.join("gone.rs"), b"OUTSIDE-VICTIM").unwrap();
    let base = FileHash::from(blake3::hash(b"base-content").into());
    swap_sub_for_outside(&root, &outside);
    let err = merge_delete(&root, Path::new("sub/gone.rs"), base).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert_eq!(
        fs::read(outside.join("gone.rs")).unwrap(),
        b"OUTSIDE-VICTIM",
        "the swap redirected the unlink outside the root"
    );
    assert_eq!(
        fs::read(root.join("sub-moved/gone.rs")).unwrap(),
        b"base-content",
        "the deletion mutated the destination before refusing"
    );
}

/// `cas_write_content` (landing/rollback writer) must refuse a parent
/// swapped to an outside symlink on both the replace and the create arm.
#[cfg(unix)]
#[test]
fn cas_write_content_refuses_a_parent_swapped_to_an_outside_symlink() {
    let _seam = SeamTest::new();
    let root_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let (root, outside) = swap_fixture(&root_dir, &outside_dir);
    fs::write(root.join("sub/f.txt"), b"base-content").unwrap();
    fs::write(outside.join("f.txt"), b"OUTSIDE-VICTIM").unwrap();
    let base = FileHash::from(blake3::hash(b"base-content").into());
    swap_sub_for_outside(&root, &outside);
    let err = cas_write_content(&root, Path::new("sub/f.txt"), b"landed", Some(base)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert_eq!(
        fs::read(outside.join("f.txt")).unwrap(),
        b"OUTSIDE-VICTIM",
        "the swap redirected the CAS write outside the root"
    );
    assert_eq!(
        fs::read(root.join("sub-moved/f.txt")).unwrap(),
        b"base-content",
        "the CAS write mutated the destination before refusing"
    );

    let root_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let (root, outside) = swap_fixture(&root_dir, &outside_dir);
    swap_sub_for_outside(&root, &outside);
    let err = cas_write_content(&root, Path::new("sub/new.txt"), b"landed", None).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(
        !outside.join("new.txt").exists(),
        "the CAS-absent create landed outside the root"
    );
    assert!(
        !root.join("sub-moved/new.txt").exists(),
        "the CAS-absent create mutated inside the root before refusing"
    );
}

/// Every `entry_state` transition shape (create, replace, mode-only,
/// to-symlink, retarget, remove) must refuse a parent swapped to an
/// outside symlink; the outside namesake stays byte-identical.
#[cfg(unix)]
#[test]
fn entry_state_transitions_refuse_a_parent_swapped_to_an_outside_symlink() {
    use crate::entry_state::{apply_tree_entry_cas, EntryState};
    use crate::tree_manifest::CanonicalMode;

    let regular = |bytes: &[u8], mode| {
        EntryState::regular(mode, FileHash::from(blake3::hash(bytes).into())).unwrap()
    };
    let shapes: Vec<(&str, EntryState, EntryState, Vec<u8>)> = vec![
        (
            "create",
            EntryState::Absent,
            regular(b"created", CanonicalMode::RegularFile),
            b"created".to_vec(),
        ),
        (
            "replace",
            regular(b"base", CanonicalMode::RegularFile),
            regular(b"next", CanonicalMode::RegularFile),
            b"next".to_vec(),
        ),
        (
            "mode-only",
            regular(b"same", CanonicalMode::RegularFile),
            regular(b"same", CanonicalMode::ExecutableFile),
            b"same".to_vec(),
        ),
        (
            "to-symlink",
            regular(b"base", CanonicalMode::RegularFile),
            EntryState::symlink(b"target-x".to_vec()).unwrap(),
            b"target-x".to_vec(),
        ),
        (
            "retarget",
            EntryState::symlink(b"target-a".to_vec()).unwrap(),
            EntryState::symlink(b"target-b".to_vec()).unwrap(),
            b"target-b".to_vec(),
        ),
        (
            "remove",
            regular(b"base", CanonicalMode::RegularFile),
            EntryState::Absent,
            Vec::new(),
        ),
    ];
    for (name, expected, next, material) in shapes {
        let _seam = SeamTest::new();
        let root_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let (root, outside) = swap_fixture(&root_dir, &outside_dir);
        // Materialize the starting state at root/sub/f and the outside namesake.
        match &expected {
            EntryState::Absent => {}
            EntryState::Regular { mode, .. } => {
                let bytes: &[u8] = if name == "mode-only" {
                    b"same"
                } else {
                    b"base"
                };
                fs::write(root.join("sub/f"), bytes).unwrap();
                let bits = if *mode == CanonicalMode::ExecutableFile {
                    0o755
                } else {
                    0o644
                };
                fs::set_permissions(
                    root.join("sub/f"),
                    <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(bits),
                )
                .unwrap();
            }
            EntryState::Symlink { target, .. } => {
                symlink(
                    String::from_utf8(target.clone()).unwrap(),
                    root.join("sub/f"),
                )
                .unwrap();
            }
        }
        let victim: Vec<u8> = match &expected {
            EntryState::Absent => b"OUTSIDE-VICTIM".to_vec(),
            EntryState::Regular { .. } => fs::read(root.join("sub/f")).unwrap(),
            EntryState::Symlink { target, .. } => target.clone(),
        };
        fs::write(outside.join("f"), &victim).unwrap();
        swap_sub_for_outside(&root, &outside);
        let err = apply_tree_entry_cas(&root, Path::new("sub/f"), &expected, &next, &material)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission, "{name}: {err:?}");
        assert_eq!(
            fs::read(outside.join("f")).unwrap(),
            victim,
            "{name}: the swap redirected the transition outside the root"
        );
        // Refused BEFORE the mutation: the moved-aside real entry still carries the expected state.
        assert_eq!(
            crate::entry_state::state_of_path(&root.join("sub-moved/f")).unwrap(),
            expected,
            "{name}: the transition mutated the destination before refusing"
        );
    }
}

/// (a1) An intermediate directory swapped for an OUTSIDE symlink BEFORE
/// the walk opens it (the exact canonicalize-then-open window) is
/// rejected: the walk sees the symlink with O_NOFOLLOW, readlinks it and
/// denies the absolute escape. The outside marker never surfaces.
#[cfg(unix)]
#[test]
fn walk_rejects_intermediate_swap_to_outside_symlink_before_open() {
    let _seam = SeamTest::new();
    let (_d, _s, h) = fixture();
    let root = h.root().to_path_buf();
    fs::create_dir_all(root.join("w/dir")).unwrap();
    fs::write(root.join("w/dir/f.txt"), b"INSIDE-ORIGINAL").unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(outside.path().join("dir")).unwrap();
    fs::write(outside.path().join("dir/f.txt"), b"OUTSIDE-SECRET").unwrap();
    // The seam fires when the walk is ABOUT to open "w": swap the real
    // directory away and put an outside-pointing symlink in its place.
    let out = outside.path().to_path_buf();
    swap_seam(&root, "w", move |root| {
        fs::rename(root.join("w"), root.join("w-moved")).unwrap();
        symlink(&out, root.join("w")).unwrap();
    });
    let r = h.read(Path::new("w/dir/f.txt"), 100);
    assert!(r.is_err(), "a pre-open symlink swap must be rejected");
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(
        err.message.contains("workspace") || err.message.contains("symlink"),
        "{err:?}"
    );
    // The attacker's file was never read and the original is intact.
    assert_eq!(
        fs::read(outside.path().join("dir/f.txt")).unwrap(),
        b"OUTSIDE-SECRET"
    );
    // Restore: replace the swapped-in symlink with the moved original.
    fs::remove_file(root.join("w")).unwrap();
    fs::rename(root.join("w-moved"), root.join("w")).unwrap();
    let ok = h.read(Path::new("w/dir/f.txt"), 100).unwrap();
    assert_eq!(ok.bytes, b"INSIDE-ORIGINAL");
}

/// (a2) A swap AFTER the directory was opened (fd already anchored)
/// cannot redirect the walk: it continues inside the ORIGINAL directory
/// and the post-open identity net then rejects the read because the path
/// name now resolves outside. The outside marker never surfaces.
#[cfg(unix)]
#[test]
fn walk_after_intermediate_swap_reads_pinned_dir_and_net_rejects() {
    let _seam = SeamTest::new();
    let (_d, _s, h) = fixture();
    let root = h.root().to_path_buf();
    fs::create_dir_all(root.join("w/dir")).unwrap();
    fs::write(root.join("w/dir/f.txt"), b"INSIDE-ORIGINAL").unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(outside.path().join("dir")).unwrap();
    fs::write(outside.path().join("dir/f.txt"), b"OUTSIDE-SECRET").unwrap();
    // The seam fires when the walk is about to open "dir" INSIDE the
    // already-anchored "w": swap "w" for an outside-pointing symlink.
    let out = outside.path().to_path_buf();
    swap_seam(&root, "dir", move |root| {
        fs::rename(root.join("w"), root.join("w-moved")).unwrap();
        symlink(&out, root.join("w")).unwrap();
    });
    let r = h.read(Path::new("w/dir/f.txt"), 100);
    assert!(
        r.is_err(),
        "path/name divergence after the swap must be rejected"
    );
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(err.message.contains("TOCTOU"), "{err:?}");
    assert_eq!(
        fs::read(outside.path().join("dir/f.txt")).unwrap(),
        b"OUTSIDE-SECRET"
    );
    fs::remove_file(root.join("w")).unwrap();
    fs::rename(root.join("w-moved"), root.join("w")).unwrap();
    let ok = h.read(Path::new("w/dir/f.txt"), 100).unwrap();
    assert_eq!(ok.bytes, b"INSIDE-ORIGINAL");
}

/// (a3) Same mid-walk swap, but the replacement symlink points INSIDE
/// the root (the moved original): the walk completes against the pinned
/// fd and reads the ORIGINAL content — never the outside file.
#[cfg(unix)]
#[test]
fn walk_after_inroot_swap_completes_with_original_content() {
    let _seam = SeamTest::new();
    let (_d, _s, h) = fixture();
    let root = h.root().to_path_buf();
    fs::create_dir_all(root.join("w/dir")).unwrap();
    fs::write(root.join("w/dir/f.txt"), b"INSIDE-ORIGINAL").unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(outside.path().join("dir")).unwrap();
    fs::write(outside.path().join("dir/f.txt"), b"OUTSIDE-SECRET").unwrap();
    swap_seam(&root, "dir", move |root| {
        fs::rename(root.join("w"), root.join("w-moved")).unwrap();
        symlink(Path::new("w-moved"), root.join("w")).unwrap();
    });
    let r = h.read(Path::new("w/dir/f.txt"), 100);
    let ok = r.expect("in-root symlink swap after anchoring must not redirect");
    assert_eq!(ok.bytes, b"INSIDE-ORIGINAL");
    assert_eq!(
        fs::read(outside.path().join("dir/f.txt")).unwrap(),
        b"OUTSIDE-SECRET"
    );
    fs::remove_file(root.join("w")).unwrap();
    fs::rename(root.join("w-moved"), root.join("w")).unwrap();
}

/// (b) FINAL-component symlinks: legitimate repo symlinks keep working
/// (relative, absolute, ".."-inside-root, intermediate dir links) by
/// explicit bounded readlink re-anchoring — while outside targets and
/// escaping ".." targets are denied, and loops fail bounded.
#[cfg(unix)]
#[test]
fn final_and_intermediate_symlinks_follow_boundedly_and_deny_escapes() {
    let (_d, _s, h) = fixture();
    let root = h.root().to_path_buf();
    fs::write(root.join("real.txt"), b"REAL-CONTENT").unwrap();
    fs::write(root.join("other.txt"), b"OTHER-CONTENT").unwrap();
    fs::create_dir_all(root.join("sub/deep")).unwrap();
    fs::create_dir_all(root.join("realdir")).unwrap();
    fs::write(root.join("realdir/x.txt"), b"VIA-DIR-LINK").unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"OUTSIDE-SECRET").unwrap();

    // Final file symlink, relative target (repo content).
    symlink(Path::new("other.txt"), root.join("alias-rel")).unwrap();
    let d = h.read(Path::new("alias-rel"), 100).unwrap();
    assert_eq!(d.bytes, b"OTHER-CONTENT");
    // Final file symlink, absolute in-root target.
    symlink(root.join("real.txt"), root.join("alias-abs")).unwrap();
    let d = h.read(Path::new("alias-abs"), 100).unwrap();
    assert_eq!(d.bytes, b"REAL-CONTENT");
    // Intermediate DIRECTORY symlink, relative target.
    symlink(Path::new("realdir"), root.join("dir-link")).unwrap();
    let d = h.read(Path::new("dir-link/x.txt"), 100).unwrap();
    assert_eq!(d.bytes, b"VIA-DIR-LINK");
    // Relative target with ".." that STAYS inside the root (canonicalize
    // today follows it; the rebase keeps that behavior).
    symlink(Path::new("../other.txt"), root.join("sub/link")).unwrap();
    let d = h.read(Path::new("sub/link"), 100).unwrap();
    assert_eq!(d.bytes, b"OTHER-CONTENT");
    // Relative ".." target that would climb ABOVE the root: denied by
    // the walker itself (resolve_fd is the pure walk, no resolve()).
    symlink(Path::new("../../../etc/hosts"), root.join("sub/evil")).unwrap();
    let r = h.resolve_fd(Path::new("sub/evil"));
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(err.message.contains("climbs above"), "{err:?}");
    // ... and the read-level path agrees (resolve() rejects the escape).
    let r = h.read(Path::new("sub/evil"), 100);
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    // Absolute target OUTSIDE the root: denied.
    symlink(outside.path().join("secret.txt"), root.join("evil-abs")).unwrap();
    let r = h.read(Path::new("evil-abs"), 100);
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert_eq!(
        fs::read(outside.path().join("secret.txt")).unwrap(),
        b"OUTSIDE-SECRET",
        "the outside file must never be read through the workspace"
    );
    // Final symlink loop: bounded hops in the pure walker...
    symlink(Path::new("loop.txt"), root.join("loop.txt")).unwrap();
    let r = h.resolve_fd(Path::new("loop.txt"));
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(err.message.contains("hop bound"), "{err:?}");
    // ... and a loud Permission from the read path (resolve() reports it
    // as an unresolvable symlink before the walk even starts).
    let r = h.read(Path::new("loop.txt"), 100);
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    // resolve_fd agrees on the escape denial (absolute, outside).
    let r = h.resolve_fd(Path::new("evil-abs"));
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(err.message.contains("leaves the workspace"), "{err:?}");
    let fd = h.resolve_fd(Path::new("alias-abs")).unwrap();
    let mut f = fs::File::from(fd);
    use std::io::Read;
    let mut buf = String::new();
    f.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, "REAL-CONTENT");
}

/// (e) Symlink loop storm across DIRECTORY symlinks (d1 -> d2 -> d1):
/// the walk fails after MAX_SYMLINK_HOPS with a typed error — never a
/// hang, never an unbounded component list. Legit chains up to the bound
/// keep working; longer chains fail loudly.
#[cfg(unix)]
#[test]
fn symlink_dir_loop_storm_fails_bounded() {
    let (_d, _s, h) = fixture();
    let root = h.root().to_path_buf();
    // Mutual loop: d1 -> d2 -> d1.
    symlink(Path::new("d2"), root.join("d1")).unwrap();
    symlink(Path::new("d1"), root.join("d2")).unwrap();
    let r = h.resolve_fd(Path::new("d1/x.txt"));
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(err.message.contains("hop bound"), "{err:?}");
    // Legit 8-link chain t0->t1->..->t7->r8: resolves (8 hops = bound).
    for i in 0..9 {
        fs::create_dir_all(root.join(format!("r{i}"))).unwrap();
        fs::write(root.join(format!("r{i}/leaf.txt")), b"leaf").unwrap();
    }
    for i in 0..7 {
        symlink(
            Path::new(&format!("t{}", i + 1)),
            root.join(format!("t{i}")),
        )
        .unwrap();
    }
    symlink(Path::new("r8"), root.join("t7")).unwrap();
    let fd = h.resolve_fd(Path::new("t0/leaf.txt")).unwrap();
    drop(fd);
    // Extend to a 9-link chain t0->..->t8->r8: denied at the bound.
    fs::remove_file(root.join("t7")).unwrap();
    symlink(Path::new("t8"), root.join("t7")).unwrap();
    symlink(Path::new("r8"), root.join("t8")).unwrap();
    let r = h.resolve_fd(Path::new("t0/leaf.txt"));
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(err.message.contains("hop bound"), "{err:?}");
}

/// (c) Hostile `rel` with `..` components (or absolute escapes) is
/// denied BEFORE any component is opened: the walk seam would panic if
/// it ever fired.
#[cfg(unix)]
#[test]
fn hostile_parent_components_denied_before_any_open() {
    let _seam = SeamTest::new();
    let (_d, _s, h) = fixture();
    fs::write(h.root().join("f.txt"), b"x").unwrap();
    let me = std::thread::current().id();
    platform::install_walk_seam(Box::new(move |_comp: &OsStr| {
        if std::thread::current().id() == me {
            panic!("no component of a hostile rel may ever be opened");
        }
    }));
    for evil in [
        "../f.txt",
        "a/../../f.txt",
        "..",
        "a/..",
        "/etc/passwd",
        "/",
    ] {
        let r = h.resolve_fd(Path::new(evil));
        let err = r.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission, "{evil}: {err:?}");
    }
    // An absolute path INSIDE the root is fine (and is stripped, then
    // walked) — the seam fires for it, so clear first.
    platform::clear_walk_seam();
    let fd = h.resolve_fd(&h.root().join("f.txt")).unwrap();
    let mut f = fs::File::from(fd);
    use std::io::Read;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"x");
    // Empty/`.` resolve to the root directory itself.
    let fd = h.resolve_fd(Path::new(".")).unwrap();
    assert!(fs::File::from(fd).metadata().unwrap().is_dir());
}

/// (d) A deep relative path (100 components) walks component-by-component
/// and succeeds.
#[cfg(unix)]
#[test]
fn deep_path_of_100_components_walks() {
    let (_d, _s, h) = fixture();
    let root = h.root().to_path_buf();
    let mut dir = root.clone();
    let mut rel = PathBuf::new();
    for i in 0..99 {
        let name = format!("d{i:03}");
        dir = dir.join(&name);
        rel.push(&name);
        fs::create_dir(&dir).unwrap();
    }
    fs::write(dir.join("f.bin"), b"DEEP-CONTENT").unwrap();
    rel.push("f.bin");
    assert_eq!(rel.components().count(), 100);
    let d = h.read(&rel, 100).unwrap();
    assert_eq!(d.bytes, b"DEEP-CONTENT");
    let fd = h.resolve_fd(&rel).unwrap();
    drop(fd);
}

/// (c-write) The guarded atomic write re-verifies the parent directory
/// with the fd walk immediately before the rename: a parent swapped for
/// an outside symlink between resolution and rename fails the write —
/// no temp, no rename, no outside file.
#[cfg(unix)]
#[test]
fn write_refuses_when_parent_dir_swapped_outside_before_rename() {
    let _seam = SeamTest::new();
    let (_d, _s, h) = fixture();
    let root = h.root().to_path_buf();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/f.txt"), b"BASE-CONTENT").unwrap();
    let base_hash = h
        .read(Path::new("sub/f.txt"), 100)
        .unwrap()
        .full_hash()
        .expect("small file read whole");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("marker"), b"M").unwrap();
    // Seam fires when the guard's parent walk is about to open "sub"
    // (immediately before the rename, after temp write + CAS recheck).
    let out = outside.path().to_path_buf();
    swap_seam(&root, "sub", move |root| {
        fs::rename(root.join("sub"), root.join("sub-moved")).unwrap();
        symlink(&out, root.join("sub")).unwrap();
    });
    let r = h.write_atomic_cas(Path::new("sub/f.txt"), base_hash, b"EDITED");
    let err = r.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    // Nothing landed outside: the outside dir holds only its own marker.
    let names: Vec<String> = fs::read_dir(outside.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["marker"],
        "temp/rename leaked outside: {names:?}"
    );
    // The moved original is untouched.
    fs::remove_file(root.join("sub")).unwrap();
    fs::rename(root.join("sub-moved"), root.join("sub")).unwrap();
    assert_eq!(fs::read(root.join("sub/f.txt")).unwrap(), b"BASE-CONTENT");
    // After restoring, the same write succeeds (guard is not in the way).
    let h2 = h
        .write_atomic_cas(Path::new("sub/f.txt"), base_hash, b"EDITED")
        .unwrap();
    let after = h.read(Path::new("sub/f.txt"), 100).unwrap();
    assert_eq!(after.full_hash(), Some(h2));
    assert_eq!(after.bytes, b"EDITED");
}

/// P1-7: a root beyond the preflight entry bound degrades WITHOUT the
/// singleton worker, while an ordinary root still attaches a real
/// watcher — one pathological workspace cannot starve watcher service
/// for every later workspace.
#[test]
fn pathological_root_degrades_but_an_ordinary_root_still_gets_a_watcher() {
    let _guard = lock_watch_tests();
    let workers_before = crate::workspace_service::watch_registration_worker_spawns();
    let pathological = tempfile::tempdir().unwrap();
    for index in 0..200 {
        std::fs::create_dir_all(pathological.path().join(format!("d{index:03}"))).unwrap();
    }
    let service = WorkspaceFileService::with_watch_preflight_entries(50);
    let big = service
        .open(WorkspaceId::new(201), pathological.path().to_path_buf())
        .unwrap();
    assert!(
        !big.watcher_attached(),
        "a root beyond the preflight bound must degrade, never wedge the worker"
    );
    // The degraded open must not have spawned a worker generation —
    // asserted around THAT open so the test is not order-dependent.
    assert_eq!(
        crate::workspace_service::watch_registration_worker_spawns(),
        workers_before,
        "the degraded path must not spawn a worker"
    );
    let ordinary = tempfile::tempdir().unwrap();
    std::fs::write(ordinary.path().join("keep.txt"), b"x").unwrap();
    let before_ordinary = crate::workspace_service::watch_registration_worker_spawns();
    let small = service
        .open(WorkspaceId::new(202), ordinary.path().to_path_buf())
        .unwrap();
    assert!(
        small.watcher_attached(),
        "an ordinary workspace must still obtain a REAL watcher"
    );
    // The ordinary open reuses the running worker or starts at most ONE
    // new generation; it never leaks more.
    assert!(
        crate::workspace_service::watch_registration_worker_spawns() <= before_ordinary + 1,
        "an ordinary workspace must reuse or start exactly one worker generation"
    );
}

/// P0: rooted directory provisioning must refuse symlinked components
/// (final, intermediate, the audit's hostile `.faktor-derived` link) and
/// a swap-to-symlink race, and must NEVER create anything outside the
/// workspace through workspace-controlled path text.
#[cfg(unix)]
#[test]
fn create_dir_all_no_follow_refuses_symlink_components_and_never_mkdirs_outside() {
    let _seam = SeamTest::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let ws = WorkspaceHandle::open_scoped(WorkspaceId::new(301), root.clone()).unwrap();

    // The ordinary case still creates nested directories.
    ws.create_dir_all_no_follow(Path::new("a/b/c")).unwrap();
    assert!(root.join("a/b/c").is_dir());

    // Final-component symlink: refused, outside untouched.
    std::os::unix::fs::symlink(&outside, root.join("final")).unwrap();
    let err = ws.create_dir_all_no_follow(Path::new("final")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");

    // Intermediate-component symlink: refused.
    std::os::unix::fs::symlink(&outside, root.join("hop")).unwrap();
    let err = ws
        .create_dir_all_no_follow(Path::new("hop/sub"))
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(!outside.join("sub").exists());

    // The audit's exact hostile shape: `.faktor-derived` -> outside.
    std::os::unix::fs::symlink(&outside, root.join(".faktor-derived")).unwrap();
    let err = ws
        .create_dir_all_no_follow(Path::new(".faktor-derived/build-scratch"))
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(
        !outside.join("build-scratch").exists(),
        "the daemon must never mkdir outside the workspace through workspace path text"
    );

    // Deterministic race: the component is swapped for an outside symlink
    // BETWEEN walk steps; the no-follow open still refuses it.
    std::fs::create_dir_all(root.join("race")).unwrap();
    let outside2 = dir.path().join("outside2");
    std::fs::create_dir_all(&outside2).unwrap();
    let root_for_seam = root.clone();
    let outside_for_seam = outside2.clone();
    swap_seam(&root_for_seam, "race", move |root| {
        let real = root.join("race-real");
        std::fs::rename(root.join("race"), &real).unwrap();
        std::os::unix::fs::symlink(&outside_for_seam, root.join("race")).unwrap();
    });
    let err = ws
        .create_dir_all_no_follow(Path::new("race/build"))
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission, "{err:?}");
    assert!(!outside2.join("build").exists());
}

/// P1-10: one wedged registration must not starve watcher service for
/// later workspaces: after the overdue bound a FRESH worker generation
/// takes over (bounded to MAX_WATCH_WORKERS), and growth stops at the cap.
#[test]
fn a_wedged_registration_never_starves_later_workspaces() {
    // The shared watcher-test lock, not a private one: the seams below
    // are PROCESS-global, so every registration-driving test must be
    // excluded for the whole wedge window or a concurrent registration
    // steals the bounded replacement budget this test counts.
    let _guard = lock_watch_tests();
    // Seam cleanup runs via Drop so even a failing assertion can never
    // leak the wedge/overdue overrides into later watcher tests.
    struct ResetSeams;
    impl Drop for ResetSeams {
        fn drop(&mut self) {
            crate::workspace_service::wedge_watch_registration_for_test(None);
            crate::workspace_service::set_watch_worker_overdue_for_test(0);
            crate::workspace_service::reset_watch_worker_for_test();
        }
    }
    let _reset = ResetSeams;
    crate::workspace_service::set_watch_worker_overdue_for_test(1);
    let wedged = tempfile::tempdir().unwrap();
    crate::workspace_service::wedge_watch_registration_for_test(Some(
        wedged.path().canonicalize().unwrap(),
    ));
    // Workspace A wedges: the registration times out and degrades.
    let service_a = WorkspaceFileService::with_watch_registration_deadline(
        std::time::Duration::from_millis(200),
    );
    let a = service_a
        .open(WorkspaceId::new(301), wedged.path().to_path_buf())
        .unwrap();
    assert!(!a.watcher_attached(), "the wedged root degrades itself");
    // Workspace B must still receive a REAL watcher via a fresh worker.
    let ordinary = tempfile::tempdir().unwrap();
    std::fs::write(ordinary.path().join("keep.txt"), b"x").unwrap();
    // A generous bound: the property under test is that B is never
    // starved by A's wedge, not that FSEvents attaches within a tight
    // wall clock. On a loaded host the OS registration itself can take
    // seconds; a starvation bug would still time out here.
    let service_b =
        WorkspaceFileService::with_watch_registration_deadline(std::time::Duration::from_secs(20));
    let b = service_b
        .open(WorkspaceId::new(302), ordinary.path().to_path_buf())
        .unwrap();
    assert!(
        b.watcher_attached(),
        "an ordinary workspace must keep succeeding behind a wedged registration"
    );
    // A second wedge consumes the last allowed worker; further roots
    // degrade WITHOUT growing the thread count past the cap.
    let wedged2 = tempfile::tempdir().unwrap();
    crate::workspace_service::wedge_watch_registration_for_test(Some(
        wedged2.path().canonicalize().unwrap(),
    ));
    let service_c = WorkspaceFileService::with_watch_registration_deadline(
        std::time::Duration::from_millis(200),
    );
    let c = service_c
        .open(WorkspaceId::new(303), wedged2.path().to_path_buf())
        .unwrap();
    assert!(!c.watcher_attached());
    let spawns_at_cap = crate::workspace_service::watch_registration_worker_spawns();
    let ordinary2 = tempfile::tempdir().unwrap();
    let service_d = WorkspaceFileService::with_watch_registration_deadline(
        std::time::Duration::from_millis(200),
    );
    let d = service_d
        .open(WorkspaceId::new(304), ordinary2.path().to_path_buf())
        .unwrap();
    assert!(
        !d.watcher_attached(),
        "at the worker cap, registrations degrade"
    );
    assert_eq!(
        crate::workspace_service::watch_registration_worker_spawns(),
        spawns_at_cap,
        "the worker count must never grow past the cap"
    );
}

/// P2-FS: the anchored root identity proves the pathname the watcher
/// backend re-resolves still names the preflighted directory; a root
/// swapped for a different directory is detected instead of silently
/// watched.
#[cfg(unix)]
#[test]
fn anchored_root_identity_detects_a_swapped_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), b"a").unwrap();
    let rooted = RootedDir::open(&root).unwrap();
    let anchored = rooted.anchored_identity().expect("unix identity");
    assert_eq!(
        anchored,
        crate::rooted::path_identity_no_follow(&root).unwrap()
    );
    // The path is swapped for a DIFFERENT directory: the anchored fd
    // still names the original, so the comparison refuses.
    std::fs::rename(&root, dir.path().join("root-old")).unwrap();
    std::fs::create_dir(&root).unwrap();
    assert_ne!(
        anchored,
        crate::rooted::path_identity_no_follow(&root).unwrap(),
        "the swapped path must not compare equal to the anchored root"
    );
}

// ------------------------------------- PERF-10 shadow-turn census probes

/// Live OS threads of this process when the platform exposes them
/// (Linux `/proc/self/task`, macOS `ps -M`); `None` elsewhere.
fn live_thread_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    let count = std::fs::read_dir("/proc/self/task").ok().map(|d| d.count());
    #[cfg(target_os = "macos")]
    let count = std::process::Command::new("ps")
        .arg("-M")
        .arg(std::process::id().to_string())
        .output()
        .ok()
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .count()
                .checked_sub(1)
        });
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let count = None;
    count
}

/// Live file descriptors of this process (Linux `/proc/self/fd`,
/// macOS `/dev/fd`); `None` elsewhere. Both listings include the
/// directory handle of the listing itself, so census bounds keep an
/// explicit small slack.
fn live_fd_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    let count = std::fs::read_dir("/proc/self/fd").ok().map(|d| d.count());
    #[cfg(target_os = "macos")]
    let count = std::fs::read_dir("/dev/fd").ok().map(|d| d.count());
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let count = None;
    count
}

/// Bounded settle for OS-resource growth assertions: parallel tests in
/// this binary can transiently overlap a census; a real per-turn leak
/// is hundreds of threads/fds and can never settle into the slack.
fn settled_count(read: fn() -> Option<usize>, before: usize, slack: usize) -> Option<usize> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut after = read().unwrap_or(before);
    while after > before + slack && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        after = read().unwrap_or(after);
    }
    Some(after)
}

/// PERF-10 fast tier: shadowed-turn-style open/close churn with the
/// REAL watcher backend (the degraded preflight-0 path used by the
/// registry-bound tests never exercises watcher attach/drop). No
/// wall-clock deadlines and no ms-scale reliance: the assertions are
/// the deterministic registry counters, the instrumented eviction
/// probe, and handle usability.
#[test]
fn real_watcher_shadow_churn_keeps_registry_and_eviction_bounded() {
    let _guard = lock_watch_tests();
    let dir = tempfile::tempdir().unwrap();
    let service =
        WorkspaceFileService::with_watch_registration_deadline(TEST_WATCH_REGISTRATION_DEADLINE);
    let id = WorkspaceId::new(21);
    // Warm the singleton registration worker BEFORE counting spawns:
    // like `watcher_registration_uses_exactly_one_worker_thread`, the
    // counted window proves REUSE — no churn turn may spawn another.
    {
        let warm = dir.path().join("warm");
        std::fs::create_dir_all(&warm).unwrap();
        drop(service.open(id, warm).unwrap());
        service.close(id);
    }
    let spawns_before = crate::workspace_service::watch_registration_worker_spawns();

    let durable_root = dir.path().join("durable");
    std::fs::create_dir_all(&durable_root).unwrap();
    let durable = service.open(id, durable_root.clone()).unwrap();
    assert!(
        durable.watcher_attached(),
        "the durable root must register a REAL watcher"
    );
    for i in 0..(crate::workspace_service::MAX_CACHED_ROOTS * 2) {
        let root = dir.path().join(format!("shadow-{i:03}"));
        std::fs::create_dir_all(&root).unwrap();
        let handle = service.open(id, root).unwrap();
        assert_eq!(handle.workspace_id(), id);
    }
    assert!(
        service.open_count() <= crate::workspace_service::MAX_CACHED_ROOTS,
        "real-watcher shadow churn must not grow the registry: {}",
        service.open_count()
    );
    assert_eq!(
        crate::workspace_service::watch_registration_worker_spawns(),
        spawns_before,
        "shadow churn must reuse the singleton registration worker"
    );
    // The held durable root survived every eviction and keeps its
    // watcher across 2x the cache bound of REAL-watcher shadow churn.
    let canonical = durable_root.canonicalize().unwrap();
    assert!(
        service
            .cached_roots()
            .iter()
            .any(|(cached_id, root)| *cached_id == id && *root == canonical),
        "the held durable root must stay cached: {:?}",
        service.cached_roots()
    );
    assert!(durable.watcher_attached());
    std::fs::write(canonical.join("still-hot.txt"), b"ok").unwrap();
    assert!(durable
        .read_default(std::path::Path::new("still-hot.txt"))
        .is_ok());

    // Deterministic eviction-cost counter (the CI-stable stand-in for
    // wall time): at capacity, each insert examines the OLDEST index
    // entry, not the whole map — with real watchers attached too.
    crate::workspace_service::EVICTION_CANDIDATES.with(|count| count.set(0));
    const ROUNDS: usize = 32;
    for i in 0..ROUNDS {
        let root = dir.path().join(format!("probe-{i:03}"));
        std::fs::create_dir_all(&root).unwrap();
        drop(service.open(id, root).unwrap());
    }
    let probes = crate::workspace_service::EVICTION_CANDIDATES.with(|count| count.get());
    assert!(
        probes <= ROUNDS * 2,
        "real-watcher eviction scanned {probes} candidates over {ROUNDS} inserts (ordered index should probe ~1)"
    );
    drop(durable);
    service.close(id);
}

/// PERF-10 fault tier: 1,000 shadowed-turn-style root open/close
/// cycles on the REAL watcher backend, with an OS thread/descriptor
/// census. The historical defect accumulated >500 FSEvents threads at
/// 1,000 turns; the census must show a PLATEAU after the cache fills
/// (never monotonic growth) and a return to baseline after close.
/// Manual: `cargo test -p faktor-fs -- --ignored --nocapture`.
#[test]
#[ignore = "[fault] 1000 real-watcher shadow turns with OS thread/fd census; run with --ignored --nocapture"]
fn fault_probe_1000_shadow_turns_with_real_watchers_census_os_resources() {
    let _guard = lock_watch_tests();
    let dir = tempfile::tempdir().unwrap();
    let service =
        WorkspaceFileService::with_watch_registration_deadline(TEST_WATCH_REGISTRATION_DEADLINE);
    let id = WorkspaceId::new(22);
    // Warm-up: the singleton registration worker and any pool threads
    // exist BEFORE the baseline, so the census measures churn only.
    {
        let warm = dir.path().join("warm");
        std::fs::create_dir_all(&warm).unwrap();
        drop(service.open(id, warm).unwrap());
        service.close(id);
    }
    let spawns_before = crate::workspace_service::watch_registration_worker_spawns();
    let threads_baseline = live_thread_count();
    let fds_baseline = live_fd_count();

    let durable_root = dir.path().join("durable");
    std::fs::create_dir_all(&durable_root).unwrap();
    let durable = service.open(id, durable_root.clone()).unwrap();
    assert!(
        durable.watcher_attached(),
        "the durable root must register a REAL watcher"
    );
    crate::workspace_service::EVICTION_CANDIDATES.with(|count| count.set(0));

    const CYCLES: usize = 1_000;
    let mut threads_early = None;
    for i in 0..CYCLES {
        let root = dir.path().join(format!("shadow-{i:04}"));
        std::fs::create_dir_all(&root).unwrap();
        let handle = service.open(id, root).unwrap();
        assert_eq!(handle.workspace_id(), id);
        // The cache is full long before cycle 300; a census there is
        // the plateau the 1,000-turn census must never exceed.
        if i == 299 {
            threads_early = live_thread_count();
        }
    }
    let threads_peak = live_thread_count();
    let fds_peak = live_fd_count();
    eprintln!(
        "[fault] census baseline threads={threads_baseline:?} fds={fds_baseline:?}; \
         plateau@300 threads={threads_early:?}; peak@{CYCLES} threads={threads_peak:?} fds={fds_peak:?}"
    );
    if let (Some(early), Some(peak)) = (threads_early, threads_peak) {
        assert!(
            peak <= early + 16,
            "OS threads grew monotonically with turn count: {early} at cycle 300 vs {peak} at cycle {CYCLES} \
             (the historical defect accumulated >500 FSEvents threads)"
        );
    }
    if let (Some(base), Some(peak)) = (fds_baseline, fds_peak) {
        assert!(
            peak <= base + 6 * crate::workspace_service::MAX_CACHED_ROOTS + 64,
            "file descriptors grew with turn count: baseline {base} vs peak {peak} at cycle {CYCLES}"
        );
    }
    assert!(
        service.open_count() <= crate::workspace_service::MAX_CACHED_ROOTS,
        "the registry must stay bounded across 1000 real-watcher turns: {}",
        service.open_count()
    );
    assert_eq!(
        crate::workspace_service::watch_registration_worker_spawns(),
        spawns_before,
        "1000 shadow turns must reuse the singleton registration worker"
    );
    let probes = crate::workspace_service::EVICTION_CANDIDATES.with(|count| count.get());
    assert!(
        probes <= CYCLES * 2,
        "eviction examined {probes} candidates over {CYCLES} churn inserts (ordered index should probe ~1 each)"
    );

    // Eviction only removed idle roots: the HELD durable root is still
    // cached, still watched, and still usable after 1000 shadow turns.
    let canonical = durable_root.canonicalize().unwrap();
    assert!(
        service
            .cached_roots()
            .iter()
            .any(|(cached_id, root)| *cached_id == id && *root == canonical),
        "the held durable root must survive 1000 shadow turns: {:?}",
        service.cached_roots()
    );
    assert!(durable.watcher_attached());
    std::fs::write(canonical.join("still-hot.txt"), b"ok").unwrap();
    assert!(durable
        .read_default(std::path::Path::new("still-hot.txt"))
        .is_ok());

    // Release everything: the process must RETURN to (near) baseline —
    // watcher teardown is bounded, so a leaked per-turn thread/fd can
    // never hide inside the settle window.
    drop(durable);
    service.close(id);
    let threads_after = settled_count(live_thread_count, threads_baseline.unwrap_or(0), 8);
    let fds_after = settled_count(live_fd_count, fds_baseline.unwrap_or(0), 32);
    eprintln!(
        "[fault] after close: threads={threads_after:?} (baseline {threads_baseline:?}), \
         fds={fds_after:?} (baseline {fds_baseline:?})"
    );
    if let (Some(base), Some(after)) = (threads_baseline, threads_after) {
        assert!(
            after <= base + 8,
            "OS threads did not return to baseline after close: baseline {base} vs {after}"
        );
    }
    if let (Some(base), Some(after)) = (fds_baseline, fds_after) {
        assert!(
            after <= base + 32,
            "file descriptors did not return to baseline after close: baseline {base} vs {after}"
        );
    }
}
