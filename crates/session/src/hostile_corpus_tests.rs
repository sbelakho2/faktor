//! Hostile-corpus certification for the session runtime (production
//! `SessionManager`/`SessionHandle` wiring, never re-implemented logic).
//!
//! Categories:
//! 1. crash recovery / replay: every recovery strategy, hostile run rows,
//!    malformed descriptors, reopen determinism, contradictions, interrupted
//!    turns, durable permission expiry at recovery;
//! 2. queued-prompt scheduling and permission lifecycle: bounds, FIFO
//!    admission, corrupt queue rows, status transitions, cancellation,
//!    expiry, suspend/resume/end-session invariants.

use std::sync::Arc;

use faktor_core::capability::{Capability, PermissionDecision};
use faktor_core::event::EventKind;
use faktor_core::hash::FileHash;
use faktor_core::id::OpId;
use faktor_core::op::{EffectStatus, OpMeta, RecoveryStrategy};
use faktor_core::state::AgentState;
use faktor_core::time::{Deadline, TestClock};

use crate::handle::tests::{session, test_manager};
use crate::recovery::{RecoveryAction, RecoveryReport};
use crate::{SessionHandle, SessionManager};

fn blake3_of(bytes: &[u8]) -> FileHash {
    FileHash::from(blake3::hash(bytes).into())
}

fn make_meta(s: &SessionHandle, m: &SessionManager, recovery: RecoveryStrategy) -> (OpMeta, OpId) {
    let op = m.try_next_op_id().unwrap();
    let meta = OpMeta::new(
        op,
        s.id(),
        Deadline::at(m.now_ms() + 60_000),
        faktor_core::retry::RetryPolicy::default(),
        faktor_core::cancellation::CancellationToken::new(),
        recovery,
        m.now_ms(),
    );
    (meta, op)
}

fn drive_to_waiting_permission(s: &SessionHandle) -> (OpId, crate::ops::PermissionRequest) {
    let turn = s.submit_prompt("x", &[]).unwrap().op_id;
    s.append_event(
        EventKind::ContextPrepared,
        AgentState::BuildingContext,
        Some(turn),
        None,
    )
    .unwrap();
    s.append_event(
        EventKind::ModelStarted,
        AgentState::WaitingForModel,
        Some(turn),
        None,
    )
    .unwrap();
    s.append_event(
        EventKind::ModelChunkReceived,
        AgentState::Streaming,
        Some(turn),
        None,
    )
    .unwrap();
    let request = s
        .request_permission(
            turn,
            &Capability::ReadWorkspace {
                path: "/w/a".into(),
            },
        )
        .unwrap();
    (turn, request)
}

fn workspace_session(
    m: &Arc<SessionManager>,
    base: &std::path::Path,
) -> (SessionHandle, std::path::PathBuf) {
    let root = base.join("ws-hostile");
    std::fs::create_dir_all(&root).unwrap();
    let ws = m.create_workspace(root.to_str().unwrap()).unwrap();
    let s = m.create_session(ws, "t", "ollama", "qwen3.8").unwrap();
    (s, root)
}

fn recover_one(
    m: &Arc<SessionManager>,
    s: &SessionHandle,
    strategy: RecoveryStrategy,
    tool: &str,
    args: serde_json::Value,
) -> (OpId, RecoveryReport) {
    drive_to_waiting_permission(s);
    let (meta, op) = make_meta(s, m, strategy);
    s.start_tool_run(meta, tool, args).unwrap();
    let report = s.recover_all().unwrap();
    (op, report)
}

// =====================================================================
// Category 1: crash recovery / replay
// =====================================================================

