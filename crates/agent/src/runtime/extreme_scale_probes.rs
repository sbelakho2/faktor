//! Extreme long-task runtime probes (NOT a soak test; `#[ignore]`).
//!
//! `scale_10k_forced_compactions` drives the REAL deterministic compactor
//! through 10,000 accepted compaction cycles over a growing transcript and
//! commits each accepted plan exactly as the runtime does (durable
//! compaction record + replacement context checkpoint), asserting the hard
//! invariants every cycle: exact wire cost, minimum reduction, identity
//! verbatim, one carried summary/digest at most (no summary-of-summary
//! drift), and a restart-identical digest.
//!
//! `scale_10k_turn_session` drives a 10,000-turn durable conversation with
//! message/event rows, forced compaction checkpoints and the real bounded
//! window loader, measuring per-turn assembly and compaction latency.

use faktor_context::{
    estimate_history_tokens, estimate_wire_turns, history_digest, CompactionRequest, Compactor,
    DurableTaskRows, RecentTurn, TaskContextProjection,
};
use faktor_session::SessionManager;
use std::time::Instant;

fn synthetic_turn(i: usize) -> RecentTurn {
    let roles = ["user", "assistant", "tool", "assistant", "tool_call"];
    let body = match i % 7 {
        0 => "y".repeat(5_000),
        3 | 6 => "z".repeat(2_000),
        _ => "x".repeat(400),
    };
    RecentTurn {
        role: roles[i % roles.len()].into(),
        text: format!("FACT-{i:06} cycle {i} {body}"),
    }
}

fn projection() -> TaskContextProjection {
    TaskContextProjection::from_durable_rows(DurableTaskRows {
        goal: "GOAL-verbatim-long-task".into(),
        criteria: vec![
            "CRITERION-1-no-loss".into(),
            "CRITERION-2-tests-pass".into(),
        ],
        task_state: "Implementing".into(),
        ..Default::default()
    })
}

fn wire_text(turns: &[RecentTurn]) -> String {
    turns
        .iter()
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
#[ignore = "scale probe (10k forced compaction cycles)"]
async fn scale_10k_forced_compactions() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), false).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "p", "m").unwrap();
    let session = handle.id();
    let compactor = Compactor::deterministic_only();
    let projection = projection();
    let target = 20_000usize;
    let mut history: Vec<RecentTurn> = (0..1_200).map(synthetic_turn).collect();
    let mut next_turn = history.len();
    let mut total_compact_us = 0u128;
    let mut total_write_us = 0u128;
    let mut max_checkpoint_bytes = 0usize;

    let wall = Instant::now();
    for cycle in 0..10_000 {
        let before = estimate_history_tokens(&history);
        assert!(
            before > target,
            "cycle {cycle}: the forced history must be over the cap"
        );
        let req = CompactionRequest {
            before_tokens: before,
            target_tokens: target,
            ..Default::default()
        };
        let plan_started = Instant::now();
        let plan = compactor
            .compact_projected(
                &history,
                &projection,
                &faktor_context::TaskLedger::default(),
                &req,
            )
            .await;
        total_compact_us += plan_started.elapsed().as_micros();
        assert!(
            plan.accepted,
            "cycle {cycle}: compaction must be accepted (after={} cap={})",
            plan.after_tokens,
            req.hard_cap()
        );
        assert_eq!(
            plan.after_tokens,
            estimate_wire_turns(&plan.kept_recent),
            "cycle {cycle}: after_tokens must equal the exact wire cost"
        );
        assert!(
            plan.after_tokens <= req.hard_cap(),
            "cycle {cycle}: accepted plan over the hard cap"
        );
        assert!(
            (plan.after_tokens as f64) <= before as f64 * 0.75,
            "cycle {cycle}: minimum reduction lost"
        );
        let kept = wire_text(&plan.kept_recent);
        assert!(kept.contains("GOAL-verbatim-long-task"));
        assert!(kept.contains("CRITERION-1-no-loss"));
        assert!(kept.contains("CRITERION-2-tests-pass"));
        assert_eq!(
            plan.kept_recent
                .iter()
                .filter(|t| t.role == "summary")
                .count(),
            history
                .iter()
                .filter(|t| t.role == "summary")
                .count()
                .min(1),
            "cycle {cycle}: summaries must never multiply"
        );
        assert!(
            plan.kept_recent
                .iter()
                .filter(|t| t.text.starts_with("[Earlier context archived:"))
                .count()
                <= 1,
            "cycle {cycle}: archive pointers must never multiply"
        );

        let write_started = Instant::now();
        let payload = serde_json::to_vec(&plan.kept_recent).unwrap();
        max_checkpoint_bytes = max_checkpoint_bytes.max(payload.len());
        let through_seq = handle.last_event_seq().unwrap().unwrap().raw();
        let digest = history_digest(&plan.kept_recent);
        let record = handle
            .commit_compaction_with_checkpoint(
                plan.before_tokens as i64,
                plan.after_tokens as i64,
                plan.target_tokens as i64,
                plan.accepted,
                "deterministic",
                &faktor_session::compaction::CompactionPolicy::default(),
                through_seq,
                &digest,
                &payload,
            )
            .unwrap();
        assert!(record.accepted);
        total_write_us += write_started.elapsed().as_micros();

        history = plan
            .kept_recent
            .iter()
            .filter(|t| t.role != "system")
            .cloned()
            .collect();
        for _ in 0..80 {
            history.push(synthetic_turn(next_turn));
            next_turn += 1;
        }
    }
    let total = wall.elapsed();
    eprintln!(
        "SCALE compactions n=10000 total_ms={} compactor_ms={} durable_write_ms={} per_compact_us={}",
        total.as_millis(),
        total_compact_us / 1_000,
        total_write_us / 1_000,
        total.as_micros() / 10_000
    );
    eprintln!(
        "SCALE compaction_checkpoint max_payload_bytes={max_checkpoint_bytes} history_turns={}",
        history.len()
    );
    // Restart parity: the last replacement context is exactly reproducible.
    let checkpoint = handle.context_checkpoint().unwrap().unwrap();
    assert_eq!(
        history_digest(&serde_json::from_slice::<Vec<RecentTurn>>(&checkpoint.payload).unwrap()),
        checkpoint.digest
    );
    drop(handle);
    drop(manager);
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), false).unwrap();
    let handle = reopened
        .get_session(session)
        .unwrap()
        .expect("session survives");
    let checkpoint = handle.context_checkpoint().unwrap().unwrap();
    assert_eq!(
        history_digest(&serde_json::from_slice::<Vec<RecentTurn>>(&checkpoint.payload).unwrap()),
        checkpoint.digest,
        "restart must reproduce the exact replacement-context digest"
    );
}

