//! Engineering Outcome Ledger persistence (economic audit item 7).
//!
//! The append-only durable record of COMPLETED runs: one `engineering_outcome`
//! row per run identity (schema migration v32), exactly-once by construction
//! and atomic with the router feed. Everything that can be validated or
//! decoded before the writer job is done on the caller's thread (audit item
//! 8: writer jobs execute SQL only); the bounded payload schema itself is
//! owned and validated loudly by `faktor-agent`'s `outcome_ledger` module.
//!
//! Exactly-once contract
//! ---------------------
//! [`Store::engineering_outcome_append`] runs ONE immediate transaction:
//!
//! 1. an existing row under the same `run_id` that is byte-identical to the
//!    submitted row returns [`EngineeringOutcomeAppend::Duplicate`] and writes
//!    nothing (a crash/replay after commit is a no-op);
//! 2. an existing row with a DIFFERENT value is a typed
//!    [`StoreError::Conflict`] — the append-only identity is never
//!    overwritten;
//! 3. a NEW row is inserted and every verified sample of `router_samples` is
//!    absorbed into `model_outcome_stats` in the SAME transaction, so a
//!    crash can neither lose the ledger row nor double-count a router sample
//!    on the next replay attempt.
//!
//! Reads validate every column fallibly (a hostile or truncated row is a
//! typed [`StoreError::Corrupt`], never a fabricated record or metric).

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

/// The exact column list of one `engineering_outcome` row (shared by every
/// read path so the decoder never drifts from the schema).
const OUTCOME_ROW_COLUMNS: &str = "run_id, session_id, task_id, payload, payload_bytes, \
     completed_at_ms, verified, verified_first_pass, integrated, local_used, frontier_used, \
     model_spend_micro, verification_spend_micro, rework_spend_micro, avoided_hosted_micro, \
     wall_time_ms, human_interventions, rollback_events, recovery_events, repair_attempts, \
     risk_class, changed_files, changed_lines, checks_passed, checks_required";

/// One Engineering Outcome Ledger row exactly as stored: the run identity,
/// scope, the bounded record payload and the metric projection columns the
/// aggregates fold without re-parsing the payload. Booleans are explicit;
/// every money/counter column is an exact non-negative integer (SQLite
/// `INTEGER` is signed 64-bit, so values above `i64::MAX` are refused typed
/// before any write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineeringOutcomeRow {
    pub run_id: String,
    pub session_id: i64,
    pub task_id: i64,
    /// The bounded strict record payload (owned by the agent module).
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
    pub model_spend_micro: i64,
    pub verification_spend_micro: i64,
    pub rework_spend_micro: i64,
    pub avoided_hosted_micro: i64,
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
            ("model_spend_micro", self.model_spend_micro),
            ("verification_spend_micro", self.verification_spend_micro),
            ("rework_spend_micro", self.rework_spend_micro),
            ("avoided_hosted_micro", self.avoided_hosted_micro),
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
    /// A new row was inserted (and every verified sample absorbed into
    /// `model_outcome_stats`) in one transaction.
    Appended,
    /// The run id already held a byte-identical row: the append is a
    /// crash/replay no-op — no second record, no second sample.
    Duplicate,
}

/// The raw (undecoded) column values of one row, bound inside a writer job
/// (get-only SQL; validation/decoding happens on the caller's thread).
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
    model_spend_micro: i64,
    verification_spend_micro: i64,
    rework_spend_micro: i64,
    avoided_hosted_micro: i64,
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

/// Bind one raw row (format `OUTCOME_ROW_COLUMNS`). Plain `get` binds only —
/// no decoding, no validation, no serialization.
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
        model_spend_micro: r.get(11)?,
        verification_spend_micro: r.get(12)?,
        rework_spend_micro: r.get(13)?,
        avoided_hosted_micro: r.get(14)?,
        wall_time_ms: r.get(15)?,
        human_interventions: r.get(16)?,
        rollback_events: r.get(17)?,
        recovery_events: r.get(18)?,
        repair_attempts: r.get(19)?,
        risk_class: r.get(20)?,
        changed_files: r.get(21)?,
        changed_lines: r.get(22)?,
        checks_passed: r.get(23)?,
        checks_required: r.get(24)?,
    })
}

/// Decode one raw row fallibly. A hostile flag/counter/invariant or a
/// payload that changed under a foreign write is a typed
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
        model_spend_micro: counter(raw.model_spend_micro, "model_spend_micro")?,
        verification_spend_micro: counter(
            raw.verification_spend_micro,
            "verification_spend_micro",
        )?,
        rework_spend_micro: counter(raw.rework_spend_micro, "rework_spend_micro")?,
        avoided_hosted_micro: counter(raw.avoided_hosted_micro, "avoided_hosted_micro")?,
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

