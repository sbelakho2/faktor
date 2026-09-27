use std::sync::{Arc, Barrier};

use super::*;

const OP: u64 = 42;

/// The three durability boundaries of every atomic session command.
const SEAMS: [&str; 3] = [
    "session_command_side_row",
    "session_command_precommit",
    "session_command_committed",
];

const CMDS: [Cmd; 6] = [
    Cmd::Permission,
    Cmd::Grant,
    Cmd::StartTool,
    Cmd::FinishTool,
    Cmd::Checkpoint,
    Cmd::Compaction,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Permission,
    Grant,
    StartTool,
    FinishTool,
    Checkpoint,
    Compaction,
}

impl Cmd {
    fn name(self) -> &'static str {
        match self {
            Cmd::Permission => "request_permission",
            Cmd::Grant => "resolve_permission",
            Cmd::StartTool => "start_tool_run",
            Cmd::FinishTool => "finish_tool_run",
            Cmd::Checkpoint => "put_checkpoint",
            Cmd::Compaction => "record_compaction",
        }
    }
}

fn setup() -> (tempfile::TempDir, Store, SessionId) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store"), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let row = store.create_session(ws, "s", "p", "m").unwrap();
    (dir, store, row.id)
}

fn ev(kind: EventKind, state: AgentState, payload: serde_json::Value) -> CommandEvent {
    CommandEvent {
        kind,
        state,
        op_id: Some(OpId::new(OP)),
        ts_ms: now_ms(),
        payload: Some(payload),
        payload_ver: 1,
    }
}

/// Execute one logical command of the fixed pipeline against the state the
/// previous commands produced.
fn execute(store: &Store, sid: SessionId, cmd: Cmd, pid: &mut Option<i64>) {
    match cmd {
        Cmd::Permission => {
            let (id, _, _) = store
                .insert_permission_and_event(
                    sid,
                    OpId::new(OP),
                    "{\"capability\":\"read\"}",
                    AgentState::Idle,
                    ev(
                        EventKind::ToolRequested,
                        AgentState::WaitingForPermission,
                        serde_json::json!({ "capability": "read" }),
                    ),
                )
                .expect("permission request");
            *pid = Some(id);
        }
        Cmd::Grant => {
            store
                .resolve_permission_and_event(
                    pid.expect("permission requested first"),
                    sid,
                    "allow",
                    AgentState::WaitingForPermission,
                    ev(
                        EventKind::PermissionGranted,
                        AgentState::ExecutingTool,
                        serde_json::json!({ "decision": "allow" }),
                    ),
                )
                .expect("permission resolution");
        }
        Cmd::StartTool => {
            store
                .start_tool_run_and_event(
                    sid,
                    OpId::new(OP),
                    "read_file",
                    serde_json::json!({ "path": "a" }),
                    serde_json::json!({ "strategy": "none" }),
                    None,
                    None,
                    AgentState::ExecutingTool,
                    ev(
                        EventKind::ToolStarted,
                        AgentState::ExecutingTool,
                        serde_json::json!({ "tool": "read_file" }),
                    ),
                )
                .expect("tool start");
        }
        Cmd::FinishTool => {
            store
                .finish_tool_run_and_event(
                    sid,
                    OpId::new(OP),
                    "completed",
                    "verified",
                    AgentState::ExecutingTool,
                    ev(
                        EventKind::ToolCompleted,
                        AgentState::Validating,
                        serde_json::json!({ "status": "completed" }),
                    ),
                )
                .expect("tool finish");
        }
        Cmd::Checkpoint => {
            store
                .put_checkpoint_and_event(
                    sid,
                    0,
                    "a.rs",
                    &"11".repeat(32),
                    &"22".repeat(32),
                    None,
                    AgentState::Validating,
                    ev(
                        EventKind::CheckpointCreated,
                        AgentState::Validating,
                        serde_json::json!({ "sequence": 0, "path": "a.rs" }),
                    ),
                )
                .expect("checkpoint");
        }
        Cmd::Compaction => {
            store
                .record_compaction_and_event(
                    sid,
                    100_000,
                    40_000,
                    50_000,
                    true,
                    "summarize",
                    AgentState::Validating,
                    ev(
                        EventKind::ContextCompacted,
                        AgentState::Validating,
                        serde_json::json!({ "accepted": true }),
                    ),
                )
                .expect("compaction");
        }
    }
}

