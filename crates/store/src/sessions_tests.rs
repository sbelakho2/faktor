// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
use super::*;

pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), true).unwrap();
    (dir, s)
}

#[test]
fn loop_signal_survives_reopen_and_trips_at_threshold() {
    // Spec §28 durable signals: crash between turns must NOT lose the
    // count — the third identical failure after a daemon restart still
    // trips.
    let dir = tempfile::tempdir().unwrap();
    let ws: WorkspaceId;
    let sid: SessionId;
    {
        let store = Store::open(dir.path().join("store"), true).unwrap();
        ws = store.create_workspace("/w").unwrap();
        let row = store.create_session(ws, "t", "p", "m").unwrap();
        sid = row.id;
        for i in 1..=2 {
            let tripped = store
                .bump_loop_signal(sid, "fail run_command", 3, i * 1000)
                .unwrap();
            assert!(!tripped, "count {i} must not trip yet");
        }
        // Reopen happens when the store drops (crash simulation).
    }
    {
        let store = Store::open(dir.path().join("store"), true).unwrap();
        assert!(
            store
                .bump_loop_signal(sid, "fail run_command", 3, 3000)
                .unwrap(),
            "third identical failure after a restart must trip"
        );
        // Progress clears everything.
        store.reset_loop_signals(sid).unwrap();
        assert!(store.loop_signal_counts(sid).unwrap().is_empty());
    }
}

#[test]
fn checkpoints_dedup_by_hash_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        for i in 0..5 {
            store
                .put_checkpoint(s.id, i, "a.rs", "hash-before", "hash-after", None)
                .unwrap();
        }
        // v3: the after-blob hash roundtrips when recorded.
        store
            .put_checkpoint(s.id, 5, "b.rs", "b1", "a1", Some("cas-after-blob"))
            .unwrap();
        s.id
    };
    let store = Store::open(dir.path(), true).unwrap();
    let cps = store.checkpoints_of(session_id).unwrap();
    assert_eq!(cps.len(), 6);
    assert_eq!(cps[0].sequence, 0);
    assert_eq!(cps[4].after_hash, "hash-after");
    assert_eq!(cps[4].after_cas_hash, None);
    assert_eq!(cps[5].after_cas_hash.as_deref(), Some("cas-after-blob"));
}

#[test]
fn checkpoint_sequence_allocation_is_atomic_under_concurrent_writers() {
    // P1 "checkpoint numbering race": the old flow derived the sequence
    // from rows.len()+1 OUTSIDE the store, so two racing writers could
    // both receive the same N+1. insert_checkpoint must allocate
    // MAX(sequence)+1 and insert in ONE transaction: 8 writers × 25
    // checkpoints = 200 rows, all sequences distinct and gapless.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let store = std::sync::Arc::new(store);
    let mut handles = Vec::new();
    for t in 0..8 {
        let store = store.clone();
        let sid = s.id;
        handles.push(std::thread::spawn(move || {
            for i in 0..25 {
                let (id, seq) = store
                    .insert_checkpoint(
                        sid,
                        &format!("f{t}-{i}.rs"),
                        true,
                        "before-hash",
                        true,
                        "after-hash",
                        None,
                    )
                    .unwrap();
                assert!(id > 0);
                assert!(seq >= 1, "sequence must be >= 1, got {seq}");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let rows = store.checkpoints_of(s.id).unwrap();
    assert_eq!(rows.len(), 200, "every racing insert must land");
    let mut seqs: Vec<i64> = rows.iter().map(|c| c.sequence).collect();
    seqs.sort_unstable();
    for (i, seq) in seqs.iter().enumerate() {
        assert_eq!(
            *seq,
            (i + 1) as i64,
            "sequence {seq} at slot {i}: must be gapless"
        );
    }
    // The duplicate-guard invariant: no two rows share a sequence.
    let unique: std::collections::HashSet<i64> = seqs.iter().copied().collect();
    assert_eq!(
        unique.len(),
        200,
        "two writers must never receive the same sequence"
    );
}

#[test]
fn insert_checkpoint_roundtrips_existence_flags() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    // missing -> non-empty file: the before side has NO content, so its
    // hash column is the empty-string sentinel, not a hash of nothing.
    let (id, seq) = store
        .insert_checkpoint(
            s.id,
            "created.rs",
            false,
            "",
            true,
            "after-hex",
            Some("after-blob-hex"),
        )
        .unwrap();
    assert!(id > 0);
    assert_eq!(seq, 1);
    // file -> deleted (second row): the after side does not exist.
    let (_, seq2) = store
        .insert_checkpoint(s.id, "deleted.rs", true, "before-hex", false, "", None)
        .unwrap();
    assert_eq!(seq2, 2, "allocation must continue the session sequence");
    let rows = store.checkpoints_of(s.id).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        !rows[0].before_exists,
        "missing side must read back as not existing"
    );
    assert!(rows[0].after_exists);
    assert_eq!(rows[0].before_hash, "", "no content -> no hash");
    assert_eq!(rows[0].after_hash, "after-hex");
    assert!(rows[1].before_exists);
    assert!(
        !rows[1].after_exists,
        "deleted side must read back as not existing"
    );
    assert_eq!(rows[1].after_hash, "");
    assert_eq!(rows[1].after_cas_hash, None);
}

#[test]
fn permissions_resolve_once_and_expire() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let op = OpId::new(5);
    let (pid, expires_ms) = store.insert_permission(s.id, op, "execute_shell").unwrap();
    assert!(
        expires_ms > now_ms(),
        "the durable deadline must be in the future"
    );
    let pending = store.pending_permission(pid).unwrap().unwrap();
    assert_eq!(pending.0, s.id);
    assert_eq!(pending.1, op);
    assert_eq!(pending.2, "execute_shell");
    store.resolve_permission(pid, s.id, "allow").unwrap();
    assert!(store.pending_permission(pid).unwrap().is_none());
    // The PERSISTED decision is the real assertion (never comparing a
    // literal to itself): read the row back.
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("allow")
    );
    // A second resolve changes exactly zero rows: typed Conflict, and
    // the first persisted decision wins.
    let err = store
        .resolve_permission(pid, s.id, "deny")
        .expect_err("second resolve must conflict");
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("allow"),
        "first decision wins durably"
    );
}

