//! The durable monetary budget ledger (P0-6/12 + P0-1 settlement + P0-2 +
//! attempt-identity accounting): the single authority over how much a task's
//! MODEL CALLS may cost.
//!
//! The agent runtime used to keep a private in-memory spend map keyed by
//! session (`AgentDeps.budget_micro` + a `spent` HashMap): a second budget
//! authority that vanished on restart and never recorded WHY money moved.
//! This module replaces it with a durable ledger over the session store
//! (schema v18): one `cost_reservation` row per paid model call attempt,
//! settled against the task row's monetary columns in the same transaction.
//!
//! # Invariants (locked by adversarial tests in this file)
//!
//! - `reserve` is atomic: spent + in-flight predictions + unresolved
//!   UNCERTAIN holds > cap refuses with a typed `BudgetError::BudgetExceeded`
//!   and writes NOTHING (no row, no spend). `max_cost_micro` NULL/0 =
//!   unlimited. When the routing decision priced the call, its immutable
//!   [`faktor_core::model::PricingSnapshot`] is persisted on the row
//!   (`pricing_snapshot_json`) — settlement prices usage against the frozen
//!   route-time capture, never against a later catalog repricing.
//!   Attempt-identity rows are reserved through `reserve_attempt` with a
//!   fresh physical-attempt op id and their own `parent_op_id`/reservation
//!   row: two attempts of the same logical op hold two separate
//!   reservations that settle and refund independently.
//! - `mark_dispatched` moves the row RESERVED -> DISPATCHED and writes the
//!   durable `dispatched_ms` marker immediately BEFORE the provider request
//!   is sent (idempotent on an already-dispatched row). Crash recovery
//!   splits on it:
//!   - still RESERVED with the marker NULL (dispatch never provably began)
//!     -> REFUNDED: the provider was never contacted, safe to release;
//!   - DISPATCHED (the request was sent and the provider MAY have billed) ->
//!     UNCERTAIN: the reservation KEEPS consuming the reserved amount
//!     (`free = max - spent - reserved - dispatched - uncertain`) until a
//!     reconcile settles it from the attempt's durable provider-call rows or
//!     the task-completion finalize charges the reserved estimate. The old
//!     v15 rule (OPEN -> ABANDONED, charged $0) undercounted spend when a
//!     crash hit between provider billing and the local settle; `abandoned`
//!     no longer exists.
//! - `refund` is pre-dispatch-only and SQL-enforced: the store's guarded
//!   UPDATE (`status IN ('reserved','open') AND dispatched_ms IS NULL`)
//!   changes ZERO rows on a dispatched/settled/refunded/uncertain
//!   reservation, and the ledger surfaces that as the typed
//!   `BudgetError::CannotRefundDispatched` — money is freed ONLY by the
//!   guarded statement, so even a caller that mis-calls refund after
//!   dispatch (the current agent runtime's post-dispatch error paths) can
//!   never release a reservation the provider may have billed.
//! - `mark_uncertain` records a post-dispatch failure (reason code +
//!   provider request id) and moves DISPATCHED -> UNCERTAIN: the row keeps
//!   consuming until reconcile/finalize closes it — never a silent $0.
//! - `settle_usage` closes a RESERVED/DISPATCHED reservation exactly once
//!   (-> SETTLED) and adds the CHOSEN actual to `task.spent_cost_micro` in
//!   ONE transaction. The chosen actual is the provider-reported cost when
//!   the usage frame carried one (authoritative), else the usage token
//!   categories (uncached input / cache reads / cache writes / output —
//!   reasoning billed at the output line) x the reservation's STORED
//!   snapshot. There is NO `tokens x 1 microUSD` local-price fallback
//!   anywhere: no price authority (no snapshot, or an Unknown-source
//!   snapshot) plus no provider-reported cost under a hard task cap is a
//!   typed [`BudgetError::UnknownPrice`] refusal (NOTHING written — the row
//!   stays RESERVED and recovery/finalize resolve it conservatively);
//!   without a cap the row settles as a documented Unknown spend (amount
//!   columns NULL, nothing folded — never a fabricated zero or one). Both
//!   the locally calculated and the provider-reported amounts are recorded
//!   on the row together with the honest `cost_basis`
//!   (`ProviderReported` | `RouteSnapshotEstimate` | `ConservativeReservation`
//!   | `Unknown`) and the amount actually folded (`settled_cost_micro`).
//!   A settlement that would push spent past the cap is recorded honestly —
//!   the money WAS spent; the NEXT reservation is what refuses.
//! - A second settle / a settle after refund is a typed `NotOpen` error
//!   (exactly-once semantics per reservation).
//! - `reconcile_uncertain` settles every UNCERTAIN reservation of one task
//!   whose ATTEMPT has a completed durable `provider_call` row FROM that
//!   row (attempt-keyed join; legacy rows join their op id as before): the
//!   completed call's tokens at the reservation's stored snapshot. A later
//!   settle for the same attempt settles the crashed attempt (each
//!   dispatched attempt may have been billed) — and two attempts of one
//!   logical op can never settle each other's rows. Rows without a
//!   completed row of their own attempt — or unpriced under a hard cap —
//!   stay UNCERTAIN.
//! - `finalize_uncertain` is the conservative task-completion backstop:
//!   every still-UNCERTAIN reservation of the task settles AT ITS RESERVED
//!   ESTIMATE (`predicted_micro`, basis `ConservativeReservation`) exactly
//!   once. Idempotent.
//! - Crash recovery ([`DurableBudgetLedger::recover_after_restart`]) is
//!   idempotent: a second run finds nothing RESERVED/DISPATCHED.
//! - Reservation ids are SQLite AUTOINCREMENT row ids: monotonic across
//!   daemon restarts, never reused.
//!
//! The trait ([`BudgetAuthority`]) is what the runtime consumes; tests that
//! never set a cap inject [`NoopBudget`]. The task row's monetary columns
//! are READ-ONLY through the task machine (upsert_task never writes them);
//! this ledger is their only writer.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use faktor_core::id::{OpId, SessionId, TaskId};
use faktor_core::model::{ExecutionClass, PricingSnapshot};
use faktor_core::op::ModelCallAttempt;
use faktor_core::state::{ChangeBudget, MAX_CHANGE_BUDGET_ENTRIES, MAX_CHANGE_BUDGET_ENTRY_CHARS};
use serde::{Deserialize, Serialize};
use tokio::task::JoinError;

use crate::manager::SessionManager;
use crate::SessionError;
/// Bound on the routing decision JSON stored on a reservation row
/// (bounded everything: an audit string is never stored unbounded).
pub const MAX_ROUTE_DECISION_JSON_BYTES: usize = 8192;

/// Bound on the pricing-snapshot JSON persisted at reserve time (the
/// snapshot is four u64 price lines + a source tag — far smaller — but a
/// hostile capture must never grow the row without limit).
pub const MAX_PRICING_SNAPSHOT_JSON_BYTES: usize = 1024;

/// One reservation's durable id (the store row's AUTOINCREMENT id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReservationId(i64);

impl ReservationId {
    /// The NOOP ledger's canonical id (never a store row).
    pub const NOOP: ReservationId = ReservationId(0);

    pub fn new(raw: i64) -> Self {
        Self(raw)
    }

    pub fn raw(&self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for ReservationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Bound on one terminal-failure reason code persisted by `mark_uncertain`
/// (bounded everything: a hostile reason string never grows a row without
/// limit).
pub const MAX_FAILURE_REASON_CODE_BYTES: usize = 512;

/// Bound on one provider request id persisted by `mark_uncertain` /
/// `settle_usage` (a provider request id is a short opaque string).
pub const MAX_REQUEST_ID_BYTES: usize = 256;

/// The typed durable state of one reservation (schema v18 vocabulary:
/// `reserved`, `dispatched`, `settled`, `refunded`, `uncertain`). Mirrors
/// the store's row status strings so ledger consumers can match states
/// without string literals; unknown statuses (hostile rows) map to `None`
/// loudly, never to a guessed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationState {
    /// Reserved: holds budget, dispatch never provably began. Refundable.
    Reserved,
    /// Dispatched: the durable marker was written — the provider request
    /// left the process and MAY have billed. Never refundable.
    Dispatched,
    /// Settled: closed at an actual (or a documented Unknown spend).
    Settled,
    /// Refunded: released pre-dispatch.
    Refunded,
    /// Uncertain: a dispatched attempt a crash or terminal failure left
    /// unsettled — keeps consuming free until reconcile/finalize.
    Uncertain,
}

impl ReservationState {
    /// The schema-v18 status string of this state.
    pub const fn as_db_status(self) -> &'static str {
        match self {
            ReservationState::Reserved => "reserved",
            ReservationState::Dispatched => "dispatched",
            ReservationState::Settled => "settled",
            ReservationState::Refunded => "refunded",
            ReservationState::Uncertain => "uncertain",
        }
    }
    /// Parse a store status string. `None` for anything outside the frozen
    /// vocabulary (legacy `open`/`abandoned` cannot exist on a v18 store and
    /// are deliberately NOT mapped to a guessed state).
    pub fn from_db_status(status: &str) -> Option<Self> {
        match status {
            "reserved" => Some(ReservationState::Reserved),
            "dispatched" => Some(ReservationState::Dispatched),
            "settled" => Some(ReservationState::Settled),
            "refunded" => Some(ReservationState::Refunded),
            "uncertain" => Some(ReservationState::Uncertain),
            _ => None,
        }
    }
}

/// What a FAILED budget read learned about the task's hard cost cap before
/// the failure. The distinction is load-bearing policy: a hard-cap (or
/// unknown) read failure must refuse a paid provider call, while a task
/// whose row was read and explicitly carries no cap may proceed with
/// accounting-unavailable telemetry — never as a silently-free budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCapEvidence {
    /// The task row (or its cap) could not be read: no cap evidence exists,
    /// so callers MUST fail closed.
    Unknown,
    /// The task row was read and explicitly carries NO cap (unlimited).
    Uncapped,
    /// The task row was read and carries this hard cap.
    HardCap(u64),
}

