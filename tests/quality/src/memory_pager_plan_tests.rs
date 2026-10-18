//! Memory pager plan + scale regression (Report.pdf row 5, act-now).
//!
//! The V2 memory pager filtered with `kind LIKE 'memory_v2:%'`. LIKE is
//! case-insensitive, treats `_` as a wildcard, and cannot use the
//! `(kind, session_id)` index, so every turn paid a growing scan. The fixed
//! form is a half-open range: `kind >= 'memory_v2:' AND kind < 'memory_v2;'`.
//!
//! These tests are adversarial:
//!   * the source must contain the range predicate and no LIKE in either
//!     pager (ledger + workspace);
//!   * EXPLAIN QUERY PLAN must name an index, never `SCAN memory_fact`;
//!   * `MEMORY_V2:x` and `memoryXv2:y` must not match the range;
//!   * 200,000 churn rows still page in bounded time (p95 and ratio caps).

use std::path::PathBuf;
use std::time::Instant;

use faktor_core::id::SessionId;
use faktor_store::{MemoryFactCursor, Store};
use rusqlite::Connection;

fn source(rel: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|error| panic!("read {rel}: {error}"))
}

fn pager_body(source: &str, name: &str) -> String {
    let start = source
        .find(&format!("fn {name}"))
        .unwrap_or_else(|| panic!("{name} not found"));
    let open = source[start..].find('{').expect("function body") + start;
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[open..open + offset].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced {name}");
}

#[test]
fn memory_pager_uses_a_half_open_range_not_like() {
    fn strip_line_comments(text: &str) -> String {
        text.lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    let cases: Vec<(&str, Vec<&str>)> = vec![
        (
            "crates/store/src/ledger.rs",
            vec!["memory_facts_page_filtered"],
        ),
        (
            "crates/store/src/longtask.rs",
            vec![
                "workspace_memory_facts_page_filtered",
                "memory_fact_count_v2",
            ],
        ),
    ];
    for (file, functions) in cases {
        let text = source(file);
        for name in functions {
            let body = strip_line_comments(&pager_body(&text, name));
            assert!(
                !body.to_ascii_uppercase().contains("LIKE"),
                "{file}::{name} still filters with LIKE (case-insensitive, `_` wildcard, no index)"
            );
            if name.contains("page_filtered") {
                assert!(
                    body.contains("kind >= ") && body.contains("kind < "),
                    "{file}::{name} lacks the half-open kind range predicate"
                );
            }
        }
    }
}

#[test]
fn memory_pager_plan_names_an_index_at_200k_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("store"), true).expect("store");
    let workspace = store
        .create_workspace("/tmp/plan-workspace")
        .expect("workspace");
    let session = store
        .create_session(workspace, "plan", "provider", "model")
        .expect("session")
        .id;

    // 200,000 UNRELATED churn rows (the report's workload), a small managed
    // V2 set, and look-alike kinds that a LIKE wildcard would wrongly match.
    let mut facts: Vec<(String, String, String)> = Vec::with_capacity(200_254);
    for index in 0..200_000u64 {
        facts.push((
            format!("churn:{index}"),
            format!("key{index}"),
            format!("value{index}"),
        ));
    }
    for index in 0..250u64 {
        facts.push((
            format!("memory_v2:fact{index}"),
            format!("key{index}"),
            format!("value{index}"),
        ));
    }
    facts.push(("MEMORY_V2:upper".into(), "k".into(), "v".into()));
    facts.push(("memoryXv2:underscore".into(), "k".into(), "v".into()));
    facts.push(("memory_v2".into(), "bare".into(), "v".into()));
    let borrowed: Vec<(&str, &str, &str)> = facts
        .iter()
        .map(|(kind, key, value)| (kind.as_str(), key.as_str(), value.as_str()))
        .collect();
    store
        .upsert_memory_facts(session, &borrowed)
        .expect("seed facts");

    let plan_sql = "EXPLAIN QUERY PLAN
        SELECT kind, key, value, updated_ms FROM memory_fact
        WHERE session_id = ?1
          AND kind >= 'memory_v2:' AND kind < 'memory_v2;'
        ORDER BY updated_ms DESC, kind DESC, key DESC
        LIMIT 201";
    let connection = Connection::open(store.path()).expect("open read connection");
    let mut statement = connection.prepare(plan_sql).expect("prepare plan");
    let plan: Vec<String> = statement
        .query_map([session.raw() as i64], |row| row.get::<_, String>(3))
        .expect("explain")
        .map(|row| row.expect("plan row"))
        .collect();
    let joined = plan.join("\n");
    assert!(
        joined.contains("USING INDEX") || joined.contains("USING COVERING INDEX"),
        "range page has no index search: {joined}"
    );
    assert!(
        !joined.contains("SCAN memory_fact"),
        "range page still scans the growing table: {joined}"
    );

    let expected: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM memory_fact
             WHERE session_id = ?1 AND kind >= 'memory_v2:' AND kind < 'memory_v2;'",
            [session.raw() as i64],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(
        expected, 250,
        "range must exclude MEMORY_V2, memoryXv2 and the bare memory_v2 kind"
    );

    // Paging stays O(page) at the probe's size; the ratio compares the
    // 200k-row session against the same query plan on a tiny session.
    // Same managed-row count, zero churn: the only difference is the 200k
    // unrelated rows, so the ratio measures churn independence, not page size.
    let control = store
        .create_session(workspace, "control", "provider", "model")
        .expect("control session")
        .id;
    let control_facts: Vec<(String, String, String)> = (0..250u64)
        .map(|index| {
            (
                format!("memory_v2:fact{index}"),
                format!("key{index}"),
                format!("value{index}"),
            )
        })
        .collect();
    let control_borrowed: Vec<(&str, &str, &str)> = control_facts
        .iter()
        .map(|(kind, key, value)| (kind.as_str(), key.as_str(), value.as_str()))
        .collect();
    store
        .upsert_memory_facts(control, &control_borrowed)
        .expect("control facts");

    let measure = |session: SessionId| {
        let mut cursor: Option<MemoryFactCursor> = None;
        let mut samples = Vec::new();
        for _ in 0..20 {
            let started = Instant::now();
            let (rows, more) = store
                .memory_facts_v2_page(session, cursor.as_ref(), 200)
                .expect("page");
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
            if !more || rows.is_empty() {
                break;
            }
            let last = rows.last().expect("row");
            cursor = Some((last.updated_ms, last.kind.clone(), last.key.clone()));
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
        (p95, samples.len())
    };
    let (large_p95, pages) = measure(session);
    let (control_p95, _) = measure(control);
    assert!(pages > 1, "expected multiple pages");
    assert!(
        large_p95 <= 5.0,
        "200k-churn page p95 {large_p95:.1} ms exceeds the report's 1 ms target with headroom"
    );
    let ratio = large_p95 / control_p95.max(0.001);
    assert!(
        ratio <= 10.0,
        "200k churn changed page cost {ratio:.1}x (churn {large_p95:.2} ms vs control {control_p95:.2} ms); the page is not churn-independent"
    );
    assert_eq!(
        store.memory_fact_count_v2(session).expect("count v2"),
        250,
        "count must use the same range and cover every managed row"
    );
}
