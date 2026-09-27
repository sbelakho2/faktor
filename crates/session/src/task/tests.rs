//! Task tests (mechanically split from `task`).

use super::*;
use crate::budget::BudgetAuthority;
use crate::handle::tests::{session, test_manager};
use crate::SessionManager;
use faktor_core::ErrorKind;
use std::sync::Arc;
use std::thread;

fn task(s: &SessionHandle) -> Task {
    Task {
        task_id: s.task_id().unwrap(),
        session_id: s.id,
        goal: "implement durable tasks".into(),
        acceptance_criteria: vec!["goal: implement durable tasks".into()],
        plan: vec!["schema".into(), "repo".into()],
        attachments: vec![],
        budget: TaskBudget {
            max_tokens: Some(100_000),
            max_turns: Some(10),
            spent_tokens: 0,
            spent_turns: 0,
        },
        state: TaskState::Pending,
        created_ms: 1,
        updated_ms: 1,
    }
}

fn criteria_task(s: &SessionHandle, task_id: TaskId, criteria: Vec<String>) -> Task {
    Task {
        task_id,
        session_id: s.id,
        goal: "gated goal".into(),
        acceptance_criteria: criteria,
        plan: vec![],
        attachments: vec![],
        budget: TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: 1,
        updated_ms: 1,
    }
}

/// Drive a task Pending -> Running -> NeedsVerification -> Verifying
/// through legal transitions; returns the revision at Verifying.
fn drive_to_verifying(s: &SessionHandle, task_id: TaskId) -> TaskRevision {
    let r1 = s.task_revision(task_id).unwrap();
    s.transition_task(task_id, r1, TaskTransition::StartRunning, None)
        .unwrap();
    let r2 = s.task_revision(task_id).unwrap();
    s.transition_task(task_id, r2, TaskTransition::RequestVerification, None)
        .unwrap();
    let r3 = s.task_revision(task_id).unwrap();
    s.transition_task(task_id, r3, TaskTransition::StartVerification, None)
        .unwrap();
    s.task_revision(task_id).unwrap()
}

fn passed_record(s: &SessionHandle, task_id: TaskId, criteria: &[String]) -> VerificationRecordId {
    let criteria: Vec<CriterionVerification> = criteria
        .iter()
        .map(|c| CriterionVerification {
            criterion_key: c.clone(),
            passed: true,
            evidence: Some("exit 0".into()),
            binding: None,
        })
        .collect();
    s.create_verification_record(
        task_id,
        None,
        criteria,
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        1,
    )
    .unwrap()
}

/// The manifest-bound twin of [`passed_record`] (FIX 1): every verdict
/// carries the criterion's own effective binding, so MODERN (V2) criteria
/// are covered through their manifest. Legacy plain-text criteria carry
/// the deterministically migrated binding too (honest and harmless).
fn passed_record_bound(
    s: &SessionHandle,
    task_id: TaskId,
    criteria: &[String],
) -> VerificationRecordId {
    let criteria: Vec<CriterionVerification> = criteria
        .iter()
        .map(|c| {
            let binding = Criterion::decode(c)
                .map(|decoded| decoded.effective_binding())
                .unwrap_or_else(|| legacy_binding_for_criterion_text(c));
            CriterionVerification {
                criterion_key: c.clone(),
                passed: true,
                evidence: Some("exit 0".into()),
                binding: Some(binding),
            }
        })
        .collect();
    s.create_verification_record(
        task_id,
        None,
        criteria,
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        1,
    )
    .unwrap()
}