/// Typed refusal of a ledger operation. Every variant is machine-readable;
/// prose-only errors are rejected in review.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error(
        "budget exceeded: reserving {predicted} micro would push spend past the task cap (free: {free})"
    )]
    BudgetExceeded { free: u64, predicted: u64 },
    #[error(
        "cap reduction refused: a new cap of {new_cap:?} micro would sit below the {committed} micro \
         already committed to this task (settled + open + uncertain reservations); spend never rewinds \
         — raise the cap first"
    )]
    CapBelowCommitted {
        new_cap: Option<u64>,
        committed: u64,
    },
    #[error("session {0} is not an orchestrated child (no identity row): a child scope needs a parent root")]
    NotAnOrchestratedChild(SessionId),
    #[error("reservation {0} does not exist")]
    UnknownReservation(i64),
    #[error("reservation {reservation} is {status}, not reserved/dispatched: exactly-once per reservation")]
    NotOpen { reservation: i64, status: String },
    #[error(
        "reservation {reservation} was already dispatched (or carries its dispatch marker): \
         a post-dispatch refund is impossible — the provider may have billed. The row stays \
         dispatched/uncertain/settled and its reserved amount keeps consuming the free budget \
         until a settlement, reconcile or the task-end finalize closes it; nothing was written"
    )]
    CannotRefundDispatched { reservation: i64 },
    #[error(
        "reservation {reservation} has no price authority (no snapshot or an Unknown price source) \
         and no provider-reported cost, under a hard task cap: settlement is refused — nothing was \
         written (recovery/finalize resolve the row conservatively)"
    )]
    UnknownPrice { reservation: i64 },
    #[error("task {task_id} of session {session_id} has no row (the task machine owns creation)")]
    MissingTask {
        session_id: SessionId,
        task_id: TaskId,
    },
    #[error(
        "task {session_id}/{task_id} is in a completion/final state: a new provider operation is \
         refused — nothing was written. The task's accounting is being closed; reservations may \
         only be created while the task permits new provider work"
    )]
    TaskStateForbidsReserve {
        session_id: SessionId,
        task_id: TaskId,
    },
    #[error("budget accounting unavailable ({cap:?}) — this is NOT an unlimited budget: {reason}")]
    Unavailable {
        cap: BudgetCapEvidence,
        reason: String,
    },
    #[error("malformed ledger input: {0}")]
    Malformed(String),
    #[error("store failure: {0}")]
    Store(String),
    #[error("ledger worker failed: {0}")]
    Worker(String),
}

impl BudgetError {
    /// True only when a failed budget read PROVED the task row explicitly
    /// carries no cap (the row was read, the reservations read then failed):
    /// only this case may proceed with accounting-unavailable telemetry.
    /// Every other failure — hard cap or no cap evidence at all — must fail
    /// closed (no paid provider call).
    pub fn read_failure_is_explicitly_uncapped(&self) -> bool {
        matches!(
            self,
            BudgetError::Unavailable {
                cap: BudgetCapEvidence::Uncapped,
                ..
            }
        )
    }
}

impl From<faktor_store::StoreError> for BudgetError {
    fn from(e: faktor_store::StoreError) -> Self {
        // The store's only Conflict on these paths is a missing task row.
        if let faktor_store::StoreError::Conflict(m) = &e {
            if m.contains("has no row") {
                return BudgetError::Malformed(m.clone());
            }
        }
        BudgetError::Store(e.to_string())
    }
}

impl From<JoinError> for BudgetError {
    fn from(e: JoinError) -> Self {
        BudgetError::Worker(e.to_string())
    }
}

impl From<BudgetError> for SessionError {
    fn from(e: BudgetError) -> Self {
        match e {
            BudgetError::BudgetExceeded { .. } => SessionError::Conflict(e.to_string()),
            BudgetError::CapBelowCommitted { .. } => SessionError::Conflict(e.to_string()),
            BudgetError::NotAnOrchestratedChild(_) => SessionError::Conflict(e.to_string()),
            BudgetError::UnknownReservation(_)
            | BudgetError::NotOpen { .. }
            | BudgetError::CannotRefundDispatched { .. }
            | BudgetError::UnknownPrice { .. }
            | BudgetError::TaskStateForbidsReserve { .. } => SessionError::Conflict(e.to_string()),
            BudgetError::MissingTask { .. } => SessionError::NotFound(e.to_string()),
            BudgetError::Unavailable { reason, .. } => SessionError::Internal(reason),
            BudgetError::Malformed(m) => SessionError::Malformed(m),
            BudgetError::Store(m) => SessionError::Store(faktor_store::StoreError::Conflict(m)),
            BudgetError::Worker(m) => SessionError::Internal(m),
        }
    }
}

impl From<BudgetError> for faktor_core::Error {
    fn from(e: BudgetError) -> Self {
        SessionError::from(e).into()
    }
}

// --------------------------------------------------------------- subscopes
// (max_cost_micro task control, additive — no store migration): a child
// scope is ONE durable enrollment row (a memory fact under the ROOT session's
// row space, beside the orchestrator's own root-scoped registry rows) naming
// a consumer CHILD task whose reservations must be admitted against BOTH its
// own task-row cap (the store enforces that atomically) and the remaining
// budget of its ROOT task row. Admission is process-serialized (the store is
// one SQLite writer owned by the one daemon process; every budget mutator
// passes through [`budget_admission_lock`]) so the root aggregate — the root
// row's own committed amounts plus every enrolled member's — can never be
// overshot by two racing reserves: parent $10 with children A $5 + B $5 can
// never collectively spend $15. Enrollment happens through
// [`DurableBudgetLedger::change_child_scope_cap`] (the typed cost axis of a
// child budget change); a child whose cap is cleared is tombstoned on its
// enrollment row (the memory-fact store has no delete surface) and stops
// consuming root budget.

/// Memory-fact kind of a budget-scope enrollment row (written under the ROOT
/// session's row space, key = the consumer session id).
pub const BUDGET_SCOPE_KIND: &str = "budget_scope";
/// Bound on one enrollment key/value (bounded everything: a hostile
/// registration can never grow a row without limit).
pub const MAX_SCOPE_CHILD_ID_CHARS: usize = 64;
/// Bound on the enrollment scan: at most this many memory-fact pages are
/// walked to enumerate a root's enrollments. A row space larger than that is
/// hostile/broken and refuses admission loudly — never a silently partial
/// root budget.
const MAX_SCOPE_FACT_PAGES: usize = 8;
/// Memory-fact page size of the enrollment scan.
const SCOPE_FACT_PAGE: i64 = 200;

/// The durable enrollment of one child task under a root task (subscope
/// admission). Written as one memory-fact row under the ROOT session; the
/// authoritative child cap stays on the child's own task row (this row's
/// `child_cap_micro` is the enrollment mirror that decides membership: a
/// `None` mirror is the tombstone of a cleared cap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetScope {
    /// The root session whose task row owns the run's money.
    pub root_session_id: SessionId,
    /// The root session's task row (the run's task row cap).
    pub root_task_id: TaskId,
    /// The consuming (child) session.
    pub consumer_session_id: SessionId,
    /// The consuming session's task row.
    pub consumer_task_id: TaskId,
    /// Orchestrator child id (informational, bounded).
    pub child_id: String,
    /// Enrollment mirror of the child's durable task-row cap; `None` =
    /// cleared (the row is a tombstone and stops consuming root budget).
    pub child_cap_micro: Option<u64>,
    pub created_ms: i64,
}

/// The process-wide admission gate of the budget ledger. The store is one
/// SQLite writer inside the ONE daemon process (no two processes share a
/// store), so a single in-process mutex serializes every mutating admission:
/// the root-subscope free-balance read and the reservation insert can never
/// interleave with a sibling's reserve or with a cap reduction — root
/// remaining is exact at every commit. It is never held across an await (all
/// lock holders are synchronous store calls inside `spawn_blocking`), so it
/// can never stall a runtime worker.
pub(crate) fn budget_admission_lock() -> &'static std::sync::Mutex<()> {
    static ADMISSION: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    ADMISSION.get_or_init(|| std::sync::Mutex::new(()))
}

/// The task's durable monetary picture at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetView {
    /// The task's cap in microUSD; `None` = unlimited.
    pub max_cost_micro: Option<u64>,
    /// Durable settled spend (the task row's `spent_cost_micro`).
    pub spent_cost_micro: u64,
    /// Sum of the predicted micro of every in-flight reservation (schema
    /// v18 vocabulary: `reserved` — dispatch never began — plus
    /// `dispatched` — the request left the process and may bill).
    pub open_reserved_micro: u64,
    /// Count of in-flight reservations (reserved + dispatched).
    pub open_reservations: usize,
    /// Sum of the predicted micro of every UNCERTAIN reservation (a crashed
    /// daemon may have dispatched them; unresolved by reconcile/finalize).
    /// These KEEP consuming the reserved amount: the provider may have
    /// billed the attempt, so its prediction is never silently released.
    pub uncertain_reserved_micro: u64,
    /// Count of UNCERTAIN reservations.
    pub uncertain_reservations: usize,
    /// Count of SETTLED reservations.
    pub settled_count: usize,
}

impl BudgetView {
    /// Free balance: cap - spent - in-flight predictions (reserved +
    /// dispatched) - unresolved UNCERTAIN holds (saturating; the ledger
    /// refuses reservations that would cross it).
    pub fn free(&self) -> u64 {
        match self.max_cost_micro {
            None => u64::MAX,
            Some(cap) => cap
                .saturating_sub(self.spent_cost_micro)
                .saturating_sub(self.open_reserved_micro)
                .saturating_sub(self.uncertain_reserved_micro),
        }
    }
}

