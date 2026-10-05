//! Durable admission recovery (audit P1): classify a STALE pending
//! submission-keyed admission against the durable facts of its reservation
//! and land it as exactly one of replayable / reclaimable / key-reuse
//! conflict — never leaving it pending, and never allowing a second
//! execution.
//!
//! The reservation named on the claim makes the classification exact:
//!
//! - `tx-<op>` (in-session run): the durable turn record, a durable queue row
//!   or the run's linkage row proves the prompt was accepted; the receipt is
//!   rebuilt from those facts (`run_id = tx-<op>`, the true queued flag).
//!   None of the three means no durable mutation happened: the claim is
//!   safely reclaimable and a retry executes once.
//! - `run-<op>` (orchestrated run): a durable plan row or any registry /
//!   assignment row under the run id proves the run was driven; otherwise
//!   the pre-spawn rows (policy/base) are not execution and the claim is
//!   reclaimable.
//! - a missing/unparseable reservation (a pre-v30 legacy row or a tampered
//!   row) has no trustworthy linkage: it lands the typed key-reuse conflict
//!   rather than gamble on a double execution.
//!
//! A linkage row carrying a request digest different from the pending row's
//! digest is the same typed conflict (never a replay of the wrong body).

use std::sync::Arc;

use faktor_core::id::{OpId, SessionId};
use faktor_store::{AdmissionResolution, PendingAdmission};

use faktor_session::SessionManager;

use super::{
    ExecError, TaskRunMode, TaskRunReceipt, TaskRunRow, ASSIGNMENT_ROW_KIND, PLAN_ROW_KIND,
    REGISTRY_ROW_KIND, TASK_RUN_ROW_KIND,
};

/// Bounded page of the pending-admission recovery scan.
pub const ADMISSION_RECOVERY_PAGE: u32 = 500;
/// Bounded page count of ONE boot recovery pass per table (a hostile store
/// with more pending rows than this is reported, never looped forever).
pub const MAX_ADMISSION_RECOVERY_PAGES: usize = 2048;

/// The classification of ONE stale pending admission against its durable
/// facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskAdmissionRecovery {
    /// The accepted fact exists: complete the row with this exact receipt.
    Replayable { receipt: TaskRunReceipt },
    /// No durable mutation of this admission exists: delete the pending row
    /// so a retry may execute once.
    Reclaimable,
    /// No trustworthy linkage (legacy/tampered reservation or a mismatched
    /// digest): land the typed key-reuse conflict.
    KeyReuseUnlinkable,
}

impl TaskAdmissionRecovery {
    /// The store resolution that lands this classification.
    pub fn resolution(&self) -> Result<AdmissionResolution, ExecError> {
        match self {
            Self::Replayable { receipt } => {
                let receipt_json = serde_json::to_string(receipt).map_err(|e| {
                    ExecError::Internal(format!("recovered run receipt serialization: {e}"))
                })?;
                Ok(AdmissionResolution::Complete(receipt_json))
            }
            Self::Reclaimable => Ok(AdmissionResolution::Reclaim),
            Self::KeyReuseUnlinkable => Ok(AdmissionResolution::KeyReuse),
        }
    }
}

/// Aggregate of one boot recovery pass (per admission table or combined).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdmissionRecoverySummary {
    /// Pending rows completed from their durable accepted facts.
    pub replayed: usize,
    /// Pending rows safely abandoned (no durable mutation).
    pub reclaimed: usize,
    /// Pending rows landed as typed key-reuse conflicts.
    pub conflicts: usize,
    /// Pending rows whose resolution failed (a store error is reported, the
    /// row stays for the next pass or the inline stale path).
    pub failed: usize,
    /// True when the bounded page budget was exhausted with rows remaining.
    pub truncated: bool,
}

impl AdmissionRecoverySummary {
    pub fn merge(&mut self, other: &Self) {
        self.replayed += other.replayed;
        self.reclaimed += other.reclaimed;
        self.conflicts += other.conflicts;
        self.failed += other.failed;
        self.truncated |= other.truncated;
    }
}

/// Parse the hex suffix of a reserved operation id (`0` is never a valid
/// op id, and a wrong width is not a reservation).
fn parse_op_hex(hex: &str) -> Option<OpId> {
    if hex.len() != 16 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let raw = u64::from_str_radix(hex, 16).ok()?;
    OpId::try_from(raw).ok()
}

