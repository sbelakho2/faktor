#![allow(clippy::await_holding_lock)]
//! TaskExecutor tests: durable submission-keyed start admission (idempotency
//! finding 1) — replay, key reuse, claim-before-mutation, release-and-retry
//! and the concurrent double-submit matrix.

use super::*;

use faktor_core::attachment::AttachmentId;
use faktor_core::completion::CompletionContract;
use faktor_core::id::OpId;
use faktor_core::state::{TaskState, TaskTransition};
use faktor_session::DurableBudgetLedger;

/// The fixed submission keys of this suite (UUID-shaped, lowercase hex).
const KEY_A: &str = "11111111-1111-4111-8111-111111111111";
const KEY_B: &str = "22222222-2222-4222-8222-222222222222";

fn linkage_row_count(env: &Env) -> usize {
    env.manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .memory_facts()
        .unwrap()
        .iter()
        .filter(|(kind, _, _)| kind == TASK_RUN_ROW_KIND)
        .count()
}

fn message_count(env: &Env) -> usize {
    env.manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .message_count()
        .unwrap() as usize
}

/// Wait until the run's durable turn record reached a terminal status (the
/// real quiescent point: the end-of-turn task-row sync runs before it).
async fn wait_turn_settled(env: &Env, op: OpId) {
    wait_until(
        || {
            env.manager
                .get_session(env.parent)
                .unwrap()
                .unwrap()
                .turn_record(op)
                .unwrap()
                .map(|r| faktor_store::is_terminal_turn_record_status(&r.status))
                .unwrap_or(false)
        },
        240,
    )
    .await;
}

