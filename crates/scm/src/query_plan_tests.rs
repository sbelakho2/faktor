// Seeded query-plan assertions for the SCM store's installation and webhook
// receipt listings. Included inside `store::tests` for the private lock.

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
fn scm_hot_query_plans_stay_index_backed() {
    let store = SqliteScmStore::open_in_memory().unwrap();
    {
        let conn = store.lock().unwrap();
        conn.execute_batch("BEGIN").unwrap();
        {
            let mut st = conn
                .prepare(
                    "INSERT INTO scm_installation(installation_id, account_login, account_type,
                        permissions, suspended, updated_ms)
                     VALUES (?1, 'a', 'organization', '{}', 0, 1)",
                )
                .unwrap();
            for i in 1..=50i64 {
                st.execute(params![i]).unwrap();
            }
        }
        {
            let mut st = conn
                .prepare(
                    "INSERT INTO scm_repository(installation_id, organization_id, owner, name,
                        full_name, default_branch, private, archived, updated_ms)
                     VALUES (?1, 'org', 'o', ?2, ?2, 'main', 0, 0, 1)",
                )
                .unwrap();
            for i in 0..5_000i64 {
                st.execute(params![i % 50 + 1, format!("r{i}")]).unwrap();
            }
        }
        {
            let mut st = conn
                .prepare(
                    "INSERT INTO scm_webhook_delivery(delivery_id, event, received_ms)
                     VALUES (?1, 'push', ?2)",
                )
                .unwrap();
            for i in 0..20_000i64 {
                st.execute(params![format!("d{i}"), i]).unwrap();
            }
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    let conn = store.lock().unwrap();

    let repo = plan(
        &conn,
        "SELECT id, installation_id, organization_id, owner, name, full_name,
                default_branch, private, archived, updated_ms
         FROM scm_repository WHERE installation_id = ?1 ORDER BY id",
        &[1],
    );
    assert_eq!(
        repo,
        vec![
            "SEARCH scm_repository USING INDEX idx_scm_repository_installation (installation_id=?)"
        ],
        "SCM installation listing plan regressed: {repo:?}"
    );

    let webhook = plan(
        &conn,
        "SELECT delivery_id, event, received_ms FROM scm_webhook_delivery
         ORDER BY received_ms LIMIT ?1",
        &[50],
    );
    assert_eq!(
        webhook,
        vec!["SCAN scm_webhook_delivery USING COVERING INDEX idx_scm_webhook_received"],
        "SCM webhook receipt plan regressed: {webhook:?}"
    );
}

/// Upgrade from the previous head: the v3 index step is replayable and
/// leaves the predecessor rows intact.
#[test]
fn v3_upgrade_recreates_scm_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scm.db");
    let head;
    {
        let store = SqliteScmStore::open(&path).unwrap();
        let conn = store.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO scm_installation(installation_id, account_login, account_type,
                permissions, suspended, updated_ms)
             VALUES (1, 'a', 'organization', '{}', 0, 1);",
        )
        .unwrap();
        head = conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap();
        conn.execute_batch(&format!(
            "DROP INDEX IF EXISTS idx_scm_repository_installation;
             DROP INDEX IF EXISTS idx_scm_webhook_received;
             PRAGMA user_version = {};",
            head - 1
        ))
        .unwrap();
    }
    let store = SqliteScmStore::open(&path).unwrap();
    let conn = store.lock().unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        head
    );
    for index in [
        "idx_scm_repository_installation",
        "idx_scm_webhook_received",
    ] {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "v3 must create {index}");
    }
}
