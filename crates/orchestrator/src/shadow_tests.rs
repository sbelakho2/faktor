//! Adversarial tests of the shadow mutation-root service (P0-48/49).
//!
//! The tests break the invariants the feature exists for: a STABLE begin
//! copy (owner_before == owner_after == copied) with bounded retries and a
//! typed refusal for a permanently drifting checkout, a durable immutable
//! run base + generation-anchored change sets, user-checkout isolation for
//! the whole shadow lifetime, staging/deletion semantics, daemon-shutdown
//! removal, symlink escapes, oversize refusals, `.git` plumbing never
//! copied, and deterministic reopen recovery. Landing is deliberately NOT
//! part of this service any more: the ONE commitment engine is the
//! executor's transactional integration pipeline, covered end-to-end in
//! `task_executor_tests.rs` (rollback, crash recovery, completion binding).
//! The "shadowed drive writes" below are staged as DIRECT writes into the
//! shadow root — the exact operation a shadow-aware tool context performs.

use std::fs;
use std::path::{Path, PathBuf};

use faktor_core::id::SessionId;
use faktor_session::{SessionManager, ShadowRow, ShadowRowState};

use super::*;
use crate::runtime::shadow::{
    ShadowCopyDrift, ShadowCopyLimits, ShadowRoots, SHADOW_MAX_BASE_ENTRIES, SHADOW_MAX_COPY_BYTES,
};

struct Fix {
    _dir: tempfile::TempDir,
    manager: Arc<SessionManager>,
    shadows: Arc<ShadowRoots>,
    user: PathBuf,
    session: SessionId,
}

fn seed_user(root: &Path) {
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), b"alpha").unwrap();
    fs::write(root.join("sub/b.txt"), b"beta").unwrap();
    fs::write(root.join("c.txt"), b"gamma").unwrap();
}

fn open_fix(limits: ShadowCopyLimits) -> Fix {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    fs::create_dir_all(&user).unwrap();
    seed_user(&user);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace(user.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws, "shadow-service", "fake", "m")
        .unwrap()
        .id();
    let shadows = ShadowRoots::new_with_limits(manager.clone(), dir.path().join("shadows"), limits);
    Fix {
        _dir: dir,
        manager,
        shadows,
        user,
        session,
    }
}

fn default_limits() -> ShadowCopyLimits {
    ShadowCopyLimits {
        max_entries: SHADOW_MAX_BASE_ENTRIES,
        max_total_bytes: SHADOW_MAX_COPY_BYTES,
    }
}

fn user_bytes(fix: &Fix, rel: &str) -> Vec<u8> {
    fs::read(fix.user.join(rel)).unwrap()
}

fn shadow_row_of(fix: &Fix) -> ShadowRow {
    fix.manager
        .shadow_row(fix.session)
        .unwrap()
        .expect("a shadow row exists")
}

fn run_base_of(fix: &Fix) -> faktor_session::ledger::RunBaseRecord {
    let handle = fix.manager.get_session(fix.session).unwrap().unwrap();
    handle
        .ledger_run_base_get(&shadow_row_of(fix).shadow_id)
        .unwrap()
        .expect("a durable run base exists")
}

fn owner_digest(fix: &Fix) -> String {
    crate::runtime::task_executor::root_manifest_digest(&fix.user).unwrap()
}