#[test]
fn session_recovery_strategy_hostile_matrix() {
    let (_d, m) = test_manager();
    let mut case = 0usize;

    type StrategyCheck = (RecoveryStrategy, fn(&RecoveryAction) -> bool, &'static str);
    let non_verify: [StrategyCheck; 4] = [
        (
            RecoveryStrategy::None,
            |a| matches!(a, RecoveryAction::NoAction),
            "interrupted",
        ),
        (
            RecoveryStrategy::Idempotent,
            |a| matches!(a, RecoveryAction::RerunAllowed),
            "failed",
        ),
        (
            RecoveryStrategy::Manual,
            |a| matches!(a, RecoveryAction::NeedsHuman),
            "interrupted",
        ),
        (
            RecoveryStrategy::MarkUnknown,
            |a| matches!(a, RecoveryAction::UnknownEffect),
            "interrupted",
        ),
    ];
    let payloads = [
        ("empty", serde_json::json!({})),
        (
            "quotes",
            serde_json::json!({"q": "'; DROP TABLE tool_run; --"}),
        ),
        ("nul", serde_json::json!({"n": "a\u{0}b"})),
        ("4k", serde_json::json!({"blob": "x".repeat(4096)})),
    ];
    for (strategy, check, expected_status) in &non_verify {
        for (label, args) in &payloads {
            case += 1;
            let s = session(&m);
            let (op, report) = recover_one(&m, &s, strategy.clone(), "run_test", args.clone());
            assert_eq!(
                report.crashed_ops.len(),
                1,
                "recovery case #{case} ({strategy:?}/{label}): exactly one crashed op"
            );
            let recovered = &report.crashed_ops[0];
            assert_eq!(
                recovered.op_id, op,
                "recovery case #{case} ({strategy:?}/{label}): op identity"
            );
            assert_eq!(
                recovered.status, *expected_status,
                "recovery case #{case} ({strategy:?}/{label}): status must be the documented one"
            );
            assert_eq!(
                recovered.effect,
                EffectStatus::Unknown,
                "recovery case #{case} ({strategy:?}/{label}): effect stays unknown"
            );
            assert!(
                check(&recovered.action),
                "recovery case #{case} ({strategy:?}/{label}): wrong action {:?}",
                recovered.action
            );
            assert!(
                s.pending_tool_runs().unwrap().is_empty(),
                "recovery case #{case} ({strategy:?}/{label}): the row is terminal"
            );
            case += 1;
            assert!(
                !s.recover_all().unwrap().applied,
                "recovery case #{case} ({strategy:?}/{label}): a second sweep is a no-op"
            );
        }
    }

    let hostile_tools: [(&str, String); 2] = [
        ("quotes", "run'; DROP TABLE tool_run; --".into()),
        ("unicode", "工具-éxéc".into()),
    ];
    for (strategy, check, expected_status) in &non_verify {
        for (label, tool) in &hostile_tools {
            case += 1;
            let s = session(&m);
            let (_, report) = recover_one(&m, &s, strategy.clone(), tool, serde_json::json!({}));
            assert_eq!(
                report.crashed_ops.len(),
                1,
                "recovery case #{case} ({strategy:?}/tool {label}): one recovered op"
            );
            assert_eq!(
                report.crashed_ops[0].tool, *tool,
                "recovery case #{case} ({strategy:?}/tool {label}): tool name roundtrips"
            );
            assert_eq!(
                report.crashed_ops[0].status, *expected_status,
                "recovery case #{case} ({strategy:?}/tool {label}): status"
            );
            assert!(
                check(&report.crashed_ops[0].action),
                "recovery case #{case} ({strategy:?}/tool {label}): action"
            );
            assert_eq!(
                s.state().unwrap(),
                AgentState::FailedRecoverable,
                "recovery case #{case} ({strategy:?}/tool {label}): crash lands recoverable"
            );
        }
    }
    case += 1;
    let s = session(&m);
    let _ = drive_to_waiting_permission(&s);
    let (meta, _op) = make_meta(&s, &m, RecoveryStrategy::None);
    let err = s
        .start_tool_run(meta, &"t".repeat(4096), serde_json::json!({}))
        .expect_err("recovery case: an oversized tool name must be refused typed");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Malformed,
        "recovery case #{case}: 4 KiB tool name is Malformed, never truncated or accepted"
    );
    assert_eq!(case, 41, "strategy corpus size drifted");
}

