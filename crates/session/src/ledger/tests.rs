//! Ledger tests (mechanically split from `ledger`).

use super::*;
use crate::handle::tests::{session, test_manager};
use std::sync::Arc;

fn raw_sql(m: &crate::SessionManager, sql: &str) {
    m.store().sql_execute(sql).unwrap();
}

fn turn_entries(s: &SessionHandle, turn: u64) {
    s.ledger_decision(
        &format!("step-{turn}"),
        &format!("choice-{turn}"),
        &format!("rationale-{turn}"),
    )
    .unwrap();
    s.ledger_routing_decision(
        turn,
        &format!("prov-{turn}"),
        &format!("model-{turn}"),
        "quality fit",
        42,
    )
    .unwrap();
    s.ledger_plan_step_added(
        turn as u32,
        &format!("do step {turn}"),
        Some(turn as u32 - 1),
    )
    .unwrap();
    s.ledger_verify_run(
        &[LedgerCheckRun {
            id: format!("check-{turn}"),
            passed: true,
        }],
        "passed",
    )
    .unwrap();
    s.ledger_turn_completed(turn).unwrap();
}

#[test]
fn verified_git_artifact_roundtrips_and_refuses_hostile_rows() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let state = faktor_fs::entry_state::EntryState::regular(
        faktor_fs::tree_manifest::CanonicalMode::ExecutableFile,
        faktor_core::hash::FileHash::from(blake3::hash(b"payload").into()),
    )
    .unwrap();
    let artifact = VerifiedGitArtifact {
        task_id: 7,
        revision: 3,
        verification_record: 11,
        verified_root_digest: "tm1:abc".into(),
        verified_manifest: vec![
            VerifiedManifestEntry {
                path: "a/b.rs".into(),
                state: state.clone(),
            },
            VerifiedManifestEntry {
                path: "link".into(),
                state: faktor_fs::entry_state::EntryState::symlink(b"a/b.rs".to_vec()).unwrap(),
            },
        ],
        git_tree_oid: Some("a".repeat(40)),
        commit_oid: None,
        local_ref: None,
        remote_ref: None,
        updated_ms: 1,
    };
    s.ledger_verified_git_artifact_set(&artifact).unwrap();
    let read = s
        .ledger_verified_git_artifact_get(7, 3)
        .unwrap()
        .expect("roundtrip");
    assert_eq!(read, artifact);
    assert!(s.ledger_verified_git_artifact_get(7, 4).unwrap().is_none());
    // A hostile manifest (traversal path) is refused BEFORE any append.
    let mut evil = artifact.clone();
    evil.verified_manifest[0].path = "../escape.rs".into();
    assert!(s.ledger_verified_git_artifact_set(&evil).is_err());
    // An unsorted manifest is refused too (canonical order is the codec).
    let mut unsorted = artifact.clone();
    unsorted.verified_manifest.swap(0, 1);
    assert!(s.ledger_verified_git_artifact_set(&unsorted).is_err());
    // A hostile oid is refused.
    let mut bad_oid = artifact.clone();
    bad_oid.commit_oid = Some("zz".repeat(20));
    assert!(s.ledger_verified_git_artifact_set(&bad_oid).is_err());
    // A malformed encoded remote ref is refused.
    let mut bad_remote = artifact.clone();
    bad_remote.remote_ref = Some("origin:refs/heads/main@nope".into());
    assert!(s.ledger_verified_git_artifact_set(&bad_remote).is_err());
    let good_remote = VerifiedGitArtifact {
        remote_ref: Some(format!("origin:refs/heads/main@{}", "b".repeat(40))),
        commit_oid: Some("b".repeat(40)),
        local_ref: Some("refs/heads/main".into()),
        ..artifact
    };
    s.ledger_verified_git_artifact_set(&good_remote).unwrap();
    let read = s.ledger_verified_git_artifact_get(7, 3).unwrap().unwrap();
    assert_eq!(read.remote_ref, good_remote.remote_ref);
}

fn assert_never_lost(s: &SessionHandle, goal: &str, open_blockers: &[&str]) {
    let view = s.ledger_view().unwrap();
    assert_eq!(view.head.goal, goal, "goal survives compaction");
    assert!(
        !view.head.criteria.is_empty(),
        "criteria survive compaction"
    );
    for b in open_blockers {
        assert!(
            view.head.open_blockers.iter().any(|o| o == b),
            "open blocker {b} must survive compaction"
        );
    }
    assert!(
        !view.head.decisions.is_empty(),
        "last decision must survive compaction"
    );
    // Entry-level: the pinned rows still exist in the stream.
    let mut page = s.ledger_entries_page(None, 500).unwrap();
    let mut entries = Vec::new();
    loop {
        let cursor = page.entries.last().map(|e| e.seq);
        entries.extend(page.entries);
        if !page.has_more {
            break;
        }
        page = s
            .ledger_entries_page(cursor, 500)
            .expect("paged entries decode");
    }
    assert!(
        entries
            .iter()
            .any(|e| matches!(&e.payload, LedgerPayload::GoalSet { goal: g } if g == goal)),
        "GoalSet entry must survive"
    );
    assert!(
        entries
            .iter()
            .any(|e| matches!(&e.payload, LedgerPayload::CriteriaSet { .. })),
        "CriteriaSet entry must survive"
    );
    assert!(
        entries
            .iter()
            .any(|e| matches!(&e.payload, LedgerPayload::Decision { .. })),
        "last Decision entry must survive"
    );
    for b in open_blockers {
        assert!(
            entries.iter().any(|e| matches!(
                &e.payload,
                LedgerPayload::BlockerOpened { reason } if reason == b
            )),
            "unresolved BlockerOpened {b} entry must survive"
        );
    }
    let head = s.manager.store().ledger_head(s.id).unwrap();
    assert!(
        head.is_some() && head.unwrap().checkpoint_seq > 0,
        "latest checkpoint head must be present"
    );
}

#[test]
fn typed_child_blocker_delegates_to_the_existing_string_payload() {
    use faktor_core::blocker::{BlockerKind, ChildBlocker, MAX_CHILD_BLOCKER_REASON_CHARS};

    let (_d, m) = test_manager();
    let s = session(&m);
    let blocker = ChildBlocker::new(
        BlockerKind::External,
        "remote deploy is pending",
        "wait for the upstream deploy, then resume",
    );
    // Open through the TYPED delegation: the durable row is still the
    // historic string `BlockerOpened` payload (additive compat).
    let seq = s
        .ledger_child_blocker_opened(&blocker)
        .unwrap()
        .expect("typed open writes the string row");
    assert!(seq > 0);
    let view = s.ledger_view().unwrap();
    assert!(view
        .head
        .open_blockers
        .iter()
        .any(|r| r == &blocker.ledger_reason()));
    let entries = collect_all(&s);
    assert!(entries.iter().any(|e| matches!(
        &e.payload,
        LedgerPayload::BlockerOpened { reason } if reason == &blocker.reason
    )));
    // Re-opening the same typed reason is the idempotent no-op.
    assert!(s.ledger_child_blocker_opened(&blocker).unwrap().is_none());
    // A hostile typed blocker is refused by the shared validator BEFORE
    // any ledger write.
    let huge = ChildBlocker::new(
        BlockerKind::Unknown,
        "x".repeat(MAX_CHILD_BLOCKER_REASON_CHARS + 1),
        "resolve",
    );
    assert!(s.ledger_child_blocker_opened(&huge).is_err());
    // Resolve through the same typed delegation.
    s.ledger_child_blocker_resolved(&blocker).unwrap();
    assert!(!s
        .ledger_view()
        .unwrap()
        .head
        .open_blockers
        .iter()
        .any(|r| r == &blocker.ledger_reason()));
    assert!(entries.iter().any(|e| matches!(
        &e.payload,
        LedgerPayload::BlockerOpened { reason } if reason == &blocker.reason
    )));
}

#[test]
fn typed_accessors_roundtrip_and_view_folds() {
    let (_d, m) = test_manager();
    let s = session(&m);
    assert!(s.ledger_view().unwrap().head.goal.is_empty());
    s.ledger_goal_set("implement the ledger").unwrap();
    s.ledger_criteria_set(
        &["cargo test".into(), "no warnings".into()],
        "cargo test + no warnings",
    )
    .unwrap();
    s.ledger_blocker_opened("check alpha failed").unwrap();
    s.ledger_decision("pick", "rust", "ecosystem").unwrap();
    s.ledger_plan_step_added(0, "read the spec", None).unwrap();
    s.ledger_plan_step_added(1, "write code", Some(0)).unwrap();
    s.ledger_routing_decision(9, "ollama", "qwen3.8", "cost", 3)
        .unwrap();
    s.ledger_epoch_bumped(None, 7).unwrap();
    s.ledger_failure_recorded("test failed: x").unwrap();
    s.ledger_turn_completed(9).unwrap();
    let view = s.ledger_view().unwrap();
    assert_eq!(view.head.goal, "implement the ledger");
    assert_eq!(view.head.criteria, vec!["cargo test", "no warnings"]);
    assert_eq!(view.head.open_blockers, vec!["check alpha failed"]);
    assert_eq!(view.head.epoch, Some(7));
    assert_eq!(view.head.routing_count, 1);
    assert_eq!(view.head.routing_tail[0].provider, "ollama");
    assert_eq!(view.head.plan_steps[1].parent_index, Some(0));
    // Blocker resolution removes the reason from the fold.
    s.ledger_blocker_resolved("check alpha failed").unwrap();
    let view = s.ledger_view().unwrap();
    assert!(view.head.open_blockers.is_empty());
    // Duplicate open of the same reason is a no-op, never an error.
    s.ledger_blocker_opened("again").unwrap();
    assert!(s.ledger_blocker_opened("again").unwrap().is_none());
    assert_eq!(
        s.ledger_view().unwrap().head.open_blockers,
        vec!["again".to_string()]
    );
    // Resolving a reason that is not open is a typed error.
    let err = s.ledger_blocker_resolved("never-opened").unwrap_err();
    assert_eq!(err.kind, faktor_core::ErrorKind::Conflict);
}

