// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Retention lifecycle of the append-only governor history
//! (`loss_marker`/`eviction`/`refusal`).
//!
//! The adversarial cases the retention exists for: detail rows must not
//! grow the database forever (old rows leave), the compacted facts must
//! stay truthful (hourly aggregates carry the exact counts/bytes), archive
//! loss semantics must survive compaction (durable per-key tombstones keep
//! visibility and dedup), and deletion must return physical pages to the
//! filesystem (page_count/freelist assertions — a logical DELETE alone is
//! not retention).

use std::collections::BTreeMap;

use crate::class::StorageClass;
use crate::config::StorageGovernorConfig;
use crate::governor::StorageGovernor;
use crate::ledger::{Ledger, HISTORY_AGGREGATE_BUCKET_MS};

/// Small explicit keep for a fast proof; production retention uses
/// [`crate::ledger::HISTORY_DETAIL_KEEP`] (32,768, the documented 32k–64k
/// band) through `StorageGovernor::prune_history_default`/reconcile.
const KEEP: u64 = 64;

fn config() -> StorageGovernorConfig {
    let quotas = StorageClass::ALL
        .into_iter()
        .map(|class| (class, 10_000_000u64))
        .collect::<BTreeMap<_, _>>();
    StorageGovernorConfig {
        total_budget_bytes: 10_000_000,
        watermark_permille: 1000,
        reservation_lease_ms: 60_000,
        quotas,
        ..Default::default()
    }
    .validated()
    .unwrap()
}

fn detail_count(ledger: &Ledger, table: &str) -> u64 {
    let conn = ledger.conn();
    let count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap();
    count.max(0) as u64
}

#[test]
fn history_pruning_keeps_last_n_compacts_the_rest_and_reclaims_pages() {
    let dir = tempfile::tempdir().unwrap();
    let governor = StorageGovernor::open(dir.path(), config()).unwrap();

    // N + M rows per kind: 464 refusals, 90 evictions, 100 loss markers —
    // every kind exceeds the keep so every kind must compact. Seeded in ONE
    // transaction (a single fsync): the retention proof must not become a
    // timing hazard for the rest of the suite.
    let (refusal_n, eviction_n, loss_n) = (464u64, 90u64, 100u64);
    let refused_bytes = 7u64;
    let evicted_bytes = 11u64;
    let lost_bytes = 13u64;
    let counters_before = governor.counters().unwrap();
    {
        let mut ledger = Ledger::open(&StorageGovernor::database_path(dir.path())).unwrap();
        ledger
            .seed_history_for_test(
                refusal_n,
                refused_bytes,
                eviction_n,
                evicted_bytes,
                loss_n,
                lost_bytes,
            )
            .unwrap();
        assert_eq!(detail_count(&ledger, "refusal"), refusal_n);
        assert_eq!(detail_count(&ledger, "eviction"), eviction_n);
        assert_eq!(detail_count(&ledger, "loss_marker"), loss_n);
    }

    // One retention pass: keep the newest KEEP detail rows of each table,
    // compact the rest.
    let report = governor.prune_history(KEEP).unwrap();
    let expected_compacted = (refusal_n - KEEP) + (eviction_n - KEEP) + (loss_n - KEEP);
    assert_eq!(
        report.compacted, expected_compacted,
        "every row beyond the keep is compacted, once"
    );

    // (1) Old detail is GONE; exactly the newest KEEP rows remain.
    let ledger = Ledger::open(&StorageGovernor::database_path(dir.path())).unwrap();
    assert_eq!(detail_count(&ledger, "refusal"), KEEP, "old refusals leave");
    assert_eq!(
        detail_count(&ledger, "eviction"),
        KEEP,
        "old evictions leave"
    );
    assert_eq!(
        detail_count(&ledger, "loss_marker"),
        KEEP,
        "old loss markers leave"
    );

    // (2) Aggregates are correct: hourly buckets, exact counts and bytes.
    let refusals = governor.history_aggregates("refusal").unwrap();
    assert!(!refusals.is_empty(), "compacted refusals are aggregated");
    let (mut events, mut bytes) = (0u64, 0u64);
    for row in &refusals {
        assert_eq!(row.class, "main_cas");
        assert_eq!(row.reason, "class_quota");
        assert_eq!(
            row.bucket_ms % HISTORY_AGGREGATE_BUCKET_MS,
            0,
            "buckets are hourly-aligned"
        );
        events += row.events;
        bytes += row.bytes;
    }
    assert_eq!(
        events,
        refusal_n - KEEP,
        "aggregate count equals compacted rows"
    );
    assert_eq!(
        bytes,
        (refusal_n - KEEP) * refused_bytes,
        "aggregate bytes equal the summed refused bytes"
    );
    let evictions = governor.history_aggregates("eviction").unwrap();
    let eviction_events: u64 = evictions.iter().map(|row| row.events).sum();
    let eviction_bytes: u64 = evictions.iter().map(|row| row.bytes).sum();
    assert_eq!(eviction_events, eviction_n - KEEP);
    assert_eq!(eviction_bytes, (eviction_n - KEEP) * evicted_bytes);
    let losses = governor.history_aggregates("loss_marker").unwrap();
    let loss_events: u64 = losses.iter().map(|row| row.events).sum();
    let loss_bytes: u64 = losses.iter().map(|row| row.bytes).sum();
    assert_eq!(loss_events, loss_n - KEEP);
    assert_eq!(loss_bytes, (loss_n - KEEP) * lost_bytes);

    // (3) Bytes were ACTUALLY reclaimed: the database shrank physically
    // (page_count) and freed pages were returned (freelist), which only
    // holds when incremental auto-vacuum really ran.
    assert!(
        report.pages_after < report.pages_before,
        "page_count must shrink: {} !< {}",
        report.pages_after,
        report.pages_before
    );
    assert!(
        report.freelist_after < report.freelist_before,
        "freelist must shrink: {} !< {}",
        report.freelist_after,
        report.freelist_before
    );
    assert!(
        report.pages_reclaimed > 0,
        "pages returned to the filesystem"
    );

    // (4) Lifetime counters are never reset by retention (they are the
    // durable totals; only detail rows rotate).
    let counters_after = governor.counters().unwrap();
    assert!(
        counters_after.refusals >= counters_before.refusals + refusal_n,
        "the refusal counter counts every event, never pruned"
    );
    assert!(counters_after.evictions >= counters_before.evictions + eviction_n);
    assert!(counters_after.loss_markers >= counters_before.loss_markers + loss_n);

    // (5) Idempotence: a second pass compacts nothing and never double
    // counts the aggregates.
    let second = governor.prune_history(KEEP).unwrap();
    assert_eq!(second.compacted, 0, "nothing new to compact");
    let refusals_again = governor.history_aggregates("refusal").unwrap();
    let events_again: u64 = refusals_again.iter().map(|row| row.events).sum();
    assert_eq!(events_again, events, "aggregates never double-count");
}

