//! `runtime::settlement`: cohesive slice of the agent runtime.

#![allow(unused_imports)]

use super::*;

// ------------------------------------------------- durable-write failure guard
//
// POLICY (silent durable-write discard audit): a durable state transition is
// never silently lost. Callers that can fail safely propagate the write error
// with `?` (the wrap helpers below); failure/cleanup paths whose original
// error/outcome must survive route the write through a `dw_note_*` helper,
// which on failure records the transition in a durable retry-on-next-open
// MARKER FILE under the session store root, journals a `CrashDetected` audit
// event and logs a structured `tracing::error!`. Every session open funnels
// through `recover_session`, whose FIRST step (before any sweep) is
// [`AgentRuntime::replay_durable_write_failures`] — so a transition lost to a
// failed write is reconstructed before the session is ever driven again.

/// Directory (under the session store root) holding retry-on-next-open marker
/// files. Deliberately NOT inside SQLite: a locked/corrupt store still leaves
/// this channel writable, which is the point of a compensation channel.
pub(crate) const DURABLE_WRITE_MARKER_DIR: &str = "durable-write-markers";

/// Hard bound of one marker file (the largest replayed intent is a bounded
/// verification result JSON; oversized string fields are truncated and the
/// marker says so).
pub(crate) const DURABLE_WRITE_MARKER_MAX_BYTES: usize = 256 * 1024;

/// Bounded marker consumption per open (adversarial defense: a flooded marker
/// directory degrades loud and bounded, never unbounded).
pub(crate) const DURABLE_WRITE_MARKER_SCAN_MAX: usize = 256;

/// Marker replay attempts before the marker is abandoned (surfaced loudly).
pub(crate) const DURABLE_WRITE_MARKER_MAX_ATTEMPTS: u64 = 8;

/// Bounded create retries when a marker file name collides (two processes
/// sharing one store root can produce the same clock-ms + seq base; the name
/// also carries pid + a random tag, and creation is `create_new` — an existing
/// marker is NEVER overwritten).
pub(crate) const DURABLE_WRITE_MARKER_CREATE_ATTEMPTS: usize = 8;

/// Total files allowed in the marker directory before GC consumes the OLDEST
/// terminal/unreadable markers. Pending markers are NEVER deleted by GC.
pub(crate) const DURABLE_WRITE_MARKER_DIR_MAX: usize = 4096;

/// Test-only override of the directory bound (0 = the production bound).
#[cfg(test)]
pub(crate) static DURABLE_WRITE_MARKER_DIR_MAX_OVERRIDE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Page size of the durable replay dedup scans (journal events / ledger
/// rows). The scans are COMPLETE (down to the first row), never a fixed
/// tail window, so a committed-then-errored write followed by arbitrary
/// later activity can never replay as a duplicate.
pub(crate) const DW_DEDUP_PAGE: u64 = 512;

/// Hard page bound of one replay dedup scan. Beyond it the scan stops with a
/// LOUD error and reports "already present" — a duplicate row is worse than
/// a skipped redundant append (the durable record itself stays authoritative).
pub(crate) const DW_DEDUP_MAX_PAGES: u32 = 4096;

// Stable site names of every discard audited (and every sibling discard in
// this file): a marker/trace always names the write it compensates.
pub(crate) const DW_SITE_RECEIPT_JOURNAL: &str = "drive_receipt.failed_journal";

pub(crate) const DW_SITE_RECEIPT_RECORD: &str = "drive_receipt.finish_turn_record";

pub(crate) const DW_SITE_ADMITTED_JOURNAL: &str = "drive_admitted.failed_journal";

pub(crate) const DW_SITE_ADMITTED_RECORD: &str = "drive_admitted.finish_turn_record";

pub(crate) const DW_SITE_RECOVERY_RECORD: &str = "recover_session.finish_turn_record";

pub(crate) const DW_SITE_CONTINUE_RECORD_DONE: &str = "continue_record.finish_turn_record";

pub(crate) const DW_SITE_CONTINUE_RECORD_FAILED: &str = "continue_record.failed_record";

pub(crate) const DW_SITE_DRIVE_RECORD: &str = "drive_turn.finish_turn_record";

pub(crate) const DW_SITE_ROUTING_JOURNAL: &str = "drive_turn_inner.routing_failed_journal";

pub(crate) const DW_SITE_BUDGET_JOURNAL: &str = "drive_turn_inner.budget_failed_journal";

pub(crate) const DW_SITE_DEBIT_IDENTITY_JOURNAL: &str = "drive_turn_inner.debit_identity_journal";

pub(crate) const DW_SITE_DEBIT_REFUSED_JOURNAL: &str = "drive_turn_inner.debit_refused_journal";

pub(crate) const DW_SITE_STALL_JOURNAL: &str = "drive_turn_inner.stall_journal";

pub(crate) const DW_SITE_STALL_EVIDENCE_JOURNAL: &str = "drive_turn_inner.stall_evidence_journal";

pub(crate) const DW_SITE_LOOP_JOURNAL: &str = "drive_turn_inner.loop_journal";

pub(crate) const DW_SITE_LOOP_DECISION: &str = "drive_turn_inner.loop_decision";

pub(crate) const DW_SITE_PROVIDER_FAILURE_JOURNAL: &str = "handle_provider_failure.failed_journal";

pub(crate) const DW_SITE_SUPERSEDE_CANCEL: &str = "run_turn_verification.supersede_cancel";

pub(crate) const DW_SITE_ROOT_ATTEMPT_CANCEL: &str =
    "verify_integrated_root_attempt.supersede_cancel";

pub(crate) const DW_SITE_BEGIN_ATTEMPT_CANCEL: &str =
    "begin_integrated_root_attempt.supersede_cancel";

pub(crate) const DW_SITE_BACKGROUND_FACT: &str = "run_turn_verification.background_fact";

pub(crate) const DW_SITE_CHECK_FAILED_FACT: &str = "run_turn_verification.check_failed_fact";

pub(crate) const DW_SITE_CHECK_UNAVAILABLE_FACT: &str =
    "run_turn_verification.check_unavailable_fact";

pub(crate) const DW_SITE_SETTLE_UNAVAILABLE_FACT: &str =
    "settle_verification_jobs.unavailable_fact";

pub(crate) const DW_SITE_SETTLE_FAILED_FACT: &str = "settle_verification_jobs.failed_fact";

pub(crate) const DW_SITE_GATE_TASK_STATE_FACT: &str = "persist_gate_facts.task_state";

pub(crate) const DW_SITE_GATE_LAST_FACT: &str = "persist_gate_facts.verification_last";

pub(crate) const DW_SITE_GATE_CRITERIA_FACT: &str = "persist_gate_facts.criteria";

pub(crate) const DW_SITE_GATE_BUDGET_FACT: &str = "finish_logical_turn.budget_blocked_fact";

pub(crate) const DW_SITE_GATE_BUDGET_DECISION: &str = "finish_logical_turn.budget_refusal_decision";

pub(crate) const DW_SITE_GATE_CHANGE_FACT: &str = "finish_logical_turn.change_budget_blocked_fact";

pub(crate) const DW_SITE_GATE_CHANGE_DECISION: &str =
    "finish_logical_turn.change_budget_refusal_decision";

pub(crate) const DW_SITE_GATE_CRITERIA_BLOCK_FACT: &str =
    "finish_logical_turn.criteria_blocked_fact";

pub(crate) const DW_SITE_GATE_CRITERIA_BLOCK_DECISION: &str =
    "finish_logical_turn.criteria_refusal_decision";

pub(crate) const DW_SITE_GATE_DOWNGRADE_FACT: &str = "finish_logical_turn.downgrade_blocked_fact";

pub(crate) const DW_SITE_GATE_DOWNGRADE_DECISION: &str =
    "finish_logical_turn.downgrade_refusal_decision";

pub(crate) const DW_SITE_JOB_RESOLVE_CORRUPT: &str = "execute_attempt_jobs.resolve_corrupt_spec";

pub(crate) const DW_SITE_JOB_RESOLVE: &str = "execute_attempt_jobs.resolve_job";

pub(crate) const DW_SITE_LEDGER_REBUILD: &str = "load_ledger.rebuild_corrupt_row";

pub(crate) const DW_SITE_EXECUTOR_REFUSAL_RECORD: &str =
    "task_executor.admit_refused_isolation.finish_turn_record";

pub(crate) const DW_SITE_ROUTE_AFTER_REFUSAL: &str = "apply_gate_to_task_row.refusal_route";

pub(crate) const DW_SITE_ATTEMPT_RECORD_LOOP_SIGNALS: &str =
    "create_attempt_record.reset_loop_signals";

pub(crate) const DW_SITE_RESTORE_GOAL_FACT: &str = "restore_task_rows.goal_fact";

pub(crate) const DW_SITE_RESTORE_STATE_FACT: &str = "restore_task_rows.state_fact";

pub(crate) const DW_SITE_RESTORE_CRITERIA_FACT: &str = "restore_task_rows.criteria_fact";

pub(crate) const DW_SITE_END_LOOP_SIGNALS: &str = "finish_logical_turn.reset_loop_signals";

pub(crate) const DW_SITE_TURN_ENVELOPE: &str = "drive_turn_inner.set_turn_envelope";

pub(crate) const DW_SITE_DRIVE_ABORT_CANCEL: &str = "drive_turn_inner.abort_cancelled";

pub(crate) const DW_SITE_DRIVE_ABORT_DISPATCH: &str = "drive_turn_inner.abort_after_dispatch";

/// The typed tool_result part answering one REFUSED tool call (the denial
/// decision itself is journaled by its own refusal path; this is only the
/// transcript answer the model must see). Test seam: a failed write here
/// exercises the crash-resume dangling-call repair.
pub(crate) const DW_SITE_DENIAL_RESULT: &str = "run_tool_calls.denial_result";

/// Queue runner: the durable queue-head RE-CHECK failed (a store read error).
/// Never treated as an empty queue; recorded as a durable retry marker when
/// the bounded retries are exhausted.
pub(crate) const DW_SITE_QUEUE_HEAD_READ: &str = "run_session_queue.pending_head_read";

/// Process-local uniqueness tail of one marker file name.
pub(crate) static DURABLE_WRITE_MARKER_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Directory bound actually in force (test override or production bound).
pub(crate) fn durable_write_marker_dir_max() -> usize {
    #[cfg(test)]
    {
        let overridden =
            DURABLE_WRITE_MARKER_DIR_MAX_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
        if overridden > 0 {
            return overridden;
        }
    }
    DURABLE_WRITE_MARKER_DIR_MAX
}

/// Create a NEW marker file and write `bytes` into it; an existing file at
/// the candidate name is NEVER overwritten — `create_new` is the collision
/// detector, and a collision retries a fresh suffix (bounded). A partial
/// write is removed before the error is returned (a half-written marker is
/// worse than none).
pub(crate) fn write_marker_file(
    dir: &std::path::Path,
    key: &str,
    bytes: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    for attempt in 0..DURABLE_WRITE_MARKER_CREATE_ATTEMPTS {
        let filename = if attempt == 0 {
            format!("{key}.json")
        } else {
            format!("{key}-{attempt}.json")
        };
        let path = dir.join(filename);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "marker name `{key}` collided {DURABLE_WRITE_MARKER_CREATE_ATTEMPTS} times; \
             refusing to overwrite an existing marker"
        ),
    ))
}