/// The "shadowed drive" write: stage `bytes` at `rel` inside the shadow
/// root (the consumers resolve this root via `SessionManager::active_root`).
fn drive_write(fix: &Fix, rel: &str, bytes: &[u8]) {
    let row = shadow_row_of(fix);
    let dst = PathBuf::from(&row.root).join(rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(dst, bytes).unwrap();
}

fn assert_no_shadow(fix: &Fix) {
    assert!(fix.manager.shadow_row(fix.session).unwrap().is_none());
}

// ---------------------------------------------------------------- begin

#[test]
fn begin_is_a_stable_copy_and_records_the_run_base() {
    // The begin contract: owner_before == owner_after == copied, the user
    // checkout is byte-identical, the shadow holds the base content, and the
    // immutable run base + base manifest + active row are durable.
    let fix = open_fix(default_limits());
    let before = owner_digest(&fix);
    let shadow = fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    let after = owner_digest(&fix);
    let copied = faktor_fs::tree_manifest::tree_manifest_digest(
        &shadow.root,
        faktor_fs::tree_manifest::MAX_TREE_MANIFEST_ENTRIES,
    )
    .unwrap();
    assert_eq!(before, after, "owner stable through the copy");
    assert_eq!(copied, before, "the copy is byte-identical to the owner");
    let row = shadow_row_of(&fix);
    assert_eq!(row.state, ShadowRowState::Active);
    assert_eq!(row.base_root, shadow.base_root.to_string_lossy());
    assert_eq!(row.root, shadow.root.to_string_lossy());
    assert!(PathBuf::from(&row.root).is_dir());
    // Isolation: the user checkout is untouched by the copy.
    assert_eq!(user_bytes(&fix, "a.txt"), b"alpha");
    assert_eq!(user_bytes(&fix, "sub/b.txt"), b"beta");
    // The run base is durable, immutable and keyed by the shadow id with the
    // exact copied digest.
    let rb = run_base_of(&fix);
    assert_eq!(rb.run_id, shadow.shadow_id);
    assert_eq!(rb.snapshot_hash, copied);
    assert_eq!(rb.root, shadow.root.to_string_lossy());
    assert!(rb.manifest_digest.len() == 64, "{}", rb.manifest_digest);
    // The base manifest is durable and readable.
    assert_eq!(
        fix.shadows.base_manifest(&shadow).unwrap().len(),
        3,
        "three base files anchored"
    );
}

#[test]
fn stable_copy_retries_once_then_accepts() {
    // The owner drifts between the before-digest and the copy on the FIRST
    // attempt; the bounded retry re-copies the stabilized tree and accepts
    // exactly that generation (drift included).
    let fix = open_fix(default_limits());
    fix.shadows.arm_copy_drift(ShadowCopyDrift::Once);
    let shadow = fix
        .shadows
        .begin_shadow(fix.session, &fix.user)
        .expect("a once-drifting owner stabilizes on retry");
    // The drift happened in the OWNER (the seam mutates it) and the accepted
    // copy is the stable post-drift tree.
    let copied = crate::runtime::task_executor::root_manifest_digest(&shadow.root).unwrap();
    assert_eq!(copied, owner_digest(&fix));
    assert_eq!(
        fs::read(shadow.root.join("shadow-copy-drift.txt")).unwrap(),
        b"drift on attempt 1"
    );
    assert_eq!(run_base_of(&fix).snapshot_hash, copied);
}

#[test]
fn stable_copy_refuses_a_permanently_drifting_owner_typed() {
    // Every attempt drifts: the copy can never stabilize, so begin refuses
    // with the typed WorkspaceDrift and leaves NO directory, NO manifest and
    // NO row behind.
    let fix = open_fix(default_limits());
    fix.shadows.arm_copy_drift(ShadowCopyDrift::Every);
    let err = fix
        .shadows
        .begin_shadow(fix.session, &fix.user)
        .expect_err("a permanently drifting owner can never be shadowed");
    assert!(
        matches!(err, crate::runtime::ExecError::WorkspaceDrift(_)),
        "{err}"
    );
    assert!(err.to_string().contains("did not stabilize"), "{err}");
    assert_no_shadow(&fix);
    let session_dir = fix
        ._dir
        .path()
        .join("shadows")
        .join(fix.session.raw().to_string());
    if session_dir.exists() {
        assert_eq!(
            fs::read_dir(&session_dir).unwrap().count(),
            0,
            "no partial shadow directory survives a refused begin"
        );
    }
}

#[test]
fn poisoned_copy_seam_recovers_and_begin_still_succeeds() {
    // The stable-copy seam is plain None/Some state: a panicking holder can
    // poison its mutex but cannot corrupt an invariant, so the next begin
    // must recover the guard and proceed — a test-seam poison must never
    // abort the run with a panic cascade.
    let fix = open_fix(default_limits());
    let seam = Arc::clone(&fix.shadows.copy_seam);
    let poisoner = std::thread::spawn(move || {
        let _guard = seam.lock().unwrap();
        panic!("poison the copy seam");
    });
    assert!(poisoner.join().is_err());
    assert!(fix.shadows.copy_seam.is_poisoned());

    // Arming after the poison recovers the guard…
    fix.shadows.arm_copy_drift(ShadowCopyDrift::Once);
    // …and the seam still fires: the once-drift is observed and retried.
    let shadow = fix
        .shadows
        .begin_shadow(fix.session, &fix.user)
        .expect("a poisoned seam recovers and begin proceeds");
    assert_eq!(
        fs::read(shadow.root.join("shadow-copy-drift.txt")).unwrap(),
        b"drift on attempt 1"
    );
    assert_eq!(owner_digest(&fix), run_base_of(&fix).snapshot_hash);
}

// ---------------------------------------------------------------- staging

#[test]
fn change_set_carries_the_generation_snapshots() {
    // (P1) Every staged set binds the immutable run base, the child start
    // map and the current child map; re-staging an unchanged tree is
    // idempotent, a later write moves only the final snapshot.
    let fix = open_fix(default_limits());
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    let rb = run_base_of(&fix);
    drive_write(&fix, "a.txt", b"agent alpha v2");
    let cs1 = fix.shadows.present_change_set(fix.session).unwrap();
    assert_eq!(
        cs1.run_base_snapshot.as_deref(),
        Some(rb.snapshot_hash.as_str())
    );
    let start = cs1
        .child_start_snapshot
        .clone()
        .expect("child start snapshot populated");
    let final1 = cs1
        .final_child_snapshot
        .clone()
        .expect("final child snapshot populated");
    assert_eq!(start.len(), 64);
    assert_eq!(final1.len(), 64);
    assert_ne!(start, final1, "the drive changed the child map");
    assert_eq!(cs1.files.len(), 1);
    // Idempotent for an unchanged shadow: same id, same anchors.
    let cs2 = fix.shadows.present_change_set(fix.session).unwrap();
    assert_eq!(cs1.id(), cs2.id());
    assert_eq!(cs2.final_child_snapshot, Some(final1.clone()));
    // A further write moves the final snapshot (and the id) but never the
    // run base.
    drive_write(&fix, "b.txt", b"agent beta v2");
    let cs3 = fix.shadows.stage_change_set(fix.session).unwrap();
    assert_eq!(
        cs3.run_base_snapshot.as_deref(),
        Some(rb.snapshot_hash.as_str()),
        "the run base is immutable across stagings"
    );
    assert_eq!(cs3.child_start_snapshot, Some(start));
    assert_ne!(cs3.final_child_snapshot, Some(final1));
    assert_ne!(cs3.id(), cs1.id());
    // The user checkout never moved.
    assert_eq!(user_bytes(&fix, "a.txt"), b"alpha");
    assert!(!fix.user.join("b.txt").exists());
}

#[test]
fn stale_change_set_is_never_presented() {
    // A stored change set whose run-base binding no longer matches the
    // shadow's durable run base is STALE: presentation re-stages against the
    // current generation instead of serving the stale set.
    let fix = open_fix(default_limits());
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    drive_write(&fix, "a.txt", b"agent alpha v2");
    let cs1 = fix.shadows.present_change_set(fix.session).unwrap();
    // Move the durable run base (a different generation's digest).
    let handle = fix.manager.get_session(fix.session).unwrap().unwrap();
    let mut rb = run_base_of(&fix);
    rb.snapshot_hash = "f".repeat(64);
    handle.ledger_run_base_set(&rb).unwrap();
    let cs2 = fix.shadows.present_change_set(fix.session).unwrap();
    assert_ne!(cs1.id(), cs2.id(), "the stale set is not served");
    assert_eq!(
        cs2.run_base_snapshot.as_deref(),
        Some("f".repeat(64).as_str())
    );
}

#[test]
fn legacy_shadow_without_run_base_refuses_staging() {
    // A manually planted legacy row (or a crashed begin that never recorded
    // the run base) can never stage an unanchored change set: the refusal is
    // typed and names the recovery (discard + begin again).
    let fix = open_fix(default_limits());
    let dir = fix
        ._dir
        .path()
        .join("shadows")
        .join(fix.session.raw().to_string())
        .join("sh-legacy");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), b"legacy").unwrap();
    fix.manager
        .put_shadow_row(
            fix.session,
            &ShadowRow {
                session_id: fix.session.raw(),
                shadow_id: "sh-legacy".into(),
                base_root: fix.user.to_string_lossy().into_owned(),
                root: dir.to_string_lossy().into_owned(),
                state: ShadowRowState::Active,
                base_entries: 1,
                base_bytes: 6,
                created_ms: 1,
            },
        )
        .unwrap();
    let err = fix.shadows.stage_change_set(fix.session).unwrap_err();
    assert!(err.to_string().contains("no durable run base"), "{err}");
    let err = fix.shadows.present_change_set(fix.session).unwrap_err();
    assert!(err.to_string().contains("no durable run base"), "{err}");
    // The checkout is untouched by the refusal.
    assert_eq!(user_bytes(&fix, "a.txt"), b"alpha");
}