#[test]
fn archive_loss_semantics_survive_retention_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let governor = StorageGovernor::open(dir.path(), config()).unwrap();
    let loss_n = 100u64;
    let counters_started = governor.counters().unwrap();
    {
        let mut ledger = Ledger::open(&StorageGovernor::database_path(dir.path())).unwrap();
        ledger
            .seed_history_for_test(0, 0, 0, 0, loss_n, 4_096)
            .unwrap();
    }
    let markers_before = governor
        .loss_markers(Some(StorageClass::CompactionArchive))
        .unwrap();
    assert_eq!(markers_before.len(), loss_n as usize);
    let counters_before = governor.counters().unwrap();
    assert_eq!(
        counters_before.loss_markers,
        counters_started.loss_markers + loss_n,
        "every recorded loss counts exactly once"
    );

    governor.prune_history(KEEP).unwrap();
    let counters_pruned = governor.counters().unwrap();
    assert_eq!(
        counters_pruned.loss_markers, counters_before.loss_markers,
        "retention never touches the lifetime loss counter"
    );

    // (a) Every compacted loss stays VISIBLE through the tombstones: the
    // loss view still reports all keys with their bytes and reason.
    let markers_after = governor
        .loss_markers(Some(StorageClass::CompactionArchive))
        .unwrap();
    assert_eq!(
        markers_after.len(),
        loss_n as usize,
        "compacted archive losses remain queryable through durable tombstones"
    );
    for (key, bytes, reason) in &markers_after {
        assert_eq!(*bytes, 4_096, "tombstones carry the lost byte count");
        assert_eq!(reason, "archive_evicted");
        assert!(key.starts_with("lost-"), "key identity survives compaction");
    }

    // (b) Re-recording a compacted loss is deduplicated: no duplicate
    // marker, no counter inflation (the tombstone IS the durable fact).
    {
        let mut ledger = Ledger::open(&StorageGovernor::database_path(dir.path())).unwrap();
        ledger
            .record_loss_marker(
                StorageClass::CompactionArchive,
                "lost-0",
                Some("digest-0"),
                4_096,
                "archive_evicted",
            )
            .unwrap();
    }
    let counters_after = governor.counters().unwrap();
    assert_eq!(
        counters_after.loss_markers, counters_pruned.loss_markers,
        "a tombstoned loss can never be counted twice"
    );
    let markers_final = governor
        .loss_markers(Some(StorageClass::CompactionArchive))
        .unwrap();
    assert_eq!(
        markers_final.len(),
        loss_n as usize,
        "the dedup never duplicates the view"
    );
    assert_eq!(
        governor.loss_tombstone_count().unwrap(),
        loss_n - KEEP,
        "one durable tombstone per compacted loss"
    );
}