#[test]
fn expired_permission_is_terminal_and_refuses_resolution() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let (pid, _) = store
        .insert_permission(s.id, OpId::new(6), "execute_shell")
        .unwrap();
    // Force the durable deadline into the past (no sleeping in tests).
    store
        .raw_conn()
        .execute(
            "UPDATE permission SET expires_ms = ?2 WHERE id = ?1",
            params![pid, now_ms() - 1],
        )
        .unwrap();
    assert!(
        store.pending_permission(pid).unwrap().is_none(),
        "an expired permission is not pending"
    );
    let err = store
        .resolve_permission(pid, s.id, "allow")
        .expect_err("an expired permission cannot be resolved");
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("expired"),
        "expiry is an explicit terminal state, never pending"
    );
    // Terminal stays terminal: another attempt changes nothing.
    let err = store
        .resolve_permission(pid, s.id, "deny")
        .expect_err("terminal expired stays terminal");
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("expired")
    );
}

pub(crate) fn permission_expiry_event() -> CommandEvent {
    CommandEvent {
        kind: EventKind::PermissionExpired,
        state: AgentState::ReadyForNextTurn,
        op_id: None,
        ts_ms: now_ms(),
        payload: Some(serde_json::json!({
            "reason": "durable deadline elapsed while no live waiter owned the request",
            "expired_count": 1,
        })),
        payload_ver: 1,
    }
}

/// Push one permission row's durable deadline far into the past without
/// sleeping (the row stays `pending`: only reconciliation terminalizes it).
pub(crate) fn force_permission_deadline_past(store: &Store, pid: i64) {
    store
        .raw_conn()
        .execute(
            "UPDATE permission SET expires_ms = expires_ms - 1000000000 WHERE id = ?1",
            params![pid],
        )
        .unwrap();
}

#[test]
fn expire_pending_permissions_terminalizes_in_one_transaction_and_is_idempotent() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let other = store.create_session(ws, "t2", "p", "m").unwrap();
    let (a, _) = store
        .insert_permission(s.id, OpId::new(11), "execute_shell")
        .unwrap();
    let (b, _) = store
        .insert_permission(s.id, OpId::new(12), "fs.write")
        .unwrap();
    let (live, _) = store
        .insert_permission(s.id, OpId::new(13), "fs.read")
        .unwrap();
    let (foreign, _) = store
        .insert_permission(other.id, OpId::new(14), "execute_shell")
        .unwrap();
    force_permission_deadline_past(&store, a);
    force_permission_deadline_past(&store, b);
    force_permission_deadline_past(&store, foreign);
    assert!(
        store
            .expired_pending_permissions(s.id, now_ms())
            .unwrap()
            .len()
            == 2
    );
    assert!(
        store
            .expired_pending_permissions(other.id, now_ms())
            .unwrap()
            .len()
            == 1
    );

    // Manual clock: the caller's now_ms is the expiry stamp.
    let now = now_ms() + 5;
    let resolution = store
        .expire_pending_permissions_for_session(
            s.id,
            now,
            AgentState::Idle,
            permission_expiry_event(),
        )
        .unwrap();
    assert_eq!(
        resolution.expired,
        vec![(a, OpId::new(11)), (b, OpId::new(12))],
        "ascending id order, both terminalized in one sweep"
    );
    assert!(resolution.event_seq.is_some());
    assert!(!resolution.is_empty());
    for (pid, want_op) in [(a, 11i64), (b, 12)] {
        let (decision, resolved_ms, op_raw): (String, Option<i64>, i64) = store
            .read()
            .unwrap()
            .query_row(
                "SELECT decision, resolved_ms, op_id FROM permission WHERE id = ?1",
                params![pid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(decision, "expired");
        assert_eq!(
            resolved_ms,
            Some(now),
            "resolved_ms is the caller's manual clock, not the wall clock"
        );
        assert_eq!(op_raw, want_op);
        assert!(store.pending_permission(pid).unwrap().is_none());
    }
    // An unexpired row and a foreign session's expired row are untouched.
    assert_eq!(
        store.permission_decision(live).unwrap().as_deref(),
        Some("pending")
    );
    assert!(store.pending_permission(live).unwrap().is_some());
    assert_eq!(
        store.permission_decision(foreign).unwrap().as_deref(),
        Some("pending"),
        "a foreign session's deadline is never swept here"
    );
    // The journal explains the sweep and names the exact rows it changed.
    let events = store.events_range(s.id, 1, None).unwrap();
    let expiry: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::PermissionExpired)
        .collect();
    assert_eq!(expiry.len(), 1, "one sweep, one event");
    assert_eq!(expiry[0].state, AgentState::ReadyForNextTurn);
    let payload = expiry[0].payload.as_ref().unwrap();
    assert_eq!(payload["permission_ids"], serde_json::json!([a, b]));
    assert_eq!(payload["op_ids"], serde_json::json!([11, 12]));
    assert!(payload["reason"].as_str().unwrap().contains("deadline"));
    // The committed event moved the session row with it.
    let state: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT state FROM session WHERE id = ?1",
            params![s.id.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert!(state.contains("ready_for_next_turn"), "{state}");

    // IDEMPOTENT second sweep: no rows, no event, no durable change.
    let seq_before = store.last_event_seq(s.id).unwrap().unwrap();
    let second = store
        .expire_pending_permissions_for_session(
            s.id,
            now,
            AgentState::ReadyForNextTurn,
            permission_expiry_event(),
        )
        .unwrap();
    assert!(second.is_empty());
    assert_eq!(second.event_seq, None);
    assert_eq!(store.last_event_seq(s.id).unwrap().unwrap(), seq_before);
    assert_eq!(
        store.permission_decision(a).unwrap().as_deref(),
        Some("expired"),
        "terminal stays terminal across sweeps"
    );
}

#[test]
fn expire_pending_permissions_refuses_wrong_kind_and_wrong_state_without_writes() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let (pid, _) = store
        .insert_permission(s.id, OpId::new(21), "execute_shell")
        .unwrap();
    force_permission_deadline_past(&store, pid);
    let before = store.last_event_seq(s.id).unwrap().unwrap();

    // A fake Deny (or any other kind) is refused: expiry is only ever
    // journaled as PermissionExpired.
    let mut wrong_kind = permission_expiry_event();
    wrong_kind.kind = EventKind::PermissionDenied;
    let err = store
        .expire_pending_permissions_for_session(s.id, now_ms(), AgentState::Idle, wrong_kind)
        .expect_err("a non-PermissionExpired event is refused");
    assert!(matches!(err, StoreError::Migration(_)), "{err:?}");
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("pending"),
        "the refused kind changed nothing"
    );

    // A stale pre-state is refused BEFORE any write (the whole command
    // rolls back: no terminalization, no event).
    let err = store
        .expire_pending_permissions_for_session(
            s.id,
            now_ms(),
            AgentState::Suspended,
            permission_expiry_event(),
        )
        .expect_err("a mismatched pre-state refuses");
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("pending")
    );
    assert_eq!(store.last_event_seq(s.id).unwrap().unwrap(), before);

    // The correct command still succeeds afterwards.
    let ok = store
        .expire_pending_permissions_for_session(
            s.id,
            now_ms(),
            AgentState::Idle,
            permission_expiry_event(),
        )
        .unwrap();
    assert_eq!(ok.expired, vec![(pid, OpId::new(21))]);
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("expired")
    );
}

