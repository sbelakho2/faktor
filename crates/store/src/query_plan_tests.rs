//! Query-plan and seeded latency assertions for the production hot paths.
//!
//! The seeded sizes are the audit's representative scale (10k sessions/
//! messages/tasks, 50k events, 4k permissions, 20k provider calls/ledger
//! entries, plus tool/turn/queue/evidence/memory rows). The assertions pin
//! the EXACT `EXPLAIN QUERY PLAN` details of each hot statement against the
//! schema head, so dropping or misordering an index fails the crate tests
//! instead of silently reintroducing a table scan. Timing is printed (best
//! of five) for the audit report; it is not asserted because CI hosts are
//! noisy. `write_path_before_after` (ignored) simulates the pre-v36 index
//! set to measure index maintenance cost.

use std::time::Instant;

use super::*;

const SESSIONS: i64 = 10_000;
const MESSAGES: i64 = 10_000;
const EVENTS: i64 = 50_000;
const TASKS: i64 = 10_000;
const PERMISSIONS: i64 = 4_000;
const PROVIDER_CALLS: i64 = 20_000;
const RESERVATIONS: i64 = 10_000;
const TOOL_RUNS: i64 = 10_000;
const TURN_RECORDS: i64 = 10_000;
const EVIDENCE: i64 = 10_000;
const MEMORY_FACTS: i64 = 4_000;
const QUEUE_ROWS: i64 = 5_000;
const LEDGER_ROWS: i64 = 20_000;

