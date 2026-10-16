//! `runtime::runtime_tests::admission_gate`: the commercial-admission-gate
//! and hostile-registry test section split out of `runtime_tests.rs`
//! mechanically (source-size ceiling). Declared as a child of
//! `runtime_tests`, so every fixture of the parent test module (and, through
//! it, of the `runtime` module) stays in scope via `use super::*`.

use super::*;

// ------------------------------------------------- commercial admission gate
// (Wave 3) The ONE gate hook is consulted at exactly three SAFE BOUNDARIES:
// new task admission, new child spawn, new provider attempt BEFORE
// dispatch. A refusal admits NOTHING; continuation/control paths (cancel,
// integration, rollback, completion) never consult it, so an entitlement
// change can never interrupt an in-flight transaction.

/// A gate that refuses every gated boundary, naming a fixed limit.
struct RefuseGate;

impl crate::admission::AdmissionGate for RefuseGate {
    fn check(
        &self,
        request: &crate::admission::AdmissionRequest,
    ) -> Result<(), crate::admission::AdmissionRefusal> {
        Err(crate::admission::AdmissionRefusal::new(
            request.boundary,
            "max_active_tasks",
            "the test gate refuses every admission",
        ))
    }
}

/// A gate that admits everything and records every consultation.
#[derive(Default)]
struct CountingGate {
    seen: StdMutex<Vec<crate::admission::AdmissionBoundary>>,
}

impl crate::admission::AdmissionGate for CountingGate {
    fn check(
        &self,
        request: &crate::admission::AdmissionRequest,
    ) -> Result<(), crate::admission::AdmissionRefusal> {
        self.seen.lock().unwrap().push(request.boundary);
        Ok(())
    }
}

#[tokio::test]
async fn admission_gate_refuses_a_new_task_before_any_durable_row() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), empty_script(), 0));
    env.orchestrator.set_admission_gate(Arc::new(RefuseGate));
    let err = run_exec(
        &env,
        raw_plan(vec![wi("analysis", WorkKind::Analysis, &[])]),
        base_config(&env, "run-gated"),
        vec![spec("analysis")],
    )
    .await
    .expect_err("the gate must refuse the new task");
    match err {
        ExecError::AdmissionRefused {
            boundary, limit, ..
        } => {
            assert_eq!(boundary, "new_task");
            assert_eq!(limit, "max_active_tasks", "the refusal names the limit");
        }
        other => panic!("expected a typed AdmissionRefused, got {other:?}"),
    }
    assert!(
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, "run-gated")
            .unwrap()
            .is_empty(),
        "a refused admission leaves no durable registry row"
    );
    // The SAME run is admitted once the gate is cleared: the refusal wrote
    // nothing that could make the retry fail differently.
    env.orchestrator.clear_admission_gate();
    let outcome = run_exec(
        &env,
        raw_plan(vec![wi("analysis", WorkKind::Analysis, &[])]),
        base_config(&env, "run-gated"),
        vec![spec("analysis")],
    )
    .await
    .expect("the same run is admitted after the gate clears");
    assert!(outcome.complete, "{outcome:?}");
}

#[tokio::test]
async fn admission_gate_is_consulted_at_every_gated_boundary_and_never_on_cancel() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), roundtrip_script(), 0));
    let gate = Arc::new(CountingGate::default());
    env.orchestrator.set_admission_gate(gate.clone());
    let outcome = run_exec(
        &env,
        raw_plan(vec![wi("analysis", WorkKind::Analysis, &[])]),
        base_config(&env, "run-count"),
        vec![spec("analysis")],
    )
    .await
    .expect("the run succeeds under an admitting gate");
    assert!(outcome.complete, "{outcome:?}");
    let seen = gate.seen.lock().unwrap().clone();
    use crate::admission::AdmissionBoundary as B;
    assert_eq!(
        seen.iter().filter(|b| **b == B::NewTask).count(),
        1,
        "exactly one new-task boundary: {seen:?}"
    );
    assert_eq!(
        seen.iter().filter(|b| **b == B::NewChildSpawn).count(),
        1,
        "exactly one child spawn: {seen:?}"
    );
    assert!(
        seen.iter().filter(|b| **b == B::NewProviderAttempt).count() >= 1,
        "the drive dispatch is a gated provider-attempt boundary: {seen:?}"
    );
    // Control/continuation paths consult NOTHING: steering/control calls on
    // the same run must not add a consultation (either outcome is acceptable
    // — the assertion is the gate count).
    let before = seen.len();
    let cancelled = env.orchestrator.cancel_child("missing-child");
    let steered = env.orchestrator.steer_child("missing-child", "note");
    let paused = env.orchestrator.pause_child("missing-child");
    let _ = (cancelled, steered, paused); // typed NotFound outcomes are fine
    assert_eq!(
        gate.seen.lock().unwrap().len(),
        before,
        "control/continuation paths are never gated"
    );
    // A poisoned gate lock FAILS CLOSED (typed, never a silent admit).
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = env.orchestrator.admission.lock().unwrap();
        panic!("poison the admission gate lock (test seam)");
    }));
    assert!(poisoned.is_err());
    let err = env
        .orchestrator
        .consult_admission(crate::admission::AdmissionRequest {
            boundary: B::NewTask,
            parent_session: env.parent.raw(),
            task_id: None,
            run_id: "run-poisoned".into(),
            provider: None,
            model: None,
            observed: Default::default(),
        })
        .expect_err("a poisoned gate lock refuses");
    match err {
        ExecError::AdmissionRefused { limit, .. } => {
            assert_eq!(limit, "admission_gate_poisoned")
        }
        other => panic!("expected a typed AdmissionRefused, got {other:?}"),
    }
}

