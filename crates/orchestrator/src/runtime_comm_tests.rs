// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Child-agent / cross-agent communication probes (split from
//! `runtime_tests.rs` to stay under the recorded source-size ceiling):
//! crash-window handoff healing + outcome truth, nested-spawn refusal, and
//! zero-orphan spawn rollback.

use super::*;

/// New-angle comms probe: a kill between a child's terminal durable row and
/// its parent handoff must not lose the child's result. The deterministic
/// `AfterChildTerminal` seam now fires inside that window; re-attach must
/// re-derive the typed handoff from the terminal row, report the child's
/// real outcome, and the parent's next turn must consume it exactly once.
#[tokio::test]
async fn fault_probe_crash_before_parent_handoff_reattach_heals_exactly_once() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), roundtrip_script(), 0));
    let p = plan(
        OwnershipModel::NoWrites,
        vec![wi("a", WorkKind::Analysis, &[])],
    );
    let mut config = base_config(&env, "run-handoff-crash");
    config.crash_seam = Some(CrashSeam::AfterChildTerminal);
    let err = run_exec(&env, p, config, vec![spec("a")])
        .await
        .expect_err("the AfterChildTerminal seam must fire");
    assert!(matches!(err, ExecError::InjectedCrashSeam(_)), "{err:?}");
    let rows =
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, "run-handoff-crash")
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ChildState::Done, "terminal rows persisted");
    let parent = env.manager.get_session(env.parent).unwrap().unwrap();
    let key = format!("child-handoff/run-handoff-crash/{}", rows[0].child_id);
    assert!(
        parent
            .memory_fact_get("internal_context", &key)
            .unwrap()
            .is_none(),
        "the crash fired BEFORE the parent handoff (the healed window)"
    );

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        env.orchestrator.reattach(
            env.parent,
            "run-handoff-crash",
            Ceilings::default(),
            base_config(&env, "x").parent_caps,
            "m".to_string(),
            env.isolated_root.clone(),
            None,
        ),
    )
    .await
    .expect("reattach exceeded the hard bound")
    .expect("reattach heals the run");
    assert!(outcome.complete, "{outcome:?}");
    let section = parent
        .memory_fact_get("internal_context", &key)
        .unwrap()
        .expect("re-attach re-armed the missing parent handoff");
    assert!(
        section.contains("outcome: done"),
        "the handoff reports the child's durable terminal outcome: {section}"
    );
    let watermark_key = format!(
        "child-handoff-delivered/run-handoff-crash/{}",
        rows[0].child_id
    );
    let watermark = rows[0].updated_ms.to_string();
    assert_eq!(
        parent
            .memory_fact_get("internal_context", &watermark_key)
            .unwrap()
            .as_deref(),
        Some(watermark.as_str()),
        "the delivery watermark binds the handoff to the exact terminal state"
    );

    // The parent's next logical turn consumes the healed handoff exactly
    // once; the durable ack (empty value) is never re-delivered.
    env.agent
        .run_turn(env.parent, "continue the parent task", &[])
        .await
        .expect("parent continuation drives");
    assert_eq!(
        parent
            .memory_fact_get("internal_context", &key)
            .unwrap()
            .as_deref(),
        Some(""),
        "the healed handoff is acked exactly once"
    );
    assert_eq!(
        parent
            .memory_fact_get("internal_context", &key)
            .unwrap()
            .as_deref(),
        Some("")
    );
}

