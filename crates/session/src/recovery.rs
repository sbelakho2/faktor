//! Crash recovery (architecture spec section 7): unfinished operations are
//! reconstructed from durable state, never blindly re-run.
//!
//! `recover_all` scans durable `tool_run` rows still in `running` status and
//! applies their recorded recovery identity:
//!
//! - A durable workspace-write postcondition (the modern shape) — verify the
//!   CURRENT file bytes through the workspace handle's anchored,
//!   symlink-bounded relative open with the post-open identity net: never a
//!   hash of a raw pathname.
//! - `VerifyHash { path, expected }` — a LEGACY strategy: `path` is only a
//!   containment CLAIM. The session's durable workspace root is resolved,
//!   the claim is proven inside it (canonicalized; `..` climbs, symlinks
//!   pointing out, different/missing roots refused), converted to a
//!   normalized workspace-relative path, and durably recorded as a modern
//!   postcondition on the running row BEFORE any read. Verification then
//!   runs through the handle. A claim that cannot be proven — or a stored
//!   postcondition the handle refuses — classifies the effect
//!   Unknown/NeedsUserInput, never Verified, and no outside file is read.
//! - `MarkUnknown` — record `effect_status = unknown` and force verification
//!   instead of re-running.
//! - `Idempotent` — safe to re-run; mark failed so the scheduler may rerun.
//! - `Manual` — never re-run automatically; requires a human.
//! - `None` — no recovery action.
//!
//! Before the crash state is decided (and before the session is interactable
//! again), the sweep reconciles durable PERMISSION expiry (P1-E): a pending
//! permission whose `expires_ms` passed while no live waiter existed is
//! terminalized (`decision = 'expired'`) and journaled as
//! `EventKind::PermissionExpired` in one transaction, landing where an
//! explicit Deny would — so a restarted session can never stay parked in
//! `WaitingForPermission` forever.
//!
//! The sweep is idempotent: finished rows are never re-scanned, and a second
//! `recover_all` appends nothing (including no second expiry event).
//!
//! Terminalization atomicity: each scanned row's terminal `tool_run` update
//! and its `RecoveryApplied` journal event commit in ONE transaction
//! (`Store::finish_recovered_tool_run_and_event`, with the session expected
//! state re-verified inside it), together with the journal sequence and
//! session state. A crash at any durability boundary of that command leaves
//! exactly the old world (row still `running`, no event) or exactly the new
//! one (terminal row AND its event) — never the pre-fix residue of a
//! terminal row whose missing event no later sweep revisits. `CrashDetected`
//! is the one preceding transition and the legacy-postcondition migration
//! write remains a documented lawful midpoint (a crash there resumes through
//! the modern relative identity exactly once).

use std::path::Path;

use faktor_core::event::EventKind;
use faktor_core::hash::FileHash;
use faktor_core::id::{OpId, SessionId};
use faktor_core::op::{EffectStatus, FilePostcondition, RecoveryStrategy};
use faktor_core::state::AgentState;
use faktor_core::WorkspaceIdentity;
use faktor_fs::{legacy_relative_path_within, workspace_relative_path_rejection, WorkspaceHandle};
use faktor_store::ToolRunRow;

use crate::handle::{is_op_active, SessionHandle};
use crate::process::OwnedProcess;
use crate::{effect_str, SessionError};

/// What recovery decided for one crashed operation.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryAction {
    /// Deterministic FS op: the file matches the expected hash — it
    /// completed before the crash.
    Verified {
        expected: FileHash,
        actual: FileHash,
    },
    /// The file does not match (or does not exist): the op truly never ran.
    NotApplied {
        expected: FileHash,
        actual: Option<FileHash>,
    },
    /// `MarkUnknown`: effects unknown, verification forced before reuse.
    UnknownEffect,
    /// `Idempotent`: safe to re-run.
    RerunAllowed,
    /// `Manual`: a human must decide.
    NeedsHuman,
    /// `None`: no recovery action.
    NoAction,
}

/// The state a crashed session may honestly land on. `FailedRecoverable` is
/// preferred; the core machine does not permit it from `ToolRequested` or
/// `WaitingForPermission`, so recovery falls back to `WaitingForPermission`
/// (the permission request is durable and resumable from that state only;
/// `NeedsUserInput` cannot be resolved back to `ExecutingTool`), and finally
/// stays put.
fn crash_target(current: AgentState) -> AgentState {
    let mut m = faktor_core::state::StateMachine::new(current);
    for t in [
        AgentState::FailedRecoverable,
        AgentState::WaitingForPermission,
        AgentState::NeedsUserInput,
    ] {
        if m.transition(t).is_ok() {
            return t;
        }
    }
    current
}

/// One recovered tool run.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredOp {
    pub op_id: OpId,
    pub tool: String,
    /// Final durable `tool_run.status`.
    pub status: String,
    pub effect: EffectStatus,
    pub action: RecoveryAction,
}

/// The outcome of a recovery sweep over one session.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryReport {
    pub session_id: SessionId,
    /// Session state after recovery.
    pub state: AgentState,
    pub crashed_ops: Vec<RecoveredOp>,
    /// Children owned at recovery time (post-restart: presumed dead or
    /// re-parented; reported so the runtime cannot silently lose them).
    pub orphans: Vec<OwnedProcess>,
    /// True when an op-active session had no running tool rows: the turn
    /// itself was interrupted mid-flight.
    pub interrupted_turn: bool,
    /// True when tool rows were pending while the journal said the session
    /// was idle/suspended/terminal — the rows are fixed, the state stands.
    pub contradiction: bool,
    /// True when any durable change was made.
    pub applied: bool,
}

fn parse_recovery(row: &ToolRunRow) -> Result<RecoveryStrategy, SessionError> {
    serde_json::from_value(row.recovery.clone()).map_err(|e| {
        SessionError::Malformed(format!(
            "tool_run {} carries an invalid recovery strategy: {e}",
            row.op_id
        ))
    })
}

/// One file-verification outcome over the session's workspace handle.
enum FileVerification {
    /// The file was read and hashed through the handle's anchored relative
    /// open (post-open identity net included).
    Hashed(FileHash),
    /// The file does not exist under the workspace root: the write never
    /// landed.
    Missing,
    /// The handle refused the read — a hostile relative-path grammar, a
    /// `..`/symlink/reparse escape, an entry swapped after resolution, an
    /// unreadable root. The effect cannot be established and NO outside file
    /// was read.
    Refused,
}

/// Open the session's effective workspace root as a one-shot, watcher-less
/// handle (recovery's explicit scope). `Ok(None)` when the session has no
/// resolvable durable root: containment cannot be proven, so the caller
/// classifies Unknown/NeedsUserInput. A store failure stays a loud error.
fn open_session_workspace(
    s: &SessionHandle,
    identity: &WorkspaceIdentity,
) -> Result<Option<WorkspaceHandle>, SessionError> {
    let Some(root) = s
        .manager()
        .resolve_workspace_root(s.id())
        .map_err(SessionError::from)?
    else {
        return Ok(None);
    };
    match WorkspaceHandle::open_scoped(identity.workspace_id, root) {
        Ok(ws) => Ok(Some(ws)),
        Err(_) => Ok(None),
    }
}