#[tokio::test]
#[ignore = "scale probe (10k-turn durable session)"]
async fn scale_10k_turn_session() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), false).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "p", "m").unwrap();
    let mut checkpoint_seq = 0u64;
    let mut next_seq = handle.last_event_seq().unwrap().unwrap().raw();
    let mut turns = 0u64;
    let mut compacted = 0u64;
    let assemble_started = Instant::now();
    for turn in 0..10_000u64 {
        // A durable user message + its journal event per turn (the real
        // conversation shape), plus an assistant answer part.
        let prompt = format!("TURN-{turn:06} {}", "x".repeat(600));
        next_seq = next_seq.max(handle.last_event_seq().unwrap().unwrap().raw());
        let seq = handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::ReadyForNextTurn,
                None,
                None,
            )
            .unwrap();
        let message = handle
            .put_message(
                seq.raw() as i64,
                "assistant",
                serde_json::json!({"parts": []}),
            )
            .unwrap();
        handle
            .put_text_part(message, &format!("ANSWER-{turn:06}"))
            .unwrap();
        next_seq = next_seq.max(seq.raw());
        // Keep the persisted prompt visible through a user message row.
        let user_seq = handle
            .force_append_event(
                faktor_core::event::EventKind::PhaseChanged,
                faktor_core::state::AgentState::ReadyForNextTurn,
                None,
                None,
            )
            .unwrap();
        handle
            .put_message(
                user_seq.raw() as i64,
                "user",
                serde_json::json!({"text": prompt}),
            )
            .unwrap();
        turns += 1;
        // Force a durable compaction/checkpoint every 100 turns, against the
        // checkpoint high-water mark so the loader reads only new content.
        // Keep 100 turns of NEW rows after the final checkpoint for the
        // bounded-window measurement (compaction is forced every 100 turns
        // except the last, so the loader has real new content to read).
        if turns.is_multiple_of(100) && turns != 10_000 {
            let rows = manager
                .messages_window_with_parts(
                    handle.id(),
                    None,
                    Some(checkpoint_seq),
                    2_000,
                    256 * 1024,
                )
                .await
                .unwrap();
            let kept: Vec<RecentTurn> = rows
                .iter()
                .map(|(row, parts)| RecentTurn {
                    role: row.role.clone(),
                    text: parts
                        .first()
                        .and_then(|p| p.data.get("text").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_string(),
                })
                .collect();
            let replaced: Vec<RecentTurn> = if kept.is_empty() {
                vec![RecentTurn {
                    role: "summary".into(),
                    text: format!("through turn {turn}"),
                }]
            } else {
                kept.into_iter().rev().take(40).collect()
            };
            let payload = serde_json::to_vec(&replaced).unwrap();
            let digest = history_digest(&replaced);
            checkpoint_seq = handle.last_event_seq().unwrap().unwrap().raw();
            handle
                .put_context_checkpoint(checkpoint_seq, &digest, &payload)
                .unwrap();
            handle
                .record_compaction_defaults(40_000, 12_000, 20_000, "deterministic")
                .unwrap();
            compacted += 1;
        }
    }
    let assemble = assemble_started.elapsed();
    // The real bounded loader after the last checkpoint: one window only.
    let load_started = Instant::now();
    for _ in 0..100 {
        let rows = manager
            .messages_window_with_parts(handle.id(), None, Some(checkpoint_seq), 2_000, 256 * 1024)
            .await
            .unwrap();
        assert!(rows.len() <= 200, "the loader reads a bounded window");
    }
    let load = load_started.elapsed();
    eprintln!(
        "SCALE turns n=10000 events={next_seq} compacted={compacted} total_ms={} per_turn_us={} window_load_us={}",
        assemble.as_millis(),
        assemble.as_micros() / 10_000,
        load.as_micros() / 100
    );
    assert_eq!(turns, 10_000);
    // The final 100 turns deliberately stay un-compacted so the bounded
    // window loader has NEW content to read.
    assert_eq!(compacted, 99);
}
