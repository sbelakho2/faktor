#![allow(clippy::await_holding_lock)]
//! TaskExecutor tests: durable submission-keyed start admission (idempotency
//! finding 1) — replay, key reuse, claim-before-mutation, release-and-retry
//! and the concurrent double-submit matrix — plus the P0-ASK execution-kind
//! admission (conversational vs engineering) and the P0-GOAL-BINDING
//! acceptance sequence.
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use super::tests_core::mutating_request;
use super::*;

use faktor_core::attachment::AttachmentId;
use faktor_core::completion::CompletionContract;
use faktor_core::id::OpId;
use faktor_core::state::{TaskState, TaskTransition};
use faktor_protocol::{
    turn_execution_row_key, ExecutionKind, TurnExecution, TURN_EXECUTION_ROW_KIND,
};
use faktor_session::DurableBudgetLedger;

/// The fixed submission keys of this suite (UUID-shaped, lowercase hex).
const KEY_A: &str = "11111111-1111-4111-8111-111111111111";
const KEY_B: &str = "22222222-2222-4222-8222-222222222222";

/// Audit P1 crash matrix, shared harness: build a keyed start request, arm
/// ONE store boundary, drive `start_task` into the injected crash (the store
/// writer dies and the start returns typed), drop the "daemon" and reopen
/// the SAME data root as a NEW boot generation with boot recovery run.
fn keyed_crash_request(env: &Env) -> TaskRunRequest {
    let mut req = request("crash matrix", vec![wi("a1", WorkKind::Analysis, &[])], env);
    req.submission_id = Some(KEY_A.to_string());
    req
}

