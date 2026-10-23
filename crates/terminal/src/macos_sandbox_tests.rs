//! Live macOS confinement probes (the ONLY platform where the sandbox-exec
//! backend compiles). Each test spawns through the real supervisor; a host
//! without a usable `/usr/bin/sandbox-exec` must fail the Required spawn
//! with the typed `sandbox_backend_unavailable` refusal, which the tests
//! accept as the documented platform boundary rather than pretending the
//! confinement ran.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use faktor_core::cancellation::CancellationToken;

use crate::{
    FilesystemIsolation, NetworkIsolation, ProcessSupervisor, SpawnConfig,
    SANDBOX_BACKEND_UNAVAILABLE,
};

fn supervisor() -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    (dir, ProcessSupervisor::new(cas))
}

/// A workspace OUTSIDE the process temp roots, so a workspace write grant is
/// distinguishable from the always-allowed temp grant. `/Users/Shared` is
/// the standard world-writable non-temp location on macOS.
fn shared_workspace(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/Users/Shared")
        .expect("macOS /Users/Shared must be writable for the live confinement probes")
}

fn os_isolated(cwd: &Path, roots: Vec<PathBuf>, script: &str) -> SpawnConfig {
    SpawnConfig {
        cmd: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: cwd.to_path_buf(),
        network_isolation: NetworkIsolation::DenyAll,
        filesystem_isolation: FilesystemIsolation::Workspace {
            roots,
            required: true,
        },
        ..SpawnConfig::default()
    }
}

fn run(
    sup: &ProcessSupervisor,
    cfg: SpawnConfig,
) -> Result<crate::CommandOutput, faktor_core::error::Error> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(sup.run(
        cfg,
        std::time::Duration::from_secs(30),
        CancellationToken::new(),
    ))
}

/// Required confinement must either refuse typed (backend unusable) or run
/// the child confined; the only acceptable outcomes, never an unconfined run.
/// The credential probe targets the daemon HOME's real `~/.ssh` (the same path
/// the generated profile denies) and is skipped only when the host has none.
#[test]
fn os_isolated_denies_network_credentials_and_outside_writes() {
    let (_dir, sup) = supervisor();
    let workspace = shared_workspace("faktor-sbx-ws-");
    let root = workspace.path().to_path_buf();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    let ssh = real_home.as_ref().map(|home| home.join(".ssh"));
    let ssh_exists = ssh.as_ref().is_some_and(|path| path.exists());
    let link = root.join("credential-link");
    if let Some(ssh) = &ssh {
        let _ = std::os::unix::fs::symlink(ssh, &link);
    }
    let script = format!(
        "printf ok > allowed.txt\n\
         printf ok > sub/nested.txt\n\
         ls ~/.ssh >/dev/null 2>&1; echo ssh=$?\n\
         cat '{link}/config' 2>&1; echo link=$?\n\
         /usr/bin/curl -sS --max-time 3 https://example.com >/dev/null 2>&1; echo net=$?\n\
         printf x > /etc/faktor-macos-escape 2>/dev/null; echo etc=$?\n\
         printf x > ../faktor-macos-parent-escape 2>/dev/null; echo parent=$?\n\
         printf x > \"$HOME/faktor-macos-home-escape\" 2>/dev/null; echo home=$?\n\
         cat allowed.txt\n",
        link = link.display(),
    );
    match run(&sup, os_isolated(&root, vec![root.clone()], &script)) {
        Ok(output) => {
            assert_eq!(output.exit_code, Some(0), "{}", output.excerpt);
            let report = output.excerpt.as_str();
            if ssh_exists {
                assert!(
                    report.contains("ssh=1"),
                    "credential read must fail: {report}"
                );
                assert!(
                    report.contains("link=1") && report.contains("Operation not permitted"),
                    "a read through a workspace symlink into the credential store must be \
                     denied by the kernel: {report}"
                );
            }
            assert!(report.contains("net="), "{report}");
            assert!(
                !report.contains("net=0"),
                "external network must be denied: {report}"
            );
            assert!(
                report.contains("etc=1"),
                "outside write must fail: {report}"
            );
            assert!(
                report.contains("parent=1"),
                "parent escape must fail: {report}"
            );
            assert!(report.contains("home=1"), "home write must fail: {report}");
            assert_eq!(
                std::fs::read_to_string(root.join("allowed.txt")).unwrap(),
                "ok",
                "workspace write must succeed"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("sub/nested.txt")).unwrap(),
                "ok",
                "nested workspace write must succeed"
            );
            assert!(!Path::new("/etc/faktor-macos-escape").exists());
            assert!(!workspace
                .path()
                .parent()
                .unwrap()
                .join("faktor-macos-parent-escape")
                .exists());
        }
        Err(err) => {
            assert_eq!(err.kind, faktor_core::error::ErrorKind::Permission);
            assert!(
                err.message.contains(SANDBOX_BACKEND_UNAVAILABLE),
                "a backend refusal must carry the typed code: {err:?}"
            );
            assert!(
                err.message.contains("BEFORE exec"),
                "the refusal must happen before exec: {err:?}"
            );
        }
    }
}

