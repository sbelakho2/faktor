//! Durable wedged-session invariants (`doctor --deep`): residue that neither
//! a live drive nor a durable recovery path can move forward.
//!
//! [`SessionHandle::recover_all`](crate::SessionHandle::recover_all) closes
//! active turn records and reconciles expired permissions at every restart,
//! and the queue runner resumes a recorded turn only from a non-terminal
//! queue row. Doctor reads the store from its OWN process: it owns no live
//! drives and no permission waiters, so its question is purely durable —
//! which rows can no process ever move again? The scan answers with typed
//! issues:
//!
//! - `active_turn_without_drive` — an op-active session carrying an active
//!   turn record with no resumable drive (no non-terminal `prompt_queue`
//!   row, no `pending` permission and no `running` tool run naming the turn
//!   op). Recovery closes such a record; a residue means recovery never ran
//!   or failed.
//! - `ownerless_pending_permission` — a `decision = 'pending'` permission
//!   row. The waiter is process-local, so a reader that is not the daemon
//!   that parked it (doctor, or a restarted daemon) can never resolve it:
//!   the row blocks the session until it is expired/denied.
//! - `applied_run_without_verification` — a terminal tool run that declared
//!   `effect = applied` while it carries a durable workspace-write marker
//!   (`postcondition`/legacy `expected_hash`), or its session is mid-turn
//!   (an interrupted active record) or its task is completion-relevant —
//!   and neither the task's revision has a verification record nor the task
//!   has an integration record. Read-only applied effects (`read_file`
//!   defaults to `applied`) carry none of those markers and are NOT
//!   unaccounted writes; flagging them would make `doctor --deep` fail on
//!   every healthy store.
//!
//! The scan is READ-ONLY and bounded: at most [`MAX_WEDGE_DETAILS`] issue
//! lines are materialized (the counters stay exact; `truncated` says more
//! remain), pending-permission rows are capped by
//! [`faktor_store::MAX_PENDING_PERMISSION_SCAN`], each session page of
//! applied runs is capped by [`MAX_SESSION_APPLIED_RUNS`], and every read
//! uses an existing index (`turn_record`, `prompt_queue`, `permission`,
//! `tool_run(session_id, status)`, `verification_record(task_id, id)`).

use std::collections::HashSet;
use std::sync::Arc;

use faktor_core::state::AgentState;
use faktor_core::Result as CoreResult;
use faktor_store::ToolRunRow;

use crate::SessionManager;

/// Bound on materialized wedge issue lines. Counters stay exact, so doctor
/// still counts every issue; only the printed details are capped.
pub const MAX_WEDGE_DETAILS: usize = 64;

/// Bound on the terminal applied tool-run page examined per session.
const MAX_SESSION_APPLIED_RUNS: usize = 64;

/// One typed wedged-session residue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWedgeIssue {
    /// Stable issue vocabulary (`active_turn_without_drive`,
    /// `ownerless_pending_permission`, `applied_run_without_verification`).
    pub kind: &'static str,
    pub detail: String,
}

/// The read-only wedge scan result. `issues` holds at most
/// [`MAX_WEDGE_DETAILS`] typed lines; the counters are exact.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionWedgeScan {
    /// Active `turn_record` rows examined.
    pub active_turns: u64,
    /// Op-active sessions whose active record has no resumable drive.
    pub active_turns_without_drive: u64,
    /// `pending` permission rows across all sessions (exact).
    pub pending_permissions: u64,
    /// Pending permission rows with no live waiter in this process (today:
    /// every row the scan reads in a process that is not the parking daemon).
    pub ownerless_pending_permissions: u64,
    /// Terminal applied runs that are durable-write/mid-turn/completion
    /// candidates.
    pub applied_write_runs: u64,
    /// Candidates with neither a verification record for the task revision
    /// nor an integration record for the task.
    pub applied_runs_without_verification: u64,
    pub issues: Vec<SessionWedgeIssue>,
    /// True when more issues exist than the detail bound can print.
    pub truncated: bool,
}

/// The op-active session states (the SAME set the recovery/turn code treats
/// as an in-flight operation): a durable active turn record in one of these
/// states must be owned by a live drive or a resumable record.
fn is_op_active_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Preparing
            | AgentState::BuildingContext
            | AgentState::WaitingForModel
            | AgentState::Streaming
            | AgentState::ToolRequested
            | AgentState::WaitingForPermission
            | AgentState::ExecutingTool
            | AgentState::Validating
            | AgentState::UpdatingMemory
    )
}

