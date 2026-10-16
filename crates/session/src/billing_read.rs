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
//! - the CLASSIFICATION comes from durable route-time authority only: the
//!   reservation's frozen [`PriceAuthority`] (`LocalZero` is the ONLY
//!   authority that establishes locality — a zero-cost hosted row is never
//!   local) plus the reservation row's durable
//!   [`ExecutionClass`](faktor_core::model::ExecutionClass) (schema v34);
//!   `Unknown` means no authority established the class and must not be
//!   counted as local or hosted;
//! - a settled reservation with no attributable call row (a conservative
//!   finalize or a crash reconciliation) still carries its exact money with
//!   the honest `UNATTRIBUTED` provenance label.
//!
//! Reads are bounded ([`MAX_DURABLE_SPEND_ROWS`] /
//! [`MAX_DURABLE_SPEND_CALL_ROWS`]); a flood past either bound refuses
//! loudly rather than returning a silently partial picture. The
//! provider-call read's truncation is carried EXPLICITLY by
//! [`DurableSpendProjection::call_rows_truncated`] so a caller that must
//! record an immutable row (the outcome ledger) can MARK the partial
//! attribution instead of tolerating it silently, while every money/usage
//! summation ([`SessionHandle::durable_spend_rows`]) refuses the partial
//! read exactly like the reservation bound (the efficiency-KPI rule: a
//! truncated bounded read is a typed refusal, never a quietly partial
//! total).

use faktor_core::id::{SessionId, TaskId};
use faktor_core::model::{ExecutionClass, PriceAuthority};

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
    /// The reservation's frozen ROUTE-TIME price authority (schema v15/v18
    /// `pricing_snapshot_json`): `Some(LocalZero)` is the only authority
    /// that establishes locality; `None` = no pricing authority was
    /// consulted (never local, never hosted).
    pub price_authority: Option<PriceAuthority>,
    /// The reservation's durable route-time execution class (schema v34).
    /// `Unknown` on legacy/NULL rows and unpriced paths — never counted as
    /// local or hosted.
    pub execution_class: ExecutionClass,
    /// The durable reservation state tag (`settled`).
    pub state: String,
    /// The settlement time (`settled_ms`), else the reservation creation
    /// time for rows whose settlement timestamp was never recorded.
    pub occurred_at_ms: i64,
    /// The source operation tag of the projection.
    pub source_operation: String,
}

impl DurableSpendRow {
    /// The durable execution classification of this settled row: the
    /// persisted route-time class when one exists, else `Local` only when
    /// the reservation's frozen pricing authority is
    /// [`PriceAuthority::LocalZero`] (pre-v34 rows whose class column is
    /// NULL), else `Unknown`. A numeric zero NEVER implies locality: a
    /// hosted zero-price row (free tier/promotion) keeps `Unknown` and is
    /// counted as neither local nor hosted.
    pub fn effective_execution_class(&self) -> ExecutionClass {
        if self.execution_class.is_known() {
            return self.execution_class;
        }
        match self.price_authority {
            Some(PriceAuthority::LocalZero) => ExecutionClass::Local,
            _ => ExecutionClass::Unknown,
        }
    }
}

