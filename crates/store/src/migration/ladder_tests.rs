use super::*;

#[test]
fn migration_v3_keeps_pre_v3_checkpoint_rows_readable() {
    // Simulate a store that was created at v2 (checkpoints without the
    // after-blob column): open a fresh store, record a checkpoint, then
    // downgrade the schema behind the API's back (DROP COLUMN + set the
    // version cursor back). Reopening must apply v3, leave the old row
    // readable, and surface after_cas_hash as NULL — never a panic and
    // never a lost row.
    let dir = tempfile::tempdir().unwrap();
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_checkpoint(s.id, 3, "f.txt", "before", "after", Some("after-blob"))
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("ALTER TABLE checkpoint DROP COLUMN after_cas_hash", [])
                .unwrap();
            // The v6 existence columns are post-v2 too: drop them so the
            // full migration chain (v3..v6) replays on reopen.
            conn.execute("ALTER TABLE checkpoint DROP COLUMN before_exists", [])
                .unwrap();
            conn.execute("ALTER TABLE checkpoint DROP COLUMN after_exists", [])
                .unwrap();
            // The v7 tool-run recovery columns + turn-record table are
            // post-v2 too: drop them so the full migration chain
            // (v3..v7) replays on reopen.
            conn.execute("ALTER TABLE tool_run DROP COLUMN replay_descriptor", [])
                .unwrap();
            conn.execute("ALTER TABLE tool_run DROP COLUMN attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE tool_run DROP COLUMN postcondition", [])
                .unwrap();
            conn.execute("DROP TABLE turn_record", []).unwrap();
            // The v8 session-identity columns are post-v2 too: drop them
            // so the full migration chain (v3..v8) replays on reopen.
            conn.execute("ALTER TABLE session DROP COLUMN worktree_id", [])
                .unwrap();
            conn.execute("ALTER TABLE session DROP COLUMN task_id", [])
                .unwrap();
            // The v9/v10 task tables are post-this-version too: restore
            // the legacy `task` layout so the migration chain past v10
            // replays on reopen.
            conn.execute("DROP TABLE task", []).unwrap();
            conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                           // post-this-version too: drop them so the full chain
                           // (past v11) replays cleanly on reopen.
            conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                .unwrap();
            // v13 prefix-stability columns are post-this-version too: drop
            // them so the full chain (past v13) replays cleanly on reopen.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain
            // (past v20) replays cleanly.
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
                [],
            )
            .unwrap();
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
                [],
            )
            .unwrap();
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            conn.execute("PRAGMA user_version = 2", []).unwrap();
        }
        s.id
    };
    // Reopen: v3 re-applies the column; the pre-v3 row must read back
    // intact with after_cas_hash = NULL.
    let store = Store::open(dir.path(), true).unwrap();
    let cps = store.checkpoints_of(sid).unwrap();
    assert_eq!(cps.len(), 1, "the old row must survive the v3 migration");
    assert_eq!(cps[0].path, "f.txt");
    assert_eq!(cps[0].before_hash, "before");
    assert_eq!(cps[0].after_hash, "after");
    assert_eq!(cps[0].after_cas_hash, None);
    assert!(cps[0].created_ms > 0);
    // And the column is writable again.
    store
        .put_checkpoint(sid, 4, "g.txt", "b", "a", Some("x"))
        .unwrap();
    assert_eq!(
        store.checkpoints_of(sid).unwrap()[1]
            .after_cas_hash
            .as_deref(),
        Some("x")
    );
}