fn seed(store: &Store) {
    let conn = store.raw_conn();
    conn.execute_batch("BEGIN").unwrap();
    conn.execute(
        "INSERT INTO workspace(id, root, created_ms) VALUES (1, '/w', 1)",
        [],
    )
    .unwrap();
    {
        let mut st = conn
            .prepare(
                "INSERT INTO session(id, workspace_id, title, provider, model, state, lifecycle,
                    created_ms, updated_ms, worktree_id, task_id)
                 VALUES (?1, 1, 't', 'p', 'm', '\"idle\"', 'open', ?1, ?1, 1, 1)",
            )
            .unwrap();
        for i in 1..=SESSIONS {
            st.execute(params![i]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO message(id, session_id, seq, role, data, created_ms)
                 VALUES (?1, ?1, 1, 'user', '{\"text\":\"hi\"}', ?1)",
            )
            .unwrap();
        for i in 1..=MESSAGES {
            st.execute(params![i]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO part(id, message_id, kind, data, created_ms)
                 VALUES (?1, ?2, 'text', '{}', 1)",
            )
            .unwrap();
        let mut id = 1i64;
        for m in 1..=MESSAGES {
            for _ in 0..3 {
                st.execute(params![id, m]).unwrap();
                id += 1;
            }
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload, payload_ver)
                 VALUES (?1, ?2, NULL, 'tick', '\"ok\"', 1, NULL, 1)",
            )
            .unwrap();
        for e in 0..EVENTS {
            st.execute(params![e % 5 + 1, (e / 5) % SESSIONS + 1])
                .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO task(task_id, session_id, goal, acceptance_criteria, plan,
                    max_tokens, max_turns, spent_tokens, spent_turns, state,
                    created_ms, updated_ms, revision, attachments, contract)
                 VALUES (1, ?1, 'g', '[]', '[]', NULL, NULL, 0, 0, '\"pending\"', ?1, ?1, 1, '[]', '{}')",
            )
            .unwrap();
        for i in 1..=TASKS {
            st.execute(params![i]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO permission(id, session_id, op_id, capability, decision,
                    resolved_ms, expires_ms)
                 VALUES (?1, ?2, ?1, 'fs.read', 'pending', NULL, 9_999_999_999)",
            )
            .unwrap();
        for i in 1..=PERMISSIONS {
            st.execute(params![i, i % SESSIONS + 1]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO provider_call(id, session_id, op_id, provider, model, started_ms,
                    ended_ms, status, tokens_in, tokens_out, error, prompt_prefix_hash,
                    prompt_tokens, prefix_stability, attempt_op_id)
                 VALUES (?1, ?2, ?1, 'p', 'm', 1, 1, 'completed', 1, 1, NULL, ?3, 10, 0.9, ?1)",
            )
            .unwrap();
        for i in 1..=PROVIDER_CALLS {
            st.execute(params![i, i % SESSIONS + 1, vec![7u8; 32]])
                .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO cost_reservation(reservation_id, session_id, task_id, op_id,
                    predicted_micro, status, created_ms, estimated_cost_micro)
                 VALUES (?1, ?2, 1, ?1, 100, 'settled', 1, 100)",
            )
            .unwrap();
        for i in 1..=RESERVATIONS {
            st.execute(params![i, i % SESSIONS + 1]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO tool_run(id, session_id, op_id, tool, args, status, started_ms,
                    ended_ms, effect_status, recovery, expected_hash)
                 VALUES (?1, ?2, ?1, 'read', '{}', ?3, 1, 2, 'applied', '{}', NULL)",
            )
            .unwrap();
        for i in 1..=TOOL_RUNS {
            let status = if i % 100 == 0 { "running" } else { "completed" };
            st.execute(params![i, i % SESSIONS + 1, status]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO turn_record(id, session_id, turn_op_id, queue_seq, prompt_message_id,
                    effective_provider, effective_model, variant, tool_mode, started_at,
                    status, updated_ms)
                 VALUES (?1, ?2, ?1, NULL, NULL, 'p', 'm', NULL, NULL, ?1, ?3, ?1)",
            )
            .unwrap();
        for i in 1..=TURN_RECORDS {
            let status = if i % 50 == 0 { "active" } else { "completed" };
            st.execute(params![i, i % SESSIONS + 1, status]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO evidence(id, session_id, workspace_id, task_id, kind, revision,
                    provenance, compressibility, compression, retrieval, compact,
                    backing_cas_hash, completeness, created_ms)
                 VALUES (?1, ?2, 1, 1, 'k', 1, '{}', 'c', '{}', '{}', '{}', NULL,
                    'complete', ?3)",
            )
            .unwrap();
        for i in 1..=EVIDENCE {
            st.execute(params![i, i % SESSIONS + 1, (i * 7) % 1000])
                .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms)
                 VALUES (?1, ?2, ?3, '{}', ?4)",
            )
            .unwrap();
        for i in 0..MEMORY_FACTS {
            st.execute(params![
                i % SESSIONS + 1,
                if i % 2 == 0 {
                    "usage"
                } else {
                    "verification_legacy"
                },
                format!("k{i}"),
                i + 1
            ])
            .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO prompt_queue(session_id, seq, op_id, prompt, status, requested_at)
                 VALUES (?1, ?2, ?2, 'p', 'pending', 1)",
            )
            .unwrap();
        for i in 0..QUEUE_ROWS {
            st.execute(params![i % SESSIONS + 1, i / SESSIONS + 1])
                .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO task_ledger(session_id, ledger, updated_ms) VALUES (?1, '{}', ?1)",
            )
            .unwrap();
        for i in 1..=SESSIONS {
            st.execute(params![i]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO ledger_entry(session_id, seq, entry_type, schema_ver, payload, created_ms)
                 VALUES (?1, ?2, 't', 1, '{}', 1)",
            )
            .unwrap();
        for i in 0..LEDGER_ROWS {
            st.execute(params![i % SESSIONS + 1, i / SESSIONS + 1])
                .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
}

fn plan(conn: &rusqlite::Connection, sql: &str, values: &[i64]) -> Vec<String> {
    let params: Vec<&dyn rusqlite::types::ToSql> = values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let mut rows = stmt.query(params.as_slice()).unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        out.push(row.get::<_, String>(3).unwrap());
    }
    out
}

fn assert_plan(name: &str, actual: Vec<String>, expected: &[&str]) {
    assert_eq!(
        actual,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "query plan regression on {name}: {actual:?}"
    );
}

fn bench_sql(conn: &rusqlite::Connection, name: &str, sql: &str, values: &[i64]) {
    let params: Vec<&dyn rusqlite::types::ToSql> = values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();
    let mut best = u128::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let mut stmt = conn.prepare(sql).unwrap();
        let mut rows = stmt.query(params.as_slice()).unwrap();
        while rows.next().unwrap().is_some() {}
        best = best.min(started.elapsed().as_micros());
    }
    println!("BENCH {name}: {best} us");
}

#[test]
fn hot_query_plans_stay_index_backed_at_seeded_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let seed_started = Instant::now();
    seed(&store);
    println!("SEED: {} ms", seed_started.elapsed().as_millis());
    let conn = store.raw_conn();

    let checks: &[(&str, &str, &[i64], &[&str])] = &[
        (
            "messages page",
            "SELECT id, session_id, seq, role, data, created_ms FROM message
             WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
            &[1, 100, 50],
            &["SEARCH message USING INDEX sqlite_autoindex_message_1 (session_id=? AND seq<?)"],
        ),
        (
            "parts of message",
            "SELECT id, message_id, kind, data, created_ms FROM part
             WHERE message_id = ?1 ORDER BY id ASC",
            &[1],
            &["SEARCH part USING INDEX idx_part_message (message_id=?)"],
        ),
        (
            "permission expiry sweep",
            "SELECT id, op_id FROM permission
             WHERE session_id = ?1 AND decision = 'pending' AND expires_ms <= ?2
             ORDER BY id ASC",
            &[1, 9_999_999_999],
            &["SEARCH permission USING INDEX idx_permission_session_pending (session_id=? AND decision=?)"],
        ),
        (
            "pending permission count",
            "SELECT COUNT(*) FROM permission WHERE decision = 'pending'",
            &[],
            &["SEARCH permission USING COVERING INDEX idx_permission_decision_id (decision=?)"],
        ),
        (
            "workspace session listing",
            "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state,
                    lifecycle, created_ms, updated_ms
             FROM session WHERE workspace_id = ?1 ORDER BY updated_ms DESC",
            &[1],
            &["SEARCH session USING INDEX idx_session_workspace_updated_id (workspace_id=?)"],
        ),
        (
            "transcript search chunk",
            "SELECT m.id, m.session_id, m.seq, m.role,
                    CASE WHEN (m.data LIKE ?3 ESCAPE '\\' OR EXISTS (
                        SELECT 1 FROM part p WHERE p.message_id = m.id
                         AND p.data LIKE ?3 ESCAPE '\\')) THEN m.data ELSE NULL END,
                    m.created_ms
             FROM message m
             WHERE m.session_id = ?1 AND m.seq < ?2
               AND (m.data LIKE ?3 ESCAPE '\\' OR EXISTS (
                    SELECT 1 FROM part p WHERE p.message_id = m.id
                     AND p.data LIKE ?3 ESCAPE '\\'))
             ORDER BY m.seq DESC LIMIT ?4",
            &[1, 100, 1, 128],
            &[
                "SEARCH m USING INDEX sqlite_autoindex_message_1 (session_id=? AND seq<?)",
                "CORRELATED SCALAR SUBQUERY 2",
                "SEARCH p USING INDEX idx_part_message (message_id=?)",
                "CORRELATED SCALAR SUBQUERY 1",
                "SEARCH p USING INDEX idx_part_message (message_id=?)",
            ],
        ),
        (
            "memory key-prefix probe",
            "SELECT EXISTS(SELECT 1 FROM memory_fact
              WHERE session_id = ?1 AND kind = ?2 AND key >= ?3 AND key < ?4)",
            &[1, 1, 1, 1],
            &[
                "SCAN CONSTANT ROW",
                "SCALAR SUBQUERY 1",
                "SEARCH memory_fact USING COVERING INDEX sqlite_autoindex_memory_fact_1 (session_id=? AND kind=? AND key>? AND key<?)",
            ],
        ),
        (
            "memory V2 page",
            "SELECT kind, key, value, updated_ms FROM memory_fact
             INDEXED BY idx_memory_fact_v2_page
             WHERE session_id = ?1 AND kind >= 'memory_v2:' AND kind < 'memory_v2;'
             ORDER BY updated_ms DESC, kind DESC, key DESC LIMIT ?2",
            &[1, 201],
            &["SEARCH memory_fact USING INDEX idx_memory_fact_v2_page (session_id=?)"],
        ),
        (
            "memory V2 count",
            "SELECT COUNT(*) FROM memory_fact INDEXED BY idx_memory_fact_v2_page
             WHERE session_id = ?1 AND kind >= 'memory_v2:' AND kind < 'memory_v2;'",
            &[1],
            &["SEARCH memory_fact USING COVERING INDEX idx_memory_fact_v2_page (session_id=?)"],
        ),
        (
            "workspace memory V2 page",
            "SELECT kind, key, value, updated_ms FROM workspace_memory_fact
             INDEXED BY idx_workspace_memory_fact_v2_page
             WHERE workspace_id = ?1 AND kind >= 'memory_v2:' AND kind < 'memory_v2;'
             ORDER BY updated_ms DESC, kind DESC, key DESC LIMIT ?2",
            &[1, 201],
            &["SEARCH workspace_memory_fact USING INDEX idx_workspace_memory_fact_v2_page (workspace_id=?)"],
        ),
        (
            "workspace memory V2 count",
            "SELECT COUNT(*) FROM workspace_memory_fact
             INDEXED BY idx_workspace_memory_fact_v2_page
             WHERE workspace_id = ?1 AND kind >= 'memory_v2:' AND kind < 'memory_v2;'",
            &[1],
            &["SEARCH workspace_memory_fact USING COVERING INDEX idx_workspace_memory_fact_v2_page (workspace_id=?)"],
        ),
        (
            "workspace history page",
            "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state,
                    lifecycle, created_ms, updated_ms
             FROM session WHERE workspace_id = ?1 AND updated_ms <= ?2
               AND (updated_ms < ?3 OR (updated_ms = ?3 AND id < ?4))
             ORDER BY updated_ms DESC, id DESC LIMIT ?5",
            &[1, 100, 50, 7, 21],
            &["SEARCH session USING INDEX idx_session_workspace_updated_id (workspace_id=? AND updated_ms<?)"],
        ),
        (
            "terminal ledger type page",
            "SELECT seq, entry_type, schema_ver, payload, created_ms FROM (
                 SELECT * FROM (SELECT seq, entry_type, schema_ver, payload, created_ms
                  FROM ledger_entry WHERE session_id = ?1 AND seq > ?2
                    AND entry_type = ?3 ORDER BY seq ASC LIMIT ?5)
                 UNION ALL
                 SELECT * FROM (SELECT seq, entry_type, schema_ver, payload, created_ms
                  FROM ledger_entry WHERE session_id = ?1 AND seq > ?2
                    AND entry_type = ?4 ORDER BY seq ASC LIMIT ?5))
             ORDER BY seq ASC LIMIT ?6",
            &[1, 0, 0, 0, 128, 128],
            &[
                "MERGE (UNION ALL)",
                "LEFT",
                "CO-ROUTINE (subquery-1)",
                "SEARCH ledger_entry USING INDEX idx_ledger_entry_session_type_seq (session_id=? AND entry_type=? AND seq>?)",
                "SCAN (subquery-1)",
                "USE TEMP B-TREE FOR ORDER BY",
                "RIGHT",
                "CO-ROUTINE (subquery-3)",
                "SEARCH ledger_entry USING INDEX idx_ledger_entry_session_type_seq (session_id=? AND entry_type=? AND seq>?)",
                "SCAN (subquery-3)",
                "USE TEMP B-TREE FOR ORDER BY",
            ],
        ),
        (
            "legacy task ledger",
            "SELECT ledger FROM task_ledger WHERE session_id = ?1
             ORDER BY updated_ms DESC LIMIT 1",
            &[1],
            &["SEARCH task_ledger USING INDEX idx_task_ledger_session_updated (session_id=?)"],
        ),
        (
            "queue head",
            "SELECT seq, op_id, prompt, files, model, variant, agent, status, requested_at
             FROM prompt_queue
             WHERE session_id = ?1 AND status IN ('pending','claimed','running')
             ORDER BY seq ASC LIMIT 1",
            &[1],
            &["SEARCH prompt_queue USING INDEX sqlite_autoindex_prompt_queue_1 (session_id=?)"],
        ),
        (
            "startup non-terminal queue scan",
            "SELECT DISTINCT session_id FROM prompt_queue
             WHERE status IN ('pending','claimed','running')",
            &[],
            &[
                "SEARCH prompt_queue USING COVERING INDEX idx_prompt_queue_status_session (status=?)",
                "USE TEMP B-TREE FOR DISTINCT",
            ],
        ),
        (
            "turn-completed count",
            "SELECT COUNT(*) FROM event WHERE session_id = ?1 AND kind = 'turn_completed'",
            &[1],
            &["SEARCH event USING COVERING INDEX idx_event_session_turn_completed (session_id=?)"],
        ),
        (
            "provider prefix rows",
            "SELECT id, prompt_prefix_hash, prompt_tokens, prefix_stability, prefix_segments_json
             FROM provider_call
             WHERE session_id = ?1 AND prompt_prefix_hash IS NOT NULL ORDER BY id ASC",
            &[1],
            &["SEARCH provider_call USING INDEX idx_provider_call_session_id (session_id=?)"],
        ),
        (
            "global running tool runs",
            "SELECT id FROM tool_run WHERE status = 'running'
             ORDER BY started_ms ASC, id ASC",
            &[],
            &["SCAN tool_run USING COVERING INDEX idx_tool_run_running"],
        ),
        (
            "newest evidence page",
            "SELECT id, session_id, workspace_id, task_id, kind, revision, provenance,
                    compressibility, compression, retrieval, compact, backing_cas_hash,
                    completeness, created_ms
             FROM evidence
             WHERE session_id = ?1 AND workspace_id = ?2 AND id < ?3
             ORDER BY id DESC LIMIT ?4",
            &[1, 1, i64::MAX, 10],
            &["SEARCH evidence USING INDEX idx_evidence_scope (session_id=? AND workspace_id=? AND id<?)"],
        ),
        (
            "task listing",
            "SELECT task_id, session_id, goal, acceptance_criteria, plan, max_tokens, max_turns,
                    spent_tokens, spent_turns, state, created_ms, updated_ms, revision,
                    attachments, contract
             FROM task WHERE session_id = ?1 ORDER BY created_ms ASC, task_id ASC",
            &[1],
            &["SEARCH task USING INDEX idx_task_session_created (session_id=?)"],
        ),
        (
            "store-open memory fact watermark",
            "SELECT COALESCE(MAX(updated_ms), 0) FROM memory_fact",
            &[],
            &["SEARCH memory_fact USING COVERING INDEX idx_memory_fact_updated"],
        ),
        (
            "legacy import session scan",
            "SELECT DISTINCT session_id FROM memory_fact
             WHERE kind IN (?1, ?2) ORDER BY session_id ASC",
            &[0, 1],
            &[
                "SEARCH memory_fact USING COVERING INDEX idx_memory_fact_kind_session (kind=?)",
                "USE TEMP B-TREE FOR DISTINCT",
            ],
        ),
        (
            "global active turn scan",
            "SELECT id FROM turn_record WHERE status = 'active'
             ORDER BY started_at ASC, id ASC",
            &[],
            &["SCAN turn_record USING COVERING INDEX idx_turn_record_active"],
        ),
    ];

    for (name, sql, values, expected) in checks {
        assert_plan(name, plan(&conn, sql, values), expected);
    }

    bench_sql(
        &conn,
        "messages_before",
        "SELECT id, session_id, seq, role, data, created_ms FROM message
         WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
        &[1, 100, 50],
    );
    bench_sql(
        &conn,
        "parts_of",
        "SELECT id, message_id, kind, data, created_ms FROM part
         WHERE message_id = ?1 ORDER BY id ASC",
        &[1],
    );
    bench_sql(
        &conn,
        "permission_pending",
        "SELECT id, op_id FROM permission
         WHERE session_id = ?1 AND decision = 'pending' AND expires_ms <= ?2 ORDER BY id ASC",
        &[1, 9_999_999_999],
    );
    bench_sql(
        &conn,
        "session_list_workspace",
        "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state,
                lifecycle, created_ms, updated_ms
         FROM session WHERE workspace_id = ?1 ORDER BY updated_ms DESC",
        &[1],
    );
    bench_sql(
        &conn,
        "task_ledger_get",
        "SELECT ledger FROM task_ledger WHERE session_id = ?1 ORDER BY updated_ms DESC LIMIT 1",
        &[1],
    );
    bench_sql(
        &conn,
        "turn_completed_count",
        "SELECT COUNT(*) FROM event WHERE session_id = ?1 AND kind = 'turn_completed'",
        &[1],
    );
    bench_sql(
        &conn,
        "evidence_newest",
        "SELECT id, session_id, workspace_id, task_id, kind, revision, provenance,
                compressibility, compression, retrieval, compact, backing_cas_hash,
                completeness, created_ms
         FROM evidence
         WHERE session_id = ?1 AND workspace_id = ?2 AND id < ?3
         ORDER BY id DESC LIMIT ?4",
        &[1, 1, i64::MAX, 10],
    );
    bench_sql(
        &conn,
        "task_list",
        "SELECT task_id, session_id, goal, acceptance_criteria, plan, max_tokens, max_turns,
                spent_tokens, spent_turns, state, created_ms, updated_ms, revision,
                attachments, contract
         FROM task WHERE session_id = ?1 ORDER BY created_ms ASC, task_id ASC",
        &[1],
    );
}

/// Audit evidence only (ignored): measure the INSERT/UPDATE hot paths with
/// the head indexes, then drop the v36 additions and recreate the pre-audit
/// duplicate indexes to report the actual write-path delta. Best of three.
#[test]
#[ignore = "[fault] manual audit benchmark"]
fn write_path_before_after() {
    static BASE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(100_000);
    fn run(conn: &rusqlite::Connection, label: &str) {
        let base = BASE.fetch_add(10_000, std::sync::atomic::Ordering::SeqCst);
        let mut best = u128::MAX;
        for iter in 0..3i64 {
            let started = Instant::now();
            conn.execute_batch("BEGIN").unwrap();
            {
                let mut event = conn
                    .prepare(
                        "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload, payload_ver)
                         VALUES (?1, 9999, NULL, 'tick', '\"ok\"', 1, NULL, 1)",
                    )
                    .unwrap();
                let mut message = conn
                    .prepare(
                        "INSERT INTO message(session_id, seq, role, data, created_ms)
                         VALUES (9999, ?1, 'user', '{}', 1)",
                    )
                    .unwrap();
                let mut part = conn
                    .prepare(
                        "INSERT INTO part(message_id, kind, data, created_ms)
                         VALUES (1, 'text', '{}', 1)",
                    )
                    .unwrap();
                let mut call = conn
                    .prepare(
                        "INSERT INTO provider_call(session_id, op_id, provider, model, started_ms,
                            status, attempt_op_id, prompt_prefix_hash, prompt_tokens)
                         VALUES (9999, ?1, 'p', 'm', 1, 'completed', ?1, ?2, 10)",
                    )
                    .unwrap();
                for i in 0..2_000 {
                    event.execute(params![base + iter * 2_000 + i]).unwrap();
                    message.execute(params![base + iter * 2_000 + i]).unwrap();
                    part.execute([]).unwrap();
                    call.execute(params![base + iter * 2_000 + i, vec![1u8; 32]])
                        .unwrap();
                }
            }
            conn.execute(
                "UPDATE session SET state = '\"idle\"', updated_ms = updated_ms + 1 WHERE id = 9999",
                [],
            )
            .unwrap();
            conn.execute_batch("COMMIT").unwrap();
            best = best.min(started.elapsed().as_micros());
        }
        println!("WRITE {label}: {best} us / 2000 rows");
    }

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    seed(&store);
    let conn = store.raw_conn();
    run(&conn, "after");
    conn.execute_batch(
        "DROP INDEX idx_part_message;
         DROP INDEX idx_provider_call_session_id;
         CREATE INDEX idx_event_session_seq ON event(session_id, seq);
         CREATE INDEX idx_message_session_seq ON message(session_id, seq);",
    )
    .unwrap();
    run(&conn, "before");
}

/// Audit evidence only (ignored): measure the same hot reads with the head
/// indexes, then drop the audited additions (and recreate the pre-audit
/// duplicates) and measure again. This is the before/after pair quoted by
/// the query-plan audit; it is not a CI timing gate.
#[test]
#[ignore = "[fault] manual audit benchmark"]
fn read_path_before_after() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    seed(&store);
    let conn = store.raw_conn();

    let parts = "SELECT id, message_id, kind, data, created_ms FROM part
         WHERE message_id = ?1 ORDER BY id ASC";
    let permission = "SELECT id, op_id FROM permission
         WHERE session_id = ?1 AND decision = 'pending' AND expires_ms <= ?2
         ORDER BY id ASC";
    let sessions = "SELECT id, workspace_id, worktree_id, task_id, title, provider, model,
            state, lifecycle, created_ms, updated_ms
         FROM session WHERE workspace_id = ?1 ORDER BY updated_ms DESC";
    let ledger = "SELECT ledger FROM task_ledger WHERE session_id = ?1
         ORDER BY updated_ms DESC LIMIT 1";
    let evidence_new = "SELECT id, session_id, workspace_id, task_id, kind, revision,
            provenance, compressibility, compression, retrieval, compact,
            backing_cas_hash, completeness, created_ms
         FROM evidence WHERE session_id = ?1 AND workspace_id = ?2 AND id < ?3
         ORDER BY id DESC LIMIT ?4";
    let evidence_old = "SELECT id, session_id, workspace_id, task_id, kind, revision,
            provenance, compressibility, compression, retrieval, compact,
            backing_cas_hash, completeness, created_ms
         FROM evidence WHERE session_id = ?1 AND workspace_id = ?2 AND id < ?3
         ORDER BY created_ms DESC, id DESC LIMIT ?4";
    let tasks = "SELECT task_id, session_id, goal, acceptance_criteria, plan, max_tokens,
            max_turns, spent_tokens, spent_turns, state, created_ms, updated_ms, revision,
            attachments, contract
         FROM task WHERE session_id = ?1 ORDER BY created_ms ASC, task_id ASC";
    let prefix = "SELECT id, prompt_prefix_hash, prompt_tokens, prefix_stability,
            prefix_segments_json FROM provider_call
         WHERE session_id = ?1 AND prompt_prefix_hash IS NOT NULL ORDER BY id ASC";
    let max_fact = "SELECT COALESCE(MAX(updated_ms), 0) FROM memory_fact";
    let kind_scan = "SELECT DISTINCT session_id FROM memory_fact
         WHERE kind IN (?1, ?2) ORDER BY session_id ASC";
    let turn_count = "SELECT COUNT(*) FROM event WHERE session_id = ?1 AND kind = 'turn_completed'";
    let turn_active = "SELECT id FROM turn_record WHERE status = 'active'
         ORDER BY started_at ASC, id ASC";
    let running = "SELECT id FROM tool_run WHERE status = 'running'
         ORDER BY started_ms ASC, id ASC";

    for (name, sql, vals) in [
        ("parts", parts, vec![1]),
        ("permission", permission, vec![1, 9_999_999_999]),
        ("sessions", sessions, vec![1]),
        ("ledger", ledger, vec![1]),
        ("evidence", evidence_new, vec![1, 1, i64::MAX, 10]),
        ("tasks", tasks, vec![1]),
        ("prefix", prefix, vec![1]),
        ("max_fact", max_fact, vec![]),
        ("kind_scan", kind_scan, vec![0, 1]),
        ("turn_count", turn_count, vec![1]),
        ("turn_active", turn_active, vec![]),
        ("running", running, vec![]),
    ] {
        bench_sql(&conn, &format!("after:{name}"), sql, &vals);
    }

    conn.execute_batch(
        "DROP INDEX idx_part_message;
         DROP INDEX idx_permission_session_pending;
         DROP INDEX idx_permission_decision_id;
         DROP INDEX idx_session_workspace_updated;
         DROP INDEX idx_task_ledger_session_updated;
         DROP INDEX idx_task_session_created;
         CREATE INDEX idx_task_session_updated ON task(session_id, updated_ms);
         DROP INDEX idx_provider_call_session_id;
         DROP INDEX idx_memory_fact_updated;
         DROP INDEX idx_memory_fact_kind_session;
         DROP INDEX idx_event_session_turn_completed;
         DROP INDEX idx_turn_record_active;
         DROP INDEX idx_tool_run_running;",
    )
    .unwrap();
    for (name, sql, vals) in [
        ("parts", parts, vec![1]),
        ("permission", permission, vec![1, 9_999_999_999]),
        ("sessions", sessions, vec![1]),
        ("ledger", ledger, vec![1]),
        ("evidence", evidence_old, vec![1, 1, i64::MAX, 10]),
        ("tasks", tasks, vec![1]),
        ("prefix", prefix, vec![1]),
        ("max_fact", max_fact, vec![]),
        ("kind_scan", kind_scan, vec![0, 1]),
        ("turn_count", turn_count, vec![1]),
        ("turn_active", turn_active, vec![]),
        ("running", running, vec![]),
    ] {
        bench_sql(&conn, &format!("before:{name}"), sql, &vals);
    }
}

