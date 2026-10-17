// Seeded query-plan assertions for the updater operation store. The
// idempotency lookup used to filter a JSON expression with no index; the
// expression index and the ordering indexes are pinned here. Included inside
// `store::tests` for the private lock.

use rusqlite::params;

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

#[test]
fn updater_hot_query_plans_stay_index_backed() {
    let store = SqliteUpdaterStore::open_in_memory().unwrap();
    {
        let conn = store.lock().unwrap();
        conn.execute_batch("BEGIN").unwrap();
        let mut st = conn
            .prepare(
                "INSERT INTO update_operation(id, kind, status, created_ms, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .unwrap();
        for i in 0..10_000i64 {
            let kind = if i % 2 == 0 { "check" } else { "apply" };
            let status = if i % 100 == 0 { "running" } else { "completed" };
            let payload = format!(
                "{{\"id\":\"op-{i}\",\"kind\":\"{kind}\",\"status\":\"{status}\",\
                 \"created_ms\":{i},\"idempotency_key\":\"key-{i}\",\"detail\":\"\"}}"
            );
            st.execute(params![format!("op-{i}"), kind, status, i, payload])
                .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    let conn = store.lock().unwrap();

    let by_key = plan(
        &conn,
        "SELECT payload FROM update_operation
         WHERE json_valid(payload)
           AND json_extract(payload, '$.idempotency_key') = ?1
         ORDER BY created_ms DESC, id DESC LIMIT 1",
        &[0],
    );
    assert_eq!(
        by_key,
        vec!["SEARCH update_operation USING INDEX update_operation_idempotency_key (<expr>=?)"],
        "updater idempotency lookup plan regressed: {by_key:?}"
    );

    let list = plan(
        &conn,
        "SELECT payload FROM update_operation ORDER BY created_ms DESC, id DESC LIMIT ?1",
        &[50],
    );
    assert_eq!(
        list,
        vec!["SCAN update_operation USING INDEX update_operation_created"],
        "updater listing plan regressed: {list:?}"
    );

    let running = plan(
        &conn,
        "SELECT payload FROM update_operation
         WHERE status = 'running' ORDER BY created_ms ASC, id ASC",
        &[],
    );
    assert_eq!(
        running,
        vec!["SEARCH update_operation USING INDEX update_operation_status (status=?)"],
        "updater running plan regressed: {running:?}"
    );

    let latest = plan(
        &conn,
        "SELECT payload FROM update_operation
         WHERE kind = ?1 AND status = ?2
         ORDER BY created_ms DESC, id DESC LIMIT 1",
        &[0, 0],
    );
    assert_eq!(
        latest,
        vec!["SEARCH update_operation USING INDEX update_operation_kind_status (kind=? AND status=?)"],
        "updater latest plan regressed: {latest:?}"
    );
}

/// Corrupt/non-JSON payloads must never brick the v4 expression-index
/// migration: the index is partial on `json_valid(payload)`, so the rebuild
/// excludes them instead of failing, the lookup query excludes them too
/// (they could never be decoded into an operation), and the schema reaches
/// the head. This is the upgrade-from-previous-head path, not a fresh DB.
#[test]
fn corrupt_payload_does_not_brick_the_idempotency_index_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("update.db");
    {
        let store = SqliteUpdaterStore::open(&path).unwrap();
        let conn = store.lock().unwrap();
        conn.execute(
            "INSERT INTO update_operation(id, kind, status, created_ms, payload)
             VALUES ('corrupt', 'check', 'running', 1, 'not-json')",
            [],
        )
        .unwrap();
        let head = UPDATER_MIGRATIONS.len() as i64;
        conn.execute_batch(&format!(
            "DROP INDEX IF EXISTS update_operation_idempotency_key;
             PRAGMA user_version = {};",
            head - 1
        ))
        .unwrap();
    }
    let store = SqliteUpdaterStore::open(&path).unwrap();
    {
        let conn = store.lock().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, UPDATER_MIGRATIONS.len() as i64);
    }
    assert!(
        store.by_key("anything").unwrap().is_none(),
        "the corrupt row is excluded, never surfaced as a replay"
    );
}

/// Differential against the OLD idempotency statement (git aa3f9e85), which
/// filtered `json_extract(payload,'$.idempotency_key') = ?` with no
/// `json_valid` guard. For every VALID row the old and new statements return
/// the same payload (including duplicate keys, created_ms ties and unicode);
/// one corrupt row makes the old statement raise a malformed-JSON error,
/// while the new partial-index predicate skips it and still answers.
#[test]
fn idempotency_lookup_differential_matches_old_sql_and_tolerates_corrupt_rows() {
    let store = SqliteUpdaterStore::open_in_memory().unwrap();
    {
        let conn = store.lock().unwrap();
        conn.execute_batch("BEGIN").unwrap();
        let mut st = conn
            .prepare(
                "INSERT INTO update_operation(id, kind, status, created_ms, payload)
                 VALUES (?1, 'check', 'completed', ?2, ?3)",
            )
            .unwrap();
        for i in 0..600i64 {
            let key = format!("key-{}-漢字", i % 17);
            // Duplicate timestamps every 9 rows force the (created_ms, id)
            // tie-break to be the only discriminator.
            let mut op = UpdateOperation::new(UpdateOpKind::Check, i % 9, Some("😀".into()));
            op.idempotency_key = Some(key);
            let payload = serde_json::to_string(&op).unwrap();
            st.execute(params![op.id.to_string(), i % 9, payload])
                .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    let keys: Vec<String> = (0..17).map(|i| format!("key-{i}-漢字")).collect();
    let mut valid = std::collections::HashMap::new();
    for key in &keys {
        let old: Option<String> = store
            .lock()
            .unwrap()
            .query_row(
                "SELECT payload FROM update_operation
                 WHERE json_extract(payload, '$.idempotency_key') = ?1
                 ORDER BY created_ms DESC, id DESC LIMIT 1",
                params![key],
                |r| r.get(0),
            )
            .ok();
        let new = store.by_key(key).unwrap();
        let new_payload = new
            .as_ref()
            .map(|op| serde_json::to_string(op).unwrap());
        assert_eq!(old, new_payload, "valid-row differential diverged for {key}");
        valid.insert(key.clone(), new_payload);
    }
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "INSERT INTO update_operation(id, kind, status, created_ms, payload)
             VALUES ('corrupt', 'check', 'running', 1, 'not-json')",
            [],
        )
        .unwrap();
        let old_err = conn
            .query_row(
                "SELECT payload FROM update_operation
                 WHERE json_extract(payload, '$.idempotency_key') = ?1
                 ORDER BY created_ms DESC, id DESC LIMIT 1",
                params!["key-0-漢字"],
                |r| r.get::<_, String>(0),
            )
            .unwrap_err();
        assert!(
            old_err.to_string().to_lowercase().contains("json"),
            "the old statement must fail on the corrupt row: {old_err}"
        );
    }
    assert_eq!(
        store
            .by_key("key-0-漢字")
            .unwrap()
            .as_ref()
            .map(|op| serde_json::to_string(op).unwrap()),
        valid["key-0-漢字"],
        "the new guarded statement keeps the same winner around a corrupt row"
    );
    assert!(store.by_key("missing").unwrap().is_none());
}