#[test]
fn migration_v6_keeps_pre_v6_checkpoint_rows_readable_as_existing() {
    // A store created at v5 records a checkpoint without existence
    // flags. Downgrade the schema behind the API's back (DROP the new
    // columns + rewind the version cursor), then reopen: v6 re-adds the
    // columns with DEFAULT 1 and the old row must read back as
    // exists:true on both sides (old rows only ever recorded real
    // files) — never a lost row, never a panic.
    let dir = tempfile::tempdir().unwrap();
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_checkpoint(s.id, 1, "f.txt", "before", "after", Some("blob"))
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("ALTER TABLE checkpoint DROP COLUMN before_exists", [])
                .unwrap();
            conn.execute("ALTER TABLE checkpoint DROP COLUMN after_exists", [])
                .unwrap();
            // The v7 tool-run recovery columns + turn-record table are
            // post-v5 too: drop them so the full migration chain
            // (v6..v7) replays on reopen.
            conn.execute("ALTER TABLE tool_run DROP COLUMN replay_descriptor", [])
                .unwrap();
            conn.execute("ALTER TABLE tool_run DROP COLUMN attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE tool_run DROP COLUMN postcondition", [])
                .unwrap();
            conn.execute("DROP TABLE turn_record", []).unwrap();
            // The v8 session-identity columns are post-v5 too: drop them
            // The v9/v10 task tables are post-this-version too: restore
            // the legacy `task` layout so the migration chain past v10
            // replays on reopen.
            conn.execute("DROP TABLE task", []).unwrap();
            conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                .unwrap();
            // so the full migration chain (v6..v8) replays on reopen.
            conn.execute("ALTER TABLE session DROP COLUMN worktree_id", [])
                .unwrap();
            conn.execute("ALTER TABLE session DROP COLUMN task_id", [])
                .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                           // post-this-version too: drop them so the full chain
                           // (past v11) replays cleanly on reopen.
            conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                .unwrap();
            // v13 prefix-stability columns are post-this-version too: drop
            // them so the full chain (past v13) replays cleanly on reopen.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain
            // (past v20) replays cleanly.
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
                [],
            )
            .unwrap();
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
                [],
            )
            .unwrap();
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            conn.execute("PRAGMA user_version = 5", []).unwrap();
        }
        s.id
    };
    let store = Store::open(dir.path(), true).unwrap();
    let cps = store.checkpoints_of(sid).unwrap();
    assert_eq!(cps.len(), 1, "the old row must survive the v6 migration");
    assert!(
        cps[0].before_exists && cps[0].after_exists,
        "pre-v6 rows have no existence marker: hash present means exists:true"
    );
    assert_eq!(cps[0].before_hash, "before");
    // And the new columns are writable again.
    store
        .insert_checkpoint(sid, "g.txt", false, "", true, "a", Some("x"))
        .unwrap();
    let rows = store.checkpoints_of(sid).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(!rows[1].before_exists);
    assert_eq!(
        rows[1].sequence, 2,
        "allocation continues after legacy rows"
    );
}

#[test]
fn migration_v7_replays_cleanly_on_a_v6_store() {
    // Simulate a store created before v7 (no turn_record table, no
    // tool_run recovery columns), then reopen: the v7 migration must
    // re-create everything without touching existing rows.
    let dir = tempfile::tempdir().unwrap();
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .start_tool_run(
                s.id,
                OpId::new(1),
                "run",
                serde_json::json!({}),
                serde_json::json!({"strategy": "none"}),
                None,
                None,
            )
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("ALTER TABLE tool_run DROP COLUMN replay_descriptor", [])
                .unwrap();
            conn.execute("ALTER TABLE tool_run DROP COLUMN attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE tool_run DROP COLUMN postcondition", [])
                .unwrap();
            // The v9/v10 task tables are post-this-version too: restore
            // the legacy `task` layout so the migration chain past v10
            // replays on reopen.
            conn.execute("DROP TABLE task", []).unwrap();
            conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                .unwrap();
            conn.execute("DROP TABLE turn_record", []).unwrap();
            // The v8 session-identity columns are post-v7 too: drop them
            // so the migration chain past v7 replays on reopen.
            conn.execute("ALTER TABLE session DROP COLUMN worktree_id", [])
                .unwrap();
            conn.execute("ALTER TABLE session DROP COLUMN task_id", [])
                .unwrap();
            // Pre-v7 stores sit at machine version 7 (the v6 comment
            // block covers TWO ALTER entries: before_exists and
            // after_exists); rewinding to 7 replays ONLY the v7 entry.                                // v11 artifacts (event payload_ver + the typed ledger) are
            // post-this-version too: drop them so the full chain
            // (past v11) replays cleanly on reopen.
            conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                .unwrap();
            // v13 prefix-stability columns are post-this-version too: drop
            // them so the full chain (past v13) replays cleanly on reopen.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain
            // (past v20) replays cleanly.
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
                [],
            )
            .unwrap();
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
                [],
            )
            .unwrap();
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            conn.execute("PRAGMA user_version = 7", []).unwrap();
        }
        s.id
    };
    let store = Store::open(dir.path(), true).unwrap();
    // The pre-v7 row survived and is readable.
    let pending = store.pending_tool_runs(sid).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].attempt, 0);
    assert!(pending[0].replay_descriptor.is_none());
    assert!(pending[0].postcondition.is_none());
    // And the new machinery works on the migrated store.
    let op = OpId::new(2);
    store
        .start_tool_run(
            sid,
            op,
            "echo",
            serde_json::json!({}),
            serde_json::json!({"strategy": "idempotent"}),
            None,
            Some(serde_json::json!({"tool_name": "echo"})),
        )
        .unwrap();
    assert_eq!(store.bump_tool_run_attempt(sid, op).unwrap(), 1);
    store
        .start_turn_record(sid, OpId::new(9), None, Some(2), "p", "m", None)
        .unwrap();
    assert_eq!(store.turn_records_of(sid).unwrap().len(), 1);
    // Reopen again: still stable.
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(store.turn_records_of(sid).unwrap().len(), 1);
}

