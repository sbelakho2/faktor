//! Adversarial filesystem-workspace confinement corpus (audit P1, Linux).
//!
//! Real spawns through [`ProcessSupervisor::run_sync`] with
//! [`FilesystemIsolation::Workspace`]: a confined child must lose every
//! read/write/traversal path outside its roots (observed as `EACCES`,
//! errno 13), keep full read/write/rename/remove inside, and a Required
//! demand on an unsupported kernel seam must refuse typed BEFORE exec
//! (marker proof — the program body never runs). The projection-agreement
//! test builds the REAL `faktor-sandbox` policy, asserts the projected
//! `workspace` tag and the spawn-layer demand, then observes the same
//! claim as kernel errno values from the child.
//!
//! Environment-dependent branches assert their predicate explicitly —
//! never a silent skip. Tests that toggle the process-global Landlock
//! failure seam serialize through the shared spawn lock.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use faktor_core::command::EnvSpec;

use crate::{
    FilesystemIsolation, FilesystemIsolationRequirement, NetworkIsolation, ProcessOwner,
    ProcessSupervisor, SpawnConfig,
};

const PROBE_ENV: &str = "FAKTOR_TEST_FS_PROBE";

/// The shared seam lock (also used by the network DenyAll tests): the
/// forced-Landlock failure flag is process-global, so every real
/// confinement spawn and every forced-state assertion excludes the others.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    crate::tests::deny_all_spawn_lock()
}

fn supervisor() -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    (dir, ProcessSupervisor::new(cas))
}

/// A workspace root and an outside sibling directory, both canonical.
struct Workspace {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    outside: PathBuf,
    secret: PathBuf,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let ws = ws.canonicalize().unwrap();
    let outside = outside.canonicalize().unwrap();
    let secret = outside.join("secret.txt");
    std::fs::write(&secret, b"top-secret-outside").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // A symlink inside the workspace pointing at the outside secret: the
    // kernel must resolve the target and deny it, never trust the spelling.
    std::os::unix::fs::symlink(&secret, ws.join("escape-link")).unwrap();
    Workspace {
        _dir: dir,
        ws,
        outside,
        secret,
    }
}

fn confined(
    cmd: &str,
    args: Vec<String>,
    ws: &Path,
    roots: Vec<PathBuf>,
    required: bool,
) -> SpawnConfig {
    SpawnConfig {
        cmd: cmd.into(),
        args,
        cwd: ws.to_path_buf(),
        env: EnvSpec::Minimal,
        owner: ProcessOwner::Daemon,
        capture: true,
        artifact_max: 256 * 1024,
        network_isolation: NetworkIsolation::Inherit,
        filesystem_isolation: FilesystemIsolation::Workspace { roots, required },
    }
}

fn landlock_available() -> bool {
    crate::sandbox::landlock_abi_for_tests().is_some()
}

/// (a) A confined child cannot READ a 0600 file outside its roots — direct
/// syscall denial (EACCES), not a shell-level check.
#[test]
fn confined_child_cannot_read_a_secret_outside_its_roots() {
    let _serial = serial();
    let w = workspace();
    let (_d, sup) = supervisor();
    let out = confined(
        "/bin/cat",
        vec![w.secret.to_string_lossy().into_owned()],
        &w.ws,
        vec![w.ws.clone()],
        true,
    );
    match sup.run_sync(out, Duration::from_secs(10), 64 * 1024, 64 * 1024) {
        Ok(out) => {
            assert!(
                landlock_available(),
                "a confined spawn succeeded without the Landlock backend"
            );
            assert_ne!(out.exit_code, Some(0), "cat must fail: {out:?}");
            assert!(
                !out.stdout_head.contains("top-secret-outside"),
                "the secret leaked: {:?}",
                out.stdout_head
            );
            let stderr = out.stderr_head.to_ascii_lowercase();
            assert!(
                stderr.contains("permission denied"),
                "the denial must be the kernel's EACCES, got: {stderr:?}"
            );
        }
        Err(err) => {
            // A kernel without Landlock: Required refuses typed BEFORE exec
            // — fail-closed, never an unconfined read.
            assert!(
                !landlock_available(),
                "this host has Landlock, so the Required confinement must not refuse: {err:?}"
            );
            assert_eq!(
                err.kind,
                faktor_core::error::ErrorKind::Permission,
                "{err:?}"
            );
        }
    }
    // Control: the parent can read the very same file (the denial is the
    // confinement, not a broken fixture).
    assert_eq!(
        std::fs::read_to_string(&w.secret).unwrap(),
        "top-secret-outside"
    );
}