#[test]
fn expire_pending_permissions_seams_reopen_old_or_new_only() {
    const SEAMS: [&str; 3] = [
        "session_command_side_row",
        "session_command_precommit",
        "session_command_committed",
    ];
    for seam in SEAMS {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let (pid, _) = store
            .insert_permission(s.id, OpId::new(31), "execute_shell")
            .unwrap();
        force_permission_deadline_past(&store, pid);
        store.crash_arm(CrashArm {
            point: seam,
            ordinal: 0,
        });
        // The deliberate in-transaction panic is caught by the writer
        // service (durable authority) and surfaced typed: the caller sees
        // a refusal, the daemon never panics, and the store stops
        // admitting mutations. Reopening is the recovery path.
        let err = store
            .expire_pending_permissions_for_session(
                s.id,
                now_ms(),
                AgentState::Idle,
                permission_expiry_event(),
            )
            .expect_err("seam must fire");
        assert!(matches!(err, StoreError::WriterUnavailable(_)), "{err:?}");
        assert!(!store.writer_available(), "seam {seam} must fire");
        drop(store);
        // Reopen from disk: exactly the old world or exactly the new one.
        let store = Store::open(dir.path(), true).unwrap();
        let decision = store.permission_decision(pid).unwrap();
        let expiry_events = store
            .events_range(s.id, 1, None)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EventKind::PermissionExpired)
            .count();
        let state: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT state FROM session WHERE id = ?1",
                params![s.id.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        match seam {
            "session_command_committed" => {
                assert_eq!(decision.as_deref(), Some("expired"), "seam {seam}");
                assert_eq!(expiry_events, 1, "seam {seam}");
                assert!(state.contains("ready_for_next_turn"), "{seam}: {state}");
            }
            _ => {
                assert_eq!(
                    decision.as_deref(),
                    Some("pending"),
                    "seam {seam}: the whole command must roll back"
                );
                assert_eq!(expiry_events, 0, "seam {seam}");
                assert!(state.contains("idle"), "{seam}: {state}");
            }
        }
    }
}

/// The recovery-purpose expiry sibling differs from the ordinary command
/// ONLY in its admission band: on identical fixtures and clocks it produces
/// byte-identical durable outcomes (same terminal rows, same event sequence,
/// same journal payload, same landing state). This pins the ordinary
/// command's behavior across the purpose plumbing (audit item 14).
#[test]
fn expire_pending_permissions_recovery_sibling_matches_ordinary_durable_outcome() {
    type Outcome = (
        Vec<(i64, OpId)>,
        Option<u64>,
        serde_json::Value,
        String,
        String,
    );

    fn outcome(recovery: bool) -> Outcome {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let (pid, _) = store
            .insert_permission(s.id, OpId::new(41), "execute_shell")
            .unwrap();
        force_permission_deadline_past(&store, pid);
        let now = now_ms() + 5;
        let resolution = if recovery {
            store
                .expire_pending_permissions_for_session_recovery(
                    s.id,
                    now,
                    AgentState::Idle,
                    permission_expiry_event(),
                )
                .unwrap()
        } else {
            store
                .expire_pending_permissions_for_session(
                    s.id,
                    now,
                    AgentState::Idle,
                    permission_expiry_event(),
                )
                .unwrap()
        };
        let event = store
            .events_range(s.id, 1, None)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EventKind::PermissionExpired)
            .expect("the sweep journaled its expiry");
        let payload = event.payload.clone().unwrap();
        let decision = store.permission_decision(pid).unwrap().unwrap();
        let state: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT state FROM session WHERE id = ?1",
                params![s.id.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        (
            resolution.expired,
            resolution.event_seq.map(|seq| seq.raw()),
            payload,
            decision,
            state,
        )
    }

    let ordinary = outcome(false);
    let recovery = outcome(true);
    assert_eq!(ordinary.0, vec![(1, OpId::new(41))]);
    assert_eq!(
        recovery, ordinary,
        "the recovery sibling is admission-only: durable outcome must be identical"
    );
    // The event payload still names the rows the transaction changed, and no
    // fake Deny is ever laundered through the recovery path.
    assert_eq!(
        ordinary.2["permission_ids"],
        serde_json::json!([ordinary.0[0].0])
    );
    assert_eq!(ordinary.2["op_ids"], serde_json::json!([41]));
    assert_eq!(ordinary.3, "expired");
}

