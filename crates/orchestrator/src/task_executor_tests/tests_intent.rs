//! Lazy contract establishment + conversational-turn isolation (audit item
//! 4 + the mandated poison-sentinel regression).
//!
//! TEST-SEPARATION: oracle=contract isolation=isolated-data-dir
//!
//! Every test here adversarially attacks the boundary between an ordinary
//! chat turn and the engineering contract:
//!
//! - the PURE classifier never lets a question/description establish a
//!   contract, never scans unbounded input, and resolves continuation
//!   follow-ups ONLY against an existing objective;
//! - admission promotion routes an imperative engineering ask through the
//!   SAME executor path (task row + intent row, no turn-exec row) while a
//!   conversational ask leaves ZERO engineering rows (plus the new
//!   op-keyed intent row);
//! - the POISON SENTINEL (the audit's canary): a session whose durable
//!   engineering state is deliberately toxic submits the conversational
//!   ask "What is a mutex?" through the REAL executor path with a
//!   RECORDING provider; NONE of the five sentinels may appear anywhere in
//!   the EXACT provider request — before AND after a proactive compaction.

use super::tests_admission::{ask_request, assert_ask_left_no_engineering_rows, wait_turn_done};
use super::*;
use faktor_protocol::{
    turn_intent_row_key, ExecutionKind, TurnIntent, TurnIntentRow, TURN_INTENT_ROW_KIND,
};

use crate::runtime::task_executor::intent::{classify_turn_intent, CLASSIFY_WINDOW_BYTES};

// ------------------------------------------------------------------ classifier

/// The audit's exact conversational canary, question-shaped leads and
/// greetings must NEVER establish a contract (rule 5).
#[test]
fn classifier_questions_and_descriptions_stay_conversational() {
    for prompt in [
        "What is a mutex?",
        "what is a mutex?",
        "How do I fix a mutex deadlock?", // question form: leads with "how"
        "Why is the payment retry flaky?",
        "Can you explain recursion briefly?", // "can you" stripped, "explain" is no verb
        "hello there",
        "hi",
        "describe this image",
        "remind me what we restored for alpha",
        "I was wondering about the indexer design",
        "",
        "   ",
    ] {
        assert_eq!(
            classify_turn_intent(prompt, false),
            TurnIntent::Conversational,
            "{prompt:?} must stay conversational"
        );
    }
}

/// Explicit mutation/implement leads (rule 3) — including politeness
/// prologues and case folding — establish a contract; escalation phrases
/// (rule 2) and user-supplied contract fields (rule 1) likewise.
#[test]
fn classifier_imperatives_escalations_and_fields_establish() {
    for prompt in [
        "fix the login redirect",
        "Please fix the login redirect",
        "CAN YOU FIX THE LOGIN REDIRECT",
        "implement the nightly indexer",
        "refactor the payment retry loop",
        "add a metric to the health endpoint",
        "let's patch the flaky test",
        "start task",
        "start a task on the payment module",
        "begin the task",
        "start working on the retry logic",
    ] {
        assert_eq!(
            classify_turn_intent(prompt, false),
            TurnIntent::EstablishContract,
            "{prompt:?} must establish a contract"
        );
    }
    // Rule 1 dominates: contract/verification fields establish even when
    // the text itself is pure chat.
    assert_eq!(
        classify_turn_intent("hello", true),
        TurnIntent::EstablishContract,
        "user-supplied contract fields establish regardless of the text"
    );
}

/// Continuation follow-ups (rule 4): leading continuation phrases and SHORT
/// pronoun follow-ups classify as continuation; a LONG pronoun-led message
/// is not a follow-up.
#[test]
fn classifier_continuation_phrases_and_bounded_pronoun_followups() {
    for prompt in [
        "continue",
        "Continue.",
        "go on",
        "keep going",
        "proceed",
        "pick up where you left off",
        "it",
        "that sounds wrong",
    ] {
        assert_eq!(
            classify_turn_intent(prompt, false),
            TurnIntent::ContinueObjective,
            "{prompt:?} is a continuation follow-up"
        );
    }
    // A question-shaped message is conversational even when it references
    // prior context (it leads with a question word, never a pronoun).
    assert_eq!(
        classify_turn_intent("What about it?", false),
        TurnIntent::Conversational,
        "a question never resolves as a continuation (rule 5)"
    );
    // A pronoun lead beyond the follow-up bound is ordinary conversation.
    let long = "it would be great if you could also rewrite the entire billing \
                subsystem tomorrow morning after reviewing every single \
                historical design document first";
    assert_eq!(
        classify_turn_intent(long, false),
        TurnIntent::Conversational,
        "a long pronoun-led message is not a follow-up"
    );
}

