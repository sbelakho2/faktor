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