/// Classify ONE stale pending admission against the durable facts of its
/// reservation. Never mutates; the caller lands the returned classification
/// through the store's fenced resolve.
pub fn resolve_pending_task_admission(
    session: &Arc<SessionManager>,
    row: &PendingAdmission,
) -> Result<TaskAdmissionRecovery, ExecError> {
    // A missing session row means no turn/run fact can exist.
    if session
        .store()
        .get_session(row.session_id)
        .map_err(|e| ExecError::Internal(format!("admission recovery session read: {e}")))?
        .is_none()
    {
        return Ok(TaskAdmissionRecovery::Reclaimable);
    }
    let Some(reservation) = row.reservation.as_deref() else {
        return Ok(TaskAdmissionRecovery::KeyReuseUnlinkable);
    };
    if let Some(hex) = reservation.strip_prefix("tx-") {
        let Some(op) = parse_op_hex(hex) else {
            return Ok(TaskAdmissionRecovery::KeyReuseUnlinkable);
        };
        return classify_in_session(session, row, reservation, op);
    }
    if let Some(hex) = reservation.strip_prefix("run-") {
        if parse_op_hex(hex).is_none() {
            return Ok(TaskAdmissionRecovery::KeyReuseUnlinkable);
        }
        return classify_orchestrated(session, row, reservation);
    }
    Ok(TaskAdmissionRecovery::KeyReuseUnlinkable)
}

fn classify_in_session(
    session: &Arc<SessionManager>,
    row: &PendingAdmission,
    reservation: &str,
    op: OpId,
) -> Result<TaskAdmissionRecovery, ExecError> {
    let store = session.store();
    let turn = store
        .turn_record_of(row.session_id, op)
        .map_err(|e| ExecError::Internal(format!("admission recovery turn record read: {e}")))?;
    let queued = if turn.is_some() {
        false
    } else {
        store
            .queue_op_ids(row.session_id)
            .map_err(|e| ExecError::Internal(format!("admission recovery queue read: {e}")))?
            .contains(&op)
    };
    let run_row = match store
        .memory_fact_get(row.session_id, TASK_RUN_ROW_KIND, reservation)
        .map_err(|e| ExecError::Internal(format!("admission recovery run row read: {e}")))?
    {
        Some(value) => match TaskRunRow::decode(&value) {
            Ok(run_row) => Some(run_row),
            // A tampered linkage row is never guessed at.
            Err(_) => return Ok(TaskAdmissionRecovery::KeyReuseUnlinkable),
        },
        None => None,
    };
    if turn.is_none() && !queued && run_row.is_none() {
        return Ok(TaskAdmissionRecovery::Reclaimable);
    }
    if let Some(stored_digest) = run_row
        .as_ref()
        .and_then(|r| r.submission_digest.as_deref())
    {
        if stored_digest != row.request_digest {
            return Ok(TaskAdmissionRecovery::KeyReuseUnlinkable);
        }
    }
    Ok(TaskAdmissionRecovery::Replayable {
        receipt: TaskRunReceipt {
            run_id: reservation.to_string(),
            mode: TaskRunMode::InSession,
            op_id: Some(op),
            queued,
        },
    })
}

fn classify_orchestrated(
    session: &Arc<SessionManager>,
    row: &PendingAdmission,
    reservation: &str,
) -> Result<TaskAdmissionRecovery, ExecError> {
    let store = session.store();
    let prefix = format!("{reservation}/");
    let plan = store
        .memory_fact_get(row.session_id, PLAN_ROW_KIND, reservation)
        .map_err(|e| ExecError::Internal(format!("admission recovery plan row read: {e}")))?
        .is_some();
    let registry = store
        .memory_fact_key_prefix_exists(row.session_id, REGISTRY_ROW_KIND, &prefix)
        .map_err(|e| ExecError::Internal(format!("admission recovery registry probe: {e}")))?;
    let assignments = store
        .memory_fact_key_prefix_exists(row.session_id, ASSIGNMENT_ROW_KIND, &prefix)
        .map_err(|e| ExecError::Internal(format!("admission recovery assignment probe: {e}")))?;
    if plan || registry || assignments {
        Ok(TaskAdmissionRecovery::Replayable {
            receipt: TaskRunReceipt {
                run_id: reservation.to_string(),
                mode: TaskRunMode::Orchestrated,
                op_id: None,
                queued: false,
            },
        })
    } else {
        // Pre-spawn rows (policy/base) are not execution: the run never
        // started, so the claim is safely reclaimable.
        Ok(TaskAdmissionRecovery::Reclaimable)
    }
}