#[test]
fn session_recovery_verify_hash_filesystem_matrix() {
    let (_d, m) = test_manager();
    let mut case = 0usize;

    case += 1;
    let (s, root) = workspace_session(&m, _d.path());
    let _ = drive_to_waiting_permission(&s);
    let bytes = b"landed";
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("a.txt"), bytes).unwrap();
    let expected = blake3_of(bytes);
    let legacy = root.join("sub").join("a.txt").to_string_lossy().to_string();
    let (meta, op) = make_meta(
        &s,
        &m,
        RecoveryStrategy::VerifyHash {
            path: legacy,
            expected,
        },
    );
    s.start_tool_run(meta, "write_file", serde_json::json!({}))
        .unwrap();
    let report = s.recover_all().unwrap();
    assert_eq!(
        report.crashed_ops[0].action,
        RecoveryAction::Verified {
            expected,
            actual: expected
        },
        "verify case #{case}: a matching file proves completion"
    );
    assert_eq!(
        report.crashed_ops[0].status, "completed",
        "verify case #{case}"
    );
    assert_eq!(
        report.crashed_ops[0].effect,
        EffectStatus::Verified,
        "verify case #{case}"
    );
    assert_eq!(report.crashed_ops[0].op_id, op, "verify case #{case}");

    case += 1;
    let (s, root) = workspace_session(&m, _d.path());
    drive_to_waiting_permission(&s);
    std::fs::write(root.join("a.txt"), b"other bytes").unwrap();
    let expected = FileHash::from([7; 32]);
    let (meta, _op) = make_meta(
        &s,
        &m,
        RecoveryStrategy::VerifyHash {
            path: root.join("a.txt").to_string_lossy().to_string(),
            expected,
        },
    );
    s.start_tool_run(meta, "write_file", serde_json::json!({}))
        .unwrap();
    let report = s.recover_all().unwrap();
    assert_eq!(
        report.crashed_ops[0].action,
        RecoveryAction::NotApplied {
            expected,
            actual: Some(blake3_of(b"other bytes"))
        },
        "verify case #{case}: a mismatching file proves the write never landed"
    );
    assert_eq!(
        report.crashed_ops[0].effect,
        EffectStatus::Failed,
        "verify case #{case}"
    );

    case += 1;
    let (s, root) = workspace_session(&m, _d.path());
    drive_to_waiting_permission(&s);
    let expected = FileHash::from([7; 32]);
    let (meta, _op) = make_meta(
        &s,
        &m,
        RecoveryStrategy::VerifyHash {
            path: root.join("missing.txt").to_string_lossy().to_string(),
            expected,
        },
    );
    s.start_tool_run(meta, "write_file", serde_json::json!({}))
        .unwrap();
    let report = s.recover_all().unwrap();
    assert_eq!(
        report.crashed_ops[0].action,
        RecoveryAction::NotApplied {
            expected,
            actual: None
        },
        "verify case #{case}: a missing file reports actual=None, never a fabricated hash"
    );

    case += 1;
    let (s, root) = workspace_session(&m, _d.path());
    drive_to_waiting_permission(&s);
    std::fs::create_dir_all(root.join("trap")).unwrap();
    let expected = FileHash::from([9; 32]);
    let (meta, _op) = make_meta(
        &s,
        &m,
        RecoveryStrategy::VerifyHash {
            path: root.join("trap").to_string_lossy().to_string(),
            expected,
        },
    );
    s.start_tool_run(meta, "write_file", serde_json::json!({}))
        .unwrap();
    let report = s.recover_all().unwrap();
    assert!(
        matches!(
            report.crashed_ops[0].action,
            RecoveryAction::NotApplied { actual: None, .. } | RecoveryAction::NeedsHuman
        ),
        "verify case #{case}: a directory is never hashable evidence: {:?}",
        report.crashed_ops[0].action
    );

    case += 1;
    let (s, root) = workspace_session(&m, _d.path());
    drive_to_waiting_permission(&s);
    let outside = root.join("..").join("outside.txt");
    std::fs::write(&outside, b"outside").unwrap();
    let expected = blake3_of(b"outside");
    let (meta, _op) = make_meta(
        &s,
        &m,
        RecoveryStrategy::VerifyHash {
            path: root
                .join("sub")
                .join("..")
                .join("..")
                .join("outside.txt")
                .to_string_lossy()
                .to_string(),
            expected,
        },
    );
    s.start_tool_run(meta, "write_file", serde_json::json!({}))
        .unwrap();
    let report = s.recover_all().unwrap();
    assert_eq!(
        report.crashed_ops[0].action,
        RecoveryAction::NeedsHuman,
        "verify case #{case}: a path escaping the workspace must never be read or verified"
    );
    assert_eq!(
        report.crashed_ops[0].effect,
        EffectStatus::Unknown,
        "verify case #{case}: an unprovable path keeps the effect unknown"
    );

    case += 1;
    let (s, _root) = workspace_session(&m, _d.path());
    drive_to_waiting_permission(&s);
    let expected = FileHash::from([1; 32]);
    let (meta, _op) = make_meta(
        &s,
        &m,
        RecoveryStrategy::VerifyHash {
            path: "/etc/hostname".into(),
            expected,
        },
    );
    s.start_tool_run(meta, "write_file", serde_json::json!({}))
        .unwrap();
    let report = s.recover_all().unwrap();
    assert_eq!(
        report.crashed_ops[0].action,
        RecoveryAction::NeedsHuman,
        "verify case #{case}: an absolute outside path is refused before any read"
    );

    case += 1;
    let (s, root) = workspace_session(&m, _d.path());
    drive_to_waiting_permission(&s);
    std::fs::write(root.join("empty.txt"), b"").unwrap();
    let expected = blake3_of(b"");
    let (meta, _op) = make_meta(
        &s,
        &m,
        RecoveryStrategy::VerifyHash {
            path: root.join("empty.txt").to_string_lossy().to_string(),
            expected,
        },
    );
    s.start_tool_run(meta, "write_file", serde_json::json!({}))
        .unwrap();
    let report = s.recover_all().unwrap();
    assert_eq!(
        report.crashed_ops[0].action,
        RecoveryAction::Verified {
            expected,
            actual: expected
        },
        "verify case #{case}: the empty file has a real BLAKE3 identity"
    );
    assert_eq!(case, 7, "filesystem verification matrix size drifted");
}

#[test]
fn session_recovery_reopen_determinism() {
    let mut case = 0usize;
    let strategies: [RecoveryStrategy; 5] = [
        RecoveryStrategy::None,
        RecoveryStrategy::Idempotent,
        RecoveryStrategy::Manual,
        RecoveryStrategy::MarkUnknown,
        RecoveryStrategy::VerifyHash {
            path: "a.txt".into(),
            expected: FileHash::from([3; 32]),
        },
    ];
    for strategy in strategies {
        case += 1;
        let dir = tempfile::tempdir().unwrap();
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let ws = m.create_workspace(root.to_str().unwrap()).unwrap();
        let s = m.create_session(ws, "t", "p", "m").unwrap();
        drive_to_waiting_permission(&s);
        let (meta, op) = make_meta(&s, &m, strategy.clone());
        s.start_tool_run(meta, "run_test", serde_json::json!({}))
            .unwrap();
        let sid = s.id();
        drop(s);
        drop(m);
        let m2 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let s2 = m2.get_session(sid).unwrap().unwrap();
        let report = s2.recover_all().unwrap();
        assert_eq!(
            report.crashed_ops.len(),
            1,
            "reopen-recovery case #{case} ({strategy:?}): the durable row is replayed after restart"
        );
        assert_eq!(
            report.crashed_ops[0].op_id, op,
            "reopen-recovery case #{case} ({strategy:?}): op identity survives reopen"
        );
        assert_eq!(
            report.crashed_ops[0].status,
            match strategy {
                RecoveryStrategy::Idempotent | RecoveryStrategy::VerifyHash { .. } => "failed",
                _ => "interrupted",
            },
            "reopen-recovery case #{case} ({strategy:?}): exact terminal status"
        );
        assert!(
            report.applied,
            "reopen-recovery case #{case} ({strategy:?}): recovery applied"
        );
        case += 1;
        let before = s2.last_event_seq().unwrap().unwrap();
        assert!(
            !s2.recover_all().unwrap().applied,
            "reopen-recovery case #{case} ({strategy:?}): a second sweep after reopen appends nothing"
        );
        assert_eq!(
            s2.last_event_seq().unwrap().unwrap(),
            before,
            "reopen-recovery case #{case} ({strategy:?}): the journal sequence is stable"
        );
    }
    assert_eq!(case, 10, "reopen determinism matrix size drifted");
}