/// The durable-spend projection of one typed task: the settled rows PLUS the
/// honesty of their provider/model attribution (the provider-call read is
/// bounded; the money itself comes from the reservation rows and stays
/// exact).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableSpendProjection {
    /// One row per settled reservation in the store's read order (newest
    /// reservation first; callers that need the run's chronological story
    /// re-order by `occurred_at_ms`/reservation id).
    pub rows: Vec<DurableSpendRow>,
    /// True when the provider-call history exceeded
    /// [`MAX_DURABLE_SPEND_CALL_ROWS`]: attribution rests on a bounded read,
    /// so it must never be claimed complete (rows whose call fell past the
    /// bound carry the `unattributed` provenance and zero counters).
    pub call_rows_truncated: bool,
    /// Settled reservations whose attributable provider-call row was NOT
    /// found (a conservative finalize / crash reconciliation, or a call row
    /// beyond the truncation bound): their provider/model is the explicit
    /// [`UNATTRIBUTED_PROVENANCE`] label, never a guess.
    pub unattributed_settled: u64,
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
    /// for the exact row sources): rows plus the explicit provider/model
    /// attribution honesty. Read-only; bounded; a store failure is loud.
    /// The provider-call read uses the canonical
    /// [`MAX_DURABLE_SPEND_CALL_ROWS`] bound.
    pub fn durable_spend_projection(
        &self,
        task_id: TaskId,
        limit: i64,
    ) -> faktor_core::Result<DurableSpendProjection> {
        self.durable_spend_projection_bounded(task_id, limit, MAX_DURABLE_SPEND_CALL_ROWS)
    }

    /// The bounded twin of [`SessionHandle::durable_spend_projection`]: the
    /// same projection with an EXPLICIT provider-call read bound (the
    /// canonical production bound is [`MAX_DURABLE_SPEND_CALL_ROWS`]; the
    /// parameter exists so callers and adversarial tests can pin a tighter
    /// bound and observe the documented truncation behavior).
    pub fn durable_spend_projection_bounded(
        &self,
        task_id: TaskId,
        limit: i64,
        call_limit: i64,
    ) -> faktor_core::Result<DurableSpendProjection> {
        let limit = limit.clamp(1, MAX_DURABLE_SPEND_ROWS);
        let call_limit = call_limit.clamp(0, MAX_DURABLE_SPEND_CALL_ROWS);
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
            return Ok(DurableSpendProjection {
                rows: Vec::new(),
                call_rows_truncated: false,
                unattributed_settled: 0,
            });
        }
        let (calls, call_rows_truncated) = store
            .provider_call_task_rows(self.id, task_id, call_limit)
            .map_err(SessionError::from)?;
        let mut rows = Vec::with_capacity(settled.len());
        let mut unattributed_settled: u64 = 0;
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
                None => {
                    unattributed_settled = unattributed_settled.saturating_add(1);
                    (
                        UNATTRIBUTED_PROVENANCE.to_string(),
                        UNATTRIBUTED_PROVENANCE.to_string(),
                    )
                }
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
                price_authority: reservation.pricing_snapshot.as_ref().map(|s| s.authority),
                execution_class: reservation.execution_class,
                state: reservation.status.clone(),
                occurred_at_ms: reservation.settled_ms.unwrap_or(reservation.created_ms),
                source_operation: "session.cost_reservation.settled".to_string(),
            });
        }
        Ok(DurableSpendProjection {
            rows,
            call_rows_truncated,
            unattributed_settled,
        })
    }

    /// The durable-spend projection of ONE typed task with the attribution
    /// honesty ENFORCED: a truncated provider-call history is a typed
    /// refusal (mirroring the reservation bound and the efficiency KPI's
    /// refusal rule), never a silently partial attribution/token total. A
    /// caller that must record an immutable row uses
    /// [`SessionHandle::durable_spend_projection`] and marks the state
    /// instead.
    pub fn durable_spend_rows(
        &self,
        task_id: TaskId,
        limit: i64,
    ) -> faktor_core::Result<Vec<DurableSpendRow>> {
        strict_projection_rows(
            self.durable_spend_projection(task_id, limit)?,
            self.id,
            task_id,
        )
    }
}

