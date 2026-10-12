//! v22 legacy verification import: the one-shot, marker-guarded projection of
//! pre-v22 verification `memory_fact` rows into the v22 tables.

use super::*;

// ------------------------------------- legacy verification import (v22)

/// `memory_fact` kind of one pre-v22 verification attempt row (one fact row
/// per attempt, key `va:{task_id}:{op_id}`). The v22 tables replaced it; the
/// import below projects these rows (and their jobs) into the real tables
/// additively — the legacy facts are NEVER deleted.
pub const LEGACY_VERIFICATION_ATTEMPT_FACT_KIND: &str = "verification_attempt";

/// `memory_fact` kind of one pre-v22 verification job row (one fact row per
/// required check, key `vj:{task_id}:{check_id}`).
pub const LEGACY_VERIFICATION_JOB_FACT_KIND: &str = "verification_job";

/// Durable marker fact kind written (same transaction as the imported rows)
/// once one session's legacy rows were imported. Its presence is the
/// exactly-once guard: a re-open never duplicates the import.
pub const VERIFICATION_V22_IMPORT_MARKER_KIND: &str = "verification_v22_import";

/// Marker fact key.
pub const VERIFICATION_V22_IMPORT_MARKER_KEY: &str = "done";

/// One legacy row the import could not project: its fact key plus a typed
/// reason. Recorded in the durable marker fact (and logged) so a corrupt
/// legacy row is skipped LOUDLY, never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyVerificationSkip {
    pub key: String,
    pub reason: String,
}

/// Outcome of one v22 legacy-verification import (or of the marker-guarded
/// no-op on every later open: all counters zero).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LegacyVerificationImport {
    pub imported_attempts: u64,
    pub imported_jobs: u64,
    pub imported_results: u64,
    /// Bounded typed notes for skipped corrupt/undecodable legacy rows.
    pub skipped: Vec<LegacyVerificationSkip>,
    /// Skipped rows beyond the bounded note list (still logged loudly).
    pub skipped_overflow: u64,
}

// ------------------------------------------------- v22 legacy import (repair)

/// Bounded typed notes carried by one import marker (fact values are capped
/// at 4096 bytes, so the marker names at most this many skipped rows; the
/// rest stay logged loudly and counted in `skipped_overflow`).
pub(crate) const MAX_LEGACY_IMPORT_SKIP_NOTES: usize = 12;

/// One skip reason stored in the marker (longer reasons are truncated on a
/// char boundary; the untruncated reason is logged).
pub(crate) const MAX_LEGACY_IMPORT_SKIP_REASON_BYTES: usize = 160;

/// The durable fact-value cap every marker value must honor.
pub(crate) const MAX_LEGACY_IMPORT_MARKER_BYTES: usize = 4096;

/// Legacy row-value schema versions this reader understands (v1 rows lack
/// `environment_fingerprint` and decode with it absent).
pub(crate) const LEGACY_VERIFICATION_SCHEMA_VER: i64 = 2;

pub(crate) const LEGACY_VERIFICATION_JOB_STATES: [&str; 6] = [
    "queued",
    "running",
    "passed",
    "failed",
    "unavailable",
    "cancelled",
];

pub(crate) const LEGACY_VERIFICATION_INLINE_STATES: [&str; 3] = ["passed", "failed", "unavailable"];

/// One pre-v22 job row value: the exact serde shape the deleted legacy
/// reader enforced (including `deny_unknown_fields` — a row from an unknown
/// future schema is a typed skip, never a partial guess).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyJobRowValue {
    pub(crate) schema_ver: i64,
    pub(crate) attempt_op: u64,
    pub(crate) task_id: u64,
    pub(crate) task_revision: u64,
    pub(crate) workspace_root: String,
    pub(crate) check_id: String,
    pub(crate) kind: String,
    pub(crate) command: String,
    pub(crate) spec_json: String,
    pub(crate) budget_ms: u64,
    pub(crate) state: String,
    pub(crate) note: Option<String>,
    pub(crate) op_id: Option<u64>,
    pub(crate) result_json: Option<String>,
    #[serde(default)]
    pub(crate) environment_fingerprint: Option<serde_json::Value>,
    pub(crate) created_ms: i64,
    pub(crate) updated_ms: i64,
    pub(crate) finished_ms: Option<i64>,
}

