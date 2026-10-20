//! Engineering Outcome Ledger persistence (economic audit item 7) and the
//! economic settlement lifecycle (P1 economic-truth fix).
//!
//! Two durable records, one boundary each:
//!
//! 1. `engineering_outcome` (schema migration v32, rebuilt by v33): the
//!    append-only immutable ENGINEERING record of COMPLETED runs — one row
//!    per run identity, exactly-once by construction and atomic with the
//!    router feed. Since v33 the row carries NO monetary columns: money is
//!    financial finality, which reservations can still move at terminal
//!    time, and an immutable row must never freeze an inexact monetary
//!    fact. Legacy v1 payloads keep their historical money inside the
//!    payload (the owning module reads it compatibly).
//! 2. `economic_settlement` (v33): the append-only settlement lifecycle —
//!    `(run_id, seq)` events carrying the typed `money_state`
//!    (`complete` / `pending` / `unavailable`) and the settled financial
//!    projection. `complete` is the only state that carries amounts; every
//!    other state stores NULL (never a fabricated zero). Legal transitions
//!    are enforced here (pending -> complete|unavailable,
//!    unavailable -> complete|pending, complete -> complete corrections
//!    only) and a byte-identical replay of the latest event is a Duplicate
//!    no-op at any clock. A non-final state never rewrites itself: the
//!    lifecycle refuses pending -> pending and unavailable -> unavailable
//!    with a different fact (a diagnostic drift is not a new financial
//!    fact), so the owning module treats a re-derived equal state as a
//!    no-op instead of wedging.
//!
//! `terminal_run_facts` (v33) is the durable terminal-turn outbox: the
//! agent path persists the terminal turn's risk class + explicit verified
//! samples BEFORE the task row can become terminal, so every producer of
//! the engineering row (the agent completion gate or the task executor's
//! settlement) derives ONE byte-identical record from durable facts. An
//! upsert is refused once the engineering row exists (facts freeze at
//! record time).
//!
//! Everything that can be validated or decoded before the writer job is
//! done on the caller's thread (audit item 8: writer jobs execute SQL
//! only); the bounded payload schemas themselves are owned and validated
//! loudly by `faktor-agent`'s `outcome_ledger` module.

use super::*;

/// Hard bound on one stored record payload (strict JSON bytes). The owning
/// module's payload is far below this; an oversized value is refused before
/// it reaches the database.
pub const MAX_ENGINEERING_OUTCOME_PAYLOAD: usize = 32 * 1024;
/// Hard bound on one run identity (ASCII bytes).
pub const MAX_ENGINEERING_OUTCOME_RUN_ID: usize = 200;
/// Hard bound on one stored risk-class tag.
pub const MAX_ENGINEERING_OUTCOME_RISK_CLASS: usize = 64;
/// Bounded page size of one ledger scan step (paging is fundamental).
pub const MAX_ENGINEERING_OUTCOME_PAGE: u64 = 500;
/// Hard bound on one terminal-run-facts payload (the durable outbox row).
pub const MAX_TERMINAL_RUN_FACTS_PAYLOAD: usize = 16 * 1024;
/// Hard bound on one settlement reason string.
pub const MAX_ECONOMIC_SETTLEMENT_REASON: usize = 512;
/// Hard bound on the settlement step projection JSON.
pub const MAX_ECONOMIC_SETTLEMENT_STEPS_JSON: usize = 16 * 1024;
/// Hard bound on the number of settled step projections of one event.
pub const MAX_ECONOMIC_SETTLEMENT_STEPS: usize = 64;
/// Hard bound on one provider/model label inside the step projection.
pub const MAX_ECONOMIC_SETTLEMENT_MODEL_TEXT: usize = 128;
/// Highest settlement payload schema this store accepts (a hostile/foreign
/// writer cannot smuggle an unbounded schema cursor into a row).
pub const MAX_ECONOMIC_SETTLEMENT_SCHEMA_VERSION: i64 = 16;
/// The states of the settlement lifecycle.
pub const ECONOMIC_SETTLEMENT_STATES: [&str; 3] = ["complete", "pending", "unavailable"];
/// Bound of one batched latest-settlement read (SQLite parameter bound).
pub const MAX_ECONOMIC_SETTLEMENT_BATCH: usize = 400;

/// The exact column list of one `engineering_outcome` row (shared by every
/// read path so the decoder never drifts from the schema). Since v33 the
/// row carries NO money columns: money lives in `economic_settlement`.
const OUTCOME_ROW_COLUMNS: &str = "run_id, session_id, task_id, payload, payload_bytes, \
     completed_at_ms, verified, verified_first_pass, integrated, local_used, frontier_used, \
     wall_time_ms, human_interventions, rollback_events, recovery_events, repair_attempts, \
     risk_class, changed_files, changed_lines, checks_passed, checks_required";

/// The exact column list of one `economic_settlement` row.
const SETTLEMENT_ROW_COLUMNS: &str = "run_id, seq, schema_version, state, reason, \
     model_spend_micro, verification_spend_micro, rework_spend_micro, avoided_hosted_micro, \
     avoided_hosted_measured, steps_json, settled_at_ms";

/// One Engineering Outcome Ledger row exactly as stored: the run identity,
/// scope, the bounded immutable ENGINEERING payload and the metric
/// projection columns the aggregates fold without re-parsing the payload.
/// Booleans are explicit; every counter column is an exact non-negative
/// integer (SQLite `INTEGER` is signed 64-bit, so values above `i64::MAX`
/// are refused typed before any write). No monetary column exists: money is
/// the settlement lifecycle's dimension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineeringOutcomeRow {
    pub run_id: String,
    pub session_id: i64,
    pub task_id: i64,
    /// The bounded strict engineering payload (owned by the agent module).
    pub payload: String,
    /// The payload's byte length, persisted explicitly and validated on
    /// every read (SQLite `length()` counts characters, not bytes).
    pub payload_bytes: i64,
    pub completed_at_ms: i64,
    /// The run reached deterministic verification (the record's
    /// `accepted && integrated && final basis verified` fold).
    pub verified: bool,
    /// The run verified on its first implementation attempt.
    pub verified_first_pass: bool,
    pub integrated: bool,
    pub local_used: bool,
    pub frontier_used: bool,
    pub wall_time_ms: i64,
    pub human_interventions: i64,
    pub rollback_events: i64,
    pub recovery_events: i64,
    pub repair_attempts: i64,
    pub risk_class: String,
    pub changed_files: i64,
    pub changed_lines: i64,
    pub checks_passed: i64,
    pub checks_required: i64,
}

impl EngineeringOutcomeRow {
    /// Bounded structural validation, on the caller's thread, BEFORE the
    /// writer job is enqueued. The record payload's exact schema is the
    /// owning module's concern; the store enforces the row's own honest
    /// shape: a non-empty bounded ASCII run id, a bounded UTF-8 risk tag, a
    /// non-empty bounded payload that parses as a JSON object, non-negative
    /// counters that fit `i64`, and the two cross-column invariants
    /// (`verified_first_pass => verified`, `checks_passed <= checks_required`).
    fn validate(&self) -> StoreResult<()> {
        if self.run_id.is_empty() || self.run_id.len() > MAX_ENGINEERING_OUTCOME_RUN_ID {
            return Err(StoreError::Malformed(format!(
                "engineering outcome run id must be 1..={MAX_ENGINEERING_OUTCOME_RUN_ID} bytes, \
                 got {}",
                self.run_id.len()
            )));
        }
        if !self.run_id.is_ascii() {
            return Err(StoreError::Malformed(
                "engineering outcome run id must be ASCII".into(),
            ));
        }
        if self.risk_class.is_empty() || self.risk_class.len() > MAX_ENGINEERING_OUTCOME_RISK_CLASS
        {
            return Err(StoreError::Malformed(format!(
                "engineering outcome risk class must be 1..={MAX_ENGINEERING_OUTCOME_RISK_CLASS} \
                 bytes, got {}",
                self.risk_class.len()
            )));
        }
        if self.payload.is_empty() {
            return Err(StoreError::Malformed(
                "engineering outcome payload must not be empty".into(),
            ));
        }
        if self.payload.len() > MAX_ENGINEERING_OUTCOME_PAYLOAD {
            return Err(StoreError::Oversized(format!(
                "engineering outcome payload is {} bytes (cap {MAX_ENGINEERING_OUTCOME_PAYLOAD})",
                self.payload.len()
            )));
        }
        if self.payload_bytes != self.payload.len() as i64 {
            return Err(StoreError::Malformed(format!(
                "engineering outcome payload_bytes {} disagrees with the payload length {}",
                self.payload_bytes,
                self.payload.len()
            )));
        }
        match serde_json::from_str::<serde_json::Value>(&self.payload) {
            Ok(serde_json::Value::Object(_)) => {}
            Ok(_) => {
                return Err(StoreError::Malformed(
                    "engineering outcome payload must be a JSON object".into(),
                ));
            }
            Err(e) => {
                return Err(StoreError::Malformed(format!(
                    "engineering outcome payload is not valid JSON: {e}"
                )));
            }
        }
        if self.session_id < 0 || self.task_id < 0 {
            return Err(StoreError::Malformed(
                "engineering outcome scope ids must be non-negative".into(),
            ));
        }
        for (what, v) in [
            ("wall_time_ms", self.wall_time_ms),
            ("human_interventions", self.human_interventions),
            ("rollback_events", self.rollback_events),
            ("recovery_events", self.recovery_events),
            ("repair_attempts", self.repair_attempts),
            ("changed_files", self.changed_files),
            ("changed_lines", self.changed_lines),
            ("checks_passed", self.checks_passed),
            ("checks_required", self.checks_required),
        ] {
            if v < 0 {
                return Err(StoreError::Malformed(format!(
                    "engineering outcome {what} must be non-negative, got {v}"
                )));
            }
        }
        if self.verified_first_pass && !self.verified {
            return Err(StoreError::Malformed(
                "engineering outcome: a first-pass verified run must be verified".into(),
            ));
        }
        if self.checks_passed > self.checks_required {
            return Err(StoreError::Malformed(format!(
                "engineering outcome: checks_passed {} exceeds checks_required {}",
                self.checks_passed, self.checks_required
            )));
        }
        Ok(())
    }
}

/// The outcome of one [`Store::engineering_outcome_append`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineeringOutcomeAppend {
    /// A new row was inserted (with its `seq = 0` settlement event and every
    /// verified sample absorbed into `model_outcome_stats`) in one
    /// transaction.
    Appended,
    /// The run id already held a byte-identical row: the append is a
    /// crash/replay no-op — no second record, no second sample, no second
    /// settlement event.
    Duplicate,
}

/// One consistent metrics-read snapshot of the Engineering Outcome Ledger
/// (see [`Store::engineering_outcome_metrics_snapshot`]): the newest
/// engineering rows plus the latest settlement event of each, all read in
/// one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineeringOutcomeMetricsSnapshot {
    pub outcomes: Vec<EngineeringOutcomeRow>,
    pub settlements: Vec<EconomicSettlementRow>,
    /// More engineering rows exist than the requested bound.
    pub truncated: bool,
}