fn run_prefix(store: &Store, sid: SessionId, upto: Cmd) -> Option<i64> {
    let mut pid = None;
    for cmd in CMDS {
        if cmd == upto {
            break;
        }
        execute(store, sid, cmd, &mut pid);
    }
    pid
}

/// Semantic durable state, timestamps excluded: equality IS old-vs-new.
#[derive(Debug, PartialEq, Eq)]
struct World(Vec<String>);

fn query_lines(conn: &Connection, sql: &str, sid: SessionId) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut rows = stmt.query(params![sid.raw() as i64]).unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        let n = row.as_ref().column_count();
        let mut line = String::new();
        for i in 0..n {
            if i > 0 {
                line.push('|');
            }
            let v: rusqlite::types::Value = row.get(i).unwrap();
            line.push_str(&format!("{v:?}"));
        }
        out.push(line);
    }
    out
}

fn world(store: &Store, sid: SessionId) -> World {
    let conn = store.raw_conn();
    let mut lines = Vec::new();
    let state: String = conn
        .query_row(
            "SELECT state FROM session WHERE id = ?1",
            params![sid.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    lines.push(format!("state:{state}"));
    lines.extend(query_lines(
        &conn,
        "SELECT seq, kind, state, op_id, payload, payload_ver FROM event WHERE session_id = ?1 ORDER BY seq",
        sid,
    ));
    lines.extend(query_lines(
        &conn,
        "SELECT id, op_id, decision, resolved_ms IS NOT NULL FROM permission WHERE session_id = ?1 ORDER BY id",
        sid,
    ));
    lines.extend(query_lines(
        &conn,
        "SELECT id, op_id, tool, status, effect_status, expected_hash FROM tool_run WHERE session_id = ?1 ORDER BY id",
        sid,
    ));
    lines.extend(query_lines(
        &conn,
        "SELECT id, sequence, path, before_hash, after_hash, restored_ms IS NOT NULL FROM checkpoint WHERE session_id = ?1 ORDER BY id",
        sid,
    ));
    lines.extend(query_lines(
        &conn,
        "SELECT id, before_tokens, after_tokens, target_tokens, accepted, strategy FROM compaction WHERE session_id = ?1 ORDER BY id",
        sid,
    ));
    World(lines)
}

fn reference_world(completed: usize) -> World {
    let (dir, store, sid) = setup();
    let mut pid = None;
    for cmd in CMDS.iter().take(completed) {
        execute(&store, sid, *cmd, &mut pid);
    }
    let w = world(&store, sid);
    drop(store);
    drop(dir);
    w
}

fn crashed_world(cmd: Cmd, seam: &'static str) -> World {
    let (dir, store, sid) = setup();
    let pid = run_prefix(&store, sid, cmd);
    store.crash_arm(CrashArm {
        point: seam,
        ordinal: 0,
    });
    // The seam's deliberate panic is caught at the durable-authority
    // boundary and surfaced as the typed unavailable state: the test
    // helper's `expect` then unwinds on THIS thread (never a daemon
    // panic), and the writer stops admitting mutations.
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut pid = pid;
        execute(&store, sid, cmd, &mut pid);
    }));
    assert!(caught.is_err(), "seam {seam} at {} must fire", cmd.name());
    assert!(
        !store.writer_available(),
        "seam {seam} at {} must mark the durable authority unavailable",
        cmd.name()
    );
    drop(store);
    let reopened = Store::open(dir.path().join("store"), true).unwrap();
    let w = world(&reopened, sid);
    drop(reopened);
    drop(dir);
    w
}

