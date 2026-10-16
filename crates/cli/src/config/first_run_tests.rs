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
/// with no audit trail behind it and no valid MAC (a hand-forged document).
fn forged_grant_record() -> Vec<u8> {
    serde_json::to_vec_pretty(&SandboxFirstRunRecord {
        version: SANDBOX_FIRST_RUN_VERSION,
        choice: SandboxShellChoice::NetworkCapableUserGranted,
        decided_at_ms: 42,
        platform: std::env::consts::OS.to_string(),
        network_backend_available: network_backend_available(),
        trade_off: NETWORK_CAPABLE_SHELL_TRADE_OFF.to_string(),
        mac: String::new(),
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
        .contains("HMAC"));
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

// ---------------------------------------------------------------------------
// Keyed tamper evidence (HMAC-SHA256): the daemon-local key, the decision
// document MAC and the chained audit MACs.
// ---------------------------------------------------------------------------

/// First use creates the key ONCE (32 random bytes, owner-only on unix) and
/// reuses it for every later record; removing it over an existing decision
/// is a typed refusal on every read (no grant), and `record_decision` never
/// silently mints a fresh key over that decision. A fresh data dir still
/// onboards normally.
#[test]
fn missing_key_is_created_once_on_first_use_and_removal_fails_closed() {
    let dir = tmp();
    let data_dir = dir.path();

    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();
    let key = std::fs::read(key_path(data_dir)).expect("first use creates the key");
    assert_eq!(key.len(), 32, "the key is 32 random bytes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(key_path(data_dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the key is owner-only");
    }

    // A second record reuses the exact same key (never re-created).
    record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 2).unwrap();
    assert_eq!(std::fs::read(key_path(data_dir)).unwrap(), key);

    // Removal: every read refuses typed (fail closed), and recording over
    // the keyless decision is refused instead of silently re-keying.
    std::fs::remove_file(key_path(data_dir)).unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(
        state.record.is_none(),
        "a keyless decision is not presented"
    );
    assert!(
        state
            .corrupt_reason
            .as_deref()
            .unwrap_or("")
            .contains("integrity key"),
        "{:?}",
        state.corrupt_reason
    );
    assert_eq!(cfg.sandbox.shell, None, "no grant without the key");
    let err = record_decision(data_dir, SandboxShellChoice::FailClosed, 3).unwrap_err();
    assert!(err.contains("refusing to mint"), "{err}");

    // A fresh directory still onboards on first use.
    let fresh = tmp();
    record_decision(fresh.path(), SandboxShellChoice::FailClosed, 4).unwrap();
    assert!(key_path(fresh.path()).is_file());
}

/// A decision document whose fields were edited (or whose MAC was stripped)
/// fails its HMAC typed: fail-closed, never a grant — and restoring the
/// exact recorded document verifies again.
#[test]
fn tampered_decision_document_fails_its_hmac_and_grants_nothing() {
    let dir = tmp();
    let data_dir = dir.path();
    let recorded =
        record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 42).unwrap();
    assert!(
        recorded.mac.len() == 64 && recorded.mac.bytes().all(|b| b.is_ascii_hexdigit()),
        "the record carries an HMAC"
    );

    let refuse = |flip: fn(&mut SandboxFirstRunRecord)| {
        let mut forged = recorded.clone();
        flip(&mut forged);
        std::fs::write(
            decision_path(data_dir),
            serde_json::to_vec_pretty(&forged).unwrap(),
        )
        .unwrap();
        let mut cfg = Config::default();
        let state = apply_effective_choice(&mut cfg, data_dir);
        assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
        assert!(state.record.is_none(), "no tampered document is presented");
        assert!(
            state
                .corrupt_reason
                .as_deref()
                .unwrap_or("")
                .contains("integrity verification"),
            "{:?}",
            state.corrupt_reason
        );
        assert_eq!(cfg.sandbox.shell, None, "no forge path may set the mode");
    };

    // One flipped field (the backend fact), the timestamp bumped, and the
    // MAC stripped entirely: every mutation refuses typed.
    refuse(|r| r.network_backend_available = !r.network_backend_available);
    refuse(|r| r.decided_at_ms += 1);
    refuse(|r| r.mac = String::new());

    // Restoring the exact recorded document is accepted again (the check is
    // exact, never a blanket refusal).
    std::fs::write(
        decision_path(data_dir),
        serde_json::to_vec_pretty(&recorded).unwrap(),
    )
    .unwrap();
    let state = inspect(data_dir, &Config::default());
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert_eq!(state.record.as_ref(), Some(&recorded));
}

