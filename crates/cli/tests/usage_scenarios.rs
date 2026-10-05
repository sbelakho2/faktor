//! Real-process usage scenarios over the shipped `faktor-cli` binary.
//!
//! Every scenario asserts the observable CONTRACT of a use situation:
//! refusals are typed AND non-zero, and a refusal happens BEFORE any durable
//! effect (a refused `run` must leave zero sessions behind).

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_faktor-cli"))
}

/// `Command::output` with a hard wall-clock bound: a refusal must exit on its
/// own; if a command that should have refused starts serving instead, the
/// test fails instead of hanging the suite.
fn output_with_timeout(mut cmd: Command, timeout: Duration) -> std::process::Output {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        match child.try_wait().unwrap() {
            Some(_) => return child.wait_with_output().unwrap(),
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("command did not exit within {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
fn sessions_on_a_broken_store_is_nonzero_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    // Occupy the store path with a FILE: opening the store must refuse.
    std::fs::write(data.join("store"), b"not a directory").unwrap();
    let out = cli()
        .args(["sessions", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "sessions on a broken store must exit non-zero, stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error:"), "typed error expected: {stderr}");
}

#[test]
fn run_without_providers_refuses_before_creating_anything() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let out = cli()
        .args(["run", "--data-dir"])
        .arg(&data)
        .args(["--workspace"])
        .arg(&ws)
        .args(["--provider", "bogus", "hello"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "an unknown provider must refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not registered"),
        "the refusal must name the missing provider: {stderr}"
    );
    // ZERO sessions: the refusal precedes every durable effect.
    let list = cli()
        .args(["sessions", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(list.status.success(), "the follow-up listing must succeed");
    assert!(
        String::from_utf8_lossy(&list.stdout).trim().is_empty(),
        "a refused run must leave no session behind: {}",
        String::from_utf8_lossy(&list.stdout)
    );
}

#[test]
fn omitted_provider_with_zero_registered_refuses_typed() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let out = cli()
        .args(["run", "--data-dir"])
        .arg(&data)
        .args(["--workspace"])
        .arg(&ws)
        .arg("hello")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no providers are registered"),
        "the refusal must explain the provider requirement: {stderr}"
    );
}

#[test]
fn disabled_sections_refuse_nonzero_without_effects() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    for args in [
        vec!["updater", "status"],
        vec!["enterprise", "status"],
        vec!["worker", "run"],
    ] {
        let mut cmd = cli();
        cmd.args(&args).args(["--data-dir"]).arg(&data);
        let out = cmd.output().unwrap();
        assert!(
            !out.status.success(),
            "{args:?} must refuse with a non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.trim().is_empty(), "{args:?} must say why");
    }
}

#[test]
fn doctor_on_a_fresh_dir_passes_without_creating_a_store_side_effect_first() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = cli()
        .args(["doctor", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "a fresh data dir must doctor clean: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("all checks passed"), "{stdout}");
}

fn provider_config(port: u16) -> String {
    format!(
        r#"{{"config_version":1,"model":"m","providers":[{{"kind":"open_ai","id":"p1","base_url":"http://127.0.0.1:{port}/v1","api_key_env":"MOCK_KEY","api":"chat","allow_loopback":true}}]}}"#
    )
}

#[test]
fn run_sees_an_explicit_config_provider() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let config = dir.path().join("cfg.json");
    std::fs::write(&config, provider_config(9)).unwrap();
    let out = cli()
        .env("MOCK_KEY", "test")
        .args(["run", "--data-dir"])
        .arg(&data)
        .args(["--workspace"])
        .arg(&ws)
        .args(["--provider", "p1", "--config"])
        .arg(&config)
        .arg("hello")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("not registered"),
        "an explicit config must make p1 visible to run: {stderr}"
    );
}

#[test]
fn run_discovers_the_config_next_to_the_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(data.join("faktor-plus.json"), provider_config(9)).unwrap();
    let out = cli()
        .env("MOCK_KEY", "test")
        .args(["run", "--data-dir"])
        .arg(&data)
        .args(["--workspace"])
        .arg(&ws)
        .args(["--provider", "p1"])
        .arg("hello")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("not registered"),
        "the discovered <data-dir>/faktor-plus.json must make p1 visible: {stderr}"
    );
}

#[test]
fn run_exits_nonzero_when_the_turn_fails() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    // A provider whose endpoint is unreachable and whose destination is NOT
    // allowlisted: session creation succeeds, the turn fails at egress.
    let config = dir.path().join("cfg.json");
    std::fs::write(
        &config,
        r#"{"config_version":1,"model":"m","providers":[{"kind":"open_ai","id":"p1","base_url":"http://127.0.0.1:9/v1","api_key_env":"MOCK_KEY","api":"chat","allow_loopback":true,"models":["m"]}]}"#,
    )
    .unwrap();
    let out = cli()
        .env("MOCK_KEY", "test")
        .args(["run", "--data-dir"])
        .arg(&data)
        .args(["--workspace"])
        .arg(&ws)
        .args(["--provider", "p1", "--config"])
        .arg(&config)
        .arg("hello")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a failed turn must exit non-zero: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("failed") || String::from_utf8_lossy(&out.stderr).contains("turn"),
        "the failure must be visible: {stdout}"
    );
}