/// New-angle depth probe: orchestration nests exactly ONE level. A child
/// session can never own another run (no grandchildren), and the refusal is
/// typed before any plan/registry row of the nested run is written.
#[tokio::test]
async fn fault_probe_orchestrated_child_cannot_spawn_grandchildren() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), empty_script(), 0));
    let ws = faktor_core::id::WorkspaceId::try_from(env.owner.workspace_id).unwrap();
    let wt = WorktreeId::try_from(env.owner.worktree_id).unwrap();
    let child = env
        .manager
        .create_child_session(
            env.parent,
            ws,
            wt,
            TaskId::new(1),
            "fake",
            "m",
            "nested child",
            ChildOwnership::ReadOnlyShared,
        )
        .unwrap();
    let nested_owner = OwnerContext {
        parent_session: child.id(),
        workspace_id: env.owner.workspace_id,
        worktree_id: env.owner.worktree_id,
        root: env.owner.root.clone(),
    };
    let p = plan(
        OwnershipModel::NoWrites,
        vec![wi("a", WorkKind::Analysis, &[])],
    );
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        env.orchestrator.execute_task(
            p,
            nested_owner,
            base_config(&env, "run-grandchild"),
            &[spec("a")],
        ),
    )
    .await
    .expect("nested spawn check is immediate")
    .expect_err("a child can never start a nested orchestrated run");
    assert!(
        matches!(err, ExecError::InvalidState(ref m) if m.contains("itself an orchestrated child")),
        "{err:?}"
    );
    // Nothing of the refused nested run was written.
    assert!(
        OrchestratorRuntime::registry_rows(env.manager.clone(), child.id(), "run-grandchild")
            .unwrap()
            .is_empty()
    );
    assert!(env
        .manager
        .get_session(child.id())
        .unwrap()
        .unwrap()
        .memory_fact_get(PLAN_ROW_KIND, "run-grandchild")
        .unwrap()
        .is_none());
}

/// New-angle zero-orphan probe: a spawn interrupted after the child session
/// exists but before its registry anchor must be rolled back to a Closed
/// tombstone, and a Closed tombstone must never count as a LIVE orphan
/// (otherwise the run can never re-attach/retry).
#[tokio::test]
async fn fault_probe_spawn_rollback_leaves_no_live_orphan() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), empty_script(), 0));
    let ws = faktor_core::id::WorkspaceId::try_from(env.owner.workspace_id).unwrap();
    let wt = WorktreeId::try_from(env.owner.worktree_id).unwrap();
    let child = env
        .manager
        .create_child_session(
            env.parent,
            ws,
            wt,
            TaskId::new(1),
            "fake",
            "m",
            "interrupted spawn",
            ChildOwnership::ReadOnlyShared,
        )
        .unwrap();
    let violations =
        OrchestratorRuntime::registry_violations(env.manager.clone(), env.parent, "run-orphan");
    assert!(
        violations
            .iter()
            .any(|v| v.contains("orphan child session")),
        "a live unregistered child is an orphan: {violations:?}"
    );
    // The spawn-failure rollback: close the just-created child.
    env.manager.delete_session(child.id()).unwrap();
    let violations =
        OrchestratorRuntime::registry_violations(env.manager.clone(), env.parent, "run-orphan");
    assert!(
        violations.is_empty(),
        "a Closed spawn rollback tombstone is not a live orphan: {violations:?}"
    );
}

/// Approval-required outcomes are ORDINARY successful tool results: the
/// child turn terminates, the run settles, and the parent receives a
/// bounded handoff reporting `done` — never a deadlock, never a fabricated
/// `approved`. (The daemon tools themselves are exercised by
/// `tests/production-wiring/tests/approval_gate.rs`: no tool can decide.)
#[tokio::test]
async fn approval_required_tool_result_does_not_stall_the_child_handoff() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let approval_result = serde_json::json!({
        "status": "approval_required",
        "kind": "engineering_constraint_relaxation",
        "subject": "req-set-1",
        "bound_revision": "7",
        "bound_digest": "a".repeat(64),
    })
    .to_string();
    let env = Arc::new(open_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::Text("proposing a consequential change".into()),
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"text": approval_result}),
                },
                ScriptedResponse::End,
            ],
            vec![
                ScriptedResponse::Text(
                    "approval pending with a human operator; no action taken".into(),
                ),
                ScriptedResponse::End,
            ],
        ],
        0,
    ));
    let p = plan(
        OwnershipModel::NoWrites,
        vec![wi("a", WorkKind::Analysis, &[])],
    );
    let outcome = run_exec(
        &env,
        p,
        base_config(&env, "run-approval-required"),
        vec![spec("a")],
    )
    .await
    .expect("an approval-required tool result must not stall the run");
    assert!(outcome.complete, "{outcome:?}");
    let rows = OrchestratorRuntime::registry_rows(
        env.manager.clone(),
        env.parent,
        "run-approval-required",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].state,
        ChildState::Done,
        "the child lifecycle is terminal (approval is a separate human queue)"
    );
    let parent = env.manager.get_session(env.parent).unwrap().unwrap();
    let key = format!("child-handoff/run-approval-required/{}", rows[0].child_id);
    let section = parent
        .memory_fact_get("internal_context", &key)
        .unwrap()
        .expect("the parent handoff was delivered");
    assert!(section.contains("outcome: done"), "{section}");
    assert!(
        !section.to_lowercase().contains("approved"),
        "a pending approval must never be reported as approved: {section}"
    );
    // The parent consumes the handoff exactly once.
    env.agent
        .run_turn(env.parent, "continue", &[])
        .await
        .expect("parent continuation drives");
    assert_eq!(
        parent
            .memory_fact_get("internal_context", &key)
            .unwrap()
            .as_deref(),
        Some("")
    );
}