/// Differential against the OLD evidence statement (git aa3f9e85:
/// `ORDER BY created_ms DESC, id DESC` with an exclusive `id < before`
/// cursor) over 600 adversarial rows (duplicate timestamps, created_ms/id
/// disagreement, unicode payloads). The old page walk permanently skips
/// rows; the rewritten id-ordered statement serves every row exactly once.
#[test]
fn evidence_old_created_order_skips_rows_the_new_id_order_serves_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/evidence-diff").unwrap();
    let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
    for id in 1..=600u64 {
        let row = EvidenceRow {
            id,
            session_id: sid,
            workspace_id: ws,
            task_id: Some(7),
            kind: "process_log".into(),
            revision: 1,
            provenance_json: r#"{"entries":["tool"]}"#.into(),
            compressibility: "aggressive".into(),
            compression_json: r#"{"algorithm":"identity"}"#.into(),
            retrieval_json: r#"{"allow_ranges":true,"allow_search":true,"max_bytes":64}"#.into(),
            compact_json: format!("{{\"text\":\"unicode-漢字😀-{id}\"}}"),
            backing_cas_hash: Some("ab".repeat(32)),
            completeness: "complete".into(),
            // Deliberately disagree with id order; ties every 40 rows.
            created_ms: ((id * 37) % 600) as i64,
        };
        store.evidence_insert(&row).unwrap();
    }
    let conn = store.raw_conn();
    let page = |sql: &str, before: i64| -> Vec<u64> {
        let mut stmt = conn.prepare(sql).unwrap();
        let mut rows = stmt
            .query(rusqlite::params![
                sid.raw() as i64,
                ws.raw() as i64,
                before,
                37i64
            ])
            .unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            out.push(row.get::<_, i64>(0).unwrap() as u64);
        }
        out
    };
    let old_sql = "SELECT id FROM evidence
         WHERE session_id = ?1 AND workspace_id = ?2 AND id < ?3
         ORDER BY created_ms DESC, id DESC LIMIT ?4";
    let new_sql = "SELECT id FROM evidence
         WHERE session_id = ?1 AND workspace_id = ?2 AND id < ?3
         ORDER BY id DESC LIMIT ?4";
    let mut old_ids = Vec::new();
    let mut before = i64::MAX;
    loop {
        let rows = page(old_sql, before);
        if rows.is_empty() {
            break;
        }
        before = *rows.last().unwrap() as i64;
        old_ids.extend(rows);
    }
    let mut new_ids = Vec::new();
    let mut before = i64::MAX;
    loop {
        let rows = page(new_sql, before);
        if rows.is_empty() {
            break;
        }
        before = *rows.last().unwrap() as i64;
        new_ids.extend(rows);
    }
    new_ids.sort_unstable();
    assert_eq!(new_ids, (1..=600).collect::<Vec<u64>>());
    assert_eq!(new_ids.len(), 600, "every row is served exactly once");
    old_ids.sort_unstable();
    old_ids.dedup();
    assert!(
        old_ids.len() < 600,
        "the old created_ms/id cursor disagreement must skip rows (served {})",
        old_ids.len()
    );
}