/// The UI-facing outcome of a budget read: the honest durable picture
/// (`Known`) or a typed unavailability with its reason (`Unavailable`).
/// There is deliberately NO synthesized "unlimited, zero spend" state — a
/// failed read is never represented as free budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetState {
    /// The durable picture was read completely.
    Known(BudgetView),
    /// The accounting could not be read; the reason is surfaced verbatim
    /// (never silently replaced by an unlimited/zero view).
    Unavailable { reason: String },
}

impl BudgetState {
    /// True when the durable picture was read.
    pub fn is_known(&self) -> bool {
        matches!(self, BudgetState::Known(_))
    }

    /// The known view, when the read succeeded.
    pub fn known(&self) -> Option<&BudgetView> {
        match self {
            BudgetState::Known(view) => Some(view),
            BudgetState::Unavailable { .. } => None,
        }
    }

    /// The failure reason, when the read failed.
    pub fn reason(&self) -> Option<&str> {
        match self {
            BudgetState::Known(_) => None,
            BudgetState::Unavailable { reason } => Some(reason),
        }
    }
}

impl From<Result<BudgetView, BudgetError>> for BudgetState {
    fn from(read: Result<BudgetView, BudgetError>) -> Self {
        match read {
            Ok(view) => BudgetState::Known(view),
            Err(e) => BudgetState::Unavailable {
                reason: e.to_string(),
            },
        }
    }
}

/// The completion-time accounting picture of one task
/// ([`DurableBudgetLedger::completion_accounting_balance`]): the counts and
/// reserved-micro sums of every reservation that still holds budget. The
/// completion gate's invariant is that reconcile + conservative finalize
/// drive both OPEN (reserved/dispatched) and UNCERTAIN counts to ZERO
/// before a task may transition VerifiedComplete — a nonzero count here
/// refuses completion and the task STAYS Verifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskCompletionBalance {
    /// Reserved-but-never-dispatched rows (refundable pre-dispatch).
    pub open_count: usize,
    /// Reserved micro of every OPEN row.
    pub open_micro: u64,
    /// Of the open rows: the DISPATCHED ones (the durable marker was
    /// written; the provider may have billed — never refundable).
    pub dispatched_count: usize,
    /// UNCERTAIN rows (crashed/terminally-failed dispatched attempts).
    pub uncertain_count: usize,
    /// Reserved micro of every UNCERTAIN row.
    pub uncertain_micro: u64,
    /// The task row's durable settled spend.
    pub spent_cost_micro: u64,
}

impl TaskCompletionBalance {
    /// True when no reservation of the task still holds budget: zero OPEN
    /// and zero UNCERTAIN rows — the accounting-before-completion gate.
    pub fn is_zero(&self) -> bool {
        self.open_count == 0 && self.uncertain_count == 0
    }
}

pub(crate) type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The budget authority the agent runtime consumes before every paid model
/// call. Async operations carry explicit futures (no hidden spawns); every
/// implementation is `Send + Sync` and must make its own durability story.
pub trait BudgetAuthority: Send + Sync {
    /// Reserve `predicted_micro` of the task's budget for one operation.
    /// `pricing_snapshot` is the immutable route-time price capture the
    /// settlement math prices the call's usage against; `None` = no pricing
    /// authority was consulted (settlement then fails closed under a hard
    /// cap, or records a documented Unknown spend without one).
    /// `execution_class` is the route-time execution classification frozen
    /// beside the snapshot (`Local` only for an authoritative local-zero
    /// route; `Unknown` = no authority — never counted as local or hosted).
    /// Fails with typed `BudgetError::BudgetExceeded` when
    /// spent + in-flight + uncertain > cap (`None`/0 cap = unlimited); a
    /// refusal writes NOTHING.
    fn reserve(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        op_id: OpId,
        predicted_micro: u64,
        pricing_snapshot: Option<PricingSnapshot>,
        execution_class: ExecutionClass,
    ) -> BoxFut<'_, Result<ReservationId, BudgetError>>;

    /// Reserve the task budget for ONE PHYSICAL ATTEMPT (attempt
    /// accounting): the reservation keys by the attempt's fresh op id and
    /// records the shared logical parent op id, so two attempts of the same
    /// logical op hold two independent reservations that settle/refund
    /// independently and crash recovery reconciles each attempt against its
    /// OWN provider-call rows — never a sibling attempt's. Failures are
    /// identical to [`BudgetAuthority::reserve`] (a denial writes nothing).
    fn reserve_attempt(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt: ModelCallAttempt,
        predicted_micro: u64,
        pricing_snapshot: Option<PricingSnapshot>,
        execution_class: ExecutionClass,
    ) -> BoxFut<'_, Result<ReservationId, BudgetError>>;

    /// Write the durable dispatch state of one reservation (immediately
    /// BEFORE the provider request is sent): the row moves `reserved` ->
    /// `dispatched`, stamps the durable `dispatched_ms` marker and the
    /// delivery state in ONE statement. Crash recovery can then tell
    /// "dispatch never provably began" (still `reserved`, marker NULL ->
    /// refunded) from "the provider may have billed" (`dispatched` ->
    /// UNCERTAIN). A second mark of the same dispatched row is idempotent;
    /// anything settled/refunded/uncertain is a typed refusal.
    fn mark_dispatched(
        &self,
        session_id: SessionId,
        reservation: ReservationId,
    ) -> BoxFut<'_, Result<(), BudgetError>>;

    /// Record a POST-DISPATCH terminal failure (attempt accounting): the
    /// reservation moves `dispatched` -> UNCERTAIN with the failure reason
    /// code and the provider request id (when known) persisted durably —
    /// the provider may have billed, so the reserved amount KEEPS consuming
    /// the free budget until a reconcile settles it from the attempt's
    /// durable provider-call rows or the task-end finalize charges the
    /// estimate. Never a silent $0. A still-`reserved` row (dispatch never
    /// provably began) is a typed refusal — refund it instead.
    fn mark_uncertain(
        &self,
        session_id: SessionId,
        reservation: ReservationId,
        reason_code: String,
        request_id: Option<String>,
    ) -> BoxFut<'_, Result<(), BudgetError>>;

    /// Settle one reservation from a provider usage frame at the chosen
    /// actual: the provider-reported cost when the frame carried one
    /// (authoritative), else the token categories x the reservation's
    /// stored route-time [`PricingSnapshot`]. Both amounts and the routing
    /// decision's JSON are recorded on the row; the chosen actual is folded
    /// into the task's spent total in the same transaction. `Ok(Some(m))`
    /// = `m` microUSD was folded; `Ok(None)` = the row settled as a
    /// documented Unknown spend (no price authority, no hard cap — nothing
    /// folded, never a fabricated number). A price-less settle under a hard
    /// cap is the typed [`BudgetError::UnknownPrice`] refusal (nothing
    /// written; the row stays OPEN for recovery/finalize).
    #[allow(clippy::too_many_arguments)]
    fn settle_usage(
        &self,
        session_id: SessionId,
        reservation: ReservationId,
        uncached_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        provider_reported_micro: Option<u64>,
        route_decision_json: Option<String>,
    ) -> BoxFut<'_, Result<Option<u64>, BudgetError>>;

    /// Release one reservation WITHOUT spending (RESERVED with a NULL
    /// dispatch marker -> REFUNDED, exactly-once). The pre-dispatch guard
    /// is enforced AT THE SQL LEVEL: a dispatched, settled, refunded or
    /// uncertain reservation changes zero rows and surfaces here as the
    /// typed [`BudgetError::CannotRefundDispatched`] — money is freed only
    /// by the guarded UPDATE, so even a mis-called post-dispatch refund
    /// (the runtime's current post-dispatch error paths) can never release
    /// a reservation the provider may have billed.
    fn refund(
        &self,
        session_id: SessionId,
        reservation: ReservationId,
    ) -> BoxFut<'_, Result<(), BudgetError>>;

    /// The task's durable monetary picture (sync read; used by route
    /// requests for the remaining-budget axis and by diagnostics). A read
    /// that cannot produce the honest picture is a typed [`BudgetError`]
    /// (never a synthesized unlimited/zero view): callers under a hard cap
    /// MUST refuse the paid call, and an explicitly uncapped caller
    /// (`BudgetCapEvidence::Uncapped`) may proceed only with
    /// accounting-unavailable telemetry.
    fn session_budget_view(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<BudgetView, BudgetError>;

    /// Crash recovery of every reservation a crashed process left OPEN,
    /// split on the durable dispatch marker (marker NULL -> REFUNDED,
    /// marker set -> UNCERTAIN which keeps consuming). Idempotent.
    fn recover_after_restart(&self);

    /// Reconcile every UNCERTAIN reservation of one task whose op has a
    /// completed durable `provider_call` row: the completed call's tokens,
    /// priced at each reservation's stored snapshot, settle the crashed
    /// attempt (a later settle for the same op id settles it). Rows without
    /// a completed row — or unpriced under a hard cap — stay UNCERTAIN for
    /// the task-completion finalize. Idempotent.
    fn reconcile_uncertain(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> BoxFut<'_, Result<faktor_store::CostReconcileReport, BudgetError>>;

    /// Conservative task-completion finalize: every still-UNCERTAIN
    /// reservation of one task settles AT its reserved estimate (the
    /// provider may have billed for a dispatched attempt whose actual never
    /// reconciled). Called once at task completion; idempotent — a second
    /// pass finds nothing UNCERTAIN and never double-charges.
    fn finalize_uncertain(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> BoxFut<'_, Result<faktor_store::CostFinalizeReport, BudgetError>>;
}

/// The durable ledger: reservations + the task row's monetary columns, both
/// owned by this type over one [`SessionManager`]'s store. Cheap to build;
/// shares the manager's store and CAS.
pub struct DurableBudgetLedger {
    session: Arc<SessionManager>,
}

impl std::fmt::Debug for DurableBudgetLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableBudgetLedger")
            .finish_non_exhaustive()
    }
}

impl DurableBudgetLedger {
    pub fn new(session: Arc<SessionManager>) -> Arc<Self> {
        Arc::new(Self { session })
    }

