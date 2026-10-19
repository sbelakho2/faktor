//! The Engineering Outcome Ledger (economic audit item 7) and the economic
//! settlement lifecycle (P1 economic-truth split).
//!
//! TWO durable dimensions of ONE completed run:
//!
//! 1. [`TerminalEngineeringOutcome`] — the IMMUTABLE engineering record:
//!    durable identity, snapshots, acceptance/integration, first-attempt
//!    verification, repair attempts, the provider/model sequence (local vs
//!    hosted), wall time, human interventions, proof coverage, risk class,
//!    changed-code scale, rollback/recovery events, the final verification
//!    basis, frontier escalations and the explicit per-call VERIFIED
//!    samples. It carries NO monetary fact: money is financial finality,
//!    which reservations can still move while the record is written, and an
//!    immutable row must never freeze an inexact monetary fact.
//! 2. [`EconomicSettlement`] — the append-only settlement lifecycle: the
//!    typed `money_state` (`complete` | `pending` | `unavailable`) with the
//!    settled financial projection. The INITIAL event lands atomically with
//!    the engineering record; late
//!    [`OutcomeLedger::finalize_economic_settlement`] passes append
//!    finalization/correction events as uncertain reservations settle, and
//!    the LATEST event is the settled projection every read derives from.
//!    VCSC reads only `complete` rows, reports the exclusion count and
//!    recomputes as settlements land.
//!
//! Zero is only for a MEASURED zero: a `pending`/`unavailable` projection
//! stores NULL amounts, and `avoided_hosted` is a typed [`MeasuredMoney`] —
//! production has no counterfactual authority, so it is `Unavailable`
//! (`value: null, measurement: unavailable`), never `0`.
//!
//! Dual producers, one durable truth
//! ---------------------------------
//! Both production producers — the runtime's completion gate and the task
//! executor's settlement — derive the row from the SAME durable facts. The
//! non-derivable terminal-turn facts (risk class + explicit verified
//! samples) are persisted FIRST into the store's `terminal_run_facts`
//! outbox by the agent path (before the task row can become terminal), and
//! [`OutcomeLedger::record_terminal_run`] reads them back. For every legal
//! interleaving of the two producers the derived row is therefore
//! byte-identical, and the store's append picks exactly one winner (the
//! loser observes `Duplicate`). The facts freeze at record time.
//!
//! Metrics
//! -------
//! [`OutcomeLedgerMetrics::compute`] is pure integer math over
//! [`OutcomeLedgerEntry`] pairs (saturating, u128 intermediates; never
//! floating point). ENGINEERING metrics fold every row; MONETARY metrics
//! fold only `complete` settlements (`money_complete_runs`) and count the
//! excluded rows (`money_incomplete_excluded`). Rates are floor-divided ppm
//! ("at least"); cost/time averages round UP (never understate). Zero
//! denominators yield zero, never a fabricated number.
//!
//! Attribution honesty and boundary-class truth
//! --------------------------------------------
//! The model sequence is a BOUNDED display: past the cap its distinct
//! provider/model groups fold into an `unknown`-class aggregate tail, and
//! the provider-call history the identity comes from is itself a bounded
//! read. Two truths are therefore recorded EXPLICITLY, independent of that
//! display:
//!
//! - [`AttributionState`] (`complete` | `partial` | `unavailable`): whether
//!   the run's provider/model attribution came from a whole provider-call
//!   read and covered every settled step. A non-complete attribution is
//!   RECORDED (the immutable row must exist) but excluded from the ledger's
//!   routing-learning feed — its samples keep the per-call feed, never
//!   dropped, never doubled — and counted by the metrics fold, never
//!   silently tolerated;
//! - `first_execution_class` / `final_execution_class`: the run's boundary
//!   execution classes, preserved exactly even when the display sequence
//!   folds, so local-first / local-final / local-to-hosted classification
//!   reads durable class authority instead of a possibly-folded boundary
//!   step. [`LocalMeasurement`] reports how much of that boundary authority
//!   exists (`measured` | `partial` | `unmeasured`) with the classed-run
//!   count, mirroring the frontier measurement.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_core::model::{ExecutionClass, RiskBucket, RouterPhase, TaskClass};
use faktor_store::{
    EconomicSettlementRow, EngineeringOutcomeRow, ModelOutcomeIncrement, ModelOutcomeSample, Store,
};
use serde::{Deserialize, Serialize};

/// Payload schema version of every engineering record this module writes
/// (v2: engineering-only; v1 legacy payloads decode compatibly).
pub const OUTCOME_RECORD_SCHEMA_V: u32 = 2;
/// The legacy engineering payload schema (v1) this module still decodes:
/// money lived inside the record; those historic values become a synthesized
/// `complete` settlement projection.
pub const LEGACY_OUTCOME_RECORD_SCHEMA_V: u32 = 1;
/// Payload schema version of one settlement event.
pub const SETTLEMENT_SCHEMA_V: u32 = 1;
/// Payload schema version of one persisted terminal-run-facts outbox row.
pub const TERMINAL_FACTS_SCHEMA_V: u32 = 1;
/// Max bytes of one run identity (ASCII; mirrors the store row bound).
pub const MAX_RUN_ID_BYTES: usize = 200;
/// Max bytes of one provider/model name.
pub const MAX_MODEL_TEXT_BYTES: usize = 128;
/// Max bytes of one snapshot digest text.
pub const MAX_SNAPSHOT_BYTES: usize = 128;
/// Max model-sequence steps of one record.
pub const MAX_RUN_MODEL_STEPS: usize = 16;
/// Max verified samples of one record.
pub const MAX_RUN_VERIFIED_SAMPLES: usize = 32;
/// Bounded page size of one ledger read.
pub const MAX_LEDGER_PAGE: u64 = 500;
/// Bounded number of records one metrics fold scans (a flood past this cap
/// reports `truncated` instead of pretending to be exact).
pub const MAX_METRICS_RECORDS: u64 = 10_000;

/// One provider/model step of the run's execution sequence (engineering:
/// identity and durable execution class; money rides the settlement
/// projection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunModelStep {
    pub provider: String,
    pub model: String,
    /// True only for an authoritatively local step (`execution_class` ==
    /// `local`). Legacy payloads without a class keep their recorded bit.
    pub local: bool,
    /// The durable execution class of this step (economic audit item 7 P1):
    /// `local` only when the step's route-time authority established
    /// locality; `unknown` when no durable authority did — Unknown is counted
    /// as NEITHER local nor hosted by every metric. `#[serde(default)]`
    /// decodes a pre-class payload as `unknown`.
    #[serde(default)]
    pub execution_class: ExecutionClass,
}

impl RunModelStep {
    /// Whether the step is an authoritatively local step. The recorded
    /// `local` bit is honored ONLY as a legacy fallback (a pre-class payload
    /// decoded as `Unknown`): a class-carrying step is local iff its durable
    /// execution class says so — a numeric zero is never inferred as local.
    pub fn is_local(&self) -> bool {
        self.execution_class.is_local() || (!self.execution_class.is_known() && self.local)
    }

    /// Whether the step is an authoritatively HOSTED step; an `Unknown`
    /// class is counted as NEITHER local nor hosted.
    pub fn is_hosted(&self) -> bool {
        self.execution_class.is_hosted()
    }

    /// The CLASS-DERIVED locality for the wire: true only when the durable
    /// execution class is `local` (the schema contract — a classless step is
    /// NEVER reported as local on `local`).
    pub const fn wire_local(&self) -> bool {
        self.execution_class.is_local()
    }

    /// The documented legacy wire marker: the recorded pre-class local bit,
    /// exposed ONLY for a step with NO durable class authority (a pre-class
    /// legacy payload or an unpriced path). True means "the stored row
    /// recorded a local execution before the class authority existed and it
    /// is NOT promoted to a class"; a class-carrying step always reports
    /// false (its recorded bit equals the class and adds no information).
    pub const fn legacy_local(&self) -> bool {
        !self.execution_class.is_known() && self.local
    }
}

/// One settled provider/model step of the run's financial projection: the
/// engineering step identity plus its settled spend in microUSD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettlementStep {
    pub provider: String,
    pub model: String,
    pub local: bool,
    /// The step's durable execution class (mirrors the engineering step).
    #[serde(default)]
    pub execution_class: ExecutionClass,
    pub spend_micro: u64,
}

/// The run's proof coverage: required checks vs the checks that actually
/// ran and passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofCoverage {
    pub checks_passed: u64,
    pub checks_required: u64,
}

impl ProofCoverage {
    /// Coverage in ppm, clamped to 100% (a hostile overlap can never claim
    /// more than full coverage); zero required checks carry no evidence.
    pub fn coverage_ppm(&self) -> u32 {
        if self.checks_required == 0 {
            return 0;
        }
        let passed = self.checks_passed.min(self.checks_required);
        meaning_ppm(passed as u128, self.checks_required as u128)
    }
}

/// The changed-code scale of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangedCodeScale {
    pub files: u64,
    pub added_lines: u64,
    pub removed_lines: u64,
}

/// The final verification basis the completion was accepted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalVerificationBasis {
    /// Deterministic required checks ran and passed.
    DeterministicChecks,
    /// Deterministic checks plus an independent review verdict.
    DeterministicChecksAndReview,
    /// Only an independent review verdict (model-authored evidence; NOT
    /// deterministic verification).
    ReviewOnly,
    /// No objective verification basis was established.
    Unverified,
}

impl FinalVerificationBasis {
    /// Whether the basis rests on deterministic checks (the honest
    /// "verified" bar).
    pub fn is_deterministic_verified(self) -> bool {
        matches!(
            self,
            FinalVerificationBasis::DeterministicChecks
                | FinalVerificationBasis::DeterministicChecksAndReview
        )
    }

    /// The stable wire tag.
    pub fn as_str(self) -> &'static str {
        match self {
            FinalVerificationBasis::DeterministicChecks => "deterministic_checks",
            FinalVerificationBasis::DeterministicChecksAndReview => {
                "deterministic_checks_and_review"
            }
            FinalVerificationBasis::ReviewOnly => "review_only",
            FinalVerificationBasis::Unverified => "unverified",
        }
    }
}

/// The typed settlement lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoneyState {
    /// Every monetary fact of the run is settled and exact.
    Complete,
    /// Reservations are still open/uncertain: the money is not final yet.
    Pending,
    /// The financial projection cannot be established (unreadable authority
    /// or settlements whose amount was never measured). Never a zero.
    Unavailable,
}

impl MoneyState {
    pub fn as_str(self) -> &'static str {
        match self {
            MoneyState::Complete => "complete",
            MoneyState::Pending => "pending",
            MoneyState::Unavailable => "unavailable",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "complete" => Some(MoneyState::Complete),
            "pending" => Some(MoneyState::Pending),
            "unavailable" => Some(MoneyState::Unavailable),
            _ => None,
        }
    }
}

/// The provider/model attribution honesty of one recorded model sequence.
/// The sequence identity comes from a BOUNDED provider-call read and the
/// display sequence folds past its cap, so attribution completeness is a
/// first-class recorded fact, never an assumption:
///
/// - `complete`: the provider-call history was read whole and every settled
///   step found its attributable call row;
/// - `partial`: the provider-call history exceeded its read bound and/or
///   some settled steps carry the explicit unattributed provenance — the
///   money stays exact, the identity does not;
/// - `unavailable`: attribution cannot be established at all: settled steps
///   exist but none carries a provider/model identity, or a payload that
///   predates this field (the honest decode default). Never a guessed
///   identity, never a claimed completeness.
///
/// A non-complete attribution is recorded (the immutable row must exist)
/// and its verified samples are excluded from the ledger's routing-learning
/// feed (they keep the per-call feed — never dropped, never doubled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionState {
    Complete,
    Partial,
    /// The honest legacy decode of a payload written before the attribution
    /// state was recorded: no authority, never a claimed completeness.
    #[default]
    Unavailable,
}

impl AttributionState {
    pub fn as_str(self) -> &'static str {
        match self {
            AttributionState::Complete => "complete",
            AttributionState::Partial => "partial",
            AttributionState::Unavailable => "unavailable",
        }
    }
}

/// One counterfactual money fact: MEASURED (with its exact value; zero is a
/// legal measured zero) or UNAVAILABLE (no authority exists — never a
/// fabricated zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MeasuredMoney {
    Measured(u64),
    #[default]
    Unavailable,
}

impl MeasuredMoney {
    pub fn measured(value: u64) -> Self {
        MeasuredMoney::Measured(value)
    }

    pub fn value(self) -> Option<u64> {
        match self {
            MeasuredMoney::Measured(value) => Some(value),
            MeasuredMoney::Unavailable => None,
        }
    }

    pub fn is_measured(self) -> bool {
        matches!(self, MeasuredMoney::Measured(_))
    }

    pub fn measurement_str(self) -> &'static str {
        match self {
            MeasuredMoney::Measured(_) => "measured",
            MeasuredMoney::Unavailable => "unavailable",
        }
    }
}

/// Durable-payload representation: a measured fact is its exact number
/// (zero is a legal MEASURED zero, distinct from absence); an unavailable
/// fact is `null`. `#[serde(default)]` fields therefore decode a missing
/// key as `Unavailable`, never as a fabricated zero.
impl serde::Serialize for MeasuredMoney {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            MeasuredMoney::Measured(value) => serializer.serialize_some(value),
            MeasuredMoney::Unavailable => serializer.serialize_none(),
        }
    }
}

impl<'de> serde::Deserialize<'de> for MeasuredMoney {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<u64>::deserialize(deserializer)? {
            Some(value) => MeasuredMoney::Measured(value),
            None => MeasuredMoney::Unavailable,
        })
    }
}

/// ONE settlement-lifecycle event of one run: the typed money state and
/// (only when `complete`) the settled financial projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EconomicSettlement {
    pub schema_version: u32,
    pub run_id: String,
    /// The event position in the append-only stream (0 = initial; assigned
    /// by the store on later appends).
    pub seq: u64,
    pub money_state: MoneyState,
    /// Why this projection was classified so (stable, bounded).
    pub reason: String,
    /// Settled model spend in microUSD — present iff `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_spend_micro: Option<u64>,
    /// Settled verification-process spend in microUSD — present iff
    /// `complete`. A measured scope zero: no separate paid verification
    /// process exists; review model calls are inside `model_spend_micro`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_spend_micro: Option<u64>,
    /// Settled rework spend in microUSD — present iff `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rework_spend_micro: Option<u64>,
    /// The counterfactual hosted inference the run avoided. Typed: the
    /// derivation has NO counterfactual authority, so it is `Unavailable`
    /// (zero only exists as an explicit measured zero).
    #[serde(default)]
    pub avoided_hosted: MeasuredMoney,
    /// The settled per-step spend projection — empty unless `complete`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<SettlementStep>,
    /// When this event settled (ms since the Unix epoch).
    pub settled_at_ms: i64,
}

