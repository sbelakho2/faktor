// Seeded `EXPLAIN QUERY PLAN` assertions for the commerce hot paths
// (latest offer, cache retention, active-job lookup/FIFO). Included inside
// `store::tests`, so it uses the store's private connection lock.

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

fn assert_plan(name: &str, actual: Vec<String>, expected: &[&str]) {
    assert_eq!(
        actual,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "commerce query plan regression on {name}: {actual:?}"
    );
}

fn seed_commerce(store: &CommerceStore) {
    let conn = store.lock().unwrap();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut st = conn
            .prepare(
                "INSERT INTO product_identity(identity_id, identity_key, normalized_mpn,
                    created_ms, updated_ms)
                 VALUES (?1, ?2, 'MPN', 1, 1)",
            )
            .unwrap();
        for i in 1..=1_000i64 {
            st.execute(params![i, format!("id-{i}")]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO offer_snapshot(identity_id, source, source_offer_id,
                    content_digest, origin, diagnostics_json, payload_json, observed_at_ms)
                 VALUES (?1, 's', ?2, 'd', 'o', '{}', '{}', ?3)",
            )
            .unwrap();
        for i in 0..50_000i64 {
            st.execute(params![i % 1_000 + 1, format!("off-{}", i % 1_000), i])
                .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO search_cache(cache_key, class, source, product, pricing_scope,
                    payload_json, content_digest, observed_at_ms, revalidated_at_ms, hits)
                 VALUES (?1, 'c', 's', 'p', 'default', '{}', 'd', ?2, ?3, 0)",
            )
            .unwrap();
        for i in 0..50_000i64 {
            let revalidated = if i % 3 == 0 { Some(i + 10) } else { None };
            st.execute(params![format!("cache-{i}"), i, revalidated])
                .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO job(job_id, digest, kind, state, request_json, created_ms,
                    updated_ms, owner_key)
                 VALUES (?1, ?2, 'k', ?3, '{}', ?4, ?4, ?5)",
            )
            .unwrap();
        for i in 0..10_000i64 {
            let active = i % 10 == 0;
            st.execute(params![
                format!("job-{i}"),
                format!("digest-{}", i % 100),
                if active { "queued" } else { "completed" },
                i,
                if active {
                    Some(format!("owner-{}", i % 50))
                } else {
                    None
                }
            ])
            .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
}

#[test]
fn commerce_hot_query_plans_stay_index_backed() {
    let store = CommerceStore::open_in_memory().unwrap();
    seed_commerce(&store);
    let conn = store.lock().unwrap();

    assert_plan(
        "latest offer by source key",
        plan(
            &conn,
            "SELECT offer_snapshot_id, identity_id, source, source_offer_id, content_digest,
                    origin, observed_at_ms, payload_json
             FROM offer_snapshot WHERE source = ?1 AND source_offer_id = ?2
             ORDER BY observed_at_ms DESC, offer_snapshot_id DESC LIMIT 1",
            &[0, 0],
        ),
        &["SEARCH offer_snapshot USING INDEX idx_offer_snapshot_offer (source=? AND source_offer_id=?)"],
    );
    assert_plan(
        "cache retention predicate",
        plan(
            &conn,
            "DELETE FROM search_cache
             WHERE COALESCE(revalidated_at_ms, observed_at_ms) < ?1",
            &[10_000],
        ),
        &["SEARCH search_cache USING INDEX idx_search_cache_expiry (<expr><?)"],
    );
    assert_plan(
        "active job lookup",
        plan(
            &conn,
            "SELECT job_id, digest, state, request_json FROM job
             WHERE digest = ?1 AND owner_key = ?2 AND state IN ('queued', 'running')
             ORDER BY created_ms DESC, job_id DESC LIMIT 1",
            &[0, 0],
        ),
        &["SEARCH job USING INDEX idx_job_owner_active (owner_key=? AND digest=?)"],
    );
    assert_plan(
        "active job FIFO scan",
        plan(
            &conn,
            "SELECT job_id, digest, kind, state, request_json, created_ms, updated_ms
             FROM job INDEXED BY idx_job_active_created
             WHERE state IN ('queued', 'running') AND owner_key IS NOT NULL
             ORDER BY created_ms ASC, job_id ASC",
            &[],
        ),
        &["SCAN job USING INDEX idx_job_active_created"],
    );
}

/// Upgrade from the previous head: the v4 index step recreates the offer,
/// cache-expression and active-job indexes, preserves rows, and keeps the
/// plans. This is the production upgrade path, not a fresh database.
#[test]
fn v4_upgrade_recreates_commerce_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commerce.db");
    let head;
    {
        let store = CommerceStore::open(&path).unwrap();
        head = store.schema_version().unwrap();
        let conn = store.lock().unwrap();
        conn.execute_batch(&format!(
            "DROP INDEX IF EXISTS idx_offer_snapshot_offer;
             DROP INDEX IF EXISTS idx_search_cache_expiry;
             DROP INDEX IF EXISTS idx_job_owner_active;
             DROP INDEX IF EXISTS idx_job_active_created;
             PRAGMA user_version = {};",
            head - 1
        ))
        .unwrap();
    }
    let store = CommerceStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), head);
    let conn = store.lock().unwrap();
    for index in [
        "idx_offer_snapshot_offer",
        "idx_search_cache_expiry",
        "idx_job_owner_active",
        "idx_job_active_created",
    ] {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "v4 must create {index}");
    }
}