    fn store(&self) -> Arc<faktor_store::Store> {
        self.session.store()
    }

    fn now_ms(&self) -> i64 {
        self.session.now_ms()
    }

    /// Set (or clear) the durable monetary cap of one task. `None` =
    /// unlimited (always legal: removing a cap can never strand committed
    /// money). Raising the cap is always legal; LOWERING it is guarded: the
    /// new cap must sit at or above the task's committed micro — settled
    /// spend plus the reserved predictions of every OPEN and UNCERTAIN row —
    /// or the typed [`BudgetError::CapBelowCommitted`] refuses and nothing
    /// is written (spend never rewinds; a reduction that would strand
    /// already-committed money is a contradiction, never a silent accept).
    /// When this task row is a ROOT of enrolled child scopes (see
    /// [`BudgetScope`]), the committed bound spans the root row AND every
    /// live enrolled child task, so the run's root cap can never be lowered
    /// underneath what its children already committed. The task row must
    /// exist (the task machine owns creation).
    pub fn set_task_max_cost(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        max_cost_micro: Option<u64>,
    ) -> Result<(), BudgetError> {
        let guard = budget_admission_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        self.set_task_max_cost_locked(session_id, task_id, max_cost_micro, true)?;
        drop(guard);
        Ok(())
    }

    /// The child cost-cap effect of a typed budget change (audit 9/H): the
    /// child's OWN durable task-row `max_cost_micro` is patched (the store
    /// enforces it atomically at every reservation) and — when the child is
    /// an orchestrated child under a root — its [`BudgetScope`] enrollment
    /// row is written under the ROOT session, enrolling the child in the
    /// root's subscope admission: from then on every reserve of this child
    /// is admitted against the child remaining AND the root remaining
    /// (root $10 with children A $5 + B $5 can never collectively spend
    /// $15). `child_id` is the orchestrator child id (informational;
    /// bounded). `None` clears the child's cap and tombstones the enrollment
    /// (a cleared child stops consuming root budget). Re-runs of the same
    /// change are idempotent upserts.
    pub fn change_child_scope_cap(
        &self,
        child_session: SessionId,
        child_id: &str,
        max_cost_micro: Option<u64>,
    ) -> Result<(), BudgetError> {
        if child_id.is_empty() || child_id.chars().count() > MAX_SCOPE_CHILD_ID_CHARS {
            return Err(BudgetError::Malformed(format!(
                "child id must be 1..={MAX_SCOPE_CHILD_ID_CHARS} characters"
            )));
        }
        let manager = self.session.clone();
        let child_handle = match manager.get_session(child_session) {
            Ok(Some(h)) => h,
            Ok(None) => {
                return Err(BudgetError::NotAnOrchestratedChild(child_session));
            }
            Err(e) => return Err(BudgetError::Store(e.to_string())),
        };
        let identity = match child_handle.orchestrator_child_identity_get() {
            Ok(Some(i)) => i,
            Ok(None) => return Err(BudgetError::NotAnOrchestratedChild(child_session)),
            Err(e) => return Err(BudgetError::Store(e.message)),
        };
        let root_session = identity.parent_session_id;
        let root_handle = match manager.get_session(root_session) {
            Ok(Some(h)) => h,
            Ok(None) => {
                return Err(BudgetError::Store(format!(
                    "the root session {root_session} of child {child_session} has no row"
                )));
            }
            Err(e) => return Err(BudgetError::Store(e.to_string())),
        };
        let root_task = root_handle
            .task_id()
            .map_err(|e| BudgetError::Store(e.message))?;
        let child_task = child_handle
            .task_id()
            .map_err(|e| BudgetError::Store(e.message))?;
        let guard = budget_admission_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // The child's own task row cap is the per-child enforcement point;
        // a child row missing yet (the task machine seeds rows lazily) is
        // seeded like the token-budget path does, never refused.
        seed_child_task_row_if_missing(&child_handle, child_session)?;
        self.set_task_max_cost_locked(child_session, child_task, max_cost_micro, false)?;
        // The enrollment row lives under the ROOT session's fact space (one
        // row per consumer; an upsert is idempotent). A tombstone row (cap
        // None) stops the child from consuming root budget.
        let created_ms = self.now_ms();
        let scope = BudgetScope {
            root_session_id: root_session,
            root_task_id: root_task,
            consumer_session_id: child_session,
            consumer_task_id: child_task,
            child_id: child_id.to_string(),
            child_cap_micro: max_cost_micro,
            created_ms,
        };
        let value = serde_json::to_string(&scope)
            .map_err(|e| BudgetError::Malformed(format!("budget scope serialization: {e}")))?;
        root_handle
            .upsert_memory_fact(BUDGET_SCOPE_KIND, &scope_key(child_session), &value)
            .map_err(|e| BudgetError::Store(e.message))?;
        drop(guard);
        Ok(())
    }

    /// The durable enrollment of `session_id` under its root, when one
    /// exists (read diagnostics/tests). `None` = the session is not an
    /// orchestrated child, has no root, or was never enrolled.
    pub fn scope_of(&self, session_id: SessionId) -> Result<Option<BudgetScope>, BudgetError> {
        let manager = self.session.clone();
        let Some(identity) = (match manager.get_session(session_id) {
            Ok(Some(h)) => h.orchestrator_child_identity_get(),
            Ok(None) => return Ok(None),
            Err(e) => return Err(BudgetError::Store(e.to_string())),
        })
        .map_err(|e| BudgetError::Store(e.message))?
        else {
            return Ok(None);
        };
        let enrollments = enrollments_of(&manager, identity.parent_session_id)?;
        Ok(enrollments
            .into_iter()
            .find(|s| s.consumer_session_id == session_id))
    }

    fn set_task_max_cost_locked(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        max_cost_micro: Option<u64>,
        include_members: bool,
    ) -> Result<(), BudgetError> {
        let manager = self.session.clone();
        let store = self.store();
        // Missing row = the store's existing typed refusal (the task
        // machine owns creation); a missing row has nothing committed, but
        // the update itself must not write through to a phantom.
        let committed = if store
            .cost_task_row(session_id, task_id)
            .map_err(BudgetError::from)?
            .is_some()
        {
            let mut committed = committed_of(&manager, session_id, task_id)?;
            if include_members {
                for scope in enrollments_of(&manager, session_id)? {
                    if scope.child_cap_micro.is_none() {
                        continue;
                    }
                    if consumer_is_live(&manager, scope.consumer_session_id)? {
                        committed = committed.saturating_add(committed_of(
                            &manager,
                            scope.consumer_session_id,
                            scope.consumer_task_id,
                        )?);
                    }
                }
            }
            committed
        } else {
            // The store refuses the write with the typed missing-row
            // conflict; nothing is guarded on a phantom row.
            0
        };
        if let Some(new_cap) = max_cost_micro {
            if new_cap < committed {
                return Err(BudgetError::CapBelowCommitted {
                    new_cap: Some(new_cap),
                    committed,
                });
            }
        }
        store
            .cost_task_cap_set(session_id, task_id, max_cost_micro)
            .map_err(|e| {
                if matches!(e, faktor_store::StoreError::Conflict(_)) {
                    BudgetError::MissingTask {
                        session_id,
                        task_id,
                    }
                } else {
                    BudgetError::Store(e.to_string())
                }
            })
    }

    /// Resolve every reservation a crashed process left OPEN (split on the
    /// durable dispatch marker, see [`BudgetAuthority::recover_after_restart`]).
    /// Inherent alias of the [`BudgetAuthority`] method so daemon boot code
    /// can call it without importing the trait. Idempotent.
    pub fn recover_after_restart(&self) {
        BudgetAuthority::recover_after_restart(self)
    }

    fn map_row_err(
        e: faktor_store::StoreError,
        session_id: SessionId,
        task_id: TaskId,
    ) -> BudgetError {
        match e {
            faktor_store::StoreError::Conflict(m)
                if m.contains("cost reserve: task") && m.contains("has no row") =>
            {
                BudgetError::MissingTask {
                    session_id,
                    task_id,
                }
            }
            faktor_store::StoreError::Conflict(m) if m.contains("cost reserve: task state") => {
                BudgetError::TaskStateForbidsReserve {
                    session_id,
                    task_id,
                }
            }
            other => BudgetError::Store(other.to_string()),
        }
    }
}

/// Future-returning helpers so the trait impls stay one-liners.
impl DurableBudgetLedger {
    async fn reserve_inner(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        op_id: OpId,
        predicted_micro: u64,
        pricing_snapshot: Option<PricingSnapshot>,
        execution_class: ExecutionClass,
    ) -> Result<ReservationId, BudgetError> {
        let store = self.store();
        let created_ms = self.now_ms();
        // Bounded: a pricing snapshot is four u64 lines + a source tag;
        // anything larger is malformed input, never stored.
        let snapshot_json = match pricing_snapshot {
            Some(s) => {
                let json = serde_json::to_string(&s)
                    .map_err(|e| BudgetError::Malformed(format!("pricing snapshot: {e}")))?;
                if json.len() > MAX_PRICING_SNAPSHOT_JSON_BYTES {
                    return Err(BudgetError::Malformed(format!(
                        "pricing snapshot JSON of {} bytes exceeds MAX_PRICING_SNAPSHOT_JSON_BYTES",
                        json.len()
                    )));
                }
                Some(json)
            }
            None => None,
        };
        // Admission + insert are ONE critical section (see
        // [`budget_admission_lock`]): the root-subscope free balance is read
        // and the reservation inserted under the same in-process gate, so
        // two racing child reserves can never jointly overshoot the root
        // cap (the child's OWN cap stays enforced atomically by the store
        // transaction itself). A root refusal writes NOTHING.
        let manager = self.session.clone();
        let out = tokio::task::spawn_blocking(move || {
            let _guard = budget_admission_lock()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(free) = consumer_root_free(&manager, session_id, task_id)? {
                if predicted_micro > free {
                    return Err(BudgetError::BudgetExceeded {
                        free,
                        predicted: predicted_micro,
                    });
                }
            }
            store
                .cost_reserve_priced(
                    session_id,
                    task_id,
                    op_id,
                    predicted_micro,
                    created_ms,
                    snapshot_json.as_deref(),
                    execution_class,
                )
                .map_err(|e| Self::map_row_err(e, session_id, task_id))
        })
        .await
        .map_err(BudgetError::from)??;
        match out {
            faktor_store::CostReserveOutcome::Granted(id) => Ok(ReservationId::new(id)),
            faktor_store::CostReserveOutcome::Exceeded { free } => {
                Err(BudgetError::BudgetExceeded {
                    free,
                    predicted: predicted_micro,
                })
            }
        }
    }