#[test]
fn wrong_session_resolution_is_refused_and_leaves_the_row_pending() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let other = store.create_session(ws, "t2", "p", "m").unwrap();
    let (pid, _) = store
        .insert_permission(s.id, OpId::new(7), "fs.write")
        .unwrap();
    let err = store
        .resolve_permission(pid, other.id, "allow")
        .expect_err("a foreign session cannot resolve the permission");
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    assert!(
        store.pending_permission(pid).unwrap().is_some(),
        "the wrong-session attempt must not consume the row"
    );
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("pending")
    );
    // The owning session still resolves it.
    store.resolve_permission(pid, s.id, "allow").unwrap();
    assert_eq!(
        store.permission_decision(pid).unwrap().as_deref(),
        Some("allow")
    );
}

#[test]
fn permission_deadline_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (pid, expires_ms, s_id) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let (pid, expires) = store
            .insert_permission(s.id, OpId::new(8), "fs.write")
            .unwrap();
        (pid, expires, s.id)
    };
    // "Restart": reopen the same data root. The durable deadline is the
    // SAME value — a restart can never extend the window.
    let store = Store::open(dir.path(), true).unwrap();
    let persisted: i64 = store
        .read()
        .unwrap()
        .query_row(
            "SELECT expires_ms FROM permission WHERE id = ?1",
            params![pid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(persisted, expires_ms, "the deadline is durable");
    if expires_ms > now_ms() {
        assert!(store.pending_permission(pid).unwrap().is_some());
        store.resolve_permission(pid, s_id, "deny").unwrap();
    } else {
        assert!(store.pending_permission(pid).unwrap().is_none());
    }
}

#[test]
fn session_state_tracks_journal() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    assert_eq!(s.state, AgentState::Idle);
    store
        .append_event(
            s.id,
            None,
            EventKind::PromptReceived,
            AgentState::Preparing,
            now_ms(),
            None,
        )
        .unwrap();
    let s2 = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(s2.state, AgentState::Preparing);
    assert!(s2.updated_ms >= s.updated_ms);
}

#[test]
fn worktree_crud() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let id = store.put_worktree(ws, "/w/wt1", "feat/x").unwrap();
    assert_eq!(id, store.put_worktree(ws, "/w/wt1", "feat/x").unwrap());
    let list = store.worktrees_of(ws).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].branch, "feat/x");
    store.remove_worktree("/w/wt1").unwrap();
    assert!(store.worktrees_of(ws).unwrap().is_empty());
}

#[test]
fn corrupted_lifecycle_fails_closed() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    // Fresh rows hold the schema DEFAULT literal `open` (bare, non-JSON);
    // that spelling must still read back as Open, not Corrupt.
    let row = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(row.lifecycle, faktor_core::state::SessionLifecycle::Open);
    // (1) Not valid JSON at all: fail closed, never silently Open.
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE session SET lifecycle = 'garbage-not-json' WHERE id = ?1",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    match store.get_session(s.id) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("garbage lifecycle must fail closed, got {other:?}"),
    }
    // (2) Structurally valid JSON but an unknown variant: also fail
    // closed (a real Closed must not reopen as Open).
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE session SET lifecycle = '\"terminated\"' WHERE id = ?1",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    match store.get_session(s.id) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("unknown lifecycle variant must fail closed, got {other:?}"),
    }
    // (3) Regression: structurally valid JSON with a VALID variant still
    // parses.
    store
        .set_session_lifecycle(s.id, faktor_core::state::SessionLifecycle::Suspended)
        .unwrap();
    let row = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(
        row.lifecycle,
        faktor_core::state::SessionLifecycle::Suspended
    );
    // (4) NULL lifecycle cannot occur: the v2 schema declares the column
    // NOT NULL DEFAULT 'open', so SQLite itself rejects a NULL write;
    // parse_lifecycle never sees None. Prove the constraint holds.
    {
        let conn = store.raw_conn();
        let err = conn
            .execute(
                "UPDATE session SET lifecycle = NULL WHERE id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap_err();
        assert!(
            matches!(err, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::ConstraintViolation),
            "NOT NULL lifecycle must reject NULL writes, got {err:?}"
        );
    }
}

pub(crate) fn end_transition() -> SessionTransition {
    SessionTransition {
        expected_lifecycle: Some(SessionLifecycle::Open),
        new_lifecycle: Some(SessionLifecycle::Closed),
        expected_state: None,
        new_state: AgentState::Completed,
        event_kind: EventKind::SessionEnded,
        event_payload: None,
        event_payload_ver: 1,
    }
}

#[test]
fn transition_session_commits_atomically() {
    // The crash window: the old code updated lifecycle in one transaction
    // and appended SessionEnded in a second; a crash between them left
    // Closed-without-event or event-without-Closed. One call must produce
    // BOTH, durably, and a reopen must see them together.
    let dir = tempfile::tempdir().unwrap();
    let sid = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let seq = store
            .transition_session(s.id, None, end_transition())
            .unwrap();
        assert_eq!(seq.raw(), 2, "SessionCreated(1) + SessionEnded(2)");
        // Fresh read (same store): both sides of the transition visible.
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.lifecycle, SessionLifecycle::Closed);
        assert_eq!(row.state, AgentState::Completed);
        let events = store.events_range(s.id, 1, None).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, EventKind::SessionEnded);
        s.id
    };
    // "Daemon restart": both persisted in the single transaction.
    let store = Store::open(dir.path(), true).unwrap();
    let row = store.get_session(sid).unwrap().unwrap();
    assert_eq!(row.lifecycle, SessionLifecycle::Closed);
    assert_eq!(row.state, AgentState::Completed);
    let events = store.events_range(sid, 1, None).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].kind, EventKind::SessionEnded);
    assert_eq!(events[1].seq.raw(), 2);
}

#[test]
fn transition_session_conflict_aborts_atomically() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .set_session_lifecycle(s.id, SessionLifecycle::Suspended)
        .unwrap();
    let err = store
        .transition_session(s.id, None, end_transition())
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(_)),
        "expected lifecycle mismatch must be Conflict, got {err:?}"
    );
    // Rollback proven: no event row appeared and nothing moved.
    assert_eq!(store.events_range(s.id, 1, None).unwrap().len(), 1);
    let row = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(row.lifecycle, SessionLifecycle::Suspended);
    assert_eq!(row.state, AgentState::Idle);
}

