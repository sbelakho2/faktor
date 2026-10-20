//! Extreme long-task session probes (NOT a soak test; `#[ignore]` for the
//! scale cases). They measure the durable context-checkpoint read (one
//! indexed range read vs the historical chunk-by-chunk walk), 10k durable
//! compaction-operation rows under a backwards wall clock, and journal
//! timestamp authority across forward/backward clock jumps plus a restart.

use super::*;
use crate::compaction::CompactionOperationState;
use faktor_core::time::{Clock, TestClock};
use std::sync::Arc;
use std::time::Instant;

fn temp_manager(clock: Arc<dyn Clock>) -> (tempfile::TempDir, Arc<SessionManager>) {
    let dir = tempfile::tempdir().unwrap();
    let manager = SessionManager::open_with_clock(
        dir.path().join("store"),
        dir.path().join("cas"),
        false,
        clock,
    )
    .unwrap();
    (dir, manager)
}

#[test]
fn journal_timestamps_absorb_forward_and_backward_wall_clock_jumps_across_restart() {
    let clock = Arc::new(TestClock::new(1_000_000));
    let (dir, manager) = temp_manager(clock.clone());
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "p", "m").unwrap();
    let session = handle.id();
    let mut seqs = Vec::new();
    let mut stamps = Vec::new();
    // One legal turn cycle: Idle/Ready -> ... -> ReadyForNextTurn.
    let cycle = [
        (
            faktor_core::event::EventKind::PromptReceived,
            faktor_core::state::AgentState::Preparing,
        ),
        (
            faktor_core::event::EventKind::ContextPrepared,
            faktor_core::state::AgentState::BuildingContext,
        ),
        (
            faktor_core::event::EventKind::ModelStarted,
            faktor_core::state::AgentState::WaitingForModel,
        ),
        (
            faktor_core::event::EventKind::ModelChunkReceived,
            faktor_core::state::AgentState::Streaming,
        ),
        (
            faktor_core::event::EventKind::PhaseChanged,
            faktor_core::state::AgentState::Validating,
        ),
        (
            faktor_core::event::EventKind::PhaseChanged,
            faktor_core::state::AgentState::UpdatingMemory,
        ),
        (
            faktor_core::event::EventKind::TurnCompleted,
            faktor_core::state::AgentState::ReadyForNextTurn,
        ),
    ];
    let append = |stamp: &mut Vec<i64>, seqs: &mut Vec<u64>| {
        for (kind, state) in cycle {
            let seq = handle.append_event(kind, state, None, None).unwrap();
            seqs.push(seq.raw());
            let event = handle
                .events_range(seq.raw(), Some(1))
                .unwrap()
                .pop()
                .unwrap();
            stamp.push(event.ts_ms);
        }
    };
    for _ in 0..4 {
        append(&mut stamps, &mut seqs);
        clock.advance(5_000);
    }
    // Backward jump (NTP correction / VM restore): the journal must clamp,
    // never go backwards.
    clock.advance(-3_600_000);
    for _ in 0..4 {
        append(&mut stamps, &mut seqs);
        clock.advance(1_000);
    }
    clock.advance(86_400_000);
    for _ in 0..2 {
        append(&mut stamps, &mut seqs);
    }
    assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "{stamps:?}");
    // SessionCreated owns seq 1; the appended events are gapless above it.
    assert_eq!(seqs, (2..=seqs.len() as u64 + 1).collect::<Vec<_>>());
    drop(handle);
    drop(manager);
    let reopened = SessionManager::open_with_clock(
        dir.path().join("store"),
        dir.path().join("cas"),
        false,
        clock,
    )
    .unwrap();
    let handle = reopened.get_session(session).unwrap().unwrap();
    assert_eq!(
        handle.replay_journal().unwrap().state,
        handle.state().unwrap()
    );
    let events = handle.events_range(1, None).unwrap();
    assert_eq!(events.len(), seqs.len() + 1);
    assert!(events.windows(2).all(|w| w[0].ts_ms <= w[1].ts_ms));
}