/// (a) A confined child cannot WRITE outside its roots, cannot traverse
/// `..` out, and cannot escape through a symlink — all kernel EACCES.
#[test]
fn confined_child_cannot_write_traverse_or_symlink_out() {
    let _serial = serial();
    let w = workspace();
    if !landlock_available() {
        // Explicit predicate, no silent skip.
        let (_d, sup) = supervisor();
        let marker = w.ws.join("no-landlock-marker");
        let cfg = confined(
            "/bin/sh",
            vec!["-c".into(), format!("echo ran > {}", marker.display())],
            &w.ws,
            vec![w.ws.clone()],
            true,
        );
        let err = sup
            .run_sync(cfg, Duration::from_secs(10), 64 * 1024, 64 * 1024)
            .unwrap_err();
        assert_eq!(
            err.kind,
            faktor_core::error::ErrorKind::Permission,
            "a kernel without Landlock must refuse the Required demand typed: {err:?}"
        );
        assert!(!marker.exists(), "the refused child never exec'd");
        return;
    }
    let (_d, sup) = supervisor();
    let written = w.outside.join("written-by-child.txt");
    let script = format!(
        "echo forbidden > '{}'; echo write_rc=$?; \
         cat '{}/../outside/secret.txt'; echo traverse_rc=$?; \
         cat '{}/escape-link'; echo symlink_rc=$?",
        written.display(),
        w.ws.display(),
        w.ws.display(),
    );
    let cfg = confined(
        "/bin/sh",
        vec!["-c".into(), script],
        &w.ws,
        vec![w.ws.clone()],
        true,
    );
    let out = sup
        .run_sync(cfg, Duration::from_secs(10), 64 * 1024, 64 * 1024)
        .unwrap();
    assert_eq!(out.exit_code, Some(0), "the shell itself must run: {out:?}");
    assert!(
        !written.exists(),
        "a confined child wrote outside its roots: {out:?}"
    );
    assert!(
        !out.stdout_head.contains("top-secret-outside"),
        "the secret leaked through `..` or the symlink: {out:?}"
    );
    let stderr = out.stderr_head.to_ascii_lowercase();
    assert!(
        stderr.contains("permission denied"),
        "the write/traversal/symlink denials must be EACCES: {stderr:?}"
    );
    // All three attempts failed (rc != 0).
    for line in out.stdout_head.lines() {
        if let Some(rc) = line
            .strip_prefix("write_rc=")
            .or_else(|| line.strip_prefix("traverse_rc="))
            .or_else(|| line.strip_prefix("symlink_rc="))
        {
            assert_ne!(rc.trim(), "0", "denied access reported success: {out:?}");
        }
    }
}

/// (P0-SEC) The anchored Landlock root acquisition refuses a symlink at ANY
/// component: the workspace root swapped to an outside symlink can never
/// redirect the granted rights, and intermediate symlink components are
/// equally rejected. Direct, deterministic unit test of the pre-exec
/// acquisition primitive.
#[test]
fn anchored_root_acquisition_refuses_every_symlink_component() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(real.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    // The real directory opens and pins the object.
    crate::sandbox::anchored_open_probe(&real).expect("real dir opens");

    // Final component swapped to an outside symlink: ELOOP, never the
    // outside object.
    let linked = dir.path().join("linked");
    std::os::unix::fs::symlink(&outside, &linked).unwrap();
    let err = crate::sandbox::anchored_open_probe(&linked).unwrap_err();
    assert!(
        matches!(err.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)),
        "a symlinked workspace root must be refused (ELOOP/ENOTDIR), got {err:?}"
    );

    // Intermediate component symlink: equally refused (each component is the
    // final component of its own openat).
    let hop = dir.path().join("hop");
    std::os::unix::fs::symlink(&real, &hop).unwrap();
    let err = crate::sandbox::anchored_open_probe(&hop.join("sub")).unwrap_err();
    assert!(
        matches!(err.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)),
        "an intermediate symlink component must be refused, got {err:?}"
    );

    // `.`/`..` components are refused outright.
    let err = crate::sandbox::anchored_open_probe(Path::new(&format!("{}/./sub", real.display())))
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    let err = crate::sandbox::anchored_open_probe(Path::new(&format!("{}/sub/..", real.display())))
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));

    // Relative paths are refused (the anchor must be the real root).
    let err = crate::sandbox::anchored_open_probe(Path::new("real")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
}