#[test]
fn task_create_get_update_list_roundtrip() {
    let (_d, m) = test_manager();
    let s = session(&m);
    assert!(s.list_tasks().unwrap().is_empty());
    let t = task(&s);
    let created = s.create_task(t.clone()).unwrap();
    assert_eq!(created.state, TaskState::Pending);
    assert_eq!(s.get_task(created.task_id).unwrap(), Some(created.clone()));
    assert_eq!(
        s.task_revision(created.task_id).unwrap(),
        TaskRevision::new(1)
    );
    // Patch: state + spend move forward; untouched fields survive; the
    // effective change bumps the revision exactly once.
    let patched = s
        .update_task(
            created.task_id,
            TaskPatch {
                state: Some(TaskState::Running),
                budget: Some(TaskBudget {
                    max_tokens: Some(100_000),
                    max_turns: Some(10),
                    spent_tokens: 40,
                    spent_turns: 1,
                }),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(patched.state, TaskState::Running);
    assert_eq!(patched.budget.spent_tokens, 40);
    assert_eq!(patched.goal, created.goal, "unpatched fields survive");
    assert_eq!(patched.created_ms, created.created_ms);
    assert_eq!(
        s.task_revision(created.task_id).unwrap(),
        TaskRevision::new(2)
    );
    assert_eq!(s.get_task(created.task_id).unwrap(), Some(patched.clone()));
    assert_eq!(s.list_tasks().unwrap().len(), 1);
}

#[test]
fn task_attachments_are_durable_typed_and_separate_from_paths() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let pdf = s
        .put_attachment("application/pdf", Some("spec.pdf"), b"%PDF-1.4 spec")
        .unwrap();
    let mut t = task(&s);
    t.attachments = vec![pdf.clone()];
    let created = s.create_task(t.clone()).unwrap();
    assert_eq!(created.attachments, vec![pdf.clone()]);
    // The durable row resolves the byte-identical typed list.
    assert_eq!(
        s.get_task(created.task_id).unwrap().unwrap().attachments,
        vec![pdf.clone()]
    );
    // A patch REPLACES the set (never merges silently).
    let second = s
        .put_attachment("text/plain", Some("notes.txt"), b"notes")
        .unwrap();
    let patched = s
        .update_task(
            created.task_id,
            TaskPatch {
                attachments: Some(vec![second.clone()]),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(patched.attachments, vec![second.clone()]);
    // Hostile attachment sets are typed refusals before any write.
    let mut hostile = t.clone();
    hostile.attachments = vec![AttachmentId {
        filename: Some("../escape".into()),
        ..second.clone()
    }];
    let err = s.create_task(Task {
        task_id: TaskId::new(9),
        ..hostile
    });
    assert!(matches!(err.unwrap_err().kind, ErrorKind::Malformed));
    let mut many = t.clone();
    many.attachments = vec![second; MAX_ATTACHMENTS_PER_TASK + 1];
    assert!(matches!(
        s.create_task(many).unwrap_err().kind,
        ErrorKind::Oversized
    ));
}

#[test]
fn update_on_missing_task_is_not_found() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let err = s
        .update_task(TaskId::new(999), TaskPatch::default())
        .unwrap_err();
    assert_eq!(err, TaskError::NotFound(TaskId::new(999)));
}

#[test]
fn oversized_goal_criteria_and_plan_are_rejected_never_truncated() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let mut t = task(&s);
    t.goal = "g".repeat(MAX_TASK_GOAL_BYTES + 1);
    assert!(matches!(
        s.create_task(t.clone()).unwrap_err().kind,
        ErrorKind::Oversized
    ));
    t.goal = "ok".into();
    t.acceptance_criteria = (0..=MAX_TASK_CRITERIA)
        .map(|i| format!("criterion {i}"))
        .collect();
    assert!(matches!(
        s.create_task(t.clone()).unwrap_err().kind,
        ErrorKind::Oversized
    ));
    t.acceptance_criteria = vec!["c".repeat(MAX_TASK_CRITERION_BYTES + 1)];
    assert!(matches!(
        s.create_task(t.clone()).unwrap_err().kind,
        ErrorKind::Oversized
    ));
    t.acceptance_criteria = vec!["c".into()];
    t.plan = (0..=MAX_TASK_PLAN_STEPS)
        .map(|i| format!("step {i}"))
        .collect();
    assert!(matches!(
        s.create_task(t.clone()).unwrap_err().kind,
        ErrorKind::Oversized
    ));
    // Rejection leaves NO trace: the store stays empty, and the
    // rejected values were never silently truncated into the row.
    assert!(s.list_tasks().unwrap().is_empty());
}

#[test]
fn oversized_patch_fields_are_rejected() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let created = s.create_task(task(&s)).unwrap();
    let err = s
        .update_task(
            created.task_id,
            TaskPatch {
                goal: Some("x".repeat(MAX_TASK_GOAL_BYTES + 1)),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Oversized(_)));
    let row = s.get_task(created.task_id).unwrap().unwrap();
    assert_eq!(row.goal, created.goal, "rejected patch left no trace");
    let err = s
        .update_task(
            created.task_id,
            TaskPatch {
                plan: Some((0..=MAX_TASK_PLAN_STEPS).map(|i| format!("s{i}")).collect()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Oversized(_)));
    // A no-op patch (nothing changes) writes nothing and bumps nothing.
    let rev_before = s.task_revision(created.task_id).unwrap();
    let noop = s
        .update_task(created.task_id, TaskPatch::default())
        .unwrap();
    assert_eq!(noop, created);
    assert_eq!(s.task_revision(created.task_id).unwrap(), rev_before);
}

#[test]
fn budget_spend_only_moves_forward() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let created = s.create_task(task(&s)).unwrap();
    let bump = |spent_tokens: u64| TaskPatch {
        budget: Some(TaskBudget {
            max_tokens: Some(100_000),
            max_turns: Some(10),
            spent_tokens,
            spent_turns: 3,
        }),
        ..Default::default()
    };
    let p1 = s.update_task(created.task_id, bump(50)).unwrap();
    assert_eq!(p1.budget.spent_tokens, 50);
    // A rewind attempt is refused by construction: the effective content
    // is unchanged, so NOTHING is written and the revision does not move.
    let rev = s.task_revision(created.task_id).unwrap();
    let p2 = s.update_task(created.task_id, bump(10)).unwrap();
    assert_eq!(p2.budget.spent_tokens, 50, "spend is monotone");
    assert_eq!(s.task_revision(created.task_id).unwrap(), rev);
    // max fields DO update (they are not counters) and bump.
    let p3 = s
        .update_task(
            created.task_id,
            TaskPatch {
                budget: Some(TaskBudget {
                    max_tokens: Some(5),
                    max_turns: Some(1),
                    spent_tokens: 0,
                    spent_turns: 0,
                }),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(p3.budget.max_tokens, Some(5));
    assert_eq!(p3.budget.spent_tokens, 50);
    assert_eq!(
        s.task_revision(created.task_id).unwrap(),
        TaskRevision::new(3)
    );
}

#[test]
fn tasks_are_session_scoped() {
    let (_d, m) = test_manager();
    let s1 = session(&m);
    let s2 = {
        let ws = m.create_workspace("/w2").unwrap();
        m.create_session(ws, "t2", "p", "m").unwrap()
    };
    let t1 = s1.create_task(task(&s1)).unwrap();
    assert!(s2.list_tasks().unwrap().is_empty());
    assert!(s2.get_task(t1.task_id).unwrap().is_none());
}

// (a) the direct patch cannot reach completion states or jump edges.
#[test]
fn patch_cannot_reach_completion_states_or_jump_edges() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let created = s.create_task(task(&s)).unwrap();
    for forbidden in [
        TaskState::VerifiedComplete,
        TaskState::Verifying,
        TaskState::NeedsVerification,
    ] {
        let err = s
            .update_task(
                created.task_id,
                TaskPatch {
                    state: Some(forbidden),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert_eq!(
            err,
            TaskError::CompletionStateViaPatch { state: forbidden },
            "{forbidden:?} must be patch-unreachable"
        );
    }
    let row = s.get_task(created.task_id).unwrap().unwrap();
    assert_eq!(row.state, TaskState::Pending, "state unchanged");
    assert_eq!(
        s.task_revision(created.task_id).unwrap(),
        TaskRevision::new(1)
    );
    // A non-completion edge the machine forbids (Pending -> Failed).
    let err = s
        .update_task(
            created.task_id,
            TaskPatch {
                state: Some(TaskState::Failed),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        TaskError::IllegalTransition {
            from: TaskState::Pending,
            to: TaskState::Failed,
            ..
        }
    ));
    assert_eq!(
        s.task_revision(created.task_id).unwrap(),
        TaskRevision::new(1)
    );
    // Legal ordinary edges apply and bump exactly once.
    s.update_task(
        created.task_id,
        TaskPatch {
            state: Some(TaskState::Running),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        s.task_revision(created.task_id).unwrap(),
        TaskRevision::new(2)
    );
}

// (b) create_task admits only Pending/Planning/Running and never
// recreates an existing row.
#[test]
fn create_task_rejects_uncreatable_states_and_recreation() {
    let (_d, m) = test_manager();
    let s = session(&m);
    for forbidden in [
        TaskState::VerifiedComplete,
        TaskState::Verifying,
        TaskState::NeedsVerification,
        TaskState::Failed,
        TaskState::Cancelled,
        TaskState::Waiting,
        TaskState::Blocked,
    ] {
        let mut t = task(&s);
        t.state = forbidden;
        let err = s.create_task(t.clone()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert!(
            err.message.contains("create_task refused"),
            "{forbidden:?}: {}",
            err.message
        );
    }
    assert!(
        s.list_tasks().unwrap().is_empty(),
        "no trace of refused creates"
    );
    let t = task(&s);
    let created = s.create_task(t.clone()).unwrap();
    assert_eq!(created.state, TaskState::Pending);
    let again = s.create_task(t.clone()).unwrap_err();
    assert_eq!(again.kind, ErrorKind::Conflict);
    assert!(again.message.contains("already exists"));
    assert_eq!(s.list_tasks().unwrap().len(), 1);
    assert_eq!(
        s.task_revision(created.task_id).unwrap(),
        TaskRevision::new(1)
    );
}

// (c)+(d) the completion transaction: typed refusals for every broken
// proof facet, state and revision untouched, happy path bumps exactly
// once, and VerifiedComplete is terminal.
#[test]
fn completion_requires_proof_records_revision_criteria_and_worktree() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let main = s
        .create_task(criteria_task(
            &s,
            s.task_id().unwrap(),
            vec!["c1".into(), "c2".into()],
        ))
        .unwrap();
    let t1 = main.task_id;
    let rev_at_verifying = drive_to_verifying(&s, t1);
    let record = passed_record(&s, t1, &["c1".into(), "c2".into()]);
    let done = s
        .complete_verified_task(t1, rev_at_verifying, record)
        .unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
    assert_eq!(
        s.task_revision(t1).unwrap(),
        rev_at_verifying.checked_next().unwrap(),
        "revision bumped exactly once"
    );
    // VerifiedComplete is terminal: content edits are frozen, no
    // transition is legal, a second completion refuses.
    let frozen = s
        .update_task(
            t1,
            TaskPatch {
                goal: Some("tamper".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(
        frozen,
        TaskError::TerminalTask {
            task_id: t1,
            state: TaskState::VerifiedComplete
        }
    );
    let rev_done = s.task_revision(t1).unwrap();
    let frozen2 = s
        .transition_task(t1, rev_done, TaskTransition::Cancel, None)
        .unwrap_err();
    assert!(matches!(frozen2, TaskError::IllegalTransition { .. }));
    let frozen3 = s.complete_verified_task(t1, rev_done, record).unwrap_err();
    assert_eq!(
        frozen3,
        TaskError::NotVerifying {
            actual: TaskState::VerifiedComplete
        }
    );
    assert_eq!(s.task_revision(t1).unwrap(), rev_done, "no double bump");

    let keep_verifying = |s: &SessionHandle, id: TaskId, rev: TaskRevision| {
        let row = s.get_task(id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Verifying);
        assert_eq!(s.task_revision(id).unwrap(), rev, "refusal must not bump");
    };

    // Missing record.
    let t2 = s
        .create_task(criteria_task(&s, TaskId::new(2), vec!["c1".into()]))
        .unwrap();
    let r2 = drive_to_verifying(&s, t2.task_id);
    let err = s
        .complete_verified_task(t2.task_id, r2, VerificationRecordId::new(42_000))
        .unwrap_err();
    assert_eq!(
        err,
        TaskError::RecordNotFound(VerificationRecordId::new(42_000))
    );
    keep_verifying(&s, t2.task_id, r2);
    // Wrong task: the record certifies task 1, not task 2.
    let err = s
        .complete_verified_task(t2.task_id, r2, record)
        .unwrap_err();
    assert_eq!(
        err,
        TaskError::RecordWrongTask {
            record,
            record_task: t1,
            requested_task: t2.task_id
        }
    );
    keep_verifying(&s, t2.task_id, r2);

    // Wrong revision: a record certifies the revision before a legal
    // content bump (budget while Verifying) and cannot complete the
    // moved task.
    let t3 = s
        .create_task(criteria_task(&s, TaskId::new(3), vec!["c1".into()]))
        .unwrap();
    let r3 = drive_to_verifying(&s, t3.task_id);
    let rec3 = passed_record(&s, t3.task_id, &["c1".into()]);
    s.update_task(
        t3.task_id,
        TaskPatch {
            budget: Some(TaskBudget {
                max_tokens: Some(1),
                max_turns: None,
                spent_tokens: 0,
                spent_turns: 0,
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let r3_after = s.task_revision(t3.task_id).unwrap();
    assert_ne!(r3_after, r3);
    let err = s
        .complete_verified_task(t3.task_id, r3_after, rec3)
        .unwrap_err();
    assert_eq!(
        err,
        TaskError::RecordWrongRevision {
            record: rec3,
            record_revision: r3,
            expected: r3_after
        }
    );
    keep_verifying(&s, t3.task_id, r3_after);
    // The STALE expected revision reports the task-side mismatch first.
    let err = s.complete_verified_task(t3.task_id, r3, rec3).unwrap_err();
    assert_eq!(
        err,
        TaskError::RevisionMismatch {
            task_id: t3.task_id,
            expected: r3,
            actual: r3_after
        }
    );
    keep_verifying(&s, t3.task_id, r3_after);

    // Record status Failed refuses with full coverage.
    let t4 = s
        .create_task(criteria_task(&s, TaskId::new(4), vec!["c1".into()]))
        .unwrap();
    let r4 = drive_to_verifying(&s, t4.task_id);
    let failed_rec = s
        .create_verification_record(
            t4.task_id,
            None,
            vec![CriterionVerification {
                criterion_key: "c1".into(),
                passed: true,
                evidence: None,
                binding: None,
            }],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Failed,
            1,
        )
        .unwrap();
    let err = s
        .complete_verified_task(t4.task_id, r4, failed_rec)
        .unwrap_err();
    assert_eq!(
        err,
        TaskError::RecordNotPassed {
            record: failed_rec,
            status: VerificationStatus::Failed
        }
    );
    keep_verifying(&s, t4.task_id, r4);

    // A record missing one current criterion refuses with the missing
    // list; full coverage completes; a passed=false entry is NOT
    // coverage.
    let t5 = s
        .create_task(criteria_task(
            &s,
            TaskId::new(5),
            vec!["c1".into(), "c2".into()],
        ))
        .unwrap();
    let r5 = drive_to_verifying(&s, t5.task_id);
    let partial = passed_record(&s, t5.task_id, &["c1".into()]);
    let err = s
        .complete_verified_task(t5.task_id, r5, partial)
        .unwrap_err();
    assert_eq!(
        err,
        TaskError::CriteriaNotCovered {
            record: partial,
            missing: vec!["c2".to_string()]
        }
    );
    keep_verifying(&s, t5.task_id, r5);
    let full = passed_record(&s, t5.task_id, &["c1".into(), "c2".into()]);
    let done5 = s.complete_verified_task(t5.task_id, r5, full).unwrap();
    assert_eq!(done5.state, TaskState::VerifiedComplete);
    let t6 = s
        .create_task(criteria_task(&s, TaskId::new(6), vec!["c1".into()]))
        .unwrap();
    let r6 = drive_to_verifying(&s, t6.task_id);
    let lying = s
        .create_verification_record(
            t6.task_id,
            None,
            vec![CriterionVerification {
                criterion_key: "c1".into(),
                passed: false,
                evidence: None,
                binding: None,
            }],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap();
    let err = s.complete_verified_task(t6.task_id, r6, lying).unwrap_err();
    assert_eq!(
        err,
        TaskError::CriteriaNotCovered {
            record: lying,
            missing: vec!["c1".to_string()]
        }
    );
    keep_verifying(&s, t6.task_id, r6);

    // (g) worktree mismatch: the record's base worktree must equal the
    // completing session's. Standalone sessions share the numeric task
    // id 1 but live in different workspaces: a record certified in /wb
    // cannot complete task 1 of a session in /wc.
    let wb = m.create_workspace("/wb").unwrap();
    let sb = m.create_session(wb, "b", "p", "m").unwrap();
    let tb = sb
        .create_task(criteria_task(&sb, sb.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let rb = drive_to_verifying(&sb, tb.task_id);
    let rec_b = passed_record(&sb, tb.task_id, &["c1".into()]);
    let wc = m.create_workspace("/wc").unwrap();
    let sc = m.create_session(wc, "c", "p", "m").unwrap();
    assert_eq!(
        sc.task_id().unwrap(),
        tb.task_id,
        "both standalone at task 1"
    );
    let tc = sc
        .create_task(criteria_task(&sc, sc.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let rc = drive_to_verifying(&sc, tc.task_id);
    let err = sc
        .complete_verified_task(tc.task_id, rc, rec_b)
        .unwrap_err();
    assert!(matches!(err, TaskError::WorktreeMismatch { .. }));
    assert_eq!(sc.task_revision(tc.task_id).unwrap(), rc);
    // And the same-workspace record completes it.
    let rec_c = passed_record(&sc, tc.task_id, &["c1".into()]);
    let done_c = sc.complete_verified_task(tc.task_id, rc, rec_c).unwrap();
    assert_eq!(done_c.state, TaskState::VerifiedComplete);
    let _ = (rb, wb, sb, tb);
}

#[test]
fn transition_task_is_the_only_way_into_completion_relevant_states() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let created = s.create_task(task(&s)).unwrap();
    let t = created.task_id;
    // Wait on a Pending task: illegal edge.
    let err = s
        .transition_task(t, TaskRevision::new(1), TaskTransition::Wait, None)
        .unwrap_err();
    assert!(matches!(err, TaskError::IllegalTransition { .. }));
    // A stale revision refuses even for a legal edge.
    let err = s
        .transition_task(t, TaskRevision::new(9), TaskTransition::StartRunning, None)
        .unwrap_err();
    assert_eq!(
        err,
        TaskError::RevisionMismatch {
            task_id: t,
            expected: TaskRevision::new(9),
            actual: TaskRevision::new(1)
        }
    );
    // Proofs do not ride transitions.
    let err = s
        .transition_task(
            t,
            TaskRevision::new(1),
            TaskTransition::StartRunning,
            Some(VerificationRecordId::new(7)),
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Malformed(_)));
    // The full chain into Verifying bumps revision every hop.
    let r1 = s.task_revision(t).unwrap();
    s.transition_task(t, r1, TaskTransition::StartRunning, None)
        .unwrap();
    let r2 = s.task_revision(t).unwrap();
    s.transition_task(t, r2, TaskTransition::Wait, None)
        .unwrap();
    let r3 = s.task_revision(t).unwrap();
    s.transition_task(t, r3, TaskTransition::ResumeFromWaiting, None)
        .unwrap();
    let r4 = s.task_revision(t).unwrap();
    s.transition_task(t, r4, TaskTransition::RequestVerification, None)
        .unwrap();
    let r5 = s.task_revision(t).unwrap();
    s.transition_task(t, r5, TaskTransition::StartVerification, None)
        .unwrap();
    let r6 = s.task_revision(t).unwrap();
    assert_eq!(r6, TaskRevision::new(6));
    assert_eq!(s.get_task(t).unwrap().unwrap().state, TaskState::Verifying);
    // Reverify and fail edges from Verifying.
    s.transition_task(t, r6, TaskTransition::Reverify, None)
        .unwrap();
    let r7 = s.task_revision(t).unwrap();
    assert_eq!(
        s.get_task(t).unwrap().unwrap().state,
        TaskState::NeedsVerification
    );
    s.transition_task(t, r7, TaskTransition::StartVerification, None)
        .unwrap();
    let r8 = s.task_revision(t).unwrap();
    s.transition_task(t, r8, TaskTransition::FailFromVerifying, None)
        .unwrap();
    assert_eq!(s.get_task(t).unwrap().unwrap().state, TaskState::Failed);
    // Failed is terminal: Cancel is illegal.
    let r9 = s.task_revision(t).unwrap();
    let err = s
        .transition_task(t, r9, TaskTransition::Cancel, None)
        .unwrap_err();
    assert!(matches!(err, TaskError::IllegalTransition { .. }));
    // Cancellation from a non-terminal state is legal.
    let t2 = s
        .create_task(criteria_task(&s, TaskId::new(7), vec![]))
        .unwrap();
    let c = s
        .transition_task(
            t2.task_id,
            TaskRevision::new(1),
            TaskTransition::Cancel,
            None,
        )
        .unwrap();
    assert_eq!(c.state, TaskState::Cancelled);
}

// (e) racing completions and finalizes: exactly one winner each.
#[test]
fn concurrent_completions_and_finalizes_win_exactly_once() {
    let (_d, m) = test_manager();
    let s = Arc::new(session(&m));
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let tid = t.task_id;
    let rev = drive_to_verifying(&s, tid);
    let rec = passed_record(&s, tid, &["c1".into()]);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let s = s.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            s.complete_verified_task(tid, rev, rec)
        }));
    }
    barrier.wait();
    let results: Vec<Result<Task, TaskError>> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "exactly one completion winner: {results:?}");
    for r in &results {
        if let Err(e) = r {
            assert!(
                matches!(
                    e,
                    TaskError::NotVerifying { .. } | TaskError::RevisionMismatch { .. }
                ),
                "loser must fail typed, got {e:?}"
            );
        }
    }
    assert_eq!(s.task_revision(tid).unwrap(), rev.checked_next().unwrap());
    assert_eq!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );

    // Record-finalize CAS: a Running record finalizes exactly once.
    let t2 = s
        .create_task(criteria_task(&s, TaskId::new(2), vec![]))
        .unwrap();
    let running = s
        .create_verification_record(
            t2.task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            1,
        )
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let s = s.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            s.finalize_verification_record(running, VerificationStatus::Passed, 99)
        }));
    }
    barrier.wait();
    let results: Vec<Result<(), TaskError>> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "exactly one finalize winner: {results:?}");
    for r in &results {
        if let Err(e) = r {
            assert!(matches!(
                e,
                TaskError::RecordNotFinalizable {
                    current: VerificationStatus::Passed,
                    ..
                }
            ));
        }
    }
    assert_eq!(
        s.get_verification_record(running).unwrap().unwrap().status,
        VerificationStatus::Passed
    );
}

// (f)+(i) crash between record creation and completion: reopen shows a
// consistent store; the completion applies exactly once afterwards, and
// records survive.
#[test]
fn records_and_state_survive_reopen_and_completion_after_crash() {
    let (dir, m) = test_manager();
    let s = session(&m);
    let sid = s.id;
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let tid = t.task_id;
    let rev = drive_to_verifying(&s, tid);
    let rec = passed_record(&s, tid, &["c1".into()]);
    let listed = s.list_verification_records(tid).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].record_id, rec);
    assert_eq!(listed[0].revision, rev);
    // Crash (drop the manager) before the completion transaction.
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    let row = s2.get_task(tid).unwrap().unwrap();
    assert_eq!(row.state, TaskState::Verifying, "crashed mid-verification");
    assert_eq!(s2.task_revision(tid).unwrap(), rev);
    assert_eq!(
        s2.get_verification_record(rec).unwrap().unwrap().revision,
        rev,
        "the record survived the crash"
    );
    // Reopen again after a second crash between record and completion…
    drop(s2);
    drop(m2);
    let m3 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s3 = m3.get_session(sid).unwrap().unwrap();
    let done = s3.complete_verified_task(tid, rev, rec).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
    assert_eq!(s3.task_revision(tid).unwrap(), rev.checked_next().unwrap());
    // Repeat completion after ANOTHER reopen refuses and does not bump.
    drop(s3);
    drop(m3);
    let m4 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s4 = m4.get_session(sid).unwrap().unwrap();
    let err = s4
        .complete_verified_task(tid, rev.checked_next().unwrap(), rec)
        .unwrap_err();
    assert_eq!(
        err,
        TaskError::NotVerifying {
            actual: TaskState::VerifiedComplete
        }
    );
    assert_eq!(s4.task_revision(tid).unwrap(), rev.checked_next().unwrap());
    let frozen = s4
        .update_task(
            tid,
            TaskPatch {
                goal: Some("tamper".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(
        frozen,
        TaskError::TerminalTask {
            task_id: tid,
            state: TaskState::VerifiedComplete
        }
    );
    let records = s4.list_verification_records(tid).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_id, rec);
}

// (g) oversized record JSON is rejected with a typed error and NO row
// is written (never truncated).
#[test]
fn oversized_record_json_is_rejected_typed_and_never_truncated() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let created = s.create_task(task(&s)).unwrap();
    let tid = created.task_id;
    // criteria_json over 128 KiB: 64 keys near the per-key cap.
    let huge_criteria: Vec<CriterionVerification> = (0..MAX_VERIFICATION_RECORD_CRITERIA)
        .map(|i| CriterionVerification {
            criterion_key: format!("{i:03}").repeat(MAX_VERIFICATION_CRITERION_KEY_BYTES / 4),
            passed: true,
            evidence: None,
            binding: None,
        })
        .collect();
    assert!(
        serde_json::to_vec(&huge_criteria).unwrap().len() > MAX_VERIFICATION_CRITERIA_JSON_BYTES
    );
    let err = s
        .create_verification_record(
            tid,
            None,
            huge_criteria,
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Oversized(_)));
    // checks_json over 256 KiB.
    let huge_checks: Vec<CheckExecution> = (0..MAX_VERIFICATION_RECORD_CHECKS)
        .map(|i| CheckExecution {
            check: format!("check {i}").repeat(MAX_VERIFICATION_CHECK_NAME_BYTES / 10),
            program: "cargo".into(),
            args: vec![],
            category: "required".into(),
            required: true,
            status: VerificationStatus::Passed,
            started_ms: 1,
            finished_ms: Some(2),
            exit: Some(0),
            summary: Some("s".repeat(MAX_VERIFICATION_SUMMARY_BYTES)),
        })
        .collect();
    assert!(serde_json::to_vec(&huge_checks).unwrap().len() > MAX_VERIFICATION_CHECKS_JSON_BYTES);
    let err = s
        .create_verification_record(
            tid,
            None,
            vec![],
            huge_checks,
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Oversized(_)));
    // A hostile non-hex digest is malformed (typed), a giant evidence is
    // oversized.
    let err = s
        .create_verification_record(
            tid,
            None,
            vec![CriterionVerification {
                criterion_key: "c1".into(),
                passed: true,
                evidence: None,
                binding: None,
            }],
            vec![],
            vec![FileStateEvidence {
                path: "src/main.rs".into(),
                digest_hex: "not-hex!!".into(),
                size: 1,
            }],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Malformed(_)));
    let err = s
        .create_verification_record(
            tid,
            None,
            vec![CriterionVerification {
                criterion_key: "c1".into(),
                passed: true,
                evidence: Some("e".repeat(MAX_VERIFICATION_EVIDENCE_BYTES + 1)),
                binding: None,
            }],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Oversized(_)));
    assert!(
        s.list_verification_records(tid).unwrap().is_empty(),
        "refused records left no trace"
    );
}

