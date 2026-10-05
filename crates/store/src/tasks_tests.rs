#[cfg(test)]
mod task_tests {
    use super::super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), true).unwrap();
        (dir, s)
    }

    #[test]
    fn corrupt_tool_run_args_and_recovery_return_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .start_tool_run(
                s.id,
                OpId::new(1),
                "write_file",
                serde_json::json!({"path": "/a"}),
                serde_json::json!({"strategy": "verify_hash"}),
                None,
                None,
            )
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE tool_run SET args = 'broken{' WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.pending_tool_runs(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt tool_run args must error, not panic: {other:?}"),
        }
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE tool_run SET args = '{\"a\":1}', recovery = 'broken{' WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.pending_tool_runs(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt tool_run recovery must error, not panic: {other:?}"),
        }
    }

    #[test]
    fn corrupt_task_ledger_returns_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_task_ledger(s.id, serde_json::json!({"tasks": []}))
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task_ledger SET ledger = 'garbage' WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_task_ledger(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt ledger must error, not panic: {other:?}"),
        }
    }

    #[test]
    fn tool_run_recovery_columns_roundtrip_and_attempts() {
        // v7: the replay descriptor, the physical-attempt counter and the
        // workspace-write postcondition ride the tool_run row.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let op = OpId::new(7);
        store
            .start_tool_run(
                s.id,
                op,
                "echo",
                serde_json::json!({"x": 1}),
                serde_json::json!({"strategy": "idempotent"}),
                None,
                Some(serde_json::json!({"tool_name": "echo", "validated_args": {"x": 1}})),
            )
            .unwrap();
        let pending = store.pending_tool_runs(s.id).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempt, 0, "the original run is attempt 0");
        assert_eq!(
            pending[0].replay_descriptor.as_ref().unwrap()["tool_name"],
            "echo"
        );
        // Postcondition annotation (recorded at execution end, pre-finish).
        store
            .record_tool_postcondition(
                s.id,
                op,
                &serde_json::json!({
                    "workspace_id": ws.raw(),
                    "worktree_id": 1,
                    "relative_path": "a.txt",
                    "expected_hash": "ab".repeat(32),
                }),
            )
            .unwrap();
        assert_eq!(
            store.pending_tool_runs(s.id).unwrap()[0]
                .postcondition
                .as_ref()
                .unwrap()["relative_path"],
            "a.txt"
        );
        // A replay bumps the attempt counter of the SAME logical row.
        assert_eq!(store.bump_tool_run_attempt(s.id, op).unwrap(), 1);
        // Hostile annotation on a finished row is loud.
        store
            .finish_tool_run(s.id, op, "completed", "applied")
            .unwrap();
        assert!(store.pending_tool_runs(s.id).unwrap().is_empty());
        assert!(store
            .record_tool_postcondition(s.id, op, &serde_json::json!({}))
            .is_err());
        assert!(store.bump_tool_run_attempt(s.id, op).is_err());
    }

    #[test]
    fn tool_run_recovery_columns_and_turn_records_survive_reopen() {
        // Requirement 2c: the descriptor + postcondition + attempt survive a
        // daemon restart and still drive a replay.
        let dir = tempfile::tempdir().unwrap();
        let (sid, op) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let op = OpId::new(31);
            store
                .start_tool_run(
                    s.id,
                    op,
                    "echo",
                    serde_json::json!({"x": 1}),
                    serde_json::json!({"strategy": "idempotent"}),
                    None,
                    Some(serde_json::json!({"tool_name": "echo"})),
                )
                .unwrap();
            store
                .record_tool_postcondition(s.id, op, &serde_json::json!({"relative_path": "a.txt"}))
                .unwrap();
            store
                .start_turn_record(s.id, OpId::new(99), None, Some(2), "p", "m", None)
                .unwrap();
            (s.id, op)
        };
        let store = Store::open(dir.path(), true).unwrap();
        let pending = store.pending_tool_runs(sid).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op_id, op);
        assert_eq!(pending[0].attempt, 0);
        assert_eq!(
            pending[0].replay_descriptor.as_ref().unwrap()["tool_name"],
            "echo"
        );
        assert_eq!(
            pending[0].postcondition.as_ref().unwrap()["relative_path"],
            "a.txt"
        );
        assert_eq!(store.bump_tool_run_attempt(sid, op).unwrap(), 1);
        let rec = store.turn_record_of(sid, OpId::new(99)).unwrap().unwrap();
        assert_eq!(rec.status, TURN_RECORD_ACTIVE);
        assert_eq!(rec.effective_model, "m");
    }

    #[test]
    fn durable_task_spend_sources_are_durable_and_monotone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        assert_eq!(store.session_usage_tokens(s.id).unwrap(), 0);
        assert_eq!(store.turn_completed_count(s.id).unwrap(), 0);
        let op = OpId::new(1);
        store
            .record_provider_call(s.id, op, "p", "m", "started", None, None, None)
            .unwrap();
        // In-flight (not yet completed) calls count their tokens too: a
        // crash can never lose spend that a gate already saw.
        store
            .record_provider_call(s.id, op, "p", "m", "completed", Some(40), Some(10), None)
            .unwrap();
        assert_eq!(store.session_usage_tokens(s.id).unwrap(), 50);
        // TurnCompleted journal events are the durable turn counter.
        store
            .append_event(
                s.id,
                Some(op),
                faktor_core::event::EventKind::TurnCompleted,
                AgentState::ReadyForNextTurn,
                5,
                None,
            )
            .unwrap();
        assert_eq!(store.turn_completed_count(s.id).unwrap(), 1);
        // Session isolation: a second session's spend never leaks.
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "t2", "p", "m").unwrap();
        assert_eq!(store.session_usage_tokens(s2.id).unwrap(), 0);
        assert_eq!(store.turn_completed_count(s2.id).unwrap(), 0);
    }

    // ---- prefix-cache stability columns (v13, audits 65-66) ----
}