#[test]
fn migration_v8_replays_cleanly_on_a_v7_store() {
    // Simulate a v7 store (no worktree_id/task_id columns on session),
    // reopen: v8 must add the columns and existing rows must read back
    // as the standalone 1/1 default — never a lost or corrupt row.
    // (Note: the v6 checkpoint block spans TWO array entries, so the
    // session-identity migration is array index 8 = schema target 9;
    // rewinding to 8 replays exactly this one entry.)
    let dir = tempfile::tempdir().unwrap();
    let (sid, ws) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("ALTER TABLE session DROP COLUMN worktree_id", [])
                .unwrap();
            conn.execute("ALTER TABLE session DROP COLUMN task_id", [])
                .unwrap();
            // The v9/v10 task tables are post-this-version too: restore
            // the legacy `task` layout so the migration chain past v10
            // replays on reopen.
            conn.execute("DROP TABLE task", []).unwrap();
            conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                           // post-this-version too: drop them so the full chain
                           // (past v11) replays cleanly on reopen.
            conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                .unwrap();
            // v13 prefix-stability columns are post-this-version too: drop
            // them so the full chain (past v13) replays cleanly on reopen.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain
            // (past v20) replays cleanly.
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
                [],
            )
            .unwrap();
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
                [],
            )
            .unwrap();
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            conn.execute("PRAGMA user_version = 8", []).unwrap();
        }
        (s.id, ws)
    };
    let store = Store::open(dir.path(), true).unwrap();
    let row = store.get_session(sid).unwrap().unwrap();
    assert_eq!(row.workspace_id, ws, "row survived the migration");
    assert_eq!(
        row.worktree_id,
        WorktreeId::new(1),
        "v8 default on old rows"
    );
    assert_eq!(row.task_id, TaskId::new(1), "v8 default on old rows");
    assert_eq!(store.list_sessions(None).unwrap().len(), 1);
}

#[test]
fn migration_v9_replays_cleanly_on_a_v8_store() {
    // Simulate a v8 store (no op_id_seq table), reopen: v9 must create
    // the table and seed the ONE global row from the migration-time
    // clock so freshly migrated databases mint ids far above any
    // pre-migration (clock+counter) id. (The v9 block is array index 9
    // = schema target 10; rewinding to 9 replays exactly this entry.)
    let dir = tempfile::tempdir().unwrap();
    let (sid, ws) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("DROP TABLE op_id_seq", []).unwrap();
            // The v9/v10 task tables are post-this-version too: restore
            // the legacy `task` layout so the migration chain past v10
            // replays on reopen.
            conn.execute("DROP TABLE task", []).unwrap();
            conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                           // post-this-version too: drop them so the full chain
                           // (past v11) replays cleanly on reopen.
            conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                .unwrap();
            // v13 prefix-stability columns are post-this-version too: drop
            // them so the full chain (past v13) replays cleanly on reopen.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain
            // (past v20) replays cleanly.
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
                [],
            )
            .unwrap();
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
                [],
            )
            .unwrap();
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            conn.execute("PRAGMA user_version = 9", []).unwrap();
        }
        (s.id, ws)
    };
    let store = Store::open(dir.path(), true).unwrap();
    // Pre-v9 rows survived the migration.
    let row = store.get_session(sid).unwrap().unwrap();
    assert_eq!(row.workspace_id, ws, "row survived the migration");
    // The seed row exists, is large, and ids start exactly there.
    let hw = store.op_id_seq_high_water().unwrap();
    assert!(
        hw > (1u64 << 20),
        "seed must dominate any pre-migration id: {hw}"
    );
    let (start, n) = store.alloc_op_ids(sid, 5).unwrap();
    assert_eq!((start, n), (hw, 5), "ids are minted at the seeded mark");
    // Reopen again: migration is a no-op and the sequence is durable.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(store.op_id_seq_high_water().unwrap(), hw + 5);
}

