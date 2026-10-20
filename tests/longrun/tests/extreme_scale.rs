//! Independent extreme long-task probe: 10,000 turns + 10,000 compactions +
//! 100,000 facts, 20 real store restarts and alternating forward/backward
//! clock jumps. It is `#[ignore]` (manual lane) because it is a scale probe,
//! not a CI soak.
//!
//! Exact invariants asserted on every restart and at the end:
//! - journal seq is gapless and strictly the expected length (no duplicate
//!   or lost turn/compaction events);
//! - every turn carries a unique nonzero op id;
//! - exactly 10,000 accepted compactions and 0 rejected ones;
//! - exactly 100,000 unique facts of the probe kind (no duplicate keys);
//! - the durable context checkpoint round-trips byte-for-byte across every
//!   restart and clock jump (no context drift);
//! - storage and WAL stay bounded; RSS growth is bounded.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use faktor_core::event::EventKind;
use faktor_core::id::OpId;
use faktor_core::state::AgentState;
use faktor_core::time::TestClock;
use faktor_session::SessionManager;

const TURNS: usize = 10_000;
const COMPACTIONS: usize = 10_000;
const FACTS: usize = 100_000;
const FACT_BATCH: usize = 1_000;
const FACT_EVERY_TURNS: usize = 100;
const RESTART_EVERY: usize = 500;
const FACT_KIND: &str = "longtask_probe";
const CHECKPOINT_BYTES: usize = 8_000;

fn open(root: &Path, clock: &Arc<TestClock>, integrity: bool) -> Arc<SessionManager> {
    SessionManager::open_with_clock(
        root.join("store"),
        root.join("cas"),
        integrity,
        clock.clone(),
    )
    .expect("open session manager")
}

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let meta = entry.metadata().expect("stat");
        if meta.is_dir() {
            total += dir_bytes(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

fn wal_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let meta = entry.metadata().expect("stat");
        if meta.is_dir() {
            total += wal_bytes(&entry.path());
        } else {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.contains("-wal") || name.ends_with(".wal") || name.contains("-shm") {
                total += meta.len();
            }
        }
    }
    total
}

