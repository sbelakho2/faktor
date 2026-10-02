//! `migration`: cohesive slice of the mechanically decomposed parent module.

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
        serde_json::to_string(&args).unwrap_or_else(|_| "[]".to_string())
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
        .map(|v| serde_json::to_string(v).unwrap_or_default());
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
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
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

/// Apply migrations transactionally; `PRAGMA user_version` is the cursor.
pub(crate) fn migrate(conn: &mut Connection) -> StoreResult<()> {
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)
            .map_err(|e| StoreError::Migration(format!("v{target}: {e}")))?;
        // The v9 op-id sequence table needs its one global row seeded from
        // the migration-time clock, which no static SQL can express. The
        // INSERT is idempotent so a replay (or a second opener racing the
        // first migration) can never double-seed or overwrite.
        if i == OP_ID_SEQ_MIGRATION_INDEX {
            tx.execute(
                "INSERT OR IGNORE INTO op_id_seq (session_scope, next_value) VALUES (0, ?1)",
                params![op_id_seq_seed()],
            )
            .map_err(|e| StoreError::Migration(format!("v{target} seed: {e}")))?;
        }
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| StoreError::Migration(format!("v{target} version write: {e}")))?;
        tx.commit()
            .map_err(|e| StoreError::Migration(format!("v{target} commit: {e}")))?;
        version = target;
    }
    // One-shot repair on EVERY open (marker-guarded, idempotent): project
    // pre-v22 verification `memory_fact` rows into the v22 tables. Runs after
    // the schema cursor reached the newest version, so the target tables
    // always exist; a store with no legacy rows pays one indexed SELECT.
    import_legacy_verification_facts_conn(conn)?;
    Ok(())
}

#[cfg(test)]
mod legacy_verification_import_tests {
    use super::*;

    const TASK: u64 = 7;

    fn attempt_json(op: u64, task: u64, revision: u64) -> String {
        serde_json::json!({
            "schema_ver": 2,
            "task_id": task,
            "op_id": op,
            "task_revision": revision,
            "workspace_root": "/w",
            "changed": ["src/a.rs"],
            "checks": [
                { "check_id": "make_build", "command": "make build", "inline": "passed" },
                { "check_id": "make_test", "command": "make test", "inline": null },
                { "check_id": "make_lint", "command": "make lint", "inline": null },
            ],
            "environment_fingerprint": null,
            "created_ms": 111,
        })
        .to_string()
    }

    fn job_json(op: u64, task: u64, check_id: &str, state: &str, result: Option<&str>) -> String {
        serde_json::json!({
            "schema_ver": 2,
            "attempt_op": op,
            "task_id": task,
            "task_revision": 3,
            "workspace_root": "/w",
            "check_id": check_id,
            "kind": "test",
            "command": format!("make {}", check_id.trim_start_matches("make_")),
            "spec_json": serde_json::json!({
                "id": check_id,
                "kind": "Test",
                "category": "Unit",
                "program": "make",
                "args": ["test"],
                "cwd_rel": ".",
                "affects": [],
                "required": true,
            })
            .to_string(),
            "budget_ms": 60_000,
            "state": state,
            "note": null,
            "op_id": if state == "running" { Some(99) } else { None },
            "result_json": result,
            "environment_fingerprint": null,
            "created_ms": 112,
            "updated_ms": 113,
            "finished_ms": if state == "running" { None } else { Some(114) },
        })
        .to_string()
    }

    fn plant(store: &Store, sid: SessionId, kind: &str, key: &str, value: &str) {
        store.upsert_memory_fact(sid, kind, key, value).unwrap();
    }

    #[test]
    fn pre_v22_rows_import_once_with_deterministic_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let sid: SessionId;
        {
            let store = Store::open(&root, true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        }
        {
            let store = Store::open(&root, true).unwrap();
            plant(
                &store,
                sid,
                LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
                &format!("va:{TASK}:4242"),
                &attempt_json(4242, TASK, 3),
            );
            plant(
                &store,
                sid,
                LEGACY_VERIFICATION_JOB_FACT_KIND,
                &format!("vj:{TASK}:make_test"),
                &job_json(4242, TASK, "make_test", "running", None),
            );
            plant(
                &store,
                sid,
                LEGACY_VERIFICATION_JOB_FACT_KIND,
                &format!("vj:{TASK}:make_lint"),
                &job_json(4242, TASK, "make_lint", "passed", Some("{\"status\":\"Passed\",\"exit\":0,\"started_ms\":1,\"finished_ms\":2,\"summary\":null,\"truncated\":false}")),
            );
        }
        // The next OPEN is the upgrade: migration completion runs the
        // one-shot repair with no explicit call.
        let store = Store::open(&root, true).unwrap();
        let view = store
            .verification_attempt_get(sid, TaskId::new(TASK), 4242)
            .unwrap()
            .expect("the legacy attempt must import with its derived identity");
        assert_eq!(view.attempt.session_id, sid);
        assert_eq!(view.attempt.task_id, TaskId::new(TASK));
        assert_eq!(view.attempt.attempt_op_id, 4242);
        assert_eq!(view.attempt.task_revision, TaskRevision::new(3));
        assert_eq!(view.changed, vec!["src/a.rs".to_string()]);
        assert_eq!(view.checks.len(), 3, "inline + background checks import");
        assert_eq!(view.checks[0].check_id, "make_build");
        assert_eq!(view.checks[0].inline_status.as_deref(), Some("passed"));
        assert_eq!(view.checks[0].state, "passed");
        assert_eq!(view.checks[1].check_id, "make_test");
        assert_eq!(view.checks[1].state, "running");
        assert_eq!(view.checks[1].op_id, Some(99));
        assert_eq!(view.checks[1].program, "make");
        assert_eq!(view.checks[1].args_json, "[\"test\"]");
        assert_eq!(view.checks[2].check_id, "make_lint");
        assert_eq!(view.checks[2].state, "passed");
        assert!(
            view.checks[2]
                .result_json
                .as_deref()
                .unwrap()
                .contains("Passed"),
            "the terminal outcome imports into verification_job_result"
        );
        // An in-flight legacy job is visible through the open-job scan, so
        // post-restart recovery owns it.
        let open = store
            .verification_jobs_open(sid, TaskId::new(TASK))
            .unwrap();
        assert_eq!(open.len(), 1, "in-flight legacy job visible after upgrade");
        assert_eq!(open[0].check_id, "make_test");
        assert_eq!(open[0].state, "running");
        // Additive: the legacy fact rows are preserved, and exactly ONE
        // durable marker records the import.
        let facts = store.memory_facts(sid).unwrap();
        assert!(facts.iter().any(|(k, key, _)| {
            k == LEGACY_VERIFICATION_ATTEMPT_FACT_KIND && key == &format!("va:{TASK}:4242")
        }));
        assert!(facts.iter().any(|(k, key, _)| {
            k == LEGACY_VERIFICATION_JOB_FACT_KIND && key == &format!("vj:{TASK}:make_test")
        }));
        assert_eq!(
            facts
                .iter()
                .filter(|(k, key, _)| {
                    k == VERIFICATION_V22_IMPORT_MARKER_KIND
                        && key == VERIFICATION_V22_IMPORT_MARKER_KEY
                })
                .count(),
            1,
            "one durable marker per imported session"
        );
        drop(store);
        // Re-open is a no-op: no duplicate attempt, checks, jobs or results.
        let store = Store::open(&root, true).unwrap();
        let again = store.import_legacy_verification_facts().unwrap();
        assert_eq!(again.imported_attempts, 0);
        assert_eq!(again.imported_jobs, 0);
        assert_eq!(again.imported_results, 0);
        let view = store
            .verification_attempt_get(sid, TaskId::new(TASK), 4242)
            .unwrap()
            .unwrap();
        assert_eq!(view.checks.len(), 3, "no duplicate checks after reopen");
        assert_eq!(
            store
                .verification_jobs_for_attempt(sid, TaskId::new(TASK), 4242)
                .unwrap()
                .len(),
            2,
            "no duplicate background jobs after reopen"
        );
    }