#[test]
fn crash_at_each_seam_reopens_exactly_old_or_exactly_new() {
    for (idx, cmd) in CMDS.iter().enumerate() {
        let old = reference_world(idx);
        let new = reference_world(idx + 1);
        assert_ne!(old, new, "{} must change the durable world", cmd.name());
        for seam in SEAMS {
            let durable = crashed_world(*cmd, seam);
            match seam {
                "session_command_committed" => assert_eq!(
                    durable,
                    new,
                    "{} after {seam}: the committed command must be durable",
                    cmd.name()
                ),
                _ => assert_eq!(
                    durable,
                    old,
                    "{} at {seam}: the crashed command must roll back whole",
                    cmd.name()
                ),
            }
        }
    }
}

/// Crash-recovery terminalization: the tool row's terminal status/effect
/// and its journal event (`RecoveryApplied`) are ONE transaction. At
/// every durability boundary the reopened world is exactly the old or
/// exactly the new one: a committed command has BOTH the terminal row
/// and the event; a rolled-back one has NEITHER.
#[test]
fn recovered_tool_finish_seams_commit_row_and_event_together() {
    let recovered = |store: &Store, sid: SessionId| -> StoreResult<EventSeq> {
        store.finish_recovered_tool_run_and_event(
            sid,
            OpId::new(OP),
            "interrupted",
            "unknown",
            EventKind::RecoveryApplied,
            AgentState::ExecutingTool,
            Some(serde_json::json!({ "action": "unknown_effect" })),
        )
    };
    for seam in SEAMS {
        // Old world: permission + grant + running tool row.
        let (dir, store, sid) = setup();
        let mut pid = None;
        execute(&store, sid, Cmd::Permission, &mut pid);
        execute(&store, sid, Cmd::Grant, &mut pid);
        execute(&store, sid, Cmd::StartTool, &mut pid);
        let old = world(&store, sid);
        // New world: the same prefix + the committed terminalization.
        let (dir2, store2, sid2) = setup();
        let mut pid2 = None;
        execute(&store2, sid2, Cmd::Permission, &mut pid2);
        execute(&store2, sid2, Cmd::Grant, &mut pid2);
        execute(&store2, sid2, Cmd::StartTool, &mut pid2);
        recovered(&store2, sid2).expect("recovered finish");
        let new = world(&store2, sid2);
        assert_ne!(old, new, "the terminalization changes the durable world");
        drop(store2);
        drop(dir2);

        store.crash_arm(CrashArm {
            point: seam,
            ordinal: 0,
        });
        let err = recovered(&store, sid).expect_err("seam must fire");
        assert!(matches!(err, StoreError::WriterUnavailable(_)), "{err:?}");
        assert!(!store.writer_available(), "seam {seam} must fire");
        drop(store);
        let reopened = Store::open(dir.path().join("store"), true).unwrap();
        let durable = world(&reopened, sid);
        drop(reopened);
        drop(dir);
        match seam {
            "session_command_committed" => assert_eq!(
                durable, new,
                "committed terminalization: row AND event durable together"
            ),
            _ => assert_eq!(
                durable, old,
                "crashed terminalization at {seam}: neither row nor event"
            ),
        }
    }
}