/// Editing any audit entry, reordering them, or deleting one from the
/// middle breaks the HMAC chain typed (fail closed); restoring the exact
/// trail verifies again.
#[test]
fn tampered_or_reordered_audit_chain_fails_typed() {
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();
    let recorded =
        record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 2).unwrap();
    let original = std::fs::read_to_string(audit_path(data_dir)).unwrap();
    let lines: Vec<serde_json::Value> = original
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);

    let write_lines = |lines: &[serde_json::Value]| {
        let text: String = lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(audit_path(data_dir), text).unwrap();
    };
    let assert_refused = || {
        let mut cfg = Config::default();
        let state = apply_effective_choice(&mut cfg, data_dir);
        assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
        assert!(state.record.is_none());
        assert!(
            state
                .corrupt_reason
                .as_deref()
                .unwrap_or("")
                .contains("integrity verification"),
            "{:?}",
            state.corrupt_reason
        );
        assert_eq!(cfg.sandbox.shell, None);
    };

    // (1) Edit a field of the entry backing the live decision.
    let mut edited = lines.clone();
    let pid = edited[1]["pid"].as_u64().unwrap();
    edited[1]["pid"] = serde_json::json!(pid + 1);
    write_lines(&edited);
    assert_refused();

    // (2) Edit a field of an EARLIER entry: the chain of the entry after it
    // (and every recomputation) breaks.
    let mut edited = lines.clone();
    edited[0]["trade_off"] = serde_json::json!("forged trade-off");
    write_lines(&edited);
    assert_refused();

    // (3) Reorder the entries: the linkage no longer matches.
    let mut reordered = lines.clone();
    reordered.swap(0, 1);
    write_lines(&reordered);
    assert_refused();

    // (4) A forged MAC on any entry refuses (a same-user attacker without
    // the key cannot recompute).
    let mut forged = lines.clone();
    forged[1]["mac"] = serde_json::json!("a".repeat(64));
    write_lines(&forged);
    assert_refused();

    // (5) Restoring the exact trail verifies again.
    std::fs::write(audit_path(data_dir), &original).unwrap();
    let state = inspect(data_dir, &Config::default());
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert_eq!(state.record.as_ref(), Some(&recorded));
}

/// A rolled-back trail cannot back the live decision: restoring an earlier
/// audit snapshot, truncating the backing entry, or deleting a middle entry
/// all leave the document unmatched or the chain broken — typed refusal,
/// fail closed; the untampered trail verifies again.
#[test]
fn rolled_back_or_truncated_audit_chain_fails_typed() {
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();
    let pre_grant = std::fs::read(audit_path(data_dir)).unwrap();
    record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 2).unwrap();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 3).unwrap();
    let full_trail = std::fs::read_to_string(audit_path(data_dir)).unwrap();
    let live_lines: Vec<&str> = full_trail.lines().collect();
    assert_eq!(live_lines.len(), 3, "three recorded decisions");

    let assert_refused = || {
        let mut cfg = Config::default();
        let state = apply_effective_choice(&mut cfg, data_dir);
        assert_eq!(
            state.decision,
            SandboxFirstRunDecision::FailClosed,
            "a rolled-back trail never grants"
        );
        assert!(state.corrupt_reason.is_some());
        assert_eq!(cfg.sandbox.shell, None);
    };

    // (1) Roll the trail back to before the non-default decisions: the live
    // document matches no chain-verified entry.
    std::fs::write(audit_path(data_dir), &pre_grant).unwrap();
    assert_refused();

    // (2) Truncate the last entry (a rollback that leaves the rest intact):
    // the live decision's entry is gone.
    std::fs::write(
        audit_path(data_dir),
        live_lines[..live_lines.len() - 1].join("\n") + "\n",
    )
    .unwrap();
    assert_refused();

    // (3) Delete a MIDDLE entry: the chain linkage of everything after it
    // breaks.
    let mut middle_deleted = live_lines.clone();
    middle_deleted.remove(1);
    std::fs::write(audit_path(data_dir), middle_deleted.join("\n") + "\n").unwrap();
    assert_refused();

    // (4) The untampered current trail verifies again (the check is exact).
    std::fs::write(audit_path(data_dir), &full_trail).unwrap();
    let state = inspect(data_dir, &Config::default());
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert!(state.record.is_some());
}