fn push_issue(scan: &mut SessionWedgeScan, kind: &'static str, detail: String) {
    if scan.issues.len() < MAX_WEDGE_DETAILS {
        scan.issues.push(SessionWedgeIssue { kind, detail });
    } else {
        scan.truncated = true;
    }
}

impl SessionManager {
    /// The read-only durable wedge scan (see the module docs). Errors are
    /// store failures only; hostile rows are decoded through the same typed
    /// mappers every other scan uses (a corrupt row fails loudly, never a
    /// silent skip).
    pub fn session_wedge_invariants(self: &Arc<Self>) -> CoreResult<SessionWedgeScan> {
        let store = self.store();
        let mut scan = SessionWedgeScan::default();

        // Pending permissions first: the active-turn drive test below reads
        // the same set (a pending permission for the turn op IS a resumable
        // record; the ownerless-permission issue is reported separately).
        let pending = store
            .all_pending_permissions()
            .map_err(crate::map_store_err)?;
        let pending_ops: HashSet<(u64, u64)> = pending
            .rows
            .iter()
            .map(|p| (p.session_id.raw(), p.op_id.raw()))
            .collect();

        // (1) Active turns with no live drive and no resumable record.
        let active_turns = store.all_active_turns().map_err(crate::map_store_err)?;
        let mut active_sessions: HashSet<u64> = HashSet::new();
        for turn in &active_turns {
            scan.active_turns += 1;
            active_sessions.insert(turn.session_id.raw());
            let Some(session) = store
                .get_session(turn.session_id)
                .map_err(crate::map_store_err)?
            else {
                continue;
            };
            if !is_op_active_state(session.state) {
                continue;
            }
            let queue_drive = store
                .queue_op_ids(turn.session_id)
                .map_err(crate::map_store_err)?
                .contains(&turn.turn_op_id);
            let permission_drive =
                pending_ops.contains(&(turn.session_id.raw(), turn.turn_op_id.raw()));
            let tool_drive = store
                .pending_tool_runs(turn.session_id)
                .map_err(crate::map_store_err)?
                .iter()
                .any(|r| r.op_id == turn.turn_op_id);
            if queue_drive || permission_drive || tool_drive {
                continue;
            }
            scan.active_turns_without_drive += 1;
            push_issue(
                &mut scan,
                "active_turn_without_drive",
                format!(
                    "active turn record {} of session {} (op {}) sits in durable state {:?} with no \
                     resumable drive: no non-terminal prompt_queue row, no pending permission and no \
                     running tool run names the turn op; recovery closes this record — if no live \
                     daemon owns it, the residue blocks every new prompt and must be recovered",
                    turn.id, turn.session_id, turn.turn_op_id, session.state
                ),
            );
        }

        // (2) Pending permissions with no live waiter. The waiter lives only
        // in the parking daemon's process; this reader owns none, so every
        // durable pending row is ownerless here.
        scan.pending_permissions = pending.total;
        for p in &pending.rows {
            scan.ownerless_pending_permissions += 1;
            push_issue(
                &mut scan,
                "ownerless_pending_permission",
                format!(
                    "pending permission {} of session {} (op {}, expires_ms {}) has no live waiter \
                     in this process; the waiter is process-local, so a reader that is not the \
                     daemon that parked it (doctor, or a restarted daemon) can never resolve it — \
                     the session stays parked until the row is expired or denied",
                    p.id, p.session_id, p.op_id, p.expires_ms
                ),
            );
        }
        if pending.total > pending.rows.len() as u64 {
            scan.ownerless_pending_permissions = pending.total;
            scan.truncated = true;
        }

        // (3) Applied write runs with no verification/integration record.
        let session_ids = store.session_ids().map_err(crate::map_store_err)?;
        for sid in session_ids {
            let Some(session) = store.get_session(sid).map_err(crate::map_store_err)? else {
                continue;
            };
            let runs = store
                .session_applied_tool_runs(sid, MAX_SESSION_APPLIED_RUNS)
                .map_err(crate::map_store_err)?;
            if runs.is_empty() {
                continue;
            }
            let task = store
                .get_task(sid, session.task_id)
                .map_err(crate::map_store_err)?;
            let completion_relevant = task
                .as_ref()
                .is_some_and(|t| t.state.is_completion_relevant());
            let mid_turn = active_sessions.contains(&sid.raw());
            let candidates: Vec<&ToolRunRow> = runs
                .iter()
                .filter(|r| {
                    // Durable write markers: the postcondition of a
                    // workspace-write tool (or a legacy VerifyHash expected
                    // hash). A read-only `applied` effect (read_file's
                    // default) carries none and is never an unaccounted
                    // write. A mid-turn or completion-relevant session makes
                    // every applied effect an obligation of that turn.
                    r.postcondition.is_some()
                        || r.expected_hash.is_some()
                        || completion_relevant
                        || mid_turn
                })
                .collect();
            if candidates.is_empty() {
                continue;
            }
            // Coverage: a verification record for the task's revision (or
            // the revision the completion consumed — completion bumps the
            // row exactly once) OR an integration record for the task.
            let verified = match &task {
                Some(t) => store
                    .verification_record_list_by_task(t.task_id)
                    .map_err(crate::map_store_err)?
                    .iter()
                    .any(|r| r.revision.raw() >= t.revision.raw().saturating_sub(1)),
                None => false,
            };
            let integrated = if verified {
                false
            } else {
                match self.get_session(sid) {
                    Ok(Some(handle)) => handle
                        .ledger_integration_record_for_task(session.task_id.raw())?
                        .is_some(),
                    Ok(None) => false,
                    // The session's ledger cannot be read (a torn journal is
                    // already reported by the journal-consistency scan): the
                    // integration record cannot be PROVEN, so the applied
                    // effect stays unaccounted — never a silent pass.
                    Err(e) => {
                        tracing::warn!(
                            session = %sid,
                            "wedge scan: integration ledger unreadable: {e}"
                        );
                        false
                    }
                }
            };
            if verified || integrated {
                continue;
            }
            for run in candidates {
                scan.applied_write_runs += 1;
                scan.applied_runs_without_verification += 1;
                push_issue(
                    &mut scan,
                    "applied_run_without_verification",
                    format!(
                        "tool run {} of session {} (op {}, tool {:?}) declared effect applied \
                         with a durable workspace write, and task {} revision {} has no \
                         verification record and no integration record; the applied bytes are \
                         unaccounted — verify the task revision or record the integration",
                        run.id,
                        sid,
                        run.op_id,
                        run.tool,
                        session.task_id,
                        task.as_ref().map(|t| t.revision.raw()).unwrap_or(0),
                    ),
                );
            }
        }

        Ok(scan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::id::OpId;
    use std::sync::Arc;

    fn manager() -> (tempfile::TempDir, Arc<SessionManager>) {
        let dir = tempfile::tempdir().unwrap();
        let m = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
            .expect("open");
        (dir, m)
    }

    fn session(m: &Arc<SessionManager>) -> crate::SessionHandle {
        let ws = m.create_workspace("/w").unwrap();
        m.create_session(ws, "t", "p", "m").unwrap()
    }

    /// A session admitted into `Preparing` (active record) with no queue,
    /// permission or tool row is the exact no-drive residue recovery closes.
    #[test]
    fn active_turn_without_drive_is_flagged() {
        let (_d, m) = manager();
        let s = session(&m);
        s.submit_prompt("never driven", &[]).unwrap();
        let scan = m.session_wedge_invariants().unwrap();
        assert_eq!(scan.active_turns, 1);
        assert_eq!(scan.active_turns_without_drive, 1, "{scan:?}");
        assert!(
            scan.issues
                .iter()
                .any(|i| i.kind == "active_turn_without_drive" && i.detail.contains("session 1")),
            "{scan:?}"
        );
    }

    /// A queued prompt row for the turn op IS a resumable drive: the queue
    /// runner re-admits/resumes it, so it is NOT a wedge.
    #[test]
    fn active_turn_with_a_queue_row_is_not_a_wedge() {
        let (_d, m) = manager();
        let s = session(&m);
        s.submit_prompt("one", &[]).unwrap();
        // A busy session queues the second prompt; the first prompt's op is
        // the active record and the second op owns the queue row. Give the
        // ACTIVE turn its own queue row by raw insert (the shape of an
        // admitted queue-driven turn).
        let record = s.active_turn_record().unwrap().unwrap();
        m.store()
            .enqueue_prompt(s.id(), record.turn_op_id, "owned", &[], None, None, None, 1)
            .unwrap();
        let scan = m.session_wedge_invariants().unwrap();
        assert_eq!(scan.active_turns_without_drive, 0, "{scan:?}");
    }

    /// A durable pending permission has no live waiter in the scan's
    /// process: typed ownerless issue.
    #[test]
    fn ownerless_pending_permission_is_flagged() {
        use faktor_core::event::EventKind;
        use faktor_core::state::AgentState;
        let (_d, m) = manager();
        let s = session(&m);
        let receipt = s.submit_prompt("x", &[]).unwrap();
        // A permission request is a ToolRequested hop: reach Streaming the
        // same way the turn driver does before asking.
        for (kind, state) in [
            (EventKind::ContextPrepared, AgentState::BuildingContext),
            (EventKind::ModelStarted, AgentState::WaitingForModel),
            (EventKind::ModelChunkReceived, AgentState::Streaming),
        ] {
            s.append_event(kind, state, None, None).unwrap();
        }
        s.request_permission(
            receipt.op_id,
            &faktor_core::capability::Capability::ReadWorkspace {
                path: "/w/a".into(),
            },
        )
        .unwrap();
        let scan = m.session_wedge_invariants().unwrap();
        assert_eq!(scan.pending_permissions, 1, "{scan:?}");
        assert_eq!(scan.ownerless_pending_permissions, 1, "{scan:?}");
        assert!(
            scan.issues
                .iter()
                .any(|i| i.kind == "ownerless_pending_permission"),
            "{scan:?}"
        );
        // The pending permission IS the active turn's resumable record: the
        // no-drive issue must not double-report it.
        assert_eq!(scan.active_turns_without_drive, 0, "{scan:?}");
    }

    /// An applied workspace write (durable postcondition) with no
    /// verification record for the task revision is an unaccounted effect.
    #[test]
    fn applied_write_without_verification_is_flagged() {
        let (_d, m) = manager();
        let s = session(&m);
        let op = m.try_next_op_id().unwrap();
        m.store()
            .start_tool_run(
                s.id(),
                op,
                "write_file",
                serde_json::json!({"path": "a.txt", "content": "x"}),
                serde_json::json!({"strategy": "mark_unknown"}),
                None,
                None,
            )
            .unwrap();
        m.store()
            .record_tool_postcondition(
                s.id(),
                op,
                &serde_json::json!({
                    "workspace_id": 1,
                    "worktree_id": 1,
                    "relative_path": "a.txt",
                    "expected_hash": "ab".repeat(32),
                }),
            )
            .unwrap();
        m.store()
            .finish_tool_run(s.id(), op, "completed", "applied")
            .unwrap();
        let scan = m.session_wedge_invariants().unwrap();
        assert_eq!(scan.applied_write_runs, 1, "{scan:?}");
        assert_eq!(scan.applied_runs_without_verification, 1, "{scan:?}");
        assert!(
            scan.issues
                .iter()
                .any(|i| i.kind == "applied_run_without_verification"),
            "{scan:?}"
        );
    }

    /// A read-only applied effect (no postcondition, no task, no active
    /// turn) is NOT an unaccounted write.
    #[test]
    fn read_only_applied_run_is_not_flagged() {
        let (_d, m) = manager();
        let s = session(&m);
        let op = m.try_next_op_id().unwrap();
        m.store()
            .start_tool_run(
                s.id(),
                op,
                "read_file",
                serde_json::json!({"path": "a.txt"}),
                serde_json::json!({"strategy": "idempotent"}),
                None,
                None,
            )
            .unwrap();
        m.store()
            .finish_tool_run(s.id(), op, "completed", "applied")
            .unwrap();
        let scan = m.session_wedge_invariants().unwrap();
        assert_eq!(scan.applied_write_runs, 0, "{scan:?}");
        assert_eq!(scan.applied_runs_without_verification, 0, "{scan:?}");
    }

    /// A fresh idle session with no residue is clean.
    #[test]
    fn idle_session_is_clean() {
        let (_d, m) = manager();
        let _s = session(&m);
        let scan = m.session_wedge_invariants().unwrap();
        assert_eq!(scan, SessionWedgeScan::default());
    }

    /// Op ids referenced only to keep the typed helper honest.
    #[test]
    fn op_id_roundtrip_sanity() {
        assert_eq!(OpId::new(7).raw(), 7);
    }
}