#[test]
fn out_of_order_child_finish_is_a_typed_append_error() {
    let (_d, m) = test_manager();
    let s = session(&m);
    // Finish without start: typed error at append time.
    let err = s.ledger_child_finished(77, "done").unwrap_err();
    assert!(err.to_string().contains("no open start"), "{err}");
    // Start -> finish is legal.
    s.ledger_child_started(77, 3, 2, "verify the change")
        .unwrap();
    s.ledger_child_finished(77, "verified").unwrap();
    // Double finish is again a typed error.
    let err = s.ledger_child_finished(77, "again").unwrap_err();
    assert!(err.to_string().contains("no open start"), "{err}");
    // Double START of the same running agent conflicts.
    s.ledger_child_started(78, 3, 2, "second").unwrap();
    let err = s.ledger_child_started(78, 3, 2, "dup").unwrap_err();
    assert_eq!(err.kind, faktor_core::ErrorKind::Conflict);
    // A finished agent may start again.
    s.ledger_child_finished(78, "done").unwrap();
    s.ledger_child_started(78, 3, 2, "restart").unwrap();
    assert!(s.ledger_child_finished(78, "done again").is_ok());
    // Zero ids are malformed.
    assert!(s.ledger_child_started(0, 1, 1, "x").is_err());
    // The fold keeps children with their outcomes.
    let view = s.ledger_view().unwrap();
    assert_eq!(view.head.children.len(), 2);
    assert_eq!(
        view.head
            .children
            .iter()
            .find(|c| c.agent_id == 77)
            .unwrap()
            .outcome
            .as_deref(),
        Some("verified")
    );
}

#[test]
fn child_presentation_rows_are_strict_and_fold_the_latest() {
    let (_d, m) = test_manager();
    let s = session(&m);
    // Hostile raw rows are loud on read, never silently parsed.
    let hostile: Vec<serde_json::Value> = vec![
        serde_json::json!({"kind":"child_presentation_changed","child_id":"","from":"foreground","to":"background","at_ms":1}),
        serde_json::json!({"kind":"child_presentation_changed","child_id":"child-0","from":"foreground","to":"foreground","at_ms":1}),
        serde_json::json!({"kind":"child_presentation_changed","child_id":"child-0","from":"foreground","to":"background","at_ms":0}),
        serde_json::json!({"kind":"child_presentation_changed","child_id":"child-0","from":"foreground","to":"paused","at_ms":1}),
        serde_json::json!({"kind":"child_presentation_changed","child_id":"a/b","from":"foreground","to":"background","at_ms":1}),
        serde_json::json!({"kind":"child_presentation_changed","child_id":"child-0","from":"foreground","at_ms":1}),
    ];
    for row in &hostile {
        assert!(
            decode_payload(ENTRY_CHILD_PRESENTATION_CHANGED, LEDGER_ENTRY_SCHEMA_V, row).is_err(),
            "hostile presentation row must be refused: {row}"
        );
    }
    // Legal rows fold the LATEST state per child into the head.
    for (child, from, to, at) in [
        (
            "child-0",
            PresentationState::Foreground,
            PresentationState::Background,
            1,
        ),
        (
            "child-0",
            PresentationState::Background,
            PresentationState::Foreground,
            2,
        ),
        (
            "child-0",
            PresentationState::Foreground,
            PresentationState::Background,
            3,
        ),
        (
            "child-1",
            PresentationState::Foreground,
            PresentationState::Background,
            4,
        ),
    ] {
        s.append_typed_entry(LedgerPayload::ChildPresentationChanged {
            child_id: child.into(),
            from,
            to,
            at_ms: at,
        })
        .unwrap();
    }
    assert_eq!(
        s.child_presentation("child-0").unwrap(),
        PresentationState::Background
    );
    assert_eq!(
        s.child_presentation("child-1").unwrap(),
        PresentationState::Background
    );
    assert_eq!(
        s.child_presentation("child-2").unwrap(),
        PresentationState::Foreground
    );
    // Compaction may prune the rows; the folded head keeps the latest.
    let report = s.compact_typed_ledger().unwrap();
    assert!(report.deleted > 0);
    assert_eq!(
        s.child_presentation("child-0").unwrap(),
        PresentationState::Background
    );
    assert!(s
        .ledger_view()
        .unwrap()
        .head
        .presentations
        .contains_key("child-1"));
}

#[test]
fn never_lose_survives_20_turns_and_6_compactions() {
    // Requirement 4a: 20 turns with decisions/blocks/children, 6
    // compactions, then GoalSet + CriteriaSet + the last Decision +
    // every unresolved BlockerOpened and the latest head are all there.
    let (_d, m) = test_manager();
    let s = session(&m);
    s.ledger_goal_set("goal-0").unwrap();
    s.ledger_criteria_set(&["c0".into(), "c1".into()], "c0 c1")
        .unwrap();
    s.ledger_epoch_bumped(None, 1).unwrap();
    for turn in 1..=20u64 {
        turn_entries(&s, turn);
        if turn == 5 {
            s.ledger_child_started(500 + turn, turn, 1, "subagent verify")
                .unwrap();
            s.ledger_child_finished(500 + turn, "done").unwrap();
        }
        if turn == 7 {
            s.ledger_blocker_opened("blocker-seven").unwrap();
        }
        if turn == 13 {
            s.ledger_blocker_opened("blocker-thirteen").unwrap();
        }
        if turn == 9 {
            // A blocker opened and later resolved must NOT linger.
            s.ledger_blocker_opened("resolved-nine").unwrap();
            s.ledger_blocker_resolved("resolved-nine").unwrap();
        }
        if turn % 3 == 0 {
            s.compact_typed_ledger().unwrap();
        }
    }
    // Three more compactions beyond the turns.
    for _ in 0..3 {
        s.compact_typed_ledger().unwrap();
    }
    assert_never_lost(&s, "goal-0", &["blocker-seven", "blocker-thirteen"]);
    let view = s.ledger_view().unwrap();
    // The last Decision entry is the decision of turn 20.
    let last = view.head.decisions.first().unwrap();
    assert_eq!(last.step, "step-20");
    assert_eq!(view.head.epoch, Some(1));
    assert_eq!(view.head.routing_count, 20);
    // Head-only content that compaction pruned from the entry stream is
    // still folded in the head: the child-agent records of turn 5 and
    // the plan DAG of the compacted steps.
    assert_eq!(view.head.children.len(), 1, "children survive in the head");
    assert_eq!(view.head.children[0].agent_id, 505);
    assert!(
        !view.head.plan_steps.is_empty(),
        "plan DAG survives in the head"
    );
    // Compaction pruned the stream: entries are bounded to the pinned
    // set + nothing newer (goal, criteria, 2 blockers, 1 decision).
    let entries = collect_all(&s);
    assert_eq!(entries.len(), 5, "only pinned entries survive: {entries:?}");
    // Resolved blockers do not linger in the fold.
    assert!(!view.head.open_blockers.iter().any(|r| r == "resolved-nine"));
}

fn collect_all(s: &SessionHandle) -> Vec<TypedLedgerEntry> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let page = s.ledger_entries_page(cursor, 500).unwrap();
        let c = page.entries.last().map(|e| e.seq);
        out.extend(page.entries);
        if !page.has_more {
            break;
        }
        cursor = c;
    }
    out
}

#[test]
fn watermark_holds_across_five_compacting_turns() {
    // Requirement 3's watermark test: five turns, each compacting.
    let (_d, m) = test_manager();
    let s = session(&m);
    s.ledger_goal_set("the-goal").unwrap();
    s.ledger_criteria_set(&["must compile".into()], "must compile")
        .unwrap();
    for turn in 1..=5u64 {
        s.ledger_blocker_opened(&format!("blocker-{turn}")).unwrap();
        turn_entries(&s, turn);
        s.compact_typed_ledger().unwrap();
        // After EVERY compaction all protected content is present.
        assert_never_lost(&s, "the-goal", &[&format!("blocker-{turn}")]);
    }
    // With five open blockers, all five survive compaction.
    s.compact_typed_ledger().unwrap();
    assert_never_lost(
        &s,
        "the-goal",
        &[
            "blocker-1",
            "blocker-2",
            "blocker-3",
            "blocker-4",
            "blocker-5",
        ],
    );
}

#[test]
fn crafted_unknown_schema_version_makes_every_typed_reader_error_loudly() {
    // Requirement 4b: a row written with schema_ver 999 makes every
    // typed reader error loudly (Corrupt), and compaction refuses.
    let (_d, m) = test_manager();
    let s = session(&m);
    s.ledger_goal_set("g").unwrap();
    raw_sql(
        &m,
        &format!(
            "INSERT INTO ledger_entry(session_id, seq, entry_type, schema_ver, payload, created_ms)
                 VALUES ({}, 2, 'goal_set', 999, '{{\"goal\": \"future\"}}', 1)",
            s.id().raw()
        ),
    );
    // Every typed reader fails loudly.
    let err = s.ledger_view().unwrap_err();
    assert!(err.to_string().contains("999"), "{err}");
    let err = s.ledger_verify_open().unwrap_err();
    assert!(err.to_string().contains("999"), "{err}");
    let err = s.ledger_entries_page(None, 10).unwrap_err();
    assert!(err.to_string().contains("999"), "{err}");
    // Compaction refuses to checkpoint it: nothing deleted, head intact.
    let head_before = s.manager.store().ledger_head(s.id()).unwrap();
    let err = s.compact_typed_ledger().unwrap_err();
    assert!(err.to_string().contains("999"), "{err}");
    let head_after = s.manager.store().ledger_head(s.id()).unwrap();
    assert_eq!(head_before, head_after, "compaction must not checkpoint");
    let rows = s.manager.store().ledger_entries(s.id(), None, 10).unwrap();
    assert_eq!(rows.len(), 2, "nothing was deleted around the corrupt row");
    // Reopening the session fails loudly too.
    let err = m.get_session(s.id()).unwrap_err();
    assert!(err.to_string().contains("999"), "{err}");
}

