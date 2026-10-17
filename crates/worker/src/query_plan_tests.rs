// Seeded query-plan assertions for the worker plane's live-lease listings
// (the indexes now carry lease_id so no page needs a temp sort). Included
// inside `store::tests` for the private connection lock.

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
fn worker_hot_query_plans_stay_index_backed() {
    let store = SqliteWorkerStore::open_in_memory().unwrap();
    {
        let conn = store.lock().unwrap();
        conn.execute_batch("BEGIN").unwrap();
        {
            let mut st = conn
                .prepare(
                    "INSERT INTO wp_worker(id, organization_id, trust_domain, revoked,
                        token_hash, payload)
                     VALUES (?1, ?2, 'td', 0, ?3, '{}')",
                )
                .unwrap();
            for i in 0..2_000i64 {
                st.execute(params![
                    format!("w{i}"),
                    format!("org-{}", i % 20),
                    format!("th-{i}")
                ])
                .unwrap();
            }
        }
        {
            let mut st = conn
                .prepare(
                    "INSERT INTO wp_lease(lease_id, organization_id, job_id, generation,
                        worker_id, state, expires_at_ms, payload)
                     VALUES (?1, ?2, ?3, ?5, ?4, 'live', 1, '{}')",
                )
                .unwrap();
            for i in 0..10_000i64 {
                st.execute(params![
                    format!("l{i:06}"),
                    format!("org-{}", i % 20),
                    format!("j{}", i % 5_000),
                    format!("w{}", i % 2_000),
                    i / 5_000 + 1
                ])
                .unwrap();
            }
        }
        {
            let mut st = conn
                .prepare(
                    "INSERT INTO wp_journal(organization_id, kind, job_id, worker_id,
                        generation, at_ms, detail)
                     VALUES (?1, 'k', NULL, NULL, NULL, ?2, '{}')",
                )
                .unwrap();
            for i in 0..20_000i64 {
                st.execute(params![format!("org-{}", i % 20), i]).unwrap();
            }
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    let conn = store.lock().unwrap();

    let org = plan(
        &conn,
        "SELECT payload FROM wp_lease
         WHERE organization_id = ?1 AND state = 'live' ORDER BY lease_id",
        &[1],
    );
    assert_eq!(
        org,
        vec!["SEARCH wp_lease USING INDEX idx_wp_lease_org_state (organization_id=? AND state=?)"],
        "worker org live-lease plan regressed: {org:?}"
    );

    let worker = plan(
        &conn,
        "SELECT payload FROM wp_lease
         WHERE worker_id = ?1 AND state = 'live' ORDER BY lease_id",
        &[1],
    );
    assert_eq!(
        worker,
        vec!["SEARCH wp_lease USING INDEX idx_wp_lease_worker (worker_id=? AND state=?)"],
        "worker live-lease plan regressed: {worker:?}"
    );

    let journal = plan(
        &conn,
        "SELECT seq, organization_id, kind, job_id, worker_id, generation, at_ms, detail
         FROM wp_journal WHERE organization_id = ?1 AND (?2 IS NULL OR seq > ?2)
         ORDER BY seq LIMIT ?3",
        &[1, 0, 50],
    );
    assert_eq!(
        journal,
        vec!["SEARCH wp_journal USING INDEX idx_wp_journal_org_seq (organization_id=?)"],
        "worker journal page plan regressed: {journal:?}"
    );
}

/// Upgrade from the previous head: the v3 replacement indexes must be
/// recreated with lease_id appended and existing leases preserved.
#[test]
fn v3_upgrade_recreates_worker_lease_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workers.db");
    let head;
    {
        let store = SqliteWorkerStore::open(&path).unwrap();
        let conn = store.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO wp_lease(lease_id, organization_id, job_id, generation, worker_id,
                state, expires_at_ms, payload)
             VALUES ('l1', 'org', 'j1', 1, 'w1', 'live', 1, '{}');",
        )
        .unwrap();
        head = conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap();
        conn.execute_batch(&format!(
            "DROP INDEX IF EXISTS idx_wp_lease_org_state;
             DROP INDEX IF EXISTS idx_wp_lease_worker;
             PRAGMA user_version = {};",
            head - 1
        ))
        .unwrap();
    }
    let store = SqliteWorkerStore::open(&path).unwrap();
    let conn = store.lock().unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        head
    );
    for index in ["idx_wp_lease_org_state", "idx_wp_lease_worker"] {
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name = ?1",
                [index],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("lease_id"),
            "{index} must be ordered by lease_id: {sql}"
        );
    }
}
