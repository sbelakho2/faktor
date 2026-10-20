//! Adversarial SessionHandle/agent-facing isolation tests (schema v43):
//! the session-scoped verification-record API can never read, finalize, or
//! complete through another session's record, even when both sessions share
//! one workspace and the same numeric task id.

use super::*;
use crate::handle::tests::test_manager;

fn criteria_task(handle: &SessionHandle, task_id: TaskId, criteria: Vec<String>) -> Task {
    Task {
        task_id,
        session_id: handle.id,
        goal: "scope goal".into(),
        acceptance_criteria: criteria,
        plan: vec![],
        attachments: vec![],
        contract: Default::default(),
        budget: TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: 1,
        updated_ms: 1,
    }
}

fn drive_to_verifying(handle: &SessionHandle, task_id: TaskId) -> TaskRevision {
    let r1 = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, r1, TaskTransition::StartRunning, None)
        .unwrap();
    let r2 = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, r2, TaskTransition::RequestVerification, None)
        .unwrap();
    let r3 = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, r3, TaskTransition::StartVerification, None)
        .unwrap();
    handle.task_revision(task_id).unwrap()
}

fn passing_record(
    handle: &SessionHandle,
    task_id: TaskId,
    criteria: &[String],
) -> VerificationRecordId {
    handle
        .create_verification_record(
            task_id,
            None,
            criteria
                .iter()
                .map(|c| CriterionVerification {
                    criterion_key: c.clone(),
                    passed: true,
                    evidence: Some("exit 0".into()),
                    binding: None,
                })
                .collect(),
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
fn same_workspace_sessions_are_isolated_across_the_agent_facing_api() {
    let (_dir, manager) = test_manager();
    let ws = manager.create_workspace("/scope-shared").unwrap();
    let a = manager.create_session(ws, "a", "fake", "m").unwrap();
    let b = manager.create_session(ws, "b", "fake", "m").unwrap();
    assert_eq!(a.task_id().unwrap(), b.task_id().unwrap(), "same task id");
    let ta = a.task_id().unwrap();
    let tb = b.task_id().unwrap();
    a.create_task(criteria_task(&a, ta, vec!["c1".into()]))
        .unwrap();
    b.create_task(criteria_task(&b, tb, vec!["c1".into()]))
        .unwrap();
    let ra = drive_to_verifying(&a, ta);
    let rb = drive_to_verifying(&b, tb);
    let rec_a = passing_record(&a, ta, &["c1".into()]);

    // List/read isolation.
    assert_eq!(a.list_verification_records(ta).unwrap().len(), 1);
    assert!(b.list_verification_records(tb).unwrap().is_empty());
    assert!(
        b.get_verification_record(rec_a).unwrap().is_none(),
        "foreign record ids are indistinguishable from absence"
    );
    let async_rows = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(manager.verification_records(b.id(), tb))
        .unwrap();
    assert!(async_rows.is_empty(), "pool read is scoped too");

    // Read-only proof binding refuses a foreign record.
    assert_eq!(
        b.verify_completion_proof_binding(tb, rec_a, None)
            .unwrap_err(),
        TaskError::RecordNotFound(rec_a)
    );
    // Finalize is scoped.
    assert_eq!(
        b.finalize_verification_record(rec_a, VerificationStatus::Failed, 9)
            .unwrap_err(),
        TaskError::RecordNotFound(rec_a)
    );
    // Completion cannot be satisfied by the sibling's passing record.
    assert_eq!(
        b.complete_verified_task(tb, rb, rec_a).unwrap_err(),
        TaskError::RecordNotFound(rec_a)
    );
    assert_eq!(b.task_revision(tb).unwrap(), rb);
    assert_eq!(b.get_task(tb).unwrap().unwrap().state, TaskState::Verifying);

    // The owner still completes with its own record.
    let done = a.complete_verified_task(ta, ra, rec_a).unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

#[test]
fn different_workspace_sessions_never_see_each_others_records() {
    let (_dir, manager) = test_manager();
    let wa = manager.create_workspace("/scope-a").unwrap();
    let wb = manager.create_workspace("/scope-b").unwrap();
    let a = manager.create_session(wa, "a", "fake", "m").unwrap();
    let b = manager.create_session(wb, "b", "fake", "m").unwrap();
    let ta = a.task_id().unwrap();
    let tb = b.task_id().unwrap();
    a.create_task(criteria_task(&a, ta, vec!["c1".into()]))
        .unwrap();
    b.create_task(criteria_task(&b, tb, vec!["c1".into()]))
        .unwrap();
    let ra = drive_to_verifying(&a, ta);
    let rb = drive_to_verifying(&b, tb);
    let rec_a = passing_record(&a, ta, &["c1".into()]);
    let _ = (ra, rb);
    assert!(b.list_verification_records(tb).unwrap().is_empty());
    assert!(b.get_verification_record(rec_a).unwrap().is_none());
    assert_eq!(
        b.complete_verified_task(tb, b.task_revision(tb).unwrap(), rec_a)
            .unwrap_err(),
        TaskError::RecordNotFound(rec_a)
    );
}