/// ONE settlement-lifecycle event exactly as stored. The `state` is the
/// typed `money_state`; amounts are present ONLY on `complete` (NULL means
/// unmeasured, never zero). `avoided_hosted_micro` is the counterfactual
/// measurement: `avoided_hosted_measured = 0` types it unavailable (NULL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EconomicSettlementRow {
    pub run_id: String,
    /// The event's position in the append-only stream. Ignored (assigned by
    /// the store) on [`Store::economic_settlement_append`]; must be 0 for
    /// the initial event passed to [`Store::engineering_outcome_append`].
    pub seq: i64,
    pub schema_version: i64,
    pub state: String,
    pub reason: String,
    pub model_spend_micro: Option<i64>,
    pub verification_spend_micro: Option<i64>,
    pub rework_spend_micro: Option<i64>,
    pub avoided_hosted_micro: Option<i64>,
    pub avoided_hosted_measured: bool,
    /// The settled per-step projection (`[{provider, model, local,
    /// spend_micro}]`), empty unless `state = complete`.
    pub steps_json: String,
    pub settled_at_ms: i64,
}

impl EconomicSettlementRow {
    /// Bounded structural validation on the caller's thread. Zero is only
    /// legal for a MEASURED zero: a non-complete event must carry NULL
    /// amounts and an unmeasured (`unavailable`) counterfactual.
    fn validate(&self) -> StoreResult<()> {
        if self.run_id.is_empty() || self.run_id.len() > MAX_ENGINEERING_OUTCOME_RUN_ID {
            return Err(StoreError::Malformed(format!(
                "economic settlement run id must be 1..={MAX_ENGINEERING_OUTCOME_RUN_ID} bytes, \
                 got {}",
                self.run_id.len()
            )));
        }
        if !self.run_id.is_ascii() {
            return Err(StoreError::Malformed(
                "economic settlement run id must be ASCII".into(),
            ));
        }
        if self.schema_version < 1 || self.schema_version > MAX_ECONOMIC_SETTLEMENT_SCHEMA_VERSION {
            return Err(StoreError::Malformed(format!(
                "economic settlement schema_version {} is outside 1..={MAX_ECONOMIC_SETTLEMENT_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if !ECONOMIC_SETTLEMENT_STATES.contains(&self.state.as_str()) {
            return Err(StoreError::Malformed(format!(
                "economic settlement state {:?} is not one of {ECONOMIC_SETTLEMENT_STATES:?}",
                self.state
            )));
        }
        if self.reason.is_empty() || self.reason.len() > MAX_ECONOMIC_SETTLEMENT_REASON {
            return Err(StoreError::Malformed(format!(
                "economic settlement reason must be 1..={MAX_ECONOMIC_SETTLEMENT_REASON} bytes, \
                 got {}",
                self.reason.len()
            )));
        }
        for (what, v) in [
            ("model_spend_micro", self.model_spend_micro),
            ("verification_spend_micro", self.verification_spend_micro),
            ("rework_spend_micro", self.rework_spend_micro),
            ("avoided_hosted_micro", self.avoided_hosted_micro),
        ] {
            if v.is_some_and(|v| v < 0) {
                return Err(StoreError::Malformed(format!(
                    "economic settlement {what} must be non-negative when present"
                )));
            }
        }
        if self.steps_json.len() > MAX_ECONOMIC_SETTLEMENT_STEPS_JSON {
            return Err(StoreError::Oversized(format!(
                "economic settlement step projection is {} bytes (cap \
                 {MAX_ECONOMIC_SETTLEMENT_STEPS_JSON})",
                self.steps_json.len()
            )));
        }
        let steps: Vec<serde_json::Value> = match serde_json::from_str(&self.steps_json) {
            Ok(steps) => steps,
            Err(e) => {
                return Err(StoreError::Malformed(format!(
                    "economic settlement step projection is not valid JSON: {e}"
                )));
            }
        };
        if steps.len() > MAX_ECONOMIC_SETTLEMENT_STEPS {
            return Err(StoreError::Oversized(format!(
                "economic settlement step projection has {} steps (cap \
                 {MAX_ECONOMIC_SETTLEMENT_STEPS})",
                steps.len()
            )));
        }
        let mut step_sum: u128 = 0;
        for (index, step) in steps.iter().enumerate() {
            let Some(object) = step.as_object() else {
                return Err(StoreError::Malformed(format!(
                    "economic settlement step {index} is not a JSON object"
                )));
            };
            let label = |what: &str| -> StoreResult<&str> {
                match object.get(what).and_then(|v| v.as_str()) {
                    Some(value)
                        if !value.is_empty()
                            && value.len() <= MAX_ECONOMIC_SETTLEMENT_MODEL_TEXT =>
                    {
                        Ok(value)
                    }
                    _ => Err(StoreError::Malformed(format!(
                        "economic settlement step {index} {what} must be a 1..=\
                         {MAX_ECONOMIC_SETTLEMENT_MODEL_TEXT} byte string"
                    ))),
                }
            };
            label("provider")?;
            label("model")?;
            if !object.get("local").is_some_and(|v| v.is_boolean()) {
                return Err(StoreError::Malformed(format!(
                    "economic settlement step {index} local must be a boolean"
                )));
            }
            let spend = object.get("spend_micro").and_then(|v| v.as_u64());
            let Some(spend) = spend else {
                return Err(StoreError::Malformed(format!(
                    "economic settlement step {index} spend_micro must be a non-negative integer"
                )));
            };
            step_sum = step_sum.saturating_add(u128::from(spend));
        }
        match self.state.as_str() {
            "complete" => {
                let (Some(model), Some(_verification), Some(rework)) = (
                    self.model_spend_micro,
                    self.verification_spend_micro,
                    self.rework_spend_micro,
                ) else {
                    return Err(StoreError::Malformed(
                        "economic settlement: a complete projection must carry its measured \
                         model/verification/rework spend"
                            .into(),
                    ));
                };
                if rework > model {
                    return Err(StoreError::Malformed(format!(
                        "economic settlement: rework spend {rework} exceeds model spend {model}"
                    )));
                }
                if step_sum != u128::from(model as u64) {
                    return Err(StoreError::Malformed(format!(
                        "economic settlement: step spend sum {step_sum} disagrees with the model \
                         spend {model}"
                    )));
                }
                if self.avoided_hosted_measured && self.avoided_hosted_micro.is_none() {
                    return Err(StoreError::Malformed(
                        "economic settlement: a measured avoided-hosted counterfactual must carry \
                         its value"
                            .into(),
                    ));
                }
                if !self.avoided_hosted_measured && self.avoided_hosted_micro.is_some() {
                    return Err(StoreError::Malformed(
                        "economic settlement: an unmeasured avoided-hosted counterfactual must \
                         not carry a value"
                            .into(),
                    ));
                }
            }
            _ => {
                if self.model_spend_micro.is_some()
                    || self.verification_spend_micro.is_some()
                    || self.rework_spend_micro.is_some()
                    || self.avoided_hosted_micro.is_some()
                    || self.avoided_hosted_measured
                {
                    return Err(StoreError::Malformed(
                        "economic settlement: only a complete projection may carry amounts; an \
                         unsettled/unavailable projection stores NULL, never a fabricated zero"
                            .into(),
                    ));
                }
                if !steps.is_empty() {
                    return Err(StoreError::Malformed(
                        "economic settlement: only a complete projection may carry settled steps"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The outcome of one [`Store::economic_settlement_append`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EconomicSettlementAppend {
    /// A new event was appended at this sequence position.
    Appended(i64),
    /// The run's latest event is semantically identical: a crash/replay
    /// no-op — no second event, no state movement.
    Duplicate,
}

/// The raw (undecoded) column values of one engineering row, bound inside a
/// writer job (get-only SQL; validation/decoding happens on the caller's
/// thread).
struct RawEngineeringOutcomeRow {
    run_id: String,
    session_id: i64,
    task_id: i64,
    payload: String,
    payload_bytes: i64,
    completed_at_ms: i64,
    verified: i64,
    verified_first_pass: i64,
    integrated: i64,
    local_used: i64,
    frontier_used: i64,
    wall_time_ms: i64,
    human_interventions: i64,
    rollback_events: i64,
    recovery_events: i64,
    repair_attempts: i64,
    risk_class: String,
    changed_files: i64,
    changed_lines: i64,
    checks_passed: i64,
    checks_required: i64,
}

/// Bind one raw engineering row (format `OUTCOME_ROW_COLUMNS`). Plain `get`
/// binds only — no decoding, no validation, no serialization.
fn raw_engineering_outcome_row(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<RawEngineeringOutcomeRow> {
    Ok(RawEngineeringOutcomeRow {
        run_id: r.get(0)?,
        session_id: r.get(1)?,
        task_id: r.get(2)?,
        payload: r.get(3)?,
        payload_bytes: r.get(4)?,
        completed_at_ms: r.get(5)?,
        verified: r.get(6)?,
        verified_first_pass: r.get(7)?,
        integrated: r.get(8)?,
        local_used: r.get(9)?,
        frontier_used: r.get(10)?,
        wall_time_ms: r.get(11)?,
        human_interventions: r.get(12)?,
        rollback_events: r.get(13)?,
        recovery_events: r.get(14)?,
        repair_attempts: r.get(15)?,
        risk_class: r.get(16)?,
        changed_files: r.get(17)?,
        changed_lines: r.get(18)?,
        checks_passed: r.get(19)?,
        checks_required: r.get(20)?,
    })
}

/// Decode one raw engineering row fallibly. A hostile flag/counter/invariant
/// or a payload that changed under a foreign write is a typed
/// [`StoreError::Corrupt`], never a fabricated record.
fn engineering_outcome_from_raw(
    raw: RawEngineeringOutcomeRow,
) -> StoreResult<EngineeringOutcomeRow> {
    let flag = |v: i64, what: &str| -> StoreResult<bool> {
        match v {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(StoreError::Corrupt(vec![format!(
                "engineering_outcome {what} is {other}, not 0/1"
            )])),
        }
    };
    let counter = |v: i64, what: &str| -> StoreResult<i64> {
        if v < 0 {
            return Err(StoreError::Corrupt(vec![format!(
                "engineering_outcome {what} is negative ({v})"
            )]));
        }
        Ok(v)
    };
    let row = EngineeringOutcomeRow {
        run_id: raw.run_id,
        session_id: counter(raw.session_id, "session_id")?,
        task_id: counter(raw.task_id, "task_id")?,
        payload: raw.payload,
        payload_bytes: counter(raw.payload_bytes, "payload_bytes")?,
        completed_at_ms: raw.completed_at_ms,
        verified: flag(raw.verified, "verified")?,
        verified_first_pass: flag(raw.verified_first_pass, "verified_first_pass")?,
        integrated: flag(raw.integrated, "integrated")?,
        local_used: flag(raw.local_used, "local_used")?,
        frontier_used: flag(raw.frontier_used, "frontier_used")?,
        wall_time_ms: counter(raw.wall_time_ms, "wall_time_ms")?,
        human_interventions: counter(raw.human_interventions, "human_interventions")?,
        rollback_events: counter(raw.rollback_events, "rollback_events")?,
        recovery_events: counter(raw.recovery_events, "recovery_events")?,
        repair_attempts: counter(raw.repair_attempts, "repair_attempts")?,
        risk_class: raw.risk_class,
        changed_files: counter(raw.changed_files, "changed_files")?,
        changed_lines: counter(raw.changed_lines, "changed_lines")?,
        checks_passed: counter(raw.checks_passed, "checks_passed")?,
        checks_required: counter(raw.checks_required, "checks_required")?,
    };
    row.validate().map_err(|e| {
        StoreError::Corrupt(vec![format!("engineering_outcome {}: {e}", row.run_id)])
    })?;
    Ok(row)
}

/// The raw (undecoded) column values of one settlement row.
#[derive(Clone)]
struct RawSettlementRow {
    run_id: String,
    seq: i64,
    schema_version: i64,
    state: String,
    reason: String,
    model_spend_micro: Option<i64>,
    verification_spend_micro: Option<i64>,
    rework_spend_micro: Option<i64>,
    avoided_hosted_micro: Option<i64>,
    avoided_hosted_measured: i64,
    steps_json: String,
    settled_at_ms: i64,
}

/// Bind one raw settlement row (format `SETTLEMENT_ROW_COLUMNS`).
fn raw_settlement_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawSettlementRow> {
    Ok(RawSettlementRow {
        run_id: r.get(0)?,
        seq: r.get(1)?,
        schema_version: r.get(2)?,
        state: r.get(3)?,
        reason: r.get(4)?,
        model_spend_micro: r.get(5)?,
        verification_spend_micro: r.get(6)?,
        rework_spend_micro: r.get(7)?,
        avoided_hosted_micro: r.get(8)?,
        avoided_hosted_measured: r.get(9)?,
        steps_json: r.get(10)?,
        settled_at_ms: r.get(11)?,
    })
}

/// Decode one raw settlement row fallibly: a hostile state/amount/step
/// projection is a typed [`StoreError::Corrupt`], never a fabricated
/// financial fact.
fn economic_settlement_from_raw(raw: RawSettlementRow) -> StoreResult<EconomicSettlementRow> {
    if raw.seq < 0 {
        return Err(StoreError::Corrupt(vec![format!(
            "economic_settlement {} seq is negative ({})",
            raw.run_id, raw.seq
        )]));
    }
    let measured = match raw.avoided_hosted_measured {
        0 => false,
        1 => true,
        other => {
            return Err(StoreError::Corrupt(vec![format!(
                "economic_settlement {} avoided_hosted_measured is {other}, not 0/1",
                raw.run_id
            )]));
        }
    };
    let row = EconomicSettlementRow {
        run_id: raw.run_id,
        seq: raw.seq,
        schema_version: raw.schema_version,
        state: raw.state,
        reason: raw.reason,
        model_spend_micro: raw.model_spend_micro,
        verification_spend_micro: raw.verification_spend_micro,
        rework_spend_micro: raw.rework_spend_micro,
        avoided_hosted_micro: raw.avoided_hosted_micro,
        avoided_hosted_measured: measured,
        steps_json: raw.steps_json,
        settled_at_ms: raw.settled_at_ms,
    };
    row.validate().map_err(|e| {
        StoreError::Corrupt(vec![format!(
            "economic_settlement {}#{}: {e}",
            row.run_id, row.seq
        )])
    })?;
    Ok(row)
}

/// Semantic identity of a STORED raw settlement row and a submitted event
/// (everything except `seq` and `settled_at_ms`: a replay of the same fact
/// is a no-op even at a new clock). Raw comparison only — the writer job
/// never decodes or parses JSON.
fn raw_settlements_semantically_equal(
    stored: &RawSettlementRow,
    submitted: &EconomicSettlementRow,
) -> bool {
    stored.schema_version == submitted.schema_version
        && stored.state == submitted.state
        && stored.reason == submitted.reason
        && stored.model_spend_micro == submitted.model_spend_micro
        && stored.verification_spend_micro == submitted.verification_spend_micro
        && stored.rework_spend_micro == submitted.rework_spend_micro
        && stored.avoided_hosted_micro == submitted.avoided_hosted_micro
        && stored.avoided_hosted_measured == i64::from(submitted.avoided_hosted_measured)
        && stored.steps_json == submitted.steps_json
}

/// Whether `next` may legally follow `latest` in the settlement lifecycle.
fn settlement_transition_legal(latest: &str, next: &str) -> bool {
    match latest {
        // Only a settled projection may be corrected; a complete fact never
        // returns to pending/unavailable.
        "complete" => next == "complete",
        // Once settled pending is gone for good: the money either became
        // measurable or was typed unavailable.
        "pending" => next == "complete" || next == "unavailable",
        // An unavailable projection is the least-informed state: either a
        // later complete reconciliation settles it, or a RECOVERED read
        // establishes that reservations are open (pending) — a legal
        // improvement, never a re-unmeasuring.
        "unavailable" => next == "complete" || next == "pending",
        _ => false,
    }
}

/// Whether a `complete -> complete` CORRECTION keeps the documented
/// monotone-repair semantics (P1 economic-truth closure): every measured
/// amount present on the latest event must stay present on the correction
/// and may only move UP (append-only settlements can add spend, never
/// remove it), and a measured avoided-hosted counterfactual never becomes
/// unmeasured. Pure metadata (reason, clock, step labels) may still move.
fn settlement_correction_monotone(latest: &RawSettlementRow, next: &EconomicSettlementRow) -> bool {
    let amount_upward = |previous: Option<i64>, candidate: Option<i64>| match (previous, candidate)
    {
        (Some(previous), Some(candidate)) => candidate >= previous,
        (Some(_), None) => false,
        (None, _) => true,
    };
    amount_upward(latest.model_spend_micro, next.model_spend_micro)
        && amount_upward(
            latest.verification_spend_micro,
            next.verification_spend_micro,
        )
        && amount_upward(latest.rework_spend_micro, next.rework_spend_micro)
        && amount_upward(latest.avoided_hosted_micro, next.avoided_hosted_micro)
        && (!(latest.avoided_hosted_measured != 0) || next.avoided_hosted_measured)
}

/// What one settlement writer job did: a fresh event at `seq`, or the
/// existing latest row (compared and classified by the caller AFTER the job
/// — the job writes nothing on that path).
enum RawSettlementAppend {
    Appended(i64),
    Existing,
}

/// What one engineering-append writer job did.
enum RawAppendOutcome {
    Appended,
    Existing(Box<RawEngineeringOutcomeRow>),
}

impl Store {
    /// Append ONE completed run's immutable engineering row, exactly once,
    /// with its `seq = 0` initial settlement event and the record's verified
    /// samples absorbed into the router's durable `model_outcome_stats`
    /// registry — all in ONE immediate transaction. See the module docs for
    /// the exactly-once contract; a conflicting re-append is a typed
    /// [`StoreError::Conflict`], never an overwrite.
    pub fn engineering_outcome_append(
        &self,
        row: &EngineeringOutcomeRow,
        router_samples: &[ModelOutcomeIncrement],
        settlement: &EconomicSettlementRow,
    ) -> StoreResult<EngineeringOutcomeAppend> {
        row.validate()?;
        if settlement.run_id != row.run_id {
            return Err(StoreError::Malformed(format!(
                "engineering outcome append: settlement run id {:?} disagrees with the row run \
                 id {:?}",
                settlement.run_id, row.run_id
            )));
        }
        if settlement.seq != 0 {
            return Err(StoreError::Malformed(
                "engineering outcome append: the initial settlement event must carry seq = 0"
                    .into(),
            ));
        }
        settlement.validate()?;
        // Preparation BEFORE enqueueing (audit item 8): every string —
        // SQL text, enum-dimension encoding, the conflict message — is
        // materialized on the caller's thread; the writer job executes SQL
        // only.
        let row = row.clone();
        let submitted = row.clone();
        let settlement = settlement.clone();
        let select_sql =
            format!("SELECT {OUTCOME_ROW_COLUMNS} FROM engineering_outcome WHERE run_id = ?1");
        let insert_sql = format!(
            "INSERT INTO engineering_outcome ({OUTCOME_ROW_COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, \
              ?19, ?20, ?21)"
        );
        let settlement_insert_sql = format!(
            "INSERT INTO economic_settlement ({SETTLEMENT_ROW_COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
        );
        let conflict_message = format!(
            "engineering outcome run id {:?} already holds a different record; the ledger is \
             append-only and never overwrites an identity",
            row.run_id
        );
        let increments: Vec<PreparedOutcomeSample> = router_samples
            .iter()
            .map(|inc| PreparedOutcomeSample {
                provider: inc.provider.clone(),
                model: inc.model.clone(),
                phase_json: outcome_db_phase(inc.phase),
                class_json: outcome_db_class(inc.task_class),
                bucket_json: outcome_db_bucket(inc.risk_bucket),
                sample: inc.sample,
            })
            .collect();
        let updated_ms = now_ms();
        let seam = Arc::clone(&self.seam);
        let outcome = self
            .writer
            .execute("engineering_outcome_append", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing = tx
                    .query_row(
                        &select_sql,
                        params![&row.run_id],
                        raw_engineering_outcome_row,
                    )
                    .optional()?;
                if let Some(existing) = existing {
                    // Nothing is written on this path; the caller compares
                    // and classifies AFTER the job.
                    return Ok(RawAppendOutcome::Existing(Box::new(existing)));
                }
                tx.execute(
                    &insert_sql,
                    params![
                        row.run_id,
                        row.session_id,
                        row.task_id,
                        row.payload,
                        row.payload_bytes,
                        row.completed_at_ms,
                        row.verified as i64,
                        row.verified_first_pass as i64,
                        row.integrated as i64,
                        row.local_used as i64,
                        row.frontier_used as i64,
                        row.wall_time_ms,
                        row.human_interventions,
                        row.rollback_events,
                        row.recovery_events,
                        row.repair_attempts,
                        row.risk_class,
                        row.changed_files,
                        row.changed_lines,
                        row.checks_passed,
                        row.checks_required
                    ],
                )?;
                // The initial settlement event rides the SAME transaction as
                // the immutable engineering row: a crash can neither leave a
                // terminal record without its money classification nor the
                // classification without its record.
                tx.execute(
                    &settlement_insert_sql,
                    params![
                        settlement.run_id,
                        settlement.seq,
                        settlement.schema_version,
                        settlement.state,
                        settlement.reason,
                        settlement.model_spend_micro,
                        settlement.verification_spend_micro,
                        settlement.rework_spend_micro,
                        settlement.avoided_hosted_micro,
                        settlement.avoided_hosted_measured as i64,
                        settlement.steps_json,
                        settlement.settled_at_ms
                    ],
                )?;
                // The router feed rides the SAME transaction as the record:
                // a crash cannot leave one without the other, and the
                // replay path above can never absorb twice.
                for inc in &increments {
                    Self::absorb_model_outcome_sample_tx(
                        &tx,
                        &PreparedOutcomeKey {
                            provider: &inc.provider,
                            model: &inc.model,
                            phase: &inc.phase_json,
                            task_class: &inc.class_json,
                            risk_bucket: &inc.bucket_json,
                        },
                        inc.sample,
                        updated_ms,
                    )?;
                }
                // Deterministic crash seam (fault certification): an armed
                // `engineering_outcome_precommit` crossing panics with the
                // transaction open — the record, the settlement event AND
                // the router feed roll back together;
                // `engineering_outcome_committed` fires with all durable, so
                // a replay after the crash is a Duplicate.
                seam.trip("engineering_outcome_precommit");
                tx.commit()?;
                seam.trip("engineering_outcome_committed");
                Ok(RawAppendOutcome::Appended)
            })?;
        match outcome {
            RawAppendOutcome::Appended => {
                self.outcome_revision
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
                Ok(EngineeringOutcomeAppend::Appended)
            }
            RawAppendOutcome::Existing(raw) => {
                let existing = engineering_outcome_from_raw(*raw)?;
                if existing == submitted {
                    Ok(EngineeringOutcomeAppend::Duplicate)
                } else {
                    Err(StoreError::Conflict(conflict_message))
                }
            }
        }
    }

    /// Append ONE settlement-lifecycle event. The store assigns the next
    /// sequence position inside the immediate transaction; a semantically
    /// identical replay of the latest event is a `Duplicate` no-op, and an
    /// illegal transition (`complete -> pending`, `pending -> pending` with
    /// a different fact, `unavailable -> unavailable` with a different fact,
    /// ...) is a typed [`StoreError::Conflict`].
    pub fn economic_settlement_append(
        &self,
        settlement: &EconomicSettlementRow,
    ) -> StoreResult<EconomicSettlementAppend> {
        settlement.validate()?;
        let settlement = settlement.clone();
        let latest_sql = format!(
            "SELECT {SETTLEMENT_ROW_COLUMNS} FROM economic_settlement WHERE run_id = ?1 \
             ORDER BY seq DESC LIMIT 1"
        );
        let insert_sql = format!(
            "INSERT INTO economic_settlement ({SETTLEMENT_ROW_COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
        );
        // Prepared BEFORE enqueueing (audit item 8): the writer job executes
        // SQL only — no formatting, serialization or decoding on the writer.
        let illegal_transition_message = format!(
            "economic settlement of run {:?} may not move from its current state into {:?}; the \
             lifecycle is pending -> complete|unavailable, unavailable -> complete|pending, \
             complete -> complete corrections only",
            settlement.run_id, settlement.state
        );
        let correction_message = format!(
            "economic settlement of run {:?} may not move a complete projection's measured \
             amounts downward (or re-unmeasure its counterfactual); complete -> complete \
             corrections repair measured amounts upward only",
            settlement.run_id
        );
        let unknown_state_corrupt: Vec<String> = vec![
            "economic_settlement: the latest event holds a state outside the lifecycle; the \
             stream is corrupt"
                .into(),
        ];
        let seq_overflow_corrupt: Vec<String> = vec![
            "economic_settlement: the event sequence overflowed; the stream is corrupt".into(),
        ];
        let seam = Arc::clone(&self.seam);
        let outcome = self
            .writer
            .execute("economic_settlement_append", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let latest = tx
                    .query_row(&latest_sql, params![&settlement.run_id], raw_settlement_row)
                    .optional()?;
                let seq = match &latest {
                    None => 0,
                    Some(latest) => {
                        if !ECONOMIC_SETTLEMENT_STATES.contains(&latest.state.as_str()) {
                            return Err(StoreError::Corrupt(unknown_state_corrupt));
                        }
                        if raw_settlements_semantically_equal(latest, &settlement) {
                            return Ok(RawSettlementAppend::Existing);
                        }
                        if !settlement_transition_legal(&latest.state, &settlement.state) {
                            return Err(StoreError::Conflict(illegal_transition_message));
                        }
                        // complete -> complete is the correction path: it may
                        // repair measured amounts UPWARD only (the store is
                        // the monotonicity authority — a downward correction
                        // is refused here even though the monotone finalize
                        // never derives one).
                        if latest.state == "complete"
                            && settlement.state == "complete"
                            && !settlement_correction_monotone(latest, &settlement)
                        {
                            return Err(StoreError::Conflict(correction_message));
                        }
                        latest
                            .seq
                            .checked_add(1)
                            .ok_or(StoreError::Corrupt(seq_overflow_corrupt))?
                    }
                };
                tx.execute(
                    &insert_sql,
                    params![
                        settlement.run_id,
                        seq,
                        settlement.schema_version,
                        settlement.state,
                        settlement.reason,
                        settlement.model_spend_micro,
                        settlement.verification_spend_micro,
                        settlement.rework_spend_micro,
                        settlement.avoided_hosted_micro,
                        settlement.avoided_hosted_measured as i64,
                        settlement.steps_json,
                        settlement.settled_at_ms
                    ],
                )?;
                seam.trip("economic_settlement_precommit");
                tx.commit()?;
                seam.trip("economic_settlement_committed");
                Ok(RawSettlementAppend::Appended(seq))
            })?;
        match outcome {
            RawSettlementAppend::Appended(seq) => {
                self.outcome_revision
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
                Ok(EconomicSettlementAppend::Appended(seq))
            }
            RawSettlementAppend::Existing => Ok(EconomicSettlementAppend::Duplicate),
        }
    }

    /// The exact durable engineering row of one run, or `None` when no
    /// completed run with that identity was ever recorded. A
    /// present-but-hostile row is a typed [`StoreError::Corrupt`].
    pub fn engineering_outcome_get(
        &self,
        run_id: &str,
    ) -> StoreResult<Option<EngineeringOutcomeRow>> {
        let conn = self.read()?;
        let raw = conn
            .query_row(
                &format!("SELECT {OUTCOME_ROW_COLUMNS} FROM engineering_outcome WHERE run_id = ?1"),
                params![run_id],
                raw_engineering_outcome_row,
            )
            .optional()?;
        raw.map(engineering_outcome_from_raw).transpose()
    }

    /// The LATEST settlement event of one run, or `None` when the run has no
    /// settlement lifecycle (a legacy record, or none at all).
    pub fn economic_settlement_latest(
        &self,
        run_id: &str,
    ) -> StoreResult<Option<EconomicSettlementRow>> {
        let conn = self.read()?;
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {SETTLEMENT_ROW_COLUMNS} FROM economic_settlement WHERE run_id = ?1 \
                     ORDER BY seq DESC LIMIT 1"
                ),
                params![run_id],
                raw_settlement_row,
            )
            .optional()?;
        raw.map(economic_settlement_from_raw).transpose()
    }

    /// The LATEST settlement event of every named run (bounded batch; a run
    /// without a settlement is simply absent). One query per bounded chunk;
    /// a corrupt row fails the batch loudly.
    pub fn economic_settlement_latest_for_runs(
        &self,
        run_ids: &[String],
    ) -> StoreResult<Vec<EconomicSettlementRow>> {
        if run_ids.is_empty() {
            return Ok(Vec::new());
        }
        if run_ids.len() > MAX_ECONOMIC_SETTLEMENT_BATCH {
            return Err(StoreError::Oversized(format!(
                "economic settlement batch read of {} ids exceeds the {MAX_ECONOMIC_SETTLEMENT_BATCH} \
                 bound",
                run_ids.len()
            )));
        }
        let conn = self.read()?;
        let placeholders = vec!["?"; run_ids.len()].join(", ");
        let sql = format!(
            "SELECT {SETTLEMENT_ROW_COLUMNS} FROM economic_settlement s \
             WHERE s.seq = (SELECT MAX(seq) FROM economic_settlement s2 WHERE s2.run_id = s.run_id) \
             AND s.run_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let raws: Vec<RawSettlementRow> = stmt
            .query_map(
                rusqlite::params_from_iter(run_ids.iter()),
                raw_settlement_row,
            )
            .and_then(|rows| rows.collect())?;
        raws.into_iter().map(economic_settlement_from_raw).collect()
    }

    /// One bounded newest-first page of engineering rows, strictly OLDER
    /// than the `(completed_at_ms, run_id)` cursor when one is given (the
    /// composite key makes the order total: no two rows can be skipped or
    /// repeated between calls). `limit` is clamped to
    /// `1..=MAX_ENGINEERING_OUTCOME_PAGE`. A corrupt row fails the page
    /// loudly.
    pub fn engineering_outcome_page(
        &self,
        before: Option<(i64, &str)>,
        limit: u64,
    ) -> StoreResult<Vec<EngineeringOutcomeRow>> {
        let limit = limit.clamp(1, MAX_ENGINEERING_OUTCOME_PAGE) as i64;
        let conn = self.read()?;
        let raws: Vec<RawEngineeringOutcomeRow> = match before {
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {OUTCOME_ROW_COLUMNS} FROM engineering_outcome \
                     ORDER BY completed_at_ms DESC, run_id DESC LIMIT ?1"
                ))?;
                stmt.query_map(params![limit], raw_engineering_outcome_row)
                    .and_then(|rows| rows.collect())
            }
            Some((at_ms, run_id)) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {OUTCOME_ROW_COLUMNS} FROM engineering_outcome \
                     WHERE completed_at_ms < ?1 OR (completed_at_ms = ?1 AND run_id < ?2) \
                     ORDER BY completed_at_ms DESC, run_id DESC LIMIT ?3"
                ))?;
                stmt.query_map(params![at_ms, run_id, limit], raw_engineering_outcome_row)
                    .and_then(|rows| rows.collect())
            }
        }?;
        raws.into_iter().map(engineering_outcome_from_raw).collect()
    }

    /// One CONSISTENT snapshot of the ledger for a metrics fold: the newest
    /// `max_records` engineering rows AND the latest settlement event of
    /// every one of them, read inside ONE deferred read transaction.
    ///
    /// The historical fold paged the ledger through separate read
    /// connections and folded each page's settlement immediately; a
    /// concurrent settlement correction could land between pages, so one
    /// run was counted with a projection from a different snapshot than its
    /// peers (a torn aggregate under concurrency). One transaction makes
    /// the whole fold see a single committed state. `truncated` is explicit
    /// when more rows exist than the requested bound.
    pub fn engineering_outcome_metrics_snapshot(
        &self,
        max_records: u64,
    ) -> StoreResult<EngineeringOutcomeMetricsSnapshot> {
        let bound = max_records.max(1).saturating_add(1);
        let limit = i64::try_from(bound).unwrap_or(i64::MAX);
        let conn = self.read()?;
        let tx = conn.unchecked_transaction()?;
        let mut stmt = tx.prepare(&format!(
            "SELECT {OUTCOME_ROW_COLUMNS} FROM engineering_outcome \
             ORDER BY completed_at_ms DESC, run_id DESC LIMIT ?1"
        ))?;
        let raws: Vec<RawEngineeringOutcomeRow> = stmt
            .query_map(params![limit], raw_engineering_outcome_row)
            .and_then(|rows| rows.collect())?;
        drop(stmt);
        let truncated = raws.len() as u64 > max_records;
        let mut outcomes = Vec::with_capacity(raws.len().min(max_records as usize));
        for raw in raws.into_iter().take(max_records as usize) {
            outcomes.push(engineering_outcome_from_raw(raw)?);
        }
        // Latest settlement of every selected run, batched inside the SAME
        // transaction (so the money projection and the engineering rows are
        // one snapshot).
        let run_ids: Vec<String> = outcomes.iter().map(|row| row.run_id.clone()).collect();
        let mut settlements = Vec::with_capacity(run_ids.len());
        for chunk in run_ids.chunks(MAX_ECONOMIC_SETTLEMENT_BATCH) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT {SETTLEMENT_ROW_COLUMNS} FROM economic_settlement s \
                 WHERE s.seq = (SELECT MAX(seq) FROM economic_settlement s2 WHERE s2.run_id = s.run_id) \
                 AND s.run_id IN ({placeholders})"
            );
            let mut stmt = tx.prepare(&sql)?;
            let raws: Vec<RawSettlementRow> = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), raw_settlement_row)
                .and_then(|rows| rows.collect())?;
            for raw in raws {
                settlements.push(economic_settlement_from_raw(raw)?);
            }
        }
        Ok(EngineeringOutcomeMetricsSnapshot {
            outcomes,
            settlements,
            truncated,
        })
    }

    /// One page of engineering rows plus the LATEST settlement of each,
    /// read inside ONE transaction. The read surface previously paged the
    /// rows and then batched settlements on a second connection, so a
    /// concurrent correction could land between them and the page showed a
    /// money projection from a different committed state than its runs.
    pub fn engineering_outcome_page_with_settlements(
        &self,
        before: Option<(i64, &str)>,
        limit: u64,
    ) -> StoreResult<(Vec<EngineeringOutcomeRow>, Vec<EconomicSettlementRow>)> {
        let limit = limit.clamp(1, MAX_ENGINEERING_OUTCOME_PAGE) as i64;
        let conn = self.read()?;
        let tx = conn.unchecked_transaction()?;
        let raws: Vec<RawEngineeringOutcomeRow> = match before {
            None => {
                let mut stmt = tx.prepare(&format!(
                    "SELECT {OUTCOME_ROW_COLUMNS} FROM engineering_outcome \
                     ORDER BY completed_at_ms DESC, run_id DESC LIMIT ?1"
                ))?;
                stmt.query_map(params![limit], raw_engineering_outcome_row)
                    .and_then(|rows| rows.collect())
            }
            Some((at_ms, run_id)) => {
                let mut stmt = tx.prepare(&format!(
                    "SELECT {OUTCOME_ROW_COLUMNS} FROM engineering_outcome \
                     WHERE completed_at_ms < ?1 OR (completed_at_ms = ?1 AND run_id < ?2) \
                     ORDER BY completed_at_ms DESC, run_id DESC LIMIT ?3"
                ))?;
                stmt.query_map(params![at_ms, run_id, limit], raw_engineering_outcome_row)
                    .and_then(|rows| rows.collect())
            }
        }?;
        let outcomes = raws
            .into_iter()
            .map(engineering_outcome_from_raw)
            .collect::<StoreResult<Vec<_>>>()?;
        let run_ids: Vec<String> = outcomes.iter().map(|row| row.run_id.clone()).collect();
        let mut settlements = Vec::with_capacity(run_ids.len());
        for chunk in run_ids.chunks(MAX_ECONOMIC_SETTLEMENT_BATCH) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT {SETTLEMENT_ROW_COLUMNS} FROM economic_settlement s \
                 WHERE s.seq = (SELECT MAX(seq) FROM economic_settlement s2 WHERE s2.run_id = s.run_id) \
                 AND s.run_id IN ({placeholders})"
            );
            let mut stmt = tx.prepare(&sql)?;
            let raws: Vec<RawSettlementRow> = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), raw_settlement_row)
                .and_then(|rows| rows.collect())?;
            for raw in raws {
                settlements.push(economic_settlement_from_raw(raw)?);
            }
        }
        Ok((outcomes, settlements))
    }

    /// Exact number of durable ledger rows (doctor/analytics surfaces).
    pub fn engineering_outcome_count(&self) -> StoreResult<u64> {
        let conn = self.read()?;
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM engineering_outcome", [], |r| r.get(0))?;
        u64::try_from(n).map_err(|_| {
            StoreError::Corrupt(vec![format!("engineering_outcome count {n} is negative")])
        })
    }

    /// Upsert the durable terminal-run facts (the terminal-turn outbox) of
    /// one run. REFUSED once the engineering row exists: the facts freeze at
    /// record time, so every later producer derives from the same source.
    pub fn terminal_run_facts_upsert(&self, run_id: &str, payload: &str) -> StoreResult<()> {
        if run_id.is_empty() || run_id.len() > MAX_ENGINEERING_OUTCOME_RUN_ID {
            return Err(StoreError::Malformed(format!(
                "terminal run facts run id must be 1..={MAX_ENGINEERING_OUTCOME_RUN_ID} bytes, \
                 got {}",
                run_id.len()
            )));
        }
        if !run_id.is_ascii() {
            return Err(StoreError::Malformed(
                "terminal run facts run id must be ASCII".into(),
            ));
        }
        if payload.is_empty() || payload.len() > MAX_TERMINAL_RUN_FACTS_PAYLOAD {
            return Err(StoreError::Oversized(format!(
                "terminal run facts payload must be 1..={MAX_TERMINAL_RUN_FACTS_PAYLOAD} bytes, \
                 got {}",
                payload.len()
            )));
        }
        match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(serde_json::Value::Object(_)) => {}
            Ok(_) => {
                return Err(StoreError::Malformed(
                    "terminal run facts payload must be a JSON object".into(),
                ));
            }
            Err(e) => {
                return Err(StoreError::Malformed(format!(
                    "terminal run facts payload is not valid JSON: {e}"
                )));
            }
        }
        let run_id = run_id.to_string();
        let payload = payload.to_string();
        let payload_bytes = payload.len() as i64;
        let updated_ms = now_ms();
        let conflict_message = format!(
            "terminal run facts of {run_id:?} are frozen: the engineering outcome record already \
             exists and the facts may not move after record time"
        );
        self.writer
            .execute("terminal_run_facts_upsert", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let recorded: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM engineering_outcome WHERE run_id = ?1",
                        params![&run_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if recorded.is_some() {
                    return Err(StoreError::Conflict(conflict_message));
                }
                tx.execute(
                    "INSERT INTO terminal_run_facts (run_id, payload, payload_bytes, updated_ms) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(run_id) DO UPDATE SET payload = excluded.payload, \
                 payload_bytes = excluded.payload_bytes, updated_ms = excluded.updated_ms",
                    params![run_id, payload, payload_bytes, updated_ms],
                )?;
                tx.commit()?;
                Ok(())
            })
    }

    /// The durable terminal-run facts payload of one run, or `None` when the
    /// agent path never persisted any (the honest default). A
    /// present-but-hostile row is a typed [`StoreError::Corrupt`].
    pub fn terminal_run_facts_get(&self, run_id: &str) -> StoreResult<Option<String>> {
        let conn = self.read()?;
        let raw: Option<(String, i64, i64)> = conn
            .query_row(
                "SELECT payload, payload_bytes, updated_ms FROM terminal_run_facts \
                 WHERE run_id = ?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((payload, payload_bytes, _updated_ms)) = raw else {
            return Ok(None);
        };
        if payload.is_empty() || payload.len() > MAX_TERMINAL_RUN_FACTS_PAYLOAD {
            return Err(StoreError::Corrupt(vec![format!(
                "terminal_run_facts {run_id} payload length {} is outside its bound",
                payload.len()
            )]));
        }
        if payload_bytes != payload.len() as i64 {
            return Err(StoreError::Corrupt(vec![format!(
                "terminal_run_facts {run_id} payload_bytes {payload_bytes} disagrees with the \
                 payload length {}",
                payload.len()
            )]));
        }
        match serde_json::from_str::<serde_json::Value>(&payload) {
            Ok(serde_json::Value::Object(_)) => Ok(Some(payload)),
            _ => Err(StoreError::Corrupt(vec![format!(
                "terminal_run_facts {run_id} payload is not a JSON object"
            )])),
        }
    }

    /// Count the RESOLVED permission decisions of one session (the durable
    /// human-intervention projection of the outcome ledger): every
    /// `allow` / `deny` / `expired` decision row is one recorded human
    /// intervention. Pending rows are not decisions and never count.
    pub fn engineering_outcome_human_interventions(&self, session: SessionId) -> StoreResult<u64> {
        let conn = self.read()?;
        let session = i64::try_from(session.raw()).map_err(|_| {
            StoreError::Malformed(format!(
                "session id {} exceeds the durable signed 64-bit domain",
                session.raw()
            ))
        })?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM permission WHERE session_id = ?1 \
             AND decision IN ('allow', 'deny', 'expired')",
            params![session],
            |r| r.get(0),
        )?;
        count_to_u64("human_interventions", n)
    }

    /// Count the durable crash-recovery events of one session
    /// (`recovery_applied` journal events; the terminal event of one real
    /// recovery pass). A corrupt/expected count is a loud typed error.
    pub fn engineering_outcome_recovery_events(&self, session: SessionId) -> StoreResult<u64> {
        let conn = self.read()?;
        let session = i64::try_from(session.raw()).map_err(|_| {
            StoreError::Malformed(format!(
                "session id {} exceeds the durable signed 64-bit domain",
                session.raw()
            ))
        })?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM event WHERE session_id = ?1 AND kind = 'recovery_applied'",
            params![session],
            |r| r.get(0),
        )?;
        count_to_u64("recovery_events", n)
    }
}