/// The typed intent of one durable write whose failure is compensated by a
/// retry-on-next-open marker (serde-tagged so every marker is self-describing
/// and replayable by an older/newer binary's own variant set).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "write", rename_all = "snake_case")]
pub(crate) enum DurableWriteIntent {
    /// A `Failed` journal append (plus its paired turn-record close).
    JournalFailed {
        op_id: u64,
        state: String,
        payload: serde_json::Value,
    },
    FinishTurnRecord {
        turn_op: u64,
        status: String,
    },
    ResolveVerificationJob {
        task_id: u64,
        attempt_op: u64,
        check_id: String,
        state: String,
        note: Option<String>,
        result_json: Option<String>,
    },
    CancelVerificationAttempt {
        task_id: u64,
        attempt_op: u64,
        note: String,
    },
    UpsertMemoryFact {
        kind: String,
        key: String,
        value: String,
    },
    Abort {
        op_id: Option<u64>,
    },
    LedgerDecision {
        step: String,
        choice: String,
        rationale: String,
    },
    ResetLoopSignals,
    RouteTaskState {
        task_id: u64,
        state: String,
    },
    /// The legacy `task_ledger` row failed to decode: reconstruct it from
    /// the durable typed authorities instead of ever defaulting.
    RebuildTaskLedger {
        reason: String,
    },
}

/// Verdict of a durable-write marker's `session` field (the marker's ONLY
/// authority to be applied). Strict on purpose: the previous `is_some_and`
/// check treated an ABSENT or non-numeric field as "mine", so a corrupt or
/// tampered marker could apply a cross-session Abort/RouteTaskState/
/// FinishTurnRecord. Only [`MarkerSession::Ours`] may be replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerSession {
    /// The marker names exactly this session and may be replayed.
    Ours,
    /// A well-formed id of a DIFFERENT session: retained for that session's
    /// open (every open scans the whole directory), never applied here.
    Foreign(u64),
    /// Absent, non-numeric, out of range, or zero: corrupt/tampered.
    /// Retained loudly; never applied anywhere.
    Malformed,
}

/// Classify one marker's session identity against the opening session. Ids
/// are non-zero by construction, so `0` is malformed, not a session.
pub(crate) fn marker_session_verdict(marker: &serde_json::Value, session: u64) -> MarkerSession {
    match marker.get("session").and_then(|value| value.as_u64()) {
        Some(0) | None => MarkerSession::Malformed,
        Some(other) if other == session => MarkerSession::Ours,
        Some(other) => MarkerSession::Foreign(other),
    }
}

/// Weak-but-honest summarizer: emits the ledger render. The compactor's hard
/// invariant rejects it when it does not shrink enough.
pub(crate) struct LedgerSummarizer;

/// The REAL separate-compaction-model summarizer (spec §9 + §36): the
/// configured compaction model streams an actual provider request that
/// summarizes the recent history. The request carries its own compactor
/// contract as the system prompt (P0 audit round 11: the agent instructions
/// used to leak in and the model answered the latest user message instead
/// of summarizing). Any failure yields NO summary — run() discards all
/// partial text and the caller returns a transcript the compactor's hard
/// cap rejects, so deterministic pruning takes over (compaction can never
/// hang, outlive the turn, or degrade on a broken compaction model).
/// (attempt-accounting audit) One budgeted compaction summarizer bundle:
/// The summarizer, its reservation, the SHARED attempt machine whose
/// guarded state decides the terminal money move after the opaque
/// `Summarizer` run, and the compaction call's attempt-keyed row identity
/// (attempt accounting, schema v18): the physical attempt, its durable
/// reservation link and the summarizer's provider/model. The identity is
/// minted with the reservation (same place) so the attempt-keyed
/// provider-call rows of a compaction call join its reservation exactly.
pub(crate) type BudgetedSummarizer = (
    Option<Arc<dyn Summarizer>>,
    Option<faktor_session::ReservationId>,
    Option<Arc<tokio::sync::Mutex<crate::AttemptAccounting>>>,
    Option<CompactCallTrace>,
);

impl AgentRuntime {
    /// Install (or clear) the commercial provider-attempt debit authority
    /// (Wave 5 residual). Additive: the host wires the cloud billing service
    /// here AFTER construction, so every existing constructor site stays
    /// byte-identical. With `None` the runtime never consults any debit
    /// authority — the pre-billing behavior exactly.
    ///
    /// Fallible (P0 monetary fail-closed): a POISONED authority slot is
    /// surfaced as [`crate::credits::DebitError::Unavailable`] — installing
    /// (or clearing) the billing authority can never silently no-op.
    pub fn set_provider_debits(
        &self,
        debits: Option<Arc<dyn crate::credits::ProviderAttemptDebits>>,
    ) -> Result<(), crate::credits::DebitError> {
        let mut slot =
            self.provider_debits
                .lock()
                .map_err(|_| crate::credits::DebitError::Unavailable {
                    reason: "provider debit authority lock poisoned".into(),
                })?;
        *slot = debits;
        Ok(())
    }

    /// The installed debit authority, when billing is enabled. Fallible
    /// (P0 monetary fail-closed): a POISONED lock is an UNAVAILABLE
    /// authority, never a silent `None` — `None` means "billing deliberately
    /// disabled" (BYOK/no-authority), and conflating the two would let a
    /// managed-provider dispatch proceed without its commercial debit
    /// authority after a poisoning.
    pub(crate) fn provider_debits(
        &self,
    ) -> Result<Option<Arc<dyn crate::credits::ProviderAttemptDebits>>, crate::credits::DebitError>
    {
        self.provider_debits
            .lock()
            .map(|slot| slot.clone())
            .map_err(|_| crate::credits::DebitError::Unavailable {
                reason: "provider debit authority lock poisoned".into(),
            })
    }

    /// Build the per-attempt commercial debit machine for one physical
    /// provider dispatch (Wave 5 residual). The machine is a strict no-op
    /// without an installed authority (billing disabled) and classifies
    /// managed vs BYOK from the AUTHORITY's configured managed-provider set —
    /// never from a provider-name comparison in the agent. The identity is
    /// the attempt's fresh op id, so a retry opens its own hold and a replay
    /// of one attempt can never be double-debited.
    pub(crate) fn attempt_debits(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        provider: &str,
        model: &str,
        attempt: faktor_core::op::ModelCallAttempt,
        estimate_micro: u64,
    ) -> Result<crate::credits::AttemptDebits, crate::credits::DebitError> {
        let debit = crate::credits::ProviderAttemptDebit {
            attempt_id: attempt.attempt_op_id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            session_id: session_id.raw(),
            task_id: Some(task_id.raw()),
            estimate_micro,
            reason: "agent_provider_attempt".to_string(),
        };
        crate::credits::AttemptDebits::new(self.provider_debits()?, debit)
    }