impl EconomicSettlement {
    /// Convert to the durable store row (the store assigns `seq` on later
    /// appends; the initial event must carry 0).
    fn to_row(&self) -> Result<EconomicSettlementRow, OutcomeLedgerError> {
        let to_i64 = |what: &str, v: u64| -> Result<i64, OutcomeLedgerError> {
            i64::try_from(v).map_err(|_| {
                OutcomeLedgerError::Invalid(format!(
                    "settlement {what} {v} exceeds the durable signed 64-bit domain"
                ))
            })
        };
        let steps_json = serde_json::to_string(&self.steps).map_err(|e| {
            OutcomeLedgerError::Invalid(format!("settlement steps serialization failed: {e}"))
        })?;
        Ok(EconomicSettlementRow {
            run_id: self.run_id.clone(),
            seq: i64::try_from(self.seq).unwrap_or(i64::MAX),
            schema_version: i64::from(self.schema_version),
            state: self.money_state.as_str().to_string(),
            reason: self.reason.clone(),
            model_spend_micro: self
                .model_spend_micro
                .map(|v| to_i64("model_spend_micro", v))
                .transpose()?,
            verification_spend_micro: self
                .verification_spend_micro
                .map(|v| to_i64("verification_spend_micro", v))
                .transpose()?,
            rework_spend_micro: self
                .rework_spend_micro
                .map(|v| to_i64("rework_spend_micro", v))
                .transpose()?,
            avoided_hosted_micro: self
                .avoided_hosted
                .value()
                .map(|v| to_i64("avoided_hosted_micro", v))
                .transpose()?,
            avoided_hosted_measured: self.avoided_hosted.is_measured(),
            steps_json,
            settled_at_ms: self.settled_at_ms,
        })
    }

    /// Decode one stored settlement row (the store already validated the
    /// row's own shape; this decode is strict about the agent schema).
    fn from_row(row: &EconomicSettlementRow) -> Result<Self, OutcomeLedgerError> {
        let corrupt = |detail: String| OutcomeLedgerError::Corrupt {
            run_id: row.run_id.clone(),
            detail,
        };
        let money_state = MoneyState::parse(&row.state)
            .ok_or_else(|| corrupt(format!("settlement state {:?} is unknown", row.state)))?;
        let to_u64 = |what: &str, v: Option<i64>| -> Result<Option<u64>, OutcomeLedgerError> {
            v.map(|v| {
                u64::try_from(v).map_err(|_| OutcomeLedgerError::Corrupt {
                    run_id: row.run_id.clone(),
                    detail: format!("settlement {what} {v} is negative"),
                })
            })
            .transpose()
        };
        let steps: Vec<SettlementStep> = serde_json::from_str(&row.steps_json)
            .map_err(|e| corrupt(format!("settlement step projection does not decode: {e}")))?;
        let avoided_hosted = if row.avoided_hosted_measured {
            let value = row.avoided_hosted_micro.ok_or_else(|| {
                corrupt("measured avoided-hosted counterfactual without a value".into())
            })?;
            MeasuredMoney::Measured(
                u64::try_from(value)
                    .map_err(|_| corrupt("negative avoided-hosted counterfactual".into()))?,
            )
        } else {
            MeasuredMoney::Unavailable
        };
        Ok(EconomicSettlement {
            schema_version: u32::try_from(row.schema_version).map_err(|_| {
                corrupt(format!("settlement schema_version {}", row.schema_version))
            })?,
            run_id: row.run_id.clone(),
            seq: u64::try_from(row.seq)
                .map_err(|_| corrupt(format!("settlement seq {} is negative", row.seq)))?,
            money_state,
            reason: row.reason.clone(),
            model_spend_micro: to_u64("model_spend_micro", row.model_spend_micro)?,
            verification_spend_micro: to_u64(
                "verification_spend_micro",
                row.verification_spend_micro,
            )?,
            rework_spend_micro: to_u64("rework_spend_micro", row.rework_spend_micro)?,
            avoided_hosted,
            steps,
            settled_at_ms: row.settled_at_ms,
        })
    }
}

/// ONE explicit verified-outcome sample of the run: the sample the
/// outcome-learning router learns from, carrying EXACTLY the attribution the
/// deterministic gate sites hold ([`crate::VerifiedCallAttribution`]
/// semantics). Identical in shape to
/// [`faktor_router::OutcomeSample`](faktor_router::outcomes::OutcomeSample);
/// a caller that only knows "the model said done" must not include it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunVerifiedSample {
    pub provider: String,
    pub model: String,
    pub phase: RouterPhase,
    /// The explicit verified-success signal: the task ultimately passed
    /// deterministic verification AND attribution identified this call.
    pub verified_success: bool,
    /// Downstream spend this sample's failure caused until the task
    /// verified; ignored on success samples (validated to be zero).
    pub rework_cost_micro: u64,
    /// Downstream turns this sample's failure caused; ignored on success
    /// samples (validated to be zero).
    pub rework_turns: u64,
}

/// ONE completed run's IMMUTABLE engineering-outcome record. Every
/// projection column of the durable row is derived EXACTLY from these
/// fields (a payload/projection drift reads as typed corruption, never as a
/// metric). No monetary field exists here: money is the
/// [`EconomicSettlement`] dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalEngineeringOutcome {
    pub schema_version: u32,
    pub run_id: String,
    pub session_id: u64,
    pub task_id: u64,
    pub task_class: TaskClass,
    /// The semantic-risk class of the change.
    pub risk_class: RiskBucket,
    /// The candidate snapshot digest verification bound (`None` when the run
    /// verified the live root directly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_snapshot: Option<String>,
    /// The final (landed) snapshot digest.
    pub final_snapshot: String,
    /// The completion was accepted.
    pub accepted: bool,
    /// The change was integrated into the owner root.
    pub integrated: bool,
    /// The run's FIRST verification pass: the certifying deterministic pass
    /// is the run's earliest durable verification attempt (no failed
    /// verification attempt precedes it). A VERIFICATION-attempt measure,
    /// never an implementation-attempt claim; the pre-rename payload tag
    /// `verified_first_attempt` decodes into this field under its honest
    /// definition.
    #[serde(alias = "verified_first_attempt")]
    pub verified_first_verification_pass: bool,
    /// Repair attempts the run needed before verifying (0 = first-pass).
    pub repair_attempts: u64,
    /// The provider/model sequence actually used, in order (engineering
    /// identity only; per-step money lives in the settlement projection).
    /// A BOUNDED display: past [`MAX_RUN_MODEL_STEPS`] distinct groups fold
    /// into an `unknown`-class aggregate tail, so the boundary classes
    /// below — not the sequence's first/last step — are the classification
    /// authority.
    pub model_sequence: Vec<RunModelStep>,
    /// The run's FIRST settled step's durable execution class, preserved
    /// exactly even when the bounded display sequence folds (survives
    /// compaction). `unknown` when no step carried a durable authority (or
    /// no step exists). `#[serde(default)]` decodes a pre-field payload as
    /// `unknown` and the local metrics then fall back to the preserved
    /// sequence boundary.
    #[serde(default)]
    pub first_execution_class: ExecutionClass,
    /// The run's FINAL settled step's durable execution class, preserved
    /// exactly even when the bounded display sequence folds it into the
    /// aggregate tail (survives compaction). Same decode contract as
    /// [`Self::first_execution_class`]; for a pre-field payload whose tail
    /// was folded the class is genuinely lost and stays `unknown` — never a
    /// guess from the aggregate step.
    #[serde(default)]
    pub final_execution_class: ExecutionClass,
    /// The provider/model attribution honesty of the recorded sequence (see
    /// [`AttributionState`]); `#[serde(default)]` decodes a pre-field
    /// payload as `unavailable` (the state was never recorded — never a
    /// claimed completeness).
    #[serde(default)]
    pub attribution_state: AttributionState,
    /// Wall time from run start to terminal outcome.
    pub wall_time_ms: u64,
    /// Human interventions (permission decisions, manual steers/edits).
    pub human_interventions: u64,
    pub proof: ProofCoverage,
    pub change_scale: ChangedCodeScale,
    pub rollback_events: u64,
    pub recovery_events: u64,
    pub final_basis: FinalVerificationBasis,
    /// Times the run escalated to a hosted frontier-tier model.
    pub frontier_escalations: u64,
    /// When the run reached its terminal outcome (ms since the Unix epoch).
    pub completed_at_ms: i64,
    /// The run's explicit verified samples (router feed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified_samples: Vec<RunVerifiedSample>,
}

/// The legacy engineering record this module still decodes (schema v1: money
/// lived inside the record). Never written anymore.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRunModelStep {
    provider: String,
    model: String,
    local: bool,
    spend_micro: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRunOutcomeRecordV1 {
    schema_version: u32,
    run_id: String,
    session_id: u64,
    task_id: u64,
    task_class: TaskClass,
    risk_class: RiskBucket,
    #[serde(default)]
    candidate_snapshot: Option<String>,
    final_snapshot: String,
    accepted: bool,
    integrated: bool,
    #[serde(
        rename = "verified_first_attempt",
        alias = "verified_first_verification_pass"
    )]
    verified_first_verification_pass: bool,
    repair_attempts: u64,
    model_sequence: Vec<LegacyRunModelStep>,
    model_spend_micro: u64,
    verification_spend_micro: u64,
    rework_spend_micro: u64,
    avoided_hosted_micro: u64,
    wall_time_ms: u64,
    human_interventions: u64,
    proof: ProofCoverage,
    change_scale: ChangedCodeScale,
    rollback_events: u64,
    recovery_events: u64,
    final_basis: FinalVerificationBasis,
    frontier_escalations: u64,
    completed_at_ms: i64,
    #[serde(default)]
    verified_samples: Vec<RunVerifiedSample>,
}

/// The historic money of a legacy v1 payload, turned into a synthesized
/// `complete` settlement projection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacyMoney {
    model_spend_micro: u64,
    verification_spend_micro: u64,
    rework_spend_micro: u64,
    avoided_hosted: MeasuredMoney,
    steps: Vec<SettlementStep>,
}

/// One decoded stored row: the engineering outcome plus (for legacy v1
/// payloads) the historic money that becomes its synthesized settlement.
struct StoredOutcome {
    outcome: TerminalEngineeringOutcome,
    legacy_money: Option<LegacyMoney>,
}

impl TerminalEngineeringOutcome {
    /// The run counts as a successfully verified change: accepted,
    /// integrated AND resting on deterministic verification.
    pub fn verified(&self) -> bool {
        self.accepted && self.integrated && self.final_basis.is_deterministic_verified()
    }

    /// The run's FIRST-step execution class with its documented legacy
    /// fallback: the exactly-preserved [`Self::first_execution_class`] when
    /// a durable authority recorded it, else the display sequence's first
    /// step class (a pre-field payload; the first step is never folded).
    /// `unknown` when neither exists — never a guessed class.
    pub fn effective_first_execution_class(&self) -> ExecutionClass {
        if self.first_execution_class.is_known() {
            return self.first_execution_class;
        }
        self.model_sequence
            .first()
            .map(|step| step.execution_class)
            .unwrap_or(ExecutionClass::Unknown)
    }

    /// The run's FINAL-step execution class with its documented legacy
    /// fallback: the exactly-preserved [`Self::final_execution_class`] when
    /// a durable authority recorded it, else the display sequence's last
    /// step class (a pre-field payload; an already-folded tail reads
    /// `unknown` — the class is genuinely lost there, never inferred from
    /// the aggregate step).
    pub fn effective_final_execution_class(&self) -> ExecutionClass {
        if self.final_execution_class.is_known() {
            return self.final_execution_class;
        }
        self.model_sequence
            .last()
            .map(|step| step.execution_class)
            .unwrap_or(ExecutionClass::Unknown)
    }

    /// Whether any step ran on a local model. The bounded display sequence
    /// is folded past its cap, so the exactly-preserved boundary classes
    /// participate too: a run whose only local step is its final (folded)
    /// step still counts as local-used.
    pub fn local_used(&self) -> bool {
        self.model_sequence.iter().any(|s| s.local)
            || self.effective_first_execution_class().is_local()
            || self.effective_final_execution_class().is_local()
    }

    /// Bounded structural validation, on the caller's thread BEFORE any
    /// write; the durable append re-validates the row shape.
    pub fn validate(&self) -> Result<(), OutcomeLedgerError> {
        if self.schema_version != OUTCOME_RECORD_SCHEMA_V {
            return Err(OutcomeLedgerError::Invalid(format!(
                "unsupported outcome record schema_version {} (this build writes \
                 {OUTCOME_RECORD_SCHEMA_V})",
                self.schema_version
            )));
        }
        if self.run_id.is_empty() || self.run_id.len() > MAX_RUN_ID_BYTES {
            return Err(OutcomeLedgerError::Invalid(format!(
                "run id must be 1..={MAX_RUN_ID_BYTES} bytes, got {}",
                self.run_id.len()
            )));
        }
        if !self.run_id.is_ascii() {
            return Err(OutcomeLedgerError::Invalid("run id must be ASCII".into()));
        }
        if self.final_snapshot.is_empty() || self.final_snapshot.len() > MAX_SNAPSHOT_BYTES {
            return Err(OutcomeLedgerError::Invalid(format!(
                "final snapshot must be 1..={MAX_SNAPSHOT_BYTES} bytes, got {}",
                self.final_snapshot.len()
            )));
        }
        if let Some(candidate) = &self.candidate_snapshot {
            if candidate.is_empty() || candidate.len() > MAX_SNAPSHOT_BYTES {
                return Err(OutcomeLedgerError::Invalid(format!(
                    "candidate snapshot must be 1..={MAX_SNAPSHOT_BYTES} bytes, got {}",
                    candidate.len()
                )));
            }
        }
        if self.model_sequence.len() > MAX_RUN_MODEL_STEPS {
            return Err(OutcomeLedgerError::Invalid(format!(
                "model sequence has {} steps (cap {MAX_RUN_MODEL_STEPS})",
                self.model_sequence.len()
            )));
        }
        for step in &self.model_sequence {
            for (what, value) in [("provider", &step.provider), ("model", &step.model)] {
                if value.is_empty() || value.len() > MAX_MODEL_TEXT_BYTES {
                    return Err(OutcomeLedgerError::Invalid(format!(
                        "model step {what} must be 1..={MAX_MODEL_TEXT_BYTES} bytes, got {}",
                        value.len()
                    )));
                }
            }
            // A durably-known class and the local bit must agree; a legacy
            // step without a class keeps its recorded bit (value records
            // predate the class authority).
            if step.execution_class.is_known() && step.local != step.execution_class.is_local() {
                return Err(OutcomeLedgerError::Invalid(format!(
                    "model step {}/{} carries execution_class {} but local={}",
                    step.provider,
                    step.model,
                    step.execution_class.as_str(),
                    step.local
                )));
            }
        }
        if self.proof.checks_passed > self.proof.checks_required {
            return Err(OutcomeLedgerError::Invalid(format!(
                "checks_passed {} exceeds checks_required {}",
                self.proof.checks_passed, self.proof.checks_required
            )));
        }
        // A run with no recorded step proves no boundary class: a
        // durably-known first/final class without any step is drift, never
        // a class authority.
        if self.model_sequence.is_empty()
            && (self.first_execution_class.is_known() || self.final_execution_class.is_known())
        {
            return Err(OutcomeLedgerError::Invalid(
                "a run with no model steps cannot carry durably-known boundary execution \
                 classes"
                    .into(),
            ));
        }
        // A COMPLETE attribution claim must not carry an explicitly
        // unattributed step (the derivation never produces one; a hostile
        // or drifted payload is a typed corruption).
        if self.attribution_state == AttributionState::Complete {
            for step in &self.model_sequence {
                if step.provider == faktor_session::UNATTRIBUTED_PROVENANCE
                    || step.model == faktor_session::UNATTRIBUTED_PROVENANCE
                {
                    return Err(OutcomeLedgerError::Invalid(format!(
                        "a complete attribution claim cannot carry the unattributed step \
                         {:?}/{:?}",
                        step.provider, step.model
                    )));
                }
            }
        }
        if self.verified_first_verification_pass && !self.verified() {
            return Err(OutcomeLedgerError::Invalid(
                "a first-attempt verified run must be accepted, integrated and rest on a \
                 deterministic verification basis"
                    .into(),
            ));
        }
        if self.verified_samples.len() > MAX_RUN_VERIFIED_SAMPLES {
            return Err(OutcomeLedgerError::Invalid(format!(
                "{} verified samples exceed the cap {MAX_RUN_VERIFIED_SAMPLES}",
                self.verified_samples.len()
            )));
        }
        for sample in &self.verified_samples {
            for (what, value) in [("provider", &sample.provider), ("model", &sample.model)] {
                if value.is_empty() || value.len() > MAX_MODEL_TEXT_BYTES {
                    return Err(OutcomeLedgerError::Invalid(format!(
                        "verified sample {what} must be 1..={MAX_MODEL_TEXT_BYTES} bytes, got {}",
                        value.len()
                    )));
                }
            }
            if sample.verified_success
                && (sample.rework_cost_micro != 0 || sample.rework_turns != 0)
            {
                return Err(OutcomeLedgerError::Invalid(format!(
                    "verified sample {}/{} is a first-pass SUCCESS carrying rework \
                     ({}, {}); a verified success cannot cause rework",
                    sample.provider, sample.model, sample.rework_cost_micro, sample.rework_turns
                )));
            }
        }
        Ok(())
    }

