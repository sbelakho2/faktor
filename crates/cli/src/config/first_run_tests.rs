//! Adversarial first-run sandbox-choice tests (audit item 17).
//!
//! The durable decision document, its audit trail, the effective-policy
//! resolution and the typed presentation. Every path that could weaken the
//! fail-closed default without an explicit operator record is exercised:
//! corrupt documents, config precedence, and cross-platform pending state.

use super::*;

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn default_is_fail_closed_and_explicit_config_wins_over_any_record() {
    let dir = tmp();
    let data_dir = dir.path();
    // No record, no config: on a backend-less platform the choice is
    // pending (fail-closed stays effective); where a backend exists no
    // choice is needed.
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.backend_available, network_backend_available());
    if network_backend_available() {
        assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    } else {
        assert_eq!(state.decision, SandboxFirstRunDecision::Pending);
        assert!(state.pending());
        assert_eq!(
            cfg.sandbox_policy().unwrap().shell_execution,
            faktor_sandbox::ShellExecutionMode::OsIsolated,
            "pending never weakens the secure default"
        );
    }
    // A recorded grant is IGNORED when the config sets `[sandbox] shell`
    // explicitly: the config key is the operator's own declaration.
    let record =
        record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 1).unwrap();
    assert_eq!(record.choice, SandboxShellChoice::NetworkCapableUserGranted);
    let mut cfg = Config {
        sandbox: SandboxCfg {
            shell: Some(faktor_sandbox::ShellExecutionMode::OsIsolated),
            ..SandboxCfg::default()
        },
        ..Config::default()
    };
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::ExplicitConfig);
    assert_eq!(
        cfg.sandbox.shell,
        Some(faktor_sandbox::ShellExecutionMode::OsIsolated)
    );
    assert_eq!(
        cfg.sandbox.network_guarantee,
        faktor_sandbox::SandboxGuarantee::Required,
        "the durable grant must not touch any configured field"
    );
}

#[test]
fn recorded_grant_switches_the_agent_class_to_the_app_level_mode() {
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(
        data_dir,
        SandboxShellChoice::NetworkCapableUserGranted,
        1234,
    )
    .unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(
        state.decision,
        SandboxFirstRunDecision::NetworkCapableUserGranted
    );
    assert_eq!(
        cfg.sandbox.shell,
        Some(faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted)
    );
    assert_eq!(
        cfg.sandbox.network_guarantee,
        faktor_sandbox::SandboxGuarantee::BestEffort
    );
    let policy = cfg.sandbox_policy().expect("the grant pairing validates");
    assert_eq!(
        policy.shell_execution,
        faktor_sandbox::ShellExecutionMode::NetworkCapableUserGranted
    );
    assert_eq!(policy.spawn_profile().shell, "network_capable_user_granted");
    assert_eq!(policy.spawn_profile().network, "best_effort");
    // The record itself carries the frozen trade-off verbatim.
    let record = state.record.expect("record present");
    assert_eq!(record.trade_off, NETWORK_CAPABLE_SHELL_TRADE_OFF);
    assert!(record.trade_off.contains("strictly weaker"));
}

#[test]
fn recorded_fail_closed_keeps_the_secure_defaults() {
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 9).unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert_eq!(cfg.sandbox.shell, None);
    assert_eq!(
        cfg.sandbox.network_guarantee,
        faktor_sandbox::SandboxGuarantee::Required
    );
    let record = state.record.expect("record present");
    assert!(record.trade_off.is_empty(), "fail-closed grants nothing");
}

#[test]
fn corrupt_decision_is_loud_and_stays_fail_closed() {
    let dir = tmp();
    let data_dir = dir.path();
    std::fs::write(decision_path(data_dir), b"{ not json").unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_some());
    assert_eq!(cfg.sandbox.shell, None, "corruption never grants");
    assert!(load_decision(data_dir).is_err());
    // A wrong schema version refuses too (never a reinterpreted grant).
    std::fs::write(
        decision_path(data_dir),
        br#"{"version": 99, "choice": "network_capable_user_granted", "decided_at_ms": 1,
             "platform": "linux", "network_backend_available": true, "trade_off": ""}"#,
    )
    .unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state
        .corrupt_reason
        .as_deref()
        .unwrap()
        .contains("unsupported version"));
    // Even with a corrupt record, the explicit config declaration still
    // wins (and the corruption stays a reported reason, never a grant).
    let mut cfg = Config {
        sandbox: SandboxCfg {
            shell: Some(faktor_sandbox::ShellExecutionMode::OsIsolated),
            ..SandboxCfg::default()
        },
        ..Config::default()
    };
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::ExplicitConfig);
    assert!(state.corrupt_reason.is_some());
}

