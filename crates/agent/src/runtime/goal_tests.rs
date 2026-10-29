//! `runtime::goal_tests`: durable redacted goal projection (P1), plus the
//! P0-ASK turn-execution row and the P0-GOAL-BINDING projection
//! classification/rebuild (adversarial: corrupt rows, foreign bindings,
//! rewound revisions, secret-only objectives).
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use super::*;
use crate::runtime::tests::*;
use crate::*;
use base64::Engine as _;
use faktor_context::ledger::GOAL_REDACTED_SECRET;

fn sk() -> String {
    format!("sk-{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6")
}

fn ghp() -> String {
    "ghp_0123456789abcdefghijklmnopqrstuv".to_string()
}

fn ledger_rows_json(handle: &faktor_session::SessionHandle) -> String {
    let mut out = String::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = handle.ledger_entries_page(cursor, 500).unwrap();
        for entry in &page.entries {
            out.push_str(&serde_json::to_string(&entry.payload).unwrap());
            out.push('\n');
        }
        if !page.has_more {
            break;
        }
        cursor = page.entries.last().map(|entry| entry.seq);
        if cursor.is_none() {
            break;
        }
    }
    out
}

fn session_in_zephyr_workspace(deps: &AgentDeps, dir: &tempfile::TempDir) -> SessionId {
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"zephyr\"\n").unwrap();
    std::fs::write(
        root.join("src/zephyr.rs"),
        "pub fn evict(cache: &mut Vec<u64>, bound: usize) {\n    while cache.len() > bound {\n        cache.remove(0);\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn eviction_policy_keeps_the_cache_bounded() {\n        let mut cache = vec![1, 2, 3];\n        evict(&mut cache, 2);\n        assert_eq!(cache.len(), 2);\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/zephyr_cache.rs"),
        "#[test]\nfn cache_eviction_regression() {}\n",
    )
    .unwrap();
    let ws = deps
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    deps.session
        .create_session(ws, "task", "fake", "m")
        .unwrap()
        .id()
}

#[test]
fn redaction_preserves_objective_and_removes_multi_and_encoded_secrets() {
    let sk = sk();
    let ghp = ghp();
    let pem_body = "MIIEowIBAAKCAQEAprivateKeyBodyNeverSurvives".to_string();
    let pem = format!("-----BEGIN RSA PRIVATE KEY-----\n{pem_body}\n-----END RSA PRIVATE KEY-----");
    let b64 = base64::engine::general_purpose::STANDARD.encode(sk.as_bytes());
    let pct = format!("sk%2D{}", "A".repeat(24));
    let hex = sk
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let prompt = format!(
        "Implement the quux subsystem and add regression tests.\ncredential {sk}\nbackup {ghp}\n{pem}\nencoded {b64} {pct} {hex}"
    );

    let redacted = redact_prompt_text(&prompt);
    for secret in [&sk, &ghp, &pem_body, &b64, &pct, &hex] {
        assert!(
            !redacted.contains(secret),
            "secret {secret:?} survived redaction: {redacted:?}"
        );
    }
    assert!(redacted.contains("Implement the quux subsystem"));
    assert!(redacted.contains("add regression tests"));
    assert!(redacted.contains(GOAL_REDACTED_SECRET));
    assert!(!contains_secret(&redacted), "{redacted:?}");
    assert_eq!(
        redact_prompt_text(&redacted),
        redacted,
        "redaction must be idempotent"
    );
}

#[test]
fn line_wrapped_encoded_secrets_are_redacted_and_the_objective_survives() {
    let sk = sk();
    let b64 = base64::engine::general_purpose::STANDARD.encode(sk.as_bytes());
    let wrapped_b64 = b64
        .as_bytes()
        .chunks(16)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let hex = sk
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let wrapped_hex = hex
        .as_bytes()
        .chunks(32)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join("\r\n");
    let prompt = format!(
        "Implement the quux subsystem and add regression tests.\n{wrapped_b64}\nKeep the public API stable.\n{wrapped_hex}"
    );

    let redacted = redact_prompt_text(&prompt);
    for chunk in b64.as_bytes().chunks(16) {
        let chunk = std::str::from_utf8(chunk).unwrap();
        assert!(
            !redacted.contains(chunk),
            "wrapped base64 chunk {chunk:?} survived: {redacted:?}"
        );
    }
    for chunk in hex.as_bytes().chunks(32) {
        let chunk = std::str::from_utf8(chunk).unwrap();
        assert!(
            !redacted.contains(chunk),
            "wrapped hex chunk {chunk:?} survived: {redacted:?}"
        );
    }
    assert!(redacted.contains("Implement the quux subsystem"));
    assert!(redacted.contains("Keep the public API stable"));
    assert!(!contains_secret(&redacted), "{redacted:?}");
    assert_eq!(redact_prompt_text(&redacted), redacted, "idempotent");
    assert!(
        contains_secret(&prompt),
        "the wrapped form is still a secret: {prompt:?}"
    );
}