    /// Pre-seed (or patch) the durable wave-9 Task budget caps of one
    /// session BEFORE its drive begins (audits 20-24 wiring: a child's
    /// budget from its WorkItem is enforced by passing it into the child
    /// drive; the typed Task row is the durable enforcement point, gated at
    /// every genuine end). `max_tokens`/`max_turns` are set to the given
    /// caps; spend counters are only ever healed forward, never rewinded.
    pub fn seed_task_budget(
        &self,
        session: SessionId,
        caps: &faktor_session::TaskBudget,
    ) -> faktor_core::Result<()> {
        let handle = self
            .deps
            .session
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("session {session}")))?;
        let task_id = handle.task_id()?;
        let now = handle.now_ms();
        let mut seed = match handle.get_task(task_id)? {
            Some(t) => t,
            None => {
                let goal = truncate(&handle.title()?, 200);
                handle.create_task(faktor_session::Task {
                    task_id,
                    session_id: session,
                    goal,
                    acceptance_criteria: Vec::new(),
                    plan: Vec::new(),
                    attachments: Vec::new(),
                    budget: faktor_session::TaskBudget {
                        max_tokens: caps.max_tokens,
                        max_turns: caps.max_turns,
                        spent_tokens: 0,
                        spent_turns: 0,
                    },
                    state: faktor_core::state::TaskState::Pending,
                    created_ms: now,
                    updated_ms: now,
                })?
            }
        };
        seed.budget.max_tokens = caps.max_tokens.or(seed.budget.max_tokens);
        seed.budget.max_turns = caps.max_turns.or(seed.budget.max_turns);
        // Audit P0-7: a TERMINAL row (VerifiedComplete/Failed/Cancelled) is
        // frozen — update_task refuses with TerminalTask. The caps a caller
        // seeds after the task already ended are a no-op, not an error: the
        // row certified its lifetime once.
        match handle.update_task(
            task_id,
            faktor_session::TaskPatch {
                budget: Some(faktor_session::TaskBudget {
                    max_tokens: seed.budget.max_tokens,
                    max_turns: seed.budget.max_turns,
                    spent_tokens: seed.budget.spent_tokens,
                    spent_turns: seed.budget.spent_turns,
                }),
                ..Default::default()
            },
        ) {
            Ok(_) => Ok(()),
            Err(faktor_session::TaskError::TerminalTask { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Durable loop signals (spec §28): a FAILING tool call bumps the
    /// session's persistent count for that exact call; any genuine progress
    /// (a successful tool or a completed test) clears the window. Returns
    /// true when the same failing call repeated across turns/restarts
    /// reaches the threshold — the runtime must stop and re-plan, not let
    /// the model grind for 40 turns.
    pub(crate) fn durable_loop_signals(
        &self,
        handle: &faktor_session::SessionHandle,
        turn_summary: &faktor_context::ledger::TurnSummary,
        detector: &LoopDetector,
    ) -> faktor_core::Result<bool> {
        let mut tripped = false;
        #[cfg(debug_assertions)]
        eprintln!(
            "dbg-loop: progress={} failures={:?}",
            turn_made_progress(turn_summary),
            turn_summary.failures
        );
        if turn_made_progress(turn_summary) {
            // Some calls succeeded this batch: the task is making progress;
            // do not punish isolated failures.
            return Ok(false);
        }
        for f in &turn_summary.failures {
            let key = format!("fail {}", truncate(f, 400));
            if handle.bump_loop_signal(&key, detector.threshold() as u32)? {
                tripped = true;
            }
        }
        Ok(tripped)
    }

    /// Settle the task's OPEN durable verification jobs (audit P0-5/26) —
    /// the text-turn settlement pass, invoked only when jobs exist and the
    /// turn changed nothing:
    ///
    /// 1. honest recovery: rows a vanished/interrupted executor left
    ///    `Running` are re-queued (typed note) and orphaned rows are
    ///    resolved Unavailable — never silently terminal;
    /// 2. every open job of the NEWEST attempt is claimed (CAS
    ///    Queued→Running), executed exactly once through the service (the
    ///    supervisor-backed executor with the job's own budget deadline and
    ///    a child cancellation), and resolved (CAS Running→terminal);
    /// 3. when every required job of the attempt is terminal, the existing
    ///    completion path proceeds from the job results: the criteria rows,
    ///    mirrors, gate, durable facts and per-attempt record are rebuilt
    ///    from the attempt record + job rows (inline outcomes of the
    ///    enqueueing turn ride the attempt record), and the gate lands
    ///    through the normal end-of-turn tail.
    ///
    /// A turn that ends mid-settlement (cancellation) returns a pending
    /// verdict: open jobs stay open and the task stays Verifying.
    /// The daemon verification executor's primitive (audit P0-5/26
    /// production wiring): claim -> execute -> resolve every OPEN job of the
    /// session's CURRENT verification attempt. It never derives checks and
    /// never settles the task — a resolved job is consumed by the next
    /// settlement of the exact attempt. Returns the number of jobs resolved
    /// (0 when nothing was open). A job claimed by a concurrent executor is
    /// skipped (the guarded CAS decides exactly one winner).
    pub async fn execute_open_verification_jobs(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Result<usize, String> {
        let row = handle
            .row()
            .map_err(|e| format!("session row unresolvable: {e}"))?;
        let root = self
            .deps
            .session
            .resolve_workspace_root(handle.id())
            .ok()
            .flatten()
            .ok_or_else(|| "session workspace root unresolvable".to_string())?;
        self.deps
            .workspaces
            .open(row.workspace_id, root.clone())
            .map_err(|e| format!("workspace could not be opened: {e}"))?;
        let attempt = handle
            .current_verification_attempt(row.task_id.raw())
            .map_err(|e| format!("verification attempt read: {e}"))?
            .ok_or_else(|| "no current verification attempt".to_string())?;
        let base_ctx = faktor_verify::exec::VerificationContext {
            session_id: handle.id().raw(),
            task_id: row.task_id.raw(),
            operation_id: attempt.op_id,
            workspace_id: row.workspace_id.raw(),
            worktree_id: row.worktree_id.raw(),
            root,
            deadline: std::time::Instant::now(),
            cancellation: CancellationToken::new().child(),
        };
        Ok(self.execute_attempt_jobs(handle, &attempt, &base_ctx).await)
    }

    /// The shared claim -> execute -> resolve loop of one attempt's open
    /// jobs. Never fails the caller: a concurrent claim loss skips the job
    /// (another executor owns it), a corrupt spec row resolves Unavailable
    /// (never left open, never a silent pass), and a resolve that loses the
    /// race is ignored (the store's CAS keeps exactly one terminal write).
    pub(crate) async fn execute_attempt_jobs(
        &self,
        handle: &faktor_session::SessionHandle,
        attempt: &faktor_session::VerificationAttempt,
        base_ctx: &faktor_verify::exec::VerificationContext,
    ) -> usize {
        let task_raw = attempt.task_id;
        let service = self.deps.verification.clone();
        let rows = match handle.verification_attempt_jobs(task_raw, attempt.op_id) {
            Ok(rows) => rows,
            Err(_) => return 0,
        };
        let mut resolved = 0usize;
        for job in rows.iter().filter(|j| j.state.is_open()) {
            // The claim marker's op id is durable-only metadata: an
            // allocation failure skips the job loudly (it stays open and is
            // retried later) instead of fabricating a zero id.
            let claim_op = match self.deps.session.try_next_op_id() {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "op-id allocation failed; verification job {} left unclaimed",
                        job.check_id
                    );
                    continue;
                }
            };
            let claimed = handle.claim_verification_job(
                task_raw,
                &job.check_id,
                attempt.op_id,
                claim_op.raw(),
            );
            let job_row = match claimed {
                Ok(j) => j,
                Err(_) => continue, // superseded/resolved concurrently — settled elsewhere
            };
            resolved += 1;
            let spec: Result<faktor_verify::exec::CheckSpec, _> =
                serde_json::from_str(&job_row.spec_json);
            let spec = match spec {
                Ok(s) => s,
                Err(e) => {
                    // Infallible executor loop: a corrupt row must still
                    // resolve Unavailable, so a lost resolve is recorded
                    // (marker + audit) and replayed, never dropped.
                    self.dw_note_resolve_verification_job(
                        handle,
                        task_raw,
                        &job_row.check_id,
                        attempt.op_id,
                        faktor_session::VerificationJobState::Unavailable,
                        Some(format!("job spec undecodable (corrupt row): {e}")),
                        None,
                        DW_SITE_JOB_RESOLVE_CORRUPT,
                    );
                    continue;
                }
            };
            let budget = Duration::from_millis(job_row.budget_ms.max(1));
            let mut vctx = base_ctx.clone();
            vctx.deadline = std::time::Instant::now() + budget;
            let outcome = service.execute(&spec, &vctx).await;
            let state = match outcome.status {
                CheckRunStatus::Passed => faktor_session::VerificationJobState::Passed,
                CheckRunStatus::Failed => faktor_session::VerificationJobState::Failed,
                CheckRunStatus::Unavailable => faktor_session::VerificationJobState::Unavailable,
            };
            let result_json = serde_json::to_string(&outcome).ok();
            self.dw_note_resolve_verification_job(
                handle,
                task_raw,
                &job_row.check_id,
                attempt.op_id,
                state,
                None,
                result_json,
                DW_SITE_JOB_RESOLVE,
            );
        }
        resolved
    }

    pub(crate) async fn settle_verification_jobs(
        &self,
        handle: &faktor_session::SessionHandle,
        cancel: &CancellationToken,
    ) -> TurnEndVerdict {
        let row = match handle.row() {
            Ok(row) => row,
            Err(e) => {
                return self.verification_pending_verdict(
                    handle,
                    &[],
                    &format!("session row unresolvable: {e}"),
                )
            }
        };
        let root = match self.deps.session.resolve_workspace_root(handle.id()) {
            Ok(Some(r)) => r,
            _ => {
                return self.verification_pending_verdict(
                    handle,
                    &[],
                    "session workspace root unresolvable; background jobs stay open",
                )
            }
        };

        let ws = match self.deps.workspaces.open(row.workspace_id, root.clone()) {
            Ok(w) => w,
            Err(_) => {
                return self.verification_pending_verdict(
                    handle,
                    &[],
                    "workspace could not be opened; background jobs stay open",
                )
            }
        };
        let task_id = row.task_id;
        let task_raw = task_id.raw();
        // Honest restart recovery happens ONCE at daemon startup (the CLI's
        // recovery sweep), never per settlement: with the daemon-level
        // verification executor a `Running` row here can be a LIVE claim
        // from that executor, and re-queueing it would clobber a valid
        // execution. The settlement below only ever claims `Queued` rows
        // through the guarded CAS, so it can never steal a live job.
        let attempt = match handle.current_verification_attempt(task_raw) {
            Ok(Some(a)) => a,
            Ok(None) => {
                return self.verification_pending_verdict(
                    handle,
                    &[],
                    "open verification jobs have no attempt record (crash residue); \
                     recovery resolved them; nothing to settle",
                )
            }
            Err(e) => {
                return self.verification_pending_verdict(
                    handle,
                    &[],
                    &format!("verification attempt read failed: {e}"),
                )
            }
        };
        let base_ctx = faktor_verify::exec::VerificationContext {
            session_id: handle.id().raw(),
            task_id: task_raw,
            operation_id: attempt.op_id,
            workspace_id: row.workspace_id.raw(),
            worktree_id: row.worktree_id.raw(),
            root: root.clone(),
            deadline: std::time::Instant::now(),
            cancellation: cancel.child(),
        };
        // Execution loop (the ONE executor primitive, shared with the
        // daemon verification executor): every open job of the attempt,
        // claimed exactly once and resolved exactly once.
        self.execute_attempt_jobs(handle, &attempt, &base_ctx).await;
        let rows = match handle.verification_attempt_jobs(task_raw, attempt.op_id) {
            Ok(r) => r,
            Err(e) => {
                return self.verification_pending_verdict(
                    handle,
                    &[],
                    &format!("job rows unreadable after settlement: {e}"),
                )
            }
        };
        if rows.iter().any(|j| j.state.is_open()) {
            // The settlement pass ended with jobs still open (cancellation):
            // the attempt is not settleable — the task stays Verifying and
            // the next genuine end retries. Nothing was lost.
            return self.verification_pending_verdict(
                handle,
                &[],
                "background verification jobs still open; a later genuine end settles them",
            );
        }
        // ---- every required job of the attempt is terminal ----
        // Rebuild the attempt's mirrors/results from durable rows: the
        // ordered attempt record (inline outcomes of the enqueueing turn)
        // + the job rows (background outcomes). The gate below then flows
        // through the EXACT tail of a normal attempt.
        let goal = match self.load_ledger(handle) {
            Ok(ledger) => ledger.goal.clone(),
            Err(e) => {
                // Infallible verdict path: the corrupt ledger already
                // recorded its rebuild marker in `load_ledger`; derive the
                // criteria from the durable task row instead of a silent
                // empty default.
                tracing::error!(
                    session = %handle.id(),
                    "task ledger unreadable while rebuilding verification results; deriving criteria from the durable task row: {e}"
                );
                self.session_task(handle)
                    .map(|task| task.goal)
                    .unwrap_or_default()
            }
        };
        let AttemptRebuild {
            mirrors,
            results,
            unavailable,
            executed,
        } = rebuild_attempt_verification(&attempt, &rows);
        let changed: Vec<String> = attempt.changed.clone();
        for check in mirrors.iter().filter(|c| c.required) {
            if unavailable.iter().any(|(id, _)| id == &check.id) {
                // Infallible verdict function: recorded (marker + audit), not
                // propagated.
                self.dw_note_upsert_memory_fact(
                    handle,
                    "verification",
                    &check.id,
                    &format!("unavailable:{}", check.command),
                    DW_SITE_SETTLE_UNAVAILABLE_FACT,
                );
            }
        }
        let criteria = criteria_rows(&goal, &mirrors);
        let acceptance = faktor_verify::acceptance(&mirrors, &results);
        if acceptance == faktor_verify::Acceptance::Fail {
            for check in mirrors.iter().filter(|c| c.required) {
                if results.iter().any(|(id, ok)| id == &check.id && !ok) {
                    // Infallible verdict function: recorded (marker + audit),
                    // not propagated.
                    self.dw_note_upsert_memory_fact(
                        handle,
                        "verification",
                        &check.id,
                        &format!("failed:{}", check.command),
                        DW_SITE_SETTLE_FAILED_FACT,
                    );
                }
            }
        }
        let failed: Vec<OutcomeReason> = mirrors
            .iter()
            .filter(|c| c.required)
            .filter(|c| results.iter().any(|(id, ok)| id == &c.id && !ok))
            .map(|c| {
                OutcomeReason::new(
                    ReasonCode::CheckFailed,
                    format!("required check '{}' ({}) failed", c.id, c.command),
                )
            })
            .collect();
        let completion = if !failed.is_empty() {
            CompletionGate::FailedVerification { reasons: failed }
        } else {
            let reasons: Vec<OutcomeReason> = unavailable
                .iter()
                .map(|(id, _)| {
                    OutcomeReason::new(
                        ReasonCode::CheckUnavailable,
                        format!("required check '{id}' unavailable"),
                    )
                })
                .collect();
            if reasons.is_empty() {
                CompletionGate::VerifiedComplete
            } else {
                CompletionGate::BlockedVerification { reasons }
            }
        };
        let status = match acceptance {
            faktor_verify::Acceptance::Fail => VerificationStatus::Failed,
            faktor_verify::Acceptance::Pass => VerificationStatus::Passed,
            faktor_verify::Acceptance::Pending => VerificationStatus::Pending,
        };
        let proof = if executed.is_empty() {
            None
        } else {
            Some(
                verification_proof_from_attempt(
                    criteria.as_deref(),
                    &mirrors,
                    &results,
                    &unavailable,
                    &executed,
                    &changed,
                    &ws,
                    None,
                )
                .await,
            )
        };
        self.persist_gate_facts(
            handle,
            &completion,
            status,
            &results,
            &changed,
            criteria.as_deref(),
        );
        TurnEndVerdict {
            verification: results,
            acceptance: Some(acceptance),
            review: None,
            completion: Some(completion),
            criteria,
            proof,
        }
    }

    pub(crate) fn load_ledger(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> faktor_core::Result<TaskLedger> {
        match handle.get_task_ledger()? {
            Some(v) => match serde_json::from_value(v) {
                Ok(ledger) => Ok(ledger),
                Err(e) => {
                    // A corrupt durable row is NEVER silently defaulted: the
                    // decode failure surfaces typed, and the retry-on-next-open
                    // marker makes recovery rebuild the row from the durable
                    // authorities (typed ledger head + task row + title).
                    let err = Error::new(
                        ErrorKind::Malformed,
                        format!("durable task ledger row undecodable: {e}"),
                    );
                    tracing::error!(
                        session = %handle.id(),
                        site = DW_SITE_LEDGER_REBUILD,
                        kind = ?err.kind,
                        "durable task-ledger row undecodable; recording a retry-on-next-open rebuild marker: {e}"
                    );
                    let intent = DurableWriteIntent::RebuildTaskLedger {
                        reason: truncate(&e.to_string(), 1024),
                    };
                    self.record_durable_write_failure(
                        handle,
                        DW_SITE_LEDGER_REBUILD,
                        &intent,
                        &err,
                    );
                    Err(err)
                }
            },
            None => Ok(TaskLedger::default()),
        }
    }

    /// Bounded reconstruction of the legacy `task_ledger` working blob from
    /// the durable authorities that own each fact: the typed session ledger
    /// head (goal/plan/decisions/last verification) first, then the typed
    /// task row and the session title. Used ONLY when the legacy row is
    /// proven undecodable; every fact stays durably owned by its typed row,
    /// so a source that cannot be read stays empty (never fabricated) and
    /// the rebuild is a projection of durable rows, not a new authority.
    pub(crate) fn rebuild_task_ledger_from_durable(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> faktor_core::Result<TaskLedger> {
        let mut ledger = TaskLedger::default();
        match handle.ledger_view() {
            Ok(view) => {
                ledger.goal = truncate(&view.head.goal, 4096);
                ledger.open_steps = view
                    .head
                    .plan_steps
                    .iter()
                    .take(64)
                    .map(|step| truncate(&step.text, 512))
                    .collect();
                ledger.decisions = view
                    .head
                    .decisions
                    .iter()
                    .take(128)
                    .map(|decision| {
                        if decision.step.is_empty() {
                            truncate(&decision.choice, 512)
                        } else {
                            truncate(
                                &format!(
                                    "{}: {} ({})",
                                    decision.step, decision.choice, decision.rationale
                                ),
                                512,
                            )
                        }
                    })
                    .collect();
                if let Some(verify) = &view.head.last_verify {
                    for check in verify.checks.iter().take(256) {
                        ledger.tests_run.push(truncate(&check.id, 128));
                        if !check.passed {
                            ledger.tests_failed.push(truncate(&check.id, 128));
                        }
                    }
                }
            }
            Err(e) => {
                // The typed head is the primary authority; when it cannot be
                // read the rebuild falls back to the task row/title and says
                // so loudly (never a silent empty projection).
                tracing::warn!(
                    session = %handle.id(),
                    "typed ledger head unreadable during legacy-ledger rebuild; falling back to the task row/title: {e}"
                );
            }
        }
        if let Some(task) = self.session_task(handle) {
            if ledger.goal.is_empty() {
                ledger.goal = truncate(&task.goal, 4096);
            }
            if ledger.open_steps.is_empty() {
                ledger.open_steps = task
                    .plan
                    .iter()
                    .take(64)
                    .map(|step| truncate(step, 512))
                    .collect();
            }
        }
        if ledger.goal.is_empty() {
            ledger.goal = truncate(&handle.title()?, 200);
        }
        Ok(ledger)
    }

    // -------------------------------------------------------- durable Task
    // (audit 25: the first-class persisted Task object; see
    // crates/session/src/task.rs for the bounded API surface)

    /// The session's CURRENT durable task row: the row whose `task_id`
    /// equals the session's adopted task identity when present, otherwise
    /// the oldest row (a session that adopted no worktree identity keeps
    /// task id 1 and one row).
    pub(crate) fn session_task(&self, handle: &faktor_session::SessionHandle) -> Option<Task> {
        let mut tasks = handle.list_tasks().unwrap_or_default();
        if tasks.is_empty() {
            return None;
        }
        let preferred = handle.task_id().ok();
        if let Some(pos) = tasks.iter().position(|t| Some(t.task_id) == preferred) {
            return Some(tasks.remove(pos));
        }
        Some(tasks.remove(0))
    }

    /// Drive-start Task integration (audit 25): restore the typed row into
    /// the runtime's memory-fact set and heal the row's spend. Runs on every
    /// drive (fresh or crash-restarted), idempotently — an existing fact is
    /// only rewritten when its value differs from the row, so facts are
    /// never duplicated. A missing row is created from the goal.
    pub(crate) fn restore_task_rows(
        &self,
        handle: &faktor_session::SessionHandle,
        ledger: &TaskLedger,
    ) -> faktor_core::Result<()> {
        let task = match self.session_task(handle) {
            Some(t) => t,
            None => {
                // First sighting: create from the durable goal so every
                // gate has a row. The budget stays unlimited until a caller
                // (or the session's durable row from a previous run) sets
                // caps — an existing row is NEVER re-created here.
                let task_id = handle.task_id()?;
                let now = handle.now_ms();
                return handle
                    .create_task(Task {
                        task_id,
                        session_id: handle.id(),
                        goal: ledger.goal.clone(),
                        acceptance_criteria: Vec::new(),
                        plan: Vec::new(),
                        attachments: Vec::new(),
                        budget: Default::default(),
                        state: faktor_core::state::TaskState::Pending,
                        created_ms: now,
                        updated_ms: now,
                    })
                    .map(|_| ());
            }
        };
        // Heal spend from durable sources (a crash between a provider call
        // and the last gate sync cannot lose spend) and reflect the row's
        // state in the memory facts — only when absent or stale.
        let patch = TaskPatch {
            budget: Some(faktor_session::TaskBudget {
                max_tokens: task.budget.max_tokens,
                max_turns: task.budget.max_turns,
                spent_tokens: handle.spent_tokens().unwrap_or(task.budget.spent_tokens),
                spent_turns: handle
                    .spent_turns()
                    .unwrap_or(task.budget.spent_turns as u64)
                    .min(u32::MAX as u64) as u32,
            }),
            ..Default::default()
        };
        let healed = handle
            .update_task(task.task_id, patch)
            .unwrap_or(task.clone());
        let state_str = serde_json::to_value(healed.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "pending".into());
        let facts = handle.memory_facts().unwrap_or_default();
        let goal_fact = facts
            .iter()
            .find(|(k, key, _)| k == "task" && key == "goal")
            .map(|(_, _, v)| v.clone());
        if !healed.goal.is_empty()
            && goal_fact.as_deref() != Some(truncate(&healed.goal, 200).as_str())
        {
            // Drive-start fact mirror: the caller can fail safely (a later
            // drive re-heals), so the write error propagates typed.
            self.guarded_upsert_memory_fact(
                handle,
                "task",
                "goal",
                &truncate(&healed.goal, 200),
                DW_SITE_RESTORE_GOAL_FACT,
            )?;
        }
        let state_fact = facts
            .iter()
            .find(|(k, key, _)| k == "task_state" && key == "state")
            .map(|(_, _, v)| v.clone());
        if state_fact.as_deref() != Some(&state_str) {
            self.guarded_upsert_memory_fact(
                handle,
                "task_state",
                "state",
                &state_str,
                DW_SITE_RESTORE_STATE_FACT,
            )?;
        }
        if !healed.acceptance_criteria.is_empty() {
            let canonical = criteria_canonical_text(&healed.acceptance_criteria);
            let criteria_fact = facts
                .iter()
                .find(|(k, key, _)| k == "criteria" && key == "0")
                .map(|(_, _, v)| v.clone());
            if criteria_fact.as_deref() != Some(canonical.as_str()) {
                self.guarded_upsert_memory_fact(
                    handle,
                    "criteria",
                    "0",
                    &canonical,
                    DW_SITE_RESTORE_CRITERIA_FACT,
                )?;
            }
        }
        Ok(())
    }

    /// Genuine-end Task CONTENT sync (audit 25 + P0-7): fold the ledger
    /// goal, the derived acceptance criteria, the append-only plan steps and
    /// the durable spend into the typed row, with spend counted from durable
    /// sources AFTER the TurnCompleted event (so this turn is included). NO
    /// state write happens here: the row's state is driven once, by
    /// [`AgentRuntime::apply_gate_to_task_row`], after every durable gate
    /// refusal (budget / strict criteria) is decided, so the row never
    /// flip-flops through a gate that is later refused. A TERMINAL row
    /// (VerifiedComplete/Failed/Cancelled) is frozen by the machine: its
    /// content is never rewritten — the row certified completion once and
    /// the ledger keeps the later history. Returns the row as persisted —
    /// the caller gates on its budget.
    pub(crate) fn sync_task_row(
        &self,
        handle: &faktor_session::SessionHandle,
        ledger: &TaskLedger,
        criteria: Option<&[String]>,
    ) -> faktor_core::Result<Option<Task>> {
        let mut task = match self.session_task(handle) {
            Some(t) => t,
            None => {
                let task_id = handle.task_id()?;
                let now = handle.now_ms();
                handle.create_task(Task {
                    task_id,
                    session_id: handle.id(),
                    goal: ledger.goal.clone(),
                    acceptance_criteria: Vec::new(),
                    plan: Vec::new(),
                    attachments: Vec::new(),
                    budget: Default::default(),
                    state: faktor_core::state::TaskState::Pending,
                    created_ms: now,
                    updated_ms: now,
                })?
            }
        };
        // Terminal rows are frozen: no content write (update_task refuses
        // TerminalTask) — the row stays byte-identical after completion.
        if task.state.is_terminal() {
            return Ok(Some(task));
        }
        // Goal mirrors the ledger (bounded to 200 chars upstream).
        if !ledger.goal.is_empty() && task.goal != ledger.goal {
            task.goal = ledger.goal.clone();
        }
        // Criteria (audits 56/57/105): the derivation is persisted as typed
        // V2 JSON in the EXISTING criteria row values (no store schema
        // change). Re-derivation merges under the audit rules: user criteria
        // survive verbatim, derived criteria are authoritative for their
        // origin, and a moved source snapshot re-derives the stale derived
        // criterion. The row write is skipped when the merged set is
        // byte-identical (no spurious revision bump). The `criteria`/`0` fact
        // stays once-only: a divergence it exposes is refused by the strict
        // gate and healed FACT-from-ROW at the next drive start, exactly as
        // before.
        if let Some(criteria) = criteria {
            if !criteria.is_empty() {
                let derived = decode_criteria(criteria);
                let merged = merge_derived_criteria(&task.criteria(), &derived);
                let encoded = encode_criteria(&merged);
                if task.acceptance_criteria != encoded {
                    task.acceptance_criteria = encoded;
                }
            }
        }
        // Plan: append-only ordered steps. Steps are discovered from the
        // ledger (open then completed, first-seen order); entries already in
        // the plan are never re-appended and the row's plan cap (256) is
        // honored by stopping, never by evicting or truncating a step.
        // Each entry is bounded to the row's step bound before the write
        // (the session layer REJECTS oversized patches — never silently
        // truncate is for API input; ledger-sourced steps are truncated to
        // the durable bound because the ledger itself caps at 4096).
        for step in ledger
            .open_steps
            .iter()
            .chain(ledger.completed_steps.iter())
        {
            if task.plan.len() >= faktor_session::MAX_TASK_PLAN_STEPS {
                break;
            }
            let step = truncate(step, faktor_session::MAX_TASK_STEP_BYTES);
            if !task.plan.iter().any(|p| p == &step) {
                let step_index = task.plan.len() as u32;
                let parent_index = if step_index == 0 {
                    None
                } else {
                    Some(step_index - 1)
                };
                // Typed ledger mirror (audit 27): each step appended to the
                // durable plan is ALSO a PlanStepAdded entry with its
                // parent_index (the prior step it extends — the plan is
                // linear, so the parent is the previous index). Mirrored
                // BEFORE the row write so a crash between the two leaves the
                // mirror as the durable record of the step.
                handle.ledger_plan_step_added(step_index, &step, parent_index)?;
                // P0-79 site b: the durable plan GREW — semantic progress
                // evidence for the expensive-cycle decisions.
                self.progress_evidence(handle.id(), ProgressEvidence::PlanStepCompleted);
                task.plan.push(step);
            }
        }
        // Spend comes from durable sources only (provider_call rows +
        // turn_completed events) so crashes never lose or double count.
        let spent_tokens = handle.spent_tokens().unwrap_or(task.budget.spent_tokens);
        let spent_turns = handle
            .spent_turns()
            .unwrap_or(task.budget.spent_turns as u64)
            .min(u32::MAX as u64) as u32;
        // Content patch WITHOUT a state field: the machine (audit P0-7)
        // rejects any patch carrying a completion-relevant state, and the
        // row's state is driven separately by apply_gate_to_task_row.
        let mut patch = TaskPatch {
            goal: Some(task.goal.clone()),
            acceptance_criteria: Some(task.acceptance_criteria.clone()),
            plan: Some(task.plan.clone()),
            attachments: Some(task.attachments.clone()),
            budget: Some(faktor_session::TaskBudget {
                max_tokens: task.budget.max_tokens,
                max_turns: task.budget.max_turns,
                spent_tokens,
                spent_turns,
            }),
            state: None,
        };
        if patch.goal.as_deref() == Some("") {
            patch.goal = None;
        }
        Ok(Some(handle.update_task(task.task_id, patch)?))
    }

    /// The FINAL gate's state-machine + proof write (audits P0-7/P0-8): the
    /// single site that moves the typed task row's STATE at a genuine end,
    /// called once per turn after every durable refusal is decided. Returns
    /// `Ok(Some(downgraded))` when the gate could not be applied as requested
    /// (a typed completion refusal — the caller rewrites the durable fact to
    /// the refused gate); `Ok(None)` when the requested gate landed.
    ///
    /// Mapping table — the runtime's earlier per-gate state PATCHES to the
    /// legal machine writes (the durable `task_state` FACT keeps recording
    /// the gate via [`CompletionGate::task_state`]; only the typed row
    /// changes encoding):
    /// ```text
    /// gate                        | old row patch   | machine write (P0-7/P0-8)
    /// ----------------------------|-----------------|---------------------------------------------
    /// VerifiedComplete            | =VerifiedComplete| Running->NV->Verifying; durable Passed
    ///                            |                  | record (record-first, one per attempt);
    ///                            |                  | complete_verified_task = the ONLY
    ///                            |                  | VerifiedComplete producer; row already
    ///                            |                  | VerifiedComplete: per-attempt record only
    ///                            |                  | (terminal rows are frozen)
    /// Unverified                  | =NeedsVerification| route to NeedsVerification (no claim ran:
    ///                            |                  | no Verifying transit, no record)
    /// VerificationPending         | =Verifying       | route to Verifying where the machine
    ///                            |                  | allows (the attempt's background jobs are
    ///                            |                  | open; a later genuine end settles them and
    ///                            |                  | drives the real gate; no record — nothing
    ///                            |                  | certified yet)
    /// FailedVerification          | =Failed         | route to Verifying (the attempt ran), land a
    ///                            |                  | Failed record, then Verifying->NeedsVerification
    ///                            |                  | (retryable: a later fixed turn re-verifies;
    ///                            |                  | the terminal Failed edge is NOT driven —
    ///                            |                  | the old row patch value Failed is unreachable
    ///                            |                  | for a row that must re-verify)
    /// BlockedVerification         | =Blocked        | route to Blocked where the machine allows
    ///                            |                  | (Running/Waiting/Pending); rows already at
    ///                            |                  | NeedsVerification/Verifying have NO edge to
    ///                            |                  | Blocked and keep NeedsVerification; no record
    /// None (no completion claim)  | (content only)  | content-only sync (sync_task_row)
    /// ```
    /// Terminal rows never move: a VerifiedComplete row stays complete (the
    /// per-attempt record is still landed as evidence), a Failed/Cancelled
    /// row refuses every gate (a typed-refusal downgrade is returned for a
    /// VerifiedComplete request — the runtime never force-writes completion).
    pub(crate) fn apply_gate_to_task_row(
        &self,
        handle: &faktor_session::SessionHandle,
        gate: Option<CompletionGate>,
        proof: Option<&VerificationProof>,
    ) -> faktor_core::Result<Option<CompletionGate>> {
        let Some(gate) = gate else {
            return Ok(None); // no completion claim this turn: nothing to drive
        };
        let Some(task) = self.session_task(handle) else {
            return Ok(None); // no row (content sync created none): nothing to drive
        };
        let task_id = task.task_id;
        let now = handle.now_ms();
        match gate {
            CompletionGate::VerifiedComplete => {
                if task.state == TaskState::VerifiedComplete {
                    // Already certified by an earlier attempt's record: the
                    // terminal row is frozen. Land THIS attempt's record as
                    // durable evidence only — complete_verified_task refuses
                    // a non-Verifying row, and re-certification is pointless.
                    if let Some(proof) = proof {
                        let _record = self.create_attempt_record(
                            handle,
                            task_id,
                            VerificationStatus::Passed,
                            proof,
                            now,
                        )?;
                    }
                    return Ok(None);
                }
                if task.state.is_terminal() {
                    // Failed/Cancelled rows cannot be certified: typed refusal
                    // downgrade, never a force-write.
                    return Ok(Some(completion_refusal_gate(&TaskError::NotVerifying {
                        actual: task.state,
                    })));
                }
                // P2 completion-contract gate: a run that declared
                // commit/push/PR steps may not certify until every requested
                // step has a durable Succeeded step-status row. This check
                // runs BEFORE the row is routed to Verifying and before any
                // attempt record exists, so an unmet contract refuses with
                // the step named and the task row never moves.
                if let CompletionContractGate::Refused(err) =
                    handle.completion_contract_gate(task_id)?
                {
                    return Ok(Some(completion_refusal_gate(&err)));
                }
                let Some(proof) = proof else {
                    return Ok(Some(completion_refusal_gate(&TaskError::Malformed(
                        "VerifiedComplete without a verification proof (no executed checks)".into(),
                    ))));
                };
                // The claim goes under verification (NeedsVerification ->
                // Verifying), THEN the durable record is created and finalized
                // Passed, THEN completion runs. Record-first: the record
                // exists durably before the state can land VerifiedComplete,
                // so a crash between record-create and completion leaves the
                // row at Verifying with a recoverable record — the next
                // attempt converges with a fresh record.
                self.route_task_to(handle, task_id, TaskState::Verifying)?;
                let record = self.create_attempt_record(
                    handle,
                    task_id,
                    VerificationStatus::Passed,
                    proof,
                    now,
                )?;
                let rev = handle.task_revision(task_id)?;
                match handle.complete_verified_task(task_id, rev, record) {
                    Ok(_completed) => Ok(None),
                    Err(err) => {
                        // The row moved between the record's certification
                        // and the completion transaction (or refused the
                        // proof): the claim reverts to NeedsVerification —
                        // a later attempt re-certifies the CURRENT revision
                        // with a fresh record (a stale/Failed record never
                        // poisons that attempt). The typed refusal above is
                        // the genuine outcome: record the revert, never
                        // replace it with a route error.
                        self.dw_note_route_task_state(
                            handle,
                            task_id,
                            TaskState::NeedsVerification,
                            DW_SITE_ROUTE_AFTER_REFUSAL,
                        );
                        Ok(Some(completion_refusal_gate(&err)))
                    }
                }
            }
            CompletionGate::FailedVerification { .. } => {
                // A required check RAN and failed: the attempt is durably
                // recorded as Failed (one record per attempt), and the
                // machine returns the row to NeedsVerification — the
                // Verifying -> NeedsVerification edge expresses "verification
                // needs another iteration", which is exactly the runtime's
                // retryable failed-gate semantic (the session stays usable
                // and a later fixed turn re-verifies). Terminal rows only
                // land the Failed evidence record (no state moves).
                if let Some(proof) = proof {
                    if !task.state.is_terminal() {
                        self.route_task_to(handle, task_id, TaskState::Verifying)?;
                    }
                    let _record = self.create_attempt_record(
                        handle,
                        task_id,
                        VerificationStatus::Failed,
                        proof,
                        now,
                    )?;
                    if !task.state.is_terminal() {
                        self.route_task_to(handle, task_id, TaskState::NeedsVerification)?;
                    }
                }
                Ok(None)
            }
            CompletionGate::BlockedVerification { .. } => {
                // Blocked is the machine's express landing only from
                // Running/Waiting/Pending; a row at NeedsVerification or
                // Verifying (a previous failed attempt or crash residue) has
                // no legal edge to Blocked and keeps NeedsVerification — the
                // claim is still awaiting re-verification. No record: a
                // blocked gate never links completion proof.
                if task.state.is_terminal() {
                    return Ok(None);
                }
                if task_route(task.state, TaskState::Blocked).is_some() {
                    self.route_task_to(handle, task_id, TaskState::Blocked)?;
                }
                Ok(None)
            }
            CompletionGate::Unverified => {
                // No objective mechanism ran: the claim exists but nothing
                // verified it. Running -> NeedsVerification (the machine's
                // RequestVerification edge) is the landing; no record exists
                // because no attempt ran.
                if task.state.is_terminal() {
                    return Ok(None);
                }
                self.route_task_to(handle, task_id, TaskState::NeedsVerification)?;
                Ok(None)
            }
            CompletionGate::VerificationPending => {
                // The attempt's required checks run as durable background
                // jobs (audit P0-5/26): the row parks at Verifying until
                // every job of the attempt settles (a later genuine end
                // resolves them and drives the real gate). No record: the
                // attempt produced no verdict yet — nothing to certify.
                // Terminal rows never move (a completed task stays complete).
                if task.state.is_terminal() {
                    return Ok(None);
                }
                match task_route(task.state, TaskState::Verifying) {
                    Some(_) => {
                        self.route_task_to(handle, task_id, TaskState::Verifying)?;
                        Ok(None)
                    }
                    None => {
                        // Blocked/Failed rows have no legal edge to Verifying
                        // (the machine's gates); the jobs still settle on a
                        // later end, but the row keeps its state until a
                        // legal gate arrives.
                        tracing::warn!(
                            session = %handle.id(),
                            task = %task_id,
                            "VerificationPending gate: task at {:?} has no legal edge to Verifying; \
                             the row stays put while the background jobs settle",
                            task.state
                        );
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Create one per-attempt durable verification record certifying the
    /// task's CURRENT revision and finalize it in one shot (audit P0-8): the
    /// row is written as Running and CAS-finalized to `Passed`/`Failed`, so
    /// the durable record exists (Running or finalized) BEFORE any
    /// completion state can land — a crash between create and finalize, or
    /// between finalize and complete_verified_task, always leaves a
    /// recoverable record and a task row that is NOT VerifiedComplete.
    pub(crate) fn create_attempt_record(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        status: VerificationStatus,
        proof: &VerificationProof,
        started_ms: i64,
    ) -> faktor_core::Result<faktor_core::id::VerificationRecordId> {
        // Schema v20 (audits 94/116/117): every attempt record lands with the
        // bounded environment fingerprint and the compact candidate-proof
        // reference. Both are computed HERE, the single record-construction
        // site, from the same evidence the record certifies.
        let check_basis: Vec<(String, String, Vec<String>)> = proof
            .checks
            .iter()
            .map(|c| (c.check.clone(), c.program.clone(), c.args.clone()))
            .collect();
        let workspace = self.fingerprint_workspace(handle);
        let (environment_fingerprint, mut candidate_proof_ref) = self.verification_fingerprint(
            handle,
            task_id,
            &check_basis,
            &proof.changed_files,
            proof.review.as_ref(),
            workspace.as_ref(),
        )?;
        // The reference pins the revision the record certifies: read it in
        // the same command region. The session write re-checks it against
        // the row's revision (a race is a typed refusal, never a mismatch).
        candidate_proof_ref.task_revision = handle.task_revision(task_id)?;
        let record_id = handle.create_verification_record_with_evidence(
            task_id,
            None, // tree_hash: the runtime's checks operate on the working tree
            proof.criteria.clone(),
            proof.checks.clone(),
            proof.changed_files.clone(),
            Vec::new(), // unrelated changes: not tracked by this runtime
            proof.review.clone(),
            VerificationStatus::Running,
            started_ms,
            Some(environment_fingerprint),
            Some(candidate_proof_ref),
        )?;
        handle.finalize_verification_record(record_id, status, handle.now_ms())?;
        // P0-79 sites (b2/f) at the verification-record finalize site: a
        // Failed record with a DIFFERENT failure fingerprint (check id +
        // summary hash) than the previous failure of the same check is
        // progress through the failure space (FailureFingerprintChanged);
        // a Passed record following a Failed one is VerificationImproved.
        // Cheap hashes over the executed rows' own summaries — never an LLM.
        if matches!(
            status,
            VerificationStatus::Failed | VerificationStatus::Passed
        ) {
            let fingerprints: Vec<(String, u64)> = if status == VerificationStatus::Failed {
                proof
                    .checks
                    .iter()
                    .filter(|c| c.status == VerificationStatus::Failed)
                    .map(|c| {
                        (
                            c.check.clone(),
                            failure_fingerprint_of(&c.check, c.summary.as_deref()),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let now = self.deps.clock.now_ms();
            let mut reset_loop_window = false;
            self.with_tracker(handle.id(), |t| {
                let moved = t.note_failure_fingerprints(&fingerprints);
                if moved && status == VerificationStatus::Failed {
                    // The durable window of text-keyed loop signals counts
                    // IDENTICAL failing calls; a moved failure state is not
                    // an identical failure — the window must not punish
                    // state-progressing turns (P0-78).
                    reset_loop_window = true;
                    t.note_evidence(now, ProgressEvidence::FailureFingerprintChanged);
                }
                if status == VerificationStatus::Passed
                    && t.last_verification_status() == Some(VerificationStatus::Failed)
                {
                    t.note_evidence(now, ProgressEvidence::VerificationImproved);
                }
                t.set_last_verification_status(status);
            });
            if reset_loop_window {
                // The durable loop-window close must not be lost silently;
                // this site runs after the record was finalized, so the loss
                // is recorded (marker + audit), not propagated.
                self.dw_note_reset_loop_signals(handle, DW_SITE_ATTEMPT_RECORD_LOOP_SIGNALS);
            }
        }
        Ok(record_id)
    }

    /// Directory of the retry-on-next-open markers (see
    /// [`DURABLE_WRITE_MARKER_DIR`]).
    pub(crate) fn durable_marker_dir(&self) -> std::path::PathBuf {
        self.deps
            .session
            .store()
            .root()
            .join(DURABLE_WRITE_MARKER_DIR)
    }

    /// Record ONE failed durable write on a path that cannot propagate the
    /// error without masking its original error/outcome. Emits the durable
    /// marker file, the `CrashDetected` audit journal event and the
    /// structured error log; not even a total store failure is silent.
    pub(crate) fn record_durable_write_failure(
        &self,
        handle: &faktor_session::SessionHandle,
        site: &'static str,
        intent: &DurableWriteIntent,
        err: &faktor_core::Error,
    ) {
        tracing::error!(
            session = %handle.id(),
            site,
            kind = ?err.kind,
            error = %err.message,
            "durable write failed on a non-propagating path; writing the retry-on-next-open marker"
        );
        let at_ms = self.deps.clock.now_ms();
        let seq = DURABLE_WRITE_MARKER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // pid + random tag: two processes sharing one store root can observe
        // the same clock millisecond and per-process seq, so the name alone
        // must not decide identity (and `write_marker_file` refuses to
        // overwrite when even the full name collides).
        let key = format!(
            "dw-{at_ms:020}-{seq:012}-{:08x}-{:016x}",
            std::process::id(),
            marker_random_tag(at_ms, seq)
        );
        let mut marker = serde_json::json!({
            "status": "pending",
            "attempts": 0u64,
            "site": site,
            "session": handle.id().raw(),
            "at_ms": at_ms,
            "error": truncate(&err.message, 1024),
            "intent": intent,
        });
        // Bound the file without corrupting it: heavy diagnostic fields are
        // dropped first and the marker records that it is truncated.
        if serde_json::to_vec(&marker)
            .map(|b| b.len())
            .unwrap_or(usize::MAX)
            > DURABLE_WRITE_MARKER_MAX_BYTES
        {
            if let Some(fields) = marker.get_mut("intent").and_then(|i| i.as_object_mut()) {
                fields.insert("result_json".into(), serde_json::Value::Null);
                if let Some(payload) = fields.get_mut("payload").and_then(|p| p.as_object_mut()) {
                    let message = payload
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or_default()
                        .to_string();
                    payload.insert(
                        "message".into(),
                        serde_json::Value::String(truncate(&message, 8192)),
                    );
                }
            }
            marker["truncated"] = serde_json::Value::Bool(true);
        }
        let bytes = match serde_json::to_vec(&marker) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(session = %handle.id(), site, "durable-write marker could not be serialized: {e}");
                Vec::new()
            }
        };
        let dir = self.durable_marker_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!(session = %handle.id(), site, dir = %dir.display(), "durable-write marker directory unwritable: {e}");
        } else if let Err(e) = write_marker_file(&dir, &key, &bytes) {
            tracing::error!(session = %handle.id(), site, dir = %dir.display(), "durable-write marker file unwritable or collided: {e}");
        }
        // Existing audit surface: a CrashDetected self-transition carrying a
        // bounded typed description of the failure (opaque payload kind, no
        // schema evolution). Best-effort — the marker file above is the
        // authoritative compensation.
        let state = match handle.state() {
            Ok(state) => state,
            Err(e) => {
                tracing::error!(session = %handle.id(), site, "durable-write audit marker skipped: session state unreadable: {e}");
                return;
            }
        };
        let op_id = match intent {
            DurableWriteIntent::JournalFailed { op_id, .. } => OpId::try_from(*op_id).ok(),
            DurableWriteIntent::FinishTurnRecord { turn_op, .. } => OpId::try_from(*turn_op).ok(),
            DurableWriteIntent::ResolveVerificationJob { attempt_op, .. }
            | DurableWriteIntent::CancelVerificationAttempt { attempt_op, .. } => {
                OpId::try_from(*attempt_op).ok()
            }
            _ => None,
        };
        let audit = serde_json::json!({
            "durable_write_failure": {
                "site": site,
                "marker": key,
                "error": truncate(&err.message, 1024),
            }
        });
        if let Err(e) = handle.force_append_event(
            faktor_core::event::EventKind::CrashDetected,
            state,
            op_id,
            Some(audit),
        ) {
            tracing::error!(session = %handle.id(), site, "durable-write audit marker could not be journaled: {e}");
        }
    }

    /// Replay the pending retry-on-next-open markers of ONE session. Called at
    /// the very top of `recover_session`, so every open path reconstructs a
    /// lost transition before the session is driven again. Idempotent per
    /// intent: a marker is removed only after its intent applied (or was
    /// terminally refused — with a loud diagnostic); a store failure keeps it
    /// for the next open, bounded by [`DURABLE_WRITE_MARKER_MAX_ATTEMPTS`].
    pub(crate) fn replay_durable_write_failures(&self, handle: &faktor_session::SessionHandle) {
        let dir = self.durable_marker_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::error!(session = %handle.id(), dir = %dir.display(), "durable-write marker directory unreadable: {e}");
                return;
            }
        };
        let mut paths: Vec<std::path::PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        // Bound the directory before scanning it: oldest terminal markers are
        // consumed first, pending work is never touched.
        gc_durable_write_markers(&dir);
        if paths.len() > DURABLE_WRITE_MARKER_SCAN_MAX {
            tracing::error!(
                session = %handle.id(),
                pending = paths.len(),
                bound = DURABLE_WRITE_MARKER_SCAN_MAX,
                "durable-write markers exceed the open-time bound; the remainder replay at later opens"
            );
        }
        for path in paths.into_iter().take(DURABLE_WRITE_MARKER_SCAN_MAX) {
            let raw = match read_bounded_file(&path, DURABLE_WRITE_MARKER_MAX_BYTES) {
                Ok(raw) => raw,
                Err(e) => {
                    // Retained (never deleted): unreadable markers are
                    // evidence; the directory bound is what contains them.
                    tracing::error!(session = %handle.id(), path = %path.display(), "durable-write marker unreadable; retaining it: {e}");
                    continue;
                }
            };
            let mut marker: serde_json::Value = match serde_json::from_slice(&raw) {
                Ok(marker) => marker,
                Err(e) => {
                    // Retained: a corrupt marker is surfaced loudly and never
                    // silently discarded; GC consumes it only over the bound.
                    tracing::error!(session = %handle.id(), path = %path.display(), "durable-write marker corrupt; retaining it for inspection: {e}");
                    continue;
                }
            };
            // The session identity field is the ONLY thing that decides
            // whether a marker may be applied to this session. A missing,
            // non-numeric, zero or otherwise unparseable value is a corrupt
            // or tampered marker: applying it cross-session would let an
            // attacker (or a partial write) abort/route/finish ANOTHER
            // session's turn. Refuse it, keep it for forensics and surface
            // it loudly; only a well-formed marker that names exactly this
            // session may be replayed here.
            match marker_session_verdict(&marker, handle.id().raw()) {
                MarkerSession::Ours => {}
                MarkerSession::Foreign(other) => {
                    // Retained for that session's own open (every open scans
                    // the whole directory). Never applied here, and the
                    // refusal is surfaced — a marker whose session field is
                    // a different id is never silently trusted.
                    tracing::warn!(
                        session = %handle.id(),
                        marker_session = other,
                        path = %path.display(),
                        "durable-write marker belongs to another session; retaining it for that session's open, never applying it here"
                    );
                    continue;
                }
                MarkerSession::Malformed => {
                    tracing::error!(
                        session = %handle.id(),
                        path = %path.display(),
                        "durable-write marker carries no usable session field (absent/non-numeric/zero); retaining it and never applying it cross-session"
                    );
                    continue;
                }
            }
            let status = marker
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            match status.as_str() {
                "pending" => {}
                // Terminal bookkeeping states: the intent already landed (or
                // was terminally refused and surfaced before). CONSUME it —
                // it is never replayed and never lingers silently.
                "applied" | "done" | "abandoned" | "refused" | "cancelled" => {
                    tracing::info!(
                        session = %handle.id(),
                        path = %path.display(),
                        %status,
                        "durable-write marker is in a terminal state; consuming it (never replayed)"
                    );
                    remove_marker_file(&path);
                    continue;
                }
                other => {
                    // Unknown states are NOT silently skipped: surfaced and
                    // retained (GC bounds them), never replayed.
                    tracing::error!(
                        session = %handle.id(),
                        path = %path.display(),
                        status = other,
                        "durable-write marker carries an UNKNOWN status; retaining it, never replaying it"
                    );
                    continue;
                }
            }
            let site = marker
                .get("site")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string();
            let attempts = marker.get("attempts").and_then(|a| a.as_u64()).unwrap_or(0) + 1;
            let marker_at_ms = marker.get("at_ms").and_then(|a| a.as_i64()).unwrap_or(0);
            let intent: Option<DurableWriteIntent> = marker
                .get("intent")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok());
            let Some(intent) = intent else {
                tracing::error!(session = %handle.id(), %site, "durable-write marker carries an unknown intent; abandoning it (surfaced, never silent)");
                remove_marker_file(&path);
                continue;
            };
            match self.apply_durable_write_intent(handle, &intent, marker_at_ms) {
                Ok(()) => {
                    tracing::warn!(
                        session = %handle.id(),
                        %site,
                        attempts,
                        "a durable write lost to a failure was reconstructed from its marker"
                    );
                    remove_marker_file(&path);
                }
                Err(e) if e.kind.is_retryable() && attempts < DURABLE_WRITE_MARKER_MAX_ATTEMPTS => {
                    marker["attempts"] = serde_json::Value::from(attempts);
                    marker["error"] = serde_json::Value::String(truncate(&e.message, 1024));
                    if let Ok(bytes) = serde_json::to_vec(&marker) {
                        if let Err(write_err) = faktor_fs::atomic::atomic_replace(&path, &bytes) {
                            tracing::error!(session = %handle.id(), %site, "durable-write marker attempt update failed: {write_err}");
                        }
                    }
                    tracing::error!(session = %handle.id(), %site, attempts, "durable-write marker replay failed (retryable); kept for the next open: {e}");
                }
                Err(e) => {
                    tracing::error!(session = %handle.id(), %site, attempts, "durable-write marker replay terminally refused; abandoning it (surfaced, never silent): {e}");
                    remove_marker_file(&path);
                }
            }
        }
    }

    /// Apply one marker intent to durable state. Store errors are retryable
    /// (the marker stays); any other error means the intended transition is
    /// no longer reachable/legal (the marker is terminally refused, loudly).
    ///
    /// Every intent carries a DURABLE dedup witness so the crash window
    /// "the write committed, then reported Err, then the process died" can
    /// never replay as a duplicate:
    /// - `JournalFailed`: a complete paged scan of the op's journal (never a
    ///   fixed tail window).
    /// - `LedgerDecision`: a complete paged scan of the ledger.
    /// - `CancelVerificationAttempt`: the attempt's job rows (nothing open =>
    ///   already terminal).
    /// - `Abort`: the active turn record / durable queue rows, plus the
    ///   marker's own timestamp (a turn STARTED AFTER the intent is never
    ///   aborted by it).
    /// - `ResolveVerificationJob`: `job.state.is_open()`.
    /// - `FinishTurnRecord` / `UpsertMemoryFact` / `ResetLoopSignals` /
    ///   `RouteTaskState` / `RebuildTaskLedger`: naturally idempotent
    ///   operations (a rewrite cannot mint a second logical result).
    pub(crate) fn apply_durable_write_intent(
        &self,
        handle: &faktor_session::SessionHandle,
        intent: &DurableWriteIntent,
        intent_at_ms: i64,
    ) -> Result<(), faktor_core::Error> {
        match intent {
            DurableWriteIntent::JournalFailed {
                op_id,
                state,
                payload,
            } => {
                let op = OpId::try_from(*op_id)?;
                let state: AgentState = serde_json::from_value(serde_json::json!(state))?;
                if !self.op_failure_already_journaled(handle, op)
                    && (state_is_op_active(handle.state()?)
                        || handle.active_turn_record()?.map(|r| r.turn_op_id) == Some(op))
                {
                    handle.force_append_event(
                        faktor_core::event::EventKind::Failed,
                        state,
                        Some(op),
                        Some(payload.clone()),
                    )?;
                } else {
                    tracing::warn!(session = %handle.id(), op = %op, "stale failed-journal marker skipped: the turn already ended");
                }
                Ok(())
            }
            DurableWriteIntent::FinishTurnRecord { turn_op, status } => {
                handle.finish_turn_record(OpId::try_from(*turn_op)?, status)?;
                Ok(())
            }
            DurableWriteIntent::ResolveVerificationJob {
                task_id,
                attempt_op,
                check_id,
                state,
                note,
                result_json,
            } => {
                let jobs = handle.verification_attempt_jobs(*task_id, *attempt_op)?;
                let Some(job) = jobs.iter().find(|job| job.check_id == *check_id) else {
                    return Ok(()); // the attempt/job is gone: nothing left to reconstruct
                };
                if !job.state.is_open() {
                    return Ok(()); // a concurrent resolver already landed it
                }
                let state: faktor_session::VerificationJobState =
                    serde_json::from_value(serde_json::json!(state))?;
                handle.resolve_verification_job(
                    *task_id,
                    check_id,
                    *attempt_op,
                    state,
                    note.clone(),
                    result_json.clone(),
                )?;
                Ok(())
            }
            DurableWriteIntent::CancelVerificationAttempt {
                task_id,
                attempt_op,
                note,
            } => {
                // Durable dedup: the attempt's job rows are the witness. An
                // attempt with no open job (or no rows at all) was already
                // cancelled/resolved — cancelling again would journal a
                // second, spurious cancellation.
                let jobs = handle.verification_attempt_jobs(*task_id, *attempt_op)?;
                if jobs.is_empty() {
                    tracing::warn!(
                        session = %handle.id(),
                        task = *task_id,
                        attempt_op = *attempt_op,
                        "stale cancel-verification marker skipped: the attempt has no job rows left"
                    );
                    return Ok(());
                }
                if !jobs.iter().any(|job| job.state.is_open()) {
                    tracing::warn!(
                        session = %handle.id(),
                        task = *task_id,
                        attempt_op = *attempt_op,
                        "stale cancel-verification marker skipped: every job of the attempt is already terminal"
                    );
                    return Ok(());
                }
                handle.cancel_verification_attempt(*task_id, *attempt_op, note)?;
                Ok(())
            }
            DurableWriteIntent::UpsertMemoryFact { kind, key, value } => {
                handle.upsert_memory_fact(kind, key, value)?;
                Ok(())
            }
            DurableWriteIntent::Abort { op_id } => {
                let op = match op_id {
                    Some(raw) => Some(OpId::try_from(*raw)?),
                    None => None,
                };
                // Durable dedup by intent identity: the abort already took
                // effect when no durable witness covers the intent's target —
                // an active turn record, a durable queue row, or an OPEN tool
                // run row of that op. An abort(None) whose active turn STARTED
                // AFTER the marker is a stale intent that must never kill a
                // LATER turn.
                let active = handle.active_turn_record()?;
                let queued = self
                    .deps
                    .session
                    .store()
                    .queue_op_ids(handle.id())
                    .map_err(|e| Error::new(ErrorKind::Store, e.to_string()))?;
                let pending_tools = handle.pending_tool_runs()?;
                let already_effective = match op {
                    Some(op) => {
                        let covered_by_record = active.as_ref().map(|r| r.turn_op_id) == Some(op);
                        let covered_by_queue = queued.contains(&op);
                        let covered_by_tool = pending_tools.iter().any(|row| row.op_id == op);
                        !covered_by_record && !covered_by_queue && !covered_by_tool
                    }
                    None => match &active {
                        Some(record) if record.started_at >= intent_at_ms => true,
                        _ => active.is_none() && queued.is_empty() && pending_tools.is_empty(),
                    },
                };
                if already_effective {
                    tracing::warn!(
                        session = %handle.id(),
                        op = ?op,
                        intent_at_ms,
                        "stale abort marker skipped: the intent is already effective durably \
                         (no turn/queue row left, or the active turn started after the intent)"
                    );
                    return Ok(());
                }
                handle.abort(op)?;
                Ok(())
            }
            DurableWriteIntent::LedgerDecision {
                step,
                choice,
                rationale,
            } => {
                // Dedup: a write that committed before reporting Err must not
                // mint a duplicate decision row on replay.
                if !self.ledger_decision_already_present(handle, step, choice, rationale) {
                    handle.ledger_decision(step, choice, rationale)?;
                }
                Ok(())
            }
            DurableWriteIntent::ResetLoopSignals => {
                handle.reset_loop_signals()?;
                Ok(())
            }
            DurableWriteIntent::RouteTaskState { task_id, state } => {
                let state: TaskState = serde_json::from_value(serde_json::json!(state))?;
                self.route_task_to(handle, TaskId::try_from(*task_id)?, state)?;
                Ok(())
            }
            DurableWriteIntent::RebuildTaskLedger { .. } => {
                // Dedup: a concurrent writer (or an earlier replay) may have
                // already landed a decodable row — then the marker is a
                // no-op. A missing row is likewise nothing to reconstruct.
                match handle.get_task_ledger()? {
                    None => return Ok(()),
                    Some(v) => {
                        if serde_json::from_value::<TaskLedger>(v).is_ok() {
                            return Ok(());
                        }
                    }
                }
                let rebuilt = self.rebuild_task_ledger_from_durable(handle)?;
                handle.put_task_ledger(serde_json::to_value(&rebuilt)?)?;
                Ok(())
            }
        }
    }

    /// True when the journal ALREADY carries a `kind` event of `op` (the write
    /// may have committed before reporting Err). COMPLETE, paged backward
    /// scan of the durable journal — never a fixed tail window. A read
    /// failure or the page bound reports `true` (conservative: never risk a
    /// duplicate) with a loud error.
    pub(crate) fn journal_has_event(
        &self,
        handle: &faktor_session::SessionHandle,
        op: OpId,
        kind: faktor_core::event::EventKind,
    ) -> faktor_core::Result<bool> {
        let Some(last) = handle.last_event_seq()? else {
            return Ok(false);
        };
        let mut hi = last.raw();
        let mut pages = 0u32;
        loop {
            let from = hi.saturating_sub(DW_DEDUP_PAGE - 1).max(1);
            let events = handle.events_range(from, Some(DW_DEDUP_PAGE))?;
            if events.iter().any(|e| e.op_id == Some(op) && e.kind == kind) {
                return Ok(true);
            }
            if events.is_empty() || from == 1 {
                return Ok(false);
            }
            hi = from - 1;
            pages += 1;
            if pages >= DW_DEDUP_MAX_PAGES {
                tracing::error!(
                    session = %handle.id(),
                    op = %op,
                    pages,
                    bound = DW_DEDUP_MAX_PAGES,
                    "durable replay dedup scan exceeded its page bound; treating the event as \
                     already present (a duplicate is worse than a skipped redundant append)"
                );
                return Ok(true);
            }
        }
    }

    /// True when the journal's COMPLETE history already carries the `Failed`
    /// event of `op` (the write may have committed before reporting Err).
    pub(crate) fn op_failure_already_journaled(
        &self,
        handle: &faktor_session::SessionHandle,
        op: OpId,
    ) -> bool {
        match self.journal_has_event(handle, op, faktor_core::event::EventKind::Failed) {
            Ok(found) => found,
            Err(e) => {
                tracing::error!(
                    session = %handle.id(),
                    op = %op,
                    error = %e.message,
                    "durable replay dedup could not scan the journal; skipping the append to \
                     avoid a duplicate: {e}"
                );
                true
            }
        }
    }

    /// True when the ledger's COMPLETE history already carries this typed
    /// decision (replay dedup: a committed-then-errored write must not mint a
    /// duplicate decision row). Paged backward over the durable ledger —
    /// never a fixed tail window; a read failure or the page bound reports
    /// `true` (conservative) with a loud error.
    pub(crate) fn ledger_decision_already_present(
        &self,
        handle: &faktor_session::SessionHandle,
        step: &str,
        choice: &str,
        rationale: &str,
    ) -> bool {
        let store = self.deps.session.store();
        let mut before: Option<i64> = None;
        let mut pages = 0u32;
        loop {
            let rows = match store.ledger_entries_desc(handle.id(), before, DW_DEDUP_PAGE) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::error!(
                        session = %handle.id(),
                        error = %e,
                        "durable replay dedup could not scan the ledger; skipping the append to \
                         avoid a duplicate: {e}"
                    );
                    return true;
                }
            };
            if rows.is_empty() {
                return false;
            }
            if rows.iter().any(|row| {
                row.payload.get("kind").and_then(|k| k.as_str()) == Some("decision")
                    && row.payload.get("step").and_then(|v| v.as_str()) == Some(step)
                    && row.payload.get("choice").and_then(|v| v.as_str()) == Some(choice)
                    && row.payload.get("rationale").and_then(|v| v.as_str()) == Some(rationale)
            }) {
                return true;
            }
            let short_page = rows.len() < DW_DEDUP_PAGE as usize;
            before = rows.last().map(|row| row.seq);
            pages += 1;
            if short_page {
                return false;
            }
            if pages >= DW_DEDUP_MAX_PAGES {
                tracing::error!(
                    session = %handle.id(),
                    pages,
                    bound = DW_DEDUP_MAX_PAGES,
                    "durable replay ledger dedup scan exceeded its page bound; treating the \
                     decision as already present (a duplicate is worse)"
                );
                return true;
            }
        }
    }

    /// Test-only fault seam: the next guarded write for `site` fails with a
    /// synthesized store error, so every discard/recovery path is exercised
    /// against a real failure without corrupting the whole store.
    pub(crate) fn take_durable_write_fault(&self, site: &str) -> Option<faktor_core::Error> {
        #[cfg(test)]
        if durable_faults_tests::take(self.deps.session.store().root(), site) {
            return Some(faktor_core::Error::new(
                ErrorKind::Store,
                format!("injected durable-write failure at `{site}`"),
            ));
        }
        #[cfg(not(test))]
        let _ = site;
        None
    }

    // ---- propagating wrap helpers (caller fails safely; typed error out) --

    pub(crate) fn guarded_upsert_memory_fact(
        &self,
        handle: &faktor_session::SessionHandle,
        kind: &str,
        key: &str,
        value: &str,
        site: &'static str,
    ) -> faktor_core::Result<()> {
        if let Some(err) = self.take_durable_write_fault(site) {
            tracing::error!(session = %handle.id(), site, "durable memory-fact write failed (propagating): {err}");
            return Err(err);
        }
        handle.upsert_memory_fact(kind, key, value).map_err(|err| {
            tracing::error!(session = %handle.id(), site, "durable memory-fact write failed (propagating): {err}");
            err
        })
    }

    pub(crate) fn guarded_ledger_decision(
        &self,
        handle: &faktor_session::SessionHandle,
        step: &str,
        choice: &str,
        rationale: &str,
        site: &'static str,
    ) -> faktor_core::Result<()> {
        if let Some(err) = self.take_durable_write_fault(site) {
            tracing::error!(session = %handle.id(), site, "durable ledger decision failed (propagating): {err}");
            return Err(err);
        }
        handle
            .ledger_decision(step, choice, rationale)
            .map(|_| ())
            .map_err(|err| {
                tracing::error!(session = %handle.id(), site, "durable ledger decision failed (propagating): {err}");
                err
            })
    }

    pub(crate) fn dw_note_ledger_decision(
        &self,
        handle: &faktor_session::SessionHandle,
        step: &str,
        choice: &str,
        rationale: &str,
        site: &'static str,
    ) {
        let intent = DurableWriteIntent::LedgerDecision {
            step: step.to_string(),
            choice: choice.to_string(),
            rationale: rationale.to_string(),
        };
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) = handle.ledger_decision(step, choice, rationale) {
            self.record_durable_write_failure(handle, site, &intent, &err);
        }
    }

    pub(crate) fn dw_note_reset_loop_signals(
        &self,
        handle: &faktor_session::SessionHandle,
        site: &'static str,
    ) {
        let intent = DurableWriteIntent::ResetLoopSignals;
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) = handle.reset_loop_signals() {
            self.record_durable_write_failure(handle, site, &intent, &err);
        }
    }

    pub(crate) fn guarded_reset_loop_signals(
        &self,
        handle: &faktor_session::SessionHandle,
        site: &'static str,
    ) -> faktor_core::Result<()> {
        if let Some(err) = self.take_durable_write_fault(site) {
            tracing::error!(session = %handle.id(), site, "durable loop-signal reset failed (propagating): {err}");
            return Err(err);
        }
        handle.reset_loop_signals().map_err(|err| {
            tracing::error!(session = %handle.id(), site, "durable loop-signal reset failed (propagating): {err}");
            err
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn guarded_set_turn_envelope(
        &self,
        handle: &faktor_session::SessionHandle,
        turn_op: OpId,
        provider: &str,
        model: &str,
        variant: Option<&str>,
        tool_mode: Option<&str>,
        site: &'static str,
    ) -> faktor_core::Result<()> {
        if let Some(err) = self.take_durable_write_fault(site) {
            tracing::error!(session = %handle.id(), site, "durable turn-envelope write failed (propagating): {err}");
            return Err(err);
        }
        handle
            .set_turn_envelope(turn_op, provider, model, variant, tool_mode)
            .map_err(|err| {
                tracing::error!(session = %handle.id(), site, "durable turn-envelope write failed (propagating): {err}");
                err
            })
    }

    pub(crate) fn dw_note_abort(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: Option<OpId>,
        site: &'static str,
    ) {
        let intent = DurableWriteIntent::Abort {
            op_id: op_id.map(|op| op.raw()),
        };
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) = handle.abort(op_id) {
            self.record_durable_write_failure(handle, site, &intent, &err);
        }
    }

    // ---- recording wrap helpers (original error/outcome preserved) --------

    pub(crate) async fn dw_note_journal_failed(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        state: AgentState,
        payload: serde_json::Value,
        site: &'static str,
    ) {
        let intent = DurableWriteIntent::JournalFailed {
            op_id: op_id.raw(),
            state: state_tag(state),
            payload: payload.clone(),
        };
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) = handle
            .append_journal_event(
                faktor_core::event::EventKind::Failed,
                state,
                Some(op_id),
                Some(payload),
            )
            .await
        {
            self.record_durable_write_failure(handle, site, &intent, &err);
        }
    }

    pub(crate) fn dw_note_finish_turn_record(
        &self,
        handle: &faktor_session::SessionHandle,
        turn_op: OpId,
        status: &str,
        site: &'static str,
    ) {
        let intent = DurableWriteIntent::FinishTurnRecord {
            turn_op: turn_op.raw(),
            status: status.to_string(),
        };
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) = handle.finish_turn_record(turn_op, status) {
            self.record_durable_write_failure(handle, site, &intent, &err);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dw_note_resolve_verification_job(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: u64,
        check_id: &str,
        attempt_op: u64,
        state: faktor_session::VerificationJobState,
        note: Option<String>,
        result_json: Option<String>,
        site: &'static str,
    ) {
        let state_tag = serde_json::to_value(state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unavailable".into());
        let intent = DurableWriteIntent::ResolveVerificationJob {
            task_id,
            attempt_op,
            check_id: check_id.to_string(),
            state: state_tag,
            note: note.clone(),
            result_json: result_json.clone(),
        };
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) =
            handle.resolve_verification_job(task_id, check_id, attempt_op, state, note, result_json)
        {
            self.record_durable_write_failure(handle, site, &intent, &err.into());
        }
    }

    pub(crate) fn dw_note_cancel_verification_attempt(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: u64,
        attempt_op: u64,
        note: &str,
        site: &'static str,
    ) {
        let intent = DurableWriteIntent::CancelVerificationAttempt {
            task_id,
            attempt_op,
            note: note.to_string(),
        };
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) = handle.cancel_verification_attempt(task_id, attempt_op, note) {
            self.record_durable_write_failure(handle, site, &intent, &err.into());
        }
    }

    pub(crate) fn dw_note_upsert_memory_fact(
        &self,
        handle: &faktor_session::SessionHandle,
        kind: &str,
        key: &str,
        value: &str,
        site: &'static str,
    ) {
        let intent = DurableWriteIntent::UpsertMemoryFact {
            kind: kind.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        };
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) = handle.upsert_memory_fact(kind, key, value) {
            self.record_durable_write_failure(handle, site, &intent, &err);
        }
    }

    pub(crate) fn dw_note_route_task_state(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        state: TaskState,
        site: &'static str,
    ) {
        let state_name = serde_json::to_value(state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "needs_verification".into());
        let intent = DurableWriteIntent::RouteTaskState {
            task_id: task_id.raw(),
            state: state_name,
        };
        if let Some(err) = self.take_durable_write_fault(site) {
            self.record_durable_write_failure(handle, site, &intent, &err);
            return;
        }
        if let Err(err) = self.route_task_to(handle, task_id, state) {
            self.record_durable_write_failure(handle, site, &intent, &err);
        }
    }
}