// (h) revisions are strictly monotone under 20 mixed updates — no
// reuse, no backwards gap — also across a reopen.
#[test]
fn revisions_stay_monotone_under_mixed_updates_and_reopen() {
    let (dir, m) = test_manager();
    let s = session(&m);
    let sid = s.id;
    let created = s.create_task(task(&s)).unwrap();
    let tid = created.task_id;
    // Start the machine so the Wait/Resume round trips are legal.
    let r0 = s.task_revision(tid).unwrap();
    s.transition_task(tid, r0, TaskTransition::StartRunning, None)
        .unwrap();
    let mut last = s.task_revision(tid).unwrap();
    let mut seen = vec![last];
    for i in 0..20u64 {
        if i % 3 == 0 {
            s.update_task(
                tid,
                TaskPatch {
                    goal: Some(format!("goal iteration {i}")),
                    ..Default::default()
                },
            )
            .unwrap();
        } else if i % 3 == 1 {
            s.update_task(
                tid,
                TaskPatch {
                    plan: Some(vec![format!("step {i}")]),
                    budget: Some(TaskBudget {
                        max_tokens: Some(i),
                        max_turns: Some(i as u32),
                        spent_tokens: 0,
                        spent_turns: 0,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        } else {
            // Ordinary state machine round trip Running -> Waiting ->
            // Running.
            let r = s.task_revision(tid).unwrap();
            s.transition_task(tid, r, TaskTransition::Wait, None)
                .unwrap();
            let r = s.task_revision(tid).unwrap();
            s.transition_task(tid, r, TaskTransition::ResumeFromWaiting, None)
                .unwrap();
        }
        let now = s.task_revision(tid).unwrap();
        assert!(
            now > last,
            "revision must strictly increase: {now:?} after {last:?}"
        );
        assert!(
            !seen.contains(&now),
            "revision {now:?} reused after {seen:?}"
        );
        seen.push(now);
        last = now;
    }
    assert!(last.raw() >= 2 + 20, "each iteration bumped at least once");
    // No-op patches do not move the revision.
    let before = last;
    s.update_task(tid, TaskPatch::default()).unwrap();
    assert_eq!(s.task_revision(tid).unwrap(), before);
    // Reopen: the sequence continues, never resetting or reusing.
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    let after_reopen = s2.task_revision(tid).unwrap();
    assert_eq!(after_reopen, before, "revision survives reopen untouched");
    s2.update_task(
        tid,
        TaskPatch {
            goal: Some("after reopen".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        s2.task_revision(tid).unwrap(),
        after_reopen.checked_next().unwrap(),
        "no reset, no reuse after reopen"
    );
}

// (j) record listing is deterministic (creation order) across calls.
#[test]
fn record_listing_is_deterministic() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let created = s.create_task(task(&s)).unwrap();
    let tid = created.task_id;
    let r1 = passed_record(&s, tid, &[]);
    let r2 = passed_record(&s, tid, &[]);
    let r3 = passed_record(&s, tid, &[]);
    let first = s.list_verification_records(tid).unwrap();
    let ids: Vec<VerificationRecordId> = first.iter().map(|r| r.record_id).collect();
    assert_eq!(ids, vec![r1, r2, r3], "creation order");
    assert_eq!(s.list_verification_records(tid).unwrap(), first, "stable");
    // Records of another task do not leak into this task's list.
    let t2 = s
        .create_task(criteria_task(&s, TaskId::new(2), vec![]))
        .unwrap();
    assert!(s.list_verification_records(t2.task_id).unwrap().is_empty());
}

// ---------------------------------------------------------------- accounting-before-completion
// (attempt-accounting completion invariant): VerifiedComplete requires
// zero OPEN + UNCERTAIN reservations, exact usage reconciled first, the
// rest charged conservatively at reserved estimates, and the task row
// only then transitioning. Fault tests crash at every seam and reopen.

fn ledger_for(m: &Arc<crate::SessionManager>) -> Arc<crate::budget::DurableBudgetLedger> {
    crate::budget::DurableBudgetLedger::new(m.clone())
}

fn snapshot() -> faktor_core::model::PricingSnapshot {
    use faktor_core::model::{MicroUsdPerMillionTokens, PriceQuote};
    faktor_core::model::PricingSnapshot::exact(
        PriceQuote {
            input: MicroUsdPerMillionTokens(10_000_000),
            output: MicroUsdPerMillionTokens(20_000_000),
            cache_read: MicroUsdPerMillionTokens(2_000_000),
            cache_write: MicroUsdPerMillionTokens(4_000_000),
        },
        7,
        "task-accounting-test".into(),
    )
}

/// A RUNNING task under a (possibly hard) cost cap. Reservations may
/// only be admitted while the task still permits new provider work, so
/// accounting tests commit their reservations at Running and only then
/// call [`finish_verifying`].
fn running_task_with_cap(s: &SessionHandle, cap: Option<u64>) -> TaskId {
    let t = s
        .create_task(criteria_task(s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let tid = t.task_id;
    ledger_for(&s.manager)
        .set_task_max_cost(s.id, tid, cap)
        .unwrap();
    let rev = s.task_revision(tid).unwrap();
    s.transition_task(tid, rev, TaskTransition::StartRunning, None)
        .unwrap();
    tid
}

/// Walk a Running task into Verifying and mint its passing record at
/// the Verifying revision.
fn finish_verifying(s: &SessionHandle, tid: TaskId) -> (TaskRevision, VerificationRecordId) {
    let rev = s.task_revision(tid).unwrap();
    s.transition_task(tid, rev, TaskTransition::RequestVerification, None)
        .unwrap();
    let rev = s.task_revision(tid).unwrap();
    s.transition_task(tid, rev, TaskTransition::StartVerification, None)
        .unwrap();
    let rev = s.task_revision(tid).unwrap();
    let rec = passed_record(s, tid, &["c1".into()]);
    (rev, rec)
}

#[tokio::test]
async fn open_reserved_reservation_refuses_completion_and_task_stays_verifying() {
    let (dir, m) = test_manager();
    let s = session(&m);
    let tid = running_task_with_cap(&s, None);
    // A reservation that was never dispatched (a lost pre-dispatch row):
    // still RESERVED, refundable — but the completion gate must refuse
    // while it holds budget.
    let ledger = ledger_for(&m);
    let r = ledger
        .reserve(s.id, tid, m.try_next_op_id().unwrap(), 5_000, None)
        .await
        .unwrap();
    let (rev, rec) = finish_verifying(&s, tid);
    let err = s.complete_verified_task(tid, rev, rec).unwrap_err();
    assert!(
        matches!(
            err,
            TaskError::AccountingIncomplete {
                open_count: 1,
                open_micro: 5_000,
                dispatched_count: 0,
                uncertain_count: 0,
                uncertain_micro: 0,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::Verifying,
        "the refusal never transitions the row"
    );
    // Refunding the lost reservation lets the SAME completion pass land
    // (nothing was charged, nothing was written by the refusal).
    let sid = s.id;
    drop(s);
    let s2 = m.get_session(sid).unwrap().unwrap();
    ledger.refund(sid, r).await.unwrap();
    let done = s2.complete_verified_task(tid, rev, rec).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
    let _ = dir;
}

#[tokio::test]
async fn dispatched_open_row_refuses_completion_until_uncertain_or_settled() {
    let (_dir, m) = test_manager();
    let s = session(&m);
    let tid = running_task_with_cap(&s, Some(1_000_000));
    let ledger = ledger_for(&m);
    // A DISPATCHED row (durable marker written, provider may have
    // billed): never refundable; the completion gate must refuse while
    // it sits open — the conservative close is mark_uncertain (or a
    // settle), never a refund.
    let r = ledger
        .reserve(s.id, tid, m.try_next_op_id().unwrap(), 8_000, None)
        .await
        .unwrap();
    ledger.mark_dispatched(s.id, r).await.unwrap();
    let (rev, rec) = finish_verifying(&s, tid);
    let err = s.complete_verified_task(tid, rev, rec).unwrap_err();
    assert!(
        matches!(
            err,
            TaskError::AccountingIncomplete {
                open_count: 1,
                open_micro: 8_000,
                dispatched_count: 1,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::Verifying
    );
    // A refund is SQL-refused on the dispatched row.
    assert!(matches!(
        ledger.refund(s.id, r).await,
        Err(crate::budget::BudgetError::CannotRefundDispatched { .. })
    ));
    // Closing it as UNCERTAIN lets completion charge the estimate and
    // land.
    ledger
        .mark_uncertain(s.id, r, "test_dispatch_left_open".into(), None)
        .await
        .unwrap();
    let done = s.complete_verified_task(tid, rev, rec).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
    let balance = ledger.completion_accounting_balance(s.id, tid).unwrap();
    assert!(balance.is_zero());
    assert_eq!(balance.spent_cost_micro, 8_000, "charged the estimate");
}

#[tokio::test]
async fn uncertain_attempt_is_charged_conservatively_at_its_reserved_estimate() {
    let (_dir, m) = test_manager();
    let s = session(&m);
    let tid = running_task_with_cap(&s, Some(1_000_000));
    let ledger = ledger_for(&m);
    // A crashed dispatched attempt with no completed provider row: exact
    // usage unknown — finalize charges the reserved estimate.
    let r = ledger
        .reserve(
            s.id,
            tid,
            m.try_next_op_id().unwrap(),
            12_000,
            Some(snapshot()),
        )
        .await
        .unwrap();
    ledger.mark_dispatched(s.id, r).await.unwrap();
    ledger
        .mark_uncertain(s.id, r, "crash".into(), None)
        .await
        .unwrap();
    let (rev, rec) = finish_verifying(&s, tid);
    let done = s.complete_verified_task(tid, rev, rec).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
    let balance = ledger.completion_accounting_balance(s.id, tid).unwrap();
    assert!(
        balance.is_zero(),
        "VerifiedComplete => zero open + uncertain"
    );
    assert_eq!(balance.spent_cost_micro, 12_000);
}

#[tokio::test]
async fn uncertain_attempt_with_known_usage_reconciles_exactly_before_charging() {
    let (_dir, m) = test_manager();
    let s = session(&m);
    let tid = running_task_with_cap(&s, Some(1_000_000));
    let ledger = ledger_for(&m);
    // The crashed attempt DID complete at the provider (a completed
    // attempt-keyed provider_call row exists): reconcile settles FROM
    // that exact usage — 900 input + 100 output tokens x the frozen
    // snapshot (10 micro / 1M tokens x ... pricing snapshot fields are
    // microUSD per MILLION tokens here) — never the 60_000 estimate.
    let logical = m.try_next_op_id().unwrap();
    let attempt =
        faktor_core::op::ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 0).unwrap();
    let r = ledger
        .reserve_attempt(s.id, tid, attempt, 60_000, Some(snapshot()))
        .await
        .unwrap();
    ledger.mark_dispatched(s.id, r).await.unwrap();
    ledger
        .mark_uncertain(s.id, r, "crash_after_completion".into(), None)
        .await
        .unwrap();
    // The completed durable row of THIS attempt (op id = the logical op,
    // attempt keyed; the exact usage is durable truth).
    s.record_provider_call_attempt(
        attempt,
        Some(r),
        "fake",
        "m",
        "completed",
        Some(900),
        Some(100),
        None,
    )
    .unwrap();
    let (rev, rec) = finish_verifying(&s, tid);
    let done = s.complete_verified_task(tid, rev, rec).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
    let balance = ledger.completion_accounting_balance(s.id, tid).unwrap();
    assert!(balance.is_zero());
    // 900 input x 10 + 100 output x 20 = 9_000 + 2_000 = 11_000 micro.
    assert_eq!(
        balance.spent_cost_micro, 11_000,
        "reconcile settles the EXACT usage, not the 60k estimate"
    );
}

/// Crash at EVERY seam of the completion sequence, reopen, and assert:
/// the task STAYS Verifying, the accounting prefix is durable and
/// idempotent, and the re-run converges to VerifiedComplete with the
/// final invariant (zero open + uncertain, totals folded, revision proof
/// exact).
#[tokio::test]
async fn crash_at_every_completion_seam_reopens_verifying_and_reruns_converge() {
    for seam in [
        CompletionCrashPoint::AfterProofLoad,
        CompletionCrashPoint::AfterReconcile,
        CompletionCrashPoint::AfterCostFold,
        CompletionCrashPoint::BeforeTransition,
    ] {
        let (dir, m) = test_manager();
        let s = session(&m);
        let sid = s.id;
        let tid = running_task_with_cap(&s, Some(1_000_000));
        let ledger = ledger_for(&m);
        // Two crashed dispatched attempts: one with exact usage known,
        // one without.
        let logical = m.try_next_op_id().unwrap();
        let exact = faktor_core::op::ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 0)
            .unwrap();
        let r1 = ledger
            .reserve_attempt(s.id, tid, exact, 60_000, Some(snapshot()))
            .await
            .unwrap();
        ledger.mark_dispatched(s.id, r1).await.unwrap();
        s.record_provider_call_attempt(
            exact,
            Some(r1),
            "fake",
            "m",
            "completed",
            Some(900),
            Some(100),
            None,
        )
        .unwrap();
        ledger
            .mark_uncertain(s.id, r1, "crash_a".into(), None)
            .await
            .unwrap();
        let r2 = ledger
            .reserve(s.id, tid, m.try_next_op_id().unwrap(), 7_000, None)
            .await
            .unwrap();
        ledger.mark_dispatched(s.id, r2).await.unwrap();
        ledger
            .mark_uncertain(s.id, r2, "crash_b".into(), None)
            .await
            .unwrap();
        // The accounting prefix is durable; now walk the task into
        // Verifying and take the passing record at that revision.
        let (rev, rec) = finish_verifying(&s, tid);
        // "Crash" at the seam: every step before it committed.
        let crashed = s
            .complete_verified_task_crashable(tid, rev, rec, Some(seam))
            .unwrap();
        assert_eq!(crashed, None, "the seam simulates process death");
        drop(s);
        drop(m);
        // Reopen: the task STAYS Verifying at the exact proof revision.
        let m2 =
            crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                .unwrap();
        let s2 = m2.get_session(sid).unwrap().unwrap();
        let row = s2.get_task(tid).unwrap().unwrap();
        assert_eq!(
            row.state,
            TaskState::Verifying,
            "no seam may transition before accounting is provably closed: {seam:?}"
        );
        assert_eq!(s2.task_revision(tid).unwrap(), rev);
        assert_eq!(
            s2.get_verification_record(rec).unwrap().unwrap().status,
            VerificationStatus::Passed
        );
        // The re-run converges: reconcile idempotent, finalize charges
        // only what reconcile left, the balance asserts zero, and the
        // transition CAS lands exactly once.
        let done = s2.complete_verified_task(tid, rev, rec).unwrap();
        assert_eq!(done.state, TaskState::VerifiedComplete, "{seam:?}");
        assert_eq!(
            s2.task_revision(tid).unwrap(),
            rev.checked_next().unwrap(),
            "the transition bumped exactly once: {seam:?}"
        );
        let balance = ledger_for(&m2)
            .completion_accounting_balance(sid, tid)
            .unwrap();
        assert!(
            balance.is_zero(),
            "VerifiedComplete => zero open + uncertain ({seam:?}): {balance:?}"
        );
        // Exact-usage attempt reconciled at 11_000; the usage-less
        // crashed attempt charged its 7_000 estimate.
        assert_eq!(balance.spent_cost_micro, 11_000 + 7_000, "{seam:?}");
        let _ = dir;
    }
}

// ------------------------------------------- typed criteria (56/57/105)

#[test]
fn typed_criteria_round_trip_and_revision_bump_on_change() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["goal: ship".into()]))
        .unwrap();
    let rev = s.task_revision(tid).unwrap();
    let criteria = vec![
        Criterion::user("goal: ship"),
        Criterion::derived(
            "required check: cargo check",
            CriterionOrigin::VerificationPolicy,
            CriterionRequirement::Required,
            Some("snap-1".into()),
        )
        .with_evidence(7),
        Criterion::derived(
            "no public API churn",
            CriterionOrigin::SemanticProvider,
            CriterionRequirement::Preferred,
            Some("provider-snap-9".into()),
        ),
    ];
    let updated = s.set_task_criteria(tid, criteria.clone()).unwrap();
    // The typed criteria ride the EXISTING row values as V2 JSON.
    assert!(
        updated
            .acceptance_criteria
            .iter()
            .all(|e| e.starts_with("v2:")),
        "{:?}",
        updated.acceptance_criteria
    );
    assert_eq!(s.task_criteria(tid).unwrap(), criteria, "round trip exact");
    assert_eq!(
        s.task_revision(tid).unwrap(),
        rev.checked_next().unwrap(),
        "a criterion change bumps the revision exactly once"
    );
    // Idempotent re-set: byte-identical, no bump.
    let again = s.set_task_criteria(tid, criteria.clone()).unwrap();
    assert_eq!(again, updated);
    assert_eq!(
        s.task_revision(tid).unwrap(),
        rev.checked_next().unwrap(),
        "an identical criteria set writes nothing"
    );
    // A metadata-only change (evidence binding) is content: it bumps.
    let mut with_evidence = criteria.clone();
    with_evidence[0].evidence_source = Some(42);
    let bumped = s.set_task_criteria(tid, with_evidence.clone()).unwrap();
    assert_eq!(bumped.acceptance_criteria, encode_criteria(&with_evidence));
    assert_eq!(
        s.task_revision(tid).unwrap(),
        rev.checked_next().unwrap().checked_next().unwrap()
    );
    // Hostile hand-crafted ids and duplicate sets are refused loudly
    // before any write.
    let mut hostile = criteria.clone();
    hostile[0].id = CriterionId::try_from(7).unwrap();
    assert!(matches!(
        s.set_task_criteria(tid, hostile).unwrap_err(),
        TaskError::Malformed(_)
    ));
    let rev_before = s.task_revision(tid).unwrap();
    let mut dup = criteria.clone();
    dup.push(criteria[0].clone());
    assert!(matches!(
        s.set_task_criteria(tid, dup).unwrap_err(),
        TaskError::Malformed(_)
    ));
    let too_many: Vec<Criterion> = (0..=MAX_TASK_CRITERIA)
        .map(|i| Criterion::user(format!("criterion {i}")))
        .collect();
    assert!(matches!(
        s.set_task_criteria(tid, too_many).unwrap_err(),
        TaskError::Oversized(_)
    ));
    assert_eq!(s.task_revision(tid).unwrap(), rev_before, "no trace");
    // Escape-heavy text inside the text bound encodes beyond the entry
    // bound: refused loudly, never silently demoted to plain text.
    let mut escape_heavy = Criterion::user("goal: ok");
    escape_heavy.text = "\\".repeat(MAX_TASK_CRITERION_TEXT_BYTES);
    escape_heavy.id = CriterionId::for_content(
        escape_heavy.origin,
        escape_heavy.requirement,
        &escape_heavy.text,
        None,
    );
    assert!(matches!(
        s.set_task_criteria(tid, vec![escape_heavy]).unwrap_err(),
        TaskError::Oversized(_)
    ));
    assert_eq!(s.task_revision(tid).unwrap(), rev_before, "no trace");
}

#[test]
fn legacy_string_migration_is_stable_and_coverage_still_validates() {
    // (a) An untouched legacy row keeps completing: the record keys are
    // the raw legacy strings the row carries.
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    let legacy = vec![
        "goal: gated goal".to_string(),
        "required check: cargo check".to_string(),
    ];
    s.create_task(criteria_task(&s, tid, legacy.clone()))
        .unwrap();
    let first = s.task_criteria(tid).unwrap();
    let second = s.task_criteria(tid).unwrap();
    assert_eq!(first, second, "repeated legacy reads are identical");
    assert_eq!(first[0].origin, CriterionOrigin::User);
    assert_eq!(first[1].origin, CriterionOrigin::ProjectPolicy);
    for criterion in &first {
        criterion.validate().unwrap();
    }
    let rev = drive_to_verifying(&s, tid);
    let record = passed_record(&s, tid, &legacy);
    let done = s.complete_verified_task(tid, rev, record).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);

    // (a2) A legacy entry that cannot fit the V2 envelope stays plain
    // (lossless) instead of being truncated or demoted with data loss.
    let s_big = session(&m);
    let tid_big = s_big.task_id().unwrap();
    let long = "x".repeat(MAX_TASK_CRITERION_BYTES);
    s_big
        .create_task(criteria_task(&s_big, tid_big, vec![long.clone()]))
        .unwrap();
    let decoded = s_big.task_criteria(tid_big).unwrap();
    assert_eq!(decoded[0].text, long, "the over-bound text is preserved");
    assert_eq!(
        encode_criteria(&decoded),
        vec![long],
        "an over-bound legacy criterion keeps its plain representation"
    );

    // (b) The migrated (V2) row completes against a record keyed by the
    // migrated row values — coverage never silently drifts.
    let s2 = session(&m);
    let tid2 = s2.task_id().unwrap();
    s2.create_task(criteria_task(&s2, tid2, legacy.clone()))
        .unwrap();
    let migrated = s2.task_criteria(tid2).unwrap();
    let migrated_row = s2.set_task_criteria(tid2, migrated.clone()).unwrap();
    let rev2 = drive_to_verifying(&s2, tid2);
    // The migrated row is MODERN (V2): the record must carry the
    // criterion's own binding to certify (FIX 1).
    let record2 = passed_record_bound(&s2, tid2, &migrated_row.acceptance_criteria);
    let done2 = s2.complete_verified_task(tid2, rev2, record2).unwrap();
    assert_eq!(done2.state, TaskState::VerifiedComplete);
}

#[test]
fn criterion_id_constructors_are_fallible_and_namespaces_never_alias() {
    // (a) Public constructors are fallible values, never panics.
    assert_eq!(
        CriterionIdError::Zero,
        CriterionId::try_from(0).unwrap_err()
    );
    let seven = std::num::NonZeroU64::new(7).unwrap();
    let legacy = CriterionId::try_from(seven).unwrap();
    assert!(legacy.is_legacy());
    assert_eq!(legacy.legacy_raw(), Some(7));
    assert_eq!(legacy.to_string(), "7", "legacy display is unchanged");
    assert_eq!(serde_json::to_string(&legacy).unwrap(), "7");
    assert_eq!(legacy.parts(), (0, 7));

    // (b) Derived ids are deterministic 128-bit values outside the
    // legacy (high == 0) namespace, and every identity field moves them.
    let alpha = CriterionId::for_content(
        CriterionOrigin::User,
        CriterionRequirement::Required,
        "alpha",
        None,
    );
    assert_eq!(
        alpha,
        CriterionId::for_content(
            CriterionOrigin::User,
            CriterionRequirement::Required,
            "alpha",
            None
        ),
        "content derivation is deterministic"
    );
    assert!(!alpha.is_legacy(), "derived ids reserve high == 0 away");
    assert_eq!(alpha.to_string().len(), 32);
    assert_eq!(alpha.to_hex().len(), 32);
    for other in [
        CriterionId::for_content(
            CriterionOrigin::User,
            CriterionRequirement::Required,
            "beta",
            None,
        ),
        CriterionId::for_content(
            CriterionOrigin::User,
            CriterionRequirement::Preferred,
            "alpha",
            None,
        ),
        CriterionId::for_content(
            CriterionOrigin::User,
            CriterionRequirement::Required,
            "alpha",
            Some("snap-1"),
        ),
        CriterionId::for_content(
            CriterionOrigin::ProjectPolicy,
            CriterionRequirement::Required,
            "alpha",
            None,
        ),
    ] {
        assert_ne!(alpha, other, "every identity field feeds the id");
    }

    // (c) Serde round trips exactly; malformed/foreign encodings are
    // typed errors, and a hex form can never encode the legacy
    // namespace (one id, one canonical encoding).
    let hex = serde_json::to_string(&alpha).unwrap();
    assert_eq!(hex.len(), 34, "32 hex chars plus quotes");
    assert_eq!(serde_json::from_str::<CriterionId>(&hex).unwrap(), alpha);
    assert!(serde_json::from_str::<CriterionId>("0").is_err());
    assert!(serde_json::from_str::<CriterionId>("-1").is_err());
    assert!(serde_json::from_str::<CriterionId>("\"00000000000000000000000000000001\"").is_err());
    assert!(serde_json::from_str::<CriterionId>("\"ABCDEFABCDEFABCDEFABCDEFABCDEF12\"").is_err());
    assert!(CriterionId::from_hex("abc").is_err());
    assert!(CriterionId::from_hex(&"0".repeat(32)).is_err());

    // (d) A legacy V2 row (id written by a pre-v22 build) still
    // validates, and a hostile crafted id never does.
    let legacy_id = CriterionId::legacy_for_content(
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        "legacy text",
        None,
    );
    let legacy_criterion = Criterion {
        id: legacy_id,
        text: "legacy text".into(),
        origin: CriterionOrigin::ProjectPolicy,
        requirement: CriterionRequirement::Required,
        evidence_source: None,
        semantic_snapshot: None,
        binding: None,
    };
    legacy_criterion.validate().unwrap();
    let mut hostile = legacy_criterion;
    hostile.id = CriterionId::try_from(9).unwrap();
    assert!(matches!(hostile.validate(), Err(TaskError::Malformed(_))));
}

#[test]
fn user_criteria_survive_rederivation_and_stale_snapshot_rederives() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec![])).unwrap();
    let v1 = vec![
        Criterion::user("goal: first"),
        Criterion::derived(
            "required check: cargo check",
            CriterionOrigin::ProjectPolicy,
            CriterionRequirement::Required,
            Some("checks:v1".into()),
        ),
    ];
    s.rederive_task_criteria(tid, v1.clone()).unwrap();
    let rev1 = s.task_revision(tid).unwrap();
    // The source snapshot moved: the derived criterion is re-derived
    // (new content id, new snapshot), a new user goal joins, the ORIGINAL
    // user criterion survives verbatim.
    let v2 = vec![
        Criterion::user("goal: second"),
        Criterion::derived(
            "required check: cargo check",
            CriterionOrigin::ProjectPolicy,
            CriterionRequirement::Required,
            Some("checks:v2".into()),
        ),
        Criterion::derived(
            "required check: cargo test",
            CriterionOrigin::ProjectPolicy,
            CriterionRequirement::Required,
            Some("checks:v2".into()),
        ),
    ];
    let row = s.rederive_task_criteria(tid, v2.clone()).unwrap();
    assert!(s.task_revision(tid).unwrap() != rev1);
    let criteria = s.task_criteria(tid).unwrap();
    assert_eq!(criteria.len(), 4, "{criteria:?}");
    let sticky = criteria
        .iter()
        .find(|c| c.text == "goal: first")
        .expect("the user criterion survives re-derivation");
    assert_eq!(sticky.id, v1[0].id);
    assert!(criteria.iter().any(|c| c.text == "goal: second"));
    let check = criteria
        .iter()
        .find(|c| c.text == "required check: cargo check")
        .unwrap();
    assert_eq!(check.semantic_snapshot.as_deref(), Some("checks:v2"));
    assert_ne!(check.id, v1[1].id, "the stale snapshot re-derived");
    assert!(criteria
        .iter()
        .any(|c| c.text == "required check: cargo test"));
    // Re-running the SAME derivation is idempotent.
    let rev2 = s.task_revision(tid).unwrap();
    let again = s.rederive_task_criteria(tid, v2).unwrap();
    assert_eq!(again.acceptance_criteria, row.acceptance_criteria);
    assert_eq!(s.task_revision(tid).unwrap(), rev2);
}

#[test]
fn criterion_change_invalidates_prior_verification() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["goal: gated goal".into()]))
        .unwrap();
    let certified_rev = drive_to_verifying(&s, tid);
    let legacy_keys = s.get_task(tid).unwrap().unwrap().acceptance_criteria;
    let stale_record = passed_record(&s, tid, &legacy_keys);
    // A criterion change (here: adding a user criterion) bumps the row.
    s.set_task_criteria(
        tid,
        vec![
            Criterion::user("goal: gated goal"),
            Criterion::user("the new seam must be named"),
        ],
    )
    .unwrap();
    let moved = s.task_revision(tid).unwrap();
    assert!(moved != certified_rev);
    // The prior PASSING record pins the old revision: completion is
    // refused typed, never silently certified.
    let err = s
        .complete_verified_task(tid, certified_rev, stale_record)
        .unwrap_err();
    assert!(
        matches!(err, TaskError::RevisionMismatch { .. }),
        "criterion change must invalidate the prior verification: {err:?}"
    );
    assert_eq!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::Verifying
    );
    // A fresh record certifying the CURRENT row completes; the row is
    // MODERN (V2 after the set above), so the record binds each
    // criterion through its own effective binding (FIX 1).
    let current_keys = s.get_task(tid).unwrap().unwrap().acceptance_criteria;
    let fresh = passed_record_bound(&s, tid, &current_keys);
    let done = s.complete_verified_task(tid, moved, fresh).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

