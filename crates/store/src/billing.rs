//! `billing`: cohesive slice of the mechanically decomposed parent module.

use super::*;
use faktor_core::billing::BillingPeriod;

/// One cost-reservation row whose task row is gone (P0-97 `doctor --deep`
/// dangling-budget scan). Rows in this list can never be settled or refunded
/// and their predicted spend is untracked: the durable ledger points at
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingReservationRow {
    pub reservation_id: i64,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub op_id: OpId,
    pub status: String,
    pub predicted_micro: u64,
}

/// Per-status counts + the dangling rows of the whole `cost_reservation`
/// table (read-only `doctor --deep` invariant scan). `reserved`/`dispatched`
/// rows whose task is gone mean an untracked prediction is still counted by
/// nothing; a `settled` row whose task is gone means spend landed on a
/// vanished envelope. `abandoned` is the legacy pre-P0-2 vocabulary, kept
/// for doctor's line format: no row can hold it after the v16 migration
/// (the CHECK forbids it), so it reads 0 on every migrated store. `open` is
/// the legacy pre-v17 vocabulary (v17 renamed it to `reserved`), so it also
/// reads 0 on every v18 store.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CostReservationScan {
    pub total: u64,
    /// IN-FLIGHT rows holding budget: the legacy pre-v17 `open` rows (0 on
    /// every v18 store) PLUS the v17 vocabulary `reserved` and `dispatched`.
    /// Folded so doctor's legacy line format ("open N") keeps reporting the
    /// true in-flight total; `reserved`/`dispatched` below are the exact
    /// per-status counts.
    pub open: u64,
    pub reserved: u64,
    pub dispatched: u64,
    pub settled: u64,
    pub refunded: u64,
    pub abandoned: u64,
    /// P0-2: reservations a crashed daemon may have dispatched (marked, never
    /// settled) — they keep consuming the reserved amount until a reconcile
    /// or the task-completion finalize closes them.
    pub uncertain: u64,
    /// Rows (of ANY status except the refunded ledger tail) whose
    /// `(session_id, task_id)` has no task row.
    pub dangling: Vec<DanglingReservationRow>,
}

/// One durable per-call prefix observation (v13), ordered oldest-first by
/// row id (= call order within the session). Read back for the router's
/// prefix-stability aggregation; rows recorded before v13 (or settled
/// without a prefix hash) are excluded by
/// [`Store::provider_call_prefix_rows`] — a missing observation is not a
/// zero.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCallPrefixRow {
    /// The provider_call row id — call order within the session.
    pub row_id: i64,
    pub session_id: SessionId,
    /// Digest of the exact cacheable-prefix byte string the call sent.
    /// Validated to be exactly 32 bytes on every read; anything else is a
    /// loud `Malformed`, never a silent misread.
    pub prompt_prefix_hash: [u8; 32],
    /// Token count of that prefix (u32 bound, matching the router's
    /// `TurnPrefix.prefix_tokens`).
    pub prompt_tokens: u32,
    /// Optional per-row prefix stability in [0, 1] recorded by the
    /// settlement site (NULL = not recorded).
    pub prefix_stability: Option<f64>,
    /// Raw additive segment observation JSON (v19): the exact per-call
    /// `PrefixObservation` the settlement site measured (eight segment
    /// digests + token counts + this call's observed cache reads). The
    /// store validates its strict shape and bounds on write AND read — a
    /// corrupt row is a loud `Malformed`, never a silently degraded
    /// observation. `None` on pre-v19 (legacy) rows: absence is an honest
    /// "no segment identity recorded", never a guess.
    pub prefix_segments_json: Option<String>,
}

/// Hard bound on the serialized per-call segment observation persisted in
/// `provider_call.prefix_segments_json` (schema v19) — mirror of the wire
/// plan's own serialization bound: a hostile row may not describe an
/// unbounded prompt.
pub const MAX_PREFIX_SEGMENTS_JSON: usize = 64 * 1024;

/// Bound on the decoded segment vector inside `prefix_segments_json`
/// (bounded everything: the observation has a fixed conceptual segment
/// count; future segmentations may grow, but never without bound).
pub const MAX_PREFIX_SEGMENTS: usize = 64;

/// Session-level aggregate of the STORED per-row prefix stabilities (v13):
/// count, mean and population std dev over rows that carry one. Rows
/// without a recorded stability contribute nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrefixStabilityAggregate {
    pub observations: u64,
    pub mean: f64,
    pub std_dev: f64,
}

// ------------------------------------------------------- durable cost ledger types
// (schema v15, P0-6/12): see the migration block + the store section below.

/// The durable monetary envelope of one task row (schema v15): the cap
/// (`None` = unlimited) and the settled spend. READ-ONLY surface — the
/// session layer never patches these through `TaskBudget`; the cost ledger
/// is their only writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCostRow {
    pub max_cost_micro: Option<u64>,
    pub spent_cost_micro: u64,
}

/// Durable cost-basis tags of one settled reservation (schema v18
/// `cost_basis` column): the honest authority behind `settled_cost_micro`.
/// `ProviderReported` = the provider's billed amount won;
/// `RouteSnapshotEstimate` = the locally calculated categories x the frozen
/// route-time [`PricingSnapshot`] won; `ConservativeReservation` = the
/// task-completion finalize charged the reserved estimate (a dispatched
/// attempt that may have billed); `Unknown` = no price authority existed and
/// the row closed as a documented Unknown spend (nothing folded). Pre-v18
/// rows read `None` — the basis was never recorded.
pub const COST_BASIS_PROVIDER_REPORTED: &str = "ProviderReported";

pub const COST_BASIS_ROUTE_SNAPSHOT_ESTIMATE: &str = "RouteSnapshotEstimate";

pub const COST_BASIS_CONSERVATIVE_RESERVATION: &str = "ConservativeReservation";

pub const COST_BASIS_UNKNOWN: &str = "Unknown";

/// Durable wire-delivery phases of one reservation (schema v18
/// `delivery_state` column): what the attempt's provider delivery reached,
/// independent of the ledger status. NULL = nothing was ever dispatched
/// (or the row predates v18); `dispatched` = the request left the process
/// (the durable marker was written); `completed` = the usage settled
/// normally; `failed` = a terminal failure was recorded (uncertain). Written
/// only by the ledger's own transitions.
pub const DELIVERY_DISPATCHED: &str = "dispatched";

pub const DELIVERY_COMPLETED: &str = "completed";

pub const DELIVERY_FAILED: &str = "failed";

/// One durable cost reservation (schema v18). Status strings are the
/// ledger's frozen vocabulary: `reserved`, `dispatched`, `settled`,
/// `refunded`, `uncertain` (the v17 migration renamed the v15/v16 `open`
/// state to `reserved` — a reservation holds budget until dispatch — and
/// promoted `dispatched` to a real state so refund-after-dispatch is
/// SQL-impossible; the P0-2 migration renamed the legacy `abandoned` state
/// to `uncertain`, see the migration block comments). `dispatched_ms` is the
/// durable dispatch marker written immediately BEFORE the provider transport
/// call (NULL = dispatch never provably began); `pricing_snapshot_json` is
/// the immutable route-time price capture the settlement math prices usage
/// against (NULL = no pricing authority was consulted). `attempt_op_id` keys
/// this row to its physical network attempt (NULL = legacy row keyed by the
/// shared logical `op_id` only); `parent_op_id` is the shared logical
/// model-call op id every attempt of one call belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostReservationRow {
    pub reservation_id: i64,
    pub session_id: SessionId,
    pub task_id: TaskId,
    /// The op that owns this reservation. Legacy rows (and every pre-attempt
    /// writer): the shared logical model-call op. Attempt-keyed rows carry
    /// the physical attempt's fresh op id; `attempt_op_id` mirrors it and
    /// `parent_op_id` names the logical parent.
    pub op_id: OpId,
    /// The physical-attempt op id this reservation keys by (NULL = legacy
    /// single-attempt row: the op that owned the row was its only attempt).
    pub attempt_op_id: Option<OpId>,
    /// The shared logical model-call op id (the parent of the attempt; NULL
    /// only on rows whose own op was already the parent).
    pub parent_op_id: Option<OpId>,
    pub predicted_micro: u64,
    pub status: String,
    pub created_ms: i64,
    pub settled_ms: Option<i64>,
    /// The durable dispatch marker (NULL = never dispatched before a crash).
    pub dispatched_ms: Option<i64>,
    pub pricing_snapshot: Option<PricingSnapshot>,
    pub provider_cost_micro: Option<u64>,
    pub provider_reported_micro: Option<u64>,
    pub route_decision_json: Option<String>,
    /// The provider request/stream id of the attempt, when one was recorded
    /// (the terminal failure path records it; NULL otherwise).
    pub request_id: Option<String>,
    /// The last known wire-delivery phase; see [`DELIVERY_DISPATCHED`].
    pub delivery_state: Option<String>,
    /// Terminal-failure reason code (set by the uncertain transition).
    pub failure_reason_code: Option<String>,
    /// How `settled_cost_micro` was arrived at; see [`COST_BASIS_PROVIDER_REPORTED`].
    pub cost_basis: Option<String>,
    /// The provider-reported amount of the settlement (the v18 canonical
    /// column; `provider_reported_micro` is its pre-v18 twin).
    pub provider_reported_cost_micro: Option<u64>,
    /// The reserve-time estimate (`predicted_micro` captured durably).
    pub estimated_cost_micro: Option<u64>,
    /// The amount actually folded into the task's spent total (NULL for
    /// Unknown closes and every pre-v18 settlement, which never recorded
    /// which amount was folded).
    pub settled_cost_micro: Option<u64>,
    /// The durable route-time execution class (schema v34): frozen when the
    /// reservation was created from the routing decision's authority.
    /// `Local` ONLY for an authoritative `PriceAuthority::LocalZero`;
    /// `Unknown` on legacy/NULL rows and unpriced paths — never counted as
    /// local or hosted. A zero-cost hosted row keeps a non-local class.
    pub execution_class: ExecutionClass,
}

/// One durable settlement outbox row (schema v45). The settlement
/// transaction writes it together with the reservation's terminal state and
/// the task's folded spend; the commercial projector drains it incrementally
/// and idempotently (one row per reservation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingOutboxRow {
    pub outbox_id: i64,
    pub reservation_id: i64,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub attempt_id: String,
    pub provider: String,
    pub model: String,
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub provider_cost_micro: u64,
    pub provider_reported_micro: Option<u64>,
    pub state: String,
    pub occurred_at_ms: i64,
    pub source_operation: String,
    pub period_id: String,
    pub period_start_ms: i64,
    pub period_end_ms: i64,
}

/// The exact settlement fact one outbox row carries. Written in the SAME
/// transaction as the reservation's `settled` transition and the task's
/// folded spend, so a crash can never leave settled money outside the
/// outbox (and a replay is idempotent by the reservation identity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingOutboxFact {
    pub reservation_id: i64,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub attempt_id: String,
    pub provider: String,
    pub model: String,
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub provider_cost_micro: u64,
    pub provider_reported_micro: Option<u64>,
    pub occurred_at_ms: i64,
    pub source_operation: String,
}

/// Bound on one outbox page read (bounded everything).
pub const MAX_BILLING_OUTBOX_PAGE: i64 = 10_000;

fn outbox_period(anchor_ms: Option<i64>, period_ms: i64, at_ms: i64) -> BillingPeriod {
    BillingPeriod::derive_or_unconfigured(anchor_ms, period_ms, at_ms)
}

fn insert_billing_outbox_tx(
    tx: &rusqlite::Transaction<'_>,
    fact: &BillingOutboxFact,
) -> StoreResult<bool> {
    let period = {
        let config: Option<(i64, i64)> = tx
            .query_row(
                "SELECT anchor_ms, period_ms FROM billing_period_config WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match config {
            Some((anchor, period_ms)) => {
                outbox_period(Some(anchor), period_ms, fact.occurred_at_ms)
            }
            None => BillingPeriod::unconfigured(),
        }
    };
    // INSERT OR IGNORE: a settlement is exactly-once, but a legacy backfill
    // or a replayed reconciliation may present the same reservation fact
    // again — the unique idempotency key makes the replay a no-op.
    let changed = tx.execute(
        "INSERT OR IGNORE INTO billing_outbox
            (idempotency_key, reservation_id, session_id, task_id, attempt_id,
             provider, model, uncached_input_tokens, cache_read_tokens,
             cache_write_tokens, output_tokens, reasoning_tokens,
             provider_cost_micro, provider_reported_micro, state,
             occurred_at_ms, source_operation, period_id, period_start_ms,
             period_end_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, 'settled', ?15, ?16, ?17, ?18, ?19)",
        params![
            format!("reservation:{}", fact.reservation_id),
            fact.reservation_id,
            fact.session_id.raw() as i64,
            fact.task_id.raw() as i64,
            fact.attempt_id,
            fact.provider,
            fact.model,
            fact.uncached_input_tokens.min(i64::MAX as u64) as i64,
            fact.cache_read_tokens.min(i64::MAX as u64) as i64,
            fact.cache_write_tokens.min(i64::MAX as u64) as i64,
            fact.output_tokens.min(i64::MAX as u64) as i64,
            fact.reasoning_tokens.min(i64::MAX as u64) as i64,
            fact.provider_cost_micro.min(i64::MAX as u64) as i64,
            fact.provider_reported_micro
                .map(|m| m.min(i64::MAX as u64) as i64),
            fact.occurred_at_ms,
            fact.source_operation,
            period.id,
            period.start_ms,
            period.end_ms,
        ],
    )?;
    Ok(changed > 0)
}

/// One `cost_reservation` row from the canonical 23-column projection, with
/// the read-time validation shared by [`Store::cost_reservations_of`] and
/// [`Store::cost_reservations_for_tasks`]: v34 execution-class tags are
/// validated loudly and a corrupt `pricing_snapshot_json` is `Corrupt` —
/// never a silent misread.
fn cost_reservation_row_map(r: &rusqlite::Row<'_>) -> StoreResult<CostReservationRow> {
    let reservation_id: i64 = r.get(0)?;
    let row_session: i64 = r.get(1)?;
    let row_task: i64 = r.get(2)?;
    let op_id: i64 = r.get(3)?;
    let predicted: i64 = r.get(4)?;
    let status: String = r.get(5)?;
    let created_ms: i64 = r.get(6)?;
    let settled_ms: Option<i64> = r.get(7)?;
    let dispatched_ms: Option<i64> = r.get(8)?;
    let snapshot_json: Option<String> = r.get(9)?;
    let provider_cost: Option<i64> = r.get(10)?;
    let provider_reported: Option<i64> = r.get(11)?;
    let route_json: Option<String> = r.get(12)?;
    let attempt_op_id: Option<i64> = r.get(13)?;
    let parent_op_id: Option<i64> = r.get(14)?;
    let request_id: Option<String> = r.get(15)?;
    let delivery_state: Option<String> = r.get(16)?;
    let failure_reason_code: Option<String> = r.get(17)?;
    let cost_basis: Option<String> = r.get(18)?;
    let provider_reported_cost_micro: Option<i64> = r.get(19)?;
    let estimated_cost_micro: Option<i64> = r.get(20)?;
    let settled_cost_micro: Option<i64> = r.get(21)?;
    let execution_class_tag: Option<String> = r.get(22)?;
    // v34 execution class: NULL (legacy/unpriced) = Unknown; an
    // unrecognized tag is a loud corruption, never a guessed class.
    let execution_class = match execution_class_tag {
        None => ExecutionClass::Unknown,
        Some(tag) => ExecutionClass::from_tag(&tag).ok_or_else(|| {
            StoreError::Corrupt(vec![format!(
                "cost_reservation {reservation_id} execution_class tag {tag:?} is not a \
                 known execution class"
            )])
        })?,
    };
    let pricing_snapshot = match snapshot_json {
        Some(json) => Some(parse_json(
            &format!("reservation {reservation_id} pricing_snapshot_json"),
            &json,
        )?),
        None => None,
    };
    Ok(CostReservationRow {
        reservation_id,
        session_id: id_field(
            &format!("cost_reservation {reservation_id} session_id"),
            row_session,
        )?,
        task_id: id_field(
            &format!("cost_reservation {reservation_id} task_id"),
            row_task,
        )?,
        op_id: id_field(&format!("cost_reservation {reservation_id} op_id"), op_id)?,
        attempt_op_id: id_field_opt(
            &format!("cost_reservation {reservation_id} attempt_op_id"),
            attempt_op_id,
        )?,
        parent_op_id: id_field_opt(
            &format!("cost_reservation {reservation_id} parent_op_id"),
            parent_op_id,
        )?,
        predicted_micro: predicted.max(0) as u64,
        status,
        created_ms,
        settled_ms,
        dispatched_ms,
        pricing_snapshot,
        provider_cost_micro: provider_cost.map(|m| m.max(0) as u64),
        provider_reported_micro: provider_reported.map(|m| m.max(0) as u64),
        route_decision_json: route_json,
        request_id,
        delivery_state,
        failure_reason_code,
        cost_basis,
        provider_reported_cost_micro: provider_reported_cost_micro.map(|m| m.max(0) as u64),
        estimated_cost_micro: estimated_cost_micro.map(|m| m.max(0) as u64),
        settled_cost_micro: settled_cost_micro.map(|m| m.max(0) as u64),
        execution_class,
    })
}

// ------------------------------------ cross-session usage batch reads (perf)
//
// The cross-session `/native/usage` aggregate used to run per-session and
// per-task store round-trips (sessions × 3 + tasks × 2 statements). These
// batch reads serve the same rows in ONE statement per bounded session/task
// chunk. Byte-identical semantics: every field is computed with the same
// expression as the single-session read, and any row the batch cannot serve
// EXACTLY (a shape the per-row validated read would reject, a session with
// task rows beyond the caller's scope, or a chunk-level SQL failure) is
// reported in `unreadable` for the caller's typed per-session fallback.