/// Repair the one crash residue shape a reclaimed admission can leave: a
/// session op-active on a bare `PromptReceived` journal entry whose turn
/// record never committed. Nothing executable exists (no text, no turn), so
/// the claim is correctly reclaimable — but a retry would queue forever
/// behind the phantom active state. Land the state on the honest crash
/// target through [`SessionHandle::recover_all`] so the retry executes once.
/// A live driver always owns a turn record or pending rows, so this never
/// disturbs one.
pub fn repair_bare_admission_wedge(session: &Arc<SessionManager>, session_id: SessionId) {
    let Ok(Some(handle)) = session.get_session(session_id) else {
        return;
    };
    match handle.is_wedged_after_bare_admission() {
        Ok(true) => {
            if let Err(e) = handle.recover_all() {
                tracing::error!(
                    target: "faktor::task_executor",
                    session = %session_id,
                    error = %e,
                    "bare-admission session repair failed; the retry may not start until the next sweep"
                );
            }
        }
        Ok(false) => {}
        Err(e) => tracing::error!(
            target: "faktor::task_executor",
            session = %session_id,
            error = %e,
            "bare-admission session repair probe failed"
        ),
    }
}

/// Resolve ONE stale pending task admission in the store (fenced by the
/// reservation the classification read). `Ok(true)` lands the resolution;
/// `Ok(false)` means the row moved on.
pub fn land_task_admission_recovery(
    session: &Arc<SessionManager>,
    row: &PendingAdmission,
) -> Result<bool, ExecError> {
    let recovery = resolve_pending_task_admission(session, row)?;
    let resolution = recovery.resolution()?;
    if matches!(recovery, TaskAdmissionRecovery::Reclaimable) {
        repair_bare_admission_wedge(session, row.session_id);
    }
    session
        .store()
        .task_admission_resolve(&row.key, row.reservation.as_deref(), resolution)
        .map_err(|e| ExecError::Internal(format!("admission recovery resolve: {e}")))
}

/// Recover EVERY pending task admission (boot pass): classify each against
/// its durable facts and land it. The pass is bounded per page and reports a
/// truncation instead of looping forever on a hostile store.
pub fn recover_pending_task_admissions(
    session: &Arc<SessionManager>,
) -> Result<AdmissionRecoverySummary, ExecError> {
    let store = session.store();
    let mut summary = AdmissionRecoverySummary::default();
    let mut after: Option<String> = None;
    for _page in 0..MAX_ADMISSION_RECOVERY_PAGES {
        let page = store
            .task_admission_pending_page(after.as_deref(), ADMISSION_RECOVERY_PAGE)
            .map_err(|e| ExecError::Internal(format!("admission recovery scan: {e}")))?;
        if page.rows.is_empty() {
            return Ok(summary);
        }
        for row in &page.rows {
            after = Some(row.key.clone());
            match resolve_pending_task_admission(session, row) {
                Ok(recovery) => {
                    if matches!(recovery, TaskAdmissionRecovery::Reclaimable) {
                        repair_bare_admission_wedge(session, row.session_id);
                    }
                    let (landed, recovery) = match recovery.resolution() {
                        Ok(resolution) => (
                            store.task_admission_resolve(
                                &row.key,
                                row.reservation.as_deref(),
                                resolution,
                            ),
                            recovery,
                        ),
                        Err(e) => {
                            // A receipt that cannot be serialized is never a
                            // reason to leave the row pending: land the typed
                            // conflict (no double execution, no wrong replay).
                            tracing::error!(
                                target: "faktor::task_executor",
                                key = %row.key,
                                session = %row.session_id,
                                error = %e,
                                "task admission recovery could not serialize its receipt; landing the typed conflict"
                            );
                            (
                                store.task_admission_resolve(
                                    &row.key,
                                    row.reservation.as_deref(),
                                    AdmissionResolution::KeyReuse,
                                ),
                                TaskAdmissionRecovery::KeyReuseUnlinkable,
                            )
                        }
                    };
                    match landed {
                        Ok(true) => match recovery {
                            TaskAdmissionRecovery::Replayable { .. } => summary.replayed += 1,
                            TaskAdmissionRecovery::Reclaimable => summary.reclaimed += 1,
                            TaskAdmissionRecovery::KeyReuseUnlinkable => summary.conflicts += 1,
                        },
                        Ok(false) => {}
                        Err(e) => {
                            summary.failed += 1;
                            tracing::error!(
                                target: "faktor::task_executor",
                                key = %row.key,
                                session = %row.session_id,
                                error = %e,
                                "pending task admission could not be resolved; it stays for the next pass"
                            );
                        }
                    }
                }
                Err(e) => {
                    summary.failed += 1;
                    tracing::error!(
                        target: "faktor::task_executor",
                        key = %row.key,
                        session = %row.session_id,
                        error = %e,
                        "pending task admission classification failed; it stays for the next pass"
                    );
                }
            }
        }
        if !page.has_more {
            return Ok(summary);
        }
    }
    summary.truncated = true;
    Ok(summary)
}

