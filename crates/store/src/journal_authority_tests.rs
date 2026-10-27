//! `ledger` journal-authority tests: the bounded per-session authority
//! (`journal_session_problems`) and the doctor sweep derived from it, plus
//! the legacy gapless/cursor event invariants they build on.

use super::tests::seed_only_event_seq;
use super::*;

pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), true).unwrap();
    (dir, s)
}

/// A journal append must survive a foreign writer that commits between the
/// append's seq read and its INSERT. The pre-fix DEFERRED transaction
/// produced SQLITE_BUSY_SNAPSHOT (surfaced as "database is locked") exactly
/// during live settlement/acceptance; IMMEDIATE takes the write lock first.
#[test]
fn event_and_typed_ledger_appends_wait_out_a_foreign_write_transaction() {
    use rusqlite::Connection;
    use std::time::Duration;

    let (dir, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let session = store.create_session(ws, "lock", "p", "m").unwrap();
    let db_path = dir.path().join("faktor-plus.db");

    let blocker = Connection::open(&db_path).unwrap();
    blocker.busy_timeout(Duration::from_secs(5)).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        blocker.execute_batch("COMMIT").unwrap();
    });
    store
        .append_event(
            session.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .expect("a foreign short write must not turn into SQLITE_BUSY_SNAPSHOT");
    handle.join().unwrap();

    let blocker = Connection::open(&db_path).unwrap();
    blocker.busy_timeout(Duration::from_secs(5)).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        blocker.execute_batch("COMMIT").unwrap();
    });
    store
        .append_ledger_entry(
            session.id,
            "test_recorded",
            1,
            serde_json::json!({"ok": true}),
        )
        .expect("a foreign short write must not fail the typed ledger");
    handle.join().unwrap();
}

/// The doctor sweep must flag EXACTLY the torn session at scale and never
/// spill an issue onto a healthy neighbour.
#[test]
fn journal_sweep_flags_exactly_the_torn_session_and_never_its_neighbours() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let healthy: Vec<_> = (0..32)
        .map(|i| {
            store
                .create_session(ws, &format!("healthy-{i}"), "p", "m")
                .unwrap()
        })
        .collect();
    let torn = store.create_session(ws, "torn", "p", "m").unwrap();
    store
        .append_event(
            torn.id,
            None,
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "DELETE FROM event WHERE session_id = ?1 AND seq = 1",
            params![torn.id.raw() as i64],
        )
        .unwrap();
        conn.execute(
            "UPDATE event SET seq = 1 WHERE session_id = ?1 AND seq = 2",
            params![torn.id.raw() as i64],
        )
        .unwrap();
    }
    let issues = store.journal_consistency_issues().unwrap();
    // Catches a sweep that crosses session boundaries or under/over-reports.
    assert_eq!(issues.len(), 2, "{issues:?}");
    for issue in &issues {
        assert!(
            issue.contains(&format!("session {}:", torn.id.raw())),
            "issue must name the torn session: {issue}"
        );
    }
    for healthy in &healthy {
        let needle = format!("session {}:", healthy.id.raw());
        assert!(
            issues.iter().all(|issue| !issue.contains(&needle)),
            "healthy session {needle} must not be flagged: {issues:?}"
        );
    }
}

#[test]
fn journal_sequences_are_gapless_under_concurrent_append() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let session = store.create_session(ws, "c", "p", "m").unwrap();
    let sid = session.id;
    let store = std::sync::Arc::new(store);
    let mut handles = vec![];
    for t in 0..8 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                store
                    .append_event(
                        sid,
                        Some(OpId::new(1 + t * 100 + i)),
                        EventKind::ModelChunkReceived,
                        AgentState::Streaming,
                        now_ms(),
                        Some(serde_json::json!({"i": i})),
                    )
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let events = store.events_range(sid, 1, None).unwrap();
    // SessionCreated(1) + 400 chunks = 401 events, seq 1..=401 gapless.
    assert_eq!(events.len(), 401);
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.seq.raw(), (i + 1) as u64, "gap at {i}");
    }
    // Resume cursor semantics: events_after(seq 400) returns exactly 1.
    let tail = store.events_after(sid, EventSeq::new(400)).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq.raw(), 401);
}