/// One session's provider-call usage aggregate from
/// [`Store::session_usage_aggregates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUsageAggregate {
    pub session_id: SessionId,
    /// `COALESCE(SUM(tokens_in), 0) + COALESCE(SUM(tokens_out), 0)`,
    /// clamped like [`Store::session_usage_tokens`].
    pub tokens: u64,
    /// Prefix rows (`prompt_prefix_hash IS NOT NULL`).
    pub prefix_rows: u64,
    pub prefix_tokens: u64,
    pub prefix_stability_observations: u64,
    /// False when a prefix row's stored shape can only be validated (or
    /// rejected) by the per-row read: a non-NULL `prefix_segments_json` or a
    /// cheap shape anomaly (wrong hash length, NULL/out-of-range tokens,
    /// out-of-range stability). The aggregate's prefix fields are then NOT
    /// authoritative for this session — the caller re-reads them through
    /// [`Store::provider_call_prefix_rows`].
    pub prefix_servable: bool,
}

/// Result of [`Store::session_usage_aggregates`].
#[derive(Debug, Clone, Default)]
pub struct SessionUsageBatch {
    /// One row per session with at least one `provider_call` row; sessions
    /// without rows are absent (zero aggregates, exactly like the
    /// single-session reads).
    pub rows: Vec<SessionUsageAggregate>,
    /// Sessions whose chunk failed at the SQL layer: the caller re-reads
    /// them through the per-session APIs (typed fallback).
    pub unreadable: Vec<SessionId>,
}

/// One task's usage-relevant scope row from [`Store::task_usage_scope`]:
/// the legacy `list_tasks` order and the raw cost columns of
/// [`Store::cost_task_row`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskUsageRow {
    pub session_id: SessionId,
    pub task_id: TaskId,
    /// Raw stored `max_cost_micro` (the caller clamps exactly like
    /// `cost_task_row`: `.map(|m| m.max(0) as u64)`).
    pub max_cost_micro: Option<i64>,
    /// Raw stored `spent_cost_micro` (the caller converts exactly like
    /// `cost_task_row`: `u64::try_from(raw).unwrap_or(u64::MAX)`).
    pub spent_cost_micro: i64,
}

/// Result of [`Store::task_usage_scope`].
#[derive(Debug, Clone, Default)]
pub struct TaskUsageBatch {
    /// The first `per_session` task rows of every servable session, in the
    /// legacy `list_tasks` order (created_ms ASC, task_id ASC).
    pub rows: Vec<TaskUsageRow>,
    /// Sessions whose task scope the batch cannot serve: a row failed the
    /// legacy `task_row_map` decode, rows exist beyond `per_session` (the
    /// legacy read decodes them), or the chunk failed at the SQL layer.
    /// The caller falls back to `list_tasks` + per-task reads.
    pub unreadable: Vec<SessionId>,
}

/// Result of one reservation batch chunk: the servable rows plus the pairs
/// whose rows failed read-time validation (the caller falls back per task).
type ReservationChunkScan = (Vec<CostReservationRow>, Vec<(SessionId, TaskId)>);

/// Result of one usage-facts batch chunk: the kind-`usage` rows plus the
/// sessions whose rows failed read-time validation (the caller falls back
/// per session).
type UsageFactsChunkScan = (Vec<(SessionId, String, String)>, Vec<SessionId>);

/// First-occurrence-deduplicated session ids: the batch reads take SETS, so
/// an id that straddles two 500-id chunks is queried (and served) exactly
/// once.
fn unique_session_ids(session_ids: &[SessionId]) -> Vec<SessionId> {
    let mut seen: std::collections::HashSet<u64> =
        std::collections::HashSet::with_capacity(session_ids.len());
    session_ids
        .iter()
        .copied()
        .filter(|s| seen.insert(s.raw()))
        .collect()
}

/// First-occurrence-deduplicated `(session, task)` pairs (see
/// [`unique_session_ids`]).
fn unique_pairs(pairs: &[(SessionId, TaskId)]) -> Vec<(SessionId, TaskId)> {
    let mut seen: std::collections::HashSet<(u64, u64)> =
        std::collections::HashSet::with_capacity(pairs.len());
    pairs
        .iter()
        .copied()
        .filter(|(s, t)| seen.insert((s.raw(), t.raw())))
        .collect()
}

/// Result of [`Store::cost_reservations_for_tasks`].
#[derive(Debug, Clone, Default)]
pub struct TaskReservationBatch {
    /// Rows of every servable pair, ordered (session, task,
    /// reservation_id DESC) — the exact per-task order
    /// [`Store::cost_reservations_of`] serves.
    pub rows: Vec<CostReservationRow>,
    /// Pairs whose rows failed the read-time validation or whose chunk
    /// failed at the SQL layer: the caller falls back to the single-task
    /// read (which reproduces the skip semantics).
    pub unreadable: Vec<(SessionId, TaskId)>,
}

/// Result of [`Store::usage_facts_by_session`]: the `kind = "usage"` memory
/// facts of a session set, in the exact per-session order
/// [`Store::memory_facts`] serves.
#[derive(Debug, Clone, Default)]
pub struct UsageFactsBatch {
    /// `(session, key, value)` rows of kind `usage`, per session ordered by
    /// `updated_ms DESC, kind DESC, key DESC`. A session in `unreadable` is
    /// never served here.
    pub rows: Vec<(SessionId, String, String)>,
    /// Sessions the batch cannot serve EXACTLY: a row of ANY kind whose
    /// storage the per-session read would refuse (the legacy read decodes
    /// every kind), or a chunk-level SQL failure. The caller re-reads them
    /// through [`Store::memory_facts`].
    pub unreadable: Vec<SessionId>,
}

/// Every `TaskState` variant with its canonical row text, prepared on the
/// caller's thread. Writer jobs that must decide a state-dependent rule from
/// the RAW `task.state` column (audit finding 5) compare against these texts
/// instead of decoding the row on the single writer owner; an unknown text is
/// corruption, exactly like a failed `parse_json`. The exhaustive `match` is
/// a compile-time drift guard: a new variant fails this build until the
/// vocabulary is extended.
pub(crate) fn task_state_vocabulary() -> Vec<(String, TaskState)> {
    use TaskState::*;
    let all = [
        Pending,
        Planning,
        Running,
        Waiting,
        Blocked,
        NeedsVerification,
        Verifying,
        VerifiedComplete,
        Failed,
        Cancelled,
    ];
    for state in all {
        match state {
            Pending | Planning | Running | Waiting | Blocked | NeedsVerification | Verifying
            | VerifiedComplete | Failed | Cancelled => {}
        }
    }
    all.into_iter()
        .map(|state| {
            (
                serde_json::to_string(&state)
                    .expect("in-process TaskState serialization cannot fail"),
                state,
            )
        })
        .collect()
}

/// Raw-text terms of the provider-operation reserve gate (audit finding 5):
/// the canonical `task.state` texts that PERMIT a new reserve, the canonical
/// texts that FORBID one together with their caller-prepared diagnostics,
/// and the corrupt-state diagnostic. Built on the CALLER's thread, so the
/// reserve writer closures compare raw columns and never decode `TaskState`
/// on the single writer owner.
fn provider_operation_gate_terms() -> (Vec<String>, Vec<(String, String)>, String) {
    let mut permitted = Vec::new();
    let mut forbidden = Vec::new();
    for (text, state) in task_state_vocabulary() {
        if Store::task_state_permits_provider_operation(state) {
            permitted.push(text);
        } else {
            forbidden.push((
                text,
                format!(
                    "cost reserve: task state {} forbids a new provider operation",
                    state.label()
                ),
            ));
        }
    }
    let corrupt = "cost reserve: task row state is not a known task state".to_string();
    (permitted, forbidden, corrupt)
}

/// One reservation attempt's outcome (schema v15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CostReserveOutcome {
    /// The reservation is reserved, holding `predicted_micro` of the task's
    /// budget. The id is the AUTOINCREMENT row id: monotonic across daemon
    /// restarts and never reused.
    Granted(i64),
    /// spent + predicted would exceed the task's cap (`max` 0/None =
    /// unlimited). NOTHING is written.
    Exceeded { free: u64 },
}

/// The state a reservation must hold for settle/mark to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CostReservationState {
    /// The reservation moved to the requested state.
    Applied,
    /// The reservation exists but is not in the required state (double
    /// settle / settle after refund / mark after settle): typed, nothing
    /// written.
    NotOpen { current: String },
    /// No reservation row with this id.
    Missing,
}

/// The outcome of a refund attempt (schema v18). The refund's guarded UPDATE
/// (`status IN ('reserved','open') AND dispatched_ms IS NULL`) makes
/// refund-after-dispatch impossible AT THE SQL LEVEL: a dispatched, settled,
/// refunded or uncertain reservation changes zero rows and is refused here
/// with the full row truth (`current` status + dispatch marker), so the
/// session layer can raise `CannotRefundDispatched` instead of freeing
/// money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefundOutcome {
    /// The reservation moved RESERVED -> REFUNDED; the prediction is free.
    Applied,
    /// The reservation exists but is no longer refundable pre-dispatch.
    /// `dispatched_ms` carries the durable marker (Some = a dispatch may
    /// have reached the provider — the money stays put).
    Blocked {
        current: String,
        dispatched_ms: Option<i64>,
    },
    /// No reservation row with this id.
    Missing,
}

/// The state a usage settlement landed in (P0-1 settlement truth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CostSettleOutcome {
    /// The reservation closed SETTLED and `actual_micro` (the locally
    /// calculated cost) was folded into the task's spent total.
    Applied { actual_micro: u64 },
    /// The reservation closed SETTLED with NO local cost: the price
    /// source was Unknown/absent and the task has no hard cap — the row
    /// records an Unknown spend (both amount columns NULL; nothing
    /// folded). Never a fabricated zero or one.
    AppliedUnknown,
    /// The reservation exists but is not `open`.
    NotOpen { current: String },
    /// No reservation row with this id.
    Missing,
    /// No price authority (no snapshot, or an Unknown-source snapshot)
    /// and the task HAS a hard cost cap: settlement is refused — typed —
    /// and NOTHING is written (the row stays OPEN; recovery/finalize
    /// resolve it conservatively). Never pretend zero or one.
    UnknownPrice { reservation: i64 },
}

/// The outcome of one [`Store::cost_reconcile_uncertain`] pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CostReconcileReport {
    /// UNCERTAIN reservations closed SETTLED from a durable
    /// `provider_call` row's tokens, priced at the row's snapshot.
    pub settled: u64,
    /// UNCERTAIN reservations closed as documented Unknown spends (no
    /// price authority, no hard cap): amount columns NULL, nothing
    /// folded.
    pub closed_unknown: u64,
    /// UNCERTAIN reservations left untouched (no price authority under
    /// a hard cap, or no completed provider-call row for their op).
    pub left_uncertain: u64,
    /// Total microUSD folded into the task's spent total.
    pub charged_micro: u64,
}

/// The outcome of one [`Store::cost_finalize_uncertain`] pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CostFinalizeReport {
    /// UNCERTAIN reservations conservatively closed SETTLED at their
    /// reserved estimate.
    pub settled: u64,
    /// Rows left UNCERTAIN (their task row is gone: nothing can fold).
    pub left_uncertain: u64,
    /// Total microUSD charged (each row's `predicted_micro`).
    pub charged_micro: u64,
}

/// Per-candidate outcome of [`Store::cost_reconcile_uncertain`] (folded into
/// the report OUTSIDE the writer command).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CostReconcileOutcome {
    Skipped,
    LeftUncertain,
    ClosedUnknown,
    Settled(u64),
}

/// Per-candidate outcome of [`Store::cost_finalize_uncertain`] (folded into
/// the report OUTSIDE the writer command).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CostFinalizeOutcome {
    Skipped,
    LeftUncertain,
    Settled(u64),
}

// ---------------------------------------------------------------------------
// Verified-outcome learning (audit items 13/14/L, migration v18 / schema
// target 19): durable per-key verified-outcome accumulators.
//
// The `model_outcome_stats` table is a MATERIALIZED PROJECTION: each row is
// keyed `(provider, model, phase, task_class, risk_bucket)` and holds the
// five accumulators exactly as the router's registry defines them. Samples
// enter ONLY through [`Store::model_outcome_stats_append`] (one
// transactional read-modify-write per fact), which mirrors the router-side
// absorb rule: `sample_count = successes_first_pass + failures_first_pass`,
// and rework sums grow ONLY on failure samples — a verified first-pass
// success can never cause rework. Rows survive reopen (append facts +
// projection), reads parse every column fallibly (`Corrupt`, never a panic)
// and the phase consult ([`Store::model_outcome_stats_phase`]) folds every
// class/risk bucket of one (provider, model, phase) with saturating sums.
// ---------------------------------------------------------------------------

/// One durable per-key verified-outcome accumulator row (schema target 19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOutcomeStatsRow {
    pub provider: String,
    pub model: String,
    pub phase: RouterPhase,
    pub task_class: TaskClass,
    pub risk_bucket: RiskBucket,
    pub successes_first_pass: u64,
    pub failures_first_pass: u64,
    pub rework_cost_micro_sum: u64,
    pub rework_turns_sum: u64,
    pub sample_count: u64,
    pub updated_ms: i64,
}

/// ONE verified-outcome fact: the explicit verified-success signal plus the
/// rework its failure eventually caused. Mirrors the router registry's
/// sample shape; "the model said done" is NOT a verified signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelOutcomeSample {
    pub verified_success: bool,
    pub rework_cost_micro: u64,
    pub rework_turns: u64,
}

/// One verified sample bound to its full outcome-registry key: the unit the
/// Engineering Outcome Ledger's atomic append writes into
/// `model_outcome_stats` in the SAME transaction that inserts the run's
/// ledger row (exactly-once record AND exactly-once router feed). Samples
/// enter only with the explicit verified attribution, exactly like
/// [`Store::model_outcome_stats_append`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOutcomeIncrement {
    pub provider: String,
    pub model: String,
    pub phase: RouterPhase,
    pub task_class: TaskClass,
    pub risk_bucket: RiskBucket,
    pub sample: ModelOutcomeSample,
}

/// One outcome-registry key with every enum dimension already encoded as
/// its durable text (prepared on the caller's thread; the writer job binds
/// the borrows as-is).
pub(crate) struct PreparedOutcomeKey<'a> {
    pub(crate) provider: &'a str,
    pub(crate) model: &'a str,
    pub(crate) phase: &'a str,
    pub(crate) task_class: &'a str,
    pub(crate) risk_bucket: &'a str,
}

/// SQLite INTEGER is signed 64-bit: a u64 accumulator above `i64::MAX`
/// cannot be stored as one integer, so every column clamps at `i64::MAX`
/// (a count of rework beyond 9.22e18 samples is unrepresentable, and the
/// equality CHECK refuses near-boundary rows loudly instead of letting the
/// projection drift).
pub(crate) fn outcome_clamp_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

pub(crate) fn outcome_db_phase(p: RouterPhase) -> String {
    serde_json::to_string(&p).expect("unit enum serialization cannot fail")
}

pub(crate) fn outcome_db_class(c: TaskClass) -> String {
    serde_json::to_string(&c).expect("unit enum serialization cannot fail")
}

pub(crate) fn outcome_db_bucket(b: RiskBucket) -> String {
    serde_json::to_string(&b).expect("unit enum serialization cannot fail")
}

pub(crate) const OUTCOME_STATS_KEY_SQL: &str =
    "provider = ?1 AND model = ?2 AND phase = ?3 AND task_class = ?4 AND risk_bucket = ?5";

/// One raw `model_outcome_stats` row exactly as stored: enum dimensions are
/// JSON-encoded TEXT and stay unparsed until [`outcome_stats_row_validate`]
/// turns the row into its typed shape.
pub(crate) type RawOutcomeStatsRow = (
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
);

pub(crate) fn outcome_stats_row_raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawOutcomeStatsRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
    ))
}

/// Parse-fallibly validates one raw projection row into its typed shape:
/// the enum texts are JSON (`"implement"`), so unknown or version-skewed
/// text is `Corrupt`, and an invariant-broken row (`sample_count !=
/// successes + failures`) is refused the same way — never silently trusted.
pub(crate) fn outcome_stats_row_validate(
    raw: RawOutcomeStatsRow,
) -> StoreResult<ModelOutcomeStatsRow> {
    let (
        provider,
        model,
        phase,
        task_class,
        risk_bucket,
        successes_raw,
        failures_raw,
        cost_sum_raw,
        turns_sum_raw,
        sample_raw,
        updated_ms,
    ) = raw;
    let ctx = |col: &str| format!("model_outcome_stats {provider}/{model} {col}");
    let phase: RouterPhase = parse_json(&ctx("phase"), &phase)?;
    let task_class: TaskClass = parse_json(&ctx("task_class"), &task_class)?;
    let risk_bucket: RiskBucket = parse_json(&ctx("risk_bucket"), &risk_bucket)?;
    let successes_first_pass = successes_raw.max(0) as u64;
    let failures_first_pass = failures_raw.max(0) as u64;
    let rework_cost_micro_sum = cost_sum_raw.max(0) as u64;
    let rework_turns_sum = turns_sum_raw.max(0) as u64;
    let sample_count = sample_raw.max(0) as u64;
    if sample_count != successes_first_pass.saturating_add(failures_first_pass) {
        return Err(StoreError::Corrupt(vec![format!(
            "model_outcome_stats {provider}/{model} {phase:?}/{task_class:?}/{risk_bucket:?} \
             sample_count {sample_count} != successes {successes_first_pass} + failures \
             {failures_first_pass}"
        )]));
    }
    Ok(ModelOutcomeStatsRow {
        provider,
        model,
        phase,
        task_class,
        risk_bucket,
        successes_first_pass,
        failures_first_pass,
        rework_cost_micro_sum,
        rework_turns_sum,
        sample_count,
        updated_ms,
    })
}