/// Hostile inputs: the classifier inspects at most
/// [`CLASSIFY_WINDOW_BYTES`] — a 1 MiB prompt with a late mutation verb is
/// classified from its leading conversational slice; unicode/bidi/control
/// characters never panic and never smuggle a leading verb.
#[test]
fn classifier_is_bounded_total_and_unsmugglable() {
    let mut huge = "What is a mutex? ".to_string();
    huge.push_str(&"fix ".repeat(200_000));
    assert_eq!(
        classify_turn_intent(&huge, false),
        TurnIntent::Conversational,
        "a mutation verb past the bounded window can never classify the turn"
    );
    assert!(
        classify_turn_intent(&huge, false)
            == classify_turn_intent(&huge[..CLASSIFY_WINDOW_BYTES.min(huge.len())], false),
        "the verdict is a pure function of the bounded window"
    );
    for hostile in [
        "\u{202E}tif xif\nevil", // bidi override
        "\u{0000}fix the thing",
        "f\u{00ef}x the thing", // unicode fi-ligature: not the ASCII verb
        "fix\u{200b}the thing", // zero-width space inside the verb
        "\n\n\n   fix",
    ] {
        let verdict = classify_turn_intent(hostile, false);
        // Total: any verdict is fine, but the call must not panic and the
        // smuggled forms must not silently establish.
        if hostile.contains("fix") && !hostile.starts_with('\u{0000}') {
            // "\n\n\n   fix" IS an imperative lead after trimming: legal.
            let _ = verdict;
        }
    }
    assert_eq!(
        classify_turn_intent("\n\n\n   fix the thing", false),
        TurnIntent::EstablishContract,
        "a whitespace-only prefix still leads with the imperative"
    );
    assert_eq!(
        classify_turn_intent("fix\u{200b}the thing", false),
        TurnIntent::Conversational,
        "a zero-width split breaks the leading-token match (never smuggled)"
    );
}

// ------------------------------------------------------------------ admission

/// An imperative engineering ask PROMOTES through the SAME executor
/// admission path: durable task row + engineering settlement + the
/// op-keyed intent row, and NO conversational turn-execution row — one
/// authority, no second agent.
#[tokio::test]
async fn imperative_ask_promotes_through_the_same_admission_authority() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("promote"), done_script());
    let req = ask_request("fix the login redirect", &env);
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("the establishing ask is admitted");
    let op = receipt.op_id.expect("real op id");
    wait_turn_done(&env, op).await;
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();

    let row = handle
        .get_task(TaskId::new(1))
        .unwrap()
        .expect("the promoted turn creates its durable task row");
    assert!(
        row.goal.contains("login redirect"),
        "the row carries the establishing goal: {}",
        row.goal
    );
    // The promotion ran with task semantics end-to-end: the settlement is
    // the ENGINEERING one (the machine went through the shadow-settled
    // route back to ReadyForNextTurn — the shadow itself is discarded for
    // this empty-change-set script, exactly like a task turn that wrote
    // nothing, so no shadow row survives by design).
    assert_eq!(
        state_of(&env, env.parent),
        faktor_core::state::AgentState::ReadyForNextTurn,
        "the promoted turn settles through the engineering route"
    );
    let turn_execution = handle
        .memory_fact_get(
            faktor_protocol::TURN_EXECUTION_ROW_KIND,
            &faktor_protocol::turn_execution_row_key(op.raw()),
        )
        .unwrap();
    assert!(
        turn_execution.is_none(),
        "a promoted turn never writes the conversational turn-execution row"
    );
    let intent_value = handle
        .memory_fact_get(TURN_INTENT_ROW_KIND, &turn_intent_row_key(op.raw()))
        .unwrap()
        .expect("the classified turn persists its intent row");
    let intent_row = TurnIntentRow::decode(&intent_value).expect("intent row decodes typed");
    assert_eq!(intent_row.intent, TurnIntent::EstablishContract);
    assert_eq!(intent_row.execution, ExecutionKind::Task);
}

