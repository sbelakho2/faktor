//! Campaign (g): abort crash atomicity.
//!
//! `SessionHandle::abort` cancels each tracked op with ONE durable command:
//! a tool op's terminal `tool_run` row and its `ToolCancelled` event commit
//! in the SAME transaction (`Store::finish_tool_run_and_event`); a turn op
//! journals `Failed` through the validated append path. This campaign
//! certifies that a crash at every per-op durability boundary — and at the
//! separate `TurnCompleted` -> `ReadyForNextTurn` transition that follows —
//! reopens on the declared residue class, with no row-without-event and no
//! event-without-row, and that the restart sweep converges every remaining
//! running row to exactly one row+event pair.
//!
//! The batch has three tracked ops (turn, tool 0, tool 1); abort iterates
//! by op id, so the per-op command sequence (and therefore each seam
//! ordinal) is deterministic.
//!
//! Boundaries:
//! - `abort.after_turn_failed` (`ev_committed` 0): the turn's `Failed`
//!   event committed before any tool transaction;
//! - `abort.row0.*` / `abort.row1.*` (`session_command_*` 0/1): inside the
//!   first or second tool row's cancellation command;
//! - `abort.turn_end.rolled_back` / `.committed` (`ev_precommit` /
//!   `ev_committed` 1): the SEPARATE turn-end transition, rolled back or
//!   durable.
//!
//! Unlike the txn campaigns this cannot converge to the abort reference
//! world byte-for-byte: abort's in-memory op registry does not survive a
//! restart, so the residue of a rolled-back per-op command is finished by
//! recovery (`RecoveryApplied`) rather than by a re-issued abort. The
//! class check therefore certifies the residue contract and the final
//! pairing invariant (exactly one terminal row and exactly one terminal
//! journal event per op), not reference equality.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use faktor_core::cancellation::CancellationToken;
use faktor_core::capability::{Capability, PermissionDecision};
use faktor_core::event::EventKind;
use faktor_core::id::{OpId, SessionId};
use faktor_core::op::{OpMeta, RecoveryStrategy};
use faktor_core::retry::RetryPolicy;
use faktor_core::state::AgentState;
use faktor_core::time::Deadline;
use faktor_session::{SessionHandle, SessionManager};
use faktor_store::CrashArm;

use super::{BoundarySpec, Campaign, CrashClass, WorldState};

/// Seeds of the full [fault]-gated campaign (the scenario is deterministic;
/// seeds only repeat the certification).
pub const FULL_SEEDS: u64 = 16;
/// Seeds of the normal-mode smoke run.
pub const SMOKE_SEEDS: u64 = 2;

const ROWS: usize = 2;

/// (name, class, seam point, seam ordinal)
const BOUNDARIES: &[(&str, CrashClass, &str, u64)] = &[
    (
        "abort.after_turn_failed",
        CrashClass::FullyCommitted,
        "ev_committed",
        0,
    ),
    (
        "abort.row0.side_row",
        CrashClass::PreOp,
        "session_command_side_row",
        0,
    ),
    (
        "abort.row0.precommit",
        CrashClass::PreOp,
        "session_command_precommit",
        0,
    ),
    (
        "abort.row0.committed",
        CrashClass::FullyCommitted,
        "session_command_committed",
        0,
    ),
    (
        "abort.row1.side_row",
        CrashClass::PreOp,
        "session_command_side_row",
        1,
    ),
    (
        "abort.row1.precommit",
        CrashClass::PreOp,
        "session_command_precommit",
        1,
    ),
    (
        "abort.row1.committed",
        CrashClass::FullyCommitted,
        "session_command_committed",
        1,
    ),
    (
        "abort.turn_end.rolled_back",
        CrashClass::PreOp,
        "ev_precommit",
        1,
    ),
    (
        "abort.turn_end.committed",
        CrashClass::FullyCommitted,
        "ev_committed",
        1,
    ),
];

static BOUNDARY_SPECS: &[BoundarySpec] = &[
    BoundarySpec {
        name: "abort.after_turn_failed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "abort.row0.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "abort.row0.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "abort.row0.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "abort.row1.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "abort.row1.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "abort.row1.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "abort.turn_end.rolled_back",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "abort.turn_end.committed",
        class: CrashClass::FullyCommitted,
    },
];

struct Fixture {
    dir: tempfile::TempDir,
    manager: Arc<SessionManager>,
    handle: SessionHandle,
    sid: SessionId,
    turn: OpId,
    rows: [OpId; ROWS],
}

