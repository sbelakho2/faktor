use super::*;

pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), true).unwrap();
    (dir, s)
}

#[test]
fn op_id_seq_seeds_high_and_reserves_contiguous_global_ranges() {
    // Fresh stores seed the ONE global row from the migration-time clock
    // (see `op_id_seq_seed`): the seed must sit far above every
    // pre-migration `clock + counter` id and stay aligned to the 1024-id
    // reservation quantum.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s1 = store.create_session(ws, "a", "p", "m").unwrap();
    let s2 = store.create_session(ws, "b", "p", "m").unwrap();
    let hw0 = store.op_id_seq_high_water().unwrap();
    assert!(
        hw0 > (1u64 << 20),
        "seed must dominate clock+counter ids: {hw0}"
    );
    assert_eq!(hw0 % 1024, 0, "seed must sit on a quantum boundary");
    // The sequence is GLOBAL: alternating sessions get contiguous ranges.
    let (a0, n0) = store.alloc_op_ids(s1.id, 100).unwrap();
    let (b0, n1) = store.alloc_op_ids(s2.id, 250).unwrap();
    let (a1, n2) = store.alloc_op_ids(s1.id, 7).unwrap();
    assert_eq!((a0, n0), (hw0, 100), "first range starts at the seed");
    assert_eq!((b0, n1), (hw0 + 100, 250), "second range is contiguous");
    assert_eq!((a1, n2), (hw0 + 350, 7), "ranges never interleave");
    assert_eq!(
        store.op_id_seq_high_water().unwrap(),
        hw0 + 357,
        "high water is one past the last reserved id"
    );
    assert_ne!(a0, 0, "zero is contractually impossible");
}