async fn crash_and_reopen(
    dir: &tempfile::TempDir,
    point: &'static str,
    ordinal: u64,
) -> (Arc<Env>, TaskRunRequest) {
    let root = dir.path().join("e");
    let env = open_env(&root, done_script());
    let req = keyed_crash_request(&env);
    env.manager
        .store()
        .crash_arm(faktor_store::CrashArm { point, ordinal });
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.executor.start_task(env.parent, req.clone())
    }));
    let crashed_result = match crashed {
        Ok(result) => result,
        Err(_) => Err(ExecError::Internal("writer panic surfaced".into())),
    };
    assert!(
        crashed_result.is_err(),
        "seam {point}#{ordinal} must interrupt the keyed start"
    );
    assert!(
        !env.manager.store().writer_available(),
        "seam {point}#{ordinal} must kill the durable writer"
    );
    let parent = env.parent;
    drop(env);
    let env = reopen_env(&root, parent, done_script());
    recover_pending_task_admissions(&env.manager).expect("boot recovery must not fail");
    (env, req)
}

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
        .reserve(
            env.parent,
            task_id,
            OpId::new(7_000_001),
            60_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
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
            .task_admission_claim(env.parent, KEY_B, "probe-digest", "tx-probe-b", 7)
            .unwrap()
            .is_fresh(),
        "a pre-acceptance failure must release the admission so a retry can execute"
    );
    env.manager
        .store()
        .task_admission_release(env.parent, KEY_B, "tx-probe-b")
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
    let mut req = request(
        "blocked duplicate",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    // A concurrent attempt already claimed the key with the EXACT digest this
    // request computes, and the claim is LIVE (current boot, unexpired
    // lease). A byte-identical duplicate is the in-flight refusal.
    let digest = crate::runtime::task_executor::task_start_digest(&req).expect("request digest");
    let now = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .now_ms();
    assert!(env
        .manager
        .store()
        .task_admission_claim(env.parent, KEY_A, &digest, "tx-00000000000000aa", now)
        .unwrap()
        .is_fresh());
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
        .task_admission_release(env.parent, KEY_A, "tx-00000000000000aa")
        .unwrap();
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("retry after the pending row released");
    wait_turn_settled(&env, receipt.op_id.expect("in-session op id")).await;
    assert_eq!(receipt.mode, TaskRunMode::InSession);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(crate) async fn a_pending_row_with_different_bytes_is_key_reused_without_touching_anything() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    let mut req = request(
        "different bytes under a live key",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    // A live pending claim whose stored digest is NOT this request's digest:
    // the contract is the typed key-reuse conflict, never a lease-long
    // in-flight answer.
    let now = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .now_ms();
    assert!(env
        .manager
        .store()
        .task_admission_claim(
            env.parent,
            KEY_A,
            "0000000000000000000000000000000000000000000000000000000000000000",
            "tx-00000000000000ab",
            now
        )
        .unwrap()
        .is_fresh());
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("a pending key with different bytes is KeyReused");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
    assert!(err.to_string().contains("different task start"), "{err}");
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(h.get_task(TaskId::new(1)).unwrap().is_none());
    assert_eq!(message_count(&env), 0);
    assert_eq!(env.provider.count(), 0);
    assert_eq!(linkage_row_count(&env), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
    // The claim path spawns the detached drive onto the runtime, so the
    // concurrent caller must run WITH a runtime context: a plain OS thread
    // panics with "there is no reactor running". Two workers give real
    // parallelism for the race.
    let other = tokio::spawn(async move { env2.executor.start_task(env2.parent, req2) });
    let first = env.executor.start_task(env.parent, req.clone());
    let second = other.await.expect("claim task");

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

// ---------------------------------------------------------------- crash matrix
//
// Audit P1: inject a crash at every durable boundary of ONE keyed in-session
// start, reopen the data root as a NEW boot generation, run boot recovery
// and retry the SAME submission key. Every case must land as exactly one of
// replay / safe reclaim + single execution / typed conflict — never a
// perpetual in-flight refusal and never a second execution (the retry after
// an accepted fact must not reach the provider).

/// Boundary 1: the very first durable act (the claim) never committed. The
/// retry observes NO row and executes exactly once.
#[tokio::test]
async fn crash_before_claim_retries_as_one_fresh_execution() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let (env, req) = crash_and_reopen(&dir, "task_admission_claim_precommit", 0).await;
    assert_eq!(
        env.manager
            .store()
            .task_admission_pending_page(None, 10)
            .unwrap()
            .rows
            .len(),
        0,
        "a pre-commit crash leaves no pending row"
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("the retry executes the one admitted run");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    wait_turn_settled(&env, receipt.op_id.expect("in-session op id")).await;
    assert_eq!(
        message_count(&env),
        2,
        "exactly one user prompt + its one assistant reply"
    );
    assert!(env.provider.count() >= 1, "the one admitted run executed");
}

/// Boundary 2: the claim committed, no mutation followed. Boot recovery
/// reclaims it; the retry executes exactly once.
#[tokio::test]
async fn crash_after_claim_before_mutation_reclaims_and_retries_once() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let (env, req) = crash_and_reopen(&dir, "task_admission_claim_committed", 0).await;
    assert_eq!(
        env.manager
            .store()
            .task_admission_pending_page(None, 10)
            .unwrap()
            .rows
            .len(),
        0,
        "boot recovery must not leave the pending row behind"
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("a reclaimed key retries as a fresh single execution");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    wait_turn_settled(&env, receipt.op_id.expect("in-session op id")).await;
    assert_eq!(message_count(&env), 2, "exactly one accepted turn ran");
}

/// Boundary 3: the first durable mutation of the admission (the
/// `PromptReceived` journal append) committed before the crash. A bare
/// journal entry is NOT an accepted prompt (no turn record, no queue row,
/// no run row): recovery reclaims, the retry recovers the session and
/// executes exactly once.
#[tokio::test]
async fn crash_after_first_durable_mutation_reclaims_and_retries_once() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let (env, req) = crash_and_reopen(&dir, "ev_committed", 0).await;
    assert_eq!(
        env.manager
            .store()
            .task_admission_pending_page(None, 10)
            .unwrap()
            .rows
            .len(),
        0
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("the stranded journal entry does not block the retry");
    wait_turn_settled(&env, receipt.op_id.expect("in-session op id")).await;
    assert_eq!(
        message_count(&env),
        2,
        "exactly one accepted turn ran (the crash window materialized no prompt)"
    );
}

/// Boundary 4: the accepted prompt exists (the turn record is durable) but
/// the executor never wrote the run linkage or the receipt. Recovery must
/// rebuild the receipt from the turn facts and the retry must replay it
/// WITHOUT reaching the provider again.
#[tokio::test]
async fn crash_after_accepted_prompt_before_receipt_replays_from_facts() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("e");
    let env = open_env(&root, done_script());
    let req = keyed_crash_request(&env);
    let digest = task_start_digest(&req).unwrap();
    let now = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .now_ms();
    let op = env.manager.try_next_op_id().unwrap();
    let reservation = format!("tx-{:016x}", op.raw());
    assert!(env
        .manager
        .store()
        .task_admission_claim(env.parent, KEY_A, &digest, &reservation, now)
        .unwrap()
        .is_fresh());
    // The durable acceptance fact exactly as a crash between `submit` and
    // `put_run_row` leaves it: turn record + journal, no run linkage row.
    let submitted = env
        .agent
        .submit_with_op_id(env.parent, &req.goal, &req.files, Some(op))
        .unwrap();
    assert!(!submitted.queued);
    let expected = TaskRunReceipt {
        run_id: reservation,
        mode: TaskRunMode::InSession,
        op_id: Some(op),
        queued: false,
    };
    let parent = env.parent;
    drop(env);

    let env = reopen_env(&root, parent, done_script());
    let summary = recover_pending_task_admissions(&env.manager).unwrap();
    assert_eq!(summary.replayed, 1, "{summary:?}");
    let replayed = env
        .executor
        .start_task(env.parent, req)
        .expect("the recovered fact replays");
    assert_eq!(replayed, expected, "the receipt is rebuilt from the facts");
    assert_eq!(message_count(&env), 1);
    assert_eq!(
        env.provider.count(),
        0,
        "a replay after recovery must not execute a second run"
    );
}

/// Boundary 5: full durable acceptance (turn + run + policy rows) before the
/// receipt completion transaction. Boot recovery completes the receipt from
/// the facts; the retry replays byte-exactly and never reaches the provider.
#[tokio::test]
async fn crash_after_full_acceptance_before_receipt_completes_from_facts() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let (env, req) = crash_and_reopen(&dir, "task_admission_complete_precommit", 0).await;
    assert_eq!(
        env.manager
            .store()
            .task_admission_pending_page(None, 10)
            .unwrap()
            .rows
            .len(),
        0,
        "boot recovery must complete the accepted claim"
    );
    let replayed = env
        .executor
        .start_task(env.parent, req)
        .expect("the accepted receipt replays");
    assert_eq!(replayed.mode, TaskRunMode::InSession);
    assert!(replayed.run_id.starts_with("tx-"), "{replayed:?}");
    assert!(replayed.op_id.is_some(), "{replayed:?}");
    assert_eq!(message_count(&env), 1);
    assert_eq!(
        env.provider.count(),
        0,
        "the recovered accepted run is never executed twice"
    );
}

/// Boundary 6: the receipt completed but the owner died before returning it.
/// The retry replays the stored bytes without any recovery or execution.
#[tokio::test]
async fn crash_after_receipt_completion_before_response_replays_stored_bytes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let (env, req) = crash_and_reopen(&dir, "task_admission_complete_committed", 0).await;
    assert_eq!(
        env.manager
            .store()
            .task_admission_pending_page(None, 10)
            .unwrap()
            .rows
            .len(),
        0
    );
    let replayed = env
        .executor
        .start_task(env.parent, req)
        .expect("a completed claim replays even with a dead previous boot");
    assert_eq!(replayed.mode, TaskRunMode::InSession);
    assert_eq!(message_count(&env), 1);
    assert_eq!(
        env.provider.count(),
        0,
        "a completed receipt replay executes nothing"
    );
}

