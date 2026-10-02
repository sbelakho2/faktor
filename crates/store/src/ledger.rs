//! `ledger`: cohesive slice of the mechanically decomposed parent module.

use super::*;

/// One memory-fact row including its durable `updated_ms` (the paging
/// order key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFactRow {
    pub kind: String,
    pub key: String,
    pub value: String,
    pub updated_ms: i64,
}

/// Cursor over the memory-fact total order `(updated_ms DESC, kind DESC,
/// key DESC)`: the `(updated_ms, kind, key)` position of the last row of a
/// page. Identifies a stable position — replaying it returns the same
/// window.
pub type MemoryFactCursor = (i64, String, String);

/// Verification depth of [`Store::journal_session_problems`]. `Open` keeps
/// session open bounded (point reads plus ONE covering-index aggregate);
/// `Deep` adds the adjacent-inversion timestamp aggregate the doctor sweep
/// runs. Both NEVER materialize the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalDepth {
    Open,
    Deep,
}

/// One typed, versioned row of the durable session ledger (audits 27,
/// 71-72; schema v11). The journal is the source of truth for *what
/// happened*; `ledger_entry` is the rich append-only typed ledger and
/// `ledger_head` its materialized checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntryRow {
    /// Per-session gapless entry sequence.
    pub seq: i64,
    /// Entry-type tag, e.g. `goal_set` / `blocker_opened` (snake_case).
    pub entry_type: String,
    /// Payload schema version of THIS row (schema v1 this wave).
    pub schema_ver: i64,
    /// Decoded JSON payload. Never opaque text: the session layer decodes
    /// it strictly by (entry_type, schema_ver) — an unknown version or a
    /// shape violation is a loud error, never a silent parse.
    pub payload: serde_json::Value,
    pub created_ms: i64,
}

/// The materialized head checkpoint of the typed ledger (v11): the folded
/// projection (`head_json`) of every entry up to `checkpoint_seq`, written
/// by compaction together with the entry deletions it summarizes. A crash
/// between an entry append and its head checkpoint is repaired on the next
/// open by folding the newer entries onto the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerHeadRow {
    pub head_json: serde_json::Value,
    pub checkpoint_seq: i64,
    pub schema_ver: i64,
    pub updated_ms: i64,
}

/// One raw `memory_fact` row of a kind doctor's orphan-child scan watches
/// (`orchestrator` identity rows in the child's row space and
/// `orchestrator_registry` rows in the parent's row space). Values stay
/// opaque here: parsing belongs to the session/orchestrator layer that owns
/// each JSON shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFactRowRef {
    /// The session whose row space the fact lives in.
    pub session_id: SessionId,
    pub kind: String,
    pub key: String,
    pub value: String,
}

pub struct QueuedPrompt {
    pub queue_seq: i64,
    pub op_id: OpId,
    pub prompt: String,
    pub files: Vec<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub agent: Option<String>,
    pub status: String,
    pub requested_at: i64,
}

/// Result of the atomic claim/admission of the queue head.
#[derive(Debug, Clone)]
pub struct AdmittedPrompt {
    pub queue_seq: i64,
    pub op_id: OpId,
    pub prompt: String,
    pub files: Vec<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub agent: Option<String>,
    /// Message seq of the materialized user message (== the admission
    /// journal event seq).
    pub message_seq: i64,
}

/// One durable evidence row (schema v21; the audit's evidence table): the
/// evidence IDENTITY the scoped evidence store reads back. `id` is the
/// GLOBALLY UNIQUE `EvidenceId` — `INTEGER PRIMARY KEY AUTOINCREMENT`, so a
/// daemon restart can never mint an id it already handed out (SQLite's
/// `sqlite_sequence` keeps the high-water mark across closed connections
/// even when a row were deleted). Scope columns (`session_id`,
/// `workspace_id`, `task_id`) are what the scoped read compares; `kind`,
/// `revision`, `provenance`, `compressibility`, `compression`, `retrieval`,
/// `compact`, `backing_cas_hash`, `completeness` and `created_ms` are the
/// envelope's own fields, JSON-encoded exactly as the evidence crate
/// serializes them (protocol-agnostic TEXT; parsed fallibly on read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRow {
    /// The globally unique evidence id; `0` on a fresh row means "assign the
    /// next id" for [`Store::evidence_insert`].
    pub id: u64,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub task_id: Option<u64>,
    pub kind: String,
    pub revision: i64,
    pub provenance_json: String,
    pub compressibility: String,
    pub compression_json: String,
    pub retrieval_json: String,
    pub compact_json: String,
    pub backing_cas_hash: Option<String>,
    pub completeness: String,
    pub created_ms: i64,
}

pub(crate) fn evidence_row_map(r: &rusqlite::Row<'_>) -> StoreResult<EvidenceRow> {
    let id_raw: i64 = r.get(0)?;
    if id_raw < 1 {
        return Err(StoreError::Corrupt(vec![format!(
            "evidence id {id_raw} is below 1"
        )]));
    }
    let id = id_raw as u64;
    let revision: i64 = r.get(5)?;
    if revision < 1 {
        return Err(StoreError::Corrupt(vec![format!(
            "evidence {id} revision {revision} is below 1"
        )]));
    }
    Ok(EvidenceRow {
        id,
        session_id: id_field(&format!("evidence {id} session_id"), r.get::<_, i64>(1)?)?,
        workspace_id: id_field(&format!("evidence {id} workspace_id"), r.get::<_, i64>(2)?)?,
        // Optional task id: the writer bit-casts u64 into the signed column,
        // so the inverse cast round-trips the full range (None stays None).
        task_id: r.get::<_, Option<i64>>(3)?.map(|t| t as u64),
        kind: r.get(4)?,
        revision,
        provenance_json: r.get(6)?,
        compressibility: r.get(7)?,
        compression_json: r.get(8)?,
        retrieval_json: r.get(9)?,
        compact_json: r.get(10)?,
        backing_cas_hash: r.get(11)?,
        completeness: r.get(12)?,
        created_ms: r.get(13)?,
    })
}

/// The seed of a freshly migrated op-id sequence: `(now_ms << 20)` rounded
/// UP to the 1024-id reservation quantum.
///
/// The 20-bit shift keeps every pre-migration id (`now_ms + counter`, where
/// the counter only ever grew from 1) far below the seed — by a factor of
/// ~2^20 in wall-clock terms, i.e. even a clock that had run ~56 million
/// years ahead before a regression cannot have minted ids at or above the
/// seed. Rounded up to 1024 so the manager's first reservation starts on a
/// quantum boundary.
pub(crate) fn op_id_seq_seed() -> i64 {
    const QUANTUM: u64 = 1024;
    let now = u64::try_from(now_ms()).unwrap_or(0);
    let base = now.saturating_mul(1 << 20);
    (base.saturating_add(QUANTUM - 1) & !(QUANTUM - 1)).min(i64::MAX as u64) as i64
}

pub(crate) fn kind_name(k: EventKind) -> &'static str {
    match k {
        EventKind::SessionCreated => "session_created",
        EventKind::PromptReceived => "prompt_received",
        EventKind::ContextPrepared => "context_prepared",
        EventKind::ModelStarted => "model_started",
        EventKind::ModelChunkReceived => "model_chunk_received",
        EventKind::ToolRequested => "tool_requested",
        EventKind::ToolStarted => "tool_started",
        EventKind::FileChanged => "file_changed",
        EventKind::ToolCompleted => "tool_completed",
        EventKind::ToolCancelled => "tool_cancelled",
        EventKind::CheckpointCreated => "checkpoint_created",
        EventKind::ContextCompacted => "context_compacted",
        EventKind::CompactRejected => "compact_rejected",
        EventKind::SubagentStarted => "subagent_started",
        EventKind::SubagentCompleted => "subagent_completed",
        EventKind::TurnCompleted => "turn_completed",
        EventKind::PermissionGranted => "permission_granted",
        EventKind::PermissionDenied => "permission_denied",
        EventKind::PermissionExpired => "permission_expired",
        EventKind::PhaseChanged => "phase_changed",
        EventKind::ReplayStarted => "replay_started",
        EventKind::PromptAdmitted => "prompt_admitted",
        EventKind::CrashDetected => "crash_detected",
        EventKind::RecoveryApplied => "recovery_applied",
        EventKind::SessionEnded => "session_ended",
        EventKind::Suspended => "suspended",
        EventKind::Resumed => "resumed",
        EventKind::Failed => "failed",
    }
}

pub(crate) fn kind_from_name(name: &str) -> Option<EventKind> {
    Some(match name {
        "session_created" => EventKind::SessionCreated,
        "prompt_received" => EventKind::PromptReceived,
        "context_prepared" => EventKind::ContextPrepared,
        "model_started" => EventKind::ModelStarted,
        "model_chunk_received" => EventKind::ModelChunkReceived,
        "tool_requested" => EventKind::ToolRequested,
        "tool_started" => EventKind::ToolStarted,
        "file_changed" => EventKind::FileChanged,
        "tool_completed" => EventKind::ToolCompleted,
        "tool_cancelled" => EventKind::ToolCancelled,
        "checkpoint_created" => EventKind::CheckpointCreated,
        "context_compacted" => EventKind::ContextCompacted,
        "compact_rejected" => EventKind::CompactRejected,
        "subagent_started" => EventKind::SubagentStarted,
        "subagent_completed" => EventKind::SubagentCompleted,
        "turn_completed" => EventKind::TurnCompleted,
        "permission_granted" => EventKind::PermissionGranted,
        "permission_denied" => EventKind::PermissionDenied,
        "permission_expired" => EventKind::PermissionExpired,
        "phase_changed" => EventKind::PhaseChanged,
        "replay_started" => EventKind::ReplayStarted,
        "prompt_admitted" => EventKind::PromptAdmitted,
        "crash_detected" => EventKind::CrashDetected,
        "recovery_applied" => EventKind::RecoveryApplied,
        "session_ended" => EventKind::SessionEnded,
        "suspended" => EventKind::Suspended,
        "resumed" => EventKind::Resumed,
        "failed" => EventKind::Failed,
        _ => return None,
    })
}

/// One typed ledger row mapper (v11).
pub(crate) fn ledger_entry_map(
    r: &rusqlite::Row<'_>,
    session_id: SessionId,
) -> StoreResult<LedgerEntryRow> {
    let seq: i64 = r.get(0)?;
    Ok(LedgerEntryRow {
        seq,
        entry_type: r.get(1)?,
        schema_ver: r.get(2)?,
        payload: parse_json(
            &format!("ledger entry {session_id}/{seq} payload"),
            &r.get::<_, String>(3)?,
        )?,
        created_ms: r.get(4)?,
    })
}

pub(crate) fn event_map(r: &rusqlite::Row<'_>, session_id: SessionId) -> StoreResult<(Event, i64)> {
    let seq = id_field::<EventSeq>(&format!("event {session_id} seq"), r.get::<_, i64>(0)?)?;
    let kind_raw = r.get::<_, String>(3)?;
    let kind = kind_from_name(&kind_raw).ok_or_else(|| {
        StoreError::Corrupt(vec![format!(
            "event {session_id}/{seq}: unknown kind {kind_raw:?}"
        )])
    })?;
    Ok((
        Event {
            seq,
            session_id,
            op_id: id_field_opt(
                &format!("event {session_id}/{seq} op_id"),
                r.get::<_, Option<i64>>(2)?,
            )?,
            kind,
            state: parse_json(
                &format!("event {session_id}/{seq} state"),
                &r.get::<_, String>(4)?,
            )?,
            ts_ms: r.get(5)?,
            payload: match r.get::<_, Option<String>>(6)? {
                Some(raw) => Some(parse_json(
                    &format!("event {session_id}/{seq} payload"),
                    &raw,
                )?),
                None => None,
            },
        },
        // The payload schema version tag (v11+; 1 on rows written before
        // versioning existed). Readers decode through it.
        r.get::<_, i64>(7)?,
    ))
}

#[cfg(test)]
mod evidence_store_tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), true).unwrap();
        (dir, s)
    }

    fn row(session: SessionId, workspace: WorkspaceId, compact: &str) -> EvidenceRow {
        EvidenceRow {
            id: 0,
            session_id: session,
            workspace_id: workspace,
            task_id: Some(7),
            kind: "process_log".into(),
            revision: 1,
            provenance_json: r#"{"entries":["tool"]}"#.into(),
            compressibility: "aggressive".into(),
            compression_json: r#"{"algorithm":"identity"}"#.into(),
            retrieval_json: r#"{"allow_ranges":true,"allow_search":true,"max_bytes":64}"#.into(),
            compact_json: compact.into(),
            backing_cas_hash: Some("ab".repeat(32)),
            completeness: "complete".into(),
            created_ms: 1234,
        }
    }

    #[test]
    fn evidence_ids_are_globally_unique_across_reopen_and_never_reissued() {
        let dir = tempfile::tempdir().unwrap();
        let first: u64;
        let second: u64;
        {
            let store = Store::open(dir.path(), true).unwrap();
            let ws = store.create_workspace("/w").unwrap();
            let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
            first = store.evidence_insert(&row(sid, ws, "first")).unwrap();
            second = store.evidence_insert(&row(sid, ws, "second")).unwrap();
            assert!(first >= 1 && second > first, "ids are monotonic");
            assert_eq!(store.evidence_high_water().unwrap(), second);
        }
        {
            // "Daemon restart": a fresh opener must keep minting ABOVE every
            // id ever issued, and the original ids must still resolve.
            let store = Store::open(dir.path(), true).unwrap();
            let ws = WorkspaceId::new(1);
            let sid = SessionId::new(1);
            let reopened = store.evidence_get(first).unwrap().expect("id survives");
            assert_eq!(reopened.compact_json, "first");
            let third = store.evidence_insert(&row(sid, ws, "third")).unwrap();
            assert!(
                third > second,
                "reopen must never reissue {second}: got {third}"
            );
            assert_eq!(store.evidence_high_water().unwrap(), third);
            // A hostile explicit insert of an EXISTING id is refused, never
            // an overwrite (the original envelope bytes stay).
            let mut dup = row(sid, ws, "overwrite attempt");
            dup.id = first;
            match store.evidence_insert(&dup) {
                Err(StoreError::Conflict(_)) => {}
                other => panic!("duplicate id must conflict, got {other:?}"),
            }
            assert_eq!(
                store.evidence_get(first).unwrap().unwrap().compact_json,
                "first"
            );
        }
    }

    #[test]
    fn evidence_hostile_negative_ids_and_sequence_refuse_typed() {
        let (_d, store) = tmp();
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        let id = store.evidence_insert(&row(sid, ws, "hostile")).unwrap();
        let conn = store.raw_conn();
        // `-1` is the two's-complement form of an id above i64::MAX: an
        // out-of-domain id smuggled past the insert boundary.
        let changed = conn
            .execute(
                "UPDATE evidence SET id = -1 WHERE id = ?1",
                params![id as i64],
            )
            .unwrap();
        assert_eq!(changed, 1);
        assert!(matches!(
            store.evidence_ids_by_backing(&"ab".repeat(32), 10),
            Err(StoreError::Corrupt(_))
        ));
        assert!(matches!(
            store.evidence_high_water(),
            Err(StoreError::Corrupt(_))
        ));
        // A negative `sqlite_sequence` row is the same class of poison.
        conn.execute(
            "UPDATE evidence SET id = ?1 WHERE id = -1",
            params![id as i64],
        )
        .unwrap();
        conn.execute(
            "UPDATE sqlite_sequence SET seq = -1 WHERE name = 'evidence'",
            [],
        )
        .unwrap();
        assert!(matches!(
            store.evidence_high_water(),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn evidence_scope_listing_and_backing_digest_index_are_bounded_and_scoped() {
        let (_d, store) = tmp();
        let ws_a = store.create_workspace("/a").unwrap();
        let ws_b = store.create_workspace("/b").unwrap();
        let sid_a = store.create_session(ws_a, "a", "p", "m").unwrap().id;
        let sid_b = store.create_session(ws_b, "b", "p", "m").unwrap().id;
        for i in 0..5 {
            store
                .evidence_insert(&row(sid_a, ws_a, &format!("a-{i}")))
                .unwrap();
        }
        store.evidence_insert(&row(sid_b, ws_b, "b-0")).unwrap();

        let a = store.evidence_list_by_scope(sid_a, ws_a, 100).unwrap();
        assert_eq!(a.len(), 5);
        assert!(a
            .iter()
            .all(|r| r.session_id == sid_a && r.workspace_id == ws_a));
        let b = store.evidence_list_by_scope(sid_b, ws_b, 100).unwrap();
        assert_eq!(b.len(), 1);
        // A scope with no rows lists nothing, never leaks another session.
        assert!(store
            .evidence_list_by_scope(sid_a, ws_b, 100)
            .unwrap()
            .is_empty());
        // `limit` is enforced; and the backing index lists every envelope
        // referencing the digest (all six share `ab`*32) without granting
        // anything by itself.
        assert_eq!(
            store.evidence_list_by_scope(sid_a, ws_a, 2).unwrap().len(),
            2
        );
        let ids = store
            .evidence_ids_by_backing(&"ab".repeat(32), 100)
            .unwrap();
        assert_eq!(ids.len(), 6);
        assert!(store
            .evidence_ids_by_backing(&"cd".repeat(32), 100)
            .unwrap()
            .is_empty());
        // Corrupt revision refuses on read (a row injected behind the API).
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE evidence SET revision = 0 WHERE id = ?1",
            params![ids[0] as i64],
        )
        .unwrap();
        drop(conn);
        match store.evidence_get(ids[0]) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("revision 0 must be corrupt, got {other:?}"),
        }
    }

    #[test]
    fn migration_v21_replays_cleanly_on_a_v20_store() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path(), true).unwrap();
            store.create_workspace("/w").unwrap();
            store
                .create_session(WorkspaceId::new(1), "t", "p", "m")
                .unwrap();
        }
        {
            // Rewind to v20: drop the v21 table + indexes and reset the
            // version cursor exactly as a pre-v21 build left the file.
            let mut conn = Connection::open(dir.path().join("faktor-plus.db")).unwrap();
            configure(&conn).unwrap();
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_evidence_scope;
                 DROP INDEX IF EXISTS idx_evidence_session_task;
                 DROP INDEX IF EXISTS idx_evidence_backing_cas;
                 DROP TABLE IF EXISTS evidence;
                 ALTER TABLE task DROP COLUMN attachments;
                 DROP TABLE IF EXISTS attachment;
                 PRAGMA user_version = 21;",
            )
            .unwrap();
            conn.execute("DELETE FROM sqlite_sequence WHERE name = 'evidence'", [])
                .unwrap();
            migrate(&mut conn).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                version, 29,
                "v28 (per-session artifact identity) is the migration head"
            );
            let ws_ok: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='evidence'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ws_ok, 1);
            for table in [
                "verification_attempt",
                "verification_attempt_changed_file",
                "verification_job",
                "verification_job_result",
            ] {
                let present: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                        params![table],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(present, 1, "v22 table {table} must exist after the replay");
            }
            drop(conn);
        }
        // The migrated store serves the full evidence surface.
        let store = Store::open(dir.path(), true).unwrap();
        let ws = WorkspaceId::new(1);
        let sid = SessionId::new(1);
        let id = store
            .evidence_insert(&row(sid, ws, "post-migrate"))
            .unwrap();
        assert_eq!(
            store.evidence_get(id).unwrap().unwrap().compact_json,
            "post-migrate"
        );
    }
}