/// The recovery terminalization refuses typed BEFORE any write on a
/// wrong expected state, and a second finish can never append an event
/// without a still-running row (no row -> no event).
#[test]
fn recovered_tool_finish_wrong_state_or_terminal_row_refuses_without_trace() {
    let (_dir, store, sid) = setup();
    let mut pid = None;
    execute(&store, sid, Cmd::Permission, &mut pid);
    execute(&store, sid, Cmd::Grant, &mut pid);
    execute(&store, sid, Cmd::StartTool, &mut pid);
    let before = world(&store, sid);
    let err = store
        .finish_recovered_tool_run_and_event(
            sid,
            OpId::new(OP),
            "interrupted",
            "unknown",
            EventKind::RecoveryApplied,
            AgentState::Suspended,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    assert_eq!(world(&store, sid), before, "refused before any write");
    store
        .finish_recovered_tool_run_and_event(
            sid,
            OpId::new(OP),
            "interrupted",
            "unknown",
            EventKind::RecoveryApplied,
            AgentState::ExecutingTool,
            None,
        )
        .expect("first terminalization");
    let after = world(&store, sid);
    let err = store
        .finish_recovered_tool_run_and_event(
            sid,
            OpId::new(OP),
            "failed",
            "unknown",
            EventKind::RecoveryApplied,
            AgentState::ExecutingTool,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    assert_eq!(
        world(&store, sid),
        after,
        "an already-terminal row can never gain a second event"
    );
}

/// The content-aware checkpoint command (the production path of
/// faktor-snapshot): ONE transaction allocates the sequence, inserts the
/// existence-bearing row and appends its `CheckpointCreated` event; a
/// wrong expected state refuses typed BEFORE any write.
#[test]
fn content_checkpoint_allocates_sequence_and_journals_atomically() {
    let (_dir, store, sid) = setup();
    let (id, sequence, seq) = store
        .insert_checkpoint_and_event(
            sid,
            "new.rs",
            false,
            "",
            true,
            &"aa".repeat(32),
            Some(&"aa".repeat(32)),
            AgentState::Idle,
        )
        .unwrap();
    assert_eq!(sequence, 1);
    let rows = store.checkpoints_of(sid).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert!(
        !rows[0].before_exists,
        "the missing before side is recorded"
    );
    assert_eq!(rows[0].before_hash, "");
    assert!(rows[0].after_exists);
    let events = store.events_range(sid, seq.raw(), None).unwrap();
    assert_eq!(events.len(), 1, "exactly one event per checkpoint row");
    assert_eq!(events[0].kind, EventKind::CheckpointCreated);
    assert_eq!(events[0].state, AgentState::Idle);
    assert_eq!(
        events[0].payload.as_ref().unwrap()["sequence"],
        serde_json::json!(1),
        "the event names the sequence the same transaction allocated"
    );
    // The next command allocates the NEXT sequence, never a duplicate.
    let (_, sequence2, _) = store
        .insert_checkpoint_and_event(
            sid,
            "second.rs",
            true,
            &"bb".repeat(32),
            true,
            &"cc".repeat(32),
            None,
            AgentState::Idle,
        )
        .unwrap();
    assert_eq!(sequence2, 2);
    // A wrong expected state refuses with Conflict before any write: no
    // new row, no event.
    let rows_before: Vec<i64> = store
        .checkpoints_of(sid)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    let events_before = store.events_range(sid, 1, None).unwrap().len();
    let err = store
        .insert_checkpoint_and_event(
            sid,
            "third.rs",
            true,
            &"dd".repeat(32),
            true,
            &"ee".repeat(32),
            None,
            AgentState::Suspended,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
    assert_eq!(
        store
            .checkpoints_of(sid)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        rows_before,
        "a refused checkpoint writes no row"
    );
    assert_eq!(
        store.events_range(sid, 1, None).unwrap().len(),
        events_before,
        "a refused checkpoint writes no event"
    );
}

#[test]
fn expected_state_mismatch_refuses_before_any_write() {
    for (idx, cmd) in CMDS.iter().enumerate() {
        let old = reference_world(idx);
        let (dir, store, sid) = setup();
        let pid = run_prefix(&store, sid, *cmd);
        // Suspended is never the pipeline's current state at any command.
        let wrong = AgentState::Suspended;
        let err = match cmd {
            Cmd::Permission => store
                .insert_permission_and_event(
                    sid,
                    OpId::new(OP),
                    "cap",
                    wrong,
                    ev(
                        EventKind::ToolRequested,
                        AgentState::WaitingForPermission,
                        serde_json::json!({}),
                    ),
                )
                .map(|_| ())
                .unwrap_err(),
            Cmd::Grant => store
                .resolve_permission_and_event(
                    pid.expect("permission"),
                    sid,
                    "allow",
                    wrong,
                    ev(
                        EventKind::PermissionGranted,
                        AgentState::ExecutingTool,
                        serde_json::json!({}),
                    ),
                )
                .map(|_| ())
                .unwrap_err(),
            Cmd::StartTool => store
                .start_tool_run_and_event(
                    sid,
                    OpId::new(OP),
                    "read_file",
                    serde_json::json!({}),
                    serde_json::json!({}),
                    None,
                    None,
                    wrong,
                    ev(
                        EventKind::ToolStarted,
                        AgentState::ExecutingTool,
                        serde_json::json!({}),
                    ),
                )
                .map(|_| ())
                .unwrap_err(),
            Cmd::FinishTool => store
                .finish_tool_run_and_event(
                    sid,
                    OpId::new(OP),
                    "completed",
                    "verified",
                    wrong,
                    ev(
                        EventKind::ToolCompleted,
                        AgentState::Validating,
                        serde_json::json!({}),
                    ),
                )
                .map(|_| ())
                .unwrap_err(),
            Cmd::Checkpoint => store
                .put_checkpoint_and_event(
                    sid,
                    0,
                    "a.rs",
                    &"11".repeat(32),
                    &"22".repeat(32),
                    None,
                    wrong,
                    ev(
                        EventKind::CheckpointCreated,
                        AgentState::Validating,
                        serde_json::json!({}),
                    ),
                )
                .map(|_| ())
                .unwrap_err(),
            Cmd::Compaction => store
                .record_compaction_and_event(
                    sid,
                    100_000,
                    40_000,
                    50_000,
                    true,
                    "summarize",
                    wrong,
                    ev(
                        EventKind::ContextCompacted,
                        AgentState::Validating,
                        serde_json::json!({}),
                    ),
                )
                .map(|_| ())
                .unwrap_err(),
        };
        assert!(
            matches!(err, StoreError::Conflict(_)),
            "{} with a wrong expected state must refuse with Conflict: {err:?}",
            cmd.name()
        );
        assert_eq!(
            world(&store, sid),
            old,
            "{} refused before any write",
            cmd.name()
        );
        drop(store);
        drop(dir);
    }
}

#[test]
fn concurrent_duplicate_resolution_has_exactly_one_winner() {
    let (_dir, store, sid) = setup();
    let (id, _, _) = store
        .insert_permission_and_event(
            sid,
            OpId::new(OP),
            "cap",
            AgentState::Idle,
            ev(
                EventKind::ToolRequested,
                AgentState::WaitingForPermission,
                serde_json::json!({}),
            ),
        )
        .unwrap();
    let store = Arc::new(store);
    let barrier = Arc::new(Barrier::new(2));
    let spawn = |decision: &'static str, kind: EventKind| {
        let store = store.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            store
                .resolve_permission_and_event(
                    id,
                    sid,
                    decision,
                    AgentState::WaitingForPermission,
                    ev(kind, AgentState::ExecutingTool, serde_json::json!({})),
                )
                .map(|_| ())
        })
    };
    let allow = spawn("allow", EventKind::PermissionGranted);
    let deny = spawn("deny", EventKind::PermissionDenied);
    let r1 = allow.join().unwrap();
    let r2 = deny.join().unwrap();
    assert!(
        r1.is_ok() != r2.is_ok(),
        "exactly one resolver must win; got {r1:?} / {r2:?}"
    );
    let loser = if r1.is_ok() { &r2 } else { &r1 };
    assert!(
        matches!(loser, Err(StoreError::Conflict(_))),
        "the loser must refuse typed: {loser:?}"
    );
    let conn = store.raw_conn();
    let resolutions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM event WHERE session_id = ?1
             AND kind IN ('permission_granted', 'permission_denied')",
            params![sid.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(resolutions, 1, "exactly one resolution event");
    let decision: String = conn
        .query_row(
            "SELECT decision FROM permission WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    let winner = if r1.is_ok() { "allow" } else { "deny" };
    assert_eq!(decision, winner, "the durable decision matches the winner");
}