/// A legacy (pre-v30) pending row has no trustworthy reservation: boot
/// recovery lands the typed key-reuse conflict instead of guessing, and a
/// retry never executes a possibly-duplicated run.
#[tokio::test]
async fn legacy_pending_row_lands_typed_conflict_and_never_re_executes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("e");
    let env = open_env(&root, done_script());
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let digest = task_start_digest(&keyed_crash_request(&env)).unwrap();
    let now = handle.now_ms();
    {
        // Plant the pre-v30 row directly (the migration keeps such rows with
        // an empty generation, deadline 0 and a NULL reservation).
        let conn = rusqlite::Connection::open(root.join("store").join("faktor-plus.db")).unwrap();
        conn.execute(
            "INSERT INTO task_admission(
                key, session_id, request_digest, state, receipt_json, created_ms,
                owner_generation, lease_deadline_ms, reservation)
             VALUES (?1, ?2, ?3, 'pending', NULL, ?4, '', 0, NULL)",
            rusqlite::params![KEY_A, env.parent.raw() as i64, digest, now],
        )
        .unwrap();
    }
    let req = keyed_crash_request(&env);
    let parent = env.parent;
    drop(env);

    let env = reopen_env(&root, parent, done_script());
    let summary = recover_pending_task_admissions(&env.manager).unwrap();
    assert_eq!(summary.conflicts, 1, "{summary:?}");
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("an unlinkable legacy claim must refuse typed");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
    assert!(!err.to_string().contains("in flight"), "{err}");
    assert!(err.to_string().contains("different task start"), "{err}");
    assert_eq!(message_count(&env), 0, "no prompt was ever materialized");
    assert_eq!(env.provider.count(), 0);
}