/// A trail that no longer authenticates under the CURRENT key (a forged
/// well-formed entry, or a replaced key) refuses further recording typed:
/// a successful `record_decision` never produces a result that cannot
/// verify. Deliberate re-onboarding (removing the artifacts) starts a fresh
/// authenticated chain.
#[test]
fn recording_over_an_unauthenticated_or_rotated_key_trail_is_refused() {
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();

    // (1) A forged entry with well-formed-shaped MACs appended to the
    // authentic trail: reads refuse, and recording refuses (no poisoning).
    let forged = serde_json::json!({
        "event": "sandbox_network_capable_shell_granted",
        "choice": "network_capable_user_granted",
        "decided_at_ms": 2,
        "platform": std::env::consts::OS,
        "network_backend_available": false,
        "trade_off": NETWORK_CAPABLE_SHELL_TRADE_OFF,
        "pid": 1,
        "prev_mac": "a".repeat(64),
        "mac": "b".repeat(64),
    });
    let mut trail = std::fs::read_to_string(audit_path(data_dir)).unwrap();
    trail.push_str(&serde_json::to_string(&forged).unwrap());
    trail.push('\n');
    std::fs::write(audit_path(data_dir), trail).unwrap();
    let state = inspect(data_dir, &Config::default());
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_some(), "the forgery is reported");
    let err = record_decision(data_dir, SandboxShellChoice::FailClosed, 3).unwrap_err();
    assert!(err.contains("refusing to record"), "{err}");

    // (2) A replaced key: the same typed refusal, and reads stay fail-closed.
    let fresh = tmp();
    record_decision(fresh.path(), SandboxShellChoice::FailClosed, 4).unwrap();
    let mut rotated = [0u8; 32];
    rotated[0] = 7;
    std::fs::write(key_path(fresh.path()), rotated).unwrap();
    // The replacement stays owner-only: the read path refuses an
    // other-accessible key before it even looks at the bytes (that refusal
    // has its own test), so this stays the rotated-KEY path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            key_path(fresh.path()),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let state = inspect(fresh.path(), &Config::default());
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_some());
    let err = record_decision(fresh.path(), SandboxShellChoice::FailClosed, 5).unwrap_err();
    assert!(err.contains("refusing to record"), "{err}");

    // (3) Deliberate re-onboarding: removing the artifacts lets first use
    // start a fresh authenticated chain.
    std::fs::remove_file(decision_path(fresh.path())).unwrap();
    std::fs::remove_file(audit_path(fresh.path())).unwrap();
    std::fs::remove_file(key_path(fresh.path())).unwrap();
    record_decision(
        fresh.path(),
        SandboxShellChoice::NetworkCapableUserGranted,
        6,
    )
    .unwrap();
    let state = inspect(fresh.path(), &Config::default());
    assert_eq!(
        state.decision,
        SandboxFirstRunDecision::NetworkCapableUserGranted
    );
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
}

// ---------------------------------------------------------------------------
// Open-once no-follow handle validation (TOCTOU). Every artifact is opened
// ONCE through the anchored data-dir authority, the OPENED HANDLE is
// fstat-validated as a regular file and every byte is read from that SAME
// handle. The seam fires in the exact inspect->read window, so the swap is
// deterministic: the read must operate on the original handle or refuse —
// never on the swapped-in entry.
// ---------------------------------------------------------------------------

/// Serializes the tests that install the process-global artifact read seam.
static SEAM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn seam_guard() -> std::sync::MutexGuard<'static, ()> {
    SEAM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The key read (record path): swap the key entry for a symlink to a decoy