// ------------------------------------- v20 evidence (audits 94/116/117)

fn fingerprint_fixture() -> EnvironmentFingerprint {
    EnvironmentFingerprint {
        platform: "macos".into(),
        arch: "aarch64".into(),
        toolchain_versions: vec![faktor_core::state::ToolVersion {
            tool: "faktor-agent".into(),
            version: "0.1.0".into(),
        }],
        manifest_hashes: vec![faktor_core::state::FingerprintFileHash {
            path: "Cargo.toml".into(),
            digest_hex: "ab".repeat(32),
        }],
        lockfile_hashes: vec![faktor_core::state::FingerprintFileHash {
            path: "Cargo.lock".into(),
            digest_hex: "cd".repeat(32),
        }],
        instruction_epoch: Some(9),
        base_tree_hash: None,
        task_contract_hash: "ef".repeat(32),
        check_argv_cwd_env_hash: "12".repeat(32),
        verification_impl_version: "faktor-agent/0.1.0".into(),
        proof_basis_digest: None,
    }
}

fn candidate_fixture(rev: TaskRevision) -> CandidateProofRef {
    CandidateProofRef {
        task_revision: rev,
        base_manifest_hash: "34".repeat(32),
        candidate_manifest_hash: "56".repeat(32),
        source_diff_evidence: Some(7),
        risk_report_evidence: None,
        accounting_snapshot_digest: "accounting:v1:0000000000000001".into(),
        run_id: None,
        run_base_snapshot: None,
        candidate_snapshot: None,
        sources_digest: None,
        changed_files_digest: None,
    }
}

