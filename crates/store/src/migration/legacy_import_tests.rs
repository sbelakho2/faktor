use super::*;

const TASK: u64 = 7;

fn attempt_json(op: u64, task: u64, revision: u64) -> String {
    serde_json::json!({
        "schema_ver": 2,
        "task_id": task,
        "op_id": op,
        "task_revision": revision,
        "workspace_root": "/w",
        "changed": ["src/a.rs"],
        "checks": [
            { "check_id": "make_build", "command": "make build", "inline": "passed" },
            { "check_id": "make_test", "command": "make test", "inline": null },
            { "check_id": "make_lint", "command": "make lint", "inline": null },
        ],
        "environment_fingerprint": null,
        "created_ms": 111,
    })
    .to_string()
}

fn job_json(op: u64, task: u64, check_id: &str, state: &str, result: Option<&str>) -> String {
    serde_json::json!({
        "schema_ver": 2,
        "attempt_op": op,
        "task_id": task,
        "task_revision": 3,
        "workspace_root": "/w",
        "check_id": check_id,
        "kind": "test",
        "command": format!("make {}", check_id.trim_start_matches("make_")),
        "spec_json": serde_json::json!({
            "id": check_id,
            "kind": "Test",
            "category": "Unit",
            "program": "make",
            "args": ["test"],
            "cwd_rel": ".",
            "affects": [],
            "required": true,
        })
        .to_string(),
        "budget_ms": 60_000,
        "state": state,
        "note": null,
        "op_id": if state == "running" { Some(99) } else { None },
        "result_json": result,
        "environment_fingerprint": null,
        "created_ms": 112,
        "updated_ms": 113,
        "finished_ms": if state == "running" { None } else { Some(114) },
    })
    .to_string()
}

fn plant(store: &Store, sid: SessionId, kind: &str, key: &str, value: &str) {
    store.upsert_memory_fact(sid, kind, key, value).unwrap();
}

