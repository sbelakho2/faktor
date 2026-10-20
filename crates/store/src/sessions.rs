//! `sessions`: cohesive slice of the mechanically decomposed parent module.
//! Cursor-paged History lives in `session_listing_page` (size split).

use super::*;

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    /// Durable worktree identity of the session (v8+). The standalone
    /// default is 1/1 (the session's worktree/task ids are adopted
    /// deliberately when a WorktreeManager-created worktree takes it over).
    pub worktree_id: WorktreeId,
    /// Durable task identity of the session (v8+); standalone default 1.
    pub task_id: TaskId,
    pub title: String,
    pub provider: String,
    pub model: String,
    pub state: AgentState,
    pub lifecycle: faktor_core::state::SessionLifecycle,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// Bound on pending-permission rows materialized by
/// [`Store::all_pending_permissions`] (`doctor --deep` wedge scan): the total
/// count stays exact, only the detail rows are capped.
pub const MAX_PENDING_PERMISSION_SCAN: usize = 256;

/// One still-`pending` permission row returned by
/// [`Store::all_pending_permissions`] (read-only `doctor --deep` wedge scan).
/// The row's waiter lives only in the daemon process, so a reader that is not
/// that process can never resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermissionRow {
    pub id: i64,
    pub session_id: SessionId,
    pub op_id: OpId,
    pub expires_ms: i64,
}

/// [`Store::all_pending_permissions`] result: the exact total plus at most
/// [`MAX_PENDING_PERMISSION_SCAN`] detail rows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PendingPermissionScan {
    pub total: u64,
    pub rows: Vec<PendingPermissionRow>,
}

/// One durable child-runtime projection row (migration v23): the child
/// session's state plus its bounded blocker truth. All blocker columns are
/// NULL when the child is not blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRuntimeRow {
    pub session_id: SessionId,
    pub child_id: String,
    pub state: String,
    pub blocker_kind: Option<String>,
    pub blocker_reason: Option<String>,
    pub blocker_dependency: Option<String>,
    pub blocker_resolution: Option<String>,
    pub last_progress_ms: Option<i64>,
    pub updated_ms: i64,
}

/// One atomic session transition: verify expected lifecycle/state, move
/// lifecycle+state, and append the journal event in a SINGLE SQLite
/// transaction. A crash can never leave the lifecycle and the journal
/// contradictory (the old two-step update-then-append had exactly that
/// window). `expected_* = None` skips the corresponding check.
#[derive(Debug, Clone)]
pub struct SessionTransition {
    /// When `Some`, the session row must have exactly this lifecycle or the
    /// transition fails with `StoreError::Conflict` and writes nothing.
    pub expected_lifecycle: Option<SessionLifecycle>,
    /// When `Some`, the lifecycle is updated to this value.
    pub new_lifecycle: Option<SessionLifecycle>,
    /// When `Some`, the session row must have exactly this state.
    pub expected_state: Option<AgentState>,
    /// The state the session row AND the journal event land on.
    pub new_state: AgentState,
    /// Journal event kind appended in the same transaction.
    pub event_kind: EventKind,
    pub event_payload: Option<serde_json::Value>,
    /// Payload schema version of `event_payload` (v11+; 1 for the original
    /// unversioned writers). Readers decode through the version; an unknown
    /// version is a loud error, never a silent parse.
    pub event_payload_ver: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRow {
    pub id: i64,
    pub session_id: SessionId,
    pub sequence: i64,
    pub path: String,
    pub before_hash: String,
    pub after_hash: String,
    /// CAS hash of the AFTER-content blob (v3+). NULL on rows recorded
    /// before the column existed: redo/diff refuse those honestly.
    pub after_cas_hash: Option<String>,
    /// Per-side EXISTENCE flags (v6+). A hash alone cannot distinguish a
    /// missing file from an empty one (both sides of a missing→empty write
    /// hash to blake3("")). When a side `exists` is false its hash column is
    /// the empty string (no content exists to address).
    ///
    /// Backward compatibility: pre-v6 rows carry NO marker, so these read as
    /// true — old rows were only recorded for real files (the caller had
    /// read/hashed the content on both sides).
    pub before_exists: bool,
    pub after_exists: bool,
    pub created_ms: i64,
    pub restored_ms: Option<i64>,
}

fn checkpoint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRow> {
    Ok(CheckpointRow {
        id: row.get(0)?,
        session_id: SessionId::new(row.get::<_, i64>(1)? as u64),
        sequence: row.get(2)?,
        path: row.get(3)?,
        before_hash: row.get(4)?,
        after_hash: row.get(5)?,
        after_cas_hash: row.get(6)?,
        before_exists: row.get::<_, i64>(7)? != 0,
        after_exists: row.get::<_, i64>(8)? != 0,
        created_ms: row.get(9)?,
        restored_ms: row.get(10)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRow {
    pub id: i64,
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub branch: String,
    pub active: bool,
}

// ------------------------------------------- atomic session commands
//
// A session command that mutates BOTH a side table (tool_run / permission
// / checkpoint / compaction) and the journal used to be two separate
// writes: a crash between them left side-table reality absent from the
// journal, violating journal-as-authority. [`SessionCommandTxn`] makes
// each such command ONE SQLite transaction (BEGIN IMMEDIATE): verify the
// session is still in the caller-observed state, mutate the side row,
// verify exactly one row changed, append the journal event through the
// shared gapless-seq path (which also moves the session state), then
// COMMIT. At every crash point the durable world is exactly the OLD state
// or exactly the NEW state, never a hybrid.

/// The journal half of one atomic session command: the event appended in
/// the SAME transaction as the command's side-table row. `ts_ms` is the
/// caller's clock (the session layer's injectable clock); side-row
/// timestamps keep using the store's wall clock exactly as the raw writes
/// did.
#[derive(Debug, Clone)]
pub struct CommandEvent {
    pub kind: EventKind,
    pub state: AgentState,
    pub op_id: Option<OpId>,
    pub ts_ms: i64,
    pub payload: Option<serde_json::Value>,
    pub payload_ver: i64,
}

/// The outcome of one durable permission-expiry reconciliation sweep
/// (crash recovery): the pending rows that were terminalized (`decision =
/// 'expired'`, `resolved_ms` stamped) and the single journal event that
/// explains them. An empty sweep is the idempotent no-op: no row touched,
/// no event appended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpiredPermissionResolution {
    /// Expired rows this sweep moved from `pending` to `expired`, as
    /// `(permission_id, op_id)`, ascending by permission id.
    pub expired: Vec<(i64, OpId)>,
    /// Sequence of the journaled `PermissionExpired` event; `None` when
    /// nothing was expired (a second sweep, or a deadline still ahead).
    pub event_seq: Option<EventSeq>,
}

impl ExpiredPermissionResolution {
    /// True when nothing was expired and nothing was journaled.
    pub fn is_empty(&self) -> bool {
        self.expired.is_empty()
    }
}

/// Every `AgentState` variant, in enum declaration order. The exhaustive
/// `match` over every arm is a compile-time drift guard (the `TaskState`
/// pattern): a new variant fails this build until the vocabulary is
/// extended, so a legitimately persisted new state can never be classified
/// `Corrupt`.
fn all_agent_states() -> [AgentState; 17] {
    use AgentState::*;
    let all = [
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
    ];
    for state in all {
        match state {
            Idle | Preparing | BuildingContext | WaitingForModel | Streaming | ToolRequested
            | WaitingForPermission | ExecutingTool | Validating | UpdatingMemory
            | ReadyForNextTurn | Completed | Cancelled | FailedRecoverable | FailedPermanent
            | NeedsUserInput | Suspended => {}
        }
    }
    all
}

/// [`all_agent_states`] serialized exactly as the durable session rows
/// serialize it, built once on the caller's thread. `begin_prepared` runs on
/// the single writer owner, so it can only SCAN this caller-prepared
/// vocabulary — never serialize a state itself.
fn valid_state_json() -> &'static [String] {
    static VALID: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    VALID.get_or_init(|| {
        all_agent_states()
            .into_iter()
            .map(|state| {
                serde_json::to_string(&state)
                    .expect("in-process AgentState serialization cannot fail")
            })
            .collect()
    })
}

/// One session state expectation PREPARED on the caller's thread for
/// [`SessionCommandTxn::begin_prepared`]: the canonical `state` JSON text a
/// durable session row must carry, the canonical vocabulary that separates a
/// valid-but-unexpected durable state (`Conflict`) from text that is not an
/// `AgentState` at all (`Corrupt`), plus every refusal message. Serialization
/// happens HERE, so the writer closure compares raw text and never decodes a
/// state or materializes a message on the single writer owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedSessionStateExpectation {
    pub state: AgentState,
    pub state_json: String,
    pub valid_state_json: &'static [String],
    pub missing_message: String,
    pub mismatch_message: String,
    pub corrupt_message: String,
}

impl PreparedSessionStateExpectation {
    /// Serialize the expected state once and prepare all three refusal
    /// messages caller-side (in-process `AgentState` serialization cannot
    /// fail).
    pub fn prepare(session: SessionId, expected: AgentState) -> Self {
        let state_json = serde_json::to_string(&expected)
            .expect("in-process AgentState serialization cannot fail");
        Self {
            state: expected,
            state_json,
            valid_state_json: valid_state_json(),
            missing_message: format!("session {session} does not exist; session command refused"),
            mismatch_message: format!(
                "session {session} state is not the expected {expected:?}; \
                 session command refused before any write"
            ),
            corrupt_message: format!(
                "session {session} durable state is not a valid AgentState; \
                 session command refused before any write (storage is corrupt)"
            ),
        }
    }
}

/// One logical session command inside ONE SQLite transaction
/// (`BEGIN IMMEDIATE`). The session must exist and be exactly in the
/// caller's `expected_state` or the command refuses with `Conflict`
/// BEFORE any write (no side row, no event). The command's side row and
/// its journal event then commit together; the deterministic crash seams
/// fire at the three durability boundaries:
///
/// - `session_command_side_row`: the side row is written, the event is
///   not yet (panic rolls the WHOLE command back — old state);
/// - `session_command_precommit`: the event is written, COMMIT is not yet
///   (panic rolls the WHOLE command back — old state);
/// - `session_command_committed`: COMMIT returned, the acknowledgement
///   was lost (the command is durable — new state).
///
/// Dropping without [`Self::commit`] rolls the whole command back,
/// exactly like a process death before the durability boundary.
///
/// The command body executes on the writer owner thread, so a seam panic
/// unwinds there and drops the in-flight transaction (rollback). The
/// [`WriterService`] contains that panic, stops admitting mutations and
/// surfaces the typed `StoreError::WriterUnavailable` to the caller; tests
/// certify the crash with [`Store::seam_crash_observed`] before reopening.
#[doc(hidden)]
pub struct SessionCommandTxn<'a> {
    pub(crate) tx: rusqlite::Transaction<'a>,
    pub(crate) session: SessionId,
    pub(crate) expected_state: AgentState,
    pub(crate) seam: &'a CrashSeam,
}