/// (P0-SEC) Deterministic pre-exec seam: the policy path is captured, THEN
/// the workspace root is swapped for an outside symlink BEFORE the child is
/// spawned. The anchored acquisition must refuse the Required spawn typed
/// (before exec): the outside marker is never read or written and the child
/// program body never runs.
#[test]
fn workspace_root_swapped_to_a_symlink_is_refused_before_exec() {
    let _serial = serial();
    if !landlock_available() {
        return; // Required is typed-refused everywhere on this kernel
    }
    let (_d, sup) = supervisor();
    let w = workspace();
    // The spawn config below captures `w.ws` as the policy root. Swap it for
    // a symlink to the outside directory first.
    let real = w.ws.with_extension("real");
    std::fs::rename(&w.ws, &real).unwrap();
    std::os::unix::fs::symlink(&w.outside, &w.ws).unwrap();
    let marker = w.outside.join("swapped-marker.txt");
    assert!(!marker.exists());

    let cfg = confined(
        "/bin/sh",
        vec!["-c".into(), format!("echo pwned > '{}'", marker.display())],
        &w.ws,
        vec![w.ws.clone()],
        true,
    );
    let err = sup
        .run_sync(cfg, Duration::from_secs(10), 64 * 1024, 64 * 1024)
        .unwrap_err();
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Permission,
        "a symlink-swapped workspace root must refuse the Required spawn typed: {err:?}"
    );
    assert!(
        !marker.exists(),
        "the refused child never exec'd and never wrote outside"
    );
    assert_eq!(
        std::fs::read_to_string(&w.secret).unwrap(),
        "top-secret-outside"
    );

    // Restore the directory shape so the fixture's Drop cleanup sees a dir.
    std::fs::remove_file(&w.ws).unwrap();
    std::fs::rename(&real, &w.ws).unwrap();
}

/// (b) The confined child still fully reads, writes, creates, renames and
/// removes inside its workspace root.
#[test]
fn confined_child_fully_reads_and_writes_inside_its_root() {
    let _serial = serial();
    let w = workspace();
    if !landlock_available() {
        return;
    }
    let (_d, sup) = supervisor();
    let script = format!(
        "set -e; cd '{}'; \
         echo inside-ok > inside.txt; \
         mkdir -p sub; echo nested > sub/nested.txt; \
         cat inside.txt; cat sub/nested.txt; \
         mv inside.txt renamed.txt; cat renamed.txt; \
         rm -f renamed.txt; test ! -e renamed.txt; \
         echo inside-done",
        w.ws.display()
    );
    let cfg = confined(
        "/bin/sh",
        vec!["-c".into(), script],
        &w.ws,
        vec![w.ws.clone()],
        true,
    );
    let out = sup
        .run_sync(cfg, Duration::from_secs(10), 64 * 1024, 64 * 1024)
        .unwrap();
    assert_eq!(out.exit_code, Some(0), "{out:?}");
    assert!(out.stdout_head.contains("inside-ok"), "{out:?}");
    assert!(out.stdout_head.contains("nested"), "{out:?}");
    assert!(out.stdout_head.contains("inside-done"), "{out:?}");
    assert!(w.ws.join("sub/nested.txt").exists());
    assert!(!w.ws.join("renamed.txt").exists());
    // The outside secret is intact and the outside directory untouched.
    assert_eq!(
        std::fs::read_to_string(&w.secret).unwrap(),
        "top-secret-outside"
    );
    assert_eq!(std::fs::read_dir(&w.outside).unwrap().count(), 1);
}

