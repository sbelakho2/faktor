//! Campaign (h): agent-runtime op-active recovery terminalization atomicity.
//!
//! `AgentRuntime::recover`'s op-active path finishes each durable `running`
//! `tool_run` row with ONE store transaction
//! (`Store::finish_recovered_tool_run_and_event`): the expected-state verify,
//! the terminal row update, the `RecoveryApplied` event, the journal sequence
//! and the session state commit together — after the batch's landing state
//! was reached by the sweep's one lawful `CrashDetected` transition. This
//! campaign certifies the RUNTIME layer (its sibling `recovery_txn` campaign
//! certifies the session sweep over the same store command): a crash at any
//! durability boundary reopens on exactly the declared class, and every
//! restart converges to the SAME durable world as the uninterrupted runtime
//! sweep — never a terminal row without its event and never an event without
//! its terminal row.
//!
//! Rigid recipe: a real session driven to `ExecutingTool` with two running
//! tool rows (`MarkUnknown`, file-free), the in-process manager dropped and
//! the store reopened (the daemon-restart condition: no live-turn registry),
//! then `recover` with one crash boundary armed.
//!
//! Boundaries:
//! - `recovery_runtime.before_txn.rolled_back` (`ev_precommit` 0): the
//!   landing `CrashDetected` transition rolled back — the sweep never
//!   reached a row transaction;
//! - `recovery_runtime.before_txn.committed` (`ev_committed` 0): the landing
//!   state is durable and no per-row transaction ran yet;
//! - `recovery_runtime.row0.*` / `row1.*` (`session_command_*` 0/1): the
//!   crash fired inside the first or second per-row transaction.
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
use faktor_session::SessionManager;
use faktor_store::CrashArm;

use super::{check_equals, BoundarySpec, Campaign, CrashClass, WorldState};

/// Seeds of the full [fault]-gated campaign (the scenario is deterministic;
/// seeds only repeat the certification).
pub const FULL_SEEDS: u64 = 16;
/// Seeds of the normal-mode smoke run.
pub const SMOKE_SEEDS: u64 = 2;

const ROWS: usize = 2;

/// (name, class, seam point, seam ordinal)
const BOUNDARIES: &[(&str, CrashClass, &str, u64)] = &[
    (
        "recovery_runtime.before_txn.rolled_back",
        CrashClass::PreOp,
        "ev_precommit",
        0,
    ),
    (
        "recovery_runtime.before_txn.committed",
        CrashClass::FullyCommitted,
        "ev_committed",
        0,
    ),
    (
        "recovery_runtime.row0.side_row",
        CrashClass::PreOp,
        "session_command_side_row",
        0,
    ),
    (
        "recovery_runtime.row0.precommit",
        CrashClass::PreOp,
        "session_command_precommit",
        0,
    ),
    (
        "recovery_runtime.row0.committed",
        CrashClass::FullyCommitted,
        "session_command_committed",
        0,
    ),
    (
        "recovery_runtime.row1.side_row",
        CrashClass::PreOp,
        "session_command_side_row",
        1,
    ),
    (
        "recovery_runtime.row1.precommit",
        CrashClass::PreOp,
        "session_command_precommit",
        1,
    ),
    (
        "recovery_runtime.row1.committed",
        CrashClass::FullyCommitted,
        "session_command_committed",
        1,
    ),
];

static BOUNDARY_SPECS: &[BoundarySpec] = &[
    BoundarySpec {
        name: "recovery_runtime.before_txn.rolled_back",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery_runtime.before_txn.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "recovery_runtime.row0.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery_runtime.row0.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery_runtime.row0.committed",
        class: CrashClass::FullyCommitted,
    },
    BoundarySpec {
        name: "recovery_runtime.row1.side_row",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery_runtime.row1.precommit",
        class: CrashClass::PreOp,
    },
    BoundarySpec {
        name: "recovery_runtime.row1.committed",
        class: CrashClass::FullyCommitted,
    },
];

/// The crashed fixture: a two-row op-active residue.
struct Fixture {
    dir: tempfile::TempDir,
    session: SessionId,
}

