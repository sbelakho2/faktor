//! Campaign (e): atomic session-command crash certification.
//!
//! Every session command that mutates a side table (permission / tool_run /
//! checkpoint / compaction) AND the journal runs in ONE SQLite transaction
//! with three deterministic durability boundaries:
//!
//! - `session_command_side_row`: the side row is written, the event is not
//!   yet (crash class `PreOp` — the whole command must roll back);
//! - `session_command_precommit`: the event is written, COMMIT is not yet
//!   (`PreOp`);
//! - `session_command_committed`: COMMIT returned, the ack was lost
//!   (`FullyCommitted` — the command must be durable).
//!
//! Rigid per-seed op recipe over ONE session of a real `faktor-store`:
//!
//! ```text
//! op 0  insert_permission_and_event (Idle -> WaitingForPermission)
//! op 1  resolve_permission_and_event (allow; -> ExecutingTool)
//! op 2  start_tool_run_and_event    (ExecutingTool)
//! op 3  finish_tool_run_and_event   (completed; -> Validating)
//! op 4  put_checkpoint_and_event    (Validating)
//! op 5  record_compaction_and_event (accepted/rejected; -> Validating)
//! ```
//!
//! Content is seed-derived and hostile-inclusive (empty payloads, multi-KiB
//! blobs, `i64::MIN`/`i64::MAX`, empty/long tool names and paths). Each op
//! derives its content from a fresh per-op LCG, so a replay from any durable
//! cursor reproduces byte-identical content.
//!
//! Certification per (seed, boundary): reopen the crashed store, prove the
//! durable world equals the expected prefix world of the declared class,
//! replay the op tail, and prove the recovered world equals the uninterrupted
//! reference world. Observability is the public reader surface (session
//! state, versioned journal events, permission decision, running tool runs,
//! checkpoints); finished tool_run/compaction rows are visible through their
//! journal event by construction, and the table-level old-or-new proof lives
//! in `faktor-store`'s `session_command_txn_tests`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use faktor_core::event::EventKind;
use faktor_core::id::{OpId, SessionId};
use faktor_core::state::AgentState;
use faktor_store::{CommandEvent, CrashArm, Store};

use super::{check_equals, BoundarySpec, Campaign, CrashClass, Lcg, WorldState};

/// Seeds of the full [fault]-gated campaign.
pub const FULL_SEEDS: u64 = 64;
/// Seeds of the normal-mode smoke run.
pub const SMOKE_SEEDS: u64 = 3;

const OPS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Permission = 0,
    Grant = 1,
    StartTool = 2,
    FinishTool = 3,
    Checkpoint = 4,
    Compaction = 5,
}

fn op_lcg(seed: u64, op: usize) -> Lcg {
    Lcg::new(seed ^ (op as u64).wrapping_mul(0x9E37_79B9_7F4A_C15F))
}

fn text(lcg: &mut Lcg, max: usize) -> String {
    let n = 1 + lcg.below(max as u64) as usize;
    let mut s = format!("seed{:x}", lcg.next_u64());
    while s.len() < n {
        s.push('c');
    }
    s
}

fn hex64(lcg: &mut Lcg) -> String {
    let mut s = String::new();
    while s.len() < 64 {
        s.push_str(&format!("{:016x}", lcg.next_u64()));
    }
    s.truncate(64);
    s
}

fn hostile_payload(lcg: &mut Lcg, op: usize) -> serde_json::Value {
    match lcg.below(4) {
        0 => serde_json::json!({ "op": op, "n": lcg.next_u64() }),
        1 => serde_json::json!({}),
        2 => serde_json::json!({ "blob": "x".repeat(1 + lcg.below(1500) as usize) }),
        _ => serde_json::json!([lcg.next_u64(), i64::MIN, i64::MAX]),
    }
}

fn event(lcg: &mut Lcg, kind: EventKind, state: AgentState, op: usize) -> CommandEvent {
    let payload = hostile_payload(lcg, op);
    CommandEvent {
        kind,
        state,
        op_id: Some(OpId::new(1)),
        ts_ms: 1_700_000_000_000 + lcg.below(1_000_000) as i64,
        payload: Some(payload),
        payload_ver: 1,
    }
}