/// Verify one workspace-relative path through the workspace handle. The
/// host-side relative grammar is refused FIRST; a read failure that is not
/// "absent" is a refusal (Unknown) — never a verification.
fn verify_through_handle(ws: &WorkspaceHandle, relative: &str) -> FileVerification {
    if workspace_relative_path_rejection(relative).is_some() {
        return FileVerification::Refused;
    }
    match ws.hash_file_streaming(Path::new(relative), None) {
        Ok((_bytes, actual)) => FileVerification::Hashed(actual),
        Err(e) if e.kind == faktor_core::ErrorKind::NotFound => FileVerification::Missing,
        Err(_) => FileVerification::Refused,
    }
}

fn classify_verification(
    expected: FileHash,
    outcome: FileVerification,
) -> (&'static str, EffectStatus, RecoveryAction) {
    match outcome {
        FileVerification::Hashed(actual) if actual == expected => (
            "completed",
            EffectStatus::Verified,
            RecoveryAction::Verified { expected, actual },
        ),
        FileVerification::Hashed(actual) => (
            "failed",
            EffectStatus::Failed,
            RecoveryAction::NotApplied {
                expected,
                actual: Some(actual),
            },
        ),
        FileVerification::Missing => (
            "failed",
            EffectStatus::Failed,
            RecoveryAction::NotApplied {
                expected,
                actual: None,
            },
        ),
        // Containment/read refused: the effect is unknown and a human must
        // decide — NEVER Verified, and no outside read happened.
        FileVerification::Refused => ("failed", EffectStatus::Unknown, RecoveryAction::NeedsHuman),
    }
}

/// One-time legacy migration (audit P1-F): the recorded pathname is only a
/// CLAIM. The session's durable workspace root is resolved, the claim is
/// canonicalized to PROVE it is inside that root, converted to a normalized
/// workspace-relative path, and durably recorded on the still-running row as
/// the modern [`FilePostcondition`] BEFORE any read — so a crash after this
/// write resumes through the handle-relative identity exactly once and never
/// re-derives capability from the legacy string. `Ok(None)` = containment
/// cannot be proven (`..` climbs, symlinks pointing out, different/missing
/// roots, hostile grammar): the caller classifies Unknown/NeedsUserInput.
fn migrate_legacy_verify_row(
    s: &SessionHandle,
    row: &ToolRunRow,
    legacy_path: &str,
    expected: FileHash,
) -> Result<Option<(FilePostcondition, WorkspaceHandle)>, SessionError> {
    let identity = s.identity()?;
    let Some(ws) = open_session_workspace(s, &identity)? else {
        return Ok(None);
    };
    let Some(relative) = legacy_relative_path_within(ws.root(), legacy_path) else {
        return Ok(None);
    };
    if workspace_relative_path_rejection(&relative).is_some() {
        return Ok(None);
    }
    let postcondition = FilePostcondition {
        workspace_id: identity.workspace_id,
        worktree_id: identity.worktree_id,
        relative_path: relative,
        expected_hash: expected,
    };
    let raw = serde_json::to_value(&postcondition)
        .map_err(|e| SessionError::Malformed(format!("legacy postcondition serialization: {e}")))?;
    s.record_tool_postcondition(row.op_id, &raw)?;
    Ok(Some((postcondition, ws)))
}

/// One applied recovery decision: the report row. The durable migration
/// evidence (`legacy_migrated_to`) is committed inside the row's
/// `RecoveryApplied` event by [`finish_recovered`], not carried here.
struct AppliedRecovery {
    op: RecoveredOp,
}

/// The stable action tag the `RecoveryApplied` payload journals.
fn action_tag(action: &RecoveryAction) -> &'static str {
    match action {
        RecoveryAction::Verified { .. } => "verified",
        RecoveryAction::NotApplied { .. } => "not_applied",
        RecoveryAction::UnknownEffect => "unknown_effect",
        RecoveryAction::RerunAllowed => "rerun_allowed",
        RecoveryAction::NeedsHuman => "needs_human",
        RecoveryAction::NoAction => "no_action",
    }
}

/// Finish one row durably and build its report entry. The terminal
/// `tool_run` update, the `RecoveryApplied` event and the journal
/// sequence/session state commit in ONE store transaction with `state`
/// re-verified inside it, so a crash can never leave a terminal row without
/// its event (or an event without its row).
fn finish_recovered(
    s: &SessionHandle,
    row: &ToolRunRow,
    status: &'static str,
    effect: EffectStatus,
    action: RecoveryAction,
    legacy_migrated_to: Option<String>,
    state: AgentState,
) -> Result<AppliedRecovery, SessionError> {
    let mut payload = serde_json::json!({
        "op_id": row.op_id.raw(),
        "tool": &row.tool,
        "status": status,
        "effect": effect_str(effect),
        "action": action_tag(&action),
    });
    if let Some(relative) = &legacy_migrated_to {
        // Durable evidence of the one-time legacy migration (audit P1-F):
        // the run was verified through this normalized workspace-relative
        // identity, never through the recorded raw pathname.
        payload["legacy_migrated_to"] = serde_json::json!(relative);
    }
    // The same typed payload gate the live append path uses: an undecodable
    // payload refuses BEFORE the transaction, so there is never a row
    // without its (valid) event.
    crate::payload::decode_payload(
        EventKind::RecoveryApplied,
        crate::payload::PAYLOAD_SCHEMA_V,
        Some(&payload),
    )?;
    s.manager()
        .store()
        .finish_recovered_tool_run_and_event(
            row.session_id,
            row.op_id,
            status,
            effect_str(effect),
            EventKind::RecoveryApplied,
            state,
            Some(payload),
        )
        .map_err(crate::map_store_err)?;
    Ok(AppliedRecovery {
        op: RecoveredOp {
            op_id: row.op_id,
            tool: row.tool.clone(),
            status: status.to_string(),
            effect,
            action,
        },
    })
}