    async fn mark_dispatched_inner(&self, reservation: ReservationId) -> Result<(), BudgetError> {
        let store = self.store();
        let at_ms = self.now_ms();
        tokio::task::spawn_blocking(move || store.cost_mark_dispatched(reservation.raw(), at_ms))
            .await
            .map_err(BudgetError::from)?
            .map_err(BudgetError::from)
            .and_then(map_reservation_state(reservation))
    }

    async fn mark_uncertain_inner(
        &self,
        reservation: ReservationId,
        reason_code: String,
        request_id: Option<String>,
    ) -> Result<(), BudgetError> {
        if reason_code.is_empty() || reason_code.len() > MAX_FAILURE_REASON_CODE_BYTES {
            return Err(BudgetError::Malformed(format!(
                "failure reason code must be 1..={MAX_FAILURE_REASON_CODE_BYTES} bytes"
            )));
        }
        if let Some(id) = &request_id {
            if id.is_empty() || id.len() > MAX_REQUEST_ID_BYTES {
                return Err(BudgetError::Malformed(format!(
                    "request id must be 1..={MAX_REQUEST_ID_BYTES} bytes"
                )));
            }
        }
        let store = self.store();
        let at_ms = self.now_ms();
        tokio::task::spawn_blocking(move || {
            store.cost_mark_uncertain(
                reservation.raw(),
                &reason_code,
                request_id.as_deref(),
                at_ms,
            )
        })
        .await
        .map_err(BudgetError::from)?
        .map_err(BudgetError::from)
        .and_then(map_reservation_state(reservation))
    }

    /// Reserve the task budget for ONE PHYSICAL ATTEMPT (attempt
    /// accounting): the reservation keys by the attempt's fresh op id and
    /// records the shared logical parent op id, so two attempts of the same
    /// logical op hold two independent reservations. Additive inherent alias
    /// of the attempt reserve (the trait's `reserve` stays the legacy
    /// shared-op entry point the current runtime calls).
    pub async fn reserve_attempt(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt: ModelCallAttempt,
        predicted_micro: u64,
        pricing_snapshot: Option<PricingSnapshot>,
        execution_class: ExecutionClass,
    ) -> Result<ReservationId, BudgetError> {
        let store = self.store();
        let created_ms = self.now_ms();
        let snapshot_json = match pricing_snapshot {
            Some(s) => {
                let json = serde_json::to_string(&s)
                    .map_err(|e| BudgetError::Malformed(format!("pricing snapshot: {e}")))?;
                if json.len() > MAX_PRICING_SNAPSHOT_JSON_BYTES {
                    return Err(BudgetError::Malformed(format!(
                        "pricing snapshot JSON of {} bytes exceeds MAX_PRICING_SNAPSHOT_JSON_BYTES",
                        json.len()
                    )));
                }
                Some(json)
            }
            None => None,
        };
        // Admission + insert in ONE critical section (the same root-subscope
        // gate the legacy reserve path applies).
        let manager = self.session.clone();
        let out = tokio::task::spawn_blocking(move || {
            let _guard = budget_admission_lock()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(free) = consumer_root_free(&manager, session_id, task_id)? {
                if predicted_micro > free {
                    return Err(BudgetError::BudgetExceeded {
                        free,
                        predicted: predicted_micro,
                    });
                }
            }
            store
                .cost_reserve_attempt(
                    session_id,
                    task_id,
                    &attempt,
                    predicted_micro,
                    created_ms,
                    snapshot_json.as_deref(),
                    execution_class,
                )
                .map_err(|e| Self::map_row_err(e, session_id, task_id))
        })
        .await
        .map_err(BudgetError::from)??;
        match out {
            faktor_store::CostReserveOutcome::Granted(id) => Ok(ReservationId::new(id)),
            faktor_store::CostReserveOutcome::Exceeded { free } => {
                Err(BudgetError::BudgetExceeded {
                    free,
                    predicted: predicted_micro,
                })
            }
        }
    }

