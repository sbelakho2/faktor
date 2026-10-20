//! `session_listing_tests`: the single-statement contract of the durable
//! session listing and the monotonic-revision History snapshot. The native
//! `/native/sessions` page serves its DTO directly from ONE `list_sessions`
//! call (perf-wave follow-up: the historical handler ran the manager listing
//! — a whole-table read plus a journal verification per session — AND a
//! second batch listing per page). These probes pin the store half: one
//! statement per call, newest listing revision first, every row exactly once
//! at 5k sessions, and a page walk that cannot skip/duplicate under
//! same-millisecond updates, inserts, concurrent renames or hard deletes.

use super::*;

#[test]
fn list_sessions_is_one_statement_newest_first_at_5k_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    const N: u64 = 5_000;
    let mut created: Vec<SessionId> = Vec::with_capacity(N as usize);
    for i in 0..N {
        let row = store
            .create_session(ws, &format!("listing {i}"), "fake", "m")
            .unwrap();
        created.push(row.id);
    }

    let reads_before = store.reader_telemetry().reads;
    let rows = store.list_sessions(None).unwrap();
    let reads_after = store.reader_telemetry().reads;
    assert_eq!(
        reads_after - reads_before,
        1,
        "the listing is ONE statement (one read borrow), never two full reads"
    );
    assert_eq!(rows.len(), N as usize);

    // Newest listing revision first: the sequence is strictly descending.
    assert!(
        rows.windows(2)
            .all(|w| w[0].listing_revision > w[1].listing_revision),
        "the listing must be newest-listing-revision first"
    );
    assert!(
        rows.windows(2).all(|w| w[0].updated_ms >= w[1].updated_ms),
        "creation in order also leaves the presentation clock non-increasing"
    );

    // Every created session appears exactly once (no dropped or duplicated
    // rows, including the 5001st boundary of nothing — the whole table).
    let mut listed: Vec<u64> = rows.iter().map(|r| r.id.raw()).collect();
    let total = listed.len();
    listed.sort_unstable();
    listed.dedup();
    assert_eq!(listed.len(), total, "no duplicates");
    created.sort_by_key(|id| id.raw());
    let created_raw: Vec<u64> = created.iter().map(|id| id.raw()).collect();
    assert_eq!(listed, created_raw, "every created session is listed");

    // A bounded workspace-filtered read is the same single statement.
    let reads_before = store.reader_telemetry().reads;
    let scoped = store.list_sessions(Some(ws)).unwrap();
    let reads_after = store.reader_telemetry().reads;
    assert_eq!(reads_after - reads_before, 1);
    assert_eq!(scoped.len(), N as usize);
}