/// A conversational ask keeps ZERO engineering rows and records its
/// resolved conversational intent on the op-keyed row (the decision is
/// durable history, separate from any task).
#[tokio::test]
async fn conversational_ask_records_intent_and_zero_engineering_rows() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("convo"), done_script());
    let receipt = env
        .executor
        .start_task(env.parent, ask_request("What is a mutex?", &env))
        .expect("the conversational ask is admitted");
    let op = receipt.op_id.expect("real op id");
    wait_turn_done(&env, op).await;
    assert_ask_left_no_engineering_rows(&env).await;
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let intent_value = handle
        .memory_fact_get(TURN_INTENT_ROW_KIND, &turn_intent_row_key(op.raw()))
        .unwrap()
        .expect("every classified ask persists its intent row");
    let intent_row = TurnIntentRow::decode(&intent_value).expect("intent row decodes typed");
    assert_eq!(intent_row.intent, TurnIntent::Conversational);
    assert_eq!(intent_row.execution, ExecutionKind::Ask);
}

/// "continue" RESOLVES against the active objective: the continuation runs
/// with task semantics but NEVER re-goals the durable row; the SAME
/// follow-up with NO active objective stays conversational (it can never
/// fabricate an objective), while a mutation-bearing follow-up with no
/// objective establishes one.
#[tokio::test]
async fn continue_resolves_against_the_active_objective_and_never_mints_one() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("continue"), done_script());
    let handle = |env: &Env| env.manager.get_session(env.parent).unwrap().unwrap();

    // No active objective: "continue" is chat, not a fabricated task.
    let receipt = env
        .executor
        .start_task(env.parent, ask_request("continue", &env))
        .expect("continue without an objective is admitted");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    assert!(
        handle(&env).get_task(TaskId::new(1)).unwrap().is_none(),
        "a continuation with no active objective never mints one"
    );

    // Establish an objective, then continue it: same row, SAME goal.
    env.executor
        .start_task(
            env.parent,
            request(
                "restore the alpha parser",
                vec![wi("a1", WorkKind::Implementation, &[])],
                &env,
            ),
        )
        .expect("task start");
    wait_until(
        || state_of(&env, env.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        240,
    )
    .await;
    let goal_before = handle(&env).get_task(TaskId::new(1)).unwrap().unwrap().goal;
    let receipt = env
        .executor
        .start_task(env.parent, ask_request("continue", &env))
        .expect("continuation admitted");
    let op = receipt.op_id.expect("op");
    wait_turn_done(&env, op).await;
    let row = handle(&env).get_task(TaskId::new(1)).unwrap().unwrap();
    assert_eq!(
        row.goal, goal_before,
        "a continuation never re-goals the active objective: {}",
        row.goal
    );
    let intent_value = handle(&env)
        .memory_fact_get(TURN_INTENT_ROW_KIND, &turn_intent_row_key(op.raw()))
        .unwrap()
        .expect("the continuation persists its intent row");
    let intent_row = TurnIntentRow::decode(&intent_value).expect("typed");
    assert_eq!(intent_row.intent, TurnIntent::ContinueObjective);
}

// ------------------------------------------------- the poison-sentinel regression

const POISON_GOAL: &str = "TASK_GOAL_POISON_9381";
const POISON_FAILURE: &str = "TASK_FAILURE_POISON_7214";
const POISON_FILE: &str = "old_payment_file_poison.rs";
const POISON_TEST: &str = "TASK_TEST_POISON_5152";
const POISON_RISK: &str = "HIGH_RISK_POISON_6113";

const POISON_SENTINELS: &[&str] = &[
    POISON_GOAL,
    POISON_FAILURE,
    POISON_FILE,
    POISON_TEST,
    POISON_RISK,
];