#[test]
/// Past the byte bound the trail is compacted through the fs-atomic
/// authority (the retained tail of complete lines plus the new entry
/// replace the file — never a hand-rolled rename), so the trail stays
/// bounded while recent history and the new decision survive.
fn audit_trail_is_append_only_bounded_and_compacts() {
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();
    record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 2).unwrap();
    let text = std::fs::read_to_string(audit_path(data_dir)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "both decisions are audited: {text}");
    assert!(lines[0].contains("sandbox_choice_fail_closed_recorded"));
    assert!(lines[1].contains("sandbox_network_capable_shell_granted"));
    // Past the byte bound the trail is COMPACTED through the fs-atomic
    // authority: a complete-line tail is retained and the new entry
    // appended; no hand-rolled rename/sidecar is ever produced.
    let tail_line = "{\"event\":\"retained\",\"choice\":\"fail_closed\"}\n";
    let mut big = String::with_capacity((1024 * 1024 + 64) as usize);
    while big.len() < 1024 * 1024 + 16 {
        big.push_str(tail_line);
    }
    std::fs::write(audit_path(data_dir), &big).unwrap();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 3).unwrap();
    let compacted = std::fs::read_to_string(audit_path(data_dir)).unwrap();
    assert!(
        compacted.contains("sandbox_choice_fail_closed_recorded"),
        "the new decision is audited"
    );
    assert!(
        compacted.contains("retained"),
        "recent complete lines survive the compaction"
    );
    assert!(
        compacted.len() <= 1024 * 1024,
        "the compacted trail is bounded: {} bytes",
        compacted.len()
    );
    assert!(
        !audit_path(data_dir).with_extension("jsonl.1").exists(),
        "compaction never leaves a hand-rolled sidecar"
    );
    // Every retained line is whole JSON (no torn lines).
    for line in compacted.lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("whole JSON line");
    }
}

#[test]
fn startup_notice_presents_the_typed_choice_and_never_hides_the_trade_off() {
    let dir = tmp();
    let data_dir = dir.path();
    let cfg = Config::default();
    let state = inspect(data_dir, &cfg);
    if network_backend_available() {
        // A kernel backend exists: nothing must be presented, the secure
        // default is simply in force.
        assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
        assert!(startup_notice(&state, data_dir).is_none());
    } else {
        assert!(state.pending());
        let text = startup_notice(&state, data_dir).expect("pending must be presented");
        assert!(text.contains("FAIL-CLOSED"));
        assert!(text.contains("--fail-closed"));
        assert!(text.contains("--network-capable-shell"));
        assert!(
            text.contains(NETWORK_CAPABLE_SHELL_TRADE_OFF),
            "the trade-off is shown, never summarized away"
        );
    }
    // A recorded grant notice carries the exact recorded trade-off.
    record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 5).unwrap();
    let state = inspect(data_dir, &cfg);
    let text = startup_notice(&state, data_dir).unwrap();
    assert!(text.contains(NETWORK_CAPABLE_SHELL_TRADE_OFF));
    assert!(text.contains("GRANTED"));
}

#[test]
fn agent_home_defaults_to_scratch_and_operator_grant_parses_strictly() {
    let cfg = Config::default();
    assert_eq!(cfg.sandbox.home, faktor_sandbox::AgentHomePolicy::Scratch);
    assert!(!cfg.sandbox.home.passes_real_home());
    // The explicit operator grant parses from the config document and
    // survives a round-trip; a hostile spelling is a parse error.
    let cfg: Config = serde_json::from_str(r#"{"sandbox": {"home": "operator_real_home"}}"#)
        .expect("the explicit grant parses");
    assert!(cfg.sandbox.home.passes_real_home());
    assert!(serde_json::from_str::<Config>(r#"{"sandbox": {"home": "real_home"}}"#).is_err());
    // The whole section keeps denying unknown keys.
    assert!(serde_json::from_str::<Config>(r#"{"sandbox": {"homes": "scratch"}}"#).is_err());
}