#[test]
fn operational_goal_stays_bounded_while_the_full_redacted_objective_survives() {
    let objective = "Implement the zephyr cache eviction policy and add regression tests";
    let mut prompt = String::new();
    for _ in 0..12 {
        prompt.push_str(&format!("credential {}\n", sk()));
    }
    prompt.push_str(objective);
    let projection = match project_prompt_goal(7, &prompt) {
        GoalProjectionOutcome::Available(projection) => projection,
        other => panic!("expected an available projection, got {other:?}"),
    };
    assert!(projection.redacted_text.contains(objective));
    assert!(
        projection.redacted_text.len() > MAX_GOAL_OPERATIONAL_BYTES,
        "the full projection must keep the whole prompt"
    );
    let mut ledger = TaskLedger::default();
    apply_projection(&mut ledger, &projection);
    assert!(
        ledger.goal.len() <= MAX_GOAL_OPERATIONAL_BYTES,
        "the operational copy is bounded"
    );
    assert!(ledger.goal.contains("zephyr"), "{}", ledger.goal);
    assert!(!contains_secret(&ledger.goal), "{}", ledger.goal);
    assert_eq!(
        ledger.goal_redacted_text.as_deref(),
        Some(projection.redacted_text.as_str())
    );
}

#[test]
fn legacy_goal_with_a_secret_is_scrubbed_and_keeps_the_objective() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let secret = ghp();
    let legacy = format!("Implement the quux subsystem and add regression tests. token {secret}");
    handle
        .put_task_ledger(serde_json::json!({ "goal": legacy }))
        .unwrap();
    handle.submit_prompt("fix the quux subsystem", &[]).unwrap();
    let mut ledger: faktor_context::TaskLedger =
        serde_json::from_value(handle.get_task_ledger().unwrap().unwrap()).unwrap();
    initialize_durable_goal(&handle, &mut ledger).unwrap();
    assert!(ledger.goal.contains("quux"));
    assert!(!ledger.goal.contains(&secret));
    assert!(!contains_secret(&ledger.goal));
    assert_eq!(
        ledger.goal_projection_state,
        Some(faktor_context::ledger::GoalProjectionState::Available)
    );
    let persisted = handle.get_task_ledger().unwrap().unwrap().to_string();
    assert!(!persisted.contains(&secret), "{persisted}");
}

#[test]
fn redaction_empty_prompt_is_a_typed_unavailable_state() {
    let secret_only = format!("here is my key: {}", ghp());
    match project_prompt_goal(9, &secret_only) {
        GoalProjectionOutcome::UnavailableAfterRedaction {
            source_message_id,
            redacted_text,
            digest,
        } => {
            assert_eq!(source_message_id, 9);
            assert!(digest.starts_with("blake3:"));
            assert!(!redacted_text.contains(&ghp()));
            assert!(redacted_text.contains(GOAL_REDACTED_SECRET));
        }
        other => panic!("expected the typed unavailable state, got {other:?}"),
    }
    let error = Error::goal_unavailable_after_redaction(9);
    assert_eq!(
        error.kind,
        ErrorKind::GoalUnavailableAfterRedaction {
            source_message_id: 9
        }
    );
    assert!(error.message.contains("goal_unavailable_after_redaction"));
}