/// Report row 8: a History walk is a SNAPSHOT on the durable listing
/// revision. The old wall-clock bound could pass a session updated in the
/// SAME millisecond as the snapshot while that session moved to the newest
/// position, so a later page skipped it. Here every row shares `updated_ms =
/// 100` (including the mid-walk update), and the walk is bound to the
/// pre-update revision: the updated row is absent from the rest of THIS walk
/// and the next walk (fresh snapshot) lists it first.
#[test]
fn same_millisecond_update_during_a_walk_cannot_skip_or_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let mut ids = Vec::new();
    for i in 0..6 {
        ids.push(
            store
                .create_session(ws, &format!("walk {i}"), "fake", "m")
                .unwrap()
                .id,
        );
    }
    // Force ONE wall-clock millisecond for every row. Revisions stay the
    // durable 1..=6 created by the writes above.
    {
        let conn = store.raw_conn();
        conn.execute("UPDATE session SET updated_ms = 100", [])
            .unwrap();
    }
    let snapshot = store.session_listing_snapshot().unwrap();
    assert_eq!(snapshot.revision, 6, "six creations allocate six revisions");

    let mut seen: Vec<u64> = Vec::new();
    let mut before: Option<(i64, i64)> = None;
    let mut pages = 0;
    loop {
        let page = store
            .list_sessions_page_at(None, before, 2, Some(snapshot.revision))
            .unwrap();
        pages += 1;
        assert!(pages <= 10, "the walk must terminate");
        let has_more = page.len() > 2;
        let page = &page[..page.len().min(2)];
        seen.extend(page.iter().map(|row| row.id.raw()));
        if pages == 1 {
            // Mid-walk rename: its wall clock is forced back to the SAME
            // millisecond as the snapshot, which the legacy bound accepted.
            let last = ids[0];
            assert!(store
                .update_session_title(last, "updated mid-walk")
                .unwrap());
            store
                .raw_conn()
                .execute(
                    "UPDATE session SET updated_ms = 100 WHERE id = ?1",
                    params![last.raw() as i64],
                )
                .unwrap();
            let bumped = store.get_session(last).unwrap().unwrap();
            assert_eq!(
                bumped.updated_ms, 100,
                "the update landed in the SAME millisecond as the snapshot"
            );
            assert!(
                bumped.listing_revision > snapshot.revision,
                "the durable revision moved the row above the snapshot bound"
            );
        }
        if !has_more {
            break;
        }
        let last = page.last().unwrap();
        before = Some((last.listing_revision, last.id.raw() as i64));
    }
    // Sessions 6,5,4,3,2 are served exactly once; the same-millisecond
    // update is excluded from THIS snapshot walk (no skip, no duplicate).
    assert_eq!(seen, vec![6, 5, 4, 3, 2], "snapshot walk: {seen:?}");
    let mut fresh = Vec::new();
    let mut before = None;
    loop {
        let page = store.list_sessions_page(None, before, 2).unwrap();
        let has_more = page.len() > 2;
        let page = &page[..page.len().min(2)];
        fresh.extend(page.iter().map(|row| row.id.raw()));
        if !has_more {
            break;
        }
        let last = page.last().unwrap();
        before = Some((last.listing_revision, last.id.raw() as i64));
    }
    assert_eq!(
        fresh,
        vec![1, 6, 5, 4, 3, 2],
        "the next walk (fresh snapshot) lists the updated conversation first"
    );
}

/// Every ordering-relevant mutation advances the global revision exactly
/// once: state, lifecycle (unconditional and conditional), worktree/task
/// identity, title and journal events. A reader that snapshots before any of
/// them cannot serve the row below a later page.
#[test]
fn every_ordering_relevant_mutation_advances_the_revision() {
    use faktor_core::state::SessionLifecycle;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let row = store.create_session(ws, "mutations", "fake", "m").unwrap();
    let id = row.id;
    let mut revision = row.listing_revision;
    let mut next = |label: &str| {
        let after = store.get_session(id).unwrap().unwrap().listing_revision;
        assert_eq!(after, revision + 1, "{label} must allocate exactly one");
        revision = after;
    };

    store
        .set_session_state(id, AgentState::ReadyForNextTurn)
        .unwrap();
    next("set_session_state");
    store
        .set_session_lifecycle(id, SessionLifecycle::Suspended)
        .unwrap();
    next("set_session_lifecycle");
    assert!(store
        .set_lifecycle_if(id, SessionLifecycle::Suspended, SessionLifecycle::Open)
        .unwrap());
    next("set_lifecycle_if");
    store
        .adopt_session_identity(id, WorktreeId::new(7), TaskId::new(9))
        .unwrap();
    next("adopt_session_identity");
    assert!(store.update_session_title(id, "renamed").unwrap());
    next("update_session_title");
    store
        .append_event(id, None, EventKind::PhaseChanged, AgentState::Idle, 1, None)
        .unwrap();
    next("append_event");
}