impl<'a> SessionCommandTxn<'a> {
    /// BEGIN IMMEDIATE, read the session row inside the transaction and
    /// verify its raw `state` text is exactly the caller-prepared canonical
    /// JSON. Missing session or a valid-but-unexpected state refuses
    /// `Conflict`; text that is not in the caller-prepared `AgentState`
    /// vocabulary refuses `Corrupt` — both before any write. The helper
    /// formats nothing, so the writer job stays SQL-only.
    pub(crate) fn begin_prepared(
        conn: &'a mut Connection,
        seam: &'a CrashSeam,
        session: SessionId,
        expected: PreparedSessionStateExpectation,
    ) -> StoreResult<Self> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT state FROM session WHERE id = ?1",
                params![session.raw() as i64],
                |r| r.get(0),
            )
            .optional()?;
        let PreparedSessionStateExpectation {
            state,
            state_json,
            valid_state_json,
            missing_message,
            mismatch_message,
            corrupt_message,
        } = expected;
        let raw = raw.ok_or(StoreError::Conflict(missing_message))?;
        if raw == state_json {
            return Ok(Self {
                tx,
                session,
                expected_state: state,
                seam,
            });
        }
        if !valid_state_json.contains(&raw) {
            return Err(StoreError::Corrupt(vec![corrupt_message]));
        }
        Err(StoreError::Conflict(mismatch_message))
    }

    /// The session this command belongs to.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The pre-state this command verified inside the transaction.
    pub fn expected_state(&self) -> AgentState {
        self.expected_state
    }

    /// The transaction connection, for the shared gapless-seq insert path.
    pub(crate) fn conn(&self) -> &Connection {
        &self.tx
    }

    /// Durability boundary: the side row is written, the event is not yet.
    pub(crate) fn side_row_applied(&self) {
        self.seam.trip("session_command_side_row");
    }

    /// Durability boundary: the event is written, COMMIT is not yet.
    pub(crate) fn precommit(&self) {
        self.seam.trip("session_command_precommit");
    }

    /// COMMIT the command, then trip the post-commit boundary: the
    /// command is durable and the acknowledgement was lost.
    pub(crate) fn commit(self) -> StoreResult<()> {
        self.tx.commit()?;
        self.seam.trip("session_command_committed");
        Ok(())
    }
}

pub(crate) fn session_row_map(r: &rusqlite::Row<'_>) -> StoreResult<SessionRow> {
    let id_raw: i64 = r.get(0)?;
    let id = id_field::<SessionId>(&format!("session id {id_raw}"), id_raw)?;
    Ok(SessionRow {
        id,
        workspace_id: id_field(&format!("session {id} workspace_id"), r.get::<_, i64>(1)?)?,
        worktree_id: id_field(&format!("session {id} worktree_id"), r.get::<_, i64>(2)?)?,
        task_id: id_field(&format!("session {id} task_id"), r.get::<_, i64>(3)?)?,
        title: r.get(4)?,
        provider: r.get(5)?,
        model: r.get(6)?,
        state: parse_json(&format!("session {id} state"), &r.get::<_, String>(7)?)?,
        lifecycle: parse_lifecycle(&format!("session {id} lifecycle"), &r.get::<_, String>(8)?)?,
        created_ms: r.get(9)?,
        updated_ms: r.get(10)?,
    })
}

/// Parse of a persisted lifecycle that FAILS CLOSED on corruption.
///
/// A session that was really Closed/FailedPermanent must never silently
/// become Open (an autonomous agent could accept work again), so unreadable
/// content surfaces as `StoreError::Corrupt` instead of defaulting to Open.
///
/// The one tolerated non-JSON spelling is the bare literal `open`: the v2
/// schema declares `lifecycle TEXT NOT NULL DEFAULT 'open'`, `create_session`
/// INSERTs exactly that SQL literal, and the v2 ALTER backfilled every
/// pre-existing row to it — it is the schema's own default representation of
/// `Open`, not corruption. The column is NOT NULL, so a NULL lifecycle cannot
/// occur; had one been read (e.g. constraints disabled), the decode would
/// fail via the `Sqlite` error rather than reopening the session.
pub(crate) fn parse_lifecycle(
    ctx: &str,
    raw: &str,
) -> StoreResult<faktor_core::state::SessionLifecycle> {
    if raw == "open" {
        return Ok(faktor_core::state::SessionLifecycle::Open);
    }
    serde_json::from_str(raw)
        .map_err(|e| StoreError::Corrupt(vec![format!("{ctx}: lifecycle {raw:?} is corrupt: {e}")]))
}

#[cfg(test)]
#[path = "session_command_txn_tests.rs"]
mod session_command_txn_tests;

#[cfg(test)]
#[path = "session_listing_tests.rs"]
mod session_listing_tests;