#[test]
fn event_page_cursors_and_limits_never_wrap_the_signed_domain() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    // Cursors at/above the top of the signed seq domain can never be
    // followed by a durable seq: empty pages, never a wrapped replay.
    let after_max = store.events_after(s.id, EventSeq::new(u64::MAX)).unwrap();
    assert!(after_max.is_empty());
    let after_edge = store
        .events_after(s.id, EventSeq::new(i64::MAX as u64))
        .unwrap();
    assert!(after_edge.is_empty());
    let range_above = store.events_range(s.id, i64::MAX as u64 + 1, None).unwrap();
    assert!(range_above.is_empty());
    let versioned_above = store
        .events_versioned_range(s.id, i64::MAX as u64 + 1, None)
        .unwrap();
    assert!(versioned_above.is_empty());
    // The largest signed seq is still a real cursor: it includes the
    // event stored AT the boundary.
    seed_only_event_seq(&store, s.id, i64::MAX);
    let boundary = store.events_range(s.id, i64::MAX as u64, None).unwrap();
    assert_eq!(boundary.len(), 1);
    let tail = store
        .events_after(s.id, EventSeq::new(i64::MAX as u64 - 1))
        .unwrap();
    assert_eq!(tail.len(), 1);
    // An explicit limit above the signed range refuses typed; it must
    // never bind a negative LIMIT (SQLite's unbounded sentinel).
    for limit in [i64::MAX as u64 + 1, u64::MAX] {
        assert!(
            matches!(
                store.events_range(s.id, 1, Some(limit)),
                Err(StoreError::Oversized(_))
            ),
            "limit {limit} must refuse typed Oversized"
        );
        assert!(
            matches!(
                store.events_versioned_range(s.id, 1, Some(limit)),
                Err(StoreError::Oversized(_))
            ),
            "versioned limit {limit} must refuse typed Oversized"
        );
    }
    // i64::MAX itself is the largest legal explicit limit; `None` stays
    // the honest unbounded page (the seeded boundary event).
    assert_eq!(
        store
            .events_range(s.id, 1, Some(i64::MAX as u64))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.events_range(s.id, 1, None).unwrap().len(), 1);
}
#[test]
fn journal_consistency_flags_later_event_masquerading_as_seq_one() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .append_event(
            s.id,
            None,
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    assert!(store.journal_consistency_issues().unwrap().is_empty());
    // Torn create: the original session_created seed is lost and the
    // LATER event is renumbered into its seq-1 slot. The journal is
    // numerically gapless (count == max == 1), so only the first-event
    // kind/state contract can flag it.
    let conn = store.raw_conn();
    conn.execute(
        "DELETE FROM event WHERE session_id = ?1 AND seq = 1",
        params![s.id.raw() as i64],
    )
    .unwrap();
    let renumbered = conn
        .execute(
            "UPDATE event SET seq = 1 WHERE session_id = ?1 AND seq = 2",
            params![s.id.raw() as i64],
        )
        .unwrap();
    assert_eq!(renumbered, 1, "the hostile renumber must land");
    drop(conn);
    let issues = store.journal_consistency_issues().unwrap();
    assert_eq!(issues.len(), 2, "kind AND state must both flag: {issues:?}");
    for needle in ["session_created", "not idle"] {
        assert!(
            issues.iter().any(|i| i.contains(needle)),
            "{needle} must be flagged: {issues:?}"
        );
    }
    for issue in &issues {
        assert!(
            issue.contains(&format!("session {}", s.id.raw())),
            "each issue must name the session: {issue}"
        );
    }
}

#[test]
fn journal_consistency_flags_backwards_timestamps() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let seed_ts = store.events_range(s.id, 1, None).unwrap()[0].ts_ms;
    store
        .append_event(
            s.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            seed_ts + 10,
            None,
        )
        .unwrap();
    assert!(store.journal_consistency_issues().unwrap().is_empty());
    store
        .raw_conn()
        .execute(
            "UPDATE event SET ts_ms = ?2 WHERE session_id = ?1 AND seq = 2",
            params![s.id.raw() as i64, seed_ts - 1],
        )
        .unwrap();
    let issues = store.journal_consistency_issues().unwrap();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0].contains("non-decreasing"),
        "the backward step must be flagged: {issues:?}"
    );
}

