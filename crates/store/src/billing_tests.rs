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

/// Adversarial servability probe (independent perf-wave audit): the
/// single-session read accepts `prompt_prefix_hash` ONLY as a 32-byte BLOB
/// (`Vec<u8>` decode); SQLite `length()` counts CHARACTERS on a TEXT value,
/// so a hostile 32-character TEXT hash must still land in `unreadable`
/// (per-session fallback reproduces the legacy skip) — never be served as a
/// servable aggregate the route would count.
#[test]
fn session_usage_batch_refuses_a_text_stored_prefix_hash() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
    for op in 1..=2u64 {
        store
            .record_provider_call_with_prefix(
                sid,
                OpId::new(op),
                "fake",
                "m",
                "completed",
                Some(10),
                Some(5),
                None,
                Some([op as u8; 32]),
                Some(7),
                Some(0.5),
            )
            .unwrap();
    }
    // Tamper behind the API's back: the newest hash becomes 32-CHARACTER
    // TEXT (not a 32-byte BLOB). `length()` still reports 32.
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE provider_call SET prompt_prefix_hash = ?1
             WHERE id = (SELECT MAX(id) FROM provider_call WHERE session_id = ?2)",
            rusqlite::params!["abcdefghijklmnopqrstuvwxyz012345", sid.raw() as i64],
        )
        .unwrap();
    }
    assert!(
        store.provider_call_prefix_rows(sid).is_err(),
        "the per-row read must refuse TEXT storage for the hash"
    );
    let batch = store.session_usage_aggregates(&[sid]).unwrap();
    let agg = batch
        .rows
        .iter()
        .find(|a| a.session_id == sid)
        .expect("the session has provider_call rows");
    assert!(
        !agg.prefix_servable,
        "the batch must NOT serve prefix aggregates the per-row read refuses: {agg:?}"
    );
    assert!(
        batch.unreadable.is_empty(),
        "a per-row refusal is not a chunk-level failure"
    );
}

/// Adversarial probe (independent perf-wave audit): `handle.memory_facts()`
/// decodes EVERY fact row (all kinds) and fails the whole per-session read
/// when one row is unreadable (non-TEXT storage), which the legacy route
/// turns into a per-session skip. The batch read filters `kind = 'usage'`
/// in SQL, so a hostile non-usage row must NOT be able to hide behind the
/// filter: it must land in `unreadable` exactly like the legacy failure.
#[test]
fn usage_facts_batch_refuses_sessions_with_unreadable_non_usage_rows() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
    store
        .upsert_memory_fact(sid, "usage", "budget", "100")
        .unwrap();
    store
        .upsert_memory_fact(sid, "usage", "spent", "7")
        .unwrap();
    // A hostile row of a DIFFERENT kind whose `kind` column holds a BLOB:
    // the per-session read fails loudly, so the route skips the session.
    {
        let conn = store.raw_conn();
        conn.execute(
            "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms)
             VALUES (?1, x'00', 'poison', 'x', 1)",
            rusqlite::params![sid.raw() as i64],
        )
        .unwrap();
    }
    assert!(
        store.memory_facts(sid).is_err(),
        "the per-session read must fail on the BLOB kind"
    );
    let batch = store.usage_facts_by_session(&[sid]).unwrap();
    assert!(
        batch.unreadable.contains(&sid),
        "the batch must fall back for the session the legacy read fails: {:?}",
        batch.rows
    );
    assert!(
        batch.rows.iter().all(|(s, _, _)| *s != sid),
        "the failing session must never be served from the batch: {:?}",
        batch.rows
    );
    // A healthy session stays servable on its own.
    let healthy = store.create_session(ws, "t2", "p", "m").unwrap().id;
    store
        .upsert_memory_fact(healthy, "usage", "budget", "5")
        .unwrap();
    let batch = store.usage_facts_by_session(&[healthy]).unwrap();
    assert!(batch.unreadable.is_empty());
    assert_eq!(
        batch.rows,
        vec![(healthy, "budget".to_string(), "5".to_string())]
    );
}