/// What one writer job actually did: a fresh insert, or the raw existing row
/// (compared and classified by the caller AFTER the job — the job itself
/// writes nothing on that path). The raw row is boxed so the enum stays
/// small.
enum RawAppendOutcome {
    Appended,
    Existing(Box<RawEngineeringOutcomeRow>),
}

impl Store {
    /// Append ONE completed run's ledger row, exactly once, and atomically
    /// absorb the record's verified samples into the router's durable
    /// `model_outcome_stats` registry. See the module docs for the
    /// exactly-once contract; a conflicting re-append is a typed
    /// [`StoreError::Conflict`], never an overwrite.
    pub fn engineering_outcome_append(
        &self,
        row: &EngineeringOutcomeRow,
        router_samples: &[ModelOutcomeIncrement],
    ) -> StoreResult<EngineeringOutcomeAppend> {
        row.validate()?;
        // Preparation BEFORE enqueueing (audit item 8): every string —
        // SQL text, enum-dimension encoding, the conflict message — is
        // materialized on the caller's thread; the writer job executes SQL
        // only.
        let row = row.clone();
        let submitted = row.clone();
        let select_sql =
            format!("SELECT {OUTCOME_ROW_COLUMNS} FROM engineering_outcome WHERE run_id = ?1");
        let insert_sql = format!(
            "INSERT INTO engineering_outcome ({OUTCOME_ROW_COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, \
              ?19, ?20, ?21, ?22, ?23, ?24, ?25)"
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
                        row.model_spend_micro,
                        row.verification_spend_micro,
                        row.rework_spend_micro,
                        row.avoided_hosted_micro,
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
                // transaction open — the record AND the router feed roll
                // back together; `engineering_outcome_committed` fires with
                // both durable, so a replay after the crash is a Duplicate.
                seam.trip("engineering_outcome_precommit");
                tx.commit()?;
                seam.trip("engineering_outcome_committed");
                Ok(RawAppendOutcome::Appended)
            })?;
        match outcome {
            RawAppendOutcome::Appended => Ok(EngineeringOutcomeAppend::Appended),
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

    /// The exact durable row of one run, or `None` when no completed run
    /// with that identity was ever recorded. A present-but-hostile row is a
    /// typed [`StoreError::Corrupt`].
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

    /// One bounded newest-first page of ledger rows, strictly OLDER than the
    /// `(completed_at_ms, run_id)` cursor when one is given (the composite
    /// key makes the order total: no two rows can be skipped or repeated
    /// between calls). `limit` is clamped to
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

    /// Exact number of durable ledger rows (doctor/analytics surfaces).
    pub fn engineering_outcome_count(&self) -> StoreResult<u64> {
        let conn = self.read()?;
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM engineering_outcome", [], |r| r.get(0))?;
        u64::try_from(n).map_err(|_| {
            StoreError::Corrupt(vec![format!("engineering_outcome count {n} is negative")])
        })
    }
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
        format!(r#"{{"schema_version":1,"run_id":"{run_id}"}}"#)
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
            model_spend_micro: 1_000,
            verification_spend_micro: 250,
            rework_spend_micro: 0,
            avoided_hosted_micro: 0,
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
        assert_eq!(
            store
                .engineering_outcome_append(&r, &[sample("p", "m", true)])
                .unwrap(),
            EngineeringOutcomeAppend::Appended
        );
        assert_eq!(
            store.engineering_outcome_get("run-1").unwrap(),
            Some(r.clone())
        );
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        // Replay (identical row): Duplicate, still ONE row, and the router
        // sample absorbed exactly once.
        assert_eq!(
            store
                .engineering_outcome_append(&r, &[sample("p", "m", true)])
                .unwrap(),
            EngineeringOutcomeAppend::Duplicate
        );
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
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
            store.engineering_outcome_append(&original, &[]).unwrap(),
            EngineeringOutcomeAppend::Appended
        );
        let mut hostile = row("run-1", 100);
        hostile.model_spend_micro = 1;
        let err = store
            .engineering_outcome_append(&hostile, &[sample("p", "m", true)])
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
    }

