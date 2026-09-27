//! `tasks`: cohesive slice of the mechanically decomposed parent module.

#![allow(unused_imports)]

use super::*;

// ------------------------------------------- verification job bounds (v22)
//
// The store backstops the session layer's bounds so a raw SQL write or a
// corrupt injected row can never outgrow the durable contract. They mirror
// `faktor_session`'s verification-job constants and the verification-record
// caps (checks <= 256, changed files <= 4096, bounded argv).

/// Changed files one attempt may certify (mirrors
/// `MAX_VERIFICATION_CHANGED_FILES`).
pub const MAX_VERIFICATION_ATTEMPT_CHANGED: usize = 4096;

/// Required checks one attempt may carry (mirrors
/// `MAX_VERIFICATION_RECORD_CHECKS`).
pub const MAX_VERIFICATION_ATTEMPT_CHECKS: usize = 256;

/// One check-id bound.
pub const MAX_VERIFICATION_JOB_CHECK_ID_BYTES: usize = 128;

/// One check kind tag bound.
pub const MAX_VERIFICATION_JOB_KIND_BYTES: usize = 16;

/// One canonical command text bound.
pub const MAX_VERIFICATION_JOB_COMMAND_BYTES: usize = 512;

/// One check program bound (mirrors `MAX_VERIFICATION_PROGRAM_BYTES`).
pub const MAX_VERIFICATION_JOB_PROGRAM_BYTES: usize = 4096;

/// Per-argument count bound (mirrors `MAX_VERIFICATION_CHECK_ARGS`).
pub const MAX_VERIFICATION_JOB_ARGS: usize = 32;

/// One argument bound (mirrors `MAX_VERIFICATION_CHECK_ARG_BYTES`).
pub const MAX_VERIFICATION_JOB_ARG_BYTES: usize = 1024;

/// One typed spec JSON bound.
pub const MAX_VERIFICATION_JOB_SPEC_JSON_BYTES: usize = 64 * 1024;

/// One result JSON bound.
pub const MAX_VERIFICATION_JOB_RESULT_JSON_BYTES: usize = 64 * 1024;

/// One job/attempt note bound.
pub const MAX_VERIFICATION_JOB_NOTE_BYTES: usize = 512;

/// One changed-file path bound (mirrors `MAX_VERIFICATION_PATH_BYTES`).
pub const MAX_VERIFICATION_ATTEMPT_PATH_BYTES: usize = 4096;

/// One attempt workspace-root bound.
pub const MAX_VERIFICATION_JOB_ROOT_BYTES: usize = 4096;

/// One job execution budget bound (ms).
pub const MAX_VERIFICATION_JOB_BUDGET_MS: u64 = 3_600_000;

/// One environment-fingerprint JSON column bound.
pub const MAX_VERIFICATION_JOB_FINGERPRINT_JSON_BYTES: usize = 64 * 1024;

/// The job states a row may durably hold.
pub const VERIFICATION_JOB_STATES: [&str; 6] = [
    "queued",
    "running",
    "passed",
    "failed",
    "unavailable",
    "cancelled",
];

/// The inline outcome states an inline check row may hold.
pub const VERIFICATION_INLINE_STATES: [&str; 3] = ["passed", "failed", "unavailable"];

/// One first-class durable Task row (audit 25, schema v10). Typed columns,
/// one row per `(session_id, task_id)`; the legacy one-row-per-session
/// ledger blob lives in the renamed `task_ledger` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub goal: String,
    pub acceptance_criteria: Vec<String>,
    /// Ordered steps, append-only durable.
    pub plan: Vec<String>,
    /// Durable typed binary/image attachments of the task (schema v24,
    /// migration index 24). SEPARATE from the workspace-relative `files`
    /// vocabulary: bytes live in the CAS under each `digest`, the metadata
    /// rides this JSON column. Rows written before v24 decode with an empty
    /// list (`'[]'` column default) — the attachment-free row stays
    /// byte-identical.
    pub attachments: Vec<AttachmentId>,
    pub max_tokens: Option<u64>,
    pub max_turns: Option<u32>,
    pub spent_tokens: u64,
    pub spent_turns: u32,
    pub state: TaskState,
    /// Per-row monotonic revision (schema v14, audit P0-7): every effective
    /// state/criteria/plan/budget mutation bumps it exactly once in the same
    /// transaction. It is the row's optimistic-lock token: the completion
    /// path refuses a record that does not certify the current revision.
    pub revision: TaskRevision,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// One first-class durable verification record (audit P0-8, schema v14):
/// the completion proof of a task. A record certifies ONE task revision
/// (`revision` == the task row's revision when the verification ran): it
/// names the task, its base worktree identity, the acceptance-criterion
/// verdicts that cover the task's criteria at that revision, the executed
/// checks, the workspace files observed with their digests, and the final
/// [`VerificationStatus`].
///
/// Records are immutable once created EXCEPT the single CAS status
/// transition `Running -> Passed|Failed`
/// ([`Store::verification_record_finalize`]): a record finalizes exactly
/// once, and an already-final record refuses a second completion attempt
/// with a typed error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRecordRow {
    pub id: VerificationRecordId,
    pub task_id: TaskId,
    pub revision: TaskRevision,
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    /// HEX digest text of the verified tree state (NULL when no tree hash
    /// was derivable — an honest "no tree observation", never a guess).
    pub tree_hash: Option<String>,
    pub criteria: Vec<CriterionVerification>,
    pub checks: Vec<CheckExecution>,
    pub changed_files: Vec<FileStateEvidence>,
    /// Paths of workspace changes the verification judged unrelated to the
    /// task (bounded text list).
    pub unrelated_changes: Vec<String>,
    /// Opaque reviewer observation (protocol-agnostic JSON, NULL when no
    /// review ran).
    pub reviewer: Option<serde_json::Value>,
    pub status: VerificationStatus,
    pub started_ms: i64,
    pub completed_ms: Option<i64>,
}

/// One verification-record row plus its raw schema-v20 evidence JSON columns:
/// `(row, environment_fingerprint_json, candidate_proof_ref_json)`. Either
/// `None` is an honest absence (a pre-v20 row or a record written without
/// that evidence); the session layer parses non-null values loudly.
pub type VerificationRecordWithEvidence = (VerificationRecordRow, Option<String>, Option<String>);

// ------------------------------------------------ verification jobs (v22)

/// One durable verification attempt (schema v22, audit P0-5/26): the
/// attempt's identity `(session_id, task_id, attempt_op_id)`, the task
/// revision it certified at enqueue, its workspace root and the bounded
/// environment fingerprint JSON. Changed files live in
/// `verification_attempt_changed_file`; every required check (inline AND
/// background) lives in `verification_job`, keyed by the same attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationAttemptRow {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub attempt_op_id: u64,
    pub task_revision: TaskRevision,
    pub workspace_root: String,
    pub environment_fingerprint_json: Option<String>,
    pub created_ms: i64,
}

/// One durable required check of one attempt (schema v22). Inline checks
/// carry `inline_status` and are terminal from birth; background checks
/// carry `spec_json` and walk `queued -> running -> terminal`. The executed
/// outcome JSON of a background check lands in `verification_job_result`
/// (keyed by the same identity) exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationJobRow {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub attempt_op_id: u64,
    pub check_id: String,
    /// Derivation order inside the attempt (0-based).
    pub ordinal: u32,
    pub task_revision: TaskRevision,
    pub workspace_root: String,
    /// `compile` | `test` | `lint`.
    pub kind: String,
    /// Canonical command text (`program arg...`).
    pub command: String,
    /// The bounded argv identity (validated by the session layer).
    pub program: String,
    pub args_json: String,
    /// The typed spec JSON (background checks only; None for inline checks).
    pub spec_json: Option<String>,
    pub budget_ms: u64,
    /// `passed` | `failed` | `unavailable` for an INLINE check; None for a
    /// background check.
    pub inline_status: Option<String>,
    /// `queued` | `running` | `passed` | `failed` | `unavailable` |
    /// `cancelled`.
    pub state: String,
    /// The executed background outcome JSON (joined from
    /// `verification_job_result`; None for inline checks and unresolved
    /// background checks).
    pub result_json: Option<String>,
    pub note: Option<String>,
    pub op_id: Option<u64>,
    pub environment_fingerprint_json: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub finished_ms: Option<i64>,
}

/// One attempt together with its changed files and every required check, in
/// derivation order (changed files by ordinal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationAttemptView {
    pub attempt: VerificationAttemptRow,
    pub changed: Vec<String>,
    pub checks: Vec<VerificationJobRow>,
}

/// Typed refusal of one `verification_job_claim`/`verification_job_resolve`
/// transition. The job row is NEVER changed by a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationJobRefusal {
    /// No job row for this `(session, task, attempt, check)`.
    Missing { check_id: String },
    /// The job is not in the required source state (only `queued` claims;
    /// only `running` resolves).
    NotOpen { check_id: String, state: String },
    /// A NEWER attempt exists for this task: attempt N can never be mutated
    /// (nor late-resolved) after N+1 was durably begun.
    Superseded {
        attempt_op_id: u64,
        newest_attempt_op_id: u64,
    },
    /// A result row already exists for this attempt/check: a result is
    /// written exactly once.
    ResultExists { check_id: String },
}

/// Result of one successful recover sweep: `requeued` Running rows became
/// Queued; `orphaned` open rows had no attempt (only possible on a hand-
/// corrupted database — foreign keys make torn begins impossible).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerificationJobRecovery {
    pub requeued: u64,
    pub orphaned: u64,
}

/// Typed refusal of a `task_complete_verified` request: every check the
/// completion transaction performs names its own variant, so callers can
/// distinguish a missing record from a wrong-revision record from an
/// uncovered criterion without parsing prose. The task row is NEVER changed
/// by a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCompletionRefusal {
    /// No task row for `(session_id, task_id)` (or the session row itself is
    /// missing).
    TaskMissing { task_id: TaskId },
    /// The task's current revision differs from the expected one (the task
    /// changed since the caller's read: re-read and re-decide).
    RevisionMismatch {
        expected: TaskRevision,
        actual: TaskRevision,
    },
    /// (a) only a `Verifying` task may be completed.
    NotVerifying { actual: TaskState },
    /// (b) no verification record row exists with this id.
    RecordMissing { record_id: VerificationRecordId },
    /// (c) the record certifies a different task.
    RecordWrongTask {
        record_id: VerificationRecordId,
        record_task: TaskId,
        requested: TaskId,
    },
    /// (d) the record certifies a different revision of the task.
    RecordWrongRevision {
        record_id: VerificationRecordId,
        record_revision: TaskRevision,
        expected: TaskRevision,
    },
    /// (e) only a `Passed` record certifies completion.
    RecordNotPassed {
        record_id: VerificationRecordId,
        status: VerificationStatus,
    },
    /// (f) the record does not cover (present with `passed = true`) every
    /// current acceptance criterion of the task.
    CriteriaNotCovered {
        record_id: VerificationRecordId,
        missing: Vec<String>,
    },
    /// (g) the record was certified against a different base worktree than
    /// the task's session currently stands on.
    WorktreeMismatch {
        record_id: VerificationRecordId,
        record_workspace: WorkspaceId,
        record_worktree: WorktreeId,
        task_workspace: WorkspaceId,
        task_worktree: WorktreeId,
    },
    /// (h) reservation rows of this task still hold budget (`reserved`,
    /// `dispatched` or `uncertain`): the accounting-before-completion gate
    /// refuses inside the completion transaction — a reserve that raced the
    /// session layer's accounting pass is caught HERE and the task stays
    /// Verifying. Nothing is transitioned.
    ReservationsHeld {
        reserved: usize,
        dispatched: usize,
        reserved_micro: u64,
        uncertain: usize,
        uncertain_micro: u64,
    },
}

/// Typed refusal of a record-finalize CAS. A record finalizes exactly once
/// (`Running -> Passed|Failed`); anything else is refused here with the
/// record's current status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordFinalizeRefusal {
    Missing {
        record_id: VerificationRecordId,
    },
    NotRunning {
        record_id: VerificationRecordId,
        current: VerificationStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRunRow {
    pub id: i64,
    pub session_id: SessionId,
    pub op_id: OpId,
    pub tool: String,
    pub args: serde_json::Value,
    pub status: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub effect_status: String,
    pub recovery: serde_json::Value,
    pub expected_hash: Option<String>,
    /// Durable replay descriptor (v7+): the stored invocation crash recovery
    /// may re-execute ONCE for idempotent tools. NULL on legacy rows.
    pub replay_descriptor: Option<serde_json::Value>,
    /// Physical attempt counter of the SAME logical operation (v7+): the
    /// original run is attempt 0; each crash recovery replay bumps it.
    pub attempt: i64,
    /// Durable workspace-write postcondition (v7+): `{workspace_id,
    /// worktree_id, relative_path, expected_hash}` — the hash of the ACTUAL
    /// bytes as written, recorded by the tool at execution end. NULL until
    /// the tool reports it (or for non-write tools).
    pub postcondition: Option<serde_json::Value>,
}

/// One durable logical-turn record (v7). Created transactionally when a
/// prompt is admitted as the ACTIVE logical turn (submit_prompt / queue
/// admission); it fixes the turn's exact operation identity and effective
/// model/provider envelope so crash recovery resumes the SAME turn with the
/// SAME identity instead of synthesizing a fresh operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecordRow {
    pub id: i64,
    pub session_id: SessionId,
    pub turn_op_id: OpId,
    /// Durable queue seq when the turn was admitted from the prompt queue.
    pub queue_seq: Option<i64>,
    /// Durable message seq of the materialized user prompt.
    pub prompt_message_id: Option<i64>,
    pub effective_provider: String,
    pub effective_model: String,
    /// Reasoning mode / variant of the logical turn (NULL when unset).
    pub variant: Option<String>,
    /// Tool-call parsing mode of the logical turn (NULL until driven).
    pub tool_mode: Option<String>,
    pub started_at: i64,
    /// active | completed | cancelled | failed
    pub status: String,
    pub updated_ms: i64,
}

pub const TURN_RECORD_ACTIVE: &str = "active";

pub const TURN_RECORD_COMPLETED: &str = "completed";

pub const TURN_RECORD_CANCELLED: &str = "cancelled";

pub const TURN_RECORD_FAILED: &str = "failed";

/// The ONE terminal-status predicate of a durable `turn_record` row: a
/// record is terminal exactly when it is no longer [`TURN_RECORD_ACTIVE`] —
/// [`TURN_RECORD_COMPLETED`], [`TURN_RECORD_CANCELLED`] or
/// [`TURN_RECORD_FAILED`] (the three statuses [`Store::finish_turn_record`]
/// accepts). Callers that wait for a turn to finalize use THIS predicate
/// instead of re-spelling `!= "active"`, so the vocabulary cannot drift.
pub fn is_terminal_turn_record_status(status: &str) -> bool {
    status != TURN_RECORD_ACTIVE
}

/// One verification-record-vs-task consistency violation (P0-97
/// `doctor --deep` wave-16 scan). Each row names its kind so doctor can
/// count per kind and print typed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationInvariantIssue {
    pub kind: &'static str,
    pub detail: String,
}

/// The wave-16 verification-consistency invariant scan (`doctor --deep`,
/// read-only): records must reference existing task rows, a `Passed` record
/// may only certify a task's current revision when that task is
/// `VerifiedComplete`, and a `VerifiedComplete` task must carry the `Passed`
/// record the completion transaction consumed (the record certifies
/// `revision - 1`: completion bumps the row revision exactly once after
/// validating the record against the pre-completion revision).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerificationInvariantScan {
    /// Every `verification_record` row, regardless of status.
    pub total_records: u64,
    /// Every `task` row in a completion-relevant state
    /// (NeedsVerification / Verifying / VerifiedComplete).
    pub relevant_tasks: u64,
    /// `VerifiedComplete` task rows.
    pub completed_tasks: u64,
    pub issues: Vec<VerificationInvariantIssue>,
}