#[tokio::test]
pub(crate) async fn submission_key_replay_returns_the_byte_identical_receipt_without_a_second_run()
{
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    let mut req = request(
        "idempotent goal",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    let first = env
        .executor
        .start_task(env.parent, req.clone())
        .expect("first keyed start");
    assert_eq!(first.mode, TaskRunMode::InSession);
    wait_turn_settled(&env, first.op_id.expect("in-session op id")).await;

    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_before = h.get_task(TaskId::new(1)).unwrap().expect("task row");
    let revision_before = h.task_revision(TaskId::new(1)).unwrap();
    let messages_before = message_count(&env);
    let calls_before = env.provider.count();
    let runs_before = linkage_row_count(&env);

    // The lost-ack replay: same key, same request — the STORED receipt comes
    // back byte-for-byte and NOTHING is touched (no re-goal, no contract
    // write, no budget seed, no prompt, no second run).
    let replay = env
        .executor
        .start_task(env.parent, req.clone())
        .expect("replay returns the original receipt");
    assert_eq!(replay, first, "the typed receipt is identical");
    assert_eq!(
        serde_json::to_string(&replay).unwrap(),
        serde_json::to_string(&first).unwrap(),
        "the serialized receipt is byte-identical"
    );
    assert_eq!(env.provider.count(), calls_before, "zero new model calls");
    assert_eq!(message_count(&env), messages_before, "zero new prompts");
    assert_eq!(linkage_row_count(&env), runs_before, "zero new run rows");
    let task_after = h.get_task(TaskId::new(1)).unwrap().unwrap();
    assert_eq!(task_after.goal, task_before.goal);
    assert_eq!(task_after.state, task_before.state);
    assert_eq!(task_after.budget, task_before.budget, "no budget seed ran");
    assert_eq!(
        h.task_revision(TaskId::new(1)).unwrap(),
        revision_before,
        "a replay never re-goals the task row"
    );
    assert_eq!(
        task_after.updated_ms, task_before.updated_ms,
        "a replay performs no task-row write at all"
    );

    // A third replay with the identical body keeps answering the same bytes.
    let third = env
        .executor
        .start_task(env.parent, req)
        .expect("third replay");
    assert_eq!(third, first);
}

#[tokio::test]
pub(crate) async fn same_key_with_a_different_goal_contract_or_attachment_is_key_reused_without_mutation(
) {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    let mut req = request(
        "original goal",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    let first = env
        .executor
        .start_task(env.parent, req.clone())
        .expect("first keyed start");
    wait_turn_settled(&env, first.op_id.expect("in-session op id")).await;
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_before = h.get_task(TaskId::new(1)).unwrap().expect("task row");
    let revision_before = h.task_revision(TaskId::new(1)).unwrap();
    let messages_before = message_count(&env);
    let calls_before = env.provider.count();
    let runs_before = linkage_row_count(&env);

    let mut hostile_variants: Vec<TaskRunRequest> = Vec::new();
    let mut different_goal = req.clone();
    different_goal.goal = "a different goal".into();
    hostile_variants.push(different_goal);
    let mut different_contract = req.clone();
    different_contract.completion_contract = Some(CompletionContract {
        include_commit: true,
        include_push: true,
        include_pr: false,
    });
    hostile_variants.push(different_contract);
    // A durable attachment reference the original request never carried: the
    // attachment axis is part of the canonical digest.
    let attachment = AttachmentId::new(
        FileHash::from([9u8; 32]),
        "application/pdf",
        Some("spec.pdf"),
        4,
    )
    .unwrap();
    env.manager
        .store()
        .put_attachment(env.parent, &attachment)
        .unwrap();
    let mut different_attachments = req.clone();
    different_attachments.attachments = vec![attachment];
    hostile_variants.push(different_attachments);

    for hostile in hostile_variants {
        let err = env
            .executor
            .start_task(env.parent, hostile.clone())
            .expect_err("a different request under the same key must refuse");
        assert!(
            matches!(err, ExecError::Conflict(_)),
            "typed conflict, not a silent replay: {err}"
        );
        assert!(
            err.to_string().contains("submission id"),
            "the refusal names the submission key: {err}"
        );
        assert!(
            err.to_string().contains("different task start"),
            "the refusal names the digest mismatch: {err}"
        );
    }
    assert_eq!(env.provider.count(), calls_before, "no model call ran");
    assert_eq!(
        message_count(&env),
        messages_before,
        "no prompt was enqueued"
    );
    assert_eq!(
        linkage_row_count(&env),
        runs_before,
        "no run row was written"
    );
    let task_after = h.get_task(TaskId::new(1)).unwrap().unwrap();
    assert_eq!(task_after.goal, task_before.goal);
    assert_eq!(h.task_revision(TaskId::new(1)).unwrap(), revision_before);
    assert_eq!(task_after.updated_ms, task_before.updated_ms);
}

#[tokio::test]
pub(crate) async fn a_completed_key_short_circuits_before_every_mutation_point() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    let mut req = request(
        "already accepted",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    let first = env
        .executor
        .start_task(env.parent, req.clone())
        .expect("first keyed start");
    wait_turn_settled(&env, first.op_id.expect("in-session op id")).await;

    // Hostile later step: certify the task row terminal. Without the claim a
    // re-execution would refuse at the frozen-row check (and, on other
    // shapes, re-goal or re-budget first); with the claim the stored receipt
    // wins before the task row is even read.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = TaskId::new(1);
    let revision = h.task_revision(task_id).unwrap();
    h.transition_task(task_id, revision, TaskTransition::Cancel, None)
        .unwrap();
    let terminal_before = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(terminal_before.state, TaskState::Cancelled);
    let revision_before = h.task_revision(task_id).unwrap();
    let calls_before = env.provider.count();
    let messages_before = message_count(&env);
    let runs_before = linkage_row_count(&env);

    let replay = env
        .executor
        .start_task(env.parent, req)
        .expect("the claim answers before the frozen-row refusal");
    assert_eq!(replay, first);
    let terminal_after = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(terminal_after.state, TaskState::Cancelled);
    assert_eq!(h.task_revision(task_id).unwrap(), revision_before);
    assert_eq!(terminal_after.updated_ms, terminal_before.updated_ms);
    assert_eq!(terminal_after.goal, terminal_before.goal);
    assert_eq!(terminal_after.budget, terminal_before.budget);
    assert_eq!(env.provider.count(), calls_before);
    assert_eq!(message_count(&env), messages_before);
    assert_eq!(linkage_row_count(&env), runs_before);
}

#[tokio::test]
pub(crate) async fn a_pre_acceptance_failure_releases_the_key_and_the_retry_executes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    // First (unkeyed) run commits a monetary cap the hostile retry will
    // undercut, so the keyed attempt fails AFTER its claim (the budget write
    // is behind the admission decision) and must release the row.
    let mut cap_owner = request("cap owner", vec![wi("a1", WorkKind::Analysis, &[])], &env);
    cap_owner.max_cost_micro = Some(10_000_000);
    let owner = env
        .executor
        .start_task(env.parent, cap_owner)
        .expect("unkeyed cap-owner run");
    wait_turn_settled(&env, owner.op_id.expect("in-session op id")).await;
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    DurableBudgetLedger::new(env.manager.clone())
        .reserve(env.parent, task_id, OpId::new(7_000_001), 60_000, None)
        .await
        .expect("reserve under the cap");

    let mut refused = request(
        "release then retry",
        vec![wi("a2", WorkKind::Analysis, &[])],
        &env,
    );
    refused.submission_id = Some(KEY_B.to_string());
    refused.max_cost_micro = Some(50_000);
    let err = env
        .executor
        .start_task(env.parent, refused.clone())
        .expect_err("a cap below the committed spend refuses the start");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
    assert!(err.to_string().contains("cost cap"), "{err}");

    // The failed attempt released its pending row: the key is claimable.
    assert!(
        env.manager
            .store()
            .task_admission_claim(env.parent, KEY_B, "probe-digest", 7)
            .unwrap()
            .is_fresh(),
        "a pre-acceptance failure must release the admission so a retry can execute"
    );
    env.manager
        .store()
        .task_admission_release(env.parent, KEY_B)
        .unwrap();

    // Retry with the same key and a SATISFIABLE cap: the start executes and
    // the replay answers the byte-identical receipt.
    let mut retry = refused.clone();
    retry.max_cost_micro = Some(10_000_000);
    let receipt = env
        .executor
        .start_task(env.parent, retry.clone())
        .expect("retry after release");
    wait_turn_settled(&env, receipt.op_id.expect("in-session op id")).await;
    let replay = env
        .executor
        .start_task(env.parent, retry)
        .expect("replay after the retried acceptance");
    assert_eq!(replay, receipt);
}

#[tokio::test]
pub(crate) async fn a_pending_row_blocks_a_duplicate_start_without_touching_anything() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    // A concurrent attempt already claimed the key (its digest does not
    // matter: a pending row refuses EVERY duplicate).
    assert!(env
        .manager
        .store()
        .task_admission_claim(env.parent, KEY_A, "in-flight-digest", 1)
        .unwrap()
        .is_fresh());
    let mut req = request(
        "blocked duplicate",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    let err = env
        .executor
        .start_task(env.parent, req.clone())
        .expect_err("a pending admission blocks the duplicate");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
    assert!(err.to_string().contains("in flight"), "{err}");
    // Nothing was mutated: no task row, no prompt, no run row, no model call.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(h.get_task(TaskId::new(1)).unwrap().is_none());
    assert_eq!(message_count(&env), 0);
    assert_eq!(env.provider.count(), 0);
    assert_eq!(linkage_row_count(&env), 0);

    // The in-flight attempt releasing its claim unblocks the retry.
    env.manager
        .store()
        .task_admission_release(env.parent, KEY_A)
        .unwrap();
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("retry after the pending row released");
    wait_turn_settled(&env, receipt.op_id.expect("in-session op id")).await;
    assert_eq!(receipt.mode, TaskRunMode::InSession);
}

#[tokio::test]
pub(crate) async fn concurrent_duplicate_starts_claim_one_key_and_run_one_prompt() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    let mut req = request(
        "double submit",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());

    let env2 = env.clone();
    let req2 = req.clone();
    let other = std::thread::spawn(move || env2.executor.start_task(env2.parent, req2));
    let first = env.executor.start_task(env.parent, req.clone());
    let second = other.join().expect("claim thread");

    let mut receipts = Vec::new();
    for result in [first, second] {
        match result {
            Ok(receipt) => receipts.push(receipt),
            Err(err) => {
                assert!(matches!(err, ExecError::Conflict(_)), "{err}");
                assert!(err.to_string().contains("in flight"), "{err}");
            }
        }
    }
    assert!(!receipts.is_empty(), "at least one caller won the key");
    let winner = receipts[0].clone();
    for receipt in &receipts {
        assert_eq!(receipt, &winner, "every accepted caller sees one receipt");
    }
    wait_turn_settled(&env, winner.op_id.expect("in-session op id")).await;
    let user_messages = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .messages_page(None, 10)
        .unwrap()
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .count();
    assert_eq!(user_messages, 1, "exactly one prompt was enqueued");
    assert_eq!(linkage_row_count(&env), 1, "exactly one run row exists");
    assert_eq!(env.provider.count(), 1, "exactly one drive ran");

    // A later replay of the SAME key is the stored receipt, still one run.
    let replay = env
        .executor
        .start_task(env.parent, req)
        .expect("post-run replay");
    assert_eq!(replay, winner);
    assert_eq!(linkage_row_count(&env), 1);
}