fn apply_strategy(
    s: &SessionHandle,
    row: &ToolRunRow,
    strategy: &RecoveryStrategy,
    state: AgentState,
) -> Result<AppliedRecovery, SessionError> {
    // A durable postcondition is the modern recovery identity: verify it
    // through the handle FIRST — exactly like the agent runtime's sweep —
    // and consult the recovery column only when no postcondition exists.
    // This is also the crash-mid-migration resume: the migrated row carries
    // the postcondition while the legacy strategy is still in the column.
    if let Some(raw) = &row.postcondition {
        let pc: FilePostcondition = serde_json::from_value(raw.clone()).map_err(|e| {
            SessionError::Malformed(format!(
                "tool_run {} carries a corrupt postcondition: {e}",
                row.op_id
            ))
        })?;
        let identity = s.identity()?;
        // A postcondition naming a FOREIGN workspace is never verified
        // against this session's root.
        let Some(ws) = (pc.workspace_id == identity.workspace_id)
            .then(|| open_session_workspace(s, &identity))
            .transpose()?
            .flatten()
        else {
            return finish_recovered(
                s,
                row,
                "failed",
                EffectStatus::Unknown,
                RecoveryAction::NeedsHuman,
                None,
                state,
            );
        };
        let (status, effect, action) = classify_verification(
            pc.expected_hash,
            verify_through_handle(&ws, &pc.relative_path),
        );
        return finish_recovered(s, row, status, effect, action, None, state);
    }
    match strategy {
        RecoveryStrategy::VerifyHash { path, expected } => {
            // The expected_hash column is redundant durability: a mismatch is
            // tampering and must be loud.
            if let Some(col) = &row.expected_hash {
                if col != &expected.to_hex() {
                    return Err(SessionError::Malformed(format!(
                        "tool_run {} expected_hash column {} disagrees with strategy {}",
                        row.op_id,
                        col,
                        expected.to_hex()
                    )));
                }
            }
            let Some((postcondition, ws)) = migrate_legacy_verify_row(s, row, path, *expected)?
            else {
                // Containment cannot be proven: Unknown/NeedsUserInput, and
                // the legacy pathname was never read.
                return finish_recovered(
                    s,
                    row,
                    "failed",
                    EffectStatus::Unknown,
                    RecoveryAction::NeedsHuman,
                    None,
                    state,
                );
            };
            // Durable audit evidence: the run was verified through this
            // normalized workspace-relative identity, never the raw path.
            let migrated = Some(postcondition.relative_path.clone());
            let (status, effect, action) = classify_verification(
                *expected,
                verify_through_handle(&ws, &postcondition.relative_path),
            );
            finish_recovered(s, row, status, effect, action, migrated, state)
        }
        RecoveryStrategy::MarkUnknown => finish_recovered(
            s,
            row,
            "interrupted",
            EffectStatus::Unknown,
            RecoveryAction::UnknownEffect,
            None,
            state,
        ),
        RecoveryStrategy::Idempotent => finish_recovered(
            s,
            row,
            "failed",
            EffectStatus::Unknown,
            RecoveryAction::RerunAllowed,
            None,
            state,
        ),
        RecoveryStrategy::Manual => finish_recovered(
            s,
            row,
            "interrupted",
            EffectStatus::Unknown,
            RecoveryAction::NeedsHuman,
            None,
            state,
        ),
        RecoveryStrategy::None => finish_recovered(
            s,
            row,
            "interrupted",
            EffectStatus::Unknown,
            RecoveryAction::NoAction,
            None,
            state,
        ),
    }
}

