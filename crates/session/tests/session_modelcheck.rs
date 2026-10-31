// TEST-SEPARATION: oracle=independent-reference isolation=isolated-data-dir
//! HAR-09 / DOM-01: reference session state machine (model check).
//!
//! An INDEPENDENT, tiny reference model of the session lifecycle (the
//! oracle) is driven side-by-side with the real `faktor-session` runtime by
//! seeded, replayable, shrinkable random command sequences. The model
//! re-implements the documented transition tables itself and never calls
//! production transition/validation code, so a production regression cannot
//! mask itself by sharing the oracle's logic. After every step the durable
//! observable state of the real runtime must equal the model's expectation
//! exactly; after a clean reload the modeled restart world must survive
//! exactly; after a crash at a store durability boundary the durable world
//! must lie inside the atomic old/new window, recovery must land on the
//! modeled crash target, and a second sweep must append nothing.
//!
//! Model ops (driver alphabet): prompt (immediate / queued steer), queue
//! admission, interior state hops, turn settle (exactly-once), assistant
//! message append, budget reserve/settle/refund/dispatch, abort, suspend/
//! resume (switch/activate), failed/reset (poison-sentinel turns), session
//! end, message eviction (deleteMessage), duplicate/out-of-order settle
//! attempts (poison), concurrent append+settle (race + both sequential
//! interleavings), clean reload, and crash-at-boundary reconstruction via
//! the store's deterministic seam.
//!
//! Failure report: seed + full op sequence + the SHRUNKEN minimal sequence.
//! The fast test runs small sequences in normal time on every
//! `cargo test -p faktor-session`; the long exhaustive campaign is
//! `#[ignore]`d and registered in scripts/certification/ignored-tests.json.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use faktor_core::event::EventKind;
use faktor_core::id::OpId;
use faktor_core::model::{ExecutionClass, MicroUsdPerMillionTokens, PriceQuote, PricingSnapshot};
use faktor_core::state::{AgentState, SessionLifecycle};
use faktor_session::budget::ReservationState;
use faktor_session::{
    BudgetAuthority, BudgetError, DurableBudgetLedger, ReservationId, SessionHandle,
    SessionManager, Task,
};

// ---------------------------------------------------------------- seeded rng

/// xorshift64*: tiny deterministic PRNG; every sequence is replayable from
/// its printed seed alone.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Never-zero state.
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

// ------------------------------------------------------- reference model core
//
// The oracle. These tables are an INDEPENDENT transcription of the session
// architecture's documented state machine; the model never invokes
// `faktor_core::state::StateMachine`, `journal::validate_transition`, or any
// other production logic to decide legality.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MState {
    Idle,
    Preparing,
    BuildingContext,
    WaitingForModel,
    Streaming,
    ToolRequested,
    WaitingForPermission,
    ExecutingTool,
    Validating,
    UpdatingMemory,
    ReadyForNextTurn,
    Completed,
    Cancelled,
    FailedRecoverable,
    FailedPermanent,
    NeedsUserInput,
    Suspended,
}

impl MState {
    /// Independent transcription of the legal per-turn transitions.
    fn allowed(self) -> &'static [MState] {
        use MState::*;
        match self {
            Idle => &[Preparing, Suspended, Completed, Cancelled],
            Preparing => &[BuildingContext, FailedRecoverable, Cancelled, Suspended],
            BuildingContext => &[WaitingForModel, FailedRecoverable, Cancelled, Suspended],
            WaitingForModel => &[
                Streaming,
                FailedRecoverable,
                Cancelled,
                Suspended,
                NeedsUserInput,
            ],
            Streaming => &[
                ToolRequested,
                Validating,
                WaitingForModel,
                FailedRecoverable,
                Cancelled,
                Suspended,
            ],
            ToolRequested => &[
                WaitingForPermission,
                ExecutingTool,
                Validating,
                Cancelled,
                Suspended,
            ],
            WaitingForPermission => &[
                ExecutingTool,
                ReadyForNextTurn,
                Cancelled,
                Suspended,
                NeedsUserInput,
            ],
            ExecutingTool => &[
                Validating,
                ToolRequested,
                FailedRecoverable,
                Cancelled,
                Suspended,
            ],
            Validating => &[
                UpdatingMemory,
                ToolRequested,
                ExecutingTool,
                FailedRecoverable,
                Completed,
                Cancelled,
                Suspended,
            ],
            UpdatingMemory => &[
                ReadyForNextTurn,
                WaitingForModel,
                Completed,
                FailedRecoverable,
                Cancelled,
                Suspended,
            ],
            ReadyForNextTurn => &[Preparing, Completed, Cancelled, Suspended, NeedsUserInput],
            Completed => &[],
            Cancelled => &[Preparing, ReadyForNextTurn, Suspended],
            FailedPermanent => &[],
            FailedRecoverable => &[Preparing, Idle, Cancelled, Suspended, NeedsUserInput],
            NeedsUserInput => &[ReadyForNextTurn, Preparing, Cancelled, Suspended],
            Suspended => &[Idle, Preparing, Cancelled, Completed],
        }
    }

    /// Legality of landing on `to` from `self`: the documented transition
    /// table PLUS the machine's idempotent self-transition rule (a re-emit
    /// landing on the CURRENT state is legal from every state, so repeated
    /// events never wedge replay).
    fn can(self, to: MState) -> bool {
        to == self || self.allowed().contains(&to)
    }

    fn is_terminal(self) -> bool {
        matches!(self, MState::Completed | MState::FailedPermanent)
    }

    /// A machine state in which some operation of the current turn is in
    /// flight (mid-turn, not between turns).
    fn is_op_active(self) -> bool {
        matches!(
            self,
            MState::Preparing
                | MState::BuildingContext
                | MState::WaitingForModel
                | MState::Streaming
                | MState::ToolRequested
                | MState::WaitingForPermission
                | MState::ExecutingTool
                | MState::Validating
                | MState::UpdatingMemory
        )
    }

    /// States a fresh prompt starts a turn from immediately.
    fn is_promptable(self) -> bool {
        matches!(
            self,
            MState::Idle
                | MState::ReadyForNextTurn
                | MState::NeedsUserInput
                | MState::FailedRecoverable
                | MState::Cancelled
                | MState::Suspended
        )
    }

    /// States eligible for atomic queue-head admission.
    fn is_admit_eligible(self) -> bool {
        matches!(
            self,
            MState::Idle | MState::ReadyForNextTurn | MState::Cancelled | MState::FailedRecoverable
        )
    }

    /// Independent transcription of the recovery crash-target preference.
    fn crash_target(self) -> MState {
        for t in [
            MState::FailedRecoverable,
            MState::WaitingForPermission,
            MState::NeedsUserInput,
        ] {
            if self.can(t) {
                return t;
            }
        }
        self
    }

    fn as_real(self) -> AgentState {
        match self {
            MState::Idle => AgentState::Idle,
            MState::Preparing => AgentState::Preparing,
            MState::BuildingContext => AgentState::BuildingContext,
            MState::WaitingForModel => AgentState::WaitingForModel,
            MState::Streaming => AgentState::Streaming,
            MState::ToolRequested => AgentState::ToolRequested,
            MState::WaitingForPermission => AgentState::WaitingForPermission,
            MState::ExecutingTool => AgentState::ExecutingTool,
            MState::Validating => AgentState::Validating,
            MState::UpdatingMemory => AgentState::UpdatingMemory,
            MState::ReadyForNextTurn => AgentState::ReadyForNextTurn,
            MState::Completed => AgentState::Completed,
            MState::Cancelled => AgentState::Cancelled,
            MState::FailedRecoverable => AgentState::FailedRecoverable,
            MState::FailedPermanent => AgentState::FailedPermanent,
            MState::NeedsUserInput => AgentState::NeedsUserInput,
            MState::Suspended => AgentState::Suspended,
        }
    }

    fn from_real(s: AgentState) -> Self {
        match s {
            AgentState::Idle => MState::Idle,
            AgentState::Preparing => MState::Preparing,
            AgentState::BuildingContext => MState::BuildingContext,
            AgentState::WaitingForModel => MState::WaitingForModel,
            AgentState::Streaming => MState::Streaming,
            AgentState::ToolRequested => MState::ToolRequested,
            AgentState::WaitingForPermission => MState::WaitingForPermission,
            AgentState::ExecutingTool => MState::ExecutingTool,
            AgentState::Validating => MState::Validating,
            AgentState::UpdatingMemory => MState::UpdatingMemory,
            AgentState::ReadyForNextTurn => MState::ReadyForNextTurn,
            AgentState::Completed => MState::Completed,
            AgentState::Cancelled => MState::Cancelled,
            AgentState::FailedRecoverable => MState::FailedRecoverable,
            AgentState::FailedPermanent => MState::FailedPermanent,
            AgentState::NeedsUserInput => MState::NeedsUserInput,
            AgentState::Suspended => MState::Suspended,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MLife {
    Open,
    Suspended,
    Closed,
}

impl MLife {
    fn as_real(self) -> SessionLifecycle {
        match self {
            MLife::Open => SessionLifecycle::Open,
            MLife::Suspended => SessionLifecycle::Suspended,
            MLife::Closed => SessionLifecycle::Closed,
        }
    }

    fn from_real(l: SessionLifecycle) -> Self {
        match l {
            SessionLifecycle::Open => MLife::Open,
            SessionLifecycle::Suspended => MLife::Suspended,
            // Closing/FailedPermanent are unreachable through the modeled ops.
            SessionLifecycle::Closing
            | SessionLifecycle::Closed
            | SessionLifecycle::FailedPermanent => MLife::Closed,
        }
    }
}

/// Durable status of ONE logical-turn record (at most one is active).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MRec {
    Active,
    Completed,
    Failed,
}

/// Reservation lifecycle as the money model tracks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MRes {
    Reserved(u64),
    Dispatched(u64),
    /// Settled. `dispatched` mirrors the row's durable `dispatched_ms`
    /// marker: the store's settle UPDATE never clears the marker, so a
    /// later refund of a settled row must distinguish settled-AFTER-dispatch
    /// (marker set -> `CannotRefundDispatched` -> `RefundDispatched`) from
    /// settled pre-dispatch (NULL marker -> `NotOpen`).
    Settled {
        dispatched: bool,
    },
    Refunded,
    Uncertain(u64),
}

impl MRes {
    fn holds(self) -> Option<u64> {
        match self {
            MRes::Reserved(p) | MRes::Dispatched(p) => Some(p),
            _ => None,
        }
    }
}