#[tokio::test]
async fn secret_prompt_persists_redacted_goal_contract_prompt_and_ledger() {
    let secret = ghp();
    let objective = "Implement the zephyr cache eviction policy and add regression tests";
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let inspected: Arc<dyn faktor_provider::Provider> = Arc::new(InspectingProvider::new(
        Arc::new(scripted_provider(vec![
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
        move |_n, request| {
            cap.lock().unwrap().push(request.system.clone());
            Ok(())
        },
    ));
    let (deps, _dir) = deps_with(inspected, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let title = handle.title().unwrap();
    assert_eq!(title, "test session");

    let prompt = format!("{objective}\nUse this deploy token: {secret}");
    runtime.run_turn(session, &prompt, &[]).await.unwrap();

    let system = captured.lock().unwrap().clone();
    assert_eq!(system.len(), 1, "one text turn");
    assert!(system[0].contains("zephyr"));
    assert!(system[0].contains("eviction"));
    assert!(system[0].contains(GOAL_REDACTED_SECRET));
    assert!(
        !system[0].contains(&secret),
        "the prompt prefix must never carry the credential: {}",
        system[0]
    );

    let ledger: faktor_context::TaskLedger =
        serde_json::from_value(handle.get_task_ledger().unwrap().unwrap()).unwrap();
    let ledger_value = serde_json::to_value(&ledger).unwrap();
    let ledger_text = ledger_value.to_string();
    assert!(!ledger_text.contains(&secret), "{ledger_text}");
    assert!(ledger.goal.contains("zephyr"));
    assert!(ledger.goal.contains("eviction"));
    assert!(!ledger.goal.contains(&secret));
    assert_eq!(
        ledger.goal_projection_state,
        Some(faktor_context::ledger::GoalProjectionState::Available)
    );
    let source_message_id = ledger.goal_source_message_id.unwrap();
    let user_id = handle
        .messages_before(None, 8)
        .unwrap()
        .into_iter()
        .find(|row| row.role == "user")
        .unwrap()
        .id;
    assert_eq!(source_message_id, user_id);
    let digest = ledger.goal_projection_digest.clone().unwrap();
    assert!(digest.starts_with("blake3:"));
    assert_eq!(
        ledger.goal_redacted_text.as_deref(),
        Some(ledger.goal.as_str())
    );

    let task = handle.get_task(handle.task_id().unwrap()).unwrap().unwrap();
    assert_eq!(task.goal, ledger.goal);
    assert!(!task.goal.contains(&secret));
    let contract_json = serde_json::to_string(&task.contract).unwrap();
    assert!(!contract_json.contains(&secret), "{contract_json}");

    let view = handle.ledger_view().unwrap();
    assert_eq!(view.head.goal, ledger.goal);
    assert!(!view.head.goal.contains(&secret));
    let rows = ledger_rows_json(&handle);
    assert!(!rows.contains(&secret), "durable ledger carried the secret");
    let facts = handle.memory_facts().unwrap();
    let facts_text = facts
        .iter()
        .map(|(kind, key, value)| format!("{kind}\u{0}{key}\u{0}{value}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!facts_text.contains(&secret), "{facts_text}");

    let messages = handle.messages_before(None, 8).unwrap();
    let user_text = messages
        .iter()
        .find(|row| row.role == "user")
        .and_then(|row| row.data.get("text"))
        .and_then(|value| value.as_str())
        .unwrap();
    assert!(
        user_text.contains(&secret),
        "the original user message keeps its normal storage policy"
    );
}

#[tokio::test]
async fn secret_and_clean_prompts_derive_the_same_contract_obligations() {
    let secret = ghp();
    let objective = "Implement the zephyr cache eviction policy and add regression tests";

    let (secret_deps, secret_dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let secret_session = session_in_zephyr_workspace(&secret_deps, &secret_dir);
    let secret_runtime = AgentRuntime::new(secret_deps).unwrap();
    secret_runtime
        .run_turn(
            secret_session,
            &format!("{objective}\nUse this deploy token: {secret}"),
            &[],
        )
        .await
        .unwrap();
    let secret_handle = secret_runtime
        .deps()
        .session
        .get_session(secret_session)
        .unwrap()
        .unwrap();
    let secret_task = secret_handle
        .get_task(secret_handle.task_id().unwrap())
        .unwrap()
        .unwrap();

    let (clean_deps, clean_dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let clean_session = session_in_zephyr_workspace(&clean_deps, &clean_dir);
    let clean_runtime = AgentRuntime::new(clean_deps).unwrap();
    clean_runtime
        .run_turn(clean_session, objective, &[])
        .await
        .unwrap();
    let clean_handle = clean_runtime
        .deps()
        .session
        .get_session(clean_session)
        .unwrap()
        .unwrap();
    let clean_task = clean_handle
        .get_task(clean_handle.task_id().unwrap())
        .unwrap()
        .unwrap();

    assert!(
        !secret_task.contract.proof_obligations.is_empty(),
        "the objective must derive proof obligations"
    );
    assert_eq!(
        secret_task.contract.proof_obligations, clean_task.contract.proof_obligations,
        "the redacted projection must derive the same proof obligations"
    );
    assert_eq!(
        secret_task.contract.risks, clean_task.contract.risks,
        "the redacted projection must derive the same risk hypotheses"
    );
}

#[tokio::test]
async fn redaction_empty_drive_fails_typed_and_never_uses_the_title() {
    let secret = ghp();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let inspected: Arc<dyn faktor_provider::Provider> = Arc::new(InspectingProvider::new(
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        move |_n, _request| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    ));
    let (deps, _dir) = deps_with(inspected, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();

    let error = runtime
        .run_turn(session, &format!("here is my key: {secret}"), &[])
        .await
        .unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::GoalUnavailableAfterRedaction {
            source_message_id: handle
                .messages_before(None, 8)
                .unwrap()
                .into_iter()
                .find(|row| row.role == "user")
                .unwrap()
                .id
        }
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no provider call may run without a goal"
    );
    let ledger: faktor_context::TaskLedger =
        serde_json::from_value(handle.get_task_ledger().unwrap().unwrap()).unwrap();
    assert!(
        ledger.goal.is_empty(),
        "the generic title must not become the goal"
    );
    assert!(ledger.goal_unavailable_after_redaction());
    assert!(!serde_json::to_string(&ledger).unwrap().contains(&secret));
    let task = handle.get_task(handle.task_id().unwrap()).unwrap();
    assert!(
        task.as_ref()
            .map(|task| task.goal.is_empty())
            .unwrap_or(true),
        "no durable task goal may fall back to the title"
    );
}

#[tokio::test]
async fn legacy_typed_ledger_head_with_a_secret_is_replaced_by_the_projection() {
    let secret = ghp();
    let objective = "Implement the zephyr cache eviction policy and add regression tests";
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    handle
        .ledger_goal_set(&format!("legacy objective token {secret}"))
        .unwrap();

    runtime
        .run_turn(
            session,
            &format!("{objective}\nUse this deploy token: {secret}"),
            &[],
        )
        .await
        .unwrap();

    let view = handle.ledger_view().unwrap();
    assert!(view.head.goal.contains("zephyr"), "{}", view.head.goal);
    assert!(!view.head.goal.contains(&secret), "{}", view.head.goal);
}

#[tokio::test]
async fn resume_after_restart_uses_the_same_redacted_goal() {
    let dir = fresh_store_dir();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let secret = ghp();
    let objective = "Implement the zephyr cache eviction policy and add regression tests";
    let prompt = format!("{objective}\nUse this deploy token: {secret}");

    let (deps, _keep) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    runtime.run_turn(session, &prompt, &[]).await.unwrap();
    let handle = manager.get_session(session).unwrap().unwrap();
    let first: faktor_context::TaskLedger =
        serde_json::from_value(handle.get_task_ledger().unwrap().unwrap()).unwrap();
    assert!(first.goal.contains("zephyr"));
    assert!(!first.goal.contains(&secret));
    let first_digest = first.goal_projection_digest.clone().unwrap();
    let first_source = first.goal_source_message_id.unwrap();
    drop(runtime);

    let manager2 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let inspected: Arc<dyn faktor_provider::Provider> = Arc::new(InspectingProvider::new(
        Arc::new(scripted_provider(vec![
            ScriptedResponse::Text("continued".into()),
            ScriptedResponse::End,
        ])),
        move |_n, request| {
            cap.lock().unwrap().push(request.system.clone());
            Ok(())
        },
    ));
    let (deps2, _keep2) = deps_sharing_session(manager2.clone(), inspected, vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    runtime2.run_turn(session, "continue", &[]).await.unwrap();
    let handle2 = manager2.get_session(session).unwrap().unwrap();
    let resumed: faktor_context::TaskLedger =
        serde_json::from_value(handle2.get_task_ledger().unwrap().unwrap()).unwrap();
    assert_eq!(
        resumed.goal, first.goal,
        "restart must keep the redacted goal"
    );
    assert_eq!(
        resumed.goal_projection_digest.as_deref(),
        Some(first_digest.as_str()),
        "restart must keep the same projection digest"
    );
    assert_eq!(resumed.goal_source_message_id, Some(first_source));
    assert!(!resumed.goal.contains(&secret));
    let system = captured.lock().unwrap().clone();
    assert_eq!(system.len(), 1);
    assert!(system[0].contains("zephyr"));
    assert!(!system[0].contains(&secret));
}

#[tokio::test]
async fn admission_seeded_title_goal_is_replaced_by_the_projected_objective() {
    let secret = ghp();
    let objective = "Implement the zephyr cache eviction policy and add regression tests";
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let now = handle.now_ms();
    handle.ledger_goal_set(&handle.title().unwrap()).unwrap();
    handle
        .create_task(faktor_session::Task {
            task_id: handle.task_id().unwrap(),
            session_id: session,
            goal: handle.title().unwrap(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            contract: Default::default(),
            budget: faktor_session::TaskBudget::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();

    runtime
        .run_turn(
            session,
            &format!("{objective}\nUse this deploy token: {secret}"),
            &[],
        )
        .await
        .unwrap();
    let task = handle.get_task(handle.task_id().unwrap()).unwrap().unwrap();
    assert_ne!(task.goal, handle.title().unwrap());
    assert!(task.goal.contains("zephyr"));
    assert!(!task.goal.contains(&secret));
    assert!(!serde_json::to_string(&task.contract)
        .unwrap()
        .contains(&secret));
    let view = handle.ledger_view().unwrap();
    assert_ne!(view.head.goal, handle.title().unwrap());
    assert!(view.head.goal.contains("zephyr"), "{}", view.head.goal);
    assert!(!view.head.goal.contains(&secret), "{}", view.head.goal);
}

// ====================================== P0-ASK turn-execution row (durable)

/// The turn-execution row's read contract (audit P0-ASK): ABSENCE is the
/// documented legacy `task` default; a PRESENT row decodes shape-typed —
/// corrupt JSON, an unknown kind and an extended row are all LOUD typed
/// refusals (never a silent fallback to either semantics), and a valid ask
/// row round-trips with its accepted attachment set.
#[test]
fn turn_execution_absence_is_task_and_every_present_corrupt_row_is_typed() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let sid = new_session(&deps);
    let handle = deps.session.get_session(sid).unwrap().unwrap();

    let op = OpId::new(7);
    assert!(
        read_turn_execution(&handle, op).unwrap().is_none(),
        "ABSENCE is the documented legacy default: an unkeyed/legacy turn is a task"
    );

    let key = faktor_protocol::turn_execution_row_key(op.raw());
    for hostile in [
        "not json",
        r#"{"execution":"implement"}"#,
        r#"{"execution":"task","extra":1}"#,
        r#"{"execution":7}"#,
    ] {
        handle
            .upsert_memory_fact(faktor_protocol::TURN_EXECUTION_ROW_KIND, &key, hostile)
            .unwrap();
        let err = read_turn_execution(&handle, op).expect_err("a corrupt row is a typed refusal");
        assert!(
            matches!(err.kind, ErrorKind::Malformed),
            "{hostile:?} must refuse typed, got {err}"
        );
    }

    let op_ask = OpId::new(8);
    let row = faktor_protocol::TurnExecution {
        execution: faktor_protocol::ExecutionKind::Ask,
        attachments: Vec::new(),
    };
    handle
        .upsert_memory_fact(
            faktor_protocol::TURN_EXECUTION_ROW_KIND,
            &faktor_protocol::turn_execution_row_key(op_ask.raw()),
            &row.encode().unwrap(),
        )
        .unwrap();
    let decoded = read_turn_execution(&handle, op_ask)
        .unwrap()
        .expect("a valid ask row decodes");
    assert_eq!(decoded, row);
    assert_eq!(decoded.execution, faktor_protocol::ExecutionKind::Ask);
}

// ================================= P0-GOAL-BINDING projection classification

/// The binding verdicts, adversarially: an UNBOUND legacy head keeps
/// (documented legacy, never a guessed identity); a bound head keeps only
/// for the SAME admitted task; a FOREIGN binding is a typed `Rebuild`
/// naming the old task; a REWOUND bound revision (the row sits below the
/// binding the ledger recorded) refuses typed `Conflict` — a rolled-back
/// task row is never driven under silently.
#[test]
fn goal_projection_binding_keeps_same_task_rebuilds_foreign_and_refuses_rewind() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let sid = new_session(&deps);
    let handle = deps.session.get_session(sid).unwrap().unwrap();
    let now = handle.now_ms();
    let task_row = |id: u64, goal: String| faktor_session::Task {
        task_id: TaskId::new(id),
        session_id: sid,
        goal,
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        contract: Default::default(),
        budget: faktor_session::TaskBudget::default(),
        state: faktor_core::state::TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    };
    handle
        .create_task(task_row(1, "restore the alpha parser".into()))
        .unwrap();
    handle
        .create_task(task_row(2, "build the beta indexer".into()))
        .unwrap();

    assert!(
        matches!(
            classify_goal_projection_binding(&handle, TaskId::new(1)).unwrap(),
            GoalProjectionBinding::Keep
        ),
        "an unbound legacy head keeps — absence is never a guessed binding"
    );

    let revision_one = handle.task_revision(TaskId::new(1)).unwrap().raw();
    handle
        .ledger_goal_set_bound("restore the alpha parser", 1, revision_one)
        .unwrap();
    assert!(
        matches!(
            classify_goal_projection_binding(&handle, TaskId::new(1)).unwrap(),
            GoalProjectionBinding::Keep
        ),
        "the bound task's continuation keeps its projection"
    );
    match classify_goal_projection_binding(&handle, TaskId::new(2)).unwrap() {
        GoalProjectionBinding::Rebuild { bound_task_id } => {
            assert_eq!(bound_task_id, 1, "the mismatch names the OLD task")
        }
        GoalProjectionBinding::Keep => panic!("a foreign binding must rebuild"),
    }

    // Rewind: the binding records a revision the durable row never had.
    handle
        .ledger_goal_set_bound("restore the alpha parser", 1, revision_one + 9999)
        .unwrap();
    let err = classify_goal_projection_binding(&handle, TaskId::new(1))
        .expect_err("a rewound binding revision must refuse typed");
    assert!(matches!(err.kind, ErrorKind::Conflict), "{err}");

    // The rebuild itself: task 2 never reads task 1's goal/plan/checks.
    let mut ledger = TaskLedger {
        goal: "restore the alpha parser".into(),
        completed_steps: vec!["parsed the old grammar".into()],
        open_steps: vec!["port the tests".into()],
        decisions: vec!["keep the lexer table".into()],
        known_failures: vec!["flaky reconnect".into()],
        changed_files: vec!["src/alpha.rs".into()],
        tests_run: vec!["alpha_test".into()],
        user_preferences: vec!["concise answers".into()],
        ..TaskLedger::default()
    };
    rebuild_goal_projection(&handle, &mut ledger, TaskId::new(2), 1).unwrap();
    assert!(
        ledger.goal.contains("beta"),
        "the rebuilt goal is THIS task's objective: {}",
        ledger.goal
    );
    assert!(!ledger.goal.contains("alpha"), "{}", ledger.goal);
    assert!(
        ledger.completed_steps.is_empty()
            && ledger.open_steps.is_empty()
            && ledger.decisions.is_empty()
            && ledger.known_failures.is_empty()
            && ledger.changed_files.is_empty()
            && ledger.tests_run.is_empty(),
        "a new task never reads the old projection's plan/checks/evidence"
    );
    assert_eq!(
        ledger.user_preferences,
        vec!["concise answers".to_string()],
        "session knowledge survives a projection rebuild"
    );
    assert_eq!(
        ledger.goal_projection_state,
        Some(faktor_context::ledger::GoalProjectionState::Available)
    );
    assert!(
        handle.get_task_ledger().unwrap().is_some(),
        "the rebuilt projection persists"
    );
    // TYPED, not silent: the rebuild is durable ledger history naming the
    // mismatch — never a quiet in-memory substitution.
    assert!(
        ledger_rows_json(&handle).contains("\"goal binding\""),
        "the rebuild lands a typed ledger Decision"
    );

    // A task objective that redacts to nothing refuses with the SAME typed
    // error a fresh projection gives — never the stale inherited goal.
    let secret_only = ghp();
    handle
        .create_task(task_row(3, secret_only.clone()))
        .unwrap();
    let mut stale = TaskLedger {
        goal: "restore the alpha parser".into(),
        completed_steps: vec!["old work".into()],
        ..TaskLedger::default()
    };
    let err = rebuild_goal_projection(&handle, &mut stale, TaskId::new(3), 1)
        .expect_err("a secret-only objective must refuse typed");
    assert!(
        matches!(err.kind, ErrorKind::GoalUnavailableAfterRedaction { .. }),
        "{err}"
    );
    assert!(
        !stale.goal.contains("alpha"),
        "the refused rebuild never leaves the stale objective in place"
    );
}