#[test]
fn session_recovery_malformed_and_contradictory_rows() {
    let (_d, m) = test_manager();
    let mut case = 0usize;

    for (label, bad) in [
        ("null", serde_json::json!(null)),
        ("bogus-strategy", serde_json::json!({"strategy": "bogus"})),
        ("array", serde_json::json!([1, 2, 3])),
        (
            "missing-detail",
            serde_json::json!({"strategy": "verify_hash"}),
        ),
    ] {
        case += 1;
        let s = session(&m);
        drive_to_waiting_permission(&s);
        let op = m.try_next_op_id().unwrap();
        m.store()
            .start_tool_run(
                s.id(),
                op,
                "run_test",
                serde_json::json!({}),
                bad,
                None,
                None,
            )
            .unwrap();
        let outcome = s.recover_all();
        let err = outcome.expect_err(&format!(
            "recovery-malformed case #{case} ({label}): corrupt recovery JSON must be a typed error"
        ));
        assert_eq!(
            err.kind,
            faktor_core::error::ErrorKind::Malformed,
            "recovery-malformed case #{case} ({label}): wrong error kind"
        );
        assert!(
            s.pending_tool_runs().unwrap().iter().any(|r| r.op_id == op),
            "recovery-malformed case #{case} ({label}): the unreadable row is never silently terminalized"
        );
    }

    case += 1;
    let s = session(&m);
    drive_to_waiting_permission(&s);
    let op = m.try_next_op_id().unwrap();
    let expected = FileHash::from([2; 32]);
    m.store()
        .start_tool_run(
            s.id(),
            op,
            "write_file",
            serde_json::json!({}),
            serde_json::to_value(RecoveryStrategy::VerifyHash {
                path: "a.txt".into(),
                expected,
            })
            .unwrap(),
            Some("00".repeat(32)),
            None,
        )
        .unwrap();
    let err = s
        .recover_all()
        .expect_err("recovery-malformed case #5: strategy/column disagreement must be loud");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Malformed,
        "recovery-malformed case #{case}: expected_hash column disagreement is tampering"
    );

    case += 1;
    let s = session(&m);
    drive_to_waiting_permission(&s);
    let (meta, op) = make_meta(&s, &m, RecoveryStrategy::None);
    s.start_tool_run(meta, "run_test", serde_json::json!({}))
        .unwrap();
    s.finish_tool_run(op, "completed", EffectStatus::Verified)
        .unwrap();
    let report = s.recover_all().unwrap();
    assert!(
        report.crashed_ops.is_empty() && s.pending_tool_runs().unwrap().is_empty(),
        "recovery-malformed case #{case}: a completed run is never recovered again"
    );

    case += 1;
    let s = session(&m);
    drive_to_waiting_permission(&s);
    let (meta, _op) = make_meta(&s, &m, RecoveryStrategy::None);
    s.start_tool_run(meta, "run_test", serde_json::json!({}))
        .unwrap();
    m.store()
        .sql_execute(&format!(
            "UPDATE session SET state = '\"idle\"' WHERE id = {}",
            s.id().raw()
        ))
        .unwrap();
    let report = s.recover_all().unwrap();
    assert!(
        report.contradiction,
        "recovery-malformed case #{case}: idle session with a running tool row is a reported contradiction"
    );
    assert_eq!(
        s.state().unwrap(),
        AgentState::Idle,
        "recovery-malformed case #{case}: the honest state stands; rows are fixed, state is not fabricated"
    );

    case += 1;
    let s = session(&m);
    drive_to_waiting_permission(&s);
    let report = s.recover_all().unwrap();
    assert!(
        report.interrupted_turn && report.crashed_ops.is_empty(),
        "recovery-malformed case #{case}: an op-active session with no tool rows is an interrupted turn"
    );
    assert_eq!(
        s.state().unwrap(),
        AgentState::WaitingForPermission,
        "recovery-malformed case #{case}: the durable permission point is preserved"
    );
    assert_eq!(case, 8, "COUNT");
}