#[test]
fn journal_session_problems_flag_each_open_invariant() {
    // Adversarial: every way the bounded per-session authority must
    // refuse a session open — and a healthy journal that must stay
    // clean. The authority is exercised directly; the session layer's
    // typed mapping is asserted in faktor-session.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();

    let healthy = store.create_session(ws, "healthy", "p", "m").unwrap();
    store
        .append_event(
            healthy.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    assert!(store
        .journal_session_problems(healthy.id, JournalDepth::Open)
        .unwrap()
        .is_empty());
    assert!(store
        .journal_session_problems(healthy.id, JournalDepth::Deep)
        .unwrap()
        .is_empty());

    // (d) a session row whose journal vanished entirely (torn creation).
    let torn = store.create_session(ws, "torn", "p", "m").unwrap();
    store
        .raw_conn()
        .execute(
            "DELETE FROM event WHERE session_id = ?1",
            params![torn.id.raw() as i64],
        )
        .unwrap();
    let problems = store
        .journal_session_problems(torn.id, JournalDepth::Open)
        .unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("no events"), "{problems:?}");
    assert!(
        problems[0].contains(&format!("session {}", torn.id.raw())),
        "the problem must name the session: {problems:?}"
    );

    // A durable deletion tombstone keeps the row but purges the journal:
    // that is an expected closed state, not torn creation.
    let deleted = store.create_session(ws, "deleted", "p", "m").unwrap();
    store
        .purge_session_content(deleted.id, "test deletion")
        .unwrap();
    assert!(
        store
            .journal_session_problems(deleted.id, JournalDepth::Open)
            .unwrap()
            .is_empty(),
        "a tombstoned session must not be reported as torn creation"
    );

    // (a) the seq-1 seed slot was supplanted by a later event, renumbered
    // into it: numerically gapless, visible only via the seed contract.
    let supplanted = store.create_session(ws, "supplanted", "p", "m").unwrap();
    store
        .append_event(
            supplanted.id,
            None,
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "DELETE FROM event WHERE session_id = ?1 AND seq = 1",
            params![supplanted.id.raw() as i64],
        )
        .unwrap();
        conn.execute(
            "UPDATE event SET seq = 1 WHERE session_id = ?1 AND seq = 2",
            params![supplanted.id.raw() as i64],
        )
        .unwrap();
    }
    let problems = store
        .journal_session_problems(supplanted.id, JournalDepth::Open)
        .unwrap();
    assert_eq!(
        problems.len(),
        2,
        "kind AND first state must both flag: {problems:?}"
    );
    for needle in ["session_created", "not idle"] {
        assert!(
            problems.iter().any(|p| p.contains(needle)),
            "{needle} must be flagged: {problems:?}"
        );
    }

    // (b) a gap: the journal is 1,3.
    let gapped = store.create_session(ws, "gapped", "p", "m").unwrap();
    for _ in 0..2 {
        store
            .append_event(
                gapped.id,
                None,
                EventKind::ModelChunkReceived,
                AgentState::Streaming,
                now_ms(),
                None,
            )
            .unwrap();
    }
    store
        .raw_conn()
        .execute(
            "DELETE FROM event WHERE session_id = ?1 AND seq = 2",
            params![gapped.id.raw() as i64],
        )
        .unwrap();
    let problems = store
        .journal_session_problems(gapped.id, JournalDepth::Open)
        .unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("1..=3"), "{problems:?}");

    // (c) the tail event's landing state disagrees with the projection.
    let drifted = store.create_session(ws, "drifted", "p", "m").unwrap();
    store
        .append_event(
            drifted.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE session SET state = '\"idle\"' WHERE id = ?1",
            params![drifted.id.raw() as i64],
        )
        .unwrap();
    let problems = store
        .journal_session_problems(drifted.id, JournalDepth::Open)
        .unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("disagrees"), "{problems:?}");

    // (d) a journal without its session row (the event FK makes this
    // unreachable through normal mutation; the authority still refuses).
    let orphan = store.create_session(ws, "orphan", "p", "m").unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "DELETE FROM event WHERE session_id = ?1",
            params![orphan.id.raw() as i64],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM session WHERE id = ?1",
            params![orphan.id.raw() as i64],
        )
        .unwrap();
    }
    let problems = store
        .journal_session_problems(orphan.id, JournalDepth::Open)
        .unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("session row is missing"),
        "{problems:?}"
    );
}

#[test]
fn journal_session_problems_tolerate_only_the_admission_reservation() {
    // The queue admission reserves the next journal seq for the
    // materialized message and moves the row to `preparing` before the
    // matching event lands: the crash residue of that window is legal
    // and must open, but only while the reservation artifact exists.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .enqueue_prompt(
            s.id,
            OpId::new(1),
            "queued",
            &[],
            None,
            None,
            None,
            now_ms(),
        )
        .unwrap();
    let (admitted, _event_seq) = store
        .admit_queue_head(s.id, &["idle"], "preparing")
        .unwrap()
        .expect("the queued head is admissible");
    assert!(store
        .journal_session_problems(s.id, JournalDepth::Open)
        .unwrap()
        .is_empty());
    // Removing the reservation artifact (the message at the reserved
    // seq) turns the same projection into drift the authority flags.
    store
        .raw_conn()
        .execute(
            "DELETE FROM message WHERE session_id = ?1 AND seq = ?2",
            params![s.id.raw() as i64, admitted.message_seq],
        )
        .unwrap();
    let problems = store
        .journal_session_problems(s.id, JournalDepth::Open)
        .unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("disagrees"), "{problems:?}");
}