/// Read-only workspace confinement: the workspace is physically
/// unmodifiable, while the explicit build/scratch root stays writable.
#[test]
fn read_only_workspace_denies_mutation_and_keeps_scratch_writable() {
    let (_dir, sup) = supervisor();
    let workspace = shared_workspace("faktor-sbx-ro-");
    let root = workspace.path().to_path_buf();
    let scratch = workspace.path().join("target");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(root.join("src.txt"), b"source").unwrap();
    let cfg = SpawnConfig {
        cmd: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "printf x > src.txt 2>/dev/null; echo src=$?; printf ok > target/out.txt 2>/dev/null; echo target=$?; cat src.txt"
                .into(),
        ],
        cwd: root.clone(),
        network_isolation: NetworkIsolation::Inherit,
        filesystem_isolation: FilesystemIsolation::WorkspaceReadOnly {
            roots: vec![root.clone()],
            writable_roots: vec![scratch.clone()],
            required: true,
        },
        ..SpawnConfig::default()
    };
    match run(&sup, cfg) {
        Ok(output) => {
            assert_eq!(output.exit_code, Some(0), "{}", output.excerpt);
            assert!(output.excerpt.contains("src=1"), "{}", output.excerpt);
            assert!(output.excerpt.contains("target=0"), "{}", output.excerpt);
            assert_eq!(
                std::fs::read_to_string(root.join("src.txt")).unwrap(),
                "source"
            );
            assert_eq!(
                std::fs::read_to_string(scratch.join("out.txt")).unwrap(),
                "ok"
            );
        }
        Err(err) => {
            assert!(err.message.contains(SANDBOX_BACKEND_UNAVAILABLE), "{err:?}");
        }
    }
}

/// The profile state and policy projection agree: on this build the sandbox
/// backend exists (sandbox crate projection says `workspace`), and a real
/// Required spawn either enforces it or refuses typed.
#[test]
fn policy_projection_matches_the_macos_backend() {
    assert!(
        faktor_sandbox::filesystem_backend_available(),
        "the macOS backend is a build fact and must be reported"
    );
    assert!(
        crate::deny_all_supported(),
        "macOS ships the DenyAll backend"
    );
    let policy = faktor_sandbox::SandboxPolicy {
        read_external: faktor_sandbox::Rule::Deny,
        write_external: faktor_sandbox::Rule::Deny,
        ..faktor_sandbox::SandboxPolicy::default()
    };
    let profile = policy.spawn_profile();
    assert_eq!(profile.filesystem, "workspace");
    assert_eq!(profile.network, "required");
    assert_eq!(profile.shell, "os_isolated");
}