    /// The exact projection row the durable store persists.
    pub fn projection_row(&self) -> Result<EngineeringOutcomeRow, OutcomeLedgerError> {
        let payload = serde_json::to_string(self).map_err(|e| {
            OutcomeLedgerError::Invalid(format!("record serialization failed: {e}"))
        })?;
        let to_i64 = |what: &str, v: u64| -> Result<i64, OutcomeLedgerError> {
            i64::try_from(v).map_err(|_| {
                OutcomeLedgerError::Invalid(format!(
                    "{what} {v} exceeds the durable signed 64-bit domain"
                ))
            })
        };
        let verified = self.verified();
        Ok(EngineeringOutcomeRow {
            run_id: self.run_id.clone(),
            session_id: to_i64("session_id", self.session_id)?,
            task_id: to_i64("task_id", self.task_id)?,
            payload_bytes: i64::try_from(payload.len()).map_err(|_| {
                OutcomeLedgerError::Invalid("payload exceeds the durable byte domain".into())
            })?,
            payload,
            completed_at_ms: self.completed_at_ms,
            verified,
            verified_first_pass: verified && self.verified_first_verification_pass,
            integrated: self.integrated,
            local_used: self.local_used(),
            frontier_used: self.frontier_escalations > 0,
            wall_time_ms: to_i64("wall_time_ms", self.wall_time_ms)?,
            human_interventions: to_i64("human_interventions", self.human_interventions)?,
            rollback_events: to_i64("rollback_events", self.rollback_events)?,
            recovery_events: to_i64("recovery_events", self.recovery_events)?,
            repair_attempts: to_i64("repair_attempts", self.repair_attempts)?,
            risk_class: risk_class_tag(self.risk_class).to_string(),
            changed_files: to_i64("changed_files", self.change_scale.files)?,
            changed_lines: to_i64(
                "changed_lines",
                self.change_scale
                    .added_lines
                    .saturating_add(self.change_scale.removed_lines),
            )?,
            checks_passed: to_i64("checks_passed", self.proof.checks_passed)?,
            checks_required: to_i64("checks_required", self.proof.checks_required)?,
        })
    }

    /// The durable per-call verified samples this record feeds into the
    /// router's `model_outcome_stats` registry (verified attribution only —
    /// the samples themselves carry the explicit signal).
    ///
    /// ATTRIBUTION GATE: a record whose provider/model attribution is not
    /// [`AttributionState::Complete`] feeds NOTHING here. Its attribution is
    /// record-local truth (the bounded provider-call read can truncate), and
    /// the immutable row must still be written — so the run's samples are
    /// excluded from the atomic learning feed instead of laundering an
    /// attribution-incomplete run into the router registry. The samples are
    /// NOT dropped: the runtime's terminal-turn reconciliation sees the
    /// exclusion and keeps them on the per-call verified feed (fed exactly
    /// once either way).
    pub fn router_increments(&self) -> Vec<ModelOutcomeIncrement> {
        if self.attribution_state != AttributionState::Complete {
            return Vec::new();
        }
        self.verified_samples
            .iter()
            .map(|s| ModelOutcomeIncrement {
                provider: s.provider.clone(),
                model: s.model.clone(),
                phase: s.phase,
                task_class: self.task_class,
                risk_bucket: self.risk_class,
                sample: ModelOutcomeSample {
                    verified_success: s.verified_success,
                    rework_cost_micro: s.rework_cost_micro,
                    rework_turns: s.rework_turns,
                },
            })
            .collect()
    }
}

/// The stable risk-class tag persisted in the projection column.
fn risk_class_tag(bucket: RiskBucket) -> &'static str {
    match bucket {
        RiskBucket::Low => "low",
        RiskBucket::Medium => "medium",
        RiskBucket::High => "high",
    }
}

/// Typed ledger failures. Every durable problem is named; a corrupt or
/// drifted row can never surface as a fabricated record or metric.
#[derive(Debug, thiserror::Error)]
pub enum OutcomeLedgerError {
    #[error("invalid run outcome record: {0}")]
    Invalid(String),
    #[error("terminal run facts are unavailable: {0}")]
    Facts(String),
    #[error("outcome ledger store failure: {0}")]
    Store(#[from] faktor_store::StoreError),
    #[error("outcome ledger row {run_id} is corrupt: {detail}")]
    Corrupt { run_id: String, detail: String },
}

/// The outcome of one [`OutcomeLedger::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerAppend {
    /// The record was durably appended (settlement event and router samples
    /// committed atomically).
    Recorded,
    /// The run id already held the byte-identical record: a crash/replay
    /// no-op — no second record, no second sample.
    Replayed,
}

impl LedgerAppend {
    pub fn was_recorded(self) -> bool {
        matches!(self, LedgerAppend::Recorded)
    }
}

/// The outcome of one [`OutcomeLedger::finalize_economic_settlement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementFinalize {
    /// No engineering record exists for the run: nothing to settle.
    Absent,
    /// The latest settlement event is already `complete` — with the identical
    /// re-derived fact or a later non-complete one (complete is final; only a
    /// semantically DIFFERENT complete correction ever moves it) — or a
    /// legacy row synthesizes a complete projection: nothing moved.
    AlreadySettled,
    /// A new settlement event was appended (the projection moved).
    Appended,
    /// The re-derived projection is semantically identical to the latest
    /// event at any clock, or a non-final state's diagnostic drift (a
    /// non-final state never rewrites itself): a deterministic no-op.
    Unchanged,
}

/// The total oldest-key cursor of one ledger page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeCursor {
    pub completed_at_ms: i64,
    pub run_id: String,
}

/// ONE ledger row as every read surface consumes it: the immutable
/// engineering outcome plus the LATEST settlement projection.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeLedgerEntry {
    pub outcome: TerminalEngineeringOutcome,
    pub settlement: EconomicSettlement,
}

/// One bounded newest-first ledger page.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeLedgerPage {
    pub entries: Vec<OutcomeLedgerEntry>,
    /// Present when the page was full: the cursor older pages continue from.
    pub next: Option<OutcomeCursor>,
}

/// The durable Engineering Outcome Ledger handle.
#[derive(Clone)]
pub struct OutcomeLedger {
    store: Arc<Store>,
    /// Test-only read-order probe (see
    /// [`OutcomeLedger::with_after_reservation_read`]): fires immediately
    /// after the reservation state read of a derivation so an adversarial
    /// test can interleave a concurrent settle EXACTLY between the two money
    /// reads. Compiled out of every non-test build.
    #[cfg(test)]
    after_reservation_read: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Test-only read-fault injector: while the counter is positive, every
    /// [`OutcomeLedger::load_stored`] read consumes one unit and fails with
    /// a typed store error, so an adversarial test can exercise the
    /// conflict-loser reconciliation's bounded retry and its
    /// non-double-counting fallback. Compiled out of every non-test build.
    #[cfg(test)]
    failed_reads: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// Test-only provider-call read bound override (see
    /// [`OutcomeLedger::with_spend_call_bound`]): the derivations read the
    /// settled spend through this bound so an adversarial test can force the
    /// documented truncation at test scale. `None` = the canonical
    /// [`faktor_session::MAX_DURABLE_SPEND_CALL_ROWS`]. Compiled out of
    /// every non-test build.
    #[cfg(test)]
    spend_call_bound_override: Option<i64>,
}

impl std::fmt::Debug for OutcomeLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutcomeLedger")
            .field("store", &self.store)
            .finish()
    }
}

impl OutcomeLedger {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            #[cfg(test)]
            after_reservation_read: None,
            #[cfg(test)]
            failed_reads: None,
            #[cfg(test)]
            spend_call_bound_override: None,
        }
    }

    /// Test-only: one probe fired immediately after the reservation state
    /// read of a derivation (before the settled-spend read). The adversarial
    /// interleaving test uses it to settle a reservation exactly in the
    /// window the documented read order must survive.
    #[cfg(test)]
    pub(crate) fn with_after_reservation_read(
        store: Arc<Store>,
        probe: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            store,
            after_reservation_read: Some(probe),
            failed_reads: None,
            spend_call_bound_override: None,
        }
    }

    /// Test-only: the FIRST `failures` durable row reads fail with a typed
    /// store error (a positive counter consumes one failure per read).
    /// Used by the conflict-loser fault-injection test.
    #[cfg(test)]
    pub(crate) fn with_failed_reads(store: Arc<Store>, failures: usize) -> Self {
        Self {
            store,
            after_reservation_read: None,
            failed_reads: Some(Arc::new(std::sync::atomic::AtomicUsize::new(failures))),
            spend_call_bound_override: None,
        }
    }

    /// Test-only: pin an explicit provider-call read bound on the
    /// derivations (see [`Self::spend_call_bound`]). The only way to observe
    /// the documented truncation at test scale without seeding the
    /// production-scale history.
    #[cfg(test)]
    pub(crate) fn with_spend_call_bound(store: Arc<Store>, bound: i64) -> Self {
        Self {
            store,
            after_reservation_read: None,
            failed_reads: None,
            spend_call_bound_override: Some(bound),
        }
    }

    /// The provider-call read bound of both derivations: the canonical
    /// session bound in production, the test-only override (if any) under
    /// `cfg(test)`.
    fn spend_call_bound(&self) -> i64 {
        #[cfg(test)]
        if let Some(bound) = self.spend_call_bound_override {
            return bound;
        }
        faktor_session::MAX_DURABLE_SPEND_CALL_ROWS
    }

    /// Fire the test-only read-order probe, if any.
    #[inline]
    fn note_reservation_read(&self) {
        #[cfg(test)]
        if let Some(probe) = &self.after_reservation_read {
            probe();
        }
    }

    /// Durably record ONE completed run — its immutable engineering outcome,
    /// its initial settlement event and its verified samples — exactly once
    /// and atomically. A replay of an identical record is a no-op; a
    /// conflicting re-append under the same run id is a typed refusal from
    /// the store.
    pub fn record(
        &self,
        outcome: &TerminalEngineeringOutcome,
        settlement: &EconomicSettlement,
    ) -> Result<LedgerAppend, OutcomeLedgerError> {
        outcome.validate()?;
        if outcome.run_id != settlement.run_id {
            return Err(OutcomeLedgerError::Invalid(format!(
                "settlement run id {:?} disagrees with the outcome run id {:?}",
                settlement.run_id, outcome.run_id
            )));
        }
        if settlement.seq != 0 {
            return Err(OutcomeLedgerError::Invalid(
                "the initial settlement event must carry seq 0".into(),
            ));
        }
        if settlement.money_state == MoneyState::Complete {
            validate_complete_projection(outcome, settlement)?;
        } else if !settlement.steps.is_empty()
            || settlement.model_spend_micro.is_some()
            || settlement.verification_spend_micro.is_some()
            || settlement.rework_spend_micro.is_some()
            || settlement.avoided_hosted.is_measured()
        {
            return Err(OutcomeLedgerError::Invalid(
                "an unsettled/unavailable projection must store no amounts, never a fabricated \
                 zero"
                    .into(),
            ));
        }
        let row = outcome.projection_row()?;
        let settlement_row = settlement.to_row()?;
        let increments = outcome.router_increments();
        match self
            .store
            .engineering_outcome_append(&row, &increments, &settlement_row)?
        {
            faktor_store::EngineeringOutcomeAppend::Appended => Ok(LedgerAppend::Recorded),
            faktor_store::EngineeringOutcomeAppend::Duplicate => Ok(LedgerAppend::Replayed),
        }
    }

    /// The exact durable engineering record of one run, or `None`. The
    /// stored payload is re-validated AND cross-checked against every
    /// projection column, so a payload/projection drift is a typed
    /// corruption, never a metric. Legacy v1 payloads are decoded
    /// compatibly (their historic money is exposed through
    /// [`Self::settlement`]).
    pub fn get(
        &self,
        run_id: &str,
    ) -> Result<Option<TerminalEngineeringOutcome>, OutcomeLedgerError> {
        Ok(self.load_stored(run_id)?.map(|stored| stored.outcome))
    }

    /// The LATEST settlement projection of one run, or `None` when the run
    /// has no record. Legacy v1 engineering rows synthesize a `complete`
    /// projection from their recorded (historic) money.
    pub fn settlement(
        &self,
        run_id: &str,
    ) -> Result<Option<EconomicSettlement>, OutcomeLedgerError> {
        let Some(stored) = self.load_stored(run_id)? else {
            return Ok(None);
        };
        let latest = self.store.economic_settlement_latest(run_id)?;
        Ok(Some(settlement_for_stored(&stored, latest)?))
    }

    /// One bounded newest-first page of ledger entries, strictly older than
    /// `cursor`. The settlement lookups are paginated in bounded chunks
    /// ([`faktor_store::MAX_ECONOMIC_SETTLEMENT_BATCH`]) so a full page never
    /// exceeds the store's batch bound, and a store-valid but agent-invalid
    /// settlement row propagates its typed corruption instead of being
    /// silently dropped into a fabricated projection.
    pub fn page(
        &self,
        cursor: Option<&OutcomeCursor>,
        limit: u64,
    ) -> Result<OutcomeLedgerPage, OutcomeLedgerError> {
        let limit = limit.clamp(1, MAX_LEDGER_PAGE);
        let rows = self.store.engineering_outcome_page(
            cursor.map(|c| (c.completed_at_ms, c.run_id.as_str())),
            limit,
        )?;
        let full = rows.len() as u64 == limit;
        let stored: Vec<StoredOutcome> = rows
            .iter()
            .map(decode_record_row)
            .collect::<Result<_, _>>()?;
        let run_ids: Vec<String> = rows.iter().map(|row| row.run_id.clone()).collect();
        let mut latest: BTreeMap<String, EconomicSettlement> = BTreeMap::new();
        for chunk in run_ids.chunks(faktor_store::MAX_ECONOMIC_SETTLEMENT_BATCH) {
            for row in self.store.economic_settlement_latest_for_runs(chunk)? {
                let settlement = EconomicSettlement::from_row(&row)?;
                latest.insert(settlement.run_id.clone(), settlement);
            }
        }
        let mut entries = Vec::with_capacity(stored.len());
        for stored in &stored {
            let settlement = match latest.remove(&stored.outcome.run_id) {
                Some(settlement) => settlement,
                None => settlement_for_stored(stored, None)?,
            };
            entries.push(OutcomeLedgerEntry {
                outcome: stored.outcome.clone(),
                settlement,
            });
        }
        let next = if full {
            rows.last().map(|last| OutcomeCursor {
                completed_at_ms: last.completed_at_ms,
                run_id: last.run_id.clone(),
            })
        } else {
            None
        };
        Ok(OutcomeLedgerPage { entries, next })
    }

    /// Exact durable record count.
    pub fn count(&self) -> Result<u64, OutcomeLedgerError> {
        Ok(self.store.engineering_outcome_count()?)
    }

    /// The bounded metrics fold over the durable ledger. See
    /// [`OutcomeLedgerMetrics::compute`] for the metric definitions; a
    /// ledger larger than [`MAX_METRICS_RECORDS`] reports `truncated`.
    pub fn metrics(&self) -> Result<OutcomeLedgerMetricsReport, OutcomeLedgerError> {
        let mut entries: Vec<OutcomeLedgerEntry> = Vec::new();
        let mut cursor: Option<OutcomeCursor> = None;
        let mut truncated = false;
        loop {
            let page = self.page(cursor.as_ref(), MAX_LEDGER_PAGE)?;
            entries.extend(page.entries);
            if entries.len() as u64 >= MAX_METRICS_RECORDS {
                truncated |= page.next.is_some();
                entries.truncate(MAX_METRICS_RECORDS as usize);
                break;
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(OutcomeLedgerMetricsReport {
            scanned: entries.len() as u64,
            metrics: OutcomeLedgerMetrics::compute(&entries),
            truncated,
        })
    }

    /// Load and decode one stored row (legacy v1 or v2 payload).
    fn load_stored(&self, run_id: &str) -> Result<Option<StoredOutcome>, OutcomeLedgerError> {
        // Test-only fault injector: consume one armed failure per read (see
        // `with_failed_reads`). Compiled out of every non-test build.
        #[cfg(test)]
        if let Some(failures) = &self.failed_reads {
            use std::sync::atomic::Ordering;
            if failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(OutcomeLedgerError::Store(faktor_store::StoreError::Busy(
                    format!("injected durable read failure for {run_id}"),
                )));
            }
        }
        self.store
            .engineering_outcome_get(run_id)?
            .map(|row| decode_record_row(&row))
            .transpose()
    }
}