    /// The typed durable state of one reservation (v18 vocabulary).
    /// `UnknownReservation` when no row exists with this id, or the row
    /// belongs to another session; a status outside the frozen vocabulary is
    /// `Malformed`, never a guessed state.
    pub fn reservation_state(
        &self,
        session_id: SessionId,
        reservation: ReservationId,
    ) -> Result<ReservationState, BudgetError> {
        match self
            .store()
            .cost_reservation_state(reservation.raw())
            .map_err(BudgetError::from)?
        {
            None => Err(BudgetError::UnknownReservation(reservation.raw())),
            Some((row_session, status, _marker)) => {
                if row_session != session_id {
                    return Err(BudgetError::UnknownReservation(reservation.raw()));
                }
                ReservationState::from_db_status(&status).ok_or_else(|| {
                    BudgetError::Malformed(format!(
                        "reservation {} carries unknown status {status:?}",
                        reservation.raw()
                    ))
                })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle_usage_inner(
        &self,
        reservation: ReservationId,
        uncached_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        provider_reported_micro: Option<u64>,
        route_decision_json: Option<String>,
    ) -> Result<Option<u64>, BudgetError> {
        if let Some(j) = &route_decision_json {
            if j.len() > MAX_ROUTE_DECISION_JSON_BYTES {
                return Err(BudgetError::Malformed(format!(
                    "route decision JSON of {} bytes exceeds MAX_ROUTE_DECISION_JSON_BYTES",
                    j.len()
                )));
            }
        }
        let store = self.store();
        let settled_ms = self.now_ms();
        // The chosen actual is decided in the STORE's settlement transaction
        // (reported wins; else categories x the row's stored snapshot) —
        // never recomputed here from a caller-supplied micro number: there
        // is no tokens x 1 microUSD fallback anywhere on this path.
        tokio::task::spawn_blocking(move || {
            store.cost_settle_usage(
                reservation.raw(),
                uncached_input_tokens,
                cache_read_tokens,
                cache_write_tokens,
                output_tokens,
                provider_reported_micro,
                route_decision_json.as_deref(),
                settled_ms,
            )
        })
        .await
        .map_err(BudgetError::from)?
        .map_err(BudgetError::from)
        .and_then(|out| match out {
            faktor_store::CostSettleOutcome::Applied { actual_micro } => Ok(Some(actual_micro)),
            // Settled as a documented Unknown spend (no price authority, no
            // hard cap): NOTHING was folded — never a fabricated zero.
            faktor_store::CostSettleOutcome::AppliedUnknown => Ok(None),
            faktor_store::CostSettleOutcome::Missing => {
                Err(BudgetError::UnknownReservation(reservation.raw()))
            }
            faktor_store::CostSettleOutcome::NotOpen { current } => Err(BudgetError::NotOpen {
                reservation: reservation.raw(),
                status: current,
            }),
            faktor_store::CostSettleOutcome::UnknownPrice { reservation: raw } => {
                Err(BudgetError::UnknownPrice { reservation: raw })
            }
        })
    }

    async fn refund_inner(&self, reservation: ReservationId) -> Result<(), BudgetError> {
        let store = self.store();
        let settled_ms = self.now_ms();
        tokio::task::spawn_blocking(move || store.cost_refund(reservation.raw(), settled_ms))
            .await
            .map_err(BudgetError::from)?
            .map_err(BudgetError::from)
            .and_then(move |out| match out {
                faktor_store::RefundOutcome::Applied => Ok(()),
                faktor_store::RefundOutcome::Missing => {
                    Err(BudgetError::UnknownReservation(reservation.raw()))
                }
                faktor_store::RefundOutcome::Blocked {
                    current,
                    dispatched_ms,
                } => {
                    // A refund reached a row that left the refundable
                    // pre-dispatch state. The typed error names the money
                    // truth: if the durable dispatch marker was written (or
                    // the row IS `dispatched`), the provider may have billed
                    // and the reservation can never be freed by a refund —
                    // the runtime must settle or mark UNCERTAIN instead.
                    if current == "dispatched" || dispatched_ms.is_some() {
                        Err(BudgetError::CannotRefundDispatched {
                            reservation: reservation.raw(),
                        })
                    } else {
                        Err(BudgetError::NotOpen {
                            reservation: reservation.raw(),
                            status: current,
                        })
                    }
                }
            })
    }

    /// The durable reservations of one task (newest first, bounded) —
    /// observability/adversarial-test surface.
    pub fn reservations_of(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        limit: i64,
    ) -> Result<Vec<faktor_store::CostReservationRow>, BudgetError> {
        self.store()
            .cost_reservations_of(session_id, task_id, limit)
            .map_err(BudgetError::from)
    }

    /// One task's completion-time accounting picture (additive sync helper
    /// of the task-completion transaction; independent of
    /// [`BudgetAuthority::session_budget_view`], which a completion GATE
    /// must never trust — a balance that fails to read is a loud
    /// [`BudgetError`], never a silently-passing zero).
    ///
    /// The picture counts every reservation row that still holds budget:
    /// OPEN (schema `reserved` — dispatch never provably began — and
    /// `dispatched` — the request left the process and may have billed)
    /// plus UNCERTAIN (a crashed/terminally-failed dispatched attempt), each
    /// with its reserved micro sum, next to the task's durable spent total.
    /// The completion invariant is: after reconcile + conservative
    /// finalize, both open and uncertain counts are ZERO — only then may a
    /// task row transition VerifiedComplete.
    pub fn completion_accounting_balance(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<TaskCompletionBalance, BudgetError> {
        let store = self.store();
        let spent = store
            .cost_task_row(session_id, task_id)
            .map_err(BudgetError::from)?
            .map(|r| r.spent_cost_micro)
            .unwrap_or(0);
        let mut balance = TaskCompletionBalance {
            spent_cost_micro: spent,
            ..TaskCompletionBalance::default()
        };
        for row in store
            .cost_reservations_of(session_id, task_id, i64::MAX)
            .map_err(BudgetError::from)?
        {
            match row.status.as_str() {
                "reserved" | "dispatched" => {
                    balance.open_count = balance.open_count.saturating_add(1);
                    balance.open_micro = balance.open_micro.saturating_add(row.predicted_micro);
                    if row.status == "dispatched" {
                        balance.dispatched_count = balance.dispatched_count.saturating_add(1);
                    }
                }
                "uncertain" => {
                    balance.uncertain_count = balance.uncertain_count.saturating_add(1);
                    balance.uncertain_micro =
                        balance.uncertain_micro.saturating_add(row.predicted_micro);
                }
                _ => {}
            }
        }
        Ok(balance)
    }

    /// Sync reconcile of every UNCERTAIN reservation of one task whose
    /// attempt has a completed durable provider-call row (additive; the
    /// task-completion transaction is synchronous and cannot await the
    /// async [`BudgetAuthority::reconcile_uncertain`]). Same semantics,
    /// same idempotence, direct store transaction.
    pub fn reconcile_uncertain_now(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<faktor_store::CostReconcileReport, BudgetError> {
        self.store()
            .cost_reconcile_uncertain(session_id, task_id, self.now_ms())
            .map_err(BudgetError::from)
    }

    /// Sync conservative task-end finalize of every still-UNCERTAIN
    /// reservation of one task at its reserved estimate (additive; the
    /// task-completion transaction is synchronous and cannot await the
    /// async [`BudgetAuthority::finalize_uncertain`]). Same semantics, same
    /// idempotence, direct store transaction.
    pub fn finalize_uncertain_now(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<faktor_store::CostFinalizeReport, BudgetError> {
        self.store()
            .cost_finalize_uncertain(session_id, task_id, self.now_ms())
            .map_err(BudgetError::from)
    }

    fn view_inner(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<BudgetView, BudgetError> {
        let store = self.store();
        // A genuinely MISSING task row is an honest absence (no cap on
        // record, zero settled spend — the reserve path still refuses the
        // missing row typed). A READ FAILURE is never an absence: it is a
        // typed `Unavailable` carrying whatever cap evidence was read, so
        // callers can never mistake a failed read for free budget.
        let row =
            store
                .cost_task_row(session_id, task_id)
                .map_err(|e| BudgetError::Unavailable {
                    cap: BudgetCapEvidence::Unknown,
                    reason: e.to_string(),
                })?;
        let (max, spent) = row
            .map(|r| (r.max_cost_micro, r.spent_cost_micro))
            .unwrap_or((None, 0));
        let rows = store
            .cost_reservations_of(session_id, task_id, i64::MAX)
            .map_err(|e| BudgetError::Unavailable {
                cap: match max {
                    Some(cap) => BudgetCapEvidence::HardCap(cap),
                    None => BudgetCapEvidence::Uncapped,
                },
                reason: e.to_string(),
            })?;
        let mut open_micro = 0u64;
        let mut open_count = 0usize;
        let mut uncertain_micro = 0u64;
        let mut uncertain_count = 0usize;
        let mut settled_count = 0usize;
        for r in &rows {
            match r.status.as_str() {
                // In-flight rows: reserved (dispatch never began) and
                // dispatched (request sent, may bill) both hold their
                // prediction.
                "reserved" | "dispatched" => {
                    open_micro = open_micro.saturating_add(r.predicted_micro);
                    open_count += 1;
                }
                "uncertain" => {
                    uncertain_micro = uncertain_micro.saturating_add(r.predicted_micro);
                    uncertain_count += 1;
                }
                "settled" => settled_count += 1,
                _ => {}
            }
        }
        Ok(BudgetView {
            max_cost_micro: max,
            spent_cost_micro: spent,
            open_reserved_micro: open_micro,
            open_reservations: open_count,
            uncertain_reserved_micro: uncertain_micro,
            uncertain_reservations: uncertain_count,
            settled_count,
        })
    }

    fn recover_inner(&self) {
        let store = self.store();
        let at = self.now_ms();
        match store.cost_recover_open_reservations(at) {
            Ok((refunded, uncertain)) => {
                tracing::debug!(
                    "budget ledger crash recovery: {refunded} never-dispatched reservations \
                     refunded, {uncertain} may-have-dispatched reservations marked UNCERTAIN"
                );
            }
            Err(e) => tracing::warn!("budget ledger crash recovery failed: {e}"),
        }
    }

    async fn reconcile_uncertain_inner(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<faktor_store::CostReconcileReport, BudgetError> {
        let store = self.store();
        let at_ms = self.now_ms();
        tokio::task::spawn_blocking(move || {
            store.cost_reconcile_uncertain(session_id, task_id, at_ms)
        })
        .await
        .map_err(BudgetError::from)?
        .map_err(BudgetError::from)
    }

    async fn finalize_uncertain_inner(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<faktor_store::CostFinalizeReport, BudgetError> {
        let store = self.store();
        let at_ms = self.now_ms();
        tokio::task::spawn_blocking(move || {
            store.cost_finalize_uncertain(session_id, task_id, at_ms)
        })
        .await
        .map_err(BudgetError::from)?
        .map_err(BudgetError::from)
    }
}

/// The enrollment-row key of one consumer under its root session.
fn scope_key(consumer_session: SessionId) -> String {
    format!("consumer-{:016x}", consumer_session.raw())
}

/// Every durable enrollment row found under `root_session`'s fact space
/// (bounded walk; a hostile row space refuses loudly, never a silently
/// partial member list). Rows under a session that is not a root return
/// empty — scanning one's own space is how a guard knows it is a root.
fn enrollments_of(
    manager: &Arc<SessionManager>,
    root_session: SessionId,
) -> Result<Vec<BudgetScope>, BudgetError> {
    let handle = match manager.get_session(root_session) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(Vec::new()),
        Err(e) => return Err(BudgetError::Store(e.to_string())),
    };
    let mut out = Vec::new();
    let mut after: Option<(i64, String, String)> = None;
    for _ in 0..MAX_SCOPE_FACT_PAGES {
        // KIND-SCOPED: a root session's fact space also carries every
        // per-turn `taskexec_run` row, so paging it whole made every budget
        // reserve O(all history). The kind index visits only enrollments.
        let page = handle
            .memory_facts_by_kind_page(BUDGET_SCOPE_KIND, after.as_ref(), SCOPE_FACT_PAGE)
            .map_err(|e| BudgetError::Store(e.message))?;
        for (_, key, value) in &page.facts {
            let scope: BudgetScope = serde_json::from_str(value).map_err(|e| {
                BudgetError::Malformed(format!(
                    "hostile budget scope row {BUDGET_SCOPE_KIND}/{key} of session {root_session}: {e}"
                ))
            })?;
            out.push(scope);
        }
        match page.cursor {
            Some(c) => after = Some(c),
            None => return Ok(out),
        }
    }
    Err(BudgetError::Malformed(format!(
        "budget scope scan of session {root_session} exceeded {MAX_SCOPE_FACT_PAGES} fact pages; \
         refusing a partial member list"
    )))
}

/// The committed micro of ONE task row: durable settled spend plus the
/// predicted micro of every OPEN (reserved/dispatched) and UNCERTAIN
/// reservation — exactly the amounts the store's free formula holds against.
fn committed_of(
    manager: &Arc<SessionManager>,
    session_id: SessionId,
    task_id: TaskId,
) -> Result<u64, BudgetError> {
    let store = manager.store();
    let row = store
        .cost_task_row(session_id, task_id)
        .map_err(BudgetError::from)?;
    let mut committed = row.map(|r| r.spent_cost_micro).unwrap_or(0);
    for r in store
        .cost_reservations_of(session_id, task_id, i64::MAX)
        .map_err(BudgetError::from)?
    {
        if matches!(r.status.as_str(), "reserved" | "dispatched" | "uncertain") {
            committed = committed.saturating_add(r.predicted_micro);
        }
    }
    Ok(committed)
}

/// Whether a consumer session is LIVE (exists and its agent state is not
/// terminal). Enrollments of dead children of a previous run stop consuming
/// a root that a later run reuses; terminal children can never spend again.
fn consumer_is_live(
    manager: &Arc<SessionManager>,
    session_id: SessionId,
) -> Result<bool, BudgetError> {
    match manager.get_session(session_id) {
        Ok(Some(h)) => {
            let state = h.state().map_err(|e| BudgetError::Store(e.message))?;
            Ok(!state.is_terminal())
        }
        Ok(None) => Ok(false),
        Err(e) => Err(BudgetError::Store(e.to_string())),
    }
}

/// The root-side free balance an ENROLLED consumer's reserve may still use:
/// `None` = no root admission applies (not an orchestrated child, not
/// enrolled, or the root row/cap does not exist). `Some(free)` = the root
/// cap minus the root row's own committed amounts minus the committed
/// amounts of every LIVE enrolled child task (including this consumer's
/// current holdings, never its about-to-land prediction). Callers MUST run
/// this under [`budget_admission_lock`] and commit in the same critical
/// section.
fn consumer_root_free(
    manager: &Arc<SessionManager>,
    consumer: SessionId,
    consumer_task: TaskId,
) -> Result<Option<u64>, BudgetError> {
    let child_handle = match manager.get_session(consumer) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(None),
        Err(e) => return Err(BudgetError::Store(e.to_string())),
    };
    let identity = match child_handle.orchestrator_child_identity_get() {
        Ok(Some(i)) => i,
        Ok(None) => return Ok(None),
        Err(e) => return Err(BudgetError::Store(e.message)),
    };
    let root_session = identity.parent_session_id;
    let root_handle = match manager.get_session(root_session) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(None),
        Err(e) => return Err(BudgetError::Store(e.to_string())),
    };
    let root_task = root_handle
        .task_id()
        .map_err(|e| BudgetError::Store(e.message))?;
    let enrollments = enrollments_of(manager, root_session)?;
    // Only an ENROLLED consumer is admitted against the root: a child whose
    // cost cap was never changed has no scope and stays bounded by its own
    // row alone.
    let enrolled = enrollments.iter().any(|s| {
        s.consumer_session_id == consumer
            && s.consumer_task_id == consumer_task
            && s.child_cap_micro.is_some()
    });
    if !enrolled {
        return Ok(None);
    }
    let store = manager.store();
    let Some(root_row) = store
        .cost_task_row(root_session, root_task)
        .map_err(BudgetError::from)?
    else {
        return Ok(None);
    };
    let Some(cap) = root_row.max_cost_micro else {
        return Ok(None);
    };
    let mut committed = committed_of(manager, root_session, root_task)?;
    for scope in &enrollments {
        if scope.child_cap_micro.is_none() {
            continue;
        }
        if scope.consumer_session_id == root_session {
            continue;
        }
        if consumer_is_live(manager, scope.consumer_session_id)? {
            committed = committed.saturating_add(committed_of(
                manager,
                scope.consumer_session_id,
                scope.consumer_task_id,
            )?);
        }
    }
    Ok(Some(cap.saturating_sub(committed)))
}

/// Seed the child's task row when the task machine has not created one yet
/// (the token-budget path seeds rows the same way; a missing monetary row is
/// never silently refused on a live child).
fn seed_child_task_row_if_missing(
    child: &crate::SessionHandle,
    child_session: SessionId,
) -> Result<(), BudgetError> {
    let task_id = child.task_id().map_err(|e| BudgetError::Store(e.message))?;
    if child
        .get_task(task_id)
        .map_err(|e| BudgetError::Store(e.message))?
        .is_some()
    {
        return Ok(());
    }
    let goal: String = child
        .title()
        .map(|t| {
            let mut goal = String::new();
            for c in t.chars() {
                if goal.len() + c.len_utf8() > crate::MAX_TASK_GOAL_BYTES {
                    break;
                }
                goal.push(c);
            }
            goal
        })
        .unwrap_or_default();
    let now = child.now_ms();
    child
        .create_task(crate::Task {
            task_id,
            session_id: child_session,
            goal,
            created_ms: now,
            updated_ms: now,
            ..Default::default()
        })
        .map_err(|e| BudgetError::Store(e.to_string()))?;
    Ok(())
}

fn map_reservation_state(
    reservation: ReservationId,
) -> impl FnOnce(faktor_store::CostReservationState) -> Result<(), BudgetError> {
    move |out| match out {
        faktor_store::CostReservationState::Applied => Ok(()),
        faktor_store::CostReservationState::Missing => {
            Err(BudgetError::UnknownReservation(reservation.raw()))
        }
        faktor_store::CostReservationState::NotOpen { current } => Err(BudgetError::NotOpen {
            reservation: reservation.raw(),
            status: current,
        }),
    }
}

impl BudgetAuthority for DurableBudgetLedger {
    fn reserve(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        op_id: OpId,
        predicted_micro: u64,
        pricing_snapshot: Option<PricingSnapshot>,
        execution_class: ExecutionClass,
    ) -> BoxFut<'_, Result<ReservationId, BudgetError>> {
        Box::pin(self.reserve_inner(
            session_id,
            task_id,
            op_id,
            predicted_micro,
            pricing_snapshot,
            execution_class,
        ))
    }

    fn reserve_attempt(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt: ModelCallAttempt,
        predicted_micro: u64,
        pricing_snapshot: Option<PricingSnapshot>,
        execution_class: ExecutionClass,
    ) -> BoxFut<'_, Result<ReservationId, BudgetError>> {
        Box::pin(async move {
            self.reserve_attempt(
                session_id,
                task_id,
                attempt,
                predicted_micro,
                pricing_snapshot,
                execution_class,
            )
            .await
        })
    }

    fn mark_dispatched(
        &self,
        _session_id: SessionId,
        reservation: ReservationId,
    ) -> BoxFut<'_, Result<(), BudgetError>> {
        Box::pin(self.mark_dispatched_inner(reservation))
    }

    fn mark_uncertain(
        &self,
        _session_id: SessionId,
        reservation: ReservationId,
        reason_code: String,
        request_id: Option<String>,
    ) -> BoxFut<'_, Result<(), BudgetError>> {
        Box::pin(self.mark_uncertain_inner(reservation, reason_code, request_id))
    }

    fn settle_usage(
        &self,
        _session_id: SessionId,
        reservation: ReservationId,
        uncached_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        provider_reported_micro: Option<u64>,
        route_decision_json: Option<String>,
    ) -> BoxFut<'_, Result<Option<u64>, BudgetError>> {
        Box::pin(self.settle_usage_inner(
            reservation,
            uncached_input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            provider_reported_micro,
            route_decision_json,
        ))
    }

    fn refund(
        &self,
        _session_id: SessionId,
        reservation: ReservationId,
    ) -> BoxFut<'_, Result<(), BudgetError>> {
        Box::pin(self.refund_inner(reservation))
    }

