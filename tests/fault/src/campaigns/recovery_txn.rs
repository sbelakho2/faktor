//! Campaign (f): crash-recovery terminalization atomicity.
//!
//! `SessionHandle::recover_all` finishes each durable `running` tool_run row
//! in ONE store transaction together with its `RecoveryApplied` event, the
//! journal sequence and the session state
//! (`Store::finish_recovered_tool_run_and_event`). This campaign certifies
//! that a crash at any durability boundary of that command reopens on
//! exactly the declared class's durable world, and that a restart's sweep
//! converges every remaining running row to exactly ONE row+event pair —
//! never the pre-fix terminal-row-without-event that no later sweep
//! revisits.
//!
//! Rigid per-seed recipe: a real session driven to `ExecutingTool` with two
//! running tool rows whose recovery strategies are seed-derived and all
//! file-free (`None` / `MarkUnknown` / `Manual` / `Idempotent`), then
//! `recover_all` with one crash boundary armed.
//!
//! Boundaries:
//! - `recovery.before_sweep` (`ev_precommit`): the `CrashDetected`
//!   transition rolls back — the sweep never started;
//! - `recovery.before_txn` (`ev_committed`): `CrashDetected` is durable and
//!   no per-row transaction ran yet;
//! - `recovery.row0.*` / `recovery.row1.*` (`session_command_*`): the crash
//!   fired inside the first or second per-row transaction.
//!
//! The residue world (immediately after reopen, before the restart sweep)
//! carries the class contract; the converged world (after the restart
//! sweep) must EQUAL the uninterrupted reference world modulo repeated
//! `CrashDetected` restart annotations (excluded from the dump).

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

use super::{check_equals, BoundarySpec, Campaign, CrashClass, Lcg, WorldState};

/// Seeds of the full [fault]-gated campaign.
pub const FULL_SEEDS: u64 = 32;
/// Seeds of the normal-mode smoke run.
pub const SMOKE_SEEDS: u64 = 2;

const ROWS: usize = 2;

/// (name, class, seam point, seam ordinal)
const BOUNDARIES: &[(&str, CrashClass, &str, u64)] = &[
    (
        "recovery.before_sweep",
        CrashClass::PreOp,
        "ev_precommit",
        0,
    ),
    (
        "recovery.before_txn",
        CrashClass::FullyCommitted,
        "ev_committed",
        0,
    ),
    (
        "recovery.row0.side_row",
        CrashClass::PreOp,
        "session_command_side_row",
        0,
    ),
    (
        "recovery.row0.precommit",
        CrashClass::PreOp,
        "session_command_precommit",
        0,
    ),
    (
        "recovery.row0.committed",
        CrashClass::FullyCommitted,
        "session_command_committed",
        0,
    ),
    (
        "recovery.row1.side_row",
        CrashClass::PreOp,
        "session_command_side_row",
        1,
    ),
    (
        "recovery.row1.precommit",
        CrashClass::PreOp,
        "session_command_precommit",
        1,
    ),
    (
        "recovery.row1.committed",
        CrashClass::FullyCommitted,
        "session_command_committed",
        1,
    ),
];

static BOUNDARY_SPECS: &[BoundarySpec] = &[
    BoundarySpec {
        name: "recovery.before_sweep",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery.before_txn",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "recovery.row0.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery.row0.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery.row0.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "recovery.row1.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery.row1.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery.row1.committed",
        class: CrashClass::FullyCommitted,
    },
];