/// key AFTER the handle was opened and validated. The read must use the
/// VALIDATED HANDLE, so the record is signed under the original key — a
/// re-open by pathname would have read the decoy and the trail check would
/// have refused under it.
#[cfg(unix)]
#[test]
fn key_read_never_follows_a_swap_planted_after_handle_validation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let _guard = seam_guard();
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();
    let original_key = std::fs::read(key_path(data_dir)).unwrap();
    let decoy = dir.path().join("decoy-key");
    std::fs::write(&decoy, [0x41u8; 32]).unwrap();
    let held = dir.path().join("held-key");

    let target = key_path(data_dir);
    let swapped = Arc::new(AtomicBool::new(false));
    let flag = swapped.clone();
    let (hook_target, hook_decoy, hook_held) = (target.clone(), decoy.clone(), held.clone());
    install_artifact_read_seam(Box::new(move |label| {
        if label == hook_target && !flag.swap(true, Ordering::SeqCst) {
            std::fs::rename(&hook_target, &hook_held).unwrap();
            std::os::unix::fs::symlink(&hook_decoy, &hook_target).unwrap();
        }
    }));
    record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 2).unwrap();
    clear_artifact_read_seam();
    assert!(swapped.load(Ordering::SeqCst), "the seam must have fired");

    // Restore the original key entry: the recorded pair verifies under the
    // ORIGINAL key — only possible if the read used the original handle.
    std::fs::remove_file(key_path(data_dir)).unwrap();
    std::fs::rename(&held, key_path(data_dir)).unwrap();
    let state = inspect(data_dir, &Config::default());
    assert_eq!(
        state.decision,
        SandboxFirstRunDecision::NetworkCapableUserGranted
    );
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert_eq!(std::fs::read(key_path(data_dir)).unwrap(), original_key);
}

/// The audit read (confirmation path): swap the audit entry for a symlink
/// to a forged decoy trail AFTER the handle was opened and validated. The
/// confirmation must read the ORIGINAL handle and still confirm the grant —
/// a re-open by pathname would have read the decoy, matched nothing and
/// refused.
#[cfg(unix)]
#[test]
fn audit_read_never_follows_a_swap_planted_after_handle_validation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let _guard = seam_guard();
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 7).unwrap();
    let trail_before = std::fs::read(audit_path(data_dir)).unwrap();
    let decoy = dir.path().join("decoy-audit.jsonl");
    std::fs::write(&decoy, b"{\"event\":\"forged-entry\"}\n").unwrap();
    let held = dir.path().join("held-audit.jsonl");

    let target = audit_path(data_dir);
    let swapped = Arc::new(AtomicBool::new(false));
    let flag = swapped.clone();
    let (hook_target, hook_decoy, hook_held) = (target.clone(), decoy.clone(), held.clone());
    install_artifact_read_seam(Box::new(move |label| {
        if label == hook_target && !flag.swap(true, Ordering::SeqCst) {
            std::fs::rename(&hook_target, &hook_held).unwrap();
            std::os::unix::fs::symlink(&hook_decoy, &hook_target).unwrap();
        }
    }));
    let state = inspect(data_dir, &Config::default());
    clear_artifact_read_seam();
    assert!(swapped.load(Ordering::SeqCst), "the seam must have fired");
    assert_eq!(
        state.decision,
        SandboxFirstRunDecision::NetworkCapableUserGranted
    );
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);

    // The byte-verbatim trail survived the read untouched; restore it.
    std::fs::remove_file(audit_path(data_dir)).unwrap();
    std::fs::rename(&held, audit_path(data_dir)).unwrap();
    assert_eq!(std::fs::read(audit_path(data_dir)).unwrap(), trail_before);
    let state = inspect(data_dir, &Config::default());
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
}

