use super::*;

fn setup() -> (tempfile::TempDir, Store, SessionId) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/vj").unwrap();
    let sid = store.create_session(ws, "vj", "p", "m").unwrap().id;
    (dir, store, sid)
}

fn attempt(sid: SessionId, op: u64, revision: u64) -> VerificationAttemptRow {
    VerificationAttemptRow {
        session_id: sid,
        task_id: TaskId::new(1),
        attempt_op_id: op,
        task_revision: TaskRevision::new(revision),
        workspace_root: "/vj".into(),
        environment_fingerprint_json: None,
        created_ms: 10,
    }
}

fn check(
    sid: SessionId,
    op: u64,
    check_id: &str,
    ordinal: u32,
    inline: Option<&str>,
) -> VerificationJobRow {
    let is_inline = inline.is_some();
    VerificationJobRow {
        session_id: sid,
        task_id: TaskId::new(1),
        attempt_op_id: op,
        check_id: check_id.into(),
        ordinal,
        task_revision: TaskRevision::new(3),
        workspace_root: "/vj".into(),
        kind: if is_inline {
            String::new()
        } else {
            "test".into()
        },
        command: format!("ctest {check_id}"),
        program: if is_inline {
            String::new()
        } else {
            "ctest".into()
        },
        args_json: "[]".into(),
        spec_json: if is_inline {
            None
        } else {
            Some("{\"id\":\"x\",\"program\":\"ctest\",\"args\":[]}".into())
        },
        budget_ms: if is_inline { 0 } else { 1_000 },
        inline_status: inline.map(str::to_string),
        state: inline.unwrap_or("queued").to_string(),
        result_json: None,
        note: None,
        op_id: None,
        environment_fingerprint_json: None,
        created_ms: 10,
        updated_ms: 10,
        finished_ms: None,
    }
}

#[test]
fn attempt_begin_is_atomic_idempotent_and_conflicts_on_open_job() {
    let (_d, store, sid) = setup();
    let changed = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
    let checks = vec![
        check(sid, 1, "inline_ok", 0, Some("passed")),
        check(sid, 1, "bg", 1, None),
    ];
    assert!(store
        .verification_attempt_begin(&attempt(sid, 1, 3), &changed, &checks)
        .unwrap());
    // Idempotent retry: the second begin writes nothing.
    assert!(!store
        .verification_attempt_begin(&attempt(sid, 1, 3), &changed, &checks)
        .unwrap());
    let view = store
        .verification_attempt_get(sid, TaskId::new(1), 1)
        .unwrap()
        .unwrap();
    assert_eq!(view.changed, changed, "changed files ordered + intact");
    assert_eq!(view.checks.len(), 2, "inline + background checks survive");
    assert_eq!(view.checks[0].state, "passed");
    assert_eq!(view.checks[0].inline_status.as_deref(), Some("passed"));
    assert_eq!(view.checks[1].state, "queued");
    assert_eq!(view.checks[1].inline_status, None);
    assert_eq!(
        store
            .verification_jobs_open(sid, TaskId::new(1))
            .unwrap()
            .len(),
        1,
        "inline checks are never open jobs"
    );
    // An open background check of the OLD attempt refuses a new begin.
    let err = store
        .verification_attempt_begin(&attempt(sid, 2, 3), &[], &[check(sid, 2, "bg", 0, None)])
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    // The commit point is atomic: a rejected second begin left no rows.
    assert!(store
        .verification_attempt_get(sid, TaskId::new(1), 2)
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .verification_attempt_cancel(sid, TaskId::new(1), 1, "superseded", 9)
            .unwrap(),
        1
    );
    assert!(store
        .verification_attempt_begin(&attempt(sid, 2, 3), &[], &[check(sid, 2, "bg", 0, None)])
        .unwrap());
    let current = store
        .verification_attempt_current(sid, TaskId::new(1))
        .unwrap()
        .unwrap();
    assert_eq!(current.attempt.attempt_op_id, 2);
}