#[test]
fn fingerprint_and_candidate_ref_roundtrip_and_survive_reopen() {
    let (dir, m) = test_manager();
    let s = session(&m);
    let sid = s.id;
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(tid).unwrap();
    let fp = fingerprint_fixture();
    let cref = candidate_fixture(rev);
    let rec = s
        .create_verification_record_with_evidence(
            tid,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            7,
            Some(fp.clone()),
            Some(cref.clone()),
        )
        .unwrap();
    let got = s.get_verification_record(rec).unwrap().unwrap();
    assert_eq!(got.environment_fingerprint.as_ref(), Some(&fp));
    assert_eq!(got.candidate_proof_ref.as_ref(), Some(&cref));
    assert_eq!(
        s.list_verification_records(tid).unwrap()[0]
            .environment_fingerprint
            .as_ref(),
        Some(&fp),
        "the list surface exposes the same evidence"
    );
    // Full reopen: the v20 JSON columns survive byte-identically.
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    let got2 = s2.get_verification_record(rec).unwrap().unwrap();
    assert_eq!(got2.environment_fingerprint.as_ref(), Some(&fp));
    assert_eq!(got2.candidate_proof_ref.as_ref(), Some(&cref));
}

#[test]
fn legacy_record_without_evidence_reads_as_absent() {
    // Compatibility: a record written through the legacy constructor
    // carries SQL NULL evidence and reads as an honest absent value.
    let (_dir, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec![])).unwrap();
    let rec = s
        .create_verification_record(
            tid,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap();
    let got = s.get_verification_record(rec).unwrap().unwrap();
    assert!(got.environment_fingerprint.is_none());
    assert!(got.candidate_proof_ref.is_none());
    assert_eq!(s.list_verification_records(tid).unwrap().len(), 1);
}

#[test]
fn hostile_or_oversized_evidence_columns_are_loud_on_read() {
    let (_dir, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec![])).unwrap();
    let base = s
        .create_verification_record(
            tid,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            1,
        )
        .unwrap();
    let row = s
        .manager
        .store()
        .verification_record_get(base)
        .unwrap()
        .unwrap();
    // A hostile injected value behind the API: not the typed shape.
    let evil = s
        .manager
        .store()
        .verification_record_put_with_evidence(&row, Some("not json"), None)
        .unwrap();
    assert!(matches!(
        s.get_verification_record(evil),
        Err(TaskError::Malformed(_))
    ));
    // An oversized injected value: typed oversized, never truncated.
    let huge = "x".repeat(MAX_VERIFICATION_FINGERPRINT_JSON_BYTES + 1);
    let fat = s
        .manager
        .store()
        .verification_record_put_with_evidence(&row, Some(&huge), None)
        .unwrap();
    assert!(matches!(
        s.get_verification_record(fat),
        Err(TaskError::Oversized(_))
    ));
    // Structurally valid JSON with an out-of-bounds field is malformed.
    let mut fp = fingerprint_fixture();
    fp.task_contract_hash = "zz".into();
    let bad = s
        .manager
        .store()
        .verification_record_put_with_evidence(
            &row,
            Some(&serde_json::to_string(&fp).unwrap()),
            None,
        )
        .unwrap();
    assert!(matches!(
        s.get_verification_record(bad),
        Err(TaskError::Malformed(_))
    ));
}

#[test]
fn oversized_or_mismatched_evidence_is_rejected_typed_before_any_write() {
    let (_dir, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec![])).unwrap();
    let rev = s.task_revision(tid).unwrap();
    // Oversized fingerprint (a version over its cap) refuses typed.
    let mut fp = fingerprint_fixture();
    fp.toolchain_versions[0].version =
        "v".repeat(faktor_core::state::MAX_ENVIRONMENT_FINGERPRINT_VERSION_BYTES + 1);
    let err = s
        .create_verification_record_with_evidence(
            tid,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            1,
            Some(fp),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Oversized(_)), "{err:?}");
    // Malformed digest refuses typed.
    let mut fp = fingerprint_fixture();
    fp.check_argv_cwd_env_hash = "nope".into();
    let err = s
        .create_verification_record_with_evidence(
            tid,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            1,
            Some(fp),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Malformed(_)), "{err:?}");
    // A candidate reference certifying the WRONG revision refuses typed.
    let err = s
        .create_verification_record_with_evidence(
            tid,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            1,
            None,
            Some(candidate_fixture(rev.checked_next().unwrap())),
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Malformed(_)), "{err:?}");
    // None of the refusals wrote a row.
    assert!(s.list_verification_records(tid).unwrap().is_empty());
}

#[test]
fn candidate_ref_matches_accounting_digest_at_completion_and_survives_reopen() {
    let (dir, m) = test_manager();
    let s = session(&m);
    let sid = s.id;
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    // No reservations exist: the accounting picture is already settled,
    // so the digest recorded at build time MUST equal the digest
    // recomputed after the completion accounting pass.
    let digest = s.accounting_snapshot_digest(tid).unwrap();
    let mut cref = candidate_fixture(rev);
    cref.accounting_snapshot_digest = digest.clone();
    let rec = s
        .create_verification_record_with_evidence(
            tid,
            None,
            vec![CriterionVerification {
                criterion_key: "c1".into(),
                passed: true,
                evidence: Some("exit 0".into()),
                binding: None,
            }],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            1,
            Some(fingerprint_fixture()),
            Some(cref),
        )
        .unwrap();
    s.finalize_verification_record(rec, VerificationStatus::Passed, 9)
        .unwrap();
    // Crash seam: reopen before completion, the reference is intact.
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    let before = s2.get_verification_record(rec).unwrap().unwrap();
    assert_eq!(
        before
            .candidate_proof_ref
            .as_ref()
            .unwrap()
            .accounting_snapshot_digest,
        digest,
        "the candidate reference survives the reopen"
    );
    let done = s2.complete_verified_task(tid, rev, rec).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
    assert_eq!(
        s2.accounting_snapshot_digest(tid).unwrap(),
        digest,
        "the recorded accounting digest matches the snapshot at completion"
    );
    let after = s2.get_verification_record(rec).unwrap().unwrap();
    assert_eq!(
        after
            .candidate_proof_ref
            .as_ref()
            .unwrap()
            .accounting_snapshot_digest,
        digest,
        "the candidate reference is immutable across completion"
    );
}

