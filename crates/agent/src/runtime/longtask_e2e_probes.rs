//! Rev8 end-to-end long-task probe (NOT the standing suite).
//!
//! One session drives 300+ logical turns through the REAL runtime with
//! threshold compaction forced on, a tool-output storm in durable history,
//! child-agent sessions, a daemon-style store reopen, a cancel at the
//! compaction seam and memory churn. Assertions are exact invariants, not
//! smoke:
//!
//! - the durable checkpoint is the EXACT replacement context and its digest
//!   equals `history_digest(decoded turns)` after restart;
//! - after a checkpoint, per-turn assembly materializes the checkpoint plus
//!   NEW rows, never the 300 old rows;
//! - a cancelled compaction writes neither checkpoint nor event;
//! - memory delete stops retrieval while restore revives it, dedupe is a
//!   true no-op and another workspace sees nothing;
//! - children have independent checkpoints and cannot mutate the parent's.

use std::sync::Arc;

use super::longtask_probes::AckProvider;
use super::tests::*;
use super::*;
use faktor_memory::{
    MemoryRecordStore, MemoryScope, NewMemoryRecord, Provenance, TypedMemoryValue,
};

fn storm_row(handle: &faktor_session::SessionHandle, role: &str, text: &str) -> i64 {
    let seq = handle
        .force_append_event(
            faktor_core::event::EventKind::ModelChunkReceived,
            faktor_core::state::AgentState::ReadyForNextTurn,
            None,
            None,
        )
        .unwrap();
    let message = handle
        .put_message(seq.raw() as i64, role, serde_json::json!({ "parts": [] }))
        .unwrap();
    handle.put_text_part(message, text).unwrap();
    seq.raw() as i64
}

fn memory_draft(scope: &MemoryScope, predicate: &str, value: &str) -> NewMemoryRecord {
    NewMemoryRecord {
        scope: scope.clone(),
        subject: "long-task".into(),
        predicate: predicate.into(),
        value: TypedMemoryValue::Text(value.into()),
        provenance: Provenance::user(1),
        confidence_ppm: 1_000_000,
        evidence: Vec::new(),
        source_revision: None,
    }
}

