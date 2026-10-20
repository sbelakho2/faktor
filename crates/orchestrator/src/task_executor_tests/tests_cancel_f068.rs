//! F068 regression: whole-run cancel after a child stop.

use super::*;

#[tokio::test]
pub(crate) async fn whole_run_cancel_after_a_child_stop_cancels_the_root_without_a_500() {
    // F068 regression: stopping ONE child then cancelling the WHOLE run used
    // to surface a 500 (`NotFound: operation ...`) from the idempotent abort
    // race and left the root row Running. The child stop is idempotent, and
    // the whole-run cancel still cancels the run scope even when every child
    // has already become terminal.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ticker = Arc::new(TickerProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        request_count: AtomicUsize::new(0),
        chunk_delay_ms: 15,
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(ticker.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    let owner_root = dir.path().join("owner");
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
        .create_session(ws, "f068", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let isolated = dir.path().join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new_owner_direct_for_test_harness(
        &orchestrator,
        manager.clone(),
        agent.clone(),
    );
    let receipt = executor
        .start_task(
            parent,
            TaskRunRequest {
                goal: "two children, stop one then cancel the run".into(),
                work_items: vec![
                    wi("a", WorkKind::Analysis, &[]),
                    wi("b", WorkKind::Analysis, &[]),
                ],
                parent_caps: crate::caps::all_lattice(),
                isolated_root: isolated,
                ..Default::default()
            },
        )
        .expect("multi-item run starts");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| !c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    wait_until(|| ticker.count() >= 2, 240).await;
    // Stop child-0 exactly like the live F094 probe. The stop is accepted
    // synchronously (durable Cancel row + bounded abort); the whole-run
    // cancel follows immediately, exactly the live sequence.
    orchestrator.cancel_child("child-0").expect("child stop");
    tokio::time::sleep(Duration::from_millis(150)).await;
    // A follow-up WHOLE-RUN cancel must be accepted (idempotent per-child)
    // and cancel the root row even though child-0 already stopped.
    executor
        .cancel_run(parent, &receipt.run_id)
        .expect("whole-run cancel after a child stop is accepted");
    let h = manager.get_session(parent).unwrap().unwrap();
    wait_until(
        || {
            h.get_task(h.task_id().unwrap())
                .unwrap()
                .is_some_and(|t| t.state == TaskState::Cancelled)
        },
        60,
    )
    .await;
    assert_eq!(
        h.get_task(h.task_id().unwrap()).unwrap().unwrap().state,
        TaskState::Cancelled,
        "the run stayed live after a successful whole-run cancel"
    );
}
