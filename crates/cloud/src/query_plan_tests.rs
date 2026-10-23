//! Seeded `EXPLAIN QUERY PLAN` assertions for the commercial control-plane
//! hot paths (retention, approvals, in-flight listing, audit boundary,
//! report-period listing). Any plan drift that reintroduces a table scan or
//! temp sort fails the crate tests.

use rusqlite::params;

use crate::store::SqliteControlPlaneStore;

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
        "commercial query plan regression on {name}: {actual:?}"
    );
}

fn seed(store: &SqliteControlPlaneStore) {
    let conn = store.lock().unwrap();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut st = conn
            .prepare("INSERT INTO cp_organization(id, payload) VALUES (?1, '{}')")
            .unwrap();
        for i in 0..500i64 {
            st.execute(params![format!("org-{i}")]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO cp_approval(id, organization_id, status, payload)
                 VALUES (?1, ?2, ?3, '{}')",
            )
            .unwrap();
        for i in 0..4_000i64 {
            st.execute(params![
                format!("appr-{i}"),
                format!("org-{}", i % 500),
                if i % 2 == 0 { "pending" } else { "approved" }
            ])
            .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO cp_idempotency(key, operation, request_hash, response, created_ms)
                 VALUES (?1, 'op', 'h', '{}', ?2)",
            )
            .unwrap();
        for i in 0..20_000i64 {
            st.execute(params![format!("key-{i}"), i]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO billing_in_flight(id, organization_id, kind, reference,
                    started_ms, ended_ms, payload)
                 VALUES (?1, ?2, 'charge', 'r', ?1, NULL, '{}')",
            )
            .unwrap();
        for i in 0..5_000i64 {
            st.execute(params![i, format!("org-{}", i % 500)]).unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO billing_report_period(organization_id, period, status,
                    next_attempt_at_ms, first_seen_ms, payload)
                 VALUES (?1, ?2, 'open', 0, ?3, '{}')",
            )
            .unwrap();
        for i in 0..2_000i64 {
            st.execute(params![format!("org-{}", i % 500), format!("p{i}"), i])
                .unwrap();
        }
    }
    {
        let mut st = conn
            .prepare(
                "INSERT INTO ent_audit_event(organization_id, event_key, action, object,
                    occurred_at_ms, payload)
                 VALUES (?1, ?2, 'a', 'o', ?3, '{}')",
            )
            .unwrap();
        for i in 0..20_000i64 {
            st.execute(params![
                format!("org-{}", i % 500),
                format!("evt-{i}"),
                i / 4
            ])
            .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
}

#[test]
fn commercial_hot_query_plans_stay_index_backed() {
    let store = SqliteControlPlaneStore::open_in_memory().unwrap();
    seed(&store);
    let conn = store.lock().unwrap();

    assert_plan(
        "idempotency age prune",
        plan(
            &conn,
            "DELETE FROM cp_idempotency WHERE created_ms < ?1",
            &[5_000],
        ),
        &["SEARCH cp_idempotency USING INDEX idx_cp_idempotency_created (created_ms<?)"],
    );
    assert_plan(
        "idempotency count prune",
        plan(
            &conn,
            "DELETE FROM cp_idempotency WHERE key IN (
                 SELECT key FROM cp_idempotency ORDER BY created_ms DESC, key DESC
                 LIMIT -1 OFFSET ?1)",
            &[100],
        ),
        &[
            "SEARCH cp_idempotency USING COVERING INDEX sqlite_autoindex_cp_idempotency_1 (key=?)",
            "LIST SUBQUERY 1",
            "SCAN cp_idempotency USING COVERING INDEX idx_cp_idempotency_created",
        ],
    );
    assert_plan(
        "idempotency max-age prune",
        plan(
            &conn,
            "DELETE FROM cp_idempotency
             WHERE created_ms < (SELECT COALESCE(MAX(created_ms), 0) FROM cp_idempotency) - ?1",
            &[100],
        ),
        &[
            "SEARCH cp_idempotency USING COVERING INDEX idx_cp_idempotency_created (created_ms<?)",
            "SCALAR SUBQUERY 1",
            "SEARCH cp_idempotency USING COVERING INDEX idx_cp_idempotency_created",
        ],
    );
    assert_plan(
        "approval NULL-status page",
        plan(
            &conn,
            "SELECT payload FROM cp_approval
             WHERE organization_id = ?1 AND (NULL IS NULL OR status = NULL)
               AND (?2 IS NULL OR id > ?2)
             ORDER BY id LIMIT ?3",
            &[1, 0, 50],
        ),
        &["SEARCH cp_approval USING INDEX idx_cp_approval_org_id (organization_id=?)"],
    );
    assert_plan(
        "approval status page",
        plan(
            &conn,
            "SELECT payload FROM cp_approval
             WHERE organization_id = ?1 AND ('pending' IS NULL OR status = 'pending')
               AND (?2 IS NULL OR id > ?2)
             ORDER BY id LIMIT ?3",
            &[1, 0, 50],
        ),
        &["SEARCH cp_approval USING INDEX idx_cp_approval_org_id (organization_id=?)"],
    );
    assert_plan(
        "in-flight listing",
        plan(
            &conn,
            "SELECT payload FROM billing_in_flight
             WHERE organization_id = ?1 ORDER BY started_ms, id",
            &[1],
        ),
        &["SEARCH billing_in_flight USING INDEX idx_billing_in_flight_org (organization_id=?)"],
    );
    assert_plan(
        "audit retention boundary",
        plan(
            &conn,
            "SELECT COALESCE(MAX(seq), 0) FROM ent_audit_event
             WHERE organization_id = ?1 AND occurred_at_ms < ?2",
            &[1, 10_000],
        ),
        &["SEARCH ent_audit_event USING COVERING INDEX idx_ent_audit_org_occurred (organization_id=? AND occurred_at_ms<?)"],
    );
    assert_plan(
        "report period listing",
        plan(
            &conn,
            "SELECT payload FROM billing_report_period
             WHERE organization_id = ?1 ORDER BY first_seen_ms DESC, period DESC LIMIT ?2",
            &[1, 50],
        ),
        &["SEARCH billing_report_period USING INDEX idx_billing_report_period_org_seen (organization_id=?)"],
    );
}

/// Upgrade from the previous head: dropping the v7 indexes and moving the
/// cursor back one version must recreate every index in ONE transactional
/// step, keep existing rows, and leave the database at the head.
#[test]
fn v7_upgrade_recreates_indexes_from_the_previous_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    let head;
    {
        let store = SqliteControlPlaneStore::open(&path).unwrap();
        let conn = store.lock().unwrap();
        conn.execute(
            "INSERT INTO cp_idempotency(key, operation, request_hash, response, created_ms)
             VALUES ('k', 'op', 'h', '{}', 1)",
            [],
        )
        .unwrap();
        head = conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap();
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_cp_idempotency_created;
             DROP INDEX IF EXISTS idx_cp_approval_org_id;
             DROP INDEX IF EXISTS idx_ent_audit_org_occurred;
             DROP INDEX IF EXISTS idx_billing_report_period_org_seen;
             DROP INDEX IF EXISTS idx_billing_in_flight_org;
             CREATE INDEX idx_billing_in_flight_org
                ON billing_in_flight(organization_id, ended_ms);
             PRAGMA user_version = 6;",
        )
        .unwrap();
    }
    let store = SqliteControlPlaneStore::open(&path).unwrap();
    let conn = store.lock().unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, head);
    for index in [
        "idx_cp_idempotency_created",
        "idx_cp_approval_org_id",
        "idx_ent_audit_org_occurred",
        "idx_billing_report_period_org_seen",
    ] {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "v7 must create {index}");
    }
    let in_flight: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_billing_in_flight_org'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        in_flight.contains("started_ms"),
        "the replaced in-flight index matches the listing order: {in_flight}"
    );
}