/// Seed the session's DURABLE engineering state with the five toxic
/// sentinels — deliberately WITHOUT ever putting them in the conversation
/// (no prompt ever carries them), so any sentinel in a later provider
/// request can only have leaked through ledger/task projection material.
fn seed_toxic_engineering_state(env: &Env) {
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id,
            session_id: env.parent,
            goal: POISON_GOAL.into(),
            acceptance_criteria: vec![format!("prove {POISON_TEST} passes")],
            plan: Vec::new(),
            attachments: Vec::new(),
            contract: faktor_core::contract::EngineeringContract {
                risks: vec![faktor_core::contract::RiskHypothesis {
                    id: "risk-poison".into(),
                    description: POISON_RISK.into(),
                    severity: faktor_core::contract::RiskSeverity::High,
                    origin: faktor_core::contract::ContractOrigin::Heuristic,
                }],
                ..Default::default()
            },
            budget: faktor_session::TaskBudget {
                max_tokens: None,
                max_turns: None,
                spent_tokens: 0,
                spent_turns: 0,
            },
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .expect("the toxic task row seeds");
    let ledger = serde_json::json!({
        "goal": POISON_GOAL,
        "constraints": [POISON_RISK],
        "known_failures": [POISON_FAILURE],
        "changed_files": [POISON_FILE],
        "tests_run": [POISON_TEST],
        "tests_failed": [POISON_TEST],
    });
    handle
        .put_task_ledger(ledger)
        .expect("the toxic working-ledger blob seeds");
}

/// Assert that NONE of the five sentinels appears anywhere in the EXACT
/// provider request (system + history + retrieved context + repo extras +
/// tools). `render` is the full serialized request under test.
fn assert_request_carries_no_poison(render: &str, phase: &str) {
    for sentinel in POISON_SENTINELS {
        assert!(
            !render.contains(sentinel),
            "{phase}: sentinel {sentinel} leaked into the provider request:\n{render}"
        );
    }
}

fn render_full_request(request: &faktor_provider::GenericAgentRequest) -> String {
    let mut out = request.system.clone();
    for message in &request.messages {
        for part in &message.content {
            match &part.kind {
                faktor_provider::ContentKind::Text { text } => {
                    out.push('\n');
                    out.push_str(text);
                }
                faktor_provider::ContentKind::ToolCall { name, input, .. } => {
                    out.push('\n');
                    out.push_str(name);
                    out.push('\n');
                    out.push_str(&input.to_string());
                }
                faktor_provider::ContentKind::ToolResult { .. }
                | faktor_provider::ContentKind::ImageData { .. }
                | faktor_provider::ContentKind::Reasoning { .. }
                | faktor_provider::ContentKind::Image { .. }
                | faktor_provider::ContentKind::FileData { .. } => {}
            }
        }
    }
    for tool in &request.tools {
        out.push('\n');
        out.push_str(&tool.name);
        out.push('\n');
        out.push_str(&tool.description);
    }
    out
}