#[test]
fn pre_v22_rows_import_once_with_deterministic_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let sid: SessionId;
    {
        let store = Store::open(&root, true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        sid = store.create_session(ws, "t", "p", "m").unwrap().id;
    }
    {
        let store = Store::open(&root, true).unwrap();
        plant(
            &store,
            sid,
            LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
            &format!("va:{TASK}:4242"),
            &attempt_json(4242, TASK, 3),
        );
        plant(
            &store,
            sid,
            LEGACY_VERIFICATION_JOB_FACT_KIND,
            &format!("vj:{TASK}:make_test"),
            &job_json(4242, TASK, "make_test", "running", None),
        );
        plant(
                &store,
                sid,
                LEGACY_VERIFICATION_JOB_FACT_KIND,
                &format!("vj:{TASK}:make_lint"),
                &job_json(
                    4242,
                    TASK,
                    "make_lint",
                    "passed",
                    Some(
                        "{\"status\":\"Passed\",\"exit\":0,\"started_ms\":1,\"finished_ms\":2,\"summary\":null,\"truncated\":false}",
                    ),
                ),
            );
    }
    // The next OPEN is the upgrade: migration completion runs the
    // one-shot repair with no explicit call.
    let store = Store::open(&root, true).unwrap();
    let view = store
        .verification_attempt_get(sid, TaskId::new(TASK), 4242)
        .unwrap()
        .expect("the legacy attempt must import with its derived identity");
    assert_eq!(view.attempt.session_id, sid);
    assert_eq!(view.attempt.task_id, TaskId::new(TASK));
    assert_eq!(view.attempt.attempt_op_id, 4242);
    assert_eq!(view.attempt.task_revision, TaskRevision::new(3));
    assert_eq!(view.changed, vec!["src/a.rs".to_string()]);
    assert_eq!(view.checks.len(), 3, "inline + background checks import");
    assert_eq!(view.checks[0].check_id, "make_build");
    assert_eq!(view.checks[0].inline_status.as_deref(), Some("passed"));
    assert_eq!(view.checks[0].state, "passed");
    assert_eq!(view.checks[1].check_id, "make_test");
    assert_eq!(view.checks[1].state, "running");
    assert_eq!(view.checks[1].op_id, Some(99));
    assert_eq!(view.checks[1].program, "make");
    assert_eq!(view.checks[1].args_json, "[\"test\"]");
    assert_eq!(view.checks[2].check_id, "make_lint");
    assert_eq!(view.checks[2].state, "passed");
    assert!(
        view.checks[2]
            .result_json
            .as_deref()
            .unwrap()
            .contains("Passed"),
        "the terminal outcome imports into verification_job_result"
    );
    // An in-flight legacy job is visible through the open-job scan, so
    // post-restart recovery owns it.
    let open = store
        .verification_jobs_open(sid, TaskId::new(TASK))
        .unwrap();
    assert_eq!(open.len(), 1, "in-flight legacy job visible after upgrade");
    assert_eq!(open[0].check_id, "make_test");
    assert_eq!(open[0].state, "running");
    // Additive: the legacy fact rows are preserved, and exactly ONE
    // durable marker records the import.
    let facts = store.memory_facts(sid).unwrap();
    assert!(facts.iter().any(|(k, key, _)| {
        k == LEGACY_VERIFICATION_ATTEMPT_FACT_KIND && key == &format!("va:{TASK}:4242")
    }));
    assert!(facts.iter().any(|(k, key, _)| {
        k == LEGACY_VERIFICATION_JOB_FACT_KIND && key == &format!("vj:{TASK}:make_test")
    }));
    assert_eq!(
        facts
            .iter()
            .filter(|(k, key, _)| {
                k == VERIFICATION_V22_IMPORT_MARKER_KIND
                    && key == VERIFICATION_V22_IMPORT_MARKER_KEY
            })
            .count(),
        1,
        "one durable marker per imported session"
    );
    drop(store);
    // Re-open is a no-op: no duplicate attempt, checks, jobs or results.
    let store = Store::open(&root, true).unwrap();
    let again = store.import_legacy_verification_facts().unwrap();
    assert_eq!(again.imported_attempts, 0);
    assert_eq!(again.imported_jobs, 0);
    assert_eq!(again.imported_results, 0);
    let view = store
        .verification_attempt_get(sid, TaskId::new(TASK), 4242)
        .unwrap()
        .unwrap();
    assert_eq!(view.checks.len(), 3, "no duplicate checks after reopen");
    assert_eq!(
        store
            .verification_jobs_for_attempt(sid, TaskId::new(TASK), 4242)
            .unwrap()
            .len(),
        2,
        "no duplicate background jobs after reopen"
    );
}