#[test]
fn migration_v10_replays_cleanly_on_a_v9_store() {
    // Simulate a v9 store (the legacy one-row-per-session ledger table
    // only; no typed task rows), reopen: the v10 block must rename the
    // legacy table to task_ledger, create the typed `task` table, and
    // leave every pre-v10 ledger row readable byte-identically.
    let dir = tempfile::tempdir().unwrap();
    let (sid, ledger_value) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let ledger = serde_json::json!({"goal": "legacy ledger row", "tasks": []});
        store.put_task_ledger(s.id, ledger.clone()).unwrap();
        {
            let conn = store.raw_conn();
            // Rewind the schema to the v9 layout: drop the migrated
            // artifacts and rename the legacy table back to `task`.
            conn.execute("DROP TABLE task", []).unwrap();
            conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                           // post-this-version too: drop them so the full chain
                           // (past v11) replays cleanly on reopen.
            conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                .unwrap();
            conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                .unwrap();
            // v13 prefix-stability columns are post-this-version too: drop
            // them so the full chain (past v13) replays cleanly on reopen.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain
            // (past v20) replays cleanly.
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
                [],
            )
            .unwrap();
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
                [],
            )
            .unwrap();
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            conn.execute("PRAGMA user_version = 9", []).unwrap();
        }
        (s.id, ledger)
    };
    let store = Store::open(dir.path(), true).unwrap();
    // The legacy ledger row survived the migration byte-identically.
    assert_eq!(store.get_task_ledger(sid).unwrap(), Some(ledger_value));
    // The typed task table exists and starts empty; writes work.
    assert!(store.list_tasks(sid).unwrap().is_empty());
    let row = TaskRow {
        task_id: TaskId::new(3),
        session_id: sid,
        goal: "typed goal".into(),
        acceptance_criteria: vec!["cargo check".into()],
        plan: vec![],
        attachments: vec![],
        contract: Default::default(),
        max_tokens: Some(10_000),
        max_turns: None,
        spent_tokens: 0,
        spent_turns: 0,
        state: TaskState::Pending,
        revision: TaskRevision::new(1),
        created_ms: 7,
        updated_ms: 7,
    };
    store.upsert_task(&row).unwrap();
    assert_eq!(store.get_task(sid, TaskId::new(3)).unwrap(), Some(row));
    // Reopen again: the migration is a no-op and both tables read back.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    assert!(store.get_task_ledger(sid).unwrap().is_some());
    let back = store.get_task(sid, TaskId::new(3)).unwrap().unwrap();
    assert_eq!(back.goal, "typed goal");
    assert_eq!(back.acceptance_criteria, vec!["cargo check".to_string()]);
    assert_eq!(back.max_tokens, Some(10_000));
    assert_eq!(back.state, TaskState::Pending);
    assert_eq!(back.revision, TaskRevision::new(1));
}

pub(crate) fn prefix_hash(seed: u8) -> [u8; 32] {
    [seed; 32]
}