#[test]
fn payload_shape_violation_is_loud_never_silent() {
    let (_d, m) = test_manager();
    let s = session(&m);
    s.ledger_goal_set("g").unwrap();
    // Valid JSON, wrong shape for its tag: strict decode fails loudly.
    raw_sql(
        &m,
        &format!(
            "INSERT INTO ledger_entry(session_id, seq, entry_type, schema_ver, payload, created_ms)
                 VALUES ({}, 2, 'goal_set', 1, '{{\"nonsense\": true}}', 1)",
            s.id().raw()
        ),
    );
    let err = s.ledger_view().unwrap_err();
    assert!(err.to_string().contains("v1 schema"), "{err}");
    assert!(m.get_session(s.id()).is_err());
}

#[test]
fn concurrent_appends_interleave_without_losing_entries() {
    // Requirement 4d: two handles append concurrently; seqs stay unique
    // and every entry lands.
    let (_d, m) = test_manager();
    let s = Arc::new(session(&m));
    let s1 = s.clone();
    let s2 = s.clone();
    let t1 = std::thread::spawn(move || {
        for i in 0..50u32 {
            // Even plan indexes (0..98); index 0 is the root step.
            let parent = if i == 0 { None } else { Some(2 * i - 1) };
            s1.ledger_plan_step_added(2 * i, &format!("a{i}"), parent)
                .unwrap();
        }
    });
    let t2 = std::thread::spawn(move || {
        for i in 0..50u32 {
            s2.ledger_plan_step_added(2 * i + 1, &format!("b{i}"), Some(2 * i))
                .unwrap();
        }
    });
    t1.join().unwrap();
    t2.join().unwrap();
    let entries = collect_all(&s);
    assert_eq!(entries.len(), 100, "every concurrent append must land");
    let mut seqs: Vec<i64> = entries.iter().map(|e| e.seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), 100, "seqs must be unique");
    let mut indexes: Vec<u32> = Vec::new();
    for e in &entries {
        if let LedgerPayload::PlanStepAdded { step_index, .. } = &e.payload {
            indexes.push(*step_index);
        }
    }
    indexes.sort_unstable();
    indexes.dedup();
    assert_eq!(indexes.len(), 100, "every planned step present");
    // The fold is coherent after the interleaving.
    let view = s.ledger_view().unwrap();
    assert_eq!(view.head.plan_steps.len(), 100);
}

#[test]
fn crash_between_append_and_checkpoint_recovers_both_orders() {
    // Requirement 4e: a crash between an entry append and the head
    // checkpoint rebuilds the head from entries — in both orders.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // First "process": appends, NO head checkpoint yet, "crash".
    let (store, cas) = (root.join("store"), root.join("cas"));
    {
        let m = crate::SessionManager::open(&store, &cas, true).unwrap();
        let ws = m.create_workspace("/w").unwrap();
        let s = m.create_session(ws, "t", "p", "m").unwrap();
        let id = s.id();
        s.ledger_goal_set("goal-batch-1").unwrap();
        s.ledger_decision("d", "c", "r").unwrap();
        assert!(
            m.store().ledger_head(id).unwrap().is_none(),
            "crash before the first checkpoint"
        );
        // Second "process" reopens and must rebuild the head.
        let m2 = crate::SessionManager::open(&store, &cas, true).unwrap();
        let s2 = m2.get_session(id).unwrap().unwrap();
        let head = m2.store().ledger_head(id).unwrap().unwrap();
        assert_eq!(head.checkpoint_seq, 2);
        let view = s2.ledger_view().unwrap();
        assert_eq!(view.head.goal, "goal-batch-1");
        // Third "process": head EXISTS, more appends land, crash again
        // before the checkpoint folds them.
        s2.ledger_decision("d2", "c2", "r2").unwrap();
        s2.ledger_routing_decision(2, "p", "m", "r", 1).unwrap();
        let m3 = crate::SessionManager::open(&store, &cas, true).unwrap();
        let s3 = m3.get_session(id).unwrap().unwrap();
        let view = s3.ledger_view().unwrap();
        assert_eq!(view.head.goal, "goal-batch-1");
        assert_eq!(view.head.decisions.len(), 2);
        assert_eq!(view.head.routing_count, 1);
        assert_eq!(
            m3.store().ledger_head(id).unwrap().unwrap().checkpoint_seq,
            4
        );
    }
}

#[test]
fn corrupted_head_recovers_but_corrupted_entry_fails_the_open() {
    // Requirement 4f: a hand-corrupted head recovers by replay from
    // entries; an entry whose JSON fails its schema decode fails the
    // session open loudly — never silently dropped.
    let dir = tempfile::tempdir().unwrap();
    let (store, cas) = (dir.path().join("store"), dir.path().join("cas"));
    let id = {
        let m = crate::SessionManager::open(&store, &cas, true).unwrap();
        let ws = m.create_workspace("/w").unwrap();
        let s = m.create_session(ws, "t", "p", "m").unwrap();
        s.ledger_goal_set("the-goal").unwrap();
        s.ledger_criteria_set(&["c".into()], "c").unwrap();
        s.ledger_decision("d", "c", "r").unwrap();
        s.ledger_blocker_opened("open-b").unwrap();
        // Head materialized.
        let _ = s.ledger_view().unwrap();
        // Hand-corrupt the head JSON.
        raw_sql(
            &m,
            &format!(
                "UPDATE ledger_head SET head_json = 'garbage{{{{' WHERE session_id = {}",
                s.id().raw()
            ),
        );
        s.id()
    };
    // Open recovers by replaying the surviving entries.
    let m2 = crate::SessionManager::open(&store, &cas, true).unwrap();
    let h2 = m2.get_session(id).unwrap().unwrap();
    let view = h2.ledger_view().unwrap();
    assert_eq!(view.head.goal, "the-goal");
    assert_eq!(
        view.head.open_blockers,
        vec!["open-b".to_string()],
        "entry replay must rebuild the full head"
    );
    assert_eq!(
        m2.store().ledger_head(id).unwrap().unwrap().checkpoint_seq,
        4
    );
    // Now corrupt an ENTRY payload: the next open fails loudly.
    h2.ledger_decision("d2", "c2", "r2").unwrap();
    let last_seq = m2.store().ledger_max_seq(id).unwrap();
    raw_sql(
            &m2,
            &format!(
                "UPDATE ledger_entry SET payload = 'not-json{{{{' WHERE session_id = {} AND seq = {last_seq}",
                id.raw()
            ),
        );
    let err = m2.get_session(id).unwrap_err();
    assert!(
        !err.to_string().is_empty(),
        "corrupt entry must fail the open loudly"
    );
    let err = h2.ledger_view().unwrap_err();
    assert!(err.to_string().contains("payload"), "{err}");
}

#[test]
fn journal_replay_fails_loudly_on_crafted_future_payload_version() {
    // A journal event row crafted with payload_ver 999 fails the typed
    // replay loudly (never a silent v1 parse).
    let (_d, m) = test_manager();
    let s = session(&m);
    s.submit_prompt("work", &[]).unwrap();
    raw_sql(
        &m,
        &format!(
            "UPDATE event SET payload_ver = 999 WHERE session_id = {} AND kind = 'prompt_received'",
            s.id().raw()
        ),
    );
    let err = s.replay_journal().unwrap_err();
    assert!(err.to_string().contains("999"), "{err}");
}

#[test]
fn typed_ledger_is_per_session() {
    let (_d, m) = test_manager();
    let s1 = session(&m);
    let ws = m.create_workspace("/w2").unwrap();
    let s2 = m.create_session(ws, "t2", "p", "m").unwrap();
    s1.ledger_goal_set("one").unwrap();
    assert!(s2.ledger_view().unwrap().head.goal.is_empty());
    s2.ledger_goal_set("two").unwrap();
    assert_eq!(s1.ledger_view().unwrap().head.goal, "one");
    assert_eq!(s2.ledger_view().unwrap().head.goal, "two");
}

#[test]
fn hostile_entry_bounds_are_rejected_before_write() {
    let (_d, m) = test_manager();
    let s = session(&m);
    assert!(s.ledger_goal_set("").is_err());
    assert!(s.ledger_goal_set(&"x".repeat(MAX_LEDGER_TEXT + 1)).is_err());
    assert!(s.ledger_blocker_opened("").is_err());
    assert!(s.ledger_decision("s", "", "r").is_err());
    assert!(s.ledger_verify_run(&[], "passed").is_err());
    assert!(s
        .ledger_verify_run(
            &[LedgerCheckRun {
                id: "c".into(),
                passed: true
            }],
            "typo"
        )
        .is_err());
    assert!(s.ledger_epoch_bumped(Some(3), 3).is_err());
    assert!(s.ledger_plan_step_added(300, "x", None).is_err());
    assert!(s.ledger_plan_step_added(4, "x", Some(4)).is_err());
    // None of the rejections wrote anything.
    assert_eq!(collect_all(&s).len(), 0);
}

// ---------------------------------------------- durable edit txn rows