// ------------------------------------------------ completion contract (P2)

fn push_contract() -> CompletionContract {
    CompletionContract {
        include_commit: false,
        include_push: true,
        include_pr: false,
    }
}

/// Default contract parity: no durable rows, no new gate, the completion
/// path behaves exactly as before and `VerifiedComplete` lands.
#[test]
fn default_contract_completion_is_byte_identical_and_writes_no_rows() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    let rec = passed_record(&s, tid, &["c1".into()]);
    assert_eq!(
        s.completion_contract_gate(tid).unwrap(),
        CompletionContractGate::Satisfied,
        "no contract means the gate reads nothing"
    );
    // An explicit all-false contract is never recorded.
    let err = s
        .set_completion_contract(tid, rev, CompletionContract::default())
        .unwrap_err();
    assert!(matches!(err, TaskError::Malformed(_)), "{err:?}");
    // A step status without any accepted contract is refused.
    let err = s
        .set_completion_step_status(
            tid,
            CompletionStep::Push,
            CompletionStepOutcome::Succeeded,
            "no run recorded this contract",
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Malformed(_)), "{err:?}");
    let done = s.complete_verified_task(tid, rev, rec).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
    // The default completion path wrote ZERO completion rows.
    assert!(s.ledger_completion_contract(tid.raw()).unwrap().is_none());
    assert!(s
        .ledger_completion_step_statuses(tid.raw(), rev.raw())
        .unwrap()
        .is_empty());
}

/// A requested step without a Succeeded row refuses with the step named
/// and the task stays Verifying; recording Succeeded afterwards lets a
/// later completion pass converge.
#[test]
fn missing_requested_step_refuses_then_completes_after_status_lands() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    // The contract is recorded against the run's start revision; the
    // task row then moves revisions through the machine.
    let contract_rev = s.task_revision(tid).unwrap();
    s.set_completion_contract(tid, contract_rev, push_contract())
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    assert_ne!(rev, contract_rev);
    let rec = passed_record(&s, tid, &["c1".into()]);
    let refusal = s.complete_verified_task(tid, rev, rec).unwrap_err();
    match &refusal {
        TaskError::CompletionStepMissing {
            task_id,
            revision,
            step,
        } => {
            assert_eq!(*task_id, tid);
            assert_eq!(*revision, contract_rev);
            assert_eq!(*step, CompletionStep::Push);
        }
        other => panic!("expected a missing-step refusal, got {other:?}"),
    }
    assert_eq!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::Verifying,
        "a refused gate never moves the row"
    );
    // Record the durable success; the SAME revision/record now completes.
    s.set_completion_step_status(
        tid,
        CompletionStep::Push,
        CompletionStepOutcome::Succeeded,
        "pushed to origin/main",
    )
    .unwrap();
    let done = s.complete_verified_task(tid, rev, rec).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

/// A Failed step is terminal for its contract revision: even a later
/// Succeeded row cannot resurrect it, and the row stays Verifying.
#[test]
fn failed_step_is_a_terminal_refusal() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let contract_rev = s.task_revision(tid).unwrap();
    s.set_completion_contract(tid, contract_rev, push_contract())
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    let rec = passed_record(&s, tid, &["c1".into()]);
    s.set_completion_step_status(
        tid,
        CompletionStep::Push,
        CompletionStepOutcome::Failed,
        "remote rejected the push (non-fast-forward)",
    )
    .unwrap();
    let err = s.complete_verified_task(tid, rev, rec).unwrap_err();
    assert_eq!(
        err,
        TaskError::CompletionStepFailed {
            task_id: tid,
            revision: contract_rev,
            step: CompletionStep::Push,
            detail: "remote rejected the push (non-fast-forward)".into(),
        }
    );
    assert_eq!(
        faktor_core::Error::from(err.clone()).kind,
        ErrorKind::Conflict
    );
    // A later Succeeded row does not resurrect the failed run.
    s.set_completion_step_status(
        tid,
        CompletionStep::Push,
        CompletionStepOutcome::Succeeded,
        "retry succeeded",
    )
    .unwrap();
    let err = s.complete_verified_task(tid, rev, rec).unwrap_err();
    assert!(matches!(
        err,
        TaskError::CompletionStepFailed {
            step: CompletionStep::Push,
            ..
        }
    ));
    assert_eq!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::Verifying
    );
}

/// Skipped is non-Succeeded: the refusal is typed, names the step and
/// stays retryable (unlike Failed).
#[test]
fn skipped_step_refuses_typed_and_stays_retryable() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let contract_rev = s.task_revision(tid).unwrap();
    s.set_completion_contract(tid, contract_rev, push_contract())
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    let rec = passed_record(&s, tid, &["c1".into()]);
    s.set_completion_step_status(
        tid,
        CompletionStep::Push,
        CompletionStepOutcome::Skipped,
        "no remote configured",
    )
    .unwrap();
    let err = s.complete_verified_task(tid, rev, rec).unwrap_err();
    match err {
        TaskError::CompletionStepNotSucceeded { step, status, .. } => {
            assert_eq!(step, CompletionStep::Push);
            assert_eq!(status, CompletionStepOutcome::Skipped);
        }
        other => panic!("expected a non-succeeded refusal, got {other:?}"),
    }
    // The same refusal is terminal only for Failed; a Succeeded row can
    // still converge a Skipped step.
    s.set_completion_step_status(
        tid,
        CompletionStep::Push,
        CompletionStepOutcome::Succeeded,
        "remote configured and pushed",
    )
    .unwrap();
    s.complete_verified_task(tid, rev, rec).unwrap();
}

/// Gate order: commit is satisfied, push is missing — the refusal names
/// push, never commit.
#[test]
fn multi_step_gate_names_the_first_missing_step_in_order() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let contract_rev = s.task_revision(tid).unwrap();
    let contract = CompletionContract {
        include_commit: true,
        include_push: true,
        include_pr: true,
    };
    s.set_completion_contract(tid, contract_rev, contract)
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    let rec = passed_record(&s, tid, &["c1".into()]);
    s.set_completion_step_status(
        tid,
        CompletionStep::Commit,
        CompletionStepOutcome::Succeeded,
        "committed 0123abc",
    )
    .unwrap();
    let err = s.complete_verified_task(tid, rev, rec).unwrap_err();
    match err {
        TaskError::CompletionStepMissing { step, .. } => {
            assert_eq!(step, CompletionStep::Push, "commit was satisfied")
        }
        other => panic!("expected a missing push refusal, got {other:?}"),
    }
    // Push now succeeds; the third requested step (pr) is the next
    // named refusal, proving the gate walks commit → push → pr.
    s.set_completion_step_status(
        tid,
        CompletionStep::Push,
        CompletionStepOutcome::Succeeded,
        "pushed 0123abc",
    )
    .unwrap();
    let err = s.complete_verified_task(tid, rev, rec).unwrap_err();
    match err {
        TaskError::CompletionStepMissing { step, .. } => {
            assert_eq!(step, CompletionStep::Pr, "commit and push were satisfied")
        }
        other => panic!("expected a missing pr refusal, got {other:?}"),
    }
    // The final step lands and the claim certifies.
    s.set_completion_step_status(
        tid,
        CompletionStep::Pr,
        CompletionStepOutcome::Succeeded,
        "opened PR #12",
    )
    .unwrap();
    s.complete_verified_task(tid, rev, rec).unwrap();
}

/// Contract immutability at the task surface: a second set for the same
/// revision is the typed Conflict refusal (never a silent overwrite).
#[test]
fn completion_contract_is_immutable_per_revision() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(tid).unwrap();
    s.set_completion_contract(tid, rev, push_contract())
        .unwrap();
    let replacement = CompletionContract {
        include_commit: true,
        include_push: false,
        include_pr: false,
    };
    let err = s
        .set_completion_contract(tid, rev, replacement)
        .unwrap_err();
    assert_eq!(
        err,
        TaskError::CompletionContractImmutable {
            task_id: tid,
            revision: rev
        }
    );
    assert_eq!(faktor_core::Error::from(err).kind, ErrorKind::Conflict);
    // The accepted contract is unchanged.
    assert_eq!(
        s.completion_contract(tid).unwrap(),
        Some((rev, push_contract()))
    );
    // A stale/fabricated revision never records a contract: the run's
    // start revision is the task row's current revision.
    let err = s
        .set_completion_contract(tid, rev.checked_next().unwrap(), replacement)
        .unwrap_err();
    assert!(matches!(err, TaskError::RevisionMismatch { .. }), "{err:?}");
    assert_eq!(
        s.completion_contract(tid).unwrap(),
        Some((rev, push_contract()))
    );
    // A contract for a task that does not exist is refused.
    let err = s
        .set_completion_contract(TaskId::new(999), rev, push_contract())
        .unwrap_err();
    assert_eq!(err, TaskError::NotFound(TaskId::new(999)));
}

/// The contract and its step rows are pinned across compaction, and a
/// reopened session re-evaluates the gate to the same verdict.
#[test]
fn completion_contract_survives_compaction_and_reopen() {
    let (dir, m) = test_manager();
    let s = session(&m);
    let sid = s.id;
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let contract_rev = s.task_revision(tid).unwrap();
    s.set_completion_contract(tid, contract_rev, push_contract())
        .unwrap();
    s.set_completion_step_status(
        tid,
        CompletionStep::Push,
        CompletionStepOutcome::Succeeded,
        "pushed before compaction",
    )
    .unwrap();
    s.ledger_goal_set("compaction pressure").unwrap();
    let report = s.compact_typed_ledger().unwrap();
    assert!(
        report.pinned.len() >= 2,
        "the contract row and its status row are pinned: {:?}",
        report.pinned
    );
    assert_eq!(
        s.completion_contract_gate(tid).unwrap(),
        CompletionContractGate::Satisfied
    );
    // Reopen on the same store: the strict open passes and the gate
    // still reads the durable contract + step outcome.
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    assert_eq!(
        s2.completion_contract(tid).unwrap(),
        Some((contract_rev, push_contract()))
    );
    let rows = s2
        .ledger_completion_step_statuses(tid.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
    assert_eq!(
        s2.completion_contract_gate(tid).unwrap(),
        CompletionContractGate::Satisfied
    );
}

/// Hostile shape at the task surface: oversized detail, zero revision
/// and a status for an unrecorded revision are typed refusals.
#[test]
fn completion_step_status_hostile_shapes_are_typed_refusals() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(tid).unwrap();
    s.set_completion_contract(tid, rev, push_contract())
        .unwrap();
    let oversized = "x".repeat(crate::ledger::MAX_COMPLETION_STEP_DETAIL + 1);
    let err = s
        .set_completion_step_status(
            tid,
            CompletionStep::Push,
            CompletionStepOutcome::Succeeded,
            &oversized,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Oversized(_)), "{err:?}");
    // A raw append for a revision with no contract is a typed Conflict.
    let err = s
        .ledger_completion_step_status(
            tid.raw(),
            rev.raw() + 1,
            CompletionStep::Push,
            CompletionStepOutcome::Succeeded,
            "orphan",
            1,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict, "{err}");
    // at_ms <= 0 and revision 0 are malformed.
    assert!(s
        .ledger_completion_step_status(
            tid.raw(),
            rev.raw(),
            CompletionStep::Push,
            CompletionStepOutcome::Succeeded,
            "ok",
            0,
        )
        .is_err());
    assert!(s
        .ledger_completion_step_status(
            tid.raw(),
            0,
            CompletionStep::Push,
            CompletionStepOutcome::Succeeded,
            "ok",
            1,
        )
        .is_err());
}

// ------------------------------------------- integration binding (P0)

/// A session over a REAL workspace root (the root-snapshot binding needs
/// a resolvable, digestible tree).
fn real_root_session(
    dir: &tempfile::TempDir,
) -> (Arc<SessionManager>, SessionHandle, std::path::PathBuf) {
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    let m = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let ws = m.create_workspace(root.to_str().unwrap()).unwrap();
    let s = m.create_session(ws, "t", "ollama", "qwen3.8").unwrap();
    (m, s, root)
}

/// The canonical tree-manifest digest (the ONE root identity shared with
/// the fs layer's `tree_manifest`), exactly what every completion
/// binding compares.
fn root_digest(root: &std::path::Path) -> String {
    current_manifest_digest(root).unwrap()
}

fn passed_record_with_tree(
    s: &SessionHandle,
    task_id: TaskId,
    criteria: &[String],
    tree: &str,
) -> VerificationRecordId {
    let criteria: Vec<CriterionVerification> = criteria
        .iter()
        .map(|c| CriterionVerification {
            criterion_key: c.clone(),
            passed: true,
            evidence: Some("exit 0".into()),
            binding: None,
        })
        .collect();
    s.create_verification_record(
        task_id,
        Some(tree.to_string()),
        criteria,
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        1,
    )
    .unwrap()
}