#[test]
fn concurrent_end_session_races() {
    // Two (well, eight) racers try to close one session. The writer service
    // serializes the transactions; the expected_lifecycle guard means
    // exactly ONE wins and exactly ONE SessionEnded event exists.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let store = std::sync::Arc::new(store);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        let sid = s.id;
        handles.push(std::thread::spawn(move || {
            store.transition_session(sid, None, end_transition())
        }));
    }
    let results: Vec<StoreResult<EventSeq>> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(StoreError::Conflict(_))))
        .count();
    assert_eq!(wins, 1, "exactly one racer must win");
    assert_eq!(conflicts, 7, "the rest must conflict, got {results:?}");
    let events = store.events_range(s.id, 1, None).unwrap();
    let ended = events
        .iter()
        .filter(|e| e.kind == EventKind::SessionEnded)
        .count();
    assert_eq!(ended, 1, "exactly one SessionEnded event");
    assert_eq!(events.len(), 2);
    let row = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(row.lifecycle, SessionLifecycle::Closed);
    assert_eq!(row.state, AgentState::Completed);
}

#[test]
fn gapless_seq_across_transition() {
    // A transition_session event must continue the journal sequence after
    // regular appends — the shared insert path must be the SAME path.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    for i in 0..3 {
        store
            .append_event(
                s.id,
                Some(OpId::new(1 + i)),
                EventKind::ModelChunkReceived,
                AgentState::Streaming,
                now_ms(),
                None,
            )
            .unwrap();
    }
    let seq = store
        .transition_session(s.id, None, end_transition())
        .unwrap();
    assert_eq!(seq.raw(), 5, "created + 3 chunks + transition = seq 5");
    let events = store.events_range(s.id, 1, None).unwrap();
    assert_eq!(events.len(), 5);
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.seq.raw(), (i + 1) as u64, "gap at {i}");
    }
    assert_eq!(events[4].kind, EventKind::SessionEnded);
}

#[test]
fn set_lifecycle_if_is_conditional() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    // Auto-resume (Suspended -> Open): true and journal untouched.
    store
        .set_session_lifecycle(s.id, SessionLifecycle::Suspended)
        .unwrap();
    assert!(store
        .set_lifecycle_if(s.id, SessionLifecycle::Suspended, SessionLifecycle::Open)
        .unwrap());
    let row = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(row.lifecycle, SessionLifecycle::Open);
    assert_eq!(store.last_event_seq(s.id).unwrap().unwrap().raw(), 1);
    // Wrong expectation: no update, no error.
    assert!(!store
        .set_lifecycle_if(s.id, SessionLifecycle::Suspended, SessionLifecycle::Closed)
        .unwrap());
    let row = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(row.lifecycle, SessionLifecycle::Open);
}

#[test]
fn transition_session_missing_session_conflicts() {
    let (_d, store) = tmp_store();
    let err = store
        .transition_session(SessionId::new(999), None, end_transition())
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(_)),
        "missing session must be Conflict, got {err:?}"
    );
}

#[test]
fn turn_record_lifecycle_exclusive_active_and_envelope() {
    // v7: at most ONE active logical-turn record per session; a new
    // admission finalizes stragglers; the envelope is updateable while
    // active; finish is idempotent.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "ollama", "qwen3.8").unwrap();
    let a = OpId::new(11);
    let b = OpId::new(22);
    store
        .start_turn_record(s.id, a, None, Some(2), "ollama", "qwen3.8", None)
        .unwrap();
    let rec = store.active_turn_record(s.id).unwrap().unwrap();
    assert_eq!(rec.turn_op_id, a);
    assert_eq!(rec.status, TURN_RECORD_ACTIVE);
    assert_eq!(rec.prompt_message_id, Some(2));
    // A second admission while the first is active finalizes the first.
    store
        .start_turn_record(s.id, b, Some(1), Some(5), "ollama", "m2", Some("v1"))
        .unwrap();
    let recs = store.turn_records_of(s.id).unwrap();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].status, TURN_RECORD_FAILED, "straggler finalized");
    assert_eq!(recs[1].status, TURN_RECORD_ACTIVE);
    assert_eq!(recs[1].queue_seq, Some(1));
    assert_eq!(recs[1].variant.as_deref(), Some("v1"));
    assert_eq!(
        store.active_turn_record(s.id).unwrap().unwrap().turn_op_id,
        b
    );
    // Envelope update (per-message model override at drive start).
    assert!(store
        .set_turn_record_envelope(
            s.id,
            b,
            "ollama",
            "override-model",
            Some("v2"),
            Some("native")
        )
        .unwrap());
    let rec = store.turn_record_of(s.id, b).unwrap().unwrap();
    assert_eq!(rec.effective_model, "override-model");
    assert_eq!(rec.tool_mode.as_deref(), Some("native"));
    // Re-admission of the SAME op upserts the SAME record (crash between
    // claim and drive; the queue row is re-admitted) — never a phantom.
    store
        .start_turn_record(s.id, b, Some(1), Some(5), "ollama", "m3", None)
        .unwrap();
    assert_eq!(store.turn_records_of(s.id).unwrap().len(), 2);
    assert_eq!(
        store
            .turn_record_of(s.id, b)
            .unwrap()
            .unwrap()
            .effective_model,
        "m3"
    );
    // Finish transitions + idempotence.
    assert!(store
        .finish_turn_record(s.id, b, TURN_RECORD_COMPLETED)
        .unwrap());
    assert!(!store
        .finish_turn_record(s.id, b, TURN_RECORD_COMPLETED)
        .unwrap());
    assert!(store.active_turn_record(s.id).unwrap().is_none());
    assert_eq!(
        store.turn_record_of(s.id, b).unwrap().unwrap().status,
        TURN_RECORD_COMPLETED
    );
    // Invalid statuses are rejected loudly with the offending value named.
    assert!(matches!(
        store.finish_turn_record(s.id, b, "bogus"),
        Err(StoreError::Migration(ref m)) if m.contains("invalid status") && m.contains("bogus")
    ));
    // Unknown op: no record.
    assert!(store.turn_record_of(s.id, OpId::new(99)).unwrap().is_none());
}