#[test]
fn prefix_columns_round_trip_across_reopen_and_legacy_rows_read_null() {
    // (d) The v13 columns must survive a full reopen with every value
    // intact, and rows written before the columns existed must honestly
    // read as "no observation" — never zeros, never guesses.
    let dir = tempfile::tempdir().unwrap();
    let (ws, sid) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store
            .create_session(ws, "prefix-roundtrip", "p", "m")
            .unwrap();
        // Legacy-shaped row: no prefix observation at all.
        store
            .record_provider_call(
                s.id,
                OpId::new(1),
                "p",
                "m",
                "completed",
                Some(10),
                Some(5),
                None,
            )
            .unwrap();
        // Full observation row.
        store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(2),
                "p",
                "m",
                "completed",
                Some(10),
                Some(5),
                None,
                Some(prefix_hash(7)),
                Some(1234),
                Some(0.625),
            )
            .unwrap();
        // Hash with no recorded per-row stability: still an observation.
        store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(3),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(9)),
                Some(10),
                None,
            )
            .unwrap();
        (ws, s.id)
    };
    // Reopen: migration is a no-op, all data reads back.
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.provider_call_prefix_rows(sid).unwrap();
    assert_eq!(rows.len(), 2, "the legacy row carries no observation");
    assert_eq!(rows[0].row_id + 1, rows[1].row_id);
    assert_eq!(rows[0].prompt_prefix_hash, prefix_hash(7));
    assert_eq!(rows[0].prompt_tokens, 1234);
    assert_eq!(rows[0].prefix_stability, Some(0.625));
    assert_eq!(rows[1].prompt_prefix_hash, prefix_hash(9));
    assert_eq!(rows[1].prompt_tokens, 10);
    assert_eq!(rows[1].prefix_stability, None);
    // Session isolation: another session sees none of it.
    let other = store.create_session(ws, "other", "p", "m").unwrap();
    assert!(store
        .provider_call_prefix_rows(other.id)
        .unwrap()
        .is_empty());
    // Second reopen still stable.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.provider_call_prefix_rows(sid).unwrap();
    assert_eq!(rows[0].prompt_prefix_hash, prefix_hash(7));
    assert_eq!(rows[1].prompt_tokens, 10);
}

/// One strict observation payload: `n` distinct 64-char hex digests and
/// one token count per segment.
pub(crate) fn segments_json(n: usize, tokens: &[u64]) -> String {
    let hashes: Vec<String> = (0..n)
        .map(|i| format!("{:02x}", i as u8).repeat(32))
        .collect();
    serde_json::json!({
        "segment_hashes": hashes,
        "segment_token_counts": tokens,
        "cache_read_tokens": 7u64,
    })
    .to_string()
}

#[test]
fn prefix_segments_json_round_trips_across_reopen_and_legacy_rows_read_null() {
    // The v19 additive payload survives a full reopen byte-identically,
    // and rows written through the legacy API honestly read as "no
    // segment observation" — never an empty JSON object, never a guess.
    let dir = tempfile::tempdir().unwrap();
    let json = segments_json(3, &[10, 20, 30]);
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "segments", "p", "m").unwrap();
        // Legacy prefix row: hash + tokens but no segment payload.
        store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(1),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(3)),
                Some(60),
                None,
            )
            .unwrap();
        // v19 row: the same shape plus the observed segments.
        store
            .record_provider_call_with_prefix_segments(
                s.id,
                OpId::new(2),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(4)),
                Some(60),
                Some(1.0),
                Some(&json),
            )
            .unwrap();
        s.id
    };
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.provider_call_prefix_rows(sid).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].prefix_segments_json, None,
        "pre-v19 rows must read as no observation"
    );
    assert_eq!(rows[1].prefix_segments_json.as_deref(), Some(json.as_str()));
    // Reopen again: still byte-identical.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.provider_call_prefix_rows(sid).unwrap();
    assert_eq!(rows[1].prefix_segments_json.as_deref(), Some(json.as_str()));
    // A session with no rows sees none of it.
    let other = store.create_workspace("/w2").unwrap();
    let other = store.create_session(other, "other", "p", "m").unwrap();
    assert!(store
        .provider_call_prefix_rows(other.id)
        .unwrap()
        .is_empty());
}