/// One pre-v22 attempt row value.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyAttemptRowValue {
    pub(crate) schema_ver: i64,
    pub(crate) task_id: u64,
    pub(crate) op_id: u64,
    pub(crate) task_revision: u64,
    pub(crate) workspace_root: String,
    pub(crate) changed: Vec<String>,
    pub(crate) checks: Vec<LegacyCheckRowValue>,
    #[serde(default)]
    pub(crate) environment_fingerprint: Option<serde_json::Value>,
    pub(crate) created_ms: i64,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyCheckRowValue {
    pub(crate) check_id: String,
    pub(crate) command: String,
    pub(crate) inline: Option<String>,
}

/// `va:{task_id}:{op_id}` -> `(task_id, op_id)`.
pub(crate) fn parse_legacy_attempt_key(key: &str) -> Option<(u64, u64)> {
    let rest = key.strip_prefix("va:")?;
    let (task, op) = rest.split_once(':')?;
    Some((task.parse().ok()?, op.parse().ok()?))
}

/// `vj:{task_id}:{check_id}` -> `(task_id, check_id)` (check ids may contain
/// `:`, so only the first two separators are structural).
pub(crate) fn parse_legacy_job_key(key: &str) -> Option<(u64, &str)> {
    let rest = key.strip_prefix("vj:")?;
    let (task, check_id) = rest.split_once(':')?;
    if check_id.is_empty() {
        return None;
    }
    Some((task.parse().ok()?, check_id))
}

pub(crate) fn truncate_legacy_reason(reason: &str) -> String {
    if reason.len() <= MAX_LEGACY_IMPORT_SKIP_REASON_BYTES {
        return reason.to_string();
    }
    let mut end = MAX_LEGACY_IMPORT_SKIP_REASON_BYTES;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &reason[..end])
}

/// Record one skipped legacy row: LOUD tracing plus a bounded typed note on
/// the durable marker. The legacy fact row itself is never touched.
pub(crate) fn push_legacy_import_skip(
    report: &mut LegacyVerificationImport,
    key: &str,
    reason: String,
) {
    tracing::error!(
        fact_key = key,
        reason = %reason,
        "legacy verification fact skipped during the v22 import (row kept; typed note recorded)"
    );
    if report.skipped.len() < MAX_LEGACY_IMPORT_SKIP_NOTES {
        report.skipped.push(LegacyVerificationSkip {
            key: key.to_string(),
            reason: truncate_legacy_reason(&reason),
        });
    } else {
        report.skipped_overflow += 1;
    }
}