#[tokio::test]
pub(crate) async fn orchestrated_starts_replay_and_refuse_duplicates_by_key() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    let mut req = request(
        "orchestrated idempotency",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &["a"]),
        ],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    let first = env
        .executor
        .start_task(env.parent, req.clone())
        .expect("keyed orchestrated start");
    assert_eq!(first.mode, TaskRunMode::Orchestrated);
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &first.run_id)
                .map(|rows| {
                    !rows.is_empty()
                        && rows.iter().all(|c| {
                            matches!(
                                c.state,
                                crate::ChildState::Done | crate::ChildState::Cancelled
                            )
                        })
                })
                .unwrap_or(false)
        },
        240,
    )
    .await;
    let children_before =
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &first.run_id)
            .unwrap()
            .len();

    let replay = env
        .executor
        .start_task(env.parent, req.clone())
        .expect("orchestrated replay returns the stored receipt");
    assert_eq!(replay, first);
    assert_eq!(
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &first.run_id)
            .unwrap()
            .len(),
        children_before,
        "a replay spawns no second child"
    );

    let mut different = req;
    different.goal = "a different orchestration".into();
    let err = env
        .executor
        .start_task(env.parent, different)
        .expect_err("same key, different orchestrated request");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
    assert!(err.to_string().contains("different task start"), "{err}");
}