#[test]
fn deletions_and_untouched_drift_stage_deterministically() {
    // A shadowed deletion stages as a base-anchored removal; an untouched
    // user drift never enters the change set (the integration decides it at
    // land time, never the staging).
    let fix = open_fix(default_limits());
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    let dir = PathBuf::from(&shadow_row_of(&fix).root);
    fs::remove_file(dir.join("sub/b.txt")).unwrap();
    // External drift on a file the shadow never touched.
    fs::write(fix.user.join("c.txt"), b"user drift on c").unwrap();
    let cs = fix.shadows.present_change_set(fix.session).unwrap();
    let paths: Vec<String> = cs
        .files
        .iter()
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(paths, vec!["sub/b.txt".to_string()]);
    let entry = &cs.files[0];
    assert!(entry.child_hash.is_none(), "the deletion has no child hash");
    assert!(entry.base_hash.is_some(), "the deletion is base-anchored");
    // No staging ever writes the checkout.
    assert_eq!(user_bytes(&fix, "c.txt"), b"user drift on c");
}

#[test]
fn no_op_staging_is_empty_and_bounded() {
    let fix = open_fix(default_limits());
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    let cs = fix.shadows.present_change_set(fix.session).unwrap();
    assert!(cs.files.is_empty(), "no writes -> no change set");
    assert_eq!(
        cs.run_base_snapshot.as_deref(),
        Some(run_base_of(&fix).snapshot_hash.as_str())
    );
}