#[test]
fn session_recovery_expires_pending_permissions_durably() {
    let mut case = 0usize;
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_000_000));
    let m = SessionManager::open_with_clock(
        dir.path().join("store"),
        dir.path().join("cas"),
        true,
        clock.clone(),
    )
    .unwrap();
    let s = session(&m);
    let (_turn, request) = drive_to_waiting_permission(&s);
    let permission_id = request.id;
    let expires = request.expires_ms;

    case += 1;
    let unexpired = s.expire_pending_permissions().unwrap();
    assert!(
        unexpired.is_empty() && unexpired.event_seq.is_none(),
        "recovery-permission case #{case}: a deadline in the future is a no-op"
    );

    case += 1;
    assert!(
        s.pending_permission(permission_id).unwrap().is_some(),
        "recovery-permission case #{case}: the permission is still resolvable before the deadline"
    );

    clock.set(expires + 1);
    case += 1;
    let expired = s.expire_pending_permissions().unwrap();
    assert_eq!(
        expired.expired.len(),
        1,
        "recovery-permission case #{case}: exactly the expired row terminalizes"
    );
    assert_eq!(
        expired.expired[0].0, permission_id,
        "recovery-permission case #{case}: the terminalized id is named"
    );
    assert!(
        expired.event_seq.is_some(),
        "recovery-permission case #{case}: the sweep journals its own event"
    );

    case += 1;
    assert!(
        s.pending_permission(permission_id).unwrap().is_none(),
        "recovery-permission case #{case}: an expired permission is no longer resolvable"
    );
    case += 1;
    let err = s
        .resolve_permission(permission_id, PermissionDecision::Deny)
        .expect_err("recovery-permission case: resolving an expired permission must refuse");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Conflict,
        "recovery-permission case #{case}: refusing after expiry is a typed conflict, never a fake deny"
    );
    case += 1;
    assert!(
        s.events_range(1, None)
            .unwrap()
            .iter()
            .any(|e| e.kind == EventKind::PermissionExpired),
        "recovery-permission case #{case}: the expiry is durable journal evidence"
    );
    case += 1;
    let again = s.expire_pending_permissions().unwrap();
    assert!(
        again.is_empty(),
        "recovery-permission case #{case}: a second sweep is idempotent"
    );
    case += 1;
    let report = s.recover_all().unwrap();
    assert!(
        report.crashed_ops.is_empty() && report.interrupted_turn,
        "recovery-permission case #{case}: after the permission terminalized there is nothing to recover"
    );
    assert_eq!(case, 8, "permission recovery matrix size drifted");
}

// =====================================================================
// Category 2: queued-prompt scheduling and permission lifecycle
// =====================================================================

#[test]
fn session_prompt_bounds_are_typed() {
    let (_d, m) = test_manager();
    let mut case = 0usize;
    let s = session(&m);

    let huge_prompt = "p".repeat(crate::MAX_PROMPT_BYTES + 1);
    case += 1;
    let err = s
        .submit_prompt(&huge_prompt, &[])
        .expect_err("prompt-bounds case: MAX_PROMPT_BYTES+1 must be refused");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Oversized,
        "prompt-bounds case #{case}: oversized prompt"
    );
    case += 1;
    let at_cap = s
        .submit_prompt(&"p".repeat(crate::MAX_PROMPT_BYTES), &[])
        .unwrap();
    assert!(
        at_cap.accepted,
        "prompt-bounds case #{case}: exactly at the cap is accepted"
    );

    let many_files: Vec<String> = (0..=crate::MAX_FILES_PER_PROMPT)
        .map(|i| format!("f{i}.txt"))
        .collect();
    case += 1;
    let err = s
        .submit_prompt("x", &many_files)
        .expect_err("prompt-bounds case: MAX_FILES_PER_PROMPT+1 must be refused");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Oversized,
        "prompt-bounds case #{case}: oversized file list"
    );
    case += 1;
    let max_files: Vec<String> = (0..crate::MAX_FILES_PER_PROMPT)
        .map(|i| format!("f{i}.txt"))
        .collect();
    assert!(
        s.submit_prompt("x", &max_files).unwrap().queued,
        "prompt-bounds case #{case}: exactly MAX_FILES_PER_PROMPT files is accepted (queued behind the active turn)"
    );
    case += 1;
    let long_path = "p".repeat(crate::MAX_FILE_PATH_BYTES + 1);
    let err = s
        .submit_prompt("x", &[long_path])
        .expect_err("prompt-bounds case: MAX_FILE_PATH_BYTES+1 must be refused");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Oversized,
        "prompt-bounds case #{case}: oversized file path"
    );
    case += 1;
    let at_cap = "p".repeat(crate::MAX_FILE_PATH_BYTES);
    assert!(
        s.submit_prompt("x", &[at_cap]).unwrap().queued,
        "prompt-bounds case #{case}: a file path exactly at the cap is accepted"
    );
    case += 1;
    let hostile = "'; DROP TABLE prompt_queue; --";
    assert!(
        s.submit_prompt(hostile, &[hostile.to_string(), "a\u{0}b".into()])
            .is_ok(),
        "prompt-bounds case #{case}: hostile prompt/file text is data, never SQL"
    );
    assert_eq!(case, 7, "COUNT");
}