/// Semantic identity of one durable settlement event and a re-derived event:
/// everything except `seq` and `settled_at_ms` (mirrors the store's raw
/// comparison — a replay of the same fact is a no-op even at a new clock).
fn settlements_semantically_equal(
    latest: &EconomicSettlement,
    derived: &EconomicSettlement,
) -> bool {
    latest.schema_version == derived.schema_version
        && latest.money_state == derived.money_state
        && latest.reason == derived.reason
        && latest.model_spend_micro == derived.model_spend_micro
        && latest.verification_spend_micro == derived.verification_spend_micro
        && latest.rework_spend_micro == derived.rework_spend_micro
        && latest.avoided_hosted == derived.avoided_hosted
        && latest.steps == derived.steps
}

/// Synthesize the settlement projection of one stored engineering record:
/// the stored latest event, or (for a legacy v1 row) the historic money, or
/// an explicit `unavailable` projection when neither exists.
fn settlement_for_stored(
    stored: &StoredOutcome,
    latest: Option<EconomicSettlementRow>,
) -> Result<EconomicSettlement, OutcomeLedgerError> {
    if let Some(row) = latest {
        return EconomicSettlement::from_row(&row);
    }
    if let Some(money) = &stored.legacy_money {
        return Ok(EconomicSettlement {
            schema_version: SETTLEMENT_SCHEMA_V,
            run_id: stored.outcome.run_id.clone(),
            seq: 0,
            money_state: MoneyState::Complete,
            reason: "legacy v1 engineering record (money recorded under the pre-settlement \
                     contract)"
                .into(),
            model_spend_micro: Some(money.model_spend_micro),
            verification_spend_micro: Some(money.verification_spend_micro),
            rework_spend_micro: Some(money.rework_spend_micro),
            avoided_hosted: money.avoided_hosted,
            steps: money.steps.clone(),
            settled_at_ms: stored.outcome.completed_at_ms,
        });
    }
    Ok(EconomicSettlement {
        schema_version: SETTLEMENT_SCHEMA_V,
        run_id: stored.outcome.run_id.clone(),
        seq: 0,
        money_state: MoneyState::Unavailable,
        reason: "no settlement lifecycle event exists for this record".into(),
        model_spend_micro: None,
        verification_spend_micro: None,
        rework_spend_micro: None,
        avoided_hosted: MeasuredMoney::Unavailable,
        steps: Vec::new(),
        settled_at_ms: stored.outcome.completed_at_ms,
    })
}

/// Decode and cross-check one durable row: payload schema/validation AND
/// exact agreement with every projection column. Legacy v1 payloads decode
/// through the legacy shape and synthesize their historic money.
fn decode_record_row(row: &EngineeringOutcomeRow) -> Result<StoredOutcome, OutcomeLedgerError> {
    let probe: serde_json::Value =
        serde_json::from_str(&row.payload).map_err(|e| OutcomeLedgerError::Corrupt {
            run_id: row.run_id.clone(),
            detail: format!("payload is not valid JSON: {e}"),
        })?;
    let version = probe
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| OutcomeLedgerError::Corrupt {
            run_id: row.run_id.clone(),
            detail: "payload carries no schema_version".into(),
        })?;
    match version as u32 {
        LEGACY_OUTCOME_RECORD_SCHEMA_V => {
            let legacy: LegacyRunOutcomeRecordV1 =
                serde_json::from_value(probe).map_err(|e| OutcomeLedgerError::Corrupt {
                    run_id: row.run_id.clone(),
                    detail: format!("legacy payload does not decode: {e}"),
                })?;
            decode_legacy_row(row, legacy)
        }
        OUTCOME_RECORD_SCHEMA_V => {
            let record: TerminalEngineeringOutcome =
                serde_json::from_value(probe).map_err(|e| OutcomeLedgerError::Corrupt {
                    run_id: row.run_id.clone(),
                    detail: format!("payload does not decode as a run outcome record: {e}"),
                })?;
            record.validate().map_err(|e| OutcomeLedgerError::Corrupt {
                run_id: row.run_id.clone(),
                detail: e.to_string(),
            })?;
            verify_projection_columns(row, &record)?;
            Ok(StoredOutcome {
                outcome: record,
                legacy_money: None,
            })
        }
        other => Err(OutcomeLedgerError::Corrupt {
            run_id: row.run_id.clone(),
            detail: format!("unsupported stored payload schema_version {other}"),
        }),
    }
}

/// Convert a legacy v1 payload into the current model plus its historic
/// money, validating the old invariants exactly as the v1 contract did.
fn decode_legacy_row(
    row: &EngineeringOutcomeRow,
    legacy: LegacyRunOutcomeRecordV1,
) -> Result<StoredOutcome, OutcomeLedgerError> {
    let corrupt = |detail: String| OutcomeLedgerError::Corrupt {
        run_id: row.run_id.clone(),
        detail,
    };
    if legacy.schema_version != LEGACY_OUTCOME_RECORD_SCHEMA_V {
        return Err(corrupt(format!(
            "legacy payload schema_version {} is not {LEGACY_OUTCOME_RECORD_SCHEMA_V}",
            legacy.schema_version
        )));
    }
    let step_sum: u128 = legacy
        .model_sequence
        .iter()
        .map(|s| u128::from(s.spend_micro))
        .sum();
    if step_sum != u128::from(legacy.model_spend_micro) {
        return Err(corrupt(format!(
            "legacy model spend {} does not equal the model-sequence spend sum {step_sum}",
            legacy.model_spend_micro
        )));
    }
    if legacy.rework_spend_micro > legacy.model_spend_micro {
        return Err(corrupt(format!(
            "legacy rework spend {} exceeds the model spend {}",
            legacy.rework_spend_micro, legacy.model_spend_micro
        )));
    }
    let outcome = TerminalEngineeringOutcome {
        schema_version: OUTCOME_RECORD_SCHEMA_V,
        run_id: legacy.run_id,
        session_id: legacy.session_id,
        task_id: legacy.task_id,
        task_class: legacy.task_class,
        risk_class: legacy.risk_class,
        candidate_snapshot: legacy.candidate_snapshot,
        final_snapshot: legacy.final_snapshot,
        accepted: legacy.accepted,
        integrated: legacy.integrated,
        verified_first_verification_pass: legacy.verified_first_verification_pass,
        repair_attempts: legacy.repair_attempts,
        model_sequence: legacy
            .model_sequence
            .iter()
            .map(|s| RunModelStep {
                provider: s.provider.clone(),
                model: s.model.clone(),
                local: s.local,
                // Legacy v1 payloads predate the class authority: `Unknown`,
                // with the recorded local bit kept as the honest fallback.
                execution_class: ExecutionClass::Unknown,
            })
            .collect(),
        // Legacy v1 payloads predate the boundary-class and attribution
        // fields: no boundary class authority exists (the sequence keeps the
        // honest legacy local bits) and no attribution completeness was ever
        // recorded — never a claimed class or completeness.
        first_execution_class: ExecutionClass::Unknown,
        final_execution_class: ExecutionClass::Unknown,
        attribution_state: AttributionState::Unavailable,
        wall_time_ms: legacy.wall_time_ms,
        human_interventions: legacy.human_interventions,
        proof: legacy.proof,
        change_scale: legacy.change_scale,
        rollback_events: legacy.rollback_events,
        recovery_events: legacy.recovery_events,
        final_basis: legacy.final_basis,
        frontier_escalations: legacy.frontier_escalations,
        completed_at_ms: legacy.completed_at_ms,
        verified_samples: legacy.verified_samples,
    };
    outcome.validate().map_err(|e| corrupt(e.to_string()))?;
    verify_projection_columns(row, &outcome).map_err(|e| corrupt(e.to_string()))?;
    let legacy_money = LegacyMoney {
        model_spend_micro: legacy.model_spend_micro,
        verification_spend_micro: legacy.verification_spend_micro,
        rework_spend_micro: legacy.rework_spend_micro,
        // The v1 contract documented `avoided_hosted_micro == 0` as
        // "unmeasured": zero was never a measured counterfactual there.
        avoided_hosted: if legacy.avoided_hosted_micro == 0 {
            MeasuredMoney::Unavailable
        } else {
            MeasuredMoney::Measured(legacy.avoided_hosted_micro)
        },
        steps: legacy
            .model_sequence
            .iter()
            .map(|s| SettlementStep {
                provider: s.provider.clone(),
                model: s.model.clone(),
                local: s.local,
                // Legacy v1 payloads predate the durable class authority:
                // their recorded local bit is NOT laundered into a class.
                execution_class: ExecutionClass::Unknown,
                spend_micro: s.spend_micro,
            })
            .collect(),
    };
    Ok(StoredOutcome {
        outcome,
        legacy_money: Some(legacy_money),
    })
}

/// Cross-check the decoded outcome against every projection column.
fn verify_projection_columns(
    row: &EngineeringOutcomeRow,
    record: &TerminalEngineeringOutcome,
) -> Result<(), OutcomeLedgerError> {
    let mismatch = |what: &str| OutcomeLedgerError::Corrupt {
        run_id: row.run_id.clone(),
        detail: format!("payload/projection drift on {what}"),
    };
    if record.run_id != row.run_id {
        return Err(mismatch("run_id"));
    }
    if record.verified() != row.verified {
        return Err(mismatch("verified"));
    }
    if (record.verified() && record.verified_first_verification_pass) != row.verified_first_pass {
        return Err(mismatch("verified_first_pass"));
    }
    if record.integrated != row.integrated {
        return Err(mismatch("integrated"));
    }
    if record.local_used() != row.local_used {
        return Err(mismatch("local_used"));
    }
    if (record.frontier_escalations > 0) != row.frontier_used {
        return Err(mismatch("frontier_used"));
    }
    if i64::try_from(record.session_id) != Ok(row.session_id) {
        return Err(mismatch("session_id"));
    }
    if i64::try_from(record.task_id) != Ok(row.task_id) {
        return Err(mismatch("task_id"));
    }
    if record.completed_at_ms != row.completed_at_ms {
        return Err(mismatch("completed_at_ms"));
    }
    if i64::try_from(record.wall_time_ms) != Ok(row.wall_time_ms)
        || i64::try_from(record.human_interventions) != Ok(row.human_interventions)
        || i64::try_from(record.rollback_events) != Ok(row.rollback_events)
        || i64::try_from(record.recovery_events) != Ok(row.recovery_events)
        || i64::try_from(record.repair_attempts) != Ok(row.repair_attempts)
    {
        return Err(mismatch("counter projection"));
    }
    if risk_class_tag(record.risk_class) != row.risk_class {
        return Err(mismatch("risk_class"));
    }
    if i64::try_from(record.change_scale.files) != Ok(row.changed_files) {
        return Err(mismatch("changed_files"));
    }
    let changed_lines = record
        .change_scale
        .added_lines
        .saturating_add(record.change_scale.removed_lines);
    if i64::try_from(changed_lines) != Ok(row.changed_lines) {
        return Err(mismatch("changed_lines"));
    }
    if i64::try_from(record.proof.checks_passed) != Ok(row.checks_passed)
        || i64::try_from(record.proof.checks_required) != Ok(row.checks_required)
    {
        return Err(mismatch("proof coverage"));
    }
    Ok(())
}