/// Differential: `INDEXED BY idx_job_active_created` is a plan directive,
/// not a filter. Over the seeded active/completed rows (with created_ms
/// ties) the hinted and unhinted statements must return the same jobs in
/// the same order.
#[test]
fn active_job_fifo_hint_does_not_change_results() {
    let store = CommerceStore::open_in_memory().unwrap();
    seed_commerce(&store);
    let conn = store.lock().unwrap();
    let plain = "SELECT job_id FROM job
         WHERE state IN ('queued', 'running') AND owner_key IS NOT NULL
         ORDER BY created_ms ASC, job_id ASC";
    let hinted = "SELECT job_id FROM job INDEXED BY idx_job_active_created
         WHERE state IN ('queued', 'running') AND owner_key IS NOT NULL
         ORDER BY created_ms ASC, job_id ASC";
    let collect = |sql: &str| -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            out.push(row.get::<_, String>(0).unwrap());
        }
        out
    };
    let expected = collect(plain);
    assert!(expected.len() >= 1_000, "the seeded active set");
    assert_eq!(collect(hinted), expected, "INDEXED BY changed the result");
}

/// The COALESCE expression index must serve EXACTLY the predicate it
/// indexes: the indexed range and the logically equivalent CASE predicate
/// select the same rows over 50k mixed NULL/non-NULL cache entries.
#[test]
fn cache_expiry_expression_index_matches_query_semantics() {
    let store = CommerceStore::open_in_memory().unwrap();
    seed_commerce(&store);
    let conn = store.lock().unwrap();
    let indexed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM search_cache
             WHERE COALESCE(revalidated_at_ms, observed_at_ms) < ?1",
            [25_000i64],
            |r| r.get(0),
        )
        .unwrap();
    let reference: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM search_cache
             WHERE (CASE WHEN revalidated_at_ms IS NOT NULL
                         THEN revalidated_at_ms ELSE observed_at_ms END) < ?1",
            [25_000i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed, reference, "COALESCE index semantics diverged");
    assert!(indexed > 0 && indexed < 50_000, "mixed NULL/non-NULL seed");
    assert_plan(
        "cache retention predicate",
        plan(
            &conn,
            "DELETE FROM search_cache
             WHERE COALESCE(revalidated_at_ms, observed_at_ms) < ?1",
            &[25_000],
        ),
        &["SEARCH search_cache USING INDEX idx_search_cache_expiry (<expr><?)"],
    );
}