/// The independent reference model of one session's durable lifecycle.
#[derive(Debug, Clone, PartialEq)]
struct Model {
    state: MState,
    life: MLife,
    /// Gapless journal sequence; 1 is `SessionCreated`.
    last_event: u64,
    /// Durable messages, ascending by seq: (seq, role).
    msgs: Vec<(i64, &'static str)>,
    /// Prompt-queue rows by status (`pending` / `claimed`; terminal rows
    /// are untracked, exactly like the observable count).
    pending: u64,
    /// Queue rows in `claimed`/`running` status: (queue seq, row op). At
    /// most one row per admission; several can pile up across recoveries.
    running: Vec<(i64, u64)>,
    /// Durable queue seq of the currently-claimed row (opaque; from receipts).
    claim_seq: Option<i64>,
    /// Every turn record ever opened: (turn op raw, status).
    records: Vec<(u64, MRec)>,
    /// The handle's IN-MEMORY turn registry (ops().all()): grows on every
    /// accepted prompt/admission, cleared by abort (all affected) and by
    /// any restart (a fresh handle starts empty). My driver's settle does
    /// not unregister — the registry feeds abort's per-op event fan-out.
    tracked: Vec<u64>,
    // ---- money (one task, one hard cap) ----
    cap: u64,
    spent: u64,
    /// Sum of predictions of every OPEN (reserved|dispatched) reservation.
    open_micro: u64,
    /// Sum of predictions of every UNCERTAIN reservation.
    uncertain_micro: u64,
    /// Parallel to the driver's real reservation-id table.
    reservations: Vec<MRes>,
}

impl Model {
    fn new(cap: u64) -> Self {
        Self {
            state: MState::Idle,
            life: MLife::Open,
            last_event: 1,
            msgs: Vec::new(),
            pending: 0,
            running: Vec::new(),
            claim_seq: None,
            records: Vec::new(),
            tracked: Vec::new(),
            cap,
            spent: 0,
            open_micro: 0,
            uncertain_micro: 0,
            reservations: Vec::new(),
        }
    }

    fn free(&self) -> u64 {
        self.cap
            .saturating_sub(self.spent)
            .saturating_sub(self.open_micro)
            .saturating_sub(self.uncertain_micro)
    }

    fn queued(&self) -> u64 {
        self.pending + self.running.len() as u64
    }

    /// Queue recovery (`recover_claimed_queue_rows`): claimed rows fall back
    /// to pending; RUNNING rows whose logical turn already ended (their
    /// turn record is no longer active) are retired to `done` — they leave
    /// the observable count. The record status read here must be the PRE-
    /// recovery status (the real tool runs BEFORE `recover_all` closes
    /// records).
    fn recover_queue(&mut self) {
        self.running.retain(|(_, op)| {
            self.records
                .iter()
                .any(|(o, s)| o == op && *s == MRec::Active)
        });
        self.claim_seq = None;
    }

    fn active_record_op(&self) -> Option<u64> {
        self.records
            .iter()
            .find(|(_, s)| *s == MRec::Active)
            .map(|(op, _)| *op)
    }

    /// `start_turn_record` semantics: any OTHER active record is finalized
    /// `failed` in the same transaction; the given op's record (upsert)
    /// becomes the single active one.
    fn start_record(&mut self, op: u64) {
        for (o, s) in &mut self.records {
            if *s == MRec::Active && *o != op {
                *s = MRec::Failed;
            }
        }
        if let Some((_, s)) = self.records.iter_mut().find(|(o, _)| *o == op) {
            *s = MRec::Active;
        } else {
            self.records.push((op, MRec::Active));
        }
    }

    fn recover_close_records(&mut self) -> bool {
        let mut closed = false;
        for (_, s) in &mut self.records {
            if *s == MRec::Active {
                *s = MRec::Failed;
                closed = true;
            }
        }
        closed
    }

    /// Restart/crash budget recovery: never-dispatched reservations are
    /// refunded; dispatched ones keep consuming as UNCERTAIN.
    fn recover_money(&mut self) {
        for r in &mut self.reservations {
            match *r {
                MRes::Reserved(p) => {
                    *r = MRes::Refunded;
                    self.open_micro = self.open_micro.saturating_sub(p);
                }
                MRes::Dispatched(p) => {
                    *r = MRes::Uncertain(p);
                    self.open_micro = self.open_micro.saturating_sub(p);
                    self.uncertain_micro = self.uncertain_micro.saturating_add(p);
                }
                _ => {}
            }
        }
    }

    /// Exact recovery prediction for a CLEAN process restart (reload): the
    /// queue's claimed rows fall back to pending, money is recovered, an
    /// in-flight turn lands on the crash target with one `CrashDetected`
    /// event, and any still-open turn record is finalized `failed`.
    fn predict_reload(&self) -> Self {
        let mut m = self.clone();
        // A reopened handle starts with an empty in-memory turn registry.
        m.tracked.clear();
        m.recover_money();
        // Real order: queue recovery first (against PRE-recovery records),
        // then the machine/record recovery.
        m.recover_queue();
        if m.state.is_op_active() {
            m.last_event += 1; // CrashDetected
            m.state = m.state.crash_target();
        }
        m.recover_close_records();
        m
    }

    fn msgs_typed(&self) -> Vec<(i64, String)> {
        self.msgs
            .iter()
            .map(|(s, r)| (*s, (*r).to_string()))
            .collect()
    }

    fn count_completed(&self) -> usize {
        self.records
            .iter()
            .filter(|(_, s)| *s == MRec::Completed)
            .count()
    }

    fn budget_tuple(&self) -> BudgetTuple {
        BudgetTuple {
            cap: Some(self.cap),
            spent: self.spent,
            open_reserved_micro: self.open_micro,
            open_count: self
                .reservations
                .iter()
                .filter(|r| r.holds().is_some())
                .count(),
            uncertain_micro: self.uncertain_micro,
            uncertain_count: self
                .reservations
                .iter()
                .filter(|r| matches!(r, MRes::Uncertain(_)))
                .count(),
            settled_count: self
                .reservations
                .iter()
                .filter(|r| matches!(r, MRes::Settled { .. }))
                .count(),
        }
    }

    fn obs(&self) -> Obs {
        let record_active = self
            .records
            .iter()
            .filter(|(_, s)| *s == MRec::Active)
            .count();
        let record_completed = self.count_completed();
        let record_failed = self
            .records
            .iter()
            .filter(|(_, s)| *s == MRec::Failed)
            .count();
        Obs {
            state: self.state.as_real(),
            life: self.life.as_real(),
            msgs: self.msgs_typed(),
            queued: self.queued() as i64,
            record_active,
            record_completed,
            record_failed,
            record_total: self.records.len(),
            tracked: {
                let mut t = self.tracked.clone();
                t.sort_unstable();
                t
            },
            last_event: self.last_event,
            budget: self.budget_tuple(),
        }
    }

    fn state_is_active(&self) -> bool {
        !self.state.is_terminal() && self.state != MState::Idle && self.state != MState::Suspended
    }