impl SessionHandle {
    /// Recover this session: unfinished operations are reconstructed from
    /// durable state. Legacy `VerifyHash` pathnames are containment-proven,
    /// migrated to a workspace-relative postcondition and verified through
    /// the workspace handle; a claim that cannot be proven lands
    /// Unknown/NeedsUserInput, never Verified.
    pub fn recover_all(&self) -> faktor_core::Result<RecoveryReport> {
        let _guard = self.command_guard();
        let session_id = self.id;

        // P1-E, FIRST — before the crash state is decided and before the
        // session is interactable again: reconcile durable permission expiry.
        // A pending permission whose deadline passed while no live waiter
        // existed (a restart) is terminalized as `expired` and journaled as
        // `PermissionExpired` (one transaction), landing where an explicit
        // Deny would. Without this the machine can stay parked in
        // `WaitingForPermission` forever on a row no resolver may ever own.
        let expired = self.expire_pending_permissions_locked()?;
        if !expired.is_empty() {
            tracing::warn!(
                session = %session_id,
                expired = expired.expired.len(),
                "reconciled expired permissions at recovery"
            );
        }

        let current = self.state()?;
        let pending = self.pending_tool_runs()?;

        // Children owned when the world stopped: after a restart they are
        // presumed dead or deliberately re-parented by the OS. Report and
        // clear — the runtime must never pretend to own zombies.
        let orphans = self.processes().drain();

        let mut report = RecoveryReport {
            session_id,
            state: current,
            crashed_ops: Vec::new(),
            orphans,
            interrupted_turn: false,
            contradiction: false,
            applied: expired.event_seq.is_some(),
        };

        if pending.is_empty() {
            if is_op_active(current) {
                // The turn itself was interrupted (no tool row survives it).
                // Journal CrashDetected and land on the honest recovery target
                // so the agent may re-plan; never re-run the turn blindly.
                let target = crash_target(current);
                tracing::warn!(session = %session_id, state = ?current, target = ?target, "interrupted turn detected");
                self.transition_locked(
                    EventKind::CrashDetected,
                    target,
                    None,
                    Some(serde_json::json!({ "recovered_from": crate::state_tag(current) })),
                )?;
                report.state = target;
                report.interrupted_turn = true;
                report.applied = true;
            }
            return Ok(report);
        }

        // Pending tool runs: the journal alone cannot tell how far they got.
        let (crash_state, contradiction) = if is_op_active(current) {
            (crash_target(current), false)
        } else {
            // Idle/Suspended/terminal with running rows: the journal and the
            // ledger disagree. Fix the rows, keep the state, say so.
            (current, true)
        };
        report.contradiction = contradiction;

        self.transition_locked(
            EventKind::CrashDetected,
            crash_state,
            None,
            Some(serde_json::json!({
                "pending_ops": pending.len(),
                "contradiction": contradiction,
            })),
        )?;

        for row in &pending {
            let strategy = parse_recovery(row)?;
            // ONE transaction per row: terminal row + `RecoveryApplied`
            // event + session sequence/state (see the store command). The
            // event lands as part of the SAME transaction as the terminal
            // row — there is no later, separately crashable append.
            let applied = apply_strategy(self, row, &strategy, crash_state)?;
            let recovered = &applied.op;
            tracing::warn!(
                session = %session_id,
                op = %recovered.op_id,
                tool = %recovered.tool,
                action = action_tag(&recovered.action),
                "recovered crashed operation"
            );
            report.crashed_ops.push(applied.op);
        }

        report.state = crash_state;
        report.applied = true;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::tests::{session, test_manager};
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::event::EventKind;
    use faktor_core::id::WorkspaceId;
    use faktor_core::op::OpMeta;
    use faktor_core::time::Deadline;
    use std::sync::Arc;

    fn blake3_of(bytes: &[u8]) -> FileHash {
        FileHash::from(blake3::hash(bytes).into())
    }

    /// A session whose durable workspace row points at a REAL, freshly
    /// created root under `base`: containment is only provable against an
    /// existing canonical root, so file-verification tests need one.
    fn workspace_session(
        m: &Arc<crate::SessionManager>,
        base: &std::path::Path,
    ) -> (SessionHandle, std::path::PathBuf) {
        let root = base.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let ws = m.create_workspace(root.to_str().unwrap()).unwrap();
        let s = m.create_session(ws, "t", "ollama", "qwen3.8").unwrap();
        (s, root)
    }

    fn recovery_events(s: &SessionHandle) -> Vec<faktor_core::event::Event> {
        s.events_range(1, None).unwrap()
    }

    fn reopen(dir: &tempfile::TempDir) -> std::sync::Arc<crate::SessionManager> {
        crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap()
    }

    fn verified_effects(events: &[faktor_core::event::Event]) -> usize {
        events
            .iter()
            .filter(|e| {
                e.kind == EventKind::RecoveryApplied
                    && e.payload.as_ref().is_some_and(|p| {
                        p.get("effect").and_then(|v| v.as_str()) == Some("verified")
                    })
            })
            .count()
    }

    fn make_meta(
        s: &SessionHandle,
        m: &crate::SessionManager,
        recovery: RecoveryStrategy,
    ) -> (OpMeta, OpId) {
        let op = m.try_next_op_id().unwrap();
        let meta = OpMeta::new(
            op,
            s.id(),
            Deadline::at(m.now_ms() + 60_000),
            faktor_core::retry::RetryPolicy::default(),
            CancellationToken::new(),
            recovery,
            m.now_ms(),
        );
        (meta, op)
    }

    fn to_executing(s: &SessionHandle) -> crate::ops::PermissionRequest {
        s.submit_prompt("x", &[]).unwrap();
        s.append_event(
            EventKind::ContextPrepared,
            AgentState::BuildingContext,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ModelStarted,
            AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            None,
            None,
        )
        .unwrap();
        // A durable permission request puts the machine at WaitingForPermission
        // and is resumable after a crash.
        let turn_op = s.ops().all()[0];
        s.request_permission(
            turn_op,
            &faktor_core::capability::Capability::ReadWorkspace {
                path: "/w/a".into(),
            },
        )
        .unwrap()
    }

    /// A legacy path PROVEN inside the workspace migrates once to the
    /// normalized relative postcondition and verifies through the handle.
    #[test]
    fn legacy_verify_inside_workspace_migrates_and_verifies_through_the_handle() {
        let (_d, m) = test_manager();
        let (s, root) = workspace_session(&m, _d.path());
        to_executing(&s);
        let bytes = b"landed";
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("a.txt"), bytes).unwrap();
        let expected = blake3_of(bytes);
        let legacy = root.join("sub").join("a.txt").to_string_lossy().to_string();
        let (meta, op) = make_meta(
            &s,
            &m,
            RecoveryStrategy::VerifyHash {
                path: legacy,
                expected,
            },
        );
        s.start_tool_run(meta, "write_file", serde_json::json!({"path": "sub/a.txt"}))
            .unwrap();
        // "Crash": nothing else happens.
        let report = s.recover_all().unwrap();
        assert!(report.applied);
        assert!(!report.contradiction);
        assert!(!report.interrupted_turn);
        assert_eq!(report.crashed_ops.len(), 1);
        assert_eq!(report.crashed_ops[0].op_id, op);
        assert_eq!(report.crashed_ops[0].status, "completed");
        assert_eq!(report.crashed_ops[0].effect, EffectStatus::Verified);
        assert_eq!(
            report.crashed_ops[0].action,
            RecoveryAction::Verified {
                expected,
                actual: expected
            }
        );
        assert_eq!(report.state, AgentState::FailedRecoverable);
        // The one-time migration is durable audit evidence: the journal
        // names the RELATIVE identity the run was verified through.
        let events = recovery_events(&s);
        assert!(events.iter().any(|e| {
            e.kind == EventKind::RecoveryApplied
                && e.op_id == Some(op)
                && e.payload.as_ref().is_some_and(|p| {
                    p.get("legacy_migrated_to").and_then(|v| v.as_str()) == Some("sub/a.txt")
                        && p.get("status").and_then(|v| v.as_str()) == Some("completed")
                })
        }));
        // Verification is a read: the file is untouched.
        assert_eq!(
            std::fs::read(root.join("sub").join("a.txt")).unwrap(),
            bytes
        );
        // Second sweep: terminal row, no new events.
        let seq = s.last_event_seq().unwrap().unwrap();
        let second = s.recover_all().unwrap();
        assert!(!second.applied);
        assert!(second.crashed_ops.is_empty());
        assert_eq!(s.last_event_seq().unwrap().unwrap(), seq);
        assert_eq!(report.state, AgentState::FailedRecoverable);
    }

    #[test]
    fn legacy_verify_hash_mismatch_marks_never_ran() {
        let (_d, m) = test_manager();
        let (s, root) = workspace_session(&m, _d.path());
        to_executing(&s);
        std::fs::write(root.join("a.txt"), b"other bytes").unwrap();
        let expected = FileHash::from([7; 32]);
        let (meta, _op) = make_meta(
            &s,
            &m,
            RecoveryStrategy::VerifyHash {
                path: root.join("a.txt").to_string_lossy().to_string(),
                expected,
            },
        );
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        let actual = blake3_of(b"other bytes");
        let report = s.recover_all().unwrap();
        assert_eq!(report.crashed_ops[0].status, "failed");
        assert_eq!(report.crashed_ops[0].effect, EffectStatus::Failed);
        assert_eq!(
            report.crashed_ops[0].action,
            RecoveryAction::NotApplied {
                expected,
                actual: Some(actual)
            }
        );
    }

    #[test]
    fn legacy_verify_missing_file_means_never_ran() {
        let (_d, m) = test_manager();
        let (s, root) = workspace_session(&m, _d.path());
        to_executing(&s);
        let expected = FileHash::from([7; 32]);
        let (meta, _op) = make_meta(
            &s,
            &m,
            RecoveryStrategy::VerifyHash {
                path: root.join("a.txt").to_string_lossy().to_string(),
                expected,
            },
        );
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        let report = s.recover_all().unwrap();
        assert_eq!(report.crashed_ops[0].status, "failed");
        assert_eq!(
            report.crashed_ops[0].action,
            RecoveryAction::NotApplied {
                expected,
                actual: None
            }
        );
    }

