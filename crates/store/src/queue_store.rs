//! Durable steering-queue store methods (F064/F084), split from ledger.

use super::*;

impl Store {
    /// Bounded visible queue of one session (all durable rows, newest seq
    /// last). The queue route projects this exact order; a row's status is
    /// never guessed.
    pub fn queue_rows(&self, session: SessionId, limit: usize) -> StoreResult<Vec<QueuedPrompt>> {
        let conn = self.read()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = conn.prepare(
            "SELECT seq, op_id, prompt, files, model, variant, agent, status, requested_at
             FROM prompt_queue WHERE session_id = ?1 ORDER BY seq ASC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![session.raw() as i64, limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let files_json: String = row.get(3)?;
            let files: Vec<String> = serde_json::from_str(&files_json).map_err(|e| {
                StoreError::Corrupt(vec![format!(
                    "prompt queue files for session {} seq {}: {e}",
                    session.raw(),
                    row.get::<_, i64>(0).unwrap_or(-1)
                )])
            })?;
            out.push(QueuedPrompt {
                queue_seq: row.get(0)?,
                op_id: OpId::new(u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0)),
                prompt: row.get(2)?,
                files,
                model: row.get(4)?,
                variant: row.get(5)?,
                agent: row.get(6)?,
                status: row.get(7)?,
                requested_at: row.get(8)?,
            });
        }
        Ok(out)
    }

    /// One visible queue row by operation id.
    pub fn queue_row_by_op(
        &self,
        session: SessionId,
        op: OpId,
    ) -> StoreResult<Option<QueuedPrompt>> {
        let conn = self.read()?;
        let raw = conn
            .query_row(
                "SELECT seq, op_id, prompt, files, model, variant, agent, status, requested_at
                 FROM prompt_queue WHERE session_id = ?1 AND op_id = ?2",
                params![session.raw() as i64, op.raw() as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
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
            Some((seq, prompt, files_json, model, variant, agent, status, requested_at)) => {
                let files: Vec<String> = serde_json::from_str(&files_json).map_err(|e| {
                    StoreError::Corrupt(vec![format!(
                        "prompt queue files for session {} seq {seq}: {e}",
                        session.raw()
                    )])
                })?;
                Ok(Some(QueuedPrompt {
                    queue_seq: seq,
                    op_id: op,
                    prompt,
                    files,
                    model,
                    variant,
                    agent,
                    status,
                    requested_at,
                }))
            }
            None => Ok(None),
        }
    }

    /// Edit a still-PENDING queued prompt (steering before admission). A
    /// claimed/running row is refused typed: its effects may already exist.
    pub fn edit_queued_prompt(
        &self,
        session: SessionId,
        op: OpId,
        prompt: &str,
        files: &[String],
    ) -> StoreResult<bool> {
        let files = files.to_vec();
        let files_json = crate::tasks::durable_json("prompt queue files", &files)?;
        let prompt = prompt.to_owned();
        self.writer.execute("edit_queued_prompt", move |conn| {
            let changed = conn.execute(
                "UPDATE prompt_queue SET prompt = ?3, files = ?4
                 WHERE session_id = ?1 AND op_id = ?2 AND status = 'pending'",
                params![session.raw() as i64, op.raw() as i64, prompt, files_json],
            )?;
            Ok(changed > 0)
        })
    }
}