#[test]
fn migration_v13_replays_cleanly_on_a_v12_store() {
    // Simulate a v12 store (provider_call without the v13 columns):
    // reopen must add the columns, keep pre-v13 rows readable as
    // observation-less, and accept full observations afterwards.
    let dir = tempfile::tempdir().unwrap();
    let (sid, row) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let row = store
            .record_provider_call(s.id, OpId::new(1), "p", "m", "ok", Some(10), Some(5), None)
            .unwrap();
        {
            let conn = store.raw_conn();
            // Rewind to the v12 layout: drop the v13 columns and the
            // schema cursor so the full chain past v13 replays.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain
            // (past v20) replays cleanly.
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
                [],
            )
            .unwrap();
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
                [],
            )
            .unwrap();
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // The v14 task-revision column + verification_record table
            // are post-this-version too: drop them so the full chain
            // (past v14) replays cleanly on reopen.
            conn.execute("ALTER TABLE task DROP COLUMN revision", [])
                .unwrap();
            conn.execute("DROP TABLE verification_record", []).unwrap();
            // The v15 cost-ledger objects are post-this-version too:
            // drop them so the full chain (past v15) replays cleanly.
            conn.execute("DROP TABLE cost_reservation", []).unwrap();
            conn.execute("ALTER TABLE task DROP COLUMN max_cost_micro", [])
                .unwrap();
            conn.execute("ALTER TABLE task DROP COLUMN spent_cost_micro", [])
                .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            let _ = conn.execute("ALTER TABLE task DROP COLUMN contract", []);
            conn.execute("PRAGMA user_version = 13", []).unwrap();
        }
        (s.id, row)
    };
    let store = Store::open(dir.path(), true).unwrap();
    // The pre-v13 row survived and reads as observation-less.
    assert!(store.provider_call_prefix_rows(sid).unwrap().is_empty());
    let _ = row;
    // Full observations write and read through the re-migrated store.
    store
        .record_provider_call_with_prefix(
            sid,
            OpId::new(2),
            "p",
            "m",
            "ok",
            None,
            None,
            None,
            Some(prefix_hash(4)),
            Some(77),
            Some(0.9),
        )
        .unwrap();
    let rows = store.provider_call_prefix_rows(sid).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].prompt_prefix_hash, prefix_hash(4));
    assert_eq!(rows[0].prompt_tokens, 77);
    assert_eq!(rows[0].prefix_stability, Some(0.9));
    // Reopen again: migration is a no-op and the observation persists.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.provider_call_prefix_rows(sid).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].prompt_tokens, 77);
}

#[test]
fn migration_v25_rebuilds_attachment_references_and_preserves_rows() {
    // A pre-v25 database carries the digest-keyed attachment table. Seed
    // one row in that exact shape, rewind the cursor, and reopen: the
    // rebuild must preserve the row and let a metadata-distinct reference
    // for the SAME blob coexist afterwards.
    let dir = tempfile::tempdir().unwrap();
    let digest = faktor_core::hash::FileHash::from([7; 32]);
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "att", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("DROP TABLE attachment", []).unwrap();
            conn.execute(
                "CREATE TABLE attachment (
                        session_id INTEGER NOT NULL REFERENCES session(id),
                        digest TEXT NOT NULL,
                        mime TEXT NOT NULL,
                        filename TEXT,
                        size INTEGER NOT NULL,
                        PRIMARY KEY (session_id, digest)
                     )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO attachment(session_id, digest, mime, filename, size)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    s.id.raw() as i64,
                    digest.to_hex(),
                    "image/png",
                    "shot.png",
                    123i64
                ],
            )
            .unwrap();
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            let _ = conn.execute("ALTER TABLE task DROP COLUMN contract", []);
            // The v35 route-time execution class is post-this-version too.
            let _ = conn.execute(
                "ALTER TABLE cost_reservation DROP COLUMN execution_class",
                [],
            );
            conn.execute("PRAGMA user_version = 25", []).unwrap();
        }
        s.id
    };
    let store = Store::open(dir.path(), true).unwrap();
    let seeded = AttachmentId::new(digest, "image/png", Some("shot.png"), 123).unwrap();
    // The pre-v25 row survived the rebuild with its exact metadata.
    assert_eq!(
        store.attachment_row(sid, &seeded).unwrap(),
        Some(seeded.clone()),
        "the pre-v25 reference must survive the rebuild"
    );
    assert_eq!(store.attachment(sid, digest).unwrap(), Some(seeded.clone()));
    // A metadata-distinct reference to the SAME blob now coexists.
    let other = AttachmentId::new(digest, "application/pdf", Some("other.pdf"), 123).unwrap();
    assert_eq!(store.put_attachment(sid, &other).unwrap(), other);
    assert_eq!(
        store.attachments_by_digest(sid, digest).unwrap(),
        vec![seeded, other]
    );
    // Reopen again: both references and the new index are durable.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(store.attachments_by_digest(sid, digest).unwrap().len(), 2);
}

