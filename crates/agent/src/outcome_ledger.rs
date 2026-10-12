//! The Engineering Outcome Ledger (economic audit item 7): the append-only
//! durable record of completed runs and the first-class metrics derived from
//! it.
//!
//! Every completed run records ONE [`RunOutcomeRecord`]: the durable task/run
//! identity, the candidate and final snapshot digests, acceptance and
//! integration, whether it verified on the FIRST implementation attempt, the
//! repair-attempt count, the provider/model sequence (local vs hosted), the
//! actual model spend, the verification spend, the rework spend, the
//! counterfactual avoided hosted inference, wall time, human interventions,
//! proof coverage, risk class, changed-code scale, rollback/recovery events,
//! the final verification basis, the frontier-escalation count and the
//! explicit per-call VERIFIED samples the outcome-learning router learns
//! from.
//!
//! Durability and exactly-once
//! ---------------------------
//! The record lands in the store crate's v32 `engineering_outcome` table via
//! [`OutcomeLedger::record`]. The durable append is exactly-once (a replay
//! of the identical record is a `Replayed` no-op; a conflicting re-append is
//! a typed refusal), and the record's verified samples are absorbed into the
//! router's durable `model_outcome_stats` registry IN THE SAME transaction —
//! so a crash can neither double-write the ledger nor double-feed the
//! router. The samples carry ONLY the explicit verified attribution
//! (deterministic gate sites); "the model said done" is not a sample, exactly
//! like the pre-existing per-call feed.
//!
//! Metrics
//! -------
//! [`OutcomeLedgerMetrics::compute`] is pure integer math over records
//! (saturating, u128 intermediates; never floating point) and exposes the
//! first-class durable metrics: Successful-Run Cost, Verified Cost per
//! Successful Change (the TOTAL eligible-cohort spend over successful
//! verified changes), Verified First-Pass Rate, Rework Cost Ratio, the
//! local split (Local-Only Verified, Local-Final Verified, Local-First
//! Success, Local-to-Frontier Fallback, Frontier-Only), Avoided Hosted
//! Inference, Mean Time to Verified Change, Human Intervention Rate,
//! Regression/Rollback Rate, Proof Coverage and Frontier Escalation Rate.
//! Rates are floor-divided ppm ("at least"); cost/time averages round UP
//! (never understate). Zero denominators yield zero, never a fabricated
//! number.
//!
//! Production producer
//! -------------------
//! [`OutcomeLedger::record_terminal_run`] is the production entry point:
//! the runtime's completion gate and the task executor's terminal run
//! settlement derive ONE record per completed run (success, failure or
//! cancellation) from durable, frozen facts and append it exactly once,
//! with the terminal turn's explicit verified samples fed atomically by
//! the same transaction.

use std::sync::Arc;

use faktor_core::model::{RiskBucket, RouterPhase, TaskClass};
use faktor_store::{EngineeringOutcomeRow, ModelOutcomeIncrement, ModelOutcomeSample, Store};
use serde::{Deserialize, Serialize};

/// Payload schema version of every ledger record this module writes.
pub const OUTCOME_RECORD_SCHEMA_V: u32 = 1;
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

/// One provider/model step of the run's execution sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunModelStep {
    pub provider: String,
    pub model: String,
    /// True for a locally-served (zero-cost) model.
    pub local: bool,
    /// The step's actual spend in microUSD (exact; the record's
    /// `model_spend_micro` must equal the sum over steps).
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

/// ONE completed run's durable engineering-outcome record. Field semantics
/// are documented at the module level; every projection column of the
/// durable row is derived EXACTLY from these fields (a payload/projection
/// drift reads as typed corruption, never as a metric).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOutcomeRecord {
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
    /// The run verified on its FIRST implementation attempt.
    pub verified_first_attempt: bool,
    /// Repair attempts the run needed before verifying (0 = first-pass).
    pub repair_attempts: u64,
    /// The provider/model sequence actually used, in order.
    pub model_sequence: Vec<RunModelStep>,
    /// Actual model spend in microUSD (== the sum over `model_sequence`).
    pub model_spend_micro: u64,
    /// Verification (check/review execution) spend in microUSD.
    pub verification_spend_micro: u64,
    /// Spend consumed by repair attempts in microUSD (<= model spend).
    pub rework_spend_micro: u64,
    /// Hosted inference the local-model substitution avoided, in microUSD
    /// (the recorded counterfactual; 0 = unmeasured, never fabricated).
    pub avoided_hosted_micro: u64,
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

impl RunOutcomeRecord {
    /// The run counts as a successfully verified change: accepted,
    /// integrated AND resting on deterministic verification.
    pub fn verified(&self) -> bool {
        self.accepted && self.integrated && self.final_basis.is_deterministic_verified()
    }

    /// Whether any step ran on a local model.
    pub fn local_used(&self) -> bool {
        self.model_sequence.iter().any(|s| s.local)
    }

    /// Bounded structural validation. Everything a metric or projection
    /// depends on is enforced here BEFORE any write; the durable append
    /// re-validates the row shape.
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
        let mut step_sum: u128 = 0;
        for step in &self.model_sequence {
            for (what, value) in [("provider", &step.provider), ("model", &step.model)] {
                if value.is_empty() || value.len() > MAX_MODEL_TEXT_BYTES {
                    return Err(OutcomeLedgerError::Invalid(format!(
                        "model step {what} must be 1..={MAX_MODEL_TEXT_BYTES} bytes, got {}",
                        value.len()
                    )));
                }
            }
            step_sum = step_sum.saturating_add(u128::from(step.spend_micro));
        }
        if step_sum != u128::from(self.model_spend_micro) {
            return Err(OutcomeLedgerError::Invalid(format!(
                "model spend {} does not equal the model-sequence spend sum {step_sum}",
                self.model_spend_micro
            )));
        }
        if self.rework_spend_micro > self.model_spend_micro {
            return Err(OutcomeLedgerError::Invalid(format!(
                "rework spend {} exceeds the model spend {}",
                self.rework_spend_micro, self.model_spend_micro
            )));
        }
        if self.proof.checks_passed > self.proof.checks_required {
            return Err(OutcomeLedgerError::Invalid(format!(
                "checks_passed {} exceeds checks_required {}",
                self.proof.checks_passed, self.proof.checks_required
            )));
        }
        if self.verified_first_attempt && !self.verified() {
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
            verified_first_pass: verified && self.verified_first_attempt,
            integrated: self.integrated,
            local_used: self.local_used(),
            frontier_used: self.frontier_escalations > 0,
            model_spend_micro: to_i64("model_spend_micro", self.model_spend_micro)?,
            verification_spend_micro: to_i64(
                "verification_spend_micro",
                self.verification_spend_micro,
            )?,
            rework_spend_micro: to_i64("rework_spend_micro", self.rework_spend_micro)?,
            avoided_hosted_micro: to_i64("avoided_hosted_micro", self.avoided_hosted_micro)?,
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
    pub fn router_increments(&self) -> Vec<ModelOutcomeIncrement> {
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
    /// The record was durably appended (router samples fed atomically).
    Recorded,
    /// The run id already held the byte-identical record: a crash/replay
    /// no-op — no second record, no second router sample.
    Replayed,
}

impl LedgerAppend {
    pub fn was_recorded(self) -> bool {
        matches!(self, LedgerAppend::Recorded)
    }
}

/// The total oldest-key cursor of one ledger page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeCursor {
    pub completed_at_ms: i64,
    pub run_id: String,
}

/// One bounded newest-first ledger page.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeLedgerPage {
    pub records: Vec<RunOutcomeRecord>,
    /// Present when the page was full: the cursor older pages continue from.
    pub next: Option<OutcomeCursor>,
}

