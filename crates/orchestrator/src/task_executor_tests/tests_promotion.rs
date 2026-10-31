//! Deferred promotion signal — the ONE deterministic mid-turn escalation
//! seam (the shipped deliverable-4 safe alternative to an in-turn
//! authority flip).
//!
//! TEST-SEPARATION: oracle=contract isolation=isolated-data-dir
//!
//! Every test here attacks the seam where a conversational turn
//! discovers it needs engineering (the classifier can miss: non-English,
//! unusual phrasing):
//!
//! - the deterministic non-English prompt is admitted CONVERSATIONAL, its
//!   model calls the reserved `promotion_request` signal, and the NEXT
//!   submission enters the SAME `TaskExecutor` admission authority
//!   (shadow + contract + task row + EstablishContract intent row with the
//!   armed objective) — with NO second agent/process, NO duplicate turn
//!   and NO duplicate provider charge;
//! - the marker is consumed EXACTLY once (CAS) at an accepted promoting
//!   intent-row persistence: a refusal later in the admission keeps it
//!   armed, and a later conversational turn can never re-promote;
//! - a corrupt marker never bricks admission (loud skip, deterministic
//!   classifier stays the fallback authority);
//! - a promoted admission records its completion-contract row exactly like
//!   any other establishing task start.

use super::tests_admission::{ask_request, wait_turn_done};
use super::*;
use faktor_core::id::OpId;
use faktor_protocol::{
    turn_intent_row_key, ExecutionKind, TurnIntent, TurnIntentRow, TurnPromotionRow,
    PROMOTION_PENDING_ROW_KEY, PROMOTION_PENDING_ROW_KIND, PROMOTION_REQUEST_TOOL,
    TURN_EXECUTION_ROW_KIND, TURN_INTENT_ROW_KIND,
};

use crate::runtime::facts_of_kind;

/// The armed pending-promotion marker of a session, if any.
fn armed_marker(handle: &faktor_session::SessionHandle) -> Option<TurnPromotionRow> {
    handle
        .memory_fact_get(PROMOTION_PENDING_ROW_KIND, PROMOTION_PENDING_ROW_KEY)
        .unwrap()
        .map(|value| TurnPromotionRow::decode(&value).unwrap())
}

/// The op-keyed intent row of one classified turn (panics when absent —
/// every ask-classified admission persists one).
fn intent_row(handle: &faktor_session::SessionHandle, op: OpId) -> TurnIntentRow {
    let value = handle
        .memory_fact_get(TURN_INTENT_ROW_KIND, &turn_intent_row_key(op.raw()))
        .unwrap()
        .expect("every classified turn persists its intent row");
    TurnIntentRow::decode(&value).expect("intent row decodes typed")
}

/// No second agent, no orphan: ZERO subagent lifecycle events may exist on
/// the session at any point of these tests.
fn assert_no_second_agent(handle: &faktor_session::SessionHandle) {
    let events = handle.events_range(1, None).unwrap();
    assert!(
        !events.iter().any(|e| matches!(
            e.kind,
            faktor_core::event::EventKind::SubagentStarted
                | faktor_core::event::EventKind::SubagentCompleted
        )),
        "the promotion seam must never spawn a second agent/process"
    );
}

/// The zero-witness of a TOOL-CARRYING conversational turn: same legs as
/// `assert_ask_left_no_engineering_rows` (no task row, no
/// completion-contract row, no shadow, zero verification records/attempts,
/// no goal binding) EXCEPT the working-ledger-blob leg — a conversational
/// turn that merely REACHED `run_tool_calls` (answered the promotion
/// signal, denied a tool, …) persists the working ledger blob by
/// pre-existing ask/task-agnostic design; the goal legs below still pin
/// that blob to zero engineering content.
async fn assert_tool_carrying_ask_left_no_engineering_rows(env: &Env) {
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
}