    /// Apply one op to the model, returning the outcome the real runtime
    /// MUST have produced. Opaque ids (op id, reservation id, queue seq)
    /// come from the real receipts; every interpretable effect is predicted
    /// here.
    fn apply(&mut self, op: &Op, ids: &ExecIds) -> StepOut {
        match op {
            Op::Prompt => {
                if self.life == MLife::Closed || self.state.is_terminal() {
                    return StepOut::Class("Conflict");
                }
                // A prompt on a suspended session auto-resumes the lifetime
                // machine (no journal event), then proceeds.
                if self.life == MLife::Suspended {
                    self.life = MLife::Open;
                }
                if self.state.is_promptable() {
                    let seq = self.last_event + 1;
                    if self.msgs.iter().any(|(s, _)| *s == seq as i64) {
                        // Documented `put_message` seq Conflict: the journal
                        // append committed first (bare PromptReceived — the
                        // wedge shape recovery converges), then the message
                        // insert collided with an out-of-band message already
                        // at the reserved seq. No message, no turn record.
                        self.last_event += 1;
                        self.state = MState::Preparing;
                        return StepOut::Class("Conflict");
                    }
                    self.last_event += 1;
                    let seq = seq as i64;
                    self.msgs.push((seq, "user"));
                    self.state = MState::Preparing;
                    if let Some(op_id) = ids.op_id {
                        self.start_record(op_id);
                        self.tracked.push(op_id);
                    }
                    self.claim_seq = None;
                    StepOut::PromptImmediate { event_seq: seq }
                } else {
                    self.last_event += 1;
                    self.pending += 1;
                    StepOut::Queued {
                        event_seq: self.last_event as i64,
                    }
                }
            }
            Op::Admit => {
                if self.pending == 0 || !self.state.is_admit_eligible() {
                    return StepOut::NoneAdmit;
                }
                let seq = self.last_event + 1;
                if self.msgs.iter().any(|(s, _)| *s == seq as i64) {
                    // The admission transaction materializes the message at
                    // the reserved seq; a collision rolls the WHOLE claim
                    // back (zero state change). Real classification note:
                    // `SessionHandle::admit_next_queued` maps store errors
                    // through `SessionError::from` (NOT `map_store_err`),
                    // so the UNIQUE violation surfaces as `Store` here while
                    // the `put_message` path types the same violation
                    // `Conflict` — reported as an inconsistency; the model
                    // pins the real behavior.
                    return StepOut::Class("Store");
                }
                self.pending -= 1;
                if let (Some(qs), Some(op_id)) = (ids.queue_seq, ids.op_id) {
                    self.running.push((qs, op_id));
                }
                // The admitted user message materializes at one past the
                // journal head WITHOUT consuming a journal sequence (the
                // store reserves that seq for the caller's PromptAdmitted).
                let seq = seq as i64;
                self.msgs.push((seq, "user"));
                self.state = MState::Preparing;
                if let Some(op_id) = ids.op_id {
                    self.start_record(op_id);
                    self.tracked.push(op_id);
                }
                self.claim_seq = ids.queue_seq;
                // The caller journals PromptAdmitted at the reserved seq
                // (Preparing -> Preparing self-transition) and marks the row
                // `running` — production parity, and it re-aligns the row
                // projection with the journal tail.
                self.last_event += 1;
                debug_assert_eq!(
                    self.last_event as i64, seq,
                    "reservation lands on message seq"
                );
                StepOut::Admitted { message_seq: seq }
            }
            Op::Hop(idx) => {
                let (_, to) = HOP_MENU[*idx];
                if self.state.can(to) {
                    self.last_event += 1;
                    self.state = to;
                    StepOut::HopOk
                } else {
                    StepOut::Class("InvalidState")
                }
            }
            Op::Settle => {
                if !self.state.can(MState::ReadyForNextTurn) {
                    return StepOut::Class("InvalidState");
                }
                self.last_event += 1;
                self.state = MState::ReadyForNextTurn;
                let finished = if let Some((_, s)) =
                    self.records.iter_mut().find(|(_, s)| *s == MRec::Active)
                {
                    *s = MRec::Completed;
                    true
                } else {
                    false
                };
                let claimed_done = if let Some(qs) = self.claim_seq {
                    self.running.retain(|(q, _)| *q != qs);
                    self.claim_seq = None;
                    true
                } else {
                    false
                };
                StepOut::SettleOk {
                    finished,
                    claimed_done,
                }
            }
            Op::AppendAssistant => {
                let seq = self.msgs.last().map(|(s, _)| s + 1).unwrap_or(1);
                self.msgs.push((seq, "assistant"));
                StepOut::Appended { seq }
            }
            Op::BudgetReserve(predicted) => {
                let free = self.free();
                if *predicted > free {
                    StepOut::Budget(BudgetRef::Exceeded {
                        free,
                        predicted: *predicted,
                    })
                } else {
                    self.open_micro += predicted;
                    self.reservations.push(MRes::Reserved(*predicted));
                    StepOut::Budget(BudgetRef::Ok)
                }
            }
            Op::BudgetSettle(idx, micro) => {
                let Some(r) = self.reservations.get(*idx).copied() else {
                    return StepOut::Budget(BudgetRef::NotOpen);
                };
                match r {
                    // The store settles exactly `reserved|dispatched` rows
                    // (anything else is typed `NotOpen`) and keeps the
                    // dispatch marker across the settle.
                    MRes::Reserved(p) => {
                        self.spent += micro;
                        self.open_micro = self.open_micro.saturating_sub(p);
                        self.reservations[*idx] = MRes::Settled { dispatched: false };
                        StepOut::Budget(BudgetRef::Ok)
                    }
                    MRes::Dispatched(p) => {
                        self.spent += micro;
                        self.open_micro = self.open_micro.saturating_sub(p);
                        self.reservations[*idx] = MRes::Settled { dispatched: true };
                        StepOut::Budget(BudgetRef::Ok)
                    }
                    _ => StepOut::Budget(BudgetRef::NotOpen),
                }
            }
            Op::BudgetRefund(idx) => {
                let Some(r) = self.reservations.get(*idx).copied() else {
                    return StepOut::Budget(BudgetRef::NotOpen);
                };
                match r {
                    MRes::Reserved(p) => {
                        self.open_micro = self.open_micro.saturating_sub(p);
                        self.reservations[*idx] = MRes::Refunded;
                        StepOut::Budget(BudgetRef::Ok)
                    }
                    // The SQL refund guard (`status IN ('reserved','open')
                    // AND dispatched_ms IS NULL`) refuses every row that
                    // carries the dispatch marker: `dispatched`, `uncertain`
                    // (recovery/mark_uncertain only ever reach marked rows)
                    // and `settled`-after-dispatch all surface as the typed
                    // `CannotRefundDispatched`.
                    MRes::Dispatched(_)
                    | MRes::Uncertain(_)
                    | MRes::Settled { dispatched: true } => {
                        StepOut::Budget(BudgetRef::RefundDispatched)
                    }
                    // Settled pre-dispatch and already-refunded rows: zero
                    // rows changed with a NULL marker -> typed `NotOpen`.
                    _ => StepOut::Budget(BudgetRef::NotOpen),
                }
            }
            Op::BudgetDispatch(idx) => {
                let Some(r) = self.reservations.get(*idx).copied() else {
                    return StepOut::Budget(BudgetRef::NotOpen);
                };
                match r {
                    MRes::Reserved(p) => {
                        self.reservations[*idx] = MRes::Dispatched(p);
                        StepOut::Budget(BudgetRef::Ok)
                    }
                    // A second mark of the same dispatched row is idempotent.
                    MRes::Dispatched(_) => StepOut::Budget(BudgetRef::Ok),
                    _ => StepOut::Budget(BudgetRef::NotOpen),
                }
            }
            Op::Abort => {
                if self.state.is_terminal() {
                    return StepOut::Class("Conflict");
                }
                // The first per-op landing (`Failed` -> Cancelled) must be
                // legal from the current state; from `Cancelled` itself it is
                // not (no self-loop), so a whole abort refuses with no trace.
                if !self.state.can(MState::Cancelled) {
                    return StepOut::Class("InvalidState");
                }
                // One `Failed` -> Cancelled landing per tracked op (later
                // ones are lawful self-transitions), or ONE landing when
                // nothing is tracked (the empty-registry branch), plus the
                // TurnCompleted -> ReadyForNextTurn that always follows.
                let landings = self.tracked.len().max(1) as u64;
                self.last_event += landings + 1;
                self.state = MState::ReadyForNextTurn;
                self.pending = 0;
                self.running.clear();
                self.claim_seq = None;
                // Abort unregisters every affected op (all tracked turns).
                self.tracked.clear();
                // The turn RECORD stays open after an abort (no finish call).
                StepOut::AbortOk
            }
            Op::Suspend => {
                if !self.state.can(MState::Suspended) {
                    return StepOut::Class("InvalidState");
                }
                if self.life != MLife::Open {
                    return StepOut::Class("Conflict");
                }
                self.last_event += 1;
                self.state = MState::Suspended;
                self.life = MLife::Suspended;
                StepOut::Ok
            }
            Op::Resume => {
                // The real path validates the journal transition FIRST, then
                // the store enforces the Suspended lifetime.
                if !self.state.can(MState::Idle) {
                    return StepOut::Class("InvalidState");
                }
                if self.life != MLife::Suspended {
                    return StepOut::Class("Conflict");
                }
                self.last_event += 1;
                self.state = MState::Idle;
                self.life = MLife::Open;
                StepOut::Ok
            }
            Op::Fail => {
                if self.state.can(MState::FailedRecoverable) {
                    self.last_event += 1;
                    self.state = MState::FailedRecoverable;
                    StepOut::Ok
                } else {
                    StepOut::Class("InvalidState")
                }
            }
            Op::Reset => {
                if self.state.can(MState::Idle) {
                    self.last_event += 1;
                    self.state = MState::Idle;
                    StepOut::Ok
                } else {
                    StepOut::Class("InvalidState")
                }
            }
            Op::End => {
                if self.life == MLife::Closed {
                    return StepOut::Class("Conflict");
                }
                if !self.state.can(MState::Completed) {
                    return StepOut::Class("InvalidState");
                }
                self.last_event += 1;
                self.state = MState::Completed;
                self.life = MLife::Closed;
                StepOut::Ok
            }
            Op::DeleteMsg(idx) => {
                let Some((seq, _)) = self.msgs.get(*idx as usize).copied() else {
                    return StepOut::Class("NotFound");
                };
                let newest = self.msgs.last().map(|(s, _)| *s);
                let turn_active = self.state_is_active() || self.active_record_op().is_some();
                if turn_active && newest == Some(seq) {
                    return StepOut::Class("Conflict");
                }
                self.msgs.remove(*idx as usize);
                StepOut::Ok
            }
            Op::PoisonSettle => {
                // Duplicate settle attempt: TurnCompleted->ReadyForNextTurn
                // again (legal only from a genuinely settleable state).
                let dup = if self.state.can(MState::ReadyForNextTurn) {
                    self.last_event += 1;
                    self.state = MState::ReadyForNextTurn;
                    "Ok"
                } else {
                    "InvalidState"
                };
                // Double money settle on a settled reservation.
                let money = if self
                    .reservations
                    .iter()
                    .any(|r| matches!(r, MRes::Settled { .. }))
                {
                    "NotOpen"
                } else {
                    "Skipped"
                };
                StepOut::Poison { dup, money }
            }
            Op::ConcurrentAppendSettle(mode) => {
                // The three interleavings (barrier race, append-then-settle,
                // settle-then-append) are commutative: one modeled outcome.
                let _ = mode;
                let seq = self.msgs.last().map(|(s, _)| s + 1).unwrap_or(1);
                self.msgs.push((seq, "assistant"));
                let settle = if self.state.can(MState::ReadyForNextTurn) {
                    self.last_event += 1;
                    self.state = MState::ReadyForNextTurn;
                    let finished = if let Some((_, s)) =
                        self.records.iter_mut().find(|(_, s)| *s == MRec::Active)
                    {
                        *s = MRec::Completed;
                        true
                    } else {
                        false
                    };
                    let claimed_done = if let Some(qs) = self.claim_seq {
                        self.running.retain(|(q, _)| *q != qs);
                        self.claim_seq = None;
                        true
                    } else {
                        false
                    };
                    StepOut::SettleOk {
                        finished,
                        claimed_done,
                    }
                } else {
                    StepOut::Class("InvalidState")
                };
                StepOut::Concurrent {
                    seq,
                    settle: Box::new(settle),
                }
            }
            Op::Reload => {
                *self = self.predict_reload();
                StepOut::Ok
            }
        }
    }
}

// ------------------------------------------------------------- driver alphabet

/// The deterministic interior-hop menu: (event kind, target state).
const HOP_MENU: [(EventKind, MState); 6] = [
    (EventKind::ContextPrepared, MState::BuildingContext),
    (EventKind::ModelStarted, MState::WaitingForModel),
    (EventKind::ModelChunkReceived, MState::Streaming),
    (EventKind::PhaseChanged, MState::Validating),
    (EventKind::PhaseChanged, MState::UpdatingMemory),
    (EventKind::PhaseChanged, MState::WaitingForModel),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Prompt,
    Admit,
    Hop(usize),
    Settle,
    AppendAssistant,
    BudgetReserve(u64),
    BudgetSettle(usize, u64),
    BudgetRefund(usize),
    BudgetDispatch(usize),
    Abort,
    Suspend,
    Resume,
    Fail,
    Reset,
    End,
    DeleteMsg(u64),
    PoisonSettle,
    /// 0 = barrier race, 1 = append-then-settle, 2 = settle-then-append.
    ConcurrentAppendSettle(u8),
    Reload,
}

/// One generated step. `crash` arms the store's deterministic durability
/// seam around the step's durable writes (only for crashable ops).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Step {
    op: Op,
    crash: Option<u8>,
}