/// P1-IDEMPOTENCY: within one generation, lease liveness is MONOTONIC. A wall
/// clock jumping forward cannot expire a live claim early, a wall clock
/// jumping backward cannot extend an abandoned one, and only the monotonic
/// admission clock passing the lease makes the row stale/recoverable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(crate) async fn wall_clock_jumps_never_move_the_admission_lease() {
    use faktor_core::time::{Clock, TestClock};
    use faktor_store::ADMISSION_LEASE_MS;

    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let wall = Arc::new(TestClock::new(1_000_000));
    let admission = Arc::new(faktor_session::TestAdmissionClock::new(10_000));
    let env = open_env_with_clocks(
        &dir.path().join("e"),
        done_script(),
        ShadowCopyLimits::default(),
        false,
        Some(wall.clone() as Arc<dyn Clock>),
        Some(admission.clone() as Arc<dyn faktor_session::AdmissionClock>),
    );
    let mut req = request(
        "lease vs wall clock",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    let digest = crate::runtime::task_executor::task_start_digest(&req).expect("digest");
    assert!(env
        .manager
        .store()
        .task_admission_claim(
            env.parent,
            KEY_A,
            &digest,
            "tx-00000000000000ba",
            env.manager.admission_now_ms()
        )
        .unwrap()
        .is_fresh());

    // Wall clock FORWARD far beyond the lease: the live claim stays live.
    wall.advance(10 * ADMISSION_LEASE_MS);
    let err = env
        .executor
        .start_task(env.parent, req.clone())
        .expect_err("a wall jump forward must not expire a live lease");
    assert!(err.to_string().contains("in flight"), "{err}");

    // Wall clock BACKWARD below the claim: equally irrelevant.
    wall.set(1);
    let err = env
        .executor
        .start_task(env.parent, req.clone())
        .expect_err("a wall jump backward must not extend or expire a lease");
    assert!(err.to_string().contains("in flight"), "{err}");

    // Only the MONOTONIC admission clock passing the deadline makes the row
    // stale: the retry recovers the abandoned claim and starts exactly once.
    admission.advance(ADMISSION_LEASE_MS + 1);
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("the monotonic lease expiring recovers the claim");
    wait_turn_settled(&env, receipt.op_id.expect("in-session op id")).await;
    assert_eq!(receipt.mode, TaskRunMode::InSession);
}

/// P1: an accepted task must never silently skip its drive. A post-admission
/// session-read failure returns a TYPED, caller-visible error while the
/// durable run stays recovery-required: zero provider calls happen, the
/// durable queue row exists for boot recovery, and an immediate same-key
/// retry refuses in-flight instead of executing a second run. (Boot
/// recovery driving exactly once is covered by the crash matrix above.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(crate) async fn post_admission_read_failure_is_typed_and_recovers_exactly_once() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    env.executor.fail_post_admission_read_for_test();
    let mut req = request(
        "post-admission read failure",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.submission_id = Some(KEY_A.to_string());
    let err = env
        .executor
        .start_task(env.parent, req.clone())
        .expect_err("a post-admission read failure must be caller-visible");
    assert!(matches!(err, ExecError::Internal(_)), "{err}");
    assert!(
        err.to_string().contains("recovery-required"),
        "the outcome must say the run is durably recorded and recovery-required: {err}"
    );
    assert_eq!(
        env.provider.count(),
        0,
        "no provider call happens for a run whose drive never spawned"
    );
    // The durable admission row itself is the recovery anchor: it exists
    // even though the drive never spawned.
    let digest = crate::runtime::task_executor::task_start_digest(&req).expect("digest");
    let claim = env
        .manager
        .store()
        .task_admission_peek(env.parent, KEY_A, &digest, env.manager.admission_now_ms())
        .unwrap();
    assert!(
        claim.is_some(),
        "the accepted run must have a durable admission row for recovery"
    );
    // The seam is one-shot; the retry replays the byte-identical durable
    // receipt (at-most-once) and still executes zero provider calls.
    let replay = env
        .executor
        .start_task(env.parent, req)
        .expect("the durable acceptance replays");
    assert!(replay.op_id.is_some(), "the replay carries the durable op");
    assert_eq!(env.provider.count(), 0, "still zero provider calls");
}

// ============================================ P0-ASK execution semantics

/// Build an ASK start: conversational execution kind, exactly one
/// read-only work item (the typed admission rule), no engineering seams.
fn ask_request(goal: &str, env: &Env) -> TaskRunRequest {
    let mut req = request(goal, vec![wi("a1", WorkKind::Analysis, &[])], env);
    req.execution = ExecutionKind::Ask;
    req
}

