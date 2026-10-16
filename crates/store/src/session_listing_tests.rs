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
