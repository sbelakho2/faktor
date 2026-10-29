// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Deterministic RUNTIME long-task probes (mandated mid-wave quality task).
//!
//! Not the standing suite: these prove (a) an accepted compaction's durable
//! checkpoint reproduces the identical context digest across a daemon
//! restart and stops replaying summarized rows, (b) a cancel during
//! compaction leaves no checkpoint and no compaction event, and (c) the
//! per-turn history load after a checkpoint is proportional to NEW content,
//! not to the 300+ turn transcript behind it.

use super::tests::*;
use super::*;

/// Seed one durable message row (with optional text part) at a fresh seq.
fn seed_row(handle: &faktor_session::SessionHandle, role: &str, text: &str, as_part: bool) -> i64 {
    let seq = handle
        .force_append_event(
            faktor_core::event::EventKind::ModelChunkReceived,
            faktor_core::state::AgentState::ReadyForNextTurn,
            None,
            None,
        )
        .unwrap();
    let message_id = handle
        .put_message(
            seq.raw() as i64,
            role,
            if as_part {
                serde_json::json!({ "parts": [] })
            } else {
                serde_json::json!({ "text": text })
            },
        )
        .unwrap();
    if as_part {
        handle.put_text_part(message_id, text).unwrap();
    }
    seq.raw() as i64
}

#[tokio::test]
async fn checkpoint_reproduces_identical_context_digest_across_restart_and_skips_old_rows() {
    let (seed_deps, dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let (manager, session) = shared_session(&seed_deps);
    let handle = manager.get_session(session).unwrap().unwrap();
    seed_row(&handle, "user", "OLD-SENTINEL prompt", false);
    seed_row(&handle, "assistant", "OLD-SENTINEL answer", true);

    let (rt_deps, _rt_dir) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![],
    );
    let runtime = AgentRuntime::new(rt_deps).unwrap();
    let (_seeded, head) = runtime
        .recent_turns_with_head(&handle, &ContextBudget::default())
        .await
        .unwrap();
    let head = head.expect("seeded history has a head seq");
    // The replacement context an accepted compaction would persist: the old
    // rows are summarized, not replayed.
    let compacted_turns = vec![RecentTurn {
        role: "summary".into(),
        text: "SUMMARY: the two old rows".into(),
    }];
    let digest = faktor_context::history_digest(&compacted_turns);
    handle
        .put_context_checkpoint(
            head,
            &digest,
            &serde_json::to_vec(&compacted_turns).unwrap(),
        )
        .unwrap();
    // The pre-restart context (same runtime, same store) is the checkpoint.
    let (pre_restart, _) = runtime
        .recent_turns_with_head(&handle, &ContextBudget::default())
        .await
        .unwrap();
    assert_eq!(pre_restart, compacted_turns);

    // "Daemon restart": a NEW manager over the SAME data directory.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let reopened_handle = reopened.get_session(session).unwrap().unwrap();
    let (restarted_deps, _dir2) = deps_sharing_session(
        reopened.clone(),
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![],
    );
    let restarted = AgentRuntime::new(restarted_deps).unwrap();
    let (after, after_head) = restarted
        .recent_turns_with_head(&reopened_handle, &ContextBudget::default())
        .await
        .unwrap();
    assert_eq!(
        after_head,
        Some(head),
        "the high-water seq must be identical"
    );
    assert_eq!(after, compacted_turns);
    assert_eq!(
        faktor_context::history_digest(&after),
        digest,
        "restart must reproduce the identical context digest"
    );
    // The summarized rows are NOT replayed (no rescan of old history).
    assert!(after.iter().all(|t| !t.text.contains("OLD-SENTINEL")));

    // New durable rows append after the checkpoint; old rows stay out.
    seed_row(&reopened_handle, "user", "NEW-TURN prompt", false);
    let (with_new, new_head) = restarted
        .recent_turns_with_head(&reopened_handle, &ContextBudget::default())
        .await
        .unwrap();
    assert_eq!(with_new.len(), after.len() + 1);
    assert!(with_new.last().unwrap().text.contains("NEW-TURN"));
    assert!(with_new.iter().all(|t| !t.text.contains("OLD-SENTINEL")));
    assert!(new_head.unwrap() > head);
}