/// The durable Engineering Outcome Ledger handle.
#[derive(Debug, Clone)]
pub struct OutcomeLedger {
    store: Arc<Store>,
}

impl OutcomeLedger {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Durably record ONE completed run, exactly once, and atomically feed
    /// its verified samples into the router's durable verified-outcome
    /// registry. A replay of an identical record is a no-op; a conflicting
    /// re-append under the same run id is a typed refusal from the store.
    pub fn record(&self, record: &RunOutcomeRecord) -> Result<LedgerAppend, OutcomeLedgerError> {
        record.validate()?;
        let row = record.projection_row()?;
        let increments = record.router_increments();
        match self.store.engineering_outcome_append(&row, &increments)? {
            faktor_store::EngineeringOutcomeAppend::Appended => Ok(LedgerAppend::Recorded),
            faktor_store::EngineeringOutcomeAppend::Duplicate => Ok(LedgerAppend::Replayed),
        }
    }

    /// The exact durable record of one run, or `None`. The stored payload is
    /// re-validated AND cross-checked against every projection column, so a
    /// payload/projection drift is a typed corruption, never a metric.
    pub fn get(&self, run_id: &str) -> Result<Option<RunOutcomeRecord>, OutcomeLedgerError> {
        self.store
            .engineering_outcome_get(run_id)?
            .map(|row| decode_record_row(&row))
            .transpose()
    }