const SEAM_POINTS: [&str; 5] = [
    "ev_precommit",
    "ev_committed",
    "session_command_precommit",
    "session_command_committed",
    "session_command_side_row",
];

/// The store's simulated-crash payload marker: the exact `[fault-seam]`
/// text the seam panic carries. When the seam fires on the store's writer
/// THREAD the panic never reaches the test thread — the writer service
/// wraps the payload into a typed `WriterUnavailable` refusal (and some
/// ops fold that refusal into their own error enum). Seam classification
/// matches THIS marker (plus the writer-availability probe), never generic
/// "panicked" wording.
const SEAM_MARKER: &str = "[fault-seam]";

/// Sentinel arm standing for "disarmed": `CrashSeam::trip` fires only when
/// the armed point equals the crossed boundary name, and no store boundary
/// is ever called with this name — so re-arming it can never fire.
/// `crates/store` exposes NO `crash_disarm()` on the store handle
/// (`crash_arm` -> `CrashSeam::arm` only replaces state), so the per-step
/// disarm re-arms this never-firing sentinel; the honest fix is a tiny
/// additive test-only `Store::crash_disarm()` on the store crate.
const SEAM_DISARMED: faktor_store::CrashArm = faktor_store::CrashArm {
    point: "__modelcheck_step_disarmed__",
    ordinal: 0,
};

fn crashable(op: &Op) -> bool {
    matches!(op, Op::Prompt | Op::Hop(_) | Op::Settle | Op::Abort)
}