/// Concurrent renames serialize on the single writer: every successful
/// rename gets its own strictly increasing durable revision, so the final
/// revision is exactly `snapshot + successful updates` and no two rows share
/// a revision.
#[test]
fn concurrent_title_updates_allocate_unique_monotonic_revisions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let target = store.create_session(ws, "target", "fake", "m").unwrap();
    let other = store.create_session(ws, "other", "fake", "m").unwrap();
    let initial = target.listing_revision;
    // The global sequence is shared: `other` was created after `target`.
    let before_updates = store.session_listing_snapshot_revision().unwrap();
    const THREADS: usize = 8;
    const PER_THREAD: usize = 25;
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let store = &store;
            let id = target.id;
            scope.spawn(move || {
                for i in 0..PER_THREAD {
                    assert!(store
                        .update_session_title(id, &format!("rename {t}/{i}"))
                        .unwrap());
                }
            });
        }
    });
    let updates = (THREADS * PER_THREAD) as i64;
    let after = store.get_session(target.id).unwrap().unwrap();
    assert!(before_updates >= initial);
    assert_eq!(
        after.listing_revision,
        before_updates + updates,
        "one durable revision per successful rename"
    );
    assert!(after.title.starts_with("rename "));
    let other_after = store.get_session(other.id).unwrap().unwrap();
    assert_ne!(
        after.listing_revision, other_after.listing_revision,
        "revisions are globally unique"
    );
    // A bounded walk still sees both rows exactly once, newest first.
    let page = store.list_sessions_page(None, None, 10).unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].id, target.id);
    assert_eq!(page[1].id, other.id);
}

/// Keyset stability under inserts and hard deletes: a row created after the
/// snapshot is absent from the walk, and deleting rows (even one already
/// served, which an offset cursor could not tolerate) never duplicates or
/// resurrects a row for the remaining pages.
#[test]
fn keyset_walk_is_stable_under_inserts_and_hard_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let mut ids = Vec::new();
    for i in 0..8 {
        ids.push(
            store
                .create_session(ws, &format!("keyset {i}"), "fake", "m")
                .unwrap()
                .id,
        );
    }
    let snapshot = store.session_listing_snapshot().unwrap();
    let first = store
        .list_sessions_page_at(None, None, 2, Some(snapshot.revision))
        .unwrap();
    let served: Vec<u64> = first[..2].iter().map(|row| row.id.raw()).collect();
    assert_eq!(served, vec![8, 7]);
    let mut before = Some((first[1].listing_revision, first[1].id.raw() as i64));

    // Insert after the snapshot: revision > snapshot, so it must not join
    // this walk.
    let inserted = store
        .create_session(ws, "inserted mid-walk", "fake", "m")
        .unwrap();
    assert!(inserted.listing_revision > snapshot.revision);
    // Hard-delete one served row (8) and one not-yet-served tail row (1).
    {
        let conn = store.raw_conn();
        for id in [ids[7], ids[0]] {
            conn.execute(
                "DELETE FROM event WHERE session_id = ?1",
                params![id.raw() as i64],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM session WHERE id = ?1",
                params![id.raw() as i64],
            )
            .unwrap();
        }
    }

    let mut seen: Vec<u64> = Vec::new();
    loop {
        let page = store
            .list_sessions_page_at(None, before, 2, Some(snapshot.revision))
            .unwrap();
        let has_more = page.len() > 2;
        let page = &page[..page.len().min(2)];
        seen.extend(page.iter().map(|row| row.id.raw()));
        if !has_more {
            break;
        }
        let last = page.last().unwrap();
        before = Some((last.listing_revision, last.id.raw() as i64));
    }
    assert_eq!(
        seen,
        vec![6, 5, 4, 3, 2],
        "no duplicate, no skipped live row, no insertion into the snapshot"
    );
    let fresh: Vec<u64> = store
        .list_sessions_page(None, None, 10)
        .unwrap()
        .iter()
        .map(|row| row.id.raw())
        .collect();
    assert_eq!(fresh, vec![9, 7, 6, 5, 4, 3, 2]);
}