/// Raw (undecoded) result of one queue-admission writer job (audit finding
/// 5): row text and integer values only. The typed [`AdmittedPrompt`] is
/// decoded by [`Store::admit_queue_head`] on the caller's thread after the
/// job returns; the writer owner never parses JSON.
struct RawAdmittedPrompt {
    queue_seq: i64,
    op_id: OpId,
    prompt: String,
    files_json: String,
    model: Option<String>,
    variant: Option<String>,
    agent: Option<String>,
    message_seq: i64,
}

/// Raw outcome of one queue-admission writer job. `Ineligible` carries the
/// raw state/files text so the caller can preserve the former decode
/// contract (a corrupt column surfaces typed; nothing was claimed).
enum RawAdmitOutcome {
    MissingSession,
    Ineligible {
        state_json: String,
        files_json: String,
    },
    Claimed(RawAdmittedPrompt),
}

/// The caller-prepared journal diagnostics of one session's event append:
/// the `MAX(seq)` decode context and the seq-overflow context. Prepared on
/// the caller's thread (audit item 8) so the insert helper stays
/// `format!`-free and the writer owner never formats or allocates a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EventSeqMessages {
    /// Decode context for a hostile `MAX(seq)` raw (`id_field` names it).
    pub(crate) seq_corrupt_message: String,
    /// Returned verbatim when the next seq leaves the `u64` or `i64` domain.
    pub(crate) seq_overflow_message: String,
}

impl EventSeqMessages {
    pub(crate) fn for_session(session_id: SessionId) -> Self {
        Self {
            seq_corrupt_message: format!("event journal of session {session_id} MAX(seq)"),
            seq_overflow_message: format!(
                "event journal of session {session_id} event seq overflow"
            ),
        }
    }

    /// The seed append of a brand-new session: the id is SQLite-allocated
    /// inside the transaction, so both diagnostics stay session-agnostic
    /// static text.
    pub(crate) fn for_new_session() -> Self {
        Self {
            seq_corrupt_message: "event journal of a new session MAX(seq)".to_owned(),
            seq_overflow_message: "event journal of a new session event seq overflow".to_owned(),
        }
    }
}

impl Store {
    /// Append an event with the next per-session sequence number, atomically.
    /// Duplicate/gap sequences are impossible under the transaction; the
    /// primary key enforces it structurally. Legacy callers keep this
    /// signature and stamp payload schema v1 (the original unversioned
    /// writers); version-aware writers use [`Store::append_event_v`].
    pub fn append_event(
        &self,
        session_id: SessionId,
        op_id: Option<OpId>,
        kind: EventKind,
        state: AgentState,
        ts_ms: i64,
        payload: Option<serde_json::Value>,
    ) -> StoreResult<EventSeq> {
        self.append_event_v(session_id, op_id, kind, state, ts_ms, payload, 1)
    }