// ---------------------------------------------------------- hostile inputs

#[test]
fn hostile_shadow_paths_and_reuse_refused() {
    // begin_shadow refuses: a base root inside the shadow root, a session
    // with a live shadow, and unknown sessions.
    let fix = open_fix(default_limits());
    let inner = fix._dir.path().join("shadows").join("sneaky");
    fs::create_dir_all(&inner).unwrap();
    let err = fix
        .shadows
        .begin_shadow(fix.session, &inner)
        .expect_err("a shadow can never shadow a shadow");
    assert!(
        err.to_string().contains("inside the daemon shadow root"),
        "{err}"
    );
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    let err = fix
        .shadows
        .begin_shadow(fix.session, &fix.user)
        .expect_err("a live shadow refuses a second begin");
    assert!(err.to_string().contains("live shadow"), "{err}");
    let err = fix
        .shadows
        .begin_shadow(SessionId::new(424_242), &fix.user)
        .expect_err("unknown sessions refuse");
    assert!(
        matches!(err, crate::runtime::ExecError::NotFound(_)),
        "{err}"
    );
    assert!(fix
        .shadows
        .stage_change_set(SessionId::new(424_242))
        .is_err());
}

/// Create a directory link `link -> target` with the strongest artifact the
/// OS allows and return its kind (`"symlink"` or `"junction"`).
///
/// Unix has only symlinks. Windows attempts a real directory symlink first
/// and, when `SeCreateSymbolicLinkPrivilege` is unavailable
/// (`ERROR_PRIVILEGE_NOT_HELD`, Developer Mode off — the usual CI runner
/// state), falls back to an unprivileged directory junction created
/// directly through the Win32 handle API ([`create_junction_unprivileged`],
/// the exact sequence `mklink /J` performs, but with NO child process —
/// child spawning is the supervisor's authority, certified by the
/// spawn-authority scan). The kind actually used is logged, never silent;
/// a failure on both paths is loud, never a skip.
#[cfg(unix)]
fn create_dir_link(link: &Path, target: &Path) -> &'static str {
    std::os::unix::fs::symlink(target, link)
        .unwrap_or_else(|e| panic!("symlink {}: {e}", link.display()));
    eprintln!(
        "[shadow-test] {} -> {}: unix directory symlink",
        link.display(),
        target.display()
    );
    "symlink"
}

#[cfg(windows)]
fn create_dir_link(link: &Path, target: &Path) -> &'static str {
    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => {
            eprintln!(
                "[shadow-test] {} -> {}: directory symlink (SeCreateSymbolicLinkPrivilege available)",
                link.display(),
                target.display()
            );
            "symlink"
        }
        Err(e) if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) => {
            create_junction_unprivileged(link, target).unwrap_or_else(|e| {
                panic!(
                    "junction {} -> {} failed (symlink privilege unavailable, direct \
                     FSCTL_SET_REPARSE_POINT also failed): {e}",
                    link.display(),
                    target.display()
                )
            });
            eprintln!(
                "[shadow-test] {} -> {}: directory junction fallback (SeCreateSymbolicLinkPrivilege unavailable)",
                link.display(),
                target.display()
            );
            "junction"
        }
        Err(e) => panic!("symlink_dir {}: {e}", link.display()),
    }
}