#[test]
fn claim_resolve_exactly_once_and_supersede_freezes_attempt_n() {
    let (_d, store, sid) = setup();
    assert!(store
        .verification_attempt_begin(&attempt(sid, 10, 3), &[], &[check(sid, 10, "bg", 0, None)],)
        .unwrap());
    let claimed = store
        .verification_job_claim(sid, TaskId::new(1), 10, "bg", 99, 11)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.state, "running");
    assert_eq!(claimed.op_id, Some(99));
    // Double claim is a typed refusal.
    match store
        .verification_job_claim(sid, TaskId::new(1), 10, "bg", 100, 11)
        .unwrap()
    {
        Err(VerificationJobRefusal::NotOpen { state, .. }) => assert_eq!(state, "running"),
        other => panic!("expected NotOpen, got {other:?}"),
    }
    let resolved = store
        .verification_job_resolve(
            sid,
            TaskId::new(1),
            10,
            "bg",
            "passed",
            None,
            Some("{\"status\":\"passed\"}"),
            12,
        )
        .unwrap()
        .unwrap();
    assert_eq!(resolved.state, "passed");
    assert_eq!(
        resolved.result_json.as_deref(),
        Some("{\"status\":\"passed\"}")
    );
    // Resolve exactly once.
    match store
        .verification_job_resolve(sid, TaskId::new(1), 10, "bg", "failed", None, None, 13)
        .unwrap()
    {
        Err(VerificationJobRefusal::NotOpen { state, .. }) => assert_eq!(state, "passed"),
        other => panic!("expected NotOpen, got {other:?}"),
    }
    // Attempt N+1 with a DIFFERENT check commits; attempt N freezes.
    assert!(store
        .verification_attempt_begin(&attempt(sid, 11, 3), &[], &[check(sid, 11, "bg2", 0, None)],)
        .unwrap());
    match store
        .verification_job_resolve(
            sid,
            TaskId::new(1),
            10,
            "bg",
            "failed",
            None,
            Some("{\"status\":\"failed\"}"),
            14,
        )
        .unwrap()
    {
        Err(VerificationJobRefusal::Superseded {
            attempt_op_id,
            newest_attempt_op_id,
        }) => {
            assert_eq!((attempt_op_id, newest_attempt_op_id), (10, 11));
        }
        other => panic!("expected Superseded, got {other:?}"),
    }
    match store
        .verification_job_claim(sid, TaskId::new(1), 10, "bg", 101, 14)
        .unwrap()
    {
        Err(VerificationJobRefusal::Superseded { .. }) => {}
        other => panic!("expected Superseded claim, got {other:?}"),
    }
    // Attempt N is byte-intact: its PASSED result was never mutated.
    let old = store
        .verification_attempt_get(sid, TaskId::new(1), 10)
        .unwrap()
        .unwrap();
    assert_eq!(old.checks[0].state, "passed");
    assert_eq!(
        old.checks[0].result_json.as_deref(),
        Some("{\"status\":\"passed\"}")
    );
    // The results are attempt-keyed rows: N+1 starts without one.
    let new = store
        .verification_attempt_get(sid, TaskId::new(1), 11)
        .unwrap()
        .unwrap();
    assert_eq!(new.checks[0].state, "queued");
    assert!(new.checks[0].result_json.is_none());
    let result_rows: i64 = store
        .read()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM verification_job_result WHERE session_id = ?1",
            params![sid.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(result_rows, 1, "only attempt N recorded a result");
}

#[test]
fn corrupt_or_foreign_job_rows_are_loud_typed_errors() {
    let (_d, store, sid) = setup();
    assert!(store
        .verification_attempt_begin(
            &attempt(sid, 20, 3),
            &[],
            &[
                check(sid, 20, "bg", 0, None),
                check(sid, 20, "inline", 1, Some("passed"))
            ],
        )
        .unwrap());
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE verification_job SET state = 'bogus' WHERE check_id = 'bg'",
            [],
        )
        .unwrap();
    }
    match store.verification_jobs_open(sid, TaskId::new(1)) {
        Err(StoreError::Malformed(msg)) => assert!(msg.contains("bogus"), "{msg}"),
        other => panic!("unknown state must be malformed, got {other:?}"),
    }
    {
        let conn = store.raw_conn();
        // Repair the state so the NEXT corruption class is isolated.
        conn.execute(
            "UPDATE verification_job SET state = 'queued' WHERE check_id = 'bg'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE verification_job SET state = 'failed' WHERE check_id = 'inline'",
            [],
        )
        .unwrap();
    }
    match store.verification_attempt_current(sid, TaskId::new(1)) {
        Err(StoreError::Malformed(msg)) => assert!(msg.contains("disagrees"), "{msg}"),
        other => panic!("inline/state drift must be malformed, got {other:?}"),
    }
    // Force a non-positive identity on the attempt row: a corrupt row
    // reads loudly instead of being trusted.
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE verification_attempt SET task_revision = 0 WHERE attempt_op_id = 20",
            [],
        )
        .unwrap();
    }
    match store.verification_attempt_current(sid, TaskId::new(1)) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("zero revision must be corrupt, got {other:?}"),
    }
}