fn strategy(lcg: &mut Lcg) -> RecoveryStrategy {
    match lcg.below(4) {
        0 => RecoveryStrategy::None,
        1 => RecoveryStrategy::MarkUnknown,
        2 => RecoveryStrategy::Manual,
        _ => RecoveryStrategy::Idempotent,
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    manager: Arc<SessionManager>,
    handle: SessionHandle,
    sid: SessionId,
    ops: Vec<OpId>,
}

fn build(seed: u64) -> Result<Fixture, String> {
    let dir = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    let manager = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .map_err(|e| e.to_string())?;
    let ws = manager.create_workspace("/w").map_err(|e| e.to_string())?;
    let handle = manager
        .create_session(ws, "recovery", "fake", "m")
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
    let mut lcg = Lcg::new(seed ^ 0x5245_4356);
    let mut ops = Vec::new();
    for i in 0..ROWS {
        let op = manager.try_next_op_id().map_err(|e| e.to_string())?;
        let meta = OpMeta::new(
            op,
            handle.id(),
            Deadline::at(manager.now_ms() + 60_000),
            RetryPolicy::default(),
            CancellationToken::new(),
            strategy(&mut lcg),
            manager.now_ms(),
        );
        handle
            .start_tool_run(meta, "read_file", serde_json::json!({ "row": i }))
            .map_err(|e| e.to_string())?;
        ops.push(op);
    }
    let sid = handle.id();
    Ok(Fixture {
        dir,
        manager,
        handle,
        sid,
        ops,
    })
}

/// Dump the durable world the campaign compares: session state, each tool
/// row's running/terminal status, and the journal (kind, state, and the
/// `RecoveryApplied` payload). `CrashDetected` events are EXCLUDED: a
/// restart legitimately journals one per sweep, so repeated restart
/// annotations are not a durable-world divergence.
fn dump(handle: &SessionHandle, ops: &[OpId], prefix: &str) -> WorldState {
    let mut lines = Vec::new();
    let state = handle.state().expect("session state");
    lines.push(format!("{prefix}state:{state:?}"));
    let running: Vec<OpId> = handle
        .pending_tool_runs()
        .expect("pending runs")
        .into_iter()
        .map(|r| r.op_id)
        .collect();
    for (i, op) in ops.iter().enumerate() {
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
        let idx = e
            .op_id
            .and_then(|o| ops.iter().position(|x| *x == o))
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".into());
        let payload = if e.kind == EventKind::RecoveryApplied {
            // Op ids come from the durable global sequence and are not
            // reproducible across store instances; the event's op identity
            // is already the deterministic row index in the `ev:` prefix.
            e.payload
                .map(|mut p| {
                    if let Some(obj) = p.as_object_mut() {
                        obj.remove("op_id");
                    }
                    p.to_string()
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        lines.push(format!(
            "{prefix}ev:{idx}:{:?}:{:?}:{payload}",
            e.kind, e.state
        ));
    }
    WorldState { lines }
}

/// The declared durable residue per boundary (state, per-row status, and
/// per-row committed `RecoveryApplied` count).
struct ResidueExpect {
    state: AgentState,
    rows: [&'static str; ROWS],
    applied: [u32; ROWS],
}

fn residue_expect(name: &str) -> ResidueExpect {
    let terminal = ["terminal", "terminal"];
    let all_running = ["running", "running"];
    match name {
        "recovery.before_sweep" => ResidueExpect {
            state: AgentState::ExecutingTool,
            rows: all_running,
            applied: [0, 0],
        },
        "recovery.before_txn" | "recovery.row0.side_row" | "recovery.row0.precommit" => {
            ResidueExpect {
                state: AgentState::FailedRecoverable,
                rows: all_running,
                applied: [0, 0],
            }
        }
        "recovery.row0.committed" | "recovery.row1.side_row" | "recovery.row1.precommit" => {
            ResidueExpect {
                state: AgentState::FailedRecoverable,
                rows: ["terminal", "running"],
                applied: [1, 0],
            }
        }
        "recovery.row1.committed" => ResidueExpect {
            state: AgentState::FailedRecoverable,
            rows: terminal,
            applied: [1, 1],
        },
        other => panic!("unknown recovery boundary {other:?}"),
    }
}

fn line_value<'a>(lines: &'a [String], prefix: &str, key: &str) -> Option<&'a str> {
    let needle = format!("{prefix}{key}");
    lines
        .iter()
        .find(|l| l.starts_with(&needle))
        .map(|l| &l[needle.len()..])
}

fn count_events(lines: &[String], prefix: &str, idx: usize, kind: &str) -> usize {
    let needle = format!("{prefix}ev:{idx}:{kind}:");
    lines.iter().filter(|l| l.starts_with(&needle)).count()
}

fn assert_residue(name: &str, lines: &[String]) -> Result<(), String> {
    let expect = residue_expect(name);
    let state = line_value(lines, "r:", "state:").ok_or("residue state missing")?;
    if state != format!("{:?}", expect.state) {
        return Err(format!(
            "boundary {name}: residue state {state:?} != {:?}",
            expect.state
        ));
    }
    for i in 0..ROWS {
        let row = line_value(lines, "r:", &format!("row:{i}:")).ok_or("residue row missing")?;
        if row != expect.rows[i] {
            return Err(format!(
                "boundary {name}: residue row {i} is {row:?} != {:?}",
                expect.rows[i]
            ));
        }
        let n = count_events(lines, "r:", i, "RecoveryApplied");
        if n != expect.applied[i] as usize {
            return Err(format!(
                "boundary {name}: residue row {i} carries {n} RecoveryApplied events, expected {}",
                expect.applied[i]
            ));
        }
        if n > 0 && expect.rows[i] != "terminal" {
            return Err(format!(
                "boundary {name}: event without its terminal row ({i})"
            ));
        }
        if expect.rows[i] == "running" && n > 0 {
            return Err(format!(
                "boundary {name}: running row {i} already carries an event"
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
    fixture.manager.store().crash_arm(CrashArm {
        point: seam,
        ordinal: *ordinal,
    });
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = fixture.handle.recover_all();
    }));
    super::expect_crash_fired(caught)?;
    drop(fixture.handle);
    drop(fixture.manager);

    // Reopen: capture the declared residue, then run the restart sweep.
    let manager = SessionManager::open(dir.join("store"), dir.join("cas"), true)
        .map_err(|e| e.to_string())?;
    let handle = manager
        .get_session(fixture.sid)
        .map_err(|e| e.to_string())?
        .ok_or("session vanished")?;
    let mut lines = dump(&handle, &fixture.ops, "r:").lines;
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
    lines.extend(dump(&handle, &fixture.ops, "f:").lines);
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
    let converged = WorldState {
        lines: recovered
            .lines
            .iter()
            .filter(|l| l.starts_with("f:"))
            .cloned()
            .collect(),
    };
    assert_residue(boundary.name, &residue)?;
    // The converged world must equal the uninterrupted reference world
    // (same setup, same recovery decisions): row+event pairs and journal
    // content identical, restart-only CrashDetected annotations excluded.
    check_equals(&converged, reference, boundary)
}

pub fn campaign() -> Campaign {
    Campaign {
        name: "recovery-terminalization-atomic-txn",
        boundaries: BOUNDARY_SPECS,
        reference,
        crash_run,
        check,
    }
}

fn reference(seed: u64) -> Result<WorldState, String> {
    let fixture = build(seed)?;
    fixture.handle.recover_all().map_err(|e| e.to_string())?;
    let world = dump(&fixture.handle, &fixture.ops, "f:");
    drop(fixture.handle);
    drop(fixture.manager);
    drop(fixture.dir);
    Ok(world)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_recovery_terminalization_boundaries() {
    let c = campaign();
    let checks = super::run_campaign(&c, SMOKE_SEEDS).expect("smoke must pass");
    assert_eq!(checks, SMOKE_SEEDS * c.boundaries.len() as u64);
}

#[test]
#[ignore = "[fault] recovery row+event terminalization crashes at every durability boundary, 32 seeds"]
fn full_recovery_terminalization_boundaries() {
    let c = campaign();
    let checks = super::run_campaign(&c, FULL_SEEDS).expect("full campaign must pass");
    assert_eq!(checks, FULL_SEEDS * c.boundaries.len() as u64);
}