/// Decoded registry-row identity ids are HOSTILE input: zero in any identity
/// field is a typed refusal at the decode boundary — never a panic in an id
/// constructor (`SessionId::new(0)`), never a silently projected row, and
/// never a fabricated graph node.
#[tokio::test]
async fn corrupt_registry_row_identity_ids_are_typed_refusals_never_panics() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), vec![], 0);
    let parent_handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let valid = ChildRuntime {
        child_id: "child-0".into(),
        parent_session_id: env.parent.raw(),
        run_id: "run-corrupt".into(),
        item_id: "impl".into(),
        kind: WorkKind::Implementation,
        session_id: 7,
        operation_id: 0,
        workspace_id: env.owner.workspace_id,
        worktree_id: env.owner.worktree_id,
        ownership: ChildOwnership::ReadOnlyShared,
        ownership_paths: vec![],
        state: ChildState::Running,
        budget_max_tokens: None,
        permissions: CapabilitySet::new(),
        model_policy: ModelPolicy::default(),
        blocker_kind: None,
        blocker_reason: None,
        blocker_dependency: None,
        blocker_resolution: None,
        last_progress_ms: None,
        created_ms: 1,
        updated_ms: 1,
        base_snapshot_id: None,
        run_base_snapshot: None,
        env_snapshot_id: None,
        execution_phase: ExecutionPhase::default(),
    };

    // Per identity field: zero fails `validate_durable` typed, naming the
    // field — while u64::MAX stays a valid, exact id (no truncation).
    type Corrupt = fn(&mut ChildRuntime);
    let cases: [(&str, Corrupt); 4] = [
        ("session_id", |r| r.session_id = 0),
        ("parent_session_id", |r| r.parent_session_id = 0),
        ("workspace_id", |r| r.workspace_id = 0),
        ("worktree_id", |r| r.worktree_id = 0),
    ];
    for (field, corrupt) in cases {
        let mut row = valid.clone();
        corrupt(&mut row);
        let err = row
            .validate_durable()
            .expect_err(&format!("zero {field} must be refused"));
        assert!(matches!(err, ExecError::Malformed(_)), "{err:?}");
        assert!(err.to_string().contains(field), "{err}");
    }
    let mut extreme = valid.clone();
    extreme.session_id = u64::MAX;
    extreme
        .validate_durable()
        .expect("u64::MAX is a valid nonzero id");

    // Valid durable row: decodes and round-trips through the registry read.
    parent_handle
        .upsert_memory_fact(
            REGISTRY_ROW_KIND,
            "run-corrupt/child-0",
            &serde_json::to_string(&valid).unwrap(),
        )
        .unwrap();
    assert_eq!(
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, "run-corrupt")
            .unwrap()
            .len(),
        1,
        "the valid row round-trips"
    );

    // Corrupt durable row (session_id 0): typed decode refusal, no panic.
    let mut hostile = valid.clone();
    hostile.session_id = 0;
    parent_handle
        .upsert_memory_fact(
            REGISTRY_ROW_KIND,
            "run-corrupt/child-0",
            &serde_json::to_string(&hostile).unwrap(),
        )
        .unwrap();
    let err = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, "run-corrupt")
        .expect_err("a zero session id must be refused at the decode boundary");
    assert_eq!(err.kind, faktor_core::ErrorKind::Malformed, "{err:?}");
    assert!(err.message.contains("session_id 0"), "{err:?}");

    // Non-returning projection backstops never panic on the hostile row.
    let mut projected = hostile.clone();
    OrchestratorRuntime::derive_child_projection(&env.manager, &mut projected);
    let err = OrchestratorRuntime::child_presentation(env.manager.clone(), &hostile)
        .expect_err("child presentation of a malformed row is a typed error");
    assert_eq!(err.kind, faktor_core::ErrorKind::Malformed, "{err:?}");
    assert!(err.message.contains("session_id"), "{err:?}");
    assert!(child_task_budget_cap(&env.manager, 0).is_none());
    assert_eq!(
        child_execution_phase(&env.manager, 0),
        ExecutionPhase::default()
    );

    // Graph assembly over the same corrupt row: typed GraphError, never a
    // panic and never a fabricated node.
    let plan = TaskPlan {
        goal: "corrupt graph".into(),
        non_goals: vec![],
        constraints: vec![],
        work_items: vec![],
    };
    let config = ExecConfig {
        run_id: "run-corrupt".into(),
        ceilings: Ceilings::default(),
        parent_caps: CapabilitySet::new(),
        provider: "fake".into(),
        default_model: "m".into(),
        isolated_root: dir.path().join("iso"),
        crash_seam: None,
    };
    env.orchestrator
        .put_plan_row(&plan, &env.owner, &config, &[])
        .unwrap();
    let err = env
        .orchestrator
        .operation_graph_run(env.parent, "run-corrupt")
        .expect_err("a zero session id must refuse graph assembly");
    assert!(matches!(err, GraphError::Malformed(_)), "{err:?}");
}

/// Tangerine-9: a spawned child's identity carries its durable class-level
/// capability budget (the projection of its effective grants), so the agent
/// tool gate enforces parent ∩ task ∩ child without any `ALL` placeholder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_child_identity_carries_the_durable_capability_budget() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(open_env(dir.path(), empty_script(), 1));
    run_isolated_done(&env, "run-budget").await;
    let (_, _, child) = env.orchestrator.locate_child("child-0").unwrap();
    let session = SessionId::try_from(child.session_id).unwrap();
    let handle = env.manager.get_session(session).unwrap().unwrap();
    let identity = handle.orchestrator_child_identity_get().unwrap().unwrap();
    assert_eq!(
        identity.permissions,
        crate::caps::class_projection(&child.permissions)
    );
}
