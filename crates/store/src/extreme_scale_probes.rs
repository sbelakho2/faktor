//! Extreme long-task scale probes (NOT a soak test; `#[ignore]` by design).
//!
//! These drive the durable store at the sizes the long-task requirement
//! names — 1k / 10k / 100k events and facts, 10k checkpoint rows and a
//! restart storm — and print per-operation / per-page / storage numbers so
//! before/after comparisons are measured, not asserted from theory. The
//! only assertions are exact invariants (gapless seq, no duplicate/missing
//! fact in a page walk, bounded retention), never wall-clock thresholds.

use super::*;
use std::time::Instant;

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store"), false).unwrap();
    (dir, store)
}

fn db_sizes(root: &std::path::Path) -> (u64, u64) {
    let main = std::fs::metadata(root.join("faktor-plus.db"))
        .map(|m| m.len())
        .unwrap_or(0);
    let wal = std::fs::metadata(root.join("faktor-plus.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    (main, wal)
}

fn append_events(
    store: &Store,
    session: SessionId,
    count: u64,
    base_ms: i64,
) -> std::time::Duration {
    let started = Instant::now();
    for i in 0..count {
        store
            .append_event(
                session,
                None,
                EventKind::ModelChunkReceived,
                AgentState::ReadyForNextTurn,
                base_ms + i as i64,
                Some(serde_json::json!({"n": i})),
            )
            .unwrap();
    }
    started.elapsed()
}

#[test]
#[ignore = "[perf] scale probe (100k durable rows)"]
fn scale_100k_events() {
    let (dir, store) = temp_store();
    let root = dir.path().join("store");
    let ws = store.create_workspace("/w").unwrap();
    let session = store.create_session(ws, "t", "p", "m").unwrap();

    for count in [1_000u64, 10_000, 100_000] {
        let took = append_events(&store, session.id, count, 1_000_000);
        eprintln!(
            "SCALE events n={count} total_ms={} per_event_us={}",
            took.as_millis(),
            took.as_micros() / count as u128
        );
    }
    // Gapless walk, bounded pages only.
    let walk_started = Instant::now();
    let mut cursor = 0u64;
    let mut seen = 0u64;
    loop {
        let page = store
            .events_range(session.id, cursor + 1, Some(1_000))
            .unwrap();
        if page.is_empty() {
            break;
        }
        for (offset, event) in page.iter().enumerate() {
            assert_eq!(event.seq.raw(), cursor + 1 + offset as u64);
        }
        cursor = page.last().unwrap().seq.raw();
        seen += page.len() as u64;
    }
    eprintln!(
        "SCALE event_walk seen={seen} total_ms={} per_page_us={}",
        walk_started.elapsed().as_millis(),
        walk_started.elapsed().as_micros() / (seen / 1_000).max(1) as u128
    );
    assert_eq!(seen, 111_001);

    store.wal_checkpoint_passive().unwrap();
    let _ = store.wal_checkpoint_truncate_bounded().unwrap();
    let (main, wal) = db_sizes(&root);
    eprintln!(
        "SCALE events_storage db_bytes={main} wal_bytes={wal} total_bytes={}",
        main + wal
    );
}

#[test]
#[ignore = "[perf] scale probe (100k durable facts)"]
fn scale_100k_facts() {
    let (dir, store) = temp_store();
    let root = dir.path().join("store");
    let ws = store.create_workspace("/w").unwrap();
    let session = store.create_session(ws, "t", "p", "m").unwrap();

    // 50k session facts of one churn kind + 50k workspace facts.
    let facts_started = Instant::now();
    for i in 0..50_000u64 {
        store
            .upsert_memory_fact(session.id, "turn", &format!("k{i:06}"), &format!("v{i}"))
            .unwrap();
    }
    let session_facts = facts_started.elapsed();
    let ws_started = Instant::now();
    for i in 0..50_000u64 {
        store
            .upsert_workspace_memory_fact(
                ws.raw(),
                "memory_v2:probe",
                &format!("k{i:06}"),
                &format!("v{i}"),
            )
            .unwrap();
    }
    let ws_facts = ws_started.elapsed();
    eprintln!(
        "SCALE facts_written session=50000 session_us_op={} workspace=50000 workspace_us_op={}",
        session_facts.as_micros() / 50_000,
        ws_facts.as_micros() / 50_000
    );

    // Generic session pager: exactly one page + one probe row per query.
    let paging_started = Instant::now();
    let mut cursor: Option<MemoryFactCursor> = None;
    let mut paged = 0usize;
    let mut pages = 0usize;
    loop {
        let (rows, has_more) = store
            .memory_facts_page(session.id, cursor.as_ref(), 200)
            .unwrap();
        for row in &rows {
            assert_eq!(row.kind, "turn");
        }
        paged += rows.len();
        pages += 1;
        if !has_more {
            break;
        }
        cursor = rows
            .last()
            .map(|r| (r.updated_ms, r.kind.clone(), r.key.clone()));
    }
    let paging = paging_started.elapsed();
    eprintln!(
        "SCALE fact_page_walk rows={paged} pages={pages} total_ms={} per_page_us={}",
        paging.as_millis(),
        paging.as_micros() / pages as u128
    );
    assert_eq!(paged, 50_000);

    // Kind-scoped page walk (managed-memory listing shape).
    let kind_started = Instant::now();
    let mut cursor: Option<MemoryFactCursor> = None;
    let mut paged = 0usize;
    let mut pages = 0usize;
    loop {
        let (rows, has_more) = store
            .memory_facts_by_kind_page(session.id, "turn", cursor.as_ref(), 200)
            .unwrap();
        paged += rows.len();
        pages += 1;
        if !has_more {
            break;
        }
        cursor = rows
            .last()
            .map(|r| (r.updated_ms, r.kind.clone(), r.key.clone()));
    }
    let kind_walk = kind_started.elapsed();
    eprintln!(
        "SCALE fact_kind_walk rows={paged} pages={pages} total_ms={} per_page_us={}",
        kind_walk.as_millis(),
        kind_walk.as_micros() / pages as u128
    );
    assert_eq!(paged, 50_000);

    // Workspace pager.
    let ws_page_started = Instant::now();
    let mut cursor: Option<MemoryFactCursor> = None;
    let mut paged = 0usize;
    loop {
        let (rows, has_more) = store
            .workspace_memory_facts_page(ws.raw(), cursor.as_ref(), 200)
            .unwrap();
        paged += rows.len();
        if !has_more {
            break;
        }
        cursor = rows
            .last()
            .map(|r| (r.updated_ms, r.kind.clone(), r.key.clone()));
    }
    eprintln!(
        "SCALE workspace_fact_walk rows={paged} total_ms={}",
        ws_page_started.elapsed().as_millis()
    );
    assert_eq!(paged, 50_000);

    // Retention: keep 1k of 50k, delete exactly 49k, pager still exact.
    let prune_started = Instant::now();
    let deleted = store
        .prune_memory_facts_kind_keep_newest(session.id, "turn", 1_000)
        .unwrap();
    eprintln!(
        "SCALE retention deleted={deleted} total_ms={}",
        prune_started.elapsed().as_millis()
    );
    assert_eq!(deleted, 49_000);
    let page = store.memory_facts_page(session.id, None, 500).unwrap().0;
    assert_eq!(page.len(), 500);
    assert!(page.iter().any(|r| r.key == "k049999"));

    // Explicit deletes are indexed and never touch another scope.
    let delete_started = Instant::now();
    for i in 0..10_000u64 {
        store
            .delete_memory_fact(session.id, "turn", &format!("k{i:06}"))
            .unwrap();
    }
    eprintln!(
        "SCALE deletes n=10000 total_ms={} per_op_us={}",
        delete_started.elapsed().as_millis(),
        delete_started.elapsed().as_micros() / 10_000
    );
    let workspace_after = store
        .workspace_memory_facts_page(ws.raw(), None, 5)
        .unwrap()
        .0;
    assert_eq!(
        workspace_after.len(),
        5,
        "session deletes never cross scope"
    );

    // The daemon's idle maintenance tick (PASSIVE then bounded TRUNCATE)
    // must fold the burst and bound the on-disk WAL.
    store.wal_checkpoint_passive().unwrap();
    let truncate_started = Instant::now();
    let truncated = store.wal_checkpoint_truncate_bounded().unwrap();
    eprintln!(
        "SCALE wal_truncate attempted={truncated} total_ms={}",
        truncate_started.elapsed().as_millis()
    );
    let (main, wal) = db_sizes(&root);
    eprintln!(
        "SCALE storage db_bytes={main} wal_bytes={wal} total_bytes={}",
        main + wal
    );
}

#[test]
#[ignore = "[perf] scale probe (10k checkpoint rows + restart storm)"]
fn scale_10k_checkpoints_and_restart_storm() {
    let (dir, mut store) = temp_store();
    let root = dir.path().join("store");
    let ws = store.create_workspace("/w").unwrap();
    let session = store.create_session(ws, "t", "p", "m").unwrap();

    // Before: the exact work the old per-write duplicate pre-scan did —
    // list EVERY checkpoint on every write.
    for count in [1_000u64, 10_000] {
        let started = Instant::now();
        for i in 0..count {
            store
                .put_checkpoint(
                    session.id,
                    i as i64,
                    &format!("src/{i:06}.rs"),
                    "before",
                    "after",
                    None,
                )
                .unwrap();
        }
        let write = started.elapsed();
        let scan_started = Instant::now();
        for _ in 0..count {
            let _ = store.checkpoints_of(session.id).unwrap();
        }
        let scan = scan_started.elapsed();
        eprintln!(
            "SCALE checkpoints n={count} write_ms={} write_us_op={} old_prescan_ms={} old_prescan_us_op={}",
            write.as_millis(),
            write.as_micros() / count as u128,
            scan.as_millis(),
            scan.as_micros() / count as u128
        );
    }
    // Point reads: restore/diff by id and latest (never a listing scan).
    let point_started = Instant::now();
    let latest = store.latest_checkpoint(session.id).unwrap().unwrap();
    for _ in 0..10_000 {
        assert!(store
            .checkpoint_by_id(session.id, latest.id)
            .unwrap()
            .is_some());
    }
    eprintln!(
        "SCALE checkpoint_point_reads n=10000 total_ms={} per_op_us={}",
        point_started.elapsed().as_millis(),
        point_started.elapsed().as_micros() / 10_000
    );
    let page = store.checkpoints_newest(session.id, 100).unwrap();
    assert_eq!(page.len(), 100);
    assert_eq!(page[0].sequence, 9_999);

    // Restart storm: 20 cold opens over the same 10k-checkpoint store.
    let storm_started = Instant::now();
    let mut last_open = std::time::Duration::ZERO;
    for i in 0..20 {
        drop(store);
        let t = Instant::now();
        store = Store::open(&root, false).unwrap();
        last_open = t.elapsed();
        let page = store.checkpoints_newest(session.id, 5).unwrap();
        assert_eq!(page.len(), 5);
        assert_eq!(page[0].sequence, 9_999);
        store
            .append_event(
                session.id,
                None,
                EventKind::ModelChunkReceived,
                AgentState::ReadyForNextTurn,
                2_000_000 + i,
                None,
            )
            .unwrap();
    }
    eprintln!(
        "SCALE restart_storm opens=20 total_ms={} per_open_us={} last_open_us={}",
        storm_started.elapsed().as_millis(),
        storm_started.elapsed().as_micros() / 20,
        last_open.as_micros()
    );
    // The reopened journal is still gapless: the 10k checkpoint commands
    // each appended one event and the storm appended one per open.
    let last = store.last_event_seq(session.id).unwrap().unwrap();
    assert_eq!(last.raw(), 1 + 11_000 + 20);
    let (main, wal) = db_sizes(&root);
    eprintln!(
        "SCALE storage_after_storm db_bytes={main} wal_bytes={wal} total_bytes={}",
        main + wal
    );
}