    /// Versioned append (v11+): stamps `payload_ver`, the payload schema
    /// version of `payload`. Every append site stamps the writer's current
    /// payload schema so a reader can refuse an unknown version loudly
    /// instead of misreading a future shape.
    #[allow(clippy::too_many_arguments)]
    pub fn append_event_v(
        &self,
        session_id: SessionId,
        op_id: Option<OpId>,
        kind: EventKind,
        state: AgentState,
        ts_ms: i64,
        payload: Option<serde_json::Value>,
        payload_ver: i64,
    ) -> StoreResult<EventSeq> {
        let seam = Arc::clone(&self.seam);
        // Preparation BEFORE enqueueing: the event payload and its landing
        // state JSON are serialized on the caller's thread.
        let payload_json = payload.map(|p| p.to_string());
        let state_json = serde_json::to_string(&state).unwrap();
        // Preparation BEFORE enqueueing: the journal diagnostic context.
        let seq_messages = EventSeqMessages::for_session(session_id);
        self.writer.execute("append_event_v", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let seq = Self::insert_event_locked(
                &tx,
                session_id,
                op_id,
                kind,
                &state_json,
                ts_ms,
                payload_json,
                payload_ver,
                &seq_messages,
            )?;
            // Durability boundary: crossing `ev_precommit` fires the crash
            // AFTER the insert executed but BEFORE the COMMIT (the append
            // rolls back); crossing `ev_committed` fires right after the
            // COMMIT returned (the append is durable, the ack was lost).
            seam.trip("ev_precommit");
            tx.commit()?;
            seam.trip("ev_committed");
            Ok(seq)
        })
    }

    /// The shared gapless-seq event insert path. Runs inside the CALLER'S
    /// transaction so `append_event`, `transition_session` and the actor's
    /// [`Store::batch_hot_writes`] groups are one atomic unit (a nested
    /// transaction here would silently demote to a savepoint). The argument
    /// is a bare `&Connection`: a live `rusqlite::Transaction` derefs to one,
    /// and the actor batch passes its outer transaction connection directly.
    /// `state_json` is the caller-prepared serialization of the event's
    /// landing state and `seq_messages` the caller-prepared diagnostic
    /// contexts — the writer owner never runs serde or formatting.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_event_locked(
        conn: &Connection,
        session_id: SessionId,
        op_id: Option<OpId>,
        kind: EventKind,
        state_json: &str,
        ts_ms: i64,
        payload_json: Option<String>,
        payload_ver: i64,
        seq_messages: &EventSeqMessages,
    ) -> StoreResult<EventSeq> {
        // Serialize appends per session so seq computation is race-free.
        // (The single-owner writer service already serializes every command;
        // the per-session query is a second belt.) ONE fallible latest-event
        // read serves BOTH the previous seq and the previous ts: any
        // decode/DB failure is loud corruption, never a silent `prev_ts = 0`
        // that would mask a break in the non-decreasing guarantee.
        let latest: Option<(i64, i64)> = conn
            .query_row(
                "SELECT seq, ts_ms FROM event WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![session_id.raw() as i64],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        let (prev_seq, prev_ts) = match latest {
            Some((raw_seq, prev_ts)) => (
                Some(id_field::<EventSeq>(
                    &seq_messages.seq_corrupt_message,
                    raw_seq,
                )?),
                Some(prev_ts),
            ),
            None => (None, None),
        };
        // Durable input never panics and never wraps: `checked_next_seq`
        // guards the u64 domain, `i64::try_from` the SQLite signed column.
        // Either failure is the caller-prepared overflow corruption, refused
        // before any row is written.
        let seq = JournalInvariants::checked_next_seq(prev_seq)
            .ok_or_else(|| StoreError::Corrupt(vec![seq_messages.seq_overflow_message.clone()]))?;
        let seq_i64 = i64::try_from(seq.raw())
            .map_err(|_| StoreError::Corrupt(vec![seq_messages.seq_overflow_message.clone()]))?;
        let ts = JournalInvariants::monotonic_ts(prev_ts, ts_ms);
        conn.execute(
            "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload, payload_ver)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                seq_i64,
                session_id.raw() as i64,
                op_id.map(|o| o.raw() as i64),
                kind_name(kind),
                state_json,
                ts,
                payload_json,
                payload_ver,
            ],
        )?;
        conn.execute(
            "UPDATE session SET state = ?2, updated_ms = ?3 WHERE id = ?1",
            params![session_id.raw() as i64, state_json, ts],
        )?;
        Ok(seq)
    }

    /// `record_compaction` as ONE transaction: insert the compaction row and
    /// append the accepted/rejected compaction event together.
    #[allow(clippy::too_many_arguments)]
    pub fn record_compaction_and_event(
        &self,
        session_id: SessionId,
        before_tokens: i64,
        after_tokens: i64,
        target_tokens: i64,
        accepted: bool,
        strategy: &str,
        expected_state: AgentState,
        event: CommandEvent,
    ) -> StoreResult<EventSeq> {
        let seam = Arc::clone(&self.seam);
        let strategy = strategy.to_owned();
        let event_payload_json = event.payload.as_ref().map(|p| p.to_string());
        // Preparation BEFORE enqueueing: the event's landing state JSON.
        let event_state_json = serde_json::to_string(&event.state).unwrap();
        // Preparation BEFORE enqueueing: the session state expectation and
        // both refusal messages (the closure formats nothing).
        let expectation = PreparedSessionStateExpectation::prepare(session_id, expected_state);
        // Preparation BEFORE enqueueing: the journal diagnostic context and
        // the timestamp are captured on the caller's thread (audit item 8) —
        // the writer job executes SQL only.
        let seq_messages = EventSeqMessages::for_session(session_id);
        let now = now_ms();
        self.writer.execute("record_compaction_and_event", move |conn| {
        let txn = SessionCommandTxn::begin_prepared(conn, &seam, session_id, expectation)?;
        let changed = txn.tx.execute(
            "INSERT INTO compaction(session_id, before_tokens, after_tokens, target_tokens, accepted, strategy, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id.raw() as i64,
                before_tokens,
                after_tokens,
                target_tokens,
                accepted as i64,
                strategy,
                now
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Migration(
                "record_compaction: expected exactly one inserted row".into(),
            ));
        }
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
        Ok(seq)
        })
    }

    // -------------------------------------------------- actor batch surface

    /// Events strictly after `after_seq` (SSE resume cursor). A cursor at or
    /// above `i64::MAX` can never be followed by a durable seq, so the page
    /// is empty — never a `u64` wrap into the whole journal.
    pub fn events_after(
        &self,
        session_id: SessionId,
        after_seq: EventSeq,
    ) -> StoreResult<Vec<Event>> {
        if after_seq.raw() >= i64::MAX as u64 {
            return Ok(Vec::new());
        }
        self.events_range(session_id, after_seq.raw() + 1, None)
    }

    pub fn events_range(
        &self,
        session_id: SessionId,
        from_seq: u64,
        limit: Option<u64>,
    ) -> StoreResult<Vec<Event>> {
        let from = match i64::try_from(from_seq) {
            Ok(from) => from,
            // No durable seq is above i64::MAX: the page is empty.
            Err(_) => return Ok(Vec::new()),
        };
        // `None` is SQLite's unbounded sentinel (`LIMIT -1`); an explicit
        // limit above the signed range is refused, never bound as a negative
        // (unbounded) LIMIT.
        let limit = match limit {
            None => -1i64,
            Some(limit) => i64::try_from(limit).map_err(|_| {
                StoreError::Oversized("event page limit exceeds SQLite signed range".into())
            })?,
        };
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT seq, session_id, op_id, kind, state, ts_ms, payload, payload_ver
             FROM event
             WHERE session_id = ?1 AND seq >= ?2 ORDER BY seq ASC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64, from, limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(event_map(row, session_id)?.0);
        }
        Ok(out)
    }

    /// Versioned twin of [`Store::events_range`] (v11+): each event rides
    /// its payload schema version so typed journal readers can decode every
    /// payload through its version — an unknown version is a loud error,
    /// never a silent parse of a future shape.
    pub fn events_versioned_range(
        &self,
        session_id: SessionId,
        from_seq: u64,
        limit: Option<u64>,
    ) -> StoreResult<Vec<(Event, i64)>> {
        let from = match i64::try_from(from_seq) {
            Ok(from) => from,
            // No durable seq is above i64::MAX: the page is empty.
            Err(_) => return Ok(Vec::new()),
        };
        // Same signed-range discipline as [`Store::events_range`].
        let limit = match limit {
            None => -1i64,
            Some(limit) => i64::try_from(limit).map_err(|_| {
                StoreError::Oversized("event page limit exceeds SQLite signed range".into())
            })?,
        };
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT seq, session_id, op_id, kind, state, ts_ms, payload, payload_ver
             FROM event
             WHERE session_id = ?1 AND seq >= ?2 ORDER BY seq ASC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64, from, limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(event_map(row, session_id)?);
        }
        Ok(out)
    }

    pub fn last_event_seq(&self, session_id: SessionId) -> StoreResult<Option<EventSeq>> {
        let conn = self.read()?;
        let out = conn.query_row(
            "SELECT MAX(seq) FROM event WHERE session_id = ?1",
            params![session_id.raw() as i64],
            |r| r.get::<_, Option<i64>>(0),
        )?;
        id_field_opt::<EventSeq>(
            &format!("event journal of session {session_id} MAX(seq)"),
            out,
        )
    }

    // ---------------------------------------------------------------- messages

    pub fn get_task_ledger(&self, session_id: SessionId) -> StoreResult<Option<serde_json::Value>> {
        let conn = self.read()?;
        let raw: Option<String> = conn
            .query_row(
                // v10: the legacy one-row-per-session ledger blob lives in
                // `task_ledger`; `task` holds the typed durable Task rows.
                "SELECT ledger FROM task_ledger WHERE session_id = ?1 ORDER BY updated_ms DESC LIMIT 1",
                params![session_id.raw() as i64],
                |r| r.get(0),
            )
            .optional()?;
        match raw {
            Some(s) => Ok(Some(parse_json(
                &format!("task ledger for session {session_id}"),
                &s,
            )?)),
            None => Ok(None),
        }
    }

    pub fn put_task_ledger(
        &self,
        session_id: SessionId,
        ledger: serde_json::Value,
    ) -> StoreResult<()> {
        // Preparation BEFORE enqueueing: the ledger JSON blob and its
        // timestamp (audit item 8).
        let ledger_json = ledger.to_string();
        let now = now_ms();
        self.writer.execute("put_task_ledger", move |conn| {
            // v10: the ledger blob moved to `task_ledger` when the typed
            // durable `task` rows took over the `task` table name.
            conn.execute(
                "DELETE FROM task_ledger WHERE session_id = ?1",
                params![session_id.raw() as i64],
            )?;
            conn.execute(
                "INSERT INTO task_ledger(session_id, ledger, updated_ms) VALUES (?1, ?2, ?3)",
                params![session_id.raw() as i64, ledger_json, now],
            )?;
            Ok(())
        })
    }

    /// Append ONE typed ledger entry with the next gapless per-session seq.
    /// `entry_type` and `schema_ver` are the row's explicit schema tag;
    /// payload is validated (typed decode) by the session layer BEFORE this
    /// call. Returns the assigned seq.
    pub fn append_ledger_entry(
        &self,
        session_id: SessionId,
        entry_type: &str,
        schema_ver: i64,
        payload: serde_json::Value,
    ) -> StoreResult<i64> {
        if entry_type.is_empty() || entry_type.len() > 64 {
            return Err(StoreError::Migration(
                "ledger entry_type must be 1..=64 chars".into(),
            ));
        }
        if schema_ver <= 0 {
            return Err(StoreError::Migration(
                "ledger entry schema_ver must be > 0".into(),
            ));
        }
        let seam = Arc::clone(&self.seam);
        let entry_type = entry_type.to_owned();
        // Preparation BEFORE enqueueing: the entry payload JSON and its
        // timestamp (audit item 8).
        let payload_json = payload.to_string();
        let now = now_ms();
        self.writer.execute("append_ledger_entry", move |conn| {
            // One transaction: seq allocation and the insert are atomic. The
            // next seq NEVER rewinds below the head checkpoint (GREATEST of the
            // entry max and the folded checkpoint), so after a compaction the
            // "fold entries after checkpoint_seq" cursor keeps advancing even
            // though pruned rows are gone.
            let tx = conn.unchecked_transaction()?;
            let prev: Option<i64> = tx.query_row(
                "SELECT MAX(seq) FROM ledger_entry WHERE session_id = ?1",
                params![session_id.raw() as i64],
                |r| r.get(0),
            )?;
            if let Some(prev) = prev {
                if prev < 1 {
                    return Err(StoreError::Corrupt(vec![
                        "typed ledger last sequence is below 1".into(),
                    ]));
                }
            }
            let checkpoint: i64 = tx.query_row(
                "SELECT COALESCE(MAX(checkpoint_seq), 0) FROM ledger_head WHERE session_id = ?1",
                params![session_id.raw() as i64],
                |r| r.get(0),
            )?;
            if checkpoint < 0 {
                return Err(StoreError::Corrupt(vec![
                    "typed ledger head checkpoint is negative".into(),
                ]));
            }
            let high_water = prev.unwrap_or(0).max(checkpoint);
            let seq = high_water.checked_add(1).ok_or_else(|| {
                StoreError::Corrupt(vec![
                    "typed ledger sequence exhausted SQLite signed range".into()
                ])
            })?;
            tx.execute(
            "INSERT INTO ledger_entry(session_id, seq, entry_type, schema_ver, payload, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id.raw() as i64,
                seq,
                entry_type,
                schema_ver,
                payload_json,
                now
            ],
        )?;
            // Durability boundary of one typed-ledger append: crash before the
            // COMMIT (entry rolls back) or right after it (entry durable).
            seam.trip("le_precommit");
            tx.commit()?;
            seam.trip("le_committed");
            Ok(seq)
        })
    }

    /// Read ledger entries of one session, ascending by seq. Bounded reads:
    /// the caller pages with `after_seq` and `limit` (paging is fundamental).
    /// `u64::MAX` is the legacy whole-ledger sentinel preserved for the
    /// existing fold callers and maps to the explicit unbounded page
    /// ([`Store::ledger_entries_page`] with `None`); any OTHER limit above
    /// the signed range refuses typed before SQL.
    pub fn ledger_entries(
        &self,
        session_id: SessionId,
        after_seq: Option<i64>,
        limit: u64,
    ) -> StoreResult<Vec<LedgerEntryRow>> {
        let limit = if limit == u64::MAX { None } else { Some(limit) };
        self.ledger_entries_page(session_id, after_seq, limit)
    }

    /// Explicit-page twin of [`Store::ledger_entries`]: `None` binds
    /// SQLite's unbounded sentinel (`LIMIT -1`) for whole-ledger folds, and
    /// an explicit limit above `i64::MAX` refuses typed — never bound as a
    /// negative (unbounded) LIMIT.
    pub fn ledger_entries_page(
        &self,
        session_id: SessionId,
        after_seq: Option<i64>,
        limit: Option<u64>,
    ) -> StoreResult<Vec<LedgerEntryRow>> {
        let limit = match limit {
            None => -1i64,
            Some(limit) => i64::try_from(limit).map_err(|_| {
                StoreError::Oversized("ledger entry page limit exceeds SQLite signed range".into())
            })?,
        };
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT seq, entry_type, schema_ver, payload, created_ms FROM ledger_entry
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            after_seq.unwrap_or(0),
            limit
        ])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(ledger_entry_map(row, session_id)?);
        }
        Ok(out)
    }

    /// Read ledger entries of one session NEWEST-FIRST (descending seq),
    /// strictly below `before_seq` (exclusive; `None` starts at the newest).
    /// Additive read used by the coordination-board reader, whose pages are
    /// newest-first by contract: the (session_id, seq) primary key serves
    /// the ORDER BY seq DESC scan, so one page reads O(limit) rows — never
    /// the whole stream. Same row shape and decode contract as
    /// [`Store::ledger_entries`]; same `u64::MAX` legacy sentinel rule.
    pub fn ledger_entries_desc(
        &self,
        session_id: SessionId,
        before_seq: Option<i64>,
        limit: u64,
    ) -> StoreResult<Vec<LedgerEntryRow>> {
        let limit = if limit == u64::MAX { None } else { Some(limit) };
        self.ledger_entries_desc_page(session_id, before_seq, limit)
    }

    /// Explicit-page twin of [`Store::ledger_entries_desc`]: `None` binds
    /// SQLite's unbounded sentinel; an explicit limit above `i64::MAX`
    /// refuses typed.
    pub fn ledger_entries_desc_page(
        &self,
        session_id: SessionId,
        before_seq: Option<i64>,
        limit: Option<u64>,
    ) -> StoreResult<Vec<LedgerEntryRow>> {
        let limit = match limit {
            None => -1i64,
            Some(limit) => i64::try_from(limit).map_err(|_| {
                StoreError::Oversized("ledger entry page limit exceeds SQLite signed range".into())
            })?,
        };
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT seq, entry_type, schema_ver, payload, created_ms FROM ledger_entry
             WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            before_seq.unwrap_or(i64::MAX),
            limit
        ])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(ledger_entry_map(row, session_id)?);
        }
        Ok(out)
    }

    /// The newest ledger seq of the session (0 when the ledger is empty).
    pub fn ledger_max_seq(&self, session_id: SessionId) -> StoreResult<i64> {
        let conn = self.read()?;
        Ok(conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM ledger_entry WHERE session_id = ?1",
            params![session_id.raw() as i64],
            |r| r.get::<_, i64>(0),
        )?)
    }

    /// The session's materialized head checkpoint, if one exists.
    pub fn ledger_head(&self, session_id: SessionId) -> StoreResult<Option<LedgerHeadRow>> {
        let conn = self.read()?;
        let raw: Option<(String, i64, i64, i64)> = conn
            .query_row(
                "SELECT head_json, checkpoint_seq, schema_ver, updated_ms
                 FROM ledger_head WHERE session_id = ?1",
                params![session_id.raw() as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        match raw {
            Some((head, checkpoint_seq, schema_ver, updated_ms)) => Ok(Some(LedgerHeadRow {
                head_json: parse_json(&format!("ledger head for session {session_id}"), &head)?,
                checkpoint_seq,
                schema_ver,
                updated_ms,
            })),
            None => Ok(None),
        }
    }

    /// Write (or refresh) the materialized head checkpoint. Standalone use:
    /// crash-recovery folding of entries appended since the last checkpoint.
    pub fn put_ledger_head(
        &self,
        session_id: SessionId,
        head_json: serde_json::Value,
        checkpoint_seq: i64,
        schema_ver: i64,
    ) -> StoreResult<()> {
        let seam = Arc::clone(&self.seam);
        // Preparation BEFORE enqueueing: the head JSON and its timestamp
        // (audit item 8).
        let head_json = head_json.to_string();
        let now = now_ms();
        self.writer.execute("put_ledger_head", move |conn| {
            // Durability boundary of the standalone head refresh: crash before
            // the autocommit statement or right after it.
            seam.trip("head_prewrite");
            conn.execute(
            "INSERT INTO ledger_head(session_id, head_json, checkpoint_seq, schema_ver, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
                head_json = excluded.head_json,
                checkpoint_seq = excluded.checkpoint_seq,
                schema_ver = excluded.schema_ver,
                updated_ms = excluded.updated_ms",
            params![
                session_id.raw() as i64,
                head_json,
                checkpoint_seq,
                schema_ver,
                now
            ],
        )?;
            seam.trip("head_written");
            Ok(())
        })
    }

    /// ONE transaction: delete every ledger entry with `seq < below_seq`
    /// except the pinned never-evict entries (`protect`), then rewrite the
    /// materialized head checkpoint to fold the deletion. The session layer
    /// computed the watermark (`below_seq`) and the pinned set — the last
    /// GoalSet/CriteriaSet/Decision and every unresolved BlockerOpened —
    /// from the FULLY DECODED entry stream: compaction refuses to run when
    /// any entry fails its schema decode (an undecodable row is never
    /// silently deleted). Returns the number of deleted entries.
    pub fn compact_ledger(
        &self,
        session_id: SessionId,
        below_seq: i64,
        protect: &[i64],
        head_json: serde_json::Value,
        checkpoint_seq: i64,
        schema_ver: i64,
    ) -> StoreResult<usize> {
        let seam = Arc::clone(&self.seam);
        let protect = protect.to_owned();
        // Preparation BEFORE enqueueing: the folded head JSON, its timestamp
        // (audit item 8) and the whole DELETE statement text — the closure
        // must not format.
        let head_json = head_json.to_string();
        let sid = session_id.raw() as i64;
        let mut sql =
            format!("DELETE FROM ledger_entry WHERE session_id = {sid} AND seq < {below_seq}");
        if !protect.is_empty() {
            sql.push_str(&format!(
                " AND seq NOT IN ({})",
                std::iter::repeat_n("?", protect.len())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        let now = now_ms();
        self.writer.execute("compact_ledger", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut params: Vec<&dyn rusqlite::types::ToSql> = Vec::new();
            for p in &protect {
                params.push(p);
            }
            let deleted = tx.execute(&sql, rusqlite::params_from_iter(params.iter()))?;
            // Durability boundary mid-fold: crash after the DELETE executed but
            // before the head rewrite (the whole compaction transaction rolls
            // back — entries and head stay consistent).
            seam.trip("compact_fold");
            tx.execute(
            "INSERT INTO ledger_head(session_id, head_json, checkpoint_seq, schema_ver, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
                head_json = excluded.head_json,
                checkpoint_seq = excluded.checkpoint_seq,
                schema_ver = excluded.schema_ver,
                 updated_ms = excluded.updated_ms",
            params![
                session_id.raw() as i64,
                head_json,
                checkpoint_seq,
                schema_ver,
                now
            ],
        )?;
            // Durability boundary of the compaction fold: crash before the
            // COMMIT (delete + head rewrite roll back together) or right after
            // it (the fold is durable).
            seam.trip("compact_precommit");
            tx.commit()?;
            seam.trip("compact_committed");
            Ok(deleted)
        })
    }

    // ---------------------------------------------------------------- durable task

    pub fn upsert_memory_fact(
        &self,
        session_id: SessionId,
        kind: &str,
        key: &str,
        value: &str,
    ) -> StoreResult<()> {
        let fact_stamp = self.fact_timestamp();
        let kind = kind.to_owned();
        let key = key.to_owned();
        let value = value.to_owned();
        self.writer.execute("upsert_memory_fact", move |conn| {
        conn.execute(
            "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, kind, key) DO UPDATE SET value = ?4, updated_ms = ?5",
            params![session_id.raw() as i64, kind, key, value, fact_stamp],
        )?;
        Ok(())
        })
    }

    /// Atomically upsert MANY memory facts of ONE session in ONE SQLite
    /// transaction (one commit + fsync): the row group either lands fully
    /// or not at all — a crash between the writes can never expose a
    /// partial group (orchestrator run-compile assignment rows depend on
    /// exactly this). Same table/conflict semantics as the single-row
    /// [`Store::upsert_memory_fact`]; value bounds remain the caller's
    /// contract (the session layer enforces them on its single-row path).
    pub fn upsert_memory_facts(
        &self,
        session_id: SessionId,
        facts: &[(&str, &str, &str)],
    ) -> StoreResult<()> {
        if facts.is_empty() {
            return Ok(());
        }
        let fact_stamp = self.fact_timestamp();
        let facts: Vec<(String, String, String)> = facts
            .iter()
            .map(|(kind, key, value)| (kind.to_string(), key.to_string(), value.to_string()))
            .collect();
        self.writer.execute("upsert_memory_facts", move |conn| {
        let tx = conn.unchecked_transaction()?;
        for (kind, key, value) in facts {
            tx.execute(
                "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id, kind, key) DO UPDATE SET value = ?4, updated_ms = ?5",
                params![
                    session_id.raw() as i64,
                    kind,
                    key,
                    value,
                    fact_stamp
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
        })
    }

    /// Deterministic newest-first page over memory facts (paging is
    /// fundamental). Rows follow the total order
    /// `(updated_ms DESC, kind DESC, key DESC)` — the same order the
    /// legacy [`Store::memory_facts`] full read uses — and a page contains
    /// only rows strictly AFTER the `after` cursor position (older in the
    /// order). Returns `(rows, has_more)` with at most `limit` rows; pass
    /// the last row's `(updated_ms, kind, key)` as the next `after` cursor.
    ///
    /// Cursor semantics under concurrent writes: an upsert only moves a row
    /// toward the NEWEST end of the order (its `updated_ms` is rewritten to
    /// now), so a backward walk can never see the same `(kind, key)` twice,
    /// and rows that existed at the walk's start and are never rewritten
    /// appear exactly once — deterministic, no duplicate, no gap.
    pub fn memory_facts_page(
        &self,
        session_id: SessionId,
        after: Option<&MemoryFactCursor>,
        limit: u64,
    ) -> StoreResult<(Vec<MemoryFactRow>, bool)> {
        let conn = self.read()?;
        let mut sql = String::from(
            "SELECT kind, key, value, updated_ms FROM memory_fact
             WHERE session_id = ?1",
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(5);
        params.push(rusqlite::types::Value::Integer(session_id.raw() as i64));
        if let Some((ms, kind, key)) = after {
            sql.push_str(
                " AND (updated_ms < ?2 OR (updated_ms = ?2 AND (kind < ?3 OR (kind = ?3 AND key < ?4))))",
            );
            params.push(rusqlite::types::Value::Integer(*ms));
            params.push(rusqlite::types::Value::Text(kind.clone()));
            params.push(rusqlite::types::Value::Text(key.clone()));
        }
        // Probe one extra row for the has_more verdict. u64 limits are
        // clamped to the i64 domain first (u64::MAX as i64 would wrap to -1
        // and silently return an empty page).
        sql.push_str(" ORDER BY updated_ms DESC, kind DESC, key DESC LIMIT ?");
        let probe = (limit.min(i64::MAX as u64) as i64).saturating_add(1);
        params.push(rusqlite::types::Value::Integer(probe));
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        let mut out: Vec<MemoryFactRow> = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(MemoryFactRow {
                kind: row.get(0)?,
                key: row.get(1)?,
                value: row.get(2)?,
                updated_ms: row.get(3)?,
            });
        }
        let has_more = out.len() as u64 > limit;
        if has_more {
            out.truncate(limit as usize);
        }
        Ok((out, has_more))
    }

    pub fn memory_facts(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Vec<(String, String, String)>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            // Deterministic total order (updated_ms DESC, kind DESC, key
            // DESC): page cursors cut this exact order, so the full read is
            // the unbounded prefix of the paged read.
            "SELECT kind, key, value FROM memory_fact WHERE session_id = ?1
             ORDER BY updated_ms DESC, kind DESC, key DESC",
        )?;
        let rows = stmt.query_map(params![session_id.raw() as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Total count of memory facts for one session (cheap; facts are small
    /// per session).
    pub fn memory_fact_count(&self, session_id: SessionId) -> StoreResult<i64> {
        let conn = self.read()?;
        let out = conn.query_row(
            "SELECT COUNT(*) FROM memory_fact WHERE session_id = ?1",
            params![session_id.raw() as i64],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(out)
    }

    // ---------------------------------------------------------------- compactions

    pub fn record_compaction(
        &self,
        session_id: SessionId,
        before_tokens: i64,
        after_tokens: i64,
        target_tokens: i64,
        accepted: bool,
        strategy: &str,
    ) -> StoreResult<()> {
        let strategy = strategy.to_owned();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("record_compaction", move |conn| {
        conn.execute(
            "INSERT INTO compaction(session_id, before_tokens, after_tokens, target_tokens, accepted, strategy, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id.raw() as i64,
                before_tokens,
                after_tokens,
                target_tokens,
                accepted as i64,
                strategy,
                now
            ],
        )?;
        Ok(())
        })
    }

    // ---------------------------------------------------------------- permissions

    // ---------------------------------------------------------------- prompt queue
    /// Durably queue a prompt that arrived while another turn was active.
    /// The full execution envelope is stored; the user conversation message
    /// is NOT materialized yet (deferred materialization — audit round 7:
    /// conversation chronology is insertion order, so the message is
    /// appended at admission, after the preceding turn's output).
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_prompt(
        &self,
        session: SessionId,
        op_id: OpId,
        prompt: &str,
        files: &[String],
        model: Option<&str>,
        variant: Option<&str>,
        agent: Option<&str>,
        requested_at: i64,
    ) -> StoreResult<i64> {
        let prompt = prompt.to_owned();
        let files = files.to_owned();
        let model = model.map(|v| v.to_owned());
        let variant = variant.map(|v| v.to_owned());
        let agent = agent.map(|v| v.to_owned());
        // Preparation BEFORE enqueueing: the queue row's JSON columns.
        let files_json = serde_json::to_string(&files).unwrap_or_else(|_| "[]".into());
        self.writer.execute("enqueue_prompt", move |conn| {
        let tx = conn.unchecked_transaction()?;
        let prev: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM prompt_queue WHERE session_id = ?1",
            params![session.raw() as i64],
            |r| r.get(0),
        )?;
        if prev < 0 {
            return Err(StoreError::Corrupt(vec![
                "prompt queue last sequence is negative".into(),
            ]));
        }
        let seq = prev.checked_add(1).ok_or_else(|| {
            StoreError::Corrupt(vec![
                "prompt queue sequence exhausted SQLite signed range".into(),
            ])
        })?;
        tx.execute(
            "INSERT INTO prompt_queue(session_id, seq, op_id, prompt, files, model, variant, agent, status, requested_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)",
            params![
                session.raw() as i64,
                seq,
                op_id.raw() as i64,
                prompt,
                files_json,
                model,
                variant,
                agent,
                requested_at
            ],
        )?;
        tx.commit()?;
        Ok(seq)
        })
    }

    /// Oldest row that is not terminal (pending/claimed/running) — FIFO head.
    ///
    /// The persisted `files` JSON is decoded fallibly: a corrupt row is a
    /// typed [`StoreError::Corrupt`] naming the session/seq, never silently an
    /// empty file list (nor a swallowed `None`).
    pub fn queue_head(&self, session: SessionId) -> StoreResult<Option<QueuedPrompt>> {
        let conn = self.read()?;
        let raw = conn
            .query_row(
                "SELECT seq, op_id, prompt, files, model, variant, agent, status, requested_at
                 FROM prompt_queue
                 WHERE session_id = ?1 AND status IN ('pending','claimed','running')
                 ORDER BY seq ASC LIMIT 1",
                params![session.raw() as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, i64>(8)?,
                    ))
                },
            )
            .optional()?;
        match raw {
            Some((
                queue_seq,
                op_id,
                prompt,
                files_json,
                model,
                variant,
                agent,
                status,
                requested_at,
            )) => Ok(Some(QueuedPrompt {
                queue_seq,
                op_id: id_field(
                    &format!("prompt_queue {session} seq {queue_seq} op_id"),
                    op_id,
                )?,
                prompt,
                files: parse_json(
                    &format!("prompt_queue {session} seq {queue_seq} files"),
                    &files_json,
                )?,
                model,
                variant,
                agent,
                status,
                requested_at,
            })),
            None => Ok(None),
        }
    }

    /// Count of rows in each durable status (diagnostics).
    pub fn queue_status_counts(&self, session: SessionId) -> StoreResult<serde_json::Value> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM prompt_queue WHERE session_id = ?1 GROUP BY status",
        )?;
        let rows = stmt.query_map(params![session.raw() as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut counts = serde_json::Map::new();
        for row in rows {
            let (k, v) = row?;
            counts.insert(k, serde_json::json!(v));
        }
        Ok(serde_json::Value::Object(counts))
    }

    /// Atomic claim + admission of the queue head (audit round 7): ONE
    /// transaction establishes (a) the head is pending and the session is
    /// eligible, (b) pending -> claimed, (c) the user message is materialized
    /// at the true conversation tail, (d) the session row moves to the
    /// target state, (e) the admission journal event seq is computed so the
    /// session layer can journal the turn-open with a gapless sequence. No
    /// other submission can cut between those operations (single writer).
    ///
    /// Returns Ok(None) when the head is absent or the session state is not
    /// in `eligible_states` (nothing is touched in either case).
    ///
    /// The writer closure is SQL-ONLY (audit finding 5): `files` and the
    /// session state are validated against caller-prepared canonical texts
    /// and decoded on the caller's thread, never on the single writer owner.
    pub fn admit_queue_head(
        &self,
        session: SessionId,
        eligible_states: &[&str],
        target_state: &str,
    ) -> StoreResult<Option<(AdmittedPrompt, i64)>> {
        type QueueHeadRow = (
            i64,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        // Preparation BEFORE enqueueing: every eligible state as the exact
        // JSON text a session row stores, so the closure only compares raw
        // text.
        let eligible_states_json: Vec<String> = eligible_states
            .iter()
            .map(|state| {
                serde_json::to_string(state)
                    .expect("in-process session-state label serialization cannot fail")
            })
            .collect();
        // Preparation BEFORE enqueueing: the target state is serialized here,
        // not on the writer owner. The message data is wrapped by SQLite's
        // `json_object` inside the SQL-only closure (byte-identical to the
        // former `json!` wrapper), so the owner thread never runs serde.
        let state_json = serde_json::to_string(target_state).unwrap();
        // Preparation BEFORE enqueueing: decode labels (the closure formats
        // nothing).
        let op_id_label = format!("prompt_queue {session} op_id");
        let session_state_label = format!("session {session} state");
        let files_label = format!("prompt_queue {session} files");
        let corrupt_files = format!("prompt_queue {session} files: not a string array");
        let session_missing = format!("session {session} has no row for queue admission");
        // The timestamp is captured once on the caller's thread (audit item
        // 8): every row this one transaction stamps shares it.
        let now = now_ms();
        let outcome = self.writer.execute("admit_queue_head", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let head: Option<QueueHeadRow> = tx
                .query_row(
                    "SELECT seq, op_id, prompt, files, model, variant, agent FROM prompt_queue
                 WHERE session_id = ?1 AND status = 'pending' ORDER BY seq ASC LIMIT 1",
                    params![session.raw() as i64],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<String>>(6)?,
                        ))
                    },
                )
                .optional()?;
            let Some((queue_seq, op_id_raw, prompt, files_json, model, variant, agent)) = head
            else {
                return Ok(None);
            };
            // Validate every persisted column BEFORE the first mutation: a
            // corrupt row surfaces as a typed `Corrupt` naming the field and
            // the transaction rolls back without claiming the prompt or
            // materializing the message. `files` must be a JSON array of
            // strings; SQL-side validation is exact, so the caller's parse
            // after commit cannot fail on a row this transaction mutated.
            let op_id = id_field(&op_id_label, op_id_raw)?;
            let files_ok: i64 = tx
                .query_row(
                    "SELECT CASE
                        WHEN json_valid(?1) = 0 THEN 0
                        WHEN json_type(?1) <> 'array' THEN 0
                        WHEN EXISTS (SELECT 1 FROM json_each(?1) WHERE type <> 'text') THEN 0
                        ELSE 1
                    END",
                    params![files_json.as_str()],
                    |r| r.get(0),
                )
                .map_err(|_| StoreError::Corrupt(vec![corrupt_files.clone()]))?;
            if files_ok != 1 {
                return Err(StoreError::Corrupt(vec![corrupt_files.clone()]));
            }
            let state_raw: Option<String> = tx
                .query_row(
                    "SELECT state FROM session WHERE id = ?1",
                    params![session.raw() as i64],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(state_raw) = state_raw else {
                return Ok(Some(RawAdmitOutcome::MissingSession));
            };
            if !eligible_states_json.iter().any(|text| text == &state_raw) {
                return Ok(Some(RawAdmitOutcome::Ineligible {
                    state_json: state_raw,
                    files_json,
                }));
            }
            tx.execute(
                "UPDATE prompt_queue SET status = 'claimed', claimed_at = ?2
             WHERE session_id = ?1 AND seq = ?3",
                params![session.raw() as i64, now, queue_seq],
            )?;
            let prev_event: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM event WHERE session_id = ?1",
                params![session.raw() as i64],
                |r| r.get(0),
            )?;
            let event_seq = prev_event + 1;
            tx.execute(
                "INSERT INTO message(session_id, seq, role, data, created_ms)
             VALUES (?1, ?2, 'user', json_object('text', ?3), ?4)",
                params![session.raw() as i64, event_seq, prompt, now],
            )?;
            tx.execute(
                "UPDATE session SET state = ?2, updated_ms = ?3 WHERE id = ?1",
                params![session.raw() as i64, state_json, now],
            )?;
            tx.commit()?;
            Ok(Some(RawAdmitOutcome::Claimed(RawAdmittedPrompt {
                queue_seq,
                op_id,
                prompt,
                files_json,
                model,
                variant,
                agent,
                message_seq: event_seq,
            })))
        })?;
        // The typed outcome is recovered HERE, on the caller's thread.
        match outcome {
            None => Ok(None),
            Some(RawAdmitOutcome::MissingSession) => Err(StoreError::Migration(session_missing)),
            Some(RawAdmitOutcome::Ineligible {
                state_json,
                files_json,
            }) => {
                // Preserve the former decode contract: a corrupt state or
                // files column surfaces typed; nothing was admitted.
                let _: AgentState = parse_json(&session_state_label, &state_json)?;
                let _: Vec<String> = parse_json(&files_label, &files_json)?;
                Ok(None)
            }
            Some(RawAdmitOutcome::Claimed(raw)) => {
                let files: Vec<String> = parse_json(&files_label, &raw.files_json)?;
                Ok(Some((
                    AdmittedPrompt {
                        queue_seq: raw.queue_seq,
                        op_id: raw.op_id,
                        prompt: raw.prompt,
                        files,
                        model: raw.model,
                        variant: raw.variant,
                        agent: raw.agent,
                        message_seq: raw.message_seq,
                    },
                    raw.message_seq,
                )))
            }
        }
    }

    pub fn mark_queue_status(
        &self,
        session: SessionId,
        queue_seq: i64,
        status: &str,
    ) -> StoreResult<()> {
        let status = status.to_owned();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer.execute("mark_queue_status", move |conn| {
            conn.execute(
                "UPDATE prompt_queue SET status = ?3, completed_at = ?4
             WHERE session_id = ?1 AND seq = ?2",
                params![session.raw() as i64, queue_seq, status, now],
            )?;
            Ok(())
        })
    }

    /// Op ids of all non-terminal queue rows for a session (abort(None)
    /// must durably cancel queued prompts too). `running` rows are included:
    /// an interrupted admission/drive leaves a running row (`running` counts
    /// as non-terminal in [`Store::queue_status_counts`]) and abort must be
    /// able to clear it, never leave it counted forever.
    pub fn queue_op_ids(&self, session: SessionId) -> StoreResult<Vec<OpId>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT seq, op_id FROM prompt_queue
             WHERE session_id = ?1 AND status IN ('pending','claimed','running')",
        )?;
        let rows = stmt.query_map(params![session.raw() as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (queue_seq, op_raw) = row?;
            out.push(id_field(
                &format!("prompt_queue {session} seq {queue_seq} op_id"),
                op_raw,
            )?);
        }
        Ok(out)
    }

    /// Durable cancellation of queued rows (abort semantics): pending,
    /// claimed AND running rows for the given ops become cancelled and are
    /// never admitted. A `running` row whose drive was interrupted is
    /// exactly the crash residue abort must be able to clear. Returns how
    /// many rows were cancelled.
    pub fn cancel_queued_ops(&self, session: SessionId, ops: &[OpId]) -> StoreResult<i64> {
        let ops = ops.to_owned();
        // Preparation BEFORE enqueueing: one timestamp for the whole cancel
        // batch, captured on the caller's thread (audit item 8).
        let now = now_ms();
        self.writer.execute("cancel_queued_ops", move |conn| {
        let mut n = 0i64;
        for op in ops {
            n += conn.execute(
                "UPDATE prompt_queue SET status = 'cancelled', completed_at = ?3
                 WHERE session_id = ?1 AND op_id = ?2 AND status IN ('pending','claimed','running')",
                params![session.raw() as i64, op.raw() as i64, now],
            )? as i64;
        }
        Ok(n)
        })
    }

    /// Recovery pass over the durable queue of one session. Two distinct
    /// crash residues, classified by the DURABLE OWNERSHIP MODEL (the
    /// `turn_record` of the row's op is the ownership witness):
    ///
    /// - `claimed` rows were never driven (crash between claim and drive):
    ///   they return to `pending` so the head is re-admitted; re-admission
    ///   upserts the SAME turn record, so the logical turn is never
    ///   duplicated. Returns that re-admitted count.
    /// - `running` rows whose op has NO active turn record are rows whose
    ///   logical turn already ended (the terminal mark was lost to the
    ///   crash); they are retired to `done` — never re-admitted, which
    ///   would deliver the same prompt twice.
    ///
    /// A `running` row whose op DOES carry an active turn record is the
    /// interrupted logical turn of that record: it is LEFT running. The
    /// queue runner resumes the recorded turn (the durable ownership
    /// witness) and marks the row terminal exactly once after the resume —
    /// so the row can never spin forever and is never delivered twice.
    /// Returns the number of claimed rows returned to pending.
    pub fn recover_claimed_queue_rows(&self, session: SessionId) -> StoreResult<i64> {
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let now = now_ms();
        self.writer
            .execute("recover_claimed_queue_rows", move |conn| {
                let tx = conn.unchecked_transaction()?;
                let reclaimed = tx.execute(
                    "UPDATE prompt_queue SET status = 'pending', claimed_at = NULL
             WHERE session_id = ?1 AND status = 'claimed'",
                    params![session.raw() as i64],
                )? as i64;
                let retired = tx.execute(
                    "UPDATE prompt_queue SET status = 'done', completed_at = ?2
             WHERE session_id = ?1 AND status = 'running' AND NOT EXISTS (
                 SELECT 1 FROM turn_record tr
                  WHERE tr.session_id = prompt_queue.session_id
                    AND tr.turn_op_id = prompt_queue.op_id
                    AND tr.status = 'active')",
                    params![session.raw() as i64, now],
                )? as i64;
                tx.commit()?;
                if retired > 0 {
                    tracing::warn!(
                        session = %session,
                        rows = retired,
                        "queue recovery retired running rows whose logical turn already ended"
                    );
                }
                Ok(reclaimed)
            })
    }

    /// All session ids with non-terminal queue rows (startup kick list).
    pub fn sessions_with_pending_queues(&self) -> StoreResult<Vec<SessionId>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT session_id FROM prompt_queue
             WHERE status IN ('pending','claimed','running')",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for v in rows {
            let raw = v?;
            out.push(id_field::<SessionId>(
                &format!("prompt_queue session_id {raw}"),
                raw,
            )?);
        }
        Ok(out)
    }

    /// Durable bump of one loop signal (spec §28): count the same key
    /// across turns and daemon restarts. Returns true when `threshold` is
    /// reached; the count then resets (a trip closes the window).
    pub fn bump_loop_signal(
        &self,
        session: SessionId,
        key: &str,
        threshold: u32,
        ts_ms: i64,
    ) -> StoreResult<bool> {
        if key.is_empty() || key.len() > 1024 || threshold < 2 {
            return Err(StoreError::Migration(
                "loop signal key must be 1..=1024 bytes; threshold >= 2".into(),
            ));
        }
        let key = key.to_owned();
        self.writer.execute("bump_loop_signal", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let prev: Option<i64> = tx
                .query_row(
                    "SELECT count FROM loop_signal WHERE session_id = ?1 AND key = ?2",
                    params![session.raw() as i64, key],
                    |r| r.get(0),
                )
                .optional()?;
            let count = prev.unwrap_or(0) + 1;
            tx.execute(
                "INSERT OR REPLACE INTO loop_signal(session_id, key, count, updated_ms)
             VALUES (?1, ?2, ?3, ?4)",
                params![session.raw() as i64, key, count, ts_ms],
            )?;
            if count >= i64::from(threshold) {
                tx.execute(
                    "DELETE FROM loop_signal WHERE session_id = ?1 AND key = ?2",
                    params![session.raw() as i64, key],
                )?;
                tx.commit()?;
                return Ok(true);
            }
            tx.commit()?;
            Ok(false)
        })
    }

    /// Clear every loop signal of the session (the task made progress).
    pub fn reset_loop_signals(&self, session: SessionId) -> StoreResult<()> {
        self.writer.execute("reset_loop_signals", move |conn| {
            conn.execute(
                "DELETE FROM loop_signal WHERE session_id = ?1",
                params![session.raw() as i64],
            )?;
            Ok(())
        })
    }

    pub fn loop_signal_counts(&self, session: SessionId) -> StoreResult<Vec<(String, i64)>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT key, count FROM loop_signal WHERE session_id = ?1 ORDER BY updated_ms DESC LIMIT 100",
        )?;
        let rows = stmt.query_map(params![session.raw() as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- op ids

    /// Atomically reserve the range `[start, start + count)` from the ONE
    /// durable op-id sequence shared by every session (schema scope 0), and
    /// return `(start, count)`. Reserved ids are handed out by the caller
    /// strictly in order, so every id is unique and strictly increasing
    /// ACROSS daemon restarts — even when a restart lands in the same
    /// millisecond or the wall clock jumped backwards (the sequence never
    /// consults the clock after migration). A crash between the commit and
    /// the use of the reserved ids only burns ids (gaps); it can never
    /// reuse one.
    ///
    /// `_session` is reserved for a future per-session scope; the current
    /// schema pins one global row (op ids are globally unique — the
    /// `tool_run.op_id` UNIQUE column), so the value is ignored.
    ///
    /// The reservation runs in an IMMEDIATE transaction, so two live stores
    /// over the same database file (a restart racing its predecessor) see
    /// each other's commits instead of double-issuing a range.
    pub fn alloc_op_ids(&self, _session: SessionId, count: u64) -> StoreResult<(u64, u64)> {
        if count == 0 {
            return Err(StoreError::Conflict(
                "alloc_op_ids: count must be non-zero".into(),
            ));
        }
        // Preparation BEFORE enqueueing: the corruption refusal diagnostic
        // (the durable `next_value` is read inside the transaction).
        let corrupted_next = "op_id_seq next_value corrupted: negative".to_owned();
        self.writer.execute("alloc_op_ids", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let next: i64 = tx
                .query_row(
                    "SELECT next_value FROM op_id_seq WHERE session_scope = 0",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::Migration(
                        "op_id_seq global row missing (migrations not applied?)".into(),
                    )
                })?;
            if next < 0 {
                return Err(StoreError::Migration(corrupted_next));
            }
            let start = next as u64;
            // `next_value` lives in a signed INTEGER column: the sequence is
            // exhausted once a reservation would cross i64::MAX.
            let end = start
                .checked_add(count)
                .filter(|end| *end <= i64::MAX as u64)
                .ok_or_else(|| {
                    StoreError::Conflict("alloc_op_ids: op-id sequence exhausted".into())
                })?;
            tx.execute(
                "UPDATE op_id_seq SET next_value = ?1 WHERE session_scope = 0",
                params![end as i64],
            )?;
            tx.commit()?;
            Ok((start, count))
        })
    }

    /// The sequence's current high-water mark: the first id NOT yet
    /// reserved (ids handed out so far are all `< high_water`). Test probe.
    pub fn op_id_seq_high_water(&self) -> StoreResult<u64> {
        let conn = self.read()?;
        let next: i64 = conn
            .query_row(
                "SELECT next_value FROM op_id_seq WHERE session_scope = 0",
                [],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::Migration(
                    "op_id_seq global row missing (migrations not applied?)".into(),
                )
            })?;
        Ok(next as u64)
    }

    /// Bounded, depth-aware journal verification for ONE session — the
    /// per-session authority behind session open ([`JournalDepth::Open`])
    /// and the doctor sweep ([`JournalDepth::Deep`]). It NEVER materializes
    /// the journal: the `(session_id, seq)` primary key plus `CHECK (seq >
    /// 0)` answers the gapless `1..=N` question from ONE covering-index
    /// aggregate (`MIN(seq) = 1 AND MAX(seq) = COUNT(*)`), and every other
    /// check is a point read (session row, first event, tail event). The
    /// first event must be the `session_created` seed in the `idle` state;
    /// the tail event's landing state must agree with the session row's
    /// projected state (the ONE tolerated divergence is the queue-admission
    /// reservation window — see below; an UNDECODABLE tail state cannot be
    /// compared and is reported by the strict event readers at open and by
    /// `Deep` here); a session row with no events is a torn write (creation
    /// seeds both in one transaction). `Deep` additionally requires
    /// non-decreasing timestamps in seq order via an adjacent-pair inversion
    /// count. Returns human-readable problems naming the session;
    /// empty = consistent.
    pub fn journal_session_problems(
        &self,
        session_id: SessionId,
        depth: JournalDepth,
    ) -> StoreResult<Vec<String>> {
        let conn = self.read()?;
        // One deferred read transaction: the checks compare several columns
        // (row projection, gap aggregate, first/tail events) and MUST see one
        // consistent snapshot. Without it, a concurrent append between reads
        // looks like a projection disagreement (a false corruption report on
        // a legitimate in-flight transition).
        let tx = conn.unchecked_transaction()?;
        let problems = Self::journal_session_problems_on(&tx, session_id, depth)?;
        drop(tx);
        Ok(problems)
    }

    fn journal_session_problems_on(
        conn: &Connection,
        session_id: SessionId,
        depth: JournalDepth,
    ) -> StoreResult<Vec<String>> {
        let sid = session_id.raw() as i64;
        let mut problems = Vec::new();
        // Point read of the projected authority (the session row). The
        // doctor iterates rows, so a missing row is only visible to direct
        // callers — it is still a problem, never an excuse to skip.
        let row_state: Option<String> = conn
            .query_row(
                "SELECT state FROM session WHERE id = ?1",
                params![sid],
                |r| r.get(0),
            )
            .optional()?;
        if row_state.is_none() {
            problems.push(format!(
                "session {session_id}: session row is missing for its journal"
            ));
        }
        // ONE covering-index aggregate over `(session_id, seq)`: COUNT/MIN/
        // MAX never touch the table rows or the payload columns.
        let (count, min_seq, max_seq): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(MIN(seq), 0), COALESCE(MAX(seq), 0)
             FROM event WHERE session_id = ?1",
            params![sid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if count == 0 {
            if row_state.is_some() {
                problems.push(format!(
                    "session {session_id}: session row exists but its journal has no events (torn creation)"
                ));
            }
            return Ok(problems);
        }
        if min_seq != 1 || count != max_seq {
            problems.push(format!(
                "session {session_id}: journal holds {count} event(s) spanning seq {min_seq}..={max_seq}; invariant is a gapless 1..={count}"
            ));
        }
        // Point read of the first event: the seq-1 seed contract. A journal
        // whose seq-1 slot was overwritten by a LATER event stays
        // numerically gapless and is only visible here.
        let first: Option<(i64, String, String)> = conn
            .query_row(
                "SELECT seq, kind, state FROM event WHERE session_id = ?1 ORDER BY seq ASC LIMIT 1",
                params![sid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((seq, kind, state)) = first {
            if seq == 1 {
                if kind != "session_created" {
                    problems.push(format!(
                        "session {session_id}: first journal event kind is {kind:?}, not session_created"
                    ));
                }
                match parse_json::<AgentState>(
                    &format!("journal first event state for session {session_id}"),
                    &state,
                ) {
                    Ok(AgentState::Idle) => {}
                    Ok(other) => problems.push(format!(
                        "session {session_id}: first journal event state is {other:?}, not idle"
                    )),
                    Err(_) => problems.push(format!(
                        "session {session_id}: first journal event state is not a valid AgentState"
                    )),
                }
            }
        }
        // Point read of the tail event: every append writes the session
        // row's projection and the event's landing state in ONE
        // transaction, so the two must agree.
        let tail: Option<String> = conn
            .query_row(
                "SELECT state FROM event WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![sid],
                |r| r.get(0),
            )
            .optional()?;
        if let (Some(row_state), Some(tail_state)) = (row_state.as_deref(), tail.as_deref()) {
            match (
                parse_json::<AgentState>(&format!("session {session_id} state"), row_state),
                parse_json::<AgentState>(
                    &format!("session {session_id} tail journal event state"),
                    tail_state,
                ),
            ) {
                (Ok(projected), Ok(landed)) if projected == landed => {}
                (Ok(projected), Ok(landed)) => {
                    // The ONE legal projection-ahead window: queue admission
                    // ([`Store::admit_queue_head`]) reserves the next journal
                    // seq for the materialized admission message and moves
                    // the row to `preparing` in ONE transaction, while the
                    // matching `prompt_admitted` event at that reserved seq
                    // lands immediately after (the agent queue runner). A
                    // crash between the two leaves the projection exactly one
                    // reserved event AHEAD of the journal — recoverable
                    // residue, not corruption. Accepted ONLY while the
                    // artifact proving the reservation exists: a message row
                    // at `MAX(event.seq) + 1` and a `preparing` projection.
                    let mut reserved_admission = false;
                    if projected == AgentState::Preparing {
                        if let Some(reserved_seq) = max_seq.checked_add(1) {
                            reserved_admission = conn.query_row(
                                "SELECT EXISTS(SELECT 1 FROM message
                                 WHERE session_id = ?1 AND seq = ?2)",
                                params![sid, reserved_seq],
                                |r| r.get(0),
                            )?;
                        }
                    }
                    if !reserved_admission {
                        problems.push(format!(
                            "session {session_id}: journal tail event state {landed:?} disagrees with the session row state {projected:?}"
                        ));
                    }
                }
                (Err(_), _) => problems.push(format!(
                    "session {session_id}: session row state is not a valid AgentState"
                )),
                (_, Err(_)) => {
                    // An undecodable event state cannot be compared: Open is
                    // the bounded projection check, and the strict event
                    // readers (journal replay, SSE, paged reads) already
                    // refuse it typed. `Deep` still reports it so the
                    // corruption is never silent.
                    if depth == JournalDepth::Deep {
                        problems.push(format!(
                            "session {session_id}: tail journal event state is not a valid AgentState"
                        ));
                    }
                }
            }
        }
        if depth == JournalDepth::Deep {
            // Timestamps never step backwards in seq order (the append path
            // clamps a backward clock, so a regression is tampering). ONE
            // adjacent-pair inversion count; never a row-by-row compare.
            let inversions: i64 = conn.query_row(
                "SELECT COUNT(*) FROM event e
                 JOIN event p ON p.session_id = e.session_id AND p.seq = e.seq - 1
                 WHERE e.session_id = ?1 AND e.ts_ms < p.ts_ms",
                params![sid],
                |r| r.get(0),
            )?;
            if inversions > 0 {
                problems.push(format!(
                    "session {session_id}: journal timestamps are not non-decreasing in seq order"
                ));
            }
        }
        Ok(problems)
    }

    /// Journal projection consistency for EVERY session (`doctor --deep`).
    /// A session's journal is the gapless sequence 1..=N whose FIRST event is
    /// the `session_created` seed in the `idle` state, whose timestamps are
    /// non-decreasing in seq order, and whose tail event agrees with the
    /// session row's projected state. This is the per-session authority
    /// [`Store::journal_session_problems`] at [`JournalDepth::Deep`], swept
    /// across the session rows (orphan event rows cannot exist: the event
    /// table's `session_id` references `session(id)`). Returns
    /// human-readable problems; empty = consistent.
    pub fn journal_consistency_issues(&self) -> StoreResult<Vec<String>> {
        let conn = self.read()?;
        // The sweep and every per-session comparison share ONE deferred read
        // snapshot so a concurrently appending session cannot present a torn
        // view (row projection read before an append, tail read after).
        let tx = conn.unchecked_transaction()?;
        let mut issues = Vec::new();
        let mut stmt = tx.prepare("SELECT id FROM session ORDER BY id ASC")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let sid_raw: i64 = row.get(0)?;
            let sid = id_field::<SessionId>(&format!("session id {sid_raw}"), sid_raw)?;
            issues.extend(Self::journal_session_problems_on(
                &tx,
                sid,
                JournalDepth::Deep,
            )?);
        }
        drop(rows);
        drop(stmt);
        drop(tx);
        Ok(issues)
    }

    /// Raw `memory_fact` rows of the given kinds across EVERY session,
    /// ascending by session then kind then key (`doctor --deep` orphan-child
    /// scan). Kinds are internal constants of the session/orchestrator
    /// layers — never user input. Bounded: more than
    /// [`MAX_ORCHESTRATOR_FACT_SCAN_ROWS`] rows is a typed refusal, never a
    /// silent truncation (bounded everything).
    pub fn memory_fact_rows_of_kinds(&self, kinds: &[&str]) -> StoreResult<Vec<MemoryFactRowRef>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read()?;
        let placeholders = vec!["?"; kinds.len()].join(",");
        let sql = format!(
            "SELECT session_id, kind, key, value FROM memory_fact
             WHERE kind IN ({placeholders})
             ORDER BY session_id ASC, kind ASC, key ASC
             LIMIT {MAX_ORCHESTRATOR_FACT_SCAN_ROWS}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            kinds.iter().map(|k| k as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt.query(params.as_slice())?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let session_raw: i64 = row.get(0)?;
            out.push(MemoryFactRowRef {
                session_id: id_field(
                    &format!("memory_fact session_id {session_raw}"),
                    session_raw,
                )?,
                kind: row.get(1)?,
                key: row.get(2)?,
                value: row.get(3)?,
            });
        }
        if out.len() as i64 >= MAX_ORCHESTRATOR_FACT_SCAN_ROWS {
            return Err(StoreError::Oversized(format!(
                "memory-fact kind scan reached the {MAX_ORCHESTRATOR_FACT_SCAN_ROWS}-row bound; refusing a partial orphan scan"
            )));
        }
        Ok(out)
    }

    // ------------------------------------------------ child runtime (v23)

    /// Insert one evidence row. `row.id == 0` asks SQLite to assign the next
    /// globally-unique id (`AUTOINCREMENT`); a non-zero id is inserted
    /// explicitly and a collision refuses with `Conflict` instead of ever
    /// overwriting an existing envelope. Returns the id actually stored.
    ///
    /// The durable evidence-id domain is `1..=i64::MAX` (the SQLite signed
    /// `INTEGER PRIMARY KEY` range): an id above it is refused BEFORE any SQL,
    /// so an id can never be stored in its two's-complement negative form and
    /// read back as corruption.
    pub fn evidence_insert(&self, row: &EvidenceRow) -> StoreResult<u64> {
        if row.revision < 1 {
            return Err(StoreError::Malformed(format!(
                "evidence revision {} is below 1",
                row.revision
            )));
        }
        let explicit_id = i64::try_from(row.id).map_err(|_| {
            StoreError::Malformed(format!(
                "evidence id {} exceeds the durable signed domain (1..=i64::MAX)",
                row.id
            ))
        })?;
        // Preparation BEFORE enqueueing: the collision refusal diagnostic
        // (the explicit id is caller-known).
        let existing_conflict = format!(
            "evidence {} already exists; refusing to overwrite a durable envelope",
            row.id
        );
        let row = row.to_owned();
        self.writer.execute("evidence_insert", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let id = if row.id == 0 {
                tx.execute(
                    "INSERT INTO evidence (
                    session_id, workspace_id, task_id, kind, revision,
                    provenance, compressibility, compression, retrieval,
                    compact, backing_cas_hash, completeness, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        row.session_id.raw() as i64,
                        row.workspace_id.raw() as i64,
                        row.task_id.map(|t| t as i64),
                        row.kind,
                        row.revision,
                        row.provenance_json,
                        row.compressibility,
                        row.compression_json,
                        row.retrieval_json,
                        row.compact_json,
                        row.backing_cas_hash,
                        row.completeness,
                        row.created_ms,
                    ],
                )?;
                tx.last_insert_rowid() as u64
            } else {
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM evidence WHERE id = ?1",
                        params![explicit_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if existing.is_some() {
                    return Err(StoreError::Conflict(existing_conflict));
                }
                tx.execute(
                    "INSERT INTO evidence (
                    id, session_id, workspace_id, task_id, kind, revision,
                    provenance, compressibility, compression, retrieval,
                    compact, backing_cas_hash, completeness, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        explicit_id,
                        row.session_id.raw() as i64,
                        row.workspace_id.raw() as i64,
                        row.task_id.map(|t| t as i64),
                        row.kind,
                        row.revision,
                        row.provenance_json,
                        row.compressibility,
                        row.compression_json,
                        row.retrieval_json,
                        row.compact_json,
                        row.backing_cas_hash,
                        row.completeness,
                        row.created_ms,
                    ],
                )?;
                row.id
            };
            tx.commit()?;
            Ok(id)
        })
    }

    /// Read one evidence row by id. Unscoped: callers acting for a session
    /// MUST compare the scope columns before serving the row (the evidence
    /// crate's `DurableEvidenceStore::get_scoped` owns that rule). An id above
    /// the durable `1..=i64::MAX` domain is refused typed before SQL: it can
    /// name no stored row and must never be folded into a negative lookup.
    pub fn evidence_get(&self, id: u64) -> StoreResult<Option<EvidenceRow>> {
        let id = i64::try_from(id).map_err(|_| {
            StoreError::Malformed(format!(
                "evidence id {id} exceeds the durable signed domain (1..=i64::MAX)"
            ))
        })?;
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, workspace_id, task_id, kind, revision,
                    provenance, compressibility, compression, retrieval,
                    compact, backing_cas_hash, completeness, created_ms
             FROM evidence WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(evidence_row_map(row)?)),
            None => Ok(None),
        }
    }

    /// Every evidence row of one session+workspace, oldest id first. The
    /// bounded scope listing the evidence layer exposes (never a whole-table
    /// scan); `limit` is clamped to a sane hard bound.
    pub fn evidence_list_by_scope(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        limit: usize,
    ) -> StoreResult<Vec<EvidenceRow>> {
        let bound = i64::try_from(limit.min(10_000)).unwrap_or(10_000);
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, workspace_id, task_id, kind, revision,
                    provenance, compressibility, compression, retrieval,
                    compact, backing_cas_hash, completeness, created_ms
             FROM evidence
             WHERE session_id = ?1 AND workspace_id = ?2
             ORDER BY id ASC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            workspace_id.raw() as i64,
            bound
        ])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(evidence_row_map(row)?);
        }
        Ok(out)
    }

    /// Additive newest-first scoped listing for the evidence recency path:
    /// every row of one session+workspace with `id < before` (no bound when
    /// `before` is `None`), ordered `created_ms DESC, id DESC` and limited.
    /// `before` is the exclusive keyset cursor (evidence ids are assigned
    /// monotonically, so it is the canonical recency boundary); scope/task
    /// filtering stays in the evidence crate. `limit` is clamped to the same
    /// hard bound as [`Store::evidence_list_by_scope`].
    pub fn evidence_list_by_scope_newest(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        before: Option<u64>,
        limit: usize,
    ) -> StoreResult<Vec<EvidenceRow>> {
        let bound = i64::try_from(limit.min(10_000)).unwrap_or(10_000);
        let cursor = match before {
            Some(before) => i64::try_from(before).map_err(|_| {
                StoreError::Malformed(format!(
                    "evidence cursor {before} exceeds the durable signed domain (1..=i64::MAX)"
                ))
            })?,
            None => i64::MAX,
        };
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, workspace_id, task_id, kind, revision,
                    provenance, compressibility, compression, retrieval,
                    compact, backing_cas_hash, completeness, created_ms
             FROM evidence
             WHERE session_id = ?1 AND workspace_id = ?2 AND id < ?3
             ORDER BY created_ms DESC, id DESC LIMIT ?4",
        )?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            workspace_id.raw() as i64,
            cursor,
            bound
        ])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(evidence_row_map(row)?);
        }
        Ok(out)
    }

    /// Evidence ids whose backing bytes hash to `backing_cas_hash`, oldest
    /// first (bounded). The digest is an audit input only: the evidence
    /// layer still enforces scope before any read, so knowing a digest never
    /// grants cross-session access. A stored id outside the durable
    /// `1..=i64::MAX` domain is typed corruption, never folded into a `u64`.
    pub fn evidence_ids_by_backing(
        &self,
        backing_cas_hash: &str,
        limit: usize,
    ) -> StoreResult<Vec<u64>> {
        let bound = i64::try_from(limit.min(10_000)).unwrap_or(10_000);
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM evidence WHERE backing_cas_hash = ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![backing_cas_hash, bound])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let id_raw: i64 = row.get(0)?;
            if id_raw < 1 {
                return Err(StoreError::Corrupt(vec![format!(
                    "evidence id {id_raw} is below 1"
                )]));
            }
            out.push(id_raw as u64);
        }
        Ok(out)
    }

    /// The current evidence id high-water mark: the largest id ever issued
    /// (0 when none). Durable across reopen via `sqlite_sequence`, so a
    /// restart can never reissue an id. A negative stored id or sequence
    /// (the two's-complement form of an id above `i64::MAX`) is typed
    /// corruption, never clamped to 0.
    pub fn evidence_high_water(&self) -> StoreResult<u64> {
        let conn = self.read()?;
        let max_id: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM evidence", [], |r| {
            r.get(0)
        })?;
        let seq: i64 = conn
            .query_row(
                "SELECT COALESCE(seq, 0) FROM sqlite_sequence WHERE name = 'evidence'",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if max_id < 0 || seq < 0 {
            return Err(StoreError::Corrupt(vec![format!(
                "evidence high-water mark is negative (max id {max_id}, sequence {seq}); \
                 the durable domain is 1..=i64::MAX"
            )]));
        }
        Ok(max_id.max(seq) as u64)
    }
}