/// Build the residue on a fresh throwaway dir: a real session driven to
/// `ExecutingTool` with two running `MarkUnknown` tool runs (the same
/// permission hop + `ToolStarted` shape the runtime's tool batch uses). No
/// file verification is involved, so the whole batch fails honestly and
/// lands `FailedRecoverable` (the deterministic recipe).
fn build(_seed: u64) -> Result<Fixture, String> {
    let dir = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    let manager = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .map_err(|e| e.to_string())?;
    let ws = manager.create_workspace("/w").map_err(|e| e.to_string())?;
    let handle = manager
        .create_session(ws, "recovery-runtime", "fake", "m")
        .map_err(|e| e.to_string())?;
    let receipt = handle.submit_prompt("x", &[]).map_err(|e| e.to_string())?;
    crate::to_streaming(&handle, receipt.op_id);
    for i in 0..ROWS {
        let perm = handle
            .request_permission(
                receipt.op_id,
                &Capability::ReadWorkspace { path: ".".into() },
            )
            .map_err(|e| e.to_string())?;
        handle
            .resolve_permission(perm.id, PermissionDecision::Allow)
            .map_err(|e| e.to_string())?;
        let op = manager.try_next_op_id().map_err(|e| e.to_string())?;
        let meta = OpMeta::new(
            op,
            handle.id(),
            Deadline::at(manager.now_ms() + 60_000),
            RetryPolicy::default(),
            CancellationToken::new(),
            RecoveryStrategy::MarkUnknown,
            manager.now_ms(),
        );
        handle
            .start_tool_run(meta, "read_file", serde_json::json!({ "row": i }))
            .map_err(|e| e.to_string())?;
    }
    let session = handle.id();
    drop(handle);
    // Drop the in-process manager: its live-turn registry is gone, which is
    // exactly the daemon-restart condition the sweep must see (a registered
    // turn token would make recovery defer as a live driver).
    drop(manager);
    Ok(Fixture { dir, session })
}

/// Reopen the store (daemon restart) and read back the session's running ops
/// in row order.
fn reopen(
    dir: &std::path::Path,
    session: SessionId,
) -> Result<(Arc<SessionManager>, Vec<OpId>), String> {
    let manager = SessionManager::open(dir.join("store"), dir.join("cas"), true)
        .map_err(|e| e.to_string())?;
    let handle = manager
        .get_session(session)
        .map_err(|e| e.to_string())?
        .ok_or("session vanished")?;
    let ops = handle
        .pending_tool_runs()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| r.op_id)
        .collect();
    Ok((manager, ops))
}

fn runtime(manager: Arc<SessionManager>) -> Arc<faktor_agent::AgentRuntime> {
    crate::test_agent(manager, vec![], Arc::new(crate::AlwaysAllow))
}