/// Wait for ONE turn: machine ready AND the op's durable turn record closed
/// (the two writes are separate; readiness alone races the row sync).
async fn wait_turn_done(env: &Env, op: OpId) {
    wait_until(
        || state_of(env, env.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        240,
    )
    .await;
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    wait_until(
        || match handle.turn_record(op) {
            Ok(Some(record)) => faktor_store::is_terminal_turn_record_status(&record.status),
            _ => false,
        },
        240,
    )
    .await;
}

/// The ZERO witness (audit P0-ASK): an ask leaves no engineering surface
/// behind — no task row, no completion-contract row, no shadow copy, no
/// verification record/attempt fact, no goal, and no working-ledger blob.
async fn assert_ask_left_no_engineering_rows(env: &Env) {
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    for id in 1..=5u64 {
        let task_id = TaskId::new(id);
        assert!(
            handle.get_task(task_id).unwrap().is_none(),
            "an ask must never create task row {id}"
        );
        assert!(
            handle.completion_contract(task_id).unwrap().is_none(),
            "an ask must never leave a completion contract row for {id}"
        );
        let records = env
            .manager
            .verification_records(env.parent, task_id)
            .await
            .unwrap();
        assert!(
            records.is_empty(),
            "an ask must leave zero verification records for {id}"
        );
    }
    assert!(
        env.manager.shadow_row(env.parent).unwrap().is_none(),
        "an ask must never open a shadow copy"
    );
    let attempts = crate::runtime::facts_of_kind(&handle, "verification_attempt").unwrap();
    assert!(
        attempts.is_empty(),
        "an ask must leave zero verification attempt facts: {attempts:?}"
    );
    let view = handle.ledger_view().unwrap();
    assert!(
        view.head.goal.is_empty(),
        "a conversational ask must never become a goal: {}",
        view.head.goal
    );
    assert_eq!(
        view.head.goal_task_id, None,
        "an ask never binds an engineering goal"
    );
    assert!(
        handle.get_task_ledger().unwrap().is_none(),
        "an ask never writes the engineering working-ledger blob"
    );
}

/// P0-ASK: a greeting and a conceptual question are CONVERSATIONAL turns —
/// zero task/contract/shadow/verification rows, no goal (a chat greeting
/// never becomes one), and the ask's durable identity rides the
/// turn-execution row with an unbound conversational policy row.
#[tokio::test]
async fn greeting_and_conceptual_asks_admit_with_zero_engineering_rows() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("ask"), done_script());

    // The task-scoped turn budget is a TYPED refusal on an ask, before any
    // durable mutation — never a silently dropped configured cap.
    let err = env
        .executor
        .start_task_with_turn_cap(env.parent, ask_request("hi", &env), Some(3))
        .expect_err("an ask carrying a task-scoped turn budget is refused typed");
    assert!(matches!(err, ExecError::InvalidPlan(_)), "{err}");
    assert_ask_left_no_engineering_rows(&env).await;

    // Greeting.
    let greet = env
        .executor
        .start_task(env.parent, ask_request("hello there", &env))
        .expect("greet ask admitted");
    assert_eq!(greet.mode, TaskRunMode::InSession);
    let greet_op = greet.op_id.expect("real op id");
    assert_ask_left_no_engineering_rows(&env).await;
    wait_turn_done(&env, greet_op).await;
    assert_ask_left_no_engineering_rows(&env).await;
    let greet_text = env
        .provider
        .text_of(0)
        .expect("the greeting request is captured");
    assert!(
        !greet_text.contains("GOAL:"),
        "a greeting must never seed a goal block: {greet_text}"
    );

    // Conceptual ask.
    let ask = env
        .executor
        .start_task(env.parent, ask_request("what is a lexer anyway", &env))
        .expect("ask admitted");
    let ask_op = ask.op_id.expect("real op id");
    wait_turn_done(&env, ask_op).await;
    assert_ask_left_no_engineering_rows(&env).await;

    // The durable identity of the ask: the turn-execution row (kind=ask,
    // empty attachment set, OP-scoped) plus ONE unbound conversational
    // policy row — no synthetic shadow-settlement row, no task binding.
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let row_value = handle
        .memory_fact_get(
            TURN_EXECUTION_ROW_KIND,
            &turn_execution_row_key(ask_op.raw()),
        )
        .unwrap()
        .expect("the ask's turn-execution row exists");
    let turn_execution = TurnExecution::decode(&row_value).expect("the row decodes typed");
    assert_eq!(turn_execution.execution, ExecutionKind::Ask);
    assert!(turn_execution.attachments.is_empty());
    let policy = handle
        .memory_fact_get(
            crate::runtime::task_executor::RUN_POLICY_ROW_KIND,
            &format!("tx-{:016x}", ask_op.raw()),
        )
        .unwrap()
        .expect("the ask's policy row exists");
    let policy_row: serde_json::Value = serde_json::from_str(&policy).expect("policy JSON");
    assert!(
        policy_row["task_id"].is_null(),
        "an ask run binds to no engineering task (absent or null — never a guessed id): {policy_row}"
    );
    assert_eq!(
        policy_row.get("no_op_disposition"),
        Some(&serde_json::json!("allowed")),
        "the conversational no-op policy is Allowed: {policy_row}"
    );
    let policy_rows =
        crate::runtime::facts_of_kind(&handle, crate::runtime::task_executor::RUN_POLICY_ROW_KIND)
            .unwrap();
    // Exactly ONE per-run conversational row per ask turn (greet + ask) and
    // NEVER the synthetic `tx-session-<id>` shadow-settlement row — that row
    // is the TASK-run identity surface and an ask must not clobber it.
    assert_eq!(
        policy_rows.len(),
        2,
        "exactly one policy row per ask turn: {policy_rows:?}"
    );
    for (key, value) in &policy_rows {
        assert!(
            !key.starts_with("tx-session-"),
            "an ask never writes the synthetic shadow-settlement row: {key}"
        );
        let row: serde_json::Value = serde_json::from_str(value).expect("policy JSON");
        assert!(
            row["task_id"].is_null(),
            "every ask policy row is task-unbound: {key} -> {row}"
        );
        assert_eq!(
            row.get("no_op_disposition"),
            Some(&serde_json::json!("allowed")),
            "every ask policy row is Allowed: {key} -> {row}"
        );
    }
}