/// One active logical-turn row that no durable path can recover after a
/// daemon crash (read-only `doctor --deep` scan): no prompt message row, no
/// queue row, no journal event and no tool-run row reference the turn's op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnrecoverableActiveTurn {
    pub record_id: i64,
    pub session_id: SessionId,
    pub turn_op_id: OpId,
    pub detail: String,
}

/// Active-turn recoverable-owner scan (`doctor --deep`, read-only): a live
/// daemon legitimately owns active rows in memory, so the check is what a
/// CRASHED daemon needs — a durable record, prompt message or queue row and
/// a replayable journal (event or tool-run row naming the turn op).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnOwnershipScan {
    /// Active `turn_record` rows across every session.
    pub active_turns: u64,
    /// Active rows with at least one durable recovery path.
    pub recoverable: u64,
    pub unrecoverable: Vec<UnrecoverableActiveTurn>,
}

pub(crate) fn task_row_map(r: &rusqlite::Row<'_>, session_id: SessionId) -> StoreResult<TaskRow> {
    let task_raw: i64 = r.get(0)?;
    let task_id = id_field::<TaskId>(&format!("task {session_id}/{task_raw} task_id"), task_raw)?;
    // The revision column is DEFAULT 1 and every write bumps it: zero is
    // corruption (a revision 0 would silently break the completion CAS).
    // Negative raw values are the bit-cast upper half and decode exactly.
    let revision = id_field::<TaskRevision>(
        &format!("task {session_id}/{task_id} revision"),
        r.get::<_, i64>(12)?,
    )?;
    Ok(TaskRow {
        task_id,
        session_id,
        goal: r.get(2)?,
        acceptance_criteria: parse_json(
            &format!("task {session_id}/{task_id} acceptance_criteria"),
            &r.get::<_, String>(3)?,
        )?,
        plan: parse_json(
            &format!("task {session_id}/{task_id} plan"),
            &r.get::<_, String>(4)?,
        )?,
        attachments: parse_json(
            &format!("task {session_id}/{task_id} attachments"),
            &r.get::<_, String>(13)?,
        )?,
        max_tokens: r.get::<_, Option<i64>>(5)?.map(|m| m.max(0) as u64),
        max_turns: match r.get::<_, Option<i64>>(6)? {
            Some(m) => Some(u32_field(
                &format!("task {session_id}/{task_id} max_turns"),
                m,
            )?),
            None => None,
        },
        spent_tokens: r.get::<_, i64>(7)?.max(0) as u64,
        spent_turns: u32_field(
            &format!("task {session_id}/{task_id} spent_turns"),
            r.get::<_, i64>(8)?,
        )?,
        state: parse_json(
            &format!("task {session_id}/{task_id} state"),
            &r.get::<_, String>(9)?,
        )?,
        revision,
        created_ms: r.get(10)?,
        updated_ms: r.get(11)?,
    })
}

pub(crate) fn verification_record_map(r: &rusqlite::Row<'_>) -> StoreResult<VerificationRecordRow> {
    let id_raw: i64 = r.get(0)?;
    let id = id_field::<VerificationRecordId>(&format!("verification_record id {id_raw}"), id_raw)?;
    // Same corruption contract as task revisions: a record certifying a
    // revision below 1 could never have been written by the API; negative
    // raw values are the bit-cast upper half and decode exactly.
    let revision = id_field::<TaskRevision>(
        &format!("verification_record {id} revision"),
        r.get::<_, i64>(2)?,
    )?;
    Ok(VerificationRecordRow {
        id,
        task_id: id_field(
            &format!("verification_record {id} task_id"),
            r.get::<_, i64>(1)?,
        )?,
        revision,
        workspace_id: id_field(
            &format!("verification_record {id} workspace_id"),
            r.get::<_, i64>(3)?,
        )?,
        worktree_id: id_field(
            &format!("verification_record {id} worktree_id"),
            r.get::<_, i64>(4)?,
        )?,
        tree_hash: r.get(5)?,
        criteria: parse_json(
            &format!("verification_record {id} criteria"),
            &r.get::<_, String>(6)?,
        )?,
        checks: parse_json(
            &format!("verification_record {id} checks"),
            &r.get::<_, String>(7)?,
        )?,
        changed_files: parse_json(
            &format!("verification_record {id} changed_files"),
            &r.get::<_, String>(8)?,
        )?,
        unrelated_changes: parse_json(
            &format!("verification_record {id} unrelated_changes"),
            &r.get::<_, String>(9)?,
        )?,
        reviewer: match r.get::<_, Option<String>>(10)? {
            Some(raw) => Some(parse_json(
                &format!("verification_record {id} reviewer"),
                &raw,
            )?),
            None => None,
        },
        status: parse_json(
            &format!("verification_record {id} status"),
            &r.get::<_, String>(11)?,
        )?,
        started_ms: r.get(12)?,
        completed_ms: r.get(13)?,
    })
}

/// The shared `verification_job` projection: the row columns in a fixed
/// order, with the executed outcome JSON joined from
/// `verification_job_result` (NULL until a background check resolves).
pub(crate) const VERIFICATION_JOB_SELECT: &str = "SELECT session_id, task_id, attempt_op_id, check_id, ordinal, task_revision, workspace_root, kind, command, program, args_json, spec_json, budget_ms, inline_status, state, note, op_id, environment_fingerprint_json, created_ms, updated_ms, finished_ms, (SELECT result_json FROM verification_job_result r WHERE r.session_id = verification_job.session_id AND r.task_id = verification_job.task_id AND r.attempt_op_id = verification_job.attempt_op_id AND r.check_id = verification_job.check_id) FROM verification_job";

pub(crate) fn verification_job_map(row: &rusqlite::Row<'_>) -> StoreResult<VerificationJobRow> {
    let session_raw: i64 = row.get(0)?;
    let task_raw: i64 = row.get(1)?;
    let attempt_raw: i64 = row.get(2)?;
    // Attempt op ids and task revisions are u64 ids bit-cast into the signed
    // column on write: the inverse cast round-trips the upper half exactly,
    // while the id constructors still refuse a structurally invalid zero.
    let attempt_op_id = id_field::<OpId>(
        &format!("verification_job attempt_op_id {attempt_raw}"),
        attempt_raw,
    )?
    .raw();
    let task_revision = id_field::<TaskRevision>(
        &format!("verification_job task_revision {session_raw}/{task_raw}/{attempt_raw}"),
        row.get::<_, i64>(5)?,
    )?;
    let check_id: String = row.get(3)?;
    let state: String = row.get(14)?;
    if !VERIFICATION_JOB_STATES.contains(&state.as_str()) {
        return Err(StoreError::Malformed(format!(
            "verification_job '{check_id}' carries unknown state {state:?}"
        )));
    }
    let inline_status: Option<String> = row.get(13)?;
    if let Some(inline) = &inline_status {
        if !VERIFICATION_INLINE_STATES.contains(&inline.as_str()) {
            return Err(StoreError::Malformed(format!(
                "verification_job '{check_id}' carries unknown inline status {inline:?}"
            )));
        }
        if state != *inline {
            return Err(StoreError::Malformed(format!(
                "verification_job '{check_id}' inline status {inline:?} disagrees with its state {state:?}"
            )));
        }
    }
    Ok(VerificationJobRow {
        session_id: id_field(
            &format!("verification_job session_id {session_raw}"),
            session_raw,
        )?,
        task_id: id_field(&format!("verification_job task_id {task_raw}"), task_raw)?,
        attempt_op_id,
        check_id,
        ordinal: u32::try_from(row.get::<_, i64>(4)?.max(0)).unwrap_or(u32::MAX),
        task_revision,
        workspace_root: row.get(6)?,
        kind: row.get(7)?,
        command: row.get(8)?,
        program: row.get(9)?,
        args_json: row.get(10)?,
        spec_json: row.get(11)?,
        budget_ms: row.get::<_, i64>(12)?.max(0) as u64,
        inline_status,
        state,
        note: row.get(15)?,
        op_id: match row.get::<_, Option<i64>>(16)? {
            Some(op) => Some(id_field::<OpId>(&format!("verification_job op_id {op}"), op)?.raw()),
            None => None,
        },
        environment_fingerprint_json: row.get(17)?,
        created_ms: row.get(18)?,
        updated_ms: row.get(19)?,
        finished_ms: row.get(20)?,
        result_json: row.get(21)?,
    })
}

pub(crate) fn verification_attempt_map(
    row: &rusqlite::Row<'_>,
) -> StoreResult<VerificationAttemptRow> {
    let session_raw: i64 = row.get(0)?;
    let task_raw: i64 = row.get(1)?;
    let attempt_raw: i64 = row.get(2)?;
    // Same bit-cast contract as the verification_job projection: the upper
    // half of the u64 id space round-trips; zero stays typed corruption.
    let attempt_op_id = id_field::<OpId>(
        &format!("verification_attempt attempt_op_id {attempt_raw}"),
        attempt_raw,
    )?
    .raw();
    let task_revision = id_field::<TaskRevision>(
        &format!("verification_attempt task_revision {session_raw}/{task_raw}/{attempt_raw}"),
        row.get::<_, i64>(3)?,
    )?;
    Ok(VerificationAttemptRow {
        session_id: id_field(
            &format!("verification_attempt session_id {session_raw}"),
            session_raw,
        )?,
        task_id: id_field(
            &format!("verification_attempt task_id {task_raw}"),
            task_raw,
        )?,
        attempt_op_id,
        task_revision,
        workspace_root: row.get(4)?,
        environment_fingerprint_json: row.get(5)?,
        created_ms: row.get(6)?,
    })
}

/// The highest attempt op of `(session, task)`, if any.
pub(crate) fn newest_attempt_op(
    conn: &Connection,
    session_id: SessionId,
    task_id: TaskId,
) -> StoreResult<Option<u64>> {
    let newest: Option<i64> = conn
        .query_row(
            "SELECT MAX(attempt_op_id) FROM verification_attempt
             WHERE session_id = ?1 AND task_id = ?2",
            params![session_id.raw() as i64, task_id.raw() as i64],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    // The column carries u64 op ids bit-cast into i64: the inverse cast is
    // the lossless decode (a zero attempt names no real row and is refused
    // by the caller's lookup path, which simply finds nothing).
    Ok(newest.map(|op| op as u64))
}

/// Fetch one background job row by identity, or `None`.
pub(crate) fn verification_job_get(
    conn: &Connection,
    session_id: SessionId,
    task_id: TaskId,
    attempt_op_id: u64,
    check_id: &str,
) -> StoreResult<Option<VerificationJobRow>> {
    let sql = format!(
        "{} WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3 AND check_id = ?4",
        VERIFICATION_JOB_SELECT
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![
        session_id.raw() as i64,
        task_id.raw() as i64,
        attempt_op_id as i64,
        check_id
    ])?;
    match rows.next()? {
        Some(row) => Ok(Some(verification_job_map(row)?)),
        None => Ok(None),
    }
}

/// Build one attempt view (attempt + changed files + every required check in
/// derivation order), or `None` when the attempt row does not exist.
pub(crate) fn verification_attempt_view(
    conn: &Connection,
    session_id: SessionId,
    task_id: TaskId,
    attempt_op_id: u64,
) -> StoreResult<Option<VerificationAttemptView>> {
    let attempt = {
        let mut stmt = conn.prepare(
            "SELECT session_id, task_id, attempt_op_id, task_revision, workspace_root,
                    environment_fingerprint_json, created_ms
             FROM verification_attempt
             WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3",
        )?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            task_id.raw() as i64,
            attempt_op_id as i64
        ])?;
        match rows.next()? {
            Some(row) => Some(verification_attempt_map(row)?),
            None => None,
        }
    };
    let Some(attempt) = attempt else {
        return Ok(None);
    };
    let mut changed: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT path FROM verification_attempt_changed_file
             WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3
             ORDER BY ordinal ASC",
        )?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            task_id.raw() as i64,
            attempt_op_id as i64
        ])?;
        while let Some(row) = rows.next()? {
            changed.push(row.get(0)?);
        }
    }
    let mut checks: Vec<VerificationJobRow> = Vec::new();
    {
        let sql = format!(
            "{} WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3
             ORDER BY ordinal ASC, check_id ASC",
            VERIFICATION_JOB_SELECT
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            task_id.raw() as i64,
            attempt_op_id as i64
        ])?;
        while let Some(row) = rows.next()? {
            checks.push(verification_job_map(row)?);
        }
    }
    Ok(Some(VerificationAttemptView {
        attempt,
        changed,
        checks,
    }))
}