#[test]
fn session_identity_defaults_to_standalone_and_adoption_is_durable() {
    // v8: sessions default to worktree 1 / task 1 (the DOCUMENTED
    // standalone identity) and adopt_identity persists the real
    // worktree/task ids durably — a reopen must read them back.
    let dir = tempfile::tempdir().unwrap();
    let (sid, ws) = {
        let store = Store::open(dir.path().join("store"), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.worktree_id, WorktreeId::new(1), "standalone default");
        assert_eq!(row.task_id, TaskId::new(1), "standalone default");
        // Adoption moves the row off the defaults.
        store
            .adopt_session_identity(s.id, WorktreeId::new(7), TaskId::new(9))
            .unwrap();
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.worktree_id, WorktreeId::new(7));
        assert_eq!(row.task_id, TaskId::new(9));
        // Unknown sessions are loud, not silent.
        assert!(store
            .adopt_session_identity(SessionId::new(9999), WorktreeId::new(2), TaskId::new(2))
            .is_err());
        (s.id, ws)
    };
    let store = Store::open(dir.path().join("store"), true).unwrap();
    let row = store.get_session(sid).unwrap().unwrap();
    assert_eq!(
        row.worktree_id,
        WorktreeId::new(7),
        "adoption survives reopen"
    );
    assert_eq!(row.task_id, TaskId::new(9));
    // list_sessions carries the same columns.
    let listed = store.list_sessions(Some(ws)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].worktree_id, WorktreeId::new(7));
}

#[test]
fn update_session_title_roundtrip_and_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "old", "p", "m").unwrap();
    assert!(store.update_session_title(s.id, "new title").unwrap());
    let row = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(row.title, "new title");
    assert!(row.updated_ms >= s.updated_ms, "updated_ms must bump");
    // Unknown sessions report false (nothing updated).
    assert!(!store
        .update_session_title(SessionId::new(9999), "x")
        .unwrap());
    // Durable across reopen.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(store.get_session(s.id).unwrap().unwrap().title, "new title");
}

#[test]
fn durable_task_repo_upsert_get_list_and_session_scope() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s1 = store.create_session(ws, "t1", "p", "m").unwrap();
    let ws2 = store.create_workspace("/w2").unwrap();
    let s2 = store.create_session(ws2, "t2", "p", "m").unwrap();
    let mut row = TaskRow {
        task_id: TaskId::new(1),
        session_id: s1.id,
        goal: "g".into(),
        acceptance_criteria: vec![],
        plan: vec!["step one".into()],
        attachments: vec![],
        contract: Default::default(),
        max_tokens: Some(1000),
        max_turns: Some(5),
        spent_tokens: 0,
        spent_turns: 0,
        state: TaskState::Pending,
        revision: TaskRevision::new(1),
        created_ms: 10,
        updated_ms: 10,
    };
    store.upsert_task(&row).unwrap();
    assert_eq!(
        store.get_task(s1.id, TaskId::new(1)).unwrap(),
        Some(row.clone())
    );
    assert_eq!(
        store.get_task(s2.id, TaskId::new(1)).unwrap(),
        None,
        "session scope"
    );
    assert_eq!(store.list_tasks(s1.id).unwrap(), vec![row.clone()]);
    assert!(store.list_tasks(s2.id).unwrap().is_empty());
    // Upsert REPLACES the same (session, task) row in place; the row
    // carries its (caller-maintained) revision.
    row.spent_tokens = 500;
    row.spent_turns = 2;
    row.state = TaskState::Blocked;
    row.revision = TaskRevision::new(2);
    row.updated_ms = 99;
    store.upsert_task(&row).unwrap();
    assert_eq!(
        store.list_tasks(s1.id).unwrap(),
        vec![row.clone()],
        "one row, replaced"
    );
    // P0-7 backstop: a raw row write that would MINT a completion state
    // out of a different state is refused — VerifiedComplete has no
    // machine edge and no proof can ride a generic upsert.
    for hostile_state in [
        TaskState::VerifiedComplete,
        TaskState::Verifying,
        TaskState::NeedsVerification,
    ] {
        let mut hostile = row.clone();
        hostile.state = hostile_state;
        let refused = store.upsert_task(&hostile);
        assert!(
            matches!(refused, Err(StoreError::Malformed(_))),
            "{hostile_state:?} must refuse a raw mint"
        );
    }
    assert_eq!(
        store.get_task(s1.id, TaskId::new(1)).unwrap(),
        Some(row.clone()),
        "refused writes left no trace"
    );
    // Same-state rewrite of a completion-relevant row is legal
    // (idempotent heal) and machine edges into NeedsVerification/
    // Verifying are legal (they are the transition-path writes).
    let mut in_verify = row.clone();
    in_verify.state = TaskState::Running;
    in_verify.revision = TaskRevision::new(3);
    store.upsert_task(&in_verify).unwrap();
    in_verify.state = TaskState::NeedsVerification;
    in_verify.revision = TaskRevision::new(4);
    store.upsert_task(&in_verify).unwrap();
    in_verify.state = TaskState::Verifying;
    in_verify.revision = TaskRevision::new(5);
    store.upsert_task(&in_verify).unwrap();
    in_verify.revision = TaskRevision::new(6);
    store.upsert_task(&in_verify).unwrap();
    assert_eq!(
        store
            .get_task(s1.id, TaskId::new(1))
            .unwrap()
            .unwrap()
            .state,
        TaskState::Verifying
    );
    // A second task of the SAME session lists oldest-first alongside it.
    let mut other = row.clone();
    other.task_id = TaskId::new(2);
    other.created_ms = 5;
    other.updated_ms = 5;
    other.state = TaskState::Running;
    store.upsert_task(&other).unwrap();
    let listed = store.list_tasks(s1.id).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].task_id, TaskId::new(2), "oldest-created first");
    // Corrupt state strings fail closed as Corrupt, never a panic.
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE task SET state = 'garbage' WHERE session_id = ?1 AND task_id = ?2",
            params![s1.id.raw() as i64, 2],
        )
        .unwrap();
    }
    match store.get_task(s1.id, TaskId::new(2)) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("corrupt task state must error, not parse: {other:?}"),
    }
    assert!(
        matches!(store.list_tasks(s1.id), Err(StoreError::Corrupt(_))),
        "corrupt rows surface as Corrupt, never a silent skip"
    );
}