/// Validate a `complete` settlement projection (the agent-level invariants;
/// the store re-validates the row's own shape).
fn validate_complete_projection(
    outcome: &TerminalEngineeringOutcome,
    settlement: &EconomicSettlement,
) -> Result<(), OutcomeLedgerError> {
    if settlement.schema_version != SETTLEMENT_SCHEMA_V {
        return Err(OutcomeLedgerError::Invalid(format!(
            "unsupported settlement schema_version {} (this build writes \
             {SETTLEMENT_SCHEMA_V})",
            settlement.schema_version
        )));
    }
    if settlement.reason.is_empty() {
        return Err(OutcomeLedgerError::Invalid(
            "a settlement event must carry a non-empty reason".into(),
        ));
    }
    let (Some(model), Some(verification), Some(rework)) = (
        settlement.model_spend_micro,
        settlement.verification_spend_micro,
        settlement.rework_spend_micro,
    ) else {
        return Err(OutcomeLedgerError::Invalid(
            "a complete settlement must carry its measured model/verification/rework spend".into(),
        ));
    };
    let _ = verification;
    if rework > model {
        return Err(OutcomeLedgerError::Invalid(format!(
            "settlement rework spend {rework} exceeds the model spend {model}"
        )));
    }
    if outcome.verified() && rework > model {
        return Err(OutcomeLedgerError::Invalid(
            "settlement rework exceeds the verified model spend".into(),
        ));
    }
    if settlement.steps.len() > MAX_RUN_MODEL_STEPS + 1 {
        return Err(OutcomeLedgerError::Invalid(format!(
            "settlement step projection has {} steps (cap {})",
            settlement.steps.len(),
            MAX_RUN_MODEL_STEPS + 1
        )));
    }
    let step_sum: u128 = settlement
        .steps
        .iter()
        .map(|s| u128::from(s.spend_micro))
        .sum();
    if step_sum != u128::from(model) {
        return Err(OutcomeLedgerError::Invalid(format!(
            "settlement step spend sum {step_sum} disagrees with the model spend {model}"
        )));
    }
    for step in &settlement.steps {
        for (what, value) in [("provider", &step.provider), ("model", &step.model)] {
            if value.is_empty() || value.len() > MAX_MODEL_TEXT_BYTES {
                return Err(OutcomeLedgerError::Invalid(format!(
                    "settlement step {what} must be 1..={MAX_MODEL_TEXT_BYTES} bytes, got {}",
                    value.len()
                )));
            }
        }
    }
    Ok(())
}

/// One metrics fold result plus its scan honesty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutcomeLedgerMetricsReport {
    pub metrics: OutcomeLedgerMetrics,
    /// Records folded.
    pub scanned: u64,
    /// True when more than [`MAX_METRICS_RECORDS`] records exist and the
    /// fold covered only the newest cap (never a silent truncation).
    pub truncated: bool,
}

/// The first-class durable engineering-outcome metrics with the P1
/// economic-truth split: ENGINEERING metrics fold every row; MONETARY
/// metrics fold only `complete` settlements and report the exclusion
/// counts. All rates are ppm floor-divided ("at least"); money/time averages
/// round UP. Every denominator of zero yields zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct OutcomeLedgerMetrics {
    pub runs: u64,
    /// Runs that verified: accepted + integrated + deterministic basis.
    pub verified_changes: u64,
    pub verified_first_verification_pass_changes: u64,
    /// Verified First-Pass Rate = first-pass verified / verified.
    pub verified_first_verification_pass_ppm: u32,
    /// Runs whose LATEST settlement is `complete` (the monetary cohort).
    pub money_complete_runs: u64,
    /// Runs whose latest settlement is `pending`.
    pub money_pending_runs: u64,
    /// Runs whose latest settlement is `unavailable`.
    pub money_unavailable_runs: u64,
    /// Pending + unavailable runs: excluded from every monetary metric
    /// (reported, never silently dropped).
    pub money_incomplete_excluded: u64,
    /// Runs whose provider/model attribution is `complete` (the whole
    /// provider-call read, every settled step attributed).
    pub attribution_complete_runs: u64,
    /// Runs whose attribution is `partial` (the provider-call history
    /// truncated and/or a settled step is explicitly unattributed).
    pub attribution_partial_runs: u64,
    /// Runs whose attribution is `unavailable` (settled steps exist but
    /// none carries a provider/model identity — including pre-field legacy
    /// payloads, whose state was never recorded).
    pub attribution_unavailable_runs: u64,
    /// Partial + unavailable runs: their verified samples are EXCLUDED from
    /// the ledger's routing-learning feed (they keep the per-call feed) —
    /// reported, never silently dropped.
    pub attribution_incomplete_excluded: u64,
    /// Verified changes among the money-complete runs: the VCSC
    /// denominator.
    pub verified_changes_money_complete: u64,
    /// model + verification spend over the VERIFIED (successful)
    /// money-complete runs (microUSD).
    pub total_verified_spend_micro: u64,
    /// Successful-Run Cost: `total_verified_spend_micro` / verified
    /// money-complete changes, rounded UP (microUSD).
    pub successful_run_cost_micro: u64,
    /// model + verification spend over ALL money-complete attempted runs
    /// (failed and cancelled runs included; microUSD).
    pub total_spend_micro: u64,
    /// Verified Cost per Successful Change (VCSC): the TOTAL eligible-cohort
    /// spend across attempted money-complete work divided by the successful
    /// verified money-complete changes, rounded UP (microUSD). Incomplete
    /// rows NEVER participate (counted by `money_incomplete_excluded`).
    pub verified_cost_per_successful_change_micro: u64,
    /// Rework spend over all money-complete runs (microUSD).
    pub total_rework_spend_micro: u64,
    /// Rework Cost Ratio = rework spend / total spend (ppm), over the
    /// money-complete cohort.
    pub rework_cost_ratio_ppm: u32,
    /// Runs that used at least one local model.
    pub local_runs: u64,
    /// Runs whose model sequence is ENTIRELY local (at least one step, every
    /// step local).
    pub local_only_runs: u64,
    pub local_only_verified_changes: u64,
    /// Local-Only Verified Rate (ppm).
    pub local_only_verified_ppm: u32,
    /// Runs whose FINAL model step is local.
    pub local_final_runs: u64,
    pub local_final_verified_changes: u64,
    /// Local-Final Verified Rate (ppm).
    pub local_final_verified_ppm: u32,
    /// Runs whose FIRST model step is local.
    pub local_first_runs: u64,
    pub local_first_success_changes: u64,
    /// Local-First Success Rate (ppm).
    pub local_first_success_ppm: u32,
    /// Runs that started local and ENDED on a durably-known HOSTED tier
    /// (fallback, never a substitution).
    pub local_to_hosted_fallback_runs: u64,
    /// Local-to-Hosted Fallback Rate (ppm) over local-first runs.
    pub local_to_hosted_fallback_ppm: u32,
    /// Runs with a model sequence whose boundary execution classes (first
    /// AND final) are BOTH durably established — the local-coverage cohort
    /// and the local mirror of `frontier_classed_runs`. The boundary-derived
    /// local cohorts (`local_first_runs`, `local_final_runs`, the fallback
    /// denominator) can only classify runs inside it.
    pub local_classed_runs: u64,
    /// How much durable boundary-class authority the local classification
    /// rests on (see [`LocalMeasurement`]): `measured` when every run with
    /// steps carries both boundary classes, `partial` when only some do,
    /// `unmeasured` when none does (never a fabricated full coverage).
    pub local_measurement: LocalMeasurement,
    /// Runs whose every step is a durably-known HOSTED tier (no local step,
    /// no unknown-class step). A run with any unknown-class step is counted
    /// as neither local nor hosted.
    pub hosted_only_runs: u64,
    /// Runs with at least one model step: the Hosted-Only Rate denominator
    /// (a run with an empty model sequence carries no hosted evidence and is
    /// excluded from the rate, exactly like its numerator).
    pub runs_with_steps: u64,
    /// Hosted-Only Rate (ppm) over runs with steps (see `runs_with_steps`).
    pub hosted_only_ppm: u32,
    /// Avoided Hosted Inference = the sum of MEASURED counterfactuals
    /// (microUSD). `None` = UNAVAILABLE (no run carried a measurement) —
    /// never a fabricated zero; zero is only a measured zero.
    pub avoided_hosted_inference_micro: Option<u64>,
    /// How many runs carried a MEASURED counterfactual (a partial sum is
    /// visible, never passed off as complete).
    pub avoided_hosted_measured_runs: u64,
    /// Wall time over verified runs (ms).
    pub total_wall_time_ms: u64,
    /// Mean Time to Verified Change (ms, ceil).
    pub mean_time_to_verified_change_ms: u64,
    pub human_intervention_runs: u64,
    /// Human Intervention Rate (ppm).
    pub human_intervention_ppm: u32,
    pub rollback_runs: u64,
    /// Regression/Rollback Rate (ppm).
    pub regression_rollback_ppm: u32,
    pub checks_passed: u64,
    pub checks_required: u64,
    /// Proof Coverage = passed / required over the runs that required any
    /// (ppm); zero required checks carry no evidence.
    pub proof_coverage_ppm: u32,
    pub frontier_escalation_runs: u64,
    /// Runs whose every model step carries a durably-known execution class:
    /// the Frontier Escalation Rate denominator under `partial` coverage
    /// (the `measured` state folds over all runs, the `unmeasured` state has
    /// no rate at all).
    pub frontier_classed_runs: u64,
    /// Frontier-escalation runs among the fully-classed runs: the numerator
    /// the `partial` Frontier Escalation Rate was folded from (a frontier
    /// run with an unknown-class step is never silently assumed in).
    pub frontier_escalation_classed_runs: u64,
    /// Frontier Escalation Rate (ppm): `Some` ONLY when durable
    /// execution-class authority exists; `None` when no run carries a known
    /// class (UNMEASURED — never a fabricated 0% rate). With partial
    /// authority the rate covers exactly the fully-classed runs and the
    /// state says `partial`.
    pub frontier_escalation_ppm: Option<u32>,
    /// How much durable execution-class authority the frontier metric rests
    /// on (see [`FrontierMeasurement`]).
    pub frontier_measurement: FrontierMeasurement,
}

/// Whether the frontier-escalation metric rests on durable route-time
/// execution-class authority (economic audit item 7 P1). Frontier is never
/// inferred: a hosted step with no durable tier authority is class `unknown`
/// and sets no frontier bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontierMeasurement {
    /// Every scanned run with a model sequence carries durably-known
    /// execution classes: the rate is measured over ALL runs.
    Measured,
    /// Some runs carry known classes, others do not: the rate covers exactly
    /// the fully-classed runs (the state reports the partiality; unknown
    /// steps are never silently assumed non-frontier).
    Partial,
    /// No run carries any execution-class authority: the rate is `None`
    /// (`null`) — unmeasured, never a fabricated 0%.
    #[default]
    Unmeasured,
}

impl FrontierMeasurement {
    pub fn as_str(self) -> &'static str {
        match self {
            FrontierMeasurement::Measured => "measured",
            FrontierMeasurement::Partial => "partial",
            FrontierMeasurement::Unmeasured => "unmeasured",
        }
    }
}

/// How much durable boundary-class authority the LOCAL classification
/// (local-first / local-final / local-to-hosted) rests on — the local mirror
/// of [`FrontierMeasurement`]. The boundary classes are preserved exactly on
/// the record (`first_execution_class` / `final_execution_class`), so a
/// folded display sequence no longer depresses the local cohorts; this state
/// reports how many runs the boundary authority actually covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalMeasurement {
    /// Every run with a model sequence carries both boundary classes: the
    /// local cohorts cover the whole population.
    Measured,
    /// Some runs carry boundary classes, others do not (e.g. an unpriced
    /// boundary step, or a pre-field payload whose folded tail genuinely
    /// lost the final class): the local cohorts cover exactly the classed
    /// runs, reported by `local_classed_runs`.
    Partial,
    /// No run carries any boundary-class authority: the local cohorts have
    /// no measured evidence — never presented as full coverage.
    #[default]
    Unmeasured,
}

impl LocalMeasurement {
    pub fn as_str(self) -> &'static str {
        match self {
            LocalMeasurement::Measured => "measured",
            LocalMeasurement::Partial => "partial",
            LocalMeasurement::Unmeasured => "unmeasured",
        }
    }
}