#[test]
fn session_queue_fifo_and_admission_semantics() {
    let (_d, m) = test_manager();
    let mut case = 0usize;
    let s = session(&m);

    case += 1;
    let first = s.submit_prompt("first", &[]).unwrap();
    assert!(
        !first.queued,
        "queue case #{case}: an idle session runs the prompt immediately"
    );
    case += 1;
    let q1 = s.submit_prompt("q1", &["a.txt".into()]).unwrap();
    assert!(
        q1.queued,
        "queue case #{case}: a prompt while Preparing queues instead of transitioning"
    );
    case += 1;
    let q2 = s.submit_prompt("q2", &[]).unwrap();
    assert!(q2.queued, "queue case #{case}: second queued prompt");
    case += 1;
    assert_eq!(
        s.queued_prompt_count().unwrap(),
        2,
        "queue case #{case}: pending queue count"
    );
    case += 1;
    assert!(
        s.admit_next_queued().unwrap().is_none(),
        "queue case #{case}: a Preparing session is not an admission target"
    );
    case += 1;
    let messages_before = s.messages_before(None, 100).unwrap().len();
    assert_eq!(
        messages_before, 1,
        "queue case #{case}: queued prompts are NOT materialized before admission"
    );

    s.abort(Some(first.op_id)).unwrap();
    case += 1;
    assert!(
        matches!(
            s.state().unwrap(),
            AgentState::Cancelled | AgentState::ReadyForNextTurn
        ),
        "queue case #{case}: targeted abort parks the session at a promptable state without killing the queue"
    );
    case += 1;
    let admitted = s.admit_next_queued().unwrap().unwrap();
    assert_eq!(
        admitted.prompt, "q1",
        "queue case #{case}: FIFO admits the oldest queued prompt"
    );
    assert_eq!(
        admitted.op_id, q1.op_id,
        "queue case #{case}: queued op identity is reused"
    );
    assert_eq!(
        admitted.files,
        vec!["a.txt".to_string()],
        "queue case #{case}: files roundtrip through the queue"
    );
    s.mark_queued_status(admitted.queue_seq, "done").unwrap();
    case += 1;
    assert_eq!(
        s.queued_prompt_count().unwrap(),
        1,
        "queue case #{case}: a done admitted row leaves exactly the second prompt"
    );
    case += 1;
    assert_eq!(
        s.messages_before(None, 100).unwrap().len(),
        2,
        "queue case #{case}: admission materializes exactly one user message"
    );

    s.abort(Some(admitted.op_id)).unwrap();
    case += 1;
    let admitted2 = s.admit_next_queued().unwrap().unwrap();
    assert_eq!(
        admitted2.prompt, "q2",
        "queue case #{case}: FIFO order for the second prompt"
    );
    s.mark_queued_status(admitted2.queue_seq, "done").unwrap();
    case += 1;
    assert_eq!(
        s.queued_prompt_count().unwrap(),
        0,
        "queue case #{case}: the queue drains"
    );
    case += 1;
    assert!(
        s.admit_next_queued().unwrap().is_none(),
        "queue case #{case}: an empty queue admits nothing"
    );
    case += 1;
    let roles: Vec<String> = s
        .messages_before(None, 100)
        .unwrap()
        .into_iter()
        .map(|m| m.role)
        .collect();
    assert_eq!(
        roles,
        vec!["user".to_string(); 3],
        "queue case #{case}: conversation chronology is insertion order"
    );

    case += 1;
    let aborted_prompt = s.submit_prompt("cancel-me", &[]).unwrap();
    assert!(
        aborted_prompt.queued,
        "queue case #{case}: queued behind the active turn"
    );
    case += 1;
    assert_eq!(
        s.abort(Some(aborted_prompt.op_id)).unwrap().op_ids,
        vec![aborted_prompt.op_id],
        "queue case #{case}: aborting a queued op cancels its row without a machine transition"
    );
    case += 1;
    assert_eq!(
        s.queued_prompt_count().unwrap(),
        0,
        "queue case #{case}: the cancelled row is no longer non-terminal"
    );
    case += 1;
    let err = s
        .abort(Some(OpId::new(999_999)))
        .expect_err("queue case: aborting an unknown op must refuse");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::NotFound,
        "queue case #{case}: unknown queued/active op"
    );
    assert_eq!(case, 18, "queue FIFO matrix size drifted");
}