#[test]
fn journal_session_problems_flags_timestamps_only_at_deep() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let seed_ts = store.events_range(s.id, 1, None).unwrap()[0].ts_ms;
    store
        .append_event(
            s.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            seed_ts + 10,
            None,
        )
        .unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE event SET ts_ms = ?2 WHERE session_id = ?1 AND seq = 2",
            params![s.id.raw() as i64, seed_ts - 1],
        )
        .unwrap();
    // Open is the bounded projection contract: timestamps are not part
    // of it, so a backwards clock alone does not refuse an open.
    assert!(store
        .journal_session_problems(s.id, JournalDepth::Open)
        .unwrap()
        .is_empty());
    let problems = store
        .journal_session_problems(s.id, JournalDepth::Deep)
        .unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("non-decreasing"), "{problems:?}");
    // The doctor sweep derives from the same authority, Deep per session.
    assert_eq!(store.journal_consistency_issues().unwrap(), problems);
}

#[test]
fn journal_session_problems_report_an_undecodable_tail_state_only_at_deep() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .append_event(
            s.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE event SET state = '\"not_a_state\"' WHERE session_id = ?1 AND seq = 2",
            params![s.id.raw() as i64],
        )
        .unwrap();
    // Open compares DECODED projections; an undecodable event state is the
    // strict readers' (journal replay / SSE / paged reads) typed refusal.
    assert!(store
        .journal_session_problems(s.id, JournalDepth::Open)
        .unwrap()
        .is_empty());
    let problems = store
        .journal_session_problems(s.id, JournalDepth::Deep)
        .unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("not a valid AgentState"),
        "{problems:?}"
    );
}

#[test]
fn journal_consistency_issues_is_the_per_session_deep_authority_swept() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let a = store.create_session(ws, "a", "p", "m").unwrap();
    let b = store.create_session(ws, "b", "p", "m").unwrap();
    store
        .append_event(
            a.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    store
        .append_event(
            b.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    // b: the projection drifted AND the timestamps step back — two
    // findings for one session, and none for a.
    store
        .raw_conn()
        .execute(
            "UPDATE session SET state = '\"idle\"' WHERE id = ?1",
            params![b.id.raw() as i64],
        )
        .unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE event SET ts_ms = 0 WHERE session_id = ?1 AND seq = 2",
            params![b.id.raw() as i64],
        )
        .unwrap();
    let mut expected = store
        .journal_session_problems(a.id, JournalDepth::Deep)
        .unwrap();
    expected.extend(
        store
            .journal_session_problems(b.id, JournalDepth::Deep)
            .unwrap(),
    );
    assert!(!expected.is_empty(), "the hostile session must flag");
    assert_eq!(store.journal_consistency_issues().unwrap(), expected);
}

#[test]
fn journal_session_problems_are_bounded_on_a_large_journal() {
    const EVENTS: u64 = 10_000;
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "large", "p", "m").unwrap();
    for i in 0..EVENTS {
        store
            .append_event(
                s.id,
                Some(OpId::new(i + 1)),
                EventKind::ModelChunkReceived,
                AgentState::Streaming,
                now_ms(),
                Some(serde_json::json!({"i": i})),
            )
            .unwrap();
    }
    assert_eq!(
        store.last_event_seq(s.id).unwrap().unwrap().raw(),
        EVENTS + 1,
        "seed event + appends are one gapless span"
    );
    assert!(store
        .journal_session_problems(s.id, JournalDepth::Open)
        .unwrap()
        .is_empty());
    assert!(store
        .journal_session_problems(s.id, JournalDepth::Deep)
        .unwrap()
        .is_empty());
    // Hostile timestamps on the SAME 10k journal: the adjacent-inversion
    // aggregate reports ONE bounded finding, never one per pair — the
    // authority never materializes or walks the rows.
    store
        .raw_conn()
        .execute(
            "UPDATE event SET ts_ms = -seq WHERE session_id = ?1",
            params![s.id.raw() as i64],
        )
        .unwrap();
    let problems = store
        .journal_session_problems(s.id, JournalDepth::Deep)
        .unwrap();
    assert_eq!(
        problems.len(),
        1,
        "one bounded finding on a 10k journal: {problems:?}"
    );
    assert!(problems[0].contains("non-decreasing"), "{problems:?}");
    // The same journal still opens (timestamps are Deep-only).
    assert!(store
        .journal_session_problems(s.id, JournalDepth::Open)
        .unwrap()
        .is_empty());
}

