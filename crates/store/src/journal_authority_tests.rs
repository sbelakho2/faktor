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
