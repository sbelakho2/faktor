//! `session_listing_tests`: the single-statement contract of the durable
//! session listing. The native `/native/sessions` page serves its DTO
//! directly from ONE `list_sessions` call (perf-wave follow-up: the
//! historical handler ran the manager listing — a whole-table read plus a
//! journal verification per session — AND a second batch listing per page).
//! These probes pin the store half: one statement per call, newest-updated
//! first, every row exactly once, at 5k sessions.

use super::*;

#[test]
fn list_sessions_is_one_statement_newest_first_at_5k_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    const N: u64 = 5_000;
    let mut created: Vec<SessionId> = Vec::with_capacity(N as usize);
    for i in 0..N {
        let row = store
            .create_session(ws, &format!("listing {i}"), "fake", "m")
            .unwrap();
        created.push(row.id);
    }

    let reads_before = store.reader_telemetry().reads;
    let rows = store.list_sessions(None).unwrap();
    let reads_after = store.reader_telemetry().reads;
    assert_eq!(
        reads_after - reads_before,
        1,
        "the listing is ONE statement (one read borrow), never two full reads"
    );
    assert_eq!(rows.len(), N as usize);

    // Newest-updated first: the sequence is non-increasing.
    assert!(
        rows.windows(2).all(|w| w[0].updated_ms >= w[1].updated_ms),
        "the listing must be newest-updated first"
    );

    // Every created session appears exactly once (no dropped or duplicated
    // rows, including the 5001st boundary of nothing — the whole table).
    let mut listed: Vec<u64> = rows.iter().map(|r| r.id.raw()).collect();
    let total = listed.len();
    listed.sort_unstable();
    listed.dedup();
    assert_eq!(listed.len(), total, "no duplicates");
    created.sort_by_key(|id| id.raw());
    let created_raw: Vec<u64> = created.iter().map(|id| id.raw()).collect();
    assert_eq!(listed, created_raw, "every created session is listed");

    // A bounded workspace-filtered read is the same single statement.
    let reads_before = store.reader_telemetry().reads;
    let scoped = store.list_sessions(Some(ws)).unwrap();
    let reads_after = store.reader_telemetry().reads;
    assert_eq!(reads_after - reads_before, 1);
    assert_eq!(scoped.len(), N as usize);
}

/// Report row 8: a History walk is a SNAPSHOT. A conversation updated
/// mid-walk must neither re-surface at page one nor be skipped by a later
/// page; under the snapshot rule it is absent from the remainder of that
/// walk and the next walk (fresh snapshot) lists it.
#[test]
fn history_walk_snapshot_never_resurfaces_or_skips_mid_walk_updates() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let mut ids = Vec::new();
    for i in 0..6 {
        ids.push(
            store
                .create_session(ws, &format!("walk {i}"), "fake", "m")
                .unwrap()
                .id,
        );
    }
    // Deterministic sort key: 100..=105 (distinct, equal to none of the
    // wall-clock values a mid-walk rename will use).
    {
        let conn = store.raw_conn();
        for (i, id) in ids.iter().enumerate() {
            conn.execute(
                "UPDATE session SET updated_ms = ?2 WHERE id = ?1",
                params![id.raw() as i64, 100 + i as i64],
            )
            .unwrap();
        }
    }
    let snapshot = 105i64;
    let mut seen: Vec<u64> = Vec::new();
    let mut before: Option<(i64, i64)> = None;
    let mut pages = 0;
    loop {
        let page = store
            .list_sessions_page_at(None, before, 2, Some(snapshot))
            .unwrap();
        pages += 1;
        assert!(pages <= 10, "the walk must terminate");
        let has_more = page.len() > 2;
        let page = &page[..page.len().min(2)];
        seen.extend(page.iter().map(|row| row.id.raw()));
        if pages == 1 {
            // Mid-walk update: the row that would be served LAST jumps to
            // the newest position under a live cursor.
            let last = ids[0];
            store
                .update_session_title(last, "updated mid-walk")
                .unwrap();
            let bumped: i64 = store
                .raw_conn()
                .query_row(
                    "SELECT updated_ms FROM session WHERE id = ?1",
                    params![last.raw() as i64],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                bumped > snapshot,
                "the update must move the row above the snapshot"
            );
        }
        if !has_more {
            break;
        }
        let last = page.last().unwrap();
        before = Some((last.updated_ms, last.id.raw() as i64));
    }
    // Sessions 6,5,4,3,2 are served exactly once; the mid-walk-updated
    // session 1 is absent from THIS snapshot walk (it moved above it).
    assert_eq!(seen, vec![6, 5, 4, 3, 2], "snapshot walk: {seen:?}");
    let mut fresh = Vec::new();
    let mut before = None;
    loop {
        let page = store.list_sessions_page(None, before, 2).unwrap();
        let has_more = page.len() > 2;
        let page = &page[..page.len().min(2)];
        fresh.extend(page.iter().map(|row| row.id.raw()));
        if !has_more {
            break;
        }
        let last = page.last().unwrap();
        before = Some((last.updated_ms, last.id.raw() as i64));
    }
    assert_eq!(
        fresh,
        vec![1, 6, 5, 4, 3, 2],
        "the next walk (fresh snapshot) lists the updated conversation first"
    );
}