#[test]
fn v22_bounds_reject_oversized_argv_counts_and_duplicates_before_write() {
    let (_d, store, sid) = setup();
    // 33 argv entries (cap 32).
    let mut wide = check(sid, 30, "bg", 0, None);
    wide.args_json = serde_json::to_string(&vec!["a"; MAX_VERIFICATION_JOB_ARGS + 1]).unwrap();
    assert!(matches!(
        store.verification_attempt_begin(&attempt(sid, 30, 3), &[], &[wide]),
        Err(StoreError::Oversized(_))
    ));
    // One oversized argument (cap 1024).
    let mut long = check(sid, 30, "bg", 0, None);
    long.args_json =
        serde_json::to_string(&vec!["x".repeat(MAX_VERIFICATION_JOB_ARG_BYTES + 1)]).unwrap();
    assert!(matches!(
        store.verification_attempt_begin(&attempt(sid, 30, 3), &[], &[long]),
        Err(StoreError::Oversized(_))
    ));
    // 4097 changed files (cap 4096).
    let changed: Vec<String> = (0..=MAX_VERIFICATION_ATTEMPT_CHANGED)
        .map(|i| format!("f{i}"))
        .collect();
    assert!(matches!(
        store.verification_attempt_begin(
            &attempt(sid, 30, 3),
            &changed,
            &[check(sid, 30, "bg", 0, None)]
        ),
        Err(StoreError::Oversized(_))
    ));
    // 257 checks (cap 256).
    let checks: Vec<VerificationJobRow> = (0..=MAX_VERIFICATION_ATTEMPT_CHECKS)
        .map(|i| check(sid, 30, &format!("c{i}"), i as u32, Some("passed")))
        .collect();
    assert!(matches!(
        store.verification_attempt_begin(&attempt(sid, 30, 3), &[], &checks),
        Err(StoreError::Oversized(_))
    ));
    // Duplicate check id / duplicate derivation ordinal.
    assert!(matches!(
        store.verification_attempt_begin(
            &attempt(sid, 30, 3),
            &[],
            &[
                check(sid, 30, "dup", 0, Some("passed")),
                check(sid, 30, "dup", 1, Some("passed"))
            ]
        ),
        Err(StoreError::Malformed(_))
    ));
    assert!(matches!(
        store.verification_attempt_begin(
            &attempt(sid, 30, 3),
            &[],
            &[
                check(sid, 30, "a", 0, Some("passed")),
                check(sid, 30, "b", 0, Some("passed"))
            ]
        ),
        Err(StoreError::Malformed(_))
    ));
    // A missing session reference is refused by the foreign key (the
    // whole begin rolls back, no rows).
    let ghost = SessionId::new(999);
    let err = store
        .verification_attempt_begin(
            &attempt(ghost, 30, 3),
            &[],
            &[check(ghost, 30, "bg", 0, None)],
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Sqlite(_)), "{err:?}");
    assert!(store
        .verification_attempt_get(sid, TaskId::new(1), 30)
        .unwrap()
        .is_none());
}

#[test]
fn recovery_requeues_running_rows_and_leaves_queued_untouched() {
    let (_d, store, sid) = setup();
    assert!(store
        .verification_attempt_begin(
            &attempt(sid, 40, 3),
            &[],
            &[
                check(sid, 40, "running", 0, None),
                check(sid, 40, "queued", 1, None)
            ],
        )
        .unwrap());
    store
        .verification_job_claim(sid, TaskId::new(1), 40, "running", 7, 8)
        .unwrap()
        .unwrap();
    let report = store.verification_jobs_requeue_running(sid, 9).unwrap();
    assert_eq!(report.requeued, 1);
    assert_eq!(report.orphaned, 0);
    let rows = store
        .verification_jobs_for_attempt(sid, TaskId::new(1), 40)
        .unwrap();
    assert_eq!(rows[0].state, "queued");
    assert!(rows[0].note.as_deref().unwrap().contains("re-queued"));
    assert_eq!(rows[0].op_id, None);
    // Idempotent: nothing left to requeue.
    assert_eq!(
        store
            .verification_jobs_requeue_running(sid, 10)
            .unwrap()
            .requeued,
        0
    );
}
