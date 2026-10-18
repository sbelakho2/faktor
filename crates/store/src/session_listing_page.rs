//! Cursor-paged, snapshot-bounded History listing (split from `sessions` so
//! the durable session module stays under the source-size ceiling).

use super::*;
use crate::sessions::session_row_map;

impl Store {
    /// The snapshot bound of a History walk: the newer of the wall clock and
    /// the newest durable `updated_ms`. The first page returns it; every
    /// later page passes it back so an update DURING the walk cannot move a
    /// conversation above the cursor (re-surface it) or below a later page
    /// (skip it). Under this rule a row updated after the snapshot is absent
    /// from the rest of THAT walk and is re-listed by the next walk, which
    /// starts from a fresh snapshot.
    pub fn session_listing_snapshot_ms(&self) -> StoreResult<i64> {
        let conn = self.read()?;
        let newest: Option<i64> =
            conn.query_row("SELECT MAX(updated_ms) FROM session", [], |row| row.get(0))?;
        Ok(newest.unwrap_or(0).max(crate::connection::now_ms()))
    }

    /// [`Self::list_sessions_page`] bound to a History-walk snapshot. A
    /// `None` snapshot keeps the legacy live-cursor behavior (used only by
    /// callers that opt out of a coherent walk).
    pub fn list_sessions_page_at(
        &self,
        workspace_id: Option<WorkspaceId>,
        before: Option<(i64, i64)>,
        limit: u64,
        snapshot_ms: Option<i64>,
    ) -> StoreResult<Vec<SessionRow>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let bound = i64::try_from(limit.saturating_add(1)).map_err(|_| {
            StoreError::Oversized("session page exceeds SQLite signed range".into())
        })?;
        let conn = self.read()?;
        let mut sql = String::from(
            "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state, lifecycle, created_ms, updated_ms
             FROM session",
        );
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(workspace) = workspace_id {
            clauses.push("workspace_id = ?");
            params.push(rusqlite::types::Value::Integer(workspace.raw() as i64));
        }
        if let Some(snapshot_ms) = snapshot_ms {
            clauses.push("updated_ms <= ?");
            params.push(rusqlite::types::Value::Integer(snapshot_ms));
        }
        if let Some((updated_ms, id)) = before {
            clauses.push("(updated_ms < ? OR (updated_ms = ? AND id < ?))");
            params.push(rusqlite::types::Value::Integer(updated_ms));
            params.push(rusqlite::types::Value::Integer(updated_ms));
            params.push(rusqlite::types::Value::Integer(id));
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY updated_ms DESC, id DESC LIMIT ?");
        params.push(rusqlite::types::Value::Integer(bound));
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(session_row_map(row)?);
        }
        Ok(out)
    }
}