/// Expected outcome classes. Budget outcomes carry the exact numbers the
/// refusal must name (`free`/`predicted` are part of the contract).
#[derive(Debug, Clone, PartialEq, Eq)]
enum StepOut {
    Ok,
    Class(&'static str),
    PromptImmediate {
        event_seq: i64,
    },
    Queued {
        event_seq: i64,
    },
    Admitted {
        message_seq: i64,
    },
    NoneAdmit,
    HopOk,
    SettleOk {
        finished: bool,
        claimed_done: bool,
    },
    Appended {
        seq: i64,
    },
    Budget(BudgetRef),
    AbortOk,
    Poison {
        dup: &'static str,
        money: &'static str,
    },
    Concurrent {
        seq: i64,
        settle: Box<StepOut>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetRef {
    Ok,
    Exceeded { free: u64, predicted: u64 },
    NotOpen,
    RefundDispatched,
}

/// Opaque identifiers harvested from real receipts (never interpreted by
/// the oracle beyond identity bookkeeping).
#[derive(Debug, Default, Clone, Copy)]
struct ExecIds {
    op_id: Option<u64>,
    queue_seq: Option<i64>,
}

/// The durable observable state, compared after every step.
#[derive(Debug, Clone, PartialEq)]
struct Obs {
    state: AgentState,
    life: SessionLifecycle,
    msgs: Vec<(i64, String)>,
    queued: i64,
    record_active: usize,
    record_completed: usize,
    record_failed: usize,
    record_total: usize,
    /// Sorted in-memory turn-registry ops (raw ids) of the live handle.
    tracked: Vec<u64>,
    last_event: u64,
    budget: BudgetTuple,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BudgetTuple {
    cap: Option<u64>,
    spent: u64,
    open_reserved_micro: u64,
    open_count: usize,
    uncertain_micro: u64,
    uncertain_count: usize,
    settled_count: usize,
}

// ------------------------------------------------------------------ the driver

/// Real execution results of one step.
struct ExecResult {
    out: StepOut,
    ids: ExecIds,
    /// True when the armed durability seam fired (crash observed).
    crashed: bool,
}

#[derive(Debug)]
struct Div {
    step: usize,
    detail: String,
}

/// One live process world (manager + session), droppable as a unit so a
/// reload/crash is a true process death: every Arc ends before the same
/// durable dir is reopened.
struct World {
    #[allow(dead_code)]
    manager: Arc<SessionManager>,
    ledger: Arc<DurableBudgetLedger>,
    handle: SessionHandle,
}

/// Scenario runtime. MULTI-THREADED on purpose: the DbActor's bridge task
/// runs on this runtime, and its worker threads must keep polling it while
/// the test thread joins concurrent workers (a current-thread runtime would
/// stall the envelope relay and deadlock the append leg).
fn test_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

struct Scen {
    dir: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    world: Option<World>,
    model: Model,
    sid: faktor_core::id::SessionId,
    task: faktor_core::id::TaskId,
    /// Real reservation ids, parallel to `model.reservations`.
    res_ids: Vec<ReservationId>,
    /// Every turn op id the real API ever returned: the universe over which
    /// the public `turn_cancellation` probe reconstructs the handle's
    /// in-memory turn registry (`ops()` itself is pub(crate)).
    known_ops: Vec<u64>,
    /// Message of the most recent real-side refusal (diagnostics only).
    last_err: Option<String>,
    /// Observed seam hits (steps classified `crashed`): the campaign must
    /// prove it exercised REAL crash reconstruction, never vacuous passes.
    crash_hits: usize,
}

const BUDGET_CAP: u64 = 1_000_000;

fn known_snapshot() -> PricingSnapshot {
    PricingSnapshot::exact(
        PriceQuote {
            input: MicroUsdPerMillionTokens(15_000_000),
            output: MicroUsdPerMillionTokens(60_000_000),
            cache_read: MicroUsdPerMillionTokens(3_000_000),
            cache_write: MicroUsdPerMillionTokens(7_000_000),
        },
        7,
        "modelcheck".into(),
    )
}

fn open_manager(dir: &std::path::Path) -> Arc<SessionManager> {
    SessionManager::open(dir.join("store"), dir.join("cas"), true).unwrap()
}

impl Scen {
    fn new(rt: tokio::runtime::Runtime) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let manager = open_manager(dir.path());
        let ws = manager.create_workspace("/w").unwrap();
        let handle = manager.create_session(ws, "mc", "fake", "m").unwrap();
        let sid = handle.id();
        let task = handle.task_id().unwrap();
        let task_row = Task {
            task_id: task,
            session_id: sid,
            goal: "modelcheck".into(),
            created_ms: 1,
            updated_ms: 1,
            ..Default::default()
        };
        handle.create_task(task_row).unwrap();
        let ledger = DurableBudgetLedger::new(manager.clone());
        ledger
            .set_task_max_cost(sid, task, Some(BUDGET_CAP))
            .unwrap();
        Self {
            dir,
            rt,
            world: Some(World {
                manager,
                ledger,
                handle,
            }),
            model: Model::new(BUDGET_CAP),
            sid,
            task,
            res_ids: Vec::new(),
            known_ops: Vec::new(),
            last_err: None,
            crash_hits: 0,
        }
    }

    /// Classified real-side refusal: stash the full message for divergence
    /// reports, return the outcome class the model compares against.
    fn fail(&mut self, e: faktor_core::Error) -> StepOut {
        let kind = class_of(&e);
        self.last_err = Some(e.to_string());
        StepOut::Class(kind)
    }

    fn w(&self) -> &World {
        self.world.as_ref().expect("live world")
    }

    fn observe(&self) -> Obs {
        let w = self.w();
        let state = w.handle.state().unwrap();
        let life = w.handle.lifecycle().unwrap();
        // Full ascending message list, paged (paging is fundamental).
        let mut newest_first: Vec<(i64, String)> = Vec::new();
        let mut before: Option<i64> = None;
        loop {
            let page = w.handle.messages_before(before, 200).unwrap();
            if page.is_empty() {
                break;
            }
            before = page.last().map(|m| m.seq);
            for m in &page {
                newest_first.push((m.seq, m.role.clone()));
            }
            if page.len() < 200 {
                break;
            }
        }
        newest_first.reverse();
        let queued = w.handle.queued_prompt_count().unwrap();
        let recs = w.handle.turn_records().unwrap();
        let record_active = recs.iter().filter(|r| r.status == "active").count();
        let record_completed = recs.iter().filter(|r| r.status == "completed").count();
        let record_failed = recs.iter().filter(|r| r.status == "failed").count();
        let record_total = recs.len();
        // Reconstruct the in-memory turn registry through its public
        // membership probe: an op is tracked iff `turn_cancellation(op)`
        // yields a live token. The universe is every turn op the API
        // returned (restarts re-probe against the fresh handle, which
        // tracks nothing — matching the model's cleared registry).
        let mut tracked: Vec<u64> = self
            .known_ops
            .iter()
            .copied()
            .filter(|op| w.handle.turn_cancellation(OpId::new(*op)).is_some())
            .collect();
        tracked.sort_unstable();
        let last_event = w.handle.last_event_seq().unwrap().unwrap().raw();
        let view = w.ledger.session_budget_view(self.sid, self.task).unwrap();
        Obs {
            state,
            life,
            msgs: newest_first,
            queued,
            record_active,
            record_completed,
            record_failed,
            record_total,
            tracked,
            last_event,
            budget: BudgetTuple {
                cap: view.max_cost_micro,
                spent: view.spent_cost_micro,
                open_reserved_micro: view.open_reserved_micro,
                open_count: view.open_reservations,
                uncertain_micro: view.uncertain_reserved_micro,
                uncertain_count: view.uncertain_reservations,
                settled_count: view.settled_count,
            },
        }
    }

    fn observe_records(&self) -> Vec<(u64, MRec)> {
        self.w()
            .handle
            .turn_records()
            .unwrap()
            .into_iter()
            .map(|r| {
                let status = match r.status.as_str() {
                    "active" => MRec::Active,
                    "completed" => MRec::Completed,
                    _ => MRec::Failed,
                };
                (r.turn_op_id.raw(), status)
            })
            .collect()
    }

    /// Durable prompt-queue rows: (queue seq, status, row op), oldest first.
    fn observe_queue(&self) -> Vec<(i64, String, u64)> {
        self.w()
            .handle
            .queue_rows(256)
            .unwrap()
            .into_iter()
            .map(|q| (q.queue_seq, q.status, q.op_id.raw()))
            .collect()
    }

    fn check_obs(&self, at: usize) -> Result<(), Div> {
        let real = self.observe();
        let want = self.model.obs();
        if real != want {
            return Err(Div {
                step: at,
                detail: format!("observable divergence\n  model: {want:#?}\n  real:  {real:#?}"),
            });
        }
        // The durable journal must always replay to the modeled state.
        let replay = self.w().handle.replay_journal().map_err(|e| Div {
            step: at,
            detail: format!("journal replay refused: {e}"),
        })?;
        if MState::from_real(replay.state) != self.model.state {
            return Err(Div {
                step: at,
                detail: format!(
                    "journal replay state {:?} diverges from modeled state {:?}",
                    replay.state, self.model.state
                ),
            });
        }
        Ok(())
    }

    fn exec(&mut self, step: &Step) -> Result<ExecResult, Div> {
        // Refusal text stashed by an earlier CRASH step is stale (that path
        // never compares outcomes): clearing it here keeps seam
        // classification scoped to THIS step's typed refusals only.
        self.last_err = None;
        if let Some(crash) = step.crash.filter(|_| crashable(&step.op)) {
            let v = crash as usize;
            let point = SEAM_POINTS[(v / 5) % SEAM_POINTS.len()];
            let ordinal = (v % 5) as u64;
            self.w()
                .manager
                .store()
                .crash_arm(faktor_store::CrashArm { point, ordinal });
        }
        // STRICT ARM SCOPING: `crash: Some(v)` promises only that THIS
        // step's durable writes cross (point, ordinal) — if the op never
        // crosses it, the step is clean and the arm must NOT survive into
        // step N+1 (arm semantics are per-step; nothing may fire inside a
        // later step). The store handle is pinned BEFORE execution because
        // a `Reload` step legitimately ends the world inside `exec_op`; the
        // pinned `Arc<Store>` keeps that writer alive and observable.
        let store = self.w().manager.store();
        let op = step.op;
        let caught = catch_unwind(AssertUnwindSafe(|| self.exec_op(&op)));
        // Seam classification on THREE surfaces, each keyed on the store's
        // exact `[fault-seam]` payload marker or the writer's health —
        // never on generic "panicked" text:
        //  (i) the seam panic unwound THIS (test) thread;
        //  (ii) it fired on the store's writer thread: the panic never
        //       reaches `caught`, the durable authority stops admitting
        //       mutations, and the caller receives the typed refusal;
        //  (iii) a typed refusal whose text embeds the exact payload
        //       (stashed by `fail`, even when the seam symptom arrived as
        //       a normal `Ok` outcome carrying `last_err`).
        let payload_seam = caught.as_ref().err().is_some_and(|payload| {
            payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .is_some_and(|m| m.contains(SEAM_MARKER))
        });
        let typed_seam = self
            .last_err
            .as_deref()
            .is_some_and(|m| m.contains(SEAM_MARKER));
        let writer_down = caught.is_ok() && !store.writer_available();
        let crashed = payload_seam || typed_seam || writer_down;
        let result = match caught {
            Ok(Ok(res)) => Ok(ExecResult {
                out: res.out,
                ids: res.ids,
                crashed,
            }),
            // An `exec_op`-level Div whose detail carries the seam payload
            // is the SAME crash surfacing through a typed-error -> Div
            // adapter: reconstruct instead of reporting a divergence.
            Ok(Err(div)) if crashed || div.detail.contains(SEAM_MARKER) => Ok(ExecResult {
                out: StepOut::Ok,
                ids: ExecIds::default(),
                crashed: true,
            }),
            Ok(Err(div)) => Err(div),
            Err(payload) if crashed => {
                let _ = payload;
                Ok(ExecResult {
                    out: StepOut::Ok,
                    ids: ExecIds::default(),
                    crashed: true,
                })
            }
            Err(payload) => Err(Div {
                step: 0,
                detail: format!(
                    "op panicked without the armed seam firing: {:?}",
                    payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("<non-string panic>")
                ),
            }),
        };
        // Per-step disarm: a clean outcome means the armed boundary never
        // fired inside its own step, so replace the arm with the
        // never-firing sentinel (see `SEAM_DISARMED`; the store crate
        // should expose a real `crash_disarm()`). Err outcomes end the run
        // (no later step can inherit the arm), so they need no disarm.
        if matches!(&result, Ok(res) if !res.crashed) {
            store.crash_arm(SEAM_DISARMED);
        }
        result
    }

    fn exec_op(&mut self, op: &Op) -> Result<ExecResult, Div> {
        // Cheap clones: the handle/ledger are Arc-backed, so no borrow of
        // `self` is held across the arms below.
        let handle = self.w().handle.clone();
        let ledger = self.w().ledger.clone();
        let sid = self.sid;
        let task = self.task;
        let mut ids = ExecIds::default();
        let out = match op {
            Op::Prompt => match handle.submit_prompt("p", &[]) {
                Ok(r) => {
                    ids.op_id = Some(r.op_id.raw());
                    if r.queued {
                        StepOut::Queued {
                            event_seq: r.event_seq.raw() as i64,
                        }
                    } else {
                        StepOut::PromptImmediate {
                            event_seq: r.event_seq.raw() as i64,
                        }
                    }
                }
                Err(e) => self.fail(e),
            },
            Op::Admit => match handle.admit_next_queued() {
                Ok(Some(a)) => {
                    ids.op_id = Some(a.op_id.raw());
                    ids.queue_seq = Some(a.queue_seq);
                    // Production parity (agent queue runner): journal
                    // PromptAdmitted at the reserved seq, then mark the row
                    // `running`.
                    let ev = handle
                        .append_event(
                            EventKind::PromptAdmitted,
                            AgentState::Preparing,
                            Some(a.op_id),
                            Some(serde_json::json!({
                                "queue_seq": a.queue_seq,
                                "message_seq": a.message_seq,
                            })),
                        )
                        .map_err(|e| Div {
                            step: 0,
                            detail: format!("PromptAdmitted journal failed: {e}"),
                        })?;
                    if ev.raw() as i64 != a.message_seq {
                        return Err(Div {
                            step: 0,
                            detail: format!(
                                "admission reservation mismatch: PromptAdmitted landed at seq {}, \
                                 message reserved at {}",
                                ev.raw(),
                                a.message_seq
                            ),
                        });
                    }
                    handle
                        .mark_queued_status(a.queue_seq, "running")
                        .map_err(|e| Div {
                            step: 0,
                            detail: format!("mark running failed: {e}"),
                        })?;
                    StepOut::Admitted {
                        message_seq: a.message_seq,
                    }
                }
                Ok(None) => StepOut::NoneAdmit,
                Err(e) => self.fail(e),
            },
            Op::Hop(idx) => {
                let (kind, to) = HOP_MENU[*idx];
                match handle.append_event(kind, to.as_real(), None, None) {
                    Ok(_) => StepOut::HopOk,
                    Err(e) => self.fail(e),
                }
            }
            Op::Settle => {
                let record_op = self.model.active_record_op();
                let claim_seq = self.model.claim_seq;
                match handle.append_event(
                    EventKind::TurnCompleted,
                    AgentState::ReadyForNextTurn,
                    record_op.map(OpId::new),
                    None,
                ) {
                    Err(e) => self.fail(e),
                    Ok(_) => {
                        let finished = match record_op {
                            Some(op_id) => handle
                                .finish_turn_record(OpId::new(op_id), "completed")
                                .map_err(|e| Div {
                                    step: 0,
                                    detail: format!("finish_turn_record failed: {e}"),
                                })?,
                            None => false,
                        };
                        match claim_seq {
                            Some(queue_seq) => {
                                handle
                                    .mark_queued_status(queue_seq, "done")
                                    .map_err(|e| Div {
                                        step: 0,
                                        detail: format!("mark_queued_status failed: {e}"),
                                    })?;
                                StepOut::SettleOk {
                                    finished,
                                    claimed_done: true,
                                }
                            }
                            None => StepOut::SettleOk {
                                finished,
                                claimed_done: false,
                            },
                        }
                    }
                }
            }
            Op::AppendAssistant => {
                let seq = handle.proposed_message_seq().map_err(|e| Div {
                    step: 0,
                    detail: format!("proposed_message_seq failed: {e}"),
                })?;
                match self.rt.block_on(handle.append_message(
                    seq,
                    "assistant",
                    serde_json::json!({ "parts": [] }),
                )) {
                    Ok(_) => StepOut::Appended { seq },
                    Err(e) => self.fail(e),
                }
            }
            Op::BudgetReserve(predicted) => {
                let res_op = OpId::new(1_000_000_000 + self.res_ids.len() as u64 + 1);
                match self.rt.block_on(ledger.reserve(
                    sid,
                    task,
                    res_op,
                    *predicted,
                    Some(known_snapshot()),
                    ExecutionClass::Unknown,
                )) {
                    Ok(id) => {
                        self.res_ids.push(id);
                        StepOut::Budget(BudgetRef::Ok)
                    }
                    Err(BudgetError::BudgetExceeded { free, predicted }) => {
                        StepOut::Budget(BudgetRef::Exceeded { free, predicted })
                    }
                    Err(_) => StepOut::Budget(BudgetRef::NotOpen),
                }
            }
            Op::BudgetSettle(idx, micro) => {
                let Some(id) = self.res_ids.get(*idx).copied() else {
                    return Ok(ExecResult {
                        out: StepOut::Budget(BudgetRef::NotOpen),
                        ids,
                        crashed: false,
                    });
                };
                match self
                    .rt
                    .block_on(ledger.settle_usage(sid, id, 0, 0, 0, 0, Some(*micro), None))
                {
                    Ok(_) => StepOut::Budget(BudgetRef::Ok),
                    Err(_) => StepOut::Budget(BudgetRef::NotOpen),
                }
            }
            Op::BudgetRefund(idx) => {
                let Some(id) = self.res_ids.get(*idx).copied() else {
                    return Ok(ExecResult {
                        out: StepOut::Budget(BudgetRef::NotOpen),
                        ids,
                        crashed: false,
                    });
                };
                match self.rt.block_on(ledger.refund(sid, id)) {
                    Ok(()) => StepOut::Budget(BudgetRef::Ok),
                    Err(BudgetError::CannotRefundDispatched { .. }) => {
                        StepOut::Budget(BudgetRef::RefundDispatched)
                    }
                    Err(_) => StepOut::Budget(BudgetRef::NotOpen),
                }
            }
            Op::BudgetDispatch(idx) => {
                let Some(id) = self.res_ids.get(*idx).copied() else {
                    return Ok(ExecResult {
                        out: StepOut::Budget(BudgetRef::NotOpen),
                        ids,
                        crashed: false,
                    });
                };
                match self.rt.block_on(ledger.mark_dispatched(sid, id)) {
                    Ok(()) => StepOut::Budget(BudgetRef::Ok),
                    Err(_) => StepOut::Budget(BudgetRef::NotOpen),
                }
            }
            Op::Abort => match handle.abort(None) {
                Ok(_) => StepOut::AbortOk,
                Err(e) => self.fail(e),
            },
            Op::Suspend => match handle.suspend() {
                Ok(_) => StepOut::Ok,
                Err(e) => self.fail(e),
            },
            Op::Resume => match handle.resume(AgentState::Idle) {
                Ok(_) => StepOut::Ok,
                Err(e) => self.fail(e),
            },
            Op::Fail => match handle.mark_failed(false, "poison sentinel") {
                Ok(_) => StepOut::Ok,
                Err(e) => self.fail(e),
            },
            Op::Reset => match handle.reset() {
                Ok(_) => StepOut::Ok,
                Err(e) => self.fail(e),
            },
            Op::End => match handle.end_session() {
                Ok(_) => StepOut::Ok,
                Err(e) => self.fail(e),
            },
            Op::DeleteMsg(idx) => {
                let seq = self
                    .model
                    .msgs
                    .get(*idx as usize)
                    .map(|(s, _)| *s)
                    .unwrap_or(i64::MAX - 7);
                match handle.delete_message(seq) {
                    Ok(()) => StepOut::Ok,
                    Err(e) => self.fail(e),
                }
            }
            Op::PoisonSettle => {
                let dup = match handle.append_event(
                    EventKind::TurnCompleted,
                    AgentState::ReadyForNextTurn,
                    self.model.active_record_op().map(OpId::new),
                    None,
                ) {
                    Ok(_) => "Ok",
                    Err(e) => {
                        self.last_err = Some(e.to_string());
                        class_of(&e)
                    }
                };
                let settled = self.settled_reservation();
                let money = match settled {
                    None => "Skipped",
                    Some(id) => {
                        match self.rt.block_on(ledger.settle_usage(
                            sid,
                            id,
                            0,
                            0,
                            0,
                            0,
                            Some(1),
                            None,
                        )) {
                            Ok(_) => "Ok",
                            Err(_) => "NotOpen",
                        }
                    }
                };
                StepOut::Poison { dup, money }
            }
            Op::ConcurrentAppendSettle(mode) => {
                let out = self.exec_concurrent(*mode)?;
                return Ok(ExecResult {
                    out,
                    ids,
                    crashed: false,
                });
            }
            Op::Reload => {
                // End the world now; the model applies `predict_reload`
                // afterwards and the restarted world is checked against it.
                drop(self.world.take());
                return Ok(ExecResult {
                    out: StepOut::Ok,
                    ids,
                    crashed: false,
                });
            }
        };
        if let Some(op_id) = ids.op_id {
            // Set-everywhere: a queued prompt returns its op id AND the same
            // id arrives again at admission — the universe must not double
            // count it (the tracked probe below would list it twice).
            if !self.known_ops.contains(&op_id) {
                self.known_ops.push(op_id);
            }
        }
        Ok(ExecResult {
            out,
            ids,
            crashed: false,
        })
    }

    fn settled_reservation(&self) -> Option<ReservationId> {
        let ledger = self.w().ledger.clone();
        for id in self.res_ids.iter().rev() {
            let state = ledger.reservation_state(self.sid, *id);
            if matches!(state, Ok(ReservationState::Settled)) {
                return Some(*id);
            }
        }
        None
    }

    /// One interleaving of append-assistant vs settle-composite:
    /// mode 0 = barrier race, 1 = append completes first, 2 = settle first.
    fn exec_concurrent(&self, mode: u8) -> Result<StepOut, Div> {
        let handle_a = self.w().handle.clone();
        let handle_b = self.w().handle.clone();
        let record_op = self.model.active_record_op();
        let claim_seq = self.model.claim_seq;
        let barrier = Arc::new(std::sync::Barrier::new(2));
        // The append leg enters the SHARED scenario runtime: its actor
        // bridge task lives there, and that runtime's worker threads keep
        // polling it while this test thread joins the workers (a private
        // per-thread runtime would either orphan the shared bridge or
        // deadlock the relay).
        let rt_handle = self.rt.handle().clone();

        let b_a = barrier.clone();
        let t_append = std::thread::spawn(move || {
            if mode == 0 {
                b_a.wait();
            }
            rt_handle.block_on(async {
                let seq = handle_a.proposed_message_seq()?;
                handle_a
                    .append_message(seq, "assistant", serde_json::json!({ "parts": [] }))
                    .await?;
                Ok::<i64, faktor_core::Error>(seq)
            })
        });
        let b_b = barrier;
        let t_settle = std::thread::spawn(move || {
            if mode == 0 {
                b_b.wait();
            }
            let ev = handle_b.append_event(
                EventKind::TurnCompleted,
                AgentState::ReadyForNextTurn,
                record_op.map(OpId::new),
                None,
            );
            match ev {
                Err(e) => Err(class_of(&e)),
                Ok(_) => {
                    let finished = match record_op {
                        Some(op_id) => {
                            match handle_b.finish_turn_record(OpId::new(op_id), "completed") {
                                Ok(b) => b,
                                Err(e) => return Err(class_of(&e)),
                            }
                        }
                        None => false,
                    };
                    match claim_seq {
                        Some(qs) => match handle_b.mark_queued_status(qs, "done") {
                            Ok(()) => Ok((finished, true)),
                            Err(e) => Err(class_of(&e)),
                        },
                        None => Ok((finished, false)),
                    }
                }
            }
        });

        let append_res = t_append.join().unwrap();
        let settle_res = t_settle.join().unwrap();

        let seq = append_res.map_err(|e| Div {
            step: 0,
            detail: format!("concurrent append failed: {e}"),
        })?;
        let settle = match settle_res {
            Ok((finished, claimed_done)) => StepOut::SettleOk {
                finished,
                claimed_done,
            },
            Err(cls) => StepOut::Class(cls),
        };
        Ok(StepOut::Concurrent {
            seq,
            settle: Box::new(settle),
        })
    }

    /// Process death and reopen: rebuild the world from the same durable
    /// dir and run the startup recovery legs (budget + queue rows).
    fn reopen_world(&mut self) -> Result<(), Div> {
        let manager = open_manager(self.dir.path());
        let ledger = DurableBudgetLedger::new(manager.clone());
        ledger.recover_after_restart();
        let handle = manager
            .get_session(self.sid)
            .map_err(|e| Div {
                step: 0,
                detail: format!("reopen get_session: {e}"),
            })?
            .ok_or_else(|| Div {
                step: 0,
                detail: "session lost across reopen".into(),
            })?;
        handle.recover_queued_rows().map_err(|e| Div {
            step: 0,
            detail: format!("recover_queued_rows: {e}"),
        })?;
        self.world = Some(World {
            manager,
            ledger,
            handle,
        });
        Ok(())
    }

    /// Clean restart: drop the world, reopen, run `recover_all`, and
    /// advance the model by the exact restart prediction.
    fn reload(&mut self) -> Result<(), Div> {
        let predicted = self.model.predict_reload();
        drop(self.world.take());
        self.reopen_world()?;
        self.w().handle.recover_all().map_err(|e| Div {
            step: 0,
            detail: format!("recover_all: {e}"),
        })?;
        self.model = predicted;
        Ok(())
    }

    /// Crash at a durable boundary: the interrupted op must leave the
    /// durable world inside its atomic old/new window, money recovery must
    /// match the model exactly, recovery must land on the modeled crash
    /// target, and a second sweep must append nothing.
    fn crash_reconstruct(&mut self, step: &Step, exec: &ExecResult) -> Result<(), Div> {
        let pre = self.model.clone();
        let mut post = pre.clone();
        let _ = post.apply(&step.op, &exec.ids);

        // Process death and reopen.
        drop(self.world.take());
        self.reopen_world()?;

        // Membership: durable messages lie inside the atomic old/new window.
        let real = self.observe();
        if real.msgs != pre.msgs_typed() && real.msgs != post.msgs_typed() {
            return Err(Div {
                step: 0,
                detail: format!(
                    "crash window violation: durable messages outside old/new window\n  \
                     pre: {:?}\n  post: {:?}\n  real: {:?}",
                    pre.msgs, post.msgs, real.msgs
                ),
            });
        }
        // The reopen already ran `recover_queued_rows`, which retires
        // running rows whose turn record ended — apply the SAME transform
        // to both candidate worlds (on clones; records/money are untouched
        // by queue recovery) before comparing counts.
        let mut pre_rec = pre.clone();
        pre_rec.recover_queue();
        let mut post_rec = post.clone();
        post_rec.recover_queue();
        let pre_queued = pre_rec.queued() as i64;
        let post_queued = post_rec.queued() as i64;
        if real.queued != pre_queued && real.queued != post_queued {
            return Err(Div {
                step: 0,
                detail: format!(
                    "crash window violation: queued count {} outside {{{pre_queued}, {post_queued}}}",
                    real.queued
                ),
            });
        }
        // Turn-record totals/completions: only settle creates completions
        // and only admission/prompts create rows — both old/new worlds.
        if real.record_total != pre.records.len() && real.record_total != post.records.len() {
            return Err(Div {
                step: 0,
                detail: format!(
                    "crash window violation: turn-record total {} outside {{{}, {}}}",
                    real.record_total,
                    pre.records.len(),
                    post.records.len()
                ),
            });
        }
        if real.record_completed != pre.count_completed()
            && real.record_completed != post.count_completed()
        {
            return Err(Div {
                step: 0,
                detail: format!(
                    "crash window violation: completed records {} outside {{{}, {}}}",
                    real.record_completed,
                    pre.count_completed(),
                    post.count_completed()
                ),
            });
        }

        // Money recovery is exact: open reserved -> refunded, dispatched ->
        // uncertain, spend never rewinds.
        post.recover_money();
        let want_budget = post.budget_tuple();
        let got_budget = real.budget;
        if got_budget != want_budget {
            return Err(Div {
                step: 0,
                detail: format!(
                    "crash money recovery divergence\n  model: {want_budget:?}\n  real:  {got_budget:?}"
                ),
            });
        }

        // Machine recovery: from the REOPENED durable state an op-active
        // machine lands on the crash target with one `CrashDetected`; any
        // open turn record is finalized failed; a second sweep must append
        // nothing.
        let s0 = self.w().handle.state().unwrap();
        let rec0 = self
            .w()
            .handle
            .active_turn_record()
            .map_err(|e| Div {
                step: 0,
                detail: format!("active_turn_record: {e}"),
            })?
            .is_some();
        self.w().handle.recover_all().map_err(|e| Div {
            step: 0,
            detail: format!("recover_all after crash: {e}"),
        })?;
        let expect_state = if MState::from_real(s0).is_op_active() {
            MState::from_real(s0).crash_target().as_real()
        } else {
            s0
        };
        let got_state = self.w().handle.state().unwrap();
        if got_state != expect_state {
            return Err(Div {
                step: 0,
                detail: format!(
                    "crash recovery landing divergence: from {s0:?} expected {expect_state:?}, \
                     got {got_state:?}"
                ),
            });
        }
        if self
            .w()
            .handle
            .active_turn_record()
            .map_err(|e| Div {
                step: 0,
                detail: format!("active_turn_record: {e}"),
            })?
            .is_some()
        {
            return Err(Div {
                step: 0,
                detail: "recovery left an active turn record".into(),
            });
        }
        let ev1 = self.w().handle.last_event_seq().unwrap().unwrap().raw();
        self.w().handle.recover_all().map_err(|e| Div {
            step: 0,
            detail: format!("second recover_all: {e}"),
        })?;
        let ev2 = self.w().handle.last_event_seq().unwrap().unwrap().raw();
        if ev1 != ev2 {
            return Err(Div {
                step: 0,
                detail: format!(
                    "recovery is not idempotent: events {ev1} -> {ev2} on a second sweep"
                ),
            });
        }

        // Resynchronize the model from the validated durable world and keep
        // driving; the exact-equality checks resume from here.
        let real = self.observe();
        let records = self.observe_records();
        let queue = self.observe_queue();
        self.model = resync_model(&real, &post, rec0, &records, &queue);
        Ok(())
    }
}

/// Rebuild the model from a crash-validated durable observation: messages
/// and money come from the checked world; the record bookkeeping is read
/// straight from the durable rows (recovery may have closed records the
/// interrupted op never created).
fn resync_model(
    obs: &Obs,
    post: &Model,
    rec0: bool,
    records: &[(u64, MRec)],
    queue: &[(i64, String, u64)],
) -> Model {
    let mut m = post.clone();
    m.state = MState::from_real(obs.state);
    m.life = MLife::from_real(obs.life);
    m.last_event = obs.last_event;
    m.msgs = obs
        .msgs
        .iter()
        .map(|(s, role)| {
            let r: &'static str = match role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                _ => "system",
            };
            (*s, r)
        })
        .collect();
    // The durable queue split is rebuilt from the rows themselves: pending
    // rows vs claimed/running rows (with their ops for later retirement).
    m.pending = queue
        .iter()
        .filter(|(_, status, _)| status == "pending")
        .count() as u64;
    m.running = queue
        .iter()
        .filter(|(_, status, _)| status == "claimed" || status == "running")
        .map(|(qs, _, op)| (*qs, *op))
        .collect();
    m.claim_seq = None;
    m.records = records.to_vec();
    // Fresh handle: the in-memory registry does not survive a restart.
    m.tracked.clear();
    let _ = rec0;
    // Money: already recovered on `post` and checked against the tuple.
    m.spent = obs.budget.spent;
    m.open_micro = obs.budget.open_reserved_micro;
    m.uncertain_micro = obs.budget.uncertain_micro;
    m
}

fn class_of(e: &faktor_core::Error) -> &'static str {
    use faktor_core::ErrorKind::*;
    match e.kind {
        InvalidState { .. } => "InvalidState",
        Conflict => "Conflict",
        NotFound => "NotFound",
        Permission => "Permission",
        Timeout => "Timeout",
        Cancelled => "Cancelled",
        Store => "Store",
        Malformed => "Malformed",
        Oversized => "Oversized",
        _ => "Other",
    }
}

// ---------------------------------------------------- step driver + shrinking

fn run_step(scen: &mut Scen, step: &Step, at: usize) -> Result<(), Div> {
    let exec = scen.exec(step)?;
    if exec.crashed {
        scen.crash_hits += 1;
        return scen.crash_reconstruct(step, &exec);
    }
    // The oracle applies the op and derives the expected outcome+world.
    let mut post = scen.model.clone();
    let pred = post.apply(&step.op, &exec.ids);
    if pred != exec.out {
        let detail = format!(
            "outcome divergence on {step:?}\n  model predicted: {pred:?}\n  real produced:  {:?}{}",
            exec.out,
            scen.last_err
                .take()
                .map(|m| format!("\n  real error: {m}"))
                .unwrap_or_default()
        );
        return Err(Div { step: at, detail });
    }
    scen.last_err = None;
    scen.model = post;
    // After a reload step the world was dropped; rebuild it now that the
    // model carries the predicted restart state.
    if matches!(step.op, Op::Reload) {
        scen.reopen_world()?;
        scen.w().handle.recover_all().map_err(|e| Div {
            step: at,
            detail: format!("recover_all after reload: {e}"),
        })?;
    }
    scen.check_obs(at)
}

/// Run one sequence end to end; on success returns the number of observed
/// seam hits (crash reconstructions actually exercised).
fn run_sequence(steps: &[Step]) -> Result<usize, Div> {
    let rt = test_rt();
    let mut scen = Scen::new(rt);
    for (at, step) in steps.iter().enumerate() {
        run_step(&mut scen, step, at).map_err(|mut d| {
            if d.step == 0 {
                d.step = at;
            }
            d
        })?;
    }
    // Final forced reload: the whole durable world must survive the restart
    // exactly as modeled.
    scen.reload().map_err(|mut d| {
        if d.step == 0 {
            d.step = steps.len();
        }
        d
    })?;
    scen.check_obs(steps.len())?;
    Ok(scen.crash_hits)
}

fn gen_sequence(seed: u64, len: usize, crash_every: usize) -> Vec<Step> {
    let mut rng = Rng::new(seed);
    let mut steps = Vec::with_capacity(len);
    for i in 0..len {
        let op = match rng.below(100) {
            0..=17 => Op::Prompt,
            18..=37 => Op::Hop(rng.below(HOP_MENU.len() as u64) as usize),
            38..=47 => Op::Settle,
            48..=57 => Op::AppendAssistant,
            58..=65 => Op::Admit,
            66..=73 => {
                // Mix small reserves with occasional oversubscription attempts.
                if rng.below(4) == 0 {
                    Op::BudgetReserve(BUDGET_CAP + 1 + rng.below(10_000))
                } else {
                    Op::BudgetReserve(10 + rng.below(200))
                }
            }
            74..=79 => Op::BudgetSettle(rng.below(6) as usize, 5 + rng.below(50)),
            80..=82 => Op::BudgetRefund(rng.below(6) as usize),
            83..=85 => Op::BudgetDispatch(rng.below(6) as usize),
            86..=90 => Op::Abort,
            91..=92 => Op::Suspend,
            93 => Op::Resume,
            94..=95 => Op::Fail,
            96 => Op::Reset,
            97 => Op::End,
            98 => Op::DeleteMsg(rng.below(8)),
            _ => {
                if rng.below(2) == 0 {
                    Op::PoisonSettle
                } else {
                    Op::ConcurrentAppendSettle(rng.below(3) as u8)
                }
            }
        };
        let crash = if crash_every > 0 && crashable(&op) && i % crash_every == crash_every - 1 {
            Some((rng.below(5) * 5 + rng.below(2)) as u8)
        } else {
            None
        };
        steps.push(Step { op, crash });
    }
    // Sprinkle clean reloads at deterministic positions.
    for at in [len / 3, (2 * len) / 3] {
        if at < steps.len() {
            steps[at] = Step {
                op: Op::Reload,
                crash: None,
            };
        }
    }
    steps
}

/// Delta-debugging style shrink: repeatedly drop chunks while the sequence
/// still diverges; always reports the smallest failing sequence found.
fn shrink<F>(steps: &[Step], fails: F) -> Vec<Step>
where
    F: Fn(&[Step]) -> bool,
{
    let mut best = steps.to_vec();
    let mut budget = 300usize;
    let mut chunk = best.len() / 2;
    while chunk >= 1 && budget > 0 {
        let mut i = 0;
        while i < best.len() && budget > 0 {
            let mut cand = best.clone();
            let end = (i + chunk).min(cand.len());
            cand.drain(i..end);
            budget -= 1;
            if !cand.is_empty() && fails(&cand) {
                best = cand;
            } else {
                i += chunk;
            }
        }
        chunk /= 2;
    }
    best
}

/// One seeded campaign: returns the observed seam-hit count so the caller
/// can assert the campaign was NOT vacuous (zero crashes would let a
/// broken crash path pass silently).
fn campaign(seed: u64, len: usize, crash_every: usize) -> usize {
    let steps = gen_sequence(seed, len, crash_every);
    let hits = match run_sequence(&steps) {
        Ok(hits) => hits,
        Err(div) => {
            let minimized = shrink(&steps, |cand| run_sequence(cand).is_err());
            let replay = run_sequence(&minimized).err();
            panic!(
                "session modelcheck divergence\n  seed: {seed:#x}\n  step: {}\n  detail: {}\n  \
                 full sequence ({} steps):\n{:#?}\n  minimized sequence ({} steps):\n{:#?}\n  \
                 minimized replay: {:?}",
                div.step,
                div.detail,
                steps.len(),
                steps,
                minimized.len(),
                minimized,
                replay.map(|d| d.detail),
            )
        }
    };
    hits
}

// ------------------------------------------------------------------- tests

/// Fast seeded subset: runs on every normal `cargo test -p faktor-session`
/// and must stay well inside the ~60s bound. The seam-hit assertion makes a
/// vacuous run (zero observed crashes) a FAILURE, never a silent pass.
#[test]
fn session_modelcheck_fast_seeded() {
    let mut hits = 0usize;
    for seed in [0xA11CEu64, 0xB0B, 0xC0FFEE, 0xD15EA5E, 0xE7E7, 0xF00D] {
        hits += campaign(seed, 50, 9);
    }
    println!("session_modelcheck_fast_seeded: {hits} seam hits observed (non-vacuity)");
    assert!(
        hits > 0,
        "vacuous campaign: no seam hit was ever observed across the fast seeds — \
         crash reconstruction never ran"
    );
}

/// Long exhaustive campaign: longer sequences, denser crash points. The
/// seam-hit assertion proves MANY real crashes were exercised (crash_every
/// arming must keep landing, even with strict per-step arm scoping).
#[test]
#[ignore = "[fault] HAR-09 long model check: longer seeded sequences with crash-at-boundary reconstruction; run manually with `cargo test -p faktor-session --test session_modelcheck -- --ignored --nocapture`"]
fn session_modelcheck_long_exhaustive() {
    let mut hits = 0usize;
    for seed in 0x1000u64..0x1018 {
        hits += campaign(seed, 240, 4);
    }
    println!("session_modelcheck_long_exhaustive: {hits} seam hits observed (non-vacuity)");
    assert!(
        hits > 0,
        "vacuous campaign: no seam hit was ever observed across the long seeds — \
         crash reconstruction never ran"
    );
}

/// Budget oversubscription: reserves are granted only up to the exact free
/// balance; every refusal names the exact `free`/`predicted` pair and
/// writes nothing.
#[test]
fn budget_oversubscription_is_refused_exactly() {
    let rt = test_rt();
    let mut scen = Scen::new(rt);
    let mut rng = Rng::new(0xDEAD_BEEF);
    for i in 0..40 {
        let predicted = if i % 3 == 0 {
            BUDGET_CAP + 1 + rng.below(1_000_000)
        } else {
            50_000 + rng.below(100_000)
        };
        let step = Step {
            op: Op::BudgetReserve(predicted),
            crash: None,
        };
        run_step(&mut scen, &step, 0).expect("model and runtime agree on every reserve");
    }
    // The oversubscription invariant: free is exactly
    // cap - spent - open - uncertain, on both sides.
    let model_free = scen.model.free();
    let real_free = scen
        .w()
        .ledger
        .session_budget_view(scen.sid, scen.task)
        .unwrap()
        .free();
    assert_eq!(model_free, real_free, "free balance must agree exactly");
    let before = scen.observe();
    let err = scen
        .rt
        .block_on(scen.w().ledger.reserve(
            scen.sid,
            scen.task,
            OpId::new(4_000_000_001),
            model_free + 1,
            Some(known_snapshot()),
            ExecutionClass::Unknown,
        ))
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: model_free,
            predicted: model_free + 1
        },
        "the refusal must name the exact free/predicted pair"
    );
    // A refusal writes nothing.
    assert_eq!(scen.observe(), before, "refused reserve leaves no trace");
    // Exactly at free is granted.
    let granted = scen
        .rt
        .block_on(scen.w().ledger.reserve(
            scen.sid,
            scen.task,
            OpId::new(4_000_000_002),
            model_free,
            Some(known_snapshot()),
            ExecutionClass::Unknown,
        ))
        .unwrap();
    scen.res_ids.push(granted);
    scen.model.open_micro += model_free;
    scen.model.reservations.push(MRes::Reserved(model_free));
    scen.check_obs(0).unwrap();
}

