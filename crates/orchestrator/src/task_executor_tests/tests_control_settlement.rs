use super::*;

/// Point 5: a rollback restores only paths still equal to OUR written
/// candidate hash — a post-landing user edit is never overwritten and is
/// reported as a rollback conflict.
#[tokio::test]
pub(crate) async fn rollback_never_overwrites_later_user_edit() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts(
            "pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
            "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        ),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::IntegrationApply { after: 1 }));
    let run_id = start_two_child_run(&env, "rollback keeps edits");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let txn = latest_txn(&env, &run_id).expect("landing txn durable");
    assert_eq!(txn.applied_count, 1);
    let applied_path = txn.paths[0].path.clone();
    let pending_path = txn.paths[1].path.clone();
    // A user edits the path we already wrote (ours must never clobber it)
    // and makes the pending path diverge from the base (forcing the
    // rollback on resume).
    std::fs::write(
        env.owner_root.join(&applied_path),
        "user post-landing edit\n",
    )
    .unwrap();
    std::fs::write(env.owner_root.join(&pending_path), "hostile drift\n").unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("a late per-path conflict rolls back");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert_eq!(
        std::fs::read(env.owner_root.join(&applied_path)).unwrap(),
        b"user post-landing edit\n",
        "the rollback never overwrote the later user edit"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join(&pending_path)).unwrap(),
        b"hostile drift\n",
        "a never-applied path is left alone"
    );
    let txn = latest_txn(&env, &run_id).unwrap();
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::RolledBack
    );
    let applied = txn.paths.iter().find(|p| p.path == applied_path).unwrap();
    assert_eq!(
        applied.state,
        faktor_session::ledger::IntegrationPathTxnState::RollbackConflict
    );
}

/// Point 6: the landed owner digest must EQUAL the verified candidate
/// snapshot; an external edit before the final digest check rolls our
/// writes back and refuses typed (never a "merge returned ok" completion).
#[tokio::test]
pub(crate) async fn final_owner_digest_must_equal_verified_candidate() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts(
            "pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
            "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        ),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterFinalOwnerSnapshot));
    let run_id = start_two_child_run(&env, "final digest equality");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let candidate_digest = root_digest(&run_candidate_root(&env, &run_id));
    assert_eq!(
        owner_digest(&env),
        candidate_digest,
        "the landing applied every path before the crash"
    );
    // An external edit lands exactly between the last apply and the final
    // equality check.
    std::fs::write(env.owner_root.join("late.txt"), "concurrent edit\n").unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("a moved owner root can never complete");
    assert!(matches!(err, ExecError::WorkspaceDrift(_)), "{err}");
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    assert_ne!(
        h.get_task(h.task_id().unwrap()).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        env.owner_root.join("late.txt").is_file(),
        "external edit kept"
    );
    assert!(
        !env.owner_root.join("child_a.rs").exists(),
        "our applied paths were rolled back after the failed equality"
    );
    let txn = latest_txn(&env, &run_id).unwrap();
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::RolledBack
    );
}