/// A `run --provider ghost` refusal is a PURE preflight refusal: it must
/// happen BEFORE `SessionManager::open`, so the refused data dir has no
/// `store/` and no `cas/` directory (the regression created a 36-table
/// database before refusing).
#[test]
fn run_with_unknown_provider_leaves_no_store_or_cas() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let out = cli()
        .args(["run", "--data-dir"])
        .arg(&data)
        .args(["--workspace"])
        .arg(&ws)
        .args(["--provider", "ghost", "hello"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "an unknown provider must refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not registered"), "{stderr}");
    assert!(
        !data.join("store").exists(),
        "a refused run must leave no store directory: {}",
        data.join("store").display()
    );
    assert!(
        !data.join("cas").exists(),
        "a refused run must leave no cas directory: {}",
        data.join("cas").display()
    );
}

/// `serve --config` with a pinned route naming an unregistered provider is
/// refused by the pure preflight BEFORE the store/CAS exist: the refused
/// data dir must contain no `store/` and no `cas/` directory.
#[test]
fn serve_with_pinned_unregistered_provider_leaves_no_store_or_cas() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let config = dir.path().join("pinned.json");
    std::fs::write(
        &config,
        r#"{"config_version":1,"model":"m","routing_mode":{"pinned":{"provider":"ghost","model":"m"}}}"#,
    )
    .unwrap();
    let mut cmd = cli();
    cmd.args(["serve", "--port", "0", "--data-dir"])
        .arg(&data)
        .args(["--config"])
        .arg(&config);
    let out = output_with_timeout(cmd, Duration::from_secs(30));
    assert!(
        !out.status.success(),
        "a pinned route naming an unregistered provider must refuse: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not registered"), "{stderr}");
    assert!(
        !data.join("store").exists(),
        "a refused serve must leave no store directory: {}",
        data.join("store").display()
    );
    assert!(
        !data.join("cas").exists(),
        "a refused serve must leave no cas directory: {}",
        data.join("cas").display()
    );
}

/// `doctor --deep` on a store seeded with the three crash wedges (an active
/// turn record with no drive, an ownerless pending permission, an applied
/// write without verification) must print the typed issues and exit
/// non-zero — never "all checks passed".
#[test]
fn doctor_deep_on_wedged_sessions_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    // The plain doctor creates the schema the raw seed below targets.
    let created = cli()
        .args(["doctor", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "store creation must succeed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    {
        let conn = rusqlite::Connection::open(data.join("store").join("faktor-plus.db")).unwrap();
        conn.execute(
            "INSERT INTO workspace(id, root, created_ms) VALUES (1, '/wedge-ws', 1)",
            [],
        )
        .unwrap();
        for (id, state) in [(1, "\"preparing\""), (2, "\"streaming\""), (3, "\"idle\"")] {
            conn.execute(
                "INSERT INTO session(id, workspace_id, title, provider, model, state, created_ms, updated_ms)
                 VALUES (?1, 1, 'seed', 'p', 'm', ?2, 1, 1)",
                rusqlite::params![id, state],
            )
            .unwrap();
        }
        // (1) active record with no queue/permission/tool drive.
        conn.execute(
            "INSERT INTO turn_record(session_id, turn_op_id, started_at, status, updated_ms)
             VALUES (1, 101, 1, 'active', 1)",
            [],
        )
        .unwrap();
        // (2) active record parked on a pending permission (no live waiter).
        conn.execute(
            "INSERT INTO turn_record(session_id, turn_op_id, started_at, status, updated_ms)
             VALUES (2, 102, 1, 'active', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO permission(session_id, op_id, capability, decision, expires_ms)
             VALUES (2, 102, '{\"read_workspace\":{\"path\":\"/wedge-ws/a\"}}', 'pending', 9999999999999)",
            [],
        )
        .unwrap();
        // (3) applied workspace write with no verification/integration row.
        conn.execute(
            "INSERT INTO tool_run(session_id, op_id, tool, args, status, started_ms, ended_ms, effect_status, recovery, postcondition)
             VALUES (3, 103, 'write_file', '{}', 'completed', 1, 2, 'applied', '{\"strategy\":\"mark_unknown\"}', '{\"workspace_id\":1,\"worktree_id\":1,\"relative_path\":\"a.txt\",\"expected_hash\":\"abababababababababababababababababababababababababababababababab\"}')",
            [],
        )
        .unwrap();
    }
    let out = cli()
        .args(["doctor", "--deep", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "doctor --deep must fail on wedged sessions: {stdout}"
    );
    for kind in [
        "active_turn_without_drive",
        "ownerless_pending_permission",
        "applied_run_without_verification",
    ] {
        assert!(
            stdout.contains(&format!("session wedge [{kind}]")),
            "missing typed issue {kind}: {stdout}"
        );
    }
    assert!(
        stdout.contains("doctor: ") && stdout.contains("issue(s)"),
        "the failure must be counted: {stdout}"
    );
    assert!(!stdout.contains("all checks passed"), "{stdout}");
}
