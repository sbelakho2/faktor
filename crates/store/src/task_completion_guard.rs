//! Caller-side typed validation of the durable completion record
//! ([`Store::task_complete_verified`]): the writer job executes SQL only, so
//! the `verification_record` JSON columns must be decoded with the SAME typed
//! mapper the completion transaction used before that refactor
//! ([`verification_record_map`], whose `deny_unknown_fields` types enforce the
//! exact column shapes) HERE, on the caller's thread. A row carrying valid
//! JSON of the WRONG SHAPE therefore refuses [`StoreError::Corrupt`] with the
//! mapper's labels again instead of being silently accepted.
//!
//! The guard's verdict is a `StoreResult` captured by the job, never unwrapped
//! caller-side: the job consults it at the exact position the old typed decode
//! occupied — after the task-missing/revision/not-verifying refusals and the
//! record presence read — so every existing refusal keeps its precedence even
//! when a corrupt record and a refused task coexist.
//!
//! The caller-side read also carries the raw column texts and scalars into the
//! transaction, which re-checks them with [`record_unchanged`] (raw binds
//! only). `task_id`, `revision`, `workspace_id`, `worktree_id` and every JSON
//! column are insert-only — the single UPDATE the store ever runs on
//! `verification_record` is the `Running -> Passed|Failed` status CAS of
//! [`Store::verification_record_finalize`] — so the CAS can never refuse a
//! legitimate sequence. The one mutable column is `status`: a finalize
//! interleaving between this validation and the transaction honestly refuses
//! the prepared `StoreError::Conflict`, and the caller re-reads and retries.

use super::*;

/// One completion record decoded and prepared on the caller's thread: the
/// typed decode accepted every column
/// ([`read_and_validate_completion_record`]), and these raw values ride along
/// so the writer job's CAS can re-check the durable row byte-for-byte without
/// decoding anything on the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedRecordShape {
    pub task_id: i64,
    pub revision: i64,
    pub workspace_id: i64,
    pub worktree_id: i64,
    pub criteria_json: String,
    pub checks_json: String,
    pub changed_files_json: String,
    pub unrelated_changes_json: String,
    pub reviewer_json: Option<String>,
    pub status: String,
}

/// Read one `verification_record` row and run the full typed decode
/// ([`verification_record_map`]) on the caller's thread: JSON parse failures,
/// `deny_unknown_fields` shape violations and invalid ids/statuses surface as
/// the mapper's typed [`StoreError::Corrupt`] labels, exactly as they did when
/// the decode ran inside the completion transaction. `None` when the row does
/// not exist.
pub(crate) fn read_and_validate_completion_record(
    conn: &Connection,
    record_id_raw: i64,
) -> StoreResult<Option<PreparedRecordShape>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, revision, workspace_id, worktree_id, tree_hash,
                criteria_json, checks_json, changed_files_json,
                unrelated_changes_json, reviewer_json, status,
                started_ms, completed_ms
         FROM verification_record WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![record_id_raw])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    // The typed verdict IS the validation; only the raw columns are kept.
    verification_record_map(row)?;
    Ok(Some(PreparedRecordShape {
        task_id: row.get(1)?,
        revision: row.get(2)?,
        workspace_id: row.get(3)?,
        worktree_id: row.get(4)?,
        criteria_json: row.get(6)?,
        checks_json: row.get(7)?,
        changed_files_json: row.get(8)?,
        unrelated_changes_json: row.get(9)?,
        reviewer_json: row.get(10)?,
        status: row.get(11)?,
    }))
}