/// P0-GOAL-BINDING acceptance sequence: greet; ask; task A; correct A;
/// finish A; ask about A; task B — B's PROVIDER REQUEST carries B's goal
/// and never A's goal block, continuations keep one objective, and the
/// cross-task rebuild is durable typed history.
#[tokio::test]
async fn goal_binding_sequence_b_task_request_carries_b_goal_and_never_as() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("binding"), done_script());
    let handle = |env: &Env| env.manager.get_session(env.parent).unwrap().unwrap();

    // greet (ask)
    let receipt = env
        .executor
        .start_task(env.parent, ask_request("hello there", &env))
        .expect("greet ask");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    // ask
    let receipt = env
        .executor
        .start_task(env.parent, ask_request("explain recursion briefly", &env))
        .expect("ask");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    assert!(
        handle(&env).get_task(TaskId::new(1)).unwrap().is_none(),
        "greet+ask before task A leave zero task rows"
    );

    // task A
    let goal_a = "restore the alpha parser so streaming reconnects";
    let before_a = env.provider.count();
    let receipt = env
        .executor
        .start_task(
            env.parent,
            request(goal_a, vec![wi("a1", WorkKind::Implementation, &[])], &env),
        )
        .expect("task A start");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    let request_a = env
        .provider
        .text_of(before_a)
        .expect("task A's request captured");
    assert!(
        request_a.contains("GOAL: restore the alpha parser"),
        "task A's own objective reaches the model: {request_a}"
    );
    let row_a_before = handle(&env)
        .get_task(TaskId::new(1))
        .unwrap()
        .expect("task A row");
    let head = handle(&env).ledger_view().unwrap();
    assert_eq!(
        head.head.goal_task_id,
        Some(1),
        "task A's projection is BOUND to task 1"
    );
    assert!(
        head.head.goal.contains("alpha"),
        "the bound head goal is A's objective: {}",
        head.head.goal
    );

    // correct A (steering on the same task)
    let receipt = env
        .executor
        .start_task(
            env.parent,
            request(
                "also double-check the reconnect backoff",
                vec![wi("a1", WorkKind::Implementation, &[])],
                &env,
            ),
        )
        .expect("steering start");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    let row_a_after = handle(&env)
        .get_task(TaskId::new(1))
        .unwrap()
        .expect("task A row survives steering");
    assert_eq!(
        row_a_after.task_id, row_a_before.task_id,
        "continuation keeps the same durable TaskId"
    );
    assert_eq!(
        row_a_after.goal, row_a_before.goal,
        "steering never replaces the recorded objective"
    );
    let head = handle(&env).ledger_view().unwrap();
    assert_eq!(
        head.head.goal_task_id,
        Some(1),
        "continuation keeps the binding"
    );
    assert!(
        head.head.goal.contains("alpha"),
        "the objective survived steering: {}",
        head.head.goal
    );

    // finish A (terminal)
    let revision = handle(&env).task_revision(TaskId::new(1)).unwrap();
    handle(&env)
        .transition_task(TaskId::new(1), revision, TaskTransition::Cancel, None)
        .expect("A finishes");

    // ask about A — zero NEW engineering rows; the ask never advances the
    // identity nor rebinds/rewrites the projection it merely READS.
    let receipt = env
        .executor
        .start_task(
            env.parent,
            ask_request("remind me what we restored for alpha", &env),
        )
        .expect("ask about A");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    assert!(
        handle(&env).get_task(TaskId::new(2)).unwrap().is_none(),
        "an ask never advances the task identity"
    );
    let head = handle(&env).ledger_view().unwrap();
    assert_eq!(
        head.head.goal_task_id,
        Some(1),
        "the ask never rebinds the goal projection"
    );
    assert!(
        head.head.goal.contains("alpha"),
        "the ask never rewrites the goal: {}",
        head.head.goal
    );

    // task B — a FRESH objective on a FRESH identity: B's provider request
    // carries B's goal block and NEVER A's (the conversation history
    // legitimately quotes A's prompt; the GOAL block is the projection).
    let goal_b = "build the beta indexer for nightly reports";
    let before_b = env.provider.count();
    let receipt = env
        .executor
        .start_task(
            env.parent,
            request(goal_b, vec![wi("b1", WorkKind::Implementation, &[])], &env),
        )
        .expect("task B start");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    let request_b = env
        .provider
        .text_of(before_b)
        .expect("task B's request captured");
    assert!(
        request_b.contains("GOAL: build the beta indexer"),
        "B's request carries B's goal: {request_b}"
    );
    assert!(
        !request_b.contains("GOAL: restore the alpha parser"),
        "B must NEVER inherit A's objective: {request_b}"
    );
    let row_b = handle(&env)
        .get_task(TaskId::new(2))
        .unwrap()
        .expect("task B row");
    assert_ne!(row_b.task_id, TaskId::new(1), "B opens its own identity");
    let head = handle(&env).ledger_view().unwrap();
    assert_eq!(
        head.head.goal_task_id,
        Some(2),
        "B's projection is bound to task 2"
    );
    assert!(
        head.head.goal.contains("beta"),
        "B's bound head goal is B's objective: {}",
        head.head.goal
    );
    // TYPED, not silent: the cross-task rebuild landed durable ledger
    // history — a Decision naming the mismatch.
    let mut rebuild_decisions = 0usize;
    let mut page = handle(&env).ledger_entries_page(None, 500).unwrap();
    loop {
        for entry in &page.entries {
            if let faktor_session::LedgerPayload::Decision { step, .. } = &entry.payload {
                if step == "goal binding" {
                    rebuild_decisions += 1;
                }
            }
        }
        if !page.has_more {
            break;
        }
        let cursor = page.entries.last().map(|entry| entry.seq);
        page = handle(&env).ledger_entries_page(cursor, 500).unwrap();
    }
    assert_eq!(
        rebuild_decisions, 1,
        "exactly one typed rebuild decision (task 1 -> task 2), never silent"
    );
}