fn edit_txn_file(path: &str, content: &[u8]) -> EditTxnLedgerFile {
    EditTxnLedgerFile {
        path: path.to_string(),
        base_digest: digest64(content.len() as u64),
        base_bytes_len: content.len() as u64,
    }
}

/// A deterministic 64-hex digest shape (the ledger validates the shape,
/// never the digest's truthfulness — that is the engine's CAS axis).
fn digest64(seed: u64) -> String {
    format!("{seed:064x}")
}

#[test]
fn edit_txn_roundtrip_and_open_set() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let f1 = edit_txn_file("a.txt", b"one");
    let f2 = edit_txn_file("b.txt", b"two");
    s.ledger_edit_txn_prepared(11, "run-1", &[f1.clone(), f2.clone()], "roll_forward")
        .unwrap();
    let open = s.ledger_open_edit_txns().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].txn_id, 11);
    assert_eq!(open[0].files, vec![f1.clone(), f2.clone()]);
    assert!(open[0].progress.is_empty());
    // Session open verification decodes the new rows (shape-strict).
    let view = s.ledger_view().unwrap();
    assert!(view.head.goal.is_empty(), "fold ignores edit txn rows");
    // Progress rows while open.
    s.ledger_edit_txn_progress(11, 0, "a.txt", "committed")
        .unwrap();
    s.ledger_edit_txn_progress(11, 1, "b.txt", "conflicted")
        .unwrap();
    let open = s.ledger_open_edit_txns().unwrap();
    assert_eq!(open[0].progress.len(), 2);
    assert_eq!(open[0].progress[1].outcome, "conflicted");
    // Terminal closes the transaction.
    s.ledger_edit_txn_committed(11, &["a.txt".to_string()], &["b.txt".to_string()], &[])
        .unwrap();
    assert!(s.ledger_open_edit_txns().unwrap().is_empty());
    // The typed stream keeps every row (strict decode, per session).
    let all = collect_all(&s);
    let kinds: Vec<&str> = all.iter().map(|e| e.entry_type.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "edit_txn_prepared",
            "edit_txn_progress",
            "edit_txn_progress",
            "edit_txn_committed"
        ]
    );
    // A second transaction with its own id is independent.
    s.ledger_edit_txn_prepared(12, "run-2", &[f1], "roll_back")
        .unwrap();
    let open = s.ledger_open_edit_txns().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].txn_id, 12);
    assert_eq!(open[0].strategy, "roll_back");
}

#[test]
fn edit_txn_open_rows_survive_restart_durably() {
    // The recovery record is DURABLE: reopen the store and the open
    // transaction is still there, decoded.
    let dir = tempfile::tempdir().unwrap();
    let (store, cas) = (dir.path().join("store"), dir.path().join("cas"));
    let id = {
        let m = crate::SessionManager::open(&store, &cas, true).unwrap();
        let ws = m.create_workspace("/w").unwrap();
        let s = m.create_session(ws, "t", "p", "m").unwrap();
        s.ledger_edit_txn_prepared(
            21,
            "run-1",
            &[edit_txn_file("a.txt", b"one")],
            "roll_forward",
        )
        .unwrap();
        s.id()
    };
    let m2 = crate::SessionManager::open(&store, &cas, true).unwrap();
    let s2 = m2.get_session(id).unwrap().unwrap();
    let open = s2.ledger_open_edit_txns().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].files[0].path, "a.txt");
}

#[test]
fn edit_txn_compaction_pins_open_rows_and_prunes_closed_ones() {
    let (_d, m) = test_manager();
    let s = session(&m);
    s.ledger_goal_set("g").unwrap();
    // OPEN transaction: prepared + one progress row, no terminal.
    s.ledger_edit_txn_prepared(
        31,
        "run-1",
        &[
            edit_txn_file("a.txt", b"one"),
            edit_txn_file("b.txt", b"two"),
        ],
        "roll_forward",
    )
    .unwrap();
    s.ledger_edit_txn_progress(31, 0, "a.txt", "committed")
        .unwrap();
    // A CLOSED transaction, older in the stream.
    s.ledger_edit_txn_prepared(
        32,
        "run-2",
        &[edit_txn_file("c.txt", b"three")],
        "roll_back",
    )
    .unwrap();
    s.ledger_edit_txn_progress(32, 0, "c.txt", "committed")
        .unwrap();
    s.ledger_edit_txn_committed(32, &["c.txt".to_string()], &[], &[])
        .unwrap();
    // Compaction while 31 is OPEN: 31's rows must survive, 32's (closed)
    // may be pruned.
    let report = s.compact_typed_ledger().unwrap();
    assert!(report.deleted > 0, "{report:?}");
    let all = collect_all(&s);
    let edit_rows: Vec<&TypedLedgerEntry> = all
        .iter()
        .filter(|e| {
            matches!(
                &e.payload,
                LedgerPayload::EditTxnPrepared { .. }
                    | LedgerPayload::EditTxnProgress { .. }
                    | LedgerPayload::EditTxnCommitted { .. }
                    | LedgerPayload::EditTxnRolledBack { .. }
            )
        })
        .collect();
    let open = s.ledger_open_edit_txns().unwrap();
    assert_eq!(open.len(), 1, "only txn 31 stays open");
    assert_eq!(open[0].txn_id, 31);
    assert_eq!(open[0].progress.len(), 1);
    assert!(
        edit_rows.iter().all(
            |e| matches!(&e.payload, LedgerPayload::EditTxnPrepared { txn_id, .. }
                    | LedgerPayload::EditTxnProgress { txn_id, .. } if *txn_id == 31)
        ),
        "compaction must prune closed txn rows and keep only open rows"
    );
    // Terminal row lands; the next compaction prunes the whole txn.
    s.ledger_edit_txn_committed(31, &["a.txt".to_string(), "b.txt".to_string()], &[], &[])
        .unwrap();
    s.compact_typed_ledger().unwrap();
    let all = collect_all(&s);
    assert!(
        !all.iter().any(|e| matches!(
            &e.payload,
            LedgerPayload::EditTxnPrepared { .. }
                | LedgerPayload::EditTxnProgress { .. }
                | LedgerPayload::EditTxnCommitted { .. }
                | LedgerPayload::EditTxnRolledBack { .. }
        )),
        "closed edit txn rows age out of the stream"
    );
    assert!(s.ledger_open_edit_txns().unwrap().is_empty());
}

#[test]
fn edit_txn_hostile_raw_store_rows_are_typed_and_never_silent() {
    // Craft rows DIRECTLY through the store (bypassing the typed
    // appenders) exactly like a hostile writer would. Every corruption
    // class must be a typed read error with NO open set returned, while
    // the session itself stays open. Each class gets a FRESH session:
    // a corrupt open txn stays corrupt for every read, by design.
    let (_d, m) = test_manager();
    let raw = |s: &SessionHandle, entry_type: &str, payload: serde_json::Value| {
        m.store()
            .append_ledger_entry(s.id(), entry_type, LEDGER_ENTRY_SCHEMA_V, payload)
            .unwrap();
    };
    let prepared_json = |txn: u64| {
        serde_json::json!({
            "kind": "edit_txn_prepared",
            "txn_id": txn,
            "session": "run-1",
            "files": [{"path": "a.txt", "base_digest": digest64(3), "base_bytes_len": 3}],
            "strategy": "roll_forward"
        })
    };
    let progress_json = |txn: u64, seq: u64, path: &str, outcome: &str| {
        serde_json::json!({
            "kind": "edit_txn_progress",
            "txn_id": txn,
            "seq": seq,
            "path": path,
            "outcome": outcome
        })
    };
    let committed_json = |txn: u64| {
        serde_json::json!({
            "kind": "edit_txn_committed",
            "txn_id": txn,
            "committed": ["a.txt"],
            "conflicted": [],
            "skipped": []
        })
    };
    // (f) hostile progress payload for a txn that was never prepared.
    let s = session(&m);
    raw(
        &s,
        "edit_txn_progress",
        progress_json(901, 0, "a.txt", "committed"),
    );
    let err = s.ledger_open_edit_txns().unwrap_err();
    assert!(err.to_string().contains("orphaned"), "{err}");
    // The session itself still opens and its other views are fine.
    assert!(m.get_session(s.id()).is_ok());
    assert!(s.ledger_view().is_ok());
    assert_eq!(collect_all(&s).len(), 1, "nothing was silently dropped");
    // Prepared + a progress row that DECODES but has a garbage outcome.
    let s = session(&m);
    raw(&s, "edit_txn_prepared", prepared_json(902));
    raw(
        &s,
        "edit_txn_progress",
        progress_json(902, 0, "a.txt", "sideways"),
    );
    let err = s.ledger_open_edit_txns().unwrap_err();
    assert!(err.to_string().contains("sideways"), "{err}");
    // Out-of-range seq against a one-file prepared row.
    let s = session(&m);
    raw(&s, "edit_txn_prepared", prepared_json(902));
    raw(
        &s,
        "edit_txn_progress",
        progress_json(902, 7, "a.txt", "committed"),
    );
    let err = s.ledger_open_edit_txns().unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
    // Duplicate progress seq.
    let s = session(&m);
    raw(&s, "edit_txn_prepared", prepared_json(903));
    raw(
        &s,
        "edit_txn_progress",
        progress_json(903, 0, "a.txt", "committed"),
    );
    raw(
        &s,
        "edit_txn_progress",
        progress_json(903, 0, "a.txt", "conflicted"),
    );
    let err = s.ledger_open_edit_txns().unwrap_err();
    assert!(err.to_string().contains("duplicate progress seq"), "{err}");
    // Progress path that does not match the prepared file at that seq.
    let s = session(&m);
    raw(&s, "edit_txn_prepared", prepared_json(904));
    raw(
        &s,
        "edit_txn_progress",
        progress_json(904, 0, "other.txt", "committed"),
    );
    let err = s.ledger_open_edit_txns().unwrap_err();
    assert!(
        err.to_string().contains("does not match prepared file"),
        "{err}"
    );
    // Duplicate prepared for one txn id (a re-begin is corruption).
    let s = session(&m);
    raw(&s, "edit_txn_prepared", prepared_json(905));
    raw(&s, "edit_txn_prepared", prepared_json(905));
    let err = s.ledger_open_edit_txns().unwrap_err();
    assert!(err.to_string().contains("duplicate prepared"), "{err}");
    // A second terminal for one txn id.
    let s = session(&m);
    raw(&s, "edit_txn_prepared", prepared_json(906));
    raw(&s, "edit_txn_committed", committed_json(906));
    raw(&s, "edit_txn_committed", committed_json(906));
    let err = s.ledger_open_edit_txns().unwrap_err();
    assert!(err.to_string().contains("duplicate terminal"), "{err}");
    // Terminal row without a prepared row.
    let s = session(&m);
    raw(&s, "edit_txn_committed", committed_json(907));
    let err = s.ledger_open_edit_txns().unwrap_err();
    assert!(err.to_string().contains("unknown edit txn"), "{err}");
    // In every hostile case the session stays open (nothing failed
    // its shape decode) and the crafted row is still there.
    assert!(m.get_session(s.id()).is_ok());
}