    fn session_budget_view(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<BudgetView, BudgetError> {
        self.view_inner(session_id, task_id)
    }

    fn recover_after_restart(&self) {
        self.recover_inner();
    }

    fn reconcile_uncertain(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> BoxFut<'_, Result<faktor_store::CostReconcileReport, BudgetError>> {
        Box::pin(self.reconcile_uncertain_inner(session_id, task_id))
    }

    fn finalize_uncertain(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> BoxFut<'_, Result<faktor_store::CostFinalizeReport, BudgetError>> {
        Box::pin(self.finalize_uncertain_inner(session_id, task_id))
    }
}

/// The unlimited ledger of test graphs that never set a cap: every reserve
/// grants, every settle/refund records nothing. `session_budget_view`
/// reports an unlimited budget with no activity.
#[derive(Debug, Default)]
pub struct NoopBudget;

impl BudgetAuthority for NoopBudget {
    fn reserve(
        &self,
        _session_id: SessionId,
        _task_id: TaskId,
        _op_id: OpId,
        _predicted_micro: u64,
        _pricing_snapshot: Option<PricingSnapshot>,
        _execution_class: ExecutionClass,
    ) -> BoxFut<'_, Result<ReservationId, BudgetError>> {
        Box::pin(async { Ok(ReservationId::NOOP) })
    }

    fn reserve_attempt(
        &self,
        _session_id: SessionId,
        _task_id: TaskId,
        _attempt: ModelCallAttempt,
        _predicted_micro: u64,
        _pricing_snapshot: Option<PricingSnapshot>,
        _execution_class: ExecutionClass,
    ) -> BoxFut<'_, Result<ReservationId, BudgetError>> {
        Box::pin(async { Ok(ReservationId::NOOP) })
    }

    fn mark_dispatched(
        &self,
        _session_id: SessionId,
        _reservation: ReservationId,
    ) -> BoxFut<'_, Result<(), BudgetError>> {
        Box::pin(async { Ok(()) })
    }

    fn mark_uncertain(
        &self,
        _session_id: SessionId,
        _reservation: ReservationId,
        _reason_code: String,
        _request_id: Option<String>,
    ) -> BoxFut<'_, Result<(), BudgetError>> {
        Box::pin(async { Ok(()) })
    }

    fn settle_usage(
        &self,
        _session_id: SessionId,
        _reservation: ReservationId,
        _uncached_input_tokens: u64,
        _cache_read_tokens: u64,
        _cache_write_tokens: u64,
        _output_tokens: u64,
        _provider_reported_micro: Option<u64>,
        _route_decision_json: Option<String>,
    ) -> BoxFut<'_, Result<Option<u64>, BudgetError>> {
        Box::pin(async { Ok(None) })
    }

    fn refund(
        &self,
        _session_id: SessionId,
        _reservation: ReservationId,
    ) -> BoxFut<'_, Result<(), BudgetError>> {
        Box::pin(async { Ok(()) })
    }

    fn session_budget_view(
        &self,
        _session_id: SessionId,
        _task_id: TaskId,
    ) -> Result<BudgetView, BudgetError> {
        Ok(BudgetView {
            max_cost_micro: None,
            spent_cost_micro: 0,
            open_reserved_micro: 0,
            open_reservations: 0,
            uncertain_reserved_micro: 0,
            uncertain_reservations: 0,
            settled_count: 0,
        })
    }

    fn recover_after_restart(&self) {}

    fn reconcile_uncertain(
        &self,
        _session_id: SessionId,
        _task_id: TaskId,
    ) -> BoxFut<'_, Result<faktor_store::CostReconcileReport, BudgetError>> {
        Box::pin(async { Ok(faktor_store::CostReconcileReport::default()) })
    }

    fn finalize_uncertain(
        &self,
        _session_id: SessionId,
        _task_id: TaskId,
    ) -> BoxFut<'_, Result<faktor_store::CostFinalizeReport, BudgetError>> {
        Box::pin(async { Ok(faktor_store::CostFinalizeReport::default()) })
    }
}

// ------------------------------------------------------- change budget (audit 57/105)

