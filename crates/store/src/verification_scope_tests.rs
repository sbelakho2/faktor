//! Adversarial isolation tests for the v43 session-scoped verification
//! records. Two sessions — same workspace, different workspaces, or different
//! stores (organizations) — must never read, finalize, or complete through
//! each other's records; unprovable legacy rows must refuse typed, never be
//! guessed into a scope.

use super::*;

fn seed_verifying(
    store: &Store,
    session_id: SessionId,
    task_id: TaskId,
    criteria: Vec<String>,
) -> TaskRow {
    let mut row = TaskRow {
        task_id,
        session_id,
        goal: "isolation goal".into(),
        acceptance_criteria: criteria,
        plan: vec![],
        attachments: vec![],
        contract: Default::default(),
        max_tokens: None,
        max_turns: None,
        spent_tokens: 0,
        spent_turns: 0,
        state: TaskState::Pending,
        revision: TaskRevision::new(1),
        created_ms: 1,
        updated_ms: 1,
    };
    store.upsert_task(&row).unwrap();
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

fn passing_record(task: &TaskRow, workspace_id: WorkspaceId) -> VerificationRecordRow {
    VerificationRecordRow {
        id: VerificationRecordId::new(1),
        session_id: Some(task.session_id),
        task_id: task.task_id,
        revision: task.revision,
        workspace_id,
        worktree_id: WorktreeId::new(1),
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
fn same_workspace_sessions_cannot_read_finalize_or_complete_foreign_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s1 = store.create_session(ws, "a", "p", "m").unwrap();
    let s2 = store.create_session(ws, "b", "p", "m").unwrap();
    // Both standalone sessions own task id 1 — the exact collision the
    // numeric-keyed table used to conflate.
    let t1 = seed_verifying(&store, s1.id, TaskId::new(1), vec!["c1".into()]);
    let t2 = seed_verifying(&store, s2.id, TaskId::new(1), vec!["c1".into()]);
    let rec = store
        .verification_record_put(&passing_record(&t1, ws))
        .unwrap();

    // Reads: the owner sees it, the same-workspace sibling sees nothing.
    assert_eq!(
        store
            .verification_record_list_by_task(s1.id, TaskId::new(1))
            .unwrap()
            .len(),
        1
    );
    assert!(store
        .verification_record_list_by_task(s2.id, TaskId::new(1))
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .verification_record_get(s2.id, rec)
            .unwrap()
            .map(|r| r.id),
        None
    );
    // Foreign is reported EXACTLY like absence (non-leaking).
    assert_eq!(
        store.verification_record_lookup(s2.id, rec).unwrap(),
        VerificationRecordLookup::Missing
    );

    // Writes: finalize through the sibling is a typed no-op.
    assert_eq!(
        store
            .verification_record_finalize(s2.id, rec, VerificationStatus::Failed, 9)
            .unwrap(),
        Err(RecordFinalizeRefusal::Missing { record_id: rec })
    );

    // Completion: the sibling's matching task/revision cannot consume the
    // owner's passing record; the row stays Verifying and unchanged.
    let refusal = store
        .task_complete_verified(s2.id, t2.task_id, t2.revision, rec, 10)
        .unwrap()
        .unwrap_err();
    assert_eq!(
        refusal,
        TaskCompletionRefusal::RecordMissing { record_id: rec }
    );
    let sibling = store.get_task(s2.id, TaskId::new(1)).unwrap().unwrap();
    assert_eq!(sibling.state, TaskState::Verifying);
    assert_eq!(sibling.revision, t2.revision);
    // The OWNER still completes with its own record.
    let done = store
        .task_complete_verified(s1.id, t1.task_id, t1.revision, rec, 11)
        .unwrap()
        .unwrap();
    assert_eq!(done.state, TaskState::VerifiedComplete);
}

#[test]
fn different_workspaces_and_org_stores_never_cross_verification_records() {
    // Different workspaces of ONE daemon.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws1 = store.create_workspace("/w1").unwrap();
    let ws2 = store.create_workspace("/w2").unwrap();
    let a = store.create_session(ws1, "a", "p", "m").unwrap();
    let b = store.create_session(ws2, "b", "p", "m").unwrap();
    let ta = seed_verifying(&store, a.id, TaskId::new(1), vec!["c1".into()]);
    let tb = seed_verifying(&store, b.id, TaskId::new(1), vec!["c1".into()]);
    let ra = store
        .verification_record_put(&passing_record(&ta, ws1))
        .unwrap();
    let rb = store
        .verification_record_put(&passing_record(&tb, ws2))
        .unwrap();
    assert_eq!(
        store.verification_record_lookup(b.id, ra).unwrap(),
        VerificationRecordLookup::Missing
    );
    assert_eq!(
        store.verification_record_lookup(a.id, rb).unwrap(),
        VerificationRecordLookup::Missing
    );
    assert_eq!(
        store
            .verification_record_list_by_task(a.id, TaskId::new(1))
            .unwrap()[0]
            .id,
        ra
    );
    assert_eq!(
        store
            .verification_record_list_by_task(b.id, TaskId::new(1))
            .unwrap()[0]
            .id,
        rb
    );

    // Different organizations = different daemon stores. Identical numeric
    // ids MUST NOT alias across the store boundary.
    let org_a = tempfile::tempdir().unwrap();
    let org_b = tempfile::tempdir().unwrap();
    let (sa, sb) = {
        let open = |root: &std::path::Path, label: &str| {
            let store = Store::open(root, true).unwrap();
            let ws = store.create_workspace("/shared-root").unwrap();
            let s = store.create_session(ws, label, "p", "m").unwrap();
            let t = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into()]);
            let mut record = passing_record(&t, ws);
            record.criteria[0].evidence = Some(label.into());
            let id = store.verification_record_put(&record).unwrap();
            (store, s.id, id)
        };
        (open(org_a.path(), "org-a"), open(org_b.path(), "org-b"))
    };
    let (store_a, session_a, record_a) = &sa;
    let (store_b, session_b, record_b) = &sb;
    assert_eq!(record_a, record_b, "identical numeric record ids");
    assert_eq!(session_a, session_b, "identical numeric session ids");
    assert_eq!(
        store_a
            .verification_record_get(*session_a, *record_a)
            .unwrap()
            .unwrap()
            .criteria[0]
            .evidence
            .as_deref(),
        Some("org-a")
    );
    assert_eq!(
        store_b
            .verification_record_get(*session_b, *record_b)
            .unwrap()
            .unwrap()
            .criteria[0]
            .evidence
            .as_deref(),
        Some("org-b")
    );
}

