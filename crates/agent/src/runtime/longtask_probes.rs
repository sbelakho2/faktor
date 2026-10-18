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