/// THE end-to-end seam: a deterministic NON-ENGLISH prompt (the
/// classifier's documented false-negative → Conversational) turns into an
/// engineering establishment on the NEXT submission through the SAME
/// executor authority — with no second agent, no duplicate turn and
/// exactly one provider charge per turn.
#[tokio::test]
async fn non_english_ask_promotes_on_the_next_submission_with_one_authority_and_no_duplicate_charge(
) {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        // Turn 1: the conversational model calls the promotion signal.
        vec![
            ScriptedResponse::ToolCall {
                id: "promo".into(),
                name: PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "fix the payment retry parser"}),
            },
            ScriptedResponse::End,
        ],
        // Turn 2: the promoted engineering turn answers in one call.
        vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
    ];
    let env = open_shadow_env(&dir.path().join("non-english"), scripts);

    // ---- Turn 1: deterministic non-English prompt → CONVERSATIONAL ask.
    let receipt1 = env
        .executor
        .start_task(
            env.parent,
            ask_request("répare le parser de paiement, s'il vous plaît", &env),
        )
        .expect("the non-English ask is admitted");
    let op1 = receipt1.op_id.expect("real op id");
    wait_turn_done(&env, op1).await;

    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let row1 = intent_row(&handle, op1);
    assert_eq!(
        row1.intent,
        TurnIntent::Conversational,
        "the deterministic classifier keeps its documented false-negative posture"
    );
    assert_eq!(row1.execution, ExecutionKind::Ask);
    // … the ask left ZERO engineering rows (including no shadow) …
    assert_tool_carrying_ask_left_no_engineering_rows(&env).await;
    // … its signal armed the durable marker with the bounded objective …
    let marker = armed_marker(&handle).expect("the call arms the pending marker");
    assert_eq!(marker.objective, "fix the payment retry parser");
    assert_eq!(marker.turn_op, op1.raw());
    // … turn 1 was charged EXACTLY one provider call (the intercepted
    // signal never re-drives the turn) …
    assert_eq!(
        env.provider.count(),
        1,
        "turn 1 costs exactly its own provider call"
    );
    // … exactly one durable run row exists (turn 1's own admission) …
    let runs1 = facts_of_kind(&handle, TASK_RUN_ROW_KIND).unwrap();
    assert_eq!(runs1.len(), 1, "one run row per admitted turn: {runs1:?}");
    assert_no_second_agent(&handle);
    assert_tool_carrying_ask_left_no_engineering_rows(&env).await;

    // ---- Turn 2: the NEXT submission is pre-marked EstablishContract.
    let receipt2 = env
        .executor
        .start_task(env.parent, ask_request("ok, vamos", &env))
        .expect("the pre-marked submission is admitted");
    let op2 = receipt2.op_id.expect("real op id");
    wait_turn_done(&env, op2).await;

    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    // SAME authority: task row + EstablishContract/Task intent row
    // carrying the ARMED objective, turn-exec row absent (promoted) …
    let task = handle
        .get_task(TaskId::new(1))
        .unwrap()
        .expect("the promoted admission establishes the task row");
    assert!(
        task.goal.contains("ok, vamos"),
        "the submission's own text stays the goal: {}",
        task.goal
    );
    let row2 = intent_row(&handle, op2);
    assert_eq!(row2.intent, TurnIntent::EstablishContract);
    assert_eq!(row2.execution, ExecutionKind::Task);
    assert_eq!(
        row2.promotion_objective.as_deref(),
        Some("fix the payment retry parser"),
        "the persisted verdict carries the signal's objective"
    );
    let turn_execution = handle
        .memory_fact_get(
            TURN_EXECUTION_ROW_KIND,
            &faktor_protocol::turn_execution_row_key(op2.raw()),
        )
        .unwrap();
    assert!(
        turn_execution.is_none(),
        "a promoted turn never writes the conversational turn-execution row"
    );
    // … the SHADOW row exists (ownership NoWrites → IsolatedWorktree took
    // effect: a mutating Implementation item on a task run shadows) …
    assert!(
        env.manager.shadow_row(env.parent).unwrap().is_some(),
        "the promoted admission opens the engineering shadow copy"
    );
    // … the marker was consumed EXACTLY once …
    assert!(
        armed_marker(&handle).is_none(),
        "the promotion marker is consumed exactly once"
    );
    // … NO duplicate turn/charge: two runs (one per submission), TWO
    // provider calls total (one per turn — turn 1 was never replayed) …
    assert_eq!(
        env.provider.count(),
        2,
        "one provider call per turn: no turn is charged twice"
    );
    let runs2 = facts_of_kind(&handle, TASK_RUN_ROW_KIND).unwrap();
    assert_eq!(
        runs2.len(),
        2,
        "exactly one run row per submission, never a duplicate: {runs2:?}"
    );
    // … and still no second agent anywhere.
    assert_no_second_agent(&handle);
}