/// One FINALIZED durable integration record (the binding authority).
fn finalized_integration(
    s: &SessionHandle,
    task_id: TaskId,
    root: &std::path::Path,
    final_hash: &str,
    files: &[&str],
) -> crate::ledger::IntegrationRecordRow {
    let hex = "a".repeat(64);
    let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    s.ledger_integration_record_set(&crate::ledger::IntegrationRecordRow {
        run_id: "run-integration-test".into(),
        task_id: task_id.raw(),
        base_revision: Some("child-0-base".into()),
        base_snapshot: Some(hex.clone()),
        run_base_snapshot: Some(hex.clone()),
        candidate_snapshot: Some(final_hash.to_string()),
        landed_snapshot: Some(final_hash.to_string()),
        proof_basis_digest: Some(format!("blake3:{}", "b".repeat(64))),
        integration_txn_id: Some(format!("blake3:{}", "c".repeat(64))),
        final_root: root.to_string_lossy().into_owned(),
        final_snapshot_hash: final_hash.to_string(),
        integrated_files: files.clone(),
        integrated_file_count: files.len() as u64,
        integrated_files_digest: hex.clone(),
        conflicts: vec![],
        conflict_count: 0,
        sources: vec![crate::ledger::IntegrationSourceRow {
            child_id: "child-0".into(),
            change_set_id: "child-0-base-cs".into(),
            candidate_root_hash: hex.clone(),
        }],
        source_count: 1,
        sources_digest: hex,
        at_ms: 1,
    })
    .unwrap();
    s.ledger_integration_record_for_task(task_id.raw())
        .unwrap()
        .unwrap()
}

/// A passing root record whose snapshot has NO durable integration record
/// can never complete: the refusal is typed and the task stays Verifying.
#[test]
fn completion_refuses_a_root_record_without_the_integration_record() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, s, root) = real_root_session(&dir);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    let digest = root_digest(&root);
    let record = passed_record_with_tree(&s, tid, &["c1".into()], &digest);
    let err = s.complete_verified_task(tid, rev, record).unwrap_err();
    assert!(
        matches!(err, TaskError::IntegrationRecordMissing { .. }),
        "{err}"
    );
    assert_ne!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    // A finalized integration row unbinds the refusal.
    finalized_integration(&s, tid, &root, &digest, &["base.txt"]);
    let done = s.complete_verified_task(tid, rev, record).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

/// An arbitrary owner-checkout edit AFTER the integration moves the root
/// snapshot: completion refuses with the typed mismatch, the durable
/// binding survives a real store restart, and restoring the certified
/// root lets the SAME record complete.
#[test]
fn owner_edit_after_integration_refuses_completion_typed_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (m, s, root) = real_root_session(&dir);
    let sid = s.id;
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    let before = root_digest(&root);
    let record = passed_record_with_tree(&s, tid, &["c1".into()], &before);
    let integration = finalized_integration(&s, tid, &root, &before, &["base.txt"]);
    // Re-appending the identical finalized row is an idempotent replay:
    // the newest row resolves byte-identically.
    assert_eq!(
        finalized_integration(&s, tid, &root, &before, &["base.txt"]),
        integration
    );
    // An unrelated owner edit (never part of the integration) moves the
    // root snapshot.
    std::fs::write(root.join("late.txt"), "owner drift\n").unwrap();
    let after = root_digest(&root);
    assert_ne!(after, before);
    match s.complete_verified_task(tid, rev, record).unwrap_err() {
        TaskError::IntegrationSnapshotMismatch {
            recorded, current, ..
        } => {
            assert_eq!(recorded, before);
            assert_eq!(current, after);
        }
        other => panic!("expected snapshot mismatch, got {other}"),
    }
    // The refusal wrote nothing: reopen the REAL store and read the same
    // durable binding back.
    drop(s);
    drop(m);
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    assert_eq!(
        s2.ledger_integration_record_for_task(tid.raw())
            .unwrap()
            .unwrap(),
        integration
    );
    let durable_record = s2
        .list_verification_records(tid)
        .unwrap()
        .into_iter()
        .find(|r| r.record_id == record)
        .expect("record durable");
    assert_eq!(durable_record.tree_hash.as_deref(), Some(before.as_str()));
    // Restore the certified root: the same record now completes.
    std::fs::remove_file(root.join("late.txt")).unwrap();
    let done = s2.complete_verified_task(tid, rev, record).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

/// Completion-contract step outcomes reference the integration snapshot;
/// a later root edit turns their `Succeeded` into a typed gate refusal.
#[test]
fn completion_step_status_is_bound_to_the_integration_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let (_m, s, root) = real_root_session(&dir);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(tid).unwrap();
    s.set_completion_contract(tid, rev, push_contract())
        .unwrap();
    let digest = root_digest(&root);
    finalized_integration(&s, tid, &root, &digest, &["base.txt"]);
    s.set_completion_step_status(
        tid,
        CompletionStep::Push,
        CompletionStepOutcome::Succeeded,
        "pushed",
    )
    .unwrap();
    let rows = s
        .ledger_completion_step_statuses(tid.raw(), rev.raw())
        .unwrap();
    assert_eq!(rows[0].snapshot.as_deref(), Some(digest.as_str()));
    assert_eq!(
        s.completion_contract_gate(tid).unwrap(),
        CompletionContractGate::Satisfied
    );
    std::fs::write(root.join("late.txt"), "drift\n").unwrap();
    match s.completion_contract_gate(tid).unwrap() {
        CompletionContractGate::Refused(TaskError::CompletionStepSnapshotMismatch {
            recorded,
            current,
            ..
        }) => {
            assert_eq!(recorded, digest);
            assert_ne!(current, digest);
        }
        other => panic!("expected step snapshot mismatch, got {other:?}"),
    }
}

/// The canonical tree-manifest digest is deterministic, skips VCS
/// bookkeeping and refuses oversized walks instead of returning a
/// partial digest; the completion-facing helper keeps the typed session
/// mapping (unresolvable root, special files).
#[test]
fn root_manifest_digest_is_deterministic_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a/x.txt"), "x").unwrap();
    std::fs::write(root.join("y.txt"), "y").unwrap();
    let first = root_digest(&root);
    assert!(
        first.starts_with(faktor_fs::tree_manifest::TREE_MANIFEST_DIGEST_PREFIX),
        "the canonical digest is version-prefixed: {first}"
    );
    std::fs::write(root.join("y.txt"), "z").unwrap();
    let changed = root_digest(&root);
    assert_ne!(changed, first);
    std::fs::create_dir_all(root.join(".git/objects")).unwrap();
    std::fs::write(root.join(".git/objects/blob"), "commit state").unwrap();
    assert_eq!(root_digest(&root), changed, "VCS bookkeeping is skipped");
    let err = faktor_fs::tree_manifest::tree_manifest_digest(&root, 1).unwrap_err();
    assert!(
        matches!(
            err,
            faktor_fs::tree_manifest::TreeManifestError::Oversized(_)
        ),
        "{err}"
    );
    // The session mapping keeps the typed refusal for an unresolvable
    // root and for a special file (equality is unprovable then).
    let err = current_manifest_digest(&dir.path().join("missing")).unwrap_err();
    assert!(
        matches!(err, TaskError::RootSnapshotUnavailable(_)),
        "{err}"
    );
    assert!(matches!(
        task_error_from_tree_manifest(faktor_fs::tree_manifest::TreeManifestError::SpecialFile {
            paths: vec!["pipe".into()],
        }),
        TaskError::RootSnapshotSpecialFile { .. }
    ));
}

// ------------------------------------------- typed criterion bindings (P0)

#[test]
fn legacy_goal_criterion_requires_aggregate_review() {
    // A legacy plain-text goal criterion decodes with the AggregateGoal
    // binding: it may only pass through the aggregate review, never by
    // the suite status. A check-derived legacy row binds its command; an
    // arbitrary prose entry binds Unavailable.
    let goal = Criterion::legacy("goal: ship the feature");
    assert_eq!(
        goal.binding,
        Some(faktor_core::state::CriterionBinding::AggregateGoal)
    );
    let check = Criterion::legacy("required check: cargo check");
    assert_eq!(
        check.binding,
        Some(faktor_core::state::CriterionBinding::RequiredCheck {
            check_id: String::new(),
            command_digest: faktor_core::state::command_binding_digest("cargo check"),
        })
    );
    let prose = Criterion::legacy("the code is nice");
    assert!(matches!(
        prose.binding,
        Some(faktor_core::state::CriterionBinding::Unavailable { .. })
    ));
    // A V2 envelope written before bindings existed decodes faithfully
    // (binding None) and the EVALUATOR migrates it from its text; the
    // migration never changes the content id.
    let user = Criterion::user("goal: ship the feature");
    let encoded = user.encode();
    let decoded = Criterion::decode(&encoded).expect("v2 decode");
    assert_eq!(decoded.binding, None);
    assert_eq!(
        decoded.effective_binding(),
        faktor_core::state::CriterionBinding::AggregateGoal
    );
    assert_eq!(decoded.id, user.id);
}

#[test]
fn one_failed_required_criterion_blocks_task() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec![]))
        .unwrap();
    // Two typed criteria, each bound to its OWN required check.
    let c1 = Criterion::derived(
        "required check: cargo check",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(faktor_core::state::CriterionBinding::RequiredCheck {
        check_id: "rust_check".into(),
        command_digest: faktor_core::state::command_binding_digest("cargo check"),
    });
    let c2 = Criterion::derived(
        "required check: cargo test",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(faktor_core::state::CriterionBinding::RequiredCheck {
        check_id: "rust_test".into(),
        command_digest: faktor_core::state::command_binding_digest("cargo test"),
    });
    s.set_task_criteria(t.task_id, vec![c1.clone(), c2.clone()])
        .unwrap();
    let rev = drive_to_verifying(&s, t.task_id);
    // A record whose c2 verdict is present but NOT passed: the task must
    // stay Verifying and completion must refuse, even though every other
    // criterion and every check passed.
    let record = s
        .create_verification_record(
            t.task_id,
            None,
            vec![
                CriterionVerification {
                    criterion_key: c1.encode(),
                    passed: true,
                    evidence: Some("exit 0".into()),
                    binding: c1.binding.clone(),
                },
                CriterionVerification {
                    criterion_key: c2.encode(),
                    passed: false,
                    evidence: Some("required check 'rust_test' failed".into()),
                    binding: c2.binding.clone(),
                },
            ],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap();
    let err = s
        .complete_verified_task(t.task_id, rev, record)
        .unwrap_err();
    assert!(
        matches!(err, TaskError::CriteriaNotCovered { ref missing, .. } if missing == &vec![c2.encode()]),
        "{err}"
    );
    assert_eq!(
        s.get_task(t.task_id).unwrap().unwrap().state,
        TaskState::Verifying,
        "a failed required criterion must block the task"
    );
}

// ------------------------------- manifest-bound proofs + durable reads (FIX 1/2)

/// FIX 1: a binding-less LEGACY record stays viewable but can never
/// certify a MODERN (V2-bound) task criterion: completion refuses with a
/// typed outcome NAMING the legacy record and post-upgrade recovery
/// forces re-verification. A re-verified record that carries the
/// criterion's own binding then completes.
#[test]
fn legacy_binding_less_record_cannot_certify_a_modern_task() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec![])).unwrap();
    let bound = Criterion::derived(
        "required check: cargo check",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::RequiredCheck {
        check_id: "rust_check".into(),
        command_digest: faktor_core::state::command_binding_digest("cargo check"),
    });
    s.set_task_criteria(tid, vec![bound.clone()]).unwrap();
    let rev = drive_to_verifying(&s, tid);
    // The legacy record: passed verdict with the right key, NO binding.
    let legacy = s
        .create_verification_record(
            tid,
            None,
            vec![CriterionVerification {
                criterion_key: bound.encode(),
                passed: true,
                evidence: Some("legacy pre-binding verdict".into()),
                binding: None,
            }],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap();
    let err = s.complete_verified_task(tid, rev, legacy).unwrap_err();
    assert!(
        matches!(
            err,
            TaskError::LegacyProofRequiresReverification {
                task_id,
                record,
                ref criteria,
            } if task_id == tid && record == legacy && criteria == &vec![bound.encode()]
        ),
        "{err}"
    );
    // The read-only binding mirror refuses identically (the store CAS is
    // never reached with a legacy proof).
    let mirror = s
        .verify_completion_proof_binding(tid, legacy, None)
        .unwrap_err();
    assert!(
        matches!(mirror, TaskError::LegacyProofRequiresReverification { .. }),
        "{mirror}"
    );
    assert_eq!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::Verifying,
        "the refusal must write nothing"
    );
    // Re-verification: a fresh verdict through the criterion's OWN
    // binding certifies.
    let fresh = s
        .create_verification_record(
            tid,
            None,
            vec![CriterionVerification {
                criterion_key: bound.encode(),
                passed: true,
                evidence: Some("re-verified under the binding".into()),
                binding: bound.binding.clone(),
            }],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap();
    let done = s.complete_verified_task(tid, rev, fresh).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

/// FIX 1: a MUTATING completion (the task carries a durable integration
/// record) without a canonical manifest-bound tree hash is typed-refused:
/// an unbound record can never certify the landed mutation. A session
/// with NO durable mutation evidence keeps the legacy in-session
/// contract, where the record certifies through its bound criteria.
#[test]
fn mutating_completion_without_a_tree_hash_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (m, s, root) = real_root_session(&dir);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    let rev = drive_to_verifying(&s, tid);
    // Durable mutation evidence: a finalized integration record for the
    // task (its changes were staged/landed through the pipeline).
    let _integration = finalized_integration(&s, tid, &root, &"d".repeat(64), &["changed.txt"]);
    let unbound = s
        .create_verification_record(
            tid,
            None,
            vec![CriterionVerification {
                criterion_key: "c1".into(),
                passed: true,
                evidence: Some("unbound proof of a landed mutation".into()),
                binding: None,
            }],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
        )
        .unwrap();
    let err = s.complete_verified_task(tid, rev, unbound).unwrap_err();
    assert!(
        matches!(
            err,
            TaskError::ManifestBindingMissing { task_id, record }
                if task_id == tid && record == unbound
        ),
        "{err}"
    );
    assert_eq!(
        s.get_task(tid).unwrap().unwrap().state,
        TaskState::Verifying,
        "the refusal must write nothing"
    );
    // Boundary: a session with NO durable mutation evidence (no
    // integration record, no live shadow with a run base) keeps the
    // legacy in-session contract — the record certifies through its
    // criteria, so a plainly non-shadowed conversation still works.
    let root2 = dir.path().join("root2");
    std::fs::create_dir_all(&root2).unwrap();
    std::fs::write(root2.join("base.txt"), "base\n").unwrap();
    let ws2 = m.create_workspace(root2.to_str().unwrap()).unwrap();
    let s2 = m.create_session(ws2, "t2", "ollama", "qwen3.8").unwrap();
    let tid2 = s2.task_id().unwrap();
    s2.create_task(criteria_task(&s2, tid2, vec!["c1".into()]))
        .unwrap();
    let rev2 = drive_to_verifying(&s2, tid2);
    let plain = passed_record(&s2, tid2, &["c1".into()]);
    let done = s2.complete_verified_task(tid2, rev2, plain).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

/// FIX 2: a PRESENT-but-corrupt completion-contract row is a typed
/// [`TaskError::CorruptDurableState`] (never `None`/satisfied), and a
/// failed durable read is [`TaskError::Store`] — while a genuinely
/// MISSING contract follows the not-found policy (`Ok(None)` /
/// satisfied gate).
#[test]
fn missing_corrupt_and_failed_durable_reads_are_distinct() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let tid = s.task_id().unwrap();
    s.create_task(criteria_task(&s, tid, vec!["c1".into()]))
        .unwrap();
    // (1) Missing: the not-found policy.
    assert_eq!(s.completion_contract(tid).unwrap(), None);
    assert_eq!(
        s.completion_contract_gate(tid).unwrap(),
        CompletionContractGate::Satisfied
    );
    // (2) PresentMalformed: a raw all-false contract row (bypassing the
    // typed appender) refuses BOTH reads as corrupt durable state.
    m.store()
        .append_ledger_entry(
            s.id,
            crate::ledger::ENTRY_COMPLETION_CONTRACT_SET,
            crate::ledger::LEDGER_ENTRY_SCHEMA_V,
            serde_json::json!({
                "kind": "completion_contract_set",
                "task_id": tid.raw(),
                "revision": 1,
                "contract": {
                    "include_commit": false,
                    "include_push": false,
                    "include_pr": false,
                },
            }),
        )
        .unwrap();
    assert!(matches!(
        s.completion_contract(tid).unwrap_err(),
        TaskError::CorruptDurableState { .. }
    ));
    assert!(matches!(
        s.completion_contract_gate(tid).unwrap_err(),
        TaskError::CorruptDurableState { .. }
    ));
    // (3) StoreFailure: with the ledger table gone the same reads are
    // errors, never "absent/satisfied".
    m.store().sql_execute("DROP TABLE ledger_entry").unwrap();
    let err = s.completion_contract(tid).unwrap_err();
    assert!(matches!(err, TaskError::Store(_)), "{err:?}");
    let err = s.completion_contract_gate(tid).unwrap_err();
    assert!(matches!(err, TaskError::Store(_)), "{err:?}");
}