/// The decision read (inspect path): swap the decision entry for a symlink
/// to a different (fail-closed, valid-hex-MAC) document AFTER the handle
/// was opened and validated. Inspect must resolve the ORIGINAL handle — a
/// re-open by pathname would have loaded the decoy and refused it against
/// the grant trail.
#[cfg(unix)]
#[test]
fn decision_read_never_follows_a_swap_planted_after_handle_validation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let _guard = seam_guard();
    let dir = tmp();
    let data_dir = dir.path();
    let recorded =
        record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 7).unwrap();
    let document_before = std::fs::read(decision_path(data_dir)).unwrap();
    let decoy = dir.path().join("decoy-decision.json");
    std::fs::write(
        &decoy,
        serde_json::to_vec_pretty(&SandboxFirstRunRecord {
            version: SANDBOX_FIRST_RUN_VERSION,
            choice: SandboxShellChoice::FailClosed,
            decided_at_ms: 99,
            platform: std::env::consts::OS.to_string(),
            network_backend_available: false,
            trade_off: String::new(),
            mac: "0".repeat(64),
        })
        .unwrap(),
    )
    .unwrap();
    let held = dir.path().join("held-decision.json");

    let target = decision_path(data_dir);
    let swapped = Arc::new(AtomicBool::new(false));
    let flag = swapped.clone();
    let (hook_target, hook_decoy, hook_held) = (target.clone(), decoy.clone(), held.clone());
    install_artifact_read_seam(Box::new(move |label| {
        if label == hook_target && !flag.swap(true, Ordering::SeqCst) {
            std::fs::rename(&hook_target, &hook_held).unwrap();
            std::os::unix::fs::symlink(&hook_decoy, &hook_target).unwrap();
        }
    }));
    let state = inspect(data_dir, &Config::default());
    clear_artifact_read_seam();
    assert!(swapped.load(Ordering::SeqCst), "the seam must have fired");
    assert_eq!(
        state.decision,
        SandboxFirstRunDecision::NetworkCapableUserGranted
    );
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert_eq!(state.record.as_ref(), Some(&recorded));

    std::fs::remove_file(decision_path(data_dir)).unwrap();
    std::fs::rename(&held, decision_path(data_dir)).unwrap();
    assert_eq!(
        std::fs::read(decision_path(data_dir)).unwrap(),
        document_before
    );
}

/// The append snapshot read (record path): swap the audit entry for a
/// symlink to a decoy AFTER the append snapshot's handle was opened and
/// validated (the verification walk already read the original entry). The
/// snapshot must come from the ORIGINAL handle — the published trail
/// retains the original content — and the anchored rename replaces the link
/// ENTRY, so the decoy target is never read or written.
#[cfg(unix)]
#[test]
fn append_snapshot_never_reads_a_swap_and_the_publish_replaces_the_link() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let _guard = seam_guard();
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();
    let trail_before = std::fs::read_to_string(audit_path(data_dir)).unwrap();
    let decoy = dir.path().join("decoy.jsonl");
    std::fs::write(&decoy, "decoy must never be read or written\n").unwrap();
    let held = dir.path().join("held-audit.jsonl");

    let target = audit_path(data_dir);
    // The 1st audit read of this record is the verification walk; the 2nd
    // is the append snapshot. Swap in the snapshot's seam.
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    let (hook_target, hook_decoy, hook_held) = (target.clone(), decoy.clone(), held.clone());
    install_artifact_read_seam(Box::new(move |label| {
        if label == hook_target && counter.fetch_add(1, Ordering::SeqCst) == 1 {
            std::fs::rename(&hook_target, &hook_held).unwrap();
            std::os::unix::fs::symlink(&hook_decoy, &hook_target).unwrap();
        }
    }));
    record_decision(data_dir, SandboxShellChoice::FailClosed, 2).unwrap();
    clear_artifact_read_seam();
    assert!(reads.load(Ordering::SeqCst) >= 2, "both reads must fire");

    // The snapshot came from the validated handle: the ORIGINAL trail is
    // retained and the new chained line lands on top of it.
    let trail = std::fs::read_to_string(audit_path(data_dir)).unwrap();
    assert!(
        trail.starts_with(&trail_before),
        "the original trail must be retained: {trail}"
    );
    assert_eq!(trail.lines().count(), 2);
    // The rename replaced the link ENTRY; the decoy target is untouched.
    assert_eq!(
        std::fs::read_to_string(&decoy).unwrap(),
        "decoy must never be read or written\n"
    );
    let state = inspect(data_dir, &Config::default());
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    // No staged temp survives the publish.
    let names: Vec<String> = std::fs::read_dir(data_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !names.iter().any(|n| n.contains(".faktor-tmp-")),
        "staged temp leaked: {names:?}"
    );
}