fn setup(root: &Path, seed: u64) -> (Store, SessionId) {
    let store = Store::open(root.join("store"), true).expect("store opens");
    let ws = store.create_workspace("/w").expect("workspace");
    let row = store
        .create_session(ws, &format!("s{seed}"), "p", "m")
        .expect("session");
    (store, row.id)
}

fn exec_op(store: &Store, session: SessionId, seed: u64, op: Op, pid: &mut Option<i64>) {
    let i = op as usize;
    let mut lcg = op_lcg(seed, i);
    match op {
        Op::Permission => {
            let capability = format!("{{\"capability\":\"{}\"}}", text(&mut lcg, 200));
            let (id, expires_ms, _seq) = store
                .insert_permission_and_event(
                    session,
                    OpId::new(1),
                    &capability,
                    AgentState::Idle,
                    event(
                        &mut lcg,
                        EventKind::ToolRequested,
                        AgentState::WaitingForPermission,
                        i,
                    ),
                )
                .expect("permission");
            // Fresh store: this is the table's first row (the campaign's
            // replay fallback relies on the deterministic id).
            assert_eq!(id, 1, "first permission row id is deterministic");
            assert!(expires_ms > 0);
            *pid = Some(id);
        }
        Op::Grant => {
            store
                .resolve_permission_and_event(
                    pid.expect("permission id"),
                    session,
                    "allow",
                    AgentState::WaitingForPermission,
                    event(
                        &mut lcg,
                        EventKind::PermissionGranted,
                        AgentState::ExecutingTool,
                        i,
                    ),
                )
                .expect("grant");
        }
        Op::StartTool => {
            let (row_id, _seq) = store
                .start_tool_run_and_event(
                    session,
                    OpId::new(1),
                    &text(&mut lcg, 256),
                    serde_json::json!({ "path": text(&mut lcg, 128) }),
                    serde_json::json!({ "strategy": "none", "seed": seed }),
                    None,
                    Some(serde_json::json!({ "kind": "none" })),
                    AgentState::ExecutingTool,
                    event(
                        &mut lcg,
                        EventKind::ToolStarted,
                        AgentState::ExecutingTool,
                        i,
                    ),
                )
                .expect("tool start");
            assert!(row_id > 0);
        }
        Op::FinishTool => {
            store
                .finish_tool_run_and_event(
                    session,
                    OpId::new(1),
                    "completed",
                    "verified",
                    AgentState::ExecutingTool,
                    event(
                        &mut lcg,
                        EventKind::ToolCompleted,
                        AgentState::Validating,
                        i,
                    ),
                )
                .expect("tool finish");
        }
        Op::Checkpoint => {
            let (row_id, _seq) = store
                .put_checkpoint_and_event(
                    session,
                    lcg.below(1_000) as i64,
                    &text(&mut lcg, 200),
                    &hex64(&mut lcg),
                    &hex64(&mut lcg),
                    Some(&hex64(&mut lcg)),
                    AgentState::Validating,
                    event(
                        &mut lcg,
                        EventKind::CheckpointCreated,
                        AgentState::Validating,
                        i,
                    ),
                )
                .expect("checkpoint");
            assert!(row_id > 0);
        }
        Op::Compaction => {
            let before = 10_000 + lcg.below(1_000_000) as i64;
            let after = lcg.below(before as u64) as i64;
            let target = lcg.below(before as u64) as i64;
            let accepted = lcg.below(2) == 0;
            let kind = if accepted {
                EventKind::ContextCompacted
            } else {
                EventKind::CompactRejected
            };
            store
                .record_compaction_and_event(
                    session,
                    before,
                    after,
                    target,
                    accepted,
                    &text(&mut lcg, 128),
                    AgentState::Validating,
                    event(&mut lcg, kind, AgentState::Validating, i),
                )
                .expect("compaction");
        }
    }
}

fn op_of(idx: usize) -> Op {
    match idx {
        0 => Op::Permission,
        1 => Op::Grant,
        2 => Op::StartTool,
        3 => Op::FinishTool,
        4 => Op::Checkpoint,
        5 => Op::Compaction,
        _ => panic!("op index {idx} out of the rigid recipe"),
    }
}