/// The real verification toolchain still starts and executes under the
/// profile (the confinement must not make builds impossible): a resolved
/// `cargo --version` reads the toolchain from the read-only home grant and
/// forks/execs inside the sandbox.
#[test]
fn verification_toolchain_runs_under_the_profile() {
    let cargo = std::process::Command::new("/usr/bin/which")
        .arg("cargo")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|path| !path.is_empty());
    let Some(cargo) = cargo else {
        eprintln!("skipping: no cargo on PATH");
        return;
    };
    let (_dir, sup) = supervisor();
    let workspace = shared_workspace("faktor-sbx-tool-");
    let cfg = SpawnConfig {
        cmd: cargo,
        args: vec!["--version".into()],
        cwd: workspace.path().to_path_buf(),
        network_isolation: NetworkIsolation::DenyAll,
        filesystem_isolation: FilesystemIsolation::Workspace {
            roots: vec![workspace.path().to_path_buf()],
            required: true,
        },
        ..SpawnConfig::default()
    };
    match run(&sup, cfg) {
        Ok(output) => {
            assert_eq!(output.exit_code, Some(0), "{}", output.excerpt);
            assert!(
                output.excerpt.contains("cargo"),
                "the toolchain must run: {}",
                output.excerpt
            );
        }
        Err(err) => assert!(
            err.message.contains(SANDBOX_BACKEND_UNAVAILABLE),
            "only a typed backend refusal may replace the live toolchain run: {err:?}"
        ),
    }
}

/// A confined tool may signal its own descendants (build timeouts, job
/// control): the profile grants `signal` only within the same sandbox, never
/// to arbitrary processes.
#[test]
fn confined_tool_can_signal_its_own_tree_but_not_the_daemon() {
    let (_dir, sup) = supervisor();
    let workspace = shared_workspace("faktor-sbx-signal-");
    let script =
        "sleep 30 & p=$!; kill $p; echo kill=$?; wait $p; echo wait=$?; kill -0 $$; echo self=$?";
    let cfg = SpawnConfig {
        cmd: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: workspace.path().to_path_buf(),
        network_isolation: NetworkIsolation::DenyAll,
        filesystem_isolation: FilesystemIsolation::Workspace {
            roots: vec![workspace.path().to_path_buf()],
            required: true,
        },
        ..SpawnConfig::default()
    };
    match run(&sup, cfg) {
        Ok(output) => {
            assert_eq!(output.exit_code, Some(0), "{}", output.excerpt);
            assert!(
                output.excerpt.contains("kill=0"),
                "same-sandbox signalling must be allowed: {}",
                output.excerpt
            );
        }
        Err(err) => assert!(err.message.contains(SANDBOX_BACKEND_UNAVAILABLE), "{err:?}"),
    }
}

/// Agent-home scratch integration: the exact child environment's HOME (a
/// scratch home, different from the daemon's real HOME) is a sanctioned
/// writable root, while the daemon's real HOME remains read-only.
#[test]
fn scratch_agent_home_is_writable_under_the_profile() {
    let (_dir, sup) = supervisor();
    let workspace = shared_workspace("faktor-sbx-ws-scratch-");
    let home = shared_workspace("faktor-sbx-scratch-home-");
    let scratch_home = home.path().to_path_buf();
    std::fs::create_dir_all(scratch_home.join(".cache")).unwrap();
    let script = "printf ok > \"$HOME/scratch-write.txt\"; echo home=$?; \
                  printf ok > \"$XDG_CACHE_HOME/x\"; echo xdg=$?; \
                  cat \"$HOME/scratch-write.txt\"";
    let cfg = SpawnConfig {
        cmd: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: workspace.path().to_path_buf(),
        env: crate::EnvSpec::toolchain_with_home(&scratch_home),
        network_isolation: NetworkIsolation::DenyAll,
        filesystem_isolation: FilesystemIsolation::Workspace {
            roots: vec![workspace.path().to_path_buf()],
            required: true,
        },
        ..SpawnConfig::default()
    };
    match run(&sup, cfg) {
        Ok(output) => {
            assert_eq!(output.exit_code, Some(0), "{}", output.excerpt);
            assert!(output.excerpt.contains("home=0"), "{}", output.excerpt);
            assert!(output.excerpt.contains("xdg=0"), "{}", output.excerpt);
            assert_eq!(
                std::fs::read_to_string(scratch_home.join("scratch-write.txt")).unwrap(),
                "ok"
            );
            assert_eq!(
                std::fs::read_to_string(scratch_home.join(".cache/x")).unwrap(),
                "ok"
            );
        }
        Err(err) => assert!(err.message.contains(SANDBOX_BACKEND_UNAVAILABLE), "{err:?}"),
    }
}