#[cfg(test)]
mod typed_ledger_tests {
    use super::super::*;

    pub(crate) fn seed_task(
        store: &Store,
        session_id: SessionId,
        task_id: TaskId,
        criteria: Vec<String>,
        state: TaskState,
    ) -> TaskRow {
        let row = TaskRow {
            task_id,
            session_id,
            goal: "g".into(),
            acceptance_criteria: criteria,
            plan: vec![],
            attachments: vec![],
            max_tokens: None,
            max_turns: None,
            spent_tokens: 0,
            spent_turns: 0,
            state,
            revision: TaskRevision::new(1),
            created_ms: 1,
            updated_ms: 1,
        };
        store.upsert_task(&row).unwrap();
        row
    }

    /// Seed a task and walk the machine into `Verifying` through the legal
    /// store edges (a row can never be created completion-relevant).
    pub(crate) fn seed_verifying(
        store: &Store,
        session_id: SessionId,
        task_id: TaskId,
        criteria: Vec<String>,
    ) -> TaskRow {
        let mut row = seed_task(store, session_id, task_id, criteria, TaskState::Pending);
        let mut bump = |state: TaskState| {
            row.state = state;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
        };
        bump(TaskState::Running);
        bump(TaskState::NeedsVerification);
        bump(TaskState::Verifying);
        row
    }

    pub(crate) fn passing_record(
        task: &TaskRow,
        ws: WorkspaceId,
        wt: WorktreeId,
    ) -> VerificationRecordRow {
        VerificationRecordRow {
            id: VerificationRecordId::new(1),
            task_id: task.task_id,
            revision: task.revision,
            workspace_id: ws,
            worktree_id: wt,
            tree_hash: None,
            criteria: task
                .acceptance_criteria
                .iter()
                .map(|c| CriterionVerification {
                    criterion_key: c.clone(),
                    passed: true,
                    evidence: Some("exit 0".into()),
                    binding: None,
                })
                .collect(),
            checks: vec![],
            changed_files: vec![],
            unrelated_changes: vec![],
            reviewer: None,
            status: VerificationStatus::Passed,
            started_ms: 1,
            completed_ms: None,
        }
    }

