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
    let key = format!("child-handoff/{}", rows[0].child_id);
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
    let watermark_key = format!("child-handoff-delivered/{}", rows[0].child_id);
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
    let key = format!("child-handoff/{}", rows[0].child_id);
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