fn run_ops(
    store: &Store,
    session: SessionId,
    seed: u64,
    from: usize,
    to: usize,
    pid: &mut Option<i64>,
) {
    for idx in from..to {
        exec_op(store, session, seed, op_of(idx), pid);
    }
}

fn dump_world(store: &Store, session: SessionId) -> WorldState {
    let mut lines = Vec::new();
    let row = store.get_session(session).unwrap().unwrap();
    lines.push(format!("state:{:?}", row.state));
    for (ev, ver) in store.events_versioned_range(session, 0, None).unwrap() {
        lines.push(format!(
            "ev:{}:{:?}:{:?}:{}:{}:{}",
            ev.seq.raw(),
            ev.kind,
            ev.state,
            ev.op_id.map(|o| o.raw()).unwrap_or(0),
            ver,
            ev.payload.map(|p| p.to_string()).unwrap_or_default()
        ));
    }
    match store.permission_decision(1).unwrap() {
        Some(decision) => lines.push(format!("perm:1:{decision}")),
        None => lines.push("perm:-".into()),
    }
    for r in store.pending_tool_runs(session).unwrap() {
        lines.push(format!(
            "toolrun:{}:{}:{}",
            r.op_id.raw(),
            r.status,
            r.effect_status
        ));
    }
    for c in store.checkpoints_of(session).unwrap() {
        lines.push(format!(
            "cp:{}:{}:{}:{}:{}:{}:{}",
            c.sequence,
            c.path,
            c.before_hash,
            c.after_hash,
            c.before_exists,
            c.after_exists,
            c.restored_ms.is_some()
        ));
    }
    WorldState { lines }
}

/// (name, crashed op index, class, seam point, seam ordinal)
const BOUNDARIES: &[(&str, usize, CrashClass, &str, u64)] = &[
    (
        "perm.side_row",
        0,
        CrashClass::PreOp,
        "session_command_side_row",
        0,
    ),
    (
        "perm.precommit",
        0,
        CrashClass::PreOp,
        "session_command_precommit",
        0,
    ),
    (
        "perm.committed",
        0,
        CrashClass::FullyCommitted,
        "session_command_committed",
        0,
    ),
    (
        "grant.side_row",
        1,
        CrashClass::PreOp,
        "session_command_side_row",
        1,
    ),
    (
        "grant.precommit",
        1,
        CrashClass::PreOp,
        "session_command_precommit",
        1,
    ),
    (
        "grant.committed",
        1,
        CrashClass::FullyCommitted,
        "session_command_committed",
        1,
    ),
    (
        "start.side_row",
        2,
        CrashClass::PreOp,
        "session_command_side_row",
        2,
    ),
    (
        "start.precommit",
        2,
        CrashClass::PreOp,
        "session_command_precommit",
        2,
    ),
    (
        "start.committed",
        2,
        CrashClass::FullyCommitted,
        "session_command_committed",
        2,
    ),
    (
        "finish.side_row",
        3,
        CrashClass::PreOp,
        "session_command_side_row",
        3,
    ),
    (
        "finish.precommit",
        3,
        CrashClass::PreOp,
        "session_command_precommit",
        3,
    ),
    (
        "finish.committed",
        3,
        CrashClass::FullyCommitted,
        "session_command_committed",
        3,
    ),
    (
        "checkpoint.side_row",
        4,
        CrashClass::PreOp,
        "session_command_side_row",
        4,
    ),
    (
        "checkpoint.precommit",
        4,
        CrashClass::PreOp,
        "session_command_precommit",
        4,
    ),
    (
        "checkpoint.committed",
        4,
        CrashClass::FullyCommitted,
        "session_command_committed",
        4,
    ),
    (
        "compaction.side_row",
        5,
        CrashClass::PreOp,
        "session_command_side_row",
        5,
    ),
    (
        "compaction.precommit",
        5,
        CrashClass::PreOp,
        "session_command_precommit",
        5,
    ),
    (
        "compaction.committed",
        5,
        CrashClass::FullyCommitted,
        "session_command_committed",
        5,
    ),
];