#[cfg(test)]
mod admission_recovery_tests {
    use super::*;

    const KEY: &str = "11111111-2222-4333-8444-555555555555";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn fixture() -> (tempfile::TempDir, Arc<SessionManager>, SessionId) {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let ws = manager.create_workspace("/tmp/admission-recovery").unwrap();
        let sid = manager.create_session(ws, "t", "p", "m").unwrap().id();
        (dir, manager, sid)
    }

    fn row(sid: SessionId, reservation: Option<&str>, digest: &str) -> PendingAdmission {
        PendingAdmission {
            key: KEY.to_string(),
            session_id: sid,
            request_digest: digest.to_string(),
            reservation: reservation.map(str::to_string),
            owner_generation: String::new(),
            lease_deadline_ms: 0,
            created_ms: 0,
        }
    }

    fn claim(session: &Arc<SessionManager>, sid: SessionId, reservation: &str) {
        assert_eq!(
            session
                .store()
                .task_admission_claim(sid, KEY, DIGEST, reservation, 1)
                .unwrap(),
            faktor_store::TaskAdmissionClaim::Fresh
        );
    }

    #[test]
    fn in_session_without_facts_is_reclaimable() {
        let (_dir, session, sid) = fixture();
        let op = session.try_next_op_id().unwrap();
        let reservation = format!("tx-{:016x}", op.raw());
        claim(&session, sid, &reservation);
        assert_eq!(
            resolve_pending_task_admission(&session, &row(sid, Some(&reservation), DIGEST))
                .unwrap(),
            TaskAdmissionRecovery::Reclaimable
        );
    }

    #[test]
    fn in_session_with_a_turn_record_replays_byte_exact() {
        let (_dir, session, sid) = fixture();
        let op = session.try_next_op_id().unwrap();
        let reservation = format!("tx-{:016x}", op.raw());
        claim(&session, sid, &reservation);
        let handle = session.get_session(sid).unwrap().unwrap();
        let submitted = handle
            .submit_prompt_with_op_id("recover me", &[], Some(op))
            .unwrap();
        assert!(!submitted.queued);
        assert_eq!(
            resolve_pending_task_admission(&session, &row(sid, Some(&reservation), DIGEST))
                .unwrap(),
            TaskAdmissionRecovery::Replayable {
                receipt: TaskRunReceipt {
                    run_id: reservation,
                    mode: TaskRunMode::InSession,
                    op_id: Some(op),
                    queued: false,
                }
            }
        );
    }

    #[test]
    fn in_session_queued_replays_with_the_true_queued_flag() {
        let (_dir, session, sid) = fixture();
        let handle = session.get_session(sid).unwrap().unwrap();
        // Make the session busy so the reservation's prompt queues durably.
        handle.submit_prompt("active turn", &[]).unwrap();
        let op = session.try_next_op_id().unwrap();
        let reservation = format!("tx-{:016x}", op.raw());
        claim(&session, sid, &reservation);
        let submitted = handle
            .submit_prompt_with_op_id("queued behind", &[], Some(op))
            .unwrap();
        assert!(submitted.queued, "the fixture must produce a queued prompt");
        assert_eq!(
            resolve_pending_task_admission(&session, &row(sid, Some(&reservation), DIGEST))
                .unwrap(),
            TaskAdmissionRecovery::Replayable {
                receipt: TaskRunReceipt {
                    run_id: reservation,
                    mode: TaskRunMode::InSession,
                    op_id: Some(op),
                    queued: true,
                }
            }
        );
    }