/// Derive the v22 `(program, args_json)` identity from a legacy opaque
/// `spec_json` (the legacy layer stored no argv index). The values are an
/// index only — execution still re-parses `spec_json` — so a spec whose
/// argv exceeds the v22 caps degrades to an empty identity instead of
/// refusing the whole attempt.
pub(crate) fn derive_legacy_argv_identity(spec_json: &str, command: &str) -> (String, String) {
    let value: Option<serde_json::Value> = serde_json::from_str(spec_json).ok();
    let program = value
        .as_ref()
        .and_then(|v| v.get("program"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            command
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string()
        });
    let args: Vec<String> = value
        .as_ref()
        .and_then(|v| v.get("args"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let program = if program.len() > MAX_VERIFICATION_JOB_PROGRAM_BYTES {
        String::new()
    } else {
        program
    };
    let args_json = if args.len() > MAX_VERIFICATION_JOB_ARGS
        || args
            .iter()
            .any(|a| a.len() > MAX_VERIFICATION_JOB_ARG_BYTES)
    {
        "[]".to_string()
    } else {
        // `Vec<String>` serialization is infallible by type.
        serde_json::to_string(&args).expect("string-array serialization cannot fail")
    };
    (program, args_json)
}

pub(crate) fn load_legacy_fact_rows(
    conn: &Connection,
    session_raw: i64,
    kind: &str,
) -> StoreResult<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT key, value FROM memory_fact
         WHERE session_id = ?1 AND kind = ?2 ORDER BY key ASC",
    )?;
    let rows = stmt.query_map(params![session_raw, kind], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The durable marker value: schema-versioned counters plus the bounded
/// typed skip notes. Shrinks until it fits the fact cap; dropped notes stay
/// counted in `skipped_overflow` (and were logged).
pub(crate) fn legacy_import_marker_json(report: &LegacyVerificationImport) -> String {
    let total_notes = report.skipped.len();
    let mut shown = report.skipped.clone();
    loop {
        let dropped = (total_notes - shown.len()) as u64;
        let value = serde_json::json!({
            "schema_ver": 1,
            "imported_attempts": report.imported_attempts,
            "imported_jobs": report.imported_jobs,
            "imported_results": report.imported_results,
            "skipped": shown
                .iter()
                .map(|s| serde_json::json!({ "key": s.key, "reason": s.reason }))
                .collect::<Vec<_>>(),
            "skipped_overflow": report.skipped_overflow + dropped,
        })
        .to_string();
        if value.len() <= MAX_LEGACY_IMPORT_MARKER_BYTES || shown.is_empty() {
            return value;
        }
        shown.pop();
    }
}

/// Project ONE legacy attempt (plus its background job rows) into the v22
/// tables. `Err(reason)` means the whole attempt is skipped (the caller
/// records the typed note); per-check problems are recorded on
/// `session_report` and the remaining checks still import. Job fact keys are
/// claimed only after the attempt validated and inserted, so a skipped
/// attempt leaves its job rows orphaned and loudly noted.
pub(crate) fn import_one_legacy_attempt(
    tx: &rusqlite::Transaction<'_>,
    session_raw: u64,
    key: &str,
    value: &str,
    jobs: &std::collections::HashMap<String, String>,
    claimed: &mut std::collections::HashSet<String>,
    session_report: &mut LegacyVerificationImport,
) -> StoreResult<std::result::Result<(u64, u64), String>> {
    let Some((key_task, key_op)) = parse_legacy_attempt_key(key) else {
        return Ok(Err(format!(
            "legacy attempt key {key:?} is not va:<task>:<op>"
        )));
    };
    let row: LegacyAttemptRowValue = match serde_json::from_str(value) {
        Ok(row) => row,
        Err(e) => return Ok(Err(format!("undecodable legacy JSON: {e}"))),
    };
    if row.schema_ver < 1 || row.schema_ver > LEGACY_VERIFICATION_SCHEMA_VER {
        return Ok(Err(format!(
            "unknown legacy schema_ver {} (reader understands 1..={LEGACY_VERIFICATION_SCHEMA_VER})",
            row.schema_ver
        )));
    }
    if row.task_id != key_task || row.op_id != key_op {
        return Ok(Err(format!(
            "legacy key names task {key_task}/op {key_op} but the row names task {}/op {}",
            row.task_id, row.op_id
        )));
    }
    if row.task_id == 0 || row.op_id == 0 || row.task_revision == 0 {
        return Ok(Err(
            "legacy attempt identity (task/op/revision) must be non-zero".into(),
        ));
    }
    let fingerprint_json = row
        .environment_fingerprint
        .as_ref()
        .map(|v| serde_json::to_string(v).expect("string serialization cannot fail"));
    let attempt = VerificationAttemptRow {
        session_id: SessionId::new(session_raw),
        task_id: TaskId::new(row.task_id),
        attempt_op_id: row.op_id,
        task_revision: TaskRevision::new(row.task_revision),
        workspace_root: row.workspace_root.clone(),
        environment_fingerprint_json: fingerprint_json.clone(),
        created_ms: row.created_ms,
    };
    let mut checks: Vec<VerificationJobRow> = Vec::new();
    let mut results: Vec<(String, String, i64)> = Vec::new();
    let mut pending_claims: Vec<String> = Vec::new();
    for (ordinal, legacy_check) in row.checks.iter().enumerate() {
        let Ok(ordinal) = u32::try_from(ordinal) else {
            return Ok(Err(
                "legacy attempt carries more checks than the v22 ordinal domain".into(),
            ));
        };
        match legacy_check.inline.as_deref() {
            Some(inline) => {
                if !LEGACY_VERIFICATION_INLINE_STATES.contains(&inline) {
                    return Ok(Err(format!(
                        "inline check '{}' carries unknown legacy status {inline:?}",
                        legacy_check.check_id
                    )));
                }
                checks.push(VerificationJobRow {
                    session_id: attempt.session_id,
                    task_id: attempt.task_id,
                    attempt_op_id: attempt.attempt_op_id,
                    check_id: legacy_check.check_id.clone(),
                    ordinal,
                    task_revision: attempt.task_revision,
                    workspace_root: attempt.workspace_root.clone(),
                    kind: String::new(),
                    command: legacy_check.command.clone(),
                    program: String::new(),
                    args_json: "[]".into(),
                    spec_json: None,
                    budget_ms: 0,
                    inline_status: Some(inline.to_string()),
                    state: inline.to_string(),
                    result_json: None,
                    note: None,
                    op_id: None,
                    environment_fingerprint_json: fingerprint_json.clone(),
                    created_ms: attempt.created_ms,
                    updated_ms: attempt.created_ms,
                    finished_ms: Some(attempt.created_ms),
                });
            }
            None => {
                let job_key = format!("vj:{}:{}", row.task_id, legacy_check.check_id);
                let Some(job_value) = jobs.get(&job_key) else {
                    push_legacy_import_skip(
                        session_report,
                        &job_key,
                        format!(
                            "background check '{}' has no legacy job row",
                            legacy_check.check_id
                        ),
                    );
                    continue;
                };
                let job: LegacyJobRowValue = match serde_json::from_str(job_value) {
                    Ok(job) => job,
                    Err(e) => {
                        push_legacy_import_skip(
                            session_report,
                            &job_key,
                            format!("undecodable legacy job JSON: {e}"),
                        );
                        continue;
                    }
                };
                if job.schema_ver < 1 || job.schema_ver > LEGACY_VERIFICATION_SCHEMA_VER {
                    push_legacy_import_skip(
                        session_report,
                        &job_key,
                        format!("unknown legacy job schema_ver {}", job.schema_ver),
                    );
                    continue;
                }
                if parse_legacy_job_key(&job_key) != Some((job.task_id, job.check_id.as_str()))
                    || job.task_id != row.task_id
                    || job.check_id != legacy_check.check_id
                    || job.attempt_op != row.op_id
                {
                    push_legacy_import_skip(
                        session_report,
                        &job_key,
                        format!(
                            "legacy job identity disagrees with its key/attempt (task {}, check {:?}, attempt {})",
                            job.task_id, job.check_id, job.attempt_op
                        ),
                    );
                    continue;
                }
                if !LEGACY_VERIFICATION_JOB_STATES.contains(&job.state.as_str()) {
                    push_legacy_import_skip(
                        session_report,
                        &job_key,
                        format!("unknown legacy job state {:?}", job.state),
                    );
                    continue;
                }
                if job.check_id.is_empty()
                    || job.kind.is_empty()
                    || job.kind.len() > MAX_VERIFICATION_JOB_KIND_BYTES
                    || job.command.is_empty()
                    || job.command.len() > MAX_VERIFICATION_JOB_COMMAND_BYTES
                    || job.spec_json.is_empty()
                    || job.spec_json.len() > MAX_VERIFICATION_JOB_SPEC_JSON_BYTES
                    || job.budget_ms == 0
                    || job.budget_ms > MAX_VERIFICATION_JOB_BUDGET_MS
                {
                    push_legacy_import_skip(
                        session_report,
                        &job_key,
                        "legacy job carries a field outside the v22 bounds".into(),
                    );
                    continue;
                }
                if job.op_id == Some(0) {
                    push_legacy_import_skip(
                        session_report,
                        &job_key,
                        "legacy job carries a zero claim op id".into(),
                    );
                    continue;
                }
                let (program, args_json) =
                    derive_legacy_argv_identity(&job.spec_json, &job.command);
                let note = match job.note {
                    Some(note)
                        if !note.is_empty() && note.len() <= MAX_VERIFICATION_JOB_NOTE_BYTES =>
                    {
                        Some(note)
                    }
                    Some(_) => {
                        push_legacy_import_skip(
                            session_report,
                            &job_key,
                            "legacy job note outside the v22 bounds; imported without it".into(),
                        );
                        None
                    }
                    None => None,
                };
                let result_json = job
                    .result_json
                    .filter(|r| !r.is_empty() && r.len() <= MAX_VERIFICATION_JOB_RESULT_JSON_BYTES);
                let job_fingerprint = job
                    .environment_fingerprint
                    .as_ref()
                    .map(|v| serde_json::to_string(v).expect("string serialization cannot fail"))
                    .or_else(|| fingerprint_json.clone());
                checks.push(VerificationJobRow {
                    session_id: attempt.session_id,
                    task_id: attempt.task_id,
                    attempt_op_id: attempt.attempt_op_id,
                    check_id: job.check_id.clone(),
                    ordinal,
                    task_revision: if job.task_revision == 0 {
                        attempt.task_revision
                    } else {
                        TaskRevision::new(job.task_revision)
                    },
                    workspace_root: job.workspace_root.clone(),
                    kind: job.kind.clone(),
                    command: job.command.clone(),
                    program,
                    args_json,
                    spec_json: Some(job.spec_json.clone()),
                    budget_ms: job.budget_ms,
                    inline_status: None,
                    state: job.state.clone(),
                    result_json: None,
                    note,
                    op_id: job.op_id,
                    environment_fingerprint_json: job_fingerprint,
                    created_ms: job.created_ms,
                    updated_ms: job.updated_ms,
                    finished_ms: job.finished_ms,
                });
                match result_json {
                    Some(result_json)
                        if matches!(job.state.as_str(), "passed" | "failed" | "unavailable") =>
                    {
                        results.push((
                            job.check_id.clone(),
                            result_json,
                            job.finished_ms.unwrap_or(job.updated_ms),
                        ));
                    }
                    Some(_) => push_legacy_import_skip(
                        session_report,
                        &job_key,
                        format!(
                            "legacy job state {:?} disagrees with its outcome JSON; the outcome was not imported",
                            job.state
                        ),
                    ),
                    None if matches!(job.state.as_str(), "passed" | "failed" | "unavailable") => {
                        push_legacy_import_skip(
                            session_report,
                            &job_key,
                            format!(
                                "terminal legacy job {:?} carries no outcome JSON; imported without a result row",
                                job.state
                            ),
                        );
                    }
                    None => {}
                }
                pending_claims.push(job_key);
            }
        }
    }
    if checks.is_empty() {
        return Ok(Err(
            "legacy attempt carries no importable required checks".into()
        ));
    }
    if let Err(e) = validate_verification_attempt(&attempt, &row.changed, &checks) {
        return Ok(Err(format!("legacy attempt fails the v22 contract: {e}")));
    }
    // A hand-edited database can carry the v22 rows without the import
    // marker: never collide with them — the existing attempt wins and the
    // legacy row is skipped loudly (the marker still lands).
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM verification_attempt
             WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3",
            params![
                attempt.session_id.raw() as i64,
                attempt.task_id.raw() as i64,
                attempt.attempt_op_id as i64
            ],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Ok(Err(
            "a v22 attempt row with this identity already exists; the legacy row was not imported"
                .into(),
        ));
    }
    tx.execute(
        "INSERT INTO verification_attempt(
            session_id, task_id, attempt_op_id, task_revision, workspace_root,
            environment_fingerprint_json, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            attempt.session_id.raw() as i64,
            attempt.task_id.raw() as i64,
            attempt.attempt_op_id as i64,
            attempt.task_revision.raw() as i64,
            attempt.workspace_root,
            attempt.environment_fingerprint_json,
            attempt.created_ms
        ],
    )?;
    for (changed_ordinal, path) in row.changed.iter().enumerate() {
        tx.execute(
            "INSERT INTO verification_attempt_changed_file(
                session_id, task_id, attempt_op_id, ordinal, path)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                attempt.session_id.raw() as i64,
                attempt.task_id.raw() as i64,
                attempt.attempt_op_id as i64,
                changed_ordinal as i64,
                path
            ],
        )?;
    }
    for check in &checks {
        tx.execute(
            "INSERT INTO verification_job(
                session_id, task_id, attempt_op_id, check_id, ordinal,
                task_revision, workspace_root, kind, command, program,
                args_json, spec_json, budget_ms, inline_status, state, note,
                op_id, environment_fingerprint_json, created_ms, updated_ms,
                finished_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                check.session_id.raw() as i64,
                check.task_id.raw() as i64,
                check.attempt_op_id as i64,
                check.check_id,
                check.ordinal as i64,
                check.task_revision.raw() as i64,
                check.workspace_root,
                check.kind,
                check.command,
                check.program,
                check.args_json,
                check.spec_json,
                check.budget_ms as i64,
                check.inline_status,
                check.state,
                check.note,
                check.op_id.map(|op| op as i64),
                check.environment_fingerprint_json,
                check.created_ms,
                check.updated_ms,
                check.finished_ms
            ],
        )?;
    }
    for (check_id, result_json, finished_ms) in &results {
        tx.execute(
            "INSERT INTO verification_job_result(
                session_id, task_id, attempt_op_id, check_id, result_json, finished_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                attempt.session_id.raw() as i64,
                attempt.task_id.raw() as i64,
                attempt.attempt_op_id as i64,
                check_id,
                result_json,
                finished_ms
            ],
        )?;
    }
    for job_key in pending_claims {
        claimed.insert(job_key);
    }
    Ok(Ok((checks.len() as u64, results.len() as u64)))
}