/// Duplicate and out-of-order settles: a duplicate `TurnCompleted` is a
/// legal idempotent SELF re-emit (no state/record churn — exactly-once
/// lives at the turn-record and money layers), a second money settle is
/// `NotOpen`, and settle-before-drive (steer poison) changes nothing.
#[test]
fn duplicate_settles_reemit_and_money_settles_exactly_once() {
    let rt = test_rt();
    let mut scen = Scen::new(rt);
    // Drive one canonical turn: prompt -> ... -> UpdatingMemory -> settle.
    for op in [
        Op::Prompt,
        Op::Hop(0),
        Op::Hop(1),
        Op::Hop(2),
        Op::Hop(3),
        Op::Hop(4),
        Op::Settle,
    ] {
        run_step(&mut scen, &Step { op, crash: None }, 0).expect("canonical turn");
    }
    assert_eq!(scen.model.state, MState::ReadyForNextTurn);
    let settled_world = scen.observe();

    // Duplicate settle (out-of-order re-emit): lands as a self-transition,
    // appends exactly one event, and never re-finishes the turn record.
    run_step(
        &mut scen,
        &Step {
            op: Op::Settle,
            crash: None,
        },
        0,
    )
    .expect("duplicate settle must match the model's self-re-emit prediction");
    let reemitted = scen.observe();
    assert_eq!(
        reemitted.state,
        AgentState::ReadyForNextTurn,
        "state unchanged"
    );
    assert_eq!(
        reemitted.last_event,
        settled_world.last_event + 1,
        "exactly one event appended"
    );
    assert_eq!(
        reemitted.record_completed, settled_world.record_completed,
        "the record is never re-finished"
    );
    assert_eq!(reemitted.msgs, settled_world.msgs, "no message churn");
    // Record-level exactly-once: the closed record refuses a second finish.
    let completed_op = scen
        .model
        .records
        .iter()
        .find(|(_, s)| *s == MRec::Completed)
        .map(|(o, _)| *o)
        .expect("a completed record exists");
    assert!(
        !scen
            .w()
            .handle
            .finish_turn_record(OpId::new(completed_op), "completed")
            .unwrap(),
        "second finish must be a no-op"
    );
    scen.check_obs(0).unwrap();

    // Money: one settle folds once; the second is NotOpen.
    let r = scen
        .rt
        .block_on(scen.w().ledger.reserve(
            scen.sid,
            scen.task,
            OpId::new(5_000_000_001),
            100,
            Some(known_snapshot()),
            ExecutionClass::Unknown,
        ))
        .unwrap();
    scen.res_ids.push(r);
    scen.model.reservations.push(MRes::Reserved(100));
    scen.model.open_micro += 100;
    scen.check_obs(0).unwrap();
    scen.rt
        .block_on(
            scen.w()
                .ledger
                .settle_usage(scen.sid, r, 0, 0, 0, 0, Some(7), None),
        )
        .unwrap();
    let last = scen.model.reservations.len() - 1;
    scen.model.reservations[last] = MRes::Settled { dispatched: false };
    scen.model.open_micro -= 100;
    scen.model.spent += 7;
    scen.check_obs(0).unwrap();
    let double = scen
        .rt
        .block_on(
            scen.w()
                .ledger
                .settle_usage(scen.sid, r, 0, 0, 0, 0, Some(9), None),
        )
        .unwrap_err();
    assert!(matches!(double, BudgetError::NotOpen { .. }), "{double:?}");
    assert_eq!(
        scen.observe().budget.spent,
        settled_world.budget.spent + 7,
        "exactly one settle folds"
    );
    scen.check_obs(0).unwrap();

    // Steer-after-settle: a fresh prompt starts a new turn; settling it
    // BEFORE driving it is refused (Preparing cannot complete).
    run_step(
        &mut scen,
        &Step {
            op: Op::Prompt,
            crash: None,
        },
        0,
    )
    .unwrap();
    let after_prompt = scen.observe();
    let err = scen
        .w()
        .handle
        .append_event(
            EventKind::TurnCompleted,
            AgentState::ReadyForNextTurn,
            None,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err.kind, faktor_core::ErrorKind::InvalidState { .. }),
        "{err:?}"
    );
    assert_eq!(scen.observe(), after_prompt);
}