    #[test]
    fn orchestrated_with_any_plan_or_registry_fact_replays() {
        let (_dir, session, sid) = fixture();
        let handle = session.get_session(sid).unwrap().unwrap();
        for (kind, key) in [
            (PLAN_ROW_KIND, "run-0000000000000011".to_string()),
            (
                REGISTRY_ROW_KIND,
                "run-0000000000000012/child-1".to_string(),
            ),
            (
                ASSIGNMENT_ROW_KIND,
                "run-0000000000000013/child-1".to_string(),
            ),
        ] {
            handle.upsert_memory_fact(kind, &key, "{}").unwrap();
            let run_id = key.split('/').next().unwrap().to_string();
            let op_hex = run_id.strip_prefix("run-").unwrap();
            let op = OpId::try_from(u64::from_str_radix(op_hex, 16).unwrap()).unwrap();
            let reservation = format!("run-{:016x}", op.raw());
            claim(&session, sid, &reservation);
            assert_eq!(
                resolve_pending_task_admission(&session, &row(sid, Some(&reservation), DIGEST))
                    .unwrap(),
                TaskAdmissionRecovery::Replayable {
                    receipt: TaskRunReceipt {
                        run_id: reservation.clone(),
                        mode: TaskRunMode::Orchestrated,
                        op_id: None,
                        queued: false,
                    }
                },
                "{kind}"
            );
            assert!(session
                .store()
                .task_admission_resolve(KEY, Some(&reservation), AdmissionResolution::Reclaim)
                .unwrap());
        }
    }

    #[test]
    fn orchestrated_pre_spawn_rows_are_reclaimable() {
        let (_dir, session, sid) = fixture();
        let handle = session.get_session(sid).unwrap().unwrap();
        // The policy row is written BEFORE acceptance (pre-spawn) and is not
        // execution: recovery must reclaim, never replay.
        let run_id = "run-0000000000000021";
        handle
            .upsert_memory_fact(super::super::RUN_POLICY_ROW_KIND, run_id, "{}")
            .unwrap();
        claim(&session, sid, run_id);
        assert_eq!(
            resolve_pending_task_admission(&session, &row(sid, Some(run_id), DIGEST)).unwrap(),
            TaskAdmissionRecovery::Reclaimable
        );
    }

    #[test]
    fn missing_or_malformed_reservations_are_typed_key_reuse() {
        let (_dir, session, sid) = fixture();
        for reservation in [
            None,
            Some("not-a-reservation"),
            Some("tx-nonhex"),
            Some("run-0"),
        ] {
            assert_eq!(
                resolve_pending_task_admission(&session, &row(sid, reservation, DIGEST)).unwrap(),
                TaskAdmissionRecovery::KeyReuseUnlinkable,
                "{reservation:?}"
            );
        }
    }

    #[test]
    fn a_mismatched_linkage_digest_is_typed_key_reuse_never_a_wrong_replay() {
        let (_dir, session, sid) = fixture();
        let op = session.try_next_op_id().unwrap();
        let reservation = format!("tx-{:016x}", op.raw());
        claim(&session, sid, &reservation);
        let handle = session.get_session(sid).unwrap().unwrap();
        handle
            .submit_prompt_with_op_id("recover me", &[], Some(op))
            .unwrap();
        let run_row = TaskRunRow {
            run_id: reservation.clone(),
            session_id: sid.raw(),
            mode: TaskRunMode::InSession,
            goal: "recover me".into(),
            item_ids: vec![],
            files: vec![],
            attachments: vec![],
            op_id: Some(op.raw()),
            model: None,
            budget_max_tokens: None,
            created_ms: 0,
            submission_digest: Some("some-other-digest".into()),
        };
        handle
            .upsert_memory_fact(
                TASK_RUN_ROW_KIND,
                &reservation,
                &serde_json::to_string(&run_row).unwrap(),
            )
            .unwrap();
        assert_eq!(
            resolve_pending_task_admission(&session, &row(sid, Some(&reservation), DIGEST))
                .unwrap(),
            TaskAdmissionRecovery::KeyReuseUnlinkable
        );
    }

    #[test]
    fn a_missing_session_is_reclaimable() {
        let (_dir, session, sid) = fixture();
        let reservation = "tx-0000000000000031";
        let missing = SessionId::new(sid.raw() + 999);
        assert_eq!(
            resolve_pending_task_admission(&session, &row(missing, Some(reservation), DIGEST))
                .unwrap(),
            TaskAdmissionRecovery::Reclaimable
        );
    }
}