/// Dump the durable world the campaign compares: session state, each row's
/// running/terminal status, and the `RecoveryApplied` journal (state + the
/// payload, with the store-global `op_id` replaced by the deterministic row
/// index). `CrashDetected` events are EXCLUDED: a restart legitimately
/// journals one per sweep, so repeated restart annotations are not a durable
/// divergence.
fn dump(
    manager: &Arc<SessionManager>,
    session: SessionId,
    ops: &[OpId],
    prefix: &str,
) -> Result<WorldState, String> {
    let handle = manager
        .get_session(session)
        .map_err(|e| e.to_string())?
        .ok_or("session vanished")?;
    let mut lines = Vec::new();
    lines.push(format!(
        "{prefix}state:{:?}",
        handle.state().map_err(|e| e.to_string())?
    ));
    let running: Vec<OpId> = handle
        .pending_tool_runs()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| r.op_id)
        .collect();
    for (i, op) in ops.iter().enumerate() {
        lines.push(format!(
            "{prefix}row:{i}:{}",
            if running.contains(op) {
                "running"
            } else {
                "terminal"
            }
        ));
    }
    for e in handle.events_range(1, None).map_err(|e| e.to_string())? {
        if e.kind == EventKind::CrashDetected {
            continue;
        }
        let idx = e
            .op_id
            .and_then(|o| ops.iter().position(|x| *x == o))
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".into());
        let payload = if e.kind == EventKind::RecoveryApplied {
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
    Ok(WorldState { lines })
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
        "recovery_runtime.before_txn.rolled_back" => ResidueExpect {
            state: AgentState::ExecutingTool,
            rows: all_running,
            applied: [0, 0],
        },
        "recovery_runtime.before_txn.committed"
        | "recovery_runtime.row0.side_row"
        | "recovery_runtime.row0.precommit" => ResidueExpect {
            state: AgentState::FailedRecoverable,
            rows: all_running,
            applied: [0, 0],
        },
        "recovery_runtime.row0.committed"
        | "recovery_runtime.row1.side_row"
        | "recovery_runtime.row1.precommit" => ResidueExpect {
            state: AgentState::FailedRecoverable,
            rows: ["terminal", "running"],
            applied: [1, 0],
        },
        "recovery_runtime.row1.committed" => ResidueExpect {
            state: AgentState::FailedRecoverable,
            rows: terminal,
            applied: [1, 1],
        },
        other => panic!("unknown recovery runtime boundary {other:?}"),
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
        if (row == "running") != (n == 0) {
            return Err(format!(
                "boundary {name}: row {i} is {row} with {n} RecoveryApplied events — \
                 a row and its event must be durable together or neither"
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
    let (manager, ops) = reopen(&dir, fixture.session)?;
    {
        let runtime = runtime(manager.clone());
        runtime.deps().session.store().crash_arm(CrashArm {
            point: seam,
            ordinal: *ordinal,
        });
        let caught = catch_unwind(AssertUnwindSafe(|| {
            let _ = runtime.recover();
        }));
        super::expect_crash_fired(caught)?;
        drop(runtime);
    }
    drop(manager);

    // Reopen: capture the declared residue, then run the restart sweep.
    let (manager, _) = reopen(&dir, fixture.session)?;
    let mut lines = dump(&manager, fixture.session, &ops, "r:")?.lines;
    let runtime = runtime(manager.clone());
    runtime.recover().map_err(|e| e.to_string())?;
    // The restart sweep is idempotent: a further sweep re-litigates nothing
    // (the transcript repair, if it ran, is not a tool_run/event change).
    let handle = manager
        .get_session(fixture.session)
        .map_err(|e| e.to_string())?
        .ok_or("session vanished")?;
    let seq = handle
        .last_event_seq()
        .map_err(|e| e.to_string())?
        .ok_or("no journal")?;
    let second = runtime.recover().map_err(|e| e.to_string())?;
    if second.iter().any(|r| !r.crashed_ops.is_empty()) {
        return Err(format!(
            "boundary {}: the second restart sweep re-litigated a crashed op",
            boundary.name
        ));
    }
    if handle.last_event_seq().map_err(|e| e.to_string())? != Some(seq) {
        return Err(format!(
            "boundary {}: the second restart sweep appended events",
            boundary.name
        ));
    }
    drop(runtime);
    lines.extend(dump(&manager, fixture.session, &ops, "f:")?.lines);
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
    // The converged world must equal the uninterrupted runtime reference
    // world: row+event pairs and journal content identical, restart-only
    // CrashDetected annotations excluded.
    check_equals(&converged, reference, boundary)
}

pub fn campaign() -> Campaign {
    Campaign {
        name: "recovery-runtime-terminalization-atomic-txn",
        boundaries: BOUNDARY_SPECS,
        reference,
        crash_run,
        check,
    }
}

fn reference(seed: u64) -> Result<WorldState, String> {
    let fixture = build(seed)?;
    let dir = fixture.dir.path().to_path_buf();
    let (manager, ops) = reopen(&dir, fixture.session)?;
    let runtime = runtime(manager.clone());
    runtime.recover().map_err(|e| e.to_string())?;
    let world = dump(&manager, fixture.session, &ops, "f:")?;
    drop(runtime);
    drop(manager);
    drop(fixture.dir);
    Ok(world)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn smoke_recovery_runtime_terminalization_boundaries() {
    let c = campaign();
    let checks = super::run_campaign(&c, SMOKE_SEEDS).expect("smoke must pass");
    assert_eq!(checks, SMOKE_SEEDS * c.boundaries.len() as u64);
}

#[test]
#[ignore = "[fault] agent-runtime recovery row+event terminalization crashes at every durability boundary, 16 seeds"]
fn full_recovery_runtime_terminalization_boundaries() {
    let c = campaign();
    let checks = super::run_campaign(&c, FULL_SEEDS).expect("full campaign must pass");
    assert_eq!(checks, FULL_SEEDS * c.boundaries.len() as u64);
}