#[test]
#[ignore = "scale probe (10k compaction operations)"]
fn scale_10k_compaction_operations_under_a_backward_clock() {
    let clock = Arc::new(TestClock::new(2_000_000));
    let (_dir, manager) = temp_manager(clock.clone());
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "p", "m").unwrap();

    let started = Instant::now();
    for i in 0..10_000i64 {
        // Every ~1k ops the wall clock steps backwards.
        if i % 1_000 == 0 {
            clock.advance(-10_000);
        }
        handle
            .begin_compaction_operation(&format!("op-{i:06}"), 20_000, 0.25, None, None)
            .unwrap();
        let advanced = handle
            .advance_compaction_operation(
                &format!("op-{i:06}"),
                CompactionOperationState::Applied,
                "applied",
                1_000_000,
                Some(100_000),
                Some(40_000),
                Some(60_000),
                Some("deterministic_pruning"),
                Some(&serde_json::json!({"accepted": true})),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert!(advanced.is_terminal());
    }
    let took = started.elapsed();
    eprintln!(
        "SCALE compaction_ops n=10000 total_ms={} per_op_us={}",
        took.as_millis(),
        took.as_micros() / 10_000
    );
    let latest = handle.latest_compaction_operation().unwrap().unwrap();
    assert_eq!(latest.operation_id, "op-009999");
    let prune_started = Instant::now();
    let deleted = handle
        .prune_compaction_operations_keep_newest(1_000)
        .unwrap();
    eprintln!(
        "SCALE compaction_prune deleted={deleted} total_ms={}",
        prune_started.elapsed().as_millis()
    );
    assert_eq!(deleted, 9_000);
    // The newest row survives the prune on every clock generation.
    let latest = handle.latest_compaction_operation().unwrap().unwrap();
    assert_eq!(latest.operation_id, "op-009999");
    assert!(handle.compaction_operation("op-000000").unwrap().is_none());
}

#[test]
#[ignore = "scale probe (large checkpoint read)"]
fn scale_large_context_checkpoint_reads() {
    let (_dir, manager) = temp_manager(Arc::new(TestClock::new(1_000_000)));
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "p", "m").unwrap();
    // ~120 chunks of narrative history (the maximum legal checkpoint is
    // 4 MiB; this size already exposes the per-chunk query walk).
    const CHUNKS: usize = 120;
    const CHUNK: usize = 3_000;
    let mut payload = Vec::with_capacity(CHUNKS * CHUNK);
    for bank in 0..CHUNKS {
        payload.extend(std::iter::repeat_n(b'a' + (bank % 26) as u8, CHUNK));
    }
    let digest = blake3::hash(&payload).to_hex().to_string();
    handle
        .put_context_checkpoint(77, &digest, &payload)
        .unwrap();

    let started = Instant::now();
    for _ in 0..1_000 {
        let read = handle.context_checkpoint().unwrap().unwrap();
        assert_eq!(read.payload.len(), payload.len());
    }
    let new_read = started.elapsed();
    // The historical shape: one indexed lookup for the head plus one per
    // declared chunk (the exact work the old reader did on EVERY turn).
    let old_started = Instant::now();
    for _ in 0..1_000 {
        let head = manager
            .store()
            .memory_fact_get(handle.id(), crate::memory::CONTEXT_CHECKPOINT_KIND, "head")
            .unwrap();
        assert!(head.is_some());
        // Match the real reader's work: reassemble the declared chunks and
        // verify the payload hash (the query walk PLUS the same payload
        // materialization the new single-range read performs).
        let mut payload = Vec::with_capacity(CHUNKS * CHUNK);
        for index in 0..CHUNKS {
            let key = format!("c/{index:06}");
            let chunk = manager
                .store()
                .memory_fact_get(handle.id(), crate::memory::CONTEXT_CHECKPOINT_KIND, &key)
                .unwrap()
                .unwrap();
            payload.extend_from_slice(chunk.as_bytes());
        }
        assert_eq!(
            blake3::hash(&payload).to_hex().to_string(),
            digest,
            "old-shape reassembly must produce the same payload"
        );
    }
    let old_read = old_started.elapsed();
    eprintln!(
        "SCALE checkpoint_read chunks={CHUNKS} n=1000 new_ms={} new_us_read={} old_ms={} old_us_read={} speedup={:.1}x",
        new_read.as_millis(),
        new_read.as_micros() / 1_000,
        old_read.as_millis(),
        old_read.as_micros() / 1_000,
        old_read.as_secs_f64() / new_read.as_secs_f64().max(f64::MIN_POSITIVE)
    );
}
