//! Startup recovery for durable admission claims (audit P1): every
//! `pending` admission row of a previous daemon boot is classified against
//! its durable reservation facts and landed as exactly one of replayable /
//! reclaimable / typed key-reuse conflict — never left pending, so a retry
//! can never face a perpetual `InFlight`.
//!
//! The classification itself lives in
//! [`faktor_orchestrator::runtime::task_executor::resolve_pending_task_admission`]
//! (the durable facts are turn records, queue rows, run linkage/plan rows and
//! registry/assignment rows). This module owns the ordinary-prompt receipt
//! projection (the `prompt_admission` table stores the prompt receipt shape,
//! while the durable facts are the same in-session run facts) and the boot
//! driver over both tables.

use std::sync::Arc;

use faktor_orchestrator::runtime::task_executor::{
    recover_pending_task_admissions, repair_bare_admission_wedge, resolve_pending_task_admission,
    TaskAdmissionRecovery, TaskRunReceipt, ADMISSION_RECOVERY_PAGE, MAX_ADMISSION_RECOVERY_PAGES,
};
use faktor_orchestrator::runtime::ExecError;
use faktor_session::SessionManager;
use faktor_store::{AdmissionResolution, PendingAdmission};

use super::prompt::PromptReceipt;
use super::session::prompt_receipt_json;

/// Aggregate counts of one boot recovery pass (re-exported type alias for
/// callers that only know the server surface).
pub use faktor_orchestrator::runtime::task_executor::AdmissionRecoverySummary;

/// Project one recovered in-session run receipt onto the ordinary-prompt
/// receipt shape (the table's stored value). A missing op id cannot happen
/// for an in-session fact; it is refused typed rather than guessed.
fn prompt_receipt_json_from_task(receipt: &TaskRunReceipt) -> Result<String, ExecError> {
    let op_id = receipt.op_id.ok_or_else(|| {
        ExecError::Internal(format!(
            "recovered in-session prompt fact {} carries no op id",
            receipt.run_id
        ))
    })?;
    Ok(prompt_receipt_json(&PromptReceipt {
        run_id: receipt.run_id.clone(),
        op_id,
        queued: receipt.queued,
        accepted: true,
    }))
}

/// Classify and land ONE stale pending `prompt_admission` row. `Ok(None)`
/// means the row moved on between the scan and the resolve (nothing to do).
pub(crate) fn land_stale_prompt_admission(
    sessions: &Arc<SessionManager>,
    row: &PendingAdmission,
) -> Result<Option<TaskAdmissionRecovery>, ExecError> {
    let recovery = resolve_pending_task_admission(sessions, row)?;
    let resolution = match &recovery {
        TaskAdmissionRecovery::Replayable { receipt } => {
            match prompt_receipt_json_from_task(receipt) {
                Ok(receipt_json) => AdmissionResolution::Complete(receipt_json),
                Err(e) => {
                    // A fact this table cannot project (an orchestrated-shaped
                    // reservation under the prompt key): never leave it pending
                    // and never replay a wrong body — land the typed conflict.
                    tracing::error!(
                        target: "faktor::server::admission_recovery",
                        key = %row.key,
                        session = %row.session_id,
                        error = %e,
                        "prompt admission fact could not be projected; landing the typed conflict"
                    );
                    AdmissionResolution::KeyReuse
                }
            }
        }
        TaskAdmissionRecovery::Reclaimable => {
            // A bare accepted-journal wedge (no turn text, no turn record) is
            // repaired so the retry can execute once instead of queueing
            // behind a phantom active turn.
            repair_bare_admission_wedge(sessions, row.session_id);
            AdmissionResolution::Reclaim
        }
        TaskAdmissionRecovery::KeyReuseUnlinkable => AdmissionResolution::KeyReuse,
    };
    let landed = sessions
        .store()
        .prompt_admission_resolve(&row.key, row.reservation.as_deref(), resolution)
        .map_err(|e| ExecError::Internal(format!("prompt admission recovery resolve: {e}")))?;
    Ok(landed.then_some(recovery))
}

/// Boot pass over the `prompt_admission` table: resolve every pending row
/// (bounded pages; a hostile store is reported, never looped forever).
pub fn recover_pending_prompt_admissions(
    sessions: &Arc<SessionManager>,
) -> AdmissionRecoverySummary {
    let store = sessions.store();
    let mut summary = AdmissionRecoverySummary::default();
    let mut after: Option<String> = None;
    for _page in 0..MAX_ADMISSION_RECOVERY_PAGES {
        let page =
            match store.prompt_admission_pending_page(after.as_deref(), ADMISSION_RECOVERY_PAGE) {
                Ok(page) => page,
                Err(e) => {
                    summary.failed += 1;
                    tracing::error!(
                        target: "faktor::server::admission_recovery",
                        error = %e,
                        "prompt admission recovery scan failed"
                    );
                    return summary;
                }
            };
        if page.rows.is_empty() {
            return summary;
        }
        for row in &page.rows {
            after = Some(row.key.clone());
            match land_stale_prompt_admission(sessions, row) {
                Ok(Some(TaskAdmissionRecovery::Replayable { .. })) => summary.replayed += 1,
                Ok(Some(TaskAdmissionRecovery::Reclaimable)) => summary.reclaimed += 1,
                Ok(Some(TaskAdmissionRecovery::KeyReuseUnlinkable)) => summary.conflicts += 1,
                Ok(None) => {}
                Err(e) => {
                    summary.failed += 1;
                    tracing::error!(
                        target: "faktor::server::admission_recovery",
                        key = %row.key,
                        session = %row.session_id,
                        error = %e,
                        "pending prompt admission could not be resolved; it stays for the next pass"
                    );
                }
            }
        }
        if !page.has_more {
            return summary;
        }
    }
    summary.truncated = true;
    summary
}

/// The daemon's boot recovery over BOTH admission tables (audit P1), run
/// next to session recovery and before the first request: every pending
/// claim of a previous generation is landed, so no retry can ever observe a
/// perpetual `InFlight`.
pub fn recover_pending_admissions(sessions: &Arc<SessionManager>) -> AdmissionRecoverySummary {
    let mut summary = match recover_pending_task_admissions(sessions) {
        Ok(summary) => summary,
        Err(e) => {
            tracing::error!(
                target: "faktor::server::admission_recovery",
                error = %e,
                "task admission recovery pass failed"
            );
            AdmissionRecoverySummary {
                failed: 1,
                ..Default::default()
            }
        }
    };
    summary.merge(&recover_pending_prompt_admissions(sessions));
    summary
}