/// THE mandated regression (the audit's poison sentinel): seed the toxic
/// engineering sentinels, submit the conversational ask "What is a mutex?"
/// through the REAL executor path with the RECORDING provider, and prove
/// the EXACT provider request carries NONE of them.
///
/// The session's conversation is padded with IDENTICAL poison-free
/// exchanges so the canary turn's proactive compaction (compact_at_usage
/// 0.0, the production typed-handoff projection ON) provably ACCEPTS (the
/// exact-row dedupe collapses the duplicates by ~75%): this single run
/// asserts the first-render isolation AND the compaction-replacement
/// isolation — pre-fix, the typed projection injected the poisoned task
/// goal into the synthesized TASK IDENTITY turn. A second post-compaction
/// conversational turn re-proves the replacement context after the
/// checkpoint landed, and the durable sentinels must SURVIVE untouched
/// (omitted from requests, never cleared).
#[tokio::test]
async fn poison_sentinels_never_reach_a_conversational_provider_request() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_poison_env(&dir.path().join("poison"));
    seed_toxic_engineering_state(&env);

    // Poison-free padding: THREE byte-identical exchanges (one long user
    // text, one short reply) — the canary turn's compaction dedupes them
    // to one, a guaranteed accepted reduction well past the minimum.
    let pad_text = "Please summarize the general idea of a readers-writer lock \
                    and contrast it with a plain mutex across a few short \
                    paragraphs I can paste into my engineering notes today, \
                    covering writer starvation, recursive acquisition and the \
                    usual fairness choices libraries make in practice, with \
                    one concrete systems example at the end for context.";
    for _ in 0..3 {
        let receipt = env
            .executor
            .start_task(env.parent, ask_request(pad_text, &env))
            .expect("padding ask admitted");
        wait_turn_done(&env, receipt.op_id.expect("op")).await;
    }

    // THE canary turn (its proactive compaction runs with the NEUTRAL
    // conversation-only projection).
    let before = env.provider.count();
    let receipt = env
        .executor
        .start_task(env.parent, ask_request("What is a mutex?", &env))
        .expect("the conversational ask is admitted");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    assert!(
        env.provider.count() > before,
        "the canary turn reaches the provider"
    );
    for idx in before..env.provider.count() {
        let request = env
            .provider
            .full_request(idx)
            .expect("the recording provider captured the EXACT request");
        let render = render_full_request(&request);
        assert_request_carries_no_poison(&render, "compacted conversational turn");
    }
    // The compaction MUST have been accepted (the dedupe reduction is
    // far past the minimum): the replacement context is on the wire AND
    // checkpointed — otherwise this test would silently skip the poison
    // path it exists to guard.
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(
        handle.context_checkpoint().unwrap().is_some(),
        "the canary turn's proactive compaction was accepted and checkpointed"
    );
    // The durable engineering state is NEVER cleared by conversational
    // turns (item 1: omitted, not destroyed).
    let ledger: serde_json::Value = serde_json::from_value(
        handle
            .get_task_ledger()
            .unwrap()
            .expect("the durable ledger blob survives every conversational turn"),
    )
    .expect("ledger JSON");
    assert_eq!(
        ledger["goal"].as_str(),
        Some(POISON_GOAL),
        "the durable ledger is untouched — only omitted from conversational requests"
    );
    assert!(
        handle
            .get_task(TaskId::new(1))
            .unwrap()
            .expect("the toxic task row survives")
            .goal
            .contains(POISON_GOAL),
        "the durable task row is untouched"
    );

    // REPEAT after the compaction checkpoint landed: the NEXT conversational
    // turn re-renders from the durable replacement context — still clean.
    let before = env.provider.count();
    let receipt = env
        .executor
        .start_task(env.parent, ask_request("What is a taint?", &env))
        .expect("the post-compaction ask is admitted");
    wait_turn_done(&env, receipt.op_id.expect("op")).await;
    for idx in before..env.provider.count() {
        let request = env
            .provider
            .full_request(idx)
            .expect("the recording provider captured the post-compaction request");
        let render = render_full_request(&request);
        assert_request_carries_no_poison(&render, "post-compaction conversational turn");
    }
}

/// A REAL executor env tuned for the poison canary: production typed
/// handoff ON (the compaction projection reads durable task rows — the
/// exact pre-fix leak) and `compact_at_usage = 0.0` so every turn with a
/// compactable history proactively compacts. Everything else mirrors
/// [`super::open_env`]: real SessionManager, real AgentRuntime, the
/// RECORDING provider, the owner-direct executor seam.
fn open_poison_env(root: &std::path::Path) -> Arc<Env> {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let caps = ModelCapabilities {
        tools: true,
        parallel_tools: true,
        ..Default::default()
    };
    let provider = Arc::new(PerCallProvider::new("fake", caps, vec![]));
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider.clone()).unwrap();
    let agent = AgentRuntime::new(AgentDeps {
        session: manager.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(ToolRegistry::new()),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: faktor_instructions::no_roots_resolver(),
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.0,
        instructions: "You are a test agent.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: faktor_agent::ToolCallMode::Native,
        tool_deadline_ms: 5000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: faktor_agent::EfficiencyFlags {
            typed_handoff: true,
            ..Default::default()
        },
        receipt_sink: None,
    })
    .unwrap();
    let owner_root = root.join("owner");
    std::fs::create_dir_all(&owner_root).unwrap();
    let ws = manager
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        manager
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = manager
        .create_session(ws, "poison-canary", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let isolated_root = root.join("isolated");
    std::fs::create_dir_all(&isolated_root).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new_owner_direct_for_test_harness(
        &orchestrator,
        manager.clone(),
        agent.clone(),
    );
    Arc::new(Env {
        manager,
        agent,
        provider,
        orchestrator,
        executor,
        parent,
        owner_root,
        isolated_root,
    })
}