/// One typed violation of a task's [`ChangeBudget`] (audit 57/105): the
/// machine-readable cause VerifiedComplete is refused for a mutating run.
/// Every variant names the offending value — never prose-only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChangeBudgetViolation {
    #[error("changed path {path:?} is outside the change budget's allowed_paths")]
    PathOutsideAllowed { path: String },
    #[error("the run changed {changed} paths, beyond max_blast_radius {max}")]
    BlastRadiusExceeded { changed: u64, max: u64 },
    #[error("semantic entity {entity:?} is outside the change budget's allowed_semantic_entities")]
    SemanticEntityOutsideAllowed { entity: String },
    #[error(
        "the change budget constrains semantic entities, but the run reports NO semantic data \
         (Unknown): the policy requires stronger verification before this change can be certified"
    )]
    SemanticDataUnknown,
    #[error("the change budget forbids a {feature} increase, but the run reports one")]
    ForbiddenIncrease { feature: &'static str },
    #[error(
        "the change budget restricts {feature}, and the change's {feature} state is Unknown: \
         stronger verification is required (strict enforcement)"
    )]
    FeatureUnknown { feature: &'static str },
}

/// What one mutating run actually changed (audit 57/105), as far as the
/// enforcing site can observe. `None` on a feature means the data does not
/// exist for this run (Unknown) — never a fabricated `false`:
/// - semantic fields are enforced when the data exists; a constrained budget
///   plus `semantic_entities: None` is a typed [`ChangeBudgetViolation::SemanticDataUnknown`];
/// - the `allow_*` flags are enforced only on an observed `Some(true)`; the
///   stricter [`ChangeBudget::check_strict`] additionally refuses Unknown.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangeObservations {
    /// The distinct repository-relative paths the run changed.
    pub changed_paths: Vec<String>,
    /// The semantic entities the run touched, when the run reports them.
    pub semantic_entities: Option<Vec<String>>,
    /// Whether the run increases the public surface.
    pub public_surface_increase: Option<bool>,
    /// Whether the run increases security risk.
    pub security_risk_increase: Option<bool>,
    /// Whether the run increases `unsafe` usage.
    pub unsafe_increase: Option<bool>,
    /// Whether the run adds new external effects.
    pub new_external_effects: Option<bool>,
    /// Whether the run weakens a contract.
    pub contract_weakening: Option<bool>,
}

/// Enforce a change budget over one run's observations. Returns EVERY
/// violation (a refusal names all causes at once). An absent budget is the
/// caller's concern (`None` != empty budget semantics): an empty
/// [`ChangeBudget`] restricts nothing.
pub fn check_change_budget(
    budget: &ChangeBudget,
    observations: &ChangeObservations,
) -> Result<(), Vec<ChangeBudgetViolation>> {
    let mut violations = Vec::new();
    for path in &observations.changed_paths {
        if !budget.allows_path(path) {
            violations.push(ChangeBudgetViolation::PathOutsideAllowed { path: path.clone() });
        }
    }
    if let Some(max) = budget.max_blast_radius {
        let changed = observations.changed_paths.len() as u64;
        if changed > max {
            violations.push(ChangeBudgetViolation::BlastRadiusExceeded { changed, max });
        }
    }
    if !budget.allowed_semantic_entities.is_empty() {
        match &observations.semantic_entities {
            None => violations.push(ChangeBudgetViolation::SemanticDataUnknown),
            Some(entities) => {
                for entity in entities {
                    if !budget.allows_semantic_entity(entity) {
                        violations.push(ChangeBudgetViolation::SemanticEntityOutsideAllowed {
                            entity: entity.clone(),
                        });
                    }
                }
            }
        }
    }
    for (feature, allowed, observed) in budget_flags(budget, observations) {
        if !allowed && observed == Some(true) {
            violations.push(ChangeBudgetViolation::ForbiddenIncrease { feature });
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// The strict policy variant of [`check_change_budget`]: a feature the
/// budget restricts with an UNKNOWN observation is also a typed
/// [`ChangeBudgetViolation::FeatureUnknown`] — the "policy requires stronger
/// verification" refusal path for unknown data.
pub fn check_change_budget_strict(
    budget: &ChangeBudget,
    observations: &ChangeObservations,
) -> Result<(), Vec<ChangeBudgetViolation>> {
    let mut violations = check_change_budget(budget, observations)
        .err()
        .unwrap_or_default();
    for (feature, allowed, observed) in budget_flags(budget, observations) {
        if !allowed && observed.is_none() {
            violations.push(ChangeBudgetViolation::FeatureUnknown { feature });
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// The five `allow_*` flags with their observed run state (feature name,
/// budget allowance, observation).
fn budget_flags(
    budget: &ChangeBudget,
    observations: &ChangeObservations,
) -> [(&'static str, bool, Option<bool>); 5] {
    [
        (
            "public_surface_increase",
            budget.allow_public_surface_increase,
            observations.public_surface_increase,
        ),
        (
            "security_risk_increase",
            budget.allow_security_risk_increase,
            observations.security_risk_increase,
        ),
        (
            "unsafe_increase",
            budget.allow_unsafe_increase,
            observations.unsafe_increase,
        ),
        (
            "new_external_effects",
            budget.allow_new_external_effects,
            observations.new_external_effects,
        ),
        (
            "contract_weakening",
            budget.allow_contract_weakening,
            observations.contract_weakening,
        ),
    ]
}

/// Bound on the serialized change-budget fact (the memory-fact value cap).
const MAX_CHANGE_BUDGET_JSON_BYTES: usize = 4096;
/// Fact kind/key of the task's change budget: an existing memory-fact row
/// (no schema change). The value is the JSON [`ChangeBudget`] or `null`
/// (cleared / absent = today's behavior).
const CHANGE_BUDGET_FACT_KIND: &str = "change_budget";
const CHANGE_BUDGET_FACT_KEY: &str = "0";

fn validate_change_budget(budget: &ChangeBudget) -> Result<(), crate::task::TaskError> {
    let lists = [
        ("allowed_paths", &budget.allowed_paths),
        (
            "allowed_semantic_entities",
            &budget.allowed_semantic_entities,
        ),
    ];
    for (what, entries) in lists {
        if entries.len() > MAX_CHANGE_BUDGET_ENTRIES {
            return Err(crate::task::TaskError::Oversized(format!(
                "{what} carries {} entries, beyond MAX_CHANGE_BUDGET_ENTRIES ({MAX_CHANGE_BUDGET_ENTRIES})",
                entries.len()
            )));
        }
        for entry in entries {
            if entry.is_empty() || entry.chars().count() > MAX_CHANGE_BUDGET_ENTRY_CHARS {
                return Err(crate::task::TaskError::Malformed(format!(
                    "{what} entry {entry:?} is empty or beyond MAX_CHANGE_BUDGET_ENTRY_CHARS ({MAX_CHANGE_BUDGET_ENTRY_CHARS})"
                )));
            }
        }
    }
    let json = serde_json::to_vec(budget)
        .map_err(|e| crate::task::TaskError::Malformed(format!("change budget json: {e}")))?;
    if json.len() > MAX_CHANGE_BUDGET_JSON_BYTES {
        return Err(crate::task::TaskError::Oversized(format!(
            "change budget JSON of {} bytes exceeds {MAX_CHANGE_BUDGET_JSON_BYTES}",
            json.len()
        )));
    }
    Ok(())
}

impl crate::SessionHandle {
    /// The task's durable change budget (audit 57/105). `None` = no budget =
    /// today's behavior (no change-scope enforcement). A corrupted/hostile
    /// fact is a typed refusal, never a silently ignored policy.
    pub fn change_budget(&self) -> Result<Option<ChangeBudget>, crate::task::TaskError> {
        // Point lookup: this runs on every tool-loop pass, and the session
        // fact space also carries per-turn run rows (1M-churn probe).
        let value = self
            .memory_fact_get(CHANGE_BUDGET_FACT_KIND, CHANGE_BUDGET_FACT_KEY)
            .map_err(|e| crate::task::TaskError::Store(e.to_string()))?;
        let Some(value) = value else {
            return Ok(None);
        };
        match serde_json::from_str::<Option<ChangeBudget>>(&value) {
            Ok(budget) => Ok(budget),
            Err(e) => Err(crate::task::TaskError::Malformed(format!(
                "change_budget fact is not a valid ChangeBudget JSON: {e}"
            ))),
        }
    }

    /// Set (Some) or clear (None) the task's durable change budget. Bounded
    /// and validated before the write; `None` stores the explicit `null`
    /// value so an absent fact and a cleared budget cannot drift.
    pub fn set_change_budget(
        &self,
        budget: Option<&ChangeBudget>,
    ) -> Result<(), crate::task::TaskError> {
        if let Some(budget) = budget {
            validate_change_budget(budget)?;
        }
        let value = serde_json::to_string(&budget)
            .map_err(|e| crate::task::TaskError::Malformed(format!("change budget json: {e}")))?;
        if value.len() > MAX_CHANGE_BUDGET_JSON_BYTES {
            return Err(crate::task::TaskError::Oversized(format!(
                "change budget JSON of {} bytes exceeds {MAX_CHANGE_BUDGET_JSON_BYTES}",
                value.len()
            )));
        }
        self.upsert_memory_fact(CHANGE_BUDGET_FACT_KIND, CHANGE_BUDGET_FACT_KEY, &value)
            .map_err(|e| crate::task::TaskError::Malformed(e.to_string()))
    }

    /// Enforce the task's change budget at the edit/steer gate (audit
    /// 57/105): a mutating run whose changed paths/entities fall outside the
    /// budget, exceed its blast radius or report a forbidden increase
    /// refuses with the typed [`crate::task::TaskError::ChangeBudgetRefused`].
    /// NO budget = today's behavior (Ok). `semantic_entities: None` under a
    /// budget that constrains semantic entities is the documented
    /// "Unknown => stronger verification" refusal.
    pub fn enforce_change_budget(
        &self,
        task_id: TaskId,
        observations: &ChangeObservations,
    ) -> Result<(), crate::task::TaskError> {
        match self.change_budget()? {
            None => Ok(()),
            Some(budget) => check_change_budget(&budget, observations).map_err(|violations| {
                crate::task::TaskError::ChangeBudgetRefused {
                    task_id,
                    violations,
                }
            }),
        }
    }
}

#[cfg(test)]
#[path = "budget_tests.rs"]
mod budget_tests;
