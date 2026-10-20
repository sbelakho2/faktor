//! Durable session-content purge and the audited deletion tombstone (D-06).
//! All hashing/serialization/clock/string work happens on the caller thread;
//! the writer job is transactional SQL only with prebuilt statements.

use rusqlite::{params, OptionalExtension};

use crate::{now_ms, SessionId, Store, StoreError, StoreResult};

/// The audited tombstone of one durable session deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDeletion {
    pub session_id: SessionId,
    pub deleted_ms: i64,
    pub purged: serde_json::Value,
}

const TABLES: &[&str] = &[
    "event",
    "message",
    "task",
    "task_ledger",
    "tool_run",
    "provider_call",
    "checkpoint",
    "artifact",
    "memory_fact",
    "compaction",
    "permission",
    "prompt_queue",
    "loop_signal",
    "turn_record",
    "ledger_entry",
    "ledger_head",
    "cost_reservation",
    "evidence",
    "verification_attempt_changed_file",
    "verification_job_result",
    "verification_job",
    "verification_attempt",
    "child_runtime",
    "attachment",
    "task_admission",
    "prompt_admission",
    "engineering_outcome",
    "compaction_operation",
    "instruction_binding",
];

impl Store {
    /// Durable, transactional session-content purge (Rev8 D-06). The session
    /// row is KEPT as the Closed tombstone; this deletes every message part,
    /// message, journal event, ledger row, attachment reference, checkpoint,
    /// task/proof row and per-session runtime record, then writes the
    /// `session_deletion` audit row in the SAME transaction. Idempotent: a
    /// second call returns the existing tombstone unchanged. A missing
    /// session is a typed NotFound.
    pub fn purge_session_content(
        &self,
        session_id: SessionId,
        reason: &str,
    ) -> StoreResult<SessionDeletion> {
        let sid = session_id.raw() as i64;
        let reason = reason.to_owned();
        // Prepare everything the writer job may not do BEFORE enqueueing:
        // wall clock, hashing, error text and every SQL string.
        let title: Option<String> = {
            let conn = self.read()?;
            conn.query_row(
                "SELECT title FROM session WHERE id = ?1",
                params![sid],
                |r| r.get(0),
            )
            .optional()
            .map_err(StoreError::from)?
        };
        let Some(title) = title else {
            return Err(StoreError::Malformed(format!(
                "session {session_id} not found"
            )));
        };
        let title_digest = blake3::hash(title.as_bytes()).to_hex().to_string();
        let deleted_ms = now_ms();
        let missing = format!("session {session_id} not found");
        let table_sql: Vec<(String, String, String)> = TABLES
            .iter()
            .map(|table| {
                (
                    (*table).to_string(),
                    format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
                    format!("DELETE FROM {table} WHERE session_id = ?1"),
                )
            })
            .collect();
        let deleted_ms = self.writer.execute(
            "purge_session_content",
            move |conn| {
                let tx = conn.unchecked_transaction().map_err(StoreError::from)?;
                if let Some(existing) = tx
                    .query_row(
                        "SELECT deleted_ms FROM session_deletion WHERE session_id = ?1",
                        params![sid],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(StoreError::from)?
                {
                    return Ok(existing);
                }
                let exists: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM session WHERE id = ?1",
                        params![sid],
                        |r| r.get(0),
                    )
                    .map_err(StoreError::from)?;
                if exists != 1 {
                    return Err(StoreError::Malformed(missing));
                }
                for (table, count_sql, _delete_sql) in &table_sql {
                    let count: i64 = tx
                        .query_row(count_sql, params![sid], |r| r.get(0))
                        .map_err(StoreError::from)?;
                    tx.execute(
                        "INSERT INTO session_deletion_purge(session_id, table_name, rows)
                         VALUES (?1, ?2, ?3)",
                        params![sid, table, count],
                    )
                    .map_err(StoreError::from)?;
                }
                // Verification records are SESSION-scoped (schema v43), never
                // task-id-scoped: a numeric task id is not unique across
                // sessions, so deleting "WHERE task_id = ?" could erase a
                // DIFFERENT session's proof rows. Rows whose pre-v43 owner
                // could not be proven stay inert (excluded from every scoped
                // read) rather than being guessed into this purge.
                let proof_rows = tx
                    .execute(
                        "DELETE FROM verification_record WHERE session_id = ?1",
                        params![sid],
                    )
                    .map_err(StoreError::from)? as i64;
                tx.execute(
                    "INSERT INTO session_deletion_purge(session_id, table_name, rows)
                     VALUES (?1, 'verification_record', ?2)",
                    params![sid, proof_rows],
                )
                .map_err(StoreError::from)?;
                // FK-safe order: children first.
                tx.execute(
                    "DELETE FROM part WHERE message_id IN (SELECT id FROM message WHERE session_id = ?1)",
                    params![sid],
                )
                .map_err(StoreError::from)?;
                for (_, _, delete_sql) in &table_sql {
                    tx.execute(delete_sql, params![sid])
                        .map_err(StoreError::from)?;
                }
                tx.execute(
                    "INSERT INTO session_deletion(session_id, title_digest, reason, deleted_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![sid, title_digest, reason, deleted_ms],
                )
                .map_err(StoreError::from)?;
                tx.commit().map_err(StoreError::from)?;
                Ok(deleted_ms)
            },
        )?;
        // A purge removes journal/ledger rows: any cached session-open
        // verification is stale. Bump the epoch even on an idempotent
        // replay (harmless, and never leaves a pre-purge cache alive).
        self.corruption_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok(SessionDeletion {
            session_id,
            deleted_ms,
            purged: self.session_purged_value(session_id)?,
        })
    }

    /// The audited deletion tombstone, when the session was deleted.
    pub fn session_deletion(&self, session_id: SessionId) -> StoreResult<Option<SessionDeletion>> {
        let conn = self.read()?;
        let deleted_ms: Option<i64> = conn
            .query_row(
                "SELECT deleted_ms FROM session_deletion WHERE session_id = ?1",
                params![session_id.raw() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        let Some(deleted_ms) = deleted_ms else {
            return Ok(None);
        };
        drop(conn);
        Ok(Some(SessionDeletion {
            session_id,
            deleted_ms,
            purged: self.session_purged_value(session_id)?,
        }))
    }

    fn session_purged_value(&self, session_id: SessionId) -> StoreResult<serde_json::Value> {
        let conn = self.read()?;
        let mut stmt = conn
            .prepare(
                "SELECT table_name, rows FROM session_deletion_purge
                 WHERE session_id = ?1 ORDER BY table_name",
            )
            .map_err(StoreError::from)?;
        let rows = stmt
            .query_map(params![session_id.raw() as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(StoreError::from)?;
        let mut map = serde_json::Map::new();
        for row in rows {
            let (table, count) = row.map_err(StoreError::from)?;
            map.insert(table, serde_json::json!(count));
        }
        Ok(serde_json::Value::Object(map))
    }
}