#[test]
fn edit_txn_appender_bounds_reject_before_journaling() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let ok_file = edit_txn_file("a.txt", b"one");
    // Zero / oversize / malformed inputs.
    assert!(s
        .ledger_edit_txn_prepared(0, "s", std::slice::from_ref(&ok_file), "roll_forward")
        .is_err());
    assert!(s
        .ledger_edit_txn_prepared(1, "", std::slice::from_ref(&ok_file), "roll_forward")
        .is_err());
    assert!(s
        .ledger_edit_txn_prepared(1, "s", &[], "roll_forward")
        .is_err());
    assert!(s
        .ledger_edit_txn_prepared(1, "s", std::slice::from_ref(&ok_file), "sideways")
        .is_err());
    let mut bad_digest = ok_file.clone();
    bad_digest.base_digest = "zz".repeat(32);
    assert!(s
        .ledger_edit_txn_prepared(1, "s", &[bad_digest], "roll_forward")
        .is_err());
    let mut long_path = ok_file.clone();
    long_path.path = "x".repeat(MAX_LEDGER_TEXT + 1);
    assert!(s
        .ledger_edit_txn_prepared(1, "s", &[long_path], "roll_forward")
        .is_err());
    // File-count bound: > MAX_EDIT_TXN_FILES files is refused.
    let many = vec![ok_file.clone(); MAX_EDIT_TXN_FILES + 1];
    let err = s
        .ledger_edit_txn_prepared(1, "s", &many, "roll_forward")
        .unwrap_err();
    assert_eq!(err.kind, faktor_core::ErrorKind::Oversized, "{err}");
    // Payload bound: one prepared row whose JSON exceeds the entry cap
    // is refused by the shared append tail (nothing journaled).
    let wide: Vec<EditTxnLedgerFile> = (0..400)
        .map(|i| EditTxnLedgerFile {
            path: format!("dir-{i}/{}", "f".repeat(120)),
            base_digest: ok_file.base_digest.clone(),
            base_bytes_len: 3,
        })
        .collect();
    let err = s
        .ledger_edit_txn_prepared(1, "s", &wide, "roll_forward")
        .unwrap_err();
    assert_eq!(err.kind, faktor_core::ErrorKind::Oversized, "{err}");
    // Progress / terminal shape bounds.
    assert!(s
        .ledger_edit_txn_progress(0, 0, "a.txt", "committed")
        .is_err());
    assert!(s.ledger_edit_txn_progress(1, 0, "", "committed").is_err());
    assert!(s.ledger_edit_txn_progress(1, 0, "a.txt", "maybe").is_err());
    assert!(s
        .ledger_edit_txn_committed(0, &["a".into()], &[], &[])
        .is_err());
    assert!(s
        .ledger_edit_txn_committed(1, &["".into()], &[], &[])
        .is_err());
    assert!(s
        .ledger_edit_txn_rolled_back(0, &["a".into()], &[])
        .is_err());
    assert!(s
        .ledger_edit_txn_rolled_back(1, &["a".into()], &["x".repeat(MAX_LEDGER_TEXT + 1)])
        .is_err());
    assert_eq!(collect_all(&s).len(), 0, "no hostile input was journaled");
}

/// Adversarial learning-record surface (audits 65-67/92): strict shape
/// bounds at append AND decode, verbatim payload round-trip, pinning
/// across watermark compaction, and a raw hostile row failing the read
/// loudly instead of being dropped.
#[test]
fn learning_records_roundtrip_pin_across_compaction_and_fail_loud() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let episode_seq = s
        .ledger_learning_record(LEARNING_RECORD_EPISODE, r#"{"episode":1}"#)
        .unwrap()
        .unwrap();
    let learning_seq = s
        .ledger_learning_record(LEARNING_RECORD_LEARNING, r#"{"learning":2}"#)
        .unwrap()
        .unwrap();
    let rows = s.ledger_learning_records().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].seq, episode_seq);
    assert_eq!(rows[0].record, LEARNING_RECORD_EPISODE);
    assert_eq!(rows[0].payload, r#"{"episode":1}"#);
    assert_eq!(rows[1].record, LEARNING_RECORD_LEARNING);

    // Shape bounds are enforced before anything is journaled: an
    // unknown kind, an empty payload and an oversized payload are all
    // loud typed errors.
    assert!(s.ledger_learning_record("bogus", "{}").is_err());
    assert!(s
        .ledger_learning_record(LEARNING_RECORD_LEARNING, "")
        .is_err());
    let oversized = "x".repeat(MAX_LEARNING_RECORD_PAYLOAD + 1);
    let err = s
        .ledger_learning_record(LEARNING_RECORD_LEARNING, &oversized)
        .unwrap_err();
    assert_eq!(err.kind, faktor_core::ErrorKind::Oversized, "{err}");
    assert_eq!(s.ledger_learning_records().unwrap().len(), 2);

    // Watermark compaction pins learning corpus rows: a mined learning
    // must never be silently aged out with turn history.
    s.ledger_goal_set("keep me").unwrap();
    let report = s.compact_typed_ledger().unwrap();
    assert!(report.pinned.contains(&episode_seq));
    assert!(report.pinned.contains(&learning_seq));
    assert_eq!(
        s.ledger_learning_records().unwrap().len(),
        2,
        "learning rows survive compaction"
    );

    // A raw hostile row (unknown record kind, bypassing the appender)
    // must fail every strict read and the session-open verification —
    // never be silently skipped.
    m.store()
        .append_ledger_entry(
            s.id(),
            ENTRY_LEARNING_RECORD,
            LEDGER_ENTRY_SCHEMA_V,
            serde_json::json!({
                "kind": "learning_record",
                "record": "bogus",
                "payload": "{}",
            }),
        )
        .unwrap();
    let err = s.ledger_learning_records().unwrap_err();
    assert!(
        err.to_string().contains("episode|learning|removed"),
        "{err}"
    );
    assert!(
        s.ledger_verify_open().is_err(),
        "corrupt row fails the open"
    );

    // A valid-shape row whose payload is semantically corrupt is still
    // decodable by THIS layer (shape only); the learning adapter is the
    // strict payload decoder and is covered in faktor-learning.
    let s2 = session(&m);
    s2.ledger_learning_record(LEARNING_RECORD_LEARNING, "not json")
        .unwrap();
    let rows = s2.ledger_learning_records().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].payload, "not json");
}

/// Adversarial completion-contract ledger surface (P2): immutability per
/// `(task, revision)`, latest-contract-wins across revisions, pinning
/// across watermark compaction, strict reopen, and hostile raw rows
/// failing every typed read loudly instead of being dropped.
#[test]
fn completion_contract_rows_are_immutable_pinned_and_reopen_stable() {
    let (dir, m) = test_manager();
    let s = session(&m);
    let sid = s.id;
    let contract_v3 = CompletionContract {
        include_commit: true,
        include_push: true,
        include_pr: false,
    };
    let contract_v4 = CompletionContract {
        include_commit: false,
        include_push: false,
        include_pr: true,
    };
    let seq_v3 = s
        .ledger_completion_contract_set(7, 3, &contract_v3)
        .unwrap()
        .unwrap();
    // Immutable per (task, revision).
    let err = s
        .ledger_completion_contract_set(7, 3, &contract_v4)
        .unwrap_err();
    assert_eq!(err.kind, faktor_core::ErrorKind::Conflict, "{err}");
    // A later revision is a new run: accepted and the newest row wins.
    let seq_v4 = s
        .ledger_completion_contract_set(7, 4, &contract_v4)
        .unwrap()
        .unwrap();
    let latest = s.ledger_completion_contract(7).unwrap().unwrap();
    assert_eq!(latest.revision, 4);
    assert_eq!(latest.contract, contract_v4);
    assert!(latest.seq > seq_v3);
    // The exact row of an older revision is still addressable.
    let old = s.ledger_completion_contract_at(7, 3).unwrap().unwrap();
    assert_eq!(old.contract, contract_v3);
    // Step statuses require the recorded contract revision.
    let status_seq = s
        .ledger_completion_step_status(
            7,
            4,
            CompletionStep::Pr,
            CompletionStepOutcome::Succeeded,
            "opened PR #12",
            99,
        )
        .unwrap()
        .unwrap();
    let err = s
        .ledger_completion_step_status(
            7,
            9,
            CompletionStep::Pr,
            CompletionStepOutcome::Succeeded,
            "orphan",
            99,
        )
        .unwrap_err();
    assert_eq!(err.kind, faktor_core::ErrorKind::Conflict, "{err}");
    let rows = s.ledger_completion_step_statuses(7, 4).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step, CompletionStep::Pr);
    assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
    assert_eq!(rows[0].detail, "opened PR #12");
    assert_eq!(rows[0].at_ms, 99);

    // Watermark compaction pins contract + status rows.
    s.ledger_goal_set("compaction pressure").unwrap();
    let report = s.compact_typed_ledger().unwrap();
    assert!(report.pinned.contains(&seq_v3));
    assert!(report.pinned.contains(&seq_v4));
    assert!(report.pinned.contains(&status_seq));
    assert!(collect_all(&s).iter().any(|e| matches!(
        &e.payload,
        LedgerPayload::CompletionContractSet { revision: 3, .. }
    )));

    // Reopen the exact store: the strict open verifies every row and the
    // reads converge byte-identically.
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    assert_eq!(
        s2.ledger_completion_contract(7).unwrap().unwrap().contract,
        contract_v4
    );
    assert_eq!(
        s2.ledger_completion_contract_at(7, 3)
            .unwrap()
            .unwrap()
            .contract,
        contract_v3
    );
    assert_eq!(s2.ledger_completion_step_statuses(7, 4).unwrap().len(), 1);
}