/// The INVERSE witness of the ask zeros on the SAME surfaces: a task run
/// still opens its isolated candidate, creates the durable task row, binds
/// its run/policy rows to that identity, writes the working-ledger blob and
/// NEVER writes the ask's turn-execution row.
#[tokio::test]
async fn task_runs_keep_isolation_rows_and_task_bound_policy() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_shadow_env(&dir.path().join("task"), done_script());
    let receipt = env
        .executor
        .start_task(
            env.parent,
            mutating_request(&env, "implement the gamma widget"),
        )
        .expect("task start");
    let op = receipt.op_id.expect("real op id");
    wait_turn_done(&env, op).await;
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();

    let row = handle
        .get_task(TaskId::new(1))
        .unwrap()
        .expect("the task path creates its durable row");
    assert!(
        row.goal.contains("gamma"),
        "the row carries the task goal: {}",
        row.goal
    );
    assert!(
        env.manager.shadow_row(env.parent).unwrap().is_some(),
        "the task path still opens its isolated candidate (isolation unchanged)"
    );
    let run_value = handle
        .memory_fact_get(TASK_RUN_ROW_KIND, &receipt.run_id)
        .unwrap()
        .expect("the run linkage row exists");
    let run_row: TaskRunRow = serde_json::from_str(&run_value).expect("run row JSON");
    assert_eq!(
        run_row.task_id,
        Some(1),
        "the task run binds to its admitted identity"
    );
    let policy = handle
        .memory_fact_get(
            crate::runtime::task_executor::RUN_POLICY_ROW_KIND,
            &receipt.run_id,
        )
        .unwrap()
        .expect("the task run's policy row exists");
    let policy_row: serde_json::Value = serde_json::from_str(&policy).expect("policy JSON");
    assert_eq!(
        policy_row.get("task_id"),
        Some(&serde_json::json!(1)),
        "the task policy row carries the admitted identity: {policy_row}"
    );
    let turn_execution = handle
        .memory_fact_get(TURN_EXECUTION_ROW_KIND, &turn_execution_row_key(op.raw()))
        .unwrap();
    assert!(
        turn_execution.is_none(),
        "a task turn never writes the ask's turn-execution row"
    );
    assert!(
        handle.get_task_ledger().unwrap().is_some(),
        "the task turn persists its engineering working ledger"
    );
    let head = handle.ledger_view().unwrap();
    assert_eq!(
        head.head.goal_task_id,
        Some(1),
        "the task turn binds its goal projection to its identity"
    );
}