impl OutcomeLedgerMetrics {
    /// Pure, integer-exact fold over a ledger entry set (saturating u128
    /// intermediates; never floating point). Engineering metrics fold every
    /// entry; monetary metrics fold ONLY `complete` settlements.
    pub fn compute(entries: &[OutcomeLedgerEntry]) -> Self {
        let mut m = OutcomeLedgerMetrics::default();
        let mut verified_spend: u128 = 0;
        let mut verified_wall: u128 = 0;
        let mut total_spend: u128 = 0;
        let mut rework_spend: u128 = 0;
        let mut avoided: u128 = 0;
        let mut avoided_measured_runs: u128 = 0;
        let mut checks_passed: u128 = 0;
        let mut checks_required: u128 = 0;
        let mut local_runs: u128 = 0;
        let mut local_only_runs: u128 = 0;
        let mut local_only_verified: u128 = 0;
        let mut local_final_runs: u128 = 0;
        let mut local_final_verified: u128 = 0;
        let mut local_first_runs: u128 = 0;
        let mut local_first_success: u128 = 0;
        let mut local_to_hosted: u128 = 0;
        let mut hosted_only: u128 = 0;
        let mut intervention_runs: u128 = 0;
        let mut rollback_runs: u128 = 0;
        let mut frontier_runs: u128 = 0;
        let mut frontier_runs_classed: u128 = 0;
        let mut runs_with_steps: u128 = 0;
        let mut classed_runs: u128 = 0;
        let mut any_known_class = false;
        let mut local_classed_runs: u128 = 0;
        let mut any_boundary_class = false;
        let mut verified_first_pass: u128 = 0;
        let mut verified_money_complete: u128 = 0;
        for entry in entries {
            let record = &entry.outcome;
            m.runs = m.runs.saturating_add(1);
            match entry.settlement.money_state {
                MoneyState::Complete => {
                    m.money_complete_runs = m.money_complete_runs.saturating_add(1)
                }
                MoneyState::Pending => {
                    m.money_pending_runs = m.money_pending_runs.saturating_add(1)
                }
                MoneyState::Unavailable => {
                    m.money_unavailable_runs = m.money_unavailable_runs.saturating_add(1)
                }
            }
            // Only a complete projection carries amounts; the derivation
            // never fabricates a zero for an unsettled fact.
            if entry.settlement.money_state == MoneyState::Complete {
                let model = entry.settlement.model_spend_micro.unwrap_or(0);
                let verification = entry.settlement.verification_spend_micro.unwrap_or(0);
                let spend = u128::from(model).saturating_add(u128::from(verification));
                total_spend = total_spend.saturating_add(spend);
                rework_spend = rework_spend
                    .saturating_add(u128::from(entry.settlement.rework_spend_micro.unwrap_or(0)));
                if record.verified() {
                    verified_money_complete = verified_money_complete.saturating_add(1);
                    verified_spend = verified_spend.saturating_add(spend);
                }
            }
            if let Some(value) = entry.settlement.avoided_hosted.value() {
                avoided = avoided.saturating_add(u128::from(value));
                avoided_measured_runs = avoided_measured_runs.saturating_add(1);
            }
            // Attribution honesty: recorded per row, counted here, never
            // silently folded into a completeness claim.
            match record.attribution_state {
                AttributionState::Complete => {
                    m.attribution_complete_runs = m.attribution_complete_runs.saturating_add(1)
                }
                AttributionState::Partial => {
                    m.attribution_partial_runs = m.attribution_partial_runs.saturating_add(1)
                }
                AttributionState::Unavailable => {
                    m.attribution_unavailable_runs =
                        m.attribution_unavailable_runs.saturating_add(1)
                }
            }
            let steps = &record.model_sequence;
            // P1 classification: every local/hosted measure is decided by the
            // durable execution class, never by a cost number. Unknown is
            // counted as NEITHER local nor hosted. The FIRST/FINAL measures
            // read the exactly-preserved boundary classes (with their
            // documented legacy fallback), so a folded display sequence
            // never depresses the local cohorts.
            let first_class = record.effective_first_execution_class();
            let final_class = record.effective_final_execution_class();
            let first_local = first_class.is_local();
            let last_local = final_class.is_local();
            let last_hosted = final_class.is_hosted();
            let all_local = !steps.is_empty() && steps.iter().all(|s| s.execution_class.is_local());
            let all_hosted =
                !steps.is_empty() && steps.iter().all(|s| s.execution_class.is_hosted());
            let any_local = steps.iter().any(|s| s.execution_class.is_local());
            if !steps.is_empty() {
                runs_with_steps = runs_with_steps.saturating_add(1);
                let all_known = steps.iter().all(|s| s.execution_class.is_known());
                if all_known {
                    classed_runs = classed_runs.saturating_add(1);
                }
                any_known_class |= steps.iter().any(|s| s.execution_class.is_known());
                // The LOCAL coverage cohort: runs with BOTH boundary classes
                // durably established. A run with only one boundary class is
                // boundary authority for the rates of that boundary but not
                // full coverage, so it reports `partial` (never measured).
                if first_class.is_known() && final_class.is_known() {
                    local_classed_runs = local_classed_runs.saturating_add(1);
                }
                if first_class.is_known() || final_class.is_known() {
                    any_boundary_class = true;
                }
            }
            if any_local {
                local_runs = local_runs.saturating_add(1);
            }
            if !steps.is_empty() && all_local {
                local_only_runs = local_only_runs.saturating_add(1);
                if record.verified() {
                    local_only_verified = local_only_verified.saturating_add(1);
                }
            }
            if !steps.is_empty() && last_local {
                local_final_runs = local_final_runs.saturating_add(1);
                if record.verified() {
                    local_final_verified = local_final_verified.saturating_add(1);
                }
            }
            if first_local {
                local_first_runs = local_first_runs.saturating_add(1);
                if last_hosted {
                    // Started local, ENDED on a durably-known hosted tier: a
                    // fallback, never a substitution — it must not count
                    // toward any local success rate.
                    local_to_hosted = local_to_hosted.saturating_add(1);
                } else if last_local && record.verified() && record.verified_first_verification_pass
                {
                    local_first_success = local_first_success.saturating_add(1);
                }
            }
            if all_hosted {
                hosted_only = hosted_only.saturating_add(1);
            }
            if record.human_interventions > 0 {
                intervention_runs = intervention_runs.saturating_add(1);
            }
            if record.rollback_events > 0 {
                rollback_runs = rollback_runs.saturating_add(1);
            }
            // Frontier escalations: the recorded counter (0-written until a
            // frontier-tier authority exists) OR a durably-known Frontier
            // step. Never inferred from a hosted step without a tier.
            let frontier_run = record.frontier_escalations > 0
                || steps
                    .iter()
                    .any(|s| s.execution_class == ExecutionClass::Frontier);
            if frontier_run {
                frontier_runs = frontier_runs.saturating_add(1);
                // Only a run WITH steps can carry full class authority: an
                // empty sequence proves nothing and never enters the classed
                // cohort (mirrors `classed_runs`, which is also gated on a
                // non-empty sequence).
                if !steps.is_empty() && steps.iter().all(|s| s.execution_class.is_known()) {
                    frontier_runs_classed = frontier_runs_classed.saturating_add(1);
                }
            }
            if record.proof.checks_required > 0 {
                checks_required =
                    checks_required.saturating_add(u128::from(record.proof.checks_required));
                checks_passed = checks_passed.saturating_add(u128::from(
                    record.proof.checks_passed.min(record.proof.checks_required),
                ));
            }
            if record.verified() {
                m.verified_changes = m.verified_changes.saturating_add(1);
                verified_wall = verified_wall.saturating_add(u128::from(record.wall_time_ms));
                if record.verified_first_verification_pass {
                    verified_first_pass = verified_first_pass.saturating_add(1);
                }
            }
        }
        let runs = u128::from(m.runs);
        m.money_incomplete_excluded = m
            .money_pending_runs
            .saturating_add(m.money_unavailable_runs);
        m.attribution_incomplete_excluded = m
            .attribution_partial_runs
            .saturating_add(m.attribution_unavailable_runs);
        m.verified_changes_money_complete = sat_u64(verified_money_complete);
        m.verified_first_verification_pass_changes = sat_u64(verified_first_pass);
        m.local_runs = sat_u64(local_runs);
        m.local_only_runs = sat_u64(local_only_runs);
        m.local_only_verified_changes = sat_u64(local_only_verified);
        m.local_final_runs = sat_u64(local_final_runs);
        m.local_final_verified_changes = sat_u64(local_final_verified);
        m.local_first_runs = sat_u64(local_first_runs);
        m.local_first_success_changes = sat_u64(local_first_success);
        m.local_to_hosted_fallback_runs = sat_u64(local_to_hosted);
        m.hosted_only_runs = sat_u64(hosted_only);
        m.runs_with_steps = sat_u64(runs_with_steps);
        m.local_classed_runs = sat_u64(local_classed_runs);
        m.human_intervention_runs = sat_u64(intervention_runs);
        m.rollback_runs = sat_u64(rollback_runs);
        m.frontier_escalation_runs = sat_u64(frontier_runs);
        m.frontier_classed_runs = sat_u64(classed_runs);
        m.frontier_escalation_classed_runs = sat_u64(frontier_runs_classed);
        m.total_verified_spend_micro = sat_u64(verified_spend);
        m.total_spend_micro = sat_u64(total_spend);
        m.total_rework_spend_micro = sat_u64(rework_spend);
        m.avoided_hosted_measured_runs = sat_u64(avoided_measured_runs);
        m.avoided_hosted_inference_micro = if avoided_measured_runs == 0 {
            None
        } else {
            Some(sat_u64(avoided))
        };
        m.total_wall_time_ms = sat_u64(verified_wall);
        m.checks_passed = sat_u64(checks_passed);
        m.checks_required = sat_u64(checks_required);
        // Rates: floor-division ppm ("at least"). Zero denominators -> 0.
        m.verified_first_verification_pass_ppm =
            meaning_ppm(verified_first_pass, u128::from(m.verified_changes));
        m.rework_cost_ratio_ppm = meaning_ppm(rework_spend, total_spend);
        m.local_only_verified_ppm = meaning_ppm(local_only_verified, local_only_runs);
        m.local_final_verified_ppm = meaning_ppm(local_final_verified, local_final_runs);
        m.local_first_success_ppm = meaning_ppm(local_first_success, local_first_runs);
        m.local_to_hosted_fallback_ppm = meaning_ppm(local_to_hosted, local_first_runs);
        m.hosted_only_ppm = meaning_ppm(hosted_only, runs_with_steps);
        m.human_intervention_ppm = meaning_ppm(intervention_runs, runs);
        m.regression_rollback_ppm = meaning_ppm(rollback_runs, runs);
        m.proof_coverage_ppm = meaning_ppm(checks_passed, checks_required);
        // Frontier honesty (P1): without durable execution-class authority
        // the rate is NULL with an `unmeasured` state — never a fabricated
        // 0%; with partial authority it covers exactly the fully-classed
        // runs; only a fully classed cohort measures over all runs.
        m.frontier_measurement = if !any_known_class {
            FrontierMeasurement::Unmeasured
        } else if classed_runs == runs_with_steps {
            FrontierMeasurement::Measured
        } else {
            FrontierMeasurement::Partial
        };
        m.frontier_escalation_ppm = match m.frontier_measurement {
            FrontierMeasurement::Unmeasured => None,
            FrontierMeasurement::Measured => Some(meaning_ppm(frontier_runs, runs)),
            FrontierMeasurement::Partial => {
                (classed_runs > 0).then(|| meaning_ppm(frontier_runs_classed, classed_runs))
            }
        };
        // Local coverage (mirrors the frontier rule): without any boundary
        // class authority the state is `unmeasured` (never a fabricated full
        // coverage); with all runs with steps classed it is `measured`; in
        // between the local cohorts cover exactly the classed runs and the
        // state reports the partiality.
        m.local_measurement = if !any_boundary_class {
            LocalMeasurement::Unmeasured
        } else if local_classed_runs == runs_with_steps {
            LocalMeasurement::Measured
        } else {
            LocalMeasurement::Partial
        };
        // Money/time averages: ceil ("never understate"); the VCSC
        // denominator is the money-complete verified cohort ONLY.
        m.successful_run_cost_micro = ceil_div_u64(verified_spend, verified_money_complete);
        m.verified_cost_per_successful_change_micro =
            ceil_div_u64(total_spend, verified_money_complete);
        m.mean_time_to_verified_change_ms =
            ceil_div_u64(verified_wall, u128::from(m.verified_changes));
        m
    }
}

/// Saturating `u128 -> u64` for folded sums.
fn sat_u64(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// Floor ppm of `n/d`, clamped to 100% and zero on an empty denominator.
fn meaning_ppm(n: u128, d: u128) -> u32 {
    if d == 0 {
        return 0;
    }
    u32::try_from((n.saturating_mul(1_000_000) / d).min(1_000_000)).unwrap_or(1_000_000)
}

/// Ceiling division of `n/d` (0 when the denominator is empty).
fn ceil_div_u64(n: u128, d: u128) -> u64 {
    if d == 0 {
        return 0;
    }
    sat_u64(n.div_ceil(d))
}

// ---------------------------------------------------------------------------
// The production producer: terminal run settlement -> one ledger record
// ---------------------------------------------------------------------------

/// The canonical per-session terminal-run identity (`tx-session-<id>`): the
/// SAME identity the task executor's in-session settlement uses, so the
/// runtime's terminal gate and the executor's settlement describe ONE run.
pub fn session_terminal_run_id(session: faktor_core::id::SessionId) -> String {
    format!("tx-session-{}", session.raw())
}

/// The non-derivable facts of one terminal run, persisted FIRST into the
/// store's durable outbox before any producer can observe the task terminal:
/// the semantic risk bucket and the terminal turn's explicit verified
/// samples. Everything else is derived from durable rows by
/// [`OutcomeLedger::record_terminal_run`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRunFacts {
    pub schema_version: u32,
    /// The risk bucket of the terminal turn's verified attribution. Only
    /// meaningful when `samples` is non-empty; records without samples use
    /// the honest default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_class: Option<RiskBucket>,
    /// The terminal turn's explicit per-call verified samples. Every sample
    /// rides the record's atomic append (the store absorbs them into the
    /// router registry IN the record's transaction).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<RunVerifiedSample>,
}

impl TerminalRunFacts {
    /// The honest default: no terminal-turn attribution was persisted.
    pub fn none() -> Self {
        Self {
            schema_version: TERMINAL_FACTS_SCHEMA_V,
            risk_class: None,
            samples: Vec::new(),
        }
    }

    /// Bounded structural validation before the durable upsert.
    pub fn validate(&self) -> Result<(), OutcomeLedgerError> {
        if self.schema_version != TERMINAL_FACTS_SCHEMA_V {
            return Err(OutcomeLedgerError::Invalid(format!(
                "unsupported terminal-run-facts schema_version {} (this build writes \
                 {TERMINAL_FACTS_SCHEMA_V})",
                self.schema_version
            )));
        }
        if self.samples.len() > MAX_RUN_VERIFIED_SAMPLES {
            return Err(OutcomeLedgerError::Invalid(format!(
                "{} terminal-run samples exceed the cap {MAX_RUN_VERIFIED_SAMPLES}",
                self.samples.len()
            )));
        }
        for sample in &self.samples {
            for (what, value) in [("provider", &sample.provider), ("model", &sample.model)] {
                if value.is_empty() || value.len() > MAX_MODEL_TEXT_BYTES {
                    return Err(OutcomeLedgerError::Invalid(format!(
                        "terminal-run sample {what} must be 1..={MAX_MODEL_TEXT_BYTES} bytes, got \
                         {}",
                        value.len()
                    )));
                }
            }
            if sample.verified_success
                && (sample.rework_cost_micro != 0 || sample.rework_turns != 0)
            {
                return Err(OutcomeLedgerError::Invalid(
                    "a terminal-run verified success cannot carry rework".into(),
                ));
            }
        }
        Ok(())
    }
}

/// The durable terminal-run-facts outbox of one run, persisted by the agent
/// path BEFORE the task row can become terminal. Freezes at record time.
#[derive(Debug, Clone)]
pub struct TerminalRunFactsOutbox {
    store: Arc<Store>,
}

impl TerminalRunFactsOutbox {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Persist (or replace) the run's terminal-turn facts. Refused once the
    /// engineering record exists (the facts are frozen).
    pub fn persist(
        &self,
        run_id: &str,
        facts: &TerminalRunFacts,
    ) -> Result<(), OutcomeLedgerError> {
        facts.validate()?;
        let payload = serde_json::to_string(facts).map_err(|e| {
            OutcomeLedgerError::Invalid(format!("terminal-run facts serialization failed: {e}"))
        })?;
        self.store.terminal_run_facts_upsert(run_id, &payload)?;
        Ok(())
    }