/// Enforce the v22 durable bounds on one attempt begin: every check belongs
/// to the attempt, ids are non-zero, every text field is bounded, argv is
/// bounded (count and per-argument bytes), inline checks carry an inline
/// outcome and background checks carry a bounded spec and budget. Oversized
/// input is a typed rejection before ANY write, never a truncation.
pub(crate) fn validate_verification_attempt(
    attempt: &VerificationAttemptRow,
    changed: &[String],
    checks: &[VerificationJobRow],
) -> StoreResult<()> {
    let malformed = |what: String| StoreError::Malformed(what);
    let oversized = |what: String| StoreError::Oversized(what);
    if attempt.session_id.raw() == 0
        || attempt.task_id.raw() == 0
        || attempt.attempt_op_id == 0
        || attempt.task_revision.raw() == 0
    {
        return Err(malformed(
            "verification attempt identity must be non-zero".into(),
        ));
    }
    if attempt.workspace_root.is_empty()
        || attempt.workspace_root.len() > MAX_VERIFICATION_JOB_ROOT_BYTES
    {
        return Err(oversized(format!(
            "attempt workspace_root of {} bytes outside 1..={MAX_VERIFICATION_JOB_ROOT_BYTES}",
            attempt.workspace_root.len()
        )));
    }
    if let Some(fp) = &attempt.environment_fingerprint_json {
        if fp.is_empty() || fp.len() > MAX_VERIFICATION_JOB_FINGERPRINT_JSON_BYTES {
            return Err(oversized(format!(
                "environment fingerprint JSON of {} bytes outside 1..={MAX_VERIFICATION_JOB_FINGERPRINT_JSON_BYTES}",
                fp.len()
            )));
        }
    }
    if changed.len() > MAX_VERIFICATION_ATTEMPT_CHANGED {
        return Err(oversized(format!(
            "{} changed files exceed MAX_VERIFICATION_ATTEMPT_CHANGED ({MAX_VERIFICATION_ATTEMPT_CHANGED})",
            changed.len()
        )));
    }
    for path in changed {
        if path.is_empty() || path.len() > MAX_VERIFICATION_ATTEMPT_PATH_BYTES {
            return Err(oversized(format!(
                "changed path of {} bytes outside 1..={MAX_VERIFICATION_ATTEMPT_PATH_BYTES}",
                path.len()
            )));
        }
    }
    if checks.is_empty() || checks.len() > MAX_VERIFICATION_ATTEMPT_CHECKS {
        return Err(oversized(format!(
            "{} required checks outside 1..={MAX_VERIFICATION_ATTEMPT_CHECKS}",
            checks.len()
        )));
    }
    let mut ordinal_seen = std::collections::HashSet::new();
    let mut check_seen = std::collections::HashSet::new();
    for check in checks {
        if check.session_id != attempt.session_id
            || check.task_id != attempt.task_id
            || check.attempt_op_id != attempt.attempt_op_id
        {
            return Err(malformed(format!(
                "check '{}' does not belong to its attempt identity",
                check.check_id
            )));
        }
        if check.check_id.is_empty() || check.check_id.len() > MAX_VERIFICATION_JOB_CHECK_ID_BYTES {
            return Err(oversized(format!(
                "check_id of {} bytes outside 1..={MAX_VERIFICATION_JOB_CHECK_ID_BYTES}",
                check.check_id.len()
            )));
        }
        if !check_seen.insert(check.check_id.clone()) {
            return Err(malformed(format!(
                "duplicate check '{}' in one attempt",
                check.check_id
            )));
        }
        if !ordinal_seen.insert(check.ordinal) {
            return Err(malformed(format!(
                "duplicate derivation ordinal {} in one attempt",
                check.ordinal
            )));
        }
        if check.command.is_empty() || check.command.len() > MAX_VERIFICATION_JOB_COMMAND_BYTES {
            return Err(oversized(format!(
                "check command of {} bytes outside 1..={MAX_VERIFICATION_JOB_COMMAND_BYTES}",
                check.command.len()
            )));
        }
        if check.program.len() > MAX_VERIFICATION_JOB_PROGRAM_BYTES {
            return Err(oversized(format!(
                "check program of {} bytes exceeds MAX_VERIFICATION_JOB_PROGRAM_BYTES ({MAX_VERIFICATION_JOB_PROGRAM_BYTES})",
                check.program.len()
            )));
        }
        let args: Vec<String> = parse_json(
            &format!("verification job {} args", check.check_id),
            &check.args_json,
        )?;
        if args.len() > MAX_VERIFICATION_JOB_ARGS {
            return Err(oversized(format!(
                "{} check args exceed MAX_VERIFICATION_JOB_ARGS ({MAX_VERIFICATION_JOB_ARGS})",
                args.len()
            )));
        }
        for arg in &args {
            if arg.len() > MAX_VERIFICATION_JOB_ARG_BYTES {
                return Err(oversized(format!(
                    "a check arg of {} bytes exceeds MAX_VERIFICATION_JOB_ARG_BYTES ({MAX_VERIFICATION_JOB_ARG_BYTES})",
                    arg.len()
                )));
            }
        }
        if let Some(note) = &check.note {
            if note.is_empty() || note.len() > MAX_VERIFICATION_JOB_NOTE_BYTES {
                return Err(oversized(format!(
                    "job note of {} bytes outside 1..={MAX_VERIFICATION_JOB_NOTE_BYTES}",
                    note.len()
                )));
            }
        }
        if check.op_id == Some(0) {
            return Err(malformed(format!(
                "check '{}' carries a zero op id",
                check.check_id
            )));
        }
        if let Some(fp) = &check.environment_fingerprint_json {
            if fp.is_empty() || fp.len() > MAX_VERIFICATION_JOB_FINGERPRINT_JSON_BYTES {
                return Err(oversized(format!(
                    "job fingerprint JSON of {} bytes outside 1..={MAX_VERIFICATION_JOB_FINGERPRINT_JSON_BYTES}",
                    fp.len()
                )));
            }
        }
        match &check.inline_status {
            Some(inline) => {
                if !VERIFICATION_INLINE_STATES.contains(&inline.as_str()) {
                    return Err(malformed(format!(
                        "inline status {inline:?} is not one of {VERIFICATION_INLINE_STATES:?}"
                    )));
                }
                if check.spec_json.is_some() {
                    return Err(malformed(format!(
                        "inline check '{}' must not carry a background spec",
                        check.check_id
                    )));
                }
                if check.state != *inline {
                    return Err(malformed(format!(
                        "inline check '{}' state {:?} must equal its inline status {inline:?}",
                        check.check_id, check.state
                    )));
                }
            }
            None => {
                let Some(spec) = &check.spec_json else {
                    return Err(malformed(format!(
                        "background check '{}' must carry its typed spec",
                        check.check_id
                    )));
                };
                if check.kind.is_empty() || check.kind.len() > MAX_VERIFICATION_JOB_KIND_BYTES {
                    return Err(oversized(format!(
                        "check kind of {} bytes outside 1..={MAX_VERIFICATION_JOB_KIND_BYTES}",
                        check.kind.len()
                    )));
                }
                if spec.is_empty() || spec.len() > MAX_VERIFICATION_JOB_SPEC_JSON_BYTES {
                    return Err(oversized(format!(
                        "job spec JSON of {} bytes outside 1..={MAX_VERIFICATION_JOB_SPEC_JSON_BYTES}",
                        spec.len()
                    )));
                }
                if check.budget_ms == 0 || check.budget_ms > MAX_VERIFICATION_JOB_BUDGET_MS {
                    return Err(oversized(format!(
                        "job budget_ms {} outside 1..={MAX_VERIFICATION_JOB_BUDGET_MS}",
                        check.budget_ms
                    )));
                }
                if !VERIFICATION_JOB_STATES.contains(&check.state.as_str()) {
                    return Err(malformed(format!(
                        "job state {:?} is not one of {VERIFICATION_JOB_STATES:?}",
                        check.state
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn tool_run_map(r: &rusqlite::Row<'_>) -> StoreResult<ToolRunRow> {
    let id = r.get::<_, i64>(0)?;
    Ok(ToolRunRow {
        id,
        session_id: id_field(&format!("tool_run {id} session_id"), r.get::<_, i64>(1)?)?,
        op_id: id_field(&format!("tool_run {id} op_id"), r.get::<_, i64>(2)?)?,
        tool: r.get(3)?,
        args: parse_json(&format!("tool_run {id} args"), &r.get::<_, String>(4)?)?,
        status: r.get(5)?,
        started_ms: r.get(6)?,
        ended_ms: r.get(7)?,
        effect_status: r.get(8)?,
        recovery: parse_json(&format!("tool_run {id} recovery"), &r.get::<_, String>(9)?)?,
        expected_hash: r.get(10)?,
        replay_descriptor: match r.get::<_, Option<String>>(11)? {
            Some(raw) => Some(parse_json(
                &format!("tool_run {id} replay_descriptor"),
                &raw,
            )?),
            None => None,
        },
        attempt: r.get(12)?,
        postcondition: match r.get::<_, Option<String>>(13)? {
            Some(raw) => Some(parse_json(&format!("tool_run {id} postcondition"), &raw)?),
            None => None,
        },
    })
}

pub(crate) fn turn_record_map(r: &rusqlite::Row<'_>) -> StoreResult<TurnRecordRow> {
    let id = r.get::<_, i64>(0)?;
    Ok(TurnRecordRow {
        id,
        session_id: id_field(&format!("turn_record {id} session_id"), r.get::<_, i64>(1)?)?,
        turn_op_id: id_field(&format!("turn_record {id} turn_op_id"), r.get::<_, i64>(2)?)?,
        queue_seq: r.get(3)?,
        prompt_message_id: r.get(4)?,
        effective_provider: r.get(5)?,
        effective_model: r.get(6)?,
        variant: r.get(7)?,
        tool_mode: r.get(8)?,
        started_at: r.get(9)?,
        status: r.get(10)?,
        updated_ms: r.get(11)?,
    })
}

#[cfg(test)]
#[path = "verification_job_store_tests.rs"]
mod verification_job_store_tests;

impl Store {
    /// `start_tool_run` as ONE transaction: insert the running tool_run row
    /// and append `ToolStarted` (state `ExecutingTool`) together. Returns
    /// `(tool_run_row_id, event_seq)`.
    #[allow(clippy::too_many_arguments)]
    pub fn start_tool_run_and_event(
        &self,
        session_id: SessionId,
        op_id: OpId,
        tool: &str,
        args: serde_json::Value,
        recovery: serde_json::Value,
        expected_hash: Option<String>,
        replay_descriptor: Option<serde_json::Value>,
        expected_state: AgentState,
        event: CommandEvent,
    ) -> StoreResult<(i64, EventSeq)> {
        let seam = Arc::clone(&self.seam);
        let tool = tool.to_owned();
        // Preparation BEFORE enqueueing: argument/recovery/replay JSON.
        let args_json = args.to_string();
        let recovery_json = recovery.to_string();
        let replay_json = replay_descriptor.map(|d| d.to_string());
        let event_payload_json = event.payload.as_ref().map(|p| p.to_string());
        self.writer.execute("start_tool_run_and_event", move |conn| {
        let txn = SessionCommandTxn::begin(conn, &seam, session_id, expected_state)?;
        let changed = txn.tx.execute(
            "INSERT INTO tool_run(session_id, op_id, tool, args, status, started_ms, effect_status, recovery, expected_hash, replay_descriptor)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5, 'unknown', ?6, ?7, ?8)",
            params![
                session_id.raw() as i64,
                op_id.raw() as i64,
                tool,
                args_json,
                now_ms(),
                recovery_json,
                expected_hash,
                replay_json,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Migration(
                "start_tool_run: expected exactly one inserted row".into(),
            ));
        }
        let row_id = txn.tx.last_insert_rowid();
        txn.side_row_applied();
        let seq = Self::insert_event_locked(
            txn.conn(),
            session_id,
            event.op_id,
            event.kind,
            event.state,
            event.ts_ms,
            event_payload_json,
            event.payload_ver,
        )?;
        txn.precommit();
        txn.commit()?;
        Ok((row_id, seq))
        })
    }

    /// `finish_tool_run` as ONE transaction: move exactly ONE still-running
    /// tool_run row to its terminal status and append the completion event
    /// together. Zero changed rows (unknown or already finished) is the typed
    /// `Conflict`; the event is never written without the row.
    pub fn finish_tool_run_and_event(
        &self,
        session_id: SessionId,
        op_id: OpId,
        status: &str,
        effect_status: &str,
        expected_state: AgentState,
        event: CommandEvent,
    ) -> StoreResult<EventSeq> {
        let seam = Arc::clone(&self.seam);
        let status = status.to_owned();
        let effect_status = effect_status.to_owned();
        let event_payload_json = event.payload.as_ref().map(|p| p.to_string());
        self.writer
            .execute("finish_tool_run_and_event", move |conn| {
                let txn = SessionCommandTxn::begin(conn, &seam, session_id, expected_state)?;
                let changed = txn.tx.execute(
                    "UPDATE tool_run SET status = ?3, effect_status = ?4, ended_ms = ?5
             WHERE session_id = ?1 AND op_id = ?2 AND status = 'running'",
                    params![
                        session_id.raw() as i64,
                        op_id.raw() as i64,
                        status,
                        effect_status,
                        now_ms()
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::Conflict(format!(
                        "tool run {op_id} is not running"
                    )));
                }
                txn.side_row_applied();
                let seq = Self::insert_event_locked(
                    txn.conn(),
                    session_id,
                    event.op_id,
                    event.kind,
                    event.state,
                    event.ts_ms,
                    event_payload_json,
                    event.payload_ver,
                )?;
                txn.precommit();
                txn.commit()?;
                Ok(seq)
            })
    }

    /// Crash/abort terminalization as ONE transaction: move exactly ONE
    /// still-running tool_run row to its terminal status/effect and append the
    /// terminal event (`RecoveryApplied` / `ToolCancelled` by `event_kind`)
    /// together, with the session re-verified in `state` before any write.
    ///
    /// `state` is both the expected pre-state and the event's landing state:
    /// the caller has already committed the state move (recovery commits
    /// `CrashDetected` onto the crash target; abort's first per-op command
    /// lands `Cancelled`), and each per-row command re-affirms it — a
    /// self-transition is lawful and idempotent. Zero changed rows (unknown
    /// or already finished) is the typed `Conflict`; the event is never
    /// written without the row. This is the recovery sibling of
    /// [`Store::finish_tool_run_and_event`]: recovery's pre-fix split of a
    /// raw `finish_tool_run` plus a much later `transition_locked` could
    /// leave a terminal tool row with no journal event that the scanner
    /// never revisits.
    ///
    /// The event is stamped with the store clock and payload schema v1
    /// (the schema every writer in this workspace currently stamps).
    #[allow(clippy::too_many_arguments)]
    pub fn finish_recovered_tool_run_and_event(
        &self,
        session_id: SessionId,
        op_id: OpId,
        status: &str,
        effect_status: &str,
        event_kind: EventKind,
        state: AgentState,
        payload: Option<serde_json::Value>,
    ) -> StoreResult<EventSeq> {
        let seam = Arc::clone(&self.seam);
        let status = status.to_owned();
        let effect_status = effect_status.to_owned();
        let payload_json = payload.map(|p| p.to_string());
        self.writer
            .execute("finish_recovered_tool_run_and_event", move |conn| {
                let txn = SessionCommandTxn::begin(conn, &seam, session_id, state)?;
                let changed = txn.tx.execute(
                    "UPDATE tool_run SET status = ?3, effect_status = ?4, ended_ms = ?5
             WHERE session_id = ?1 AND op_id = ?2 AND status = 'running'",
                    params![
                        session_id.raw() as i64,
                        op_id.raw() as i64,
                        status,
                        effect_status,
                        now_ms()
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::Conflict(format!(
                        "recovered tool run {op_id} is not running"
                    )));
                }
                txn.side_row_applied();
                let seq = Self::insert_event_locked(
                    txn.conn(),
                    session_id,
                    Some(op_id),
                    event_kind,
                    state,
                    now_ms(),
                    payload_json,
                    1,
                )?;
                txn.precommit();
                txn.commit()?;
                Ok(seq)
            })
    }

    /// Upsert one first-class durable Task row (audit 25). The row key is
    /// `(session_id, task_id)`; a second upsert of the same key replaces the
    /// goal/criteria/plan/state/budget/spend columns in place (created_ms is
    /// caller-preserved: the session layer reads the row before patching).
    /// The caller enforces the bounded-field contract (goal/criteria/plan
    /// caps); the store only persists.
    pub fn upsert_task(&self, t: &TaskRow) -> StoreResult<()> {
        let t = t.to_owned();
        // Preparation BEFORE enqueueing: every serialized TaskRow column.
        let criteria_json =
            serde_json::to_string(&t.acceptance_criteria).unwrap_or_else(|_| "[]".into());
        let plan_json = serde_json::to_string(&t.plan).unwrap_or_else(|_| "[]".into());
        let state_json = serde_json::to_string(&t.state)
            .expect("in-process TaskState serialization cannot fail");
        let attachments_json =
            serde_json::to_string(&t.attachments).unwrap_or_else(|_| "[]".into());
        self.writer.execute("upsert_task", move |conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_state_raw: Option<String> = tx
            .query_row(
                "SELECT state FROM task WHERE session_id = ?1 AND task_id = ?2",
                params![t.session_id.raw() as i64, t.task_id.raw() as i64],
                |r| r.get(0),
            )
            .optional()?;
        let current_state: Option<TaskState> = match current_state_raw {
            Some(raw) => Some(parse_json(
                &format!("task {}/{} state", t.session_id, t.task_id),
                &raw,
            )?),
            None => None,
        };
        // P0-7 chokepoint backstop: completion-relevant states
        // (NeedsVerification/Verifying/VerifiedComplete) may be written
        // through this generic row path only when the row already holds
        // that exact state (idempotent heal) or when the machine allows the
        // edge into it (Running -> NeedsVerification,
        // NeedsVerification -> Verifying). VerifiedComplete has NO machine
        // edge and is produced exclusively by
        // [`Store::task_complete_verified`] against a passing record — a raw
        // row write can never mint a completion proof.
        if t.state.is_completion_relevant() {
            let legal = match current_state {
                Some(cur) => cur == t.state || cur.allowed_transitions().contains(&t.state),
                None => false,
            };
            if !legal {
                return Err(StoreError::Malformed(format!(
                    "task {}/{}: completion-relevant state {:?} may only be reached through the task machine (transition_task/complete_verified_task), never a raw row write",
                    t.session_id, t.task_id, t.state
                )));
            }
        }
        tx.execute(
            "INSERT INTO task(task_id, session_id, goal, acceptance_criteria, plan,
                              max_tokens, max_turns, spent_tokens, spent_turns,
                              state, created_ms, updated_ms, revision, attachments)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(session_id, task_id) DO UPDATE SET
                goal = excluded.goal,
                acceptance_criteria = excluded.acceptance_criteria,
                plan = excluded.plan,
                max_tokens = excluded.max_tokens,
                max_turns = excluded.max_turns,
                spent_tokens = excluded.spent_tokens,
                spent_turns = excluded.spent_turns,
                state = excluded.state,
                created_ms = excluded.created_ms,
                updated_ms = excluded.updated_ms,
                revision = excluded.revision,
                attachments = excluded.attachments",
            params![
                t.task_id.raw() as i64,
                t.session_id.raw() as i64,
                t.goal,
                criteria_json,
                plan_json,
                t.max_tokens.map(|m| m as i64),
                t.max_turns.map(|m| m as i64),
                t.spent_tokens.min(i64::MAX as u64) as i64,
                t.spent_turns.min(i64::MAX as u32) as i64,
                // In-process constructed enum (see create_session).
                state_json,
                t.created_ms,
                t.updated_ms,
                t.revision.raw() as i64,
                attachments_json,
            ],
        )?;
        tx.commit()?;
        Ok(())
        })
    }

    pub fn get_task(&self, session_id: SessionId, task_id: TaskId) -> StoreResult<Option<TaskRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, session_id, goal, acceptance_criteria, plan,
                    max_tokens, max_turns, spent_tokens, spent_turns,
                    state, created_ms, updated_ms, revision, attachments
             FROM task WHERE session_id = ?1 AND task_id = ?2",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64, task_id.raw() as i64])?;
        match rows.next()? {
            Some(row) => Ok(Some(task_row_map(row, session_id)?)),
            None => Ok(None),
        }
    }

    /// Every durable task row of a session, oldest-created first.
    /// The one completion path (audit P0-7/P0-8): validate the proof and
    /// move the task to `VerifiedComplete` in ONE transaction. The task row,
    /// the session row (the task's base worktree) and the verification
    /// record row are all read INSIDE the transaction and checked against
    /// each other before anything is written:
    ///
    /// (a) the task's state is `Verifying` (a `NeedsVerification` task must
    ///     first transition to `Verifying` — completion never skips the
    ///     verifier);
    /// (b) a `verification_record` row exists with `record_id`;
    /// (c) `record.task_id == task_id`;
    /// (d) `record.revision == expected_revision` — the record must certify
    ///     exactly the revision the caller is completing (and the task row
    ///     must still BE at that revision: any change since the caller's
    ///     read bumps it and refuses here);
    /// (e) `record.status == Passed`;
    /// (f) the record covers EVERY current acceptance criterion of the task
    ///     (present with `passed = true`; extra record criteria are fine,
    ///     missing ones refuse);
    /// (g) `record.workspace_id/worktree_id` equal the task's current base
    ///     worktree (the session row);
    /// (h) NO reservation row of the task still holds budget (`reserved`,
    ///     `dispatched` or `uncertain` — counted INSIDE this IMMEDIATE
    ///     transaction): a reserve that raced the caller's accounting pass
    ///     refuses the completion typed and the task stays Verifying.
    ///
    /// Only then does the transaction write `VerifiedComplete` and bump the
    /// revision exactly once. Every refusal leaves the task row untouched.
    pub fn task_complete_verified(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        expected_revision: TaskRevision,
        record_id: VerificationRecordId,
        now: i64,
    ) -> StoreResult<std::result::Result<TaskRow, TaskCompletionRefusal>> {
        self.writer.execute("task_complete_verified", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // The task's current base worktree: the session row (v8 identity).
            let base: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT workspace_id, worktree_id FROM session WHERE id = ?1",
                    params![session_id.raw() as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((task_ws, task_wt)) = base else {
                return Ok(Err(TaskCompletionRefusal::TaskMissing { task_id }));
            };
            let task = {
                let mut stmt = tx.prepare(
                    "SELECT task_id, session_id, goal, acceptance_criteria, plan,
                        max_tokens, max_turns, spent_tokens, spent_turns,
                        state, created_ms, updated_ms, revision, attachments
                 FROM task WHERE session_id = ?1 AND task_id = ?2",
                )?;
                let mut rows =
                    stmt.query(params![session_id.raw() as i64, task_id.raw() as i64])?;
                match rows.next()? {
                    Some(row) => Some(task_row_map(row, session_id)?),
                    None => None,
                }
            };
            let Some(task) = task else {
                return Ok(Err(TaskCompletionRefusal::TaskMissing { task_id }));
            };
            if task.revision != expected_revision {
                return Ok(Err(TaskCompletionRefusal::RevisionMismatch {
                    expected: expected_revision,
                    actual: task.revision,
                }));
            }
            if task.state != TaskState::Verifying {
                return Ok(Err(TaskCompletionRefusal::NotVerifying {
                    actual: task.state,
                }));
            }
            let record = {
                let mut stmt = tx.prepare(
                    "SELECT id, task_id, revision, workspace_id, worktree_id, tree_hash,
                        criteria_json, checks_json, changed_files_json,
                        unrelated_changes_json, reviewer_json, status,
                        started_ms, completed_ms
                 FROM verification_record WHERE id = ?1",
                )?;
                let mut rows = stmt.query(params![record_id.raw() as i64])?;
                match rows.next()? {
                    Some(row) => Some(verification_record_map(row)?),
                    None => None,
                }
            };
            let Some(record) = record else {
                return Ok(Err(TaskCompletionRefusal::RecordMissing { record_id }));
            };
            if record.task_id != task_id {
                return Ok(Err(TaskCompletionRefusal::RecordWrongTask {
                    record_id,
                    record_task: record.task_id,
                    requested: task_id,
                }));
            }
            if record.revision != expected_revision {
                return Ok(Err(TaskCompletionRefusal::RecordWrongRevision {
                    record_id,
                    record_revision: record.revision,
                    expected: expected_revision,
                }));
            }
            if record.status != VerificationStatus::Passed {
                return Ok(Err(TaskCompletionRefusal::RecordNotPassed {
                    record_id,
                    status: record.status,
                }));
            }
            let missing: Vec<String> = task
                .acceptance_criteria
                .iter()
                .filter(|c| {
                    !record
                        .criteria
                        .iter()
                        .any(|cv| cv.passed && &cv.criterion_key == *c)
                })
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Ok(Err(TaskCompletionRefusal::CriteriaNotCovered {
                    record_id,
                    missing,
                }));
            }
            let task_ws_id =
                id_field::<WorkspaceId>(&format!("session {session_id} workspace_id"), task_ws)?;
            let task_wt_id =
                id_field::<WorktreeId>(&format!("session {session_id} worktree_id"), task_wt)?;
            if record.workspace_id != task_ws_id || record.worktree_id != task_wt_id {
                return Ok(Err(TaskCompletionRefusal::WorktreeMismatch {
                    record_id,
                    record_workspace: record.workspace_id,
                    record_worktree: record.worktree_id,
                    task_workspace: task_ws_id,
                    task_worktree: task_wt_id,
                }));
            }
            // (h) THE ACCOUNTING GATE (completion-vs-reserve invariant): no
            // reservation of this task may still hold budget — `reserved`
            // (dispatch never began), `dispatched` (the provider may have
            // billed) or `uncertain` (a crashed dispatched attempt). The count
            // runs INSIDE this IMMEDIATE transaction, so a reserve that landed
            // after the session layer's accounting pass but before this write is
            // caught: any nonzero count rolls the transaction back with a typed
            // refusal and the task row stays exactly as it was (Verifying).
            let (reserved, dispatched, reserved_micro, uncertain, uncertain_micro) = tx.query_row(
                "SELECT
                     COALESCE(SUM(CASE WHEN status = 'reserved' THEN 1 ELSE 0 END), 0),
                     COALESCE(SUM(CASE WHEN status = 'dispatched' THEN 1 ELSE 0 END), 0),
                     COALESCE(SUM(CASE WHEN status IN ('reserved', 'dispatched')
                                       THEN predicted_micro ELSE 0 END), 0),
                     COALESCE(SUM(CASE WHEN status = 'uncertain' THEN 1 ELSE 0 END), 0),
                     COALESCE(SUM(CASE WHEN status = 'uncertain'
                                       THEN predicted_micro ELSE 0 END), 0)
                 FROM cost_reservation
                 WHERE session_id = ?1 AND task_id = ?2",
                params![session_id.raw() as i64, task_id.raw() as i64],
                |r| {
                    Ok((
                        usize::try_from(r.get::<_, i64>(0)?).unwrap_or(usize::MAX),
                        usize::try_from(r.get::<_, i64>(1)?).unwrap_or(usize::MAX),
                        u64::try_from(r.get::<_, i64>(2)?).unwrap_or(u64::MAX),
                        usize::try_from(r.get::<_, i64>(3)?).unwrap_or(usize::MAX),
                        u64::try_from(r.get::<_, i64>(4)?).unwrap_or(u64::MAX),
                    ))
                },
            )?;
            if reserved
                .saturating_add(dispatched)
                .saturating_add(uncertain)
                != 0
            {
                tx.rollback()?;
                return Ok(Err(TaskCompletionRefusal::ReservationsHeld {
                    reserved,
                    dispatched,
                    reserved_micro,
                    uncertain,
                    uncertain_micro,
                }));
            }
            let new_revision = expected_revision.checked_next().ok_or_else(|| {
                StoreError::Malformed(format!(
                    "task {session_id}/{task_id} revision overflow at completion"
                ))
            })?;
            let updated = tx.execute(
                "UPDATE task SET state = ?3, revision = ?4, updated_ms = ?5
             WHERE session_id = ?1 AND task_id = ?2 AND revision = ?6",
                params![
                    session_id.raw() as i64,
                    task_id.raw() as i64,
                    // In-process constructed enum (see create_session).
                    serde_json::to_string(&TaskState::VerifiedComplete).unwrap(),
                    new_revision.raw() as i64,
                    now,
                    expected_revision.raw() as i64
                ],
            )?;
            if updated != 1 {
                return Err(StoreError::Conflict(format!(
                    "task {session_id}/{task_id} vanished between validation and write"
                )));
            }
            tx.commit()?;
            let mut completed = task;
            completed.state = TaskState::VerifiedComplete;
            completed.revision = new_revision;
            completed.updated_ms = now;
            Ok(Ok(completed))
        })
    }

    // ------------------------------------------------------- verification records

    /// Persist one verification record. The record is immutable after this
    /// call except the single CAS finalize (`Running -> Passed|Failed`);
    /// `rec.id` is ignored and the fresh row id is returned. Bounded JSON
    /// columns are the caller's contract (the session layer rejects
    /// oversized criteria/checks before any write, mirroring
    /// [`Store::upsert_task`]).
    pub fn verification_record_put(
        &self,
        rec: &VerificationRecordRow,
    ) -> StoreResult<VerificationRecordId> {
        self.verification_record_put_with_evidence(rec, None, None)
    }

    /// Additive v20 twin of [`Store::verification_record_put`] (audits
    /// 94/116/117): one INSERT carrying the record together with its two
    /// optional evidence JSON columns — the bounded environment fingerprint
    /// and the candidate-proof reference. `None` writes SQL `NULL` (honestly
    /// absent, exactly like a pre-v20 row); the session layer validates and
    /// bounds both payloads BEFORE this call.
    pub fn verification_record_put_with_evidence(
        &self,
        rec: &VerificationRecordRow,
        environment_fingerprint_json: Option<&str>,
        candidate_proof_ref_json: Option<&str>,
    ) -> StoreResult<VerificationRecordId> {
        let rec = rec.to_owned();
        let environment_fingerprint_json = environment_fingerprint_json.map(|v| v.to_owned());
        let candidate_proof_ref_json = candidate_proof_ref_json.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: every serialized record column.
        let criteria_json = serde_json::to_string(&rec.criteria).unwrap_or_else(|_| "[]".into());
        let checks_json = serde_json::to_string(&rec.checks).unwrap_or_else(|_| "[]".into());
        let changed_files_json =
            serde_json::to_string(&rec.changed_files).unwrap_or_else(|_| "[]".into());
        let unrelated_changes_json =
            serde_json::to_string(&rec.unrelated_changes).unwrap_or_else(|_| "[]".into());
        let status_json =
            serde_json::to_string(&rec.status).expect("in-process status serialization");
        let reviewer_json = rec.reviewer.as_ref().map(|v| v.to_string());
        self.writer
            .execute("verification_record_put_with_evidence", move |conn| {
                conn.execute(
                    "INSERT INTO verification_record(
                task_id, revision, workspace_id, worktree_id, tree_hash,
                criteria_json, checks_json, changed_files_json,
                unrelated_changes_json, reviewer_json, status,
                started_ms, completed_ms,
                environment_fingerprint_json, candidate_proof_ref_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    params![
                        rec.task_id.raw() as i64,
                        rec.revision.raw() as i64,
                        rec.workspace_id.raw() as i64,
                        rec.worktree_id.raw() as i64,
                        rec.tree_hash,
                        criteria_json,
                        checks_json,
                        changed_files_json,
                        unrelated_changes_json,
                        reviewer_json,
                        status_json,
                        rec.started_ms,
                        rec.completed_ms,
                        environment_fingerprint_json,
                        candidate_proof_ref_json,
                    ],
                )?;
                let id = conn.last_insert_rowid();
                // SQLite rowids start at 1, so a fresh row id is always a valid
                // (non-zero) record id.
                Ok(VerificationRecordId::new(id as u64))
            })
    }

    /// Additive v20 twin of [`Store::verification_record_get`]: the row plus
    /// its raw `(environment_fingerprint_json, candidate_proof_ref_json)`
    /// evidence columns. `None` on either side means the record predates v20
    /// or was written without that evidence — an honest absence the session
    /// layer maps to an absent typed value, never a guess.
    pub fn verification_record_get_with_evidence(
        &self,
        record_id: VerificationRecordId,
    ) -> StoreResult<Option<VerificationRecordWithEvidence>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, revision, workspace_id, worktree_id, tree_hash,
                    criteria_json, checks_json, changed_files_json,
                    unrelated_changes_json, reviewer_json, status,
                    started_ms, completed_ms,
                    environment_fingerprint_json, candidate_proof_ref_json
             FROM verification_record WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![record_id.raw() as i64])?;
        match rows.next()? {
            Some(row) => Ok(Some((
                verification_record_map(row)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, Option<String>>(15)?,
            ))),
            None => Ok(None),
        }
    }

    /// Every verification record of one task with its evidence columns, in
    /// deterministic creation order (`id ASC`) — the v20 twin of
    /// [`Store::verification_record_list_by_task`].
    pub fn verification_record_list_by_task_with_evidence(
        &self,
        task_id: TaskId,
    ) -> StoreResult<Vec<VerificationRecordWithEvidence>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, revision, workspace_id, worktree_id, tree_hash,
                    criteria_json, checks_json, changed_files_json,
                    unrelated_changes_json, reviewer_json, status,
                    started_ms, completed_ms,
                    environment_fingerprint_json, candidate_proof_ref_json
             FROM verification_record WHERE task_id = ?1 ORDER BY id ASC",
        )?;
        let mut rows = stmt.query(params![task_id.raw() as i64])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((
                verification_record_map(row)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, Option<String>>(15)?,
            ));
        }
        Ok(out)
    }

    pub fn verification_record_get(
        &self,
        record_id: VerificationRecordId,
    ) -> StoreResult<Option<VerificationRecordRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, revision, workspace_id, worktree_id, tree_hash,
                    criteria_json, checks_json, changed_files_json,
                    unrelated_changes_json, reviewer_json, status,
                    started_ms, completed_ms
             FROM verification_record WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![record_id.raw() as i64])?;
        match rows.next()? {
            Some(row) => Ok(Some(verification_record_map(row)?)),
            None => Ok(None),
        }
    }

    /// Every verification record of one task, in deterministic creation
    /// order (`id ASC`).
    pub fn verification_record_list_by_task(
        &self,
        task_id: TaskId,
    ) -> StoreResult<Vec<VerificationRecordRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, revision, workspace_id, worktree_id, tree_hash,
                    criteria_json, checks_json, changed_files_json,
                    unrelated_changes_json, reviewer_json, status,
                    started_ms, completed_ms
             FROM verification_record WHERE task_id = ?1 ORDER BY id ASC",
        )?;
        let mut rows = stmt.query(params![task_id.raw() as i64])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(verification_record_map(row)?);
        }
        Ok(out)
    }

    /// The record's single allowed status write: a CAS from `Running` to
    /// `Passed` or `Failed`. Exactly one finalize wins; a second attempt on
    /// an already-final record is refused with its current status, and the
    /// finalize never rewinds (a finalized record is immutable).
    pub fn verification_record_finalize(
        &self,
        record_id: VerificationRecordId,
        new_status: VerificationStatus,
        completed_ms: i64,
    ) -> StoreResult<std::result::Result<(), RecordFinalizeRefusal>> {
        if !matches!(
            new_status,
            VerificationStatus::Passed | VerificationStatus::Failed
        ) {
            return Err(StoreError::Malformed(format!(
                "record {record_id}: finalize status must be Passed or Failed, got {new_status:?}"
            )));
        }
        self.writer
            .execute("verification_record_finalize", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let updated = tx.execute(
                    "UPDATE verification_record
             SET status = ?2, completed_ms = ?3
             WHERE id = ?1 AND status = ?4",
                    params![
                        record_id.raw() as i64,
                        serde_json::to_string(&new_status).unwrap(),
                        completed_ms,
                        serde_json::to_string(&VerificationStatus::Running).unwrap()
                    ],
                )?;
                if updated == 1 {
                    tx.commit()?;
                    return Ok(Ok(()));
                }
                // The CAS missed: surface the current status so callers can tell an
                // already-final record from a not-yet-started one.
                let current_raw: Option<String> = tx
                    .query_row(
                        "SELECT status FROM verification_record WHERE id = ?1",
                        params![record_id.raw() as i64],
                        |r| r.get(0),
                    )
                    .optional()?;
                let current: Option<VerificationStatus> = match current_raw {
                    Some(raw) => Some(parse_json(
                        &format!("verification_record {record_id} status"),
                        &raw,
                    )?),
                    None => None,
                };
                match current {
                    Some(current) => Ok(Err(RecordFinalizeRefusal::NotRunning {
                        record_id,
                        current,
                    })),
                    None => Ok(Err(RecordFinalizeRefusal::Missing { record_id })),
                }
            })
    }

    // ------------------------------------------------ verification jobs (v22)

    /// Begin ONE verification attempt durably (schema v22): the attempt row,
    /// its changed-file rows and every required check row (inline outcomes
    /// AND background job definitions) in ONE immediate transaction. The
    /// commit is the attempt's commit point, so a crash can never leave torn
    /// job rows — either the whole attempt is durable or none of it is.
    ///
    /// Idempotent by identity: an existing `(session, task, attempt_op)`
    /// returns `Ok(false)` and writes nothing (a retried begin after a
    /// crash). An OPEN background check of a DIFFERENT attempt refuses with
    /// [`StoreError::Conflict`] (supersede first — an open job is never
    /// silently replaced). Every bound is enforced before any write.
    pub fn verification_attempt_begin(
        &self,
        attempt: &VerificationAttemptRow,
        changed: &[String],
        checks: &[VerificationJobRow],
    ) -> StoreResult<bool> {
        validate_verification_attempt(attempt, changed, checks)?;
        let attempt = attempt.to_owned();
        let changed = changed.to_owned();
        let checks = checks.to_owned();
        self.writer
            .execute("verification_attempt_begin", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
                    return Ok(false);
                }
                for check in &checks {
                    if check.inline_status.is_some() {
                        continue;
                    }
                    let open_elsewhere: Option<i64> = tx
                        .query_row(
                            "SELECT attempt_op_id FROM verification_job
                     WHERE session_id = ?1 AND task_id = ?2 AND check_id = ?3
                       AND inline_status IS NULL AND state IN ('queued', 'running')
                       AND attempt_op_id <> ?4",
                            params![
                                attempt.session_id.raw() as i64,
                                attempt.task_id.raw() as i64,
                                check.check_id,
                                attempt.attempt_op_id as i64
                            ],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if let Some(prior) = open_elsewhere {
                        return Err(StoreError::Conflict(format!(
                    "check '{}' has an open job of attempt {prior}; supersede that attempt before \
                     beginning attempt {}",
                    check.check_id, attempt.attempt_op_id
                )));
                    }
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
                for (ordinal, path) in changed.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO verification_attempt_changed_file(
                    session_id, task_id, attempt_op_id, ordinal, path)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            attempt.session_id.raw() as i64,
                            attempt.task_id.raw() as i64,
                            attempt.attempt_op_id as i64,
                            ordinal as i64,
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
                tx.commit()?;
                Ok(true)
            })
    }

    /// One attempt view by identity, or `None`.
    pub fn verification_attempt_get(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt_op_id: u64,
    ) -> StoreResult<Option<VerificationAttemptView>> {
        let conn = self.read()?;
        verification_attempt_view(&conn, session_id, task_id, attempt_op_id)
    }

    /// The NEWEST attempt view of `(session, task)` (highest attempt op), or
    /// `None`. Attempt ops are monotonic, so the max is the current attempt.
    pub fn verification_attempt_current(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> StoreResult<Option<VerificationAttemptView>> {
        let conn = self.read()?;
        let newest: Option<i64> = conn
            .query_row(
                "SELECT MAX(attempt_op_id) FROM verification_attempt
                 WHERE session_id = ?1 AND task_id = ?2",
                params![session_id.raw() as i64, task_id.raw() as i64],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match newest {
            // Bit-cast: u64 attempt op ids live in the signed column.
            Some(op) => verification_attempt_view(&conn, session_id, task_id, op as u64),
            None => Ok(None),
        }
    }

    /// Every OPEN (queued|running) BACKGROUND job of `(session, task)`,
    /// ordered `(task, check-id)`. Rows are decoded FIRST (a corrupt state
    /// is a loud typed error, never filtered away by SQL), then filtered.
    pub fn verification_jobs_open(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> StoreResult<Vec<VerificationJobRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(&format!(
            "{} WHERE session_id = ?1 AND task_id = ?2 AND inline_status IS NULL
             ORDER BY check_id ASC",
            VERIFICATION_JOB_SELECT
        ))?;
        let mut rows = stmt.query(params![session_id.raw() as i64, task_id.raw() as i64])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let job = verification_job_map(row)?;
            if matches!(job.state.as_str(), "queued" | "running") {
                out.push(job);
            }
        }
        Ok(out)
    }

    /// Every BACKGROUND job row of one attempt (any state), ordered by
    /// derivation ordinal (check-id fallback).
    pub fn verification_jobs_for_attempt(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt_op_id: u64,
    ) -> StoreResult<Vec<VerificationJobRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(&format!(
            "{VERIFICATION_JOB_SELECT}
             WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3
               AND inline_status IS NULL
             ORDER BY ordinal ASC, check_id ASC"
        ))?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            task_id.raw() as i64,
            attempt_op_id as i64
        ])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(verification_job_map(row)?);
        }
        Ok(out)
    }

    /// Cancel every OPEN background job of one attempt to `cancelled` with a
    /// typed note. Returns the number of rows cancelled. A cancelled job is
    /// terminal and can never certify completion.
    pub fn verification_attempt_cancel(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt_op_id: u64,
        reason: &str,
        now: i64,
    ) -> StoreResult<u64> {
        if reason.is_empty() || reason.len() > MAX_VERIFICATION_JOB_NOTE_BYTES {
            return Err(StoreError::Oversized(format!(
                "cancel note of {} bytes outside 1..={MAX_VERIFICATION_JOB_NOTE_BYTES}",
                reason.len()
            )));
        }
        let reason = reason.to_owned();
        self.writer
            .execute("verification_attempt_cancel", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let cancelled = tx.execute(
                    "UPDATE verification_job
             SET state = 'cancelled', note = ?4, updated_ms = ?5, finished_ms = ?5
             WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3
               AND inline_status IS NULL AND state IN ('queued', 'running')",
                    params![
                        session_id.raw() as i64,
                        task_id.raw() as i64,
                        attempt_op_id as i64,
                        reason,
                        now
                    ],
                )?;
                tx.commit()?;
                Ok(cancelled as u64)
            })
    }

    /// Claim one `queued` background job: the guarded CAS to `running` with
    /// the executor's op attached. A job is claimed at most once per attempt;
    /// a NEWER attempt makes every mutation of attempt N a typed
    /// [`VerificationJobRefusal::Superseded`].
    pub fn verification_job_claim(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt_op_id: u64,
        check_id: &str,
        op_id: u64,
        now: i64,
    ) -> StoreResult<std::result::Result<VerificationJobRow, VerificationJobRefusal>> {
        if op_id == 0 {
            return Err(StoreError::Malformed(
                "job claim op_id must be non-zero".into(),
            ));
        }
        let check_id = check_id.to_owned();
        self.writer.execute("verification_job_claim", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(newest) = newest_attempt_op(&tx, session_id, task_id)? {
                if newest > attempt_op_id {
                    return Ok(Err(VerificationJobRefusal::Superseded {
                        attempt_op_id,
                        newest_attempt_op_id: newest,
                    }));
                }
            }
            let row = verification_job_get(&tx, session_id, task_id, attempt_op_id, &check_id)?;
            let Some(mut job) = row else {
                return Ok(Err(VerificationJobRefusal::Missing {
                    check_id: check_id.to_string(),
                }));
            };
            if job.inline_status.is_some() || job.state != "queued" {
                return Ok(Err(VerificationJobRefusal::NotOpen {
                    check_id: check_id.to_string(),
                    state: job.state,
                }));
            }
            job.state = "running".into();
            job.op_id = Some(op_id);
            job.updated_ms = now;
            tx.execute(
                "UPDATE verification_job SET state = 'running', op_id = ?5, updated_ms = ?6
             WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3
               AND check_id = ?4 AND state = 'queued'",
                params![
                    session_id.raw() as i64,
                    task_id.raw() as i64,
                    attempt_op_id as i64,
                    check_id,
                    op_id as i64,
                    now
                ],
            )?;
            tx.commit()?;
            Ok(Ok(job))
        })
    }

    /// Resolve one `running` background job to a terminal state and record
    /// its typed outcome exactly once. The attempt-N identity of the row and
    /// the result is structural: a result for attempt N is a DIFFERENT row
    /// from attempt N+1, and once N+1 exists attempt N is frozen — a late
    /// resolve refuses with [`VerificationJobRefusal::Superseded`].
    #[allow(clippy::too_many_arguments)]
    pub fn verification_job_resolve(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        attempt_op_id: u64,
        check_id: &str,
        state: &str,
        note: Option<&str>,
        result_json: Option<&str>,
        now: i64,
    ) -> StoreResult<std::result::Result<VerificationJobRow, VerificationJobRefusal>> {
        if !matches!(state, "passed" | "failed" | "unavailable" | "cancelled") {
            return Err(StoreError::Malformed(format!(
                "resolve state {state:?} is not terminal"
            )));
        }
        if let Some(note) = note {
            if note.is_empty() || note.len() > MAX_VERIFICATION_JOB_NOTE_BYTES {
                return Err(StoreError::Oversized(format!(
                    "resolve note of {} bytes outside 1..={MAX_VERIFICATION_JOB_NOTE_BYTES}",
                    note.len()
                )));
            }
        }
        if let Some(result) = result_json {
            if result.is_empty() || result.len() > MAX_VERIFICATION_JOB_RESULT_JSON_BYTES {
                return Err(StoreError::Oversized(format!(
                    "result json of {} bytes outside 1..={MAX_VERIFICATION_JOB_RESULT_JSON_BYTES}",
                    result.len()
                )));
            }
        }
        let check_id = check_id.to_owned();
        let state = state.to_owned();
        let note = note.map(|v| v.to_owned());
        let result_json = result_json.map(|v| v.to_owned());
        self.writer
            .execute("verification_job_resolve", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(newest) = newest_attempt_op(&tx, session_id, task_id)? {
                    if newest > attempt_op_id {
                        return Ok(Err(VerificationJobRefusal::Superseded {
                            attempt_op_id,
                            newest_attempt_op_id: newest,
                        }));
                    }
                }
                let Some(mut job) =
                    verification_job_get(&tx, session_id, task_id, attempt_op_id, &check_id)?
                else {
                    return Ok(Err(VerificationJobRefusal::Missing {
                        check_id: check_id.to_string(),
                    }));
                };
                if job.inline_status.is_some() || job.state != "running" {
                    return Ok(Err(VerificationJobRefusal::NotOpen {
                        check_id: check_id.to_string(),
                        state: job.state,
                    }));
                }
                if let Some(result) = result_json {
                    let already: Option<i64> = tx
                        .query_row(
                            "SELECT 1 FROM verification_job_result
                     WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3
                       AND check_id = ?4",
                            params![
                                session_id.raw() as i64,
                                task_id.raw() as i64,
                                attempt_op_id as i64,
                                check_id
                            ],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if already.is_some() {
                        return Ok(Err(VerificationJobRefusal::ResultExists {
                            check_id: check_id.to_string(),
                        }));
                    }
                    tx.execute(
                        "INSERT INTO verification_job_result(
                    session_id, task_id, attempt_op_id, check_id, result_json, finished_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            session_id.raw() as i64,
                            task_id.raw() as i64,
                            attempt_op_id as i64,
                            check_id,
                            result,
                            now
                        ],
                    )?;
                    job.result_json = Some(result.to_string());
                }
                job.state = state.to_string();
                job.note = note.clone();
                job.updated_ms = now;
                job.finished_ms = Some(now);
                tx.execute(
                    "UPDATE verification_job
             SET state = ?5, note = ?6, updated_ms = ?7, finished_ms = ?7
             WHERE session_id = ?1 AND task_id = ?2 AND attempt_op_id = ?3
               AND check_id = ?4 AND state = 'running'",
                    params![
                        session_id.raw() as i64,
                        task_id.raw() as i64,
                        attempt_op_id as i64,
                        check_id,
                        state,
                        note,
                        now
                    ],
                )?;
                tx.commit()?;
                Ok(Ok(job))
            })
    }

    /// Honest post-restart recovery for one session (schema v22): every
    /// `running` row — an executor died mid-check — is re-queued with a typed
    /// note (it never certified anything; re-running it deterministically is
    /// the only honest path to a verdict), and open rows whose attempt row is
    /// missing (impossible under the foreign keys; counted for hand-corrupted
    /// databases) are orphaned to `unavailable`. Idempotent.
    pub fn verification_jobs_requeue_running(
        &self,
        session_id: SessionId,
        now: i64,
    ) -> StoreResult<VerificationJobRecovery> {
        self.writer
            .execute("verification_jobs_requeue_running", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut report = VerificationJobRecovery::default();
                let orphaned = tx.execute(
                    "UPDATE verification_job
             SET state = 'unavailable', finished_ms = ?2, updated_ms = ?2,
                 note = 'orphaned job: its attempt record is missing; never certified'
             WHERE session_id = ?1 AND state IN ('queued', 'running')
               AND NOT EXISTS (
                   SELECT 1 FROM verification_attempt a
                   WHERE a.session_id = verification_job.session_id
                     AND a.task_id = verification_job.task_id
                     AND a.attempt_op_id = verification_job.attempt_op_id)",
                    params![session_id.raw() as i64, now],
                )?;
                report.orphaned = orphaned as u64;
                let requeued = tx.execute(
                    "UPDATE verification_job
             SET state = 'queued', op_id = NULL, note =
                 're-queued after a restart: the previous executor died mid-check and never \
                  produced a verdict; the check re-runs deterministically',
                 updated_ms = ?2, finished_ms = NULL
             WHERE session_id = ?1 AND state = 'running'",
                    params![session_id.raw() as i64, now],
                )?;
                report.requeued = requeued as u64;
                tx.commit()?;
                Ok(report)
            })
    }

    /// One-shot, idempotent v22 legacy-verification repair (invoked by every
    /// open after migration AND exposed for explicit repair tests): project
    /// pre-v22 `memory_fact` verification rows into the real tables. A
    /// session already carrying the durable import marker is skipped whole;
    /// corrupt rows are skipped loudly with typed notes and left in place.
    pub fn import_legacy_verification_facts(&self) -> StoreResult<LegacyVerificationImport> {
        self.writer
            .execute("import_legacy_verification_facts", move |conn| {
                import_legacy_verification_facts_conn(conn)
            })
    }

    pub fn list_tasks(&self, session_id: SessionId) -> StoreResult<Vec<TaskRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, session_id, goal, acceptance_criteria, plan,
                    max_tokens, max_turns, spent_tokens, spent_turns,
                    state, created_ms, updated_ms, revision, attachments
             FROM task WHERE session_id = ?1 ORDER BY created_ms ASC, task_id ASC",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(task_row_map(row, session_id)?);
        }
        Ok(out)
    }

    /// Durable logical-turn count of a session: journal events with kind
    /// `turn_completed`. Each genuine turn end appends exactly one, so this
    /// is the crash-safe spent-turns source for the task budget.
    pub fn turn_completed_count(&self, session_id: SessionId) -> StoreResult<u64> {
        let conn = self.read()?;
        let out: i64 = conn.query_row(
            "SELECT COUNT(*) FROM event WHERE session_id = ?1 AND kind = 'turn_completed'",
            params![session_id.raw() as i64],
            |r| r.get(0),
        )?;
        Ok(out.max(0) as u64)
    }

    // ---------------------------------------------------------------- tool runs

    #[allow(clippy::too_many_arguments)]
    pub fn start_tool_run(
        &self,
        session_id: SessionId,
        op_id: OpId,
        tool: &str,
        args: serde_json::Value,
        recovery: serde_json::Value,
        expected_hash: Option<String>,
        replay_descriptor: Option<serde_json::Value>,
    ) -> StoreResult<i64> {
        let tool = tool.to_owned();
        // Preparation BEFORE enqueueing: argument/recovery/replay JSON.
        let args_json = args.to_string();
        let recovery_json = recovery.to_string();
        let replay_json = replay_descriptor.map(|d| d.to_string());
        self.writer.execute("start_tool_run", move |conn| {
        conn.execute(
            "INSERT INTO tool_run(session_id, op_id, tool, args, status, started_ms, effect_status, recovery, expected_hash, replay_descriptor)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5, 'unknown', ?6, ?7, ?8)",
            params![
                session_id.raw() as i64,
                op_id.raw() as i64,
                tool,
                args_json,
                now_ms(),
                recovery_json,
                expected_hash,
                replay_json,
            ],
        )?;
        Ok(conn.last_insert_rowid())
        })
    }

    /// Record the workspace-write postcondition a tool reported at execution
    /// end (v7): recovery verifies the CURRENT file bytes against it through
    /// the workspace file service — never a hash inferred from args JSON.
    /// Only a still-running row may be annotated (loud otherwise).
    #[allow(clippy::too_many_arguments)]
    pub fn record_tool_postcondition(
        &self,
        session_id: SessionId,
        op_id: OpId,
        postcondition: &serde_json::Value,
    ) -> StoreResult<()> {
        let postcondition = postcondition.to_owned();
        // Preparation BEFORE enqueueing: the postcondition JSON.
        let postcondition_json = postcondition.to_string();
        self.writer
            .execute("record_tool_postcondition", move |conn| {
                let n = conn.execute(
                    "UPDATE tool_run SET postcondition = ?3
             WHERE session_id = ?1 AND op_id = ?2 AND status = 'running'",
                    params![
                        session_id.raw() as i64,
                        op_id.raw() as i64,
                        postcondition_json
                    ],
                )?;
                if n == 0 {
                    return Err(StoreError::Migration(
                        "record_tool_postcondition: no running row".into(),
                    ));
                }
                Ok(())
            })
    }

    /// Bump the physical-attempt counter of one still-running tool run (v7:
    /// a crash-recovery replay is a NEW PHYSICAL attempt of the SAME logical
    /// operation). Loud when the row is not running.
    pub fn bump_tool_run_attempt(&self, session_id: SessionId, op_id: OpId) -> StoreResult<i64> {
        self.writer.execute("bump_tool_run_attempt", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let n = tx.execute(
                "UPDATE tool_run SET attempt = attempt + 1
             WHERE session_id = ?1 AND op_id = ?2 AND status = 'running'",
                params![session_id.raw() as i64, op_id.raw() as i64],
            )?;
            if n == 0 {
                return Err(StoreError::Migration(
                    "bump_tool_run_attempt: no running row".into(),
                ));
            }
            let attempt: i64 = tx.query_row(
                "SELECT attempt FROM tool_run WHERE session_id = ?1 AND op_id = ?2",
                params![session_id.raw() as i64, op_id.raw() as i64],
                |r| r.get(0),
            )?;
            tx.commit()?;
            Ok(attempt)
        })
    }

    pub fn finish_tool_run(
        &self,
        session_id: SessionId,
        op_id: OpId,
        status: &str,
        effect_status: &str,
    ) -> StoreResult<()> {
        let status = status.to_owned();
        let effect_status = effect_status.to_owned();
        self.writer.execute("finish_tool_run", move |conn| {
            let n = conn.execute(
                "UPDATE tool_run SET status = ?3, effect_status = ?4, ended_ms = ?5
             WHERE session_id = ?1 AND op_id = ?2",
                params![
                    session_id.raw() as i64,
                    op_id.raw() as i64,
                    status,
                    effect_status,
                    now_ms()
                ],
            )?;
            if n == 0 {
                return Err(StoreError::Migration(
                    "finish_tool_run: no matching row".into(),
                ));
            }
            Ok(())
        })
    }

    pub fn set_tool_run_effect(
        &self,
        session_id: SessionId,
        op_id: OpId,
        effect_status: &str,
    ) -> StoreResult<()> {
        let effect_status = effect_status.to_owned();
        self.writer.execute("set_tool_run_effect", move |conn| {
            conn.execute(
                "UPDATE tool_run SET effect_status = ?3 WHERE session_id = ?1 AND op_id = ?2",
                params![session_id.raw() as i64, op_id.raw() as i64, effect_status],
            )?;
            Ok(())
        })
    }

    /// Unfinished tool runs (ToolStarted without ToolCompleted): the crash
    /// recovery scanner's input.
    pub fn pending_tool_runs(&self, session_id: SessionId) -> StoreResult<Vec<ToolRunRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, op_id, tool, args, status, started_ms, ended_ms, effect_status, recovery, expected_hash, replay_descriptor, attempt, postcondition
             FROM tool_run WHERE session_id = ?1 AND status = 'running'",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(tool_run_map(row)?);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- turn records

    /// Durably open a logical-turn record. Called transactionally when a
    /// prompt is admitted as the ACTIVE logical turn (immediate admission in
    /// `submit_prompt`, or queue admission). Re-admission of the SAME turn op
    /// (a crash between admission and the first drive; the queue row is
    /// re-admitted after recovery) UPSERTS the same record — the turn's
    /// identity is never duplicated. Any OTHER still-active record of the
    /// session is finalized as failed in the same transaction (at most one
    /// active logical turn may exist per session).
    #[allow(clippy::too_many_arguments)]
    pub fn start_turn_record(
        &self,
        session_id: SessionId,
        turn_op_id: OpId,
        queue_seq: Option<i64>,
        prompt_message_id: Option<i64>,
        provider: &str,
        model: &str,
        variant: Option<&str>,
    ) -> StoreResult<i64> {
        let provider = provider.to_owned();
        let model = model.to_owned();
        let variant = variant.map(|v| v.to_owned());
        self.writer.execute("start_turn_record", move |conn| {
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE turn_record SET status = ?3, updated_ms = ?4
             WHERE session_id = ?1 AND status = 'active' AND turn_op_id != ?2",
            params![
                session_id.raw() as i64,
                turn_op_id.raw() as i64,
                TURN_RECORD_FAILED,
                now_ms()
            ],
        )?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO turn_record(session_id, turn_op_id, queue_seq, prompt_message_id, effective_provider, effective_model, variant, started_at, status, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?8)
             ON CONFLICT(session_id, turn_op_id) DO UPDATE SET
                queue_seq = excluded.queue_seq,
                prompt_message_id = excluded.prompt_message_id,
                effective_provider = excluded.effective_provider,
                effective_model = excluded.effective_model,
                variant = excluded.variant,
                started_at = excluded.started_at,
                status = 'active',
                updated_ms = excluded.updated_ms",
            params![
                session_id.raw() as i64,
                turn_op_id.raw() as i64,
                queue_seq,
                prompt_message_id,
                provider,
                model,
                variant,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
        })
    }

    /// Finalize the record's effective envelope at logical-turn start (the
    /// per-message model override and the tool mode are only known once the
    /// runtime drives the turn). Only an active record is updated.
    pub fn set_turn_record_envelope(
        &self,
        session_id: SessionId,
        turn_op_id: OpId,
        provider: &str,
        model: &str,
        variant: Option<&str>,
        tool_mode: Option<&str>,
    ) -> StoreResult<bool> {
        let provider = provider.to_owned();
        let model = model.to_owned();
        let variant = variant.map(|v| v.to_owned());
        let tool_mode = tool_mode.map(|v| v.to_owned());
        self.writer.execute("set_turn_record_envelope", move |conn| {
        let n = conn.execute(
            "UPDATE turn_record SET effective_provider = ?3, effective_model = ?4, variant = ?5, tool_mode = ?6, updated_ms = ?7
             WHERE session_id = ?1 AND turn_op_id = ?2 AND status = 'active'",
            params![
                session_id.raw() as i64,
                turn_op_id.raw() as i64,
                provider,
                model,
                variant,
                tool_mode,
                now_ms()
            ],
        )?;
        Ok(n > 0)
        })
    }

    /// Close an active turn record (completed | cancelled | failed).
    /// No-op when the record is absent or already closed (idempotent).
    pub fn finish_turn_record(
        &self,
        session_id: SessionId,
        turn_op_id: OpId,
        status: &str,
    ) -> StoreResult<bool> {
        if !matches!(
            status,
            TURN_RECORD_COMPLETED | TURN_RECORD_CANCELLED | TURN_RECORD_FAILED
        ) {
            return Err(StoreError::Migration(format!(
                "finish_turn_record: invalid status {status:?}"
            )));
        }
        let status = status.to_owned();
        self.writer.execute("finish_turn_record", move |conn| {
            let n = conn.execute(
                "UPDATE turn_record SET status = ?3, updated_ms = ?4
             WHERE session_id = ?1 AND turn_op_id = ?2 AND status = 'active'",
                params![
                    session_id.raw() as i64,
                    turn_op_id.raw() as i64,
                    status,
                    now_ms()
                ],
            )?;
            Ok(n > 0)
        })
    }

    /// The session's single active logical-turn record (at most one exists).
    pub fn active_turn_record(&self, session_id: SessionId) -> StoreResult<Option<TurnRecordRow>> {
        let conn = self.read()?;
        query_row_optional(
            &conn,
            "SELECT id, session_id, turn_op_id, queue_seq, prompt_message_id, effective_provider, effective_model, variant, tool_mode, started_at, status, updated_ms
             FROM turn_record WHERE session_id = ?1 AND status = 'active'
             ORDER BY started_at DESC, id DESC LIMIT 1",
            params![session_id.raw() as i64],
            turn_record_map,
        )
    }

    pub fn turn_record_of(
        &self,
        session_id: SessionId,
        turn_op_id: OpId,
    ) -> StoreResult<Option<TurnRecordRow>> {
        let conn = self.read()?;
        query_row_optional(
            &conn,
            "SELECT id, session_id, turn_op_id, queue_seq, prompt_message_id, effective_provider, effective_model, variant, tool_mode, started_at, status, updated_ms
             FROM turn_record WHERE session_id = ?1 AND turn_op_id = ?2",
            params![session_id.raw() as i64, turn_op_id.raw() as i64],
            turn_record_map,
        )
    }

    /// Every turn record of a session (oldest first; diagnostics/tests).
    pub fn turn_records_of(&self, session_id: SessionId) -> StoreResult<Vec<TurnRecordRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, turn_op_id, queue_seq, prompt_message_id, effective_provider, effective_model, variant, tool_mode, started_at, status, updated_ms
             FROM turn_record WHERE session_id = ?1 ORDER BY started_at ASC, id ASC",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(turn_record_map(row)?);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- provider calls

    /// EVERY unfinished (still `running`) tool run across ALL sessions —
    /// `doctor --deep` and cross-session recovery audits. Unfinished rows
    /// are crash leftovers that recovery replays at the next start. Doctor
    /// reports them as information (a live daemon legitimately has running
    /// rows) rather than as errors.
    pub fn all_running_tool_rows(&self) -> StoreResult<Vec<ToolRunRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, op_id, tool, args, status, started_ms, ended_ms, effect_status, recovery, expected_hash, replay_descriptor, attempt, postcondition
             FROM tool_run WHERE status = 'running' ORDER BY started_ms ASC, id ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(tool_run_map(row)?);
        }
        Ok(out)
    }

    /// Every ACTIVE logical-turn record across ALL sessions (`doctor
    /// --deep`): at most one active turn may exist per session while a
    /// daemon is live, so several active rows after a crash are the durable
    /// picture recovery resumes from. Informational in doctor.
    pub fn all_active_turns(&self) -> StoreResult<Vec<TurnRecordRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, turn_op_id, queue_seq, prompt_message_id, effective_provider, effective_model, variant, tool_mode, started_at, status, updated_ms
             FROM turn_record WHERE status = 'active' ORDER BY started_at ASC, id ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(turn_record_map(row)?);
        }
        Ok(out)
    }

    /// The wave-16 verification-record consistency invariant (`doctor
    /// --deep`, read-only, P0-97). Issue kinds:
    ///
    /// - `record_without_task` — a record whose `task_id` matches no task
    ///   row at all;
    /// - `passed_on_uncompleted` — a `Passed` record certifying the CURRENT
    ///   revision of a task that is not `VerifiedComplete` (a `Passed` claim
    ///   only holds at the completed revision; the completion transaction
    ///   would have consumed it);
    /// - `verified_without_record` — a `VerifiedComplete` task with no
    ///   `Passed` record certifying the revision the completion consumed
    ///   (revision N requires a record certifying N-1: completion bumps the
    ///   row exactly once).
    pub fn verification_record_invariants(&self) -> StoreResult<VerificationInvariantScan> {
        let conn = self.read()?;
        let mut scan = VerificationInvariantScan::default();
        // Tasks: session_id, task_id, state, revision (typed parse: a
        // corrupt state text fails the scan loudly — never guessed).
        struct TaskRef {
            session_id: i64,
            task_id: i64,
            state: TaskState,
            revision: i64,
        }
        let mut tasks: Vec<TaskRef> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT session_id, task_id, state, revision FROM task ORDER BY session_id ASC, task_id ASC",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let task = TaskRef {
                    session_id: row.get(0)?,
                    task_id: row.get(1)?,
                    state: parse_json(
                        &format!(
                            "task {}/{} state",
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?
                        ),
                        &row.get::<_, String>(2)?,
                    )?,
                    revision: row.get(3)?,
                };
                if task.state.is_completion_relevant() {
                    scan.relevant_tasks += 1;
                }
                if task.state == TaskState::VerifiedComplete {
                    scan.completed_tasks += 1;
                }
                tasks.push(task);
            }
        }
        // Records: id, task_id, revision, status (typed parse of the status
        // JSON text, same corruption contract as the row mappers).
        struct RecordRef {
            id: i64,
            task_id: i64,
            revision: i64,
            status: VerificationStatus,
        }
        let mut records: Vec<RecordRef> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id, task_id, revision, status FROM verification_record ORDER BY id ASC",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                records.push(RecordRef {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    revision: row.get(2)?,
                    status: parse_json(
                        &format!("verification_record {} status", row.get::<_, i64>(0)?),
                        &row.get::<_, String>(3)?,
                    )?,
                });
            }
        }
        scan.total_records = records.len() as u64;
        // (1) Records whose task row is gone entirely.
        for r in &records {
            if !tasks.iter().any(|t| t.task_id == r.task_id) {
                scan.issues.push(VerificationInvariantIssue {
                    kind: "record_without_task",
                    detail: format!(
                        "verification record {} (status {:?}, revision {}) references task {} which has no task row",
                        r.id, r.status, r.revision, r.task_id
                    ),
                });
            }
        }
        // (2) A Passed record may certify the current revision only of a
        // VerifiedComplete task.
        for t in &tasks {
            if t.state == TaskState::VerifiedComplete {
                continue;
            }
            for r in &records {
                if r.task_id == t.task_id
                    && r.status == VerificationStatus::Passed
                    && r.revision == t.revision
                {
                    scan.issues.push(VerificationInvariantIssue {
                        kind: "passed_on_uncompleted",
                        detail: format!(
                            "Passed verification record {} certifies the current revision {} of task {}/{} whose state is {:?}, not VerifiedComplete",
                            r.id, t.revision, t.session_id, t.task_id, t.state
                        ),
                    });
                }
            }
        }
        // (3) VerifiedComplete without the Passed record the completion
        // consumed (revision N needs a Passed record certifying N-1).
        for t in &tasks {
            if t.state != TaskState::VerifiedComplete {
                continue;
            }
            let certified = if t.revision >= 2 {
                records.iter().any(|r| {
                    r.task_id == t.task_id
                        && r.status == VerificationStatus::Passed
                        && r.revision == t.revision - 1
                })
            } else {
                false
            };
            if !certified {
                scan.issues.push(VerificationInvariantIssue {
                    kind: "verified_without_record",
                    detail: format!(
                        "task {}/{} is VerifiedComplete at revision {} but no Passed verification record certifies its completion revision {}",
                        t.session_id, t.task_id, t.revision, t.revision.saturating_sub(1)
                    ),
                });
            }
        }
        Ok(scan)
    }

    /// The active-turn recoverable-owner invariant (`doctor --deep`,
    /// read-only, P0-97): a live daemon legitimately owns active turn rows
    /// in memory, so doctor's question is the crashed-daemon one — can
    /// recovery own this row? A turn is recoverable when at least one
    /// durable anchor exists: its prompt message row, its prompt-queue row,
    /// a journal event naming its turn op, or a tool-run row naming it.
    pub fn active_turn_ownership_invariants(&self) -> StoreResult<TurnOwnershipScan> {
        let conn = self.read()?;
        let mut scan = TurnOwnershipScan::default();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, turn_op_id, queue_seq, prompt_message_id
             FROM turn_record WHERE status = 'active' ORDER BY id ASC",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let record_id: i64 = row.get(0)?;
            let session_id: i64 = row.get(1)?;
            let turn_op_id: i64 = row.get(2)?;
            let queue_seq: Option<i64> = row.get(3)?;
            let prompt_message_id: Option<i64> = row.get(4)?;
            scan.active_turns += 1;
            let anchored = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> StoreResult<bool> {
                let n: i64 = conn.query_row(sql, params, |r| r.get(0))?;
                Ok(n > 0)
            };
            let message_anchor = match prompt_message_id {
                Some(mid) => anchored(
                    // `prompt_message_id` records the prompt's message SEQ
                    // (== the PromptReceived journal event seq); the row id
                    // is a separate autoincrement.
                    "SELECT COUNT(*) FROM message WHERE session_id = ?1 AND seq = ?2",
                    &[&session_id, &mid],
                )?,
                None => false,
            };
            let queue_anchor = match queue_seq {
                Some(seq) => anchored(
                    "SELECT COUNT(*) FROM prompt_queue WHERE session_id = ?1 AND seq = ?2",
                    &[&session_id, &seq],
                )?,
                None => false,
            };
            let event_anchor = anchored(
                "SELECT COUNT(*) FROM event WHERE session_id = ?1 AND op_id = ?2",
                &[&session_id, &turn_op_id],
            )?;
            let tool_anchor = anchored(
                "SELECT COUNT(*) FROM tool_run WHERE session_id = ?1 AND op_id = ?2",
                &[&session_id, &turn_op_id],
            )?;
            if message_anchor || queue_anchor || event_anchor || tool_anchor {
                scan.recoverable += 1;
            } else {
                scan.unrecoverable.push(UnrecoverableActiveTurn {
                    record_id,
                    session_id: id_field(
                        &format!("turn_record {record_id} session_id"),
                        session_id,
                    )?,
                    turn_op_id: id_field(
                        &format!("turn_record {record_id} turn_op_id"),
                        turn_op_id,
                    )?,
                    detail: format!(
                        "active turn record {record_id} of session {session_id} (op {turn_op_id}) has no prompt message row, no prompt-queue row, no journal event and no tool-run row naming it — nothing can recover it after a crash"
                    ),
                });
            }
        }
        Ok(scan)
    }

    /// Whether a task in `state` may begin a NEW paid provider operation.
    /// A task that entered the verification/completion path
    /// (`NeedsVerification`/`Verifying`) or any terminal state
    /// (`VerifiedComplete`/`Failed`/`Cancelled`) may not: its accounting is
    /// being closed, and a reservation landing now could strand money after
    /// the completion gate. `Pending`/`Planning`/`Running`/`Waiting`/
    /// `Blocked` permit new provider operations.
    pub(crate) fn task_state_permits_provider_operation(state: TaskState) -> bool {
        !state.is_terminal() && !state.is_completion_relevant()
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
    fn corrupt_tool_run_args_and_recovery_return_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .start_tool_run(
                s.id,
                OpId::new(1),
                "write_file",
                serde_json::json!({"path": "/a"}),
                serde_json::json!({"strategy": "verify_hash"}),
                None,
                None,
            )
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE tool_run SET args = 'broken{' WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.pending_tool_runs(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt tool_run args must error, not panic: {other:?}"),
        }
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE tool_run SET args = '{\"a\":1}', recovery = 'broken{' WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.pending_tool_runs(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt tool_run recovery must error, not panic: {other:?}"),
        }
    }

    #[test]
    fn corrupt_task_ledger_returns_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_task_ledger(s.id, serde_json::json!({"tasks": []}))
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task_ledger SET ledger = 'garbage' WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_task_ledger(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt ledger must error, not panic: {other:?}"),
        }
    }

    #[test]
    fn tool_run_recovery_columns_roundtrip_and_attempts() {
        // v7: the replay descriptor, the physical-attempt counter and the
        // workspace-write postcondition ride the tool_run row.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let op = OpId::new(7);
        store
            .start_tool_run(
                s.id,
                op,
                "echo",
                serde_json::json!({"x": 1}),
                serde_json::json!({"strategy": "idempotent"}),
                None,
                Some(serde_json::json!({"tool_name": "echo", "validated_args": {"x": 1}})),
            )
            .unwrap();
        let pending = store.pending_tool_runs(s.id).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempt, 0, "the original run is attempt 0");
        assert_eq!(
            pending[0].replay_descriptor.as_ref().unwrap()["tool_name"],
            "echo"
        );
        // Postcondition annotation (recorded at execution end, pre-finish).
        store
            .record_tool_postcondition(
                s.id,
                op,
                &serde_json::json!({
                    "workspace_id": ws.raw(),
                    "worktree_id": 1,
                    "relative_path": "a.txt",
                    "expected_hash": "ab".repeat(32),
                }),
            )
            .unwrap();
        assert_eq!(
            store.pending_tool_runs(s.id).unwrap()[0]
                .postcondition
                .as_ref()
                .unwrap()["relative_path"],
            "a.txt"
        );
        // A replay bumps the attempt counter of the SAME logical row.
        assert_eq!(store.bump_tool_run_attempt(s.id, op).unwrap(), 1);
        // Hostile annotation on a finished row is loud.
        store
            .finish_tool_run(s.id, op, "completed", "applied")
            .unwrap();
        assert!(store.pending_tool_runs(s.id).unwrap().is_empty());
        assert!(store
            .record_tool_postcondition(s.id, op, &serde_json::json!({}))
            .is_err());
        assert!(store.bump_tool_run_attempt(s.id, op).is_err());
    }

    #[test]
    fn tool_run_recovery_columns_and_turn_records_survive_reopen() {
        // Requirement 2c: the descriptor + postcondition + attempt survive a
        // daemon restart and still drive a replay.
        let dir = tempfile::tempdir().unwrap();
        let (sid, op) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let op = OpId::new(31);
            store
                .start_tool_run(
                    s.id,
                    op,
                    "echo",
                    serde_json::json!({"x": 1}),
                    serde_json::json!({"strategy": "idempotent"}),
                    None,
                    Some(serde_json::json!({"tool_name": "echo"})),
                )
                .unwrap();
            store
                .record_tool_postcondition(s.id, op, &serde_json::json!({"relative_path": "a.txt"}))
                .unwrap();
            store
                .start_turn_record(s.id, OpId::new(99), None, Some(2), "p", "m", None)
                .unwrap();
            (s.id, op)
        };
        let store = Store::open(dir.path(), true).unwrap();
        let pending = store.pending_tool_runs(sid).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op_id, op);
        assert_eq!(pending[0].attempt, 0);
        assert_eq!(
            pending[0].replay_descriptor.as_ref().unwrap()["tool_name"],
            "echo"
        );
        assert_eq!(
            pending[0].postcondition.as_ref().unwrap()["relative_path"],
            "a.txt"
        );
        assert_eq!(store.bump_tool_run_attempt(sid, op).unwrap(), 1);
        let rec = store.turn_record_of(sid, OpId::new(99)).unwrap().unwrap();
        assert_eq!(rec.status, TURN_RECORD_ACTIVE);
        assert_eq!(rec.effective_model, "m");
    }

    #[test]
    fn durable_task_spend_sources_are_durable_and_monotone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        assert_eq!(store.session_usage_tokens(s.id).unwrap(), 0);
        assert_eq!(store.turn_completed_count(s.id).unwrap(), 0);
        let op = OpId::new(1);
        store
            .record_provider_call(s.id, op, "p", "m", "started", None, None, None)
            .unwrap();
        // In-flight (not yet completed) calls count their tokens too: a
        // crash can never lose spend that a gate already saw.
        store
            .record_provider_call(s.id, op, "p", "m", "completed", Some(40), Some(10), None)
            .unwrap();
        assert_eq!(store.session_usage_tokens(s.id).unwrap(), 50);
        // TurnCompleted journal events are the durable turn counter.
        store
            .append_event(
                s.id,
                Some(op),
                faktor_core::event::EventKind::TurnCompleted,
                AgentState::ReadyForNextTurn,
                5,
                None,
            )
            .unwrap();
        assert_eq!(store.turn_completed_count(s.id).unwrap(), 1);
        // Session isolation: a second session's spend never leaks.
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "t2", "p", "m").unwrap();
        assert_eq!(store.session_usage_tokens(s2.id).unwrap(), 0);
        assert_eq!(store.turn_completed_count(s2.id).unwrap(), 0);
    }

    // ---- prefix-cache stability columns (v13, audits 65-66) ----
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
    fn completion_path_validates_every_proof_facet_in_one_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into(), "c2".into()]);
        let now = now_ms();
        // (b) missing record: typed refusal, row untouched.
        let miss = store
            .task_complete_verified(
                s.id,
                task.task_id,
                task.revision,
                VerificationRecordId::new(999),
                now,
            )
            .unwrap()
            .unwrap_err();
        assert!(matches!(miss, TaskCompletionRefusal::RecordMissing { .. }));
        let record = passing_record(&task, ws, WorktreeId::new(1));
        let rec_id = store.verification_record_put(&record).unwrap();
        // Happy path: one transaction validates (a)-(g) and completes.
        let done = store
            .task_complete_verified(s.id, task.task_id, task.revision, rec_id, now)
            .unwrap()
            .unwrap();
        assert_eq!(done.state, TaskState::VerifiedComplete);
        assert_eq!(done.revision, task.revision.checked_next().unwrap());
        assert_eq!(store.get_task(s.id, task.task_id).unwrap().unwrap(), done);
        // A second completion is refused: stale revision first, then the
        // machine (the task is no longer Verifying).
        let again = store
            .task_complete_verified(s.id, task.task_id, task.revision, rec_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            again,
            TaskCompletionRefusal::RevisionMismatch { .. }
        ));
        let again = store
            .task_complete_verified(s.id, task.task_id, done.revision, rec_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            again,
            TaskCompletionRefusal::NotVerifying {
                actual: TaskState::VerifiedComplete
            }
        ));
        assert_eq!(
            store
                .get_task(s.id, task.task_id)
                .unwrap()
                .unwrap()
                .revision,
            done.revision,
            "refused completions never bump"
        );

        // (c) wrong task: a record certifying task 1 cannot complete task 2.
        let task2 = seed_verifying(&store, s.id, TaskId::new(2), vec!["c1".into()]);
        let r2 = store
            .task_complete_verified(s.id, task2.task_id, task2.revision, rec_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(r2, TaskCompletionRefusal::RecordWrongTask { .. }));

        // (d) wrong revision: a record certifying task 2's CURRENT revision
        // cannot complete task 2 once the task has moved PAST it (the
        // record must certify the revision being completed).
        let task2_rec = passing_record(&task2, ws, WorktreeId::new(1));
        let t2_id = store.verification_record_put(&task2_rec).unwrap();
        let mut bumped = store.get_task(s.id, TaskId::new(2)).unwrap().unwrap();
        bumped.revision = bumped.revision.checked_next().unwrap();
        store.upsert_task(&bumped).unwrap();
        let stale = store
            .task_complete_verified(s.id, task2.task_id, bumped.revision, t2_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            stale,
            TaskCompletionRefusal::RecordWrongRevision { .. }
        ));

        // (e) Failed record refuses.
        let task3 = seed_verifying(&store, s.id, TaskId::new(3), vec!["c1".into()]);
        let mut failed_rec = passing_record(&task3, ws, WorktreeId::new(1));
        failed_rec.status = VerificationStatus::Failed;
        let f_id = store.verification_record_put(&failed_rec).unwrap();
        let r3 = store
            .task_complete_verified(s.id, task3.task_id, task3.revision, f_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            r3,
            TaskCompletionRefusal::RecordNotPassed {
                status: VerificationStatus::Failed,
                ..
            }
        ));

        // (f) a record missing one criterion refuses with the missing list.
        let task4 = seed_verifying(&store, s.id, TaskId::new(4), vec!["c1".into(), "c2".into()]);
        let mut partial = passing_record(&task4, ws, WorktreeId::new(1));
        partial.criteria.pop();
        let p_id = store.verification_record_put(&partial).unwrap();
        let r4 = store
            .task_complete_verified(s.id, task4.task_id, task4.revision, p_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            r4,
            TaskCompletionRefusal::CriteriaNotCovered { missing, .. }
                if missing == vec!["c2".to_string()]
        ));
        // Extra record criteria are fine: a record covering c1..c2 plus an
        // extra c3 completes.
        let mut extra = passing_record(&task4, ws, WorktreeId::new(1));
        extra.criteria.push(CriterionVerification {
            criterion_key: "c3".into(),
            passed: true,
            evidence: None,
            binding: None,
        });
        let e_id = store.verification_record_put(&extra).unwrap();
        let ok4 = store
            .task_complete_verified(s.id, task4.task_id, task4.revision, e_id, now)
            .unwrap()
            .unwrap();
        assert_eq!(ok4.state, TaskState::VerifiedComplete);
        // A passed=false entry for a task criterion counts as NOT covered.
        let task5 = seed_verifying(&store, s.id, TaskId::new(5), vec!["c1".into()]);
        let mut lying = passing_record(&task5, ws, WorktreeId::new(1));
        lying.criteria[0].passed = false;
        let l_id = store.verification_record_put(&lying).unwrap();
        let r5 = store
            .task_complete_verified(s.id, task5.task_id, task5.revision, l_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            r5,
            TaskCompletionRefusal::CriteriaNotCovered { missing, .. }
                if missing == vec!["c1".to_string()]
        ));

        // (g) worktree mismatch: a record certified against another
        // workspace's identity refuses even when everything else matches.
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "t2", "p", "m").unwrap();
        let task6 = seed_verifying(&store, s2.id, TaskId::new(1), vec!["c1".into()]);
        let mut foreign = passing_record(&task6, ws, WorktreeId::new(1));
        foreign.workspace_id = ws;
        let x_id = store.verification_record_put(&foreign).unwrap();
        let r6 = store
            .task_complete_verified(s2.id, task6.task_id, task6.revision, x_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(r6, TaskCompletionRefusal::WorktreeMismatch { .. }));

        // Revision mismatch of the TASK (expected != actual) is reported
        // before the record is even consulted.
        let stale_expected2 = store
            .task_complete_verified(s.id, task2.task_id, task2.revision, rec_id, now)
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            stale_expected2,
            TaskCompletionRefusal::RevisionMismatch { .. }
        ));
    }

    #[test]
    fn completion_gate_refuses_every_held_reservation_status_and_keeps_verifying() {
        // The SQL gate is exhaustive over the three budget-holding statuses:
        // each refuses the completion transaction typed with the exact
        // counts and leaves the task row byte-untouched (rollback).
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        for (n, status) in ["reserved", "dispatched", "uncertain"]
            .into_iter()
            .enumerate()
        {
            let task_id = TaskId::new(n as u64 + 1);
            // The reserve is admitted while the task still permits new
            // provider work (Running); the task then walks the legal edges
            // into Verifying — the completion gate must catch the row the
            // accounting pass raced.
            seed_task(&store, s.id, task_id, vec![], TaskState::Running);
            let rid = match store
                .cost_reserve_priced(s.id, task_id, OpId::new(100 + n as u64), 77, now_ms(), None)
                .unwrap()
            {
                CostReserveOutcome::Granted(id) => id,
                other => panic!("{status}: reserve refused: {other:?}"),
            };
            if status != "reserved" {
                assert!(
                    matches!(
                        store.cost_mark_dispatched(rid, now_ms()).unwrap(),
                        CostReservationState::Applied
                    ),
                    "{status}: dispatch marker"
                );
            }
            if status == "uncertain" {
                assert!(
                    matches!(
                        store
                            .cost_mark_uncertain(rid, "race-fault", None, now_ms())
                            .unwrap(),
                        CostReservationState::Applied
                    ),
                    "{status}: uncertain marker"
                );
            }
            let mut row = store.get_task(s.id, task_id).unwrap().unwrap();
            row.state = TaskState::NeedsVerification;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
            row.state = TaskState::Verifying;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
            let verifying_revision = row.revision;
            let record = passing_record(&row, ws, WorktreeId::new(1));
            let rec_id = store.verification_record_put(&record).unwrap();
            let refusal = store
                .task_complete_verified(s.id, task_id, verifying_revision, rec_id, now_ms())
                .unwrap()
                .unwrap_err();
            match (status, refusal) {
                (
                    "reserved",
                    TaskCompletionRefusal::ReservationsHeld {
                        reserved,
                        dispatched,
                        uncertain,
                        reserved_micro,
                        uncertain_micro,
                    },
                ) => {
                    assert_eq!((reserved, dispatched, uncertain), (1, 0, 0));
                    assert_eq!(reserved_micro, 77);
                    assert_eq!(uncertain_micro, 0);
                }
                (
                    "dispatched",
                    TaskCompletionRefusal::ReservationsHeld {
                        reserved,
                        dispatched,
                        uncertain,
                        ..
                    },
                ) => {
                    assert_eq!((reserved, dispatched, uncertain), (0, 1, 0));
                }
                (
                    "uncertain",
                    TaskCompletionRefusal::ReservationsHeld {
                        reserved,
                        dispatched,
                        uncertain,
                        uncertain_micro,
                        ..
                    },
                ) => {
                    assert_eq!((reserved, dispatched, uncertain), (0, 0, 1));
                    assert_eq!(uncertain_micro, 77);
                }
                other => panic!("{status}: wrong refusal: {other:?}"),
            }
            let row = store.get_task(s.id, task_id).unwrap().unwrap();
            assert_eq!(row.state, TaskState::Verifying, "{status}: state moved");
            assert_eq!(row.revision, verifying_revision, "{status}: revision moved");
        }
    }

    #[test]
    fn record_finalize_cas_wins_exactly_once_and_lists_are_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let put = |status: VerificationStatus| {
            store
                .verification_record_put(&VerificationRecordRow {
                    id: VerificationRecordId::new(1),
                    task_id: task.task_id,
                    revision: task.revision,
                    workspace_id: ws,
                    worktree_id: WorktreeId::new(1),
                    tree_hash: Some("ab".repeat(32)),
                    criteria: vec![],
                    checks: vec![CheckExecution {
                        check: "compile".into(),
                        program: "cargo".into(),
                        args: vec!["check".into()],
                        category: "required".into(),
                        required: true,
                        status,
                        started_ms: 1,
                        finished_ms: Some(2),
                        exit: Some(0),
                        summary: None,
                    }],
                    changed_files: vec![FileStateEvidence {
                        path: "src/main.rs".into(),
                        digest_hex: "cd".repeat(32),
                        size: 10,
                    }],
                    unrelated_changes: vec!["vendor/".into()],
                    reviewer: Some(serde_json::json!({"verdict": "pass"})),
                    status,
                    started_ms: 1,
                    completed_ms: None,
                })
                .unwrap()
        };
        let rec_a = put(VerificationStatus::Running);
        let rec_b = put(VerificationStatus::Passed);
        assert_ne!(rec_a, rec_b, "fresh row ids");
        // Deterministic list order (creation order), stable across reads.
        let list = store
            .verification_record_list_by_task(task.task_id)
            .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, rec_a);
        assert_eq!(list[1].id, rec_b);
        assert_eq!(list[0].checks[0].check, "compile");
        assert_eq!(list[1].changed_files[0].size, 10);
        assert_eq!(list[1].reviewer.as_ref().unwrap()["verdict"], "pass");
        let again = store
            .verification_record_list_by_task(task.task_id)
            .unwrap();
        assert_eq!(again, list, "deterministic across reads");
        assert!(store
            .verification_record_list_by_task(TaskId::new(404))
            .unwrap()
            .is_empty());
        // CAS finalize: exactly one Running -> Passed wins.
        assert_eq!(
            store
                .verification_record_finalize(rec_a, VerificationStatus::Passed, 99)
                .unwrap(),
            Ok(())
        );
        // Second attempt on the now-final record: typed refusal with the
        // CURRENT status (a second finalize can never rewind or re-run).
        assert_eq!(
            store
                .verification_record_finalize(rec_a, VerificationStatus::Failed, 100)
                .unwrap(),
            Err(RecordFinalizeRefusal::NotRunning {
                record_id: rec_a,
                current: VerificationStatus::Passed
            })
        );
        // A Pending record cannot finalize either (only Running may).
        let rec_c = put(VerificationStatus::Pending);
        assert_eq!(
            store
                .verification_record_finalize(rec_c, VerificationStatus::Passed, 101)
                .unwrap(),
            Err(RecordFinalizeRefusal::NotRunning {
                record_id: rec_c,
                current: VerificationStatus::Pending
            })
        );
        // A missing record is its own typed refusal.
        assert_eq!(
            store
                .verification_record_finalize(
                    VerificationRecordId::new(909),
                    VerificationStatus::Passed,
                    1
                )
                .unwrap(),
            Err(RecordFinalizeRefusal::Missing {
                record_id: VerificationRecordId::new(909)
            })
        );
        // Only Passed/Failed may finalize; Unavailable is malformed.
        assert!(matches!(
            store.verification_record_finalize(rec_c, VerificationStatus::Running, 1),
            Err(StoreError::Malformed(_))
        ));
        // The finalized row read back with its completed_ms.
        let row = store.verification_record_get(rec_a).unwrap().unwrap();
        assert_eq!(row.status, VerificationStatus::Passed);
        assert_eq!(row.completed_ms, Some(99));
    }

    #[test]
    fn records_and_completion_survive_reopen_like_a_crash_boundary() {
        // A crash can happen anywhere between record creation and the
        // completion transaction. Each boundary leaves a consistent store:
        // after a reopen the exact same completion either still applies or
        // is refused by the machine — never half-applied.
        let dir = tempfile::tempdir().unwrap();
        let (sid, tid, rec_id) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into()]);
            let rec = passing_record(&task, ws, WorktreeId::new(1));
            let rec_id = store.verification_record_put(&rec).unwrap();
            // Crash here: record durably written, task still Verifying.
            (s.id, task.task_id, rec_id)
        };
        let store = Store::open(dir.path(), true).unwrap();
        // The record survived and the task reads exactly as it crashed.
        assert_eq!(
            store
                .verification_record_get(rec_id)
                .unwrap()
                .unwrap()
                .status,
            VerificationStatus::Passed
        );
        let task = store.get_task(sid, tid).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Verifying);
        let done = store
            .task_complete_verified(sid, tid, task.revision, rec_id, now_ms())
            .unwrap()
            .unwrap();
        assert_eq!(done.state, TaskState::VerifiedComplete);
        assert_eq!(done.revision, task.revision.checked_next().unwrap());
        drop(store);
        // Crash after completion: reopen shows the completed row, and a
        // re-completion attempt is refused without touching it.
        let store = Store::open(dir.path(), true).unwrap();
        let task = store.get_task(sid, tid).unwrap().unwrap();
        assert_eq!(task.state, TaskState::VerifiedComplete);
        assert_eq!(task.revision, TaskRevision::new(5));
        let again = store
            .task_complete_verified(sid, tid, task.revision, rec_id, now_ms())
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            again,
            TaskCompletionRefusal::NotVerifying {
                actual: TaskState::VerifiedComplete
            }
        ));
        // A hostile PARTIAL write through raw SQL (state flipped out of
        // Verifying WITHOUT a revision bump) is caught by the machine on the
        // next completion attempt.
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET state = ?1 WHERE session_id = ?2 AND task_id = ?3",
                params![
                    serde_json::to_string(&TaskState::Running).unwrap(),
                    sid.raw() as i64,
                    tid.raw() as i64
                ],
            )
            .unwrap();
        }
        let task = store.get_task(sid, tid).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Running);
        let refused = store
            .task_complete_verified(sid, tid, task.revision, rec_id, now_ms())
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            refused,
            TaskCompletionRefusal::NotVerifying { .. }
        ));
    }

    #[test]
    fn corrupt_task_revision_reads_as_corruption_never_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Pending);
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET revision = 0 WHERE session_id = 1 AND task_id = 1",
                [],
            )
            .unwrap();
        }
        assert!(matches!(
            store.get_task(s.id, TaskId::new(1)),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupt_task_turn_counts_are_typed_field_errors_never_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Pending);
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET max_turns = ?1 WHERE session_id = ?2 AND task_id = 1",
                params![i64::from(u32::MAX) + 1, s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_task(s.id, TaskId::new(1)) {
            Err(StoreError::Corrupt(msgs)) => {
                assert!(
                    msgs.iter().any(|m| m.contains("max_turns")),
                    "the refusal must name the max_turns field: {msgs:?}"
                );
            }
            other => panic!("oversize persisted max_turns must be a typed error: {other:?}"),
        }
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET max_turns = NULL, spent_turns = -1
                 WHERE session_id = ?1 AND task_id = 1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_task(s.id, TaskId::new(1)) {
            Err(StoreError::Corrupt(msgs)) => {
                assert!(
                    msgs.iter().any(|m| m.contains("spent_turns")),
                    "the refusal must name the spent_turns field: {msgs:?}"
                );
            }
            other => panic!("negative persisted spent_turns must be a typed error: {other:?}"),
        }
    }

    #[test]
    fn u32_turn_counts_round_trip_without_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let mut row = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Pending);
        row.max_tokens = Some(i64::MAX as u64);
        row.max_turns = Some(u32::MAX);
        row.spent_tokens = i64::MAX as u64;
        row.spent_turns = u32::MAX;
        store.upsert_task(&row).unwrap();
        assert_eq!(
            store.get_task(s.id, TaskId::new(1)).unwrap(),
            Some(row),
            "the u32 extremes must survive the i64 column round-trip exactly"
        );
        assert_eq!(
            store.list_tasks(s.id).unwrap()[0].spent_turns,
            u32::MAX,
            "list_tasks shares the checked mapping"
        );
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
    fn attempt_reservations_and_provider_rows_key_by_attempt_op_id() {
        // (iii) Two physical attempts of the SAME logical op carry distinct
        // attempt op ids, separate reservations (each holding its own
        // prediction, each refundable independently) and separate
        // provider-call rows through the attempt writers.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        store.cost_task_cap_set(s.id, tid, Some(10_000)).unwrap();
        let now = now_ms();
        let logical = OpId::new(700);
        let a1 = faktor_core::op::ModelCallAttempt::new(logical, OpId::new(701), 0).unwrap();
        let a2 = faktor_core::op::ModelCallAttempt::new(logical, OpId::new(702), 1).unwrap();
        let CostReserveOutcome::Granted(r1) = store
            .cost_reserve_attempt(s.id, tid, &a1, 1_000, now, None)
            .unwrap()
        else {
            panic!("attempt 1 reserve granted")
        };
        let CostReserveOutcome::Granted(r2) = store
            .cost_reserve_attempt(s.id, tid, &a2, 2_000, now, None)
            .unwrap()
        else {
            panic!("attempt 2 reserve granted")
        };
        assert_ne!(r1, r2, "one reservation per attempt");
        assert_eq!(free_micro(&store, s.id, tid), 7_000, "both holds count");
        let rows = store.cost_reservations_of(s.id, tid, 10).unwrap();
        assert_eq!(rows.len(), 2);
        let row1 = rows.iter().find(|r| r.reservation_id == r1).unwrap();
        assert_eq!(row1.attempt_op_id, Some(OpId::new(701)));
        assert_eq!(row1.parent_op_id, Some(logical));
        assert_eq!(
            row1.op_id,
            OpId::new(701),
            "attempt rows key by their own op"
        );
        let row2 = rows.iter().find(|r| r.reservation_id == r2).unwrap();
        assert_eq!(row2.attempt_op_id, Some(OpId::new(702)));
        assert_ne!(row1.attempt_op_id, row2.attempt_op_id);

        // One provider-call row per attempt through the attempt writer.
        let p1 = store
            .record_provider_call_attempt(
                s.id,
                &a1,
                Some(r1),
                "fake",
                "m",
                "completed",
                Some(10),
                Some(20),
                None,
            )
            .unwrap();
        let p2 = store
            .record_provider_call_attempt(
                s.id,
                &a2,
                Some(r2),
                "fake",
                "m",
                "completed",
                Some(30),
                Some(40),
                None,
            )
            .unwrap();
        assert_ne!(p1, p2);
        let calls: Vec<(i64, i64, i64, i64, i64)> = {
            let conn = store.read().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT op_id, parent_model_call_op_id, attempt_op_id,
                            attempt_ordinal, reservation_id
                     FROM provider_call WHERE session_id = ?1 ORDER BY id ASC",
                )
                .unwrap();
            let mut out = Vec::new();
            let rows = stmt
                .query_map([s.id.raw() as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap();
            for r in rows {
                out.push(r.unwrap());
            }
            out
        };
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            (700, 700, 701, 0, r1),
            "attempt 1 row: shared logical op in op_id + parent, attempt 701"
        );
        assert_eq!(
            calls[1],
            (700, 700, 702, 1, r2),
            "attempt 2 row: same logical parent, own attempt 702, own reservation"
        );
        // Independent refunds: releasing attempt 1 leaves attempt 2's hold.
        store.cost_refund(r1, now).unwrap();
        assert_eq!(free_micro(&store, s.id, tid), 8_000);
        assert_eq!(
            store
                .cost_reservations_of(s.id, tid, 10)
                .unwrap()
                .iter()
                .find(|r| r.reservation_id == r1)
                .unwrap()
                .status,
            "refunded"
        );
        store.cost_refund(r2, now).unwrap();
        assert_eq!(free_micro(&store, s.id, tid), 10_000);
    }
}