/// Mutation witness for INV-QUEUE-ADMISSION-FILES: the file-array predicate
/// must use json_each's own `type` column (never `json_type(value)`, which
/// re-parses the de-quoted text) behind the json_valid/json_type guards; the
/// same checker detects the old wrapping form.
fn queue_file_array_predicate_present(source: &str) -> bool {
    source.contains("WHEN json_valid(?1) = 0 THEN 0")
        && source.contains("WHEN json_type(?1) <> 'array' THEN 0")
        && source.contains("SELECT 1 FROM json_each(?1) WHERE type <> 'text'")
        && !source.contains("json_type(value)")
}

#[test]
fn queue_file_array_predicate_is_pinned_against_mutation() {
    const SOURCE: &str = include_str!("ledger.rs");
    assert!(
        queue_file_array_predicate_present(SOURCE),
        "the queue file-array predicate changed: it must use json_each's type column"
    );
    let mutated = SOURCE.replace(
        "SELECT 1 FROM json_each(?1) WHERE type <> 'text'",
        "SELECT 1 FROM json_each(?1) WHERE json_type(value) <> 'text'",
    );
    assert!(
        !queue_file_array_predicate_present(&mutated),
        "the witness must detect the json_type(value) wrap"
    );
}

/// The open/doctor checks must see ONE snapshot. The checks read the row
/// projection, the gap aggregate and the tail event separately, so without a
/// read transaction a concurrent append can tear the view (row read before
/// the append, tail read after) and report a false projection disagreement
/// on a legitimately in-flight transition. While a writer hammers appends,
/// validation must never report that disagreement.
///
/// Load-robust by construction (the old shape raced an UNBOUNDED writer
/// against a fixed validation count: a descheduled reader let the journal
/// grow without bound and the race could go unattested):
/// - the writer's volume is bounded (`WRITER_TARGET`), so the journal stays
///   small no matter how the host schedules the two threads;
/// - the validations only start after the writer has durably interleaved
///   `MIN_INTERLEAVED` appends (observable condition) — a starved writer can
///   never make this test vacuously green;
/// - the ceiling only fails a genuinely wedged host or writer.
///
/// Every validation still asserts the invariant (teeth unchanged).
#[test]
fn journal_open_validation_never_tears_under_concurrent_append() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// Bounded writer volume: enough to interleave with the validations
    /// under any scheduling, small enough that the journal stays a few MB
    /// when the reader is descheduled.
    const WRITER_TARGET: u64 = 5_000;
    /// Validations to run while the writer is (at least initially) racing.
    const MIN_VALIDATIONS: u64 = 2_000;
    /// Completed appends that must be observed before validations count:
    /// without them the race was never exercised and the run is refused.
    const MIN_INTERLEAVED: u64 = 100;
    /// Deadlock-only ceiling for the interleaving gate.
    const DEADLINE: Duration = Duration::from_secs(120);

    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let session = store.create_session(ws, "c", "p", "m").unwrap();
    let sid = session.id;
    let store = std::sync::Arc::new(store);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let appended = std::sync::Arc::new(AtomicU64::new(0));
    let writer = {
        let store = store.clone();
        let stop = stop.clone();
        let appended = appended.clone();
        std::thread::spawn(move || {
            let states = [
                AgentState::BuildingContext,
                AgentState::WaitingForModel,
                AgentState::Streaming,
            ];
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) && appended.load(Ordering::Relaxed) < WRITER_TARGET
            {
                let state = states[(i % 3) as usize];
                store
                    .append_event(
                        sid,
                        Some(OpId::new(1_000 + i)),
                        EventKind::PhaseChanged,
                        state,
                        now_ms(),
                        None,
                    )
                    .unwrap();
                appended.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        })
    };
    // Interleaving gate: wait (bounded, observable) until the writer has
    // actually appended against the store the validations will read.
    let deadline = Instant::now() + DEADLINE;
    while appended.load(Ordering::Relaxed) < MIN_INTERLEAVED {
        assert!(
            Instant::now() < deadline,
            "the append writer never interleaved ({} appends); wedged host or writer",
            appended.load(Ordering::Relaxed)
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    for _ in 0..MIN_VALIDATIONS {
        let problems = store
            .journal_session_problems(sid, JournalDepth::Open)
            .unwrap();
        for problem in &problems {
            assert!(
                !problem.contains("disagrees with the session row state"),
                "a torn read reported a false projection disagreement: {problem}"
            );
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
}