/// FIX 2: the explicit durable-read distinction is never collapsed — a
/// missing row, a present valid row, a present corrupt row and a failed
/// store read are four distinct outcomes on every classified read.
#[test]
fn durable_read_distinguishes_missing_valid_malformed_and_store_failure() {
    let (_d, m) = test_manager();
    let s = session(&m);
    // (1) Missing: nothing was written for these identities.
    assert!(s.ledger_completion_contract_read(7).is_missing());
    assert!(s.ledger_run_base_read("run-x").is_missing());
    assert!(s.ledger_integration_txn_read("run-x").is_missing());
    assert!(s.ledger_integration_record_for_task_read(7).is_missing());
    // (2) PresentValid: the typed appenders land decodable rows.
    let contract = CompletionContract {
        include_commit: true,
        include_push: false,
        include_pr: false,
    };
    s.ledger_completion_contract_set(7, 3, &contract)
        .unwrap()
        .unwrap();
    assert!(matches!(
        s.ledger_completion_contract_read(7),
        DurableRead::PresentValid(ref row) if row.contract == contract
    ));
    s.ledger_run_base_set(&RunBaseRecord {
        run_id: "run-x".into(),
        workspace_id: 1,
        worktree_id: 1,
        snapshot_hash: format!("tm1:{}", "a".repeat(64)),
        manifest_digest: "b".repeat(64),
        root: "/base".into(),
        created_ms: 1,
    })
    .unwrap();
    assert!(matches!(
        s.ledger_run_base_read("run-x"),
        DurableRead::PresentValid(ref row) if row.run_id == "run-x"
    ));
    // (3) PresentMalformed: a raw all-false contract row (bypassing the
    // typed appender) is CORRUPTION on read — never "no contract".
    let s2 = session(&m);
    m.store()
        .append_ledger_entry(
            s2.id,
            ENTRY_COMPLETION_CONTRACT_SET,
            LEDGER_ENTRY_SCHEMA_V,
            serde_json::json!({
                "kind": "completion_contract_set",
                "task_id": 9,
                "revision": 1,
                "contract": {
                    "include_commit": false,
                    "include_push": false,
                    "include_pr": false,
                },
            }),
        )
        .unwrap();
    assert!(
        s2.ledger_completion_contract_read(9).is_present_malformed(),
        "a corrupt contract row must classify as PresentMalformed"
    );
    // (4) StoreFailure: with the ledger table gone the SAME read is a
    // failed store read — an error, never Missing and never "valid".
    m.store().sql_execute("DROP TABLE ledger_entry").unwrap();
    assert!(matches!(
        s2.ledger_completion_contract_read(9),
        DurableRead::StoreFailure(_)
    ));
    assert!(matches!(
        s2.ledger_run_base_read("run-x"),
        DurableRead::StoreFailure(_)
    ));
}

/// Hostile completion rows fail the strict decode and the session-open
/// verification: an all-false contract row, an unknown step tag, an
/// unknown status tag, a non-positive `at_ms`, and an oversized detail.
#[test]
fn hostile_completion_rows_fail_loudly() {
    let (_d, m) = test_manager();
    let raw = |s: &SessionHandle, entry_type: &str, json: serde_json::Value| {
        m.store()
            .append_ledger_entry(s.id, entry_type, LEDGER_ENTRY_SCHEMA_V, json)
            .unwrap();
    };
    // The appender refuses an all-false contract before journaling.
    let s = session(&m);
    assert!(s
        .ledger_completion_contract_set(1, 1, &CompletionContract::default())
        .is_err());
    assert_eq!(collect_all(&s).len(), 0);
    // A raw all-false contract row is corruption: loud on read + open.
    raw(
        &s,
        ENTRY_COMPLETION_CONTRACT_SET,
        serde_json::json!({
            "kind": "completion_contract_set",
            "task_id": 1,
            "revision": 1,
            "contract": {
                "include_commit": false,
                "include_push": false,
                "include_pr": false,
            },
        }),
    );
    let err = s.ledger_completion_contract(1).unwrap_err();
    assert!(err.to_string().contains("all-false"), "{err}");
    assert!(s.ledger_verify_open().is_err());

    // Unknown step tag cannot even decode.
    let s = session(&m);
    raw(
        &s,
        ENTRY_COMPLETION_STEP_STATUS,
        serde_json::json!({
            "kind": "completion_step_status",
            "task_id": 1,
            "revision": 1,
            "step": "deploy",
            "status": "succeeded",
            "detail": "",
            "at_ms": 1,
        }),
    );
    let err = s.ledger_completion_step_statuses(1, 1).unwrap_err();
    assert!(err.to_string().contains("schema"), "{err}");

    // Unknown status tag.
    let s = session(&m);
    raw(
        &s,
        ENTRY_COMPLETION_STEP_STATUS,
        serde_json::json!({
            "kind": "completion_step_status",
            "task_id": 1,
            "revision": 1,
            "step": "push",
            "status": "maybe",
            "detail": "",
            "at_ms": 1,
        }),
    );
    assert!(s.ledger_completion_step_statuses(1, 1).is_err());

    // Non-positive at_ms.
    let s = session(&m);
    raw(
        &s,
        ENTRY_COMPLETION_STEP_STATUS,
        serde_json::json!({
            "kind": "completion_step_status",
            "task_id": 1,
            "revision": 1,
            "step": "push",
            "status": "succeeded",
            "detail": "",
            "at_ms": 0,
        }),
    );
    let err = s.ledger_completion_step_statuses(1, 1).unwrap_err();
    assert!(err.to_string().contains("at_ms"), "{err}");

    // Oversized detail.
    let s = session(&m);
    raw(
        &s,
        ENTRY_COMPLETION_STEP_STATUS,
        serde_json::json!({
            "kind": "completion_step_status",
            "task_id": 1,
            "revision": 1,
            "step": "push",
            "status": "succeeded",
            "detail": "x".repeat(MAX_COMPLETION_STEP_DETAIL + 1),
            "at_ms": 1,
        }),
    );
    let err = s.ledger_completion_step_statuses(1, 1).unwrap_err();
    assert_eq!(err.kind, faktor_core::ErrorKind::Oversized, "{err}");
}

/// Hardening: the explicit integration-identity fields
/// (`run_base_snapshot`/`candidate_snapshot`/`landed_snapshot`/
/// `proof_basis_digest`/`integration_txn_id`) round-trip a real reopen
/// byte-identically, and the txn id is the same before and after.
#[test]
fn integration_explicit_identity_fields_survive_reopen() {
    use faktor_core::id::SessionId as Sid;
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let cas = dir.path().join("cas");
    let m = crate::SessionManager::open(store.clone(), cas.clone(), true).unwrap();
    let s = session(&m);
    let sid: Sid = s.id;
    let hex = |c: char| c.to_string().repeat(64);
    let txn = IntegrationTxnRow {
        run_id: "run-identity".into(),
        task_id: 9,
        owner_root: "/owner".into(),
        candidate_root: "/candidate".into(),
        run_base_snapshot: hex('1'),
        verified_candidate_snapshot: hex('2'),
        sources_digest: hex('3'),
        phase: IntegrationTxnPhase::Landed,
        paths: Vec::new(),
        path_count: 0,
        applied_count: 0,
        conflicts: Vec::new(),
        at_ms: 5,
    };
    let txn_id = txn.txn_id();
    assert!(txn_id.starts_with("blake3:"));
    assert_eq!(txn_id, txn.txn_id(), "content identity is stable");
    s.ledger_integration_txn_set(&txn).unwrap();
    let record = IntegrationRecordRow {
        run_id: "run-identity".into(),
        task_id: 9,
        base_revision: None,
        base_snapshot: Some(hex('1')),
        run_base_snapshot: Some(hex('1')),
        candidate_snapshot: Some(hex('2')),
        landed_snapshot: Some(hex('4')),
        proof_basis_digest: Some(format!("blake3:{}", hex('5'))),
        integration_txn_id: Some(txn_id.clone()),
        final_root: "/owner".into(),
        final_snapshot_hash: hex('4'),
        integrated_files: vec!["src/lib.rs".into()],
        integrated_file_count: 1,
        integrated_files_digest: hex('6'),
        conflicts: Vec::new(),
        conflict_count: 0,
        sources: Vec::new(),
        source_count: 0,
        sources_digest: String::new(),
        at_ms: 6,
    };
    s.ledger_integration_record_set(&record).unwrap();
    assert_eq!(
        s.ledger_integration_record_for_task(9).unwrap().unwrap(),
        record
    );
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(store, cas, true).unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    assert_eq!(
        s2.ledger_integration_record_for_task(9).unwrap().unwrap(),
        record,
        "explicit identity fields must survive a real reopen"
    );
    let txn2 = s2.ledger_integration_txn_for_run("run-identity").unwrap();
    assert_eq!(txn2.unwrap().txn_id(), txn_id, "txn id survives reopen");
}