/// One-shot, idempotent v22 repair: project every session's pre-v22
/// verification `memory_fact` rows into the real
/// `verification_attempt`/`_changed_file`/`_job`/`_job_result` tables.
///
/// Exactly-once: the imported rows and a durable per-session marker fact
/// (`verification_v22_import`/`done`) commit in ONE transaction; a session
/// whose marker exists is never scanned again. Legacy fact rows are
/// preserved (additive upgrade). Every undecodable/corrupt row is skipped
/// with a LOUD tracing error, a typed note on the marker and the row left
/// in place — never a silent drop, never a deletion.
pub(crate) fn import_legacy_verification_facts_conn(
    conn: &mut Connection,
) -> StoreResult<LegacyVerificationImport> {
    let mut report = LegacyVerificationImport::default();
    let legacy_sessions: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT session_id FROM memory_fact
             WHERE kind IN (?1, ?2) ORDER BY session_id ASC",
        )?;
        let rows = stmt.query_map(
            params![
                LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
                LEGACY_VERIFICATION_JOB_FACT_KIND
            ],
            |r| r.get::<_, i64>(0),
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        out
    };
    if legacy_sessions.is_empty() {
        return Ok(report);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let import_time = now_ms();
    for session_raw in legacy_sessions {
        if session_raw <= 0 {
            // Impossible under the foreign keys; a hand-edited row has no
            // session row space to carry a marker note. Surface it loudly.
            tracing::error!(
                session_id = session_raw,
                "legacy verification fact rows under a non-positive session id; skipped (no row space for a marker)"
            );
            continue;
        }
        let already: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM memory_fact
                 WHERE session_id = ?1 AND kind = ?2 AND key = ?3",
                params![
                    session_raw,
                    VERIFICATION_V22_IMPORT_MARKER_KIND,
                    VERIFICATION_V22_IMPORT_MARKER_KEY
                ],
                |r| r.get(0),
            )
            .optional()?;
        if already.is_some() {
            continue;
        }
        let attempts =
            load_legacy_fact_rows(&tx, session_raw, LEGACY_VERIFICATION_ATTEMPT_FACT_KIND)?;
        let jobs = load_legacy_fact_rows(&tx, session_raw, LEGACY_VERIFICATION_JOB_FACT_KIND)?;
        let job_map: std::collections::HashMap<String, String> = jobs.iter().cloned().collect();
        let mut session_report = LegacyVerificationImport::default();
        let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (key, value) in &attempts {
            match import_one_legacy_attempt(
                &tx,
                session_raw as u64,
                key,
                value,
                &job_map,
                &mut claimed,
                &mut session_report,
            )? {
                Ok((imported_jobs, imported_results)) => {
                    session_report.imported_attempts += 1;
                    session_report.imported_jobs += imported_jobs;
                    session_report.imported_results += imported_results;
                }
                Err(reason) => push_legacy_import_skip(&mut session_report, key, reason),
            }
        }
        for (key, _) in &jobs {
            if !claimed.contains(key) {
                push_legacy_import_skip(
                    &mut session_report,
                    key,
                    "legacy job row was not imported: its attempt row is missing or was skipped"
                        .into(),
                );
            }
        }
        let marker = legacy_import_marker_json(&session_report);
        tx.execute(
            "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_raw,
                VERIFICATION_V22_IMPORT_MARKER_KIND,
                VERIFICATION_V22_IMPORT_MARKER_KEY,
                marker,
                import_time
            ],
        )?;
        report.imported_attempts += session_report.imported_attempts;
        report.imported_jobs += session_report.imported_jobs;
        report.imported_results += session_report.imported_results;
        report.skipped_overflow += session_report.skipped_overflow;
        for skip in session_report.skipped {
            if report.skipped.len() < MAX_LEGACY_IMPORT_SKIP_NOTES {
                report.skipped.push(skip);
            } else {
                report.skipped_overflow += 1;
            }
        }
    }
    tx.commit()?;
    Ok(report)
}

#[cfg(test)]
#[path = "legacy_import_tests.rs"]
mod legacy_verification_import_tests;