#[tokio::test]
async fn cancel_during_compaction_leaves_no_checkpoint_and_no_event() {
    let (deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let (manager, session) = shared_session(&deps);
    let handle = manager.get_session(session).unwrap().unwrap();
    seed_row(&handle, "user", "do the thing", false);
    let runtime = AgentRuntime::new(deps).unwrap();

    // A long synthetic history forces a real deterministic plan.
    let history: Vec<RecentTurn> = (0..400)
        .map(|i| RecentTurn {
            role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
            text: format!("turn {i} {}", "z".repeat(600)),
        })
        .collect();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let before_events = handle.events_range(1, None).unwrap().len();
    let plan = runtime
        .try_compact_with_head(
            &handle,
            &history,
            &TaskLedger::default(),
            &ContextBudget::default(),
            &cancel,
            Some(1),
            false,
        )
        .await
        .unwrap();
    assert!(plan.is_none(), "a cancelled turn must not apply compaction");
    assert_eq!(
        handle.events_range(1, None).unwrap().len(),
        before_events,
        "no ContextCompacted/CompactRejected may be journaled after a cancel"
    );
    assert!(
        handle.context_checkpoint().unwrap().is_none(),
        "no replacement context may be persisted after a cancel"
    );
}

#[tokio::test]
async fn per_turn_load_after_checkpoint_reads_only_new_content() {
    let (deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let (manager, session) = shared_session(&deps);
    let handle = manager.get_session(session).unwrap().unwrap();
    // 300 old turns with large text parts (~1.5 MiB behind the checkpoint).
    let mut last_seq = 0;
    for i in 0..300 {
        last_seq = seed_row(
            &handle,
            if i % 2 == 0 { "user" } else { "assistant" },
            &format!("OLD-{i:03} {}", "q".repeat(5_000)),
            true,
        );
    }
    let checkpoint_turns = vec![
        RecentTurn {
            role: "system".into(),
            text: "TASK IDENTITY (verbatim): long task".into(),
        },
        RecentTurn {
            role: "summary".into(),
            text: "SUMMARY: 300 turns compressed".into(),
        },
        RecentTurn {
            role: "user".into(),
            text: "carried prompt".into(),
        },
    ];
    handle
        .put_context_checkpoint(
            last_seq as u64,
            &faktor_context::history_digest(&checkpoint_turns),
            &serde_json::to_vec(&checkpoint_turns).unwrap(),
        )
        .unwrap();
    let runtime = AgentRuntime::new(deps).unwrap();
    let (turns, head) = runtime
        .recent_turns_with_head(&handle, &ContextBudget::default())
        .await
        .unwrap();
    assert_eq!(turns, checkpoint_turns);
    assert_eq!(head, Some(last_seq as u64));
    assert!(turns.iter().all(|t| !t.text.contains("OLD-")));
    // One new turn costs exactly one new turn of assembly, not 300 rows.
    seed_row(&handle, "user", "NEW-CONTENT", false);
    let (with_new, _) = runtime
        .recent_turns_with_head(&handle, &ContextBudget::default())
        .await
        .unwrap();
    assert_eq!(with_new.len(), checkpoint_turns.len() + 1);
    assert!(with_new.last().unwrap().text.contains("NEW-CONTENT"));
}

#[test]
fn mid_history_orphan_tool_pairs_are_removed_from_the_wire() {
    let call = faktor_provider::RequestMessage {
        role: faktor_provider::Role::Assistant,
        content: vec![
            faktor_provider::ContentPart::tool_call("call-known", "echo", serde_json::json!({})),
            faktor_provider::ContentPart::tool_call(
                "call-unanswered",
                "echo",
                serde_json::json!({}),
            ),
        ],
    };
    let results = faktor_provider::RequestMessage {
        role: faktor_provider::Role::User,
        content: vec![
            faktor_provider::ContentPart::tool_result("ok", false, "call-known"),
            faktor_provider::ContentPart::tool_result("ghost", false, "call-ghost"),
        ],
    };
    let out = sanitize_tool_pairing(vec![call, results]);
    assert_eq!(out.len(), 2, "both messages keep their paired part");
    let calls: Vec<&str> = out[0]
        .content
        .iter()
        .filter_map(|p| match &p.kind {
            faktor_provider::ContentKind::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec!["call-known"], "unanswered call is dropped");
    let results: Vec<&str> = out[1]
        .content
        .iter()
        .filter_map(|p| p.tool_call_id.as_deref())
        .collect();
    assert_eq!(results, vec!["call-known"], "ghost result is dropped");
}

/// A provider that answers EVERY stream with one text chunk and a clean
/// Done. The probe needs no per-turn script (one script entry per logical
/// turn would make the measurement depend on test plumbing); this keeps the
/// one-runtime/one-session shape real for the full run.
pub(crate) struct AckProvider;

impl faktor_provider::Provider for AckProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            context: 200_000,
            ..Default::default()
        }
    }

    fn capability_resolution(&self, model: &str) -> faktor_core::model::CapabilityResolution {
        faktor_core::model::CapabilityResolution::known(self.capabilities(model))
    }

    fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderChunk::Text { text: "ack".into() }),
            Ok(ProviderChunk::Done),
        ]))
    }
}