/// A promoted admission records its COMPLETION-CONTRACT row exactly like
/// any other establishing task start (record-first at admission), and the
/// armed objective rides the persisted intent row while the marker is
/// consumed exactly once.
#[tokio::test]
async fn promoted_admission_records_the_completion_contract_row_like_any_task_start() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            ScriptedResponse::ToolCall {
                id: "promo".into(),
                name: PROMOTION_REQUEST_TOOL.into(),
                input: serde_json::json!({"objective": "ship the retry parser fix"}),
            },
            ScriptedResponse::End,
        ],
        vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
    ];
    let env = open_env(&dir.path().join("contract"), scripts);

    // Arm the promotion on a conversational turn.
    let receipt1 = env
        .executor
        .start_task(env.parent, ask_request("hm", &env))
        .expect("ask admitted");
    let op1 = receipt1.op_id.expect("op");
    wait_turn_done(&env, op1).await;
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(armed_marker(&handle).is_some(), "armed by turn 1");

    // The next submission carries an explicit completion contract (rule 1
    // would establish anyway — the marker consumption + objective attachment
    // are what this pins, together with the record-first contract row).
    let mut req2 = ask_request("ok, vamos", &env);
    req2.completion_contract = Some(faktor_core::completion::CompletionContract {
        include_commit: true,
        include_push: false,
        include_pr: false,
    });
    let receipt2 = env
        .executor
        .start_task(env.parent, req2)
        .expect("the pre-marked submission is admitted");
    let op2 = receipt2.op_id.expect("op");
    wait_turn_done(&env, op2).await;

    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(
        handle
            .completion_contract(TaskId::new(1))
            .unwrap()
            .is_some(),
        "a promoted admission records its completion-contract row"
    );
    let row2 = intent_row(&handle, op2);
    assert_eq!(row2.intent, TurnIntent::EstablishContract);
    assert_eq!(
        row2.promotion_objective.as_deref(),
        Some("ship the retry parser fix")
    );
    assert!(
        armed_marker(&handle).is_none(),
        "consumed exactly once at the persisted verdict"
    );
    assert_eq!(env.provider.count(), 2, "one call per turn");
    assert_no_second_agent(&handle);
}

/// Exactly-once consumption against admission FAILURE: a refusal later in
/// the same admission never eats the armed promotion, the next valid
/// submission still establishes, and the one AFTER it is conversational
/// again (the marker can never re-promote twice).
#[tokio::test]
async fn armed_marker_survives_a_refused_admission_and_is_consumed_exactly_once() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![
        vec![
            ScriptedResponse::Text("answer".into()),
            ScriptedResponse::End,
        ],
        vec![
            ScriptedResponse::Text("reply".into()),
            ScriptedResponse::End,
        ],
    ];
    let env = open_env(&dir.path().join("exactly-once"), scripts);
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let prior = TurnPromotionRow::new("fix the parser", 999, 1).unwrap();
    handle
        .upsert_memory_fact(
            PROMOTION_PENDING_ROW_KIND,
            PROMOTION_PENDING_ROW_KEY,
            &prior.encode().unwrap(),
        )
        .unwrap();

    // A refused admission (empty goal fails validation AFTER the
    // classifier consult) must leave the armed marker byte-identical.
    let err = env
        .executor
        .start_task(env.parent, ask_request("", &env))
        .expect_err("an empty goal is a typed refusal");
    assert!(
        matches!(err, ExecError::InvalidPlan(_)),
        "the refusal stays typed: {err:?}"
    );
    assert_eq!(
        armed_marker(&handle).expect("the refusal never eats the promotion"),
        prior,
        "a refused admission must leave the marker untouched"
    );

    // The next valid submission is pre-marked and establishes …
    let receipt1 = env
        .executor
        .start_task(env.parent, ask_request("What is a mutex?", &env))
        .expect("the pre-marked submission is admitted");
    let op1 = receipt1.op_id.expect("op");
    wait_turn_done(&env, op1).await;
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let row1 = intent_row(&handle, op1);
    assert_eq!(row1.intent, TurnIntent::EstablishContract);
    assert_eq!(
        row1.promotion_objective.as_deref(),
        Some("fix the parser"),
        "the consumed marker's objective is the verdict's audit field"
    );
    assert!(handle.get_task(TaskId::new(1)).unwrap().is_some());
    assert!(armed_marker(&handle).is_none(), "consumed exactly once");
    assert_eq!(env.provider.count(), 1);

    // … and the following conversational turn can NEVER re-promote: the
    // marker is gone, the verdict is conversational, the active goal is
    // untouched.
    let receipt2 = env
        .executor
        .start_task(env.parent, ask_request("thanks, that helps", &env))
        .expect("the follow-up is admitted");
    let op2 = receipt2.op_id.expect("op");
    wait_turn_done(&env, op2).await;
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let row2 = intent_row(&handle, op2);
    assert_eq!(
        row2.intent,
        TurnIntent::Conversational,
        "an exhausted promotion must never promote twice"
    );
    assert!(row2.promotion_objective.is_none());
    let task = handle.get_task(TaskId::new(1)).unwrap().unwrap();
    assert!(
        task.goal.contains("What is a mutex?"),
        "a conversational turn never re-goals: {}",
        task.goal
    );
    assert_eq!(env.provider.count(), 2, "one call per turn");
}