/// The ledger's recorded proof-basis digest is the SAME config-bound
/// digest the root verification record embeds: a layered configuration
/// change (a system-layer value) changes the basis digest, and the
/// ledger's binding check refuses the stale basis across a real reopen.
#[test]
fn ledger_proof_basis_binding_carries_the_effective_config_digest() {
    use faktor_core::id::SessionId as Sid;
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let cas = dir.path().join("cas");
    let m = crate::SessionManager::open(store.clone(), cas.clone(), true).unwrap();
    let s = session(&m);
    let sid: Sid = s.id;
    let hex = |c: char| c.to_string().repeat(64);
    let config = |system_value: &str| {
        crate::task::layered_effective_config_digest(&[crate::task::ProofConfigLayer::of_value(
            crate::task::ProofConfigScope::System,
            7,
            system_value,
        )])
        .unwrap()
    };
    let basis_digest = |system_value: &str| {
        crate::task::ProofBasis {
            task_id: 1,
            task_revision: 1,
            task_contract_digest: hex('1'),
            candidate_snapshot: hex('2'),
            integration_sources_digest: hex('3'),
            changed_files_digest: hex('4'),
            checks: Vec::new(),
            verification_impl_version: "faktor-test/0.1".into(),
            tool_versions: Vec::new(),
            env_projection: vec![("RUSTFLAGS".into(), "<absent>".into())],
            instruction_epoch: None,
            criteria: Vec::new(),
            reviewer_digest: None,
            evidence_digests: Vec::new(),
        }
        .bind_config_digest(&config(system_value))
        .unwrap()
        .digest()
    };
    let recorded = basis_digest("network=allow");
    assert_ne!(recorded, basis_digest("network=deny"));
    let record = IntegrationRecordRow {
        run_id: "run-config-basis".into(),
        task_id: 1,
        base_revision: None,
        base_snapshot: Some(hex('1')),
        run_base_snapshot: Some(hex('1')),
        candidate_snapshot: Some(hex('2')),
        landed_snapshot: Some(hex('5')),
        proof_basis_digest: Some(recorded.clone()),
        integration_txn_id: None,
        final_root: "/owner".into(),
        final_snapshot_hash: hex('5'),
        integrated_files: Vec::new(),
        integrated_file_count: 0,
        integrated_files_digest: String::new(),
        conflicts: Vec::new(),
        conflict_count: 0,
        sources: Vec::new(),
        source_count: 0,
        sources_digest: String::new(),
        at_ms: 7,
    };
    s.ledger_integration_record_set(&record).unwrap();
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(store, cas, true).unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    let read = s2.ledger_integration_record_for_task(1).unwrap().unwrap();
    assert_eq!(read.proof_basis_digest(), Some(recorded.as_str()));
    assert!(read.is_bound_to_proof_basis(&recorded));
    assert!(
        !read.is_bound_to_proof_basis(&basis_digest("network=deny")),
        "a changed system-layer value must not be reusable as the recorded basis"
    );
}

/// Hardening: a tampered raw row that disagrees between the deprecated
/// alias and the explicit field (or binds a hostile digest) is a typed
/// read error — never a silently accepted identity.
#[test]
fn hostile_integration_identity_rows_fail_loudly() {
    let (_d, m) = test_manager();
    let raw = |s: &SessionHandle, record: serde_json::Value| {
        m.store()
            .append_ledger_entry(
                s.id(),
                ENTRY_INTEGRATION_RECORD,
                LEDGER_ENTRY_SCHEMA_V,
                serde_json::json!({ "kind": "integration_recorded", "record": record }),
            )
            .unwrap();
    };
    let base = |record: serde_json::Value| {
        let mut v = serde_json::json!({
            "run_id": "run-hostile",
            "task_id": 1,
            "final_root": "/owner",
            "final_snapshot_hash": "44".repeat(32),
            "integrated_files": [],
            "integrated_file_count": 0,
            "integrated_files_digest": "",
            "conflicts": [],
            "conflict_count": 0,
            "sources": [],
            "source_count": 0,
            "sources_digest": "",
            "at_ms": 1,
        });
        for (k, val) in record.as_object().unwrap() {
            v[k.as_str()] = val.clone();
        }
        v
    };
    // Deprecated alias and explicit run-base field disagree.
    let s = session(&m);
    raw(
        &s,
        base(serde_json::json!({
            "base_snapshot": "11".repeat(32),
            "run_base_snapshot": "22".repeat(32),
        })),
    );
    let err = s.ledger_integration_record_for_task(1).unwrap_err();
    assert!(err.to_string().contains("disagree"), "{err}");
    // Landed snapshot and final hash disagree.
    let s = session(&m);
    raw(
        &s,
        base(serde_json::json!({
            "run_base_snapshot": "11".repeat(32),
            "landed_snapshot": "22".repeat(32),
        })),
    );
    let err = s.ledger_integration_record_for_task(1).unwrap_err();
    assert!(err.to_string().contains("disagree"), "{err}");
    // Hostile non-hex explicit digest.
    let s = session(&m);
    raw(
        &s,
        base(serde_json::json!({ "candidate_snapshot": "not-hex" })),
    );
    assert!(s.ledger_integration_record_for_task(1).is_err());
    // Legacy row WITHOUT the new fields decodes additively (never a
    // missing-field failure).
    let s = session(&m);
    raw(&s, base(serde_json::json!({})));
    let legacy = s.ledger_integration_record_for_task(1).unwrap().unwrap();
    assert!(legacy.run_base_snapshot.is_none());
    assert!(legacy.candidate_snapshot.is_none());
    assert!(legacy.landed_snapshot.is_none());
    assert!(legacy.proof_basis_digest.is_none());
    assert!(legacy.integration_txn_id.is_none());
}

// -------------------------------------------------------- terminal rows

fn terminal_row(terminal_id: &str) -> TerminalDurableRow {
    TerminalDurableRow {
        terminal_id: terminal_id.to_string(),
        session_id: 1,
        task_id: 1,
        agent_id: None,
        operation_id: 7,
        pid: 4242,
        start_time_ms: 1_700_000_000_000,
        at_ms: 1_700_000_000_100,
        execution_profile: String::new(),
    }
}

#[test]
fn terminal_lifecycle_rows_round_trip_and_survive_compaction() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let row = terminal_row("7f1a2b3c-0000-4000-8000-000000000001");
    s.ledger_terminal_created(&row).unwrap();
    s.ledger_terminal_running(&row).unwrap();
    let exited = TerminalDurableRow {
        at_ms: row.at_ms + 10,
        ..row.clone()
    };
    s.ledger_terminal_exited(&exited, Some(0)).unwrap();

    // A hostile consumer cannot forge an illegal reconcile disposition.
    let err = s
        .ledger_terminal_reconciled(&exited, "obliterated")
        .unwrap_err();
    assert!(err.to_string().contains("killed|collected"), "{err}");

    let records = s.ledger_terminal_rows(None).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].kind, TerminalEventKind::Created);
    assert_eq!(records[1].kind, TerminalEventKind::Running);
    assert_eq!(records[2].kind, TerminalEventKind::Exited);
    assert_eq!(records[2].exit_code, Some(0));
    assert_eq!(records[2].row.pid, 4242);
    assert_eq!(records[2].row.start_time_ms, 1_700_000_000_000);

    // The terminal stream is pinned across compaction: turn history is
    // pruned, every terminal row survives byte-for-byte.
    for index in 0..40 {
        turn_entries(&s, index + 1);
    }
    let report = s.compact_typed_ledger().unwrap();
    assert!(report.deleted > 0, "{report:?}");
    let after = s.ledger_terminal_rows(None).unwrap();
    assert_eq!(after.len(), 3, "terminal rows are never evicted");
    assert_eq!(after[0].row.terminal_id, row.terminal_id);
    assert_eq!(after[2].kind, TerminalEventKind::Exited);
}

#[test]
fn terminal_execution_profile_round_trips_durably() {
    // The effective execution profile is a durable row fact: it survives
    // the round trip byte-for-byte (a legacy empty profile stays legal).
    let (_d, m) = test_manager();
    let s = session(&m);
    let mut profiled = terminal_row("7f1a2b3c-0000-4000-8000-00000000000f");
    profiled.execution_profile = "{\"cwd\":\"/tmp\",\"capabilities\":\"*\"}".into();
    s.ledger_terminal_created(&profiled).unwrap();
    let records = s.ledger_terminal_rows(None).unwrap();
    let stored = records
        .iter()
        .find(|record| record.row.terminal_id == profiled.terminal_id)
        .expect("profiled row");
    assert_eq!(stored.row.execution_profile, profiled.execution_profile);
}