    /// One bounded newest-first page, strictly older than `cursor`.
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
        let mut records = Vec::with_capacity(rows.len());
        for row in &rows {
            records.push(decode_record_row(row)?);
        }
        let next = if full {
            rows.last().map(|last| OutcomeCursor {
                completed_at_ms: last.completed_at_ms,
                run_id: last.run_id.clone(),
            })
        } else {
            None
        };
        Ok(OutcomeLedgerPage { records, next })
    }

    /// Exact durable record count.
    pub fn count(&self) -> Result<u64, OutcomeLedgerError> {
        Ok(self.store.engineering_outcome_count()?)
    }

    /// The bounded metrics fold over the durable ledger. See
    /// [`OutcomeLedgerMetrics::compute`] for the metric definitions; a
    /// ledger larger than [`MAX_METRICS_RECORDS`] reports `truncated`.
    pub fn metrics(&self) -> Result<OutcomeLedgerMetricsReport, OutcomeLedgerError> {
        let mut records: Vec<RunOutcomeRecord> = Vec::new();
        let mut cursor: Option<OutcomeCursor> = None;
        let mut truncated = false;
        loop {
            let page = self.page(cursor.as_ref(), MAX_LEDGER_PAGE)?;
            records.extend(page.records);
            if records.len() as u64 >= MAX_METRICS_RECORDS {
                truncated |= page.next.is_some();
                records.truncate(MAX_METRICS_RECORDS as usize);
                break;
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(OutcomeLedgerMetricsReport {
            scanned: records.len() as u64,
            metrics: OutcomeLedgerMetrics::compute(&records),
            truncated,
        })
    }
}

/// Decode and cross-check one durable row: payload schema/validation AND
/// exact agreement with every projection column.
fn decode_record_row(row: &EngineeringOutcomeRow) -> Result<RunOutcomeRecord, OutcomeLedgerError> {
    let record: RunOutcomeRecord =
        serde_json::from_str(&row.payload).map_err(|e| OutcomeLedgerError::Corrupt {
            run_id: row.run_id.clone(),
            detail: format!("payload does not decode as a run outcome record: {e}"),
        })?;
    record.validate().map_err(|e| OutcomeLedgerError::Corrupt {
        run_id: row.run_id.clone(),
        detail: e.to_string(),
    })?;
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
    if (record.verified() && record.verified_first_attempt) != row.verified_first_pass {
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
    if i64::try_from(record.model_spend_micro) != Ok(row.model_spend_micro)
        || i64::try_from(record.verification_spend_micro) != Ok(row.verification_spend_micro)
        || i64::try_from(record.rework_spend_micro) != Ok(row.rework_spend_micro)
        || i64::try_from(record.avoided_hosted_micro) != Ok(row.avoided_hosted_micro)
        || i64::try_from(record.wall_time_ms) != Ok(row.wall_time_ms)
        || i64::try_from(record.human_interventions) != Ok(row.human_interventions)
        || i64::try_from(record.rollback_events) != Ok(row.rollback_events)
        || i64::try_from(record.recovery_events) != Ok(row.recovery_events)
        || i64::try_from(record.repair_attempts) != Ok(row.repair_attempts)
    {
        return Err(mismatch("money/counter projection"));
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
    Ok(record)
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

/// The first-class durable engineering-outcome metrics (economic audit item
/// 7). All rates are ppm floor-divided ("at least"); money/time averages
/// round UP. Every denominator of zero yields zero, never a fabricated
/// number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct OutcomeLedgerMetrics {
    pub runs: u64,
    /// Runs that verified: accepted + integrated + deterministic basis.
    pub verified_changes: u64,
    pub verified_first_pass_changes: u64,
    /// Verified First-Pass Rate = first-pass verified / verified.
    pub verified_first_pass_ppm: u32,
    /// model + verification spend over the VERIFIED (successful) runs
    /// (microUSD).
    pub total_verified_spend_micro: u64,
    /// Successful-Run Cost: `total_verified_spend_micro` / verified changes,
    /// rounded UP (microUSD) — the average spend of a run that DID succeed.
    /// This is NOT the cost of a successful change: failed attempts are
    /// excluded from this numerator but not from the business.
    pub successful_run_cost_micro: u64,
    /// model + verification spend over ALL attempted runs (the eligible
    /// cohort spend, failures and cancellations included; microUSD).
    pub total_spend_micro: u64,
    /// Verified Cost per Successful Change (VCSC): the TOTAL eligible-cohort
    /// spend across attempted work (`total_spend_micro`) divided by the
    /// successful verified changes, rounded UP (microUSD). Audit example:
    /// 9 failed attempts at $10 plus 1 verified change at $1 => $91 per
    /// successful change (never $1).
    pub verified_cost_per_successful_change_micro: u64,
    /// Rework spend over all runs (microUSD).
    pub total_rework_spend_micro: u64,
    /// Rework Cost Ratio = rework spend / total spend (ppm).
    pub rework_cost_ratio_ppm: u32,
    /// Runs that used at least one local model.
    pub local_runs: u64,
    /// Runs whose model sequence is ENTIRELY local (at least one step, every
    /// step local): the denominator of the Local-Only Verified Rate.
    pub local_only_runs: u64,
    /// Verified changes among the local-only runs: the local model produced
    /// the whole verified run.
    pub local_only_verified_changes: u64,
    /// Local-Only Verified Rate = local_only_verified_changes /
    /// local_only_runs (ppm): how often a purely local run verifies.
    pub local_only_verified_ppm: u32,
    /// Runs whose FINAL model step is local: the denominator of the
    /// Local-Final Verified Rate.
    pub local_final_runs: u64,
    /// Verified changes among the local-final runs.
    pub local_final_verified_changes: u64,
    /// Local-Final Verified Rate = local_final_verified_changes /
    /// local_final_runs (ppm).
    pub local_final_verified_ppm: u32,
    /// Runs whose FIRST model step is local (the denominator of the
    /// local-first and local-to-frontier rates).
    pub local_first_runs: u64,
    /// Local-first runs that verified on their FIRST implementation attempt.
    pub local_first_success_changes: u64,
    /// Local-First Success Rate = local_first_success_changes /
    /// local_first_runs (ppm).
    pub local_first_success_ppm: u32,
    /// Runs that started local and ENDED on a hosted (non-local) provider:
    /// a local-then-hosted repair is a FALLBACK, never a local substitution.
    pub local_to_frontier_fallback_runs: u64,
    /// Local-to-Frontier Fallback Rate = local_to_frontier_fallback_runs /
    /// local_first_runs (ppm).
    pub local_to_frontier_fallback_ppm: u32,
    /// Runs with NO local step at all (the whole sequence ran on hosted
    /// providers).
    pub frontier_only_runs: u64,
    /// Frontier-Only Rate = frontier_only_runs / all runs (ppm).
    pub frontier_only_ppm: u32,
    /// Avoided Hosted Inference = the recorded counterfactual over all
    /// runs (microUSD; only explicitly recorded values, never fabricated).
    pub avoided_hosted_inference_micro: u64,
    /// Wall time over verified runs (ms).
    pub total_wall_time_ms: u64,
    /// Mean Time to Verified Change: the above / verified changes, rounded
    /// UP (ms).
    pub mean_time_to_verified_change_ms: u64,
    pub human_intervention_runs: u64,
    /// Human Intervention Rate = runs with >= 1 intervention / all runs (ppm).
    pub human_intervention_ppm: u32,
    pub rollback_runs: u64,
    /// Regression/Rollback Rate = runs with >= 1 rollback event / all runs
    /// (ppm).
    pub regression_rollback_ppm: u32,
    pub checks_passed: u64,
    pub checks_required: u64,
    /// Proof Coverage = passed checks / required checks over the runs that
    /// required any (ppm); zero required checks carry no evidence.
    pub proof_coverage_ppm: u32,
    pub frontier_escalation_runs: u64,
    /// Frontier Escalation Rate = runs with >= 1 frontier escalation / all
    /// runs (ppm).
    pub frontier_escalation_ppm: u32,
}

impl OutcomeLedgerMetrics {
    /// Pure, integer-exact fold over a record set (saturating u128
    /// intermediates; never floating point).
    pub fn compute(records: &[RunOutcomeRecord]) -> Self {
        let mut m = OutcomeLedgerMetrics::default();
        let mut verified_spend: u128 = 0;
        let mut verified_wall: u128 = 0;
        let mut total_spend: u128 = 0;
        let mut rework_spend: u128 = 0;
        let mut avoided: u128 = 0;
        let mut checks_passed: u128 = 0;
        let mut checks_required: u128 = 0;
        let mut local_runs: u128 = 0;
        let mut local_only_runs: u128 = 0;
        let mut local_only_verified: u128 = 0;
        let mut local_final_runs: u128 = 0;
        let mut local_final_verified: u128 = 0;
        let mut local_first_runs: u128 = 0;
        let mut local_first_success: u128 = 0;
        let mut local_to_frontier: u128 = 0;
        let mut frontier_only: u128 = 0;
        let mut intervention_runs: u128 = 0;
        let mut rollback_runs: u128 = 0;
        let mut frontier_runs: u128 = 0;
        let mut verified_first_pass: u128 = 0;
        for record in records {
            m.runs = m.runs.saturating_add(1);
            let spend = u128::from(record.model_spend_micro)
                .saturating_add(u128::from(record.verification_spend_micro));
            total_spend = total_spend.saturating_add(spend);
            rework_spend = rework_spend.saturating_add(u128::from(record.rework_spend_micro));
            avoided = avoided.saturating_add(u128::from(record.avoided_hosted_micro));
            let steps = &record.model_sequence;
            let first_local = steps.first().is_some_and(|s| s.local);
            let last_local = steps.last().is_some_and(|s| s.local);
            let all_local = !steps.is_empty() && steps.iter().all(|s| s.local);
            if record.local_used() {
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
                if steps.last().is_some_and(|s| !s.local) {
                    // Started local, ENDED hosted: a fallback, never a
                    // substitution — it must not count toward any local
                    // success rate.
                    local_to_frontier = local_to_frontier.saturating_add(1);
                } else if record.verified() && record.verified_first_attempt {
                    local_first_success = local_first_success.saturating_add(1);
                }
            }
            if !record.local_used() && !steps.is_empty() {
                frontier_only = frontier_only.saturating_add(1);
            }
            if record.human_interventions > 0 {
                intervention_runs = intervention_runs.saturating_add(1);
            }
            if record.rollback_events > 0 {
                rollback_runs = rollback_runs.saturating_add(1);
            }
            if record.frontier_escalations > 0 {
                frontier_runs = frontier_runs.saturating_add(1);
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
                verified_spend = verified_spend.saturating_add(spend);
                verified_wall = verified_wall.saturating_add(u128::from(record.wall_time_ms));
                if record.verified_first_attempt {
                    verified_first_pass = verified_first_pass.saturating_add(1);
                }
            }
        }
        let runs = u128::from(m.runs);
        m.verified_first_pass_changes = sat_u64(verified_first_pass);
        m.local_runs = sat_u64(local_runs);
        m.local_only_runs = sat_u64(local_only_runs);
        m.local_only_verified_changes = sat_u64(local_only_verified);
        m.local_final_runs = sat_u64(local_final_runs);
        m.local_final_verified_changes = sat_u64(local_final_verified);
        m.local_first_runs = sat_u64(local_first_runs);
        m.local_first_success_changes = sat_u64(local_first_success);
        m.local_to_frontier_fallback_runs = sat_u64(local_to_frontier);
        m.frontier_only_runs = sat_u64(frontier_only);
        m.human_intervention_runs = sat_u64(intervention_runs);
        m.rollback_runs = sat_u64(rollback_runs);
        m.frontier_escalation_runs = sat_u64(frontier_runs);
        m.total_verified_spend_micro = sat_u64(verified_spend);
        m.total_spend_micro = sat_u64(total_spend);
        m.total_rework_spend_micro = sat_u64(rework_spend);
        m.avoided_hosted_inference_micro = sat_u64(avoided);
        m.total_wall_time_ms = sat_u64(verified_wall);
        m.checks_passed = sat_u64(checks_passed);
        m.checks_required = sat_u64(checks_required);
        // Rates: floor-division ppm ("at least"). Zero denominators -> 0.
        m.verified_first_pass_ppm =
            meaning_ppm(verified_first_pass, u128::from(m.verified_changes));
        m.rework_cost_ratio_ppm = meaning_ppm(rework_spend, total_spend);
        m.local_only_verified_ppm = meaning_ppm(local_only_verified, local_only_runs);
        m.local_final_verified_ppm = meaning_ppm(local_final_verified, local_final_runs);
        m.local_first_success_ppm = meaning_ppm(local_first_success, local_first_runs);
        m.local_to_frontier_fallback_ppm = meaning_ppm(local_to_frontier, local_first_runs);
        m.frontier_only_ppm = meaning_ppm(frontier_only, runs);
        m.human_intervention_ppm = meaning_ppm(intervention_runs, runs);
        m.regression_rollback_ppm = meaning_ppm(rollback_runs, runs);
        m.proof_coverage_ppm = meaning_ppm(checks_passed, checks_required);
        m.frontier_escalation_ppm = meaning_ppm(frontier_runs, runs);
        // Money/time averages: ceil ("never understate").
        m.successful_run_cost_micro = ceil_div_u64(verified_spend, u128::from(m.verified_changes));
        m.verified_cost_per_successful_change_micro =
            ceil_div_u64(total_spend, u128::from(m.verified_changes));
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
/// runtime's terminal gate and the executor's settlement describe ONE run
/// (a task row is created once per session and can reach a terminal state
/// once, so the identity is total).
pub fn session_terminal_run_id(session: faktor_core::id::SessionId) -> String {
    format!("tx-session-{}", session.raw())
}

/// The non-derivable facts of one terminal run: the semantic risk bucket the
/// terminal turn's attributed samples key by, and the explicit verified
/// samples the terminal turn collected. Everything else is derived from
/// durable rows by [`OutcomeLedger::record_terminal_run`].
#[derive(Debug, Clone, Default)]
pub struct TerminalRunFacts {
    /// The risk bucket of the terminal turn's verified attribution. Only
    /// meaningful when `samples` is non-empty (the router feed keys by it);
    /// records without samples use the honest default.
    pub risk_class: Option<RiskBucket>,
    /// The terminal turn's explicit per-call verified samples. Every sample
    /// rides the record's atomic append (the store absorbs them into the
    /// router registry IN the record's transaction).
    pub samples: Vec<RunVerifiedSample>,
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
    /// - otherwise the record is derived purely from durable facts and
    ///   validated before the append: task identity/state/time, the
    ///   verification records (attempts, proof coverage, review evidence),
    ///   the settled spend rows (provider/model sequence, local zero-cost
    ///   steps, spend, rework after the first failed attempt), the durable
    ///   integration record (snapshots), the integration transaction
    ///   (rollback), the permission decisions (human interventions) and the
    ///   recovery events;
    /// - the append is the store's exactly-once immediate transaction: the
    ///   row and the router samples commit together or not at all, and a
    ///   byte-identical replay is a `Replayed` no-op.
    ///
    /// Facts with no durable authority are honest zeros, documented per
    /// field below (never fabricated numbers).
    pub fn record_terminal_run(
        &self,
        handle: &faktor_session::SessionHandle,
        run_id: &str,
        facts: TerminalRunFacts,
    ) -> Result<Option<LedgerAppend>, OutcomeLedgerError> {
        if self.get(run_id)?.is_some() {
            return Ok(None);
        }
        let record = self.derive_terminal_run_record(handle, run_id, facts)?;
        Ok(Some(self.record(&record)?))
    }

    /// Derive the terminal run record from durable rows only. Every read is
    /// fallible; a failed read refuses the record (never a partial or
    /// fabricated one). Deterministic over frozen terminal state, so a
    /// concurrent re-derivation of the same run produces a byte-identical
    /// row.
    fn derive_terminal_run_record(
        &self,
        handle: &faktor_session::SessionHandle,
        run_id: &str,
        facts: TerminalRunFacts,
    ) -> Result<RunOutcomeRecord, OutcomeLedgerError> {
        let task_id = handle
            .task_id()
            .map_err(|e| OutcomeLedgerError::Facts(format!("task id read failed: {e}")))?;
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
        let spend = handle
            .durable_spend_rows(task_id, faktor_session::MAX_DURABLE_SPEND_ROWS)
            .map_err(|e| {
                OutcomeLedgerError::Facts(format!("settled spend rows read failed: {e}"))
            })?;
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
        // Attempts: every Failed verification record is one repair attempt;
        // a verified run with zero failed records verified first-pass.
        let repair_attempts = records
            .iter()
            .filter(|r| r.status == crate::VerificationStatus::Failed)
            .count() as u64;
        // The certifying record: the newest Passed record for an accepted
        // run, else the newest attempt record (evidence of the failure).
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
        // The verified predicate of THIS derivation (the record does not
        // exist yet): accepted + integrated + deterministic basis.
        let verified = accepted && final_basis.is_deterministic_verified();
        // The provider/model sequence is the settled-spend projection
        // (settled reservations in occurrence order). A locally-served
        // (zero-cost) attributed settlement is one local step; the
        // unattributed provenance label is never local. Groups are
        // first-seen ordered and single-stepped; a pathological run with
        // more than MAX_RUN_MODEL_STEPS distinct groups folds the excess
        // into one explicit aggregate tail step (the spend sum is exact).
        let mut groups: Vec<RunModelStep> = Vec::new();
        let mut folded_groups: u64 = 0;
        let mut folded_spend: u128 = 0;
        let mut model_spend: u128 = 0;
        for row in &spend {
            model_spend = model_spend.saturating_add(u128::from(row.provider_cost_micro));
            let local = row.provider != faktor_session::UNATTRIBUTED_PROVENANCE
                && row.provider_cost_micro == 0;
            if let Some(last) = groups.last_mut() {
                if last.provider == row.provider && last.model == row.model && last.local == local {
                    last.spend_micro = last.spend_micro.saturating_add(row.provider_cost_micro);
                    continue;
                }
            }
            if groups.len() < MAX_RUN_MODEL_STEPS - 1 {
                groups.push(RunModelStep {
                    provider: row.provider.clone(),
                    model: row.model.clone(),
                    local,
                    spend_micro: row.provider_cost_micro,
                });
            } else {
                folded_groups = folded_groups.saturating_add(1);
                folded_spend = folded_spend.saturating_add(u128::from(row.provider_cost_micro));
            }
        }
        if folded_groups > 0 {
            groups.push(RunModelStep {
                provider: "aggregate".into(),
                model: format!("{folded_groups} folded steps"),
                local: false,
                spend_micro: sat_u64(folded_spend),
            });
        }
        let model_spend_micro = sat_u64(model_spend);
        // Rework: settled spend STRICTLY AFTER the first failed attempt's
        // completion; zero when no attempt failed (never fabricated).
        let rework_spend_micro = match records
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
                sat_u64(rework).min(model_spend_micro)
            }
            None => 0,
        };
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
        let record = RunOutcomeRecord {
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
            // First attempt: no repair, resting on the DETERMINISTIC basis.
            // A review-only no-op completion is a legitimate terminal
            // record, but it is not a verified change (`verified()` is
            // false), so it never claims a verified first pass.
            verified_first_attempt: repair_attempts == 0 && verified,
            repair_attempts,
            model_sequence: groups,
            model_spend_micro,
            // No durable verification-process spend ledger exists; review
            // model calls are part of the settled model spend above.
            verification_spend_micro: 0,
            rework_spend_micro,
            // No durable counterfactual authority: 0 = unmeasured.
            avoided_hosted_micro: 0,
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
            verified_samples: facts.samples,
        };
        record.validate()?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreOutcomeStore;
    use faktor_router::OutcomeView;

    fn step(provider: &str, model: &str, local: bool, spend_micro: u64) -> RunModelStep {
        RunModelStep {
            provider: provider.into(),
            model: model.into(),
            local,
            spend_micro,
        }
    }

    fn sample(
        provider: &str,
        model: &str,
        phase: RouterPhase,
        verified_success: bool,
        rework_cost_micro: u64,
        rework_turns: u64,
    ) -> RunVerifiedSample {
        RunVerifiedSample {
            provider: provider.into(),
            model: model.into(),
            phase,
            verified_success,
            rework_cost_micro,
            rework_turns,
        }
    }

    /// A fully-populated record where every required field is non-default.
    fn full_record() -> RunOutcomeRecord {
        RunOutcomeRecord {
            schema_version: OUTCOME_RECORD_SCHEMA_V,
            run_id: "run-2026-0001".into(),
            session_id: 41,
            task_id: 7,
            task_class: TaskClass::Hard,
            risk_class: RiskBucket::High,
            candidate_snapshot: Some("tm1:candidate-9f00".into()),
            final_snapshot: "tm1:final-1234".into(),
            accepted: true,
            integrated: true,
            verified_first_attempt: false,
            repair_attempts: 1,
            model_sequence: vec![
                step("ollama", "qwen3.8", true, 0),
                step("anthropic", "claude-opus", false, 3_000_000),
            ],
            model_spend_micro: 3_000_000,
            verification_spend_micro: 250_000,
            rework_spend_micro: 3_000_000,
            avoided_hosted_micro: 400_000,
            wall_time_ms: 12_345,
            human_interventions: 2,
            proof: ProofCoverage {
                checks_passed: 4,
                checks_required: 5,
            },
            change_scale: ChangedCodeScale {
                files: 3,
                added_lines: 120,
                removed_lines: 14,
            },
            rollback_events: 1,
            recovery_events: 1,
            final_basis: FinalVerificationBasis::DeterministicChecksAndReview,
            frontier_escalations: 1,
            completed_at_ms: 1_760_000_000_000,
            verified_samples: vec![
                sample(
                    "ollama",
                    "qwen3.8",
                    RouterPhase::Implement,
                    false,
                    3_000_000,
                    2,
                ),
                sample(
                    "anthropic",
                    "claude-opus",
                    RouterPhase::Implement,
                    true,
                    0,
                    0,
                ),
            ],
        }
    }

    fn fixture() -> Vec<RunOutcomeRecord> {
        let mut a = full_record();
        a.run_id = "r-a".into();
        a.task_class = TaskClass::Medium;
        a.risk_class = RiskBucket::Low;
        a.candidate_snapshot = None;
        a.verified_first_attempt = true;
        a.model_sequence = vec![step("hosted", "m-standard", false, 1_000_000)];
        a.model_spend_micro = 1_000_000;
        a.verification_spend_micro = 100_000;
        a.rework_spend_micro = 0;
        a.avoided_hosted_micro = 0;
        a.wall_time_ms = 1_000;
        a.human_interventions = 0;
        a.proof = ProofCoverage {
            checks_passed: 5,
            checks_required: 5,
        };
        a.rollback_events = 0;
        a.recovery_events = 0;
        a.final_basis = FinalVerificationBasis::DeterministicChecks;
        a.frontier_escalations = 0;
        a.verified_samples = vec![sample(
            "hosted",
            "m-standard",
            RouterPhase::Implement,
            true,
            0,
            0,
        )];

        let mut b = a.clone();
        b.run_id = "r-b".into();
        b.model_sequence = vec![step("ollama", "local-7b", true, 0)];
        b.model_spend_micro = 0;
        b.verification_spend_micro = 50_000;
        b.avoided_hosted_micro = 250_000;
        b.wall_time_ms = 2_000;
        b.proof = ProofCoverage {
            checks_passed: 4,
            checks_required: 4,
        };
        b.verified_samples = vec![sample(
            "ollama",
            "local-7b",
            RouterPhase::Implement,
            true,
            0,
            0,
        )];
        vec![a, b]
    }

    #[test]
    fn record_written_exactly_once_per_run_with_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ledger = OutcomeLedger::new(store);
        let record = full_record();
        assert_eq!(ledger.record(&record).unwrap(), LedgerAppend::Recorded);
        // Every field survives the durable round trip byte-exactly.
        let read = ledger.get(&record.run_id).unwrap().unwrap();
        assert_eq!(read, record);
        assert_eq!(
            read.candidate_snapshot.as_deref(),
            Some("tm1:candidate-9f00")
        );
        assert_eq!(read.final_snapshot, "tm1:final-1234");
        assert!(read.accepted && read.integrated);
        assert!(!read.verified_first_attempt && read.repair_attempts == 1);
        assert_eq!(read.model_sequence.len(), 2);
        assert_eq!(
            read.model_spend_micro + read.verification_spend_micro,
            3_250_000
        );
        assert_eq!(read.rework_spend_micro, 3_000_000);
        assert_eq!(read.proof.checks_passed, 4);
        assert_eq!(read.change_scale.files, 3);
        assert_eq!((read.rollback_events, read.recovery_events), (1, 1));
        assert_eq!(
            read.final_basis,
            FinalVerificationBasis::DeterministicChecksAndReview
        );
        assert_eq!(read.verified_samples.len(), 2);
        assert_eq!(ledger.count().unwrap(), 1);
        // The identical replay is a no-op: ONE record, never two.
        assert_eq!(ledger.record(&record).unwrap(), LedgerAppend::Replayed);
        assert_eq!(ledger.count().unwrap(), 1);
    }

    #[test]
    fn metrics_fixture_includes_local_zero_cost_substitution_and_rework() {
        let mut records = fixture();
        // C: local-then-hosted repair (rework), one human intervention.
        let mut c = records[0].clone();
        c.run_id = "r-c".into();
        c.verified_first_attempt = false;
        c.repair_attempts = 1;
        c.model_sequence = vec![
            step("ollama", "local-7b", true, 0),
            step("hosted", "m-strong", false, 3_000_000),
        ];
        c.model_spend_micro = 3_000_000;
        c.verification_spend_micro = 200_000;
        c.rework_spend_micro = 3_000_000;
        c.wall_time_ms = 5_000;
        c.human_interventions = 1;
        c.proof = ProofCoverage {
            checks_passed: 6,
            checks_required: 6,
        };
        c.frontier_escalations = 1;
        // D: rolled back, review-only basis (NOT a verified change).
        let mut d = records[0].clone();
        d.run_id = "r-d".into();
        d.integrated = false;
        d.verified_first_attempt = true;
        d.model_sequence = vec![step("hosted", "m-strong", false, 500_000)];
        d.model_spend_micro = 500_000;
        d.verification_spend_micro = 0;
        d.wall_time_ms = 3_000;
        d.proof = ProofCoverage::default();
        d.rollback_events = 2;
        d.recovery_events = 1;
        d.final_basis = FinalVerificationBasis::ReviewOnly;
        d.frontier_escalations = 1;
        // E: frontier escalation, deterministic checks.
        let mut e = records[0].clone();
        e.run_id = "r-e".into();
        e.model_sequence = vec![step("hosted", "m-frontier", false, 700_000)];
        e.model_spend_micro = 700_000;
        e.verification_spend_micro = 0;
        e.wall_time_ms = 4_000;
        e.proof = ProofCoverage {
            checks_passed: 2,
            checks_required: 2,
        };
        e.frontier_escalations = 1;
        records.push(c);
        records.push(d);
        records.push(e);

        let m = OutcomeLedgerMetrics::compute(&records);
        assert_eq!(m.runs, 5);
        assert_eq!(
            m.verified_changes, 4,
            "D is review-only and integrated=false"
        );
        assert_eq!(m.verified_first_pass_changes, 3);
        assert_eq!(m.verified_first_pass_ppm, 750_000);
        // Verified spend: A 1_100_000 + B 50_000 + C 3_200_000 + E 700_000.
        assert_eq!(m.total_verified_spend_micro, 5_050_000);
        // Successful-Run Cost = verified spend / verified changes.
        assert_eq!(m.successful_run_cost_micro, 1_262_500);
        assert_eq!(m.total_spend_micro, 5_550_000);
        // VCSC = TOTAL eligible cohort spend / verified changes (the failed
        // D's 500_000 is in the denominator's numerator, never dropped).
        assert_eq!(m.verified_cost_per_successful_change_micro, 1_387_500);
        assert_eq!(m.total_rework_spend_micro, 3_000_000);
        assert_eq!(m.rework_cost_ratio_ppm, 540_540);
        assert_eq!(m.local_runs, 2, "B and C used a local model");
        // B is local-only and verified; C is a local-then-hosted repair and
        // must NEVER count as a local substitution.
        assert_eq!(m.local_only_runs, 1);
        assert_eq!(m.local_only_verified_changes, 1);
        assert_eq!(m.local_only_verified_ppm, 1_000_000);
        assert_eq!(m.local_final_runs, 1);
        assert_eq!(m.local_final_verified_changes, 1);
        assert_eq!(m.local_final_verified_ppm, 1_000_000);
        assert_eq!(m.local_first_runs, 2);
        assert_eq!(m.local_first_success_changes, 1);
        assert_eq!(m.local_first_success_ppm, 500_000);
        assert_eq!(m.local_to_frontier_fallback_runs, 1);
        assert_eq!(m.local_to_frontier_fallback_ppm, 500_000);
        assert_eq!(m.frontier_only_runs, 3);
        assert_eq!(m.frontier_only_ppm, 600_000);
        assert_eq!(
            m.avoided_hosted_inference_micro, 250_000,
            "only B recorded a counterfactual; zero stays zero"
        );
        assert_eq!(m.total_wall_time_ms, 12_000);
        assert_eq!(m.mean_time_to_verified_change_ms, 3_000);
        assert_eq!(m.human_intervention_runs, 1);
        assert_eq!(m.human_intervention_ppm, 200_000);
        assert_eq!(m.rollback_runs, 1);
        assert_eq!(m.regression_rollback_ppm, 200_000);
        assert_eq!((m.checks_passed, m.checks_required), (17, 17));
        assert_eq!(m.proof_coverage_ppm, 1_000_000);
        assert_eq!(m.frontier_escalation_runs, 3);
        assert_eq!(m.frontier_escalation_ppm, 600_000);
    }

    #[test]
    fn terminal_run_producer_crashes_cleanly_and_replays_without_duplicates() {
        use faktor_core::state::{TaskState, TaskTransition};
        use faktor_session::{SessionManager, Task as SessionTask, TaskBudget};

        let dir = tempfile::tempdir().unwrap();
        let store_root = dir.path().join("store");
        let cas_root = dir.path().join("cas");
        let run_id;
        let session_id;
        {
            let manager = SessionManager::open_quick(store_root.clone(), cas_root.clone()).unwrap();
            let workspace = manager.create_workspace("/tmp/pw-outcome").unwrap();
            let handle = manager
                .create_session(workspace, "ledger fault", "p", "m")
                .unwrap();
            session_id = handle.id();
            let task_id = handle.task_id().unwrap();
            let now = handle.now_ms();
            handle
                .create_task(SessionTask {
                    task_id,
                    session_id,
                    goal: "fault run".into(),
                    acceptance_criteria: Vec::new(),
                    plan: Vec::new(),
                    attachments: Vec::new(),
                    contract: Default::default(),
                    budget: TaskBudget::default(),
                    state: TaskState::Pending,
                    created_ms: now,
                    updated_ms: now,
                })
                .unwrap();
            // A cancelled terminal run: the record must still exist.
            for transition in [TaskTransition::StartRunning, TaskTransition::Cancel] {
                let revision = handle.task_revision(task_id).unwrap();
                handle
                    .transition_task(task_id, revision, transition, None)
                    .unwrap();
            }
            run_id = session_terminal_run_id(session_id);
            let ledger = OutcomeLedger::new(manager.store());
            // The armed precommit boundary: the append (and its router feed)
            // rolls back together; nothing is recorded.
            manager.store().crash_arm(faktor_store::CrashArm {
                point: "engineering_outcome_precommit",
                ordinal: 0,
            });
            let facts = TerminalRunFacts {
                risk_class: Some(RiskBucket::Low),
                samples: vec![sample("p", "m", RouterPhase::Implement, false, 0, 0)],
            };
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ledger.record_terminal_run(&handle, &run_id, facts.clone())
            }));
            assert!(
                manager.store().seam_crash_observed(&caught),
                "the armed precommit seam must fire"
            );
        }
        {
            // Reopen over the same durable root: the crash left no record
            // and no sample; the SAME settlement retry records exactly one.
            let manager = SessionManager::open_quick(store_root, cas_root).unwrap();
            let handle = manager.get_session(session_id).unwrap().unwrap();
            let ledger = OutcomeLedger::new(manager.store());
            assert_eq!(
                ledger.count().unwrap(),
                0,
                "the crash rolled back the record"
            );
            let facts = TerminalRunFacts {
                risk_class: Some(RiskBucket::Low),
                samples: vec![sample("p", "m", RouterPhase::Implement, false, 0, 0)],
            };
            assert_eq!(
                ledger.record_terminal_run(&handle, &run_id, facts).unwrap(),
                Some(LedgerAppend::Recorded)
            );
            let record = ledger.get(&run_id).unwrap().unwrap();
            assert!(!record.accepted && !record.integrated);
            assert_eq!(record.final_basis, FinalVerificationBasis::Unverified);
            assert_eq!(record.risk_class, RiskBucket::Low);
            assert_eq!(record.verified_samples.len(), 1);
            assert_eq!(ledger.count().unwrap(), 1);
            let key = faktor_router::OutcomeKey {
                provider: "p".into(),
                model: "m".into(),
                phase: RouterPhase::Implement,
                task_class: TaskClass::Medium,
                risk_bucket: RiskBucket::Low,
            };
            let view = StoreOutcomeStore::new(manager.store());
            assert_eq!(
                view.stats(&key)
                    .expect("the record fed the router")
                    .sample_count,
                1
            );
            // The settlement replay: a present record short-circuits — no
            // second row, no second sample.
            assert_eq!(
                ledger
                    .record_terminal_run(&handle, &run_id, TerminalRunFacts::default())
                    .unwrap(),
                None
            );
            assert_eq!(ledger.count().unwrap(), 1);
            assert_eq!(view.stats(&key).unwrap().sample_count, 1);
        }
    }

    #[test]
    fn accepted_run_without_a_deterministic_basis_never_claims_a_verified_first_pass() {
        // Regression: the reviewer-proved no-op path (RequiresCriterionProof)
        // completes with zero checks and a root record that persists no
        // reviewer JSON — a non-deterministic basis. Such a run is a
        // legitimate terminal record, but it must NOT claim
        // `verified_first_attempt` (the validation invariant: that flag
        // implies a deterministic verified basis).
        use faktor_core::state::{TaskState, TaskTransition};
        use faktor_session::{SessionManager, Task as SessionTask, TaskBudget};

        let dir = tempfile::tempdir().unwrap();
        let manager =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let workspace = manager.create_workspace("/tmp/pw-review-only").unwrap();
        let handle = manager
            .create_session(workspace, "review-only", "p", "m")
            .unwrap();
        let session_id = handle.id();
        let task_id = handle.task_id().unwrap();
        let now = handle.now_ms();
        handle
            .create_task(SessionTask {
                task_id,
                session_id,
                goal: "reviewer-proved no-op".into(),
                acceptance_criteria: Vec::new(),
                plan: Vec::new(),
                attachments: Vec::new(),
                contract: Default::default(),
                budget: TaskBudget::default(),
                state: TaskState::Pending,
                created_ms: now,
                updated_ms: now,
            })
            .unwrap();
        for transition in [
            TaskTransition::StartRunning,
            TaskTransition::RequestVerification,
            TaskTransition::StartVerification,
        ] {
            let revision = handle.task_revision(task_id).unwrap();
            handle
                .transition_task(task_id, revision, transition, None)
                .unwrap();
        }
        // The no-op completion record: zero checks, zero criteria and NO
        // persisted reviewer (the historical shape of every orchestrated
        // root record) — a review-only proof by construction.
        let record = handle
            .create_verification_record_with_evidence(
                task_id,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
                crate::VerificationStatus::Passed,
                now,
                None,
                None,
            )
            .unwrap();
        let revision = handle.task_revision(task_id).unwrap();
        handle
            .complete_verified_task(task_id, revision, record)
            .unwrap();
        assert_eq!(
            handle.get_task(task_id).unwrap().unwrap().state,
            TaskState::VerifiedComplete
        );

        let run_id = session_terminal_run_id(session_id);
        let ledger = OutcomeLedger::new(manager.store());
        let append = ledger
            .record_terminal_run(&handle, &run_id, TerminalRunFacts::default())
            .unwrap();
        assert_eq!(append, Some(LedgerAppend::Recorded));
        let record = ledger.get(&run_id).unwrap().unwrap();
        assert!(record.accepted && record.integrated);
        assert_eq!(record.repair_attempts, 0, "no attempt ever failed");
        assert_eq!(record.final_basis, FinalVerificationBasis::Unverified);
        assert!(
            !record.verified_first_attempt,
            "a non-deterministic basis never claims a verified first pass"
        );
        assert!(!record.verified());
    }

    #[test]
    fn vcsc_counts_the_whole_attempt_cohort_not_only_the_successes() {
        // Audit example: 9 failed attempts at $10 plus 1 verified change at
        // $1 => $91 per successful verified change, NOT $1.
        let mut records = Vec::new();
        for index in 0..9u64 {
            let mut failed = full_record();
            failed.run_id = format!("fail-{index}");
            failed.accepted = false;
            failed.integrated = false;
            failed.verified_first_attempt = false;
            failed.repair_attempts = 1;
            failed.model_sequence = vec![step("hosted", "m", false, 10_000_000)];
            failed.model_spend_micro = 10_000_000;
            failed.verification_spend_micro = 0;
            failed.rework_spend_micro = 10_000_000;
            failed.final_basis = FinalVerificationBasis::Unverified;
            failed.verified_samples.clear();
            records.push(failed);
        }
        let mut success = full_record();
        success.run_id = "success".into();
        success.model_sequence = vec![step("hosted", "m", false, 1_000_000)];
        success.model_spend_micro = 1_000_000;
        success.verification_spend_micro = 0;
        success.rework_spend_micro = 0;
        success.verified_samples.clear();
        records.push(success);
        let m = OutcomeLedgerMetrics::compute(&records);
        assert_eq!(m.runs, 10);
        assert_eq!(m.verified_changes, 1);
        // The average spend of a run that DID succeed...
        assert_eq!(m.successful_run_cost_micro, 1_000_000);
        assert_eq!(m.total_verified_spend_micro, 1_000_000);
        // ...versus the cost of a successful CHANGE over the whole cohort.
        assert_eq!(m.total_spend_micro, 91_000_000);
        assert_eq!(m.verified_cost_per_successful_change_micro, 91_000_000);
        assert_ne!(
            m.verified_cost_per_successful_change_micro, m.successful_run_cost_micro,
            "the audit's $1-vs-$91 distinction must be visible"
        );
    }

    #[test]
    fn money_paths_remain_exact_above_f64_precision_and_round_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ledger = OutcomeLedger::new(store.clone());
        // 2^53 + 1: any f64/f32 path would silently corrupt this.
        let big = 9_007_199_254_740_993u64;
        let mut record = full_record();
        record.run_id = "run-big".into();
        record.model_sequence = vec![step("hosted", "m", false, big)];
        record.model_spend_micro = big;
        record.verification_spend_micro = 1;
        record.rework_spend_micro = big;
        record.candidate_snapshot = None;
        record.verified_samples = vec![sample("hosted", "m", RouterPhase::Implement, true, 0, 0)];
        assert_eq!(ledger.record(&record).unwrap(), LedgerAppend::Recorded);
        let read = ledger.get("run-big").unwrap().unwrap();
        assert_eq!(read.model_spend_micro, big);
        let m = OutcomeLedgerMetrics::compute(std::slice::from_ref(&read));
        assert_eq!(m.total_verified_spend_micro, big + 1);
        // ceil div of (2^53+1+1)/1 stays exact; no float ever participates.
        assert_eq!(m.successful_run_cost_micro, big + 1);
        assert_eq!(m.rework_cost_ratio_ppm, 999_999);
        // Two verified records with an odd micro sum: the per-change average
        // rounds UP (never understates spend).
        let mut second = read.clone();
        second.run_id = "run-big-2".into();
        assert_eq!(ledger.record(&second).unwrap(), LedgerAppend::Recorded);
        let report = ledger.metrics().unwrap();
        assert_eq!(report.metrics.runs, 2);
        assert_eq!(report.metrics.verified_changes, 2);
        assert_eq!(report.metrics.total_verified_spend_micro, (big + 1) * 2);
        assert_eq!(report.metrics.successful_run_cost_micro, big + 1);
    }

    #[test]
    fn router_consumes_the_ledger_and_replay_never_double_counts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ledger = OutcomeLedger::new(store.clone());
        let record = full_record(); // run-2026-0001: one failure + one success sample
        assert_eq!(ledger.record(&record).unwrap(), LedgerAppend::Recorded);
        // The router's durable registry (the SAME view a production
        // RouterService consults) learned exactly the record's samples.
        let view = StoreOutcomeStore::new(store.clone());
        let key = faktor_router::OutcomeKey {
            provider: "ollama".into(),
            model: "qwen3.8".into(),
            phase: RouterPhase::Implement,
            task_class: TaskClass::Hard,
            risk_bucket: RiskBucket::High,
        };
        let failure = view.stats(&key).expect("failure sample recorded");
        assert_eq!((failure.sample_count, failure.failures_first_pass), (1, 1));
        assert_eq!(failure.rework_cost_micro_sum, 3_000_000);
        assert_eq!(failure.rework_turns_sum, 2);
        let success_key = faktor_router::OutcomeKey {
            provider: "anthropic".into(),
            model: "claude-opus".into(),
            ..key.clone()
        };
        let success = view.stats(&success_key).expect("success sample recorded");
        assert_eq!((success.sample_count, success.successes_first_pass), (1, 1));
        // Replaying the identical record must NOT feed the registry twice.
        assert_eq!(ledger.record(&record).unwrap(), LedgerAppend::Replayed);
        let again = view.stats(&key).unwrap();
        assert_eq!(again.sample_count, 1);
        assert_eq!(again.rework_cost_micro_sum, 3_000_000);
        // A conflicting re-append under the same run id is refused AND
        // leaves the registry untouched.
        let mut hostile = record.clone();
        hostile.model_spend_micro = 1;
        hostile.rework_spend_micro = 0;
        hostile.model_sequence = vec![step("ollama", "qwen3.8", true, 1)];
        let err = ledger.record(&hostile).unwrap_err();
        assert!(matches!(err, OutcomeLedgerError::Store(_)), "got {err:?}");
        assert_eq!(view.stats(&key).unwrap().sample_count, 1);
    }

    #[test]
    fn invalid_records_are_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ledger = OutcomeLedger::new(store);
        let assert_invalid = |record: &RunOutcomeRecord| {
            assert!(
                matches!(ledger.record(record), Err(OutcomeLedgerError::Invalid(_))),
                "record must be refused before any write: {record:?}"
            );
        };
        let mut r = full_record();
        r.schema_version = 2;
        assert_invalid(&r);
        let mut r = full_record();
        r.run_id = "run-ünicode".into();
        assert_invalid(&r);
        let mut r = full_record();
        r.model_spend_micro += 1; // step-sum mismatch
        assert_invalid(&r);
        let mut r = full_record();
        r.rework_spend_micro = r.model_spend_micro + 1;
        assert_invalid(&r);
        let mut r = full_record();
        r.proof.checks_passed = 6;
        assert_invalid(&r);
        let mut r = full_record();
        r.verified_first_attempt = true; // needs a deterministic verified run
        r.final_basis = FinalVerificationBasis::Unverified;
        assert_invalid(&r);
        let mut r = full_record();
        r.verified_samples = vec![sample("p", "m", RouterPhase::Implement, true, 5, 0)];
        assert_invalid(&r);
        let mut r = full_record();
        r.model_sequence = (0..=MAX_RUN_MODEL_STEPS)
            .map(|i| step("p", &format!("m{i}"), false, 1))
            .collect();
        r.model_spend_micro = (MAX_RUN_MODEL_STEPS + 1) as u64;
        assert_invalid(&r);
        let mut r = full_record();
        r.final_snapshot = "x".repeat(MAX_SNAPSHOT_BYTES + 1);
        assert_invalid(&r);
        let mut r = full_record();
        r.model_spend_micro = u64::MAX;
        r.rework_spend_micro = 0;
        r.model_sequence = vec![step("p", "m", false, u64::MAX)];
        assert_invalid(&r); // above the durable signed 64-bit domain
        assert_eq!(ledger.count().unwrap(), 0, "no invalid record ever landed");
    }

    #[test]
    fn page_cursor_is_total_and_metrics_fold_reports_honest_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ledger = OutcomeLedger::new(store);
        for i in 0..5i64 {
            let mut r = full_record();
            r.run_id = format!("run-{i}");
            r.completed_at_ms = 1_000 + i;
            assert_eq!(ledger.record(&r).unwrap(), LedgerAppend::Recorded);
        }
        let first = ledger.page(None, 2).unwrap();
        assert_eq!(first.records.len(), 2);
        assert_eq!(first.records[0].run_id, "run-4");
        let next = first.next.expect("full page carries its cursor");
        let second = ledger.page(Some(&next), 2).unwrap();
        assert_eq!(
            second
                .records
                .iter()
                .map(|r| r.run_id.as_str())
                .collect::<Vec<_>>(),
            vec!["run-2", "run-1"]
        );
        let third = ledger.page(second.next.as_ref(), 2).unwrap();
        assert_eq!(third.records.len(), 1);
        assert_eq!(third.records[0].run_id, "run-0");
        assert!(third.next.is_none(), "a short page ends the scan");
        let report = ledger.metrics().unwrap();
        assert!(!report.truncated);
        assert_eq!((report.scanned, report.metrics.runs), (5, 5));
    }

    #[test]
    fn zero_and_hostile_metric_inputs_never_panic_or_fabricate() {
        // No records: every metric is an honest zero.
        assert_eq!(
            OutcomeLedgerMetrics::compute(&[]),
            OutcomeLedgerMetrics::default()
        );
        // Hostile (directly constructed, unvalidated) counters saturate and
        // clamp instead of panicking or claiming > 100%.
        let mut hostile = full_record();
        hostile.proof = ProofCoverage {
            checks_passed: u64::MAX,
            checks_required: 1,
        };
        hostile.model_spend_micro = u64::MAX;
        hostile.verification_spend_micro = u64::MAX;
        hostile.model_sequence = vec![step("p", "m", false, u64::MAX)];
        hostile.verified_samples.clear();
        let m = OutcomeLedgerMetrics::compute(std::slice::from_ref(&hostile));
        assert!(m.proof_coverage_ppm <= 1_000_000);
        assert!(m.rework_cost_ratio_ppm <= 1_000_000);
        assert!(m.successful_run_cost_micro > 0);
    }
}