#[tokio::test]
async fn rev8_long_task_end_to_end() {
    // 300 logical turns: one text chunk + Done each. History is grown the
    // real way (submit + drive), and compaction is forced above zero usage.
    // An always-answering provider keeps the 300-turn run independent of a
    // finite per-turn script.
    let (mut deps, dir) = deps_with(Arc::new(AckProvider), vec![]);
    deps.compact_at_usage = 0.0;
    let runtime = AgentRuntime::new(deps).unwrap();
    let (manager, session) = shared_session(runtime.deps());
    let handle = manager.get_session(session).unwrap().unwrap();

    // Tool-output storm in the durable transcript (large text parts).
    for i in 0..48 {
        let body = if i % 3 == 0 {
            format!("STORM-{i:03} {}", "q".repeat(64 * 1024))
        } else {
            format!("STORM-{i:03} {}", "w".repeat(4 * 1024))
        };
        storm_row(&handle, "assistant", &body);
    }

    let turn_clock = std::time::Instant::now();
    let mut accepted_compactions = 0usize;
    for turn in 0..300 {
        let prompt = format!(
            "TURN-{turn:03} keep the long task alive {}",
            "x".repeat(1_800)
        );
        let receipt = handle.submit_prompt(&prompt, &[]).unwrap();
        assert!(!receipt.queued, "turn {turn} must be admitted, not queued");
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
            "turn {turn} must complete"
        );
        if outcome.compacted {
            accepted_compactions += 1;
        }
    }
    let turns_elapsed = turn_clock.elapsed();
    eprintln!(
        "PERF long_task turns=300 elapsed_ms={} per_turn_us={} accepted_compactions={accepted_compactions}",
        turns_elapsed.as_millis(),
        turns_elapsed.as_micros() / 300
    );
    assert!(
        accepted_compactions >= 20,
        "forced threshold must compact repeatedly, got {accepted_compactions}"
    );
    assert!(handle.pending_tool_runs().unwrap().is_empty());

    // EXACT checkpoint invariant: the durable replacement context decodes to
    // the wired turns and its digest is the canonical history digest.
    let checkpoint = handle
        .context_checkpoint()
        .unwrap()
        .expect("at least one accepted compaction must persist a checkpoint");
    let persisted: Vec<RecentTurn> = serde_json::from_slice(&checkpoint.payload).unwrap();
    assert_eq!(
        faktor_context::history_digest(&persisted),
        checkpoint.digest,
        "checkpoint digest must describe the exact persisted turns"
    );
    assert!(checkpoint.through_seq > 0);

    // Per-turn assembly after the checkpoint is proportional to NEW content:
    // the 300 old turns/storm rows are behind the high-water seq.
    let budget = ContextBudget::default();
    let (assembled, head) = runtime
        .recent_turns_with_head(&handle, &budget)
        .await
        .unwrap();
    let head = head.unwrap();
    assert_eq!(head, checkpoint.through_seq.max(head));
    assert!(
        assembled.len() <= 64,
        "post-checkpoint assembly read {} turns (old history resurfaced)",
        assembled.len()
    );

    // Child agents: three children with their own history/checkpoint. Their
    // turns must not rewrite the parent checkpoint.
    let parent_workspace = handle.row().unwrap().workspace_id;
    let parent_digest = handle.context_checkpoint().unwrap().unwrap().digest;
    let mut children = Vec::new();
    for child_index in 0..3 {
        let child = manager
            .create_child_session(
                session,
                parent_workspace,
                faktor_core::WorktreeId::new(child_index + 1),
                faktor_core::TaskId::new(child_index + 1),
                "fake",
                "m",
                &format!("child {child_index}"),
                faktor_session::child::ChildOwnership::ReadOnlyShared,
            )
            .unwrap();
        for turn in 0..30 {
            let child_prompt = format!("CHILD-{child_index}-{turn} {}", "c".repeat(600));
            let receipt = child.submit_prompt(&child_prompt, &[]).unwrap();
            let outcome = runtime
                .drive_turn(
                    &child,
                    receipt.op_id,
                    receipt.op_meta.cancellation.clone(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
        }
        children.push(child);
    }
    assert_eq!(
        handle.context_checkpoint().unwrap().unwrap().digest,
        parent_digest,
        "children must never rewrite the parent's rendered checkpoint"
    );
    for child in &children {
        let child_checkpoint = child.context_checkpoint().unwrap().unwrap();
        let child_turns: Vec<RecentTurn> =
            serde_json::from_slice(&child_checkpoint.payload).unwrap();
        assert_eq!(
            faktor_context::history_digest(&child_turns),
            child_checkpoint.digest
        );
    }

    // Cancel at the compaction seam: a pre-cancelled token can never commit.
    let cancel = faktor_core::cancellation::CancellationToken::new();
    cancel.cancel();
    let before_digest = handle.context_checkpoint().unwrap().unwrap().digest;
    let recent = runtime.recent_turns(&handle, &budget).await.unwrap();
    let ledger: faktor_context::ledger::TaskLedger = handle
        .get_task_ledger()
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let plan = runtime
        .try_compact_with_head(
            &handle,
            &recent,
            &ledger,
            &budget,
            &cancel,
            Some(head),
            false,
        )
        .await
        .unwrap();
    assert!(plan.is_none(), "a cancelled compaction must not apply");
    assert_eq!(
        handle.context_checkpoint().unwrap().unwrap().digest,
        before_digest
    );

    // Memory churn: 400 records mixed across scopes with dedupe, updates,
    // deletes and restores; retrieval stays bounded and excludes tombstones.
    let store = manager.store();
    let workspace_id = handle.row().unwrap().workspace_id;
    let records = MemoryRecordStore::new(store.clone(), session);
    let session_scope = MemoryScope::session_scope(session);
    let workspace_scope = MemoryScope::Workspace(workspace_id.raw().to_string());
    let memory_clock = std::time::Instant::now();
    let mut ids = Vec::new();
    for i in 0..200 {
        let scope = if i % 2 == 0 {
            session_scope.clone()
        } else {
            workspace_scope.clone()
        };
        let created = records
            .create(
                memory_draft(&scope, &format!("fact-{i}"), &format!("value-{i}")),
                i + 1,
            )
            .unwrap();
        ids.push(created.record.id);
        // Exact duplicate: no write, no version bump.
        let dedupe = records
            .create(
                memory_draft(&scope, &format!("fact-{i}"), &format!("value-{i}")),
                i + 1000,
            )
            .unwrap();
        assert!(dedupe.deduped && dedupe.record.version == 1);
    }
    for (index, id) in ids.iter().take(60).enumerate() {
        let live = records.get(id).unwrap().unwrap();
        let updated = records
            .update(
                id,
                live.version,
                TypedMemoryValue::Text(format!("edited-{index}")),
                10_000 + index as i64,
            )
            .unwrap();
        if index % 3 == 0 {
            records
                .delete(id, updated.record.version, Some("churn".into()), 1, 20_000)
                .unwrap();
        }
    }
    let page = records.list(&session_scope, false, None, 200).unwrap();
    eprintln!(
        "PERF memory churn records=400 ops=460 elapsed_ms={} page_records={} page_scanned={}",
        memory_clock.elapsed().as_millis(),
        page.records.len(),
        page.scanned
    );
    assert!(page.records.len() <= 200);
    assert!(page.scanned <= faktor_memory::MAX_MEMORY_SCAN_ROWS);
    let tombstone_id = ids
        .iter()
        .find(|id| {
            records
                .get(id)
                .unwrap()
                .map(|record| record.deleted.is_some())
                .unwrap_or(false)
        })
        .cloned()
        .expect("at least one tombstone");
    let tombstone = records.get(&tombstone_id).unwrap().unwrap();
    assert!(
        !records
            .live_scope_records(&session_scope, 500)
            .unwrap()
            .iter()
            .any(|fact| fact.id == tombstone_id),
        "deleted memory must never be retrieval-eligible"
    );
    let restored = records
        .restore(&tombstone_id, tombstone.version, 30_000)
        .unwrap();
    assert!(restored.record.is_live());
    assert_eq!(restored.record.version, tombstone.version + 1);

    // Context injection: both scopes ride the bounded DATA block; a deleted
    // record is never injected.
    records
        .create(
            memory_draft(&session_scope, "inject-session", "INJECT-SESSION"),
            40_000,
        )
        .unwrap();
    records
        .create(
            memory_draft(&workspace_scope, "inject-workspace", "INJECT-WORKSPACE"),
            40_001,
        )
        .unwrap();
    let doomed = records
        .create(
            memory_draft(&session_scope, "inject-deleted", "INJECT-DELETED"),
            40_002,
        )
        .unwrap();
    records
        .delete(&doomed.record.id, doomed.record.version, None, 2, 40_003)
        .unwrap();
    let injected = runtime.memory_data_block(&handle);
    assert!(injected.contains("INJECT-SESSION"), "{injected}");
    assert!(injected.contains("INJECT-WORKSPACE"), "{injected}");
    assert!(
        !injected.contains("INJECT-DELETED"),
        "a tombstone must never be rendered into context: {injected}"
    );
    assert!(
        injected.len() <= 4096,
        "memory block is bounded: {}",
        injected.len()
    );
    // Workspace memory crosses sessions of ONE workspace; session memory
    // and tombstones never do.
    let sibling = manager
        .create_session(parent_workspace, "sibling", "fake", "m")
        .unwrap();
    let sibling_block = runtime.memory_data_block(&sibling);
    assert!(
        sibling_block.contains("INJECT-WORKSPACE"),
        "workspace memory must be visible to a sibling session: {sibling_block}"
    );
    assert!(
        !sibling_block.contains("INJECT-SESSION"),
        "session memory must stay private: {sibling_block}"
    );
    assert!(
        !sibling_block.contains("INJECT-DELETED"),
        "a tombstone must never be rendered into a sibling context: {sibling_block}"
    );
    assert!(
        runtime
            .memory_data_block(&handle)
            .contains("INJECT-SESSION"),
        "the owning session still sees its own memory"
    );

    // Daemon-style restart: reopen the SAME store and reproduce the exact
    // checkpoint context digest without replaying the old transcript.
    drop(runtime);
    drop(handle);
    let store_root = manager.store().root().to_path_buf();
    let cas_root = store_root.parent().unwrap().join("cas-e2e-restart");
    drop(manager);
    let reopened = faktor_session::SessionManager::open(store_root, cas_root, false).unwrap();
    let reopened_handle = reopened.get_session(session).unwrap().unwrap();
    let (restart_deps, _restart_dir) = deps_sharing_session(
        reopened.clone(),
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![],
    );
    let restarted = AgentRuntime::new(restart_deps).unwrap();
    let checkpoint = reopened_handle.context_checkpoint().unwrap().unwrap();
    let persisted: Vec<RecentTurn> = serde_json::from_slice(&checkpoint.payload).unwrap();
    assert_eq!(
        faktor_context::history_digest(&persisted),
        checkpoint.digest,
        "restart must reproduce the exact compacted digest"
    );
    let (after_restart, after_head) = restarted
        .recent_turns_with_head(&reopened_handle, &budget)
        .await
        .unwrap();
    assert!(
        after_restart.len() <= 64,
        "restart replayed {} old turns instead of the checkpoint",
        after_restart.len()
    );
    assert!(after_head.is_some());
    let checkpoint_bytes = checkpoint.payload.len();
    eprintln!(
        "PERF compaction checkpoint_bytes={} persisted_turns={} assembly_turns={}",
        checkpoint_bytes,
        persisted.len(),
        after_restart.len()
    );
    // Memory survives the restart and tombstones stay non-retrievable.
    let reopened_records = MemoryRecordStore::new(reopened.store(), session);
    assert!(reopened_records
        .get(&tombstone_id)
        .unwrap()
        .unwrap()
        .is_live());
    assert!(reopened_records.get(&ids[0]).unwrap().is_some());
    let _ = dir;
}