#[test]
fn terminal_row_shape_violations_are_loud_and_never_parse() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let mut zero_pid = terminal_row("7f1a2b3c-0000-4000-8000-000000000002");
    zero_pid.pid = 0;
    assert!(s.ledger_terminal_created(&zero_pid).is_err());
    let mut hostile_id = terminal_row("7f1a2b3c-0000-4000-8000-000000000003");
    hostile_id.terminal_id = "a".repeat(MAX_TERMINAL_ID_BYTES + 1);
    assert!(s.ledger_terminal_running(&hostile_id).is_err());
    let mut slash = terminal_row("bad/id");
    slash.terminal_id = "bad/id".into();
    assert!(s.ledger_terminal_created(&slash).is_err());
    let mut oversized_profile = terminal_row("7f1a2b3c-0000-4000-8000-000000000005");
    oversized_profile.execution_profile = "p".repeat(MAX_TERMINAL_PROFILE_BYTES + 1);
    assert!(s.ledger_terminal_created(&oversized_profile).is_err());
    let mut nul_profile = terminal_row("7f1a2b3c-0000-4000-8000-000000000006");
    nul_profile.execution_profile = "profile\0evil".into();
    assert!(s.ledger_terminal_created(&nul_profile).is_err());
    // Nothing was journaled by the refusals.
    assert!(s.ledger_terminal_rows(None).unwrap().is_empty());

    // A raw-store hostile row (valid JSON, wrong shape) fails the strict
    // decode loudly on read instead of being treated as absent.
    s.ledger_terminal_created(&terminal_row("7f1a2b3c-0000-4000-8000-000000000004"))
        .unwrap();
    raw_sql(
        &m,
        "UPDATE ledger_entry SET payload = json('{\"kind\":\"terminal_created\"}') \
             WHERE entry_type = 'terminal_created'",
    );
    let err = s.ledger_terminal_rows(None).unwrap_err();
    assert!(err.to_string().contains("schema"), "{err}");
}

// ------------------------------------------- external-operation identity

const EXTERNAL_OP_KEY: &str = "task:1:rev:1:github:pull_request";

fn external_operation_row(
    key: &str,
    head: &str,
    state: ExternalOperationState,
) -> ExternalOperationRow {
    let input = ExternalOperationInput {
        organization: "acme".into(),
        repository: "widgets".into(),
        head: head.into(),
        base: "main".into(),
        marker: "faktor:task:1:rev:1".into(),
    };
    let completed = state == ExternalOperationState::Completed;
    ExternalOperationRow {
        id: ExternalOperationRow::content_id(key, "github", "pull_request", &input),
        operation_key: key.into(),
        provider: "github".into(),
        kind: "pull_request".into(),
        input,
        state,
        remote_object_id: completed.then(|| "pr-1".into()),
        remote_object_version: completed.then(|| "v1".into()),
        started_at: 7,
        reconciled_at: None,
    }
}

#[test]
fn external_operation_rows_round_trip_and_latest_state_wins() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let prepared =
        external_operation_row(EXTERNAL_OP_KEY, "main", ExternalOperationState::Prepared);
    s.ledger_external_operation_set(&prepared).unwrap();
    assert!(s.ledger_external_operation_read("other-key").is_missing());
    assert_eq!(
        s.ledger_external_operation_read(EXTERNAL_OP_KEY).valid(),
        Some(prepared.clone())
    );
    let mut completed = prepared.clone();
    completed.state = ExternalOperationState::Completed;
    completed.remote_object_id = Some("pr-1".into());
    completed.remote_object_version = Some("v1".into());
    completed.reconciled_at = Some(8);
    s.ledger_external_operation_set(&completed).unwrap();
    assert_eq!(
        s.ledger_external_operation_read(EXTERNAL_OP_KEY).valid(),
        Some(completed.clone()),
        "the latest row of one operation key wins"
    );
    assert_eq!(
        s.ledger_external_operations().unwrap(),
        vec![prepared, completed]
    );
    // A hostile raw row is a LOUD read failure, never "no operation".
    raw_sql(
        &m,
        "UPDATE ledger_entry SET payload = json('{\"kind\":\"external_operation_recorded\"}') \
             WHERE entry_type = 'external_operation'",
    );
    assert!(s
        .ledger_external_operations()
        .unwrap_err()
        .to_string()
        .contains("schema"));
    assert!(s
        .ledger_external_operation_read(EXTERNAL_OP_KEY)
        .is_present_malformed());
}

#[test]
fn external_operation_rows_refuse_shape_violations() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let prepared =
        external_operation_row(EXTERNAL_OP_KEY, "main", ExternalOperationState::Prepared);
    // A hand-mismatched deterministic id.
    let mut tampered = prepared.clone();
    tampered.id = format!("blake3:{}", "0".repeat(64));
    assert!(s.ledger_external_operation_set(&tampered).is_err());
    // A prepared row may not carry a remote identity.
    let mut bogus = prepared.clone();
    bogus.remote_object_id = Some("pr-1".into());
    assert!(s.ledger_external_operation_set(&bogus).is_err());
    // A completed row must carry both remote id and version.
    let mut incomplete = prepared.clone();
    incomplete.state = ExternalOperationState::Completed;
    assert!(s.ledger_external_operation_set(&incomplete).is_err());
    // reconciled_at may not precede started_at.
    let mut backwards = prepared.clone();
    backwards.state = ExternalOperationState::Failed;
    backwards.reconciled_at = Some(1);
    assert!(s.ledger_external_operation_set(&backwards).is_err());
    // Control characters are refused.
    let mut hostile = prepared.clone();
    hostile.input.head = "main\n".into();
    assert!(s.ledger_external_operation_set(&hostile).is_err());
    // A different head is a DIFFERENT row (the operation key conflict is
    // resolved by the caller, never by an id collision).
    let other = external_operation_row(
        EXTERNAL_OP_KEY,
        "other-branch",
        ExternalOperationState::Prepared,
    );
    assert_ne!(prepared.id, other.id);
    // Nothing was journaled by the refusals.
    assert!(s.ledger_external_operations().unwrap().is_empty());
}

#[test]
fn external_operation_rows_survive_compaction() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let prepared =
        external_operation_row(EXTERNAL_OP_KEY, "main", ExternalOperationState::Prepared);
    s.ledger_external_operation_set(&prepared).unwrap();
    // Unpinned rows below the watermark so the compaction has victims.
    for i in 0..5 {
        s.ledger_failure_recorded(&format!("failure-{i}")).unwrap();
    }
    let mut completed = prepared.clone();
    completed.state = ExternalOperationState::Completed;
    completed.remote_object_id = Some("pr-1".into());
    completed.remote_object_version = Some("v1".into());
    s.ledger_external_operation_set(&completed).unwrap();
    let report = s.compact_typed_ledger().unwrap();
    assert!(report.deleted > 0, "{report:?}");
    assert_eq!(
        s.ledger_external_operation_read(EXTERNAL_OP_KEY).valid(),
        Some(completed),
        "the reconciliation authority outlives compaction"
    );
}

#[test]
fn legacy_fnv_authority_digests_are_refused_at_the_ledger_boundary() {
    let (_d, m) = test_manager();
    let s = session(&m);
    // The canonical (BLAKE3) shapes classify as authoritative.
    let canonical = RunBaseRecord {
        run_id: "run-ok".into(),
        workspace_id: 1,
        worktree_id: 1,
        snapshot_hash: format!("tm1:{}", "a".repeat(64)),
        manifest_digest: "b".repeat(64),
        root: "/base".into(),
        created_ms: 1,
    };
    assert_eq!(
        canonical.manifest_digest_kind(),
        AuthorityDigestKind::Blake3Hex
    );
    assert!(canonical.legacy_manifest_digest().is_none());
    s.ledger_run_base_set(&canonical).unwrap();

    // A legacy 64-bit FNV manifest digest is refused typed, and nothing
    // is appended (the row can be VIEWED only from before the change).
    let legacy = RunBaseRecord {
        manifest_digest: "0123456789abcdef".into(),
        ..canonical.clone()
    };
    assert_eq!(
        legacy.manifest_digest_kind(),
        AuthorityDigestKind::LegacyFnv
    );
    let err = s.ledger_run_base_set(&legacy).unwrap_err();
    assert!(
        err.to_string().contains("legacy FNV") && err.to_string().contains("restage"),
        "{err}"
    );
    assert_eq!(
        s.ledger_run_base_read("run-ok").valid(),
        Some(canonical),
        "the refused legacy append wrote nothing"
    );

    // A labelled FNV digest in an integration record is refused typed
    // before any row lands.
    let hex = |c: char| c.to_string().repeat(64);
    let mut record = IntegrationRecordRow {
        run_id: "run-legacy".into(),
        task_id: 7,
        base_revision: None,
        base_snapshot: Some(hex('1')),
        run_base_snapshot: Some(hex('1')),
        candidate_snapshot: Some(hex('2')),
        landed_snapshot: Some(hex('4')),
        proof_basis_digest: Some(format!("blake3:{}", hex('5'))),
        integration_txn_id: None,
        final_root: "/owner".into(),
        final_snapshot_hash: hex('4'),
        integrated_files: Vec::new(),
        integrated_file_count: 0,
        integrated_files_digest: String::new(),
        conflicts: Vec::new(),
        conflict_count: 0,
        sources: vec![IntegrationSourceRow {
            child_id: "child-1".into(),
            change_set_id: format!("cs-{}", hex('7')),
            candidate_root_hash: hex('8'),
        }],
        source_count: 1,
        sources_digest: hex('9'),
        at_ms: 6,
    };
    assert_eq!(record.sources_digest_kind(), AuthorityDigestKind::Blake3Hex);
    record.sources_digest = "fnv1a64:fedcba9876543210".into();
    assert!(record.legacy_authority_digest().is_some());
    let err = s.ledger_integration_record_set(&record).unwrap_err();
    assert!(err.to_string().contains("legacy FNV"), "{err}");
    assert!(s.ledger_integration_record_for_task_read(7).is_missing());
}