    #[test]
    fn recover_all_unknown_effect_and_manual_never_rerun() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_executing(&s);
        let (meta, op_unknown) = make_meta(&s, &m, RecoveryStrategy::MarkUnknown);
        s.start_tool_run(meta, "run_test", serde_json::json!({}))
            .unwrap();
        let (meta2, op_manual) = make_meta(&s, &m, RecoveryStrategy::Manual);
        s.start_tool_run(meta2, "deploy", serde_json::json!({}))
            .unwrap();
        let report = s.recover_all().unwrap();
        assert_eq!(report.crashed_ops.len(), 2);
        let by_op = |o: OpId| report.crashed_ops.iter().find(|r| r.op_id == o).unwrap();
        let u = by_op(op_unknown);
        assert_eq!(u.status, "interrupted");
        assert_eq!(u.effect, EffectStatus::Unknown);
        assert_eq!(u.action, RecoveryAction::UnknownEffect);
        let man = by_op(op_manual);
        assert_eq!(man.status, "interrupted");
        assert_eq!(man.action, RecoveryAction::NeedsHuman);
        // Nothing was re-run: no new tool rows, no TurnCompleted.
        assert!(s.pending_tool_runs().unwrap().is_empty());
        assert!(!s
            .events_range(1, None)
            .unwrap()
            .iter()
            .any(|e| e.kind == EventKind::TurnCompleted));
    }

    #[test]
    fn recover_all_idempotent_no_duplicate_events() {
        let (_d, m) = test_manager();
        let (s, root) = workspace_session(&m, _d.path());
        to_executing(&s);
        let bytes = b"landed";
        std::fs::write(root.join("a.txt"), bytes).unwrap();
        let expected = blake3_of(bytes);
        let (meta, _op) = make_meta(
            &s,
            &m,
            RecoveryStrategy::VerifyHash {
                path: root.join("a.txt").to_string_lossy().to_string(),
                expected,
            },
        );
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        let first = s.recover_all().unwrap();
        assert!(first.applied);
        assert_eq!(first.crashed_ops[0].effect, EffectStatus::Verified);
        let events_after_first = s.last_event_seq().unwrap().unwrap().raw();
        // Second sweep: nothing pending, nothing to do, no new events.
        let second = s.recover_all().unwrap();
        assert!(!second.applied);
        assert!(second.crashed_ops.is_empty());
        assert_eq!(
            s.last_event_seq().unwrap().unwrap().raw(),
            events_after_first
        );
        // Third sweep still idempotent.
        assert!(!s.recover_all().unwrap().applied);
    }

    /// Terminalization is ONE transaction per scanned row: the terminal
    /// `tool_run` update and its `RecoveryApplied` event commit together
    /// with the session state/sequence. Crash at each durability boundary
    /// (`ev_precommit`/`ev_committed` = before the per-row txn starts,
    /// `session_command_*` = inside row 0 or row 1) reopens on exactly the
    /// old or exactly the new durable world; a restart's sweep converges
    /// every remaining row to exactly one row+event pair, and further
    /// sweeps append nothing.
    #[test]
    fn recovery_terminalizes_row_and_event_in_one_transaction() {
        const SEAMS: [&str; 5] = [
            "ev_precommit",
            "ev_committed",
            "session_command_side_row",
            "session_command_precommit",
            "session_command_committed",
        ];
        for seam in SEAMS {
            let ordinals: &[u64] = if seam.starts_with("ev_") {
                &[0]
            } else {
                &[0, 1]
            };
            for &ordinal in ordinals {
                let (dir, m) = test_manager();
                let s = session(&m);
                to_executing(&s);
                let mut ops = Vec::new();
                for i in 0..2 {
                    let (meta, op) = make_meta(&s, &m, RecoveryStrategy::None);
                    s.start_tool_run(meta, "read_file", serde_json::json!({ "i": i }))
                        .unwrap();
                    ops.push(op);
                }
                let sid = s.id();
                m.store().crash_arm(faktor_store::CrashArm {
                    point: seam,
                    ordinal,
                });
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = s.recover_all();
                }));
                assert!(caught.is_err(), "seam {seam} ordinal {ordinal} must fire");
                drop(s);
                drop(m);
                let m2 = reopen(&dir);
                let after = m2.get_session(sid).unwrap().unwrap();
                let running: std::collections::HashSet<OpId> = after
                    .pending_tool_runs()
                    .unwrap()
                    .into_iter()
                    .map(|r| r.op_id)
                    .collect();
                let applied_events = |op: OpId| {
                    after
                        .events_range(1, None)
                        .unwrap()
                        .iter()
                        .filter(|e| e.kind == EventKind::RecoveryApplied && e.op_id == Some(op))
                        .count()
                };
                let i = ordinal as usize;
                for (k, op) in ops.iter().enumerate() {
                    let committed = match seam {
                        "session_command_committed" => k <= i,
                        "session_command_side_row" | "session_command_precommit" => k < i,
                        // CrashDetected: no per-row transaction ran at all.
                        _ => false,
                    };
                    if committed {
                        assert!(
                            !running.contains(op),
                            "{seam}/{ordinal}: terminal row is durable"
                        );
                        assert_eq!(
                            applied_events(*op),
                            1,
                            "{seam}/{ordinal}: terminal row AND its event committed together"
                        );
                    } else {
                        assert!(
                            running.contains(op),
                            "{seam}/{ordinal}: row without a committed event stays running"
                        );
                        assert_eq!(
                            applied_events(*op),
                            0,
                            "{seam}/{ordinal}: no event without its row"
                        );
                    }
                }
                // The pre-sweep crash still lands on a replayed-legal journal.
                after.replay_journal().unwrap();
                // Restart convergence: the next sweep terminalizes every
                // remaining row exactly once; already-paired rows keep
                // exactly one event (never a duplicate).
                let report = after.recover_all().unwrap();
                assert!(after.pending_tool_runs().unwrap().is_empty());
                for op in &ops {
                    assert_eq!(
                        applied_events(*op),
                        1,
                        "{seam}/{ordinal}: exactly one row+event pair for {op}"
                    );
                }
                assert!(report.crashed_ops.len() <= ops.len());
                // Further sweeps append nothing (idempotent).
                let seq = after.last_event_seq().unwrap().unwrap();
                assert!(!after.recover_all().unwrap().applied);
                assert_eq!(after.last_event_seq().unwrap().unwrap(), seq);
            }
        }
    }

    #[test]
    fn recover_all_detects_interrupted_turn_without_tool_runs() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_executing(&s);
        // No tool rows: the crash hit the model stream itself.
        let report = s.recover_all().unwrap();
        assert!(report.interrupted_turn);
        assert!(report.crashed_ops.is_empty());
        // The crash hit the turn at the permission point; the durable
        // permission request survives, so the session keeps waiting on it.
        assert_eq!(report.state, AgentState::WaitingForPermission);
        assert_eq!(s.state().unwrap(), AgentState::WaitingForPermission);
        // The permission can still be resolved after recovery.
        let (_, op, _) = s.pending_permission(1).unwrap().unwrap();
        let _ = op;
        s.resolve_permission(1, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ReadyForNextTurn);
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == EventKind::CrashDetected)
                .count(),
            1
        );
        // From FailedRecoverable the user may re-prompt (never blind replay).
        s.submit_prompt("try again", &[]).unwrap();
        assert_eq!(s.state().unwrap(), AgentState::Preparing);
    }

    /// P1-E: a pending permission whose deadline elapsed while the daemon was
    /// down (no live waiter existed) must be reconciled by recovery BEFORE
    /// the crash state is decided: durable row terminal `expired`, one
    /// `PermissionExpired` journal event (never a fake Deny), and the session
    /// must not stay parked in `WaitingForPermission`.
    #[test]
    fn recover_expires_pending_permissions_before_deciding_state() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let clock = Arc::new(faktor_core::time::TestClock::new(t0));
        let m = crate::SessionManager::open_with_clock(
            dir.path().join("store"),
            dir.path().join("cas"),
            true,
            clock.clone(),
        )
        .unwrap();
        let s = session(&m);
        let req = to_executing(&s);
        assert_eq!(s.state().unwrap(), AgentState::WaitingForPermission);
        let sid = s.id();
        // CRASH: the world stops with the permission still pending.
        drop(s);
        drop(m);
        // The daemon was down past the durable deadline and nothing was live.
        clock.set(req.expires_ms + 1);
        let m2 = crate::SessionManager::open_with_clock(
            dir.path().join("store"),
            dir.path().join("cas"),
            true,
            clock.clone(),
        )
        .unwrap();
        let s2 = m2.get_session(sid).unwrap().unwrap();
        let report = s2.recover_all().unwrap();
        assert!(
            report.applied,
            "the expiry reconciliation changed durable state"
        );
        assert!(
            !report.interrupted_turn,
            "the elapsed permission was the only durable interruption"
        );
        assert_eq!(report.state, AgentState::ReadyForNextTurn);
        assert_eq!(s2.state().unwrap(), AgentState::ReadyForNextTurn);
        assert_ne!(s2.state().unwrap(), AgentState::WaitingForPermission);
        // Durable row: terminal expired, and NOT resolvable any more.
        assert!(s2.pending_permission(req.id).unwrap().is_none());
        assert_eq!(
            m2.store().permission_decision(req.id).unwrap().as_deref(),
            Some("expired")
        );
        // The journal explains why: PermissionExpired (never a fake Deny)
        // with the exact rows the sweep changed.
        let events = s2.events_range(1, None).unwrap();
        let expired: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::PermissionExpired)
            .collect();
        assert_eq!(expired.len(), 1, "one sweep, one event");
        assert_eq!(expired[0].op_id, Some(req.op_id));
        assert_eq!(expired[0].state, AgentState::ReadyForNextTurn);
        assert_eq!(
            expired[0].payload.as_ref().unwrap()["permission_ids"],
            serde_json::json!([req.id])
        );
        assert!(!events.iter().any(|e| matches!(
            e.kind,
            EventKind::PermissionGranted | EventKind::PermissionDenied
        )));
        // SECOND recovery: idempotent — no rows, no events.
        let seq = s2.last_event_seq().unwrap().unwrap();
        let second = s2.recover_all().unwrap();
        assert!(!second.applied);
        assert!(second.crashed_ops.is_empty());
        assert_eq!(s2.last_event_seq().unwrap().unwrap(), seq);
        assert_eq!(s2.state().unwrap(), AgentState::ReadyForNextTurn);
        assert_eq!(
            m2.store().permission_decision(req.id).unwrap().as_deref(),
            Some("expired")
        );
        // The recovered session is interactable again.
        s2.submit_prompt("try again", &[]).unwrap();
        assert_eq!(s2.state().unwrap(), AgentState::Preparing);
    }

    #[test]
    fn recover_all_terminal_contradiction_flagged_and_rows_fixed() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_executing(&s);
        let (meta, op) = make_meta(&s, &m, RecoveryStrategy::MarkUnknown);
        s.start_tool_run(meta, "run_test", serde_json::json!({}))
            .unwrap();
        // Corrupt the journal: a ToolStarted exists but the session row is
        // forced to Completed (no legal transition does this).
        s.force_append_event(EventKind::TurnCompleted, AgentState::Completed, None, None)
            .unwrap();
        let report = s.recover_all().unwrap();
        assert!(
            report.contradiction,
            "journal says Completed, tool row says running"
        );
        assert_eq!(
            report.state,
            AgentState::Completed,
            "state stands; rows are fixed"
        );
        assert_eq!(report.crashed_ops.len(), 1);
        assert_eq!(report.crashed_ops[0].op_id, op);
        assert!(s.pending_tool_runs().unwrap().is_empty(), "rows fixed");
    }

    #[test]
    fn journal_corruption_detected_by_replay() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.submit_prompt("x", &[]).unwrap();
        // Preparing -> Streaming skips the whole chain: corruption.
        s.force_append_event(EventKind::ModelStarted, AgentState::Streaming, None, None)
            .unwrap();
        let err = s.replay_journal().unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Internal);
    }

    #[test]
    fn recover_all_rejects_corrupt_recovery_json() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_executing(&s);
        // Bypass the typed API: store a garbage recovery strategy directly.
        m.store()
            .start_tool_run(
                s.id(),
                m.try_next_op_id().unwrap(),
                "run_test",
                serde_json::json!({}),
                serde_json::json!({ "strategy": "delete_everything" }),
                None,
                None,
            )
            .unwrap();
        let err = s.recover_all().unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Malformed);
    }

    #[test]
    fn recover_all_expected_hash_column_mismatch_is_malformed() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_executing(&s);
        let expected = FileHash::from([7; 32]);
        // The strategy says one hash; the column says another: tampering.
        m.store()
            .start_tool_run(
                s.id(),
                m.try_next_op_id().unwrap(),
                "write_file",
                serde_json::json!({}),
                serde_json::to_value(RecoveryStrategy::VerifyHash {
                    path: "/w/a.txt".into(),
                    expected,
                })
                .unwrap(),
                Some(FileHash::from([9; 32]).to_hex()),
                None,
            )
            .unwrap();
        let err = s.recover_all().unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Malformed);
    }

    #[test]
    fn recover_all_orphan_processes_reported_and_cleared() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let op = s.submit_prompt("x", &[]).unwrap().op_id;
        s.register_process(1234, op).unwrap();
        let report = s.recover_all().unwrap();
        assert_eq!(report.orphans.len(), 1);
        assert_eq!(report.orphans[0].pid, 1234);
        assert!(s.owned_processes().unwrap().is_empty(), "registry cleared");
    }

    #[test]
    fn recover_all_idle_session_is_noop() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let report = s.recover_all().unwrap();
        assert!(!report.applied);
        assert!(!report.interrupted_turn);
        assert_eq!(report.state, AgentState::Idle);
        assert_eq!(
            s.last_event_seq().unwrap().unwrap().raw(),
            1,
            "no events appended"
        );
    }

    /// Legacy paths that cannot be PROVEN inside the workspace — a `..`
    /// climb, a symlink pointing out, a different root — classify the effect
    /// failed/Unknown and demand a human decision: NEVER Verified. The
    /// outside markers deliberately hold bytes matching `expected`, so the
    /// old raw-path hash would have "verified" them; containment proof is
    /// the only thing standing between the row and a false completion.
    #[test]
    fn legacy_verify_paths_outside_workspace_are_unknown_never_verified() {
        let (_d, m) = test_manager();
        let (s, root) = workspace_session(&m, _d.path());
        to_executing(&s);
        let marker = b"outside-marker";
        let expected = blake3_of(marker);
        // (1) A `..` climb out of the workspace.
        let climb = _d.path().join("outside-climb.txt");
        std::fs::write(&climb, marker).unwrap();
        // (2) A different root entirely.
        let other_root = tempfile::tempdir().unwrap();
        let different = other_root.path().join("other.txt");
        std::fs::write(&different, marker).unwrap();
        let mut outside = vec![climb.clone(), different.clone()];
        let mut paths = vec![
            root.join("..")
                .join("outside-climb.txt")
                .to_string_lossy()
                .to_string(),
            different.to_string_lossy().to_string(),
        ];
        // (3) A symlink inside the root pointing OUT of it.
        #[cfg(unix)]
        {
            let out_dir = _d.path().join("outside-dir");
            std::fs::create_dir_all(&out_dir).unwrap();
            let target = out_dir.join("secret.txt");
            std::fs::write(&target, marker).unwrap();
            std::os::unix::fs::symlink(&out_dir, root.join("link")).unwrap();
            outside.push(target);
            paths.push(
                root.join("link")
                    .join("secret.txt")
                    .to_string_lossy()
                    .to_string(),
            );
        }
        for path in &paths {
            let (meta, _op) = make_meta(
                &s,
                &m,
                RecoveryStrategy::VerifyHash {
                    path: path.clone(),
                    expected,
                },
            );
            s.start_tool_run(meta, "write_file", serde_json::json!({}))
                .unwrap();
        }
        let report = s.recover_all().unwrap();
        assert_eq!(report.crashed_ops.len(), paths.len());
        for op in &report.crashed_ops {
            assert_eq!(op.status, "failed", "{op:?}");
            assert_eq!(op.effect, EffectStatus::Unknown, "{op:?}");
            assert_eq!(op.action, RecoveryAction::NeedsHuman, "{op:?}");
        }
        // Unverifiable legacy rows are terminally failed, never re-scanned.
        assert!(s.pending_tool_runs().unwrap().is_empty());
        // No completion was ever journaled for these rows.
        assert_eq!(verified_effects(&recovery_events(&s)), 0);
        // The outside markers are untouched: no outside file was hashed into
        // a completion and none was written.
        for path in &outside {
            assert_eq!(std::fs::read(path).unwrap(), marker);
        }
        // A second sweep must not re-litigate the terminal rows.
        let second = s.recover_all().unwrap();
        assert!(second.crashed_ops.is_empty());
    }

    /// A session whose durable workspace root is MISSING cannot prove any
    /// containment: the effect is Unknown/NeedsUserInput, never Verified —
    /// even though the claimed path exists and its bytes match `expected`
    /// (the old raw-path hasher would have falsely verified it).
    #[test]
    fn legacy_verify_missing_workspace_root_is_unknown_never_verified() {
        let (_d, m) = test_manager();
        let missing = _d.path().join("missing-root");
        let ws = m.create_workspace(missing.to_str().unwrap()).unwrap();
        let s = m.create_session(ws, "t", "ollama", "qwen3.8").unwrap();
        to_executing(&s);
        let marker = b"outside-marker";
        let outside = _d.path().join("marker.txt");
        std::fs::write(&outside, marker).unwrap();
        let expected = blake3_of(marker);
        let (meta, _op) = make_meta(
            &s,
            &m,
            RecoveryStrategy::VerifyHash {
                path: outside.to_string_lossy().to_string(),
                expected,
            },
        );
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        let report = s.recover_all().unwrap();
        assert_eq!(report.crashed_ops.len(), 1);
        assert_eq!(report.crashed_ops[0].status, "failed");
        assert_eq!(report.crashed_ops[0].effect, EffectStatus::Unknown);
        assert_eq!(report.crashed_ops[0].action, RecoveryAction::NeedsHuman);
        assert_eq!(std::fs::read(&outside).unwrap(), marker);
        assert!(s.pending_tool_runs().unwrap().is_empty());
    }

    /// A swap AFTER the one-time migration (the migration write landed, the
    /// verification did not) cannot redirect the read: verification goes
    /// through the handle's relative, symlink-bounded open, so a parent
    /// directory swapped for a symlink out of the workspace is refused — the
    /// outside file whose bytes match `expected` is never hashed into a
    /// completion and the row is classified Unknown/NeedsUserInput.
    #[cfg(unix)]
    #[test]
    fn legacy_verify_swap_after_migration_is_refused_never_verified() {
        let (_d, m) = test_manager();
        let (s, root) = workspace_session(&m, _d.path());
        to_executing(&s);
        let payload = b"payload";
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("a.txt"), payload).unwrap();
        let outside_dir = _d.path().join("outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        // Equal bytes outside: a raw absolute-path read would "verify".
        std::fs::write(outside_dir.join("a.txt"), payload).unwrap();
        let expected = blake3_of(payload);
        let legacy = root.join("sub").join("a.txt").to_string_lossy().to_string();
        let (meta, op) = make_meta(
            &s,
            &m,
            RecoveryStrategy::VerifyHash {
                path: legacy.clone(),
                expected,
            },
        );
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        // Deterministic crash MID-migration: run the migration (the durable
        // midpoint of the sweep's legacy arm) and stop before verification.
        let pending = s.pending_tool_runs().unwrap();
        let row = pending.iter().find(|r| r.op_id == op).unwrap();
        let (pc, _ws) = migrate_legacy_verify_row(&s, row, &legacy, expected)
            .unwrap()
            .expect("a provably-inside path must migrate");
        assert_eq!(pc.relative_path, "sub/a.txt");
        // The durable midpoint: the running row now carries the modern
        // relative postcondition.
        let pending = s.pending_tool_runs().unwrap();
        assert_eq!(pending.len(), 1);
        let stored: FilePostcondition =
            serde_json::from_value(pending[0].postcondition.clone().unwrap()).unwrap();
        assert_eq!(stored.relative_path, "sub/a.txt");
        // The race: the verified path's parent becomes a symlink to the
        // outside directory AFTER the migration proof.
        std::fs::remove_dir_all(root.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside_dir, root.join("sub")).unwrap();
        let report = s.recover_all().unwrap();
        assert_eq!(report.crashed_ops.len(), 1);
        assert_eq!(report.crashed_ops[0].status, "failed");
        assert_eq!(report.crashed_ops[0].effect, EffectStatus::Unknown);
        assert_eq!(report.crashed_ops[0].action, RecoveryAction::NeedsHuman);
        // The row is terminal (visible, never silently completed) and the
        // outside file was never hashed into a completion.
        assert!(s.pending_tool_runs().unwrap().is_empty());
        assert_eq!(verified_effects(&recovery_events(&s)), 0);
        assert_eq!(std::fs::read(outside_dir.join("a.txt")).unwrap(), payload);
    }

    /// The one-time migration is durable BEFORE any verification: a crash
    /// after the migration write but before the finish resumes through the
    /// modern relative postcondition exactly once — the legacy string is
    /// never consulted again. The session's effective root is re-pointed at
    /// a live shadow (P0-48) whose only shared identity with the legacy
    /// path is the RELATIVE one: re-deriving the absolute string against the
    /// new root could not prove containment, so only the postcondition can
    /// verify.
    #[test]
    fn legacy_verify_migration_midpoint_resumes_once_through_the_postcondition() {
        let (_d, m) = test_manager();
        let (s, root) = workspace_session(&m, _d.path());
        to_executing(&s);
        let payload = b"landed";
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("a.txt"), payload).unwrap();
        let expected = blake3_of(payload);
        let legacy = root.join("sub").join("a.txt").to_string_lossy().to_string();
        let (meta, op) = make_meta(
            &s,
            &m,
            RecoveryStrategy::VerifyHash {
                path: legacy.clone(),
                expected,
            },
        );
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        // Crash mid-migration: the durable postcondition write landed, the
        // finish did not.
        let pending = s.pending_tool_runs().unwrap();
        let row = pending.iter().find(|r| r.op_id == op).unwrap();
        let (pc, _ws) = migrate_legacy_verify_row(&s, row, &legacy, expected)
            .unwrap()
            .expect("a provably-inside path must migrate");
        assert_eq!(pc.relative_path, "sub/a.txt");
        // The session now runs shadowed: the same relative file exists under
        // the shadow root, and the legacy absolute string (which points into
        // the ORIGINAL root) can no longer prove containment.
        let shadow = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(shadow.path().join("sub")).unwrap();
        std::fs::write(shadow.path().join("sub").join("a.txt"), payload).unwrap();
        m.put_shadow_row(
            s.id(),
            &crate::ShadowRow {
                session_id: s.id().raw(),
                shadow_id: "shadow-1".into(),
                base_root: root.to_string_lossy().to_string(),
                root: shadow.path().to_string_lossy().to_string(),
                state: crate::ShadowRowState::Active,
                base_entries: 0,
                base_bytes: 0,
                created_ms: 0,
            },
        )
        .unwrap();
        let report = s.recover_all().unwrap();
        assert_eq!(report.crashed_ops.len(), 1);
        assert_eq!(report.crashed_ops[0].status, "completed");
        assert_eq!(report.crashed_ops[0].effect, EffectStatus::Verified);
        // Exactly ONE RecoveryApplied for the op: the resume happened once.
        let events = recovery_events(&s);
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::RecoveryApplied && e.op_id == Some(op))
                .count(),
            1
        );
        assert_eq!(verified_effects(&events), 1);
        // Verification is a read: both copies are untouched.
        assert_eq!(
            std::fs::read(root.join("sub").join("a.txt")).unwrap(),
            payload
        );
        assert_eq!(
            std::fs::read(shadow.path().join("sub").join("a.txt")).unwrap(),
            payload
        );
        // Third sweep: terminal, nothing appended.
        let seq = s.last_event_seq().unwrap().unwrap();
        let third = s.recover_all().unwrap();
        assert!(!third.applied);
        assert!(third.crashed_ops.is_empty());
        assert_eq!(s.last_event_seq().unwrap().unwrap(), seq);
    }

    /// A durable postcondition (the modern shape) is verified through the
    /// handle; a foreign workspace id or a hostile relative path is
    /// Unknown/NeedsUserInput — never Verified, never read.
    #[test]
    fn durable_postcondition_verifies_through_the_handle_and_refuses_foreign_or_hostile_ones() {
        let (_d, m) = test_manager();
        let (s, root) = workspace_session(&m, _d.path());
        to_executing(&s);
        let payload = b"landed";
        std::fs::write(root.join("a.txt"), payload).unwrap();
        let expected = blake3_of(payload);
        let identity = s.identity().unwrap();
        // (1) Matching postcondition -> completed/Verified.
        let (meta, op_ok) = make_meta(&s, &m, RecoveryStrategy::MarkUnknown);
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        let ok = FilePostcondition {
            workspace_id: identity.workspace_id,
            worktree_id: identity.worktree_id,
            relative_path: "a.txt".into(),
            expected_hash: expected,
        };
        s.record_tool_postcondition(op_ok, &serde_json::to_value(&ok).unwrap())
            .unwrap();
        // (2) A hostile relative path is refused before any read: the marker
        // outside the workspace holds matching bytes.
        let marker = _d.path().join("marker.txt");
        std::fs::write(&marker, payload).unwrap();
        let (meta, op_hostile) = make_meta(&s, &m, RecoveryStrategy::MarkUnknown);
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        let hostile = FilePostcondition {
            relative_path: "../marker.txt".into(),
            ..ok.clone()
        };
        s.record_tool_postcondition(op_hostile, &serde_json::to_value(&hostile).unwrap())
            .unwrap();
        // (3) A postcondition naming a FOREIGN workspace id is refused even
        // though the relative path and bytes match.
        let (meta, op_foreign) = make_meta(&s, &m, RecoveryStrategy::MarkUnknown);
        s.start_tool_run(meta, "write_file", serde_json::json!({}))
            .unwrap();
        let foreign = FilePostcondition {
            workspace_id: WorkspaceId::new(999),
            ..ok.clone()
        };
        s.record_tool_postcondition(op_foreign, &serde_json::to_value(&foreign).unwrap())
            .unwrap();
        let report = s.recover_all().unwrap();
        let by_op = |o: OpId| report.crashed_ops.iter().find(|r| r.op_id == o).unwrap();
        let legit = by_op(op_ok);
        assert_eq!(legit.status, "completed");
        assert_eq!(legit.effect, EffectStatus::Verified);
        for op in [op_hostile, op_foreign] {
            let r = by_op(op);
            assert_eq!(r.status, "failed", "{r:?}");
            assert_eq!(r.effect, EffectStatus::Unknown, "{r:?}");
            assert_eq!(r.action, RecoveryAction::NeedsHuman, "{r:?}");
        }
        // Only the legitimate row verified; the outside marker is untouched
        // and the rows are terminal.
        assert_eq!(verified_effects(&recovery_events(&s)), 1);
        assert_eq!(std::fs::read(&marker).unwrap(), payload);
        assert!(s.pending_tool_runs().unwrap().is_empty());
    }
}