/// CASE INV-HANDOFF-RUN-QUALIFIED: child ids are minted PER RUN (`child-0` in
/// every plan), so two runs of the SAME parent session both own a `child-0`.
/// The durable handoff key is run-qualified, so neither delivery overwrites
/// or watermark-suppresses the other; the parent's next turn consumes BOTH,
/// each with its own run's goal.
#[tokio::test]
async fn two_runs_of_one_parent_keep_distinct_child_handoffs() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let mut scripts = roundtrip_script();
    scripts.extend(roundtrip_script());
    scripts.extend(empty_script());
    let env = Arc::new(open_env(dir.path(), scripts, 0));

    let first_plan = plan(
        OwnershipModel::NoWrites,
        vec![wi("alpha", WorkKind::Analysis, &[])],
    );
    let first = run_exec(
        &env,
        first_plan,
        base_config(&env, "run-alpha"),
        vec![spec("alpha")],
    )
    .await
    .expect("first run completes");
    assert!(first.complete, "{first:?}");

    let second_plan = plan(
        OwnershipModel::NoWrites,
        vec![wi("beta", WorkKind::Analysis, &[])],
    );
    let second = run_exec(
        &env,
        second_plan,
        base_config(&env, "run-beta"),
        vec![spec("beta")],
    )
    .await
    .expect("second run completes");
    assert!(second.complete, "{second:?}");

    // Both runs minted the SAME per-run child id; only the run qualifier
    // keeps their durable deliveries apart.
    assert_eq!(first.children[0].child_id, second.children[0].child_id);
    let parent = env.manager.get_session(env.parent).unwrap().unwrap();
    let alpha_key = format!("child-handoff/run-alpha/{}", first.children[0].child_id);
    let beta_key = format!("child-handoff/run-beta/{}", second.children[0].child_id);
    let alpha = parent
        .memory_fact_get("internal_context", &alpha_key)
        .unwrap()
        .expect("the first run's handoff is still pending");
    let beta = parent
        .memory_fact_get("internal_context", &beta_key)
        .unwrap()
        .expect("the second run's handoff is still pending");
    assert!(alpha.contains("work alpha"), "{alpha}");
    assert!(!alpha.contains("work beta"), "no sibling leak: {alpha}");
    assert!(beta.contains("work beta"), "{beta}");
    assert!(!beta.contains("work alpha"), "no sibling leak: {beta}");
    assert!(
        parent
            .memory_fact_get(
                "internal_context",
                &format!("child-handoff/{}", first.children[0].child_id)
            )
            .unwrap()
            .is_none(),
        "a session-wide key must never be written (collision surface)"
    );

    // The parent consumes BOTH exactly once in one turn.
    let before = env.provider.count();
    env.agent
        .run_turn(env.parent, "continue both runs", &[])
        .await
        .expect("the parent continuation drives");
    let system = env
        .provider
        .text_of(before)
        .expect("parent request captured");
    assert!(
        system.contains("work alpha"),
        "alpha handoff missing: {system}"
    );
    assert!(
        system.contains("work beta"),
        "beta handoff missing: {system}"
    );
    for key in [&alpha_key, &beta_key] {
        assert_eq!(
            parent
                .memory_fact_get("internal_context", key)
                .unwrap()
                .as_deref(),
            Some(""),
            "each run-qualified handoff is acked exactly once"
        );
    }
}

