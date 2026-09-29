//! `sessions`: cohesive slice of the mechanically decomposed parent module.

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
        self.writer.execute("create_session", move |conn| {
        conn.execute(
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
        let id: i64 = conn.last_insert_rowid();
        // Seed the journal with SessionCreated so every session starts at seq 1.
        // The session row and its seed event are one transaction.
        let tx = conn.unchecked_transaction()?;
        Self::insert_event_locked(
            &tx,
            SessionId::new(id as u64),
            None,
            EventKind::SessionCreated,
            &idle_state_json,
            now,
            Some(created_payload_json),
            1,
            &seq_messages,
        )?;
        tx.commit()?;
        Ok(
            match Self::get_session_locked(conn, SessionId::new(id as u64))? {
                Some(row) => row,
                None => {
                    return Err(StoreError::Corrupt(vec![
                        "just-created session not readable back".into(),
                    ]))
                }
            },
        )
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
                let mut ids_json = String::from("[]");
                for (pid, _) in &expired {
                    ids_json = txn.conn().query_row(
                        "SELECT json_set(?1, '$[#]', ?2)",
                        params![ids_json, *pid],
                        |r| r.get::<_, String>(0),
                    )?;
                }
                let mut ops_json = String::from("[]");
                for (_, pop) in &expired {
                    ops_json = txn.conn().query_row(
                        "SELECT json_set(?1, '$[#]', ?2)",
                        params![ops_json, pop.raw() as i64],
                        |r| r.get::<_, String>(0),
                    )?;
                }
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

    pub fn checkpoints_of(&self, session_id: SessionId) -> StoreResult<Vec<CheckpointRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, sequence, path, before_hash, after_hash, after_cas_hash, before_exists, after_exists, created_ms, restored_ms
             FROM checkpoint WHERE session_id = ?1 ORDER BY sequence ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![session_id.raw() as i64], |r| {
            Ok(CheckpointRow {
                id: r.get(0)?,
                session_id,
                sequence: r.get(2)?,
                path: r.get(3)?,
                before_hash: r.get(4)?,
                after_hash: r.get(5)?,
                after_cas_hash: r.get(6)?,
                before_exists: r.get::<_, i64>(7)? != 0,
                after_exists: r.get::<_, i64>(8)? != 0,
                created_ms: r.get(9)?,
                restored_ms: r.get(10)?,
            })
        })?;
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
mod tests {
    use super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), true).unwrap();
        (dir, s)
    }

    #[test]
    fn loop_signal_survives_reopen_and_trips_at_threshold() {
        // Spec §28 durable signals: crash between turns must NOT lose the
        // count — the third identical failure after a daemon restart still
        // trips.
        let dir = tempfile::tempdir().unwrap();
        let ws: WorkspaceId;
        let sid: SessionId;
        {
            let store = Store::open(dir.path().join("store"), true).unwrap();
            ws = store.create_workspace("/w").unwrap();
            let row = store.create_session(ws, "t", "p", "m").unwrap();
            sid = row.id;
            for i in 1..=2 {
                let tripped = store
                    .bump_loop_signal(sid, "fail run_command", 3, i * 1000)
                    .unwrap();
                assert!(!tripped, "count {i} must not trip yet");
            }
            // Reopen happens when the store drops (crash simulation).
        }
        {
            let store = Store::open(dir.path().join("store"), true).unwrap();
            assert!(
                store
                    .bump_loop_signal(sid, "fail run_command", 3, 3000)
                    .unwrap(),
                "third identical failure after a restart must trip"
            );
            // Progress clears everything.
            store.reset_loop_signals(sid).unwrap();
            assert!(store.loop_signal_counts(sid).unwrap().is_empty());
        }
    }

    #[test]
    fn checkpoints_dedup_by_hash_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            for i in 0..5 {
                store
                    .put_checkpoint(s.id, i, "a.rs", "hash-before", "hash-after", None)
                    .unwrap();
            }
            // v3: the after-blob hash roundtrips when recorded.
            store
                .put_checkpoint(s.id, 5, "b.rs", "b1", "a1", Some("cas-after-blob"))
                .unwrap();
            s.id
        };
        let store = Store::open(dir.path(), true).unwrap();
        let cps = store.checkpoints_of(session_id).unwrap();
        assert_eq!(cps.len(), 6);
        assert_eq!(cps[0].sequence, 0);
        assert_eq!(cps[4].after_hash, "hash-after");
        assert_eq!(cps[4].after_cas_hash, None);
        assert_eq!(cps[5].after_cas_hash.as_deref(), Some("cas-after-blob"));
    }

    #[test]
    fn checkpoint_sequence_allocation_is_atomic_under_concurrent_writers() {
        // P1 "checkpoint numbering race": the old flow derived the sequence
        // from rows.len()+1 OUTSIDE the store, so two racing writers could
        // both receive the same N+1. insert_checkpoint must allocate
        // MAX(sequence)+1 and insert in ONE transaction: 8 writers × 25
        // checkpoints = 200 rows, all sequences distinct and gapless.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let store = std::sync::Arc::new(store);
        let mut handles = Vec::new();
        for t in 0..8 {
            let store = store.clone();
            let sid = s.id;
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    let (id, seq) = store
                        .insert_checkpoint(
                            sid,
                            &format!("f{t}-{i}.rs"),
                            true,
                            "before-hash",
                            true,
                            "after-hash",
                            None,
                        )
                        .unwrap();
                    assert!(id > 0);
                    assert!(seq >= 1, "sequence must be >= 1, got {seq}");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let rows = store.checkpoints_of(s.id).unwrap();
        assert_eq!(rows.len(), 200, "every racing insert must land");
        let mut seqs: Vec<i64> = rows.iter().map(|c| c.sequence).collect();
        seqs.sort_unstable();
        for (i, seq) in seqs.iter().enumerate() {
            assert_eq!(
                *seq,
                (i + 1) as i64,
                "sequence {seq} at slot {i}: must be gapless"
            );
        }
        // The duplicate-guard invariant: no two rows share a sequence.
        let unique: std::collections::HashSet<i64> = seqs.iter().copied().collect();
        assert_eq!(
            unique.len(),
            200,
            "two writers must never receive the same sequence"
        );
    }

    #[test]
    fn insert_checkpoint_roundtrips_existence_flags() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        // missing -> non-empty file: the before side has NO content, so its
        // hash column is the empty-string sentinel, not a hash of nothing.
        let (id, seq) = store
            .insert_checkpoint(
                s.id,
                "created.rs",
                false,
                "",
                true,
                "after-hex",
                Some("after-blob-hex"),
            )
            .unwrap();
        assert!(id > 0);
        assert_eq!(seq, 1);
        // file -> deleted (second row): the after side does not exist.
        let (_, seq2) = store
            .insert_checkpoint(s.id, "deleted.rs", true, "before-hex", false, "", None)
            .unwrap();
        assert_eq!(seq2, 2, "allocation must continue the session sequence");
        let rows = store.checkpoints_of(s.id).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            !rows[0].before_exists,
            "missing side must read back as not existing"
        );
        assert!(rows[0].after_exists);
        assert_eq!(rows[0].before_hash, "", "no content -> no hash");
        assert_eq!(rows[0].after_hash, "after-hex");
        assert!(rows[1].before_exists);
        assert!(
            !rows[1].after_exists,
            "deleted side must read back as not existing"
        );
        assert_eq!(rows[1].after_hash, "");
        assert_eq!(rows[1].after_cas_hash, None);
    }

    #[test]
    fn permissions_resolve_once_and_expire() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let op = OpId::new(5);
        let (pid, expires_ms) = store.insert_permission(s.id, op, "execute_shell").unwrap();
        assert!(
            expires_ms > now_ms(),
            "the durable deadline must be in the future"
        );
        let pending = store.pending_permission(pid).unwrap().unwrap();
        assert_eq!(pending.0, s.id);
        assert_eq!(pending.1, op);
        assert_eq!(pending.2, "execute_shell");
        store.resolve_permission(pid, s.id, "allow").unwrap();
        assert!(store.pending_permission(pid).unwrap().is_none());
        // The PERSISTED decision is the real assertion (never comparing a
        // literal to itself): read the row back.
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("allow")
        );
        // A second resolve changes exactly zero rows: typed Conflict, and
        // the first persisted decision wins.
        let err = store
            .resolve_permission(pid, s.id, "deny")
            .expect_err("second resolve must conflict");
        assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("allow"),
            "first decision wins durably"
        );
    }

    #[test]
    fn expired_permission_is_terminal_and_refuses_resolution() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let (pid, _) = store
            .insert_permission(s.id, OpId::new(6), "execute_shell")
            .unwrap();
        // Force the durable deadline into the past (no sleeping in tests).
        store
            .raw_conn()
            .execute(
                "UPDATE permission SET expires_ms = ?2 WHERE id = ?1",
                params![pid, now_ms() - 1],
            )
            .unwrap();
        assert!(
            store.pending_permission(pid).unwrap().is_none(),
            "an expired permission is not pending"
        );
        let err = store
            .resolve_permission(pid, s.id, "allow")
            .expect_err("an expired permission cannot be resolved");
        assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("expired"),
            "expiry is an explicit terminal state, never pending"
        );
        // Terminal stays terminal: another attempt changes nothing.
        let err = store
            .resolve_permission(pid, s.id, "deny")
            .expect_err("terminal expired stays terminal");
        assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("expired")
        );
    }

    pub(crate) fn permission_expiry_event() -> CommandEvent {
        CommandEvent {
            kind: EventKind::PermissionExpired,
            state: AgentState::ReadyForNextTurn,
            op_id: None,
            ts_ms: now_ms(),
            payload: Some(serde_json::json!({
                "reason": "durable deadline elapsed while no live waiter owned the request",
                "expired_count": 1,
            })),
            payload_ver: 1,
        }
    }

    /// Push one permission row's durable deadline far into the past without
    /// sleeping (the row stays `pending`: only reconciliation terminalizes it).
    pub(crate) fn force_permission_deadline_past(store: &Store, pid: i64) {
        store
            .raw_conn()
            .execute(
                "UPDATE permission SET expires_ms = expires_ms - 1000000000 WHERE id = ?1",
                params![pid],
            )
            .unwrap();
    }

    #[test]
    fn expire_pending_permissions_terminalizes_in_one_transaction_and_is_idempotent() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let other = store.create_session(ws, "t2", "p", "m").unwrap();
        let (a, _) = store
            .insert_permission(s.id, OpId::new(11), "execute_shell")
            .unwrap();
        let (b, _) = store
            .insert_permission(s.id, OpId::new(12), "fs.write")
            .unwrap();
        let (live, _) = store
            .insert_permission(s.id, OpId::new(13), "fs.read")
            .unwrap();
        let (foreign, _) = store
            .insert_permission(other.id, OpId::new(14), "execute_shell")
            .unwrap();
        force_permission_deadline_past(&store, a);
        force_permission_deadline_past(&store, b);
        force_permission_deadline_past(&store, foreign);
        assert!(
            store
                .expired_pending_permissions(s.id, now_ms())
                .unwrap()
                .len()
                == 2
        );
        assert!(
            store
                .expired_pending_permissions(other.id, now_ms())
                .unwrap()
                .len()
                == 1
        );

        // Manual clock: the caller's now_ms is the expiry stamp.
        let now = now_ms() + 5;
        let resolution = store
            .expire_pending_permissions_for_session(
                s.id,
                now,
                AgentState::Idle,
                permission_expiry_event(),
            )
            .unwrap();
        assert_eq!(
            resolution.expired,
            vec![(a, OpId::new(11)), (b, OpId::new(12))],
            "ascending id order, both terminalized in one sweep"
        );
        assert!(resolution.event_seq.is_some());
        assert!(!resolution.is_empty());
        for (pid, want_op) in [(a, 11i64), (b, 12)] {
            let (decision, resolved_ms, op_raw): (String, Option<i64>, i64) = store
                .read()
                .unwrap()
                .query_row(
                    "SELECT decision, resolved_ms, op_id FROM permission WHERE id = ?1",
                    params![pid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(decision, "expired");
            assert_eq!(
                resolved_ms,
                Some(now),
                "resolved_ms is the caller's manual clock, not the wall clock"
            );
            assert_eq!(op_raw, want_op);
            assert!(store.pending_permission(pid).unwrap().is_none());
        }
        // An unexpired row and a foreign session's expired row are untouched.
        assert_eq!(
            store.permission_decision(live).unwrap().as_deref(),
            Some("pending")
        );
        assert!(store.pending_permission(live).unwrap().is_some());
        assert_eq!(
            store.permission_decision(foreign).unwrap().as_deref(),
            Some("pending"),
            "a foreign session's deadline is never swept here"
        );
        // The journal explains the sweep and names the exact rows it changed.
        let events = store.events_range(s.id, 1, None).unwrap();
        let expiry: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::PermissionExpired)
            .collect();
        assert_eq!(expiry.len(), 1, "one sweep, one event");
        assert_eq!(expiry[0].state, AgentState::ReadyForNextTurn);
        let payload = expiry[0].payload.as_ref().unwrap();
        assert_eq!(payload["permission_ids"], serde_json::json!([a, b]));
        assert_eq!(payload["op_ids"], serde_json::json!([11, 12]));
        assert!(payload["reason"].as_str().unwrap().contains("deadline"));
        // The committed event moved the session row with it.
        let state: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT state FROM session WHERE id = ?1",
                params![s.id.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert!(state.contains("ready_for_next_turn"), "{state}");

        // IDEMPOTENT second sweep: no rows, no event, no durable change.
        let seq_before = store.last_event_seq(s.id).unwrap().unwrap();
        let second = store
            .expire_pending_permissions_for_session(
                s.id,
                now,
                AgentState::ReadyForNextTurn,
                permission_expiry_event(),
            )
            .unwrap();
        assert!(second.is_empty());
        assert_eq!(second.event_seq, None);
        assert_eq!(store.last_event_seq(s.id).unwrap().unwrap(), seq_before);
        assert_eq!(
            store.permission_decision(a).unwrap().as_deref(),
            Some("expired"),
            "terminal stays terminal across sweeps"
        );
    }

    #[test]
    fn expire_pending_permissions_refuses_wrong_kind_and_wrong_state_without_writes() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let (pid, _) = store
            .insert_permission(s.id, OpId::new(21), "execute_shell")
            .unwrap();
        force_permission_deadline_past(&store, pid);
        let before = store.last_event_seq(s.id).unwrap().unwrap();

        // A fake Deny (or any other kind) is refused: expiry is only ever
        // journaled as PermissionExpired.
        let mut wrong_kind = permission_expiry_event();
        wrong_kind.kind = EventKind::PermissionDenied;
        let err = store
            .expire_pending_permissions_for_session(s.id, now_ms(), AgentState::Idle, wrong_kind)
            .expect_err("a non-PermissionExpired event is refused");
        assert!(matches!(err, StoreError::Migration(_)), "{err:?}");
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("pending"),
            "the refused kind changed nothing"
        );

        // A stale pre-state is refused BEFORE any write (the whole command
        // rolls back: no terminalization, no event).
        let err = store
            .expire_pending_permissions_for_session(
                s.id,
                now_ms(),
                AgentState::Suspended,
                permission_expiry_event(),
            )
            .expect_err("a mismatched pre-state refuses");
        assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("pending")
        );
        assert_eq!(store.last_event_seq(s.id).unwrap().unwrap(), before);

        // The correct command still succeeds afterwards.
        let ok = store
            .expire_pending_permissions_for_session(
                s.id,
                now_ms(),
                AgentState::Idle,
                permission_expiry_event(),
            )
            .unwrap();
        assert_eq!(ok.expired, vec![(pid, OpId::new(21))]);
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("expired")
        );
    }

    #[test]
    fn expire_pending_permissions_seams_reopen_old_or_new_only() {
        const SEAMS: [&str; 3] = [
            "session_command_side_row",
            "session_command_precommit",
            "session_command_committed",
        ];
        for seam in SEAMS {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let (pid, _) = store
                .insert_permission(s.id, OpId::new(31), "execute_shell")
                .unwrap();
            force_permission_deadline_past(&store, pid);
            store.crash_arm(CrashArm {
                point: seam,
                ordinal: 0,
            });
            // The deliberate in-transaction panic is caught by the writer
            // service (durable authority) and surfaced typed: the caller sees
            // a refusal, the daemon never panics, and the store stops
            // admitting mutations. Reopening is the recovery path.
            let err = store
                .expire_pending_permissions_for_session(
                    s.id,
                    now_ms(),
                    AgentState::Idle,
                    permission_expiry_event(),
                )
                .expect_err("seam must fire");
            assert!(matches!(err, StoreError::WriterUnavailable(_)), "{err:?}");
            assert!(!store.writer_available(), "seam {seam} must fire");
            drop(store);
            // Reopen from disk: exactly the old world or exactly the new one.
            let store = Store::open(dir.path(), true).unwrap();
            let decision = store.permission_decision(pid).unwrap();
            let expiry_events = store
                .events_range(s.id, 1, None)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == EventKind::PermissionExpired)
                .count();
            let state: String = store
                .read()
                .unwrap()
                .query_row(
                    "SELECT state FROM session WHERE id = ?1",
                    params![s.id.raw() as i64],
                    |r| r.get(0),
                )
                .unwrap();
            match seam {
                "session_command_committed" => {
                    assert_eq!(decision.as_deref(), Some("expired"), "seam {seam}");
                    assert_eq!(expiry_events, 1, "seam {seam}");
                    assert!(state.contains("ready_for_next_turn"), "{seam}: {state}");
                }
                _ => {
                    assert_eq!(
                        decision.as_deref(),
                        Some("pending"),
                        "seam {seam}: the whole command must roll back"
                    );
                    assert_eq!(expiry_events, 0, "seam {seam}");
                    assert!(state.contains("idle"), "{seam}: {state}");
                }
            }
        }
    }

    #[test]
    fn wrong_session_resolution_is_refused_and_leaves_the_row_pending() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let other = store.create_session(ws, "t2", "p", "m").unwrap();
        let (pid, _) = store
            .insert_permission(s.id, OpId::new(7), "fs.write")
            .unwrap();
        let err = store
            .resolve_permission(pid, other.id, "allow")
            .expect_err("a foreign session cannot resolve the permission");
        assert!(matches!(err, StoreError::Conflict(_)), "{err:?}");
        assert!(
            store.pending_permission(pid).unwrap().is_some(),
            "the wrong-session attempt must not consume the row"
        );
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("pending")
        );
        // The owning session still resolves it.
        store.resolve_permission(pid, s.id, "allow").unwrap();
        assert_eq!(
            store.permission_decision(pid).unwrap().as_deref(),
            Some("allow")
        );
    }

    #[test]
    fn permission_deadline_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (pid, expires_ms, s_id) = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let (pid, expires) = store
                .insert_permission(s.id, OpId::new(8), "fs.write")
                .unwrap();
            (pid, expires, s.id)
        };
        // "Restart": reopen the same data root. The durable deadline is the
        // SAME value — a restart can never extend the window.
        let store = Store::open(dir.path(), true).unwrap();
        let persisted: i64 = store
            .read()
            .unwrap()
            .query_row(
                "SELECT expires_ms FROM permission WHERE id = ?1",
                params![pid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(persisted, expires_ms, "the deadline is durable");
        if expires_ms > now_ms() {
            assert!(store.pending_permission(pid).unwrap().is_some());
            store.resolve_permission(pid, s_id, "deny").unwrap();
        } else {
            assert!(store.pending_permission(pid).unwrap().is_none());
        }
    }

    #[test]
    fn session_state_tracks_journal() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        assert_eq!(s.state, AgentState::Idle);
        store
            .append_event(
                s.id,
                None,
                EventKind::PromptReceived,
                AgentState::Preparing,
                now_ms(),
                None,
            )
            .unwrap();
        let s2 = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(s2.state, AgentState::Preparing);
        assert!(s2.updated_ms >= s.updated_ms);
    }

    #[test]
    fn worktree_crud() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let id = store.put_worktree(ws, "/w/wt1", "feat/x").unwrap();
        assert_eq!(id, store.put_worktree(ws, "/w/wt1", "feat/x").unwrap());
        let list = store.worktrees_of(ws).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].branch, "feat/x");
        store.remove_worktree("/w/wt1").unwrap();
        assert!(store.worktrees_of(ws).unwrap().is_empty());
    }

    #[test]
    fn corrupted_lifecycle_fails_closed() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        // Fresh rows hold the schema DEFAULT literal `open` (bare, non-JSON);
        // that spelling must still read back as Open, not Corrupt.
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.lifecycle, faktor_core::state::SessionLifecycle::Open);
        // (1) Not valid JSON at all: fail closed, never silently Open.
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE session SET lifecycle = 'garbage-not-json' WHERE id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_session(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("garbage lifecycle must fail closed, got {other:?}"),
        }
        // (2) Structurally valid JSON but an unknown variant: also fail
        // closed (a real Closed must not reopen as Open).
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE session SET lifecycle = '\"terminated\"' WHERE id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_session(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("unknown lifecycle variant must fail closed, got {other:?}"),
        }
        // (3) Regression: structurally valid JSON with a VALID variant still
        // parses.
        store
            .set_session_lifecycle(s.id, faktor_core::state::SessionLifecycle::Suspended)
            .unwrap();
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(
            row.lifecycle,
            faktor_core::state::SessionLifecycle::Suspended
        );
        // (4) NULL lifecycle cannot occur: the v2 schema declares the column
        // NOT NULL DEFAULT 'open', so SQLite itself rejects a NULL write;
        // parse_lifecycle never sees None. Prove the constraint holds.
        {
            let conn = store.raw_conn();
            let err = conn
                .execute(
                    "UPDATE session SET lifecycle = NULL WHERE id = ?1",
                    params![s.id.raw() as i64],
                )
                .unwrap_err();
            assert!(
                matches!(err, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::ConstraintViolation),
                "NOT NULL lifecycle must reject NULL writes, got {err:?}"
            );
        }
    }

    pub(crate) fn end_transition() -> SessionTransition {
        SessionTransition {
            expected_lifecycle: Some(SessionLifecycle::Open),
            new_lifecycle: Some(SessionLifecycle::Closed),
            expected_state: None,
            new_state: AgentState::Completed,
            event_kind: EventKind::SessionEnded,
            event_payload: None,
            event_payload_ver: 1,
        }
    }

    #[test]
    fn transition_session_commits_atomically() {
        // The crash window: the old code updated lifecycle in one transaction
        // and appended SessionEnded in a second; a crash between them left
        // Closed-without-event or event-without-Closed. One call must produce
        // BOTH, durably, and a reopen must see them together.
        let dir = tempfile::tempdir().unwrap();
        let sid = {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let seq = store
                .transition_session(s.id, None, end_transition())
                .unwrap();
            assert_eq!(seq.raw(), 2, "SessionCreated(1) + SessionEnded(2)");
            // Fresh read (same store): both sides of the transition visible.
            let row = store.get_session(s.id).unwrap().unwrap();
            assert_eq!(row.lifecycle, SessionLifecycle::Closed);
            assert_eq!(row.state, AgentState::Completed);
            let events = store.events_range(s.id, 1, None).unwrap();
            assert_eq!(events.len(), 2);
            assert_eq!(events[1].kind, EventKind::SessionEnded);
            s.id
        };
        // "Daemon restart": both persisted in the single transaction.
        let store = Store::open(dir.path(), true).unwrap();
        let row = store.get_session(sid).unwrap().unwrap();
        assert_eq!(row.lifecycle, SessionLifecycle::Closed);
        assert_eq!(row.state, AgentState::Completed);
        let events = store.events_range(sid, 1, None).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, EventKind::SessionEnded);
        assert_eq!(events[1].seq.raw(), 2);
    }

    #[test]
    fn transition_session_conflict_aborts_atomically() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .set_session_lifecycle(s.id, SessionLifecycle::Suspended)
            .unwrap();
        let err = store
            .transition_session(s.id, None, end_transition())
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict(_)),
            "expected lifecycle mismatch must be Conflict, got {err:?}"
        );
        // Rollback proven: no event row appeared and nothing moved.
        assert_eq!(store.events_range(s.id, 1, None).unwrap().len(), 1);
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.lifecycle, SessionLifecycle::Suspended);
        assert_eq!(row.state, AgentState::Idle);
    }

    #[test]
    fn concurrent_end_session_races() {
        // Two (well, eight) racers try to close one session. The writer service
        // serializes the transactions; the expected_lifecycle guard means
        // exactly ONE wins and exactly ONE SessionEnded event exists.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let store = std::sync::Arc::new(store);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            let sid = s.id;
            handles.push(std::thread::spawn(move || {
                store.transition_session(sid, None, end_transition())
            }));
        }
        let results: Vec<StoreResult<EventSeq>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wins = results.iter().filter(|r| r.is_ok()).count();
        let conflicts = results
            .iter()
            .filter(|r| matches!(r, Err(StoreError::Conflict(_))))
            .count();
        assert_eq!(wins, 1, "exactly one racer must win");
        assert_eq!(conflicts, 7, "the rest must conflict, got {results:?}");
        let events = store.events_range(s.id, 1, None).unwrap();
        let ended = events
            .iter()
            .filter(|e| e.kind == EventKind::SessionEnded)
            .count();
        assert_eq!(ended, 1, "exactly one SessionEnded event");
        assert_eq!(events.len(), 2);
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.lifecycle, SessionLifecycle::Closed);
        assert_eq!(row.state, AgentState::Completed);
    }

    #[test]
    fn gapless_seq_across_transition() {
        // A transition_session event must continue the journal sequence after
        // regular appends — the shared insert path must be the SAME path.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        for i in 0..3 {
            store
                .append_event(
                    s.id,
                    Some(OpId::new(1 + i)),
                    EventKind::ModelChunkReceived,
                    AgentState::Streaming,
                    now_ms(),
                    None,
                )
                .unwrap();
        }
        let seq = store
            .transition_session(s.id, None, end_transition())
            .unwrap();
        assert_eq!(seq.raw(), 5, "created + 3 chunks + transition = seq 5");
        let events = store.events_range(s.id, 1, None).unwrap();
        assert_eq!(events.len(), 5);
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.seq.raw(), (i + 1) as u64, "gap at {i}");
        }
        assert_eq!(events[4].kind, EventKind::SessionEnded);
    }

    #[test]
    fn set_lifecycle_if_is_conditional() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        // Auto-resume (Suspended -> Open): true and journal untouched.
        store
            .set_session_lifecycle(s.id, SessionLifecycle::Suspended)
            .unwrap();
        assert!(store
            .set_lifecycle_if(s.id, SessionLifecycle::Suspended, SessionLifecycle::Open)
            .unwrap());
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.lifecycle, SessionLifecycle::Open);
        assert_eq!(store.last_event_seq(s.id).unwrap().unwrap().raw(), 1);
        // Wrong expectation: no update, no error.
        assert!(!store
            .set_lifecycle_if(s.id, SessionLifecycle::Suspended, SessionLifecycle::Closed)
            .unwrap());
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.lifecycle, SessionLifecycle::Open);
    }

    #[test]
    fn transition_session_missing_session_conflicts() {
        let (_d, store) = tmp_store();
        let err = store
            .transition_session(SessionId::new(999), None, end_transition())
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict(_)),
            "missing session must be Conflict, got {err:?}"
        );
    }

    #[test]
    fn turn_record_lifecycle_exclusive_active_and_envelope() {
        // v7: at most ONE active logical-turn record per session; a new
        // admission finalizes stragglers; the envelope is updateable while
        // active; finish is idempotent.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "ollama", "qwen3.8").unwrap();
        let a = OpId::new(11);
        let b = OpId::new(22);
        store
            .start_turn_record(s.id, a, None, Some(2), "ollama", "qwen3.8", None)
            .unwrap();
        let rec = store.active_turn_record(s.id).unwrap().unwrap();
        assert_eq!(rec.turn_op_id, a);
        assert_eq!(rec.status, TURN_RECORD_ACTIVE);
        assert_eq!(rec.prompt_message_id, Some(2));
        // A second admission while the first is active finalizes the first.
        store
            .start_turn_record(s.id, b, Some(1), Some(5), "ollama", "m2", Some("v1"))
            .unwrap();
        let recs = store.turn_records_of(s.id).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].status, TURN_RECORD_FAILED, "straggler finalized");
        assert_eq!(recs[1].status, TURN_RECORD_ACTIVE);
        assert_eq!(recs[1].queue_seq, Some(1));
        assert_eq!(recs[1].variant.as_deref(), Some("v1"));
        assert_eq!(
            store.active_turn_record(s.id).unwrap().unwrap().turn_op_id,
            b
        );
        // Envelope update (per-message model override at drive start).
        assert!(store
            .set_turn_record_envelope(
                s.id,
                b,
                "ollama",
                "override-model",
                Some("v2"),
                Some("native")
            )
            .unwrap());
        let rec = store.turn_record_of(s.id, b).unwrap().unwrap();
        assert_eq!(rec.effective_model, "override-model");
        assert_eq!(rec.tool_mode.as_deref(), Some("native"));
        // Re-admission of the SAME op upserts the SAME record (crash between
        // claim and drive; the queue row is re-admitted) — never a phantom.
        store
            .start_turn_record(s.id, b, Some(1), Some(5), "ollama", "m3", None)
            .unwrap();
        assert_eq!(store.turn_records_of(s.id).unwrap().len(), 2);
        assert_eq!(
            store
                .turn_record_of(s.id, b)
                .unwrap()
                .unwrap()
                .effective_model,
            "m3"
        );
        // Finish transitions + idempotence.
        assert!(store
            .finish_turn_record(s.id, b, TURN_RECORD_COMPLETED)
            .unwrap());
        assert!(!store
            .finish_turn_record(s.id, b, TURN_RECORD_COMPLETED)
            .unwrap());
        assert!(store.active_turn_record(s.id).unwrap().is_none());
        assert_eq!(
            store.turn_record_of(s.id, b).unwrap().unwrap().status,
            TURN_RECORD_COMPLETED
        );
        // Invalid statuses are rejected loudly.
        assert!(store.finish_turn_record(s.id, b, "bogus").is_err());
        // Unknown op: no record.
        assert!(store.turn_record_of(s.id, OpId::new(99)).unwrap().is_none());
    }

    #[test]
    fn session_identity_defaults_to_standalone_and_adoption_is_durable() {
        // v8: sessions default to worktree 1 / task 1 (the DOCUMENTED
        // standalone identity) and adopt_identity persists the real
        // worktree/task ids durably — a reopen must read them back.
        let dir = tempfile::tempdir().unwrap();
        let (sid, ws) = {
            let store = Store::open(dir.path().join("store"), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let s = store.create_session(ws, "t", "p", "m").unwrap();
            let row = store.get_session(s.id).unwrap().unwrap();
            assert_eq!(row.worktree_id, WorktreeId::new(1), "standalone default");
            assert_eq!(row.task_id, TaskId::new(1), "standalone default");
            // Adoption moves the row off the defaults.
            store
                .adopt_session_identity(s.id, WorktreeId::new(7), TaskId::new(9))
                .unwrap();
            let row = store.get_session(s.id).unwrap().unwrap();
            assert_eq!(row.worktree_id, WorktreeId::new(7));
            assert_eq!(row.task_id, TaskId::new(9));
            // Unknown sessions are loud, not silent.
            assert!(store
                .adopt_session_identity(SessionId::new(9999), WorktreeId::new(2), TaskId::new(2))
                .is_err());
            (s.id, ws)
        };
        let store = Store::open(dir.path().join("store"), true).unwrap();
        let row = store.get_session(sid).unwrap().unwrap();
        assert_eq!(
            row.worktree_id,
            WorktreeId::new(7),
            "adoption survives reopen"
        );
        assert_eq!(row.task_id, TaskId::new(9));
        // list_sessions carries the same columns.
        let listed = store.list_sessions(Some(ws)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].worktree_id, WorktreeId::new(7));
    }

    #[test]
    fn update_session_title_roundtrip_and_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "old", "p", "m").unwrap();
        assert!(store.update_session_title(s.id, "new title").unwrap());
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.title, "new title");
        assert!(row.updated_ms >= s.updated_ms, "updated_ms must bump");
        // Unknown sessions report false (nothing updated).
        assert!(!store
            .update_session_title(SessionId::new(9999), "x")
            .unwrap());
        // Durable across reopen.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.get_session(s.id).unwrap().unwrap().title, "new title");
    }

    #[test]
    fn durable_task_repo_upsert_get_list_and_session_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s1 = store.create_session(ws, "t1", "p", "m").unwrap();
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "t2", "p", "m").unwrap();
        let mut row = TaskRow {
            task_id: TaskId::new(1),
            session_id: s1.id,
            goal: "g".into(),
            acceptance_criteria: vec![],
            plan: vec!["step one".into()],
            attachments: vec![],
            max_tokens: Some(1000),
            max_turns: Some(5),
            spent_tokens: 0,
            spent_turns: 0,
            state: TaskState::Pending,
            revision: TaskRevision::new(1),
            created_ms: 10,
            updated_ms: 10,
        };
        store.upsert_task(&row).unwrap();
        assert_eq!(
            store.get_task(s1.id, TaskId::new(1)).unwrap(),
            Some(row.clone())
        );
        assert_eq!(
            store.get_task(s2.id, TaskId::new(1)).unwrap(),
            None,
            "session scope"
        );
        assert_eq!(store.list_tasks(s1.id).unwrap(), vec![row.clone()]);
        assert!(store.list_tasks(s2.id).unwrap().is_empty());
        // Upsert REPLACES the same (session, task) row in place; the row
        // carries its (caller-maintained) revision.
        row.spent_tokens = 500;
        row.spent_turns = 2;
        row.state = TaskState::Blocked;
        row.revision = TaskRevision::new(2);
        row.updated_ms = 99;
        store.upsert_task(&row).unwrap();
        assert_eq!(
            store.list_tasks(s1.id).unwrap(),
            vec![row.clone()],
            "one row, replaced"
        );
        // P0-7 backstop: a raw row write that would MINT a completion state
        // out of a different state is refused — VerifiedComplete has no
        // machine edge and no proof can ride a generic upsert.
        for hostile_state in [
            TaskState::VerifiedComplete,
            TaskState::Verifying,
            TaskState::NeedsVerification,
        ] {
            let mut hostile = row.clone();
            hostile.state = hostile_state;
            let refused = store.upsert_task(&hostile);
            assert!(
                matches!(refused, Err(StoreError::Malformed(_))),
                "{hostile_state:?} must refuse a raw mint"
            );
        }
        assert_eq!(
            store.get_task(s1.id, TaskId::new(1)).unwrap(),
            Some(row.clone()),
            "refused writes left no trace"
        );
        // Same-state rewrite of a completion-relevant row is legal
        // (idempotent heal) and machine edges into NeedsVerification/
        // Verifying are legal (they are the transition-path writes).
        let mut in_verify = row.clone();
        in_verify.state = TaskState::Running;
        in_verify.revision = TaskRevision::new(3);
        store.upsert_task(&in_verify).unwrap();
        in_verify.state = TaskState::NeedsVerification;
        in_verify.revision = TaskRevision::new(4);
        store.upsert_task(&in_verify).unwrap();
        in_verify.state = TaskState::Verifying;
        in_verify.revision = TaskRevision::new(5);
        store.upsert_task(&in_verify).unwrap();
        in_verify.revision = TaskRevision::new(6);
        store.upsert_task(&in_verify).unwrap();
        assert_eq!(
            store
                .get_task(s1.id, TaskId::new(1))
                .unwrap()
                .unwrap()
                .state,
            TaskState::Verifying
        );
        // A second task of the SAME session lists oldest-first alongside it.
        let mut other = row.clone();
        other.task_id = TaskId::new(2);
        other.created_ms = 5;
        other.updated_ms = 5;
        other.state = TaskState::Running;
        store.upsert_task(&other).unwrap();
        let listed = store.list_tasks(s1.id).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].task_id, TaskId::new(2), "oldest-created first");
        // Corrupt state strings fail closed as Corrupt, never a panic.
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE task SET state = 'garbage' WHERE session_id = ?1 AND task_id = ?2",
                params![s1.id.raw() as i64, 2],
            )
            .unwrap();
        }
        match store.get_task(s1.id, TaskId::new(2)) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt task state must error, not parse: {other:?}"),
        }
        assert!(
            matches!(store.list_tasks(s1.id), Err(StoreError::Corrupt(_))),
            "corrupt rows surface as Corrupt, never a silent skip"
        );
    }

    pub(crate) fn prefix_hash(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    #[test]
    fn stored_stability_aggregate_math_and_empty_sessions() {
        // (d) The additive aggregate query: mean + population std dev over
        // the rows that carry a recorded stability, and None on sessions
        // with nothing recorded.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        // No observations at all.
        assert_eq!(store.session_stored_prefix_stability(s.id).unwrap(), None);
        // Legacy rows (no prefix data) do not count either.
        store
            .record_provider_call(s.id, OpId::new(1), "p", "m", "ok", None, None, None)
            .unwrap();
        assert_eq!(store.session_stored_prefix_stability(s.id).unwrap(), None);
        // Recorded stabilities: 0.5, 0.5, 1.0 -> mean 2/3, σ ≈ 0.2357.
        for (i, st) in [0.5f64, 0.5, 1.0].iter().enumerate() {
            store
                .record_provider_call_with_prefix(
                    s.id,
                    OpId::new(2 + i as u64),
                    "p",
                    "m",
                    "ok",
                    None,
                    None,
                    None,
                    Some(prefix_hash(i as u8 + 3)),
                    Some(100),
                    Some(*st),
                )
                .unwrap();
        }
        // Rows with a hash but NULL stability contribute nothing.
        store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(9),
                "p",
                "m",
                "ok",
                None,
                None,
                None,
                Some(prefix_hash(9)),
                Some(50),
                None,
            )
            .unwrap();
        let agg = store
            .session_stored_prefix_stability(s.id)
            .unwrap()
            .unwrap();
        assert_eq!(agg.observations, 3);
        assert!((agg.mean - 2.0 / 3.0).abs() < 1e-12);
        let want_std = ((0.5f64 - 2.0 / 3.0).powi(2) * 2.0 + (1.0f64 - 2.0 / 3.0).powi(2)) / 3.0;
        assert!((agg.std_dev - want_std.sqrt()).abs() < 1e-12);
        // Isolation: the second session still has nothing.
        let s2 = store.create_session(ws, "t2", "p", "m").unwrap();
        assert_eq!(store.session_stored_prefix_stability(s2.id).unwrap(), None);
    }

    #[test]
    fn all_running_tool_rows_and_active_turns_scan_every_session() {
        // Deep doctor queries are GLOBAL: running tool runs and active turn
        // records from two sessions must both surface.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s1 = store.create_session(ws, "a", "p", "m").unwrap();
        let s2 = store.create_session(ws, "b", "p", "m").unwrap();
        store
            .start_tool_run(
                s1.id,
                OpId::new(11),
                "echo",
                serde_json::json!({}),
                serde_json::json!({"strategy": "idempotent"}),
                None,
                None,
            )
            .unwrap();
        store
            .start_tool_run(
                s2.id,
                OpId::new(22),
                "write_file",
                serde_json::json!({"path": "/x"}),
                serde_json::json!({"strategy": "verify_hash"}),
                Some("ab".repeat(32)),
                None,
            )
            .unwrap();
        // One finished run must NOT appear.
        store
            .start_tool_run(
                s1.id,
                OpId::new(33),
                "echo",
                serde_json::json!({}),
                serde_json::json!({"strategy": "none"}),
                None,
                None,
            )
            .unwrap();
        store
            .finish_tool_run(s1.id, OpId::new(33), "completed", "applied")
            .unwrap();
        store
            .start_turn_record(s1.id, OpId::new(101), None, Some(2), "p", "m", None)
            .unwrap();
        store
            .start_turn_record(s2.id, OpId::new(202), None, Some(2), "p", "m", Some("v1"))
            .unwrap();
        // A second, then finished, record on s1 must not appear as active.
        store
            .finish_turn_record(s1.id, OpId::new(101), TURN_RECORD_FAILED)
            .unwrap();
        store
            .start_turn_record(s1.id, OpId::new(303), None, Some(2), "p", "m", None)
            .unwrap();
        store
            .finish_turn_record(s1.id, OpId::new(303), TURN_RECORD_COMPLETED)
            .unwrap();
        let running = store.all_running_tool_rows().unwrap();
        assert_eq!(running.len(), 2, "both sessions' running rows surface");
        assert!(running
            .iter()
            .any(|r| r.session_id == s1.id && r.op_id == OpId::new(11)));
        assert!(running
            .iter()
            .any(|r| r.session_id == s2.id && r.op_id == OpId::new(22)));
        let active = store.all_active_turns().unwrap();
        assert_eq!(active.len(), 1, "only session 2's turn is still active");
        assert_eq!(active[0].turn_op_id, OpId::new(202));
    }

    #[test]
    fn journal_consistency_flags_gaps_and_torn_sessions() {
        // A gapless journal stays clean; a deleted middle event and a
        // session row without a journal are both flagged.
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        for i in 0..3 {
            store
                .append_event(
                    s.id,
                    Some(OpId::new(1 + i)),
                    EventKind::ModelChunkReceived,
                    AgentState::Streaming,
                    now_ms(),
                    None,
                )
                .unwrap();
        }
        assert!(store.journal_consistency_issues().unwrap().is_empty());
        // Torn session: raw session row with no journal (bypasses the API).
        {
            let conn = store.raw_conn();
            conn.execute(
                "INSERT INTO session(workspace_id, title, provider, model, state, lifecycle, created_ms, updated_ms)
                 VALUES (?1, 'torn', 'p', 'm', '\"idle\"', 'open', 0, 0)",
                params![ws.raw() as i64],
            )
            .unwrap();
        }
        let issues = store.journal_consistency_issues().unwrap();
        assert_eq!(issues.len(), 1, "one torn session");
        assert!(issues[0].contains("no events"));
        // A gap: delete the middle event of the healthy session.
        {
            let conn = store.raw_conn();
            conn.execute(
                "DELETE FROM event WHERE session_id = ?1 AND seq = 2",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        let issues = store.journal_consistency_issues().unwrap();
        let gap = issues
            .iter()
            .find(|i| i.contains(&format!("session {}", s.id.raw())))
            .expect("gap must be flagged");
        assert!(gap.contains("1..=3"), "gap issue: {gap}");
    }

    pub(crate) fn hot_session(store: &Store) -> SessionId {
        let ws = store.create_workspace("/w").unwrap();
        store.create_session(ws, "t", "p", "m").unwrap().id
    }

    pub(crate) fn hot_session_sids(store: &Store, n: u64) -> Vec<SessionId> {
        let ws = store.create_workspace("/w").unwrap();
        (1..=n)
            .map(|_| store.create_session(ws, "t", "p", "m").unwrap().id)
            .collect()
    }

    #[test]
    fn batch_hot_writes_events_stay_gapless_across_grouped_sessions() {
        // Two sessions journaling in one group: per-session MAX(seq) is
        // computed inside the shared transaction, so seqs stay gapless per
        // session even when the groups interleave.
        let (_d, store) = tmp_store();
        let sids = hot_session_sids(&store, 2);
        let writes = sids
            .iter()
            .enumerate()
            .flat_map(|(i, sid)| {
                vec![
                    HotWrite::AppendEvent {
                        session_id: *sid,
                        op_id: None,
                        kind: EventKind::PromptReceived,
                        state: AgentState::Preparing,
                        ts_ms: 10 + i as i64,
                        payload: None,
                        payload_ver: 1,
                    },
                    HotWrite::AppendEvent {
                        session_id: *sid,
                        op_id: None,
                        kind: EventKind::ContextPrepared,
                        state: AgentState::BuildingContext,
                        ts_ms: 20 + i as i64,
                        payload: None,
                        payload_ver: 1,
                    },
                ]
            })
            .collect::<Vec<_>>();
        let (out, _) = store.batch_hot_writes(&writes).unwrap();
        assert!(out.iter().all(|r| r.is_ok()));
        for sid in &sids {
            let seqs: Vec<u64> = store
                .events_range(*sid, 1, None)
                .unwrap()
                .into_iter()
                .map(|e| e.seq.raw())
                .collect();
            assert_eq!(seqs, vec![1, 2, 3], "seed + two grouped events, gapless");
        }
        // Replay accepts the interleaved journal (state machine coherent).
        for sid in &sids {
            let events = store.events_range(*sid, 1, None).unwrap();
            let state = events.last().unwrap().state;
            assert_eq!(state, AgentState::BuildingContext);
        }
    }

    // ------------------------------------------------- WAL maintenance tests

    /// `(frames in WAL, frames backfilled into the main file)` WITHOUT doing
    /// any checkpoint work: `NOOP` only reports the WAL status, so it
    /// observes exactly what the last real checkpoint left behind.
    pub(crate) fn wal_backfill(store: &Store) -> (i64, i64) {
        store
            .read()
            .unwrap()
            .query_row("PRAGMA wal_checkpoint(NOOP)", [], |r| {
                Ok((r.get(1)?, r.get(2)?))
            })
            .unwrap()
    }

    #[test]
    fn autocheckpoint_is_disabled_and_passive_checkpoint_folds_the_wal() {
        // The actor's 5 ms gate can only hold if SQLite never checkpoints
        // inside a committing statement: configure must disable it on every
        // connection, and the explicit PASSIVE checkpoint must fold the
        // frames it leaves behind.
        let (_dir, store) = tmp_store();
        let auto: i64 = store
            .read()
            .unwrap()
            .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
            .unwrap();
        assert_eq!(auto, 0, "wal_autocheckpoint must be disabled");
        let sid = hot_session(&store);
        for seq in 1..=200i64 {
            store
                .put_message(sid, seq, "assistant", serde_json::json!({ "i": seq }))
                .unwrap();
        }
        assert!(
            wal_backfill(&store).0 > 0,
            "autocheckpoint is off: the frames must still be in the WAL"
        );
        assert_eq!(wal_backfill(&store).1, 0, "SQLite folded nothing by itself");
        store.wal_checkpoint_passive().unwrap();
        let (log, backfill) = wal_backfill(&store);
        assert_eq!(
            backfill, log,
            "explicit PASSIVE folded every frame ({backfill}/{log})"
        );
        // The fold is not a data loss: rows stay readable through the store.
        assert_eq!(store.message_count(sid).unwrap(), 200);
    }

    #[test]
    fn passive_checkpoint_never_waits_for_a_pinned_reader_and_folds_after() {
        // Maintenance must be bounded: PASSIVE never blocks on a reader that
        // pins a snapshot. It folds what it safely can (frames after the
        // pinned read mark stay in the WAL), the pinned reader keeps its
        // snapshot, and a later checkpoint (after release) folds the rest.
        let (_dir, store) = tmp_store();
        let sid = hot_session(&store);
        for seq in 1..=64i64 {
            store
                .put_message(sid, seq, "assistant", serde_json::json!({ "i": seq }))
                .unwrap();
        }
        let reader = store.read().unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let n: i64 = reader
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 64, "reader snapshot starts at 64 rows");
        for seq in 65..=128i64 {
            store
                .put_message(sid, seq, "assistant", serde_json::json!({ "i": seq }))
                .unwrap();
        }
        let (log, before) = wal_backfill(&store);
        assert!(before < log, "frames are waiting to be folded");
        let t0 = Instant::now();
        store.wal_checkpoint_passive().unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "PASSIVE must not wait for the pinned reader (took {elapsed:?})"
        );
        // The reader's snapshot is untouched by the checkpoint.
        let n: i64 = reader
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 64, "pinned snapshot unchanged");
        let (log, backfill) = wal_backfill(&store);
        assert!(
            backfill > 0 && backfill < log,
            "PASSIVE folded only up to the pinned read mark ({backfill}/{log})"
        );
        // Release the snapshot explicitly: the pooled connection survives, so
        // an open read transaction would keep pinning the WAL in the pool.
        reader.execute_batch("COMMIT").unwrap();
        drop(reader);
        store.wal_checkpoint_passive().unwrap();
        let (log, backfill) = wal_backfill(&store);
        assert_eq!(
            backfill, log,
            "after release PASSIVE folds the rest ({backfill}/{log})"
        );
        assert_eq!(store.message_count(sid).unwrap(), 128);
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

    pub(crate) fn sid(store: &Store) -> SessionId {
        let ws = store.create_workspace("/w").unwrap();
        store.create_session(ws, "t", "p", "m").unwrap().id
    }

    #[test]
    fn appends_never_rewind_below_the_head_checkpoint() {
        // Post-compaction appends keep allocating ABOVE the checkpoint seq
        // (the session fold cursor must never rewind over pruned rows).
        let (_d, store) = tmp_store();
        let s = sid(&store);
        store
            .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g"}))
            .unwrap();
        store
            .put_ledger_head(s, serde_json::json!({"schema_ver": 1}), 1, 1)
            .unwrap();
        let deleted = store
            .compact_ledger(s, 2, &[], serde_json::json!({"schema_ver": 1}), 1, 1)
            .unwrap();
        assert_eq!(deleted, 1);
        let seq = store
            .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g2"}))
            .unwrap();
        assert!(
            seq > 1,
            "new seq must sit above the pruned checkpoint, got {seq}"
        );
    }

    #[test]
    fn corrupt_queue_row_json_is_refused_typed_never_defaulted() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .enqueue_prompt(
                s.id,
                OpId::new(1),
                "hi",
                &["a.rs".into()],
                None,
                None,
                None,
                1,
            )
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE prompt_queue SET files = 'not json' WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.queue_head(s.id) {
            Err(StoreError::Corrupt(msgs)) => assert!(
                msgs.iter().any(|m| m.contains("files")),
                "the refusal must name the files column: {msgs:?}"
            ),
            Err(e) => panic!("corrupt queue files must be a typed Corrupt error, got {e:?}"),
            Ok(_) => panic!("corrupt queue files must not silently decode or vanish"),
        }
        match store.admit_queue_head(s.id, &["idle"], "preparing") {
            Err(StoreError::Corrupt(msgs)) => assert!(
                msgs.iter().any(|m| m.contains("files")),
                "the refusal must name the files column: {msgs:?}"
            ),
            other => panic!("admission of a corrupt row must refuse typed: {other:?}"),
        }
        let status: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT status FROM prompt_queue WHERE session_id = ?1",
                params![s.id.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "pending",
            "a refused admission must not claim the row"
        );
        assert!(
            store.messages_before(s.id, None, 10).unwrap().is_empty(),
            "a refused admission must not materialize the user message"
        );
    }

    /// Assert a read path refused a structurally invalid `op_id` as a typed
    /// `Corrupt` naming the column and the failure reason.
    pub(crate) fn assert_corrupt_op_id<T>(outcome: StoreResult<T>, what: &str, needle: &str) {
        match outcome {
            Err(StoreError::Corrupt(msgs)) => assert!(
                msgs.iter()
                    .any(|m| m.contains("op_id") && m.contains(needle)),
                "{what}: the refusal must name op_id and {needle:?}: {msgs:?}"
            ),
            Err(e) => panic!("{what}: a corrupt op_id must refuse typed, got {e}"),
            Ok(_) => panic!("{what}: a corrupt op_id must refuse typed, got a decoded value"),
        }
    }

    /// A persisted `prompt_queue.op_id` that is zero can never have been
    /// written by the typed API: every read path must refuse it as a typed
    /// `Corrupt` naming the row and column — never panic (`OpId::new(0)`) or
    /// mint `1`. Negative raw values are the bit-cast upper half and decode
    /// back to the exact u64 op id.
    #[test]
    fn corrupt_queue_op_id_is_refused_typed_never_panics_or_mints() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .enqueue_prompt(s.id, OpId::new(7), "first", &[], None, None, None, 1)
            .unwrap();
        store
            .enqueue_prompt(s.id, OpId::new(9), "second", &[], None, None, None, 2)
            .unwrap();

        // Valid rows round-trip before corruption.
        let head = store.queue_head(s.id).unwrap().unwrap();
        assert_eq!((head.queue_seq, head.op_id), (1, OpId::new(7)));
        assert_eq!(
            store.queue_op_ids(s.id).unwrap(),
            vec![OpId::new(7), OpId::new(9)]
        );

        // Zero is structurally invalid: every read path refuses it typed and
        // admission changes nothing.
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE prompt_queue SET op_id = 0 WHERE session_id = ?1 AND seq = 1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        assert_corrupt_op_id(store.queue_head(s.id), "queue_head", "cannot be 0");
        assert_corrupt_op_id(store.queue_op_ids(s.id), "queue_op_ids", "cannot be 0");
        assert_corrupt_op_id(
            store.admit_queue_head(s.id, &["idle"], "preparing"),
            "admit_queue_head",
            "cannot be 0",
        );
        let status: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT status FROM prompt_queue WHERE session_id = ?1 AND seq = 1",
                params![s.id.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "pending",
            "a refused admission must not claim the row"
        );
        assert!(
            store.messages_before(s.id, None, 10).unwrap().is_empty(),
            "a refused admission must not materialize the user message"
        );
        assert_eq!(
            store.get_session(s.id).unwrap().unwrap().state,
            AgentState::Idle,
            "a refused admission must not move the session"
        );

        // Negative raw values are the bit-cast upper half: the read paths
        // decode them back to the exact u64 op ids instead of refusing.
        for wanted in [u64::MAX, u64::MAX - 2, 2u64.pow(63)] {
            let bad = wanted as i64;
            {
                let conn = store.raw_conn();
                conn.execute(
                    "UPDATE prompt_queue SET op_id = ?2 WHERE session_id = ?1 AND seq = 1",
                    params![s.id.raw() as i64, bad],
                )
                .unwrap();
            }
            assert_eq!(
                store.queue_head(s.id).unwrap().unwrap().op_id.raw(),
                wanted,
                "queue_head op_id {wanted} must round-trip exactly"
            );
            assert_eq!(
                store.queue_op_ids(s.id).unwrap()[0].raw(),
                wanted,
                "queue_op_ids {wanted} must round-trip exactly"
            );
        }

        // Repair the row: the head still admits exactly once and the queue
        // then advances to the next row (the fix never wedges the FIFO).
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE prompt_queue SET op_id = ?2 WHERE session_id = ?1 AND seq = 1",
                params![s.id.raw() as i64, 7i64],
            )
            .unwrap();
        }
        let (admitted, _) = store
            .admit_queue_head(s.id, &["idle"], "preparing")
            .unwrap()
            .unwrap();
        assert_eq!((admitted.queue_seq, admitted.op_id), (1, OpId::new(7)));
        store.mark_queue_status(s.id, 1, "completed").unwrap();
        let next = store.queue_head(s.id).unwrap().unwrap();
        assert_eq!(
            (next.queue_seq, next.op_id),
            (2, OpId::new(9)),
            "the queue head advances to the next row"
        );
    }

    /// Crash-residue recovery of the durable queue (adversarial): a
    /// `claimed` row returns to pending (re-admitted later); a `running` row
    /// whose logical turn already ended (no active turn record) is retired to
    /// `done` — re-admitting it would deliver the prompt twice; a `running`
    /// row OWNED by an active turn record is left running for the queue
    /// runner to resume and consume exactly once; and `abort(None)` can
    /// cancel every non-terminal row, running rows included.
    #[test]
    fn queue_recovery_classifies_running_rows_by_turn_record_ownership() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        // Row 1: admitted but never driven (claimed) — crash between claim
        // and the first drive.
        store
            .enqueue_prompt(s.id, OpId::new(11), "one", &[], None, None, None, 1)
            .unwrap();
        // Row 2: admitted, drive started (running) and its turn record is
        // ACTIVE — the recoverable interrupted turn.
        store
            .enqueue_prompt(s.id, OpId::new(12), "two", &[], None, None, None, 2)
            .unwrap();
        // Row 3: running with NO active turn record — the turn ended but the
        // terminal mark was lost. Rows 2 and 3 both run in one pass.
        store
            .enqueue_prompt(s.id, OpId::new(13), "three", &[], None, None, None, 3)
            .unwrap();
        store
            .raw_conn()
            .execute(
                "UPDATE prompt_queue SET status = CASE seq
                     WHEN 1 THEN 'claimed'
                     WHEN 2 THEN 'running'
                     ELSE 'running' END
                 WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        store
            .start_turn_record(s.id, OpId::new(12), Some(2), None, "p", "m", None)
            .unwrap();

        let reclaimed = store.recover_claimed_queue_rows(s.id).unwrap();
        assert_eq!(reclaimed, 1, "the claimed row returns to pending");
        let statuses: Vec<(i64, String)> = {
            let conn = store.read().unwrap();
            let mut stmt = conn
                .prepare("SELECT seq, status FROM prompt_queue WHERE session_id = ?1 ORDER BY seq")
                .unwrap();
            let rows = stmt
                .query_map(params![s.id.raw() as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(
            statuses,
            vec![
                (1, "pending".to_string()),
                (2, "running".to_string()),
                (3, "done".to_string()),
            ],
            "claimed -> pending; owned running stays; orphaned running is retired"
        );
        // The runnable marker still lists the session (rows 1/2 remain).
        assert_eq!(
            store.sessions_with_pending_queues().unwrap(),
            vec![s.id],
            "the runnable marker keeps listing the session"
        );
        // Idempotent: a second recovery changes nothing.
        assert_eq!(store.recover_claimed_queue_rows(s.id).unwrap(), 0);
        // abort(None) semantics: every non-terminal row (pending AND
        // running) is cancellable through the op-id scan.
        let ops = store.queue_op_ids(s.id).unwrap();
        assert_eq!(
            ops,
            vec![OpId::new(11), OpId::new(12)],
            "queue_op_ids covers pending and running rows"
        );
        assert_eq!(store.cancel_queued_ops(s.id, &ops).unwrap(), 2);
        assert!(store.sessions_with_pending_queues().unwrap().is_empty());
        assert!(store.queue_op_ids(s.id).unwrap().is_empty());
    }

    #[test]
    fn corrupt_session_state_refuses_admission_typed_without_claiming() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .enqueue_prompt(s.id, OpId::new(1), "hi", &[], None, None, None, 1)
            .unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE session SET state = 'not json' WHERE id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.admit_queue_head(s.id, &["idle"], "preparing") {
            Err(StoreError::Corrupt(msgs)) => assert!(
                msgs.iter().any(|m| m.contains("state")),
                "the refusal must name the state column: {msgs:?}"
            ),
            other => panic!("corrupt session state must refuse admission typed: {other:?}"),
        }
        let status: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT status FROM prompt_queue WHERE session_id = ?1",
                params![s.id.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "pending",
            "a refused admission must not claim the row"
        );
    }

    fn pending_queue_row_with_files(store: &Store, files_json: &str) -> SessionId {
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .enqueue_prompt(s.id, OpId::new(1), "hi", &[], None, None, None, 1)
            .unwrap();
        store
            .raw_conn()
            .execute(
                "UPDATE prompt_queue SET files = ?2 WHERE session_id = ?1 AND seq = 1",
                params![s.id.raw() as i64, files_json],
            )
            .unwrap();
        s.id
    }

    /// The `files` column is a string array or it is corrupt: JSON1's
    /// `json_each.value` dequotes text elements, so `json_type(value)` parses
    /// `src/lib.rs` as JSON and the old predicate raised "malformed JSON" for
    /// EVERY non-empty list (and silently passed `[null]`). The admission
    /// validation must classify on `json_each.type`, accept exactly the empty
    /// array and string arrays, and refuse everything else typed with zero
    /// mutation.
    #[test]
    fn queue_admission_files_column_accepts_only_json_string_arrays() {
        for (files_json, accepted) in [
            ("[]", Some(Vec::<String>::new())),
            (r#"["src/lib.rs"]"#, Some(vec!["src/lib.rs".to_string()])),
            (r#"["a","b"]"#, Some(vec!["a".to_string(), "b".to_string()])),
            ("[1]", None),
            ("[null]", None),
            (r#"{"x":"a"}"#, None),
            ("not-json", None),
        ] {
            let (_d, store) = tmp_store();
            let s = pending_queue_row_with_files(&store, files_json);
            match (store.admit_queue_head(s, &["idle"], "preparing"), accepted) {
                (Ok(Some((admitted, _))), Some(expected)) => {
                    assert_eq!(
                        admitted.files, expected,
                        "files {files_json} must round-trip"
                    );
                    let status: String = store
                        .read()
                        .unwrap()
                        .query_row(
                            "SELECT status FROM prompt_queue WHERE session_id = ?1",
                            params![s.raw() as i64],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert_eq!(status, "claimed");
                    assert_eq!(
                        store.messages_before(s, None, 10).unwrap().len(),
                        1,
                        "exactly one user message per admitted prompt"
                    );
                    assert!(
                        store
                            .admit_queue_head(s, &["idle"], "preparing")
                            .unwrap()
                            .is_none(),
                        "a re-admit must be a no-op: the head was claimed"
                    );
                    assert_eq!(store.messages_before(s, None, 10).unwrap().len(), 1);
                }
                (Err(StoreError::Corrupt(msgs)), None) => {
                    assert!(
                        msgs.iter().any(|m| m.contains("files")),
                        "{files_json}: the refusal must name the files column: {msgs:?}"
                    );
                    assert_admission_untouched(&store, s);
                    assert_eq!(
                        store.get_session(s).unwrap().unwrap().state,
                        AgentState::Idle,
                        "a refused admission must not move the session"
                    );
                    assert!(
                        store.admit_queue_head(s, &["idle"], "preparing").is_err(),
                        "the refusal is deterministic: the corrupt row is still refused"
                    );
                }
                (other, _) => panic!("files {files_json} classified {other:?}"),
            }
        }
    }

    /// Assert a refused admission left the durable world exactly as it was:
    /// the head is still pending and no message was materialized.
    fn assert_admission_untouched(store: &Store, s: SessionId) {
        let status: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT status FROM prompt_queue WHERE session_id = ?1 ORDER BY seq LIMIT 1",
                params![s.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending", "a refused admission must not claim");
        assert!(
            store.messages_before(s, None, 10).unwrap().is_empty(),
            "a refused admission must not materialize a message"
        );
    }

    /// The ineligible branch decodes the durable session text as a TYPED
    /// `AgentState`: `"zombie"` is well-formed JSON but not a state, so it
    /// must surface `Corrupt` (never a silent `Ok(None)` "not eligible"),
    /// with zero queue/message/session mutation. A valid but genuinely
    /// ineligible state is still the intended `Ok(None)`.
    #[test]
    fn admit_queue_head_decodes_durable_session_state_typed() {
        for (corrupt_state, needle) in [("\"zombie\"", "state"), ("not-json", "state")] {
            let (_d, store) = tmp_store();
            let s = pending_queue_row_with_files(&store, "[]");
            store
                .raw_conn()
                .execute(
                    "UPDATE session SET state = ?2 WHERE id = ?1",
                    params![s.raw() as i64, corrupt_state],
                )
                .unwrap();
            match store.admit_queue_head(s, &["idle"], "preparing") {
                Err(StoreError::Corrupt(msgs)) => assert!(
                    msgs.iter().any(|m| m.contains(needle)),
                    "{corrupt_state}: the refusal must name the state column: {msgs:?}"
                ),
                other => panic!("state {corrupt_state} must refuse typed, got {other:?}"),
            }
            let status: String = store
                .read()
                .unwrap()
                .query_row(
                    "SELECT status FROM prompt_queue WHERE session_id = ?1",
                    params![s.raw() as i64],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(status, "pending");
            assert!(store.messages_before(s, None, 10).unwrap().is_empty());
            let durable: String = store
                .read()
                .unwrap()
                .query_row(
                    "SELECT state FROM session WHERE id = ?1",
                    params![s.raw() as i64],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(durable, corrupt_state, "the corrupt row is never rewritten");
        }
        let (_d, store) = tmp_store();
        let s = pending_queue_row_with_files(&store, "[]");
        store
            .raw_conn()
            .execute(
                "UPDATE session SET state = '\"completed\"' WHERE id = ?1",
                params![s.raw() as i64],
            )
            .unwrap();
        assert!(
            store
                .admit_queue_head(s, &["idle"], "preparing")
                .unwrap()
                .is_none(),
            "a valid ineligible state is a clean no-op"
        );
        assert_admission_untouched(&store, s);
        assert_eq!(
            store.get_session(s).unwrap().unwrap().state,
            AgentState::Completed,
            "a valid ineligible state is never rewritten"
        );
    }

    /// End-to-end queued-files case (the regression that was missing): while
    /// a turn is active, a prompt with a real file list queues; the store's
    /// public admission path (exactly the call `SessionHandle::
    /// admit_next_queued` makes) then returns the SAME files, claims the row
    /// and materializes the user message exactly once, and a re-admit is a
    /// no-op.
    #[test]
    fn queued_prompt_with_files_admits_exactly_once_after_turn() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        for (from, to, kind) in [
            (
                AgentState::Idle,
                AgentState::Preparing,
                EventKind::PhaseChanged,
            ),
            (
                AgentState::Preparing,
                AgentState::BuildingContext,
                EventKind::PhaseChanged,
            ),
            (
                AgentState::BuildingContext,
                AgentState::WaitingForModel,
                EventKind::PhaseChanged,
            ),
            (
                AgentState::WaitingForModel,
                AgentState::Streaming,
                EventKind::PhaseChanged,
            ),
        ] {
            store
                .transition_session(
                    s.id,
                    Some(OpId::new(9)),
                    SessionTransition {
                        expected_lifecycle: None,
                        new_lifecycle: None,
                        expected_state: Some(from),
                        new_state: to,
                        event_kind: kind,
                        event_payload: None,
                        event_payload_ver: 1,
                    },
                )
                .unwrap();
        }
        store
            .enqueue_prompt(
                s.id,
                OpId::new(10),
                "queued with files",
                &["src/lib.rs".to_string()],
                None,
                None,
                None,
                7,
            )
            .unwrap();
        let handle_eligible = [
            "idle",
            "ready_for_next_turn",
            "cancelled",
            "failed_recoverable",
        ];
        assert!(
            store
                .admit_queue_head(s.id, &handle_eligible, "preparing")
                .unwrap()
                .is_none(),
            "a prompt queued during a streaming turn must not admit mid-turn"
        );
        assert_eq!(
            store.queue_status_counts(s.id).unwrap()["pending"].as_i64(),
            Some(1)
        );
        assert!(store.messages_before(s.id, None, 10).unwrap().is_empty());
        store
            .transition_session(
                s.id,
                Some(OpId::new(9)),
                SessionTransition {
                    expected_lifecycle: None,
                    new_lifecycle: None,
                    expected_state: Some(AgentState::Streaming),
                    new_state: AgentState::ReadyForNextTurn,
                    event_kind: EventKind::TurnCompleted,
                    event_payload: None,
                    event_payload_ver: 1,
                },
            )
            .unwrap();
        let (admitted, message_seq) = store
            .admit_queue_head(s.id, &handle_eligible, "preparing")
            .unwrap()
            .expect("the queued prompt must admit once the turn ended");
        assert_eq!(admitted.op_id, OpId::new(10));
        assert_eq!(admitted.queue_seq, 1);
        assert_eq!(admitted.prompt, "queued with files");
        assert_eq!(admitted.files, vec!["src/lib.rs".to_string()]);
        assert_eq!(message_seq, admitted.message_seq);
        assert_eq!(
            store.get_session(s.id).unwrap().unwrap().state,
            AgentState::Preparing
        );
        assert_eq!(
            store.queue_status_counts(s.id).unwrap()["claimed"].as_i64(),
            Some(1)
        );
        let messages = store.messages_before(s.id, None, 10).unwrap();
        assert_eq!(messages.len(), 1, "materialized exactly once");
        assert_eq!(messages[0].seq, admitted.message_seq);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].data["text"], "queued with files");
        assert!(
            store
                .admit_queue_head(s.id, &handle_eligible, "preparing")
                .unwrap()
                .is_none(),
            "re-admit must be a no-op: no pending head remains"
        );
        assert_eq!(store.messages_before(s.id, None, 10).unwrap().len(), 1);
        assert_eq!(
            store.queue_status_counts(s.id).unwrap()["claimed"].as_i64(),
            Some(1)
        );
    }

    // ------------------------------------------- v17 attempt-identity ledger
    // (schema v18): refund-after-dispatch is SQL-impossible, reservations and
    // provider-call rows key by attempt_op_id, delivery/cost-basis columns
    // ride the row, crash recovery splits reserved-vs-dispatched.
}