/// MEASURED long-run effectiveness: 120 real logical turns with repeated
/// threshold compaction. Captures the ACTUAL wire prompt size at every
/// provider call, so the report can state tokens/turn, the compaction
/// dividend, and the bounded steady state rather than asserting a proxy.
#[tokio::test]
async fn repeated_compaction_keeps_prompt_tokens_bounded_and_linear_in_new_content() {
    const TURNS: usize = 120;
    let measured: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = measured.clone();
    let provider = Arc::new(AckProvider);
    let inspected = Arc::new(InspectingProvider::new(provider, move |_n, req| {
        let prompt = format!("{}\n{}", req.system, request_text(req));
        sink.lock()
            .unwrap()
            .push(faktor_context::Estimator.estimate_tokens(&prompt));
        Ok(())
    }));
    let (mut deps, _dir) = deps_with(inspected, vec![]);
    // 35% of the effective window: the prompt crosses it every few turns,
    // so compaction runs repeatedly with a real, measurable dividend
    // instead of thrashing on a deliberately tiny threshold.
    deps.compact_at_usage = 0.35;
    let runtime = AgentRuntime::new(deps).unwrap();
    let (manager, session) = shared_session(runtime.deps());
    let handle = manager.get_session(session).unwrap().unwrap();

    let mut compactions = 0usize;
    let mut last_history_turns = 0usize;
    for turn in 0..TURNS {
        let prompt = format!("TURN-{turn:03} keep going {}", "x".repeat(5_000));
        let receipt = handle.submit_prompt(&prompt, &[]).unwrap();
        let outcome = runtime
            .drive_turn(
                &handle,
                receipt.op_id,
                receipt.op_meta.cancellation.clone(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.final_state,
            AgentState::ReadyForNextTurn,
            "turn {turn}: stop_reason={:?} events={:?}",
            outcome.stop_reason,
            handle
                .events_range(1, None)
                .unwrap()
                .into_iter()
                .rev()
                .take(4)
                .map(|event| (event.kind, event.payload))
                .collect::<Vec<_>>()
        );
        if outcome.compacted {
            compactions += 1;
        }
        let (recent, _) = runtime
            .recent_turns_with_head(&handle, &ContextBudget::default())
            .await
            .unwrap();
        last_history_turns = recent.len();
    }
    let prompts = measured.lock().unwrap().clone();
    assert_eq!(prompts.len(), TURNS, "one provider call per turn");
    let total: usize = prompts.iter().sum();
    let avg = total / prompts.len();
    let max = *prompts.iter().max().unwrap();
    let saved: usize = prompts
        .windows(2)
        .map(|pair| pair[0].saturating_sub(pair[1]))
        .sum();
    let checkpoint_bytes = handle
        .context_checkpoint()
        .unwrap()
        .map(|checkpoint| checkpoint.payload.len())
        .unwrap_or(0);
    eprintln!(
        "PERF repeated_compaction turns={TURNS} compactions={compactions} \
avg_prompt_tokens={avg} max_prompt_tokens={max} total_prompt_tokens={total} \
positive_drop_tokens={saved} final_history_turns={last_history_turns} \
checkpoint_bytes={checkpoint_bytes}"
    );

    assert!(
        compactions >= 3,
        "threshold must compact repeatedly: {compactions}"
    );
    assert!(
        max < 50_000,
        "repeated compaction must keep the wire near the 35% threshold, max={max}"
    );
    assert!(
        last_history_turns < 64,
        "post-checkpoint assembly is bounded, not linear in the 120-turn transcript: {last_history_turns}"
    );
    assert!(
        saved > 0,
        "repeated compaction must show real prompt-token drops: {saved}"
    );
}