/// Evict-then-reload: deletions leave durable holes that paging skips
/// (never renumbers) and a reload preserves the exact ordering.
#[test]
fn evict_then_reload_preserves_order_and_holes() {
    let rt = test_rt();
    let mut scen = Scen::new(rt);
    // Three turns, each with an assistant echo.
    for _ in 0..3 {
        run_step(
            &mut scen,
            &Step {
                op: Op::Prompt,
                crash: None,
            },
            0,
        )
        .unwrap();
        for hop in 0..5 {
            run_step(
                &mut scen,
                &Step {
                    op: Op::Hop(hop),
                    crash: None,
                },
                0,
            )
            .unwrap();
        }
        run_step(
            &mut scen,
            &Step {
                op: Op::AppendAssistant,
                crash: None,
            },
            0,
        )
        .unwrap();
        run_step(
            &mut scen,
            &Step {
                op: Op::Settle,
                crash: None,
            },
            0,
        )
        .unwrap();
    }
    let seqs: Vec<i64> = scen.model.msgs.iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs.len(), 6, "3 user + 3 assistant messages");
    // Delete two middle messages (no turn in flight, no tool parts).
    scen.w().handle.delete_message(seqs[1]).unwrap();
    scen.model.msgs.remove(1);
    scen.w().handle.delete_message(seqs[3]).unwrap();
    scen.model.msgs.remove(2); // list shifted after first removal
    scen.check_obs(0).unwrap();
    // Reload: the holes must survive and paging must skip them.
    run_step(
        &mut scen,
        &Step {
            op: Op::Reload,
            crash: None,
        },
        0,
    )
    .unwrap();
    scen.check_obs(0).unwrap();
    let page = scen.w().handle.latest_messages_page(50).unwrap();
    // The page is newest-first (the webview's convention); walk it back to
    // ascending order before comparing with the model's transcript order.
    let mut walked: Vec<i64> = page.messages.iter().map(|m| m.seq).collect();
    walked.reverse();
    let expected: Vec<i64> = scen.model.msgs.iter().map(|(s, _)| *s).collect();
    assert_eq!(walked, expected, "paging skips holes, never renumbers");
    assert!(!page.has_more);
}

/// Concurrent append+settle: the barrier race plus both sequential
/// interleavings must land on the one modeled outcome with zero divergence.
#[test]
fn concurrent_append_and_settle_agree_in_all_interleavings() {
    for mode in [0u8, 1, 2] {
        let rt = test_rt();
        let mut scen = Scen::new(rt);
        // Drive to UpdatingMemory (settleable).
        for op in [
            Op::Prompt,
            Op::Hop(0),
            Op::Hop(1),
            Op::Hop(2),
            Op::Hop(3),
            Op::Hop(4),
        ] {
            run_step(&mut scen, &Step { op, crash: None }, 0).unwrap();
        }
        run_step(
            &mut scen,
            &Step {
                op: Op::ConcurrentAppendSettle(mode),
                crash: None,
            },
            0,
        )
        .unwrap();
        assert_eq!(scen.model.state, MState::ReadyForNextTurn, "mode {mode}");
        assert_eq!(
            scen.observe().msgs.len(),
            2,
            "user + assistant (mode {mode})"
        );
        assert_eq!(
            scen.observe().record_completed,
            1,
            "settle folded exactly once (mode {mode})"
        );
        scen.check_obs(0).unwrap();
    }
}