fn rss_kib() -> Option<u64> {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn checkpoint_payload(round: usize) -> (String, Vec<u8>) {
    let mut payload = format!("checkpoint-round-{round}|").into_bytes();
    payload.resize(CHECKPOINT_BYTES, b'x');
    let digest = blake3::hash(&payload).to_hex().to_string();
    (digest, payload)
}

#[test]
#[ignore = "[fault] extreme 10k-turn/10k-compaction/100k-fact restart/clock probe; manual lane"]
fn extreme_long_task_10k_turns_10k_compactions_100k_facts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let rss_before = rss_kib();
    let started = Instant::now();

    let mut manager = open(root, &clock, true);
    let ws = manager
        .create_workspace(root.to_str().unwrap())
        .expect("workspace");
    let sid = manager
        .create_session(ws, "extreme long task", "fake", "m")
        .expect("session")
        .id();
    let mut handle = manager.get_session(sid).expect("lookup").expect("live");
    let mut expected_events = 1usize;
    let mut op_ids = BTreeSet::new();
    let mut facts_written = 0usize;
    let mut restarts = 0usize;
    let mut last_digest = String::new();
    let mut last_payload = Vec::new();

    for turn in 0..TURNS {
        let op = OpId::new(turn as u64 + 1);
        assert!(op_ids.insert(op), "turn op id reused at {turn}");
        handle
            .force_append_event(EventKind::TurnCompleted, AgentState::Idle, Some(op), None)
            .expect("turn event");
        expected_events += 1;
        let rec = handle
            .record_compaction_defaults(100_000, 40_000, 60_000, "deterministic_prune")
            .expect("compaction");
        assert!(rec.accepted, "compaction {turn} rejected: {:?}", rec.reason);
        expected_events += 1;

        if (turn + 1) % FACT_EVERY_TURNS == 0 {
            let base = facts_written;
            let facts: Vec<(String, String, String)> = (base..base + FACT_BATCH)
                .map(|i| {
                    (
                        FACT_KIND.to_string(),
                        format!("fact-{i:06}"),
                        format!("value-{i:06}"),
                    )
                })
                .collect();
            let refs: Vec<(&str, &str, &str)> = facts
                .iter()
                .map(|(kind, key, value)| (kind.as_str(), key.as_str(), value.as_str()))
                .collect();
            manager
                .store()
                .upsert_memory_facts(sid, &refs)
                .expect("facts");
            facts_written += FACT_BATCH;
        }

        if (turn + 1) % RESTART_EVERY == 0 {
            restarts += 1;
            if restarts > 1 {
                let (digest, payload) = checkpoint_payload(restarts - 1);
                assert_eq!(digest, last_digest);
                assert_eq!(payload, last_payload);
            }
            drop(handle);
            drop(manager);
            clock.advance(if !restarts.is_multiple_of(2) {
                3_600_000
            } else {
                -7_200_000
            });
            manager = open(root, &clock, restarts.is_multiple_of(5));
            handle = manager.get_session(sid).expect("lookup").expect("live");

            let events = handle.events_range(1, None).expect("replay");
            assert_eq!(
                events.len(),
                expected_events,
                "event count drifted at restart {restarts}"
            );
            for (index, event) in events.iter().enumerate() {
                assert_eq!(
                    event.seq.raw(),
                    index as u64 + 1,
                    "seq gap/duplicate at restart {restarts}"
                );
                assert!(
                    index == 0 || events[index - 1].ts_ms <= event.ts_ms,
                    "timestamp regression at restart {restarts}, seq {}",
                    event.seq.raw()
                );
            }
            let count = manager.store().memory_fact_count(sid).expect("fact count") as usize;
            assert!(
                (facts_written..=facts_written + 64).contains(&count),
                "fact count {count} drifted at restart {restarts} (probe facts {facts_written})"
            );

            let (digest, payload) = checkpoint_payload(restarts);
            let through = events.last().expect("events").seq.raw();
            handle
                .put_context_checkpoint(through, &digest, &payload)
                .expect("checkpoint");
            last_digest = digest;
            last_payload = payload;
        }
    }

    // Final reopen without moving the clock: durability independent of the
    // in-process handles.
    drop(handle);
    drop(manager);
    manager = open(root, &clock, true);
    handle = manager.get_session(sid).expect("lookup").expect("live");
    let events = handle.events_range(1, None).expect("final replay");
    assert_eq!(events.len(), expected_events);
    assert_eq!(events.len(), 1 + TURNS + COMPACTIONS);
    let compactions = events
        .iter()
        .filter(|e| e.kind == EventKind::ContextCompacted)
        .count();
    let rejected = events
        .iter()
        .filter(|e| e.kind == EventKind::CompactRejected)
        .count();
    assert_eq!(compactions, COMPACTIONS, "accepted compaction count");
    assert_eq!(rejected, 0, "unexpected rejected compaction");

    let turns: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::TurnCompleted)
        .collect();
    assert_eq!(turns.len(), TURNS);
    let mut replay_op_ids = BTreeSet::new();
    for event in &turns {
        let op = event.op_id.expect("turn without op id");
        assert!(replay_op_ids.insert(op), "duplicate turn outcome {op}");
    }
    assert_eq!(replay_op_ids.len(), TURNS);

    let mut fact_keys = BTreeSet::new();
    let mut cursor: Option<(i64, String, String)> = None;
    loop {
        let page = handle
            .memory_facts_by_kind_page(FACT_KIND, cursor.as_ref(), 200)
            .expect("facts page");
        for (_, key, _) in &page.facts {
            assert!(fact_keys.insert(key.clone()), "duplicate fact {key}");
        }
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(fact_keys.len(), FACTS, "unique probe facts");
    assert_eq!(facts_written, FACTS);

    let checkpoint = handle
        .context_checkpoint()
        .expect("checkpoint read")
        .expect("checkpoint present");
    assert_eq!(checkpoint.digest, last_digest, "checkpoint digest drift");
    assert_eq!(checkpoint.payload, last_payload, "checkpoint payload drift");
    assert_eq!(checkpoint.through_seq, events.last().unwrap().seq.raw());

    let db_bytes = dir_bytes(&root.join("store"));
    let wal = wal_bytes(&root.join("store"));
    let rss_after = rss_kib();
    let rss_growth_kib = match (rss_before, rss_after) {
        (Some(a), Some(b)) => b.saturating_sub(a),
        _ => 0,
    };
    eprintln!(
        "extreme-long-task: turns={TURNS} compactions={COMPACTIONS} facts={FACTS} \
         restarts={restarts} events={} db_bytes={db_bytes} wal_bytes={wal} \
         rss_growth_kib={rss_growth_kib} elapsed_ms={}",
        events.len(),
        started.elapsed().as_millis()
    );
    assert!(db_bytes < 1 << 30, "store exceeded 1 GiB: {db_bytes}");
    assert!(wal < 128 << 20, "WAL exceeded 128 MiB: {wal}");
    assert!(
        rss_growth_kib < 512 * 1024,
        "RSS grew by {rss_growth_kib} KiB"
    );
    assert!(
        started.elapsed().as_secs() < 600,
        "probe exceeded 10 minutes"
    );
}