/// The production approval page binds the status as a PARAMETER
/// (`?2 IS NULL OR status = ?2`), so a literal-only plan assertion can miss
/// the real plan. With a NULL status parameter the org+id index must still
/// serve the page (and the id cursor), not a scan/temp sort.
#[test]
fn approval_null_parameter_plan_uses_the_org_index() {
    let store = SqliteControlPlaneStore::open_in_memory().unwrap();
    seed(&store);
    let conn = store.lock().unwrap();
    let sql = "SELECT payload FROM cp_approval
         WHERE organization_id = ?1
           AND (?2 IS NULL OR status = ?2)
           AND (?3 IS NULL OR id > ?3)
         ORDER BY id LIMIT ?4";
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let mut rows = stmt
        .query(params![
            "org-0",
            Option::<String>::None,
            Option::<String>::None,
            50i64
        ])
        .unwrap();
    let mut details = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        details.push(row.get::<_, String>(3).unwrap());
    }
    assert_eq!(
        details,
        vec!["SEARCH cp_approval USING INDEX idx_cp_approval_org_id (organization_id=?)"],
        "the real NULL-status parameter plan regressed: {details:?}"
    );
}

/// The per-request auth/tenant gate is a fixed set of POINT lookups (token
/// hash -> session/service account -> user -> organization -> membership).
/// Each statement must stay index-backed: a table scan or a per-row N+1
/// membership lookup would turn every authenticated request into O(rows).
#[test]
fn auth_and_tenant_gate_queries_are_index_point_lookups() {
    let store = SqliteControlPlaneStore::open_in_memory().unwrap();
    seed(&store);
    let conn = store.lock().unwrap();
    let cases: &[(&str, &[&dyn rusqlite::types::ToSql])] = &[
        (
            "SELECT payload FROM cp_auth_session WHERE token_hash = ?1",
            &[&"hash" as &dyn rusqlite::types::ToSql],
        ),
        (
            "SELECT payload FROM cp_service_account WHERE token_hash = ?1",
            &[&"hash" as &dyn rusqlite::types::ToSql],
        ),
        (
            "SELECT payload FROM cp_membership WHERE organization_id = ?1 AND user_id = ?2",
            &[
                &"org-0" as &dyn rusqlite::types::ToSql,
                &"usr" as &dyn rusqlite::types::ToSql,
            ],
        ),
        (
            "SELECT payload FROM cp_user WHERE id = ?1",
            &[&"usr" as &dyn rusqlite::types::ToSql],
        ),
        (
            "SELECT payload FROM cp_organization WHERE id = ?1",
            &[&"org-0" as &dyn rusqlite::types::ToSql],
        ),
    ];
    for (sql, values) in cases {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let mut rows = stmt.query(*values).unwrap();
        let mut details = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            details.push(row.get::<_, String>(3).unwrap());
        }
        assert!(!details.is_empty(), "no plan for {sql}");
        for detail in &details {
            assert!(
                !detail.contains("SCAN "),
                "auth/tenant gate query regressed to a scan ({sql}): {details:?}"
            );
            assert!(
                detail.contains("SEARCH "),
                "auth/tenant gate query is not a point lookup ({sql}): {details:?}"
            );
        }
    }
}