static BOUNDARY_SPECS: &[BoundarySpec] = &[
    BoundarySpec {
        name: "perm.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "perm.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "perm.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "grant.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "grant.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "grant.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "start.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "start.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "start.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "finish.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "finish.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "finish.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "checkpoint.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "checkpoint.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "checkpoint.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "compaction.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "compaction.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "compaction.committed",
        class: CrashClass::FullyCommitted,
    },
];

fn reference(seed: u64) -> Result<WorldState, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (store, session) = setup(dir.path(), seed);
    let mut pid = None;
    run_ops(&store, session, seed, 0, OPS, &mut pid);
    let world = dump_world(&store, session);
    drop(store);
    Ok(world)
}

/// Durable prefix oracle: the expected world after exactly `completed` ops.
fn prefix_world(seed: u64, completed: usize) -> WorldState {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, session) = setup(dir.path(), seed);
    let mut pid = None;
    run_ops(&store, session, seed, 0, completed, &mut pid);
    let world = dump_world(&store, session);
    drop(store);
    world
}

fn crash_run(seed: u64, boundary: &BoundarySpec) -> Result<WorldState, String> {
    let (_, crashed_op, class, point, ordinal) = BOUNDARIES
        .iter()
        .find(|(name, ..)| *name == boundary.name)
        .ok_or_else(|| format!("unknown boundary {:?}", boundary.name))?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;

    // --- crashed run: fresh store, armed seam, ops through the crashed op.
    let mut pid: Option<i64> = None;
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let (store, session) = setup(dir.path(), seed);
        store.crash_arm(CrashArm {
            point,
            ordinal: *ordinal,
        });
        run_ops(&store, session, seed, 0, crashed_op + 1, &mut pid);
    }));
    super::expect_crash_fired(caught)?;

    // --- reopen: the durable world must be EXACTLY the expected prefix.
    let reopened = Store::open(dir.path().join("store"), true).map_err(|e| e.to_string())?;
    let sessions = reopened.list_sessions(None).map_err(|e| e.to_string())?;
    assert_eq!(sessions.len(), 1, "recipe creates exactly one session");
    let session = sessions[0].id;
    let durable = dump_world(&reopened, session);
    let (completed, replay_from) = match class {
        CrashClass::PreOp => (*crashed_op, *crashed_op),
        CrashClass::FullyCommitted => (crashed_op + 1, crashed_op + 1),
        CrashClass::Ambiguous => unreachable!("campaign (e) declares no ambiguous boundary"),
    };
    let expected = prefix_world(seed, completed);
    if durable != expected {
        return Err(format!(
            "durable world after crash diverges from the declared {} class ({completed} completed ops):\n{}",
            class_name(class),
            durable.diff(&expected)
        ));
    }

    // --- recovery contract: replay the tail from the durable cursor. The
    // permission id is 1 whenever op 0 committed (fresh store, one
    // permission row); when op 0 rolled back, replaying it re-derives the id.
    let mut replay_pid = pid.or(Some(1));
    run_ops(&reopened, session, seed, replay_from, OPS, &mut replay_pid);
    Ok(dump_world(&reopened, session))
}

fn class_name(c: &CrashClass) -> &'static str {
    match c {
        CrashClass::PreOp => "PreOp",
        CrashClass::FullyCommitted => "FullyCommitted",
        CrashClass::Ambiguous => "Ambiguous",
    }
}

pub fn campaign() -> Campaign {
    Campaign {
        name: "session-command-atomic-txn",
        boundaries: BOUNDARY_SPECS,
        reference,
        crash_run,
        check: check_equals,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_session_command_boundaries() {
    let c = campaign();
    let checks = super::run_campaign(&c, SMOKE_SEEDS).expect("smoke must pass");
    assert_eq!(checks, SMOKE_SEEDS * c.boundaries.len() as u64);
}

#[test]
#[ignore = "[fault] atomic session commands crash at every durability boundary, 64 seeds"]
fn full_session_command_boundaries() {
    let c = campaign();
    let checks = super::run_campaign(&c, FULL_SEEDS).expect("full campaign must pass");
    assert_eq!(checks, FULL_SEEDS * c.boundaries.len() as u64);
}