/// FIFO, symlink (even to a device) and socket objects planted at every
/// artifact path are refused typed — a FIFO must never block a read, a
/// symlink must never be followed, and a symlinked audit trail must refuse
/// recording without touching the link's target.
#[cfg(unix)]
#[test]
fn non_regular_objects_at_every_artifact_path_are_refused_typed() {
    let dir = tmp();
    let data_dir = dir.path();
    std::fs::create_dir_all(data_dir).unwrap();

    // (1) FIFO at the KEY path: refused WITHOUT blocking (non-blocking
    // open + handle fstat). A blocking open would hang this test.
    let status = std::process::Command::new("mkfifo")
        .arg(key_path(data_dir))
        .status()
        .expect("mkfifo must be available on unix hosts");
    assert!(status.success());
    let err = record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap_err();
    assert!(err.contains("regular file"), "{err}");
    std::fs::remove_file(key_path(data_dir)).unwrap();

    // (2) Symlink at the KEY path (to a device): refused, never followed.
    std::os::unix::fs::symlink("/dev/zero", key_path(data_dir)).unwrap();
    let err = record_decision(data_dir, SandboxShellChoice::FailClosed, 2).unwrap_err();
    assert!(err.contains("regular file"), "{err}");
    std::fs::remove_file(key_path(data_dir)).unwrap();

    // (3) FIFO at the DECISION path: inspect refuses typed, no block.
    let fifo = decision_path(data_dir);
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must be available on unix hosts");
    assert!(status.success());
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

    // (4) Unix socket at the DECISION path: refused typed (special file).
    let socket_path = decision_path(data_dir);
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_some(), "{:?}", state.corrupt_reason);
    drop(listener);
    std::fs::remove_file(&socket_path).unwrap();

    // (5) A symlinked audit trail refuses RECORDING through the anchored
    // write path, and the link's target stays byte-identical.
    record_decision(data_dir, SandboxShellChoice::FailClosed, 3).unwrap();
    let real = data_dir.join("real-audit.jsonl");
    std::fs::rename(audit_path(data_dir), &real).unwrap();
    let decoy_target = dir.path().join("decoy-target.jsonl");
    std::fs::write(&decoy_target, "must survive\n").unwrap();
    std::os::unix::fs::symlink(&decoy_target, audit_path(data_dir)).unwrap();
    let err = record_decision(data_dir, SandboxShellChoice::FailClosed, 4).unwrap_err();
    assert!(err.contains("regular file"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&decoy_target).unwrap(),
        "must survive\n"
    );
    // The refused record never advanced the decision document.
    let live = std::fs::read(decision_path(data_dir)).unwrap();
    let live: SandboxFirstRunRecord = serde_json::from_slice(&live).unwrap();
    assert_eq!(live.decided_at_ms, 3);
}

/// An owner-other-accessible integrity key (`0644`) is refused typed: a key
/// any other identity can read or write cannot anchor the tamper evidence.
#[cfg(unix)]
#[test]
fn an_other_accessible_integrity_key_is_refused_typed() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tmp();
    let data_dir = dir.path();
    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();
    std::fs::set_permissions(key_path(data_dir), std::fs::Permissions::from_mode(0o644)).unwrap();

    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.record.is_none());
    assert!(
        state
            .corrupt_reason
            .as_deref()
            .unwrap_or("")
            .contains("accessible to other users"),
        "{:?}",
        state.corrupt_reason
    );
    assert_eq!(cfg.sandbox.shell, None);
    let err = record_decision(data_dir, SandboxShellChoice::FailClosed, 2).unwrap_err();
    assert!(err.contains("accessible to other users"), "{err}");
}

