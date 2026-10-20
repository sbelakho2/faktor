//! Extreme long-task probe regressions for durable ordering and indexed
//! point reads (NOT a soak test): every case fails deterministically before
//! the corresponding fix.
//!
//! - workspace-scoped fact stamps must reseed the monotone sequence at open,
//!   or a backward wall clock re-enters an old row's order position;
//! - checkpoint restore/latest reads must resolve by index, not by loading
//!   the whole listing and scanning it.

use super::*;

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store"), false).unwrap();
    (dir, store)
}

#[test]
fn reopen_seeds_the_fact_clock_above_workspace_rows() {
    let (dir, store) = temp_store();
    let ws = store.create_workspace("/w").unwrap();
    let session = store.create_session(ws, "t", "p", "m").unwrap();
    let future = crate::connection::now_ms().saturating_add(1_000_000_000);
    drop(store);
    // A workspace row written far in the future (or by a machine whose
    // clock was later corrected backwards) is the durable high-water mark.
    let conn = rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db")).unwrap();
    conn.execute(
        "INSERT INTO workspace_memory_fact(workspace_id, kind, key, value, updated_ms)
         VALUES (?1, 'memory_v2:test', 'future', 'v', ?2)",
        rusqlite::params![ws.raw() as i64, future],
    )
    .unwrap();
    drop(conn);

    let store = Store::open(dir.path().join("store"), false).unwrap();
    store
        .upsert_workspace_memory_fact(ws.raw(), "memory_v2:test", "after-restart", "v")
        .unwrap();
    let (rows, _) = store
        .workspace_memory_facts_page(ws.raw(), None, 10)
        .unwrap();
    let row = rows
        .iter()
        .find(|r| r.key == "after-restart")
        .expect("the new row must be readable");
    assert!(
        row.updated_ms > future,
        "a restart must stamp the new workspace fact strictly above the \
         durable high-water mark (got {} <= {future})",
        row.updated_ms
    );
    // The session-scoped pager shares the same monotone authority.
    store
        .upsert_memory_fact(session.id, "memory_v2:test", "after-restart", "v")
        .unwrap();
    let page = store
        .memory_facts_by_kind_page(session.id, "memory_v2:test", None, 10)
        .unwrap()
        .0;
    assert!(
        page.last().unwrap().updated_ms > future,
        "session facts must also sort above the durable high-water mark"
    );
}

#[test]
fn checkpoint_point_reads_are_indexed_and_scoped() {
    let (_dir, store) = temp_store();
    let ws = store.create_workspace("/w").unwrap();
    let a = store.create_session(ws, "a", "p", "m").unwrap();
    let b = store.create_session(ws, "b", "p", "m").unwrap();
    for sequence in 0..64i64 {
        store
            .put_checkpoint(
                a.id,
                sequence,
                &format!("file-{sequence}.rs"),
                "before",
                "after",
                None,
            )
            .unwrap();
    }
    store
        .put_checkpoint(b.id, 0, "other.rs", "before", "after", None)
        .unwrap();
    let latest = store.latest_checkpoint(a.id).unwrap().unwrap();
    assert_eq!(latest.sequence, 63);
    let by_id = store.checkpoint_by_id(a.id, latest.id).unwrap().unwrap();
    assert_eq!(by_id, latest);
    // A row of another session is invisible through the scoped read even
    // when the raw id is known.
    let foreign = store.latest_checkpoint(b.id).unwrap().unwrap();
    assert!(store.checkpoint_by_id(a.id, foreign.id).unwrap().is_none());
    // The bounded newest page keeps the exact newest-first order.
    let page = store.checkpoints_newest(a.id, 5).unwrap();
    let seqs: Vec<i64> = page.iter().map(|r| r.sequence).collect();
    assert_eq!(seqs, vec![63, 62, 61, 60, 59]);
}