/// (c) Required on an unsupported-kernel seam refuses typed BEFORE exec:
/// the marker proves the program body never ran, no process exists, and the
/// same demand under BestEffort runs application-policy-only afterwards.
#[test]
fn required_workspace_refuses_typed_before_exec_without_a_kernel_backend() {
    let _serial = serial();
    let w = workspace();
    let (_d, sup) = supervisor();
    let marker = w.ws.join("required-body-ran.txt");
    let cfg = confined(
        "/bin/sh",
        vec!["-c".into(), format!("echo ran > {}", marker.display())],
        &w.ws,
        vec![w.ws.clone()],
        true,
    );
    crate::sandbox::force_landlock_failure_for_tests(true);
    let result = sup.run_sync(cfg, Duration::from_secs(10), 64 * 1024, 64 * 1024);
    crate::sandbox::force_landlock_failure_for_tests(false);
    let err = result.expect_err("Required must fail closed without the kernel backend");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Permission,
        "{err:?}"
    );
    assert!(
        err.message.contains("sandbox unavailable")
            && err
                .message
                .contains("FilesystemIsolation::Workspace(required)")
            && err.message.contains("unenforced"),
        "the typed filesystem refusal must surface: {err:?}"
    );
    assert!(
        !marker.exists(),
        "the refused child never exec'd its program body"
    );
    assert!(
        sup.alive().is_empty(),
        "no process may exist after the refusal"
    );
    // BestEffort on the same seam falls back to application-policy-only and
    // actually runs (the projection tag promised best-effort, not a jail).
    let marker2 = w.ws.join("best-effort-body-ran.txt");
    let cfg = confined(
        "/bin/sh",
        vec!["-c".into(), format!("echo ran > {}", marker2.display())],
        &w.ws,
        vec![w.ws.clone()],
        false,
    );
    crate::sandbox::force_landlock_failure_for_tests(true);
    let result = sup.run_sync(cfg, Duration::from_secs(10), 64 * 1024, 64 * 1024);
    crate::sandbox::force_landlock_failure_for_tests(false);
    let out = result.expect("BestEffort must fall back, never refuse");
    assert_eq!(out.exit_code, Some(0), "{out:?}");
    assert!(marker2.exists(), "the best-effort child ran: {out:?}");
}

/// The probe child: runs inside a confined spawn and reports the RAW errno
/// of each denied/required operation to a report file inside the workspace
/// (the only writable path it has). The same test binary must be added as a
/// read/execute root by the parent so its own exec is allowed.
#[test]
fn filesystem_confinement_probe_child() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return; // normal suite run: the parent half owns the assertions
    }
    let report = std::env::var("FAKTOR_TEST_FS_REPORT").expect("report path");
    let outside = std::env::var("FAKTOR_TEST_FS_OUTSIDE").expect("outside path");
    let ws = std::env::var("FAKTOR_TEST_FS_WS").expect("workspace path");
    let mut lines: Vec<String> = Vec::new();
    fn errno(e: &std::io::Error) -> String {
        e.raw_os_error().unwrap_or(-1).to_string()
    }

    let read_outside = std::fs::read_to_string(format!("{outside}/secret.txt"));
    lines.push(format!(
        "read_outside_errno={}",
        match &read_outside {
            Ok(_) => "none".to_string(),
            Err(e) => errno(e),
        }
    ));
    let write_outside = std::fs::write(format!("{outside}/probe-written.txt"), b"x");
    lines.push(format!(
        "write_outside_errno={}",
        match &write_outside {
            Ok(()) => "none".to_string(),
            Err(e) => errno(e),
        }
    ));
    let traversal = std::fs::read_to_string(format!("{ws}/../outside/secret.txt"));
    lines.push(format!(
        "traversal_errno={}",
        match &traversal {
            Ok(_) => "none".to_string(),
            Err(e) => errno(e),
        }
    ));
    let symlink = std::fs::read_to_string(format!("{ws}/escape-link"));
    lines.push(format!(
        "symlink_errno={}",
        match &symlink {
            Ok(_) => "none".to_string(),
            Err(e) => errno(e),
        }
    ));
    let inside_write = std::fs::write(format!("{ws}/probe-inside.txt"), b"inside-ok");
    lines.push(format!(
        "inside_write={}",
        if inside_write.is_ok() { "ok" } else { "failed" }
    ));
    let inside_read = std::fs::read_to_string(format!("{ws}/probe-inside.txt"));
    lines.push(format!(
        "inside_read={}",
        match inside_read.as_deref() {
            Ok("inside-ok") => "ok".to_string(),
            _ => "failed".to_string(),
        }
    ));
    // Optional network probe (composition test): a connect to the parent
    // listener must fail under DenyAll.
    if let Ok(address) = std::env::var("FAKTOR_TEST_FS_TCP") {
        let connected = address
            .parse::<std::net::SocketAddr>()
            .ok()
            .and_then(|addr| {
                std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()
            })
            .is_some();
        lines.push(format!(
            "tcp={}",
            if connected { "connected" } else { "failed" }
        ));
    }
    let report_ok = std::fs::write(&report, lines.join("\n")).is_ok();
    std::process::exit(if report_ok { 0 } else { 3 });
}

struct ProbeReport {
    stdout: String,
    exit_code: Option<i32>,
    report: String,
}