/// Absent artifacts (and an absent data dir) read as absent, never as
/// errors: `Ok(None)` from the raw decode and a corruption-free state.
#[test]
fn absent_artifacts_and_an_absent_data_dir_read_as_absent_never_errors() {
    let dir = tmp();
    let data_dir = dir.path().join("never-created");
    assert!(load_decision(&data_dir).unwrap().is_none());
    let state = inspect(&data_dir, &Config::default());
    assert!(state.record.is_none());
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);

    std::fs::create_dir_all(&data_dir).unwrap();
    assert!(load_decision(&data_dir).unwrap().is_none());
    let state = inspect(&data_dir, &Config::default());
    assert!(state.record.is_none());
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
}

/// Pins the documented freshness semantics EXACTLY. The keyed MAC chain
/// proves integrity BETWEEN RETAINED ENTRIES, never freshness of the head:
/// bounded compaction legitimately anchors a new first entry, so a
/// byte-verbatim restore of a previously-valid (decision document + trail)
/// pair — a rollback/replay of an earlier valid state — VERIFIES and is
/// ACCEPTED. No local file-domain check can do better (a same-user attacker
/// who can restore those bytes controls every local file); anti-rollback
/// needs a monotonic head in a different state domain (the documented
/// upgrade path). What IS still refused: restoring an earlier trail WITHOUT
/// its matching document (the live document matches no chain-verified
/// entry).
#[test]
fn rollback_of_a_previously_valid_pair_is_accepted_and_unmatched_restores_are_refused() {
    let dir = tmp();
    let data_dir = dir.path();
    // State A: explicit fail-closed. State B: the grant. Live state: fail
    // closed again (3).
    record_decision(data_dir, SandboxShellChoice::FailClosed, 1).unwrap();
    let pair_a = (
        std::fs::read(decision_path(data_dir)).unwrap(),
        std::fs::read(audit_path(data_dir)).unwrap(),
    );
    let granted =
        record_decision(data_dir, SandboxShellChoice::NetworkCapableUserGranted, 2).unwrap();
    let pair_b = (
        std::fs::read(decision_path(data_dir)).unwrap(),
        std::fs::read(audit_path(data_dir)).unwrap(),
    );
    record_decision(data_dir, SandboxShellChoice::FailClosed, 3).unwrap();
    let state = inspect(data_dir, &Config::default());
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);

    // (1) Verbatim rollback of the GRANT pair: ACCEPTED — the documented
    // guarantee does not (and cannot) detect a previously-valid state
    // restored wholesale. This is the exact semantics the module documents.
    std::fs::write(decision_path(data_dir), &pair_b.0).unwrap();
    std::fs::write(audit_path(data_dir), &pair_b.1).unwrap();
    let state = inspect(data_dir, &Config::default());
    assert_eq!(
        state.decision,
        SandboxFirstRunDecision::NetworkCapableUserGranted,
        "a byte-verbatim restore of a previously-valid pair is accepted (freshness is NOT proven)"
    );
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert_eq!(state.record.as_ref().map(|r| r.decided_at_ms), Some(2));
    assert_eq!(state.record.as_ref().map(|r| &r.mac), Some(&granted.mac));

    // (2) The earlier TRAIL alone (a rollback to a shorter trail) with the
    // grant document still live: REFUSED — the live document matches no
    // chain-verified entry. The chain proves consistency between the pair,
    // not freshness of the head.
    std::fs::write(audit_path(data_dir), &pair_a.1).unwrap();
    let mut cfg = Config::default();
    let state = apply_effective_choice(&mut cfg, data_dir);
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_some());
    assert_eq!(cfg.sandbox.shell, None, "no unmatched restore may grant");

    // (3) The earlier fail-closed PAIR restored verbatim is accepted again
    // (the check is exact; a rollback to a valid fail-closed state is a
    // no-op for the effective policy).
    std::fs::write(decision_path(data_dir), &pair_a.0).unwrap();
    let state = inspect(data_dir, &Config::default());
    assert_eq!(state.decision, SandboxFirstRunDecision::FailClosed);
    assert!(state.corrupt_reason.is_none(), "{:?}", state.corrupt_reason);
    assert_eq!(state.record.as_ref().map(|r| r.decided_at_ms), Some(1));
}
