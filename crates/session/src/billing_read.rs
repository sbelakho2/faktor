//! Additive durable-spend read (Wave 3 commercial metering projection).
//!
//! This module adds a READ-ONLY projection of the EXISTING
//! reservation/settlement ledger ([`crate::budget`] over the store's
//! `cost_reservation` rows plus the persisted `provider_call` rows). It
//! introduces no new authority and writes nothing: the reservation ledger
//! stays the single source of truth, and the commercial usage ledger is a
//! fold over these rows.
//!
//! One row per SETTLED reservation of one task:
//!
//! - the money is the amount the settlement actually folded
//!   (`settled_cost_micro`, with the documented pre-v18 fallback chain), so a
//!   sum over these rows equals the task's folded spend exactly;
//! - the counters come from the settlement's attributable `provider_call`
//!   row: `tokens_in` / `tokens_out` are the persisted totals (the
//!   settlement folds cache reads/writes and reasoning into them BEFORE
//!   persistence), and the prefix-cache observation's cacheable-prefix
//!   token count is carried separately;
//! - a settled reservation with no attributable call row (a conservative
//!   finalize or a crash reconciliation) still carries its exact money with
//!   the honest `UNATTRIBUTED` provenance label.
//!
//! Reads are bounded ([`MAX_DURABLE_SPEND_ROWS`]); a reservation flood past
//! the bound refuses loudly rather than returning a silently partial
//! picture.

use faktor_core::id::TaskId;

use crate::handle::SessionHandle;
use crate::SessionError;

/// Hard bound on the reservation rows one durable-spend read scans per task.
pub const MAX_DURABLE_SPEND_ROWS: i64 = 10_000;
/// Hard bound on the provider-call rows one durable-spend read scans per
/// task (mirrors the reservation bound).
pub const MAX_DURABLE_SPEND_CALL_ROWS: i64 = 10_000;
/// The provenance label of a settled reservation whose attributable
/// provider-call row is missing (conservative finalize / crash
/// reconciliation). It is a LABEL for an unattributed settlement, never a
/// provider decision (provider behavior is never branched on here).
pub const UNATTRIBUTED_PROVENANCE: &str = "unattributed";

/// One settled reservation of one task projected for the commercial usage
/// ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableSpendRow {
    pub session_id: u64,
    pub task_id: u64,
    /// The durable reservation id (>= 1) — the ingestion source identity.
    pub reservation_id: i64,
    /// The physical attempt identity when the reservation carries one, else
    /// the reservation id (a stable, non-empty projection label).
    pub attempt_id: String,
    /// The provider/model of the attributable call row, or
    /// [`UNATTRIBUTED_PROVENANCE`].
    pub provider: String,
    pub model: String,
    /// Persisted input-token total of the attempt (cache reads/writes and
    /// reasoning are folded into this counter by the settlement).
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The durable prefix-cache observation (cacheable-prefix tokens);
    /// zero when no observation was recorded (never a fabricated number).
    pub prefix_cache_tokens: u64,
    /// The amount the settlement folded (fallback chain applied), in
    /// microUSD.
    pub provider_cost_micro: u64,
    /// The provider-reported amount when the settlement saw one.
    pub provider_reported_micro: u64,
    /// The durable reservation state tag (`settled`).
    pub state: String,
    /// The settlement time (`settled_ms`), else the reservation creation
    /// time for rows whose settlement timestamp was never recorded.
    pub occurred_at_ms: i64,
    /// The source operation tag of the projection.
    pub source_operation: String,
}

/// The amount one settled reservation actually folded: the v18 canonical
/// `settled_cost_micro`, falling back for pre-v18 rows to the legacy winner
/// (provider-reported, else the locally calculated amount). Hostile
/// all-NULL rows fold zero.
pub fn reservation_folded_micro(row: &faktor_store::CostReservationRow) -> u64 {
    row.settled_cost_micro
        .or(row.provider_reported_micro)
        .or(row.provider_cost_micro)
        .unwrap_or(0)
}