pub(crate) fn prefix_hash(seed: u8) -> [u8; 32] {
    [seed; 32]
}

#[test]
fn stored_stability_aggregate_math_and_empty_sessions() {
    // (d) The additive aggregate query: mean + population std dev over
    // the rows that carry a recorded stability, and None on sessions
    // with nothing recorded.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    // No observations at all.
    assert_eq!(store.session_stored_prefix_stability(s.id).unwrap(), None);
    // Legacy rows (no prefix data) do not count either.
    store
        .record_provider_call(s.id, OpId::new(1), "p", "m", "ok", None, None, None)
        .unwrap();
    assert_eq!(store.session_stored_prefix_stability(s.id).unwrap(), None);
    // Recorded stabilities: 0.5, 0.5, 1.0 -> mean 2/3, σ ≈ 0.2357.
    for (i, st) in [0.5f64, 0.5, 1.0].iter().enumerate() {
        store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(2 + i as u64),
                "p",
                "m",
                "ok",
                None,
                None,
                None,
                Some(prefix_hash(i as u8 + 3)),
                Some(100),
                Some(*st),
            )
            .unwrap();
    }
    // Rows with a hash but NULL stability contribute nothing.
    store
        .record_provider_call_with_prefix(
            s.id,
            OpId::new(9),
            "p",
            "m",
            "ok",
            None,
            None,
            None,
            Some(prefix_hash(9)),
            Some(50),
            None,
        )
        .unwrap();
    let agg = store
        .session_stored_prefix_stability(s.id)
        .unwrap()
        .unwrap();
    assert_eq!(agg.observations, 3);
    assert!((agg.mean - 2.0 / 3.0).abs() < 1e-12);
    let want_std = ((0.5f64 - 2.0 / 3.0).powi(2) * 2.0 + (1.0f64 - 2.0 / 3.0).powi(2)) / 3.0;
    assert!((agg.std_dev - want_std.sqrt()).abs() < 1e-12);
    // Isolation: the second session still has nothing.
    let s2 = store.create_session(ws, "t2", "p", "m").unwrap();
    assert_eq!(store.session_stored_prefix_stability(s2.id).unwrap(), None);
}

#[test]
fn all_running_tool_rows_and_active_turns_scan_every_session() {
    // Deep doctor queries are GLOBAL: running tool runs and active turn
    // records from two sessions must both surface.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s1 = store.create_session(ws, "a", "p", "m").unwrap();
    let s2 = store.create_session(ws, "b", "p", "m").unwrap();
    store
        .start_tool_run(
            s1.id,
            OpId::new(11),
            "echo",
            serde_json::json!({}),
            serde_json::json!({"strategy": "idempotent"}),
            None,
            None,
        )
        .unwrap();
    store
        .start_tool_run(
            s2.id,
            OpId::new(22),
            "write_file",
            serde_json::json!({"path": "/x"}),
            serde_json::json!({"strategy": "verify_hash"}),
            Some("ab".repeat(32)),
            None,
        )
        .unwrap();
    // One finished run must NOT appear.
    store
        .start_tool_run(
            s1.id,
            OpId::new(33),
            "echo",
            serde_json::json!({}),
            serde_json::json!({"strategy": "none"}),
            None,
            None,
        )
        .unwrap();
    store
        .finish_tool_run(s1.id, OpId::new(33), "completed", "applied")
        .unwrap();
    store
        .start_turn_record(s1.id, OpId::new(101), None, Some(2), "p", "m", None)
        .unwrap();
    store
        .start_turn_record(s2.id, OpId::new(202), None, Some(2), "p", "m", Some("v1"))
        .unwrap();
    // A second, then finished, record on s1 must not appear as active.
    store
        .finish_turn_record(s1.id, OpId::new(101), TURN_RECORD_FAILED)
        .unwrap();
    store
        .start_turn_record(s1.id, OpId::new(303), None, Some(2), "p", "m", None)
        .unwrap();
    store
        .finish_turn_record(s1.id, OpId::new(303), TURN_RECORD_COMPLETED)
        .unwrap();
    let running = store.all_running_tool_rows().unwrap();
    assert_eq!(running.len(), 2, "both sessions' running rows surface");
    assert!(running
        .iter()
        .any(|r| r.session_id == s1.id && r.op_id == OpId::new(11)));
    assert!(running
        .iter()
        .any(|r| r.session_id == s2.id && r.op_id == OpId::new(22)));
    let active = store.all_active_turns().unwrap();
    assert_eq!(active.len(), 1, "only session 2's turn is still active");
    assert_eq!(active[0].turn_op_id, OpId::new(202));
}