    #[test]
    fn corrupt_legacy_rows_are_loudly_skipped_and_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let sid: SessionId;
        {
            let store = Store::open(&root, true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        }
        let store = Store::open(&root, true).unwrap();
        // (a) undecodable attempt JSON.
        plant(
            &store,
            sid,
            LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
            "va:8:1",
            "{ this is not json",
        );
        // (b) an unknown future schema version.
        plant(
            &store,
            sid,
            LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
            "va:8:2",
            &serde_json::json!({
                "schema_ver": 99,
                "task_id": 8,
                "op_id": 2,
                "task_revision": 1,
                "workspace_root": "/w",
                "changed": [],
                "checks": [],
                "created_ms": 1,
            })
            .to_string(),
        );
        // (c) key/value identity disagreement.
        let mut mismatched: serde_json::Value =
            serde_json::from_str(&attempt_json(4, 8, 1)).unwrap();
        mismatched["op_id"] = serde_json::json!(4);
        plant(
            &store,
            sid,
            LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
            "va:8:3",
            &mismatched.to_string(),
        );
        // (d) a valid attempt whose only background job is corrupt: the
        // inline check still imports, the corrupt job is skipped loudly.
        let attempt_value = serde_json::json!({
            "schema_ver": 2,
            "task_id": 9,
            "op_id": 10,
            "task_revision": 1,
            "workspace_root": "/w",
            "changed": [],
            "checks": [
                { "check_id": "inline_ok", "command": "make ok", "inline": "passed" },
                { "check_id": "bad_job", "command": "make bad", "inline": null },
            ],
            "created_ms": 5,
        })
        .to_string();
        plant(
            &store,
            sid,
            LEGACY_VERIFICATION_ATTEMPT_FACT_KIND,
            "va:9:10",
            &attempt_value,
        );
        plant(
            &store,
            sid,
            LEGACY_VERIFICATION_JOB_FACT_KIND,
            "vj:9:bad_job",
            &job_json(10, 9, "bad_job", "zombie", None),
        );
        // (e) an orphan job whose attempt row does not exist.
        plant(
            &store,
            sid,
            LEGACY_VERIFICATION_JOB_FACT_KIND,
            "vj:9:orphan",
            &job_json(11, 9, "orphan", "queued", None),
        );
        let report = store.import_legacy_verification_facts().unwrap();
        assert_eq!(report.imported_attempts, 1, "only the resolvable attempt");
        assert_eq!(report.imported_jobs, 1, "only its inline check");
        assert_eq!(
            report.skipped_overflow, 0,
            "the bounded note list holds every skip: {:?}",
            report.skipped
        );
        let reason_for = |key: &str| {
            report
                .skipped
                .iter()
                .find(|s| s.key == key)
                .unwrap_or_else(|| panic!("no typed skip note for {key}: {:?}", report.skipped))
                .reason
                .clone()
        };
        assert!(
            reason_for("va:8:1").contains("undecodable"),
            "{}",
            reason_for("va:8:1")
        );
        assert!(
            reason_for("va:8:2").contains("schema_ver"),
            "{}",
            reason_for("va:8:2")
        );
        assert!(
            reason_for("va:8:3").contains("row names"),
            "{}",
            reason_for("va:8:3")
        );
        assert!(
            reason_for("vj:9:bad_job").contains("unknown legacy job state"),
            "{}",
            reason_for("vj:9:bad_job")
        );
        assert!(
            reason_for("vj:9:orphan").contains("not imported"),
            "{}",
            reason_for("vj:9:orphan")
        );
        // The skips are durable on the marker (typed note), the corrupt
        // legacy rows are NEVER deleted, and the resolvable attempt is live.
        let facts = store.memory_facts(sid).unwrap();
        let marker = facts
            .iter()
            .find(|(k, key, _)| {
                k == VERIFICATION_V22_IMPORT_MARKER_KIND
                    && key == VERIFICATION_V22_IMPORT_MARKER_KEY
            })
            .map(|(_, _, value)| value.clone())
            .expect("marker fact");
        assert!(marker.contains("\"skipped\""), "{marker}");
        assert!(marker.contains("unknown legacy job state"), "{marker}");
        assert!(facts
            .iter()
            .any(|(k, key, _)| k == "verification_attempt" && key == "va:8:1"));
        assert!(facts
            .iter()
            .any(|(k, key, _)| k == "verification_job" && key == "vj:9:orphan"));
        let view = store
            .verification_attempt_get(sid, TaskId::new(9), 10)
            .unwrap()
            .unwrap();
        assert_eq!(view.checks.len(), 1, "inline check imported");
        assert_eq!(view.checks[0].check_id, "inline_ok");
        // A corrupt legacy attempt never minted a half-written row.
        assert!(store
            .verification_attempt_get(sid, TaskId::new(8), 3)
            .unwrap()
            .is_none());
        // Re-open: marker-guarded, nothing duplicated, corrupt rows stay.
        drop(store);
        let store = Store::open(&root, true).unwrap();
        assert_eq!(
            store
                .verification_attempt_get(sid, TaskId::new(9), 10)
                .unwrap()
                .unwrap()
                .checks
                .len(),
            1
        );
        assert!(store
            .memory_facts(sid)
            .unwrap()
            .iter()
            .any(|(_, key, _)| key == "va:8:1"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_v3_keeps_pre_v3_checkpoint_rows_readable() {
        // Simulate a store that was created at v2 (checkpoints without the
        // after-blob column): open a fresh store, record a checkpoint, then
        // downgrade the schema behind the API's back (DROP COLUMN + set the
        // version cursor back). Reopening must apply v3, leave the old row
        // readable, and surface after_cas_hash as NULL — never a panic and
        // never a lost row.
        let dir = tempfile::tempdir().unwrap();
        let sid = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            store
                .put_checkpoint(s.id, 3, "f.txt", "before", "after", Some("after-blob"))
                .unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("ALTER TABLE checkpoint DROP COLUMN after_cas_hash", [])
                    .unwrap();
                // The v6 existence columns are post-v2 too: drop them so the
                // full migration chain (v3..v6) replays on reopen.
                conn.execute("ALTER TABLE checkpoint DROP COLUMN before_exists", [])
                    .unwrap();
                conn.execute("ALTER TABLE checkpoint DROP COLUMN after_exists", [])
                    .unwrap();
                // The v7 tool-run recovery columns + turn-record table are
                // post-v2 too: drop them so the full migration chain
                // (v3..v7) replays on reopen.
                conn.execute("ALTER TABLE tool_run DROP COLUMN replay_descriptor", [])
                    .unwrap();
                conn.execute("ALTER TABLE tool_run DROP COLUMN attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE tool_run DROP COLUMN postcondition", [])
                    .unwrap();
                conn.execute("DROP TABLE turn_record", []).unwrap();
                // The v8 session-identity columns are post-v2 too: drop them
                // so the full migration chain (v3..v8) replays on reopen.
                conn.execute("ALTER TABLE session DROP COLUMN worktree_id", [])
                    .unwrap();
                conn.execute("ALTER TABLE session DROP COLUMN task_id", [])
                    .unwrap();
                // The v9/v10 task tables are post-this-version too: restore
                // the legacy `task` layout so the migration chain past v10
                // replays on reopen.
                conn.execute("DROP TABLE task", []).unwrap();
                conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                    .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                               // post-this-version too: drop them so the full chain
                               // (past v11) replays cleanly on reopen.
                conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                    .unwrap();
                // v13 prefix-stability columns are post-this-version too: drop
                // them so the full chain (past v13) replays cleanly on reopen.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too: drop them so the full chain
                // (past v20) replays cleanly.
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
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 2", []).unwrap();
            }
            s.id
        };
        // Reopen: v3 re-applies the column; the pre-v3 row must read back
        // intact with after_cas_hash = NULL.
        let store = Store::open(dir.path(), true).unwrap();
        let cps = store.checkpoints_of(sid).unwrap();
        assert_eq!(cps.len(), 1, "the old row must survive the v3 migration");
        assert_eq!(cps[0].path, "f.txt");
        assert_eq!(cps[0].before_hash, "before");
        assert_eq!(cps[0].after_hash, "after");
        assert_eq!(cps[0].after_cas_hash, None);
        assert!(cps[0].created_ms > 0);
        // And the column is writable again.
        store
            .put_checkpoint(sid, 4, "g.txt", "b", "a", Some("x"))
            .unwrap();
        assert_eq!(
            store.checkpoints_of(sid).unwrap()[1]
                .after_cas_hash
                .as_deref(),
            Some("x")
        );
    }

    #[test]
    fn migration_v6_keeps_pre_v6_checkpoint_rows_readable_as_existing() {
        // A store created at v5 records a checkpoint without existence
        // flags. Downgrade the schema behind the API's back (DROP the new
        // columns + rewind the version cursor), then reopen: v6 re-adds the
        // columns with DEFAULT 1 and the old row must read back as
        // exists:true on both sides (old rows only ever recorded real
        // files) — never a lost row, never a panic.
        let dir = tempfile::tempdir().unwrap();
        let sid = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            store
                .put_checkpoint(s.id, 1, "f.txt", "before", "after", Some("blob"))
                .unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("ALTER TABLE checkpoint DROP COLUMN before_exists", [])
                    .unwrap();
                conn.execute("ALTER TABLE checkpoint DROP COLUMN after_exists", [])
                    .unwrap();
                // The v7 tool-run recovery columns + turn-record table are
                // post-v5 too: drop them so the full migration chain
                // (v6..v7) replays on reopen.
                conn.execute("ALTER TABLE tool_run DROP COLUMN replay_descriptor", [])
                    .unwrap();
                conn.execute("ALTER TABLE tool_run DROP COLUMN attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE tool_run DROP COLUMN postcondition", [])
                    .unwrap();
                conn.execute("DROP TABLE turn_record", []).unwrap();
                // The v8 session-identity columns are post-v5 too: drop them
                // The v9/v10 task tables are post-this-version too: restore
                // the legacy `task` layout so the migration chain past v10
                // replays on reopen.
                conn.execute("DROP TABLE task", []).unwrap();
                conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                    .unwrap();
                // so the full migration chain (v6..v8) replays on reopen.
                conn.execute("ALTER TABLE session DROP COLUMN worktree_id", [])
                    .unwrap();
                conn.execute("ALTER TABLE session DROP COLUMN task_id", [])
                    .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                               // post-this-version too: drop them so the full chain
                               // (past v11) replays cleanly on reopen.
                conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                    .unwrap();
                // v13 prefix-stability columns are post-this-version too: drop
                // them so the full chain (past v13) replays cleanly on reopen.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too: drop them so the full chain
                // (past v20) replays cleanly.
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
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 5", []).unwrap();
            }
            s.id
        };
        let store = Store::open(dir.path(), true).unwrap();
        let cps = store.checkpoints_of(sid).unwrap();
        assert_eq!(cps.len(), 1, "the old row must survive the v6 migration");
        assert!(
            cps[0].before_exists && cps[0].after_exists,
            "pre-v6 rows have no existence marker: hash present means exists:true"
        );
        assert_eq!(cps[0].before_hash, "before");
        // And the new columns are writable again.
        store
            .insert_checkpoint(sid, "g.txt", false, "", true, "a", Some("x"))
            .unwrap();
        let rows = store.checkpoints_of(sid).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!rows[1].before_exists);
        assert_eq!(
            rows[1].sequence, 2,
            "allocation continues after legacy rows"
        );
    }

    #[test]
    fn migration_v7_replays_cleanly_on_a_v6_store() {
        // Simulate a store created before v7 (no turn_record table, no
        // tool_run recovery columns), then reopen: the v7 migration must
        // re-create everything without touching existing rows.
        let dir = tempfile::tempdir().unwrap();
        let sid = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            store
                .start_tool_run(
                    s.id,
                    OpId::new(1),
                    "run",
                    serde_json::json!({}),
                    serde_json::json!({"strategy": "none"}),
                    None,
                    None,
                )
                .unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("ALTER TABLE tool_run DROP COLUMN replay_descriptor", [])
                    .unwrap();
                conn.execute("ALTER TABLE tool_run DROP COLUMN attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE tool_run DROP COLUMN postcondition", [])
                    .unwrap();
                // The v9/v10 task tables are post-this-version too: restore
                // the legacy `task` layout so the migration chain past v10
                // replays on reopen.
                conn.execute("DROP TABLE task", []).unwrap();
                conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                    .unwrap();
                conn.execute("DROP TABLE turn_record", []).unwrap();
                // The v8 session-identity columns are post-v7 too: drop them
                // so the migration chain past v7 replays on reopen.
                conn.execute("ALTER TABLE session DROP COLUMN worktree_id", [])
                    .unwrap();
                conn.execute("ALTER TABLE session DROP COLUMN task_id", [])
                    .unwrap();
                // Pre-v7 stores sit at machine version 7 (the v6 comment
                // block covers TWO ALTER entries: before_exists and
                // after_exists); rewinding to 7 replays ONLY the v7 entry.                                // v11 artifacts (event payload_ver + the typed ledger) are
                // post-this-version too: drop them so the full chain
                // (past v11) replays cleanly on reopen.
                conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                    .unwrap();
                // v13 prefix-stability columns are post-this-version too: drop
                // them so the full chain (past v13) replays cleanly on reopen.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too: drop them so the full chain
                // (past v20) replays cleanly.
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
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 7", []).unwrap();
            }
            s.id
        };
        let store = Store::open(dir.path(), true).unwrap();
        // The pre-v7 row survived and is readable.
        let pending = store.pending_tool_runs(sid).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempt, 0);
        assert!(pending[0].replay_descriptor.is_none());
        assert!(pending[0].postcondition.is_none());
        // And the new machinery works on the migrated store.
        let op = OpId::new(2);
        store
            .start_tool_run(
                sid,
                op,
                "echo",
                serde_json::json!({}),
                serde_json::json!({"strategy": "idempotent"}),
                None,
                Some(serde_json::json!({"tool_name": "echo"})),
            )
            .unwrap();
        assert_eq!(store.bump_tool_run_attempt(sid, op).unwrap(), 1);
        store
            .start_turn_record(sid, OpId::new(9), None, Some(2), "p", "m", None)
            .unwrap();
        assert_eq!(store.turn_records_of(sid).unwrap().len(), 1);
        // Reopen again: still stable.
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.turn_records_of(sid).unwrap().len(), 1);
    }

    #[test]
    fn migration_v8_replays_cleanly_on_a_v7_store() {
        // Simulate a v7 store (no worktree_id/task_id columns on session),
        // reopen: v8 must add the columns and existing rows must read back
        // as the standalone 1/1 default — never a lost or corrupt row.
        // (Note: the v6 checkpoint block spans TWO array entries, so the
        // session-identity migration is array index 8 = schema target 9;
        // rewinding to 8 replays exactly this one entry.)
        let dir = tempfile::tempdir().unwrap();
        let (sid, ws) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("ALTER TABLE session DROP COLUMN worktree_id", [])
                    .unwrap();
                conn.execute("ALTER TABLE session DROP COLUMN task_id", [])
                    .unwrap();
                // The v9/v10 task tables are post-this-version too: restore
                // the legacy `task` layout so the migration chain past v10
                // replays on reopen.
                conn.execute("DROP TABLE task", []).unwrap();
                conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                    .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                               // post-this-version too: drop them so the full chain
                               // (past v11) replays cleanly on reopen.
                conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                    .unwrap();
                // v13 prefix-stability columns are post-this-version too: drop
                // them so the full chain (past v13) replays cleanly on reopen.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too: drop them so the full chain
                // (past v20) replays cleanly.
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
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 8", []).unwrap();
            }
            (s.id, ws)
        };
        let store = Store::open(dir.path(), true).unwrap();
        let row = store.get_session(sid).unwrap().unwrap();
        assert_eq!(row.workspace_id, ws, "row survived the migration");
        assert_eq!(
            row.worktree_id,
            WorktreeId::new(1),
            "v8 default on old rows"
        );
        assert_eq!(row.task_id, TaskId::new(1), "v8 default on old rows");
        assert_eq!(store.list_sessions(None).unwrap().len(), 1);
    }

    #[test]
    fn migration_v9_replays_cleanly_on_a_v8_store() {
        // Simulate a v8 store (no op_id_seq table), reopen: v9 must create
        // the table and seed the ONE global row from the migration-time
        // clock so freshly migrated databases mint ids far above any
        // pre-migration (clock+counter) id. (The v9 block is array index 9
        // = schema target 10; rewinding to 9 replays exactly this entry.)
        let dir = tempfile::tempdir().unwrap();
        let (sid, ws) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("DROP TABLE op_id_seq", []).unwrap();
                // The v9/v10 task tables are post-this-version too: restore
                // the legacy `task` layout so the migration chain past v10
                // replays on reopen.
                conn.execute("DROP TABLE task", []).unwrap();
                conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                    .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                               // post-this-version too: drop them so the full chain
                               // (past v11) replays cleanly on reopen.
                conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                    .unwrap();
                // v13 prefix-stability columns are post-this-version too: drop
                // them so the full chain (past v13) replays cleanly on reopen.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too: drop them so the full chain
                // (past v20) replays cleanly.
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
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 9", []).unwrap();
            }
            (s.id, ws)
        };
        let store = Store::open(dir.path(), true).unwrap();
        // Pre-v9 rows survived the migration.
        let row = store.get_session(sid).unwrap().unwrap();
        assert_eq!(row.workspace_id, ws, "row survived the migration");
        // The seed row exists, is large, and ids start exactly there.
        let hw = store.op_id_seq_high_water().unwrap();
        assert!(
            hw > (1u64 << 20),
            "seed must dominate any pre-migration id: {hw}"
        );
        let (start, n) = store.alloc_op_ids(sid, 5).unwrap();
        assert_eq!((start, n), (hw, 5), "ids are minted at the seeded mark");
        // Reopen again: migration is a no-op and the sequence is durable.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.op_id_seq_high_water().unwrap(), hw + 5);
    }

    #[test]
    fn migration_v10_replays_cleanly_on_a_v9_store() {
        // Simulate a v9 store (the legacy one-row-per-session ledger table
        // only; no typed task rows), reopen: the v10 block must rename the
        // legacy table to task_ledger, create the typed `task` table, and
        // leave every pre-v10 ledger row readable byte-identically.
        let dir = tempfile::tempdir().unwrap();
        let (sid, ledger_value) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let ledger = serde_json::json!({"goal": "legacy ledger row", "tasks": []});
            store.put_task_ledger(s.id, ledger.clone()).unwrap();
            {
                let conn = store.raw_conn();
                // Rewind the schema to the v9 layout: drop the migrated
                // artifacts and rename the legacy table back to `task`.
                conn.execute("DROP TABLE task", []).unwrap();
                conn.execute("ALTER TABLE task_ledger RENAME TO task", [])
                    .unwrap(); // v11 artifacts (event payload_ver + the typed ledger) are
                               // post-this-version too: drop them so the full chain
                               // (past v11) replays cleanly on reopen.
                conn.execute("ALTER TABLE event DROP COLUMN payload_ver", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_entry", [])
                    .unwrap();
                conn.execute("DROP TABLE IF EXISTS ledger_head", [])
                    .unwrap();
                // v13 prefix-stability columns are post-this-version too: drop
                // them so the full chain (past v13) replays cleanly on reopen.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too: drop them so the full chain
                // (past v20) replays cleanly.
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
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 9", []).unwrap();
            }
            (s.id, ledger)
        };
        let store = Store::open(dir.path(), true).unwrap();
        // The legacy ledger row survived the migration byte-identically.
        assert_eq!(store.get_task_ledger(sid).unwrap(), Some(ledger_value));
        // The typed task table exists and starts empty; writes work.
        assert!(store.list_tasks(sid).unwrap().is_empty());
        let row = TaskRow {
            task_id: TaskId::new(3),
            session_id: sid,
            goal: "typed goal".into(),
            acceptance_criteria: vec!["cargo check".into()],
            plan: vec![],
            attachments: vec![],
            max_tokens: Some(10_000),
            max_turns: None,
            spent_tokens: 0,
            spent_turns: 0,
            state: TaskState::Pending,
            revision: TaskRevision::new(1),
            created_ms: 7,
            updated_ms: 7,
        };
        store.upsert_task(&row).unwrap();
        assert_eq!(store.get_task(sid, TaskId::new(3)).unwrap(), Some(row));
        // Reopen again: the migration is a no-op and both tables read back.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert!(store.get_task_ledger(sid).unwrap().is_some());
        let back = store.get_task(sid, TaskId::new(3)).unwrap().unwrap();
        assert_eq!(back.goal, "typed goal");
        assert_eq!(back.acceptance_criteria, vec!["cargo check".to_string()]);
        assert_eq!(back.max_tokens, Some(10_000));
        assert_eq!(back.state, TaskState::Pending);
        assert_eq!(back.revision, TaskRevision::new(1));
    }

    pub(crate) fn prefix_hash(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    #[test]
    fn prefix_columns_round_trip_across_reopen_and_legacy_rows_read_null() {
        // (d) The v13 columns must survive a full reopen with every value
        // intact, and rows written before the columns existed must honestly
        // read as "no observation" — never zeros, never guesses.
        let dir = tempfile::tempdir().unwrap();
        let (ws, sid) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store
                .create_session(ws, "prefix-roundtrip", "p", "m")
                .unwrap();
            // Legacy-shaped row: no prefix observation at all.
            store
                .record_provider_call(
                    s.id,
                    OpId::new(1),
                    "p",
                    "m",
                    "completed",
                    Some(10),
                    Some(5),
                    None,
                )
                .unwrap();
            // Full observation row.
            store
                .record_provider_call_with_prefix(
                    s.id,
                    OpId::new(2),
                    "p",
                    "m",
                    "completed",
                    Some(10),
                    Some(5),
                    None,
                    Some(prefix_hash(7)),
                    Some(1234),
                    Some(0.625),
                )
                .unwrap();
            // Hash with no recorded per-row stability: still an observation.
            store
                .record_provider_call_with_prefix(
                    s.id,
                    OpId::new(3),
                    "p",
                    "m",
                    "completed",
                    None,
                    None,
                    None,
                    Some(prefix_hash(9)),
                    Some(10),
                    None,
                )
                .unwrap();
            (ws, s.id)
        };
        // Reopen: migration is a no-op, all data reads back.
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.provider_call_prefix_rows(sid).unwrap();
        assert_eq!(rows.len(), 2, "the legacy row carries no observation");
        assert_eq!(rows[0].row_id + 1, rows[1].row_id);
        assert_eq!(rows[0].prompt_prefix_hash, prefix_hash(7));
        assert_eq!(rows[0].prompt_tokens, 1234);
        assert_eq!(rows[0].prefix_stability, Some(0.625));
        assert_eq!(rows[1].prompt_prefix_hash, prefix_hash(9));
        assert_eq!(rows[1].prompt_tokens, 10);
        assert_eq!(rows[1].prefix_stability, None);
        // Session isolation: another session sees none of it.
        let other = store.create_session(ws, "other", "p", "m").unwrap();
        assert!(store
            .provider_call_prefix_rows(other.id)
            .unwrap()
            .is_empty());
        // Second reopen still stable.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.provider_call_prefix_rows(sid).unwrap();
        assert_eq!(rows[0].prompt_prefix_hash, prefix_hash(7));
        assert_eq!(rows[1].prompt_tokens, 10);
    }

    /// One strict observation payload: `n` distinct 64-char hex digests and
    /// one token count per segment.
    pub(crate) fn segments_json(n: usize, tokens: &[u64]) -> String {
        let hashes: Vec<String> = (0..n)
            .map(|i| format!("{:02x}", i as u8).repeat(32))
            .collect();
        serde_json::json!({
            "segment_hashes": hashes,
            "segment_token_counts": tokens,
            "cache_read_tokens": 7u64,
        })
        .to_string()
    }

    #[test]
    fn prefix_segments_json_round_trips_across_reopen_and_legacy_rows_read_null() {
        // The v19 additive payload survives a full reopen byte-identically,
        // and rows written through the legacy API honestly read as "no
        // segment observation" — never an empty JSON object, never a guess.
        let dir = tempfile::tempdir().unwrap();
        let json = segments_json(3, &[10, 20, 30]);
        let sid = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "segments", "p", "m").unwrap();
            // Legacy prefix row: hash + tokens but no segment payload.
            store
                .record_provider_call_with_prefix(
                    s.id,
                    OpId::new(1),
                    "p",
                    "m",
                    "completed",
                    None,
                    None,
                    None,
                    Some(prefix_hash(3)),
                    Some(60),
                    None,
                )
                .unwrap();
            // v19 row: the same shape plus the observed segments.
            store
                .record_provider_call_with_prefix_segments(
                    s.id,
                    OpId::new(2),
                    "p",
                    "m",
                    "completed",
                    None,
                    None,
                    None,
                    Some(prefix_hash(4)),
                    Some(60),
                    Some(1.0),
                    Some(&json),
                )
                .unwrap();
            s.id
        };
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.provider_call_prefix_rows(sid).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].prefix_segments_json, None,
            "pre-v19 rows must read as no observation"
        );
        assert_eq!(rows[1].prefix_segments_json.as_deref(), Some(json.as_str()));
        // Reopen again: still byte-identical.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.provider_call_prefix_rows(sid).unwrap();
        assert_eq!(rows[1].prefix_segments_json.as_deref(), Some(json.as_str()));
        // A session with no rows sees none of it.
        let other = store.create_workspace("/w2").unwrap();
        let other = store.create_session(other, "other", "p", "m").unwrap();
        assert!(store
            .provider_call_prefix_rows(other.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn migration_v13_replays_cleanly_on_a_v12_store() {
        // Simulate a v12 store (provider_call without the v13 columns):
        // reopen must add the columns, keep pre-v13 rows readable as
        // observation-less, and accept full observations afterwards.
        let dir = tempfile::tempdir().unwrap();
        let (sid, row) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let row = store
                .record_provider_call(s.id, OpId::new(1), "p", "m", "ok", Some(10), Some(5), None)
                .unwrap();
            {
                let conn = store.raw_conn();
                // Rewind to the v12 layout: drop the v13 columns and the
                // schema cursor so the full chain past v13 replays.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prompt_prefix_hash",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prompt_tokens", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN prefix_stability", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too: drop them so the full chain
                // (past v20) replays cleanly.
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
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // The v14 task-revision column + verification_record table
                // are post-this-version too: drop them so the full chain
                // (past v14) replays cleanly on reopen.
                conn.execute("ALTER TABLE task DROP COLUMN revision", [])
                    .unwrap();
                conn.execute("DROP TABLE verification_record", []).unwrap();
                // The v15 cost-ledger objects are post-this-version too:
                // drop them so the full chain (past v15) replays cleanly.
                conn.execute("DROP TABLE cost_reservation", []).unwrap();
                conn.execute("ALTER TABLE task DROP COLUMN max_cost_micro", [])
                    .unwrap();
                conn.execute("ALTER TABLE task DROP COLUMN spent_cost_micro", [])
                    .unwrap();
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 13", []).unwrap();
            }
            (s.id, row)
        };
        let store = Store::open(dir.path(), true).unwrap();
        // The pre-v13 row survived and reads as observation-less.
        assert!(store.provider_call_prefix_rows(sid).unwrap().is_empty());
        let _ = row;
        // Full observations write and read through the re-migrated store.
        store
            .record_provider_call_with_prefix(
                sid,
                OpId::new(2),
                "p",
                "m",
                "ok",
                None,
                None,
                None,
                Some(prefix_hash(4)),
                Some(77),
                Some(0.9),
            )
            .unwrap();
        let rows = store.provider_call_prefix_rows(sid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompt_prefix_hash, prefix_hash(4));
        assert_eq!(rows[0].prompt_tokens, 77);
        assert_eq!(rows[0].prefix_stability, Some(0.9));
        // Reopen again: migration is a no-op and the observation persists.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.provider_call_prefix_rows(sid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompt_tokens, 77);
    }

    #[test]
    fn migration_v25_rebuilds_attachment_references_and_preserves_rows() {
        // A pre-v25 database carries the digest-keyed attachment table. Seed
        // one row in that exact shape, rewind the cursor, and reopen: the
        // rebuild must preserve the row and let a metadata-distinct reference
        // for the SAME blob coexist afterwards.
        let dir = tempfile::tempdir().unwrap();
        let digest = faktor_core::hash::FileHash::from([7; 32]);
        let sid = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "att", "p", "m").unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("DROP TABLE attachment", []).unwrap();
                conn.execute(
                    "CREATE TABLE attachment (
                        session_id INTEGER NOT NULL REFERENCES session(id),
                        digest TEXT NOT NULL,
                        mime TEXT NOT NULL,
                        filename TEXT,
                        size INTEGER NOT NULL,
                        PRIMARY KEY (session_id, digest)
                     )",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO attachment(session_id, digest, mime, filename, size)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        s.id.raw() as i64,
                        digest.to_hex(),
                        "image/png",
                        "shot.png",
                        123i64
                    ],
                )
                .unwrap();
                conn.execute("PRAGMA user_version = 25", []).unwrap();
            }
            s.id
        };
        let store = Store::open(dir.path(), true).unwrap();
        let seeded = AttachmentId::new(digest, "image/png", Some("shot.png"), 123).unwrap();
        // The pre-v25 row survived the rebuild with its exact metadata.
        assert_eq!(
            store.attachment_row(sid, &seeded).unwrap(),
            Some(seeded.clone()),
            "the pre-v25 reference must survive the rebuild"
        );
        assert_eq!(store.attachment(sid, digest).unwrap(), Some(seeded.clone()));
        // A metadata-distinct reference to the SAME blob now coexists.
        let other = AttachmentId::new(digest, "application/pdf", Some("other.pdf"), 123).unwrap();
        assert_eq!(store.put_attachment(sid, &other).unwrap(), other);
        assert_eq!(
            store.attachments_by_digest(sid, digest).unwrap(),
            vec![seeded, other]
        );
        // Reopen again: both references and the new index are durable.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.attachments_by_digest(sid, digest).unwrap().len(), 2);
    }

    #[test]
    fn migration_v28_rebuilds_artifact_identity_per_session_and_preserves_rows() {
        // A pre-v28 database carries the globally-unique cas_hash artifact
        // table. Seed one row in that exact shape, rewind the cursor, and
        // reopen: the rebuild must preserve the row (id and metadata) and let
        // a SECOND session store the same bytes with its own metadata.
        let dir = tempfile::tempdir().unwrap();
        let sid = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "att", "p", "m").unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("DROP TABLE artifact", []).unwrap();
                conn.execute(
                    "CREATE TABLE artifact (
                        id INTEGER PRIMARY KEY,
                        session_id INTEGER NOT NULL REFERENCES session(id),
                        kind TEXT NOT NULL,
                        cas_hash TEXT NOT NULL UNIQUE,
                        summary TEXT NOT NULL,
                        created_ms INTEGER NOT NULL,
                        size INTEGER NOT NULL
                     )",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO artifact(session_id, kind, cas_hash, summary, created_ms, size)
                     VALUES (?1, 'command_output', 'hash-legacy', 'legacy sum', 7, 42)",
                    params![s.id.raw() as i64],
                )
                .unwrap();
                conn.execute("PRAGMA user_version = 28", []).unwrap();
            }
            s.id
        };
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(
            store.artifact_for(sid, "hash-legacy").unwrap(),
            Some(("legacy sum".to_string(), "command_output".to_string())),
            "the pre-v28 row survives the rebuild"
        );
        // The preserved row keeps its id through the id-preserving rebuild.
        assert_eq!(
            store
                .put_artifact(sid, "command_output", "hash-legacy", "legacy sum", 42)
                .unwrap(),
            1,
            "the legacy artifact row keeps its id"
        );
        // A second session's identical bytes are no longer discarded: both
        // rows coexist with their own metadata.
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "other", "p", "m").unwrap();
        let other_id = store
            .put_artifact(s2.id, "tool_output", "hash-legacy", "other sum", 42)
            .unwrap();
        assert_ne!(other_id, 1, "per-session artifact rows coexist");
        assert_eq!(
            store.artifact_for(s2.id, "hash-legacy").unwrap(),
            Some(("other sum".to_string(), "tool_output".to_string()))
        );
        // Reopen again: both rows and the per-session uniqueness are durable.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(
            store.artifact_for(sid, "hash-legacy").unwrap(),
            Some(("legacy sum".to_string(), "command_output".to_string()))
        );
        assert_eq!(
            store.artifact_for(s2.id, "hash-legacy").unwrap(),
            Some(("other sum".to_string(), "tool_output".to_string()))
        );
        assert_eq!(
            store
                .put_artifact(sid, "command_output", "hash-legacy", "legacy sum", 42)
                .unwrap(),
            1
        );
    }

    #[test]
    fn fast_open_recovers_migrations_and_data_and_refuses_corruption() {
        // Audit 43: the fast production open must still run WAL recovery +
        // migrations and refuse a corrupt store — it just skips the full
        // scan. Data written by a full-check open must read back through a
        // fast open, and vice versa.
        let dir = tempfile::tempdir().unwrap();
        let sid = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "fast", "p", "m").unwrap();
            store
                .put_message(s.id, 1, "user", serde_json::json!({"text": "hi"}))
                .unwrap();
            s.id
        };
        let fast = Store::open_fast(dir.path()).unwrap();
        let row = fast.get_session(sid).unwrap().unwrap();
        assert_eq!(row.title, "fast");
        assert_eq!(fast.message_count(sid).unwrap(), 1);
        // The deep scan runs fine on a fast-opened store.
        assert!(fast.deep_integrity_check().unwrap().is_empty());
        // And a fast-opened store's writes survive a full-check reopen.
        let ws2 = fast.create_workspace("/w2").unwrap();
        fast.create_session(ws2, "s2", "p", "m").unwrap();
        drop(fast);
        let full = Store::open(dir.path(), true).unwrap();
        assert_eq!(full.list_sessions(None).unwrap().len(), 2);
        // Corrupt/truncated files refuse to open fast (never silently serve).
        let garbage = tempfile::tempdir().unwrap();
        std::fs::write(
            garbage.path().join("faktor-plus.db"),
            b"this is not a sqlite database at all - no magic header anywhere",
        )
        .unwrap();
        match Store::open_fast(garbage.path()) {
            Err(StoreError::Sqlite(_)) | Err(StoreError::Corrupt(_)) => {}
            other => panic!("fast open must refuse garbage, got {other:?}"),
        }
        let truncated = tempfile::tempdir().unwrap();
        std::fs::write(
            truncated.path().join("faktor-plus.db"),
            b"SQLite format 3\x00",
        )
        .unwrap();
        match Store::open_fast(truncated.path()) {
            Err(StoreError::Sqlite(_)) | Err(StoreError::Corrupt(_)) => {}
            other => panic!("fast open must refuse truncation, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod typed_ledger_tests {
    use super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        (dir, store)
    }

    #[test]
    fn migration_v14_replays_cleanly_on_a_v13_store() {
        // Simulate a v13 store (typed task rows WITHOUT the revision column,
        // no verification_record table): reopen must add the column
        // (backfilling every legacy row to revision 1), create the record
        // table, and keep legacy rows readable.
        let dir = tempfile::tempdir().unwrap();
        let (sid, tid) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let row = TaskRow {
                task_id: TaskId::new(1),
                session_id: s.id,
                goal: "legacy goal".into(),
                acceptance_criteria: vec!["cargo check".into()],
                plan: vec![],
                attachments: vec![],
                max_tokens: None,
                max_turns: None,
                spent_tokens: 0,
                spent_turns: 0,
                state: TaskState::Running,
                revision: TaskRevision::new(1),
                created_ms: 5,
                updated_ms: 5,
            };
            store.upsert_task(&row).unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("ALTER TABLE task DROP COLUMN revision", [])
                    .unwrap();
                conn.execute("DROP TABLE verification_record", []).unwrap();
                // The v15 cost-ledger objects are post-this-version too:
                // drop them so the full chain (past v15) replays cleanly.
                conn.execute("DROP TABLE cost_reservation", []).unwrap();
                conn.execute("ALTER TABLE task DROP COLUMN max_cost_micro", [])
                    .unwrap();
                conn.execute("ALTER TABLE task DROP COLUMN spent_cost_micro", [])
                    .unwrap();
                // The v17 (attempt-identity) provider_call columns are
                // post-this-version too.
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 14", []).unwrap();
            }
            (s.id, TaskId::new(1))
        };
        let store = Store::open(dir.path(), true).unwrap();
        // The pre-v14 row survived and reads back at revision 1 (DEFAULT
        // backfill), with its content intact.
        let back = store.get_task(sid, tid).unwrap().unwrap();
        assert_eq!(back.state, TaskState::Running);
        assert_eq!(back.revision, TaskRevision::new(1));
        assert_eq!(back.acceptance_criteria, vec!["cargo check".to_string()]);
        assert_eq!(back.created_ms, 5);
        // The new surface is writable and readable.
        let mut row = back.clone();
        row.revision = TaskRevision::new(2);
        store.upsert_task(&row).unwrap();
        assert_eq!(
            store.get_task(sid, tid).unwrap().unwrap().revision,
            TaskRevision::new(2)
        );
        let rec = VerificationRecordRow {
            id: VerificationRecordId::new(1),
            task_id: tid,
            revision: TaskRevision::new(2),
            workspace_id: WorkspaceId::new(1),
            worktree_id: WorktreeId::new(1),
            tree_hash: None,
            criteria: vec![],
            checks: vec![],
            changed_files: vec![],
            unrelated_changes: vec![],
            reviewer: None,
            status: VerificationStatus::Running,
            started_ms: 1,
            completed_ms: None,
        };
        let rec_id = store.verification_record_put(&rec).unwrap();
        assert_eq!(rec_id, VerificationRecordId::new(1));
        // Reopen again: migration is a no-op; both rows survive.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(
            store.get_task(sid, tid).unwrap().unwrap().revision,
            TaskRevision::new(2)
        );
        assert_eq!(
            store
                .verification_record_get(rec_id)
                .unwrap()
                .unwrap()
                .task_id,
            tid
        );
    }

    #[test]
    fn migration_v16_renames_legacy_abandoned_reservations_to_uncertain_and_adds_the_marker() {
        // Simulate a v15 store (cost_reservation WITHOUT dispatched_ms /
        // pricing_snapshot_json and with the legacy 'abandoned' state), then
        // reopen: the v16 reservation-table migration must rebuild the table
        // — legacy 'abandoned' rows become 'uncertain' (their prediction
        // KEEPS consuming the reserved amount), pre-v17 rows read as
        // never-dispatched and unpriced, and the CHECK now forbids
        // 'abandoned' outright. The store reopens through the CURRENT
        // migration chain (v16 then v17), so the legacy v15 `open` state
        // ends at the v17 `reserved` vocabulary.
        let dir = tempfile::tempdir().unwrap();
        let (sid, tid) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
            let tid = task.task_id;
            store.cost_task_cap_set(s.id, tid, Some(1_000)).unwrap();
            {
                let conn = store.raw_conn();
                // Downgrade the post-v16 objects this rewind replays (the
                // v18 attempt columns on provider_call; the v16 migration
                // itself rebuilds cost_reservation from the v15 shape
                // below).
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too (the table itself predates neither
                // test): drop them so the full chain (past v20) replays.
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
                // Rebuild the table in its v15 shape (old CHECK, no marker,
                // no snapshot column) and seed one row per legacy status.
                conn.execute("DROP TABLE cost_reservation", []).unwrap();
                conn.execute(
                    "CREATE TABLE cost_reservation (
                        reservation_id INTEGER PRIMARY KEY AUTOINCREMENT,
                        session_id INTEGER NOT NULL,
                        task_id INTEGER NOT NULL,
                        op_id INTEGER NOT NULL,
                        predicted_micro INTEGER NOT NULL,
                        status TEXT NOT NULL CHECK (status IN ('open', 'settled', 'refunded', 'abandoned')),
                        created_ms INTEGER NOT NULL,
                        settled_ms INTEGER,
                        provider_cost_micro INTEGER,
                        provider_reported_micro INTEGER,
                        route_decision_json TEXT
                     )",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO cost_reservation
                        (reservation_id, session_id, task_id, op_id, predicted_micro, status,
                         created_ms, settled_ms, provider_cost_micro, provider_reported_micro,
                         route_decision_json)
                     VALUES
                        (1, ?1, ?2, 1, 800, 'abandoned', 1, NULL, NULL, NULL, NULL),
                        (2, ?1, ?2, 2, 100, 'open', 1, NULL, NULL, NULL, NULL),
                        (3, ?1, ?2, 3, 300, 'settled', 1, 9, 250, 250, '{\"provider\":\"p\"}'),
                        (4, ?1, ?2, 4, 50, 'refunded', 1, 2, NULL, NULL, NULL)",
                    params![s.id.raw() as i64, tid.raw() as i64],
                )
                .unwrap();
                // Rewind the cursor: the v16 (index 16, target 17) and v17
                // (index 17, target 18) blocks replay on reopen.
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 16", []).unwrap();
            }
            (s.id, tid)
        };
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
        assert_eq!(rows.len(), 4, "every legacy row survived the rebuilds");
        let by_op = |op: i64| {
            rows.iter()
                .find(|r| r.op_id == OpId::new(op as u64))
                .unwrap()
                .clone()
        };
        let abandoned = by_op(1);
        assert_eq!(
            abandoned.status, "uncertain",
            "legacy 'abandoned' rows migrate to 'uncertain'"
        );
        assert_eq!(
            abandoned.predicted_micro, 800,
            "the migrated prediction is preserved"
        );
        assert_eq!(
            abandoned.dispatched_ms, None,
            "pre-v17 rows read as never-dispatched (no marker column existed)"
        );
        assert_eq!(
            abandoned.pricing_snapshot, None,
            "pre-v17 rows read as unpriced (no snapshot column existed)"
        );
        assert_eq!(
            by_op(2).status,
            "reserved",
            "the v17 migration renames legacy 'open' rows to 'reserved'"
        );
        assert_eq!(
            by_op(2).attempt_op_id,
            None,
            "legacy rows carry no attempt identity (op_id was their only op)"
        );
        assert_eq!(by_op(2).parent_op_id, Some(OpId::new(2)));
        assert_eq!(by_op(2).estimated_cost_micro, Some(100));
        assert_eq!(by_op(2).provider_reported_cost_micro, None);
        let settled = by_op(3);
        assert_eq!(settled.status, "settled");
        assert_eq!(settled.provider_cost_micro, Some(250));
        assert_eq!(settled.provider_reported_micro, Some(250));
        assert_eq!(
            settled.provider_reported_cost_micro,
            Some(250),
            "the v18 canonical provider-reported column backfills losslessly"
        );
        assert_eq!(
            settled.settled_cost_micro, None,
            "pre-v18 settlements never recorded which amount was folded: an honest NULL"
        );
        assert_eq!(
            settled.cost_basis, None,
            "pre-v18 settlements never recorded a basis: an honest NULL"
        );
        assert_eq!(settled.estimated_cost_micro, Some(300));
        assert_eq!(by_op(4).status, "refunded");
        // The migrated UNCERTAIN row's prediction KEEPS consuming free:
        // 1000 - 800 (uncertain) - 100 (reserved) = 100 free — a 101
        // reserve refuses with the typed exceeded outcome.
        let out = store
            .cost_reserve_priced(sid, tid, OpId::new(5), 101, now_ms(), None)
            .unwrap();
        assert!(
            matches!(out, CostReserveOutcome::Exceeded { free: 100 }),
            "the migrated uncertain hold consumes the reserved amount: {out:?}"
        );
        // The new CHECK forbids the legacy vocabulary outright (both
        // 'abandoned' and 'open' are gone from the v18 vocabulary).
        for hostile in ["abandoned", "open"] {
            let insert = store.raw_conn().execute(
                "INSERT INTO cost_reservation
                    (session_id, task_id, op_id, predicted_micro, status, created_ms)
                 VALUES (1, 1, 9, 1, ?1, 1)",
                [hostile],
            );
            assert!(
                matches!(&insert, Err(rusqlite::Error::SqliteFailure(..))),
                "the rebuilt CHECK rejects legacy state {hostile:?}: {insert:?}"
            );
        }
        // Reopen again: the migrations are a no-op and the data is stable.
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows.iter().filter(|r| r.status == "uncertain").count(), 1);
        assert_eq!(rows.iter().filter(|r| r.status == "reserved").count(), 1);
        assert_eq!(rows.iter().filter(|r| r.status == "settled").count(), 1);
        assert_eq!(rows.iter().filter(|r| r.status == "refunded").count(), 1);
    }

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
    fn verification_record_evidence_columns_roundtrip_and_legacy_put_stays_null() {
        // Schema v20 (audits 94/116/117): the additive evidence columns carry
        // whatever opaque bounded JSON the caller validated; the legacy put
        // path writes SQL NULL and reads back as an honest absence. Both
        // survive a reopen.
        let dir = tempfile::tempdir().unwrap();
        let (legacy, with_evidence) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into()]);
            let rec = passing_record(&task, ws, WorktreeId::new(1));
            let legacy = store.verification_record_put(&rec).unwrap();
            let fingerprint = r#"{"platform":"macos","arch":"aarch64"}"#;
            let candidate = r#"{"task_revision":1,"accounting_snapshot_digest":"accounting:v1:0"}"#;
            let with_evidence = store
                .verification_record_put_with_evidence(&rec, Some(fingerprint), Some(candidate))
                .unwrap();
            (legacy, with_evidence)
        };
        let store = Store::open(dir.path(), true).unwrap();
        let (row, fingerprint, candidate) = store
            .verification_record_get_with_evidence(legacy)
            .unwrap()
            .unwrap();
        assert_eq!(row.task_id.raw(), 1);
        assert!(fingerprint.is_none(), "legacy put writes NULL evidence");
        assert!(candidate.is_none(), "legacy put writes NULL evidence");
        let (_, fingerprint, candidate) = store
            .verification_record_get_with_evidence(with_evidence)
            .unwrap()
            .unwrap();
        assert_eq!(
            fingerprint.as_deref(),
            Some(r#"{"platform":"macos","arch":"aarch64"}"#)
        );
        assert_eq!(
            candidate.as_deref(),
            Some(r#"{"task_revision":1,"accounting_snapshot_digest":"accounting:v1:0"}"#)
        );
        let list = store
            .verification_record_list_by_task_with_evidence(TaskId::new(1))
            .unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].1.is_none());
        assert!(list[1].1.is_some());
    }

    /// Assert a read path refused a corrupt id COLUMN as a typed `Corrupt`
    /// naming the row and column — never a panic, a wrap or a minted id.
    pub(crate) fn assert_corrupt_id<T>(outcome: StoreResult<T>, what: &str, column: &str) {
        match outcome {
            Err(StoreError::Corrupt(msgs)) => assert!(
                msgs.iter().any(|m| m.contains(column)),
                "{what}: the refusal must name {column}: {msgs:?}"
            ),
            Err(e) => panic!("{what}: a corrupt {column} must refuse typed, got {e}"),
            Ok(_) => panic!("{what}: a corrupt {column} must refuse typed, got a decoded value"),
        }
    }

    /// Run one raw write with foreign keys OFF, then restore them: corrupting
    /// a referenced id column behind the typed API's back is exactly the
    /// hand-corrupted database the read-time decodes must survive.
    pub(crate) fn corrupt_ignoring_fks(store: &Store, sql: &str, params: &[&dyn rusqlite::ToSql]) {
        let conn = store.raw_conn();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(sql, params).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    }

    /// PIN (P2): a negative `i64` observed in a real id COLUMN is the
    /// intentional SQLite two's-complement legacy of a high-half `u64` id —
    /// not corruption — while zero stays typed `Corrupt`. Reads the column
    /// back through a real row mapper (`all_active_turns`), not just the
    /// decode helper.
    #[test]
    fn negative_legacy_id_column_decodes_to_the_high_half_not_corruption() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let turn_id = store
            .start_turn_record(s.id, OpId::new(5), None, None, "p", "m", None)
            .unwrap();
        for wanted in [u64::MAX, 2u64.pow(63), u64::MAX - 1] {
            let raw = wanted as i64;
            assert!(raw < 0, "the fixture must exercise a negative legacy raw");
            corrupt_ignoring_fks(
                &store,
                "UPDATE turn_record SET session_id = ?2 WHERE id = ?1",
                &[&turn_id, &raw],
            );
            assert_eq!(
                store.all_active_turns().unwrap()[0].session_id.raw(),
                wanted,
                "{raw} is the bit-cast of the high-half id {wanted}: intentional, not corruption"
            );
        }
        corrupt_ignoring_fks(
            &store,
            "UPDATE turn_record SET session_id = 0 WHERE id = ?1",
            &[&turn_id],
        );
        assert_corrupt_id(store.all_active_turns(), "all_active_turns", "session_id");
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
    fn migration_v17_turns_v16_open_rows_into_reserved_and_keeps_the_marker_truth() {
        // (ii + legacy crash window) A v16 store wrote dispatch markers
        // WITHOUT changing the row's `open` status. After the v17 migration
        // those rows read `reserved` + marker — still refund-impossible (the
        // guarded SQL requires a NULL marker) and still recovered as
        // UNCERTAIN, never as a $0 refund.
        let dir = tempfile::tempdir().unwrap();
        let (sid, tid) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
            let tid = task.task_id;
            store.cost_task_cap_set(s.id, tid, Some(1_000)).unwrap();
            {
                let conn = store.raw_conn();
                conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                    .unwrap();
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                    [],
                )
                .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                    .unwrap();
                conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                    .unwrap();
                // The v19 segment-observation column is post-this-version
                // too: drop it so the full chain (past v19) replays cleanly.
                conn.execute(
                    "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                    [],
                )
                .unwrap();
                // The v20 verification-record evidence columns are
                // post-this-version too: drop them so the full chain
                // (past v20) replays cleanly.
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
                // Rebuild the table in its EXACT v16 shape (the schema the
                // v16 writer produced: dispatched_ms + pricing_snapshot_json
                // present, vocabulary open/settled/refunded/uncertain) and
                // seed the two crash shapes the v16 writer could leave: an
                // `open` row dispatch never began and an `open` row whose
                // marker was written (dispatch may have reached the
                // provider).
                conn.execute("DROP TABLE cost_reservation", []).unwrap();
                conn.execute(
                    "CREATE TABLE cost_reservation (
                        reservation_id INTEGER PRIMARY KEY AUTOINCREMENT,
                        session_id INTEGER NOT NULL,
                        task_id INTEGER NOT NULL,
                        op_id INTEGER NOT NULL,
                        predicted_micro INTEGER NOT NULL,
                        status TEXT NOT NULL CHECK (status IN ('open', 'settled', 'refunded', 'uncertain')),
                        created_ms INTEGER NOT NULL,
                        settled_ms INTEGER,
                        dispatched_ms INTEGER,
                        pricing_snapshot_json TEXT,
                        provider_cost_micro INTEGER,
                        provider_reported_micro INTEGER,
                        route_decision_json TEXT
                     )",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO cost_reservation
                        (reservation_id, session_id, task_id, op_id, predicted_micro, status,
                         created_ms, settled_ms, dispatched_ms, pricing_snapshot_json,
                         provider_cost_micro, provider_reported_micro, route_decision_json)
                     VALUES
                        (1, ?1, ?2, 1, 200, 'open', 1, NULL, NULL, NULL, NULL, NULL, NULL),
                        (2, ?1, ?2, 2, 300, 'open', 1, NULL, 555, NULL, NULL, NULL, NULL),
                        (3, ?1, ?2, 3, 400, 'settled', 1, 9, 9, NULL, 250, 250, NULL)",
                    params![s.id.raw() as i64, tid.raw() as i64],
                )
                .unwrap();
                // Rewind the cursor to the v16 schema target: ONLY the v17
                // block (index 17, target 18) replays on reopen.
                // v24 attachments are post-this-version: drop them (tolerantly,
                // some legacy shapes lack the table/column) so the full chain
                // replays cleanly on reopen.
                let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
                let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
                let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
                conn.execute("PRAGMA user_version = 17", []).unwrap();
            }
            (s.id, tid)
        };
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
        let by_op = |op: i64| {
            rows.iter()
                .find(|r| r.op_id == OpId::new(op as u64))
                .unwrap()
                .clone()
        };
        let pre_marker = by_op(1);
        assert_eq!(pre_marker.status, "reserved");
        assert_eq!(pre_marker.dispatched_ms, None);
        let legacy_dispatched = by_op(2);
        assert_eq!(
            legacy_dispatched.status, "reserved",
            "v16 open + marker migrates to reserved + marker (nothing lossy)"
        );
        assert_eq!(legacy_dispatched.dispatched_ms, Some(555));
        assert_eq!(legacy_dispatched.parent_op_id, Some(OpId::new(2)));
        assert_eq!(legacy_dispatched.attempt_op_id, None);
        assert_eq!(by_op(3).status, "settled");
        // The refund guard reads the MARKER, not the status name: the
        // migrated dispatched row is unrefundable even though it reads
        // `reserved`.
        let out = store
            .cost_refund(by_op(2).reservation_id, now_ms())
            .unwrap();
        assert_eq!(
            out,
            RefundOutcome::Blocked {
                current: "reserved".into(),
                dispatched_ms: Some(555)
            },
            "a migrated v16 dispatch (reserved + marker) can never refund"
        );
        // Recovery reads it as may-have-dispatched -> UNCERTAIN.
        let (refunded, uncertain) = store.cost_recover_open_reservations(now_ms()).unwrap();
        assert_eq!(refunded, 1, "only the truly pre-dispatch row refunds");
        assert_eq!(uncertain, 1, "the migrated dispatched row goes UNCERTAIN");
        let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
        assert_eq!(
            rows.iter()
                .find(|r| r.reservation_id == legacy_dispatched.reservation_id)
                .unwrap()
                .status,
            "uncertain"
        );
        assert_eq!(
            free_micro(&store, sid, tid),
            700,
            "cap 1000 - 0 folded spend - 300 uncertain hold = 700 (the 200 refund is free)"
        );
        // Reopen again: the migration is a no-op and every row is stable.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.iter().filter(|r| r.status == "reserved").count(), 0);
        assert_eq!(rows.iter().filter(|r| r.status == "settled").count(), 1);
        assert_eq!(rows.iter().filter(|r| r.status == "refunded").count(), 1);
        assert_eq!(rows.iter().filter(|r| r.status == "uncertain").count(), 1);
    }
}