/// v43 -> v44 migration: existing rows are backfilled to dense revisions in
/// stable id order and the sequence resumes above them, so the first write
/// after the upgrade can never collide with a legacy revision.
#[test]
fn migration_backfills_stable_revisions_and_seeds_the_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut ids = Vec::new();
    {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        for i in 0..5 {
            ids.push(
                store
                    .create_session(ws, &format!("legacy {i}"), "fake", "m")
                    .unwrap()
                    .id,
            );
        }
        let conn = store.raw_conn();
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_session_listing_revision_id;
             DROP INDEX IF EXISTS idx_session_workspace_listing_revision_id;
             ALTER TABLE session DROP COLUMN listing_revision;
             DROP TABLE listing_revision_seq;
             PRAGMA user_version = 43;",
        )
        .unwrap();
    }
    let store = Store::open(dir.path(), true).unwrap();
    let conn = store.raw_conn();
    let revisions: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT listing_revision FROM session ORDER BY id ASC")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    assert_eq!(revisions, vec![1, 2, 3, 4, 5], "dense, stable id order");
    let next: i64 = conn
        .query_row(
            "SELECT next_value FROM listing_revision_seq WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(next, 6, "the sequence resumes above the backfill");
    let ws = store.create_workspace("/w2").unwrap();
    let created = store
        .create_session(ws, "post-upgrade", "fake", "m")
        .unwrap();
    assert_eq!(
        created.listing_revision, 6,
        "the first post-upgrade write takes 6"
    );
    let page = store
        .list_sessions_page_at(None, None, 10, Some(created.listing_revision))
        .unwrap();
    let mut walked: Vec<u64> = page.iter().map(|row| row.id.raw()).collect();
    walked.sort_unstable();
    let mut expected: Vec<u64> = ids.iter().map(|id| id.raw()).collect();
    expected.push(created.id.raw());
    expected.sort_unstable();
    assert_eq!(walked, expected, "migrated rows remain page-readable");
}

/// ([fault]) 1M-session History walk probe:
/// `cargo test -p faktor-store fault_history_walk -- --ignored --nocapture`.
/// The per-workspace page must ride the `(workspace, listing_revision, id)`
/// index (no whole-workspace sort), and 500 keyset pages must be
/// ordered/unique.
#[test]
#[ignore = "[fault] 1M-session History walk probe; run explicitly with --ignored"]
fn fault_history_walk_is_keyset_bounded_at_one_million_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let control = store.create_workspace("/control").unwrap();
    let conn = store.raw_conn();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    {
        let mut stmt = conn
            .prepare(
                "INSERT INTO session(id, workspace_id, title, provider, model, state, lifecycle,
                    created_ms, updated_ms, worktree_id, task_id, listing_revision)
                 VALUES (?1, ?2, 't', 'p', 'm', '\"idle\"', 'open', ?3, ?3, 1, 1, ?1)",
            )
            .unwrap();
        for id in 1..=1_000_000i64 {
            stmt.execute(params![id, ws.raw() as i64, id]).unwrap();
        }
        for id in 1..=200i64 {
            stmt.execute(params![2_000_000 + id, control.raw() as i64, id])
                .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();

    let started = std::time::Instant::now();
    let snapshot = store.session_listing_snapshot().unwrap();
    let snapshot_elapsed = started.elapsed();
    assert!(snapshot.revision >= 1_000_000);
    assert!(snapshot.wall_ms > 0);

    let mut before: Option<(i64, i64)> = None;
    let mut seen = 0usize;
    let mut last: Option<(i64, i64)> = None;
    let mut slowest = std::time::Duration::ZERO;
    for _ in 0..500 {
        let started = std::time::Instant::now();
        let page = store
            .list_sessions_page_at(Some(ws), before, 200, Some(snapshot.revision))
            .unwrap();
        slowest = slowest.max(started.elapsed());
        assert_eq!(page.len(), 201, "full page plus the probe row");
        let page = &page[..200];
        for row in page {
            if let Some(previous) = last {
                assert!(
                    (row.listing_revision, row.id.raw() as i64) < previous,
                    "keyset order must strictly descend"
                );
            }
            last = Some((row.listing_revision, row.id.raw() as i64));
        }
        seen += page.len();
        let tail = page.last().unwrap();
        before = Some((tail.listing_revision, tail.id.raw() as i64));
    }
    assert_eq!(seen, 100_000);
    eprintln!(
        "[fault] 1M-session History: snapshot {snapshot_elapsed:?}, 500 pages in {slowest:?} worst page"
    );
    assert!(
        slowest < std::time::Duration::from_millis(20),
        "a 200-row page over 1M sessions took {slowest:?}"
    );

    let control_page = store
        .list_sessions_page_at(Some(control), None, 200, Some(snapshot.revision))
        .unwrap();
    assert_eq!(
        control_page.len(),
        200,
        "the small workspace remains page-bounded"
    );
}