    /// The run's durable terminal-turn facts, or the honest default.
    pub fn load(&self, run_id: &str) -> Result<TerminalRunFacts, OutcomeLedgerError> {
        let Some(payload) = self.store.terminal_run_facts_get(run_id)? else {
            return Ok(TerminalRunFacts::none());
        };
        let facts: TerminalRunFacts = serde_json::from_str(&payload).map_err(|e| {
            OutcomeLedgerError::Facts(format!(
                "stored terminal-run facts of {run_id} do not decode: {e}"
            ))
        })?;
        facts.validate().map_err(|e| {
            OutcomeLedgerError::Facts(format!(
                "stored terminal-run facts of {run_id} are invalid: {e}"
            ))
        })?;
        Ok(facts)
    }
}

/// The derived economic projection of one run before it becomes a durable
/// settlement event.
struct EconomicProjection {
    state: MoneyState,
    reason: String,
    model_spend_micro: Option<u64>,
    verification_spend_micro: Option<u64>,
    rework_spend_micro: Option<u64>,
    avoided_hosted: MeasuredMoney,
    steps: Vec<SettlementStep>,
}

impl EconomicProjection {
    fn into_event(self, run_id: &str, seq: u64, settled_at_ms: i64) -> EconomicSettlement {
        EconomicSettlement {
            schema_version: SETTLEMENT_SCHEMA_V,
            run_id: run_id.to_string(),
            seq,
            money_state: self.state,
            reason: self.reason,
            model_spend_micro: self.model_spend_micro,
            verification_spend_micro: self.verification_spend_micro,
            rework_spend_micro: self.rework_spend_micro,
            avoided_hosted: self.avoided_hosted,
            steps: self.steps,
            settled_at_ms,
        }
    }
}

/// Group the settled spend rows into the run's provider/model step
/// projection (occurrence order, first-seen groups, one explicit aggregate
/// tail past the cap; the spend sum stays exact). Returns the engineering
/// steps (identity + durable execution class), the financial steps (with
/// spend) and the EXACT boundary execution classes (first and final settled
/// row) — preserved independent of the bounded display sequence, so the
/// local-first/local-final/local-to-hosted classification survives folding.
///
/// P1 classification: the class comes from the reservation's DURABLE
/// route-time authority ([`faktor_session::DurableSpendRow::effective_execution_class`]);
/// a numeric zero is NEVER locality — a hosted zero-price row stays
/// `Unknown` and is counted as neither local nor hosted.
fn group_spend_rows(
    spend: &[faktor_session::DurableSpendRow],
) -> (
    Vec<RunModelStep>,
    Vec<SettlementStep>,
    u128,
    ExecutionClass,
    ExecutionClass,
) {
    let mut groups: Vec<RunModelStep> = Vec::new();
    let mut financial: Vec<SettlementStep> = Vec::new();
    let mut folded_groups: u64 = 0;
    let mut folded_spend: u128 = 0;
    let mut model_spend: u128 = 0;
    // The session read returns newest-first; the projection is in EXECUTION
    // order (oldest first), ties broken by reservation id (creation order),
    // so first/last-step classification is deterministic.
    let mut ordered: Vec<&faktor_session::DurableSpendRow> = spend.iter().collect();
    ordered.sort_by_key(|row| (row.occurred_at_ms, row.reservation_id));
    // The EXACT boundary classes: taken from the ordered settled rows, not
    // from the possibly-folded display sequence below.
    let first_class = ordered
        .first()
        .map(|row| row.effective_execution_class())
        .unwrap_or(ExecutionClass::Unknown);
    let final_class = ordered
        .last()
        .map(|row| row.effective_execution_class())
        .unwrap_or(ExecutionClass::Unknown);
    for row in ordered {
        model_spend = model_spend.saturating_add(u128::from(row.provider_cost_micro));
        let execution_class = row.effective_execution_class();
        let local = execution_class.is_local();
        if let Some(last) = groups.last_mut() {
            if last.provider == row.provider
                && last.model == row.model
                && last.execution_class == execution_class
            {
                if let Some(fin) = financial.last_mut() {
                    fin.spend_micro = fin.spend_micro.saturating_add(row.provider_cost_micro);
                }
                continue;
            }
        }
        if groups.len() < MAX_RUN_MODEL_STEPS - 1 {
            groups.push(RunModelStep {
                provider: row.provider.clone(),
                model: row.model.clone(),
                local,
                execution_class,
            });
            financial.push(SettlementStep {
                provider: row.provider.clone(),
                model: row.model.clone(),
                local,
                execution_class,
                spend_micro: row.provider_cost_micro,
            });
        } else {
            folded_groups = folded_groups.saturating_add(1);
            folded_spend = folded_spend.saturating_add(u128::from(row.provider_cost_micro));
        }
    }
    if folded_groups > 0 {
        // The aggregate tail folds steps of mixed classes: it carries the
        // honest `Unknown` class (never a guessed local or hosted one).
        groups.push(RunModelStep {
            provider: "aggregate".into(),
            model: format!("{folded_groups} folded steps"),
            local: false,
            execution_class: ExecutionClass::Unknown,
        });
        financial.push(SettlementStep {
            provider: "aggregate".into(),
            model: format!("{folded_groups} folded steps"),
            local: false,
            execution_class: ExecutionClass::Unknown,
            spend_micro: sat_u64(folded_spend),
        });
    }
    (groups, financial, model_spend, first_class, final_class)
}

/// The recorded attribution honesty of one run's model sequence from the
/// bounded provider-call projection ONLY (the settled rows, the truncation
/// flag and the unattributed count — never the folded display sequence):
///
/// - no settled step: `complete` (nothing to attribute);
/// - settled steps exist but none is attributed: `unavailable` (no
///   provider/model identity authority at all);
/// - the call read truncated AND/OR some settled step is unattributed:
///   `partial`;
/// - a whole call read with every settled step attributed: `complete`.
fn derive_attribution_state(
    projection: &faktor_session::DurableSpendProjection,
) -> AttributionState {
    if projection.rows.is_empty() {
        return AttributionState::Complete;
    }
    let attributed = projection
        .rows
        .iter()
        .filter(|row| row.provider != faktor_session::UNATTRIBUTED_PROVENANCE)
        .count();
    if attributed == 0 {
        return AttributionState::Unavailable;
    }
    if projection.call_rows_truncated || attributed < projection.rows.len() {
        return AttributionState::Partial;
    }
    AttributionState::Complete
}

/// The settled rework spend: settled rows STRICTLY AFTER the first failed
/// attempt's completion; zero when no attempt failed (never fabricated).
fn derive_rework_spend(
    spend: &[faktor_session::DurableSpendRow],
    records: &[faktor_session::VerificationRecord],
    model_spend: u128,
) -> u64 {
    match records
        .iter()
        .find(|r| r.status == crate::VerificationStatus::Failed)
    {
        Some(first_failure) => {
            let at = first_failure
                .completed_ms
                .unwrap_or(first_failure.started_ms);
            let rework: u128 = spend
                .iter()
                .filter(|row| row.occurred_at_ms > at)
                .map(|row| u128::from(row.provider_cost_micro))
                .sum();
            sat_u64(rework).min(sat_u64(model_spend))
        }
        None => 0,
    }
}

impl OutcomeLedger {
    /// The PRODUCTION producer (economic audit item 7 P1): record ONE
    /// completed run from its durable, terminal state. Called at the
    /// terminal settlement of every run — the runtime's completion gate and
    /// the task executor's post-run settlement — and idempotent by
    /// construction:
    ///
    /// - an already-recorded run id returns `Ok(None)` WITHOUT re-deriving
    ///   (a replay never rewrites and never double-feeds the router);
    /// - the terminal-turn facts are read from the DURABLE outbox, never
    ///   from call-site arguments: every producer derives byte-identical
    ///   rows for every legal interleaving (the dual-producer invariant);
    /// - otherwise the engineering record is derived purely from durable
    ///   facts and validated before the append: task identity/state/time,
    ///   the verification records, the settled spend rows, the durable
    ///   integration record, the integration transaction, the permission
    ///   decisions and the recovery events;
    /// - the bounded provider-call history never silently truncates: a
    ///   truncated read (or an explicitly unattributed settled step) types
    ///   the record's `attribution_state` `partial`/`unavailable`, and the
    ///   record's samples are then excluded from the routing-learning feed
    ///   (they keep the per-call feed);
    /// - the money projection is derived from the reservation ledger: an
    ///   open/dispatched/uncertain reservation types the initial settlement
    ///   `pending` (amounts NULL), an unreadable ledger or an unmetered
    ///   settlement types it `unavailable`, and only a fully settled ledger
    ///   types it `complete` with measured amounts;
    /// - the append is the store's exactly-once immediate transaction: the
    ///   row, the `seq = 0` settlement event and the router samples commit
    ///   together or not at all, and a byte-identical replay is a
    ///   `Replayed` no-op.
    pub fn record_terminal_run(
        &self,
        handle: &faktor_session::SessionHandle,
        run_id: &str,
        task_id: faktor_core::id::TaskId,
    ) -> Result<Option<LedgerAppend>, OutcomeLedgerError> {
        if self.get(run_id)?.is_some() {
            return Ok(None);
        }
        let facts = TerminalRunFactsOutbox::new(self.store.clone()).load(run_id)?;
        let (outcome, settlement) = self.derive_terminal_run(handle, run_id, task_id, &facts)?;
        Ok(Some(self.record(&outcome, &settlement)?))
    }

    /// Late economic settlement finalization: re-derive the financial
    /// projection from durable rows and append the next settlement event
    /// when the money MOVED. Never wedges:
    ///
    /// - a semantically identical re-derivation is a no-op at any clock;
    /// - a NON-final state never rewrites itself: a changed `pending` or
    ///   `unavailable` reason is diagnostic drift, not a new financial fact
    ///   (the store lifecycle forbids those transitions), so the first event
    ///   stands until the money reaches a final state — `Unchanged`;
    /// - `unavailable -> pending` is a legal improvement (a recovered read
    ///   established that reservations are open), as are `unavailable ->
    ///   complete` and `pending -> complete|unavailable`;
    /// - `complete` is FINAL: a later non-complete re-derivation never
    ///   rewrites it (`AlreadySettled`), but a SEMANTICALLY DIFFERENT later
    ///   `complete` re-derivation IS appended as the documented
    ///   complete -> complete correction.
    pub fn finalize_economic_settlement(
        &self,
        handle: &faktor_session::SessionHandle,
        run_id: &str,
    ) -> Result<SettlementFinalize, OutcomeLedgerError> {
        let Some(stored) = self.load_stored(run_id)? else {
            return Ok(SettlementFinalize::Absent);
        };
        let latest = self.store.economic_settlement_latest(run_id)?;
        let latest_decoded = latest
            .as_ref()
            .map(EconomicSettlement::from_row)
            .transpose()?;
        if latest.is_none() && stored.legacy_money.is_some() {
            // Legacy rows synthesize a complete projection: nothing to move.
            return Ok(SettlementFinalize::AlreadySettled);
        }
        // The run's admitted task identity, frozen with its engineering
        // record. Re-reading the session's CURRENT identity here would fold a
        // later task's reservation ledger into this run's settlement.
        let task_id = faktor_core::id::TaskId::new(stored.outcome.task_id);
        let projection = self.derive_economic_projection(handle, task_id)?;
        let event = projection.into_event(run_id, 0, handle.now_ms());
        if let Some(latest) = &latest_decoded {
            if latest.money_state == event.money_state {
                match event.money_state {
                    // A correction only lands when the settled projection
                    // actually MOVED; the identical replay stays a no-op.
                    MoneyState::Complete => {
                        if settlements_semantically_equal(latest, &event) {
                            return Ok(SettlementFinalize::AlreadySettled);
                        }
                    }
                    // A non-final state never rewrites itself (the store
                    // forbids pending->pending / unavailable->unavailable
                    // with a different fact): return the deterministic no-op
                    // instead of a forever-conflict.
                    MoneyState::Pending | MoneyState::Unavailable => {
                        return Ok(SettlementFinalize::Unchanged);
                    }
                }
            } else if latest.money_state == MoneyState::Complete {
                // complete is FINAL: a later non-complete re-derivation never
                // rewrites it (the store refuses the transition).
                return Ok(SettlementFinalize::AlreadySettled);
            }
            // Everything else is a legal forward move: pending -> complete |
            // unavailable, unavailable -> complete | pending.
        }
        match self.store.economic_settlement_append(&event.to_row()?)? {
            faktor_store::EconomicSettlementAppend::Appended(_) => Ok(SettlementFinalize::Appended),
            faktor_store::EconomicSettlementAppend::Duplicate => Ok(SettlementFinalize::Unchanged),
        }
    }