#[test]
fn writes_require_their_own_session_scope_and_refuse_forged_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let other = store.create_workspace("/other").unwrap();
    let s = store.create_session(ws, "a", "p", "m").unwrap();
    let foreign = store.create_session(other, "b", "p", "m").unwrap();
    let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into()]);

    // No durable session binding: never written.
    let mut unbound = passing_record(&task, ws);
    unbound.session_id = None;
    assert!(matches!(
        store.verification_record_put(&unbound),
        Err(StoreError::Malformed(message)) if message.contains("session binding")
    ));
    // A session id that does not exist: never written.
    let mut ghost = passing_record(&task, ws);
    ghost.session_id = Some(SessionId::new(99_999));
    assert!(matches!(
        store.verification_record_put(&ghost),
        Err(StoreError::Malformed(message)) if message.contains("not a durable session")
    ));
    // A real session paired with ANOTHER workspace's identity: never written.
    let mut forged = passing_record(&task, ws);
    forged.session_id = Some(foreign.id);
    assert!(matches!(
        store.verification_record_put(&forged),
        Err(StoreError::Malformed(message)) if message.contains("does not own workspace")
    ));
    assert!(store
        .verification_record_list_by_task(s.id, TaskId::new(1))
        .unwrap()
        .is_empty());
    assert!(store
        .verification_record_list_by_task(foreign.id, TaskId::new(1))
        .unwrap()
        .is_empty());
}

#[test]
fn unprovable_legacy_rows_are_excluded_and_refuse_typed_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s1 = store.create_session(ws, "a", "p", "m").unwrap();
    let s2 = store.create_session(ws, "b", "p", "m").unwrap();
    let t1 = seed_verifying(&store, s1.id, TaskId::new(1), vec!["c1".into()]);
    let _t2 = seed_verifying(&store, s2.id, TaskId::new(1), vec!["c1".into()]);
    // Simulate a pre-v43 row whose owner was ambiguous at migration time.
    let rec = store
        .verification_record_put(&passing_record(&t1, ws))
        .unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE verification_record SET session_id = NULL WHERE id = ?1",
            params![rec.raw() as i64],
        )
        .unwrap();

    assert!(store
        .verification_record_list_by_task(s1.id, TaskId::new(1))
        .unwrap()
        .is_empty());
    assert_eq!(
        store.verification_record_lookup(s1.id, rec).unwrap(),
        VerificationRecordLookup::UnboundLegacy { record_id: rec }
    );
    assert_eq!(
        store
            .verification_record_finalize(s1.id, rec, VerificationStatus::Failed, 9)
            .unwrap(),
        Err(RecordFinalizeRefusal::UnboundLegacy { record_id: rec })
    );
    let refusal = store
        .task_complete_verified(s1.id, t1.task_id, t1.revision, rec, 10)
        .unwrap()
        .unwrap_err();
    assert_eq!(
        refusal,
        TaskCompletionRefusal::RecordUnboundLegacy { record_id: rec }
    );
    assert_eq!(
        store
            .get_task(s1.id, TaskId::new(1))
            .unwrap()
            .unwrap()
            .state,
        TaskState::Verifying
    );
    let scan = store.verification_record_invariants().unwrap();
    assert_eq!(scan.unbound_legacy_records, 1);
    assert!(scan
        .issues
        .iter()
        .any(|issue| issue.kind == "record_without_session"));
}

#[test]
fn session_mismatched_workspace_row_refuses_scope_mismatch_without_leaking() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws1 = store.create_workspace("/w1").unwrap();
    let ws2 = store.create_workspace("/w2").unwrap();
    let s1 = store.create_session(ws1, "a", "p", "m").unwrap();
    let _s2 = store.create_session(ws2, "b", "p", "m").unwrap();
    let t1 = seed_verifying(&store, s1.id, TaskId::new(1), vec!["c1".into()]);
    let rec = store
        .verification_record_put(&passing_record(&t1, ws1))
        .unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE verification_record SET workspace_id = ?1 WHERE id = ?2",
            params![ws2.raw() as i64, rec.raw() as i64],
        )
        .unwrap();
    // The caller's OWN session id names the row, so a scope disagreement is
    // a typed corruption signal, not a cross-session existence oracle.
    assert_eq!(
        store.verification_record_lookup(s1.id, rec).unwrap(),
        VerificationRecordLookup::ScopeMismatch { record_id: rec }
    );
    assert_eq!(
        store
            .verification_record_finalize(s1.id, rec, VerificationStatus::Failed, 9)
            .unwrap(),
        Err(RecordFinalizeRefusal::ScopeMismatch { record_id: rec })
    );
    let refusal = store
        .task_complete_verified(s1.id, t1.task_id, t1.revision, rec, 10)
        .unwrap()
        .unwrap_err();
    assert_eq!(
        refusal,
        TaskCompletionRefusal::RecordScopeMismatch { record_id: rec }
    );
}