fn build(seed: u64) -> Result<Fixture, String> {
    let dir = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    let manager = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .map_err(|e| e.to_string())?;
    let ws = manager.create_workspace("/w").map_err(|e| e.to_string())?;
    let handle = manager
        .create_session(ws, &format!("abort{seed}"), "fake", "m")
        .map_err(|e| e.to_string())?;
    let turn = handle
        .submit_prompt("x", &[])
        .map_err(|e| e.to_string())?
        .op_id;
    for (kind, state) in [
        (EventKind::ContextPrepared, AgentState::BuildingContext),
        (EventKind::ModelStarted, AgentState::WaitingForModel),
        (EventKind::ModelChunkReceived, AgentState::Streaming),
    ] {
        handle
            .append_event(kind, state, Some(turn), None)
            .map_err(|e| e.to_string())?;
    }
    let req = handle
        .request_permission(
            turn,
            &Capability::ReadWorkspace {
                path: "/w/a".into(),
            },
        )
        .map_err(|e| e.to_string())?;
    handle
        .resolve_permission(req.id, PermissionDecision::Allow)
        .map_err(|e| e.to_string())?;
    let mut rows = [OpId::new(1); ROWS];
    for (i, slot) in rows.iter_mut().enumerate() {
        let op = manager.try_next_op_id().map_err(|e| e.to_string())?;
        let meta = OpMeta::new(
            op,
            handle.id(),
            Deadline::at(manager.now_ms() + 60_000),
            RetryPolicy::default(),
            CancellationToken::new(),
            RecoveryStrategy::None,
            manager.now_ms(),
        );
        handle
            .start_tool_run(meta, "read_file", serde_json::json!({ "row": i }))
            .map_err(|e| e.to_string())?;
        *slot = op;
    }
    let sid = handle.id();
    Ok(Fixture {
        dir,
        manager,
        handle,
        sid,
        turn,
        rows,
    })
}

/// Dump the durable world: session state, each tool row's running/terminal
/// status and the turn's/per-row terminal event counts. `CrashDetected`
/// restart annotations are excluded (each sweep legitimately journals one).
fn dump(handle: &SessionHandle, turn: OpId, rows: &[OpId; ROWS], prefix: &str) -> WorldState {
    let mut lines = Vec::new();
    let state = handle.state().expect("session state");
    lines.push(format!("{prefix}state:{state:?}"));
    let running: Vec<OpId> = handle
        .pending_tool_runs()
        .expect("pending runs")
        .into_iter()
        .map(|r| r.op_id)
        .collect();
    for (i, op) in rows.iter().enumerate() {
        let status = if running.contains(op) {
            "running"
        } else {
            "terminal"
        };
        lines.push(format!("{prefix}row:{i}:{status}"));
    }
    for e in handle.events_range(1, None).expect("events") {
        if e.kind == EventKind::CrashDetected {
            continue;
        }
        let idx = if e.op_id == Some(turn) {
            "turn".to_string()
        } else {
            e.op_id
                .and_then(|o| rows.iter().position(|x| *x == o))
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".into())
        };
        let payload = if e.kind == EventKind::ToolCancelled {
            // Op ids come from the durable global sequence and are not
            // reproducible across store instances; the event's op identity
            // is already the deterministic row index in the `ev:` prefix.
            let mut p = e.payload.unwrap_or_default();
            if let Some(obj) = p.as_object_mut() {
                obj.remove("op_id");
            }
            format!(":{p}")
        } else {
            String::new()
        };
        lines.push(format!("{prefix}ev:{idx}:{:?}{payload}", e.kind));
    }
    WorldState { lines }
}