/// A CORRUPT pending marker never bricks an ask admission: the upgrade is
/// skipped LOUDLY (the deterministic classifier stays the fallback
/// authority), the conversational turn drives normally, and the corrupt
/// row is left for the next valid arm to heal.
#[tokio::test]
async fn corrupt_pending_marker_never_blocks_ask_admission() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts = vec![vec![
        ScriptedResponse::Text("answer".into()),
        ScriptedResponse::End,
    ]];
    let env = open_env(&dir.path().join("corrupt-marker"), scripts);
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    handle
        .upsert_memory_fact(
            PROMOTION_PENDING_ROW_KIND,
            PROMOTION_PENDING_ROW_KEY,
            "{not json",
        )
        .unwrap();

    let receipt = env
        .executor
        .start_task(env.parent, ask_request("What is a mutex?", &env))
        .expect("a corrupt advisory marker must never refuse admission");
    let op = receipt.op_id.expect("op");
    wait_turn_done(&env, op).await;

    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let row = intent_row(&handle, op);
    assert_eq!(
        row.intent,
        TurnIntent::Conversational,
        "a corrupt marker degrades to the deterministic verdict, loudly"
    );
    assert_eq!(
        handle
            .memory_fact_get(PROMOTION_PENDING_ROW_KIND, PROMOTION_PENDING_ROW_KEY)
            .unwrap()
            .as_deref(),
        Some("{not json"),
        "the corrupt row is retained (postmortem/next-arm heal), never silently deleted"
    );
    assert!(
        handle.get_task(TaskId::new(1)).unwrap().is_none(),
        "no task row may appear for the degraded conversational turn"
    );
    assert_eq!(
        env.provider.count(),
        1,
        "the turn still drives exactly once"
    );
}

/// The upgrade only ever fires for the ORDINARY conversational shape: a
/// MULTI-ITEM start with an armed marker is not upgraded by the marker
/// (the single-item gate) and the typed ask rule refuses the multi-item
/// conversational plan — the marker stays armed for a later valid
/// submission.
#[tokio::test]
async fn multi_item_admission_is_never_marker_upgraded() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("multi-item"), vec![]);
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let prior = TurnPromotionRow::new("fix the parser", 999, 1).unwrap();
    handle
        .upsert_memory_fact(
            PROMOTION_PENDING_ROW_KIND,
            PROMOTION_PENDING_ROW_KEY,
            &prior.encode().unwrap(),
        )
        .unwrap();

    let mut req = request(
        "What is a mutex?",
        vec![
            wi("a1", WorkKind::Analysis, &[]),
            wi("a2", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    req.execution = ExecutionKind::Ask;
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("an ask is exactly one conversational work item");
    assert!(
        matches!(err, ExecError::InvalidPlan(_)),
        "the refusal stays typed: {err:?}"
    );
    assert_eq!(
        armed_marker(&handle).expect("still armed"),
        prior,
        "a multi-item refusal never consumes or upgrades the marker"
    );
}