/// Evidence for the cross-session usage batch reads (perf wave): the
/// historical `/native/usage` shape — per-session and per-task round-trips —
/// versus the batch API, on the SAME 5k-session corpus. Every aggregate
/// folded from the batch must equal the per-session folds byte for byte,
/// and the batch must win the wall clock measured in this same run.
#[test]
fn cross_session_usage_batch_matches_legacy_reads_at_5k_sessions() {
    use std::time::Instant;
    const N: u64 = 5_000;
    const TASK_SCAN: usize = 200; // MAX_NATIVE_LIST on the server side.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let mut ids: Vec<SessionId> = Vec::with_capacity(N as usize);
    let mut pairs: Vec<(SessionId, TaskId)> = Vec::with_capacity(N as usize);
    let task_id = TaskId::new(1);
    for i in 0..N {
        let s = store
            .create_session(ws, &format!("t{i}"), "fake", "m")
            .unwrap();
        let sid = s.id;
        ids.push(sid);
        store
            .record_provider_call_with_prefix(
                sid,
                OpId::new(i + 1),
                "fake",
                "m",
                "completed",
                Some(i % 7),
                Some(3),
                None,
                Some([((i % 251) as u8) ^ 0x5a; 32]),
                Some(i % 1_000),
                Some(0.5),
            )
            .unwrap();
        store
            .upsert_task(&TaskRow {
                task_id,
                session_id: sid,
                goal: format!("g{i}"),
                acceptance_criteria: vec![],
                plan: vec![],
                attachments: vec![],
                contract: Default::default(),
                max_tokens: None,
                max_turns: None,
                spent_tokens: 0,
                spent_turns: 0,
                state: TaskState::Running,
                revision: TaskRevision::new(1),
                created_ms: i as i64,
                updated_ms: i as i64,
            })
            .unwrap();
        let crate::CostReserveOutcome::Granted(r) = store
            .cost_reserve(sid, task_id, OpId::new(i + 1), 100, i as i64)
            .unwrap()
        else {
            panic!("reserve must be granted");
        };
        store
            .cost_settle(r, 60, Some(60), Some(55), None, i as i64 + 1)
            .unwrap();
        pairs.push((sid, task_id));
    }

    // ---- legacy per-session / per-task reads (the historical route shape)
    let legacy_start = Instant::now();
    let mut legacy_tokens = 0u64;
    let mut legacy_prefix_rows = 0u64;
    let mut legacy_prefix_tokens = 0u64;
    let mut legacy_obs = 0u64;
    let mut legacy_spent = 0u64;
    let mut legacy_settled = 0u64;
    let mut legacy_settled_spent = 0u64;
    for sid in &ids {
        legacy_tokens += store.session_usage_tokens(*sid).unwrap();
        for r in store.provider_call_prefix_rows(*sid).unwrap() {
            legacy_prefix_rows += 1;
            legacy_prefix_tokens += r.prompt_tokens as u64;
            if r.prefix_stability.is_some() {
                legacy_obs += 1;
            }
        }
        let tasks = store.list_tasks(*sid).unwrap();
        for t in tasks.iter().take(TASK_SCAN) {
            if let Ok(Some(cost)) = store.cost_task_row(*sid, t.task_id) {
                legacy_spent += cost.spent_cost_micro;
            }
            let rows = store.cost_reservations_of(*sid, t.task_id, 10_000).unwrap();
            for row in rows {
                if row.status == "settled" {
                    legacy_settled += 1;
                    legacy_settled_spent += row
                        .settled_cost_micro
                        .or(row.provider_reported_micro)
                        .or(row.provider_cost_micro)
                        .unwrap_or(0);
                }
            }
        }
    }
    let legacy_elapsed = legacy_start.elapsed();

    // ---- batch reads (the new route shape: one statement per chunk)
    let batch_start = Instant::now();
    let usage = store.session_usage_aggregates(&ids).unwrap();
    let tasks = store.task_usage_scope(&ids, TASK_SCAN as u32).unwrap();
    let reservations = store.cost_reservations_for_tasks(&pairs, 10_000).unwrap();
    let batch_elapsed = batch_start.elapsed();
    assert!(usage.unreadable.is_empty(), "all sessions servable");
    assert!(tasks.unreadable.is_empty(), "all task scopes servable");
    assert!(reservations.unreadable.is_empty(), "all pairs servable");

    // Byte-for-byte equivalence of every folded aggregate.
    let mut batch_tokens = 0u64;
    let mut batch_prefix_rows = 0u64;
    let mut batch_prefix_tokens = 0u64;
    let mut batch_obs = 0u64;
    for agg in &usage.rows {
        assert!(agg.prefix_servable);
        batch_tokens += agg.tokens;
        batch_prefix_rows += agg.prefix_rows;
        batch_prefix_tokens += agg.prefix_tokens;
        batch_obs += agg.prefix_stability_observations;
    }
    assert_eq!(batch_tokens, legacy_tokens);
    assert_eq!(batch_prefix_rows, legacy_prefix_rows);
    assert_eq!(batch_prefix_tokens, legacy_prefix_tokens);
    assert_eq!(batch_obs, legacy_obs);
    let mut batch_spent = 0u64;
    let mut scoped = 0usize;
    for row in &tasks.rows {
        batch_spent += u64::try_from(row.spent_cost_micro).unwrap_or(u64::MAX);
        scoped += 1;
    }
    assert_eq!(scoped, N as usize);
    assert_eq!(batch_spent, legacy_spent);
    let mut batch_settled = 0u64;
    let mut batch_settled_spent = 0u64;
    for row in &reservations.rows {
        if row.status == "settled" {
            batch_settled += 1;
            batch_settled_spent += row
                .settled_cost_micro
                .or(row.provider_reported_micro)
                .or(row.provider_cost_micro)
                .unwrap_or(0);
        }
    }
    assert_eq!(batch_settled, legacy_settled);
    assert_eq!(batch_settled_spent, legacy_settled_spent);

    eprintln!(
        "cross-session usage ({N} sessions x provider_call + task + reservation): legacy per-session reads {legacy_elapsed:?} -> batch {batch_elapsed:?}"
    );
    assert!(
        batch_elapsed < legacy_elapsed,
        "the batch must beat per-session reads: legacy {legacy_elapsed:?} batch {batch_elapsed:?}"
    );
}