/// Fallible `i64 -> u64` counter read (a negative SQLite COUNT is corruption).
fn count_to_u64(what: &str, n: i64) -> StoreResult<u64> {
    u64::try_from(n).map_err(|_| {
        StoreError::Corrupt(vec![format!(
            "engineering_outcome {what} count {n} is negative"
        )])
    })
}

/// One prepared sample of the router feed (enum dimensions already encoded
/// on the caller's thread).
struct PreparedOutcomeSample {
    provider: String,
    model: String,
    phase_json: String,
    class_json: String,
    bucket_json: String,
    sample: ModelOutcomeSample,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_of(run_id: &str) -> String {
        format!(r#"{{"schema_version":2,"run_id":"{run_id}"}}"#)
    }

    fn row(run_id: &str, completed_at_ms: i64) -> EngineeringOutcomeRow {
        let payload = payload_of(run_id);
        EngineeringOutcomeRow {
            payload_bytes: payload.len() as i64,
            run_id: run_id.into(),
            session_id: 7,
            task_id: 11,
            payload,
            completed_at_ms,
            verified: true,
            verified_first_pass: true,
            integrated: true,
            local_used: false,
            frontier_used: false,
            wall_time_ms: 500,
            human_interventions: 0,
            rollback_events: 0,
            recovery_events: 0,
            repair_attempts: 0,
            risk_class: "medium".into(),
            changed_files: 2,
            changed_lines: 40,
            checks_passed: 3,
            checks_required: 3,
        }
    }

    fn steps_json(entries: &[(&str, &str, bool, u64)]) -> String {
        let steps: Vec<serde_json::Value> = entries
            .iter()
            .map(|(provider, model, local, spend)| {
                serde_json::json!({
                    "provider": provider,
                    "model": model,
                    "local": local,
                    "spend_micro": spend,
                })
            })
            .collect();
        serde_json::Value::Array(steps).to_string()
    }

    fn settlement(run_id: &str, seq: i64, state: &str) -> EconomicSettlementRow {
        let complete = state == "complete";
        EconomicSettlementRow {
            run_id: run_id.into(),
            seq,
            schema_version: 1,
            state: state.into(),
            reason: "test".into(),
            model_spend_micro: complete.then_some(1_000),
            verification_spend_micro: complete.then_some(250),
            rework_spend_micro: complete.then_some(0),
            avoided_hosted_micro: None,
            avoided_hosted_measured: false,
            steps_json: if complete {
                steps_json(&[("p", "m", false, 1_000)])
            } else {
                "[]".into()
            },
            settled_at_ms: 100,
        }
    }

    fn sample(provider: &str, model: &str, verified_success: bool) -> ModelOutcomeIncrement {
        ModelOutcomeIncrement {
            provider: provider.into(),
            model: model.into(),
            phase: RouterPhase::Implement,
            task_class: TaskClass::Medium,
            risk_bucket: RiskBucket::Low,
            sample: ModelOutcomeSample {
                verified_success,
                rework_cost_micro: if verified_success { 0 } else { 900 },
                rework_turns: if verified_success { 0 } else { 2 },
            },
        }
    }

    #[test]
    fn ledger_row_round_trips_and_replay_is_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let r = row("run-1", 100);
        let s = settlement("run-1", 0, "complete");
        assert_eq!(
            store
                .engineering_outcome_append(&r, &[sample("p", "m", true)], &s)
                .unwrap(),
            EngineeringOutcomeAppend::Appended
        );
        assert_eq!(
            store.engineering_outcome_get("run-1").unwrap(),
            Some(r.clone())
        );
        assert_eq!(
            store.economic_settlement_latest("run-1").unwrap(),
            Some(s.clone())
        );
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        // Replay (identical row): Duplicate, still ONE row, ONE settlement
        // event and the router sample absorbed exactly once.
        assert_eq!(
            store
                .engineering_outcome_append(&r, &[sample("p", "m", true)], &s)
                .unwrap(),
            EngineeringOutcomeAppend::Duplicate
        );
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        assert_eq!(
            store
                .economic_settlement_latest_for_runs(&["run-1".to_string()])
                .unwrap()
                .len(),
            1
        );
        let stats = store
            .model_outcome_stats_get(
                "p",
                "m",
                RouterPhase::Implement,
                TaskClass::Medium,
                RiskBucket::Low,
            )
            .unwrap()
            .unwrap();
        assert_eq!(stats.sample_count, 1);
        assert_eq!(stats.successes_first_pass, 1);
        // Reopen: migrations are a no-op, the record and the feed survive.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.engineering_outcome_get("run-1").unwrap(), Some(r));
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
    }