/// P0-ASK typed refusals (unit): every engineering seam an ask must never
/// carry is a TYPED InvalidPlan — never a silent drop, never a half-applied
/// task behavior. The default task run accepts the identical shapes.
#[test]
fn ask_validate_refuses_every_engineering_seam_typed() {
    let bare = || TaskRunRequest {
        goal: "chat".into(),
        work_items: vec![WorkItem::new("main", "chat", WorkKind::Analysis)],
        execution: ExecutionKind::Ask,
        ..Default::default()
    };
    bare().validate().expect("a bare ask is admissible");

    let mut multi = bare();
    multi.work_items.push(wi("b", WorkKind::Analysis, &[]));
    let err = multi.validate().expect_err("multi-item ask is refused");
    assert!(matches!(err, ExecError::InvalidPlan(_)), "{err}");
    assert!(
        err.to_string().contains("one conversational work item"),
        "{err}"
    );

    let mut implementation = bare();
    implementation.work_items[0].kind = WorkKind::Implementation;
    let err = implementation
        .validate()
        .expect_err("an implementation work item is refused for ask");
    assert!(err.to_string().contains("read-only kind"), "{err}");

    let mut criteria = bare();
    criteria.criteria = vec!["ship it".into()];
    let err = criteria.validate().expect_err("ask criteria are refused");
    assert!(err.to_string().contains("no acceptance criteria"), "{err}");

    let mut contract = bare();
    contract.completion_contract = Some(CompletionContract::default());
    let err = contract
        .validate()
        .expect_err("an ask completion contract is refused");
    assert!(err.to_string().contains("no completion contract"), "{err}");

    let mut tokens = bare();
    tokens.max_tokens = Some(100);
    let err = tokens.validate().expect_err("ask token caps are refused");
    assert!(err.to_string().contains("budget seam"), "{err}");

    let mut money = bare();
    money.max_cost_micro = Some(1);
    let err = money.validate().expect_err("ask money caps are refused");
    assert!(err.to_string().contains("budget seam"), "{err}");

    let mut gate = bare();
    gate.no_op_disposition = Some(NoOpDisposition::RequiresCriterionProof);
    let err = gate
        .validate()
        .expect_err("an ask can never carry a completion-gate disposition");
    assert!(err.to_string().contains("Allowed"), "{err}");

    // The ENGINEERING default accepts the same shape: no silent behavior
    // change in either direction.
    let task = TaskRunRequest {
        goal: "ship".into(),
        work_items: vec![WorkItem::new("main", "ship", WorkKind::Implementation)],
        execution: ExecutionKind::Task,
        ..Default::default()
    };
    task.validate().expect("the default task run is unchanged");
}

/// The submission-keyed admission digest separates the execution kind TYPED:
/// the SAME body flipped to `ask` is a different digest (KeyReused, never a
/// receipt replay of the other semantics), while a task body stays stable.
#[test]
fn task_start_digest_domain_separates_ask_from_task_typed() {
    let task = TaskRunRequest {
        goal: "ship".into(),
        work_items: vec![WorkItem::new("main", "ship", WorkKind::Analysis)],
        ..Default::default()
    };
    assert_eq!(task.execution, ExecutionKind::Task, "default is task");
    let task_digest = task_start_digest(&task).expect("task digest");
    assert_eq!(
        task_digest,
        task_start_digest(&task).expect("task digest again"),
        "a task digest is stable across recomputation"
    );
    let mut ask = task.clone();
    ask.execution = ExecutionKind::Ask;
    let ask_digest = task_start_digest(&ask).expect("ask digest");
    assert_ne!(
        task_digest, ask_digest,
        "the SAME body flipped to ask must be a different digest — typed KeyReused, never a replay of the other semantics"
    );
}