    #[test]
    fn race_writers_never_double_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let r = row("run-race", 42);
        let appended = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let store = &store;
                let r = &r;
                handles
                    .push(scope.spawn(move || store.engineering_outcome_append(r, &[]).unwrap()));
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
    fn hostile_rows_are_refused_before_any_write_and_corrupt_rows_read_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        // Refusals happen on the caller's thread (no row is written).
        let empty_id = row("", 1);
        assert!(matches!(
            store
                .engineering_outcome_append(&empty_id, &[])
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let non_ascii = row("run-é", 1);
        assert!(matches!(
            store
                .engineering_outcome_append(&non_ascii, &[])
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut inconsistent = row("run-x", 1);
        inconsistent.checks_passed = 4;
        inconsistent.checks_required = 3;
        assert!(matches!(
            store
                .engineering_outcome_append(&inconsistent, &[])
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut first_pass_unverified = row("run-x", 1);
        first_pass_unverified.verified = false;
        assert!(matches!(
            store
                .engineering_outcome_append(&first_pass_unverified, &[])
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut oversized = row("run-x", 1);
        oversized.payload = format!("{{\"pad\":\"{}\"}}", "x".repeat(64 * 1024));
        oversized.payload_bytes = oversized.payload.len() as i64;
        assert!(matches!(
            store
                .engineering_outcome_append(&oversized, &[])
                .unwrap_err(),
            StoreError::Oversized(_)
        ));
        let mut not_json = row("run-x", 1);
        not_json.payload = "not-json".into();
        not_json.payload_bytes = not_json.payload.len() as i64;
        assert!(matches!(
            store
                .engineering_outcome_append(&not_json, &[])
                .unwrap_err(),
            StoreError::Malformed(_)
        ));
        let mut above_i64 = row("run-x", 1);
        above_i64.model_spend_micro = i64::MAX; // fits
        assert_eq!(
            store.engineering_outcome_append(&above_i64, &[]).unwrap(),
            EngineeringOutcomeAppend::Appended
        );
        let mut negative = row("run-y", 1);
        negative.model_spend_micro = -1;
        let err = store.engineering_outcome_append(&negative, &[]);
        assert!(matches!(err, Err(StoreError::Malformed(_))), "got {err:?}");
        assert_eq!(store.engineering_outcome_count().unwrap(), 1);
        // A hostile INJECTED row (empty risk tag) reads as Corrupt, never as
        // a fabricated record. (SQL CHECKs already forbid foreign flags and
        // broken cross-column invariants, so a hostile writer cannot even
        // land those; the decoder still refuses what the schema admits.)
        let conn = store.raw_conn();
        conn.execute(
            "INSERT INTO engineering_outcome (run_id, session_id, task_id, payload, payload_bytes,
                completed_at_ms, verified, verified_first_pass, integrated, local_used,
                frontier_used, model_spend_micro, verification_spend_micro, rework_spend_micro,
                avoided_hosted_micro, wall_time_ms, human_interventions, rollback_events,
                recovery_events, repair_attempts, risk_class, changed_files, changed_lines,
                checks_passed, checks_required)
             VALUES ('run-hostile', 1, 1, '{\"x\":1}', 7, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                     0, 0, '', 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        let err = store.engineering_outcome_get("run-hostile").unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "got {err:?}");
        // A hostile payload-length drift is corrupt too.
        conn.execute(
            "INSERT INTO engineering_outcome (run_id, session_id, task_id, payload, payload_bytes,
                completed_at_ms, verified, verified_first_pass, integrated, local_used,
                frontier_used, model_spend_micro, verification_spend_micro, rework_spend_micro,
                avoided_hosted_micro, wall_time_ms, human_interventions, rollback_events,
                recovery_events, repair_attempts, risk_class, changed_files, changed_lines,
                checks_passed, checks_required)
             VALUES ('run-drift', 1, 1, '{\"x\":1}', 999, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                     0, 0, 'low', 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        let err = store.engineering_outcome_get("run-drift").unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "got {err:?}");
        // A hostile non-JSON payload with an honest byte length is corrupt.
        conn.execute(
            "INSERT INTO engineering_outcome (run_id, session_id, task_id, payload, payload_bytes,
                completed_at_ms, verified, verified_first_pass, integrated, local_used,
                frontier_used, model_spend_micro, verification_spend_micro, rework_spend_micro,
                avoided_hosted_micro, wall_time_ms, human_interventions, rollback_events,
                recovery_events, repair_attempts, risk_class, changed_files, changed_lines,
                checks_passed, checks_required)
             VALUES ('run-garbage', 1, 1, 'not-json', 8, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                     0, 0, 'low', 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        let err = store.engineering_outcome_get("run-garbage").unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "got {err:?}");
    }

    #[test]
    fn page_cursor_is_total_strict_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        for (id, at) in [("a", 10i64), ("b", 20), ("c", 20), ("d", 30)] {
            assert_eq!(
                store.engineering_outcome_append(&row(id, at), &[]).unwrap(),
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
        // --- crash at the precommit boundary: record AND router feed roll
        // back together.
        {
            let store = Store::open(dir.path(), true).unwrap();
            store.crash_arm(CrashArm {
                point: "engineering_outcome_precommit",
                ordinal: 0,
            });
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                store.engineering_outcome_append(&row("run-crash", 7), &[sample("p", "m", true)])
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
                    .engineering_outcome_append(&row("run-crash", 7), &[sample("p", "m", true)])
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
                store.engineering_outcome_append(&row("run-post", 8), &[sample("p", "m", true)])
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
                    .engineering_outcome_append(&row("run-post", 8), &[sample("p", "m", true)])
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
            .engineering_outcome_append(&row("run-feed", 1), &samples)
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
}