#[test]
fn corrupt_legacy_rows_are_loudly_skipped_and_left_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let sid: SessionId;
    {
        let store = Store::open(&root, true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        sid = store.create_session(ws, "t", "p", "m").unwrap().id;
    }
    let store = Store::open(&root, true).unwrap();
    // (a) undecodable attempt JSON.
    plant(
        &store,
        sid,
        LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
        "va:8:1",
        "{ this is not json",
    );
    // (b) an unknown future schema version.
    plant(
        &store,
        sid,
        LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
        "va:8:2",
        &serde_json::json!({
            "schema_ver": 99,
            "task_id": 8,
            "op_id": 2,
            "task_revision": 1,
            "workspace_root": "/w",
            "changed": [],
            "checks": [],
            "created_ms": 1,
        })
        .to_string(),
    );
    // (c) key/value identity disagreement.
    let mut mismatched: serde_json::Value = serde_json::from_str(&attempt_json(4, 8, 1)).unwrap();
    mismatched["op_id"] = serde_json::json!(4);
    plant(
        &store,
        sid,
        LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
        "va:8:3",
        &mismatched.to_string(),
    );
    // (d) a valid attempt whose only background job is corrupt: the
    // inline check still imports, the corrupt job is skipped loudly.
    let attempt_value = serde_json::json!({
        "schema_ver": 2,
        "task_id": 9,
        "op_id": 10,
        "task_revision": 1,
        "workspace_root": "/w",
        "changed": [],
        "checks": [
            { "check_id": "inline_ok", "command": "make ok", "inline": "passed" },
            { "check_id": "bad_job", "command": "make bad", "inline": null },
        ],
        "created_ms": 5,
    })
    .to_string();
    plant(
        &store,
        sid,
        LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
        "va:9:10",
        &attempt_value,
    );
    plant(
        &store,
        sid,
        LEGACY_VERIFICATION_JOB_FACT_KIND,
        "vj:9:bad_job",
        &job_json(10, 9, "bad_job", "zombie", None),
    );
    // (e) an orphan job whose attempt row does not exist.
    plant(
        &store,
        sid,
        LEGACY_VERIFICATION_JOB_FACT_KIND,
        "vj:9:orphan",
        &job_json(11, 9, "orphan", "queued", None),
    );
    let report = store.import_legacy_verification_facts().unwrap();
    assert_eq!(report.imported_attempts, 1, "only the resolvable attempt");
    assert_eq!(report.imported_jobs, 1, "only its inline check");
    assert_eq!(
        report.skipped_overflow, 0,
        "the bounded note list holds every skip: {:?}",
        report.skipped
    );
    let reason_for = |key: &str| {
        report
            .skipped
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("no typed skip note for {key}: {:?}", report.skipped))
            .reason
            .clone()
    };
    assert!(
        reason_for("va:8:1").contains("undecodable"),
        "{}",
        reason_for("va:8:1")
    );
    assert!(
        reason_for("va:8:2").contains("schema_ver"),
        "{}",
        reason_for("va:8:2")
    );
    assert!(
        reason_for("va:8:3").contains("row names"),
        "{}",
        reason_for("va:8:3")
    );
    assert!(
        reason_for("vj:9:bad_job").contains("unknown legacy job state"),
        "{}",
        reason_for("vj:9:bad_job")
    );
    assert!(
        reason_for("vj:9:orphan").contains("not imported"),
        "{}",
        reason_for("vj:9:orphan")
    );
    // The skips are durable on the marker (typed note), the corrupt
    // legacy rows are NEVER deleted, and the resolvable attempt is live.
    let facts = store.memory_facts(sid).unwrap();
    let marker = facts
        .iter()
        .find(|(k, key, _)| {
            k == VERIFICATION_V22_IMPORT_MARKER_KIND && key == VERIFICATION_V22_IMPORT_MARKER_KEY
        })
        .map(|(_, _, value)| value.clone())
        .expect("marker fact");
    assert!(marker.contains("\"skipped\""), "{marker}");
    assert!(marker.contains("unknown legacy job state"), "{marker}");
    assert!(facts
        .iter()
        .any(|(k, key, _)| k == "verification_attempt" && key == "va:8:1"));
    assert!(facts
        .iter()
        .any(|(k, key, _)| k == "verification_job" && key == "vj:9:orphan"));
    let view = store
        .verification_attempt_get(sid, TaskId::new(9), 10)
        .unwrap()
        .unwrap();
    assert_eq!(view.checks.len(), 1, "inline check imported");
    assert_eq!(view.checks[0].check_id, "inline_ok");
    // A corrupt legacy attempt never minted a half-written row.
    assert!(store
        .verification_attempt_get(sid, TaskId::new(8), 3)
        .unwrap()
        .is_none());
    // Re-open: marker-guarded, nothing duplicated, corrupt rows stay.
    drop(store);
    let store = Store::open(&root, true).unwrap();
    assert_eq!(
        store
            .verification_attempt_get(sid, TaskId::new(9), 10)
            .unwrap()
            .unwrap()
            .checks
            .len(),
        1
    );
    assert!(store
        .memory_facts(sid)
        .unwrap()
        .iter()
        .any(|(_, key, _)| key == "va:8:1"));
}