#[cfg(test)]
#[path = "journal_authority_tests.rs"]
mod journal_authority_tests;

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), true).unwrap();
        (dir, s)
    }

    #[test]
    fn truncated_db_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("faktor-plus.db"), b"SQLite format 3\x00").unwrap();
        match Store::open(dir.path(), true) {
            Err(StoreError::Sqlite(_)) | Err(StoreError::Corrupt(_)) => {}
            other => panic!("truncated db must fail cleanly, got {other:?}"),
        }
    }

    #[test]
    fn prompt_queue_sequence_exhaustion_refuses_typed_without_a_row() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "INSERT INTO prompt_queue(session_id, seq, op_id, prompt, files, status, requested_at)
                 VALUES (?1, ?2, 1, 'hostile', '[]', 'pending', 0)",
                params![s.id.raw() as i64, i64::MAX],
            )
            .unwrap();
        }
        let err = store
            .enqueue_prompt(s.id, OpId::new(2), "next", &[], None, None, None, 0)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Corrupt(ref msgs)
                if msgs.iter().any(|m| m.contains("exhausted"))),
            "the i64::MAX queue seq must refuse as exhaustion: {err:?}"
        );
        let rows: i64 = store
            .raw_conn()
            .query_row(
                "SELECT COUNT(*) FROM prompt_queue WHERE session_id = ?1",
                params![s.id.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "the refused enqueue must insert no row");
    }

    #[test]
    fn workspace_root_roundtrip_and_unknown() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/root/x").unwrap();
        assert_eq!(
            store.workspace_root(ws).unwrap().as_deref(),
            Some("/root/x")
        );
        assert_eq!(store.workspace_root(WorkspaceId::new(999)).unwrap(), None);
    }

    #[test]
    fn workspace_create_is_idempotent() {
        let (_d, store) = tmp_store();
        let a = store.create_workspace("/same").unwrap();
        let b = store.create_workspace("/same").unwrap();
        assert_eq!(a, b);
        let c = store.create_workspace("/other").unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn adversarial_duplicate_event_append_is_structurally_impossible() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        // Try to insert a duplicate (session, seq) by bypassing the API:
        // the PRIMARY KEY must reject it.
        let conn = store.raw_conn();
        let r = conn.execute(
            "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload) VALUES (1, ?1, NULL, 'model_started', '\"streaming\"', 0, NULL)",
            params![s.id.raw() as i64],
        );
        assert!(r.is_err(), "duplicate (session,seq) must be rejected by PK");
    }

    fn event_row_count(store: &Store, sid: SessionId) -> i64 {
        let conn = store.raw_conn();
        conn.query_row(
            "SELECT COUNT(*) FROM event WHERE session_id = ?1",
            params![sid.raw() as i64],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Hostile raw write of the session's event seq: `CHECK (seq > 0)` is
    /// bypassed the way an attacker with file access (or a database written
    /// before the CHECK existed) bypasses it, so the append guard itself must
    /// be what refuses.
    pub(crate) fn seed_only_event_seq(store: &Store, sid: SessionId, seq: i64) {
        let conn = store.raw_conn();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        let changed = conn
            .execute(
                "UPDATE event SET seq = ?2 WHERE session_id = ?1",
                params![sid.raw() as i64, seq],
            )
            .unwrap();
        assert_eq!(changed, 1, "the hostile seed must rewrite the event row");
    }

    /// A refused append must leave the journal and the session row exactly as
    /// they were: no event row appeared and neither `state` nor `updated_ms`
    /// moved.
    fn assert_append_refused_without_mutation(store: &Store, before: &SessionRow, rows: i64) {
        assert_eq!(
            event_row_count(store, before.id),
            rows,
            "a refused append must insert no event row"
        );
        let after = store.get_session(before.id).unwrap().unwrap();
        assert_eq!(after.state, before.state, "session state must be untouched");
        assert_eq!(
            after.updated_ms, before.updated_ms,
            "session updated_ms must be untouched"
        );
    }

    fn hostile_append(store: &Store, sid: SessionId) -> StoreError {
        store
            .append_event(
                sid,
                None,
                EventKind::PromptReceived,
                AgentState::Preparing,
                now_ms(),
                None,
            )
            .unwrap_err()
    }

    #[test]
    fn hostile_i64_max_prev_seq_append_refuses_typed_without_mutation() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        seed_only_event_seq(&store, s.id, i64::MAX);
        match hostile_append(&store, s.id) {
            StoreError::Corrupt(msgs) => assert!(
                msgs.iter().any(|m| m.contains("overflow")),
                "the signed-boundary refusal must name the overflow: {msgs:?}"
            ),
            other => panic!("i64::MAX previous seq must refuse typed Corrupt, got {other:?}"),
        }
        assert_eq!(
            store.last_event_seq(s.id).unwrap().unwrap().raw(),
            i64::MAX as u64,
            "the hostile row itself is untouched, never wrapped"
        );
        assert_append_refused_without_mutation(&store, &s, 1);
    }

    #[test]
    fn hostile_negative_prev_seq_append_refuses_typed_without_mutation() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        seed_only_event_seq(&store, s.id, -1);
        match hostile_append(&store, s.id) {
            StoreError::Corrupt(msgs) => assert!(
                msgs.iter().any(|m| m.contains("overflow")),
                "a -1 seq (the u64 upper half) must refuse as overflow: {msgs:?}"
            ),
            other => panic!("a negative previous seq must refuse typed Corrupt, got {other:?}"),
        }
        assert_eq!(
            store.last_event_seq(s.id).unwrap().unwrap().raw(),
            u64::MAX,
            "-1 decodes as u64::MAX, the exact upper-half read is preserved"
        );
        assert_append_refused_without_mutation(&store, &s, 1);
    }

    #[test]
    fn hostile_zero_prev_seq_append_refuses_typed_without_mutation() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        seed_only_event_seq(&store, s.id, 0);
        match hostile_append(&store, s.id) {
            StoreError::Corrupt(msgs) => assert!(
                msgs.iter().any(|m| m.contains("MAX(seq)")),
                "a zero seq must refuse through the decode context: {msgs:?}"
            ),
            other => panic!("a zero previous seq must refuse typed Corrupt, got {other:?}"),
        }
        assert_append_refused_without_mutation(&store, &s, 1);
    }

    #[test]
    fn seq_at_i64_max_minus_one_appends_once_then_refuses_at_the_boundary() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        seed_only_event_seq(&store, s.id, i64::MAX - 1);
        let seq = store
            .append_event(
                s.id,
                None,
                EventKind::PromptReceived,
                AgentState::Preparing,
                now_ms(),
                None,
            )
            .expect("the last signed seq is still usable");
        assert_eq!(seq.raw(), i64::MAX as u64);
        let before = store.get_session(s.id).unwrap().unwrap();
        match hostile_append(&store, s.id) {
            StoreError::Corrupt(msgs) => assert!(
                msgs.iter().any(|m| m.contains("overflow")),
                "the next append must name the overflow: {msgs:?}"
            ),
            other => panic!("the i64::MAX boundary must refuse typed Corrupt, got {other:?}"),
        }
        assert_append_refused_without_mutation(&store, &before, 2);
    }

    #[test]
    fn hostile_prev_ts_type_append_refuses_typed_without_mutation() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE event SET ts_ms = 'hostile-text' WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        let err = hostile_append(&store, s.id);
        assert!(
            matches!(err, StoreError::Corrupt(_) | StoreError::Sqlite(_)),
            "a non-integer previous ts must refuse typed, never default to 0: {err:?}"
        );
        assert_append_refused_without_mutation(&store, &s, 1);
    }

    #[test]
    fn append_ts_is_monotonic_and_seq_is_gapless() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let seed_ts = store.events_range(s.id, 1, None).unwrap()[0].ts_ms;
        let seq2 = store
            .append_event(
                s.id,
                None,
                EventKind::PromptReceived,
                AgentState::Preparing,
                seed_ts - 60_000,
                None,
            )
            .unwrap();
        assert_eq!(seq2.raw(), 2);
        let seq3 = store
            .append_event(
                s.id,
                None,
                EventKind::ModelStarted,
                AgentState::Streaming,
                seed_ts + 60_000,
                None,
            )
            .unwrap();
        assert_eq!(seq3.raw(), 3);
        let events = store.events_range(s.id, 1, None).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].ts_ms, seed_ts);
        assert_eq!(
            events[1].ts_ms, seed_ts,
            "a backward clock clamps to the previous ts"
        );
        assert_eq!(events[2].ts_ms, seed_ts + 60_000);
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.seq.raw(), (i + 1) as u64, "gapless seq at {i}");
        }
    }

    #[test]
    fn memory_facts_upsert_and_query() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .upsert_memory_fact(s.id, "decision", "framework", "rust")
            .unwrap();
        store
            .upsert_memory_fact(s.id, "decision", "framework", "rust+tokio")
            .unwrap();
        let facts = store.memory_facts(s.id).unwrap();
        assert_eq!(facts.len(), 1, "upsert must not duplicate");
        assert_eq!(facts[0].2, "rust+tokio");
    }

    #[test]
    fn corrupt_event_kind_returns_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload)
                 VALUES (2, ?1, NULL, 'bogus', '\"idle\"', 0, NULL)",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.events_range(s.id, 1, None) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("unknown event kind must error, not panic: {other:?}"),
        }
    }

    #[test]
    fn corrupt_event_state_returns_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload)
                 VALUES (2, ?1, NULL, 'model_started', '\"not_a_state\"', 0, NULL)",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.events_range(s.id, 1, None) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt event state must error, not panic: {other:?}"),
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
    fn expected_state_mismatch_same() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
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
        let mut t = end_transition();
        // The turn machine is mid-turn (Preparing), not Idle.
        t.expected_state = Some(AgentState::Idle);
        let err = store.transition_session(s.id, None, t).unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict(_)),
            "expected state mismatch must be Conflict, got {err:?}"
        );
        // No SessionEnded row, lifecycle still Open, state still Preparing.
        assert_eq!(store.last_event_seq(s.id).unwrap().unwrap().raw(), 2);
        let row = store.get_session(s.id).unwrap().unwrap();
        assert_eq!(row.lifecycle, SessionLifecycle::Open);
        assert_eq!(row.state, AgentState::Preparing);
    }

    #[test]
    fn alloc_op_ids_rejects_zero_count_and_sequence_exhaustion() {
        let (_d, store) = tmp_store();
        let hw0 = store.op_id_seq_high_water().unwrap();
        assert!(store.alloc_op_ids(SessionId::new(1), 0).is_err());
        // A reservation crossing the signed INTEGER column ceiling is
        // refused and writes nothing (checked before the UPDATE).
        let overflow = i64::MAX as u64 - hw0 + 1;
        assert!(store.alloc_op_ids(SessionId::new(1), overflow).is_err());
        assert_eq!(
            store.op_id_seq_high_water().unwrap(),
            hw0,
            "failed reservations must not move the sequence"
        );
    }

    #[test]
    fn op_id_seq_ranges_never_overlap_across_live_instances() {
        // Two LIVE stores over the same file (a restart racing its
        // predecessor before the old connection is gone): every reservation
        // must be an atomic read+update, so no two ranges overlap and the
        // global order is preserved under contention.
        let dir = tempfile::tempdir().unwrap();
        let a = Arc::new(Store::open(dir.path(), true).unwrap());
        let b = Arc::new(Store::open(dir.path(), true).unwrap());
        let ranges = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
        let mut handles = Vec::new();
        for store in [
            a.clone(),
            a.clone(),
            a.clone(),
            b.clone(),
            b.clone(),
            b.clone(),
        ] {
            let ranges = ranges.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    let (start, n) = store.alloc_op_ids(SessionId::new(1), 40).unwrap();
                    ranges.lock().unwrap().push((start, n));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let mut ranges = ranges.lock().unwrap().clone();
        ranges.sort_by_key(|(start, _)| *start);
        assert_eq!(ranges.len(), 150, "6 threads x 25 reservations");
        for w in ranges.windows(2) {
            let (s0, n0) = w[0];
            let (s1, _n1) = w[1];
            assert!(s1 > s0, "reservation starts strictly increase");
            assert!(
                s1 >= s0 + n0,
                "ranges never overlap: [{s0}, {}) vs [{s1}, {})",
                s0 + n0,
                s1 + _n1
            );
        }
    }

    pub(crate) fn prefix_hash(seed: u8) -> [u8; 32] {
        [seed; 32]
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
    fn malformed_prefix_segments_writes_are_refused_before_any_row_lands() {
        // The typed API is the first gate: malformed, oversized, or
        // shape-suspect payloads never touch a row. Every refusal is typed.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let write = |json: &str| {
            store.record_provider_call_with_prefix_segments(
                s.id,
                OpId::new(1),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(1)),
                Some(1),
                None,
                Some(json),
            )
        };
        for (json, what) in [
            ("not json", "garbage"),
            ("{", "truncated"),
            (
                r#"{"segment_hashes":[],"segment_token_counts":[1],"cache_read_tokens":0}"#,
                "length mismatch",
            ),
            (
                r#"{"segment_hashes":["zz"],"segment_token_counts":[1],"cache_read_tokens":0}"#,
                "non-hex digest",
            ),
            (
                r#"{"segment_hashes":[],"segment_token_counts":[],"cache_read_tokens":0,"extra":1}"#,
                "unknown field",
            ),
        ] {
            assert!(
                matches!(write(json), Err(StoreError::Malformed(_))),
                "{what} must be a loud Malformed"
            );
        }
        // Over the byte bound: Oversized, never stored.
        let oversized = format!(
            r#"{{"segment_hashes":[],"segment_token_counts":[],"cache_read_tokens":0,"pad":"{}"}}"#,
            "x".repeat(MAX_PREFIX_SEGMENTS_JSON)
        );
        assert!(matches!(write(&oversized), Err(StoreError::Oversized(_))));
        // 65 segments: over the segment bound.
        let too_many = segments_json(
            MAX_PREFIX_SEGMENTS + 1,
            &vec![1u64; MAX_PREFIX_SEGMENTS + 1],
        );
        assert!(matches!(write(&too_many), Err(StoreError::Malformed(_))));
        // Nothing landed.
        assert!(store.provider_call_prefix_rows(s.id).unwrap().is_empty());
    }

    #[test]
    fn oversized_and_malformed_writes_are_rejected_loudly() {
        // (d) The typed API refuses oversized token counts and malformed
        // stability values BEFORE anything touches the row.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let err = store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(1),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(1)),
                Some(u32::MAX as u64 + 1),
                None,
            )
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Oversized(_)),
            "oversized tokens must be rejected loudly: {err:?}"
        );
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 1.0001] {
            let err = store
                .record_provider_call_with_prefix(
                    s.id,
                    OpId::new(1),
                    "p",
                    "m",
                    "completed",
                    None,
                    None,
                    None,
                    Some(prefix_hash(1)),
                    Some(100),
                    Some(bad),
                )
                .unwrap_err();
            assert!(
                matches!(err, StoreError::Malformed(_)),
                "stability {bad} must be rejected loudly: {err:?}"
            );
        }
        // The u32 boundary itself is accepted.
        store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(2),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(2)),
                Some(u32::MAX as u64),
                Some(0.0),
            )
            .unwrap();
        // Nothing was recorded by the rejected attempts.
        let rows = store.provider_call_prefix_rows(s.id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompt_tokens, u32::MAX);
        assert_eq!(rows[0].prefix_stability, Some(0.0));
    }

    #[test]
    fn quick_check_and_deep_check_agree_on_healthy_stores() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .append_event(
                s.id,
                None,
                EventKind::ModelChunkReceived,
                AgentState::Streaming,
                now_ms(),
                None,
            )
            .unwrap();
        assert!(store.quick_integrity_check().unwrap().is_empty());
        assert!(store.deep_integrity_check().unwrap().is_empty());
        let d = store.diagnostics_quick().unwrap();
        assert_eq!(d["journal_mode"], "wal");
        assert_eq!(d["sessions"], 1);
        assert_eq!(d["integrity"], serde_json::json!([]));
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
    fn batch_hot_writes_commit_in_one_group_and_fsync_before_returning() {
        let (_d, store) = tmp_store();
        let sid = hot_session(&store);
        // 1 event (session seed is seq 1) + 1 message + 2 parts on it + one
        // usage-settlement row.
        let writes = vec![
            HotWrite::AppendEvent {
                session_id: sid,
                op_id: Some(OpId::new(7)),
                kind: EventKind::ModelStarted,
                state: AgentState::Streaming,
                ts_ms: 100,
                payload: None,
                payload_ver: 1,
            },
            HotWrite::PutMessage {
                session_id: sid,
                // The runtime aligns message seqs with journal event seqs.
                seq: 2,
                role: "assistant".into(),
                data: serde_json::json!({ "parts": [] }),
            },
        ];
        let (out, timing) = store.batch_hot_writes(&writes).unwrap();
        assert_eq!(out.len(), 2);
        assert!(timing.commit_us > 0, "fsync commit is measured");
        let out = out;
        let seq = match &out[0] {
            Ok(HotWriteOutcome::EventSeq(s)) => s.raw(),
            other => panic!("expected event seq, got {other:?}"),
        };
        assert_eq!(seq, 2, "second journal event of the session");
        let mid = match &out[1] {
            Ok(HotWriteOutcome::RowId(id)) => *id,
            other => panic!("expected row id, got {other:?}"),
        };
        // The whole group was one transaction: the message and the event are
        // both durable and coherent (message seq == journal seq 2).
        assert!(store.get_session(sid).unwrap().is_some());
        let msgs = store.messages_before(sid, None, 10).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].seq, 2);
        // A follow-up batch can reference the row of an earlier one.
        let parts = vec![
            HotWrite::PutPart {
                message_id: mid,
                kind: "text".into(),
                data: serde_json::json!({ "text": "hello" }),
            },
            HotWrite::RecordProviderCall {
                session_id: sid,
                op_id: OpId::new(7),
                provider: "ollama".into(),
                model: "qwen3.8".into(),
                status: "completed".into(),
                tokens_in: Some(100),
                tokens_out: Some(50),
                error: None,
            },
        ];
        let (out, _) = store.batch_hot_writes(&parts).unwrap();
        assert!(out.iter().all(|r| r.is_ok()), "parts + usage must commit");
        assert_eq!(store.parts_of(mid).unwrap().len(), 1);
        assert_eq!(store.session_usage_tokens(sid).unwrap(), 150);
    }

    #[test]
    fn batch_hot_writes_isolate_a_failing_write_to_its_savepoint() {
        // One hostile write in the group (duplicate (session, seq) message)
        // must fail ONLY itself: neighbors commit, per-write errors are
        // reported, and the writer connection is left transaction-free.
        let (_d, store) = tmp_store();
        let sids = hot_session_sids(&store, 2);
        let writes = vec![
            HotWrite::PutMessage {
                session_id: sids[0],
                seq: 1,
                role: "user".into(),
                data: serde_json::json!({ "text": "a" }),
            },
            // Duplicate (session, seq) on the SAME session: constraint hit.
            HotWrite::PutMessage {
                session_id: sids[0],
                seq: 1,
                role: "user".into(),
                data: serde_json::json!({ "text": "b" }),
            },
            // Foreign key violation: no such message row.
            HotWrite::PutPart {
                message_id: i64::MAX,
                kind: "text".into(),
                data: serde_json::json!({ "text": "orphan" }),
            },
            // Unrelated session must still land.
            HotWrite::PutMessage {
                session_id: sids[1],
                seq: 1,
                role: "user".into(),
                data: serde_json::json!({ "text": "c" }),
            },
        ];
        let (out, _) = store.batch_hot_writes(&writes).unwrap();
        assert!(out[0].is_ok(), "first insert must commit");
        assert!(out[1].is_err(), "duplicate seq must fail in its savepoint");
        assert!(out[2].is_err(), "orphan part must fail in its savepoint");
        assert!(
            out[3].is_ok(),
            "the unrelated session must survive the group"
        );
        assert_eq!(
            store.message_count(sids[0]).unwrap(),
            1,
            "only the first (session, seq) row exists"
        );
        assert_eq!(store.message_count(sids[1]).unwrap(), 1);
        // Empty groups are a no-op and never touch the writer.
        assert_eq!(store.batch_hot_writes(&[]).unwrap().0.len(), 0);
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

    /// One refused typed-ledger append (the hostile callers want the error).
    fn refused_append(store: &Store, session: SessionId) -> StoreError {
        store
            .append_ledger_entry(session, "goal_set", 1, serde_json::json!({"goal": "next"}))
            .unwrap_err()
    }

    fn assert_ledger_corrupt_contains(err: StoreError, needle: &str) {
        match err {
            StoreError::Corrupt(msgs) => assert!(
                msgs.iter().any(|m| m.contains(needle)),
                "the refusal must name {needle:?}: {msgs:?}"
            ),
            other => panic!("must refuse typed Corrupt naming {needle:?}, got {other:?}"),
        }
    }

    /// Hostile raw ledger row: no API can mint a seq at/below the signed
    /// floor, so a tampered database is the only way to seed one.
    fn seed_ledger_entry(store: &Store, session: SessionId, seq: i64) {
        store
            .raw_conn()
            .execute(
                "INSERT INTO ledger_entry(session_id, seq, entry_type, schema_ver, payload, created_ms)
                 VALUES (?1, ?2, 'goal_set', 1, '{\"goal\":\"hostile\"}', 1)",
                params![session.raw() as i64, seq],
            )
            .unwrap();
    }

    #[test]
    fn ledger_entries_append_gapless_and_page_bounded() {
        let (_d, store) = tmp_store();
        let s = sid(&store);
        let seq1 = store
            .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g"}))
            .unwrap();
        let seq2 = store
            .append_ledger_entry(s, "blocker_opened", 1, serde_json::json!({"reason": "r"}))
            .unwrap();
        assert_eq!((seq1, seq2), (1, 2));
        let page = store.ledger_entries(s, None, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].entry_type, "goal_set");
        assert_eq!(page[0].schema_ver, 1);
        assert_eq!(page[0].payload, serde_json::json!({"goal": "g"}));
        let rest = store.ledger_entries(s, Some(1), 10).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].entry_type, "blocker_opened");
        // Sessions are isolated.
        let s2 = sid(&store);
        assert!(store.ledger_entries(s2, None, 10).unwrap().is_empty());
        assert_eq!(store.ledger_max_seq(s2).unwrap(), 0);
    }

    #[test]
    fn ledger_append_refuses_at_the_sqlite_signed_ceiling_without_mutation() {
        let (_d, store) = tmp_store();
        let s = sid(&store);
        // Empty entry table, head checkpoint at i64::MAX: the next typed seq
        // would be i64::MAX + 1, outside SQLite's signed range. The append
        // must refuse typed, insert no row and leave the head untouched.
        store
            .put_ledger_head(s, serde_json::json!({"goal": "g"}), i64::MAX, 1)
            .unwrap();
        assert_ledger_corrupt_contains(refused_append(&store, s), "exhausted");
        assert!(
            store.ledger_entries(s, None, 10).unwrap().is_empty(),
            "a refused append must insert no entry"
        );
        let head = store.ledger_head(s).unwrap().unwrap();
        assert_eq!(head.checkpoint_seq, i64::MAX);
        assert_eq!(head.head_json["goal"], "g");
    }

    #[test]
    fn ledger_append_refuses_at_prev_i64_max_and_corrupt_sequence_bounds() {
        let (_d, store) = tmp_store();
        let s = sid(&store);
        seed_ledger_entry(&store, s, i64::MAX);
        assert_ledger_corrupt_contains(refused_append(&store, s), "exhausted");
        assert_eq!(
            store.ledger_max_seq(s).unwrap(),
            i64::MAX,
            "the hostile row is the only row; the refused append inserted none"
        );

        // prev below 1 is corruption, never a minted non-positive seq.
        let s2 = sid(&store);
        seed_ledger_entry(&store, s2, 0);
        assert_ledger_corrupt_contains(refused_append(&store, s2), "below 1");
        assert_eq!(store.ledger_max_seq(s2).unwrap(), 0, "no row inserted");

        // A negative head checkpoint is corruption too.
        let s3 = sid(&store);
        store
            .put_ledger_head(s3, serde_json::json!({"schema_ver": 1}), -1, 1)
            .unwrap();
        assert_ledger_corrupt_contains(refused_append(&store, s3), "negative");
        assert!(store.ledger_entries(s3, None, 10).unwrap().is_empty());
    }

    #[test]
    fn ledger_page_limits_refuse_above_the_signed_range() {
        let (_d, store) = tmp_store();
        let s = sid(&store);
        store
            .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g"}))
            .unwrap();
        // The largest signed limit is a real bounded page; `None` and the
        // legacy `u64::MAX` sentinel stay the honest unbounded page.
        let pages = [
            store.ledger_entries(s, None, i64::MAX as u64).unwrap(),
            store.ledger_entries_desc(s, None, i64::MAX as u64).unwrap(),
            store.ledger_entries_page(s, None, None).unwrap(),
            store.ledger_entries_desc_page(s, None, None).unwrap(),
            store.ledger_entries(s, None, u64::MAX).unwrap(),
            store.ledger_entries_desc(s, None, u64::MAX).unwrap(),
        ];
        assert!(pages.iter().all(|rows| rows.len() == 1), "{pages:?}");
        // Any OTHER out-of-range limit refuses typed on both APIs.
        let oversized =
            |rows: StoreResult<Vec<LedgerEntryRow>>| matches!(rows, Err(StoreError::Oversized(_)));
        for limit in [i64::MAX as u64 + 1, u64::MAX] {
            assert!(oversized(store.ledger_entries_page(s, None, Some(limit))));
            assert!(oversized(store.ledger_entries_desc_page(
                s,
                None,
                Some(limit)
            )));
        }
        assert!(oversized(store.ledger_entries(
            s,
            None,
            i64::MAX as u64 + 1
        )));
        assert!(oversized(store.ledger_entries_desc(
            s,
            None,
            i64::MAX as u64 + 1
        )));
    }

    #[test]
    fn head_roundtrips_and_compaction_is_atomic_with_head_rewrite() {
        let (_d, store) = tmp_store();
        let s = sid(&store);
        assert!(store.ledger_head(s).unwrap().is_none());
        // A corrupt head blob reads back as an error (the session layer
        // maps that to a rebuild-from-entries).
        store
            .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g"}))
            .unwrap();
        store
            .append_ledger_entry(
                s,
                "decision",
                1,
                serde_json::json!({"step": "s", "choice": "c", "rationale": "r"}),
            )
            .unwrap();
        store
            .append_ledger_entry(s, "routing_decision", 1, serde_json::json!({"turn": 1, "provider": "p", "model": "m", "reasoning": "why", "cost_micro": 7}))
            .unwrap();
        store
            .put_ledger_head(s, serde_json::json!({"schema_ver": 1, "goal": "g"}), 3, 1)
            .unwrap();
        let head = store.ledger_head(s).unwrap().unwrap();
        assert_eq!(head.checkpoint_seq, 3);
        assert_eq!(head.head_json["goal"], "g");
        // Compaction: delete below 3 except the goal (seq 1): only the
        // decision (seq 2) is removed; head rewritten atomically.
        let deleted = store
            .compact_ledger(
                s,
                3,
                &[1],
                serde_json::json!({"schema_ver": 1, "goal": "g", "pruned": true}),
                3,
                1,
            )
            .unwrap();
        assert_eq!(deleted, 1);
        let rows = store.ledger_entries(s, None, 10).unwrap();
        assert_eq!(
            rows.len(),
            2,
            "goal pinned + seq-3 row kept above watermark"
        );
        assert_eq!(rows[0].entry_type, "goal_set");
        assert_eq!(rows[1].entry_type, "routing_decision");
        let head = store.ledger_head(s).unwrap().unwrap();
        assert_eq!(head.head_json["pruned"], true);
        assert_eq!(head.checkpoint_seq, 3);
    }

    #[test]
    fn compaction_protect_list_is_never_evicted() {
        // The never-FIFO-evict rule is enforced in faktor-session; here the
        // STORE contract is: protect rows survive a below-watermark delete.
        let (_d, store) = tmp_store();
        let s = sid(&store);
        for i in 1..=5 {
            store
                .append_ledger_entry(
                    s,
                    "decision",
                    1,
                    serde_json::json!({"step": format!("s{i}"), "choice": "c", "rationale": "r"}),
                )
                .unwrap();
        }
        let deleted = store
            .compact_ledger(s, 6, &[2, 5], serde_json::json!({"schema_ver": 1}), 5, 1)
            .unwrap();
        assert_eq!(deleted, 3);
        let kept: Vec<i64> = store
            .ledger_entries(s, None, 10)
            .unwrap()
            .into_iter()
            .map(|r| r.seq)
            .collect();
        assert_eq!(
            kept,
            vec![2, 5],
            "protected rows survive even below the watermark"
        );
        // A compaction that deletes nothing still rewrites the head.
        let deleted = store
            .compact_ledger(s, 0, &[], serde_json::json!({"schema_ver": 1}), 5, 1)
            .unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(store.ledger_head(s).unwrap().unwrap().checkpoint_seq, 5);
    }

    #[test]
    fn event_payload_versions_stamp_and_read_back() {
        let (_d, store) = tmp_store();
        let s = sid(&store);
        // Legacy append stamps v1 (the historical unversioned writers).
        store
            .append_event(
                s,
                None,
                EventKind::ModelStarted,
                AgentState::Streaming,
                1,
                None,
            )
            .unwrap();
        // Version-aware append stamps its own version.
        store
            .append_event_v(
                s,
                None,
                EventKind::Failed,
                AgentState::FailedRecoverable,
                2,
                Some(serde_json::json!({"message": "x"})),
                2,
            )
            .unwrap();
        let rows = store.events_versioned_range(s, 1, None).unwrap();
        assert_eq!(rows.len(), 3, "seed + two appends");
        assert_eq!(rows[1].1, 1, "legacy writer stamps historical v1");
        assert_eq!(rows[2].1, 2, "versioned writer stamps its schema version");
        assert_eq!(rows[2].0.payload, Some(serde_json::json!({"message": "x"})));
        // The plain reader sees the same events (payload_ver never changes
        // the wire shape).
        let plain = store.events_range(s, 1, None).unwrap();
        assert_eq!(plain.len(), 3);
    }
    // ------------------------------------------------------- index state rows

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

    /// Every u64-backed id type decodes its own full range through the
    /// SQLite bit-cast: u64::MAX, u64::MAX-2 and 2^63 (i64::MIN as raw) all
    /// round-trip exactly, while zero stays typed corruption.
    #[test]
    fn sql_id_bit_cast_round_trips_every_id_type() {
        macro_rules! round_trip {
            ($ty:ty, $raw:expr, $wanted:expr) => {
                assert_eq!(
                    id_field::<$ty>("unit-test id column", $raw).unwrap().raw(),
                    $wanted,
                    "{} must round-trip {}",
                    stringify!($ty),
                    $wanted
                );
                assert!(
                    id_field::<$ty>("unit-test id column", 0).is_err(),
                    "{} zero must stay typed corruption",
                    stringify!($ty)
                );
            };
        }
        for wanted in [u64::MAX, u64::MAX - 2, u64::MAX / 2 + 1] {
            let raw = wanted as i64;
            round_trip!(SessionId, raw, wanted);
            round_trip!(WorkspaceId, raw, wanted);
            round_trip!(WorktreeId, raw, wanted);
            round_trip!(TaskId, raw, wanted);
            round_trip!(TaskRevision, raw, wanted);
            round_trip!(OpId, raw, wanted);
            round_trip!(EventSeq, raw, wanted);
            round_trip!(VerificationRecordId, raw, wanted);
        }
    }

    /// The same persisted `op_id` decode class on the other read paths:
    /// permission, event, tool_run and turn_record rows with a zero `op_id`
    /// refuse typed instead of panicking or minting an id, while negative
    /// bit-cast raws decode back to the exact u64.
    #[test]
    fn corrupt_op_id_columns_refuse_typed_across_read_paths() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let pid = store
            .insert_permission(s.id, OpId::new(3), "fs.write")
            .unwrap()
            .0;
        assert_eq!(
            store.pending_permission(pid).unwrap().unwrap().1,
            OpId::new(3)
        );
        let trid = store
            .start_tool_run(
                s.id,
                OpId::new(4),
                "write_file",
                serde_json::json!({"path": "/a"}),
                serde_json::json!({"strategy": "verify_hash"}),
                None,
                None,
            )
            .unwrap();
        let turid = store
            .start_turn_record(s.id, OpId::new(5), None, None, "p", "m", None)
            .unwrap();
        assert_eq!(
            store.pending_tool_runs(s.id).unwrap()[0].op_id,
            OpId::new(4)
        );
        assert_eq!(
            store.active_turn_record(s.id).unwrap().unwrap().turn_op_id,
            OpId::new(5)
        );
        assert_eq!(store.events_range(s.id, 1, None).unwrap()[0].op_id, None);

        // Zero is structurally invalid on every one of these columns.
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE permission SET op_id = 0 WHERE id = ?1",
                params![pid],
            )
            .unwrap();
            conn.execute(
                "UPDATE event SET op_id = 0 WHERE session_id = ?1 AND seq = 1",
                params![s.id.raw() as i64],
            )
            .unwrap();
            conn.execute("UPDATE tool_run SET op_id = 0 WHERE id = ?1", params![trid])
                .unwrap();
            conn.execute(
                "UPDATE turn_record SET turn_op_id = 0 WHERE id = ?1",
                params![turid],
            )
            .unwrap();
        }
        assert_corrupt_op_id(store.pending_permission(pid), "permission", "cannot be 0");
        assert_corrupt_op_id(store.events_range(s.id, 1, None), "event", "cannot be 0");
        assert_corrupt_op_id(store.pending_tool_runs(s.id), "tool_run", "cannot be 0");
        assert_corrupt_op_id(store.active_turn_record(s.id), "turn_record", "cannot be 0");

        // Negative raw values are the bit-cast upper half: every read path
        // decodes them back to the exact u64 op id (u64::MAX, -3, 2^63).
        for wanted in [u64::MAX, u64::MAX - 2, 2u64.pow(63)] {
            let bad = wanted as i64;
            {
                let conn = store.raw_conn();
                conn.execute(
                    "UPDATE permission SET op_id = ?2 WHERE id = ?1",
                    params![pid, bad],
                )
                .unwrap();
                conn.execute(
                    "UPDATE event SET op_id = ?2 WHERE session_id = ?1 AND seq = 1",
                    params![s.id.raw() as i64, bad],
                )
                .unwrap();
                conn.execute(
                    "UPDATE tool_run SET op_id = ?2 WHERE id = ?1",
                    params![trid, bad],
                )
                .unwrap();
                conn.execute(
                    "UPDATE turn_record SET turn_op_id = ?2 WHERE id = ?1",
                    params![turid, bad],
                )
                .unwrap();
            }
            assert_eq!(
                store.pending_permission(pid).unwrap().unwrap().1.raw(),
                wanted,
                "permission op_id {wanted} must round-trip exactly"
            );
            assert_eq!(
                store.events_range(s.id, 1, None).unwrap()[0]
                    .op_id
                    .unwrap()
                    .raw(),
                wanted,
                "event op_id {wanted} must round-trip exactly"
            );
            assert_eq!(
                store.pending_tool_runs(s.id).unwrap()[0].op_id.raw(),
                wanted,
                "tool_run op_id {wanted} must round-trip exactly"
            );
            assert_eq!(
                store
                    .active_turn_record(s.id)
                    .unwrap()
                    .unwrap()
                    .turn_op_id
                    .raw(),
                wanted,
                "turn_record turn_op_id {wanted} must round-trip exactly"
            );
        }
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
    fn mark_uncertain_records_reason_and_request_id_on_a_dispatched_row() {
        // (v) A post-dispatch failure marks the row UNCERTAIN with the
        // failure reason code and provider request id recorded durably; the
        // row keeps consuming free until the finalize charges the estimate.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        store.cost_task_cap_set(s.id, tid, Some(5_000)).unwrap();
        let now = now_ms();
        let CostReserveOutcome::Granted(r) = store
            .cost_reserve(s.id, tid, OpId::new(9), 2_000, now)
            .unwrap()
        else {
            panic!("reserve granted")
        };
        // mark_uncertain on a never-dispatched row is a typed refusal.
        assert!(matches!(
            store.cost_mark_uncertain(r, "stream_error", Some("req-1"), now),
            Ok(CostReservationState::NotOpen { current }) if current == "reserved"
        ));
        store.cost_mark_dispatched(r, now).unwrap();
        assert_eq!(
            store
                .cost_mark_uncertain(r, "stall_verdict", Some("req-42"), now + 1)
                .unwrap(),
            CostReservationState::Applied
        );
        assert_eq!(
            store.cost_mark_uncertain(r, "x", None, now + 2).unwrap(),
            CostReservationState::NotOpen {
                current: "uncertain".into()
            },
            "an already-uncertain row refuses a second reason (exactly-once reason capture)"
        );
        let rows = store.cost_reservations_of(s.id, tid, 10).unwrap();
        assert_eq!(rows[0].status, "uncertain");
        assert_eq!(
            rows[0].failure_reason_code.as_deref(),
            Some("stall_verdict")
        );
        assert_eq!(rows[0].request_id.as_deref(), Some("req-42"));
        assert_eq!(rows[0].delivery_state.as_deref(), Some("failed"));
        assert_eq!(
            free_micro(&store, s.id, tid),
            3_000,
            "uncertain keeps consuming"
        );
        // The task-completion finalize closes it at the reserved estimate.
        let report = store.cost_finalize_uncertain(s.id, tid, now + 3).unwrap();
        assert_eq!(report.settled, 1);
        assert_eq!(report.charged_micro, 2_000);
        let rows = store.cost_reservations_of(s.id, tid, 10).unwrap();
        assert_eq!(rows[0].status, "settled");
        assert_eq!(
            rows[0].cost_basis.as_deref(),
            Some("ConservativeReservation")
        );
        assert_eq!(rows[0].settled_cost_micro, Some(2_000));
        assert_eq!(rows[0].estimated_cost_micro, Some(2_000));
    }
}