#[allow(clippy::too_many_arguments)]
fn run_probe(
    sup: &Arc<ProcessSupervisor>,
    w: &Workspace,
    filesystem: FilesystemIsolation,
    network: NetworkIsolation,
    tcp: Option<std::net::SocketAddr>,
) -> Result<ProbeReport, faktor_core::error::Error> {
    let exe = std::env::current_exe().unwrap();
    let exe_dir = exe.parent().unwrap().canonicalize().unwrap();
    let report = w.ws.join("probe-report.txt");
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
        (PROBE_ENV.into(), "1".into()),
        (
            "FAKTOR_TEST_FS_REPORT".into(),
            report.to_string_lossy().into_owned().into(),
        ),
        (
            "FAKTOR_TEST_FS_OUTSIDE".into(),
            w.outside.to_string_lossy().into_owned().into(),
        ),
        (
            "FAKTOR_TEST_FS_WS".into(),
            w.ws.to_string_lossy().into_owned().into(),
        ),
    ];
    if let Some(tcp) = tcp {
        env.push(("FAKTOR_TEST_FS_TCP".into(), tcp.to_string().into()));
    }
    let mut roots = vec![w.ws.clone()];
    if matches!(&filesystem, FilesystemIsolation::Workspace { .. }) {
        // The test binary itself must be executable under the ruleset.
        roots.push(exe_dir);
    }
    let filesystem = match filesystem {
        FilesystemIsolation::Workspace { required, .. } => {
            FilesystemIsolation::Workspace { roots, required }
        }
        FilesystemIsolation::WorkspaceReadOnly {
            writable_roots,
            required,
            ..
        } => FilesystemIsolation::WorkspaceReadOnly {
            roots,
            writable_roots,
            required,
        },
        FilesystemIsolation::Inherit => FilesystemIsolation::Inherit,
    };
    let cfg = SpawnConfig {
        cmd: exe.to_string_lossy().into_owned(),
        args: vec![
            "--exact".into(),
            "filesystem_hostile::filesystem_confinement_probe_child".into(),
        ],
        cwd: w.ws.clone(),
        env: EnvSpec::Explicit(env),
        owner: ProcessOwner::Daemon,
        capture: true,
        artifact_max: 256 * 1024,
        network_isolation: network,
        filesystem_isolation: filesystem,
    };
    let out = sup.run_sync(cfg, Duration::from_secs(30), 64 * 1024, 64 * 1024)?;
    let report = std::fs::read_to_string(&report).unwrap_or_default();
    Ok(ProbeReport {
        stdout: format!("{}{}", out.stdout_head, out.stderr_head),
        exit_code: out.exit_code,
        report,
    })
}

/// (e) The policy projection and the kernel enforcement are the SAME claim:
/// the real sandbox policy projects `workspace`, maps to a Required
/// workspace demand, and the confined child observes EACCES (errno 13) for
/// every outside operation while inside operations succeed.
#[test]
fn policy_projection_and_kernel_enforcement_agree() {
    use faktor_sandbox::{FilesystemGuarantee, PermissionEngine, Rule, SandboxPolicy};

    let _serial = serial();
    let w = workspace();
    if !landlock_available() {
        return;
    }
    let (_d, sup) = supervisor();
    let policy = SandboxPolicy {
        read_external: Rule::Deny,
        write_external: Rule::Deny,
        filesystem_guarantee: FilesystemGuarantee::Required,
        ..Default::default()
    };
    // The projection says exactly what the spawn layer is about to enforce.
    assert_eq!(
        policy.spawn_profile().filesystem,
        "workspace",
        "the projection must claim the jail only with the backend present"
    );
    let engine = PermissionEngine::new(policy, Some(w.ws.clone()));
    let requirement = engine.spawn_filesystem_requirement();
    assert_eq!(
        requirement,
        FilesystemIsolationRequirement::Workspace { best_effort: false }
    );
    let filesystem = FilesystemIsolation::for_requirement(requirement, vec![w.ws.clone()]);
    assert_eq!(filesystem.as_tag(), "workspace");
    let probe = run_probe(&sup, &w, filesystem, NetworkIsolation::Inherit, None).unwrap();
    assert_eq!(probe.exit_code, Some(0), "{}", probe.stdout);
    for expected in [
        "read_outside_errno=13",
        "write_outside_errno=13",
        "traversal_errno=13",
        "symlink_errno=13",
        "inside_write=ok",
        "inside_read=ok",
    ] {
        assert!(
            probe.report.contains(expected),
            "projection/enforcement disagreement: missing {expected:?} in {:?}",
            probe.report
        );
    }
}

