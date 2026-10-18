//! Deterministic LONG-TASK probes (mandated mid-wave quality task).
//!
//! These are NOT the standing compactor suite: they drive a 300-turn
//! synthetic session through repeated compaction cycles with large tool
//! outputs, assert the hard invariant against the EXACT wire cost, prove the
//! durable projection/facts survive every cycle, and instrument the keep scan
//! to show per-compaction work stays a constant number of passes over the
//! loaded window instead of rescanning the whole conversation.

use super::*;
use crate::compactor::take_turn_visits;

fn synthetic_history(turns: usize) -> Vec<RecentTurn> {
    let roles = ["user", "assistant", "tool", "assistant", "tool_call"];
    (0..turns)
        .map(|i| {
            let role = roles[i % roles.len()];
            // Large tool outputs (but below the 16 KiB evidence fold) and
            // bulky assistant answers: the long-task shape.
            let body = if i % 7 == 0 {
                "y".repeat(5_000)
            } else if i % 3 == 0 {
                "z".repeat(2_000)
            } else {
                "x".repeat(400)
            };
            RecentTurn {
                role: role.to_string(),
                text: format!("FACT-{i:04} cycle {i} {body}"),
            }
        })
        .collect()
}

fn projected() -> TaskContextProjection {
    TaskContextProjection::from_durable_rows(DurableTaskRows {
        goal: "GOAL-verbatim-ship-the-long-task".into(),
        criteria: vec![
            "CRITERION-1-tests-pass".into(),
            "CRITERION-2-no-loss".into(),
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
async fn three_hundred_turns_through_repeated_compactions_keep_exact_invariant_and_facts() {
    let compactor = Compactor::deterministic_only();
    let projection = projected();
    let ledger = TaskLedger::default();
    let target = 20_000usize;
    let mut history = synthetic_history(300);
    let initial_turns = history.len();
    let mut archived_text = String::new();
    let mut total_visits = 0usize;
    let mut total_residue = 0usize;
    let mut max_window = 0usize;

    for cycle in 0..10 {
        let before = estimate_history_tokens(&history);
        assert!(before > target, "cycle {cycle}: history must be over cap");
        let req = CompactionRequest {
            before_tokens: before,
            target_tokens: target,
            ..Default::default()
        };
        let plan = compactor
            .compact_projected(&history, &projection, &ledger, &req)
            .await;
        assert!(
            plan.accepted,
            "cycle {cycle}: compaction must be accepted (after={} cap={})",
            plan.after_tokens,
            req.hard_cap()
        );
        // THE HARD INVARIANT IS MEASURED, NOT DECLARED: after_tokens must be
        // the exact estimator cost of the turns that ride the next request.
        assert_eq!(
            plan.after_tokens,
            estimate_wire_turns(&plan.kept_recent),
            "cycle {cycle}: reported after_tokens must equal the actual wire cost"
        );
        assert!(
            plan.after_tokens <= req.hard_cap(),
            "cycle {cycle}: accepted plan exceeds the hard cap"
        );
        assert!(
            (plan.after_tokens as f64) <= before as f64 * 0.75,
            "cycle {cycle}: the configured 25% minimum reduction must hold"
        );
        // Goal + criteria ride VERBATIM on the wire (never summarized).
        let kept_text = wire_text(&plan.kept_recent);
        assert!(kept_text.contains("GOAL-verbatim-ship-the-long-task"));
        assert!(kept_text.contains("CRITERION-1-tests-pass"));
        assert!(kept_text.contains("CRITERION-2-no-loss"));

        // Nothing is dropped: every turn is either on the kept wire or in
        // the lossless archive chunks (oldest-first chunks concatenate to
        // the rendered evicted turns).
        let evicted_text = plan.archive_chunks.concat();
        archived_text.push_str(&evicted_text);
        for turn in &history {
            if turn.role == "summary"
                || (turn.role == "assistant" && turn.text.starts_with("[Earlier context archived:"))
            {
                // Metadata is regenerated/rewritten by the next cycle; the
                // durable payload lives in the CAS manifest and the durable
                // projection, not in this pointer text.
                continue;
            }
            let in_kept = plan.kept_recent.iter().any(|k| k.text.contains(&turn.text));
            let in_archive = evicted_text.contains(&turn.text);
            assert!(
                in_kept || in_archive,
                "cycle {cycle}: turn {:?} was neither kept nor archived",
                &turn.text[..turn.text.len().min(24)]
            );
        }

        // Instrumented work: the keep scan visits at most a constant number
        // of passes over the turns it was given. The window fed to the next
        // cycle is the bounded kept window, so work never grows with the
        // 300+ turn transcript.
        let visits = take_turn_visits();
        total_visits += visits;
        total_residue += history.len();
        assert!(
            visits <= history.len() * 4 + 16,
            "cycle {cycle}: keep scan visited {visits} turns for a {}-turn window",
            history.len()
        );
        max_window = max_window.max(plan.kept_recent.len());

        // Next cycle: the runtime strips regenerated SYSTEM blocks before
        // compacting, keeps prior summaries/digest (which the compactor
        // carries verbatim), then the session continues with new turns.
        history = plan
            .kept_recent
            .iter()
            .filter(|t| t.role != "system")
            .cloned()
            .collect();
        history.extend(synthetic_history(20));
    }

    assert!(!archived_text.is_empty(), "the cycles must archive");
    assert!(
        total_visits <= total_residue * 4 + 16 * 10,
        "total keep-scan work must stay linear in the per-cycle windows"
    );
    assert!(
        max_window < initial_turns,
        "the retained window must stay bounded below the original transcript"
    );
    // No drift/duplication: at most one prior summary is carried, and the
    // digest pointer is regenerated, never multiplied.
    let summaries = history.iter().filter(|t| t.role == "summary").count();
    assert!(summaries <= 1, "prior summaries must not accumulate");
    let digests = history
        .iter()
        .filter(|t| t.text.starts_with("[Earlier context archived:"))
        .count();
    assert!(digests <= 1, "archive pointers must not accumulate");
    let systems = history.iter().filter(|t| t.role == "system").count();
    assert_eq!(
        systems, 0,
        "synthesized blocks are regenerated, not carried"
    );
}

#[tokio::test]
async fn a_blank_summary_can_never_wipe_the_history() {
    struct Blank;
    impl Summarizer for Blank {
        fn summarize<'a>(
            &'a self,
            _residue: &'a [RecentTurn],
            _durable_facts: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            Box::pin(async move { "   \n\t ".to_string() })
        }
    }
    let compactor = Compactor::new(Some(Arc::new(Blank)));
    let history = synthetic_history(200);
    let projection = projected();
    let before = estimate_history_tokens(&history);
    let req = CompactionRequest {
        before_tokens: before,
        target_tokens: 20_000,
        summarizer_residue_budget_tokens: 0, // always ask the model
        ..Default::default()
    };
    let plan = compactor
        .compact_projected(&history, &projection, &TaskLedger::default(), &req)
        .await;
    assert!(plan.accepted, "deterministic fallback must still compact");
    assert_eq!(plan.strategy, CompactionStrategy::Rejected);
    assert!(
        plan.kept_recent.iter().all(|t| t.text.trim() != ""),
        "a blank summary must never ride the wire"
    );
    assert_eq!(
        plan.after_tokens,
        estimate_wire_turns(&plan.kept_recent),
        "the fallback after_tokens must still be exact"
    );
    assert!(wire_text(&plan.kept_recent).contains("GOAL-verbatim-ship-the-long-task"));
}

#[tokio::test]
async fn a_prior_summary_is_carried_verbatim_never_resummarized() {
    let compactor = Compactor::deterministic_only();
    let mut history = synthetic_history(120);
    let summary_text = "SUMMARY-v1: parser is recursive descent; tests green";
    history.insert(
        0,
        RecentTurn {
            role: "summary".into(),
            text: summary_text.into(),
        },
    );
    let before = estimate_history_tokens(&history);
    let req = CompactionRequest {
        before_tokens: before,
        target_tokens: 15_000,
        ..Default::default()
    };
    let plan = compactor
        .compact_projected(&history, &projected(), &TaskLedger::default(), &req)
        .await;
    assert!(plan.accepted);
    assert_eq!(
        plan.kept_recent
            .iter()
            .filter(|t| t.role == "summary")
            .count(),
        1,
        "exactly one prior summary is carried"
    );
    assert!(plan
        .kept_recent
        .iter()
        .any(|t| t.role == "summary" && t.text == summary_text));
}

#[tokio::test]
async fn identity_that_cannot_fit_is_a_typed_rejection_not_an_over_cap_success() {
    let compactor = Compactor::deterministic_only();
    let projection = TaskContextProjection::from_durable_rows(DurableTaskRows {
        goal: "G".repeat(50_000),
        criteria: vec!["C".repeat(50_000)],
        ..Default::default()
    });
    let history = synthetic_history(50);
    let req = CompactionRequest {
        before_tokens: 5_000,
        target_tokens: 4_000,
        ..Default::default()
    };
    let plan = compactor
        .compact_projected(&history, &projection, &TaskLedger::default(), &req)
        .await;
    assert!(!plan.accepted, "identity alone exceeds the cap");
    assert_eq!(plan.strategy, CompactionStrategy::Rejected);
    assert_eq!(
        plan.after_tokens,
        estimate_wire_turns(&plan.kept_recent),
        "even a rejected plan reports its exact wire cost"
    );
    assert!(plan.after_tokens > req.hard_cap());
}

#[tokio::test]
async fn an_over_cap_active_prompt_is_a_typed_rejection_never_a_silent_drop() {
    let compactor = Compactor::deterministic_only();
    let prompt = format!("ACTIVE-INSTRUCTION {}", "p".repeat(100_000));
    let history = vec![
        RecentTurn {
            role: "assistant".into(),
            text: "previous answer".into(),
        },
        RecentTurn {
            role: "user".into(),
            text: prompt.clone(),
        },
    ];
    let req = CompactionRequest {
        before_tokens: 40_000,
        target_tokens: 1_000,
        ..Default::default()
    };
    let plan = compactor
        .compact_projected(&history, &projected(), &TaskLedger::default(), &req)
        .await;
    assert!(
        !plan.accepted,
        "a plan that drops the active prompt must be rejected"
    );
    assert_eq!(plan.strategy, CompactionStrategy::Rejected);
    assert_eq!(
        plan.after_tokens,
        estimate_wire_turns(&plan.kept_recent),
        "the rejected plan still reports its exact wire cost"
    );
}

#[tokio::test]
async fn an_llm_summary_never_drops_the_active_instruction() {
    struct Terse;
    impl Summarizer for Terse {
        fn summarize<'a>(
            &'a self,
            _residue: &'a [RecentTurn],
            _durable_facts: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            Box::pin(async move { "SUMMARY that omits the ask".to_string() })
        }
    }
    let active = "ACTIVE-ASK-keep-the-cache-warm";
    let mut history = synthetic_history(120);
    history.push(RecentTurn {
        role: "user".into(),
        text: active.into(),
    });
    let before = estimate_history_tokens(&history);
    let req = CompactionRequest {
        before_tokens: before,
        target_tokens: 40_000,
        summarizer_residue_budget_tokens: 0, // force the LLM path
        ..Default::default()
    };
    let plan = Compactor::new(Some(Arc::new(Terse)))
        .compact_projected(&history, &projected(), &TaskLedger::default(), &req)
        .await;
    assert!(plan.accepted, "the summary + prompt must fit");
    assert_eq!(plan.strategy, CompactionStrategy::LlmSummary);
    assert!(
        plan.kept_recent
            .iter()
            .any(|t| t.role == "user" && t.text == active),
        "the active user instruction must ride verbatim alongside the summary"
    );
    assert_eq!(plan.after_tokens, estimate_wire_turns(&plan.kept_recent));
}