#[test]
fn migration_v28_rebuilds_artifact_identity_per_session_and_preserves_rows() {
    // A pre-v28 database carries the globally-unique cas_hash artifact
    // table. Seed one row in that exact shape, rewind the cursor, and
    // reopen: the rebuild must preserve the row (id and metadata) and let
    // a SECOND session store the same bytes with its own metadata.
    let dir = tempfile::tempdir().unwrap();
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "att", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("DROP TABLE artifact", []).unwrap();
            conn.execute(
                "CREATE TABLE artifact (
                        id INTEGER PRIMARY KEY,
                        session_id INTEGER NOT NULL REFERENCES session(id),
                        kind TEXT NOT NULL,
                        cas_hash TEXT NOT NULL UNIQUE,
                        summary TEXT NOT NULL,
                        created_ms INTEGER NOT NULL,
                        size INTEGER NOT NULL
                     )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO artifact(session_id, kind, cas_hash, summary, created_ms, size)
                     VALUES (?1, 'command_output', 'hash-legacy', 'legacy sum', 7, 42)",
                params![s.id.raw() as i64],
            )
            .unwrap();
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            let _ = conn.execute("ALTER TABLE task DROP COLUMN contract", []);
            // The v35 route-time execution class is post-this-version too.
            let _ = conn.execute(
                "ALTER TABLE cost_reservation DROP COLUMN execution_class",
                [],
            );
            conn.execute("PRAGMA user_version = 28", []).unwrap();
        }
        s.id
    };
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(
        store.artifact_for(sid, "hash-legacy").unwrap(),
        Some(("legacy sum".to_string(), "command_output".to_string())),
        "the pre-v28 row survives the rebuild"
    );
    // The preserved row keeps its id through the id-preserving rebuild.
    assert_eq!(
        store
            .put_artifact(sid, "command_output", "hash-legacy", "legacy sum", 42)
            .unwrap(),
        1,
        "the legacy artifact row keeps its id"
    );
    // A second session's identical bytes are no longer discarded: both
    // rows coexist with their own metadata.
    let ws2 = store.create_workspace("/w2").unwrap();
    let s2 = store.create_session(ws2, "other", "p", "m").unwrap();
    let other_id = store
        .put_artifact(s2.id, "tool_output", "hash-legacy", "other sum", 42)
        .unwrap();
    assert_ne!(other_id, 1, "per-session artifact rows coexist");
    assert_eq!(
        store.artifact_for(s2.id, "hash-legacy").unwrap(),
        Some(("other sum".to_string(), "tool_output".to_string()))
    );
    // Reopen again: both rows and the per-session uniqueness are durable.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(
        store.artifact_for(sid, "hash-legacy").unwrap(),
        Some(("legacy sum".to_string(), "command_output".to_string()))
    );
    assert_eq!(
        store.artifact_for(s2.id, "hash-legacy").unwrap(),
        Some(("other sum".to_string(), "tool_output".to_string()))
    );
    assert_eq!(
        store
            .put_artifact(sid, "command_output", "hash-legacy", "legacy sum", 42)
            .unwrap(),
        1
    );
}

#[test]
fn fast_open_recovers_migrations_and_data_and_refuses_corruption() {
    // Audit 43: the fast production open must still run WAL recovery +
    // migrations and refuse a corrupt store — it just skips the full
    // scan. Data written by a full-check open must read back through a
    // fast open, and vice versa.
    let dir = tempfile::tempdir().unwrap();
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "fast", "p", "m").unwrap();
        store
            .put_message(s.id, 1, "user", serde_json::json!({"text": "hi"}))
            .unwrap();
        s.id
    };
    let fast = Store::open_fast(dir.path()).unwrap();
    let row = fast.get_session(sid).unwrap().unwrap();
    assert_eq!(row.title, "fast");
    assert_eq!(fast.message_count(sid).unwrap(), 1);
    // The deep scan runs fine on a fast-opened store.
    assert!(fast.deep_integrity_check().unwrap().is_empty());
    // And a fast-opened store's writes survive a full-check reopen.
    let ws2 = fast.create_workspace("/w2").unwrap();
    fast.create_session(ws2, "s2", "p", "m").unwrap();
    drop(fast);
    let full = Store::open(dir.path(), true).unwrap();
    assert_eq!(full.list_sessions(None).unwrap().len(), 2);
    // Corrupt/truncated files refuse to open fast (never silently serve).
    let garbage = tempfile::tempdir().unwrap();
    std::fs::write(
        garbage.path().join("faktor-plus.db"),
        b"this is not a sqlite database at all - no magic header anywhere",
    )
    .unwrap();
    match Store::open_fast(garbage.path()) {
        Err(StoreError::Sqlite(_)) | Err(StoreError::Corrupt(_)) => {}
        other => panic!("fast open must refuse garbage, got {other:?}"),
    }
    let truncated = tempfile::tempdir().unwrap();
    std::fs::write(
        truncated.path().join("faktor-plus.db"),
        b"SQLite format 3\x00",
    )
    .unwrap();
    match Store::open_fast(truncated.path()) {
        Err(StoreError::Sqlite(_)) | Err(StoreError::Corrupt(_)) => {}
        other => panic!("fast open must refuse truncation, got {other:?}"),
    }
}