/// (d) Network isolation still holds when composed with the filesystem
/// confinement: the same child is denied every outside path AND the TCP
/// connect to the parent listener, while inside-workspace work lands.
#[test]
fn deny_all_network_and_workspace_confinement_compose() {
    use std::net::TcpListener;

    let _serial = serial();
    let w = workspace();
    if !landlock_available() {
        return;
    }
    let (_d, sup) = supervisor();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let filesystem = FilesystemIsolation::Workspace {
        roots: vec![w.ws.clone()],
        required: true,
    };
    match run_probe(
        &sup,
        &w,
        filesystem,
        NetworkIsolation::DenyAll,
        Some(address),
    ) {
        Ok(probe) => {
            assert_eq!(probe.exit_code, Some(0), "{}", probe.stdout);
            assert!(
                probe.report.contains("tcp=failed"),
                "the netns must refuse the parent-listener connect: {:?}",
                probe.report
            );
            assert!(
                probe.report.contains("read_outside_errno=13"),
                "{:?}",
                probe.report
            );
            assert!(
                probe.report.contains("inside_write=ok"),
                "{:?}",
                probe.report
            );
        }
        Err(err) => {
            // A host whose kernel/user-namespace policy refuses the netns
            // unshare: DenyAll must refuse typed BEFORE exec — never an
            // unisolated child. Explicit predicate, no silent skip.
            assert_eq!(
                err.kind,
                faktor_core::error::ErrorKind::Permission,
                "a DenyAll refusal must be typed: {err:?}"
            );
            assert!(
                !crate::sandbox::isolation_must_succeed_for_tests(),
                "this host can unshare a netns, so the DenyAll spawn must not refuse: {err:?}"
            );
        }
    }
}

/// The external spawn-confinement seam is exact: `Inherit` installs
/// nothing, a workspace demand on Linux installs the Landlock hook, and the
/// combiner never fabricates an empty hook (an empty hook would be a silent
/// unconfined run under a confinement claim).
#[test]
fn spawn_confinement_seam_is_honest_for_every_mode() {
    assert!(
        crate::workspace_spawn_confinement(&FilesystemIsolation::Inherit).is_none(),
        "Inherit installs no hook"
    );
    let workspace = FilesystemIsolation::Workspace {
        roots: vec![PathBuf::from("/tmp/faktor-fs-seam")],
        required: true,
    };
    assert!(
        crate::workspace_spawn_confinement(&workspace).is_some(),
        "Linux must ship the Landlock hook for a workspace demand"
    );
    assert!(
        crate::combined_spawn_confinement(Vec::new()).is_none(),
        "no hooks means no hook, never an empty runnable one"
    );
    let hook = crate::workspace_spawn_confinement(&workspace).unwrap();
    assert!(
        crate::combined_spawn_confinement(vec![hook]).is_some(),
        "a composed confinement is still installed"
    );
}

/// The additive mode/requirement/tag contract is exact: defaults are
/// Inherit, `for_requirement` maps both workspace levels, and no conversion
/// can silently produce a `workspace` tag from Inherit.
#[test]
fn filesystem_isolation_mapping_and_tags_are_exact() {
    assert_eq!(
        SpawnConfig::default().filesystem_isolation,
        FilesystemIsolation::Inherit,
        "the additive confinement field must default to Inherit"
    );
    let roots = vec![PathBuf::from("/tmp/faktor-fs-map")];
    for (requirement, tag, required) in [
        (
            FilesystemIsolationRequirement::Workspace { best_effort: false },
            "workspace",
            true,
        ),
        (
            FilesystemIsolationRequirement::Workspace { best_effort: true },
            "workspace_best_effort",
            false,
        ),
        (FilesystemIsolationRequirement::Inherit, "inherit", false),
    ] {
        let isolation = FilesystemIsolation::for_requirement(requirement, roots.clone());
        assert_eq!(isolation.as_tag(), tag, "{requirement:?}");
        assert_eq!(isolation.is_required(), required, "{requirement:?}");
        if requirement == FilesystemIsolationRequirement::Inherit {
            assert!(isolation.roots().is_empty(), "Inherit carries no roots");
        } else {
            assert_eq!(isolation.roots(), roots, "{requirement:?} carries roots");
        }
    }
    // The raw carry without roots is never a workspace claim.
    assert_eq!(
        FilesystemIsolation::for_requirement(
            FilesystemIsolationRequirement::Workspace { best_effort: false },
            Vec::new(),
        )
        .as_tag(),
        "workspace",
        "the tag is the DEMAND; the spawn layer owns the typed refusal"
    );
}