/// Create an unprivileged directory junction (a mount-point reparse point)
/// with the Win32 handle API: `CreateDirectoryW`, then `CreateFileW` with
/// `FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS`, then
/// `DeviceIoControl(FSCTL_SET_REPARSE_POINT)` with a `MOUNT_POINT` reparse
/// buffer holding the absolute `\??\…` substitute name and the display
/// name. Junction creation needs no special privilege; this is the
/// documented in-process equivalent of `mklink /J` with no process start.
#[cfg(windows)]
fn create_junction_unprivileged(link: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    // Junctions resolve through an absolute NT-namespace substitute name
    // (`\??\C:\…`); verbatim `\\?\` and UNC spellings are mapped
    // explicitly because `absolute()` may return either form.
    let absolute = std::path::absolute(target)?;
    let shown = absolute.to_string_lossy();
    let shown = shown.strip_prefix(r"\\?\").unwrap_or(&shown);
    let shown = match shown.strip_prefix("UNC\\") {
        Some(rest) => format!("UNC\\{rest}"),
        None => shown.to_string(),
    };
    let substitute: Vec<u16> = format!(r"\??\{shown}").encode_utf16().collect();
    let print: Vec<u16> = shown.encode_utf16().collect();

    let link_w = wide(link);
    // SAFETY: `link_w` is NUL-terminated; null security attributes are the
    // CreateDirectoryW default.
    if unsafe { CreateDirectoryW(link_w.as_ptr(), std::ptr::null()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `link_w` is NUL-terminated; null security attributes and no
    // template file are the documented defaults.
    let handle = unsafe {
        CreateFileW(
            link_w.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = std::io::Error::last_os_error();
        let _ = fs::remove_dir(link);
        return Err(err);
    }
    // REPARSE_DATA_BUFFER, mount-point form: 8-byte header (tag, data
    // length, reserved) + four USHORT offsets/lengths + UTF-16 substitute
    // and print names (offsets are byte offsets into the name area; no NUL
    // terminators are required).
    let substitute_bytes = substitute.len() * 2;
    let print_bytes = print.len() * 2;
    let data_len = 8 + substitute_bytes + print_bytes;
    let mut buf = vec![0u8; 8 + data_len];
    buf[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buf[4..6].copy_from_slice(&(data_len as u16).to_le_bytes());
    buf[8..10].copy_from_slice(&0u16.to_le_bytes());
    buf[10..12].copy_from_slice(&(substitute_bytes as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&(substitute_bytes as u16).to_le_bytes());
    buf[14..16].copy_from_slice(&(print_bytes as u16).to_le_bytes());
    let mut at = 16usize;
    for unit in substitute.iter().chain(print.iter()) {
        buf[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        at += 2;
    }
    let mut returned = 0u32;
    // SAFETY: `handle` is an open directory handle owned here; `buf` is a
    // fully initialized input buffer; the output buffer is empty/absent and
    // the call is synchronous (null OVERLAPPED).
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            buf.as_ptr().cast(),
            buf.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    let result = if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    };
    // SAFETY: the handle was opened above and is not used afterwards.
    unsafe { CloseHandle(handle) };
    if result.is_err() {
        let _ = fs::remove_dir(link);
    }
    result
}

/// Remove a directory link created by [`create_dir_link`] without touching
/// the linked target.
#[cfg(unix)]
fn remove_dir_link(link: &Path) {
    fs::remove_file(link).unwrap_or_else(|e| panic!("remove symlink {}: {e}", link.display()));
}

#[cfg(windows)]
fn remove_dir_link(link: &Path) {
    // RemoveDirectoryW on a symlink/junction removes the link itself, never
    // the target's content.
    fs::remove_dir(link).unwrap_or_else(|e| panic!("remove link {}: {e}", link.display()));
}

#[test]
fn symlink_escape_from_shadow_copy_rejected() {
    // (f): a checkout whose symlink/junction leaves the tree must refuse the
    // shadow begin loudly — nothing is copied, no row is written.
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"outside secret").unwrap();
    let user = dir.path().join("user");
    fs::create_dir_all(&user).unwrap();
    fs::write(user.join("a.txt"), b"alpha").unwrap();
    fs::create_dir_all(user.join("sub")).unwrap();
    fs::write(user.join("sub/b.txt"), b"beta").unwrap();
    let kind = create_dir_link(&user.join("leak"), &outside);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace(user.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws, "escape", "fake", "m")
        .unwrap()
        .id();
    let shadows = ShadowRoots::new_with_limits(
        manager.clone(),
        dir.path().join("shadows"),
        default_limits(),
    );
    let err = shadows
        .begin_shadow(session, &user)
        .expect_err("escape refused");
    assert!(
        err.to_string().contains("symlink") || err.to_string().contains("escape"),
        "{kind} escape was not refused: {err}"
    );
    assert!(manager.shadow_row(session).unwrap().is_none());
    // Nothing of the copy survives: the daemon-owned shadow area is empty
    // of shadow directories (the session-level dir may exist empty).
    let shadows_area = dir.path().join("shadows");
    if shadows_area.exists() {
        for session_dir in fs::read_dir(&shadows_area).unwrap().flatten() {
            let leftovers: Vec<_> = fs::read_dir(session_dir.path())
                .unwrap()
                .flatten()
                .collect();
            assert!(leftovers.is_empty(), "{:?}", leftovers);
        }
    }
    // Companion: a RELATIVE in-root link is copied LITERALLY (manifest
    // semantics: the copy is the SAME tree; the walk never follows links)
    // and still resolves inside the shadow to the real sub directory. An
    // ABSOLUTE in-root link is refused: a literal copy would point it back
    // at the user checkout, so a daemon-owned shadow can never hold it.
    remove_dir_link(&user.join("leak"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("sub", user.join("alias")).unwrap();
        let shadow = shadows
            .begin_shadow(session, &user)
            .expect("a relative in-root directory link is accepted");
        assert_eq!(
            fs::read(shadow.root.join("alias/b.txt")).unwrap(),
            b"beta",
            "the literal in-root link resolves inside the shadow copy"
        );
        assert_eq!(
            manager.shadow_row(session).unwrap().unwrap().state,
            ShadowRowState::Active
        );
    }
    #[cfg(windows)]
    {
        let kind = create_dir_link(&user.join("alias"), &user.join("sub"));
        let err = shadows
            .begin_shadow(session, &user)
            .expect_err("an absolute in-root link is refused");
        assert!(
            err.to_string().contains("symlink") || err.to_string().contains("escape"),
            "{kind} absolute link was not refused: {err}"
        );
        assert!(manager.shadow_row(session).unwrap().is_none());
    }
}

#[test]
fn oversize_base_refused_before_any_mutation() {
    // (g): a base tree beyond the copy caps is a typed Oversized refusal;
    // nothing is copied, no row exists, the user checkout is untouched.
    let fix = open_fix(ShadowCopyLimits {
        max_entries: 2, // the seed has three files
        max_total_bytes: SHADOW_MAX_COPY_BYTES,
    });
    let err = fix
        .shadows
        .begin_shadow(fix.session, &fix.user)
        .expect_err("entry cap exceeded");
    assert!(
        matches!(err, crate::runtime::ExecError::Oversized(_)),
        "{err}"
    );
    assert_no_shadow(&fix);
    assert_eq!(user_bytes(&fix, "a.txt"), b"alpha");
    // Byte cap too.
    let fix2 = open_fix(ShadowCopyLimits {
        max_entries: SHADOW_MAX_BASE_ENTRIES,
        max_total_bytes: 4, // every seed file is 5 bytes
    });
    let err = fix2
        .shadows
        .begin_shadow(fix2.session, &fix2.user)
        .expect_err("byte cap exceeded");
    assert!(
        matches!(err, crate::runtime::ExecError::Oversized(_)),
        "{err}"
    );
    assert_no_shadow(&fix2);
}

#[test]
#[should_panic(expected = "shadow copy caps must be >= 1")]
fn zero_caps_refused_at_construction() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let _ = ShadowRoots::new_with_limits(
        manager,
        dir.path().join("s"),
        ShadowCopyLimits {
            max_entries: 0,
            max_total_bytes: 1,
        },
    );
}

#[test]
fn git_plumbing_never_copied_git_or_plain_root() {
    // (h): no git API materializes an at-revision tree outside the repo
    // (faktor-git's only creation path adds a worktree INSIDE the user
    // checkout — forbidden), so every root uses the bounded fs copy; the
    // `.git` plumbing of a git root is detected and skipped by the copy.
    let fix = open_fix(default_limits());
    fs::create_dir_all(fix.user.join(".git/objects/aa")).unwrap();
    fs::write(fix.user.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
    fs::write(fix.user.join(".git/objects/aa/bb"), vec![0x7f; 1024 * 1024]).unwrap();
    let shadow = fix
        .shadows
        .begin_shadow(fix.session, &fix.user)
        .expect("the copy skips .git, so the byte cap is not hit");
    let row = shadow_row_of(&fix);
    assert_eq!(row.base_entries, 3, "only the three content files copied");
    assert!(
        !shadow.root.join(".git").exists(),
        "no plumbing in the shadow"
    );
    assert!(fix.user.join(".git/HEAD").exists(), "user .git untouched");
    drive_write(&fix, "a.txt", b"alpha via git-root shadow");
    // Staging after the write yields only content entries (never .git).
    let manifest = fix.shadows.present_change_set(fix.session).unwrap();
    assert!(
        manifest.files.iter().all(|f| !f.path.starts_with(".git")),
        "no .git entry can ever be staged: {:?}",
        manifest.files
    );
    assert_eq!(manifest.files.len(), 1);
    assert!(fix.user.join(".git/HEAD").exists());
    // A non-git root (no .git at all) uses the same fs-copy path.
    let fix2 = open_fix(default_limits());
    fix2.shadows.begin_shadow(fix2.session, &fix2.user).unwrap();
    assert!(PathBuf::from(&shadow_row_of(&fix2).root).is_dir());
}

// ------------------------------------------------------- discard/teardown

#[test]
fn discard_removes_dir_marks_row_and_retires_cleanly() {
    let fix = open_fix(default_limits());
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    let dir = PathBuf::from(&shadow_row_of(&fix).root);
    fix.shadows.discard(fix.session).unwrap();
    assert!(!dir.exists());
    assert_eq!(shadow_row_of(&fix).state, ShadowRowState::Discarded);
    assert_eq!(user_bytes(&fix, "a.txt"), b"alpha", "discard never writes");
    // stage after discard is a typed refusal (the tombstone row exists;
    // only live shadows may stage).
    let err = fix.shadows.stage_change_set(fix.session).unwrap_err();
    assert!(
        err.to_string().contains("only Active/IntegrationBlocked"),
        "{err}"
    );
    // Double discard is idempotent on the directory; the tombstone stays.
    fix.shadows.discard(fix.session).unwrap();
    assert_eq!(shadow_row_of(&fix).state, ShadowRowState::Discarded);
    // A shadow-less session has nothing to discard.
    let err = fix
        .shadows
        .discard(SessionId::new(999_999))
        .expect_err("unknown session");
    assert!(
        matches!(err, crate::runtime::ExecError::NotFound(_)),
        "{err}"
    );
}

#[test]
fn integration_blocked_retains_the_shadow_and_resumes_staging() {
    // The integration-blocked state is LIVE: the directory stays, staging
    // stays available, and the row can be retired by the executor's landing
    // pipeline whenever the drift resolves.
    let fix = open_fix(default_limits());
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    let dir = PathBuf::from(&shadow_row_of(&fix).root);
    drive_write(&fix, "a.txt", b"agent alpha v2");
    fix.shadows.mark_integration_blocked(fix.session).unwrap();
    assert!(dir.is_dir());
    assert_eq!(
        shadow_row_of(&fix).state,
        ShadowRowState::IntegrationBlocked
    );
    let cs = fix.shadows.present_change_set(fix.session).unwrap();
    assert_eq!(cs.files.len(), 1, "staging resumes while blocked");
    assert_eq!(user_bytes(&fix, "a.txt"), b"alpha");
    fix.shadows.mark_integrated(fix.session).unwrap();
    assert!(!dir.exists());
    assert_eq!(shadow_row_of(&fix).state, ShadowRowState::Integrated);
}

#[test]
fn drop_removes_every_live_shadow() {
    // (e): daemon shutdown (the service's Drop) removes every shadow dir
    // and retires the durable rows; the user checkouts are untouched.
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let user_a = dir.path().join("user-a");
    let user_b = dir.path().join("user-b");
    for u in [&user_a, &user_b] {
        fs::create_dir_all(u).unwrap();
        fs::write(u.join("a.txt"), b"alpha").unwrap();
    }
    let shadows = ShadowRoots::new(manager.clone(), dir.path().join("shadows"));
    let ws_a = manager.create_workspace(user_a.to_str().unwrap()).unwrap();
    let ws_b = manager.create_workspace(user_b.to_str().unwrap()).unwrap();
    let s_a = manager.create_session(ws_a, "a", "fake", "m").unwrap().id();
    let s_b = manager.create_session(ws_b, "b", "fake", "m").unwrap().id();
    let row_a = manager.shadow_row(s_a).unwrap();
    assert!(row_a.is_none());
    shadows.begin_shadow(s_a, &user_a).unwrap();
    shadows.begin_shadow(s_b, &user_b).unwrap();
    let dir_a = PathBuf::from(&manager.shadow_row(s_a).unwrap().unwrap().root);
    let dir_b = PathBuf::from(&manager.shadow_row(s_b).unwrap().unwrap().root);
    assert!(dir_a.is_dir() && dir_b.is_dir());
    drop(shadows);
    assert!(
        !dir_a.exists() && !dir_b.exists(),
        "Drop removed both shadows"
    );
    let manager2 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    assert_eq!(
        manager2.shadow_row(s_a).unwrap().unwrap().state,
        ShadowRowState::Discarded,
        "rows retired durably"
    );
    assert_eq!(
        manager2.shadow_row(s_b).unwrap().unwrap().state,
        ShadowRowState::Discarded
    );
    assert_eq!(fs::read(user_a.join("a.txt")).unwrap(), b"alpha");
}

#[test]
fn reconcile_deterministically_settles_crash_residue() {
    let fix = open_fix(default_limits());
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    // Crash residue 1: the shadow dir vanished without a discard.
    let dir = PathBuf::from(&shadow_row_of(&fix).root);
    fs::remove_dir_all(&dir).unwrap();
    let actions = fix.shadows.reconcile().unwrap();
    assert!(
        actions.iter().any(|a| a.contains("directory gone")),
        "{actions:?}"
    );
    assert_eq!(shadow_row_of(&fix).state, ShadowRowState::Discarded);
    // Crash residue 2: a row-less shadow dir (crash between the copy and
    // the row write) is removed.
    fix.shadows.begin_shadow(fix.session, &fix.user).unwrap();
    let session_dir = dir.parent().unwrap();
    let stray = session_dir.join("sh-00000000deadbeef");
    fs::create_dir_all(&stray).unwrap();
    fs::write(stray.join("partial"), b"x").unwrap();
    let actions = fix.shadows.reconcile().unwrap();
    assert!(
        actions.iter().any(|a| a.contains("row-less")),
        "{actions:?}"
    );
    assert!(!stray.exists());
    // A live shadow that survived reconcile stays live (its dir is the
    // CURRENT row's dir — a fresh generation was begun above).
    let row = shadow_row_of(&fix);
    assert_eq!(row.state, ShadowRowState::Active);
    assert!(PathBuf::from(&row.root).is_dir());
}

#[test]
fn shadow_survives_reopen_with_its_run_base() {
    // (d): a crashed daemon's shadow (row + dir + base manifest + run base +
    // staged change set are all durable) is fully readable after a manager
    // reopen; the staged generation still binds the ORIGINAL run base, so a
    // reopened executor can land it through the transactional pipeline.
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    fs::create_dir_all(&user).unwrap();
    fs::write(user.join("a.txt"), b"alpha").unwrap();
    let (session, shadow_id, shadow_dir, run_base_hash) = {
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let ws = manager.create_workspace(user.to_str().unwrap()).unwrap();
        let session = manager
            .create_session(ws, "reopen", "fake", "m")
            .unwrap()
            .id();
        let shadows = ShadowRoots::new(manager.clone(), dir.path().join("shadows"));
        let shadow = shadows.begin_shadow(session, &user).unwrap();
        // The shadowed drive wrote before the crash...
        fs::write(shadow.root.join("a.txt"), b"agent post-crash state").unwrap();
        let run_base_hash = shadows
            .manager()
            .get_session(session)
            .unwrap()
            .unwrap()
            .ledger_run_base_get(&shadow.shadow_id)
            .unwrap()
            .unwrap()
            .snapshot_hash;
        let shadow_dir = shadow.root.clone();
        // Simulate a CRASH (no Drop): the service is leaked, exactly like a
        // killed daemon — the durable row + dir survive for reopen.
        std::mem::forget(shadows);
        (session, shadow.shadow_id.clone(), shadow_dir, run_base_hash)
    };
    // The daemon restarts: reopen reads the durable shadow row + dir.
    let manager2 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let shadows2 = ShadowRoots::new(manager2.clone(), dir.path().join("shadows"));
    let row = manager2.shadow_row(session).unwrap().expect("row survives");
    assert_eq!(row.shadow_id, shadow_id);
    assert_eq!(row.state, ShadowRowState::Active);
    assert!(shadow_dir.is_dir(), "shadow dir survives the crash");
    assert_eq!(
        fs::read(shadow_dir.join("a.txt")).unwrap(),
        b"agent post-crash state"
    );
    // Deterministic continuation: the run base + staged change set are read
    // back byte-identically from durable rows.
    let rb = shadows2
        .manager()
        .get_session(session)
        .unwrap()
        .unwrap()
        .ledger_run_base_get(&shadow_id)
        .unwrap()
        .expect("run base survives");
    assert_eq!(rb.snapshot_hash, run_base_hash);
    let cs = shadows2.present_change_set(session).unwrap();
    assert_eq!(cs.files.len(), 1);
    assert_eq!(
        cs.run_base_snapshot.as_deref(),
        Some(run_base_hash.as_str())
    );
    assert_eq!(fs::read(user.join("a.txt")).unwrap(), b"alpha");
}