/// The completion transaction's CAS: re-read the durable record and demand
/// byte/scalar equality with every prepared column. `false` when nothing was
/// prepared (no row at validation time) or when the row changed — including
/// the legal `Running -> Passed` finalize racing the caller — and the job maps
/// it to the prepared typed conflict. SQL and binds only: this runs on the
/// writer owner and must not decode, parse, format or allocate.
pub(crate) fn record_unchanged(
    conn: &Connection,
    record_id_raw: i64,
    prepared: Option<&PreparedRecordShape>,
) -> StoreResult<bool> {
    let Some(p) = prepared else {
        return Ok(false);
    };
    let mut stmt = conn.prepare(
        "SELECT task_id = ?2 AND revision = ?3 AND workspace_id = ?4
                AND worktree_id = ?5 AND criteria_json = ?6 AND checks_json = ?7
                AND changed_files_json = ?8 AND unrelated_changes_json = ?9
                AND reviewer_json IS ?10 AND status = ?11
         FROM verification_record WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![
        record_id_raw,
        p.task_id,
        p.revision,
        p.workspace_id,
        p.worktree_id,
        p.criteria_json,
        p.checks_json,
        p.changed_files_json,
        p.unrelated_changes_json,
        p.reviewer_json,
        p.status
    ])?;
    match rows.next()? {
        Some(row) => Ok(row.get::<_, i64>(0)? == 1),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tmp_store() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        (dir, store)
    }

    fn seed_task(
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

    /// Walk the machine into `Verifying` through the legal store edges (a row
    /// can never be created completion-relevant).
    fn seed_verifying(
        store: &Store,
        session_id: SessionId,
        task_id: TaskId,
        criteria: Vec<String>,
    ) -> TaskRow {
        let mut row = seed_task(store, session_id, task_id, criteria, TaskState::Pending);
        for state in [
            TaskState::Running,
            TaskState::NeedsVerification,
            TaskState::Verifying,
        ] {
            row.state = state;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
        }
        row
    }

    fn passing_record(task: &TaskRow, ws: WorkspaceId, wt: WorktreeId) -> VerificationRecordRow {
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
            checks: vec![CheckExecution {
                check: "compile".into(),
                program: "cargo".into(),
                args: vec!["check".into()],
                category: "required".into(),
                required: true,
                status: VerificationStatus::Passed,
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
            reviewer: None,
            status: VerificationStatus::Passed,
            started_ms: 1,
            completed_ms: None,
        }
    }

    /// A record row carrying valid JSON of the WRONG SHAPE must refuse
    /// `Corrupt` with the typed mapper's own labels — the restore of the
    /// decode the SQL-only job gave up — and the typed read path agrees on the
    /// same row.
    #[test]
    fn wrong_shape_completion_record_columns_refuse_corrupt() {
        for (column, bad) in [
            ("checks_json", r#"{"x": 1}"#),
            ("changed_files_json", r#"["a"]"#),
            ("unrelated_changes_json", r#"{"x": 1}"#),
        ] {
            let (_d, store) = tmp_store();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into()]);
            let rec_id = store
                .verification_record_put(&passing_record(&task, ws, WorktreeId::new(1)))
                .unwrap();
            {
                let conn = store.raw_conn();
                conn.execute(
                    &format!("UPDATE verification_record SET {column} = ?1 WHERE id = ?2"),
                    params![bad, rec_id.raw() as i64],
                )
                .unwrap();
            }
            let stem = column.trim_end_matches("_json");
            match store.task_complete_verified(s.id, task.task_id, task.revision, rec_id, 5) {
                Err(StoreError::Corrupt(msgs)) => assert!(
                    msgs.iter().any(|m| m.contains(stem)),
                    "{column}: refusal must name the column: {msgs:?}"
                ),
                other => panic!("{column}: wrong-shape JSON must refuse Corrupt: {other:?}"),
            }
            match store.verification_record_get(rec_id) {
                Err(StoreError::Corrupt(msgs)) => assert!(
                    msgs.iter().any(|m| m.contains(stem)),
                    "{column}: the typed mapper must agree: {msgs:?}"
                ),
                other => panic!("{column}: typed read must refuse Corrupt: {other:?}"),
            }
            let row = store.get_task(s.id, task.task_id).unwrap().unwrap();
            assert_eq!(row.state, TaskState::Verifying, "{column}: zero mutation");
            assert_eq!(row.revision, task.revision, "{column}: zero mutation");
        }
    }

    /// The durable record changing between the caller-side validation and the
    /// transaction is refused typed with ZERO task/record mutation. The change
    /// is injected through a second connection while the completion job is
    /// parked behind a writer gate, so the interleaving is deterministic: the
    /// validation read strictly precedes the enqueue.
    #[test]
    fn record_changed_between_validation_and_write_conflicts_with_zero_mutation() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into()]);
        let rec_id = store
            .verification_record_put(&passing_record(&task, ws, WorktreeId::new(1)))
            .unwrap();

        // Park the single writer owner on a gate job; the completion call
        // validates the record caller-side, then queues behind the gate.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let gate = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .writer_debug_job("completion_cas_gate", move |_conn| {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    })
                    .unwrap()
            })
        };
        entered_rx.recv().unwrap();
        let completion = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                store.task_complete_verified(s.id, task.task_id, task.revision, rec_id, 7)
            })
        };
        // The enqueue strictly follows the validation read: once the job is
        // pending, the OLD row has already been decoded — change it now.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while store.writer_telemetry().pending_depth == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "completion job never queued"
            );
            std::thread::yield_now();
        }
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE verification_record SET reviewer_json = ?1 WHERE id = ?2",
                params![r#"{"verdict": "pass"}"#, rec_id.raw() as i64],
            )
            .unwrap();
        }
        release_tx.send(()).unwrap();
        let outcome = completion.join().unwrap();
        gate.join().unwrap();
        match outcome {
            Err(StoreError::Conflict(msg)) => assert!(
                msg.contains("changed before write"),
                "refusal must carry the prepared conflict: {msg}"
            ),
            other => panic!("record changed after validation must refuse typed: {other:?}"),
        }
        let task_row = store.get_task(s.id, task.task_id).unwrap().unwrap();
        assert_eq!(task_row.state, TaskState::Verifying, "zero task mutation");
        assert_eq!(task_row.revision, task.revision, "zero task mutation");
        let record = store.verification_record_get(rec_id).unwrap().unwrap();
        assert_eq!(
            record.status,
            VerificationStatus::Passed,
            "record status untouched by the refusal"
        );
        assert_eq!(record.checks.len(), 1, "record untouched by the refusal");
        assert_eq!(
            record.reviewer,
            Some(serde_json::json!({"verdict": "pass"})),
            "the refusal did not revert the concurrent write"
        );
    }
}