    #[test]
    fn conflicting_identity_is_refused_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let original = row("run-1", 100);
        assert_eq!(
            store
                .engineering_outcome_append(&original, &[], &settlement("run-1", 0, "complete"))
                .unwrap(),
            EngineeringOutcomeAppend::Appended
        );
        let mut hostile = row("run-1", 100);
        hostile.wall_time_ms = 1;
        let err = store
            .engineering_outcome_append(
                &hostile,
                &[sample("p", "m", true)],
                &settlement("run-1", 0, "pending"),
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
        // The original row is intact AND the hostile sample never landed.
        assert_eq!(
            store.engineering_outcome_get("run-1").unwrap(),
            Some(original)
        );
        assert!(store
            .model_outcome_stats_get(
                "p",
                "m",
                RouterPhase::Implement,
                TaskClass::Medium,
                RiskBucket::Low
            )
            .unwrap()
            .is_none());
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        // The settlement of the refused append never landed either.
        assert_eq!(
            store
                .economic_settlement_latest("run-1")
                .unwrap()
                .unwrap()
                .state,
            "complete"
        );
    }

    #[test]
    fn race_writers_never_double_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let r = row("run-race", 42);
        let s = settlement("run-race", 0, "complete");
        let appended = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let store = &store;
                let r = &r;
                let s = &s;
                handles.push(
                    scope.spawn(move || store.engineering_outcome_append(r, &[], s).unwrap()),
                );
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|o| *o == EngineeringOutcomeAppend::Appended)
                .count()
        });
        assert_eq!(appended, 1, "exactly one racing append inserts the row");
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        assert_eq!(store.engineering_outcome_get("run-race").unwrap(), Some(r));
    }

    #[test]
    fn settlement_lifecycle_is_append_only_monotone_and_replay_safe() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        store
            .engineering_outcome_append(&row("run-s", 5), &[], &settlement("run-s", 0, "pending"))
            .unwrap();
        // A byte-identical replay at the same clock is a Duplicate no-op.
        assert_eq!(
            store
                .economic_settlement_append(&settlement("run-s", 0, "pending"))
                .unwrap(),
            EconomicSettlementAppend::Duplicate
        );
        // A semantically identical replay at a NEW clock is STILL a Duplicate
        // no-op: `seq` and `settled_at_ms` are not part of the settlement
        // fact, so a later derivation of the same fact never appends.
        let mut replayed_later = settlement("run-s", 0, "pending");
        replayed_later.settled_at_ms += 60_000;
        assert_eq!(
            store.economic_settlement_append(&replayed_later).unwrap(),
            EconomicSettlementAppend::Duplicate
        );
        // pending -> pending with a DIFFERENT fact (a changed reason) is
        // refused typed: a non-final state never rewrites itself.
        let mut drift = settlement("run-s", 0, "pending");
        drift.reason = "the counts changed".into();
        let err = store.economic_settlement_append(&drift).unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
        // pending -> complete appends seq 1.
        let complete = settlement("run-s", 0, "complete");
        assert_eq!(
            store.economic_settlement_append(&complete).unwrap(),
            EconomicSettlementAppend::Appended(1)
        );
        // complete -> complete correction appends seq 2.
        let mut corrected = complete.clone();
        corrected.rework_spend_micro = Some(250);
        assert_eq!(
            store.economic_settlement_append(&corrected).unwrap(),
            EconomicSettlementAppend::Appended(2)
        );
        // A settlement_at_ms-only replay of the correction is a Duplicate.
        let mut corrected_later = corrected.clone();
        corrected_later.settled_at_ms += 60_000;
        assert_eq!(
            store.economic_settlement_append(&corrected_later).unwrap(),
            EconomicSettlementAppend::Duplicate
        );
        // A CORRECTION that only moves the reason is a legal complete ->
        // complete append (the documented correction path).
        let mut re_reasoned = corrected.clone();
        re_reasoned.reason = "corrected after reconciliation".into();
        assert_eq!(
            store.economic_settlement_append(&re_reasoned).unwrap(),
            EconomicSettlementAppend::Appended(3)
        );
        // complete -> pending is refused typed.
        let mut regress = complete.clone();
        regress.state = "pending".into();
        regress.model_spend_micro = None;
        regress.verification_spend_micro = None;
        regress.rework_spend_micro = None;
        regress.steps_json = "[]".into();
        let err = store.economic_settlement_append(&regress).unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
        // The latest projection is the correction; the history is intact.
        let latest = store.economic_settlement_latest("run-s").unwrap().unwrap();
        assert_eq!(latest.seq, 3);
        assert_eq!(latest.rework_spend_micro, Some(250));
        assert_eq!(latest.reason, "corrected after reconciliation");
    }

    /// The store is the monotonicity authority for `complete -> complete`
    /// corrections: a downward repair of any measured amount (or a
    /// re-unmeasured counterfactual) is refused typed, while an upward
    /// repair lands. The monotone finalize never derives a downward
    /// correction, but the raw store API must not accept one either.
    #[test]
    fn complete_to_complete_corrections_repair_measured_amounts_upward_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let complete = settlement("run-corr", 0, "complete");
        store
            .engineering_outcome_append(&row("run-corr", 5), &[], &complete)
            .unwrap();
        // Upward repair: model spend 1000 -> 1200 (its step projection
        // moves with it) and rework 0 -> 100.
        let mut upward = complete.clone();
        upward.model_spend_micro = Some(1_200);
        upward.rework_spend_micro = Some(100);
        upward.steps_json = steps_json(&[("p", "m", false, 1_200)]);
        assert_eq!(
            store.economic_settlement_append(&upward).unwrap(),
            EconomicSettlementAppend::Appended(1)
        );
        // A downward repair of the model spend is refused typed.
        let mut downward = upward.clone();
        downward.model_spend_micro = Some(500);
        downward.rework_spend_micro = Some(0);
        downward.steps_json = steps_json(&[("p", "m", false, 500)]);
        let err = store.economic_settlement_append(&downward).unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
        // A downward repair of ANY other measured amount is refused too.
        for field in ["verification", "rework"] {
            let mut per_field = upward.clone();
            match field {
                "verification" => per_field.verification_spend_micro = Some(100),
                _ => per_field.rework_spend_micro = Some(1),
            }
            let err = store.economic_settlement_append(&per_field).unwrap_err();
            assert!(
                matches!(err, StoreError::Conflict(_)),
                "{field}: got {err:?}"
            );
        }
        // A measured avoided-hosted counterfactual lands…
        let mut measured = upward.clone();
        measured.avoided_hosted_micro = Some(5);
        measured.avoided_hosted_measured = true;
        assert_eq!(
            store.economic_settlement_append(&measured).unwrap(),
            EconomicSettlementAppend::Appended(2)
        );
        // …and can never become unmeasured (the counterfactual only improves).
        let mut unmeasured = measured.clone();
        unmeasured.avoided_hosted_micro = None;
        unmeasured.avoided_hosted_measured = false;
        let err = store.economic_settlement_append(&unmeasured).unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
        // An equal-amounts correction (metadata only) still lands.
        let mut re_reasoned = measured.clone();
        re_reasoned.reason = "corrected after reconciliation".into();
        assert_eq!(
            store.economic_settlement_append(&re_reasoned).unwrap(),
            EconomicSettlementAppend::Appended(3)
        );
        // The refused downward corrections wrote NOTHING: the latest event
        // is the last accepted correction.
        let latest = store
            .economic_settlement_latest("run-corr")
            .unwrap()
            .unwrap();
        assert_eq!(latest.seq, 3);
        assert_eq!(latest.model_spend_micro, Some(1_200));
        assert_eq!(latest.avoided_hosted_micro, Some(5));
    }

    #[test]
    fn unavailable_may_recover_to_complete_or_pending_but_never_re_unmeasures() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        store
            .engineering_outcome_append(
                &row("run-u", 5),
                &[],
                &settlement("run-u", 0, "unavailable"),
            )
            .unwrap();
        // An identical replay at a new clock is a Duplicate no-op.
        let mut replay = settlement("run-u", 0, "unavailable");
        replay.settled_at_ms += 60_000;
        assert_eq!(
            store.economic_settlement_append(&replay).unwrap(),
            EconomicSettlementAppend::Duplicate
        );
        // A DIFFERENT unavailable fact is refused typed: the stream never
        // re-types itself unavailable.
        let mut still = settlement("run-u", 0, "unavailable");
        still.reason = "still unreadable".into();
        let err = store.economic_settlement_append(&still).unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
        // A RECOVERED read may legally improve unavailable -> pending (the
        // ledger became readable and reservations are open).
        let mut pending = settlement("run-u", 0, "pending");
        pending.reason = "read recovered: 1 open reservation".into();
        assert_eq!(
            store.economic_settlement_append(&pending).unwrap(),
            EconomicSettlementAppend::Appended(1)
        );
        // ... and the recovered stream settles to complete.
        assert_eq!(
            store
                .economic_settlement_append(&settlement("run-u", 0, "complete"))
                .unwrap(),
            EconomicSettlementAppend::Appended(2)
        );
        // After complete, the unmeasured state can never return.
        let mut regress = settlement("run-u", 0, "unavailable");
        regress.reason = "unread again".into();
        let err = store.economic_settlement_append(&regress).unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
    }

    #[test]
    fn schema_rebuild_replays_over_a_nonempty_engineering_outcome_table() {
        // The engineering-outcome table was REBUILT by the migration whose
        // schema target is 34 (dropping the four money columns); the rebuild
        // inserts from the predecessor table into a fresh one and renames.
        // This pins that the rebuild REPLAYED over a NON-EMPTY table
        // preserves every row, that a replayed open at head loses nothing,
        // and that the money columns really are gone.
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path(), true).unwrap();
            let r = row("run-rebuild", 77);
            assert_eq!(
                store
                    .engineering_outcome_append(&r, &[], &settlement("run-rebuild", 0, "complete"))
                    .unwrap(),
                EngineeringOutcomeAppend::Appended
            );
        }
        // Downgrade the file to the state just BEFORE the rebuild (schema
        // target 33): the pre-rebuild table WITH its money columns, and the
        // pre-v34 reservation table without the execution-class column.
        let db = dir.path().join("faktor-plus.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            crate::drop_verification_scope_for_rewind(&conn);
            conn.execute_batch(
                "ALTER TABLE engineering_outcome RENAME TO engineering_outcome_head;
                 CREATE TABLE engineering_outcome (
                     run_id TEXT PRIMARY KEY,
                     session_id INTEGER NOT NULL CHECK (session_id >= 0),
                     task_id INTEGER NOT NULL CHECK (task_id >= 0),
                     payload TEXT NOT NULL,
                     payload_bytes INTEGER NOT NULL CHECK (payload_bytes >= 0),
                     completed_at_ms INTEGER NOT NULL,
                     verified INTEGER NOT NULL CHECK (verified IN (0, 1)),
                     verified_first_pass INTEGER NOT NULL CHECK (verified_first_pass IN (0, 1)),
                     integrated INTEGER NOT NULL CHECK (integrated IN (0, 1)),
                     local_used INTEGER NOT NULL CHECK (local_used IN (0, 1)),
                     frontier_used INTEGER NOT NULL CHECK (frontier_used IN (0, 1)),
                     model_spend_micro INTEGER NOT NULL CHECK (model_spend_micro >= 0),
                     verification_spend_micro INTEGER NOT NULL
                         CHECK (verification_spend_micro >= 0),
                     rework_spend_micro INTEGER NOT NULL CHECK (rework_spend_micro >= 0),
                     avoided_hosted_micro INTEGER NOT NULL
                         CHECK (avoided_hosted_micro >= 0),
                     wall_time_ms INTEGER NOT NULL CHECK (wall_time_ms >= 0),
                     human_interventions INTEGER NOT NULL CHECK (human_interventions >= 0),
                     rollback_events INTEGER NOT NULL CHECK (rollback_events >= 0),
                     recovery_events INTEGER NOT NULL CHECK (recovery_events >= 0),
                     repair_attempts INTEGER NOT NULL CHECK (repair_attempts >= 0),
                     risk_class TEXT NOT NULL,
                     changed_files INTEGER NOT NULL CHECK (changed_files >= 0),
                     changed_lines INTEGER NOT NULL CHECK (changed_lines >= 0),
                     checks_passed INTEGER NOT NULL CHECK (checks_passed >= 0),
                     checks_required INTEGER NOT NULL CHECK (checks_required >= 0),
                     CHECK (verified_first_pass <= verified),
                     CHECK (checks_passed <= checks_required)
                 );
                 INSERT INTO engineering_outcome (run_id, session_id, task_id, payload,
                     payload_bytes, completed_at_ms, verified, verified_first_pass, integrated,
                     local_used, frontier_used, wall_time_ms, human_interventions, rollback_events,
                     recovery_events, repair_attempts, risk_class, changed_files, changed_lines,
                     checks_passed, checks_required, model_spend_micro, verification_spend_micro,
                     rework_spend_micro, avoided_hosted_micro)
                     SELECT run_id, session_id, task_id, payload, payload_bytes, completed_at_ms,
                     verified, verified_first_pass, integrated, local_used, frontier_used,
                     wall_time_ms, human_interventions, rollback_events, recovery_events,
                     repair_attempts, risk_class, changed_files, changed_lines, checks_passed,
                     checks_required, 0, 0, 0, 0
                     FROM engineering_outcome_head;
                 DROP TABLE engineering_outcome_head;
                 ALTER TABLE cost_reservation DROP COLUMN execution_class;
                 PRAGMA user_version = 33;",
            )
            .unwrap();
        }
        // Reopen: the rebuild replays over the NON-EMPTY predecessor table.
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        assert_eq!(
            store
                .engineering_outcome_get("run-rebuild")
                .unwrap()
                .unwrap()
                .run_id,
            "run-rebuild"
        );
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            let money_columns: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('engineering_outcome') \
                     WHERE name IN ('model_spend_micro', 'verification_spend_micro', \
                     'rework_spend_micro', 'avoided_hosted_micro')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(money_columns, 0, "the rebuild must drop every money column");
            assert_eq!(
                conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                crate::schema::SCHEMA_HEAD
            );
        }
        // A REPLAYED open (already at head, row still present) is a no-op
        // that loses nothing.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        assert_eq!(
            store
                .engineering_outcome_get("run-rebuild")
                .unwrap()
                .unwrap()
                .run_id,
            "run-rebuild"
        );
        // The settlement event written atomically with the row survived too.
        assert_eq!(
            store
                .economic_settlement_latest("run-rebuild")
                .unwrap()
                .unwrap()
                .state,
            "complete"
        );
    }

    #[test]
    fn settlement_rows_reject_fabricated_zeroes_and_hostile_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        // A pending event carrying amounts (a fabricated zero) is refused.
        let mut hostile = settlement("run-x", 0, "pending");
        hostile.model_spend_micro = Some(0);
        assert!(matches!(
            store.economic_settlement_append(&hostile).unwrap_err(),
            StoreError::Malformed(_)
        ));
        // A measured flag without a value is refused.
        let mut hostile = settlement("run-x", 0, "complete");
        hostile.avoided_hosted_measured = true;
        assert!(matches!(
            store.economic_settlement_append(&hostile).unwrap_err(),
            StoreError::Malformed(_)
        ));
        // A step projection disagreeing with the model spend is refused.
        let mut hostile = settlement("run-x", 0, "complete");
        hostile.model_spend_micro = Some(999);
        assert!(matches!(
            store.economic_settlement_append(&hostile).unwrap_err(),
            StoreError::Malformed(_)
        ));
        // Unknown state, negative amount and oversized reason are refused.
        let mut hostile = settlement("run-x", 0, "complete");
        hostile.state = "final".into();
        assert!(matches!(
            store.economic_settlement_append(&hostile).unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut hostile = settlement("run-x", 0, "complete");
        hostile.avoided_hosted_measured = true;
        hostile.avoided_hosted_micro = Some(-1);
        assert!(matches!(
            store.economic_settlement_append(&hostile).unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut hostile = settlement("run-x", 0, "complete");
        hostile.reason = "x".repeat(MAX_ECONOMIC_SETTLEMENT_REASON + 1);
        assert!(matches!(
            store.economic_settlement_append(&hostile).unwrap_err(),
            StoreError::Malformed(_)
        ));
        assert!(store.economic_settlement_latest("run-x").unwrap().is_none());
    }

    #[test]
    fn hostile_rows_are_refused_before_any_write_and_corrupt_rows_read_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let s = settlement("run-x", 0, "complete");
        // Refusals happen on the caller's thread (no row is written).
        let empty_id = row("", 1);
        assert!(matches!(
            store
                .engineering_outcome_append(&empty_id, &[], &s)
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let non_ascii = row("run-é", 1);
        assert!(matches!(
            store
                .engineering_outcome_append(&non_ascii, &[], &s)
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut inconsistent = row("run-x", 1);
        inconsistent.checks_passed = 4;
        inconsistent.checks_required = 3;
        assert!(matches!(
            store
                .engineering_outcome_append(&inconsistent, &[], &s)
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut first_pass_unverified = row("run-x", 1);
        first_pass_unverified.verified = false;
        assert!(matches!(
            store
                .engineering_outcome_append(&first_pass_unverified, &[], &s)
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut oversized = row("run-x", 1);
        oversized.payload = format!("{{\"pad\":\"{}\"}}", "x".repeat(64 * 1024));
        oversized.payload_bytes = oversized.payload.len() as i64;
        assert!(matches!(
            store
                .engineering_outcome_append(&oversized, &[], &s)
                .unwrap_err(),
            StoreError::Oversized(_)
        ));
        let mut not_json = row("run-x", 1);
        not_json.payload = "not-json".into();
        not_json.payload_bytes = not_json.payload.len() as i64;
        assert!(matches!(
            store
                .engineering_outcome_append(&not_json, &[], &s)
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        // A settlement whose run id disagrees with the row is refused.
        assert!(matches!(
            store
                .engineering_outcome_append(
                    &row("run-x", 1),
                    &[],
                    &settlement("other", 0, "complete")
                )
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        // A non-zero initial seq is refused.
        assert!(matches!(
            store
                .engineering_outcome_append(
                    &row("run-x", 1),
                    &[],
                    &settlement("run-x", 3, "complete")
                )
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        assert_eq!(store.engineering_outcome_count().unwrap(), 0);
        assert_eq!(
            store
                .engineering_outcome_append(
                    &row("run-x", 1),
                    &[],
                    &settlement("run-x", 0, "complete")
                )
                .unwrap(),
            EngineeringOutcomeAppend::Appended
        );
        // A hostile INJECTED row (empty risk tag) reads as Corrupt, never as
        // a fabricated record. (SQL CHECKs already forbid foreign flags and
        // broken cross-column invariants, so a hostile writer cannot even
        // land those; the decoder still refuses what the schema admits.)
        let conn = store.raw_conn();
        conn.execute(
            "INSERT INTO engineering_outcome (run_id, session_id, task_id, payload, payload_bytes,
                completed_at_ms, verified, verified_first_pass, integrated, local_used,
                frontier_used, wall_time_ms, human_interventions, rollback_events,
                recovery_events, repair_attempts, risk_class, changed_files, changed_lines,
                checks_passed, checks_required)
             VALUES ('run-hostile', 1, 1, '{\"x\":1}', 7, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, '', 0,
                     0, 0, 0)",
            [],
        )
        .unwrap();
        let err = store.engineering_outcome_get("run-hostile").unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "got {err:?}");
        // A hostile payload-length drift is corrupt too.
        conn.execute(
            "INSERT INTO engineering_outcome (run_id, session_id, task_id, payload, payload_bytes,
                completed_at_ms, verified, verified_first_pass, integrated, local_used,
                frontier_used, wall_time_ms, human_interventions, rollback_events,
                recovery_events, repair_attempts, risk_class, changed_files, changed_lines,
                checks_passed, checks_required)
             VALUES ('run-drift', 1, 1, '{\"x\":1}', 999, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 'low',
                     0, 0, 0, 0)",
            [],
        )
        .unwrap();
        let err = store.engineering_outcome_get("run-drift").unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "got {err:?}");
        // A hostile settlement row with an UNKNOWN state cannot even land
        // (the schema CHECK forbids states outside the lifecycle)...
        let unknown_state = conn.execute(
            "INSERT INTO economic_settlement (run_id, seq, schema_version, state, reason,
                model_spend_micro, verification_spend_micro, rework_spend_micro,
                avoided_hosted_micro, avoided_hosted_measured, steps_json, settled_at_ms)
             VALUES ('run-x', 9, 1, 'final', 'r', 0, 0, 0, 0, 0, '[]', 0)",
            [],
        );
        assert!(
            unknown_state.is_err(),
            "the schema must refuse unknown states"
        );
        // A pending row carrying amounts is corruption on read.
        conn.execute(
            "INSERT INTO economic_settlement (run_id, seq, schema_version, state, reason,
                model_spend_micro, verification_spend_micro, rework_spend_micro,
                avoided_hosted_micro, avoided_hosted_measured, steps_json, settled_at_ms)
             VALUES ('run-x', 10, 1, 'pending', 'r', 0, 0, 0, 0, 0, '[]', 0)",
            [],
        )
        .unwrap();
        let err = store.economic_settlement_latest("run-x").unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "got {err:?}");
        let err = store
            .economic_settlement_latest_for_runs(&["run-x".to_string()])
            .unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn terminal_run_facts_upsert_freezes_once_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        store
            .terminal_run_facts_upsert("run-f", "{\"schema_version\":1,\"samples\":[]}")
            .unwrap();
        assert_eq!(
            store.terminal_run_facts_get("run-f").unwrap().as_deref(),
            Some("{\"schema_version\":1,\"samples\":[]}")
        );
        // A later upsert before the record wins (the completing turn's facts).
        store
            .terminal_run_facts_upsert("run-f", "{\"schema_version\":1,\"samples\":[1]}")
            .unwrap();
        assert_eq!(
            store.terminal_run_facts_get("run-f").unwrap().as_deref(),
            Some("{\"schema_version\":1,\"samples\":[1]}")
        );
        store
            .engineering_outcome_append(&row("run-f", 1), &[], &settlement("run-f", 0, "complete"))
            .unwrap();
        // Recorded: the facts freeze — a hostile overwrite is refused typed.
        let err = store
            .terminal_run_facts_upsert("run-f", "{\"schema_version\":1,\"samples\":[2]}")
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
        assert_eq!(
            store.terminal_run_facts_get("run-f").unwrap().as_deref(),
            Some("{\"schema_version\":1,\"samples\":[1]}")
        );
        // Hostile payloads are refused before any write; hostile injected
        // rows read as corruption.
        let err = store
            .terminal_run_facts_upsert("run-g", "not-json")
            .unwrap_err();
        assert!(matches!(err, StoreError::Malformed(_)), "got {err:?}");
        assert!(store.terminal_run_facts_get("run-g").unwrap().is_none());
        let conn = store.raw_conn();
        conn.execute(
            "INSERT INTO terminal_run_facts (run_id, payload, payload_bytes, updated_ms) \
             VALUES ('run-h', 'not-json', 8, 0)",
            [],
        )
        .unwrap();
        let err = store.terminal_run_facts_get("run-h").unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn page_cursor_is_total_strict_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        for (id, at) in [("a", 10i64), ("b", 20), ("c", 20), ("d", 30)] {
            assert_eq!(
                store
                    .engineering_outcome_append(&row(id, at), &[], &settlement(id, 0, "complete"))
                    .unwrap(),
                EngineeringOutcomeAppend::Appended
            );
        }
        let first = store.engineering_outcome_page(None, 2).unwrap();
        let first_ids: Vec<&str> = first.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(first_ids, vec!["d", "c"], "newest first, tie broken by id");
        let last = first.last().unwrap();
        let second = store
            .engineering_outcome_page(Some((last.completed_at_ms, &last.run_id)), 2)
            .unwrap();
        let second_ids: Vec<&str> = second.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(second_ids, vec!["b", "a"]);
        // The cursor is strict: repeating it with a larger limit still
        // skips exactly the rows already seen.
        let third = store
            .engineering_outcome_page(Some((last.completed_at_ms, &last.run_id)), 500)
            .unwrap();
        assert!(third.iter().all(|r| r.run_id != "d" && r.run_id != "c"));
        // A zero limit clamps to one (bounded), an oversized one to the cap.
        assert_eq!(store.engineering_outcome_page(None, 0).unwrap().len(), 1);
        assert_eq!(
            store
                .engineering_outcome_page(None, u64::MAX)
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn crash_before_commit_leaves_no_record_and_crash_after_commit_replays_as_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        // --- crash at the precommit boundary: record, settlement event AND
        // router feed roll back together.
        {
            let store = Store::open(dir.path(), true).unwrap();
            store.crash_arm(CrashArm {
                point: "engineering_outcome_precommit",
                ordinal: 0,
            });
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                store.engineering_outcome_append(
                    &row("run-crash", 7),
                    &[sample("p", "m", true)],
                    &settlement("run-crash", 0, "complete"),
                )
            }));
            assert!(
                store.seam_crash_observed(&caught),
                "the armed precommit boundary must have fired"
            );
        }
        {
            let store = Store::open(dir.path(), true).unwrap();
            assert_eq!(
                store.engineering_outcome_count().unwrap(),
                0,
                "the uncommitted record rolled back with its transaction"
            );
            assert!(store
                .economic_settlement_latest("run-crash")
                .unwrap()
                .is_none());
            assert!(
                store
                    .model_outcome_stats_get(
                        "p",
                        "m",
                        RouterPhase::Implement,
                        TaskClass::Medium,
                        RiskBucket::Low
                    )
                    .unwrap()
                    .is_none(),
                "the router feed rolled back with the record"
            );
            // Replay after the crash: exactly one record, exactly one sample.
            assert_eq!(
                store
                    .engineering_outcome_append(
                        &row("run-crash", 7),
                        &[sample("p", "m", true)],
                        &settlement("run-crash", 0, "complete"),
                    )
                    .unwrap(),
                EngineeringOutcomeAppend::Appended
            );
            assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        }
        // --- crash at the committed boundary: the record is durable and the
        // replay is a no-op (never a second record or sample).
        {
            let store = Store::open(dir.path(), true).unwrap();
            store.crash_arm(CrashArm {
                point: "engineering_outcome_committed",
                ordinal: 0,
            });
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                store.engineering_outcome_append(
                    &row("run-post", 8),
                    &[sample("p", "m", true)],
                    &settlement("run-post", 0, "complete"),
                )
            }));
            assert!(
                store.seam_crash_observed(&caught),
                "the armed committed boundary must have fired"
            );
        }
        {
            let store = Store::open(dir.path(), true).unwrap();
            assert_eq!(
                store.engineering_outcome_count().unwrap(),
                2,
                "the committed record survived the crash"
            );
            assert_eq!(
                store
                    .engineering_outcome_append(
                        &row("run-post", 8),
                        &[sample("p", "m", true)],
                        &settlement("run-post", 0, "complete"),
                    )
                    .unwrap(),
                EngineeringOutcomeAppend::Duplicate
            );
            assert_eq!(store.engineering_outcome_count().unwrap(), 2);
            let stats = store
                .model_outcome_stats_get(
                    "p",
                    "m",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                stats.sample_count, 2,
                "one sample per run, never doubled across crash/replay"
            );
        }
    }

    #[test]
    fn settlement_crash_seams_leave_the_stream_consistent() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path(), true).unwrap();
            store
                .engineering_outcome_append(
                    &row("run-s", 9),
                    &[],
                    &settlement("run-s", 0, "pending"),
                )
                .unwrap();
            store.crash_arm(CrashArm {
                point: "economic_settlement_precommit",
                ordinal: 0,
            });
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                store.economic_settlement_append(&settlement("run-s", 0, "complete"))
            }));
            assert!(
                store.seam_crash_observed(&caught),
                "the armed settlement seam must fire"
            );
        }
        {
            let store = Store::open(dir.path(), true).unwrap();
            assert_eq!(
                store
                    .economic_settlement_latest("run-s")
                    .unwrap()
                    .unwrap()
                    .state,
                "pending",
                "the uncommitted correction rolled back"
            );
            assert_eq!(
                store
                    .economic_settlement_append(&settlement("run-s", 0, "complete"))
                    .unwrap(),
                EconomicSettlementAppend::Appended(1)
            );
        }
    }

    #[test]
    fn ledger_append_feeds_verified_samples_atomically_and_failure_samples_count_rework() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let samples = vec![
            sample("cheap", "m1", true),
            ModelOutcomeIncrement {
                provider: "strong".into(),
                model: "m2".into(),
                phase: RouterPhase::Review,
                task_class: TaskClass::Hard,
                risk_bucket: RiskBucket::High,
                sample: ModelOutcomeSample {
                    verified_success: false,
                    rework_cost_micro: 2_400_000,
                    rework_turns: 3,
                },
            },
        ];
        store
            .engineering_outcome_append(
                &row("run-feed", 1),
                &samples,
                &settlement("run-feed", 0, "complete"),
            )
            .unwrap();
        let cheap = store
            .model_outcome_stats_get(
                "cheap",
                "m1",
                RouterPhase::Implement,
                TaskClass::Medium,
                RiskBucket::Low,
            )
            .unwrap()
            .unwrap();
        assert_eq!((cheap.sample_count, cheap.successes_first_pass), (1, 1));
        let strong = store
            .model_outcome_stats_get(
                "strong",
                "m2",
                RouterPhase::Review,
                TaskClass::Hard,
                RiskBucket::High,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                strong.sample_count,
                strong.failures_first_pass,
                strong.rework_cost_micro_sum,
                strong.rework_turns_sum
            ),
            (1, 1, 2_400_000, 3)
        );
    }

    /// The metrics snapshot is ONE consistent transaction: every selected
    /// engineering row carries its LATEST settlement event, the bound is
    /// explicit, and the outcome revision advances exactly on writes.
    #[test]
    fn metrics_snapshot_is_one_consistent_bounded_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.outcome_revision(), 0);
        for (i, run) in ["run-a", "run-b", "run-c"].iter().enumerate() {
            store
                .engineering_outcome_append(
                    &row(run, 1_000 + i as i64),
                    &[],
                    &settlement(run, 0, "pending"),
                )
                .unwrap();
        }
        assert_eq!(store.outcome_revision(), 3);

        // A correction lands on run-b; the snapshot must carry seq 1.
        store
            .economic_settlement_append(&settlement("run-b", 0, "complete"))
            .unwrap();
        assert_eq!(store.outcome_revision(), 4);

        let snap = store.engineering_outcome_metrics_snapshot(10).unwrap();
        assert!(!snap.truncated);
        assert_eq!(snap.outcomes.len(), 3);
        assert_eq!(snap.settlements.len(), 3);
        let mut by_run: std::collections::BTreeMap<&str, &EconomicSettlementRow> = snap
            .settlements
            .iter()
            .map(|s| (s.run_id.as_str(), s))
            .collect();
        assert_eq!(by_run.remove("run-b").unwrap().state, "complete");
        assert_eq!(by_run.remove("run-c").unwrap().state, "pending");
        assert_eq!(
            snap.outcomes.first().map(|r| r.run_id.as_str()),
            Some("run-c"),
            "newest completed_at_ms first"
        );

        let bounded = store.engineering_outcome_metrics_snapshot(2).unwrap();
        assert!(bounded.truncated, "the bound is named, never silent");
        assert_eq!(bounded.outcomes.len(), 2);
        assert_eq!(bounded.settlements.len(), 2);
    }
}