/// Chunk-boundary probe (independent perf-wave audit): 1,001 sessions cross
/// two 500-id chunks plus a one-id tail. Every aggregate must equal the
/// per-session computation exactly, duplicate ids must not double count, and
/// a nonexistent id must stay absent.
#[test]
fn session_usage_batch_chunk_boundaries_match_per_session_reads() {
    const N: u64 = 1_001;
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let mut ids = Vec::with_capacity(N as usize + 1);
    for i in 0..N {
        let s = store
            .create_session(ws, &format!("t{i}"), "fake", "m")
            .unwrap();
        store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(i + 1),
                "fake",
                "m",
                "completed",
                Some(i % 5),
                Some(i % 3),
                None,
                Some([(i % 251) as u8; 32]),
                Some(i % 11),
                if i % 2 == 0 { Some(0.25) } else { None },
            )
            .unwrap();
        ids.push(s.id);
    }
    // Duplicate the first id and append ids from another workspace: the
    // batch must answer each real session exactly once per report row.
    let ghost = store.create_session(ws, "ghost", "fake", "m").unwrap().id;
    let mut probe_ids = ids.clone();
    probe_ids.push(ids[0]);
    probe_ids.push(ghost);
    let batch = store.session_usage_aggregates(&probe_ids).unwrap();
    assert!(batch.unreadable.is_empty());
    for sid in &ids {
        let expected_tokens = store.session_usage_tokens(*sid).unwrap();
        let expected_prefix = store.provider_call_prefix_rows(*sid).unwrap();
        let agg = batch
            .rows
            .iter()
            .find(|a| a.session_id == *sid)
            .expect("served session");
        assert_eq!(agg.tokens, expected_tokens, "tokens for {sid:?}");
        assert_eq!(agg.prefix_rows, expected_prefix.len() as u64);
        assert_eq!(
            agg.prefix_tokens,
            expected_prefix
                .iter()
                .map(|r| r.prompt_tokens as u64)
                .sum::<u64>()
        );
        assert_eq!(
            agg.prefix_stability_observations,
            expected_prefix
                .iter()
                .filter(|r| r.prefix_stability.is_some())
                .count() as u64
        );
        assert!(agg.prefix_servable);
    }
    // Sessions without provider_call rows are absent (zero aggregates).
    let no_rows = store.create_session(ws, "empty", "fake", "m").unwrap().id;
    let empty_batch = store.session_usage_aggregates(&[no_rows]).unwrap();
    assert!(empty_batch.rows.is_empty());
    assert!(empty_batch.unreadable.is_empty());
}

/// Reservation batch probe (independent perf-wave audit): duplicate pairs
/// must not double-serve rows (a fold would double count), missing pairs stay
/// absent, and the cross-product `IN (sessions) AND IN (tasks)` shape must
/// never serve a pair the caller did not ask for.
#[test]
fn cost_reservations_batch_never_double_serves_duplicate_pairs() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s1 = store.create_session(ws, "t1", "p", "m").unwrap();
    let s2 = store.create_session(ws, "t2", "p", "m").unwrap();
    let t1 = TaskId::new(1);
    let t2 = TaskId::new(2);
    for (sid, task) in [(s1.id, t1), (s2.id, t2)] {
        store
            .upsert_task(&crate::TaskRow {
                task_id: task,
                session_id: sid,
                goal: "g".into(),
                acceptance_criteria: vec![],
                plan: vec![],
                attachments: vec![],
                contract: Default::default(),
                max_tokens: None,
                max_turns: None,
                spent_tokens: 0,
                spent_turns: 0,
                state: crate::TaskState::Running,
                revision: crate::TaskRevision::new(1),
                created_ms: 1,
                updated_ms: 1,
            })
            .unwrap();
        let crate::CostReserveOutcome::Granted(r) =
            store.cost_reserve(sid, task, OpId::new(1), 100, 1).unwrap()
        else {
            panic!("reserve must be granted");
        };
        store
            .cost_settle(r, 60, Some(60), Some(55), None, 2)
            .unwrap();
    }
    // Duplicate the SAME pair at positions that straddle the 500 chunk
    // boundary: chunk 0 gets one copy, chunk 1 gets the other.
    let mut pairs = vec![(s1.id, t1)];
    pairs.push((s2.id, t2));
    // Pad to 501 with the ghost pair first so the duplicate lands in a
    // different chunk than its twin.
    for _ in 0..499 {
        pairs.push((s2.id, t1));
    }
    pairs.push((s1.id, t1));
    assert_eq!(pairs.len(), 502);
    let batch = store.cost_reservations_for_tasks(&pairs, 10_000).unwrap();
    let served: Vec<_> = batch
        .rows
        .iter()
        .filter(|r| r.session_id == s1.id && r.task_id == t1)
        .collect();
    assert_eq!(
        served.len(),
        1,
        "the duplicated pair must be served exactly once across chunks: {served:?}"
    );
    // Never-served pairs requested as (s2, t1) stay absent for s2's real
    // scope; the caller only consumes rows keyed by its pairs.
    let ghost = store
        .cost_reservations_for_tasks(&[(s1.id, t2)], 10_000)
        .unwrap();
    assert!(
        ghost.rows.is_empty(),
        "a pair with no rows stays absent: {:?}",
        ghost.rows
    );
}
