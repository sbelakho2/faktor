//! Adversarial corpus for the process-supervisor contracts.
//!
//! Output-cap boundaries, exit/signal/timeout outcomes, environment scrubbing
//! through `EnvSpec`, descriptor hygiene, typed spawn failures, detached
//! child reaping, owner-scoped kill/transfer and the bounded live registry.
//! Every row asserts one outcome with a message naming the row.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use faktor_core::command::{env_name_is_denied, EnvSpec};
use faktor_core::error::ErrorKind;
use faktor_core::id::{SessionId, WorkspaceId};

use crate::{ProcessOwner, ProcessSupervisor, SpawnConfig};

fn supervisor() -> Arc<ProcessSupervisor> {
    ProcessSupervisor::try_shared().expect("shared supervisor")
}

/// A private supervisor over its own CAS: global registry-count assertions
/// must never race other tests sharing the process-wide instance.
fn private_supervisor(max_live: usize) -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).expect("cas opens"));
    (dir, ProcessSupervisor::with_limit(cas, max_live))
}

fn wait_alive_count(sup: &ProcessSupervisor, expected: usize, label: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sup.alive().len() != expected {
        assert!(
            std::time::Instant::now() < deadline,
            "case {label}: live count must become {expected}, got {}",
            sup.alive().len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn reap_id(sup: &ProcessSupervisor, id: u64, label: &str) -> crate::Reaped {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(reaped) = sup.reap().into_iter().find(|r| r.id == id) {
            return reaped;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "case {label}: row {id} must be reaped within the bound"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn cfg(script: &str) -> SpawnConfig {
    SpawnConfig {
        cmd: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: PathBuf::from("/"),
        env: EnvSpec::Minimal,
        owner: ProcessOwner::Daemon,
        capture: true,
        artifact_max: 1 << 20,
        ..SpawnConfig::default()
    }
}

fn alive_pid(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Output caps: the returned head is never larger than the requested cap,
/// truncation is flagged exactly when the stream exceeded it, and the run
/// stays bounded.
#[test]
fn run_sync_output_cap_matrix() {
    let sup = supervisor();
    struct Row {
        label: &'static str,
        script: &'static str,
        stdout_cap: usize,
        stderr_cap: usize,
    }
    let rows = [
        Row { label: "small-under-cap", script: "printf abc", stdout_cap: 64, stderr_cap: 64 },
        Row { label: "exact-cap", script: "printf 0123456789", stdout_cap: 10, stderr_cap: 10 },
        Row { label: "over-cap-one", script: "printf 0123456789X", stdout_cap: 10, stderr_cap: 10 },
        Row { label: "flood-over-cap", script: "head -c 100000 /dev/zero | tr '\\0' 'z'", stdout_cap: 64, stderr_cap: 64 },
        Row { label: "stderr-over-cap", script: "head -c 100000 /dev/zero | tr '\\0' 'e' 1>&2", stdout_cap: 64, stderr_cap: 128 },
        Row { label: "both-over", script: "head -c 50000 /dev/zero | tr '\\0' 'o'; head -c 50000 /dev/zero | tr '\\0' 'r' 1>&2", stdout_cap: 100, stderr_cap: 200 },
        Row { label: "zero-cap", script: "printf x", stdout_cap: 0, stderr_cap: 0 },
        Row { label: "empty-caps-no-output", script: "true", stdout_cap: 0, stderr_cap: 0 },
        Row { label: "unicode-split-at-cap", script: "printf '\\303\\251\\303\\251\\303\\251'", stdout_cap: 5, stderr_cap: 5 },
        Row { label: "nul-bytes", script: "printf 'a\\000b\\000c'", stdout_cap: 64, stderr_cap: 64 },
    ];
    for row in rows {
        let started = std::time::Instant::now();
        let output = sup
            .run_sync(
                cfg(row.script),
                Duration::from_secs(10),
                row.stdout_cap,
                row.stderr_cap,
            )
            .unwrap_or_else(|e| panic!("case {}: run must succeed: {e}", row.label));
        assert!(
            started.elapsed() < Duration::from_secs(9),
            "case {}: the run must be bounded, took {:?}",
            row.label,
            started.elapsed()
        );
        // The head is capped in BYTES then lossily decoded: character count
        // can never exceed the byte cap (each lossy replacement consumes one
        // input byte) even when the cap splits a multi-byte sequence.
        assert!(
            output.stdout_head.chars().count() <= row.stdout_cap,
            "case {}: stdout head chars exceed cap {}: {:?}",
            row.label,
            row.stdout_cap,
            output.stdout_head
        );
        assert!(
            output.stderr_head.chars().count() <= row.stderr_cap,
            "case {}: stderr head chars exceed cap {}: {:?}",
            row.label,
            row.stderr_cap,
            output.stderr_head
        );
        if row.script == "true" {
            assert!(
                !output.stdout_truncated && !output.stderr_truncated,
                "case {}: no output means no truncation",
                row.label
            );
        }
        assert!(
            !output.timed_out,
            "case {}: a fast script must not time out",
            row.label
        );
        assert_eq!(
            output.exit_code,
            Some(0),
            "case {}: exit code must be observed",
            row.label
        );
    }
}

/// Exit-code and timeout outcomes: real codes are propagated, signals report
/// `None`, and a deadline overrun is flagged `timed_out` with the child
/// killed and reaped within the bound.
#[test]
fn run_sync_exit_signal_and_timeout_matrix() {
    let sup = supervisor();
    let rows: [(&str, &str, Option<i32>, bool); 8] = [
        ("exit-0", "exit 0", Some(0), false),
        ("exit-1", "exit 1", Some(1), false),
        ("exit-7", "exit 7", Some(7), false),
        ("exit-127", "exit 127", Some(127), false),
        ("exit-42-stderr", "echo why 1>&2; exit 42", Some(42), false),
        ("signal-kill-self", "kill -9 $$", None, false),
        ("signal-term-self", "kill -TERM $$", None, false),
        ("timeout-sleep", "sleep 30", None, true),
    ];
    for (label, script, exit, timed_out) in rows {
        let started = std::time::Instant::now();
        let output = sup
            .run_sync(cfg(script), Duration::from_millis(300), 4096, 4096)
            .unwrap_or_else(|e| panic!("case {label}: run must not error: {e}"));
        assert_eq!(
            output.exit_code, exit,
            "case {label}: exit code must be exactly {exit:?}"
        );
        assert_eq!(
            output.timed_out, timed_out,
            "case {label}: timeout flag must be exactly {timed_out}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "case {label}: the run must be bounded, took {:?}",
            started.elapsed()
        );
    }
}

/// Environment scrubbing: explicit secrets are denied, explicit values are
/// applied, Minimal is empty plus the universal prompt guard, and inherited
/// daemon variables never leak through a baseline allowlist that lacks them.
#[test]
fn env_scrubbing_matrix() {
    let sup = supervisor();
    struct Row {
        label: &'static str,
        spec: EnvSpec,
        expect_present: &'static [&'static str],
        expect_absent: &'static [&'static str],
    }
    const SECRET_MARKER: &str = "faktor-hostile-secret-value";
    let rows = vec![
        Row {
            label: "minimal-empty",
            spec: EnvSpec::Minimal,
            expect_present: &["GIT_TERMINAL_PROMPT=0"],
            expect_absent: &["PATH=", "HOME=", "FAKTOR_PARENT_ONLY="],
        },
        Row {
            label: "explicit-values-applied",
            spec: EnvSpec::Explicit(vec![
                ("FAKTOR_A".into(), "one".into()),
                ("FAKTOR_B".into(), "two".into()),
            ]),
            expect_present: &["FAKTOR_A=one", "FAKTOR_B=two"],
            expect_absent: &["PATH="],
        },
        Row {
            label: "explicit-empty-means-parent-copy",
            spec: EnvSpec::Explicit(vec![("FAKTOR_MISSING_XYZ".into(), "".into())]),
            expect_present: &["GIT_TERMINAL_PROMPT=0"],
            expect_absent: &["FAKTOR_MISSING_XYZ="],
        },
        Row {
            label: "secret-api-key-denied",
            spec: EnvSpec::Explicit(vec![("MY_API_KEY".into(), SECRET_MARKER.into())]),
            expect_present: &[],
            expect_absent: &[SECRET_MARKER],
        },
        Row {
            label: "secret-password-denied",
            spec: EnvSpec::Explicit(vec![("DB_PASSWORD".into(), SECRET_MARKER.into())]),
            expect_present: &[],
            expect_absent: &[SECRET_MARKER],
        },
        Row {
            label: "secret-secret-denied",
            spec: EnvSpec::Explicit(vec![("APP_SECRET".into(), SECRET_MARKER.into())]),
            expect_present: &[],
            expect_absent: &[SECRET_MARKER],
        },
        Row {
            label: "secret-token-suffix-denied",
            spec: EnvSpec::Explicit(vec![("GITHUB_TOKEN".into(), SECRET_MARKER.into())]),
            expect_present: &[],
            expect_absent: &[SECRET_MARKER],
        },
        Row {
            label: "denied-exact-server-password",
            spec: EnvSpec::Explicit(vec![
                ("FAKTOR_SERVER_PASSWORD".into(), SECRET_MARKER.into()),
                ("KEEP_ME".into(), "yes".into()),
            ]),
            expect_present: &["KEEP_ME=yes"],
            expect_absent: &[SECRET_MARKER],
        },
        Row {
            label: "last-explicit-entry-wins",
            spec: EnvSpec::Explicit(vec![
                ("DUP".into(), "first".into()),
                ("DUP".into(), "second".into()),
            ]),
            expect_present: &["DUP=second"],
            expect_absent: &["DUP=first"],
        },
        Row {
            label: "allowlist-missing-names-skipped",
            spec: EnvSpec::Allowlisted(vec!["FAKTOR_DEFINITELY_UNSET_XYZ".into()]),
            expect_present: &[],
            expect_absent: &["FAKTOR_DEFINITELY_UNSET_XYZ="],
        },
    ];
    for row in rows {
        let mut config = cfg("env");
        config.env = row.spec;
        let output = sup
            .run_sync(config, Duration::from_secs(5), 64 * 1024, 4096)
            .unwrap_or_else(|e| panic!("case {}: env run failed: {e}", row.label));
        for needle in row.expect_present {
            assert!(
                output.stdout_head.contains(needle),
                "case {}: environment must contain {needle:?}: {:?}",
                row.label,
                output.stdout_head
            );
        }
        for needle in row.expect_absent {
            assert!(
                !output.stdout_head.contains(needle),
                "case {}: environment must NOT contain {needle:?}: {:?}",
                row.label,
                output.stdout_head
            );
        }
    }
    // The deny predicate itself, over hostile spellings.
    let denied = [
        "API_KEY",
        "MY_API_KEY",
        "api_key",
        "DB_PASSWORD",
        "password",
        "CLIENT_SECRET",
        "GITHUB_TOKEN",
        "ACCESS_TOKEN",
        "FAKTOR_SERVER_PASSWORD",
    ];
    for name in denied {
        assert!(
            env_name_is_denied(std::ffi::OsStr::new(name)),
            "case deny-{name}: must be denied"
        );
    }
    for name in ["PATH", "HOME", "GIT_TERMINAL_PROMPT", "MY_TOKENS"] {
        assert!(
            !env_name_is_denied(std::ffi::OsStr::new(name)),
            "case keep-{name}: must not be denied"
        );
    }
}

/// Descriptor hygiene: the child sees only the standard descriptors (plus
/// whatever its own runtime opens transiently); no daemon descriptor leaks
/// across exec.
#[test]
#[cfg(target_os = "linux")]
fn child_descriptor_hygiene_matrix() {
    let sup = supervisor();
    let probes = ["true", "echo hi", "sh -c 'true'", "env", "id", "pwd"];
    for (index, script) in probes.into_iter().enumerate() {
        let probe = format!("{script}\nls -1 /proc/self/fd | sort -n");
        let output = sup
            .run_sync(cfg(&probe), Duration::from_secs(5), 4096, 4096)
            .unwrap_or_else(|e| panic!("probe {index}: fd listing failed: {e}"));
        let fds: Vec<usize> = output
            .stdout_head
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();
        for standard in [0usize, 1, 2] {
            assert!(
                fds.contains(&standard),
                "probe {index}: fd {standard} must exist: {fds:?}"
            );
        }
        assert!(
            fds.iter().all(|fd| *fd <= 3),
            "probe {index}: no descriptor above 3 may survive exec: {fds:?} (script {script:?})"
        );
    }
}

/// Spawn failures are typed before any process exists: missing binary, empty
/// command, missing cwd, non-executable file, a directory as the program, a
/// NUL in an argument and an oversized argument list.
#[test]
fn spawn_failure_matrix_is_typed() {
    let (_dir, sup) = private_supervisor(128);
    let dir = tempfile::tempdir().unwrap();
    let non_exec = dir.path().join("not-executable");
    std::fs::write(&non_exec, b"#!/bin/sh\ntrue\n").unwrap();
    let mut permissions = std::fs::metadata(&non_exec).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt as _;
    permissions.set_mode(0o644);
    std::fs::set_permissions(&non_exec, permissions).unwrap();

    let rows: Vec<(&str, SpawnConfig)> = vec![
        (
            "missing-binary",
            SpawnConfig {
                cmd: "faktor-no-such-binary-xyz".into(),
                ..SpawnConfig::default()
            },
        ),
        (
            "empty-command",
            SpawnConfig {
                cmd: String::new(),
                ..SpawnConfig::default()
            },
        ),
        (
            "directory-as-program",
            SpawnConfig {
                cmd: dir.path().to_string_lossy().into_owned(),
                ..SpawnConfig::default()
            },
        ),
        (
            "non-executable-file",
            SpawnConfig {
                cmd: non_exec.to_string_lossy().into_owned(),
                ..SpawnConfig::default()
            },
        ),
        (
            "missing-cwd",
            SpawnConfig {
                cmd: "true".into(),
                cwd: dir.path().join("gone"),
                ..SpawnConfig::default()
            },
        ),
        (
            "nul-argument",
            SpawnConfig {
                cmd: "sh".into(),
                args: vec!["-c".into(), "true\0false".into()],
                cwd: dir.path().to_path_buf(),
                ..SpawnConfig::default()
            },
        ),
    ];
    for (label, config) in rows {
        let result = sup.spawn(config);
        let err = result
            .err()
            .unwrap_or_else(|| panic!("case {label}: spawn must fail"));
        assert!(
            !err.message.is_empty(),
            "case {label}: the refusal must carry a typed message: {err}"
        );
        assert!(
            matches!(
                err.kind,
                ErrorKind::Internal
                    | ErrorKind::NotFound
                    | ErrorKind::Permission
                    | ErrorKind::Oversized
                    | ErrorKind::Malformed
            ),
            "case {label}: a spawn failure must be one of the typed spawn errors: {err}"
        );
    }
    assert_eq!(
        sup.alive().len(),
        0,
        "no failed spawn may leave a live registry row"
    );
}

/// Detached spawn + reap and owner-scoped kill: only the addressed owner's
/// children die, transfer moves the kill scope, and repeated rounds leave the
/// registry empty.
#[test]
fn detached_kill_scoping_and_reap_matrix() {
    let (_dir, sup) = private_supervisor(128);
    let session = SessionId::new(7);
    let other_session = SessionId::new(8);
    let workspace = WorkspaceId::new(9);

    let child = sup
        .spawn(SpawnConfig {
            cmd: "sleep".into(),
            args: vec!["300".into()],
            cwd: PathBuf::from("/"),
            owner: ProcessOwner::Session(session),
            ..SpawnConfig::default()
        })
        .expect("spawn session child");
    let pid = child.pid;
    assert!(alive_pid(pid), "the spawned child must be alive");
    wait_alive_count(&sup, 1, "one-live-row");

    // Wrong owner: nothing is killed.
    let killed = sup.kill_all_for(ProcessOwner::Verification(session));
    assert!(
        killed.is_empty(),
        "another owner's scope must stay untouched: {killed:?}"
    );
    assert!(
        alive_pid(pid),
        "the child must survive the wrong-scope kill"
    );

    // Transfer moves the scope.
    sup.transfer(child.id, ProcessOwner::Workspace(workspace))
        .expect("transfer must succeed for a live row");
    assert!(
        sup.kill_all_for(ProcessOwner::Session(session)).is_empty(),
        "the old owner must no longer own the row"
    );
    assert!(alive_pid(pid), "transfer must not kill the child by itself");
    let killed = sup.kill_all_for(ProcessOwner::Workspace(workspace));
    assert_eq!(killed, vec![child.id], "the new owner kills the row");
    let reaped = reap_id(&sup, child.id, "workspace-kill");
    assert_eq!(reaped.pid, pid, "the reaped pid must match");
    assert!(!alive_pid(pid), "the killed and reaped child must be gone");
    wait_alive_count(&sup, 0, "registry-empty");

    // Typed refusals for unknown rows.
    let err = sup
        .transfer(999_999, ProcessOwner::Daemon)
        .expect_err("transfer of an unknown row must be typed");
    assert!(!err.message.is_empty());
    let err = sup
        .kill(999_999, 10)
        .expect_err("kill of an unknown row must be typed");
    assert!(!err.message.is_empty());

    // Repeated start/kill rounds leave nothing behind.
    for round in 0..8u32 {
        let child = sup
            .spawn(SpawnConfig {
                cmd: "sleep".into(),
                args: vec!["300".into()],
                cwd: PathBuf::from("/"),
                owner: ProcessOwner::Session(other_session),
                ..SpawnConfig::default()
            })
            .unwrap_or_else(|e| panic!("round {round}: spawn failed: {e}"));
        let pid = child.pid;
        sup.kill(child.id, 0)
            .unwrap_or_else(|e| panic!("round {round}: kill failed: {e}"));
        let reaped = reap_id(&sup, child.id, "round-reap");
        assert_eq!(reaped.pid, pid, "round {round}: the reaped pid must match");
        assert!(!alive_pid(pid), "round {round}: the child must be gone");
    }
    wait_alive_count(&sup, 0, "storm-empty");
}

/// The live registry ceiling: a spawn past the limit is refused typed BEFORE
/// a process exists, and freeing a slot admits the next child.
#[test]
fn bounded_registry_refuses_past_the_ceiling() {
    let (_dir, sup) = private_supervisor(1);
    let make = || SpawnConfig {
        cmd: "sleep".into(),
        args: vec!["300".into()],
        cwd: PathBuf::from("/"),
        owner: ProcessOwner::Daemon,
        ..SpawnConfig::default()
    };
    let first = sup.spawn(make()).expect("first child fits the ceiling");
    let err = sup
        .spawn(make())
        .expect_err("a second child past the ceiling must be refused");
    assert_eq!(
        err.kind,
        ErrorKind::Oversized,
        "the ceiling refusal must be typed Oversized: {err}"
    );
    wait_alive_count(&sup, 1, "refused-must-not-register");
    sup.kill(first.id, 0).expect("kill first");
    let _ = reap_id(&sup, first.id, "first-reap");
    let third = sup
        .spawn(make())
        .expect("a freed slot admits the next child");
    sup.kill(third.id, 0).expect("kill third");
    let _ = reap_id(&sup, third.id, "third-reap");
}