#[test]
fn session_queue_corruption_and_status_machine() {
    let (_d, m) = test_manager();
    let mut case = 0usize;
    let s = session(&m);
    let active = s.submit_prompt("active", &[]).unwrap().op_id;
    s.submit_prompt("q1", &[]).unwrap();
    s.submit_prompt("q2", &[]).unwrap();
    s.abort(Some(active)).unwrap();
    let q1_seq = m.store().queue_head(s.id()).unwrap().unwrap().queue_seq;

    case += 1;
    s.mark_queued_status(q1_seq, "claimed").unwrap();
    let counts = s.queue_status_counts().unwrap();
    assert_eq!(
        counts.get("claimed").and_then(|v| v.as_i64()),
        Some(1),
        "queue-corrupt case #{case}: claimed is counted"
    );
    case += 1;
    s.mark_queued_status(q1_seq, "done").unwrap();
    assert_eq!(
        s.queued_prompt_count().unwrap(),
        1,
        "queue-corrupt case #{case}: a done row leaves the non-terminal count"
    );
    case += 1;
    s.mark_queued_status(999_999, "done").unwrap();
    assert_eq!(
        s.queued_prompt_count().unwrap(),
        1,
        "queue-corrupt case #{case}: marking an unknown seq is a no-op, never an error or phantom row"
    );
    let q2_seq = m.store().queue_head(s.id()).unwrap().unwrap().queue_seq;
    case += 1;
    s.mark_queued_status(q2_seq, "hostile'; DROP TABLE prompt_queue; --")
        .unwrap();
    let counts = s.queue_status_counts().unwrap();
    assert!(
        counts
            .get("hostile'; DROP TABLE prompt_queue; --")
            .and_then(|v| v.as_i64())
            .is_some(),
        "queue-corrupt case #{case}: status text is opaque data"
    );
    case += 1;
    assert!(
        s.admit_next_queued().unwrap().is_none(),
        "queue-corrupt case #{case}: a row in an unknown status is never admitted"
    );

    let s = session(&m);
    let active = s.submit_prompt("active", &[]).unwrap().op_id;
    s.submit_prompt("q", &[]).unwrap();
    s.abort(Some(active)).unwrap();
    let q_seq = m.store().queue_head(s.id()).unwrap().unwrap().queue_seq;
    m.store()
        .sql_execute(&format!(
            "UPDATE prompt_queue SET files = 'broken{{' WHERE session_id = {} AND seq = {}",
            s.id().raw(),
            q_seq
        ))
        .unwrap();
    case += 1;
    let err = s
        .admit_next_queued()
        .expect_err("queue-corrupt case: corrupt files JSON must be typed");
    assert!(
        err.kind == faktor_core::error::ErrorKind::Store
            && err.message.to_lowercase().contains("corrupt"),
        "queue-corrupt case #{case}: corrupt files must surface as a typed corruption: {err:?}"
    );

    let s = session(&m);
    let active = s.submit_prompt("active", &[]).unwrap().op_id;
    s.submit_prompt("q", &[]).unwrap();
    s.abort(Some(active)).unwrap();
    let q_seq = m.store().queue_head(s.id()).unwrap().unwrap().queue_seq;
    m.store()
        .sql_execute(&format!(
            "UPDATE prompt_queue SET op_id = 0 WHERE session_id = {} AND seq = {}",
            s.id().raw(),
            q_seq
        ))
        .unwrap();
    case += 1;
    let err = s
        .admit_next_queued()
        .expect_err("queue-corrupt case: op_id 0 must be typed corruption");
    assert!(
        err.kind == faktor_core::error::ErrorKind::Store
            && err.message.to_lowercase().contains("corrupt"),
        "queue-corrupt case #{case}: zero op id must surface as typed corruption: {err:?}"
    );

    let s = session(&m);
    let active = s.submit_prompt("active", &[]).unwrap().op_id;
    s.submit_prompt("q", &[]).unwrap();
    s.abort(Some(active)).unwrap();
    let q_seq = m.store().queue_head(s.id()).unwrap().unwrap().queue_seq;
    s.mark_queued_status(q_seq, "claimed").unwrap();
    case += 1;
    assert_eq!(
        s.recover_queued_rows().unwrap(),
        1,
        "queue-corrupt case #{case}: a claimed row recovers to pending"
    );
    case += 1;
    s.mark_queued_status(q_seq, "claimed").unwrap();
    s.mark_queued_status(q_seq, "running").unwrap();
    let _ = s.recover_queued_rows().unwrap();
    let counts = s.queue_status_counts().unwrap();
    assert_eq!(
        counts.get("done").and_then(|v| v.as_i64()),
        Some(1),
        "queue-corrupt case #{case}: a running row with no active turn record is retired to done"
    );
    assert!(
        counts.get("running").and_then(|v| v.as_i64()).unwrap_or(0) == 0,
        "queue-corrupt case #{case}: no running residue"
    );
    case += 1;
    assert!(
        s.admit_next_queued().unwrap().is_none(),
        "queue-corrupt case #{case}: a retired row is never admitted"
    );
    assert_eq!(case, 10, "queue corruption matrix size drifted");
}

#[test]
fn session_permission_lifecycle_hostile() {
    let mut case = 0usize;
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(2_000_000));
    let m = SessionManager::open_with_clock(
        dir.path().join("store"),
        dir.path().join("cas"),
        true,
        clock.clone(),
    )
    .unwrap();
    let s = session(&m);
    let (_turn, req) = drive_to_waiting_permission(&s);

    case += 1;
    let err = s
        .request_permission(
            OpId::new(999_999),
            &Capability::ReadWorkspace { path: "/w".into() },
        )
        .expect_err("permission case: an untracked op must be refused");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::NotFound,
        "permission case #{case}: untracked op"
    );

    case += 1;
    assert!(
        s.pending_permission(req.id).unwrap().is_some(),
        "permission case #{case}: a request is durably pending"
    );
    case += 1;
    let err = s
        .resolve_permission(req.id, PermissionDecision::Allow)
        .and_then(|_| s.resolve_permission(req.id, PermissionDecision::Allow))
        .expect_err("permission case: a losing double resolve must refuse");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Conflict,
        "permission case #{case}: double resolve is a typed conflict"
    );
    case += 1;
    assert_eq!(
        s.state().unwrap(),
        AgentState::ExecutingTool,
        "permission case #{case}: Allow lands ExecutingTool"
    );
    assert_eq!(
        m.store().permission_decision(req.id).unwrap().as_deref(),
        Some("allow"),
        "permission case #{case}: the resolved row is durably allow (never pending)"
    );

    let s2 = session(&m);
    let turn2 = s2.submit_prompt("x", &[]).unwrap().op_id;
    s2.append_event(
        EventKind::ContextPrepared,
        AgentState::BuildingContext,
        Some(turn2),
        None,
    )
    .unwrap();
    s2.append_event(
        EventKind::ModelStarted,
        AgentState::WaitingForModel,
        Some(turn2),
        None,
    )
    .unwrap();
    s2.append_event(
        EventKind::ModelChunkReceived,
        AgentState::Streaming,
        Some(turn2),
        None,
    )
    .unwrap();
    let req2 = s2
        .request_permission(
            turn2,
            &Capability::ReadWorkspace {
                path: "/w/'; DROP TABLE permission; --".into(),
            },
        )
        .unwrap();
    case += 1;
    let (_, op_back, cap_back) = s2.pending_permission(req2.id).unwrap().unwrap();
    assert_eq!(op_back, turn2, "permission case #{case}: op identity");
    assert!(
        cap_back.contains("DROP TABLE"),
        "permission case #{case}: hostile capability text is durable data"
    );
    case += 1;
    clock.set(req2.expires_ms + 1);
    let expired = s2.expire_pending_permissions().unwrap();
    assert_eq!(
        expired.expired,
        vec![(req2.id, turn2)],
        "permission case #{case}: the expired row is terminalized with its op"
    );
    case += 1;
    assert_eq!(
        s2.state().unwrap(),
        AgentState::ReadyForNextTurn,
        "permission case #{case}: expiry lands where an explicit Deny would"
    );
    case += 1;
    let err = s2
        .resolve_permission(req2.id, PermissionDecision::Allow)
        .expect_err("permission case: resolving after expiry must refuse");
    assert!(
        matches!(
            err.kind,
            faktor_core::error::ErrorKind::Conflict
                | faktor_core::error::ErrorKind::InvalidState { .. }
        ),
        "permission case #{case}: expiry is durable; late allow is refused typed, got {:?}",
        err.kind
    );
    assert_eq!(
        s2.state().unwrap(),
        AgentState::ReadyForNextTurn,
        "permission case #{case}: the refused late allow never moves the state"
    );
    case += 1;
    assert!(
        s2.events_range(1, None)
            .unwrap()
            .iter()
            .any(|e| e.kind == EventKind::PermissionExpired),
        "permission case #{case}: expiry journal event"
    );
    assert_eq!(case, 9, "permission lifecycle matrix size drifted");
}

