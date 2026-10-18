//! Durable per-run policy rows: the no-op disposition and — critically —
//! the task identity the run was admitted under. Settlement resolves the
//! run's OWN identity from this row instead of the session's current (which
//! a newer prompt may own), so a still-running tail can never fold the old
//! goal/spend/completion into the next task.

use faktor_core::TaskId;

use super::{DurableStateError, ExecError, NoOpDisposition, RUN_POLICY_ROW_KIND};

/// The durable per-run policy value (kind [`RUN_POLICY_ROW_KIND`], key = run
/// id). Additive: an old row without the field decodes with `None`, and
/// `None` resolves to the mutating-task default (`RequiresCriterionProof`) —
/// never a silent `Allowed`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunPolicyRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) no_op_disposition: Option<NoOpDisposition>,
    /// The task identity the run was admitted under. Settlement resolves
    /// this row (never the session's CURRENT task) so a prompt that starts
    /// while an earlier run settles can never fold the old run's
    /// goal/spend/completion into the new task. Additive: legacy rows decode
    /// with `None` and refuse settlement typed instead of guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) task_id: Option<u64>,
}

/// Record one orchestrated run's durable policy row BEFORE the run is
/// claimed (the settlement reads it back; a failed write leaves no claimed
/// slot).
pub(crate) fn write_run_policy_row(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
    no_op_disposition: Option<NoOpDisposition>,
    task_id: Option<u64>,
) -> Result<(), ExecError> {
    let value = serde_json::to_string(&RunPolicyRow {
        no_op_disposition,
        task_id,
    })
    .map_err(|e| ExecError::Internal(format!("run policy row encode: {e}")))?;
    handle
        .upsert_memory_fact(RUN_POLICY_ROW_KIND, run_id, &value)
        .map_err(|e| ExecError::Internal(format!("run policy row write: {e}")))
}

/// The run's bound task identity from its durable policy row. A legacy row
/// without the binding (or no row at all) is a typed refusal: settlement
/// must never fall back to the session's current identity (that is the
/// cross-task fold the scale probe found).
pub(crate) fn run_policy_task_id(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
) -> Result<TaskId, ExecError> {
    let Some(value) = handle
        .memory_fact_get(RUN_POLICY_ROW_KIND, run_id)
        .map_err(|e| ExecError::Internal(format!("run policy row read of {run_id}: {e}")))?
    else {
        return Err(ExecError::Conflict(format!(
            "run {run_id} has no recorded task identity; refusing to settle it into the session's current task"
        )));
    };
    let row: RunPolicyRow = serde_json::from_str(&value).map_err(|e| {
        ExecError::from(DurableStateError::CorruptDurableState {
            what: format!("run policy row of run {run_id}"),
            detail: e.to_string(),
        })
    })?;
    row.task_id
        .map(TaskId::new)
        .ok_or_else(|| {
            ExecError::Conflict(format!(
                "run {run_id} predates the task-identity binding; refusing to settle it into the session's current task"
            ))
        })
}
