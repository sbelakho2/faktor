//! `billing`: cohesive slice of the mechanically decomposed parent module.

use super::*;

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
// in the test-only `faktor-tests-efficiency` crate; this method is the
// durable read it derives from.
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
            .execute("record_provider_call_with_prefix_segments", move |conn| {
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
            .execute("record_provider_call_attempt", move |conn| {
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
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, prompt_prefix_hash, prompt_tokens, prefix_stability, prefix_segments_json
             FROM provider_call
             WHERE session_id = ?1 AND prompt_prefix_hash IS NOT NULL
             ORDER BY id ASC",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64])?;
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
                    ))
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
                return Err(StoreError::Conflict(no_row));
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
        )
    }

    /// [`Store::cost_reserve`] plus the immutable route-time price capture
    /// (P0-1): the snapshot is persisted on the reservation so settlement —
    /// including settlement that happens after a daemon restart — prices the
    /// call's usage against exactly the prices the router froze at route
    /// time. Bounded by the session layer before this call.
    pub fn cost_reserve_priced(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        op_id: OpId,
        predicted_micro: u64,
        created_ms: i64,
        pricing_snapshot_json: Option<&str>,
    ) -> StoreResult<CostReserveOutcome> {
        self.cost_reserve_inner(
            session_id,
            task_id,
            op_id,
            predicted_micro,
            created_ms,
            pricing_snapshot_json,
        )
    }

    pub(crate) fn cost_reserve_inner(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        op_id: OpId,
        predicted_micro: u64,
        created_ms: i64,
        pricing_snapshot_json: Option<&str>,
    ) -> StoreResult<CostReserveOutcome> {
        let pricing_snapshot_json = pricing_snapshot_json.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: the raw state gate terms and the
        // no-row conflict text (audit finding 5 — the closure formats
        // nothing and never decodes `TaskState`).
        let (permitted_states, forbidden_states, corrupt_state) = provider_operation_gate_terms();
        let no_row = format!("cost reserve: task {task_id} of session {session_id} has no row");
        self.writer.execute("cost_reserve_inner", move |conn| {
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
            return Err(StoreError::Conflict(no_row));
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
                None => Err(StoreError::Corrupt(vec![corrupt_state])),
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
                 pricing_snapshot_json, estimated_cost_micro)
             VALUES (?1, ?2, ?3, ?4, 'reserved', ?5, ?6, ?4)
             RETURNING reservation_id",
            params![
                session_id.raw() as i64,
                task_id.raw() as i64,
                op_id.raw() as i64,
                predicted_micro.min(i64::MAX as u64) as i64,
                created_ms,
                pricing_snapshot_json
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
    ) -> StoreResult<CostReserveOutcome> {
        let attempt = attempt.to_owned();
        let pricing_snapshot_json = pricing_snapshot_json.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: the raw state gate terms and the
        // no-row conflict text (audit finding 5 — the closure formats
        // nothing and never decodes `TaskState`).
        let (permitted_states, forbidden_states, corrupt_state) = provider_operation_gate_terms();
        let no_row = format!("cost reserve: task {task_id} of session {session_id} has no row");
        self.writer.execute("cost_reserve_attempt", move |conn| {
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
                return Err(StoreError::Conflict(no_row));
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
                    None => Err(StoreError::Corrupt(vec![corrupt_state])),
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
                 estimated_cost_micro)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, 'reserved', ?6, ?7, ?5)
             RETURNING reservation_id",
                params![
                    session_id.raw() as i64,
                    task_id.raw() as i64,
                    attempt.attempt_op_id.raw() as i64,
                    attempt.logical_op_id.raw() as i64,
                    predicted,
                    created_ms,
                    pricing_snapshot_json
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
        // Preparation BEFORE enqueueing: the refusal diagnostic (the task ids
        // are read inside the transaction, so the message is static).
        let task_missing =
            "cost settle: task has no row; the task machine owns creation".to_owned();
        self.writer.execute("cost_settle", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<(i64, i64, String)> = tx
                .query_row(
                    "SELECT session_id, task_id, status FROM cost_reservation
                 WHERE reservation_id = ?1",
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
                return Err(StoreError::Conflict(task_missing));
            }
            tx.commit()?;
            Ok(CostReservationState::Applied)
        })
    }

    /// Settle one RESERVED/DISPATCHED reservation from a provider usage
    /// frame (P0-1 settlement truth, schema v18) in ONE transaction. The
    /// chosen actual: the provider-reported cost when the usage frame
    /// carried one (authoritative); otherwise the token categories x the
    /// reservation's stored route-time [`PricingSnapshot`] — exactly the
    /// price lines the router froze, never a fabricated per-token number.
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
        let route_decision_json = route_decision_json.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: every refusal diagnostic (the task
        // ids are read inside the transaction, so that message is static).
        let snapshot_context = format!("reservation {reservation_id} pricing_snapshot_json");
        let task_missing =
            "cost settle: task has no row; the task machine owns creation".to_owned();
        self.writer.execute("cost_settle_usage", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<(i64, i64, String, Option<String>)> = tx
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
            // The chosen actual: the provider-reported cost when the usage frame
            // carried one (authoritative); else the locally calculated
            // categories x snapshot. Unknown price + no reported cost: with a
            // hard cap this is a typed refusal (the cap was protected by the
            // reservation's prediction, but a fabricated actual must never
            // land); with no cap the row closes as a documented Unknown spend.
            let chosen_i64: i64 = match provider_reported_micro {
                Some(_) => reported_i64.expect("reported Some maps to Some"),
                None => match local_i64 {
                    Some(local) => local,
                    None => {
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
                        tx.commit()?;
                        return Ok(CostSettleOutcome::AppliedUnknown);
                    }
                },
            };
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
                return Err(StoreError::Conflict(task_missing));
            }
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
    /// settles from ITS OWN attempt's row — never from the sibling's. Each
    /// reservation settles in its own immediate transaction. Rows without a
    /// completed provider-call row of their own attempt — or unpriced under
    /// a hard cap — stay UNCERTAIN (the task-completion finalize is their
    /// conservative backstop). Idempotent: a second pass finds the settled
    /// rows closed.
    pub fn cost_reconcile_uncertain(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        at_ms: i64,
    ) -> StoreResult<CostReconcileReport> {
        let mut report = CostReconcileReport::default();
        let candidates: Vec<i64> = {
            let conn = self.read()?;
            let mut stmt = conn.prepare(
                "SELECT cr.reservation_id
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
                |r| r.get::<_, i64>(0),
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };
        for reservation_id in candidates {
            // One prepared command per candidate: the transaction body runs
            // on the writer owner; the report folds OUTSIDE it.
            let snapshot_context = format!("reservation {reservation_id} pricing_snapshot_json");
            let outcome = self.writer.execute("cost_reconcile_uncertain", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                // Only a row still UNCERTAIN settles (concurrent recovery or
                // a previous pass may have closed it).
                let row: Option<(u64, Option<String>)> = tx
                    .query_row(
                        "SELECT predicted_micro, pricing_snapshot_json FROM cost_reservation
                         WHERE reservation_id = ?1 AND status = 'uncertain'",
                        params![reservation_id],
                        |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?)),
                    )
                    .optional()?;
                let Some((_predicted, snapshot_json)) = row else {
                    tx.commit()?;
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
                let tokens: Option<(i64, i64)> = tx
                    .query_row(
                        "SELECT COALESCE(p.tokens_in, 0), COALESCE(p.tokens_out, 0)
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
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some((tokens_in, tokens_out)) = tokens else {
                    tx.commit()?;
                    return Ok(CostReconcileOutcome::LeftUncertain);
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
                            // roll the row back for doctor's dangling scan.
                            tx.rollback()?;
                            return Ok(CostReconcileOutcome::LeftUncertain);
                        }
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
                            CostReconcileOutcome::ClosedUnknown
                        }
                    }
                };
                tx.commit()?;
                Ok(outcome)
            })?;
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
    /// into a vanished envelope). Each reservation settles in its own
    /// immediate transaction.
    pub fn cost_finalize_uncertain(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        at_ms: i64,
    ) -> StoreResult<CostFinalizeReport> {
        let mut report = CostFinalizeReport::default();
        let candidates: Vec<i64> = {
            let conn = self.read()?;
            let mut stmt = conn.prepare(
                "SELECT reservation_id FROM cost_reservation
                 WHERE session_id = ?1 AND task_id = ?2 AND status = 'uncertain'
                 ORDER BY reservation_id ASC",
            )?;
            let rows = stmt.query_map(
                params![session_id.raw() as i64, task_id.raw() as i64],
                |r| r.get::<_, i64>(0),
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };
        for reservation_id in candidates {
            // One prepared command per candidate: the transaction body runs
            // on the writer owner; the report folds OUTSIDE it.
            let outcome = self
                .writer
                .execute("cost_finalize_uncertain", move |conn| {
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let predicted: Option<u64> = tx
                        .query_row(
                            "SELECT predicted_micro FROM cost_reservation
                         WHERE reservation_id = ?1 AND status = 'uncertain'",
                            params![reservation_id],
                            |r| r.get::<_, i64>(0).map(|v| v.max(0) as u64),
                        )
                        .optional()?;
                    let Some(predicted) = predicted else {
                        tx.commit()?;
                        return Ok(CostFinalizeOutcome::Skipped);
                    };
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
                        params![predicted_i64, session_id.raw() as i64, task_id.raw() as i64],
                    )?;
                    if n == 0 {
                        // The task row is gone: nothing can fold — roll the row
                        // back so the finalize stays idempotent and doctor's
                        // dangling scan can see it.
                        tx.rollback()?;
                        return Ok(CostFinalizeOutcome::LeftUncertain);
                    }
                    tx.commit()?;
                    Ok(CostFinalizeOutcome::Settled(predicted))
                })?;
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
                    settled_cost_micro
             FROM cost_reservation
             WHERE session_id = ?1 AND task_id = ?2
             ORDER BY reservation_id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![session_id.raw() as i64, task_id.raw() as i64, limit.max(0)],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<i64>>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, Option<i64>>(13)?,
                    r.get::<_, Option<i64>>(14)?,
                    r.get::<_, Option<String>>(15)?,
                    r.get::<_, Option<String>>(16)?,
                    r.get::<_, Option<String>>(17)?,
                    r.get::<_, Option<String>>(18)?,
                    r.get::<_, Option<i64>>(19)?,
                    r.get::<_, Option<i64>>(20)?,
                    r.get::<_, Option<i64>>(21)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (
                reservation_id,
                row_session,
                row_task,
                op_id,
                predicted,
                status,
                created_ms,
                settled_ms,
                dispatched_ms,
                snapshot_json,
                provider_cost,
                provider_reported,
                route_json,
                attempt_op_id,
                parent_op_id,
                request_id,
                delivery_state,
                failure_reason_code,
                cost_basis,
                provider_reported_cost_micro,
                estimated_cost_micro,
                settled_cost_micro,
            ) = row?;
            let pricing_snapshot = match snapshot_json {
                Some(json) => Some(parse_json(
                    &format!("reservation {reservation_id} pricing_snapshot_json"),
                    &json,
                )?),
                None => None,
            };
            out.push(CostReservationRow {
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
            });
        }
        Ok(out)
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
                let current = tx
                    .query_row(
                        "SELECT successes_first_pass, failures_first_pass, rework_cost_micro_sum,
                        rework_turns_sum, sample_count
                 FROM model_outcome_stats
                 WHERE provider = ?1 AND model = ?2 AND phase = ?3 AND task_class = ?4
                   AND risk_bucket = ?5",
                        params![provider, model, phase_json, class_json, bucket_json],
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
                let (mut successes, mut failures, mut cost_sum, mut turns_sum, mut count) =
                    match current {
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
                        provider,
                        model,
                        phase_json,
                        class_json,
                        bucket_json,
                        outcome_clamp_i64(successes),
                        outcome_clamp_i64(failures),
                        outcome_clamp_i64(cost_sum),
                        outcome_clamp_i64(turns_sum),
                        outcome_clamp_i64(count),
                        updated_ms
                    ],
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
mod tests {
    use super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), true).unwrap();
        (dir, s)
    }

    #[test]
    fn op_id_seq_seeds_high_and_reserves_contiguous_global_ranges() {
        // Fresh stores seed the ONE global row from the migration-time clock
        // (see `op_id_seq_seed`): the seed must sit far above every
        // pre-migration `clock + counter` id and stay aligned to the 1024-id
        // reservation quantum.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s1 = store.create_session(ws, "a", "p", "m").unwrap();
        let s2 = store.create_session(ws, "b", "p", "m").unwrap();
        let hw0 = store.op_id_seq_high_water().unwrap();
        assert!(
            hw0 > (1u64 << 20),
            "seed must dominate clock+counter ids: {hw0}"
        );
        assert_eq!(hw0 % 1024, 0, "seed must sit on a quantum boundary");
        // The sequence is GLOBAL: alternating sessions get contiguous ranges.
        let (a0, n0) = store.alloc_op_ids(s1.id, 100).unwrap();
        let (b0, n1) = store.alloc_op_ids(s2.id, 250).unwrap();
        let (a1, n2) = store.alloc_op_ids(s1.id, 7).unwrap();
        assert_eq!((a0, n0), (hw0, 100), "first range starts at the seed");
        assert_eq!((b0, n1), (hw0 + 100, 250), "second range is contiguous");
        assert_eq!((a1, n2), (hw0 + 350, 7), "ranges never interleave");
        assert_eq!(
            store.op_id_seq_high_water().unwrap(),
            hw0 + 357,
            "high water is one past the last reserved id"
        );
        assert_ne!(a0, 0, "zero is contractually impossible");
    }
}

#[cfg(test)]
mod typed_ledger_tests {
    use super::*;

    pub(crate) fn seed_task(
        store: &Store,
        session_id: SessionId,
        task_id: TaskId,
        criteria: Vec<String>,
        state: TaskState,
    ) -> TaskRow {
        let row = TaskRow {
            task_id,
            session_id,
            goal: "g".into(),
            acceptance_criteria: criteria,
            plan: vec![],
            attachments: vec![],
            max_tokens: None,
            max_turns: None,
            spent_tokens: 0,
            spent_turns: 0,
            state,
            revision: TaskRevision::new(1),
            created_ms: 1,
            updated_ms: 1,
        };
        store.upsert_task(&row).unwrap();
        row
    }

    /// Seed a task and walk the machine into `Verifying` through the legal
    /// store edges (a row can never be created completion-relevant).
    pub(crate) fn seed_verifying(
        store: &Store,
        session_id: SessionId,
        task_id: TaskId,
        criteria: Vec<String>,
    ) -> TaskRow {
        let mut row = seed_task(store, session_id, task_id, criteria, TaskState::Pending);
        let mut bump = |state: TaskState| {
            row.state = state;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
        };
        bump(TaskState::Running);
        bump(TaskState::NeedsVerification);
        bump(TaskState::Verifying);
        row
    }

    pub(crate) fn passing_record(
        task: &TaskRow,
        ws: WorkspaceId,
        wt: WorktreeId,
    ) -> VerificationRecordRow {
        VerificationRecordRow {
            id: VerificationRecordId::new(1),
            task_id: task.task_id,
            revision: task.revision,
            workspace_id: ws,
            worktree_id: wt,
            tree_hash: None,
            criteria: task
                .acceptance_criteria
                .iter()
                .map(|c| CriterionVerification {
                    criterion_key: c.clone(),
                    passed: true,
                    evidence: Some("exit 0".into()),
                    binding: None,
                })
                .collect(),
            checks: vec![],
            changed_files: vec![],
            unrelated_changes: vec![],
            reviewer: None,
            status: VerificationStatus::Passed,
            started_ms: 1,
            completed_ms: None,
        }
    }

    #[test]
    fn completion_vs_reserve_race_never_completes_with_held_reservations() {
        // Adversarial: 1,000 barrier-controlled races of the completion
        // sequence (Running -> NeedsVerification -> Verifying then the
        // exclusive completion transaction) against a concurrent reservation
        // on the SAME task. The race is real: a reserve is admitted while the
        // task is still Running and the completion finds the row in its
        // in-transaction COUNT; or the completion lands first and the late
        // reserve is refused by the task-state gate. The two can never both
        // commit. After EVERY race the invariant is checked directly: a task
        // row that reads VerifiedComplete has zero reserved, zero dispatched
        // and zero uncertain reservation rows; a reserve that landed first
        // leaves the task Verifying with exactly its held row and a typed
        // ReservationsHeld refusal.
        const RACES: usize = 1_000;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "race", "p", "m").unwrap();
        let sid = s.id;
        let mut reserve_wins = 0usize;
        let mut complete_wins = 0usize;
        for i in 0..RACES {
            let task_id = TaskId::new(i as u64 + 1);
            // Seed Pending (rev 1) -> Running (rev 2). The completion side
            // then walks Running -> NeedsVerification (rev 3) -> Verifying
            // (rev 4) before its exclusive completion.
            let task = seed_task(&store, sid, task_id, vec![], TaskState::Pending);
            let mut running = task.clone();
            running.state = TaskState::Running;
            running.revision = running.revision.checked_next().unwrap();
            store.upsert_task(&running).unwrap();
            // The passing record certifies the revision the completion side
            // will present (rev 4) — the record can exist before the task
            // reaches Verifying (the completion transaction checks both).
            let verifying_revision = TaskRevision::new(4);
            let record_task = TaskRow {
                revision: verifying_revision,
                ..running.clone()
            };
            let record = passing_record(&record_task, ws, WorktreeId::new(1));
            let rec_id = store.verification_record_put(&record).unwrap();
            let barrier = std::sync::Barrier::new(2);
            // The two racers leave the barrier together. Three fifths of the
            // races run free (true writer-lock arbitration); one fifth gives
            // each side a 1ms head start so BOTH orderings are exercised on
            // every host (the OS wake order alone is not a fair scheduler).
            let fork = i % 5;
            let do_reserve = || {
                barrier.wait();
                if fork == 3 {
                    // Completion head start: reserve-first must not be the
                    // only ordering this host can produce.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                store.cost_reserve_priced(
                    sid,
                    task_id,
                    OpId::new(i as u64 + 1),
                    100,
                    now_ms(),
                    None,
                )
            };
            let do_complete = || {
                barrier.wait();
                if fork == 4 {
                    // Reserve head start.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                let mut row = store.get_task(sid, task_id).unwrap().unwrap();
                row.state = TaskState::NeedsVerification;
                row.revision = row.revision.checked_next().unwrap();
                store.upsert_task(&row).unwrap();
                row.state = TaskState::Verifying;
                row.revision = row.revision.checked_next().unwrap();
                store.upsert_task(&row).unwrap();
                store.task_complete_verified(sid, task_id, row.revision, rec_id, now_ms())
            };
            let (reserve_out, complete_out) = std::thread::scope(|scope| {
                let reserve = scope.spawn(do_reserve);
                let complete = scope.spawn(do_complete);
                (reserve.join().unwrap(), complete.join().unwrap())
            });
            let rows = store.cost_reservations_of(sid, task_id, i64::MAX).unwrap();
            let count = |status: &str| rows.iter().filter(|r| r.status == status).count();
            // The outer Result is the store call itself; the inner one is the
            // typed completion refusal (proof/revision/accounting gate).
            match (reserve_out, complete_out.unwrap()) {
                (Ok(CostReserveOutcome::Granted(_)), Ok(_)) => {
                    panic!("race {i}: reserve AND completion both committed")
                }
                (Ok(CostReserveOutcome::Granted(_)), Err(refusal)) => {
                    assert!(
                        matches!(
                            refusal,
                            TaskCompletionRefusal::ReservationsHeld {
                                reserved: 1,
                                dispatched: 0,
                                uncertain: 0,
                                reserved_micro: 100,
                                ..
                            }
                        ),
                        "race {i}: completion must name the held row: {refusal:?}"
                    );
                    assert_eq!(count("reserved"), 1);
                    assert_eq!(
                        store.get_task(sid, task_id).unwrap().unwrap().state,
                        TaskState::Verifying,
                        "race {i}: a refused completion never transitions"
                    );
                    reserve_wins += 1;
                }
                (Ok(CostReserveOutcome::Exceeded { .. }), _) => {
                    panic!("race {i}: an uncapped task refused a reserve")
                }
                (Err(reserve_err), Ok(_)) => {
                    // Completion won the write lock: the reserve must have
                    // been refused by the task-state gate, writing nothing.
                    assert!(
                        reserve_err.to_string().contains("cost reserve: task state"),
                        "race {i}: a reserve after completion must refuse typed on state: \
                         {reserve_err}"
                    );
                    assert_eq!(
                        store.get_task(sid, task_id).unwrap().unwrap().state,
                        TaskState::VerifiedComplete
                    );
                    assert_eq!(count("reserved"), 0, "race {i}: reserved must be 0");
                    assert_eq!(count("dispatched"), 0, "race {i}: dispatched must be 0");
                    assert_eq!(count("uncertain"), 0, "race {i}: uncertain must be 0");
                    complete_wins += 1;
                }
                (Err(reserve_err), Err(refusal)) => {
                    panic!("race {i}: neither side committed ({reserve_err} / {refusal:?})")
                }
            }
        }
        eprintln!(
            "completion-vs-reserve races: reserve-first {reserve_wins}, completion-first {complete_wins}"
        );
        assert!(
            reserve_wins > 0,
            "the race never exercised the reserve-first ordering"
        );
        assert!(
            complete_wins > 0,
            "the race never exercised the completion-first ordering"
        );
    }

    #[test]
    fn reserve_refused_once_the_task_permits_no_new_provider_operation() {
        // The reserve transaction's task-state condition: completion-relevant
        // and terminal states refuse typed and write NOTHING; the remaining
        // machine states admit. VerifiedComplete is produced through the
        // completion path (raw row writes cannot mint it).
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        // Permitted: every state that still allows new provider work.
        for (n, state) in [
            TaskState::Pending,
            TaskState::Planning,
            TaskState::Running,
            TaskState::Waiting,
            TaskState::Blocked,
        ]
        .into_iter()
        .enumerate()
        {
            let task_id = TaskId::new(n as u64 + 1);
            seed_task(&store, s.id, task_id, vec![], state);
            let out = store
                .cost_reserve_priced(s.id, task_id, OpId::new(n as u64 + 1), 1, now_ms(), None)
                .unwrap();
            assert!(
                matches!(out, CostReserveOutcome::Granted(_)),
                "{state:?} must permit a reserve: {out:?}"
            );
        }
        // Refused: completion-relevant and terminal states.
        let mut n = 100u64;
        for state in [
            TaskState::NeedsVerification,
            TaskState::Verifying,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            n += 1;
            let task_id = TaskId::new(n);
            if state == TaskState::NeedsVerification {
                // NeedsVerification is completion-relevant: walk the legal
                // machine edge out of Verifying (never a raw seed).
                let mut row = seed_verifying(&store, s.id, task_id, vec![]);
                row.state = TaskState::NeedsVerification;
                row.revision = row.revision.checked_next().unwrap();
                store.upsert_task(&row).unwrap();
            } else if state == TaskState::Verifying {
                seed_verifying(&store, s.id, task_id, vec![]);
            } else {
                seed_task(&store, s.id, task_id, vec![], state);
            }
            let err = store
                .cost_reserve_priced(s.id, task_id, OpId::new(n), 1, now_ms(), None)
                .unwrap_err();
            assert!(
                err.to_string().contains("cost reserve: task state"),
                "{state:?} must refuse typed: {err}"
            );
            assert!(
                store
                    .cost_reservations_of(s.id, task_id, 10)
                    .unwrap()
                    .is_empty(),
                "{state:?}: a refused reserve wrote a row"
            );
        }
        // VerifiedComplete (only the completion transaction may produce it).
        let task_id = TaskId::new(200);
        let task = seed_verifying(&store, s.id, task_id, vec![]);
        let record = passing_record(&task, ws, WorktreeId::new(1));
        let rec_id = store.verification_record_put(&record).unwrap();
        store
            .task_complete_verified(s.id, task_id, task.revision, rec_id, now_ms())
            .unwrap()
            .unwrap();
        let err = store
            .cost_reserve_priced(s.id, task_id, OpId::new(200), 1, now_ms(), None)
            .unwrap_err();
        assert!(
            err.to_string().contains("cost reserve: task state"),
            "VerifiedComplete must refuse typed: {err}"
        );
        assert!(store
            .cost_reservations_of(s.id, task_id, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn corrupt_task_state_fails_reserve_and_completion_typed_without_writes() {
        // SQL failure mode: the task row's state column cannot be parsed.
        // Both the reservation transaction and the completion transaction
        // must fail TYPED before writing anything (no partial transition, no
        // reservation row); healing the row restores both paths — the error
        // was the corruption, not a poisoned connection.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_verifying(&store, s.id, TaskId::new(1), vec![]);
        let record = passing_record(&task, ws, WorktreeId::new(1));
        let rec_id = store.verification_record_put(&record).unwrap();
        store
            .raw_conn()
            .execute(
                "UPDATE task SET state = '{not-a-state}' WHERE session_id = ?1 AND task_id = ?2",
                params![s.id.raw() as i64, task.task_id.raw() as i64],
            )
            .unwrap();
        let err = store
            .cost_reserve_priced(s.id, task.task_id, OpId::new(9), 1, now_ms(), None)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Corrupt(_)),
            "corrupt state must be a typed parse failure: {err}"
        );
        assert!(store
            .cost_reservations_of(s.id, task.task_id, 10)
            .unwrap()
            .is_empty());
        let err = store
            .task_complete_verified(s.id, task.task_id, task.revision, rec_id, now_ms())
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Corrupt(_)),
            "corrupt state must fail completion typed: {err}"
        );
        let raw: (String, i64) = store
            .raw_conn()
            .query_row(
                "SELECT state, revision FROM task WHERE session_id = ?1 AND task_id = ?2",
                params![s.id.raw() as i64, task.task_id.raw() as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(raw.0, "{not-a-state}", "the corrupt marker is untouched");
        assert_eq!(
            raw.1,
            task.revision.raw() as i64,
            "the failed completion transaction rolled back whole"
        );
        // Heal the row behind the API's back (a state that permits new
        // provider work): the reserve path works again — the failure was
        // the corruption, not a poisoned connection.
        store
            .raw_conn()
            .execute(
                "UPDATE task SET state = '\"running\"' WHERE session_id = ?1 AND task_id = ?2",
                params![s.id.raw() as i64, task.task_id.raw() as i64],
            )
            .unwrap();
        assert!(matches!(
            store
                .cost_reserve_priced(s.id, task.task_id, OpId::new(10), 1, now_ms(), None)
                .unwrap(),
            CostReserveOutcome::Granted(_)
        ));
    }

    pub(crate) fn known_snapshot_json() -> String {
        use faktor_core::model::{MicroUsdPerMillionTokens, PriceQuote};
        serde_json::to_string(&PricingSnapshot::exact(
            PriceQuote {
                input: MicroUsdPerMillionTokens(15_000_000),
                output: MicroUsdPerMillionTokens(60_000_000),
                cache_read: MicroUsdPerMillionTokens(3_000_000),
                cache_write: MicroUsdPerMillionTokens(7_000_000),
            },
            7,
            "store-test".into(),
        ))
        .unwrap()
    }

    /// Free budget of one task = cap - spent - holding predictions
    /// (reserved + dispatched + uncertain), the store's own formula.
    pub(crate) fn free_micro(store: &Store, session: SessionId, task: TaskId) -> u64 {
        let row = store.cost_task_row(session, task).unwrap().unwrap();
        let max = row.max_cost_micro.unwrap_or(0);
        let rows = store.cost_reservations_of(session, task, i64::MAX).unwrap();
        let held: u64 = rows
            .iter()
            .filter(|r| matches!(r.status.as_str(), "reserved" | "dispatched" | "uncertain"))
            .map(|r| r.predicted_micro)
            .sum();
        max.saturating_sub(row.spent_cost_micro)
            .saturating_sub(held)
    }

    #[test]
    fn refund_is_sql_guarded_after_dispatch_and_after_terminal_states() {
        // (i) The hardened refund: pre-dispatch refunds work; every
        // post-dispatch or terminal state leaves the row UNTOUCHED with the
        // free budget unchanged — enforced by the guarded UPDATE, so even a
        // mis-calling runtime can never free a dispatched reservation.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        store.cost_task_cap_set(s.id, tid, Some(1_000)).unwrap();
        let now = now_ms();

        // Refund BEFORE dispatch: applied, money free again.
        let CostReserveOutcome::Granted(r) = store
            .cost_reserve(s.id, tid, OpId::new(1), 400, now)
            .unwrap()
        else {
            panic!("reserve 1 granted")
        };
        assert_eq!(free_micro(&store, s.id, tid), 600);
        assert_eq!(store.cost_refund(r, now).unwrap(), RefundOutcome::Applied);
        assert_eq!(free_micro(&store, s.id, tid), 1_000, "refund frees money");

        // Refund AFTER dispatch: the guarded UPDATE changes zero rows.
        let CostReserveOutcome::Granted(r2) = store
            .cost_reserve(s.id, tid, OpId::new(2), 400, now)
            .unwrap()
        else {
            panic!("reserve 2 granted")
        };
        store.cost_mark_dispatched(r2, now).unwrap();
        assert_eq!(free_micro(&store, s.id, tid), 600);
        let out = store.cost_refund(r2, now + 1).unwrap();
        assert_eq!(
            out,
            RefundOutcome::Blocked {
                current: "dispatched".into(),
                dispatched_ms: Some(now)
            },
            "the refund of a dispatched row is refused with its row truth"
        );
        let rows = store.cost_reservations_of(s.id, tid, 10).unwrap();
        assert_eq!(rows[0].status, "dispatched", "row untouched");
        assert_eq!(rows[0].dispatched_ms, Some(now));
        assert_eq!(free_micro(&store, s.id, tid), 600, "free unchanged");

        // Refund after SETTLE and after REFUND: typed refusals, untouched.
        store
            .cost_settle(r2, 400, Some(400), Some(390), None, now + 2)
            .unwrap();
        let out = store.cost_refund(r2, now + 3).unwrap();
        assert!(matches!(out, RefundOutcome::Blocked { current, .. } if current == "settled"));
        let CostReserveOutcome::Granted(r3) = store
            .cost_reserve(s.id, tid, OpId::new(3), 100, now)
            .unwrap()
        else {
            panic!("reserve 3 granted")
        };
        assert_eq!(
            store.cost_refund(r3, now + 1).unwrap(),
            RefundOutcome::Applied
        );
        let out = store.cost_refund(r3, now + 2).unwrap();
        assert!(matches!(out, RefundOutcome::Blocked { current, .. } if current == "refunded"));
        // Missing reservation.
        assert_eq!(
            store.cost_refund(99_999, now).unwrap(),
            RefundOutcome::Missing
        );
    }

    #[test]
    fn crash_recovery_splits_reserved_from_dispatched_and_uncertain_consumes_free() {
        // (iv) Crash windows: a never-dispatched reservation refunds and
        // restores free; a dispatched one (marker written) becomes UNCERTAIN
        // and KEEPS consuming free.
        let dir = tempfile::tempdir().unwrap();
        let (sid, tid) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
            let tid = task.task_id;
            store.cost_task_cap_set(s.id, tid, Some(1_000)).unwrap();
            let now = now_ms();
            // (a) reserved: dispatch never began.
            let CostReserveOutcome::Granted(_pre) = store
                .cost_reserve(s.id, tid, OpId::new(1), 200, now)
                .unwrap()
            else {
                panic!("pre-marker reserve")
            };
            // (b) dispatched: the request left the process.
            let CostReserveOutcome::Granted(post) = store
                .cost_reserve(s.id, tid, OpId::new(2), 300, now)
                .unwrap()
            else {
                panic!("post-marker reserve")
            };
            store.cost_mark_dispatched(post, now).unwrap();
            (s.id, tid)
        };
        let store = Store::open(dir.path(), true).unwrap();
        let (refunded, uncertain) = store.cost_recover_open_reservations(now_ms()).unwrap();
        assert_eq!(refunded, 1, "the reserved pre-marker row refunds");
        assert_eq!(uncertain, 1, "the dispatched row goes UNCERTAIN");
        let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
        let by_op = |op: u64| rows.iter().find(|r| r.op_id == OpId::new(op)).unwrap();
        assert_eq!(by_op(1).status, "refunded");
        assert_eq!(by_op(1).dispatched_ms, None);
        assert_eq!(by_op(2).status, "uncertain");
        assert!(by_op(2).dispatched_ms.is_some(), "the marker survives");
        assert_eq!(
            by_op(2).failure_reason_code.as_deref(),
            Some("crash_recovery_post_dispatch_marker")
        );
        assert_eq!(
            free_micro(&store, sid, tid),
            700,
            "the refunded prediction is free again; the uncertain hold (300) consumes"
        );
        // Idempotent recovery.
        let (refunded, uncertain) = store.cost_recover_open_reservations(now_ms()).unwrap();
        assert_eq!((refunded, uncertain), (0, 0));
    }

    #[test]
    fn settle_records_an_honest_cost_basis_and_amounts() {
        // (vi) Settlement writes provider_reported_cost_micro /
        // estimated_cost_micro / settled_cost_micro / cost_basis so the
        // ledger reports HOW the settled number was arrived at:
        // ProviderReported when the provider billed, RouteSnapshotEstimate
        // when categories x the frozen snapshot won, Unknown when nothing
        // was folded.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        let now = now_ms();
        // Provider-reported wins: basis ProviderReported.
        let CostReserveOutcome::Granted(r1) = store
            .cost_reserve_priced(
                s.id,
                tid,
                OpId::new(1),
                100_000,
                now,
                Some(&known_snapshot_json()),
            )
            .unwrap()
        else {
            panic!("reserve r1")
        };
        store.cost_mark_dispatched(r1, now).unwrap();
        store
            .cost_settle_usage(r1, 100_000, 0, 0, 2_000, Some(9_999), None, now + 1)
            .unwrap();
        let row = store.cost_reservations_of(s.id, tid, 10).unwrap()[0].clone();
        assert_eq!(row.status, "settled");
        assert_eq!(row.provider_reported_cost_micro, Some(9_999));
        assert_eq!(row.provider_reported_micro, Some(9_999));
        assert_eq!(
            row.provider_cost_micro,
            Some(1_620_000),
            "local 100k@15+2k@60"
        );
        assert_eq!(row.settled_cost_micro, Some(9_999), "the folded amount");
        assert_eq!(row.cost_basis.as_deref(), Some("ProviderReported"));
        assert_eq!(row.estimated_cost_micro, Some(100_000));
        assert_eq!(row.delivery_state.as_deref(), Some("completed"));
        assert_eq!(
            store
                .cost_task_row(s.id, tid)
                .unwrap()
                .unwrap()
                .spent_cost_micro,
            9_999
        );

        // No provider report: categories x the frozen snapshot, basis
        // RouteSnapshotEstimate.
        let CostReserveOutcome::Granted(r2) = store
            .cost_reserve_priced(
                s.id,
                tid,
                OpId::new(2),
                2_000_000,
                now,
                Some(&known_snapshot_json()),
            )
            .unwrap()
        else {
            panic!("reserve r2")
        };
        store.cost_mark_dispatched(r2, now).unwrap();
        store
            .cost_settle_usage(r2, 100_000, 0, 0, 2_000, None, None, now + 1)
            .unwrap();
        let row2 = store.cost_reservations_of(s.id, tid, 10).unwrap()[0].clone();
        assert_eq!(row2.cost_basis.as_deref(), Some("RouteSnapshotEstimate"));
        assert_eq!(row2.settled_cost_micro, Some(1_620_000));
        assert_eq!(row2.provider_reported_cost_micro, None);

        // No price authority + no cap: documented Unknown spend, basis
        // Unknown, nothing folded.
        let CostReserveOutcome::Granted(r3) = store
            .cost_reserve(s.id, tid, OpId::new(3), 100, now)
            .unwrap()
        else {
            panic!("reserve r3")
        };
        store.cost_mark_dispatched(r3, now).unwrap();
        let settled = store
            .cost_settle_usage(r3, 1_000, 0, 0, 2_000, None, None, now + 1)
            .unwrap();
        assert!(matches!(settled, CostSettleOutcome::AppliedUnknown));
        let row3 = store.cost_reservations_of(s.id, tid, 10).unwrap()[0].clone();
        assert_eq!(row3.cost_basis.as_deref(), Some("Unknown"));
        assert_eq!(row3.settled_cost_micro, None);
        assert_eq!(row3.estimated_cost_micro, Some(100));
    }

    #[test]
    fn reconcile_settles_each_uncertain_attempt_from_its_own_provider_row() {
        // (vii) Two dispatched attempts of ONE logical op crash UNCERTAIN;
        // each attempt's own completed provider-call row settles ITS OWN
        // reservation — never the sibling's — even when the sibling's row is
        // the newest completed row the old op_id join would have picked.
        let dir = tempfile::tempdir().unwrap();
        let (sid, tid, r1, r2, a1, a2) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
            let tid = task.task_id;
            store
                .cost_task_cap_set(s.id, tid, Some(10_000_000))
                .unwrap();
            let now = now_ms();
            let logical = OpId::new(800);
            let a1 = faktor_core::op::ModelCallAttempt::new(logical, OpId::new(801), 0).unwrap();
            let a2 = faktor_core::op::ModelCallAttempt::new(logical, OpId::new(802), 1).unwrap();
            let CostReserveOutcome::Granted(r1) = store
                .cost_reserve_attempt(s.id, tid, &a1, 50_000, now, Some(&known_snapshot_json()))
                .unwrap()
            else {
                panic!("reserve a1")
            };
            let CostReserveOutcome::Granted(r2) = store
                .cost_reserve_attempt(s.id, tid, &a2, 50_000, now, Some(&known_snapshot_json()))
                .unwrap()
            else {
                panic!("reserve a2")
            };
            store.cost_mark_dispatched(r1, now).unwrap();
            store.cost_mark_dispatched(r2, now).unwrap();
            // Crash: both dispatched rows never settled.
            (s.id, tid, r1, r2, a1, a2)
        };
        let store = Store::open(dir.path(), true).unwrap();
        let (refunded, uncertain) = store.cost_recover_open_reservations(now_ms()).unwrap();
        assert_eq!((refunded, uncertain), (0, 2));
        // The resumed logical op completes BOTH attempts' provider rows, but
        // attempt 2's row lands FIRST and attempt 1's row is the newest
        // completed row overall: the old "latest completed row of the op"
        // join would have settled attempt 2's reservation from attempt 1's
        // tokens. The attempt join must not.
        store
            .record_provider_call_attempt(
                sid,
                &a2,
                Some(r2),
                "fake",
                "m",
                "completed",
                Some(1_000),
                Some(100),
                None,
            )
            .unwrap();
        store
            .record_provider_call_attempt(
                sid,
                &a1,
                Some(r1),
                "fake",
                "m",
                "completed",
                Some(4_000),
                Some(2_000),
                None,
            )
            .unwrap();
        let report = store.cost_reconcile_uncertain(sid, tid, now_ms()).unwrap();
        assert_eq!(report.settled, 2);
        // attempt 1: 4_000 @15 + 2_000 @60 = 60_000 + 120_000 = 180_000.
        // attempt 2: 1_000 @15 + 100 @60 = 15_000 + 6_000 = 21_000.
        assert_eq!(report.charged_micro, 180_000 + 21_000);
        let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
        let row1 = rows.iter().find(|r| r.reservation_id == r1).unwrap();
        assert_eq!(row1.provider_cost_micro, Some(180_000), "a1's own tokens");
        let row2 = rows.iter().find(|r| r.reservation_id == r2).unwrap();
        assert_eq!(
            row2.provider_cost_micro,
            Some(21_000),
            "a2 settles from a2's row, never the sibling's newest row"
        );
        assert_eq!(row1.cost_basis.as_deref(), Some("RouteSnapshotEstimate"));
        assert_eq!(row1.settled_cost_micro, Some(180_000));
        // Idempotent: a second pass settles nothing.
        let report = store.cost_reconcile_uncertain(sid, tid, now_ms()).unwrap();
        assert_eq!(report, CostReconcileReport::default());
    }

    // ------------------------------------------------- model outcome stats
    // (migration v18 / schema target 19, audit items 13/14/L)

    pub(crate) fn outcome_sample(
        verified_success: bool,
        rework_cost: u64,
        rework_turns: u64,
    ) -> ModelOutcomeSample {
        ModelOutcomeSample {
            verified_success,
            rework_cost_micro: rework_cost,
            rework_turns,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn assert_row(
        row: &ModelOutcomeStatsRow,
        provider: &str,
        model: &str,
        phase: RouterPhase,
        task_class: TaskClass,
        risk_bucket: RiskBucket,
        successes: u64,
        failures: u64,
        cost: u64,
        turns: u64,
    ) {
        assert_eq!(row.provider, provider);
        assert_eq!(row.model, model);
        assert_eq!(row.phase, phase);
        assert_eq!(row.task_class, task_class);
        assert_eq!(row.risk_bucket, risk_bucket);
        assert_eq!(row.successes_first_pass, successes);
        assert_eq!(row.failures_first_pass, failures);
        assert_eq!(row.rework_cost_micro_sum, cost);
        assert_eq!(row.rework_turns_sum, turns);
        assert_eq!(row.sample_count, successes + failures);
    }

    #[test]
    fn model_outcome_stats_survive_reopen_and_fold_per_phase() {
        // Append facts + projection: exact-key reads stay exact, the phase
        // fold sums every class/risk bucket, and a drop/reopen returns
        // byte-identical rows (durable stats surviving reopen).
        let dir = tempfile::tempdir().unwrap();
        let (s_id, t_id, phase) = {
            let store = Store::open(dir.path().join("store"), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let tid = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running).task_id;
            // Medium/Low: 55 clean + 45 failing (2.4M cost / 3 turns each).
            for _ in 0..55 {
                store
                    .model_outcome_stats_append(
                        "cheap",
                        "m1",
                        RouterPhase::Implement,
                        TaskClass::Medium,
                        RiskBucket::Low,
                        outcome_sample(true, 0, 0),
                    )
                    .unwrap();
            }
            for _ in 0..45 {
                store
                    .model_outcome_stats_append(
                        "cheap",
                        "m1",
                        RouterPhase::Implement,
                        TaskClass::Medium,
                        RiskBucket::Low,
                        outcome_sample(false, 2_400_000, 3),
                    )
                    .unwrap();
            }
            // Hard/High: the same totals again, exercising the fold.
            for _ in 0..55 {
                store
                    .model_outcome_stats_append(
                        "cheap",
                        "m1",
                        RouterPhase::Implement,
                        TaskClass::Hard,
                        RiskBucket::High,
                        outcome_sample(true, 0, 0),
                    )
                    .unwrap();
            }
            for _ in 0..45 {
                store
                    .model_outcome_stats_append(
                        "cheap",
                        "m1",
                        RouterPhase::Implement,
                        TaskClass::Hard,
                        RiskBucket::High,
                        outcome_sample(false, 2_400_000, 3),
                    )
                    .unwrap();
            }
            // Strong model + a different phase: must NOT leak into folds.
            for _ in 0..94 {
                store
                    .model_outcome_stats_append(
                        "strong",
                        "m2",
                        RouterPhase::Implement,
                        TaskClass::Medium,
                        RiskBucket::Low,
                        outcome_sample(true, 0, 0),
                    )
                    .unwrap();
            }
            for _ in 0..6 {
                store
                    .model_outcome_stats_append(
                        "strong",
                        "m2",
                        RouterPhase::Implement,
                        TaskClass::Medium,
                        RiskBucket::Low,
                        outcome_sample(false, 960_000, 2),
                    )
                    .unwrap();
            }
            for _ in 0..3 {
                store
                    .model_outcome_stats_append(
                        "strong",
                        "m2",
                        RouterPhase::Review,
                        TaskClass::Easy,
                        RiskBucket::Low,
                        outcome_sample(true, 0, 0),
                    )
                    .unwrap();
            }
            let exact = store
                .model_outcome_stats_get(
                    "cheap",
                    "m1",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                )
                .unwrap()
                .unwrap();
            assert_row(
                &exact,
                "cheap",
                "m1",
                RouterPhase::Implement,
                TaskClass::Medium,
                RiskBucket::Low,
                55,
                45,
                45 * 2_400_000,
                45 * 3,
            );
            let fold = store
                .model_outcome_stats_phase("cheap", "m1", RouterPhase::Implement)
                .unwrap()
                .unwrap();
            assert_eq!(fold.provider, "cheap");
            assert_eq!(fold.model, "m1");
            assert_eq!(fold.phase, RouterPhase::Implement);
            assert_eq!(fold.successes_first_pass, 110);
            assert_eq!(fold.failures_first_pass, 90);
            assert_eq!(fold.rework_cost_micro_sum, 90 * 2_400_000);
            assert_eq!(fold.rework_turns_sum, 90 * 3);
            assert_eq!(fold.sample_count, 200);
            // The fold's class/risk columns are the first folded row's (the
            // PK text order is deterministic); only the sums are meaningful.
            assert!(matches!(
                fold.task_class,
                TaskClass::Medium | TaskClass::Hard
            ));
            assert!(matches!(
                fold.risk_bucket,
                RiskBucket::Low | RiskBucket::High
            ));
            let strong = store
                .model_outcome_stats_phase("strong", "m2", RouterPhase::Implement)
                .unwrap()
                .unwrap();
            assert_row(
                &strong,
                "strong",
                "m2",
                RouterPhase::Implement,
                TaskClass::Medium,
                RiskBucket::Low,
                94,
                6,
                6 * 960_000,
                6 * 2,
            );
            // The Review samples never leak into the Implement fold.
            assert!(store
                .model_outcome_stats_phase("strong", "m2", RouterPhase::Review)
                .unwrap()
                .is_some());
            (s.id, tid, RouterPhase::Implement)
        };
        // Reopen: migrations are a no-op (only the v18 block could replay,
        // and CREATE IF NOT EXISTS keeps rows), every row reads identical.
        let store = Store::open(dir.path().join("store"), true).unwrap();
        let v: i64 = {
            let conn = store.read().unwrap();
            conn.query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            v, 29,
            "schema target 29 after the v28 per-session artifact rebuild"
        );
        let fold = store
            .model_outcome_stats_phase("cheap", "m1", phase)
            .unwrap()
            .unwrap();
        assert_eq!(fold.successes_first_pass, 110);
        assert_eq!(fold.failures_first_pass, 90);
        assert_eq!(fold.rework_cost_micro_sum, 90 * 2_400_000);
        assert_eq!(fold.rework_turns_sum, 90 * 3);
        assert_eq!(fold.sample_count, 200);
        let exact = store
            .model_outcome_stats_get("strong", "m2", phase, TaskClass::Medium, RiskBucket::Low)
            .unwrap()
            .unwrap();
        assert_row(
            &exact,
            "strong",
            "m2",
            phase,
            TaskClass::Medium,
            RiskBucket::Low,
            94,
            6,
            6 * 960_000,
            6 * 2,
        );
        assert!(store
            .model_outcome_stats_get("nobody", "m", phase, TaskClass::Medium, RiskBucket::Low,)
            .unwrap()
            .is_none());
        drop(store);
        // Rewind to the previous schema target: the v18 block replays and
        // existing rows SURVIVE (CREATE IF NOT EXISTS is idempotent), while
        // the v19 column must be dropped first because its ADDITIVE ALTER is
        // not idempotent by design (the full migration chain owns the
        // column's existence).
        {
            let conn = rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db"))
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain (past v20)
            // replays cleanly.
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
                [],
            )
            .unwrap();
            conn.execute(
                "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
                [],
            )
            .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            conn.execute("PRAGMA user_version = 18", []).unwrap();
        }
        let store = Store::open(dir.path().join("store"), true).unwrap();
        let fold = store
            .model_outcome_stats_phase("cheap", "m1", phase)
            .unwrap()
            .unwrap();
        assert_eq!(fold.successes_first_pass, 110);
        assert_eq!(fold.failures_first_pass, 90);
        assert_eq!(fold.rework_cost_micro_sum, 90 * 2_400_000);
        assert_eq!(fold.rework_turns_sum, 90 * 3);
        assert_eq!(fold.sample_count, 200);
        assert_eq!(store.get_session(s_id).unwrap().unwrap().id, s_id);
        assert_eq!(store.get_task(s_id, t_id).unwrap().unwrap().task_id, t_id);
    }

    #[test]
    fn model_outcome_stats_race_writers_never_lose_a_sample() {
        // N threads hammering the SAME key through the shared store: the
        // single writer owner serializes the transactional read-modify-write,
        // so every sample lands exactly once — no lost updates, no broken
        // invariant (adversarial duplicate-replay shape: each thread is a
        // distinct "caller" and 10 identical appends must count 10).
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::open(dir.path(), true).unwrap());
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..25u64 {
                    let ok = (t + i) % 3 != 0;
                    store
                        .model_outcome_stats_append(
                            "p",
                            "m",
                            RouterPhase::Implement,
                            TaskClass::Hard,
                            RiskBucket::High,
                            outcome_sample(ok, if ok { 0 } else { 1_000 }, 1),
                        )
                        .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let row = store
            .model_outcome_stats_get(
                "p",
                "m",
                RouterPhase::Implement,
                TaskClass::Hard,
                RiskBucket::High,
            )
            .unwrap()
            .unwrap();
        assert_eq!(row.sample_count, 8 * 25, "every appended sample lands once");
        assert_eq!(
            row.successes_first_pass + row.failures_first_pass,
            row.sample_count
        );
        // Cross-thread writes never bleed into another key of the same
        // phase fold.
        let fold = store
            .model_outcome_stats_phase("p", "m", RouterPhase::Implement)
            .unwrap()
            .unwrap();
        assert_eq!(fold.sample_count, 8 * 25);
        assert_eq!(fold.failures_first_pass, row.failures_first_pass);
    }

    #[test]
    fn model_outcome_stats_success_samples_never_carry_rework_and_hostile_rows_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        // A verified first-pass success can NEVER carry rework, even when a
        // hostile caller hands hostile magnitudes (u64::MAX clamps at the
        // SQLite INTEGER ceiling instead of overflowing or wrapping).
        store
            .model_outcome_stats_append(
                "p",
                "m",
                RouterPhase::Implement,
                TaskClass::Easy,
                RiskBucket::Low,
                outcome_sample(true, u64::MAX, u64::MAX),
            )
            .unwrap();
        store
            .model_outcome_stats_append(
                "p",
                "m",
                RouterPhase::Implement,
                TaskClass::Easy,
                RiskBucket::Low,
                outcome_sample(false, u64::MAX, u64::MAX),
            )
            .unwrap();
        let row = store
            .model_outcome_stats_get(
                "p",
                "m",
                RouterPhase::Implement,
                TaskClass::Easy,
                RiskBucket::Low,
            )
            .unwrap()
            .unwrap();
        assert_eq!(row.successes_first_pass, 1);
        assert_eq!(row.failures_first_pass, 1);
        assert_eq!(
            row.rework_cost_micro_sum,
            i64::MAX as u64,
            "failure rework clamps at i64::MAX"
        );
        assert_eq!(row.rework_turns_sum, i64::MAX as u64);
        // CHECKs hold the projection invariant at the SQL level: a direct
        // (API-bypassing) INSERT whose sample_count contradicts its
        // successes + failures is refused outright.
        let err = store
            .sql_execute(
                "INSERT INTO model_outcome_stats VALUES (
                    'bad1','m','\"implement\"','\"easy\"','\"low\"', 5, 0, 0, 0, 1, 1)",
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
        // Enum text corruption is refused by the typed readers whenever the
        // row stays addressable: corrupting a KEY dimension renames the row
        // out of every typed lookup (Ok(None), never a guessed enum), so
        // the remaining corrupt-shape attack is the arithmetic invariant —
        // hostile DDL can drop the CHECKs, but the read path still refuses
        // the invariant-broken row it smuggles in.
        store
            .sql_execute(
                "DROP TABLE model_outcome_stats;
                 CREATE TABLE model_outcome_stats (
                    provider TEXT NOT NULL, model TEXT NOT NULL,
                    phase TEXT NOT NULL, task_class TEXT NOT NULL,
                    risk_bucket TEXT NOT NULL,
                    successes_first_pass INTEGER NOT NULL,
                    failures_first_pass INTEGER NOT NULL,
                    rework_cost_micro_sum INTEGER NOT NULL,
                    rework_turns_sum INTEGER NOT NULL,
                    sample_count INTEGER NOT NULL,
                    updated_ms INTEGER NOT NULL,
                    PRIMARY KEY (provider, model, phase, task_class, risk_bucket)
                 ) WITHOUT ROWID;
                 INSERT INTO model_outcome_stats VALUES (
                    'bad3','m','\"implement\"','\"easy\"','\"low\"', 5, 0, 0, 0, 1, 1)",
            )
            .unwrap();
        match store.model_outcome_stats_get(
            "bad3",
            "m",
            RouterPhase::Implement,
            TaskClass::Easy,
            RiskBucket::Low,
        ) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("invariant-broken row must be Corrupt, got {other:?}"),
        }
        match store.model_outcome_stats_phase("bad3", "m", RouterPhase::Implement) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("the phase fold must refuse the same row, got {other:?}"),
        }
    }
}
