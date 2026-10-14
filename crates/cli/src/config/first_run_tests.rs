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

// ---------------------------------------------------------------------------
// Audit confirmation (tamper evidence): a decision document alone grants
// nothing; only a document matching its append-only entry is a decision.
// ---------------------------------------------------------------------------

/// A well-formed grant document EXACTLY as `record_decision` writes it, but
/// with no audit trail behind it.
fn forged_grant_record() -> Vec<u8> {
    serde_json::to_vec_pretty(&SandboxFirstRunRecord {
        version: SANDBOX_FIRST_RUN_VERSION,
        choice: SandboxShellChoice::NetworkCapableUserGranted,
        decided_at_ms: 42,
        platform: std::env::consts::OS.to_string(),
        network_backend_available: network_backend_available(),
        trade_off: NETWORK_CAPABLE_SHELL_TRADE_OFF.to_string(),
    })
    .unwrap()
}

/// Item: a hand-written decision document — even a byte-perfect grant — is
/// NOT an operator decision; only the recorded path (audit FIRST, then the
/// document) is. Flipping a recorded fail-closed into a grant or mutating a
/// recorded field is refused; restoring the exact recorded document is
/// accepted again (the check is exact, never a blanket refusal).
#[test]
fn a_forged_or_flipped_decision_document_grants_nothing() {
    let dir = tmp();
    let data_dir = dir.path();

    // (1) A perfect forged grant alone: refused typed, fail-closed, and
    // never presented as a recorded decision.
    std::fs::write(decision_path(data_dir), forged_grant_record()).unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(
        state.decision,
        SandboxFirstRunDecision::FailClosed,
        "a document with no matching audit entry must never grant"
    );
    assert!(
        state.record.is_none(),
        "an unconfirmed document is not a recorded decision"
    );
    assert!(state
        .corrupt_reason
        .as_deref()
        .unwrap_or("")
        .contains("audit"));
    assert_eq!(cfg.sandbox.shell, None, "no forge path may set the mode");
    assert_eq!(
        cfg.sandbox.network_guarantee,
        faktor_sandbox::SandboxGuarantee::Required
    );

    // (2) The recorded grant (audit + document) IS accepted — including
    // when the same timestamp is used, proving the confirmation is exact.
    let recorded =
        record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 42).unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(
        state.decision,
        SandboxFirstRunDecision::NetworkCapableUserGranted
    );
    assert_eq!(state.record.as_ref(), Some(&recorded));

    // (3) Mutating ONE recorded field (the backend fact) breaks the match.
    let mut flipped = recorded.clone();
    flipped.network_backend_available = !flipped.network_backend_available;
    std::fs::write(
        decision_path(data_dir),
        serde_json::to_vec_pretty(&flipped).unwrap(),
    )
    .unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert_eq!(cfg.sandbox.shell, None);

    // (4) Flipping a recorded FAIL-CLOSED into a grant cannot reuse the
    // fail-closed audit event.
    record_decision(data_dir, SandboxShellChoice::FailClosed, 7).unwrap();
    let original = std::fs::read(decision_path(data_dir)).unwrap();
    let mut forged: SandboxFirstRunRecord = serde_json::from_slice(&original).unwrap();
    forged.choice = SandboxShellChoice::NetworkCapableUserGranted;
    forged.trade_off = NETWORK_CAPABLE_SHELL_TRADE_OFF.to_string();
    std::fs::write(
        decision_path(data_dir),
        serde_json::to_vec_pretty(&forged).unwrap(),
    )
    .unwrap();
    let state = apply_effective_choice(&mut Config::default(), data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_some());

    // (5) Restoring the exact recorded document is accepted again.
    std::fs::write(decision_path(data_dir), &original).unwrap();
    let state = inspect(data_dir, &Config::default());
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert!(state.record.is_some());
}

/// A legitimate record whose audit trail is replaced by a hostile
/// multi-megabyte file is untrusted (fail closed, loud), and a decision
/// document beyond its own byte bound is refused WITHOUT being read.
#[test]
fn oversized_decision_and_audit_files_fail_closed() {
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 3).unwrap();
    {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(audit_path(data_dir))
            .unwrap();
        // 5 MiB of sparse zeros: far beyond the confirm bound and containing
        // no matching entry.
        file.set_len(5 * 1024 * 1024).unwrap();
    }
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_some());
    assert_eq!(cfg.sandbox.shell, None);

    // A decision document past 64 KiB is refused as oversized: never a
    // parse of a hostile multi-gigabyte file, never a grant.
    std::fs::write(decision_path(data_dir), vec![b'x'; 128 * 1024]).unwrap();
    let state = apply_effective_choice(&mut Config::default(), data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state
        .corrupt_reason
        .as_deref()
        .unwrap_or("")
        .contains("bound"));
    assert!(load_decision(data_dir).is_err());
}

/// A non-regular object planted at either artifact path is hostile input:
/// a FIFO at the decision path must be refused (never opened — opening one
/// would block every daemon start), and a symlinked audit trail is never
/// followed. Both stay fail-closed and loud.
#[cfg(unix)]
#[test]
fn non_regular_decision_and_audit_objects_fail_closed() {
    let dir = tmp();
    let data_dir = dir.path();

    let fifo = decision_path(data_dir);
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must be available on unix hosts");
    assert!(status.success(), "mkfifo failed: {status:?}");
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(
        state
            .corrupt_reason
            .as_deref()
            .unwrap_or("")
            .contains("regular file"),
        "{:?}",
        state.corrupt_reason
    );
    assert_eq!(cfg.sandbox.shell, None);
    std::fs::remove_file(&fifo).unwrap();

    // A recorded decision whose audit trail was swapped for a symlink is
    // untrusted: the trail is never followed to another file.
    record_decision(data_dir, SandboxShellChoice::FailClosed, 5).unwrap();
    let real = data_dir.join("real-audit.jsonl");
    std::fs::rename(audit_path(data_dir), &real).unwrap();
    std::os::unix::fs::symlink(&real, audit_path(data_dir)).unwrap();
    let state = apply_effective_choice(&mut Config::default(), data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(
        state
            .corrupt_reason
            .as_deref()
            .unwrap_or("")
            .contains("regular file"),
        "{:?}",
        state.corrupt_reason
    );
}

/// Compaction of a hostile 64 MiB audit file reads only a bounded tail:
/// the trail lands bounded, the new decision stays auditable, and the
/// record is still confirmed afterwards.
#[test]
fn audit_compaction_of_a_hostile_oversized_file_stays_bounded() {
    let dir = tmp();
    let data_dir = dir.path();
    std::fs::create_dir_all(data_dir).unwrap();
    {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(audit_path(data_dir))
            .unwrap();
        // Sparse zeros carry no newline, so the retained tail is empty and
        // only the new entry survives — but the READ itself must never
        // materialize the 64 MiB file.
        file.set_len(64 * 1024 * 1024).unwrap();
    }
    record_decision(data_dir, SandboxShellChoice::FailClosed, 11).unwrap();
    let compacted = std::fs::read_to_string(audit_path(data_dir)).unwrap();
    assert!(compacted.contains("sandbox_choice_fail_closed_recorded"));
    assert!(
        compacted.len() <= 1024 * 1024,
        "compaction must stay bounded: {} bytes",
        compacted.len()
    );
    let state = inspect(data_dir, &Config::default());
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert!(state.record.is_some());
}