#[test]
fn proof_record_cannot_be_reused_after_check_basis_change() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(t.task_id).unwrap();
    let basis = proof_basis_fixture(t.task_id.raw(), rev.raw(), "check-basis-a");
    let (fp, cref) = fingerprint_with_basis(&basis, rev);
    let record = s
        .create_verification_record_with_evidence(
            t.task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
            Some(fp),
            Some(cref),
        )
        .unwrap();
    assert!(s
        .verification_record_reusable(record, &basis)
        .unwrap()
        .is_allowed());
    // A different check (id) basis: same criteria, same snapshot — the
    // record must NOT be reusable.
    let changed = proof_basis_fixture(t.task_id.raw(), rev.raw(), "check-basis-b");
    match s.verification_record_reusable(record, &changed).unwrap() {
        ProofReuse::Refused { reason } => {
            assert!(reason.contains("identical basis"), "{reason}")
        }
        ProofReuse::Allowed => panic!("a moved check basis must refuse reuse"),
    }
    // A legacy record without a basis is never reusable either.
    let legacy = passed_record(&s, t.task_id, &["c1".into()]);
    assert!(!s
        .verification_record_reusable(legacy, &basis)
        .unwrap()
        .is_allowed());
}

#[test]
fn proof_record_cannot_be_reused_after_tool_version_change() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(t.task_id).unwrap();
    let mut basis = proof_basis_fixture(t.task_id.raw(), rev.raw(), "check-basis-a");
    let (fp, cref) = fingerprint_with_basis(&basis, rev);
    let record = s
        .create_verification_record_with_evidence(
            t.task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
            Some(fp),
            Some(cref),
        )
        .unwrap();
    assert!(s
        .verification_record_reusable(record, &basis)
        .unwrap()
        .is_allowed());
    // The ONLY change is a tool version: still a different basis.
    basis.tool_versions.push(ToolVersion {
        tool: "rustup-toolchain".into(),
        version: "nightly-2099".into(),
    });
    assert!(!s
        .verification_record_reusable(record, &basis)
        .unwrap()
        .is_allowed());
}

#[test]
fn proof_record_cannot_be_reused_after_system_layer_config_change() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(t.task_id).unwrap();
    let config = |system_value: &str| {
        layered_effective_config_digest(&[
            ProofConfigLayer::of_value(ProofConfigScope::System, 4, system_value),
            ProofConfigLayer::of_value(ProofConfigScope::Task, 1, "task-overrides"),
        ])
        .unwrap()
    };
    let basis = proof_basis_fixture(t.task_id.raw(), rev.raw(), "check-basis-a")
        .bind_config_digest(&config("network=allow"))
        .unwrap();
    let record = s
        .create_verification_record_bound_to_basis(
            t.task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
            &basis,
            None,
        )
        .unwrap();
    // The row EMBEDS the configuration-bound basis digest.
    let row = s.get_verification_record(record).unwrap().unwrap();
    assert_eq!(
        row.environment_fingerprint
            .as_ref()
            .and_then(|f| f.proof_basis_digest.as_deref()),
        Some(basis.digest().as_str())
    );
    assert!(s
        .verification_record_reusable(record, &basis)
        .unwrap()
        .is_allowed());

    // A SYSTEM-layer value change yields a different layered digest, so
    // the basis digest differs and the recorded proof is refused.
    let changed = proof_basis_fixture(t.task_id.raw(), rev.raw(), "check-basis-a")
        .bind_config_digest(&config("network=deny"))
        .unwrap();
    assert_ne!(basis.digest(), changed.digest());
    match s.verification_record_reusable(record, &changed).unwrap() {
        ProofReuse::Refused { reason } => {
            assert!(reason.contains("identical basis"), "{reason}")
        }
        ProofReuse::Allowed => panic!("a changed system-layer value must refuse reuse"),
    }
}

#[test]
fn unbound_basis_cannot_write_a_configuration_attributable_record() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(t.task_id).unwrap();
    let unbound = proof_basis_fixture(t.task_id.raw(), rev.raw(), "check-basis-a");
    assert!(unbound.require_config_digest().is_err());
    let err = s
        .create_verification_record_bound_to_basis(
            t.task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
            &unbound,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TaskError::Malformed(_)), "{err:?}");
    assert!(s.list_verification_records(t.task_id).unwrap().is_empty());
}

#[test]
fn layered_config_digest_is_order_sensitive_bounded_and_strict() {
    let system = ProofConfigLayer::of_value(ProofConfigScope::System, 1, "a");
    let task = ProofConfigLayer::of_value(ProofConfigScope::Task, 1, "b");
    let digest = layered_effective_config_digest(&[system.clone(), task.clone()]).unwrap();
    assert!(digest.starts_with("blake3:"));
    assert!(layered_effective_config_digest(&[task.clone(), system.clone()]).is_err());
    assert!(
        layered_effective_config_digest(&[system.clone(), system.clone()]).is_err(),
        "a duplicated layer is never silently deduplicated"
    );
    assert!(ProofConfigLayer::new(ProofConfigScope::System, 1, "").is_err());
    assert!(ProofConfigLayer::new(ProofConfigScope::System, 1, "has space").is_err());

    // Binding replaces a previous value (one binding per basis) and the
    // basis digest covers it.
    let basis = proof_basis_fixture(1, 1, "c");
    let bound = basis.clone().bind_config_digest(&digest).unwrap();
    assert_eq!(bound.config_digest(), Some(digest.as_str()));
    assert_eq!(
        bound
            .env_projection
            .iter()
            .filter(|(k, _)| k == ProofBasis::CONFIG_DIGEST_KEY)
            .count(),
        1
    );
    assert_ne!(basis.digest(), bound.digest());
    assert!(basis.clone().bind_config_digest("").is_err());
    assert!(basis
        .clone()
        .bind_config_digest("x".repeat(1024).as_str())
        .is_err());
}

#[test]
fn legacy_fnv_basis_is_refused_for_reuse_and_record_creation_with_a_typed_error() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(t.task_id).unwrap();
    let config = layered_effective_config_digest(&[ProofConfigLayer::of_value(
        ProofConfigScope::Task,
        1,
        "task-overrides",
    )])
    .unwrap();
    let mut basis = proof_basis_fixture(t.task_id.raw(), rev.raw(), "check-basis-a")
        .bind_config_digest(&config)
        .unwrap();
    // A canonical basis is reusable-eligible in shape.
    assert!(basis.legacy_authority_digest().is_none());
    basis.task_contract_digest = "fnv1a64:0123456789abcdef".into();
    let err = s
        .verification_record_reusable(VerificationRecordId::new(1), &basis)
        .unwrap_err();
    match err {
        TaskError::LegacyAuthorityDigest {
            ref digest,
            ref field,
        } => {
            assert_eq!(digest, "fnv1a64:0123456789abcdef");
            assert!(field.contains("task_contract_digest"), "{field}");
        }
        other => panic!("expected the typed legacy refusal, got {other:?}"),
    }
    // A legacy-contaminated basis can never mint a NEW record either:
    // completion must restage/reverify.
    let err = s
        .create_verification_record_bound_to_basis(
            t.task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
            &basis,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err, TaskError::LegacyAuthorityDigest { .. }),
        "{err:?}"
    );
    assert!(s.list_verification_records(t.task_id).unwrap().is_empty());
}

#[test]
fn legacy_fnv_fingerprint_is_refused_for_reuse_with_a_typed_error() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let t = s
        .create_task(criteria_task(&s, s.task_id().unwrap(), vec!["c1".into()]))
        .unwrap();
    let rev = s.task_revision(t.task_id).unwrap();
    let config = layered_effective_config_digest(&[ProofConfigLayer::of_value(
        ProofConfigScope::Task,
        1,
        "task-overrides",
    )])
    .unwrap();
    let basis = proof_basis_fixture(t.task_id.raw(), rev.raw(), "check-basis-a")
        .bind_config_digest(&config)
        .unwrap();
    let (mut fp, cref) = fingerprint_with_basis(&basis, rev);
    // A pre-BLAKE3 record's fingerprint carries a 16-hex FNV contract
    // hash: it decodes (the record stays viewable) but never reuses.
    fp.task_contract_hash = "0123456789abcdef".into();
    let record = s
        .create_verification_record_with_evidence(
            t.task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            1,
            Some(fp),
            Some(cref),
        )
        .unwrap();
    let viewable = s.get_verification_record(record).unwrap().unwrap();
    assert_eq!(
        viewable
            .environment_fingerprint
            .as_ref()
            .map(|f| f.task_contract_hash.as_str()),
        Some("0123456789abcdef"),
        "the legacy row still DECODES for viewing"
    );
    let err = s.verification_record_reusable(record, &basis).unwrap_err();
    match err {
        TaskError::LegacyAuthorityDigest {
            ref digest,
            ref field,
        } => {
            assert_eq!(digest, "0123456789abcdef");
            assert!(field.contains("task_contract_hash"), "{field}");
        }
        other => panic!("expected the typed legacy refusal, got {other:?}"),
    }
}

fn proof_basis_fixture(task_id: u64, revision: u64, check: &str) -> ProofBasis {
    ProofBasis {
        task_id,
        task_revision: revision,
        task_contract_digest: "ab".repeat(32),
        candidate_snapshot: "cd".repeat(32),
        integration_sources_digest: "ef".repeat(32),
        changed_files_digest: "12".repeat(32),
        checks: vec![ProofBasisCheck {
            check_id: "rust_check".into(),
            program: check.into(),
            args: vec!["--workspace".into()],
        }],
        verification_impl_version: "faktor-agent/0.1.0".into(),
        tool_versions: vec![ToolVersion {
            tool: "faktor-agent".into(),
            version: "0.1.0".into(),
        }],
        env_projection: vec![("RUSTFLAGS".into(), "<absent>".into())],
        instruction_epoch: Some(3),
        criteria: vec![ProofBasisCriterion {
            criterion_id: "c1".into(),
            binding_digest: None,
        }],
        reviewer_digest: None,
        evidence_digests: vec![],
    }
}

fn fingerprint_with_basis(
    basis: &ProofBasis,
    rev: TaskRevision,
) -> (EnvironmentFingerprint, CandidateProofRef) {
    let mut fp = fingerprint_fixture();
    fp.proof_basis_digest = Some(basis.digest());
    let mut cref = candidate_fixture(rev);
    cref.run_id = None;
    (fp, cref)
}
