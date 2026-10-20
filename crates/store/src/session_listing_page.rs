//! Cursor-paged, snapshot-bounded History listing (split from `sessions` so
//! the durable session module stays under the source-size ceiling).
//!
//! History orders and pages on the durable `session.listing_revision`
//! sequence, never on wall-clock `updated_ms`: a same-millisecond update gets
//! a strictly larger revision and therefore cannot pass a page walk's
//! snapshot bound while silently moving across its cursor. `updated_ms`
//! remains presentation metadata.

use super::*;
use crate::sessions::session_row_map;

/// The coherent snapshot of one History walk. `revision` is the durable
/// ordering bound; `wall_ms` is preserved as presentation metadata (the
/// first page echoes it so clients can display the walk time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionListingSnapshot {
    pub revision: i64,
    pub wall_ms: i64,
}

impl Store {
    /// Capture the snapshot of a new History walk: the newest durable
    /// `listing_revision` plus the wall clock for presentation. A mutation
    /// that commits after this read receives a strictly larger revision, so
    /// the page walk never re-lists it above the cursor and never serves it
    /// below a later page; the next walk (fresh snapshot) lists it.
    pub fn session_listing_snapshot(&self) -> StoreResult<SessionListingSnapshot> {
        let conn = self.read()?;
        let revision: i64 = conn.query_row(
            "SELECT COALESCE(MAX(listing_revision), 0) FROM session",
            [],
            |row| row.get(0),
        )?;
        Ok(SessionListingSnapshot {
            revision,
            wall_ms: crate::connection::now_ms(),
        })
    }

    /// The durable ordering bound alone (the correctness contract).
    pub fn session_listing_snapshot_revision(&self) -> StoreResult<i64> {
        Ok(self.session_listing_snapshot()?.revision)
    }

    /// Presentation metadata only. Retained for callers that still want the
    /// walk's wall-clock reading; it is never an ordering/paging bound.
    pub fn session_listing_snapshot_ms(&self) -> StoreResult<i64> {
        Ok(self.session_listing_snapshot()?.wall_ms)
    }

    /// [`Self::list_sessions_page`] bound to a History-walk snapshot
    /// revision. `before` is the `(listing_revision, id)` cursor of the LAST
    /// row of the previous page. A `None` snapshot revision keeps the legacy
    /// live-cursor behavior (used only by callers that opt out of a coherent
    /// walk).
    pub fn list_sessions_page_at(
        &self,
        workspace_id: Option<WorkspaceId>,
        before: Option<(i64, i64)>,
        limit: u64,
        snapshot_revision: Option<i64>,
    ) -> StoreResult<Vec<SessionRow>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let bound = i64::try_from(limit.saturating_add(1)).map_err(|_| {
            StoreError::Oversized("session page exceeds SQLite signed range".into())
        })?;
        let conn = self.read()?;
        let mut sql = String::from(
            "SELECT id, workspace_id, worktree_id, task_id, title, provider, model, state, lifecycle, created_ms, updated_ms, listing_revision
             FROM session",
        );
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(workspace) = workspace_id {
            clauses.push("workspace_id = ?");
            params.push(rusqlite::types::Value::Integer(workspace.raw() as i64));
        }
        if let Some(snapshot_revision) = snapshot_revision {
            clauses.push("listing_revision <= ?");
            params.push(rusqlite::types::Value::Integer(snapshot_revision));
        }
        if let Some((listing_revision, id)) = before {
            clauses.push("(listing_revision < ? OR (listing_revision = ? AND id < ?))");
            params.push(rusqlite::types::Value::Integer(listing_revision));
            params.push(rusqlite::types::Value::Integer(listing_revision));
            params.push(rusqlite::types::Value::Integer(id));
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY listing_revision DESC, id DESC LIMIT ?");
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