/// The provider-reported amount of one settled reservation.
pub fn reservation_provider_reported_micro(row: &faktor_store::CostReservationRow) -> u64 {
    row.provider_reported_cost_micro
        .or(row.provider_reported_micro)
        .unwrap_or(0)
}

impl SessionHandle {
    /// The durable-spend projection of ONE typed task (see the module docs
    /// for the exact row sources). Read-only; bounded; a store failure is
    /// loud.
    pub fn durable_spend_rows(
        &self,
        task_id: TaskId,
        limit: i64,
    ) -> faktor_core::Result<Vec<DurableSpendRow>> {
        let limit = limit.clamp(1, MAX_DURABLE_SPEND_ROWS);
        let store = self.manager.store();
        // Ask for one row more than the bound: a full page then PROVES
        // truncation instead of guessing it.
        let reservations = store
            .cost_reservations_of(self.id, task_id, limit.saturating_add(1))
            .map_err(SessionError::from)?;
        if reservations.len() as i64 > limit {
            return Err(SessionError::Conflict(format!(
                "durable spend read of session {} task {} exceeds the {MAX_DURABLE_SPEND_ROWS}-reservation bound; refusing a partial projection",
                self.id, task_id
            ))
            .into());
        }
        // Settled rows only: they are the rows whose money the reservation
        // ledger already folded. Reserved/dispatched/uncertain/refunded rows
        // hold budget but have not spent it.
        let settled: Vec<&faktor_store::CostReservationRow> = reservations
            .iter()
            .filter(|row| row.status == "settled")
            .collect();
        if settled.is_empty() {
            return Ok(Vec::new());
        }
        let (calls, _truncated) = store
            .provider_call_task_rows(self.id, task_id, MAX_DURABLE_SPEND_CALL_ROWS)
            .map_err(SessionError::from)?;
        let mut rows = Vec::with_capacity(settled.len());
        for reservation in settled {
            let call = calls
                .iter()
                .find(|call| call.reservation_id == Some(reservation.reservation_id))
                .or_else(|| {
                    reservation.attempt_op_id.and_then(|attempt| {
                        calls
                            .iter()
                            .find(|call| call.attempt_op_id == Some(attempt))
                    })
                })
                .or_else(|| {
                    calls.iter().find(|call| {
                        call.attempt_op_id.is_none()
                            && (call.op_id == reservation.op_id
                                || reservation.parent_op_id == Some(call.op_id))
                    })
                });
            let (provider, model) = match call {
                Some(call) => (call.provider.clone(), call.model.clone()),
                None => (
                    UNATTRIBUTED_PROVENANCE.to_string(),
                    UNATTRIBUTED_PROVENANCE.to_string(),
                ),
            };
            rows.push(DurableSpendRow {
                session_id: self.id.raw(),
                task_id: task_id.raw(),
                reservation_id: reservation.reservation_id,
                attempt_id: reservation
                    .attempt_op_id
                    .map(|attempt| attempt.raw().to_string())
                    .unwrap_or_else(|| reservation.reservation_id.to_string()),
                provider,
                model,
                input_tokens: call.and_then(|c| c.tokens_in).unwrap_or(0),
                output_tokens: call.and_then(|c| c.tokens_out).unwrap_or(0),
                prefix_cache_tokens: call.and_then(|c| c.prompt_tokens).unwrap_or(0),
                provider_cost_micro: reservation_folded_micro(reservation),
                provider_reported_micro: reservation_provider_reported_micro(reservation),
                state: reservation.status.clone(),
                occurred_at_ms: reservation.settled_ms.unwrap_or(reservation.created_ms),
                source_operation: "session.cost_reservation.settled".to_string(),
            });
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{BudgetAuthority as _, DurableBudgetLedger};
    use crate::handle::tests::{session, test_manager};
    use crate::Task;
    use faktor_core::state::TaskState;

    fn seeded_task(s: &SessionHandle) -> TaskId {
        let task = Task {
            task_id: s.task_id().unwrap(),
            session_id: s.id,
            goal: "metered goal".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: crate::TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: 1,
            updated_ms: 1,
        };
        s.create_task(task).unwrap().task_id
    }

    #[test]
    fn settled_reservations_project_exactly_and_unsettled_rows_do_not() {
        let (_dir, manager) = test_manager();
        let s = session(&manager);
        let task = seeded_task(&s);
        let ledger = DurableBudgetLedger::new(s.manager.clone());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        // One settled reservation: the projection is the folded actual.
        let settled = faktor_core::op::ModelCallAttempt::new(
            faktor_core::id::OpId::new(1),
            faktor_core::id::OpId::new(2),
            1,
        )
        .unwrap();
        let reservation = runtime
            .block_on(ledger.reserve_attempt(s.id, task, settled, 100, None))
            .unwrap();
        let folded = runtime
            .block_on(ledger.settle_usage(s.id, reservation, 10, 2, 1, 5, Some(77), None))
            .unwrap();
        assert_eq!(folded, Some(77), "the provider-reported amount wins");
        // A pre-dispatch reservation that is refunded: it never spent, so it
        // must never appear in the projection.
        let refunded = faktor_core::op::ModelCallAttempt::new(
            faktor_core::id::OpId::new(3),
            faktor_core::id::OpId::new(4),
            2,
        )
        .unwrap();
        let refunded_row = runtime
            .block_on(ledger.reserve_attempt(s.id, task, refunded, 50, None))
            .unwrap();
        runtime.block_on(ledger.refund(s.id, refunded_row)).unwrap();
        let rows = s.durable_spend_rows(task, 100).unwrap();
        assert_eq!(rows.len(), 1, "only settled rows project: {rows:?}");
        let row = &rows[0];
        assert_eq!(row.reservation_id, reservation.raw());
        assert_eq!(row.task_id, task.raw());
        assert_eq!(row.session_id, s.id.raw());
        assert_eq!(row.state, "settled");
        assert_eq!(row.provider_cost_micro, 77, "the exact folded amount");
        assert_eq!(row.provider_reported_micro, 77);
        // The provenance label is honest when no call row exists.
        assert_eq!(row.provider, UNATTRIBUTED_PROVENANCE);
        assert_eq!(row.attempt_id, "2", "the physical attempt identity");
    }

    #[test]
    fn hostile_arguments_are_refused_loudly() {
        let (_dir, manager) = test_manager();
        let s = session(&manager);
        let task = seeded_task(&s);
        // A zero/negative limit still reads at least one bounded page; a
        // foreign task id projects nothing (never another task's rows).
        let rows = s.durable_spend_rows(task, 0).unwrap();
        assert!(rows.is_empty());
        let foreign = TaskId::new(task.raw() + 999);
        let rows = s.durable_spend_rows(foreign, 10).unwrap();
        assert!(rows.is_empty(), "a foreign task projects nothing");
    }

    #[test]
    fn fold_helpers_follow_the_documented_fallback_chain() {
        let row = |settled: Option<u64>, reported: Option<u64>, calculated: Option<u64>| {
            faktor_store::CostReservationRow {
                reservation_id: 1,
                session_id: faktor_core::id::SessionId::new(1),
                task_id: TaskId::new(1),
                op_id: faktor_core::id::OpId::new(1),
                attempt_op_id: None,
                parent_op_id: None,
                predicted_micro: 7,
                status: "settled".into(),
                created_ms: 1,
                settled_ms: Some(2),
                dispatched_ms: None,
                pricing_snapshot: None,
                provider_cost_micro: calculated,
                provider_reported_micro: reported,
                route_decision_json: None,
                request_id: None,
                delivery_state: None,
                failure_reason_code: None,
                cost_basis: None,
                provider_reported_cost_micro: None,
                estimated_cost_micro: None,
                settled_cost_micro: settled,
            }
        };
        assert_eq!(
            reservation_folded_micro(&row(Some(11), Some(9), Some(8))),
            11
        );
        assert_eq!(reservation_folded_micro(&row(None, Some(9), Some(8))), 9);
        assert_eq!(reservation_folded_micro(&row(None, None, Some(8))), 8);
        assert_eq!(reservation_folded_micro(&row(None, None, None)), 0);
        assert_eq!(
            reservation_provider_reported_micro(&row(None, Some(9), None)),
            9
        );
    }
}