/// Strict shape check for the v19 `provider_call.prefix_segments_json`
/// payload — the durable mirror of the wire plan's per-call
/// `PrefixObservation` serialization. Valid BOTH on write (the typed API
/// refuses hostile payloads before anything touches the row) and on read
/// (a payload injected behind the API's back is a loud `Malformed`, never a
/// silently degraded observation):
///
/// ```text
/// { "segment_hashes": ["<64 hex>", ...],
///   "segment_token_counts": [<u64>, ...],
///   "cache_read_tokens": <u64> }
/// ```
///
/// Exactly those three fields (unknown fields are corruption, matching the
/// wire type's own strict decode), byte-bounded by
/// [`MAX_PREFIX_SEGMENTS_JSON`], hash/token vectors of EQUAL length and at
/// most [`MAX_PREFIX_SEGMENTS`] entries, every hash a 64-char hex digest.
pub(crate) fn validate_prefix_segments_json(json: &str) -> StoreResult<()> {
    if json.len() > MAX_PREFIX_SEGMENTS_JSON {
        return Err(StoreError::Oversized(format!(
            "prefix_segments_json is {} bytes, over the {MAX_PREFIX_SEGMENTS_JSON}-byte bound",
            json.len()
        )));
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawObservation {
        segment_hashes: Vec<String>,
        segment_token_counts: Vec<u64>,
        cache_read_tokens: u64,
    }
    let raw: RawObservation = serde_json::from_str(json).map_err(|e| {
        StoreError::Malformed(format!(
            "prefix_segments_json is not a valid observation: {e}"
        ))
    })?;
    if raw.segment_hashes.len() != raw.segment_token_counts.len() {
        return Err(StoreError::Malformed(format!(
            "prefix_segments_json hash/token length mismatch: {} vs {}",
            raw.segment_hashes.len(),
            raw.segment_token_counts.len()
        )));
    }
    if raw.segment_hashes.len() > MAX_PREFIX_SEGMENTS {
        return Err(StoreError::Malformed(format!(
            "prefix_segments_json carries {} segments, over the {MAX_PREFIX_SEGMENTS} bound",
            raw.segment_hashes.len()
        )));
    }
    for hash in &raw.segment_hashes {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::Malformed(
                "prefix_segments_json carries a non-64-char-hex segment digest".into(),
            ));
        }
    }
    // `cache_read_tokens` is decoded but not otherwise constrained: any
    // provider-reported count is a legal observation, it never prices a
    // call by itself.
    let _ = raw.cache_read_tokens;
    Ok(())
}

// ---------------------------------------------------------------------------
// Efficiency KPI read (audit 84-88, additive; NO schema change): the
// per-task projection of durable `provider_call` rows the efficiency harness
// sums into `TaskEfficiencyMetrics`. This read exists because no existing
// surface attributes provider calls to a TASK: the v18 attempt columns and
// the legacy logical-op columns already carry the linkage, but only at the
// SQL level. The derivation itself (summing input/output tokens, counting
// usage rows, attributing cache from the durable prefix observation) lives
// in test-only harnesses; this method is the durable read they derive
// from.
// ---------------------------------------------------------------------------

/// One `provider_call` row attributable to one task, as read by
/// [`Store::provider_call_task_rows`].
///
/// Two row classes share the table and are distinguished by their counters:
///
/// - a USAGE row is the physical (or legacy logical) call record: it carries
///   `tokens_in`/`tokens_out`, or a non-`completed` status (a failure row
///   with no counters is still a call);
/// - a PREFIX-OBSERVATION row is written by
///   [`Store::record_provider_call_with_prefix`] (the v13 settlement twin):
///   `completed` status, NULL usage counters, and the durable
///   `prompt_tokens` + `prefix_stability` pair. It describes the SAME call
///   as its usage row, so callers must never count it as a call (the
///   efficiency harness classifies it exactly this way).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCallTaskRow {
    /// The `provider_call` row id (call order within the session).
    pub row_id: i64,
    /// The shared logical model-call op the row keys by.
    pub op_id: OpId,
    /// The physical attempt op id (v18 attempt rows; NULL on legacy and
    /// prefix-observation rows).
    pub attempt_op_id: Option<OpId>,
    /// The reservation this call keys to (v18 attempt rows; NULL otherwise).
    pub reservation_id: Option<i64>,
    pub provider: String,
    pub model: String,
    pub status: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// Cacheable-prefix token count of the durable prefix observation (NULL
    /// when no observation was recorded — never a fabricated zero).
    pub prompt_tokens: Option<u64>,
    /// Per-turn prefix stability in [0, 1] of the observation (NULL when no
    /// observation was recorded).
    pub prefix_stability: Option<f64>,
}

/// Read-time guard for a durable counter column: negative SQLite integers
/// are corrupt (a counter is non-negative), and are surfaced as a loud
/// `Malformed` instead of being clamped into a silently wrong KPI.
pub(crate) fn efficiency_counter(raw: Option<i64>, what: &str) -> StoreResult<Option<u64>> {
    match raw {
        None => Ok(None),
        Some(v) if v < 0 => Err(StoreError::Malformed(format!(
            "{what} is negative ({v}): a durable token counter cannot be negative"
        ))),
        Some(v) => Ok(Some(v as u64)),
    }
}

impl Store {
    /// Install (or replace) the daemon-wired billing period anchor/length.
    /// The settlement path stamps outbox rows from this pair; an absent pair
    /// stamps the honest all-time bucket.
    pub fn set_billing_period_config(&self, anchor_ms: i64, period_ms: i64) -> StoreResult<()> {
        if anchor_ms < 0 || period_ms <= 0 || period_ms > faktor_core::MAX_BILLING_PERIOD_MS {
            return Err(StoreError::Conflict(format!(
                "billing period config ({anchor_ms}, {period_ms}) is outside the legal domain"
            )));
        }
        self.writer
            .execute("set_billing_period_config", move |conn| {
                conn.execute(
                    "INSERT INTO billing_period_config (id, anchor_ms, period_ms)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET
                    anchor_ms = excluded.anchor_ms,
                    period_ms = excluded.period_ms",
                    params![anchor_ms, period_ms],
                )?;
                Ok(())
            })
    }

    /// The daemon-wired billing period configuration, if any.
    pub fn billing_period_config(&self) -> StoreResult<Option<(i64, i64)>> {
        let conn = self.read()?;
        conn.query_row(
            "SELECT anchor_ms, period_ms FROM billing_period_config WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// One bounded ascending page of the durable settlement outbox strictly
    /// after `after_id`.
    pub fn billing_outbox_after(
        &self,
        after_id: i64,
        limit: usize,
    ) -> StoreResult<Vec<BillingOutboxRow>> {
        let limit = i64::try_from(limit)
            .unwrap_or(MAX_BILLING_OUTBOX_PAGE)
            .clamp(0, MAX_BILLING_OUTBOX_PAGE);
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT outbox_id, reservation_id, session_id, task_id, attempt_id,
                    provider, model, uncached_input_tokens, cache_read_tokens,
                    cache_write_tokens, output_tokens, reasoning_tokens,
                    provider_cost_micro, provider_reported_micro, state,
                    occurred_at_ms, source_operation, period_id, period_start_ms,
                    period_end_ms
             FROM billing_outbox WHERE outbox_id > ?1
             ORDER BY outbox_id ASC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![after_id, limit])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let outbox_id: i64 = r.get(0)?;
            out.push(BillingOutboxRow {
                outbox_id,
                reservation_id: r.get(1)?,
                session_id: id_field(&format!("billing_outbox {outbox_id} session_id"), r.get(2)?)?,
                task_id: id_field(&format!("billing_outbox {outbox_id} task_id"), r.get(3)?)?,
                attempt_id: r.get(4)?,
                provider: r.get(5)?,
                model: r.get(6)?,
                uncached_input_tokens: efficiency_counter(
                    r.get(7)?,
                    "billing_outbox uncached_input_tokens",
                )?
                .unwrap_or(0),
                cache_read_tokens: efficiency_counter(
                    r.get(8)?,
                    "billing_outbox cache_read_tokens",
                )?
                .unwrap_or(0),
                cache_write_tokens: efficiency_counter(
                    r.get(9)?,
                    "billing_outbox cache_write_tokens",
                )?
                .unwrap_or(0),
                output_tokens: efficiency_counter(r.get(10)?, "billing_outbox output_tokens")?
                    .unwrap_or(0),
                reasoning_tokens: efficiency_counter(
                    r.get(11)?,
                    "billing_outbox reasoning_tokens",
                )?
                .unwrap_or(0),
                provider_cost_micro: efficiency_counter(
                    r.get(12)?,
                    "billing_outbox provider_cost_micro",
                )?
                .unwrap_or(0),
                provider_reported_micro: efficiency_counter(
                    r.get(13)?,
                    "billing_outbox provider_reported_micro",
                )?,
                state: r.get(14)?,
                occurred_at_ms: r.get(15)?,
                source_operation: r.get(16)?,
                period_id: r.get(17)?,
                period_start_ms: r.get(18)?,
                period_end_ms: r.get(19)?,
            });
        }
        Ok(out)
    }

    /// Whether the one-time legacy backfill scanned the pre-outbox
    /// reservation ledger into the outbox.
    pub fn billing_outbox_backfill_done(&self) -> StoreResult<bool> {
        let conn = self.read()?;
        Ok(conn
            .query_row(
                "SELECT done FROM billing_outbox_backfill WHERE id = 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|done| done != 0)
            .unwrap_or(false))
    }

    /// Mark the one-time legacy backfill complete (durable).
    pub fn mark_billing_outbox_backfill_done(&self) -> StoreResult<()> {
        self.writer
            .execute("mark_billing_outbox_backfill_done", |conn| {
                conn.execute(
                    "INSERT INTO billing_outbox_backfill (id, done) VALUES (1, 1)
                 ON CONFLICT(id) DO UPDATE SET done = 1",
                    [],
                )?;
                Ok(())
            })
    }

    /// Write one legacy/backfill settlement fact into the outbox through the
    /// ordinary writer (idempotent by the reservation id).
    pub fn billing_outbox_record(&self, fact: BillingOutboxFact) -> StoreResult<bool> {
        self.writer.execute("billing_outbox_record", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let inserted = insert_billing_outbox_tx(&tx, &fact)?;
            tx.commit()?;
            Ok(inserted)
        })
    }

    // Fixed-arity provider telemetry; the parameter list is a stable call
    // contract used across the workspace.
    #[allow(clippy::too_many_arguments)]
    pub fn record_provider_call(
        &self,
        session_id: SessionId,
        op_id: OpId,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
    ) -> StoreResult<i64> {
        self.record_provider_call_with_prefix(
            session_id, op_id, provider, model, status, tokens_in, tokens_out, error, None, None,
            None,
        )
    }

    /// `record_provider_call` plus the additive prefix-cache stability
    /// observation (v13, audits 65-66): the digest of the exact
    /// cacheable-prefix byte string this call sent, its token count, and
    /// (optionally, once a fill site can measure it) the call's per-turn
    /// prefix stability. All three are optional and default to NULL — a row
    /// without them is a row with no prefix observation, never a guessed
    /// one. Values are validated LOUDLY: `prompt_prefix_hash` must be
    /// exactly 32 bytes, `prompt_tokens` must fit a `u32` (a single prompt
    /// prefix beyond 4.29e9 tokens is rejected as oversized, matching the
    /// router's `TurnPrefix.prefix_tokens`), and `prefix_stability` must be
    /// finite in [0, 1] (NaN/out-of-range is `Malformed`, never silently
    /// stored as NULL or a nonsense number).
    #[allow(clippy::too_many_arguments)]
    pub fn record_provider_call_with_prefix(
        &self,
        session_id: SessionId,
        op_id: OpId,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
        prompt_prefix_hash: Option<[u8; 32]>,
        prompt_tokens: Option<u64>,
        prefix_stability: Option<f64>,
    ) -> StoreResult<i64> {
        self.record_provider_call_with_prefix_segments(
            session_id,
            op_id,
            provider,
            model,
            status,
            tokens_in,
            tokens_out,
            error,
            prompt_prefix_hash,
            prompt_tokens,
            prefix_stability,
            // Legacy callers record no per-call segment observation: the
            // v19 column stays NULL and routing keeps the binary pair rule.
            None,
        )
    }