/// previous-head -> head (query-plan indexes): a predecessor database must gain every
/// audited index in ONE transactional migration, keep its rows, and keep the
/// new plans. A verified backup of the migrated store must restore the same
/// indexes and plans. `user_version` is set back to the predecessor AFTER
/// dropping the new indexes, so the replay is exactly the production path.
#[test]
fn upgrade_from_previous_head_restores_hot_path_indexes_and_backup_keeps_them() {
    let dir = tempfile::tempdir().unwrap();
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_message(s.id, 1, "user", serde_json::json!({"text": "hello"}))
            .unwrap();
        {
            let conn = store.raw_conn();
            for index in [
                "idx_part_message",
                "idx_permission_session_pending",
                "idx_permission_decision_id",
                "idx_session_workspace_updated",
                "idx_task_ledger_session_updated",
                "idx_prompt_queue_status_session",
                "idx_memory_fact_kind_session",
                "idx_memory_fact_updated",
                "idx_event_session_turn_completed",
                "idx_tool_run_running",
                "idx_provider_call_session_id",
                "idx_turn_record_active",
                "idx_task_session_created",
            ] {
                conn.execute_batch(&format!("DROP INDEX IF EXISTS {index};"))
                    .unwrap();
            }
            // The v1 duplicates are recreated so the replay also has to drop
            // them again (the migration must be idempotent, not just additive).
            let prev_head = SCHEMA_HEAD - 1;
            conn.execute_batch(&format!(
                "CREATE INDEX IF NOT EXISTS idx_event_session_seq ON event(session_id, seq);
                 CREATE INDEX IF NOT EXISTS idx_message_session_seq ON message(session_id, seq);
                 PRAGMA user_version = {prev_head};"
            ))
            .unwrap();
        }
        s.id
    };

    let store = Store::open(dir.path(), true).unwrap();
    {
        let conn = store.raw_conn();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_HEAD, "the v35 step completed");
        for index in [
            "idx_part_message",
            "idx_permission_session_pending",
            "idx_permission_decision_id",
            "idx_session_workspace_updated",
            "idx_task_ledger_session_updated",
            "idx_prompt_queue_status_session",
            "idx_memory_fact_kind_session",
            "idx_memory_fact_updated",
            "idx_event_session_turn_completed",
            "idx_tool_run_running",
            "idx_provider_call_session_id",
            "idx_turn_record_active",
            "idx_task_session_created",
        ] {
            let present: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    params![index],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "v35 must create {index}");
        }
        for dropped in ["idx_event_session_seq", "idx_message_session_seq"] {
            let present: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    params![dropped],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(present, 0, "v35 must drop the duplicate {dropped}");
        }
    }
    assert_eq!(
        store.messages_before(sid, None, 10).unwrap()[0].data["text"],
        "hello",
        "the predecessor row survives the migration"
    );

    // Backup/restore: the new indexes are schema objects, so the restored
    // copy must answer the same plan with the same index.
    let backup_path = dir.path().join("backup.db");
    store.backup_to(&backup_path).unwrap();
    let restored_dir = tempfile::tempdir().unwrap();
    std::fs::copy(&backup_path, restored_dir.path().join("faktor-plus.db")).unwrap();
    let restored = Store::open(restored_dir.path(), true).unwrap();
    let conn = restored.raw_conn();
    let parts_plan: Vec<String> = {
        let mut stmt = conn
            .prepare("EXPLAIN QUERY PLAN SELECT id FROM part WHERE message_id = ?1 ORDER BY id ASC")
            .unwrap();
        let mut rows = stmt.query(params![1i64]).unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            out.push(row.get::<_, String>(3).unwrap());
        }
        out
    };
    assert_eq!(
        parts_plan,
        vec!["SEARCH part USING COVERING INDEX idx_part_message (message_id=?)"],
        "a restored backup keeps the head plan"
    );
}