impl Store {
    pub fn create_session(
        &self,
        workspace_id: WorkspaceId,
        title: &str,
        provider: &str,
        model: &str,
    ) -> StoreResult<SessionRow> {
        let title = title.to_owned();
        let provider = provider.to_owned();
        let model = model.to_owned();
        // Preparation BEFORE enqueueing: the seed event's JSON is serialized
        // here, not on the writer owner.
        let created_payload_json =
            serde_json::json!({ "title": title, "provider": provider, "model": model }).to_string();
        // In-process constructed enum: serialization of a unit variant can
        // never fail, and it is prepared here rather than on the writer owner.
        let idle_state_json = serde_json::to_string(&AgentState::Idle).unwrap();
        // Preparation BEFORE enqueueing: the timestamp and the journal
        // diagnostic context are captured on the caller's thread (audit item
        // 8). The session id is SQLite-allocated inside the transaction, so
        // the seed diagnostics are the caller-prepared static text.
        let seq_messages = EventSeqMessages::for_new_session();
        let now = now_ms();
        let seam = Arc::clone(&self.seam);
        self.writer.execute("create_session", move |conn| {
        // ONE transaction: the session row and its seed event commit
        // together or not at all (a crash between them used to leave a
        // session row whose journal could later start at seq 1 with an
        // event that is not SessionCreated).
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO session(workspace_id, title, provider, model, state, lifecycle, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?6)",
            params![
                workspace_id.raw() as i64,
                title,
                provider,
                model,
                idle_state_json,
                now
            ],
        )?;
        let id: i64 = tx.last_insert_rowid();
        let sid = SessionId::new(id as u64);
        // Durability boundary: the session row is written, the seed event is
        // not yet (panic rolls the WHOLE creation back — no session row).
        seam.trip("create_session_row");
        // Seed the journal with SessionCreated so every session starts at seq 1.
        Self::insert_event_locked(
            &tx,
            sid,
            None,
            EventKind::SessionCreated,
            &idle_state_json,
            now,
            Some(created_payload_json),
            1,
            &seq_messages,
        )?;
        // Durability boundary: the seed event is written, COMMIT is not yet
        // (panic rolls the WHOLE creation back — no session, no event).
        seam.trip("create_session_precommit");
        let row = Self::get_session_locked(&tx, sid)?.ok_or_else(|| {
            StoreError::Corrupt(vec![
                "just-created session not readable inside creation transaction".into(),
            ])
        })?;
        tx.commit()?;
        // Durability boundary: COMMIT returned, the acknowledgement was lost
        // (the session row and its seq-1 seed event are durable together).
        seam.trip("create_session_committed");
        Ok(row)
        })
    }

    pub fn get_session(&self, id: SessionId) -> StoreResult<Option<SessionRow>> {
        let conn = self.read()?;
        let row = Self::get_session_locked(&conn, id)?;
        Ok(row)
    }

    pub(crate) fn get_session_locked(
        conn: &Connection,
        id: SessionId,
    ) -> StoreResult<Option<SessionRow>> {
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state, lifecycle, created_ms, updated_ms
             FROM session WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.raw() as i64])?;
        match rows.next()? {
            Some(row) => Ok(Some(session_row_map(row)?)),
            None => Ok(None),
        }
    }

    /// The durable session listing, newest-updated first. This is the page
    /// routes' single read (perf-wave follow-up): `/native/sessions` serves
    /// its DTO directly from ONE call's result — the historical handler ran
    /// the manager listing AND a second listing join, reading the whole
    /// table twice per page — so the contract is exactly one statement per
    /// call. `workspace_id = Some` filters to one workspace; `None` lists
    /// every session.
    pub fn list_sessions(&self, workspace_id: Option<WorkspaceId>) -> StoreResult<Vec<SessionRow>> {
        let conn = self.read()?;
        let mut stmt = match workspace_id {
            Some(_) => conn.prepare(
                "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state, lifecycle, created_ms, updated_ms
                 FROM session WHERE workspace_id = ?1 ORDER BY updated_ms DESC",
            )?,
            None => conn.prepare(
                "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state, lifecycle, created_ms, updated_ms
                 FROM session ORDER BY updated_ms DESC",
            )?,
        };
        let mut rows = match workspace_id {
            Some(w) => stmt.query(params![w.raw() as i64])?,
            None => stmt.query([])?,
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(session_row_map(row)?);
        }
        Ok(out)
    }

    /// One bounded, cursor-paged History listing. `before` is the
    /// `(updated_ms, id)` cursor of the LAST row of the previous page; the
    /// next page is strictly older (or the same millisecond with a smaller
    /// id, matching the stable `updated_ms DESC, id DESC` order). At most
    /// `limit + 1` rows are read so the caller can report `hasMore` exactly
    /// without scanning the table. No workspace filter is applied when
    /// `workspace_id` is `None` (the global History listing).
    pub fn list_sessions_page(
        &self,
        workspace_id: Option<WorkspaceId>,
        before: Option<(i64, i64)>,
        limit: u64,
    ) -> StoreResult<Vec<SessionRow>> {
        self.list_sessions_page_at(workspace_id, before, limit, None)
    }

    /// Durably adopt a worktree/task identity (v8). The standalone session
    /// default is 1/1; WorktreeManager-created worktrees call this to make
    /// the identity durable so every later tool call (and crash recovery)
    /// carries the REAL worktree/task ids. The journal is intentionally
    /// untouched: adoption is identity bookkeeping, not a turn transition.
    pub fn adopt_session_identity(
        &self,
        id: SessionId,
        worktree_id: WorktreeId,
        task_id: TaskId,
    ) -> StoreResult<()> {
        if worktree_id.raw() == 0 || task_id.raw() == 0 {
            return Err(StoreError::Migration(
                "worktree/task ids must be non-zero".into(),
            ));
        }
        // Preparation BEFORE enqueueing: the refusal diagnostic and the
        // timestamp are captured on the caller's thread (audit item 8) — the
        // writer job executes SQL only.
        let missing_session = format!("adopt_session_identity: session {id} does not exist");
        let now = now_ms();
        self.writer.execute("adopt_session_identity", move |conn| {
            let n = conn.execute(
                "UPDATE session SET worktree_id = ?2, task_id = ?3, updated_ms = ?4 WHERE id = ?1",
                params![
                    id.raw() as i64,
                    worktree_id.raw() as i64,
                    task_id.raw() as i64,
                    now
                ],
            )?;
            if n == 0 {
                return Err(StoreError::Migration(missing_session));
            }
            Ok(())
        })
    }

    pub fn set_session_lifecycle(
        &self,
        id: SessionId,
        lifecycle: faktor_core::state::SessionLifecycle,
    ) -> StoreResult<()> {
        // In-process constructed enum prepared BEFORE enqueueing.
        let lifecycle_json = serde_json::to_string(&lifecycle).unwrap();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("set_session_lifecycle", move |conn| {
            conn.execute(
                "UPDATE session SET lifecycle = ?2, updated_ms = ?3 WHERE id = ?1",
                params![id.raw() as i64, lifecycle_json, now],
            )?;
            Ok(())
        })
    }

    pub fn set_session_state(&self, id: SessionId, state: AgentState) -> StoreResult<()> {
        // In-process constructed enum prepared BEFORE enqueueing.
        let state_json = serde_json::to_string(&state).unwrap();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("set_session_state", move |conn| {
            conn.execute(
                "UPDATE session SET state = ?2, updated_ms = ?3 WHERE id = ?1",
                params![id.raw() as i64, state_json, now],
            )?;
            Ok(())
        })
    }

    /// Single conditional lifecycle UPDATE (`WHERE lifecycle = expected`).
    /// Returns whether a row was updated. Used by prompt auto-resume
    /// (`Suspended -> Open`); the journal is intentionally untouched there —
    /// resuming on prompt is not a new event.
    pub fn set_lifecycle_if(
        &self,
        id: SessionId,
        expected: SessionLifecycle,
        new: SessionLifecycle,
    ) -> StoreResult<bool> {
        // In-process constructed enums prepared BEFORE enqueueing.
        let expected_json = serde_json::to_string(&expected).unwrap();
        let new_json = serde_json::to_string(&new).unwrap();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("set_lifecycle_if", move |conn| {
            let n = conn.execute(
                "UPDATE session SET lifecycle = ?3, updated_ms = ?4
             WHERE id = ?1 AND lifecycle = ?2",
                params![id.raw() as i64, expected_json, new_json, now],
            )?;
            Ok(n > 0)
        })
    }

    /// Durable session-title update (session.update, P1). Bumps
    /// `updated_ms` so list ordering reflects the rename. Returns whether a
    /// row was updated (callers check existence first for a clean
    /// NotFound). The journal is intentionally untouched: the title is
    /// session metadata, not a state-machine transition.
    pub fn update_session_title(&self, id: SessionId, title: &str) -> StoreResult<bool> {
        let title = title.to_owned();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("update_session_title", move |conn| {
            let n = conn.execute(
                "UPDATE session SET title = ?2, updated_ms = ?3 WHERE id = ?1",
                params![id.raw() as i64, title, now],
            )?;
            Ok(n > 0)
        })
    }

    /// ONE SQLite transaction: read the session row, verify
    /// `expected_lifecycle`/`expected_state` (mismatch -> `Conflict`, nothing
    /// written), update lifecycle+state+updated_ms, append the event with the
    /// next gapless per-session seq, commit. Returns the event seq.
    ///
    /// This is the atomic guard for lifecycle+event transitions: the session
    /// layer's `end_session`/`suspend`/`resume` call it so a crash between
    /// "update lifecycle" and "append event" can never be observed.
    pub fn transition_session(
        &self,
        session_id: SessionId,
        op_id: Option<OpId>,
        t: SessionTransition,
    ) -> StoreResult<EventSeq> {
        // Preparation BEFORE enqueueing: event payload JSON serialization and
        // the in-process enum strings the transaction writes.
        let transition_payload_json = t.event_payload.as_ref().map(|p| p.to_string());
        let state_json = serde_json::to_string(&t.new_state).unwrap();
        let new_lifecycle_json = t
            .new_lifecycle
            .as_ref()
            .map(|l| serde_json::to_string(l).unwrap());
        // Preparation BEFORE enqueueing: every refusal message. The durable
        // mismatch values are read inside the transaction, but both enums are
        // closed, so each possible diagnostic is prepared here (the closure
        // formats nothing).
        let missing_message = format!("session {session_id} does not exist; cannot transition");
        let lifecycle_mismatch = t.expected_lifecycle.map(|expected| {
            [
                SessionLifecycle::Open,
                SessionLifecycle::Suspended,
                SessionLifecycle::Closing,
                SessionLifecycle::Closed,
                SessionLifecycle::FailedPermanent,
            ]
            .into_iter()
            .map(|durable| {
                (
                    durable,
                    format!("session {session_id} lifecycle is {durable:?}, expected {expected:?}"),
                )
            })
            .collect::<Vec<_>>()
        });
        let state_mismatch = t.expected_state.map(|expected| {
            all_agent_states()
                .into_iter()
                .map(|durable| {
                    (
                        durable,
                        format!("session {session_id} state is {durable:?}, expected {expected:?}"),
                    )
                })
                .collect::<Vec<_>>()
        });
        // Preparation BEFORE enqueueing: the journal diagnostic context and
        // the timestamp are captured on the caller's thread (audit item 8) —
        // the writer job executes SQL only.
        let seq_messages = EventSeqMessages::for_session(session_id);
        let now = now_ms();
        self.writer.execute("transition_session", move |conn| {
            let tx = conn.unchecked_transaction()?;
            // (a) read the session row inside the transaction.
            let Some(row) = Self::get_session_locked(&tx, session_id)? else {
                return Err(StoreError::Conflict(missing_message));
            };
            // (b) verify the expected values; mismatch aborts with nothing written.
            if let Some(expected) = t.expected_lifecycle {
                if row.lifecycle != expected {
                    let message = lifecycle_mismatch
                        .map(|candidates| {
                            candidates
                                .into_iter()
                                .find(|(durable, _)| *durable == row.lifecycle)
                                .map(|(_, message)| message)
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    return Err(StoreError::Conflict(message));
                }
            }
            if let Some(expected) = t.expected_state {
                if row.state != expected {
                    let message = state_mismatch
                        .map(|candidates| {
                            candidates
                                .into_iter()
                                .find(|(durable, _)| *durable == row.state)
                                .map(|(_, message)| message)
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    return Err(StoreError::Conflict(message));
                }
            }
            // (c) update lifecycle+state+updated_ms.
            // In-process constructed enums prepared on the caller's thread.
            match new_lifecycle_json {
                Some(lifecycle_json) => {
                    tx.execute(
                    "UPDATE session SET lifecycle = ?2, state = ?3, updated_ms = ?4 WHERE id = ?1",
                    params![session_id.raw() as i64, lifecycle_json, state_json, now],
                )?;
                }
                None => {
                    tx.execute(
                        "UPDATE session SET state = ?2, updated_ms = ?3 WHERE id = ?1",
                        params![session_id.raw() as i64, state_json, now],
                    )?;
                }
            }
            // (d) append the event with the next gapless seq (shared insert path).
            let seq = Self::insert_event_locked(
                &tx,
                session_id,
                op_id,
                t.event_kind,
                &state_json,
                now,
                transition_payload_json,
                t.event_payload_ver,
                &seq_messages,
            )?;
            // (e) commit: lifecycle change and event are durable together.
            tx.commit()?;
            Ok(seq)
        })
    }

    // ---------------------------------------------------------------- event journal

    /// `request_permission` as ONE transaction: insert the pending permission
    /// row and append `ToolRequested` (state `WaitingForPermission`) together.
    /// Returns `(permission_id, expires_ms, event_seq)`. The permission window
    /// is stamped from the store clock exactly like the raw insert.
    pub fn insert_permission_and_event(
        &self,
        session_id: SessionId,
        op_id: OpId,
        capability: &str,
        expected_state: AgentState,
        event: CommandEvent,
    ) -> StoreResult<(i64, i64, EventSeq)> {
        let seam = Arc::clone(&self.seam);
        let capability = capability.to_owned();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        // The event payload is serialized caller-side; the one field this
        // transaction alone can know (the allocated permission id) is patched
        // by SQL after the INSERT, never by serializing on the owner thread.
        let payload_base = event.payload.as_ref().map(std::string::ToString::to_string);
        // Preparation BEFORE enqueueing: the event's landing state JSON.
        let event_state_json = serde_json::to_string(&event.state).unwrap();
        // Preparation BEFORE enqueueing: the session state expectation, all
        // refusal messages and the journal diagnostic context (the closure
        // formats nothing).
        let expectation = PreparedSessionStateExpectation::prepare(session_id, expected_state);
        let seq_messages = EventSeqMessages::for_session(session_id);
        self.writer
            .execute("insert_permission_and_event", move |conn| {
                let txn = SessionCommandTxn::begin_prepared(conn, &seam, session_id, expectation)?;
                let expires_ms = now + Self::PERMISSION_WINDOW_MS;
                let changed = txn.tx.execute(
                    "INSERT INTO permission(session_id, op_id, capability, decision, expires_ms)
             VALUES (?1, ?2, ?3, 'pending', ?4)",
                    params![
                        session_id.raw() as i64,
                        op_id.raw() as i64,
                        capability,
                        expires_ms
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::Migration(
                        "insert_permission: expected exactly one inserted row".into(),
                    ));
                }
                let id = txn.tx.last_insert_rowid();
                // The journal names the id this transaction actually allocated;
                // the caller cannot know it before the insert, so the
                // caller-prepared payload is patched by SQL (non-object
                // payloads are returned unchanged by `json_set`, exactly like
                // the former in-closure object patch).
                let payload_json = match payload_base {
                    Some(base) => Some(txn.conn().query_row(
                        "SELECT json_set(?1, '$.permission_id', ?2)",
                        params![base, id],
                        |r| r.get::<_, String>(0),
                    )?),
                    None => None,
                };
                txn.side_row_applied();
                let seq = Self::insert_event_locked(
                    txn.conn(),
                    session_id,
                    event.op_id,
                    event.kind,
                    &event_state_json,
                    event.ts_ms,
                    payload_json,
                    event.payload_ver,
                    &seq_messages,
                )?;
                txn.precommit();
                txn.commit()?;
                Ok((id, expires_ms, seq))
            })
    }

    /// `resolve_permission` as ONE transaction: terminalize an expired row /
    /// change exactly ONE pending row owned by `session_id`, append the
    /// decision event and commit. A refusal (unknown, already terminal, wrong
    /// session, expired) still commits the expiry terminalization — exactly
    /// like the raw resolver — and returns the typed `Conflict`; the decision
    /// event is never written when no row changed. The event's op id is taken
    /// from the durable permission row, so the journal always names the same
    /// operation the row belongs to.
    pub fn resolve_permission_and_event(
        &self,
        id: i64,
        session_id: SessionId,
        decision: &str,
        expected_state: AgentState,
        event: CommandEvent,
    ) -> StoreResult<EventSeq> {
        let seam = Arc::clone(&self.seam);
        let decision = decision.to_owned();
        let event_payload_json = event.payload.as_ref().map(|p| p.to_string());
        // Preparation BEFORE enqueueing: the event's landing state JSON.
        let event_state_json = serde_json::to_string(&event.state).unwrap();
        // Preparation BEFORE enqueueing: the session state expectation, all
        // refusal messages and the journal diagnostic context (the closure
        // formats nothing).
        let expectation = PreparedSessionStateExpectation::prepare(session_id, expected_state);
        let not_pending = format!("permission {id} is not pending");
        let op_id_context = format!("permission {id} op_id");
        let seq_messages = EventSeqMessages::for_session(session_id);
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer
            .execute("resolve_permission_and_event", move |conn| {
                let txn = SessionCommandTxn::begin_prepared(conn, &seam, session_id, expectation)?;
                txn.tx.execute(
                    "UPDATE permission SET decision = 'expired', resolved_ms = ?2
             WHERE id = ?1 AND decision = 'pending' AND expires_ms <= ?2",
                    params![id, now],
                )?;
                let changed = txn.tx.execute(
                    "UPDATE permission SET decision = ?2, resolved_ms = ?3
             WHERE id = ?1 AND session_id = ?4 AND decision = 'pending' AND expires_ms > ?3",
                    params![id, decision, now, session_id.raw() as i64],
                )?;
                if changed != 1 {
                    // Commit BEFORE refusing: an expired row's terminalization must
                    // survive even though the resolution itself is refused.
                    txn.commit()?;
                    return Err(StoreError::Conflict(not_pending));
                }
                let op_raw: i64 = txn.tx.query_row(
                    "SELECT op_id FROM permission WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )?;
                let op_id = id_field(&op_id_context, op_raw)?;
                txn.side_row_applied();
                let seq = Self::insert_event_locked(
                    txn.conn(),
                    session_id,
                    Some(op_id),
                    event.kind,
                    &event_state_json,
                    event.ts_ms,
                    event_payload_json,
                    event.payload_ver,
                    &seq_messages,
                )?;
                txn.precommit();
                txn.commit()?;
                Ok(seq)
            })
    }

    /// Reconcile a session's EXPIRED pending permissions as ONE transaction
    /// (crash recovery; P1-E): SELECT every still-`pending` row of the
    /// session whose durable `expires_ms <= now_ms`, terminalize ALL of them
    /// (`decision = 'expired'`, `resolved_ms = now_ms`) and append exactly
    /// ONE `PermissionExpired` journal event describing them — never a fake
    /// `PermissionDenied`. The caller supplies the pre-state it observed
    /// (re-verified inside the transaction before any write) and the landing
    /// state it computed with the SAME sibling-batch rule an explicit Deny
    /// uses; the event kind must be `PermissionExpired`, so an expiry can
    /// never be laundered through another kind.
    ///
    /// `now_ms` is the caller's clock (the session layer's injectable one),
    /// never the store wall clock, so recovery tests can drive it manually.
    ///
    /// Zero expired rows is a clean no-op: the transaction is rolled back,
    /// no row is touched, no event is appended and `event_seq` is `None` —
    /// a second recovery sweep is byte-for-byte invisible.
    pub fn expire_pending_permissions_for_session(
        &self,
        session_id: SessionId,
        now_ms: i64,
        expected_state: AgentState,
        mut event: CommandEvent,
    ) -> StoreResult<ExpiredPermissionResolution> {
        if event.kind != EventKind::PermissionExpired {
            return Err(StoreError::Migration(format!(
                "expire_pending_permissions_for_session requires a PermissionExpired \
                 event, got {:?}",
                event.kind
            )));
        }
        let seam = Arc::clone(&self.seam);
        // The event payload is serialized caller-side; the ids this sweep
        // terminalizes are known only inside the transaction, so they are
        // patched into the prepared payload by SQL (never by serializing on
        // the owner thread).
        let payload_base = event.payload.as_ref().map(std::string::ToString::to_string);
        // Preparation BEFORE enqueueing: the event's landing state JSON.
        let event_state_json = serde_json::to_string(&event.state).unwrap();
        // Preparation BEFORE enqueueing: the session state expectation, the
        // sweep's refusal diagnostics and the journal diagnostic context (the
        // closure formats nothing). The terminalized rows are discovered
        // inside the transaction, so their refusal is the static message.
        let expectation = PreparedSessionStateExpectation::prepare(session_id, expected_state);
        let expired_op_id_context = "expired pending permission op_id";
        let terminalized_mismatch = "expire_pending_permissions: terminalized row count \
                                     mismatch; refusing a partial sweep"
            .to_owned();
        let seq_messages = EventSeqMessages::for_session(session_id);
        self.writer
            .execute("expire_pending_permissions_for_session", move |conn| {
                let txn =
                    SessionCommandTxn::begin_prepared(conn, &seam, session_id, expectation)?;
                let expired: Vec<(i64, OpId)> = {
                    let mut stmt = txn.tx.prepare(
                        "SELECT id, op_id FROM permission
                 WHERE session_id = ?1 AND decision = 'pending' AND expires_ms <= ?2
                 ORDER BY id ASC",
                    )?;
                    let rows = stmt
                        .query_map(params![session_id.raw() as i64, now_ms], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    rows.into_iter()
                        .map(|(id, op_raw)| {
                            Ok((id, id_field(expired_op_id_context, op_raw)?))
                        })
                        .collect::<StoreResult<Vec<_>>>()?
                };
                if expired.is_empty() {
                    // Drop without commit: the durable world is untouched.
                    return Ok(ExpiredPermissionResolution::default());
                }
                // ONE indexed aggregate pass builds both arrays; the former
                // per-row `json_set('$[#]', ...)` chain re-parsed and
                // re-serialized the growing array once per row (O(n^2)).
                let (ids_json, ops_json): (String, String) = txn.tx.query_row(
                    "SELECT COALESCE(json_group_array(pid), '[]'),
                            COALESCE(json_group_array(opid), '[]')
                     FROM (SELECT id AS pid, op_id AS opid FROM permission
                           WHERE session_id = ?1 AND decision = 'pending' AND expires_ms <= ?2
                           ORDER BY id ASC)",
                    params![session_id.raw() as i64, now_ms],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let changed = txn.tx.execute(
                    "UPDATE permission SET decision = 'expired', resolved_ms = ?2
             WHERE session_id = ?1 AND decision = 'pending' AND expires_ms <= ?2",
                    params![session_id.raw() as i64, now_ms],
                )?;
                if changed != expired.len() {
                    return Err(StoreError::Migration(terminalized_mismatch));
                }
                // The journal names the rows this transaction actually terminalized;
                // the caller cannot know them before the SELECT under the same lock,
                // so the caller-prepared payload is patched by SQL (an existing
                // object gets both arrays replaced; a missing payload becomes the
                // object; a non-object payload is left unchanged, exactly like the
                // former in-closure patch).
                let payload_json = match payload_base {
                    Some(base) => {
                        let is_object = txn.conn().query_row(
                            "SELECT json_type(?1) = 'object'",
                            params![base],
                            |r| r.get::<_, i64>(0),
                        )? == 1;
                        if is_object {
                            Some(txn.conn().query_row(
                                "SELECT json_set(?1, '$.permission_ids', json(?2), '$.op_ids', json(?3))",
                                params![base, ids_json, ops_json],
                                |r| r.get::<_, String>(0),
                            )?)
                        } else {
                            Some(base)
                        }
                    }
                    None => Some(txn.conn().query_row(
                        "SELECT json_object('permission_ids', json(?1), 'op_ids', json(?2))",
                        params![ids_json, ops_json],
                        |r| r.get::<_, String>(0),
                    )?),
                };
                if event.op_id.is_none() && expired.len() == 1 {
                    event.op_id = Some(expired[0].1);
                }
                txn.side_row_applied();
                let seq = Self::insert_event_locked(
                    txn.conn(),
                    session_id,
                    event.op_id,
                    event.kind,
                    &event_state_json,
                    event.ts_ms,
                    payload_json,
                    event.payload_ver,
                    &seq_messages,
                )?;
                txn.precommit();
                txn.commit()?;
                Ok(ExpiredPermissionResolution {
                    expired,
                    event_seq: Some(seq),
                })
            })
    }

    /// `put_checkpoint` as ONE transaction: insert the checkpoint row (with a
    /// duplicate-sequence check inside the same transaction) and append
    /// `CheckpointCreated` together. Returns `(checkpoint_row_id, event_seq)`.
    #[allow(clippy::too_many_arguments)]
    pub fn put_checkpoint_and_event(
        &self,
        session_id: SessionId,
        sequence: i64,
        path: &str,
        before_hash: &str,
        after_hash: &str,
        after_cas_hash: Option<&str>,
        expected_state: AgentState,
        event: CommandEvent,
    ) -> StoreResult<(i64, EventSeq)> {
        let seam = Arc::clone(&self.seam);
        let path = path.to_owned();
        let before_hash = before_hash.to_owned();
        let after_hash = after_hash.to_owned();
        let after_cas_hash = after_cas_hash.map(|v| v.to_owned());
        let event_payload_json = event.payload.as_ref().map(|p| p.to_string());
        // Preparation BEFORE enqueueing: the event's landing state JSON.
        let event_state_json = serde_json::to_string(&event.state).unwrap();
        // Preparation BEFORE enqueueing: the session state expectation, all
        // refusal messages and the journal diagnostic context (the closure
        // formats nothing).
        let expectation = PreparedSessionStateExpectation::prepare(session_id, expected_state);
        let duplicate_sequence = format!("checkpoint sequence {sequence} already exists");
        let seq_messages = EventSeqMessages::for_session(session_id);
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("put_checkpoint_and_event", move |conn| {
        let txn = SessionCommandTxn::begin_prepared(conn, &seam, session_id, expectation)?;
        let duplicate: Option<i64> = txn
            .tx
            .query_row(
                "SELECT id FROM checkpoint WHERE session_id = ?1 AND sequence = ?2 LIMIT 1",
                params![session_id.raw() as i64, sequence],
                |r| r.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            return Err(StoreError::Conflict(duplicate_sequence));
        }
        let changed = txn.tx.execute(
            "INSERT INTO checkpoint(session_id, sequence, path, before_hash, after_hash, after_cas_hash, before_exists, after_exists, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id.raw() as i64,
                sequence,
                path,
                before_hash,
                after_hash,
                after_cas_hash,
                1i64,
                1i64,
                now
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Migration(
                "put_checkpoint: expected exactly one inserted row".into(),
            ));
        }
        let id = txn.tx.last_insert_rowid();
        txn.side_row_applied();
        let seq = Self::insert_event_locked(
            txn.conn(),
            session_id,
            event.op_id,
            event.kind,
            &event_state_json,
            event.ts_ms,
            event_payload_json,
            event.payload_ver,
            &seq_messages,
        )?;
        txn.precommit();
        txn.commit()?;
        Ok((id, seq))
        })
    }

    /// The CONTENT-AWARE checkpoint command: allocate the per-session
    /// sequence, insert the existence-bearing checkpoint row and append its
    /// `CheckpointCreated` event in ONE transaction, after verifying the
    /// session is still in `expected_state` (a moved session refuses typed
    /// before any write). This is the production path of the CAS-backed
    /// checkpoint store (faktor-snapshot): a row can never commit without
    /// its journal event, and two concurrent writers can never both receive
    /// the same sequence. [`Store::put_checkpoint_and_event`] remains the
    /// caller-sequenced hash-only variant.
    ///
    /// The event carries the store's canonical `CheckpointCreated` payload
    /// shape (`sequence` is the value this transaction allocated, matching
    /// the row) and is stamped with the transaction's wall clock, exactly
    /// like the row's `created_ms`. Returns
    /// `(checkpoint_row_id, allocated_sequence, event_seq)`.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_checkpoint_and_event(
        &self,
        session_id: SessionId,
        path: &str,
        before_exists: bool,
        before_hash: &str,
        after_exists: bool,
        after_hash: &str,
        after_cas_hash: Option<&str>,
        expected_state: AgentState,
    ) -> StoreResult<(i64, i64, EventSeq)> {
        let seam = Arc::clone(&self.seam);
        let path = path.to_owned();
        let before_hash = before_hash.to_owned();
        let after_hash = after_hash.to_owned();
        let after_cas_hash = after_cas_hash.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: every statically known field is
        // serialized here. The transaction-allocated `sequence` is patched
        // into this template by SQLite inside the SQL-only closure, so the
        // owner thread never runs serde.
        let checkpoint_payload_template = serde_json::json!({
            "sequence": 0,
            "path": path,
            "before_hash": before_hash,
            "after_hash": after_hash,
            "before_exists": before_exists,
            "after_exists": after_exists,
        })
        .to_string();
        // Preparation BEFORE enqueueing: the event's landing state JSON
        // (`expected_state` is both the pre-state and the event's state).
        let event_state_json = serde_json::to_string(&expected_state).unwrap();
        // Preparation BEFORE enqueueing: the session state expectation and
        // the journal diagnostic context (the closure formats nothing).
        let expectation = PreparedSessionStateExpectation::prepare(session_id, expected_state);
        let seq_messages = EventSeqMessages::for_session(session_id);
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let ts = now_ms();
        self.writer.execute("insert_checkpoint_and_event", move |conn| {
        let txn = SessionCommandTxn::begin_prepared(conn, &seam, session_id, expectation)?;
        let prev: i64 = txn.tx.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM checkpoint WHERE session_id = ?1",
            params![session_id.raw() as i64],
            |r| r.get(0),
        )?;
        let sequence = prev + 1;
        let changed = txn.tx.execute(
            "INSERT INTO checkpoint(session_id, sequence, path, before_hash, after_hash, after_cas_hash, before_exists, after_exists, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id.raw() as i64,
                sequence,
                path,
                before_hash,
                after_hash,
                after_cas_hash,
                before_exists as i64,
                after_exists as i64,
                ts
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Migration(
                "insert_checkpoint_and_event: expected exactly one inserted row".into(),
            ));
        }
        let id = txn.tx.last_insert_rowid();
        txn.side_row_applied();
        let seq = Self::insert_event_locked(
            txn.conn(),
            session_id,
            None,
            EventKind::CheckpointCreated,
            &event_state_json,
            ts,
            // `sequence` is allocated by the transaction; patch the
            // caller-prepared template with it using SQLite (SQL-only).
            Some(txn.tx.query_row(
                "SELECT json_set(?1, '$.sequence', ?2)",
                params![checkpoint_payload_template, sequence],
                |r| r.get(0),
            )?),
            1,
            &seq_messages,
        )?;
        txn.precommit();
        txn.commit()?;
        Ok((id, sequence, seq))
        })
    }

    /// Durable token spending of a session: the sum of every recorded
    /// provider-call input+output token count (NULL counters count as 0).
    /// This is the task budget's crash-safe spend source (provider_call rows
    /// are written before any gate can evaluate the budget).
    pub fn session_usage_tokens(&self, session_id: SessionId) -> StoreResult<u64> {
        let conn = self.read()?;
        let out: i64 = conn.query_row(
            "SELECT COALESCE(SUM(tokens_in), 0) + COALESCE(SUM(tokens_out), 0)
             FROM provider_call WHERE session_id = ?1",
            params![session_id.raw() as i64],
            |r| r.get(0),
        )?;
        Ok(out.max(0) as u64)
    }

    /// ALLOCATE the next per-session checkpoint sequence and insert the row
    /// in ONE transaction (P1 "checkpoint numbering race"): two concurrent
    /// writers must never both receive the same sequence. The sequence is
    /// `MAX(sequence)+1` over the session's rows, computed and inserted in
    /// ONE writer-service transaction, so allocation is atomic and gapless regardless
    /// of what any caller guessed outside the store.
    ///
    /// `before_hash`/`after_hash` carry the side's content hash, or the empty
    /// string when that side does not exist (`before_exists=false`). Returns
    /// the row id and the allocated sequence.
    ///
    /// ROW-ONLY: this path appends no journal event. Production checkpoint
    /// writes go through [`Store::insert_checkpoint_and_event`], which
    /// commits the row and its `CheckpointCreated` event in one
    /// `SessionCommandTxn`; this raw insert survives for tests that need to
    /// fabricate rows directly (corrupt/legacy fixtures).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_checkpoint(
        &self,
        session_id: SessionId,
        path: &str,
        before_exists: bool,
        before_hash: &str,
        after_exists: bool,
        after_hash: &str,
        after_cas_hash: Option<&str>,
    ) -> StoreResult<(i64, i64)> {
        let path = path.to_owned();
        let before_hash = before_hash.to_owned();
        let after_hash = after_hash.to_owned();
        let after_cas_hash = after_cas_hash.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("insert_checkpoint", move |conn| {
        let tx = conn.unchecked_transaction()?;
        let prev: i64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM checkpoint WHERE session_id = ?1",
            params![session_id.raw() as i64],
            |r| r.get(0),
        )?;
        let sequence = prev + 1;
        tx.execute(
            "INSERT INTO checkpoint(session_id, sequence, path, before_hash, after_hash, after_cas_hash, before_exists, after_exists, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id.raw() as i64,
                sequence,
                path,
                before_hash,
                after_hash,
                after_cas_hash,
                before_exists as i64,
                after_exists as i64,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok((id, sequence))
        })
    }

    /// Raw ROW-ONLY checkpoint insert at an explicit caller-chosen sequence
    /// (test fixtures that need a bare row: corrupt/legacy shapes). Both
    /// sides exist. The production content-aware path is
    /// [`Store::insert_checkpoint_and_event`] (atomic sequence allocation +
    /// `CheckpointCreated` event); the caller-sequenced journaled path is
    /// [`Store::put_checkpoint_and_event`].
    pub fn put_checkpoint(
        &self,
        session_id: SessionId,
        sequence: i64,
        path: &str,
        before_hash: &str,
        after_hash: &str,
        after_cas_hash: Option<&str>,
    ) -> StoreResult<i64> {
        let path = path.to_owned();
        let before_hash = before_hash.to_owned();
        let after_hash = after_hash.to_owned();
        let after_cas_hash = after_cas_hash.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("put_checkpoint", move |conn| {
        conn.execute(
            "INSERT INTO checkpoint(session_id, sequence, path, before_hash, after_hash, after_cas_hash, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id.raw() as i64,
                sequence,
                path,
                before_hash,
                after_hash,
                after_cas_hash,
                now
            ],
        )?;
        Ok(conn.last_insert_rowid())
        })
    }

    /// Every checkpoint of one session, oldest sequence first. This is the
    /// LISTING read; per-restore/diff lookups must use the indexed
    /// [`Store::checkpoint_by_id`] / [`Store::latest_checkpoint`] instead
    /// (a full listing per operation is O(history) and quadratic under
    /// checkpoint churn).
    pub fn checkpoints_of(&self, session_id: SessionId) -> StoreResult<Vec<CheckpointRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, sequence, path, before_hash, after_hash, after_cas_hash, before_exists, after_exists, created_ms, restored_ms
             FROM checkpoint WHERE session_id = ?1 ORDER BY sequence ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![session_id.raw() as i64], checkpoint_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// ONE checkpoint row by its durable id, scoped to the session. The
    /// primary key lookup replaces the historical "list every checkpoint and
    /// scan for the id" restore/diff path (O(n) per restore, O(n²) over a
    /// checkpoint-churn session).
    pub fn checkpoint_by_id(
        &self,
        session_id: SessionId,
        id: i64,
    ) -> StoreResult<Option<CheckpointRow>> {
        let conn = self.read()?;
        Ok(conn
            .query_row(
                "SELECT id, session_id, sequence, path, before_hash, after_hash, after_cas_hash, before_exists, after_exists, created_ms, restored_ms
                 FROM checkpoint WHERE id = ?1 AND session_id = ?2",
                params![id, session_id.raw() as i64],
                checkpoint_row,
            )
            .optional()?)
    }

    /// The NEWEST checkpoint row of one session (the `(session_id, sequence)`
    /// index serves the read; `id` breaks duplicate-sequence ties
    /// deterministically). This is what a session-detail projection and the
    /// latest-diff route need instead of materializing the whole listing.
    pub fn latest_checkpoint(&self, session_id: SessionId) -> StoreResult<Option<CheckpointRow>> {
        let conn = self.read()?;
        Ok(conn
            .query_row(
                "SELECT id, session_id, sequence, path, before_hash, after_hash, after_cas_hash, before_exists, after_exists, created_ms, restored_ms
                 FROM checkpoint WHERE session_id = ?1
                 ORDER BY sequence DESC, id DESC LIMIT 1",
                params![session_id.raw() as i64],
                checkpoint_row,
            )
            .optional()?)
    }

    /// A bounded NEWEST-first checkpoint page (the listing route's exact
    /// order). The route used to materialize and sort every checkpoint of a
    /// churned session before applying its display cap.
    pub fn checkpoints_newest(
        &self,
        session_id: SessionId,
        limit: u64,
    ) -> StoreResult<Vec<CheckpointRow>> {
        let limit = i64::try_from(limit.max(1)).map_err(|_| {
            StoreError::Oversized("checkpoint page limit exceeds SQLite signed range".into())
        })?;
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, sequence, path, before_hash, after_hash, after_cas_hash, before_exists, after_exists, created_ms, restored_ms
             FROM checkpoint WHERE session_id = ?1
             ORDER BY sequence DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id.raw() as i64, limit], checkpoint_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Redo/undo marker: rollback sets restored_ms, redo clears it (a row
    /// must not read as "restored" after an unrevert; audit round 5).
    pub fn clear_checkpoint_restored(&self, id: i64) -> StoreResult<()> {
        self.writer
            .execute("clear_checkpoint_restored", move |conn| {
                conn.execute(
                    "UPDATE checkpoint SET restored_ms = NULL WHERE id = ?1",
                    params![id],
                )?;
                Ok(())
            })
    }

    pub fn mark_checkpoint_restored(&self, id: i64) -> StoreResult<()> {
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer
            .execute("mark_checkpoint_restored", move |conn| {
                conn.execute(
                    "UPDATE checkpoint SET restored_ms = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
                Ok(())
            })
    }

    // ---------------------------------------------------------------- artifacts

    pub fn put_worktree(
        &self,
        workspace_id: WorkspaceId,
        path: &str,
        branch: &str,
    ) -> StoreResult<i64> {
        let path = path.to_owned();
        let branch = branch.to_owned();
        self.writer.execute("put_worktree", move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO worktree(workspace_id, path, branch, active)
             VALUES (?1, ?2, ?3, 1)",
                params![workspace_id.raw() as i64, path, branch],
            )?;
            conn.query_row(
                "SELECT id FROM worktree WHERE path = ?1",
                params![path],
                |r| r.get(0),
            )
            .map_err(Into::into)
        })
    }

    pub fn worktrees_of(&self, workspace_id: WorkspaceId) -> StoreResult<Vec<WorktreeRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, path, branch, active FROM worktree WHERE workspace_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![workspace_id.raw() as i64], |r| {
            Ok(WorktreeRow {
                id: r.get(0)?,
                workspace_id,
                path: r.get(2)?,
                branch: r.get(3)?,
                active: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn remove_worktree(&self, path: &str) -> StoreResult<()> {
        let path = path.to_owned();
        self.writer.execute("remove_worktree", move |conn| {
            conn.execute("DELETE FROM worktree WHERE path = ?1", params![path])?;
            Ok(())
        })
    }

    // ---------------------------------------------------------------- memory facts

    /// The durable window (ms) one permission request stays resolvable. It is
    /// written once at insert and NEVER refreshed: the live requester computes
    /// its remaining wait from this durable deadline, so a daemon restart
    /// cannot grant a fresh full window.
    pub const PERMISSION_WINDOW_MS: i64 = 60_000;

    /// Insert a pending permission and return `(id, expires_ms)`. The caller
    /// (the live requester) derives its timeout from the returned durable
    /// deadline.
    pub fn insert_permission(
        &self,
        session_id: SessionId,
        op_id: OpId,
        capability: &str,
    ) -> StoreResult<(i64, i64)> {
        let capability = capability.to_owned();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("insert_permission", move |conn| {
            let expires_ms = now + Self::PERMISSION_WINDOW_MS;
            conn.execute(
                "INSERT INTO permission(session_id, op_id, capability, decision, expires_ms)
             VALUES (?1, ?2, ?3, 'pending', ?4)",
                params![
                    session_id.raw() as i64,
                    op_id.raw() as i64,
                    capability,
                    expires_ms
                ],
            )?;
            Ok((conn.last_insert_rowid(), expires_ms))
        })
    }

    /// Resolve a pending, UNEXPIRED permission owned by `session_id`.
    ///
    /// Expiry is durable state, not a filter only some readers remember: an
    /// id whose `expires_ms` passed while still `pending` is atomically
    /// transitioned to the terminal `expired` decision (never left pending),
    /// and the resolution refuses. The update requires the session to own the
    /// row and exactly one row to change; zero rows (unknown, already
    /// terminal, wrong session, expired) is the typed
    /// `Conflict("permission {id} is not pending")`, so a losing double
    /// resolve can never journal.
    pub fn resolve_permission(
        &self,
        id: i64,
        session_id: SessionId,
        decision: &str,
    ) -> StoreResult<()> {
        let decision = decision.to_owned();
        // Preparation BEFORE enqueueing: the refusal diagnostic and the
        // timestamp are captured on the caller's thread (audit item 8) — the
        // writer job executes SQL only.
        let not_pending = format!("permission {id} is not pending");
        let now = now_ms();
        self.writer.execute("resolve_permission", move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE permission SET decision = 'expired', resolved_ms = ?2
             WHERE id = ?1 AND decision = 'pending' AND expires_ms <= ?2",
                params![id, now],
            )?;
            let changed = tx.execute(
                "UPDATE permission SET decision = ?2, resolved_ms = ?3
             WHERE id = ?1 AND session_id = ?4 AND decision = 'pending' AND expires_ms > ?3",
                params![id, decision, now, session_id.raw() as i64],
            )?;
            // Commit BEFORE refusing: an expired row's terminalization must
            // survive even though the resolution itself is refused.
            tx.commit()?;
            if changed != 1 {
                return Err(StoreError::Conflict(not_pending));
            }
            Ok(())
        })
    }

    /// Every still-`pending` permission row of `session_id` whose durable
    /// `expires_ms <= now_ms`, ascending by id — the recovery sweep's
    /// read-side view (the atomic
    /// [`Self::expire_pending_permissions_for_session`] re-derives the same
    /// set inside its transaction). Unknown sessions and sessions without
    /// expired rows both read as an empty vector.
    pub fn expired_pending_permissions(
        &self,
        session_id: SessionId,
        now_ms: i64,
    ) -> StoreResult<Vec<(i64, OpId)>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, op_id FROM permission
             WHERE session_id = ?1 AND decision = 'pending' AND expires_ms <= ?2
             ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id.raw() as i64, now_ms], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, op_raw)| Ok((id, id_field(&format!("permission {id} op_id"), op_raw)?)))
            .collect()
    }

    /// A still-resolvable pending permission: `None` for unknown, already
    /// terminal, or EXPIRED rows (the durable deadline is the filter; an
    /// expired row is terminalized by [`Self::resolve_permission`]).
    pub fn pending_permission(&self, id: i64) -> StoreResult<Option<(SessionId, OpId, String)>> {
        let conn = self.read()?;
        let raw: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT session_id, op_id, capability FROM permission
                 WHERE id = ?1 AND decision = 'pending' AND expires_ms > ?2",
                params![id, now_ms()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match raw {
            Some((session_raw, op_raw, capability)) => Ok(Some((
                id_field(&format!("permission {id} session_id"), session_raw)?,
                id_field(&format!("permission {id} op_id"), op_raw)?,
                capability,
            ))),
            None => Ok(None),
        }
    }

    /// The durable `decision` of a permission row in ANY state (`pending`,
    /// `allow`, `deny`, `expired`), for audit and tests. `None` = unknown id.
    pub fn permission_decision(&self, id: i64) -> StoreResult<Option<String>> {
        let conn = self.read()?;
        Ok(conn
            .query_row(
                "SELECT decision FROM permission WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Every `decision = 'pending'` permission row across all sessions, with
    /// the exact total and at most [`MAX_PENDING_PERMISSION_SCAN`] rows
    /// (bounded materialization for `doctor --deep`; the count stays exact).
    pub fn all_pending_permissions(&self) -> StoreResult<PendingPermissionScan> {
        let conn = self.read()?;
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM permission WHERE decision = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, op_id, expires_ms FROM permission
             WHERE decision = 'pending' ORDER BY id ASC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![MAX_PENDING_PERMISSION_SCAN as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, session_raw, op_raw, expires_ms) in rows {
            out.push(PendingPermissionRow {
                id,
                session_id: id_field(&format!("permission {id} session_id"), session_raw)?,
                op_id: id_field(&format!("permission {id} op_id"), op_raw)?,
                expires_ms,
            });
        }
        Ok(PendingPermissionScan {
            total: total.max(0) as u64,
            rows: out,
        })
    }

    /// Every session row id, ascending (`doctor --deep` orphan scans).
    pub fn session_ids(&self) -> StoreResult<Vec<SessionId>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare("SELECT id FROM session ORDER BY id ASC")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let raw: i64 = row.get(0)?;
            out.push(id_field::<SessionId>(&format!("session id {raw}"), raw)?);
        }
        Ok(out)
    }

    /// Insert-or-replace the durable child-runtime projection row of one
    /// CHILD session (migration v23): the child's state plus its blocker
    /// truth. The session layer validates the text bounds before calling.
    pub fn child_runtime_put(&self, row: &ChildRuntimeRow) -> StoreResult<()> {
        let row = row.to_owned();
        self.writer.execute("child_runtime_put", move |conn| {
            conn.execute(
                "INSERT INTO child_runtime(
                 session_id, child_id, state, blocker_kind, blocker_reason,
                 blocker_dependency, blocker_resolution, last_progress_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(session_id) DO UPDATE SET
                 child_id = excluded.child_id,
                 state = excluded.state,
                 blocker_kind = excluded.blocker_kind,
                 blocker_reason = excluded.blocker_reason,
                 blocker_dependency = excluded.blocker_dependency,
                 blocker_resolution = excluded.blocker_resolution,
                 last_progress_ms = excluded.last_progress_ms,
                 updated_ms = excluded.updated_ms",
                params![
                    row.session_id.raw() as i64,
                    row.child_id,
                    row.state,
                    row.blocker_kind,
                    row.blocker_reason,
                    row.blocker_dependency,
                    row.blocker_resolution,
                    row.last_progress_ms,
                    row.updated_ms,
                ],
            )?;
            Ok(())
        })
    }

    /// The durable child-runtime projection row of one child session.
    pub fn child_runtime_get(&self, session_id: SessionId) -> StoreResult<Option<ChildRuntimeRow>> {
        let conn = self.read()?;
        let out = conn
            .query_row(
                "SELECT session_id, child_id, state, blocker_kind, blocker_reason,
                        blocker_dependency, blocker_resolution, last_progress_ms, updated_ms
                 FROM child_runtime WHERE session_id = ?1",
                params![session_id.raw() as i64],
                |r| {
                    Ok(ChildRuntimeRow {
                        session_id,
                        child_id: r.get(1)?,
                        state: r.get(2)?,
                        blocker_kind: r.get(3)?,
                        blocker_reason: r.get(4)?,
                        blocker_dependency: r.get(5)?,
                        blocker_resolution: r.get(6)?,
                        last_progress_ms: r.get(7)?,
                        updated_ms: r.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(out)
    }

    /// Drop the child-runtime projection row (the blocker was cleared).
    pub fn child_runtime_delete(&self, session_id: SessionId) -> StoreResult<()> {
        self.writer.execute("child_runtime_delete", move |conn| {
            conn.execute(
                "DELETE FROM child_runtime WHERE session_id = ?1",
                params![session_id.raw() as i64],
            )?;
            Ok(())
        })
    }

    // ------------------------------------------------- index state machine
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "sessions_typed_ledger_tests.rs"]
mod typed_ledger_tests;