    /// Derive the terminal engineering record AND the initial settlement
    /// projection from durable rows only. Every read is fallible; a failed
    /// spend read refuses the record (never a partial or fabricated one).
    /// Deterministic over frozen terminal state, so a concurrent
    /// re-derivation of the same run produces a byte-identical row.
    fn derive_terminal_run(
        &self,
        handle: &faktor_session::SessionHandle,
        run_id: &str,
        task_id: faktor_core::id::TaskId,
        facts: &TerminalRunFacts,
    ) -> Result<(TerminalEngineeringOutcome, EconomicSettlement), OutcomeLedgerError> {
        // The FIRST money read of the derivation is the reservation state:
        // the settled-spend read below may only ever be a superset, so a
        // `complete` classification can never miss a reservation that was
        // open when this read ran (the documented read order).
        let reservations = self.read_reservation_state(handle, task_id);
        self.note_reservation_read();
        let task = handle
            .get_task(task_id)
            .map_err(|e| OutcomeLedgerError::Facts(format!("task row read failed: {e}")))?
            .ok_or_else(|| {
                OutcomeLedgerError::Facts(format!(
                    "terminal run {run_id} has no durable task row; refusing to record"
                ))
            })?;
        let revision = handle
            .task_revision(task_id)
            .map_err(|e| OutcomeLedgerError::Facts(format!("task revision read failed: {e}")))?;
        let records = handle.list_verification_records(task_id).map_err(|e| {
            OutcomeLedgerError::Facts(format!("verification records read failed: {e}"))
        })?;
        // The settled-spend PROJECTION (never the strict rows fold): the
        // immutable record must exist even when the bounded provider-call
        // history truncated, so the truncation is carried EXPLICITLY into
        // the record's attribution state instead of being discarded.
        let spend_projection = handle
            .durable_spend_projection_bounded(
                task_id,
                faktor_session::MAX_DURABLE_SPEND_ROWS,
                self.spend_call_bound(),
            )
            .map_err(|e| {
                OutcomeLedgerError::Facts(format!("settled spend rows read failed: {e}"))
            })?;
        let attribution_state = derive_attribution_state(&spend_projection);
        let mut spend = spend_projection.rows;
        // The durable read returns newest-first; the model SEQUENCE is the
        // chronological story (the first implementation attempt first) and
        // the rework window is order-sensitive, so order once by the durable
        // reservation key.
        spend.sort_by_key(|row| row.reservation_id);
        let integration = handle
            .ledger_integration_record_for_task(task_id.raw())
            .map_err(|e| {
                OutcomeLedgerError::Facts(format!("integration record read failed: {e}"))
            })?;
        let txn = match handle.ledger_integration_txn_read(run_id) {
            faktor_session::ledger::DurableRead::PresentValid(txn) => Some(txn),
            faktor_session::ledger::DurableRead::Missing => None,
            faktor_session::ledger::DurableRead::PresentMalformed(detail) => {
                return Err(OutcomeLedgerError::Facts(format!(
                    "integration transaction of run {run_id} is corrupt: {detail}"
                )))
            }
            faktor_session::ledger::DurableRead::StoreFailure(detail) => {
                return Err(OutcomeLedgerError::Facts(format!(
                    "integration transaction read failed: {detail}"
                )))
            }
        };
        let store = &self.store;

        let accepted = task.state == crate::TaskState::VerifiedComplete;
        let completed_at_ms = task.updated_ms.max(task.created_ms);
        let wall_time_ms =
            u64::try_from(completed_at_ms.saturating_sub(task.created_ms)).unwrap_or(0);
        // Verification attempts: records are in creation order, so the first
        // record is the run's FIRST verification attempt. The flag this
        // ledger exposes is a verification-pass measure: that first attempt
        // passed. It is deliberately NOT derived from implementation
        // attempts — no durable implementation-attempt identity exists
        // (`provider_call.attempt_op_id`/`attempt_ordinal` are physical wire
        // retries of ONE logical model call; `verification_record` rows
        // exist only once verification ran), so several implementation/model
        // attempts before the first verification can neither set nor clear
        // it. For an accepted run no failed record can follow the certifying
        // pass (terminal task rows are frozen), so "no failed verification
        // record" and "the earliest record passed" are the same predicate.
        let first_verification_passed = records
            .first()
            .is_some_and(|r| r.status == crate::VerificationStatus::Passed);
        let repair_attempts = records
            .iter()
            .filter(|r| r.status == crate::VerificationStatus::Failed)
            .count() as u64;
        let certifying = if accepted {
            records
                .iter()
                .rev()
                .find(|r| r.status == crate::VerificationStatus::Passed)
        } else {
            records.last()
        };
        let (checks_passed, checks_required) = match certifying {
            Some(record) => (
                record
                    .checks
                    .iter()
                    .filter(|c| c.status == crate::VerificationStatus::Passed)
                    .count() as u64,
                record.checks.len() as u64,
            ),
            None => (0, 0),
        };
        let has_review = certifying.is_some_and(|r| r.reviewer.is_some());
        let final_basis = if !accepted {
            FinalVerificationBasis::Unverified
        } else if checks_required > 0 {
            if has_review {
                FinalVerificationBasis::DeterministicChecksAndReview
            } else {
                FinalVerificationBasis::DeterministicChecks
            }
        } else if has_review {
            FinalVerificationBasis::ReviewOnly
        } else {
            FinalVerificationBasis::Unverified
        };
        let verified = accepted && final_basis.is_deterministic_verified();
        let (groups, settlement_steps, model_spend, first_execution_class, final_execution_class) =
            group_spend_rows(&spend);
        let rework_spend_micro = derive_rework_spend(&spend, &records, model_spend);
        let candidate_snapshot = integration
            .as_ref()
            .and_then(|r| r.candidate_snapshot.clone())
            .or_else(|| {
                certifying
                    .and_then(|r| r.candidate_proof_ref.as_ref())
                    .and_then(|r| r.candidate_snapshot.clone())
            });
        let final_snapshot = integration
            .as_ref()
            .and_then(|r| r.landed_snapshot.clone())
            .or_else(|| {
                integration
                    .as_ref()
                    .and_then(|r| r.candidate_snapshot.clone())
            })
            .or_else(|| candidate_snapshot.clone())
            .or_else(|| certifying.and_then(|r| r.tree_hash.clone()))
            .or_else(|| {
                certifying
                    .and_then(|r| r.environment_fingerprint.as_ref())
                    .and_then(|f| f.base_tree_hash.clone())
            })
            .unwrap_or_else(|| format!("unavailable:rev-{}", revision.raw()));
        let changed_files = certifying
            .map(|r| r.changed_files.len() as u64)
            .unwrap_or(0);
        let rollback_events = match txn {
            Some(txn)
                if matches!(
                    txn.phase,
                    faktor_session::ledger::IntegrationTxnPhase::RollingBack
                        | faktor_session::ledger::IntegrationTxnPhase::RolledBack
                ) =>
            {
                1
            }
            _ => 0,
        };
        let human_interventions = store
            .engineering_outcome_human_interventions(handle.id())
            .map_err(|e| {
                OutcomeLedgerError::Facts(format!("human interventions read failed: {e}"))
            })?;
        let recovery_events = store
            .engineering_outcome_recovery_events(handle.id())
            .map_err(|e| OutcomeLedgerError::Facts(format!("recovery events read failed: {e}")))?;
        let outcome = TerminalEngineeringOutcome {
            schema_version: OUTCOME_RECORD_SCHEMA_V,
            run_id: run_id.to_string(),
            session_id: handle.id().raw(),
            task_id: task_id.raw(),
            // The runtime's graph carries no per-task class dimension; the
            // honest Medium default is the SAME class the router feed uses.
            task_class: TaskClass::Medium,
            risk_class: facts.risk_class.unwrap_or(RiskBucket::Medium),
            candidate_snapshot,
            final_snapshot,
            accepted,
            // A VerifiedComplete task row can only be reached after its
            // changes landed on the owner root (or were written there
            // directly); failed/cancelled runs integrated nothing.
            integrated: accepted,
            // First VERIFICATION pass: the run's EARLIEST verification
            // attempt passed AND the run is a verified change. Never an
            // implementation-attempt claim (see `first_verification_passed`
            // above); a review-only no-op completion never claims it.
            verified_first_verification_pass: first_verification_passed && verified,
            repair_attempts,
            model_sequence: groups,
            // The EXACT boundary classes of the settled rows (independent of
            // the bounded display sequence above): local-first/local-final/
            // local-to-hosted classification reads these, never a possibly
            // folded boundary step.
            first_execution_class,
            final_execution_class,
            attribution_state,
            wall_time_ms,
            human_interventions,
            proof: ProofCoverage {
                checks_passed,
                checks_required,
            },
            // Files come from the certifying record's content-addressed
            // evidence; no durable line-count source exists (honest zeros).
            change_scale: ChangedCodeScale {
                files: changed_files,
                added_lines: 0,
                removed_lines: 0,
            },
            rollback_events,
            recovery_events,
            final_basis,
            // No durable frontier-tier authority exists; a hosted step is
            // never INFERRED as frontier (0 = unrecorded).
            frontier_escalations: 0,
            completed_at_ms,
            verified_samples: facts.samples.clone(),
        };
        outcome.validate()?;
        let projection = self.classify_economic_projection(
            task_id,
            reservations,
            model_spend,
            rework_spend_micro,
            settlement_steps,
        )?;
        let settlement = projection.into_event(run_id, 0, completed_at_ms);
        if settlement.money_state == MoneyState::Complete {
            validate_complete_projection(&outcome, &settlement)?;
        }
        Ok((outcome, settlement))
    }

    /// Derive only the financial projection (the late-finalization path):
    /// the reservation state is read FIRST, then the settled-spend rows (the
    /// same documented order as the record derivation).
    fn derive_economic_projection(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: faktor_core::id::TaskId,
    ) -> Result<EconomicProjection, OutcomeLedgerError> {
        let reservations = self.read_reservation_state(handle, task_id);
        self.note_reservation_read();
        // The money-only path uses the same bounded PROJECTION as the record
        // derivation: a truncated provider-call history affects attribution
        // only, never the money (which comes from the reservation rows), so
        // it must not wedge the settlement lifecycle.
        let mut spend = handle
            .durable_spend_projection_bounded(
                task_id,
                faktor_session::MAX_DURABLE_SPEND_ROWS,
                self.spend_call_bound(),
            )
            .map_err(|e| OutcomeLedgerError::Facts(format!("settled spend rows read failed: {e}")))?
            .rows;
        // Same chronological ordering as the record derivation: the settled
        // per-step projection tells the run's story oldest-first.
        spend.sort_by_key(|row| row.reservation_id);
        let records = handle.list_verification_records(task_id).map_err(|e| {
            OutcomeLedgerError::Facts(format!("verification records read failed: {e}"))
        })?;
        let (_, settlement_steps, model_spend, _, _) = group_spend_rows(&spend);
        let rework_spend_micro = derive_rework_spend(&spend, &records, model_spend);
        self.classify_economic_projection(
            task_id,
            reservations,
            model_spend,
            rework_spend_micro,
            settlement_steps,
        )
    }

    /// Classify the run's money state from a reservation state captured
    /// BEFORE the settled-spend read (see [`Self::read_reservation_state`])
    /// and, only when complete, carry the measured amounts. The documented
    /// order is reservations FIRST: settlements are append-only and
    /// reservations only move forward, so any later settled-spend read is a
    /// superset of the reservations' settled set — a `complete`
    /// classification can never miss a reservation that was open when the
    /// classification started.
    fn classify_economic_projection(
        &self,
        task_id: faktor_core::id::TaskId,
        reservations: ReservationState,
        model_spend: u128,
        rework_spend_micro: u64,
        settlement_steps: Vec<SettlementStep>,
    ) -> Result<EconomicProjection, OutcomeLedgerError> {
        let unavailable = |reason: String| EconomicProjection {
            state: MoneyState::Unavailable,
            reason,
            model_spend_micro: None,
            verification_spend_micro: None,
            rework_spend_micro: None,
            avoided_hosted: MeasuredMoney::Unavailable,
            steps: Vec::new(),
        };
        let (open, uncertain, unmetered, unknown_status) = match reservations {
            ReservationState::Unreadable(reason) | ReservationState::Oversized(reason) => {
                return Ok(unavailable(reason))
            }
            ReservationState::Observed {
                open,
                uncertain,
                unmetered,
                unknown_status,
            } => (open, uncertain, unmetered, unknown_status),
        };
        if open > 0 || uncertain > 0 {
            return Ok(EconomicProjection {
                state: MoneyState::Pending,
                reason: format!(
                    "{open} open and {uncertain} uncertain reservation(s) of task {} are still \
                     unsettled; the monetary projection is not final",
                    task_id.raw()
                ),
                model_spend_micro: None,
                verification_spend_micro: None,
                rework_spend_micro: None,
                avoided_hosted: MeasuredMoney::Unavailable,
                steps: Vec::new(),
            });
        }
        if unmetered > 0 {
            return Ok(unavailable(format!(
                "{unmetered} settled reservation(s) of task {} carry no measured amount; the \
                 monetary projection is unavailable (never a fabricated zero)",
                task_id.raw()
            )));
        }
        if unknown_status > 0 {
            return Ok(unavailable(format!(
                "{unknown_status} reservation(s) of task {} carry an unknown status; the \
                 monetary projection refuses to guess",
                task_id.raw()
            )));
        }
        let model_spend_micro = sat_u64(model_spend);
        let step_sum: u128 = settlement_steps
            .iter()
            .map(|s| u128::from(s.spend_micro))
            .sum();
        if step_sum != u128::from(model_spend_micro) {
            return Ok(unavailable(
                "the settled step projection disagrees with the settled model spend; the \
                 monetary projection refuses an inconsistent sum"
                    .into(),
            ));
        }
        Ok(EconomicProjection {
            state: MoneyState::Complete,
            reason: "every reservation of the run is settled and measured".into(),
            model_spend_micro: Some(model_spend_micro),
            // Measured scope zero: no separate paid verification process
            // exists; review model calls are inside the model spend.
            verification_spend_micro: Some(0),
            rework_spend_micro: Some(rework_spend_micro),
            // NO counterfactual authority exists in production: the avoided
            // hosted inference is explicitly UNAVAILABLE, never zero.
            avoided_hosted: MeasuredMoney::Unavailable,
            steps: settlement_steps,
        })
    }

    /// Read the reservation-ledger state of one task — the FIRST money read
    /// of every derivation, ALWAYS before the settled-spend read. The read
    /// outcome is data, not a failure: an unreadable/oversized ledger types
    /// the settlement `unavailable` while the engineering record still
    /// records.
    fn read_reservation_state(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: faktor_core::id::TaskId,
    ) -> ReservationState {
        let reservations = match self.store.cost_reservations_of(
            handle.id(),
            task_id,
            faktor_session::MAX_DURABLE_SPEND_ROWS.saturating_add(1),
        ) {
            Ok(rows) => rows,
            Err(e) => {
                return ReservationState::Unreadable(format!(
                    "reservation ledger unreadable: {e}; the monetary projection cannot be \
                     established"
                ))
            }
        };
        if reservations.len() as i64 > faktor_session::MAX_DURABLE_SPEND_ROWS {
            return ReservationState::Oversized(format!(
                "reservation ledger of task {} exceeds the {}-row read bound; the monetary \
                 projection refuses a partial sum",
                task_id.raw(),
                faktor_session::MAX_DURABLE_SPEND_ROWS
            ));
        }
        let mut open = 0u64;
        let mut uncertain = 0u64;
        let mut unmetered = 0u64;
        let mut unknown_status = 0u64;
        for row in &reservations {
            match row.status.as_str() {
                "reserved" | "dispatched" => open = open.saturating_add(1),
                "uncertain" => uncertain = uncertain.saturating_add(1),
                "settled" => {
                    if row.cost_basis.as_deref() == Some("Unknown")
                        || (row.cost_basis.is_none()
                            && row.settled_cost_micro.is_none()
                            && row.provider_reported_micro.is_none()
                            && row.provider_reported_cost_micro.is_none()
                            && row.provider_cost_micro.is_none())
                    {
                        unmetered = unmetered.saturating_add(1);
                    }
                }
                // Refunded rows never spent; nothing else is legal.
                "refunded" => {}
                _ => unknown_status = unknown_status.saturating_add(1),
            }
        }
        ReservationState::Observed {
            open,
            uncertain,
            unmetered,
            unknown_status,
        }
    }
}

/// The reservation-ledger state captured by the FIRST money read of one
/// derivation (see [`OutcomeLedger::read_reservation_state`]). Capturing it
/// as a value is what makes the documented read order structural: the
/// classification consumes the captured state, so it can never be derived
/// from a reservation read that happened after the spend read.
enum ReservationState {
    /// The ledger read failed; the reason is the ready-to-store diagnostic.
    Unreadable(String),
    /// The ledger exceeds the bounded read; never a partial sum.
    Oversized(String),
    /// The status counts observed by the first read.
    Observed {
        open: u64,
        uncertain: u64,
        unmetered: u64,
        unknown_status: u64,
    },
}

#[cfg(test)]
#[path = "outcome_ledger_tests.rs"]
mod tests;