/// The strict fold of one projection for callers that sum money/usage: a
/// truncated provider-call history is refused typed (mirroring the
/// reservation bound), never a silently partial attribution or token total.
fn strict_projection_rows(
    projection: DurableSpendProjection,
    session_id: SessionId,
    task_id: TaskId,
) -> faktor_core::Result<Vec<DurableSpendRow>> {
    if projection.call_rows_truncated {
        return Err(SessionError::Conflict(format!(
            "durable spend read of session {session_id} task {task_id} exceeds the \
             {MAX_DURABLE_SPEND_CALL_ROWS}-provider-call bound; refusing a partial \
             attribution"
        ))
        .into());
    }
    Ok(projection.rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{BudgetAuthority as _, DurableBudgetLedger};
    use crate::handle::tests::{session, test_manager};
    use crate::Task;

    fn seeded_task(s: &SessionHandle) -> TaskId {
        let task = Task {
            task_id: s.task_id().unwrap(),
            session_id: s.id,
            goal: "metered goal".into(),
            created_ms: 1,
            updated_ms: 1,
            ..Default::default()
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
            .block_on(ledger.reserve_attempt(
                s.id,
                task,
                settled,
                100,
                None,
                ExecutionClass::Unknown,
            ))
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
            .block_on(ledger.reserve_attempt(
                s.id,
                task,
                refunded,
                50,
                None,
                ExecutionClass::Unknown,
            ))
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

    /// The provider-call truncation is CARRIED, never discarded: a bounded
    /// projection reports it explicitly (with the unattributed provenance on
    /// the rows whose call fell past the bound) and the strict money/usage
    /// fold refuses it typed; a whole history stays complete.
    #[test]
    fn truncated_provider_call_history_is_reported_and_the_strict_fold_refuses_it() {
        let (_dir, manager) = test_manager();
        let s = session(&manager);
        let task = seeded_task(&s);
        let ledger = DurableBudgetLedger::new(s.manager.clone());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for (index, (logical, physical)) in [(1u64, 2u64), (3, 4)].into_iter().enumerate() {
            let attempt = faktor_core::op::ModelCallAttempt::new(
                faktor_core::id::OpId::new(logical),
                faktor_core::id::OpId::new(physical),
                index as u32 + 1,
            )
            .unwrap();
            let reservation = runtime
                .block_on(ledger.reserve_attempt(
                    s.id,
                    task,
                    attempt,
                    100,
                    None,
                    ExecutionClass::HostedEconomy,
                ))
                .unwrap();
            runtime
                .block_on(ledger.settle_usage(s.id, reservation, 5, 0, 0, 5, Some(70), None))
                .unwrap();
            s.record_provider_call_attempt(
                attempt,
                Some(reservation),
                if index == 0 { "p-oldest" } else { "p-newest" },
                "m",
                "completed",
                Some(5),
                Some(5),
                None,
            )
            .unwrap();
        }
        // The canonical bound sees the whole (short) history: complete.
        let projection = s.durable_spend_projection(task, 100).unwrap();
        assert!(!projection.call_rows_truncated);
        assert_eq!(projection.unattributed_settled, 0);
        assert_eq!(projection.rows.len(), 2);
        assert!(projection
            .rows
            .iter()
            .all(|row| row.provider != UNATTRIBUTED_PROVENANCE));
        let strict = s.durable_spend_rows(task, 100).unwrap();
        assert_eq!(strict.len(), 2, "a whole history is unchanged");
        // A tighter explicit bound proves the truncation is observable: the
        // newest call row fell past it, so its settled reservation carries
        // the explicit unattributed provenance (never a guess).
        let truncated = s.durable_spend_projection_bounded(task, 100, 1).unwrap();
        assert!(
            truncated.call_rows_truncated,
            "the full bounded page PROVES truncation"
        );
        assert_eq!(truncated.unattributed_settled, 1);
        assert!(truncated.rows.iter().any(|row| {
            row.provider == UNATTRIBUTED_PROVENANCE
                && row.model == UNATTRIBUTED_PROVENANCE
                && row.execution_class == ExecutionClass::HostedEconomy
        }));
        // Money is untouched by attribution truncation: the reservation
        // rows stay the money truth (identical folded sums).
        let sum = |rows: &[DurableSpendRow]| -> u64 {
            rows.iter()
                .map(|row| row.provider_cost_micro)
                .fold(0u64, u64::saturating_add)
        };
        assert_eq!(sum(&truncated.rows), sum(&projection.rows));
        // The strict fold refuses the partial attribution typed (the
        // efficiency-KPI refusal rule), never a silently partial picture.
        let err = strict_projection_rows(truncated, s.id, task).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("refusing a partial attribution"),
            "{message}"
        );
        assert!(
            message.contains(&MAX_DURABLE_SPEND_CALL_ROWS.to_string()),
            "{message}"
        );
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
                execution_class: ExecutionClass::Unknown,
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

    #[test]
    fn execution_class_comes_from_frozen_route_authority_never_from_a_zero_cost() {
        use faktor_core::model::{PriceAuthority, PriceQuote, PricingSnapshot};

        let (_dir, manager) = test_manager();
        let s = session(&manager);
        let task = seeded_task(&s);
        let ledger = DurableBudgetLedger::new(s.manager.clone());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let attempt = |logical: u64, physical: u64, ordinal: u32| {
            faktor_core::op::ModelCallAttempt::new(
                faktor_core::id::OpId::new(logical),
                faktor_core::id::OpId::new(physical),
                ordinal,
            )
            .unwrap()
        };
        // (1) A HOSTED zero-price authority (free tier / promotion /
        // internal endpoint): a numeric zero is NOT locality — the durable
        // class stays Unknown and is counted as neither local nor hosted.
        let hosted = runtime
            .block_on(ledger.reserve_attempt(
                s.id,
                task,
                attempt(1, 2, 1),
                10,
                Some(PricingSnapshot::exact(
                    PriceQuote::ZERO,
                    1,
                    "hosted-free-tier".into(),
                )),
                ExecutionClass::from_price_authority(PriceAuthority::Exact),
            ))
            .unwrap();
        runtime
            .block_on(ledger.settle_usage(s.id, hosted, 5, 0, 0, 5, None, None))
            .unwrap();
        // (2) An authoritative LOCAL zero: LocalZero IS local.
        let local = runtime
            .block_on(ledger.reserve_attempt(
                s.id,
                task,
                attempt(3, 4, 2),
                10,
                Some(PricingSnapshot::local_zero(1, "ollama".into())),
                ExecutionClass::from_price_authority(PriceAuthority::LocalZero),
            ))
            .unwrap();
        runtime
            .block_on(ledger.settle_usage(s.id, local, 5, 0, 0, 5, None, None))
            .unwrap();
        // (3) No pricing authority at all: Unknown — never local/hosted.
        let unpriced = runtime
            .block_on(ledger.reserve_attempt(
                s.id,
                task,
                attempt(5, 6, 3),
                10,
                None,
                ExecutionClass::Unknown,
            ))
            .unwrap();
        runtime
            .block_on(ledger.settle_usage(s.id, unpriced, 5, 0, 0, 5, None, None))
            .unwrap();

        let mut rows = s.durable_spend_rows(task, 100).unwrap();
        rows.sort_by_key(|r| r.reservation_id);
        assert_eq!(rows.len(), 3);
        let hosted_row = &rows[0];
        assert_eq!(
            hosted_row.provider_cost_micro, 0,
            "the hosted free tier folds a real zero"
        );
        assert_eq!(hosted_row.price_authority, Some(PriceAuthority::Exact));
        assert_eq!(
            hosted_row.effective_execution_class(),
            ExecutionClass::Unknown,
            "a zero-cost hosted row is NOT local"
        );
        assert!(!hosted_row.effective_execution_class().is_local());
        assert!(!hosted_row.effective_execution_class().is_hosted());
        let local_row = &rows[1];
        assert_eq!(local_row.price_authority, Some(PriceAuthority::LocalZero));
        assert_eq!(
            local_row.effective_execution_class(),
            ExecutionClass::Local,
            "a LocalZero reservation IS local"
        );
        let unpriced_row = &rows[2];
        assert_eq!(unpriced_row.price_authority, None);
        assert_eq!(
            unpriced_row.effective_execution_class(),
            ExecutionClass::Unknown
        );
    }
}