/// The declared durable residue per boundary.
struct ResidueExpect {
    state: AgentState,
    rows: [&'static str; ROWS],
    tool_cancelled: [u32; ROWS],
    recovery_applied: [u32; ROWS],
    turn_failed: u32,
    turn_completed: u32,
}

fn residue_expect(name: &str) -> ResidueExpect {
    let terminal = ["terminal", "terminal"];
    let all_running = ["running", "running"];
    let mut e = ResidueExpect {
        state: AgentState::Cancelled,
        rows: all_running,
        tool_cancelled: [0, 0],
        recovery_applied: [0, 0],
        turn_failed: 1,
        turn_completed: 0,
    };
    match name {
        "abort.after_turn_failed" | "abort.row0.side_row" | "abort.row0.precommit" => {}
        "abort.row0.committed" | "abort.row1.side_row" | "abort.row1.precommit" => {
            e.rows = ["terminal", "running"];
            e.tool_cancelled = [1, 0];
        }
        "abort.row1.committed" | "abort.turn_end.rolled_back" => {
            e.rows = terminal;
            e.tool_cancelled = [1, 1];
        }
        "abort.turn_end.committed" => {
            e.state = AgentState::ReadyForNextTurn;
            e.rows = terminal;
            e.tool_cancelled = [1, 1];
            e.turn_completed = 1;
        }
        other => panic!("unknown abort boundary {other:?}"),
    }
    e
}

fn line_value<'a>(lines: &'a [String], prefix: &str, key: &str) -> Option<&'a str> {
    let needle = format!("{prefix}{key}");
    lines
        .iter()
        .find(|l| l.starts_with(&needle))
        .map(|l| &l[needle.len()..])
}

fn count_events(lines: &[String], prefix: &str, idx: &str, kind: &str) -> usize {
    let needle = format!("{prefix}ev:{idx}:{kind}");
    lines.iter().filter(|l| l.starts_with(&needle)).count()
}

fn assert_residue(name: &str, lines: &[String]) -> Result<(), String> {
    let e = residue_expect(name);
    let state = line_value(lines, "r:", "state:").ok_or("residue state missing")?;
    if state != format!("{:?}", e.state) {
        return Err(format!(
            "boundary {name}: residue state {state:?} != {:?}",
            e.state
        ));
    }
    for i in 0..ROWS {
        let row = line_value(lines, "r:", &format!("row:{i}:")).ok_or("residue row missing")?;
        if row != e.rows[i] {
            return Err(format!(
                "boundary {name}: residue row {i} is {row:?} != {:?}",
                e.rows[i]
            ));
        }
        let tc = count_events(lines, "r:", &i.to_string(), "ToolCancelled");
        let ra = count_events(lines, "r:", &i.to_string(), "RecoveryApplied");
        if tc != e.tool_cancelled[i] as usize {
            return Err(format!(
                "boundary {name}: row {i} carries {tc} ToolCancelled events, expected {}",
                e.tool_cancelled[i]
            ));
        }
        if ra != e.recovery_applied[i] as usize {
            return Err(format!(
                "boundary {name}: row {i} carries {ra} RecoveryApplied events, expected {}",
                e.recovery_applied[i]
            ));
        }
        if e.rows[i] == "running" && tc + ra > 0 {
            return Err(format!(
                "boundary {name}: running row {i} already carries a terminal event"
            ));
        }
        if e.rows[i] == "terminal" && tc + ra != 1 {
            return Err(format!(
                "boundary {name}: terminal row {i} must carry exactly one terminal event (tc={tc}, ra={ra})"
            ));
        }
    }
    let failed = count_events(lines, "r:", "turn", "Failed");
    if failed != e.turn_failed as usize {
        return Err(format!(
            "boundary {name}: {failed} Failed events for the turn, expected {}",
            e.turn_failed
        ));
    }
    let completed = count_events(lines, "r:", "turn", "TurnCompleted");
    if completed != e.turn_completed as usize {
        return Err(format!(
            "boundary {name}: {completed} TurnCompleted events, expected {}",
            e.turn_completed
        ));
    }
    Ok(())
}

/// The post-restart invariant: every tool row terminal with exactly one
/// terminal journal event, at most one `Failed` and one `TurnCompleted` for
/// the turn, and a legal promptable landing state.
fn assert_final(name: &str, lines: &[String]) -> Result<(), String> {
    if count_events(lines, "f:", "turn", "Failed") != 1 {
        return Err(format!(
            "{name}: the turn must carry exactly one Failed event"
        ));
    }
    let completed = count_events(lines, "f:", "turn", "TurnCompleted");
    if completed > 1 {
        return Err(format!("{name}: {completed} TurnCompleted events"));
    }
    let state = line_value(lines, "f:", "state:").ok_or("final state missing")?;
    if !matches!(state, "Cancelled" | "ReadyForNextTurn") {
        return Err(format!("{name}: illegal post-abort landing {state:?}"));
    }
    if (state == "ReadyForNextTurn") != (completed == 1) {
        return Err(format!(
            "{name}: TurnCompleted/ReadyForNextTurn are not paired (state={state:?}, completed={completed})"
        ));
    }
    for i in 0..ROWS {
        let row = line_value(lines, "f:", &format!("row:{i}:")).ok_or("final row missing")?;
        let tc = count_events(lines, "f:", &i.to_string(), "ToolCancelled");
        let ra = count_events(lines, "f:", &i.to_string(), "RecoveryApplied");
        if row != "terminal" {
            return Err(format!("{name}: row {i} is still {row:?}"));
        }
        if tc + ra != 1 {
            return Err(format!(
                "{name}: row {i} carries {tc} ToolCancelled + {ra} RecoveryApplied, expected exactly one"
            ));
        }
    }
    Ok(())
}