#[test]
fn session_lifecycle_immutability_and_resume() {
    let (_d, m) = test_manager();
    let mut case = 0usize;

    let s = session(&m);
    case += 1;
    let seq = s.end_session().unwrap();
    assert!(
        seq.raw() >= 2,
        "lifecycle case #{case}: end journals from Idle"
    );
    case += 1;
    let err = s
        .submit_prompt("after-close", &[])
        .expect_err("lifecycle case: a closed session must not accept prompts");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Conflict,
        "lifecycle case #{case}: closed session refuses prompts"
    );
    case += 1;
    let err = s
        .end_session()
        .expect_err("lifecycle case: double end must refuse");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Conflict,
        "lifecycle case #{case}: double end"
    );
    case += 1;
    let err = s
        .resume(AgentState::ReadyForNextTurn)
        .expect_err("lifecycle case: a closed session must not resume");
    assert!(
        matches!(
            err.kind,
            faktor_core::error::ErrorKind::Conflict | faktor_core::error::ErrorKind::Malformed
        ),
        "lifecycle case #{case}: closed lifecycle is terminal, got {:?}",
        err.kind
    );

    let s2 = session(&m);
    let r = s2.submit_prompt("first", &[]).unwrap();
    s2.abort(Some(r.op_id)).unwrap();
    case += 1;
    assert!(
        matches!(
            s2.state().unwrap(),
            AgentState::Cancelled | AgentState::ReadyForNextTurn
        ),
        "lifecycle case #{case}: a cancelled turn is a promptable state"
    );
    case += 1;
    assert!(
        s2.submit_prompt("resume", &[]).unwrap().accepted,
        "lifecycle case #{case}: a prompt from Cancelled starts a fresh turn"
    );
    case += 1;
    let suspended = s2.suspend().unwrap();
    assert!(
        suspended.raw() >= 1,
        "lifecycle case #{case}: suspend journals"
    );
    case += 1;
    assert_eq!(
        s2.lifecycle().unwrap(),
        faktor_core::state::SessionLifecycle::Suspended,
        "lifecycle case #{case}: suspended lifecycle"
    );
    case += 1;
    let auto = s2.submit_prompt("auto-resume", &[]).unwrap();
    assert!(
        auto.accepted,
        "lifecycle case #{case}: a prompt auto-resumes a Suspended session"
    );
    assert_eq!(
        s2.lifecycle().unwrap(),
        faktor_core::state::SessionLifecycle::Open,
        "lifecycle case #{case}: auto-resume reopens the lifecycle"
    );
    case += 1;
    let s3 = session(&m);
    assert!(
        s3.reset().unwrap().raw() >= 1,
        "lifecycle case #{case}: reset from Idle journals"
    );
    case += 1;
    let err = s2
        .mark_failed(true, "boom")
        .and_then(|_| s2.submit_prompt("after", &[]))
        .expect_err("lifecycle case: a permanently failed session must not accept prompts");
    assert_eq!(
        err.kind,
        faktor_core::error::ErrorKind::Conflict,
        "lifecycle case #{case}: terminal FailedPermanent refuses prompts"
    );
    case += 1;
    let err = s2
        .end_session()
        .expect_err("lifecycle case: a terminal state cannot end again");
    assert!(
        matches!(
            err.kind,
            faktor_core::error::ErrorKind::Conflict
                | faktor_core::error::ErrorKind::InvalidState { .. }
        ),
        "lifecycle case #{case}: terminal immutability, got {:?}",
        err.kind
    );
    assert_eq!(case, 12, "lifecycle matrix size drifted");
}