#[test]
fn journal_consistency_flags_gaps_and_torn_sessions() {
    // A gapless journal stays clean; a deleted middle event and a
    // session row without a journal are both flagged.
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    for i in 0..3 {
        store
            .append_event(
                s.id,
                Some(OpId::new(1 + i)),
                EventKind::ModelChunkReceived,
                AgentState::Streaming,
                now_ms(),
                None,
            )
            .unwrap();
    }
    assert!(store.journal_consistency_issues().unwrap().is_empty());
    // Torn session: raw session row with no journal (bypasses the API).
    {
        let conn = store.raw_conn();
        conn.execute(
            "INSERT INTO session(workspace_id, title, provider, model, state, lifecycle, created_ms, updated_ms)
             VALUES (?1, 'torn', 'p', 'm', '\"idle\"', 'open', 0, 0)",
            params![ws.raw() as i64],
        )
        .unwrap();
    }
    let issues = store.journal_consistency_issues().unwrap();
    assert_eq!(issues.len(), 1, "one torn session");
    assert!(issues[0].contains("no events"));
    // A gap: delete the middle event of the healthy session.
    {
        let conn = store.raw_conn();
        conn.execute(
            "DELETE FROM event WHERE session_id = ?1 AND seq = 2",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    let issues = store.journal_consistency_issues().unwrap();
    let gap = issues
        .iter()
        .find(|i| i.contains(&format!("session {}", s.id.raw())))
        .expect("gap must be flagged");
    assert!(gap.contains("1..=3"), "gap issue: {gap}");
}

pub(crate) fn hot_session(store: &Store) -> SessionId {
    let ws = store.create_workspace("/w").unwrap();
    store.create_session(ws, "t", "p", "m").unwrap().id
}

pub(crate) fn hot_session_sids(store: &Store, n: u64) -> Vec<SessionId> {
    let ws = store.create_workspace("/w").unwrap();
    (1..=n)
        .map(|_| store.create_session(ws, "t", "p", "m").unwrap().id)
        .collect()
}

#[test]
fn batch_hot_writes_events_stay_gapless_across_grouped_sessions() {
    // Two sessions journaling in one group: per-session MAX(seq) is
    // computed inside the shared transaction, so seqs stay gapless per
    // session even when the groups interleave.
    let (_d, store) = tmp_store();
    let sids = hot_session_sids(&store, 2);
    let writes = sids
        .iter()
        .enumerate()
        .flat_map(|(i, sid)| {
            vec![
                HotWrite::AppendEvent {
                    session_id: *sid,
                    op_id: None,
                    kind: EventKind::PromptReceived,
                    state: AgentState::Preparing,
                    ts_ms: 10 + i as i64,
                    payload: None,
                    payload_ver: 1,
                },
                HotWrite::AppendEvent {
                    session_id: *sid,
                    op_id: None,
                    kind: EventKind::ContextPrepared,
                    state: AgentState::BuildingContext,
                    ts_ms: 20 + i as i64,
                    payload: None,
                    payload_ver: 1,
                },
            ]
        })
        .collect::<Vec<_>>();
    let (out, _) = store.batch_hot_writes(&writes).unwrap();
    assert!(out.iter().all(|r| r.is_ok()));
    for sid in &sids {
        let seqs: Vec<u64> = store
            .events_range(*sid, 1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.seq.raw())
            .collect();
        assert_eq!(seqs, vec![1, 2, 3], "seed + two grouped events, gapless");
    }
    // Replay accepts the interleaved journal (state machine coherent).
    for sid in &sids {
        let events = store.events_range(*sid, 1, None).unwrap();
        let state = events.last().unwrap().state;
        assert_eq!(state, AgentState::BuildingContext);
    }
}

// ------------------------------------------------- WAL maintenance tests

/// `(frames in WAL, frames backfilled into the main file)` WITHOUT doing
/// any checkpoint work: `NOOP` only reports the WAL status, so it
/// observes exactly what the last real checkpoint left behind.
pub(crate) fn wal_backfill(store: &Store) -> (i64, i64) {
    store
        .read()
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(NOOP)", [], |r| {
            Ok((r.get(1)?, r.get(2)?))
        })
        .unwrap()
}

#[test]
fn autocheckpoint_is_disabled_and_passive_checkpoint_folds_the_wal() {
    // The actor's 5 ms gate can only hold if SQLite never checkpoints
    // inside a committing statement: configure must disable it on every
    // connection, and the explicit PASSIVE checkpoint must fold the
    // frames it leaves behind.
    let (_dir, store) = tmp_store();
    let auto: i64 = store
        .read()
        .unwrap()
        .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
        .unwrap();
    assert_eq!(auto, 0, "wal_autocheckpoint must be disabled");
    let sid = hot_session(&store);
    for seq in 1..=200i64 {
        store
            .put_message(sid, seq, "assistant", serde_json::json!({ "i": seq }))
            .unwrap();
    }
    assert!(
        wal_backfill(&store).0 > 0,
        "autocheckpoint is off: the frames must still be in the WAL"
    );
    assert_eq!(wal_backfill(&store).1, 0, "SQLite folded nothing by itself");
    store.wal_checkpoint_passive().unwrap();
    let (log, backfill) = wal_backfill(&store);
    assert_eq!(
        backfill, log,
        "explicit PASSIVE folded every frame ({backfill}/{log})"
    );
    // The fold is not a data loss: rows stay readable through the store.
    assert_eq!(store.message_count(sid).unwrap(), 200);
}

#[test]
fn passive_checkpoint_never_waits_for_a_pinned_reader_and_folds_after() {
    // Maintenance must be bounded: PASSIVE never blocks on a reader that
    // pins a snapshot. It folds what it safely can (frames after the
    // pinned read mark stay in the WAL), the pinned reader keeps its
    // snapshot, and a later checkpoint (after release) folds the rest.
    let (_dir, store) = tmp_store();
    let sid = hot_session(&store);
    for seq in 1..=64i64 {
        store
            .put_message(sid, seq, "assistant", serde_json::json!({ "i": seq }))
            .unwrap();
    }
    let reader = store.read().unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let n: i64 = reader
        .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 64, "reader snapshot starts at 64 rows");
    for seq in 65..=128i64 {
        store
            .put_message(sid, seq, "assistant", serde_json::json!({ "i": seq }))
            .unwrap();
    }
    let (log, before) = wal_backfill(&store);
    assert!(before < log, "frames are waiting to be folded");
    let t0 = Instant::now();
    store.wal_checkpoint_passive().unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "PASSIVE must not wait for the pinned reader (took {elapsed:?})"
    );
    // The reader's snapshot is untouched by the checkpoint.
    let n: i64 = reader
        .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 64, "pinned snapshot unchanged");
    let (log, backfill) = wal_backfill(&store);
    assert!(
        backfill > 0 && backfill < log,
        "PASSIVE folded only up to the pinned read mark ({backfill}/{log})"
    );
    // Release the snapshot explicitly: the pooled connection survives, so
    // an open read transaction would keep pinning the WAL in the pool.
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    store.wal_checkpoint_passive().unwrap();
    let (log, backfill) = wal_backfill(&store);
    assert_eq!(
        backfill, log,
        "after release PASSIVE folds the rest ({backfill}/{log})"
    );
    assert_eq!(store.message_count(sid).unwrap(), 128);
}