fn crash_run(seed: u64, boundary: &BoundarySpec) -> Result<WorldState, String> {
    let (_, _, seam, ordinal) = BOUNDARIES
        .iter()
        .find(|(name, ..)| *name == boundary.name)
        .ok_or_else(|| format!("unknown boundary {:?}", boundary.name))?;
    let fixture = build(seed)?;
    let dir = fixture.dir.path().to_path_buf();
    let sid = fixture.sid;
    let turn = fixture.turn;
    let rows = fixture.rows;
    fixture.manager.store().crash_arm(CrashArm {
        point: seam,
        ordinal: *ordinal,
    });
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = fixture.handle.abort(None);
    }));
    super::expect_crash_fired(caught)?;
    drop(fixture.handle);
    drop(fixture.manager);
    // `fixture.dir` stays alive until the end of the run: the reopen below
    // reads the same on-disk store.

    let manager = SessionManager::open(dir.join("store"), dir.join("cas"), true)
        .map_err(|e| e.to_string())?;
    let handle = manager
        .get_session(sid)
        .map_err(|e| e.to_string())?
        .ok_or("session vanished")?;
    let mut lines = dump(&handle, turn, &rows, "r:").lines;
    handle.recover_all().map_err(|e| e.to_string())?;
    // The restart sweep is idempotent: a further sweep appends nothing.
    let seq = handle
        .last_event_seq()
        .map_err(|e| e.to_string())?
        .ok_or("no journal")?;
    if handle.recover_all().map_err(|e| e.to_string())?.applied {
        return Err(format!(
            "boundary {}: the second restart sweep changed durable state",
            boundary.name
        ));
    }
    if handle.last_event_seq().map_err(|e| e.to_string())? != Some(seq) {
        return Err(format!(
            "boundary {}: the second restart sweep appended events",
            boundary.name
        ));
    }
    lines.extend(dump(&handle, turn, &rows, "f:").lines);
    Ok(WorldState { lines })
}

fn check(
    recovered: &WorldState,
    reference: &WorldState,
    boundary: &BoundarySpec,
) -> Result<(), String> {
    let residue: Vec<String> = recovered
        .lines
        .iter()
        .filter(|l| l.starts_with("r:"))
        .cloned()
        .collect();
    let final_lines: Vec<String> = recovered
        .lines
        .iter()
        .filter(|l| l.starts_with("f:"))
        .cloned()
        .collect();
    assert_residue(boundary.name, &residue)?;
    assert_final(boundary.name, &final_lines)?;
    // The uninterrupted abort reference must satisfy the same final
    // pairing invariant (self-check of the certification contract).
    assert_final("<reference>", &reference.lines)?;
    Ok(())
}

pub fn campaign() -> Campaign {
    Campaign {
        name: "abort-terminalization-atomic-txn",
        boundaries: BOUNDARY_SPECS,
        reference,
        crash_run,
        check,
    }
}

fn reference(seed: u64) -> Result<WorldState, String> {
    let fixture = build(seed)?;
    fixture.handle.abort(None).map_err(|e| e.to_string())?;
    let world = dump(&fixture.handle, fixture.turn, &fixture.rows, "f:");
    drop(fixture.handle);
    drop(fixture.manager);
    drop(fixture.dir);
    Ok(world)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_abort_terminalization_boundaries() {
    let c = campaign();
    let checks = super::run_campaign(&c, SMOKE_SEEDS).expect("smoke must pass");
    assert_eq!(checks, SMOKE_SEEDS * c.boundaries.len() as u64);
}

#[test]
#[ignore = "[fault] abort row+event terminalization crashes at every durability boundary and the turn end, 16 seeds"]
fn full_abort_terminalization_boundaries() {
    let c = campaign();
    let checks = super::run_campaign(&c, FULL_SEEDS).expect("full campaign must pass");
    assert_eq!(checks, FULL_SEEDS * c.boundaries.len() as u64);
}