    #[test]
    fn completion_path_validates_every_proof_facet_in_one_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into(), "c2".into()]);
        let now = now_ms();
        // (b) missing record: typed refusal, row untouched.
        let miss = store
            .task_complete_verified(
                s.id,
                task.task_id,
                task.revision,
                VerificationRecordId::new(999),
                now,
            )
            .unwrap()
            .unwrap_err();
        assert!(matches!(miss, TaskCompletionRefusal::RecordMissing { .. }));
        let record = passing_record(&task, ws, WorktreeId::new(1));
        let rec_id = store.verification_record_put(&record).unwrap();
        // Happy path: one transaction validates (a)-(g) and completes.
        let done = store
            .task_complete_verified(s.id, task.task_id, task.revision, rec_id, now)
            .unwrap()
            .unwrap();
        assert_eq!(done.state, TaskState::VerifiedComplete);
        assert_eq!(done.revision, task.revision.checked_next().unwrap());
        assert_eq!(store.get_task(s.id, task.task_id).unwrap().unwrap(), done);
        // A second completion is refused: stale revision first, then the
        // machine (the task is no longer Verifying).
        let again = store
            .task_complete_verified(s.id, task.task_id, task.revision, rec_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            again,
            TaskCompletionRefusal::RevisionMismatch { .. }
        ));
        let again = store
            .task_complete_verified(s.id, task.task_id, done.revision, rec_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            again,
            TaskCompletionRefusal::NotVerifying {
                actual: TaskState::VerifiedComplete
            }
        ));
        assert_eq!(
            store
                .get_task(s.id, task.task_id)
                .unwrap()
                .unwrap()
                .revision,
            done.revision,
            "refused completions never bump"
        );

        // (c) wrong task: a record certifying task 1 cannot complete task 2.
        let task2 = seed_verifying(&store, s.id, TaskId::new(2), vec!["c1".into()]);
        let r2 = store
            .task_complete_verified(s.id, task2.task_id, task2.revision, rec_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(r2, TaskCompletionRefusal::RecordWrongTask { .. }));

        // (d) wrong revision: a record certifying task 2's CURRENT revision
        // cannot complete task 2 once the task has moved PAST it (the
        // record must certify the revision being completed).
        let task2_rec = passing_record(&task2, ws, WorktreeId::new(1));
        let t2_id = store.verification_record_put(&task2_rec).unwrap();
        let mut bumped = store.get_task(s.id, TaskId::new(2)).unwrap().unwrap();
        bumped.revision = bumped.revision.checked_next().unwrap();
        store.upsert_task(&bumped).unwrap();
        let stale = store
            .task_complete_verified(s.id, task2.task_id, bumped.revision, t2_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            stale,
            TaskCompletionRefusal::RecordWrongRevision { .. }
        ));

        // (e) Failed record refuses.
        let task3 = seed_verifying(&store, s.id, TaskId::new(3), vec!["c1".into()]);
        let mut failed_rec = passing_record(&task3, ws, WorktreeId::new(1));
        failed_rec.status = VerificationStatus::Failed;
        let f_id = store.verification_record_put(&failed_rec).unwrap();
        let r3 = store
            .task_complete_verified(s.id, task3.task_id, task3.revision, f_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            r3,
            TaskCompletionRefusal::RecordNotPassed {
                status: VerificationStatus::Failed,
                ..
            }
        ));

        // (f) a record missing one criterion refuses with the missing list.
        let task4 = seed_verifying(&store, s.id, TaskId::new(4), vec!["c1".into(), "c2".into()]);
        let mut partial = passing_record(&task4, ws, WorktreeId::new(1));
        partial.criteria.pop();
        let p_id = store.verification_record_put(&partial).unwrap();
        let r4 = store
            .task_complete_verified(s.id, task4.task_id, task4.revision, p_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            r4,
            TaskCompletionRefusal::CriteriaNotCovered { missing, .. }
                if missing == vec!["c2".to_string()]
        ));
        // Extra record criteria are fine: a record covering c1..c2 plus an
        // extra c3 completes.
        let mut extra = passing_record(&task4, ws, WorktreeId::new(1));
        extra.criteria.push(CriterionVerification {
            criterion_key: "c3".into(),
            passed: true,
            evidence: None,
            binding: None,
        });
        let e_id = store.verification_record_put(&extra).unwrap();
        let ok4 = store
            .task_complete_verified(s.id, task4.task_id, task4.revision, e_id, now)
            .unwrap()
            .unwrap();
        assert_eq!(ok4.state, TaskState::VerifiedComplete);
        // A passed=false entry for a task criterion counts as NOT covered.
        let task5 = seed_verifying(&store, s.id, TaskId::new(5), vec!["c1".into()]);
        let mut lying = passing_record(&task5, ws, WorktreeId::new(1));
        lying.criteria[0].passed = false;
        let l_id = store.verification_record_put(&lying).unwrap();
        let r5 = store
            .task_complete_verified(s.id, task5.task_id, task5.revision, l_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            r5,
            TaskCompletionRefusal::CriteriaNotCovered { missing, .. }
                if missing == vec!["c1".to_string()]
        ));

        // (g) worktree mismatch: a record certified against another
        // workspace's identity refuses even when everything else matches.
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "t2", "p", "m").unwrap();
        let task6 = seed_verifying(&store, s2.id, TaskId::new(1), vec!["c1".into()]);
        let mut foreign = passing_record(&task6, ws, WorktreeId::new(1));
        foreign.workspace_id = ws;
        let x_id = store.verification_record_put(&foreign).unwrap();
        let r6 = store
            .task_complete_verified(s2.id, task6.task_id, task6.revision, x_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(r6, TaskCompletionRefusal::WorktreeMismatch { .. }));

        // Revision mismatch of the TASK (expected != actual) is reported
        // before the record is even consulted.
        let stale_expected2 = store
            .task_complete_verified(s.id, task2.task_id, task2.revision, rec_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            stale_expected2,
            TaskCompletionRefusal::RevisionMismatch { .. }
        ));
    }

    #[test]
    fn completion_gate_refuses_every_held_reservation_status_and_keeps_verifying() {
        // The SQL gate is exhaustive over the three budget-holding statuses:
        // each refuses the completion transaction typed with the exact
        // counts and leaves the task row byte-untouched (rollback).
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        for (n, status) in ["reserved", "dispatched", "uncertain"]
            .into_iter()
            .enumerate()
        {
            let task_id = TaskId::new(n as u64 + 1);
            // The reserve is admitted while the task still permits new
            // provider work (Running); the task then walks the legal edges
            // into Verifying — the completion gate must catch the row the
            // accounting pass raced.
            seed_task(&store, s.id, task_id, vec![], TaskState::Running);
            let rid = match store
                .cost_reserve_priced(s.id, task_id, OpId::new(100 + n as u64), 77, now_ms(), None)
                .unwrap()
            {
                CostReserveOutcome::Granted(id) => id,
                other => panic!("{status}: reserve refused: {other:?}"),
            };
            if status != "reserved" {
                assert!(
                    matches!(
                        store.cost_mark_dispatched(rid, now_ms()).unwrap(),
                        CostReservationState::Applied
                    ),
                    "{status}: dispatch marker"
                );
            }
            if status == "uncertain" {
                assert!(
                    matches!(
                        store
                            .cost_mark_uncertain(rid, "race-fault", None, now_ms())
                            .unwrap(),
                        CostReservationState::Applied
                    ),
                    "{status}: uncertain marker"
                );
            }
            let mut row = store.get_task(s.id, task_id).unwrap().unwrap();
            row.state = TaskState::NeedsVerification;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
            row.state = TaskState::Verifying;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
            let verifying_revision = row.revision;
            let record = passing_record(&row, ws, WorktreeId::new(1));
            let rec_id = store.verification_record_put(&record).unwrap();
            let refusal = store
                .task_complete_verified(s.id, task_id, verifying_revision, rec_id, now_ms())
                .unwrap()
                .unwrap_err();
            match (status, refusal) {
                (
                    "reserved",
                    TaskCompletionRefusal::ReservationsHeld {
                        reserved,
                        dispatched,
                        uncertain,
                        reserved_micro,
                        uncertain_micro,
                    },
                ) => {
                    assert_eq!((reserved, dispatched, uncertain), (1, 0, 0));
                    assert_eq!(reserved_micro, 77);
                    assert_eq!(uncertain_micro, 0);
                }
                (
                    "dispatched",
                    TaskCompletionRefusal::ReservationsHeld {
                        reserved,
                        dispatched,
                        uncertain,
                        ..
                    },
                ) => {
                    assert_eq!((reserved, dispatched, uncertain), (0, 1, 0));
                }
                (
                    "uncertain",
                    TaskCompletionRefusal::ReservationsHeld {
                        reserved,
                        dispatched,
                        uncertain,
                        uncertain_micro,
                        ..
                    },
                ) => {
                    assert_eq!((reserved, dispatched, uncertain), (0, 0, 1));
                    assert_eq!(uncertain_micro, 77);
                }
                other => panic!("{status}: wrong refusal: {other:?}"),
            }
            let row = store.get_task(s.id, task_id).unwrap().unwrap();
            assert_eq!(row.state, TaskState::Verifying, "{status}: state moved");
            assert_eq!(row.revision, verifying_revision, "{status}: revision moved");
        }
    }

    #[test]
    fn record_finalize_cas_wins_exactly_once_and_lists_are_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let put = |status: VerificationStatus| {
            store
                .verification_record_put(&VerificationRecordRow {
                    id: VerificationRecordId::new(1),
                    task_id: task.task_id,
                    revision: task.revision,
                    workspace_id: ws,
                    worktree_id: WorktreeId::new(1),
                    tree_hash: Some("ab".repeat(32)),
                    criteria: vec![],
                    checks: vec![CheckExecution {
                        check: "compile".into(),
                        program: "cargo".into(),
                        args: vec!["check".into()],
                        category: "required".into(),
                        required: true,
                        status,
                        started_ms: 1,
                        finished_ms: Some(2),
                        exit: Some(0),
                        summary: None,
                    }],
                    changed_files: vec![FileStateEvidence {
                        path: "src/main.rs".into(),
                        digest_hex: "cd".repeat(32),
                        size: 10,
                    }],
                    unrelated_changes: vec!["vendor/".into()],
                    reviewer: Some(serde_json::json!({"verdict": "pass"})),
                    status,
                    started_ms: 1,
                    completed_ms: None,
                })
                .unwrap()
        };
        let rec_a = put(VerificationStatus::Running);
        let rec_b = put(VerificationStatus::Passed);
        assert_ne!(rec_a, rec_b, "fresh row ids");
        // Deterministic list order (creation order), stable across reads.
        let list = store
            .verification_record_list_by_task(task.task_id)
            .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, rec_a);
        assert_eq!(list[1].id, rec_b);
        assert_eq!(list[0].checks[0].check, "compile");
        assert_eq!(list[1].changed_files[0].size, 10);
        assert_eq!(list[1].reviewer.as_ref().unwrap()["verdict"], "pass");
        let again = store
            .verification_record_list_by_task(task.task_id)
            .unwrap();
        assert_eq!(again, list, "deterministic across reads");
        assert!(store
            .verification_record_list_by_task(TaskId::new(404))
            .unwrap()
            .is_empty());
        // CAS finalize: exactly one Running -> Passed wins.
        assert_eq!(
            store
                .verification_record_finalize(rec_a, VerificationStatus::Passed, 99)
                .unwrap(),
            Ok(())
        );
        // Second attempt on the now-final record: typed refusal with the
        // CURRENT status (a second finalize can never rewind or re-run).
        assert_eq!(
            store
                .verification_record_finalize(rec_a, VerificationStatus::Failed, 100)
                .unwrap(),
            Err(RecordFinalizeRefusal::NotRunning {
                record_id: rec_a,
                current: VerificationStatus::Passed
            })
        );
        // A Pending record cannot finalize either (only Running may).
        let rec_c = put(VerificationStatus::Pending);
        assert_eq!(
            store
                .verification_record_finalize(rec_c, VerificationStatus::Passed, 101)
                .unwrap(),
            Err(RecordFinalizeRefusal::NotRunning {
                record_id: rec_c,
                current: VerificationStatus::Pending
            })
        );
        // A missing record is its own typed refusal.
        assert_eq!(
            store
                .verification_record_finalize(
                    VerificationRecordId::new(909),
                    VerificationStatus::Passed,
                    1
                )
                .unwrap(),
            Err(RecordFinalizeRefusal::Missing {
                record_id: VerificationRecordId::new(909)
            })
        );
        // Only Passed/Failed may finalize; Unavailable is malformed.
        assert!(matches!(
            store.verification_record_finalize(rec_c, VerificationStatus::Running, 1),
            Err(StoreError::Malformed(_))
        ));
        // The finalized row read back with its completed_ms.
        let row = store.verification_record_get(rec_a).unwrap().unwrap();
        assert_eq!(row.status, VerificationStatus::Passed);
        assert_eq!(row.completed_ms, Some(99));
    }

    #[test]
    fn records_and_completion_survive_reopen_like_a_crash_boundary() {
        // A crash can happen anywhere between record creation and the
        // completion transaction. Each boundary leaves a consistent store:
        // after a reopen the exact same completion either still applies or
        // is refused by the machine — never half-applied.
        let dir = tempfile::tempdir().unwrap();
        let (sid, tid, rec_id) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into()]);
            let rec = passing_record(&task, ws, WorktreeId::new(1));
            let rec_id = store.verification_record_put(&rec).unwrap();
            // Crash here: record durably written, task still Verifying.
            (s.id, task.task_id, rec_id)
        };
        let store = Store::open(dir.path(), true).unwrap();
        // The record survived and the task reads exactly as it crashed.
        assert_eq!(
            store
                .verification_record_get(rec_id)
                .unwrap()
                .unwrap()
                .status,
            VerificationStatus::Passed
        );
        let task = store.get_task(sid, tid).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Verifying);
        let done = store
            .task_complete_verified(sid, tid, task.revision, rec_id, now_ms())
            .unwrap()
            .unwrap();
        assert_eq!(done.state, TaskState::VerifiedComplete);
        assert_eq!(done.revision, task.revision.checked_next().unwrap());
        drop(store);
        // Crash after completion: reopen shows the completed row, and a
        // re-completion attempt is refused without touching it.
        let store = Store::open(dir.path(), true).unwrap();
        let task = store.get_task(sid, tid).unwrap().unwrap();
        assert_eq!(task.state, TaskState::VerifiedComplete);
        assert_eq!(task.revision, TaskRevision::new(5));
        let again = store
            .task_complete_verified(sid, tid, task.revision, rec_id, now_ms())
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            again,
            TaskCompletionRefusal::NotVerifying {
                actual: TaskState::VerifiedComplete
            }
        ));
        // A hostile PARTIAL write through raw SQL (state flipped out of
        // Verifying WITHOUT a revision bump) is caught by the machine on the
        // next completion attempt.
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET state = ?1 WHERE session_id = ?2 AND task_id = ?3",
                params![
                    serde_json::to_string(&TaskState::Running).unwrap(),
                    sid.raw() as i64,
                    tid.raw() as i64
                ],
            )
            .unwrap();
        }
        let task = store.get_task(sid, tid).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Running);
        let refused = store
            .task_complete_verified(sid, tid, task.revision, rec_id, now_ms())
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            refused,
            TaskCompletionRefusal::NotVerifying { .. }
        ));
    }

    #[test]
    fn corrupt_task_revision_reads_as_corruption_never_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Pending);
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET revision = 0 WHERE session_id = 1 AND task_id = 1",
                [],
            )
            .unwrap();
        }
        assert!(matches!(
            store.get_task(s.id, TaskId::new(1)),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupt_task_turn_counts_are_typed_field_errors_never_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Pending);
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET max_turns = ?1 WHERE session_id = ?2 AND task_id = 1",
                params![i64::from(u32::MAX) + 1, s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_task(s.id, TaskId::new(1)) {
            Err(StoreError::Corrupt(msgs)) => {
                assert!(
                    msgs.iter().any(|m| m.contains("max_turns")),
                    "the refusal must name the max_turns field: {msgs:?}"
                );
            }
            other => panic!("oversize persisted max_turns must be a typed error: {other:?}"),
        }
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET max_turns = NULL, spent_turns = -1
                 WHERE session_id = ?1 AND task_id = 1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_task(s.id, TaskId::new(1)) {
            Err(StoreError::Corrupt(msgs)) => {
                assert!(
                    msgs.iter().any(|m| m.contains("spent_turns")),
                    "the refusal must name the spent_turns field: {msgs:?}"
                );
            }
            other => panic!("negative persisted spent_turns must be a typed error: {other:?}"),
        }
    }

    #[test]
    fn u32_turn_counts_round_trip_without_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let mut row = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Pending);
        row.max_tokens = Some(i64::MAX as u64);
        row.max_turns = Some(u32::MAX);
        row.spent_tokens = i64::MAX as u64;
        row.spent_turns = u32::MAX;
        store.upsert_task(&row).unwrap();
        assert_eq!(
            store.get_task(s.id, TaskId::new(1)).unwrap(),
            Some(row),
            "the u32 extremes must survive the i64 column round-trip exactly"
        );
        assert_eq!(
            store.list_tasks(s.id).unwrap()[0].spent_turns,
            u32::MAX,
            "list_tasks shares the checked mapping"
        );
    }

    /// Free budget of one task = cap - spent - holding predictions
    /// (reserved + dispatched + uncertain), the store's own formula.
    pub(crate) fn free_micro(store: &Store, session: SessionId, task: TaskId) -> u64 {
        let row = store.cost_task_row(session, task).unwrap().unwrap();
        let max = row.max_cost_micro.unwrap_or(0);
        let rows = store.cost_reservations_of(session, task, i64::MAX).unwrap();
        let held: u64 = rows
            .iter()
            .filter(|r| matches!(r.status.as_str(), "reserved" | "dispatched" | "uncertain"))
            .map(|r| r.predicted_micro)
            .sum();
        max.saturating_sub(row.spent_cost_micro)
            .saturating_sub(held)
    }

    #[test]
    fn attempt_reservations_and_provider_rows_key_by_attempt_op_id() {
        // (iii) Two physical attempts of the SAME logical op carry distinct
        // attempt op ids, separate reservations (each holding its own
        // prediction, each refundable independently) and separate
        // provider-call rows through the attempt writers.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        store.cost_task_cap_set(s.id, tid, Some(10_000)).unwrap();
        let now = now_ms();
        let logical = OpId::new(700);
        let a1 = faktor_core::op::ModelCallAttempt::new(logical, OpId::new(701), 0).unwrap();
        let a2 = faktor_core::op::ModelCallAttempt::new(logical, OpId::new(702), 1).unwrap();
        let CostReserveOutcome::Granted(r1) = store
            .cost_reserve_attempt(s.id, tid, &a1, 1_000, now, None)
            .unwrap()
        else {
            panic!("attempt 1 reserve granted")
        };
        let CostReserveOutcome::Granted(r2) = store
            .cost_reserve_attempt(s.id, tid, &a2, 2_000, now, None)
            .unwrap()
        else {
            panic!("attempt 2 reserve granted")
        };
        assert_ne!(r1, r2, "one reservation per attempt");
        assert_eq!(free_micro(&store, s.id, tid), 7_000, "both holds count");
        let rows = store.cost_reservations_of(s.id, tid, 10).unwrap();
        assert_eq!(rows.len(), 2);
        let row1 = rows.iter().find(|r| r.reservation_id == r1).unwrap();
        assert_eq!(row1.attempt_op_id, Some(OpId::new(701)));
        assert_eq!(row1.parent_op_id, Some(logical));
        assert_eq!(
            row1.op_id,
            OpId::new(701),
            "attempt rows key by their own op"
        );
        let row2 = rows.iter().find(|r| r.reservation_id == r2).unwrap();
        assert_eq!(row2.attempt_op_id, Some(OpId::new(702)));
        assert_ne!(row1.attempt_op_id, row2.attempt_op_id);

        // One provider-call row per attempt through the attempt writer.
        let p1 = store
            .record_provider_call_attempt(
                s.id,
                &a1,
                Some(r1),
                "fake",
                "m",
                "completed",
                Some(10),
                Some(20),
                None,
            )
            .unwrap();
        let p2 = store
            .record_provider_call_attempt(
                s.id,
                &a2,
                Some(r2),
                "fake",
                "m",
                "completed",
                Some(30),
                Some(40),
                None,
            )
            .unwrap();
        assert_ne!(p1, p2);
        let calls: Vec<(i64, i64, i64, i64, i64)> = {
            let conn = store.read().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT op_id, parent_model_call_op_id, attempt_op_id,
                            attempt_ordinal, reservation_id
                     FROM provider_call WHERE session_id = ?1 ORDER BY id ASC",
                )
                .unwrap();
            let mut out = Vec::new();
            let rows = stmt
                .query_map([s.id.raw() as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap();
            for r in rows {
                out.push(r.unwrap());
            }
            out
        };
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            (700, 700, 701, 0, r1),
            "attempt 1 row: shared logical op in op_id + parent, attempt 701"
        );
        assert_eq!(
            calls[1],
            (700, 700, 702, 1, r2),
            "attempt 2 row: same logical parent, own attempt 702, own reservation"
        );
        // Independent refunds: releasing attempt 1 leaves attempt 2's hold.
        store.cost_refund(r1, now).unwrap();
        assert_eq!(free_micro(&store, s.id, tid), 8_000);
        assert_eq!(
            store
                .cost_reservations_of(s.id, tid, 10)
                .unwrap()
                .iter()
                .find(|r| r.reservation_id == r1)
                .unwrap()
                .status,
            "refunded"
        );
        store.cost_refund(r2, now).unwrap();
        assert_eq!(free_micro(&store, s.id, tid), 10_000);
    }
}