/// CASE INV-HANDOFF-CANCELLED: a Cancelled child is a terminal OUTCOME,
/// not an absence. Its parent handoff is delivered with the typed
/// `outcome: cancelled` (never a fabricated success), so the parent knows
/// the item stopped instead of inferring silence.
#[tokio::test]
async fn cancelled_child_reports_an_explicit_cancelled_handoff() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), empty_script(), 0));
    let run = "run-cancelled-handoff";
    let child_id = "child-0";
    let session_id = craft_child_row(&env, run, child_id, "analysis", ChildState::Cancelled, None);
    let child = env
        .manager
        .get_session(session_id)
        .unwrap()
        .expect("crafted child session");
    child
        .orchestrator_child_runtime_put(&faktor_session::child::ChildRuntimeBlockerRow {
            child_id: child_id.into(),
            state: "cancelled".into(),
            blocker: None,
            updated_ms: 77,
        })
        .unwrap();

    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, run).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ChildState::Cancelled);
    env.orchestrator
        .deliver_child_handoff(env.parent, run, &rows[0]);

    let parent = env.manager.get_session(env.parent).unwrap().unwrap();
    let key = format!("child-handoff/{run}/{child_id}");
    let section = parent
        .memory_fact_get("internal_context", &key)
        .unwrap()
        .expect("a cancelled child's terminal outcome is delivered");
    assert!(
        section.contains("outcome: cancelled"),
        "the handoff names the honest outcome: {section}"
    );
    assert!(
        !section.contains("outcome: done") && !section.contains("outcome: failed"),
        "cancellation is never reported as success or failure: {section}"
    );
}

/// CASE MC-HANDOFF-RUN-KEY / MC-HANDOFF-CANCELLED (mutation witnesses): the handoff key MUST carry
/// the run id, and cancellation MUST be in the delivered terminal set.
fn handoff_key_guard_present(source: &str) -> bool {
    source.contains("format!(\"{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{run_id}/{child_id}\")")
        && source
            .contains("format!(\"{PENDING_INTERNAL_CONTEXT_DELIVERED_PREFIX}{run_id}/{child_id}\")")
}

fn cancelled_handoff_guard_present(source: &str) -> bool {
    source.contains("ChildState::Done | ChildState::Failed | ChildState::Cancelled")
}

#[test]
fn handoff_run_qualification_and_cancellation_guards_are_pinned() {
    const SOURCE: &str = include_str!("runtime_handoff.rs");
    assert!(
        handoff_key_guard_present(SOURCE),
        "the child-handoff fact key lost its run qualifier (sibling overwrite)"
    );
    let unqualified = SOURCE.replace(
        "format!(\"{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{run_id}/{child_id}\")",
        "format!(\"{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{child_id}\")",
    );
    assert!(
        !handoff_key_guard_present(&unqualified),
        "the witness must detect a session-wide key regression"
    );
    assert!(
        cancelled_handoff_guard_present(SOURCE),
        "the delivered terminal set lost Cancelled (silent child outcome)"
    );
    let no_cancel = SOURCE.replace(
        "ChildState::Done | ChildState::Failed | ChildState::Cancelled",
        "ChildState::Done | ChildState::Failed",
    );
    assert!(
        !cancelled_handoff_guard_present(&no_cancel),
        "the witness must detect dropped cancellation delivery"
    );
}

/// Upgrade-window exactly-once: a pre-run-qualified pending handoff from an
/// older daemon is still THE delivery for that child. Re-attach healing must
/// not re-deliver it under the run-qualified key (the parent would consume
/// the result twice).
#[tokio::test]
async fn heal_does_not_duplicate_a_legacy_unqualified_handoff() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), empty_script(), 0));
    let run = "run-legacy-hash";
    let child_id = "child-0";
    let _session = craft_child_row(&env, run, child_id, "analysis", ChildState::Done, None);
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, run).unwrap();
    let parent = env.manager.get_session(env.parent).unwrap().unwrap();
    parent
        .upsert_memory_fact(
            "internal_context",
            &format!("child-handoff/{child_id}"),
            "[faktor:internal-context]\nlegacy delivery\n[/faktor:internal-context]",
        )
        .unwrap();
    let mut children = std::collections::HashMap::new();
    children.insert(child_id.to_string(), rows[0].clone());
    env.orchestrator
        .heal_pending_child_handoffs(env.parent, run, &children);
    assert!(
        parent
            .memory_fact_get(
                "internal_context",
                &format!("child-handoff/{run}/{child_id}")
            )
            .unwrap()
            .is_none(),
        "a legacy pending section is the once delivery; healing must not duplicate it"
    );
    assert!(parent
        .memory_fact_get("internal_context", &format!("child-handoff/{child_id}"))
        .unwrap()
        .is_some());
}
