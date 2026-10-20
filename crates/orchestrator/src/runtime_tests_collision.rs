//! Cross-run child-id authority probes (split from `runtime_tests.rs` to
//! stay under the recorded source-size ceiling).

use super::*;

/// Two LIVE runs of different sessions legitimately mint the same per-run
/// child ids (`child-0` in every plan). The runtime's by-id child lookup
/// used to return the FIRST HashMap match, so a control/read from one
/// session could silently target the OTHER session's child. Ambiguous ids
/// must refuse typed before any control row is written.
#[tokio::test]
async fn child_id_collision_across_live_runs_is_a_typed_conflict() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), empty_script(), 0));
    let p = plan(
        OwnershipModel::NoWrites,
        vec![wi("a", WorkKind::Analysis, &[])],
    );
    let make_row = |run: &str| ChildRuntime {
        child_id: "child-0".into(),
        parent_session_id: env.parent.raw(),
        run_id: run.into(),
        item_id: "a".into(),
        kind: WorkKind::Analysis,
        session_id: env.parent.raw(),
        operation_id: 0,
        workspace_id: env.owner.workspace_id,
        worktree_id: env.owner.worktree_id,
        ownership: ChildOwnership::ReadOnlyShared,
        ownership_paths: Vec::new(),
        state: ChildState::Running,
        budget_max_tokens: None,
        permissions: crate::caps::all_lattice(),
        model_policy: ModelPolicy { model: None },
        blocker_kind: None,
        blocker_reason: None,
        blocker_dependency: None,
        blocker_resolution: None,
        last_progress_ms: None,
        created_ms: 0,
        updated_ms: 0,
        base_snapshot_id: None,
        run_base_snapshot: None,
        env_snapshot_id: None,
        execution_phase: ExecutionPhase::Planning,
    };
    for run in ["run-collide-a", "run-collide-b"] {
        let mut exec = env.orchestrator.build_exec_state(
            p.clone(),
            env.owner.clone(),
            base_config(&env, run),
            HashMap::new(),
        );
        exec.children.insert("child-0".into(), make_row(run));
        env.orchestrator.install_run(exec).unwrap();
    }
    // The read is ambiguous: named conflict, never an arbitrary child.
    let err = env
        .orchestrator
        .child("child-0")
        .expect_err("ambiguous child id must refuse");
    let msg = format!("{err:?}");
    assert!(
        matches!(err, ExecError::Conflict(_))
            && msg.contains("run-collide-a")
            && msg.contains("run-collide-b"),
        "{msg}"
    );
    // Every control path funnels through the same lookup and refuses BEFORE
    // enqueueing: no session receives a stale/steered control.
    for err in [
        env.orchestrator
            .pause_child("child-0")
            .expect_err("pause must refuse"),
        env.orchestrator
            .steer_child("child-0", "cross-run steer")
            .expect_err("steer must refuse"),
        env.orchestrator
            .cancel_child("child-0")
            .expect_err("cancel must refuse"),
    ] {
        assert!(matches!(err, ExecError::Conflict(_)), "{err:?}");
    }
    let parent = env.manager.get_session(env.parent).unwrap().unwrap();
    let identities: Vec<_> = env
        .manager
        .list_sessions(None)
        .unwrap()
        .into_iter()
        .filter(|h| h.orchestrator_child_identity_get().ok().flatten().is_some())
        .collect();
    assert!(
        identities.is_empty(),
        "no child identity/control row may be created by a refused ambiguous control"
    );
    assert!(parent.orchestrator_ctl_all().unwrap().is_empty());
}
