//! `session_search`: bounded durable session-title search.

use super::*;

impl Store {
    /// Durable bounded title search over every session (D-04/History): a
    /// case-insensitive substring scan ordered newest-first. Unknown search
    /// is never interpreted as SQL; the query is capped before the statement.
    pub fn search_session_titles(&self, query: &str, limit: u64) -> StoreResult<Vec<SessionRow>> {
        if query.is_empty() || query.len() > 256 {
            return Err(StoreError::Malformed(
                "session title search query must be 1..=256 bytes".into(),
            ));
        }
        let limit = i64::try_from(limit.clamp(1, 1000)).map_err(|_| {
            StoreError::Oversized("session title search page exceeds SQLite signed range".into())
        })?;
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state, lifecycle, created_ms, updated_ms, listing_revision
             FROM session WHERE instr(lower(title), lower(?1)) > 0
             ORDER BY listing_revision DESC, id DESC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![query, limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(session_row_map(row)?);
        }
        Ok(out)
    }
}