    /// Additive v19 twin of [`Store::record_provider_call_with_prefix`]:
    /// the prefix observation row additionally carries the raw per-call
    /// segment observation JSON (ordered segment digests + token counts +
    /// observed cache reads) the settlement site measured. The payload is
    /// validated LOUDLY before anything touches the row: bounded by
    /// [`MAX_PREFIX_SEGMENTS_JSON`], exactly the expected strict fields,
    /// hash/token vectors of equal bounded length, every hash a 32-byte hex
    /// digest. `None` records the legacy NULL — no segments, never a guess.
    #[allow(clippy::too_many_arguments)]
    pub fn record_provider_call_with_prefix_segments(
        &self,
        session_id: SessionId,
        op_id: OpId,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
        prompt_prefix_hash: Option<[u8; 32]>,
        prompt_tokens: Option<u64>,
        prefix_stability: Option<f64>,
        prefix_segments_json: Option<&str>,
    ) -> StoreResult<i64> {
        let prompt_tokens = prompt_tokens
            .map(|t| {
                u32::try_from(t).map_err(|_| {
                    StoreError::Oversized(format!(
                        "prompt_tokens {t} exceeds the u32 prefix-token bound"
                    ))
                })
            })
            .transpose()?;
        if let Some(s) = prefix_stability {
            if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                return Err(StoreError::Malformed(format!(
                    "prefix_stability must be finite in [0, 1], got {s}"
                )));
            }
        }
        if let Some(json) = prefix_segments_json {
            validate_prefix_segments_json(json)?;
        }
        let provider = provider.to_owned();
        let model = model.to_owned();
        let status = status.to_owned();
        let error = error.map(|v| v.to_owned());
        let prefix_segments_json = prefix_segments_json.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let ts_ms = now_ms();
        self.writer
            .execute_retry_busy("record_provider_call_with_prefix_segments", move |conn| {
                Self::insert_provider_call_on(
                    conn,
                    session_id,
                    op_id,
                    &provider,
                    &model,
                    &status,
                    tokens_in,
                    tokens_out,
                    error.as_deref(),
                    prompt_prefix_hash,
                    prompt_tokens,
                    prefix_stability,
                    prefix_segments_json.as_deref(),
                    None,
                    None,
                    None,
                    None,
                    ts_ms,
                )
            })
    }

    /// Attempt-oriented provider-call record (attempt accounting, schema
    /// v18): `op_id` keeps its meaning as the shared logical model-call op
    /// id this attempt belongs to (`parent_model_call_op_id` mirrors it
    /// durably), and the row keys to its physical attempt through
    /// `attempt_op_id` (+ `attempt_ordinal`) and to its money through
    /// `reservation_id`. Reconciliation of an uncertain reservation joins
    /// `provider_call.attempt_op_id = cost_reservation.attempt_op_id`, so
    /// two attempts of the same logical op can never settle each other's
    /// crashed reservations. Legacy callers (the current agent runtime) keep
    /// writing through [`Store::record_provider_call`], leaving the attempt
    /// columns NULL — those rows remain the single physical attempt of
    /// their op, exactly as before.
    #[allow(clippy::too_many_arguments)]
    pub fn record_provider_call_attempt(
        &self,
        session_id: SessionId,
        attempt: &faktor_core::op::ModelCallAttempt,
        reservation_id: Option<i64>,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
    ) -> StoreResult<i64> {
        let attempt = attempt.to_owned();
        let provider = provider.to_owned();
        let model = model.to_owned();
        let status = status.to_owned();
        let error = error.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let ts_ms = now_ms();
        self.writer
            .execute_retry_busy("record_provider_call_attempt", move |conn| {
                conn.execute(
                    "INSERT INTO provider_call(session_id, op_id, parent_model_call_op_id,
                attempt_op_id, attempt_ordinal, reservation_id,
                provider, model, started_ms, ended_ms, status, tokens_in,
                tokens_out, error)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        session_id.raw() as i64,
                        attempt.logical_op_id.raw() as i64,
                        attempt.attempt_op_id.raw() as i64,
                        attempt.ordinal as i64,
                        reservation_id,
                        provider,
                        model,
                        ts_ms,
                        status,
                        tokens_in.map(|t| t as i64),
                        tokens_out.map(|t| t as i64),
                        error
                    ],
                )?;
                Ok(conn.last_insert_rowid())
            })
    }

    /// Shared single-row provider-call insert; see [`Self::insert_message_on`].
    /// The usage-settlement row of the hot append surface. The caller supplies
    /// `ts_ms` (captured before enqueueing) and it stamps both `started_ms` and
    /// `ended_ms`. The four attempt parameters are the additive v18 surface and
    /// `prefix_segments_json` the additive v19 one: `None` everywhere
    /// records a legacy row (no attempt identity, no reservation link, no
    /// segment observation).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_provider_call_on(
        conn: &Connection,
        session_id: SessionId,
        op_id: OpId,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
        prompt_prefix_hash: Option<[u8; 32]>,
        prompt_tokens: Option<u32>,
        prefix_stability: Option<f64>,
        prefix_segments_json: Option<&str>,
        attempt_op_id: Option<OpId>,
        attempt_ordinal: Option<u32>,
        parent_model_call_op_id: Option<OpId>,
        reservation_id: Option<i64>,
        ts_ms: i64,
    ) -> StoreResult<i64> {
        conn.execute(
            "INSERT INTO provider_call(session_id, op_id, provider, model, started_ms, ended_ms, status, tokens_in, tokens_out, error, prompt_prefix_hash, prompt_tokens, prefix_stability, prefix_segments_json, attempt_op_id, attempt_ordinal, parent_model_call_op_id, reservation_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                session_id.raw() as i64,
                op_id.raw() as i64,
                provider,
                model,
                ts_ms,
                ts_ms,
                status,
                tokens_in.map(|t| t as i64),
                tokens_out.map(|t| t as i64),
                error,
                prompt_prefix_hash.map(Vec::from),
                prompt_tokens.map(i64::from),
                prefix_stability,
                prefix_segments_json,
                attempt_op_id.map(|id| id.raw() as i64),
                attempt_ordinal.map(i64::from),
                parent_model_call_op_id.map(|id| id.raw() as i64),
                reservation_id
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The session's durable prefix observations, oldest call first (v13).
    /// Rows whose `prompt_prefix_hash` is NULL (pre-v13 or settled without a
    /// prefix) are excluded — a missing observation is not a zero.
    /// Read-time validation is loud: a corrupt shape injected behind the
    /// API's back (wrong-length hash, out-of-range tokens/stability, or a
    /// malformed v19 segment payload) is a `Malformed` error, never a silent
    /// misread.
    pub fn provider_call_prefix_rows(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Vec<ProviderCallPrefixRow>> {
        self.provider_call_prefix_rows_limited(session_id, None)
    }

    /// The newest `limit` provider-call prefix rows of one session, returned
    /// oldest-first. Routing stability only measures each turn against its
    /// IMMEDIATELY previous turn, so the agent reads a bounded tail instead
    /// of re-decoding every call the session ever made (the 1,000-turn probe
    /// exposed the unbounded full-scan cost). `None` keeps the historical
    /// full read for usage/projection callers that really aggregate history.
    pub fn provider_call_prefix_rows_limited(
        &self,
        session_id: SessionId,
        limit: Option<u64>,
    ) -> StoreResult<Vec<ProviderCallPrefixRow>> {
        let conn = self.read()?;
        let sql = match limit {
            Some(_) => {
                "SELECT id, prompt_prefix_hash, prompt_tokens, prefix_stability, prefix_segments_json
                 FROM provider_call
                 WHERE session_id = ?1 AND prompt_prefix_hash IS NOT NULL
                 ORDER BY id DESC LIMIT ?2"
            }
            None => {
                "SELECT id, prompt_prefix_hash, prompt_tokens, prefix_stability, prefix_segments_json
                 FROM provider_call
                 WHERE session_id = ?1 AND prompt_prefix_hash IS NOT NULL
                 ORDER BY id ASC"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let mut rows = match limit {
            Some(limit) => stmt.query(params![
                session_id.raw() as i64,
                i64::try_from(limit).map_err(|_| {
                    StoreError::Oversized("provider prefix history limit exceeds i64".into())
                })?
            ])?,
            None => stmt.query(params![session_id.raw() as i64])?,
        };
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let hash: Option<Vec<u8>> = r.get(1)?;
            let Some(hash) = hash else {
                return Err(StoreError::Malformed(
                    "provider_call prefix row has NULL hash despite the filter".into(),
                ));
            };
            let hash: [u8; 32] = hash.try_into().map_err(|v: Vec<u8>| {
                StoreError::Malformed(format!(
                    "provider_call prefix hash must be exactly 32 bytes, got {}",
                    v.len()
                ))
            })?;
            let tokens: Option<i64> = r.get(2)?;
            let tokens = match tokens {
                None => {
                    return Err(StoreError::Malformed(
                        "provider_call prefix row has NULL prompt_tokens".into(),
                    ));
                }
                Some(t) => u32::try_from(t).map_err(|_| {
                    StoreError::Malformed(format!(
                        "provider_call prompt_tokens {t} out of u32 range"
                    ))
                })?,
            };
            let stability: Option<f64> = r.get(3)?;
            if let Some(s) = stability {
                if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                    return Err(StoreError::Malformed(format!(
                        "provider_call prefix_stability {s} out of [0, 1]"
                    )));
                }
            }
            let prefix_segments_json: Option<String> = r.get(4)?;
            if let Some(json) = &prefix_segments_json {
                validate_prefix_segments_json(json)?;
            }
            out.push(ProviderCallPrefixRow {
                row_id: r.get(0)?,
                session_id,
                prompt_prefix_hash: hash,
                prompt_tokens: tokens,
                prefix_stability: stability,
                prefix_segments_json,
            });
        }
        if limit.is_some() {
            out.reverse();
        }
        Ok(out)
    }

    /// Additive aggregate query over the stored per-row prefix stabilities
    /// of one session (v13): count, mean and population std dev, computed
    /// in SQL then finished in guarded float math. `None` when the session
    /// has no rows with a recorded stability. Never parses the hash BLOBs —
    /// corrupt prefix rows surface through
    /// [`Store::provider_call_prefix_rows`] instead.
    pub fn session_stored_prefix_stability(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<PrefixStabilityAggregate>> {
        let conn = self.read()?;
        let (count, sum, sum_sq): (i64, f64, f64) = conn.query_row(
            "SELECT COUNT(prefix_stability), COALESCE(SUM(prefix_stability), 0.0),
                    COALESCE(SUM(prefix_stability * prefix_stability), 0.0)
             FROM provider_call
             WHERE session_id = ?1
               AND prompt_prefix_hash IS NOT NULL
               AND prefix_stability IS NOT NULL",
            params![session_id.raw() as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if count == 0 {
            return Ok(None);
        }
        // Loud, never silent: a corrupt stability (out of [0, 1]) injected
        // behind the API's back must fail the aggregate, not pollute it.
        let bad: i64 = conn.query_row(
            "SELECT COUNT(*) FROM provider_call
             WHERE session_id = ?1
               AND prompt_prefix_hash IS NOT NULL
               AND prefix_stability IS NOT NULL
               AND (prefix_stability < 0.0 OR prefix_stability > 1.0)",
            params![session_id.raw() as i64],
            |r| r.get(0),
        )?;
        if bad > 0 {
            return Err(StoreError::Malformed(format!(
                "provider_call prefix_stability out of [0, 1] on {bad} row(s)"
            )));
        }
        let n = count as f64;
        // Population variance; guard the float subtraction from a hair
        // below zero on hostile magnitudes.
        let var = ((sum_sq - sum * sum / n) / n).max(0.0);
        Ok(Some(PrefixStabilityAggregate {
            observations: count as u64,
            mean: sum / n,
            std_dev: var.sqrt(),
        }))
    }

    // ---------------------------------------------------------------- checkpoints

    /// The durable budget ledger invariant scan (`doctor --deep`, read-only,
    /// P0-97): every `cost_reservation` row is counted by status and every
    /// row whose `(session_id, task_id)` task row no longer exists is listed
    /// as dangling. A reservation is the ledger's handle onto its task
    /// envelope: a dangling row can never settle or refund, so its predicted
    /// spend silently vanishes from the cap math.
    pub fn cost_reservation_invariants(&self) -> StoreResult<CostReservationScan> {
        let conn = self.read()?;
        let mut scan = CostReservationScan::default();
        {
            let mut stmt =
                conn.prepare("SELECT status, COUNT(*) FROM cost_reservation GROUP BY status")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let status: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                let count = count.max(0) as u64;
                scan.total = scan.total.saturating_add(count);
                match status.as_str() {
                    "open" => {
                        // Legacy pre-v17 vocabulary: 0 on every v18 store.
                        scan.open += count;
                    }
                    "reserved" => {
                        scan.reserved += count;
                        scan.open += count;
                    }
                    "dispatched" => {
                        scan.dispatched += count;
                        scan.open += count;
                    }
                    "settled" => scan.settled += count,
                    "refunded" => scan.refunded += count,
                    "abandoned" => scan.abandoned += count,
                    "uncertain" => scan.uncertain += count,
                    _ => {}
                }
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT cr.reservation_id, cr.session_id, cr.task_id, cr.op_id,
                        cr.predicted_micro, cr.status
                 FROM cost_reservation cr
                 WHERE NOT EXISTS (
                     SELECT 1 FROM task t
                     WHERE t.session_id = cr.session_id AND t.task_id = cr.task_id)
                 ORDER BY cr.reservation_id ASC",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let reservation_id: i64 = row.get(0)?;
                scan.dangling.push(DanglingReservationRow {
                    reservation_id,
                    session_id: id_field(
                        &format!("cost_reservation {reservation_id} session_id"),
                        row.get::<_, i64>(1)?,
                    )?,
                    task_id: id_field(
                        &format!("cost_reservation {reservation_id} task_id"),
                        row.get::<_, i64>(2)?,
                    )?,
                    op_id: id_field(
                        &format!("cost_reservation {reservation_id} op_id"),
                        row.get::<_, i64>(3)?,
                    )?,
                    predicted_micro: row.get::<_, i64>(4)?.max(0) as u64,
                    status: row.get(5)?,
                });
            }
        }
        Ok(scan)
    }

    /// The durable monetary envelope of one task row (schema v15): the cap
    /// (`None` = unlimited) and the settled spend.
    pub fn cost_task_row(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> StoreResult<Option<TaskCostRow>> {
        let conn = self.read()?;
        conn.query_row(
            "SELECT max_cost_micro, spent_cost_micro FROM task
             WHERE session_id = ?1 AND task_id = ?2",
            params![session_id.raw() as i64, task_id.raw() as i64],
            |r| {
                Ok(TaskCostRow {
                    max_cost_micro: r.get::<_, Option<i64>>(0)?.map(|m| m.max(0) as u64),
                    spent_cost_micro: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(u64::MAX),
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Set (or clear) the durable monetary cap of one task row (v15).
    /// `None` = unlimited. A missing task row is a typed `Conflict` — the
    /// task machine owns row creation.
    pub fn cost_task_cap_set(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        max_cost_micro: Option<u64>,
    ) -> StoreResult<()> {
        // Preparation BEFORE enqueueing: the refusal diagnostic (the closure
        // formats nothing).
        let no_row = format!(
            "task {task_id} of session {session_id} has no row; the task machine owns creation"
        );
        self.writer.execute("cost_task_cap_set", move |conn| {
            let n = conn.execute(
                "UPDATE task SET max_cost_micro = ?1 WHERE session_id = ?2 AND task_id = ?3",
                params![
                    max_cost_micro.map(|m| m.min(i64::MAX as u64) as i64),
                    session_id.raw() as i64,
                    task_id.raw() as i64
                ],
            )?;
            if n == 0 {
                return Err(StoreError::Conflict(no_row.clone()));
            }
            Ok(())
        })
    }

    /// Reserve `predicted_micro` of the task's monetary budget in ONE
    /// transaction: the cap is read and the reservation inserted atomically
    /// (spent + predicted > cap => nothing written). A missing task row is a
    /// typed `Conflict` (drives create the row before any paid call); a task
    /// row in a completion/final state (`NeedsVerification`, `Verifying`,
    /// `VerifiedComplete`, `Failed`, `Cancelled`) refuses a new provider
    /// operation with a typed `Conflict` and writes NOTHING.
    ///
    /// This is the legacy entry point (no pricing snapshot: the row is
    /// reserved unpriced — settlement then cannot price it and fails closed
    /// under a hard cap); the settlement layer uses
    /// [`Store::cost_reserve_priced`], which additionally persists the
    /// route-time [`PricingSnapshot`] the call will be settled against.
    pub fn cost_reserve(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        op_id: OpId,
        predicted_micro: u64,
        created_ms: i64,
    ) -> StoreResult<CostReserveOutcome> {
        self.cost_reserve_inner(
            session_id,
            task_id,
            op_id,
            predicted_micro,
            created_ms,
            None,
            // No pricing authority consulted here: no execution-class
            // authority either (never inferred from numbers).
            ExecutionClass::Unknown,
        )
    }

    /// [`Store::cost_reserve`] plus the immutable route-time price capture
    /// (P0-1): the snapshot is persisted on the reservation so settlement —
    /// including settlement that happens after a daemon restart — prices the
    /// call's usage against exactly the prices the router froze at route
    /// time. `execution_class` is the route-time execution classification
    /// (schema v34) frozen beside the snapshot; `Local` only for an
    /// authoritative local-zero route, `Unknown` when no authority
    /// established one. Bounded by the session layer before this call.
    #[allow(clippy::too_many_arguments)]
    pub fn cost_reserve_priced(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        op_id: OpId,
        predicted_micro: u64,
        created_ms: i64,
        pricing_snapshot_json: Option<&str>,
        execution_class: ExecutionClass,
    ) -> StoreResult<CostReserveOutcome> {
        self.cost_reserve_inner(
            session_id,
            task_id,
            op_id,
            predicted_micro,
            created_ms,
            pricing_snapshot_json,
            execution_class,
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cost_reserve_inner(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        op_id: OpId,
        predicted_micro: u64,
        created_ms: i64,
        pricing_snapshot_json: Option<&str>,
        execution_class: ExecutionClass,
    ) -> StoreResult<CostReserveOutcome> {
        let pricing_snapshot_json = pricing_snapshot_json.map(|v| v.to_owned());
        // The durable tag: Unknown (and legacy NULL) = no class authority.
        let execution_class_db = execution_class.is_known().then(|| execution_class.as_str());
        // Preparation BEFORE enqueueing: the raw state gate terms and the
        // no-row conflict text (audit finding 5 — the closure formats
        // nothing and never decodes `TaskState`).
        let (permitted_states, forbidden_states, corrupt_state) = provider_operation_gate_terms();
        let no_row = format!("cost reserve: task {task_id} of session {session_id} has no row");
        self.writer
            .execute_retry_busy("cost_reserve_inner", move |conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row_opt: Option<(Option<i64>, String)> = tx
            .query_row(
                "SELECT max_cost_micro, state FROM task WHERE session_id = ?1 AND task_id = ?2",
                params![session_id.raw() as i64, task_id.raw() as i64],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        // Outer None = no task row; inner None = the row's cap is NULL =
        // unlimited (distinct states: an unlimited cap is a valid cap).
        let Some((cap, state_json)) = row_opt else {
            tx.rollback()?;
            return Err(StoreError::Conflict(no_row.clone()));
        };
        // The task-state gate: a reserve while the task is in a
        // completion/final state refuses typed and writes NOTHING (the same
        // predicate the completion transaction's zero-reservation gate
        // closes from the other side). The raw state text is matched against
        // the caller-prepared vocabulary.
        if !permitted_states.iter().any(|text| text == &state_json) {
            tx.rollback()?;
            return match forbidden_states
                .iter()
                .find(|(text, _)| text == &state_json)
            {
                Some((_, message)) => Err(StoreError::Conflict(message.clone())),
                None => Err(StoreError::Corrupt(vec![corrupt_state.clone()])),
            };
        }
        let spent: i64 = tx.query_row(
            "SELECT spent_cost_micro FROM task WHERE session_id = ?1 AND task_id = ?2",
            params![session_id.raw() as i64, task_id.raw() as i64],
            |r| r.get(0),
        )?;
        // Free balance subtracts BOTH the settled spend and the predictions
        // of every row that still holds budget (OPEN + UNCERTAIN — a
        // reservation a crashed daemon may have dispatched keeps consuming
        // the reserved amount until reconcile/finalize closes it):
        // ceiling = spent + in-flight + free; two concurrent reservations can
        // never jointly overshoot the cap.
        //
        // The open sum is computed with `total()` (floating point, which
        // never raises SQLite's integer-overflow error) and clamped back
        // into the signed domain BEFORE the cast: individually valid
        // predictions near i64::MAX must not poison every later reserve on
        // the task with a raw "integer overflow" SQL failure — the free
        // balance simply saturates to 0 instead.
        let open_sum: i64 = tx.query_row(
            "SELECT CAST(MIN(total(CASE WHEN predicted_micro > 0 THEN predicted_micro ELSE 0 END),
                              9223372036854774784.0) AS INTEGER)
             FROM cost_reservation
             WHERE session_id = ?1 AND task_id = ?2 AND status IN ('reserved', 'dispatched', 'uncertain')",
            params![session_id.raw() as i64, task_id.raw() as i64],
            |r| r.get(0),
        )?;
        // NULL cap = unlimited: `cap.unwrap_or(0)` keeps the free balance at
        // 0-minus-nothing (i.e. everything is free) and the `cap > 0` guard
        // below skips the refusal.
        let cap_limit = cap.unwrap_or(0);
        // saturating_sub clamps at i64::MIN, not at 0: clamp to zero here —
        // a task can never have negative free balance.
        let free = cap_limit
            .max(0)
            .saturating_sub(spent.max(0))
            .saturating_sub(open_sum.max(0))
            .max(0);
        if cap_limit > 0 && i64::try_from(predicted_micro).unwrap_or(i64::MAX) > free {
            tx.rollback()?;
            return Ok(CostReserveOutcome::Exceeded { free: free as u64 });
        }
        let id = tx.query_row(
            "INSERT INTO cost_reservation
                (session_id, task_id, op_id, predicted_micro, status, created_ms,
                 pricing_snapshot_json, estimated_cost_micro, execution_class)
             VALUES (?1, ?2, ?3, ?4, 'reserved', ?5, ?6, ?4, ?7)
             RETURNING reservation_id",
            params![
                session_id.raw() as i64,
                task_id.raw() as i64,
                op_id.raw() as i64,
                predicted_micro.min(i64::MAX as u64) as i64,
                created_ms,
                pricing_snapshot_json,
                execution_class_db
            ],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(CostReserveOutcome::Granted(id))
        })
    }

    /// [`Store::cost_reserve_priced`] for one PHYSICAL ATTEMPT (attempt
    /// accounting, schema v18): the reservation keys by the attempt's fresh
    /// op id (`attempt_op_id`, also stored in `op_id` — the row's own op),
    /// carries the shared logical parent op id (`parent_op_id`, what legacy
    /// rows stored in `op_id`) and durably freezes the reserve-time estimate
    /// in `estimated_cost_micro`. Two attempts of the SAME logical op get
    /// two distinct rows with distinct `attempt_op_id`s; legacy `op_id`
    /// joins between reservations and provider-call rows never apply to
    /// attempt rows (reconciliation joins `attempt_op_id` instead).
    #[allow(clippy::too_many_arguments)]
    pub fn cost_reserve_attempt(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt: &faktor_core::op::ModelCallAttempt,
        predicted_micro: u64,
        created_ms: i64,
        pricing_snapshot_json: Option<&str>,
        execution_class: ExecutionClass,
    ) -> StoreResult<CostReserveOutcome> {
        let attempt = attempt.to_owned();
        let pricing_snapshot_json = pricing_snapshot_json.map(|v| v.to_owned());
        // The durable tag: Unknown (and legacy NULL) = no class authority.
        let execution_class_db = execution_class.is_known().then(|| execution_class.as_str());
        // Preparation BEFORE enqueueing: the raw state gate terms and the
        // no-row conflict text (audit finding 5 — the closure formats
        // nothing and never decodes `TaskState`).
        let (permitted_states, forbidden_states, corrupt_state) = provider_operation_gate_terms();
        let no_row = format!("cost reserve: task {task_id} of session {session_id} has no row");
        self.writer
            .execute_retry_busy("cost_reserve_attempt", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let row_opt: Option<(Option<i64>, String)> = tx
                .query_row(
                    "SELECT max_cost_micro, state FROM task WHERE session_id = ?1 AND task_id = ?2",
                    params![session_id.raw() as i64, task_id.raw() as i64],
                    |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
                let Some((cap, state_json)) = row_opt else {
                    tx.rollback()?;
                    return Err(StoreError::Conflict(no_row.clone()));
                };
                // The task-state gate: the raw state text is matched against the
                // caller-prepared vocabulary (no decode on the writer owner).
                if !permitted_states.iter().any(|text| text == &state_json) {
                    tx.rollback()?;
                    return match forbidden_states
                        .iter()
                        .find(|(text, _)| text == &state_json)
                    {
                        Some((_, message)) => Err(StoreError::Conflict(message.clone())),
                        None => Err(StoreError::Corrupt(vec![corrupt_state.clone()])),
                    };
                }
                let spent: i64 = tx.query_row(
                    "SELECT spent_cost_micro FROM task WHERE session_id = ?1 AND task_id = ?2",
                    params![session_id.raw() as i64, task_id.raw() as i64],
                    |r| r.get(0),
                )?;
                let open_sum: i64 = tx.query_row(
                    "SELECT COALESCE(SUM(predicted_micro), 0) FROM cost_reservation
             WHERE session_id = ?1 AND task_id = ?2
               AND status IN ('reserved', 'dispatched', 'uncertain')",
                    params![session_id.raw() as i64, task_id.raw() as i64],
                    |r| r.get(0),
                )?;
                let cap_limit = cap.unwrap_or(0);
                let free = cap_limit
                    .max(0)
                    .saturating_sub(spent.max(0))
                    .saturating_sub(open_sum.max(0))
                    .max(0);
                if cap_limit > 0 && i64::try_from(predicted_micro).unwrap_or(i64::MAX) > free {
                    tx.rollback()?;
                    return Ok(CostReserveOutcome::Exceeded { free: free as u64 });
                }
                let predicted = predicted_micro.min(i64::MAX as u64) as i64;
                let id = tx.query_row(
                    "INSERT INTO cost_reservation
                (session_id, task_id, op_id, attempt_op_id, parent_op_id,
                 predicted_micro, status, created_ms, pricing_snapshot_json,
                 estimated_cost_micro, execution_class)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, 'reserved', ?6, ?7, ?5, ?8)
             RETURNING reservation_id",
                    params![
                        session_id.raw() as i64,
                        task_id.raw() as i64,
                        attempt.attempt_op_id.raw() as i64,
                        attempt.logical_op_id.raw() as i64,
                        predicted,
                        created_ms,
                        pricing_snapshot_json,
                        execution_class_db
                    ],
                    |r| r.get(0),
                )?;
                tx.commit()?;
                Ok(CostReserveOutcome::Granted(id))
            })
    }

    /// The current state of one reservation row (attempt-accounting
    /// observability): `(session, status, dispatched_ms)`. `None` = no row.
    pub fn cost_reservation_state(
        &self,
        reservation_id: i64,
    ) -> StoreResult<Option<(SessionId, String, Option<i64>)>> {
        let conn = self.read()?;
        let out = query_row_optional(
            &conn,
            "SELECT session_id, status, dispatched_ms FROM cost_reservation
             WHERE reservation_id = ?1",
            params![reservation_id],
            |r| {
                Ok((
                    id_field(
                        &format!("cost_reservation {reservation_id} session_id"),
                        r.get::<_, i64>(0)?,
                    )?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )?;
        Ok(out)
    }

    /// Settle one RESERVED/DISPATCHED reservation and fold `actual_micro`
    /// into the task's spent total in ONE transaction (schema v18). The
    /// provider-reported cost, the locally calculated cost and the routing
    /// decision's JSON are recorded on the row (all bounded by the session
    /// layer before this call); the canonical v18 columns are written too:
    /// `settled_cost_micro` = the folded actual, `provider_reported_cost_micro`
    /// = the provider report, `cost_basis` = the honest authority behind the
    /// folded amount (ProviderReported when a provider report was given —
    /// the caller's documented numeric settle IS that report's authority —
    /// else RouteSnapshotEstimate for a pre-computed estimate-charge site).
    /// An overshooting actual is recorded honestly (the money was spent);
    /// the NEXT reservation is what refuses.
    ///
    /// LEGACY numeric settle: the caller supplies the actual directly (no
    /// price math). The settlement layer's truthful usage settlement is
    /// [`Store::cost_settle_usage`]; this entry point remains for callers
    /// that hold a pre-computed actual (documented estimate-charge sites and
    /// direct-store tests) and for the pre-P0-1 wire path.
    #[allow(clippy::too_many_arguments)]
    pub fn cost_settle(
        &self,
        reservation_id: i64,
        actual_micro: u64,
        provider_cost_micro: Option<u64>,
        provider_reported_micro: Option<u64>,
        route_decision_json: Option<&str>,
        settled_ms: i64,
    ) -> StoreResult<CostReservationState> {
        let route_decision_json = route_decision_json.map(|v| v.to_owned());
        let prepared_attempt = {
            let conn = self.read()?;
            conn.query_row(
                "SELECT op_id, attempt_op_id FROM cost_reservation WHERE reservation_id = ?1",
                params![reservation_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?
            .map(|(op_id, attempt_op_id)| attempt_op_id.unwrap_or(op_id).to_string())
            .unwrap_or_default()
        };
        let source_operation = "cost_settle".to_owned();
        // Preparation BEFORE enqueueing: the refusal diagnostic (the task ids
        // are read inside the transaction, so the message is static).
        let task_missing =
            "cost settle: task has no row; the task machine owns creation".to_owned();
        self.writer.execute_retry_busy("cost_settle", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<(i64, i64, String)> = tx
                .query_row(
                    "SELECT session_id, task_id, status
                 FROM cost_reservation WHERE reservation_id = ?1",
                    params![reservation_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((session_id, task_id, status)) = row else {
                tx.rollback()?;
                return Ok(CostReservationState::Missing);
            };
            if status != "reserved" && status != "dispatched" {
                tx.rollback()?;
                return Ok(CostReservationState::NotOpen { current: status });
            }
            let actual = actual_micro.min(i64::MAX as u64) as i64;
            let basis = if provider_reported_micro.is_some() {
                COST_BASIS_PROVIDER_REPORTED
            } else {
                COST_BASIS_ROUTE_SNAPSHOT_ESTIMATE
            };
            tx.execute(
                "UPDATE cost_reservation
             SET status = 'settled', settled_ms = ?1, delivery_state = ?6,
                 provider_cost_micro = ?2, provider_reported_micro = ?3,
                 provider_reported_cost_micro = ?3, settled_cost_micro = ?7,
                 cost_basis = ?8,
                 route_decision_json = ?4
             WHERE reservation_id = ?5",
                params![
                    settled_ms,
                    provider_cost_micro.map(|m| m.min(i64::MAX as u64) as i64),
                    provider_reported_micro.map(|m| m.min(i64::MAX as u64) as i64),
                    route_decision_json,
                    reservation_id,
                    DELIVERY_COMPLETED,
                    actual,
                    basis
                ],
            )?;
            let n = tx.execute(
                "UPDATE task SET spent_cost_micro = spent_cost_micro + ?1
             WHERE session_id = ?2 AND task_id = ?3",
                params![actual, session_id, task_id],
            )?;
            if n == 0 {
                tx.rollback()?;
                return Err(StoreError::Conflict(task_missing.clone()));
            }
            let fact = BillingOutboxFact {
                reservation_id,
                session_id: id_field("cost_settle session_id", session_id)?,
                task_id: id_field("cost_settle task_id", task_id)?,
                attempt_id: prepared_attempt.clone(),
                provider: String::new(),
                model: String::new(),
                uncached_input_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                provider_cost_micro: actual.max(0) as u64,
                provider_reported_micro,
                occurred_at_ms: settled_ms,
                source_operation: source_operation.clone(),
            };
            insert_billing_outbox_tx(&tx, &fact)?;
            tx.commit()?;
            Ok(CostReservationState::Applied)
        })
    }

    /// Settle one RESERVED/DISPATCHED reservation from a provider usage
    /// frame (P0-1 settlement truth, schema v18) in ONE transaction. The
    /// chosen actual: the HIGHER of the provider-reported cost and the token
    /// categories x the reservation's stored route-time [`PricingSnapshot`]
    /// — exactly the price lines the router froze, never a fabricated
    /// per-token number. A provider report may raise an invoice, but it can
    /// never undercut the locally priced usage of its own reported token
    /// categories (a spoofed 0/under-report cannot make a paid call free and
    /// bypass the task cap).
    /// An Unknown price source (or no snapshot at all) with a hard task cap
    /// is a typed [`CostSettleOutcome::UnknownPrice`] refusal (nothing
    /// written); with no cap the reservation closes as an honest Unknown
    /// spend (amount columns NULL, nothing folded into the task total).
    ///
    /// The locally calculated amount and the provider-reported amount are
    /// BOTH recorded on the row when present; the canonical v18 columns are
    /// written too — `settled_cost_micro` = the amount actually folded,
    /// `provider_reported_cost_micro` = the provider report,
    /// `cost_basis` = ProviderReported when the reported amount won, else
    /// RouteSnapshotEstimate when the categories x snapshot actual won.
    /// An overshooting chosen actual is recorded honestly; the NEXT
    /// reservation is what refuses.
    #[allow(clippy::too_many_arguments)]
    pub fn cost_settle_usage(
        &self,
        reservation_id: i64,
        uncached_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        provider_reported_micro: Option<u64>,
        route_decision_json: Option<&str>,
        settled_ms: i64,
    ) -> StoreResult<CostSettleOutcome> {
        self.cost_settle_usage_with_context(
            reservation_id,
            uncached_input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            provider_reported_micro,
            route_decision_json,
            settled_ms,
            "",
            "",
        )
    }

    /// [`Store::cost_settle_usage`] with the attempt's provider/model, which
    /// the outbox row carries so the commercial projection can classify the
    /// spend without re-listing the session's calls.
    #[allow(clippy::too_many_arguments)]
    pub fn cost_settle_usage_with_context(
        &self,
        reservation_id: i64,
        uncached_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        provider_reported_micro: Option<u64>,
        route_decision_json: Option<&str>,
        settled_ms: i64,
        provider: &str,
        model: &str,
    ) -> StoreResult<CostSettleOutcome> {
        #[allow(clippy::type_complexity)]
        type CostReservationRow = (i64, i64, String, Option<String>);
        let route_decision_json = route_decision_json.map(|v| v.to_owned());
        let provider = provider.to_string();
        let model = model.to_string();
        let prepared_attempt = {
            let conn = self.read()?;
            conn.query_row(
                "SELECT op_id, attempt_op_id FROM cost_reservation WHERE reservation_id = ?1",
                params![reservation_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?
            .map(|(op_id, attempt_op_id)| attempt_op_id.unwrap_or(op_id).to_string())
            .unwrap_or_default()
        };
        let source_operation = "cost_settle_usage".to_owned();
        // Preparation BEFORE enqueueing: every refusal diagnostic (the task
        // ids are read inside the transaction, so that message is static).
        let snapshot_context = format!("reservation {reservation_id} pricing_snapshot_json");
        let task_missing =
            "cost settle: task has no row; the task machine owns creation".to_owned();
        self.writer
            .execute_retry_busy("cost_settle_usage", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let row: Option<CostReservationRow> = tx
                    .query_row(
                        "SELECT session_id, task_id, status, pricing_snapshot_json
                     FROM cost_reservation WHERE reservation_id = ?1",
                        params![reservation_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                let Some((session_id, task_id, status, snapshot_json)) = row else {
                    tx.rollback()?;
                    return Ok(CostSettleOutcome::Missing);
                };
                let outbox_fact = |cost_micro: u64| BillingOutboxFact {
                    reservation_id,
                    session_id: SessionId::new(session_id as u64),
                    task_id: TaskId::new(task_id as u64),
                    attempt_id: prepared_attempt.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                    uncached_input_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    output_tokens,
                    reasoning_tokens: 0,
                    provider_cost_micro: cost_micro,
                    provider_reported_micro,
                    occurred_at_ms: settled_ms,
                    source_operation: source_operation.clone(),
                };
                if status != "reserved" && status != "dispatched" {
                    tx.rollback()?;
                    return Ok(CostSettleOutcome::NotOpen { current: status });
                }
                let snapshot = match snapshot_json {
                    Some(json) => Some(parse_json::<PricingSnapshot>(&snapshot_context, &json)?),
                    None => None,
                };
                // The locally calculated actual: categories x the frozen snapshot.
                // Unknown/absent snapshot => no honest number exists.
                let local = snapshot.and_then(|s| {
                    s.settle_cost(
                        uncached_input_tokens,
                        cache_read_tokens,
                        cache_write_tokens,
                        output_tokens,
                    )
                });
                let (local_i64, reported_i64) = (
                    local.map(|m| m.min(i64::MAX as u64) as i64),
                    provider_reported_micro.map(|m| m.min(i64::MAX as u64) as i64),
                );
                // The chosen actual: the HIGHER of the provider-reported cost and
                // the locally calculated categories x snapshot. A provider report
                // may raise an invoice above the local price lines, but it can
                // never LOWER the amount below what its own reported token
                // categories price at the frozen snapshot — a hostile/buggy
                // provider reporting cost 0 (or any under-report) must not make a
                // paid call free and bypass the task budget. Unknown price + no
                // reported cost: with a hard cap this is a typed refusal (the cap
                // was protected by the reservation's prediction, but a fabricated
                // actual must never land); with no cap the row closes as a
                // documented Unknown spend.
                let chosen_i64: i64 = match (reported_i64, local_i64) {
                    (Some(reported), Some(local)) => reported.max(local),
                    (Some(reported), None) => reported,
                    (None, Some(local)) => local,
                    (None, None) => {
                        let has_cap: bool = tx
                            .query_row(
                                "SELECT max_cost_micro > 0 FROM task
                             WHERE session_id = ?1 AND task_id = ?2",
                                params![session_id, task_id],
                                |r| r.get(0),
                            )
                            .unwrap_or(false);
                        if has_cap {
                            tx.rollback()?;
                            return Ok(CostSettleOutcome::UnknownPrice {
                                reservation: reservation_id,
                            });
                        }
                        // No cap: record the Unknown spend honestly — status
                        // settled, both amount columns NULL, nothing folded,
                        // cost_basis Unknown.
                        tx.execute(
                            "UPDATE cost_reservation
                         SET status = 'settled', settled_ms = ?1, delivery_state = ?3,
                             provider_cost_micro = NULL, provider_reported_micro = NULL,
                             provider_reported_cost_micro = NULL, settled_cost_micro = NULL,
                             cost_basis = ?4,
                             route_decision_json = ?2
                         WHERE reservation_id = ?5",
                            params![
                                settled_ms,
                                route_decision_json,
                                DELIVERY_COMPLETED,
                                COST_BASIS_UNKNOWN,
                                reservation_id
                            ],
                        )?;
                        insert_billing_outbox_tx(&tx, &outbox_fact(0))?;
                        tx.commit()?;
                        return Ok(CostSettleOutcome::AppliedUnknown);
                    }
                };
                // The basis names the authority behind the FOLDED amount: a
                // provider report that won, otherwise the local price lines.
                let basis = if reported_i64 == Some(chosen_i64) {
                    COST_BASIS_PROVIDER_REPORTED
                } else {
                    COST_BASIS_ROUTE_SNAPSHOT_ESTIMATE
                };
                tx.execute(
                    "UPDATE cost_reservation
             SET status = 'settled', settled_ms = ?1, delivery_state = ?6,
                 provider_cost_micro = ?2, provider_reported_micro = ?3,
                 provider_reported_cost_micro = ?3, settled_cost_micro = ?7,
                 cost_basis = ?8,
                 route_decision_json = ?4
             WHERE reservation_id = ?5",
                    params![
                        settled_ms,
                        local_i64,
                        reported_i64,
                        route_decision_json,
                        reservation_id,
                        DELIVERY_COMPLETED,
                        chosen_i64,
                        basis
                    ],
                )?;
                let n = tx.execute(
                    "UPDATE task SET spent_cost_micro = spent_cost_micro + ?1
             WHERE session_id = ?2 AND task_id = ?3",
                    params![chosen_i64, session_id, task_id],
                )?;
                if n == 0 {
                    tx.rollback()?;
                    return Err(StoreError::Conflict(task_missing.clone()));
                }
                insert_billing_outbox_tx(&tx, &outbox_fact(chosen_i64.max(0) as u64))?;
                tx.commit()?;
                Ok(CostSettleOutcome::Applied {
                    actual_micro: chosen_i64.max(0) as u64,
                })
            })
    }

    /// Mark one RESERVED reservation as DISPATCHED (P0-2 + attempt-identity
    /// accounting): the status moves `reserved` -> `dispatched` AND the
    /// durable `dispatched_ms` marker + `delivery_state` are written in the
    /// SAME statement, immediately BEFORE the provider transport call. Crash
    /// recovery can then tell "dispatch never provably began" (still
    /// `reserved`, marker NULL -> refund) from "the provider may have
    /// billed" (`dispatched`, marker set -> UNCERTAIN), and the SQL-level
    /// refund guard (refund only touches `reserved` + marker NULL) can never
    /// free a dispatched reservation. A second mark of the same
    /// still-dispatchable reservation is idempotent (a retry re-marking its
    /// own row); anything settled/refunded/uncertain is a typed refusal.
    pub fn cost_mark_dispatched(
        &self,
        reservation_id: i64,
        dispatched_ms: i64,
    ) -> StoreResult<CostReservationState> {
        self.writer.execute("cost_mark_dispatched", move |conn| {
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM cost_reservation WHERE reservation_id = ?1",
                    params![reservation_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(status) = status else {
                return Ok(CostReservationState::Missing);
            };
            if status != "reserved" && status != "dispatched" {
                return Ok(CostReservationState::NotOpen { current: status });
            }
            conn.execute(
                "UPDATE cost_reservation
             SET status = 'dispatched', dispatched_ms = ?1, delivery_state = ?2
             WHERE reservation_id = ?3 AND status IN ('reserved', 'dispatched')",
                params![dispatched_ms, DELIVERY_DISPATCHED, reservation_id],
            )?;
            Ok(CostReservationState::Applied)
        })
    }

    /// Refund one pre-dispatch reservation (`reserved`/legacy `open` with a
    /// NULL dispatch marker -> REFUNDED): the prediction is released and the
    /// spent total is untouched. The guard lives in the SQL itself —
    /// `WHERE ... AND status IN ('reserved','open') AND dispatched_ms IS
    /// NULL` — so a refund that reaches a dispatched, settled, refunded or
    /// uncertain reservation changes ZERO rows and is refused here with the
    /// row's current status and dispatch marker ([`RefundOutcome::Blocked`]):
    /// money is freed only by the guarded UPDATE, never by a caller's
    /// pre-check. This makes the pre-dispatch-only refund enforceable even
    /// against a caller that mis-calls refund after dispatch (the agent
    /// runtime's current post-dispatch refund sites): the money stays put.
    pub fn cost_refund(&self, reservation_id: i64, settled_ms: i64) -> StoreResult<RefundOutcome> {
        self.writer.execute("cost_refund", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<(String, Option<i64>)> = tx
                .query_row(
                    "SELECT status, dispatched_ms FROM cost_reservation WHERE reservation_id = ?1",
                    params![reservation_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((status, dispatched_ms)) = row else {
                tx.rollback()?;
                return Ok(RefundOutcome::Missing);
            };
            let n = tx.execute(
                "UPDATE cost_reservation SET status = 'refunded', settled_ms = ?2
             WHERE reservation_id = ?1
               AND status IN ('reserved', 'open') AND dispatched_ms IS NULL",
                params![reservation_id, settled_ms],
            )?;
            if n == 0 {
                tx.rollback()?;
                // The guarded UPDATE changed nothing: the row is no longer
                // refundable pre-dispatch (dispatched / settled / refunded /
                // uncertain, or a legacy reserved+marker row). Typed, nothing
                // written, money untouched.
                return Ok(RefundOutcome::Blocked {
                    current: status,
                    dispatched_ms,
                });
            }
            tx.commit()?;
            Ok(RefundOutcome::Applied)
        })
    }

    /// Crash recovery (P0-6/12): every pre-dispatch reservation of a crashed
    /// process becomes REFUNDED and is NEVER counted as spent — the op never
    /// settled, so its prediction was never spent (ops that DID run carry
    /// settled rows). Idempotent: a second recovery finds nothing reserved.
    ///
    /// LEGACY entry point retained for direct-store callers that predate the
    /// P0-2 marker semantics AND the v17 state vocabulary: the legacy
    /// `abandoned` state no longer exists (the v17 CHECK forbids it — an
    /// abandoned reservation charged $0, the exact hole the P0-2 marker
    /// semantics close), so the honest replacement for "close every
    /// still-open row of a crashed process" IS the marker split:
    /// pre-dispatch rows refund, post-marker rows go UNCERTAIN. This entry
    /// point delegates to that split and reports the number of rows closed.
    /// The settlement layer's recovery is
    /// [`Store::cost_recover_open_reservations`].
    pub fn cost_abandon_open_reservations(&self, at_ms: i64) -> StoreResult<u64> {
        let (refunded, uncertain) = self.cost_recover_open_reservations(at_ms)?;
        Ok(refunded + uncertain)
    }

    /// P0-2 crash recovery of every pre-dispatch reservation of a crashed
    /// process, split on the durable dispatch marker in ONE transaction
    /// (schema v18: `reserved`/`dispatched` are the crash-relevant states —
    /// a legacy v16 row can still read `reserved` + marker after migration).
    /// Idempotent (a second recovery finds nothing open). Returns
    /// `(refunded, uncertain)`:
    ///
    /// - `reserved` with `dispatched_ms` NULL — dispatch never provably
    ///   began, so the provider was never contacted: REFUNDED, free budget
    ///   restored;
    /// - `dispatched` (or a legacy `reserved`/`open` row with `dispatched_ms`
    ///   set) — the provider request was sent and may have been billed:
    ///   UNCERTAIN, the reserved amount keeps consuming the task's free
    ///   budget until a reconcile settles it from the attempt's durable
    ///   provider-call rows or the task-completion finalize charges the
    ///   estimate. `failure_reason_code` records the crash for forensics.
    pub fn cost_recover_open_reservations(&self, at_ms: i64) -> StoreResult<(u64, u64)> {
        self.writer
            .execute("cost_recover_open_reservations", move |conn| {
                let refunded = conn.execute(
                    "UPDATE cost_reservation SET status = 'refunded', settled_ms = ?1
             WHERE status IN ('reserved', 'open') AND dispatched_ms IS NULL",
                    params![at_ms],
                )?;
                let uncertain = conn.execute(
                    "UPDATE cost_reservation
             SET status = 'uncertain', settled_ms = ?1, failure_reason_code = ?2
             WHERE status = 'dispatched'
                OR (status IN ('reserved', 'open') AND dispatched_ms IS NOT NULL)",
                    params![at_ms, "crash_recovery_post_dispatch_marker"],
                )?;
                Ok((refunded as u64, uncertain as u64))
            })
    }

    /// P0-2 reconcile: every UNCERTAIN reservation of one task settles FROM
    /// the durable `provider_call` row that belongs to the SAME ATTEMPT
    /// (attempt accounting, schema v18): an attempt-keyed reservation joins
    /// `provider_call.attempt_op_id = cost_reservation.attempt_op_id`; a
    /// legacy reservation (attempt_op_id NULL) joins its op id as before —
    /// the completed call's recorded tokens, priced at the reservation's
    /// stored route-time snapshot (a crash-resumed op that completes is the
    /// durable settlement basis for the crashed attempt of the same op: the
    /// provider billed each dispatched attempt). When two attempts of one
    /// logical op exist, the attempt join guarantees each reservation
    /// settles from ITS OWN attempt's row — never from the sibling's. Every
    /// candidate keeps its own SAVEPOINT rollback scope (a row that must
    /// write nothing — Skipped / LeftUncertain / task row gone — is undone
    /// row-locally), while the WHOLE candidate set commits in ONE writer
    /// job: the single writer owner serialized the former per-candidate
    /// jobs anyway, so per-row commit independence bought only one
    /// transaction frame + one channel round-trip PER ROW. Rows without a
    /// completed provider-call row of their own attempt — or unpriced under
    /// a hard cap — stay UNCERTAIN (the task-completion finalize is their
    /// conservative backstop). Idempotent: a second pass finds the settled
    /// rows closed; a pass that fails at the SQL layer writes NOTHING and
    /// the next pass re-runs the still-uncertain rows.
    pub fn cost_reconcile_uncertain(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        at_ms: i64,
    ) -> StoreResult<CostReconcileReport> {
        let mut report = CostReconcileReport::default();
        let candidates: Vec<(i64, String)> = {
            let conn = self.read()?;
            let mut stmt = conn.prepare(
                "SELECT cr.reservation_id, cr.op_id, cr.attempt_op_id
                 FROM cost_reservation cr
                 WHERE cr.session_id = ?1 AND cr.task_id = ?2 AND cr.status = 'uncertain'
                   AND EXISTS (
                     SELECT 1 FROM provider_call p
                     WHERE p.session_id = cr.session_id
                       AND p.status = 'completed'
                       AND (
                         (cr.attempt_op_id IS NOT NULL
                          AND p.attempt_op_id = cr.attempt_op_id)
                         OR
                         (cr.attempt_op_id IS NULL AND p.attempt_op_id IS NULL
                          AND p.op_id = cr.op_id))
                 )
                 ORDER BY cr.reservation_id ASC",
            )?;
            let rows = stmt.query_map(
                params![session_id.raw() as i64, task_id.raw() as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (reservation_id, op_id, attempt_op_id) = row?;
                out.push((reservation_id, attempt_op_id.unwrap_or(op_id).to_string()));
            }
            out
        };
        // ONE writer job for the WHOLE candidate set: every candidate runs
        // inside its own SAVEPOINT and the pass commits ONCE (see the
        // function docs). The report still folds OUTSIDE the job. The
        // per-candidate snapshot context is materialized HERE (the caller's
        // thread), never inside the single writer owner.
        let source_operation = "cost_reconcile_uncertain".to_owned();
        let candidates: Vec<(i64, String, String)> = candidates
            .into_iter()
            .map(|(id, attempt_id)| {
                (
                    id,
                    format!("reservation {id} pricing_snapshot_json"),
                    attempt_id,
                )
            })
            .collect();
        let candidate_count = candidates.len();
        let outcomes: Vec<CostReconcileOutcome> = if candidate_count == 0 {
            Vec::new()
        } else {
            self.writer.execute("cost_reconcile_uncertain", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut outcomes = Vec::with_capacity(candidate_count);
                for (reservation_id, snapshot_context, attempt_id) in candidates {
                    // ONE fixed savepoint name: each candidate's savepoint is
                    // released before the next begins, and the constant text
                    // lets the connection's statement cache reuse the
                    // prepared statement (zero per-candidate allocation).
                    tx.execute_batch("SAVEPOINT cost_reconcile_candidate")?;
                    let outcome = (|| -> StoreResult<CostReconcileOutcome> {
                // Only a row still UNCERTAIN settles (concurrent recovery or
                // a previous pass may have closed it).
                let row: Option<(u64, Option<String>)> = tx
                    .query_row(
                        "SELECT predicted_micro, pricing_snapshot_json
                         FROM cost_reservation
                         WHERE reservation_id = ?1 AND status = 'uncertain'",
                        params![reservation_id],
                        |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?)),
                    )
                    .optional()?;
                let Some((_predicted, snapshot_json)) = row else {
                    return Ok(CostReconcileOutcome::Skipped);
                };
                let snapshot = match snapshot_json {
                    Some(json) => Some(parse_json::<PricingSnapshot>(&snapshot_context, &json)?),
                    None => None,
                };
                // The completed provider-call row of THIS SAME attempt (or, for
                // a legacy reservation, of its op) — never a sibling attempt's
                // row. Audit-13 primary counters; cache/reasoning detail is not
                // persisted on the provider-call row, so the settled basis is
                // input + output.
                let tokens: Option<(i64, i64, String, String)> = tx
                    .query_row(
                        "SELECT COALESCE(p.tokens_in, 0), COALESCE(p.tokens_out, 0),
                                p.provider, p.model
                         FROM provider_call p
                         JOIN cost_reservation cr ON cr.reservation_id = ?1
                           AND p.session_id = cr.session_id
                           AND p.status = 'completed'
                           AND (
                             (cr.attempt_op_id IS NOT NULL
                              AND p.attempt_op_id = cr.attempt_op_id)
                             OR
                             (cr.attempt_op_id IS NULL AND p.attempt_op_id IS NULL
                              AND p.op_id = cr.op_id))
                         ORDER BY p.id DESC LIMIT 1",
                        params![reservation_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                let Some((tokens_in, tokens_out, call_provider, call_model)) = tokens else {
                    return Ok(CostReconcileOutcome::LeftUncertain);
                };
                let outbox_fact = |cost_micro: u64| BillingOutboxFact {
                    reservation_id,
                    session_id,
                    task_id,
                    attempt_id: attempt_id.clone(),
                    provider: call_provider.clone(),
                    model: call_model.clone(),
                    uncached_input_tokens: tokens_in.max(0) as u64,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: tokens_out.max(0) as u64,
                    reasoning_tokens: 0,
                    provider_cost_micro: cost_micro,
                    provider_reported_micro: None,
                    occurred_at_ms: at_ms,
                    source_operation: source_operation.clone(),
                };
                let actual = snapshot.and_then(|s| {
                    s.settle_cost(tokens_in.max(0) as u64, 0, 0, tokens_out.max(0) as u64)
                });
                let outcome = match actual {
                    Some(cost) => {
                        let cost = cost.min(i64::MAX as u64) as i64;
                        tx.execute(
                            "UPDATE cost_reservation
                             SET status = 'settled', settled_ms = ?1, provider_cost_micro = ?2,
                                 provider_reported_micro = NULL,
                                 provider_reported_cost_micro = NULL,
                                 settled_cost_micro = ?2,
                                 estimated_cost_micro = COALESCE(estimated_cost_micro, predicted_micro),
                                 cost_basis = ?3, delivery_state = NULL
                             WHERE reservation_id = ?4",
                            params![at_ms, cost, COST_BASIS_ROUTE_SNAPSHOT_ESTIMATE, reservation_id],
                        )?;
                        let n = tx.execute(
                            "UPDATE task SET spent_cost_micro = spent_cost_micro + ?1
                             WHERE session_id = ?2 AND task_id = ?3",
                            params![cost, session_id.raw() as i64, task_id.raw() as i64],
                        )?;
                        if n == 0 {
                            // The task row is gone: nothing can fold into it —
                            // the savepoint undo keeps it for doctor's
                            // dangling scan.
                            return Ok(CostReconcileOutcome::LeftUncertain);
                        }
                        insert_billing_outbox_tx(&tx, &outbox_fact(cost.max(0) as u64))?;
                        CostReconcileOutcome::Settled(cost.max(0) as u64)
                    }
                    None => {
                        let has_cap: bool = tx
                            .query_row(
                                "SELECT max_cost_micro > 0 FROM task
                                 WHERE session_id = ?1 AND task_id = ?2",
                                params![session_id.raw() as i64, task_id.raw() as i64],
                                |r| r.get(0),
                            )
                            .unwrap_or(false);
                        if has_cap {
                            // Unpriced under a hard cap: cannot settle honestly —
                            // the finalize (reserved estimate) is the backstop.
                            CostReconcileOutcome::LeftUncertain
                        } else {
                            tx.execute(
                                "UPDATE cost_reservation
                                 SET status = 'settled', settled_ms = ?1,
                                     provider_cost_micro = NULL, provider_reported_micro = NULL,
                                     provider_reported_cost_micro = NULL,
                                     settled_cost_micro = NULL, cost_basis = ?3,
                                     delivery_state = NULL
                                 WHERE reservation_id = ?2",
                                params![at_ms, reservation_id, COST_BASIS_UNKNOWN],
                            )?;
                            insert_billing_outbox_tx(&tx, &outbox_fact(0))?;
                            CostReconcileOutcome::ClosedUnknown
                        }
                    }
                };
                        Ok(outcome)
                    })();
                    match outcome {
                        Ok(outcome) => {
                            // A mutating outcome keeps its writes (RELEASE); a
                            // no-write outcome is undone to the savepoint,
                            // exactly like the former per-candidate rollback.
                            if matches!(
                                outcome,
                                CostReconcileOutcome::Settled(_)
                                    | CostReconcileOutcome::ClosedUnknown
                            ) {
                                tx.execute_batch("RELEASE cost_reconcile_candidate")?;
                            } else {
                                tx.execute_batch("ROLLBACK TO cost_reconcile_candidate")?;
                                tx.execute_batch("RELEASE cost_reconcile_candidate")?;
                            }
                            outcomes.push(outcome);
                        }
                        Err(e) => return Err(e),
                    }
                }
                tx.commit()?;
                Ok(outcomes)
            })?
        };
        // The report folds OUTSIDE the writer job.
        for outcome in outcomes {
            match outcome {
                CostReconcileOutcome::Skipped => {}
                CostReconcileOutcome::LeftUncertain => report.left_uncertain += 1,
                CostReconcileOutcome::ClosedUnknown => report.closed_unknown += 1,
                CostReconcileOutcome::Settled(cost) => {
                    report.settled += 1;
                    report.charged_micro = report.charged_micro.saturating_add(cost);
                }
            }
        }
        Ok(report)
    }

    /// P0-2 conservative task-completion finalize: every still-UNCERTAIN
    /// reservation of one task settles at its RESERVED ESTIMATE — the
    /// provider may have billed for a dispatched attempt whose actual never
    /// reconciled, and the reserved prediction is the honest bound the
    /// ledger already committed to. Called ONCE when the task ends (the
    /// runtime's completion-gate site); idempotent — a second pass finds
    /// nothing UNCERTAIN and never double-charges. Rows whose task row is
    /// gone are left UNCERTAIN for doctor's dangling scan (nothing can fold
    /// into a vanished envelope). Every candidate keeps its own SAVEPOINT
    /// rollback scope while the whole set commits in ONE writer job (the
    /// single writer owner serialized the former per-candidate jobs anyway).
    pub fn cost_finalize_uncertain(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        at_ms: i64,
    ) -> StoreResult<CostFinalizeReport> {
        let mut report = CostFinalizeReport::default();
        let candidates: Vec<(i64, String)> = {
            let conn = self.read()?;
            let mut stmt = conn.prepare(
                "SELECT reservation_id, op_id, attempt_op_id FROM cost_reservation
                 WHERE session_id = ?1 AND task_id = ?2 AND status = 'uncertain'
                 ORDER BY reservation_id ASC",
            )?;
            let rows = stmt.query_map(
                params![session_id.raw() as i64, task_id.raw() as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (reservation_id, op_id, attempt_op_id) = row?;
                out.push((reservation_id, attempt_op_id.unwrap_or(op_id).to_string()));
            }
            out
        };
        // ONE writer job for the WHOLE candidate set: every candidate runs
        // inside its own SAVEPOINT and the pass commits ONCE. A candidate
        // that must write nothing is rolled back to its savepoint
        // (row-local, exactly like the former per-candidate `rollback()`).
        let source_operation = "cost_finalize_uncertain".to_owned();
        let outcomes: Vec<CostFinalizeOutcome> = if candidates.is_empty() {
            Vec::new()
        } else {
            self.writer
                .execute("cost_finalize_uncertain", move |conn| {
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let mut outcomes = Vec::with_capacity(candidates.len());
                    for (reservation_id, attempt_id) in candidates {
                        tx.execute_batch("SAVEPOINT cost_finalize_candidate")?;
                        let outcome = (|| -> StoreResult<CostFinalizeOutcome> {
                            let row: Option<(i64,)> = tx
                                .query_row(
                                    "SELECT predicted_micro
                                     FROM cost_reservation
                                     WHERE reservation_id = ?1 AND status = 'uncertain'",
                                    params![reservation_id],
                                    |r| Ok((r.get(0)?,)),
                                )
                                .optional()?;
                            let Some((predicted,)) = row else {
                                return Ok(CostFinalizeOutcome::Skipped);
                            };
                            let predicted = predicted.max(0) as u64;
                            let predicted_i64 = predicted.min(i64::MAX as u64) as i64;
                            tx.execute(
                                "UPDATE cost_reservation
                                 SET status = 'settled', settled_ms = ?1, provider_cost_micro = ?2,
                                     provider_reported_micro = NULL,
                                     provider_reported_cost_micro = NULL,
                                     settled_cost_micro = ?2,
                                     estimated_cost_micro = COALESCE(estimated_cost_micro, predicted_micro),
                                     cost_basis = ?3,
                                     delivery_state = NULL
                                 WHERE reservation_id = ?4 AND status = 'uncertain'",
                                params![
                                    at_ms,
                                    predicted_i64,
                                    COST_BASIS_CONSERVATIVE_RESERVATION,
                                    reservation_id
                                ],
                            )?;
                            let n = tx.execute(
                                "UPDATE task SET spent_cost_micro = spent_cost_micro + ?1
                                 WHERE session_id = ?2 AND task_id = ?3",
                                params![
                                    predicted_i64,
                                    session_id.raw() as i64,
                                    task_id.raw() as i64
                                ],
                            )?;
                            if n == 0 {
                                // The task row is gone: nothing can fold —
                                // the savepoint undo keeps the finalize
                                // idempotent and doctor's dangling scan can
                                // see the row.
                                return Ok(CostFinalizeOutcome::LeftUncertain);
                            }
                            let call: Option<(String, String)> = tx
                                .query_row(
                                    "SELECT provider, model FROM provider_call
                                     WHERE session_id = ?1 AND reservation_id = ?2
                                     ORDER BY id DESC LIMIT 1",
                                    params![session_id.raw() as i64, reservation_id],
                                    |r| Ok((r.get(0)?, r.get(1)?)),
                                )
                                .optional()?;
                            let (provider, model) = call.unwrap_or_default();
                            insert_billing_outbox_tx(
                                &tx,
                                &BillingOutboxFact {
                                    reservation_id,
                                    session_id,
                                    task_id,
                                    attempt_id: attempt_id.clone(),
                                    provider,
                                    model,
                                    uncached_input_tokens: 0,
                                    cache_read_tokens: 0,
                                    cache_write_tokens: 0,
                                    output_tokens: 0,
                                    reasoning_tokens: 0,
                                    provider_cost_micro: predicted,
                                    provider_reported_micro: None,
                                    occurred_at_ms: at_ms,
                                    source_operation: source_operation.clone(),
                                },
                            )?;
                            Ok(CostFinalizeOutcome::Settled(predicted))
                        })();
                        match outcome {
                            Ok(outcome) => {
                                if matches!(outcome, CostFinalizeOutcome::Settled(_)) {
                                    tx.execute_batch("RELEASE cost_finalize_candidate")?;
                                } else {
                                    tx.execute_batch("ROLLBACK TO cost_finalize_candidate")?;
                                    tx.execute_batch("RELEASE cost_finalize_candidate")?;
                                }
                                outcomes.push(outcome);
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    tx.commit()?;
                    Ok(outcomes)
                })?
        };
        // The report folds OUTSIDE the writer job.
        for outcome in outcomes {
            match outcome {
                CostFinalizeOutcome::Skipped => {}
                CostFinalizeOutcome::LeftUncertain => report.left_uncertain += 1,
                CostFinalizeOutcome::Settled(predicted) => {
                    report.settled += 1;
                    report.charged_micro = report.charged_micro.saturating_add(predicted);
                }
            }
        }
        Ok(report)
    }

    /// The durable reservations of one task, newest first (bounded reads:
    /// at most `limit` rows). A corrupt `pricing_snapshot_json` fails loudly
    /// (`Corrupt`) — a reservation whose price capture cannot be read must
    /// never be silently treated as unpriced.
    pub fn cost_reservations_of(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        limit: i64,
    ) -> StoreResult<Vec<CostReservationRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT reservation_id, session_id, task_id, op_id, predicted_micro,
                    status, created_ms, settled_ms, dispatched_ms,
                    pricing_snapshot_json, provider_cost_micro,
                    provider_reported_micro, route_decision_json,
                    attempt_op_id, parent_op_id, request_id, delivery_state,
                    failure_reason_code, cost_basis,
                    provider_reported_cost_micro, estimated_cost_micro,
                    settled_cost_micro, execution_class
             FROM cost_reservation
             WHERE session_id = ?1 AND task_id = ?2
             ORDER BY reservation_id DESC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            task_id.raw() as i64,
            limit.max(0)
        ])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(cost_reservation_row_map(row)?);
        }
        Ok(out)
    }

    /// Batch form of [`Store::cost_reservations_of`] over a SET of
    /// `(session, task)` pairs: the newest `limit` rows of every pair in ONE
    /// statement per chunk (window-ranked, same per-row validation and
    /// ordering as the single-task read). A pair whose rows fail the
    /// read-time validation — or whose chunk fails at the SQL layer — is
    /// reported in `unreadable`; the caller re-reads it through
    /// [`Store::cost_reservations_of`] (typed fallback), which reproduces
    /// the single-task skip semantics exactly.
    pub fn cost_reservations_for_tasks(
        &self,
        pairs: &[(SessionId, TaskId)],
        limit: i64,
    ) -> StoreResult<TaskReservationBatch> {
        const PAIR_CHUNK: usize = 500;
        let mut batch = TaskReservationBatch::default();
        // SET semantics (the doc contract): a repeated pair is served ONCE,
        // never once per chunk it appears in.
        let unique = unique_pairs(pairs);
        for chunk in unique.chunks(PAIR_CHUNK) {
            match self.cost_reservations_for_task_chunk(chunk, limit) {
                Ok((rows, failed)) => {
                    batch.rows.extend(rows);
                    batch.unreadable.extend(failed);
                }
                // A chunk-level SQL failure degrades exactly: every pair in
                // the chunk goes to the caller's per-task fallback.
                Err(_) => batch.unreadable.extend(chunk.iter().copied()),
            }
        }
        Ok(batch)
    }

    /// One chunk of [`Store::cost_reservations_for_tasks`]: the servable
    /// rows plus the pairs the caller must re-read through the single-task
    /// API.
    fn cost_reservations_for_task_chunk(
        &self,
        chunk: &[(SessionId, TaskId)],
        limit: i64,
    ) -> StoreResult<ReservationChunkScan> {
        let sid_list: std::collections::BTreeSet<i64> =
            chunk.iter().map(|(s, _)| s.raw() as i64).collect();
        let task_list: std::collections::BTreeSet<i64> =
            chunk.iter().map(|(_, t)| t.raw() as i64).collect();
        let sid_ph = std::iter::repeat_n("?", sid_list.len())
            .collect::<Vec<_>>()
            .join(",");
        let task_ph = std::iter::repeat_n("?", task_list.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT reservation_id, session_id, task_id, op_id, predicted_micro,
                    status, created_ms, settled_ms, dispatched_ms,
                    pricing_snapshot_json, provider_cost_micro,
                    provider_reported_micro, route_decision_json,
                    attempt_op_id, parent_op_id, request_id, delivery_state,
                    failure_reason_code, cost_basis,
                    provider_reported_cost_micro, estimated_cost_micro,
                    settled_cost_micro, execution_class
             FROM (
                 SELECT *, ROW_NUMBER() OVER (
                     PARTITION BY session_id, task_id
                     ORDER BY reservation_id DESC
                 ) AS rn
                 FROM cost_reservation
                 WHERE session_id IN ({sid_ph}) AND task_id IN ({task_ph})
             )
             WHERE rn <= ?{}
             ORDER BY session_id ASC, task_id ASC, reservation_id DESC",
            sid_list.len() + task_list.len() + 1
        );
        let conn = self.read()?;
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<rusqlite::types::Value> =
            Vec::with_capacity(sid_list.len() + task_list.len() + 1);
        for sid in &sid_list {
            params_vec.push(rusqlite::types::Value::Integer(*sid));
        }
        for tid in &task_list {
            params_vec.push(rusqlite::types::Value::Integer(*tid));
        }
        params_vec.push(rusqlite::types::Value::Integer(limit.max(0)));
        let mut rows = stmt.query(rusqlite::params_from_iter(params_vec))?;
        // Rows are ordered by (session, task): a pair that fails validation
        // is marked once and its remaining rows are dropped (the caller's
        // fallback reproduces the skip).
        let mut out = Vec::new();
        let mut failed_pairs: Vec<(SessionId, TaskId)> = Vec::new();
        let mut dead: Option<(i64, i64)> = None;
        while let Some(r) = rows.next()? {
            let raw_sid: i64 = r.get(1)?;
            let raw_task: i64 = r.get(2)?;
            if dead == Some((raw_sid, raw_task)) {
                continue;
            }
            match cost_reservation_row_map(r) {
                Ok(row) => out.push(row),
                Err(_) => {
                    dead = Some((raw_sid, raw_task));
                    let sid = id_field::<SessionId>(
                        &format!("cost_reservation batch session_id {raw_sid}"),
                        raw_sid,
                    )
                    .ok();
                    let task_id = id_field::<TaskId>(
                        &format!("cost_reservation batch task_id {raw_task}"),
                        raw_task,
                    )
                    .ok();
                    if let (Some(sid), Some(task_id)) = (sid, task_id) {
                        failed_pairs.push((sid, task_id));
                    }
                }
            }
        }
        Ok((out, failed_pairs))
    }

    /// Batch form of [`Store::session_usage_tokens`] +
    /// [`Store::provider_call_prefix_rows`] over a SET of sessions in one
    /// statement per bounded chunk (the cross-session `/native/usage`
    /// aggregate). The token sum replicates the single-session expression
    /// exactly; prefix aggregates are exact unless `prefix_servable` is
    /// false (see [`SessionUsageAggregate`]).
    pub fn session_usage_aggregates(
        &self,
        session_ids: &[SessionId],
    ) -> StoreResult<SessionUsageBatch> {
        const SESSION_CHUNK: usize = 500;
        let mut batch = SessionUsageBatch::default();
        // SET semantics (the doc contract): a repeated id is served ONCE,
        // never once per chunk it appears in.
        let unique = unique_session_ids(session_ids);
        for chunk in unique.chunks(SESSION_CHUNK) {
            let ids: std::collections::BTreeSet<i64> =
                chunk.iter().map(|s| s.raw() as i64).collect();
            let ph = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT session_id,
                        COALESCE(SUM(tokens_in), 0) + COALESCE(SUM(tokens_out), 0),
                        COUNT(CASE WHEN prompt_prefix_hash IS NOT NULL THEN 1 END),
                        COALESCE(SUM(CASE WHEN prompt_prefix_hash IS NOT NULL
                                          THEN COALESCE(prompt_tokens, 0) ELSE 0 END), 0),
                        COUNT(CASE WHEN prompt_prefix_hash IS NOT NULL
                                    AND prefix_stability IS NOT NULL THEN 1 END),
                        COALESCE(SUM(CASE WHEN prompt_prefix_hash IS NOT NULL AND (
                                        typeof(prompt_prefix_hash) != 'blob'
                                        OR length(prompt_prefix_hash) != 32
                                        OR prompt_tokens IS NULL
                                        OR prompt_tokens < 0
                                        OR prompt_tokens > 4294967295
                                        OR (prefix_stability IS NOT NULL
                                            AND (prefix_stability < 0.0 OR prefix_stability > 1.0))
                                        OR prefix_segments_json IS NOT NULL
                                    ) THEN 1 ELSE 0 END), 0)
                 FROM provider_call
                 WHERE session_id IN ({ph})
                 GROUP BY session_id
                 ORDER BY session_id"
            );
            let scan = || -> StoreResult<Vec<SessionUsageAggregate>> {
                let conn = self.read()?;
                let mut stmt = conn.prepare(&sql)?;
                let params_vec: Vec<rusqlite::types::Value> = ids
                    .iter()
                    .map(|sid| rusqlite::types::Value::Integer(*sid))
                    .collect();
                let mut rows = stmt.query(rusqlite::params_from_iter(params_vec))?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    let raw_sid: i64 = r.get(0)?;
                    let Ok(session_id) = id_field::<SessionId>(
                        &format!("usage batch session_id {raw_sid}"),
                        raw_sid,
                    ) else {
                        continue; // not a decodable id: cannot be in the set
                    };
                    let tokens: i64 = r.get(1)?;
                    let prefix_rows: i64 = r.get(2)?;
                    let prefix_tokens: i64 = r.get(3)?;
                    let prefix_stability_observations: i64 = r.get(4)?;
                    let prefix_anomalies: i64 = r.get(5)?;
                    out.push(SessionUsageAggregate {
                        session_id,
                        tokens: tokens.max(0) as u64,
                        prefix_rows: prefix_rows.max(0) as u64,
                        prefix_tokens: prefix_tokens.max(0) as u64,
                        prefix_stability_observations: prefix_stability_observations.max(0) as u64,
                        prefix_servable: prefix_anomalies == 0,
                    });
                }
                Ok(out)
            };
            match scan() {
                Ok(rows) => batch.rows.extend(rows),
                Err(_) => batch.unreadable.extend(chunk.iter().copied()),
            }
        }
        Ok(batch)
    }

    /// Batch form of [`Store::list_tasks`] + [`Store::cost_task_row`] over a
    /// SET of sessions: the first `per_session` task rows of every session in
    /// the legacy order, with the raw cost columns, validated by the SAME
    /// `task_row_map` decode as `list_tasks`. A session with rows beyond
    /// `per_session` (the legacy read decodes them) or a row failing the
    /// decode is reported in `unreadable` for the caller's typed fallback.
    pub fn task_usage_scope(
        &self,
        session_ids: &[SessionId],
        per_session: u32,
    ) -> StoreResult<TaskUsageBatch> {
        const SESSION_CHUNK: usize = 500;
        let mut batch = TaskUsageBatch::default();
        // SET semantics (the doc contract): a repeated id is served ONCE,
        // never once per chunk it appears in.
        let unique = unique_session_ids(session_ids);
        for chunk in unique.chunks(SESSION_CHUNK) {
            let ids: std::collections::BTreeSet<i64> =
                chunk.iter().map(|s| s.raw() as i64).collect();
            let ph = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT task_id, session_id, goal, acceptance_criteria, plan,
                        max_tokens, max_turns, spent_tokens, spent_turns,
                        state, created_ms, updated_ms, revision, attachments,
                        contract, max_cost_micro, spent_cost_micro, rn
                 FROM (
                     SELECT task_id, session_id, goal, acceptance_criteria, plan,
                            max_tokens, max_turns, spent_tokens, spent_turns,
                            state, created_ms, updated_ms, revision, attachments,
                            contract, max_cost_micro, spent_cost_micro,
                            ROW_NUMBER() OVER (
                                PARTITION BY session_id
                                ORDER BY created_ms ASC, task_id ASC
                            ) AS rn
                     FROM task WHERE session_id IN ({ph})
                 )
                 WHERE rn <= {}
                 ORDER BY session_id ASC, rn ASC",
                i64::from(per_session) + 1
            );
            let scan = || -> StoreResult<(Vec<TaskUsageRow>, Vec<SessionId>)> {
                let conn = self.read()?;
                let mut stmt = conn.prepare(&sql)?;
                let params_vec: Vec<rusqlite::types::Value> = ids
                    .iter()
                    .map(|sid| rusqlite::types::Value::Integer(*sid))
                    .collect();
                let mut rows = stmt.query(rusqlite::params_from_iter(params_vec))?;
                let mut out = Vec::new();
                let mut unreadable: Vec<SessionId> = Vec::new();
                while let Some(r) = rows.next()? {
                    // Column order matches `task_row_map` (task_id first).
                    let raw_sid: i64 = r.get(1)?;
                    let Ok(session_id) = id_field::<SessionId>(
                        &format!("usage batch session_id {raw_sid}"),
                        raw_sid,
                    ) else {
                        continue; // not a decodable id: cannot be in the set
                    };
                    let rn: i64 = r.get(17)?;
                    if rn > i64::from(per_session) {
                        // Rows exist beyond the caller's scope: the legacy
                        // read would decode them, so this session falls back.
                        if !unreadable.contains(&session_id) {
                            unreadable.push(session_id);
                        }
                        continue;
                    }
                    match task_row_map(r, session_id) {
                        Ok(row) => {
                            let max_cost_micro: Option<i64> = r.get(15)?;
                            let spent_cost_micro: i64 = r.get(16)?;
                            out.push(TaskUsageRow {
                                session_id,
                                task_id: row.task_id,
                                max_cost_micro,
                                spent_cost_micro,
                            });
                        }
                        Err(_) => {
                            if !unreadable.contains(&session_id) {
                                unreadable.push(session_id);
                            }
                        }
                    }
                }
                Ok((out, unreadable))
            };
            match scan() {
                Ok((rows, unreadable)) => {
                    batch.rows.extend(rows);
                    batch.unreadable.extend(unreadable);
                }
                Err(_) => batch.unreadable.extend(chunk.iter().copied()),
            }
        }
        Ok(batch)
    }

    /// Batch form of [`Store::memory_facts`] filtered to `kind = "usage"`
    /// over a SET of sessions: one statement per bounded chunk, in the SAME
    /// per-session order (`updated_ms DESC, kind DESC, key DESC`), so the
    /// caller's last-wins fold over `budget`/`spent` keys is byte-identical.
    /// A session with an unreadable row of ANY kind (the legacy read decodes
    /// every kind and fails the session) lands in `unreadable` instead of
    /// being silently filtered out by the `kind` predicate.
    pub fn usage_facts_by_session(
        &self,
        session_ids: &[SessionId],
    ) -> StoreResult<UsageFactsBatch> {
        const SESSION_CHUNK: usize = 500;
        let mut batch = UsageFactsBatch::default();
        // SET semantics (the doc contract): a repeated id is served ONCE,
        // never once per chunk it appears in.
        let unique = unique_session_ids(session_ids);
        for chunk in unique.chunks(SESSION_CHUNK) {
            let ids: std::collections::BTreeSet<i64> =
                chunk.iter().map(|s| s.raw() as i64).collect();
            let ph = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(",");
            // The legacy read decodes every kind, so an unreadable row of
            // ANY kind fails the whole session; `kind = 'usage'` alone would
            // silently skip those rows. The anomaly probes keep the fetched
            // set small while making every row the legacy read would refuse
            // visible here: a non-TEXT kind/key/value fails the row decode
            // and marks the session for the per-session fallback.
            let sql = format!(
                "SELECT session_id, kind, key, value FROM memory_fact
                 WHERE session_id IN ({ph}) AND (
                     kind = 'usage'
                     OR typeof(kind) != 'text'
                     OR typeof(key) != 'text'
                     OR typeof(value) != 'text'
                 )
                 ORDER BY session_id ASC, updated_ms DESC, kind DESC, key DESC"
            );
            let scan = || -> StoreResult<UsageFactsChunkScan> {
                let conn = self.read()?;
                let mut stmt = conn.prepare(&sql)?;
                let params_vec: Vec<rusqlite::types::Value> = ids
                    .iter()
                    .map(|sid| rusqlite::types::Value::Integer(*sid))
                    .collect();
                let mut rows = stmt.query(rusqlite::params_from_iter(params_vec))?;
                let mut out = Vec::new();
                let mut unreadable: Vec<SessionId> = Vec::new();
                while let Some(r) = rows.next()? {
                    let raw_sid: i64 = r.get(0)?;
                    let Ok(session_id) = id_field::<SessionId>(
                        &format!("usage batch session_id {raw_sid}"),
                        raw_sid,
                    ) else {
                        continue; // not a decodable id: cannot be in the set
                    };
                    // Anomaly probes can only decode as TEXT when they are
                    // ordinary usage rows; anything else is the legacy
                    // read's loud failure, reported per session.
                    let row = (|| -> rusqlite::Result<(String, String, String)> {
                        let kind: String = r.get(1)?;
                        let key: String = r.get(2)?;
                        let value: String = r.get(3)?;
                        Ok((kind, key, value))
                    })();
                    match row {
                        Ok((kind, key, value)) => {
                            if kind == "usage" {
                                out.push((session_id, key, value));
                            }
                        }
                        Err(_) => {
                            if !unreadable.contains(&session_id) {
                                unreadable.push(session_id);
                            }
                        }
                    }
                }
                // A session that failed anywhere in the scan is never served
                // from the batch: the caller re-reads it per session (the
                // row order must not decide which rows leak through).
                out.retain(|(session_id, _, _)| !unreadable.contains(session_id));
                Ok((out, unreadable))
            };
            match scan() {
                Ok((rows, unreadable)) => {
                    batch.rows.extend(rows);
                    batch.unreadable.extend(unreadable);
                }
                Err(_) => batch.unreadable.extend(chunk.iter().copied()),
            }
        }
        Ok(batch)
    }

    /// Mark one DISPATCHED reservation UNCERTAIN (post-dispatch failure
    /// accounting, schema v18): a dispatched attempt whose request left the
    /// process but never settled (stream error, stall verdict, cancellation)
    /// becomes UNCERTAIN — the provider may have billed — and KEEPS
    /// consuming the reserved amount until reconcile/finalize closes it. The
    /// failure reason code and the provider request id (when known) are
    /// recorded durably for recovery forensics. Anything not `dispatched` is
    /// a typed refusal: a still-`reserved` row was never dispatched (refund
    /// it), and a settled/refunded/uncertain row is already closed.
    pub fn cost_mark_uncertain(
        &self,
        reservation_id: i64,
        reason_code: &str,
        request_id: Option<&str>,
        at_ms: i64,
    ) -> StoreResult<CostReservationState> {
        let reason_code = reason_code.to_owned();
        let request_id = request_id.map(|v| v.to_owned());
        self.writer.execute("cost_mark_uncertain", move |conn| {
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM cost_reservation WHERE reservation_id = ?1",
                    params![reservation_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(status) = status else {
                return Ok(CostReservationState::Missing);
            };
            if status != "dispatched" {
                return Ok(CostReservationState::NotOpen { current: status });
            }
            conn.execute(
                "UPDATE cost_reservation
             SET status = 'uncertain', settled_ms = ?1,
                 failure_reason_code = ?2, request_id = ?3, delivery_state = ?4
             WHERE reservation_id = ?5 AND status = 'dispatched'",
                params![
                    at_ms,
                    reason_code,
                    request_id,
                    DELIVERY_FAILED,
                    reservation_id
                ],
            )?;
            Ok(CostReservationState::Applied)
        })
    }

    /// Absorb ONE verified sample into one `model_outcome_stats` key inside
    /// an OPEN immediate transaction (the shared body of
    /// [`Store::model_outcome_stats_append`] and the Engineering Outcome
    /// Ledger's atomic record+feed append). The caller prepared every enum
    /// dimension string BEFORE enqueueing (audit item 8: writer jobs execute
    /// SQL only) and owns the transaction/commit.
    pub(crate) fn absorb_model_outcome_sample_tx(
        tx: &rusqlite::Transaction<'_>,
        key: &PreparedOutcomeKey<'_>,
        sample: ModelOutcomeSample,
        updated_ms: i64,
    ) -> StoreResult<()> {
        let current = tx
            .query_row(
                "SELECT successes_first_pass, failures_first_pass, rework_cost_micro_sum,
                        rework_turns_sum, sample_count
                 FROM model_outcome_stats
                 WHERE provider = ?1 AND model = ?2 AND phase = ?3 AND task_class = ?4
                   AND risk_bucket = ?5",
                params![
                    key.provider,
                    key.model,
                    key.phase,
                    key.task_class,
                    key.risk_bucket
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let (mut successes, mut failures, mut cost_sum, mut turns_sum, mut count) = match current {
            Some((s, f, c, t, n)) => (
                s.max(0) as u64,
                f.max(0) as u64,
                c.max(0) as u64,
                t.max(0) as u64,
                n.max(0) as u64,
            ),
            None => (0, 0, 0, 0, 0),
        };
        count = count.saturating_add(1);
        if sample.verified_success {
            successes = successes.saturating_add(1);
        } else {
            failures = failures.saturating_add(1);
            cost_sum = cost_sum.saturating_add(sample.rework_cost_micro);
            turns_sum = turns_sum.saturating_add(sample.rework_turns);
        }
        tx.execute(
            "INSERT INTO model_outcome_stats (
                provider, model, phase, task_class, risk_bucket,
                successes_first_pass, failures_first_pass, rework_cost_micro_sum,
                rework_turns_sum, sample_count, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(provider, model, phase, task_class, risk_bucket)
             DO UPDATE SET
                successes_first_pass = ?6,
                failures_first_pass = ?7,
                rework_cost_micro_sum = ?8,
                rework_turns_sum = ?9,
                sample_count = ?10,
                updated_ms = ?11",
            params![
                key.provider,
                key.model,
                key.phase,
                key.task_class,
                key.risk_bucket,
                outcome_clamp_i64(successes),
                outcome_clamp_i64(failures),
                outcome_clamp_i64(cost_sum),
                outcome_clamp_i64(turns_sum),
                outcome_clamp_i64(count),
                updated_ms
            ],
        )?;
        Ok(())
    }

    /// Append ONE verified sample to a key's durable projection (migration
    /// v18). The write is a single immediate transaction: the projection
    /// row is read, absorbed with the router registry's saturating rule,
    /// and written back — a crash can never leave a half-absorbed row, and
    /// the single-owner writer service serializes concurrent appenders. A success sample
    /// carries zero rework even when a hostile caller hands nonzero
    /// cost/turn values; `verified_success = false` records a FAILURE
    /// sample (rework was needed), never a success.
    pub fn model_outcome_stats_append(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
        task_class: TaskClass,
        risk_bucket: RiskBucket,
        sample: ModelOutcomeSample,
    ) -> StoreResult<()> {
        let provider = provider.to_owned();
        let model = model.to_owned();
        // Preparation BEFORE enqueueing: the timestamp and the JSON-encoded
        // enum dimensions are captured on the caller's thread (audit item 8)
        // — the writer job executes SQL only.
        let updated_ms = now_ms();
        let phase_json = outcome_db_phase(phase);
        let class_json = outcome_db_class(task_class);
        let bucket_json = outcome_db_bucket(risk_bucket);
        self.writer
            .execute("model_outcome_stats_append", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                Self::absorb_model_outcome_sample_tx(
                    &tx,
                    &PreparedOutcomeKey {
                        provider: &provider,
                        model: &model,
                        phase: &phase_json,
                        task_class: &class_json,
                        risk_bucket: &bucket_json,
                    },
                    sample,
                    updated_ms,
                )?;
                tx.commit()?;
                Ok(())
            })
    }

    /// The exact per-key projection row, or `None` when the key has no
    /// samples. Every column is parsed fallibly: an unreadable enum text,
    /// a negative count or a broken `sample_count = successes + failures`
    /// invariant surfaces as `StoreError::Corrupt`, never a silent number.
    pub fn model_outcome_stats_get(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
        task_class: TaskClass,
        risk_bucket: RiskBucket,
    ) -> StoreResult<Option<ModelOutcomeStatsRow>> {
        let conn = self.read()?;
        let raw: Option<RawOutcomeStatsRow> = conn
            .query_row(
                &format!(
                    "SELECT provider, model, phase, task_class, risk_bucket,
                            successes_first_pass, failures_first_pass,
                            rework_cost_micro_sum, rework_turns_sum,
                            sample_count, updated_ms
                     FROM model_outcome_stats WHERE {OUTCOME_STATS_KEY_SQL}"
                ),
                params![
                    provider,
                    model,
                    outcome_db_phase(phase),
                    outcome_db_class(task_class),
                    outcome_db_bucket(risk_bucket)
                ],
                outcome_stats_row_raw,
            )
            .optional()?;
        match raw {
            Some(raw) => Ok(Some(outcome_stats_row_validate(raw)?)),
            None => Ok(None),
        }
    }

    /// The per-phase consult the economic router performs: every
    /// class/risk bucket row of one (provider, model, phase) folded into
    /// one saturating accumulator row (a route request carries no
    /// class/risk dimensions of its own). `None` when no row exists for the
    /// triple. A corrupt source row fails the whole consult loudly.
    pub fn model_outcome_stats_phase(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
    ) -> StoreResult<Option<ModelOutcomeStatsRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT provider, model, phase, task_class, risk_bucket,
                    successes_first_pass, failures_first_pass,
                    rework_cost_micro_sum, rework_turns_sum,
                    sample_count, updated_ms
             FROM model_outcome_stats
             WHERE provider = ?1 AND model = ?2 AND phase = ?3",
        )?;
        let mut rows = stmt.query(params![provider, model, outcome_db_phase(phase)])?;
        let mut acc: Option<ModelOutcomeStatsRow> = None;
        while let Some(row) = rows.next()? {
            let validated = outcome_stats_row_validate(outcome_stats_row_raw(row)?)?;
            acc = Some(match acc {
                None => validated,
                Some(mut a) => {
                    a.successes_first_pass = a
                        .successes_first_pass
                        .saturating_add(validated.successes_first_pass);
                    a.failures_first_pass = a
                        .failures_first_pass
                        .saturating_add(validated.failures_first_pass);
                    a.rework_cost_micro_sum = a
                        .rework_cost_micro_sum
                        .saturating_add(validated.rework_cost_micro_sum);
                    a.rework_turns_sum = a
                        .rework_turns_sum
                        .saturating_add(validated.rework_turns_sum);
                    a.sample_count = a.sample_count.saturating_add(validated.sample_count);
                    a.updated_ms = a.updated_ms.max(validated.updated_ms);
                    a
                }
            });
        }
        Ok(acc)
    }

    // ---------------------------------------------------------------------
    // Durable evidence authority (v21)
    // ---------------------------------------------------------------------

    /// Every durable `provider_call` row attributable to
    /// `(session_id, task_id)`, oldest row first (bounded by `limit`).
    ///
    /// ATTRIBUTION: a row belongs to the task when it keys one of the task's
    /// `cost_reservation` rows —
    ///
    /// - by `reservation_id` (v18 attempt usage rows), or
    /// - by `attempt_op_id` (attempt usage rows whose reservation link was
    ///   never written), or
    /// - by the shared logical op: `cost_reservation.op_id` for legacy
    ///   single-attempt reservations and their legacy usage rows, and
    ///   `parent_op_id` for the v13 prefix-observation rows (which carry no
    ///   attempt/reservation identity of their own).
    ///
    /// Prefix-observation rows are intentionally included: they carry NULL
    /// usage counters, so a summation over a task's rows never double counts
    /// a call's usage, while their `prompt_tokens`/`prefix_stability` pair is
    /// the only durable cache attribution that exists.
    ///
    /// The returned flag is `true` when MORE attributable rows exist beyond
    /// `limit` (the caller asked for a bounded read; the KPI derivation
    /// refuses partial totals). A negative `limit` reads zero rows and
    /// reports truncation. Values are validated on read: a negative token
    /// counter, or a prefix stability outside [0, 1] / non-finite, is a
    /// loud `Malformed`, never a silently wrong number.
    pub fn provider_call_task_rows(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        limit: i64,
    ) -> StoreResult<(Vec<ProviderCallTaskRow>, bool)> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT p.id, p.op_id, p.attempt_op_id, p.reservation_id,
                    p.provider, p.model, p.status, p.started_ms, p.ended_ms,
                    p.tokens_in, p.tokens_out, p.prompt_tokens,
                    p.prefix_stability
             FROM provider_call p
             WHERE p.session_id = ?1
               AND EXISTS (
                   SELECT 1 FROM cost_reservation r
                   WHERE r.session_id = p.session_id AND r.task_id = ?2
                     AND (
                         r.reservation_id = p.reservation_id
                         OR (p.attempt_op_id IS NOT NULL
                             AND r.attempt_op_id = p.attempt_op_id)
                         OR (p.attempt_op_id IS NULL
                             AND (r.op_id = p.op_id
                                  OR r.parent_op_id = p.op_id))
                     )
               )
             ORDER BY p.id ASC LIMIT ?3",
        )?;
        let max = limit.max(0);
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            task_id.raw() as i64,
            max.saturating_add(1)
        ])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let stability: Option<f64> = r.get(12)?;
            if let Some(s) = stability {
                if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                    return Err(StoreError::Malformed(format!(
                        "provider_call prefix_stability {s} out of [0, 1]"
                    )));
                }
            }
            let row_id: i64 = r.get(0)?;
            let attempt_op_id: Option<i64> = r.get(2)?;
            out.push(ProviderCallTaskRow {
                row_id,
                op_id: id_field(
                    &format!("provider_call {row_id} op_id"),
                    r.get::<_, i64>(1)?,
                )?,
                attempt_op_id: id_field_opt(
                    &format!("provider_call {row_id} attempt_op_id"),
                    attempt_op_id,
                )?,
                reservation_id: r.get(3)?,
                provider: r.get(4)?,
                model: r.get(5)?,
                status: r.get(6)?,
                started_ms: r.get(7)?,
                ended_ms: r.get(8)?,
                tokens_in: efficiency_counter(r.get(9)?, "provider_call.tokens_in")?,
                tokens_out: efficiency_counter(r.get(10)?, "provider_call.tokens_out")?,
                prompt_tokens: efficiency_counter(r.get(11)?, "provider_call.prompt_tokens")?,
                prefix_stability: stability,
            });
        }
        let truncated = out.len() as i64 > max;
        if truncated {
            out.truncate(max as usize);
        }
        Ok((out, truncated))
    }
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "billing_typed_ledger_tests.rs"]
mod typed_ledger_tests;
