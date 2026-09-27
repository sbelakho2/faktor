//! `index`: cohesive slice of the mechanically decomposed parent module.

use super::*;

/// One durable per-workspace repository-index state row (schema v12, audits
/// 30/64): the `faktor-index` IndexService persists its WorkspaceIndexState
/// machine here. `state_json` is opaque JSON owned by the index layer (like
/// every other TEXT payload in this schema); `generation` is the numeric
/// generation that row names (0 for NotStarted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStateRow {
    pub workspace_id: WorkspaceId,
    pub state_json: String,
    pub generation: i64,
    pub updated_ms: i64,
}

/// One append-only transition-journal row of a workspace's index state.
/// Written in the SAME transaction as the row it transitions from/to, so
/// the journal can never contradict the current row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStateLogRow {
    pub id: i64,
    pub workspace_id: WorkspaceId,
    /// Legal-machine transition kind owned by the index layer (e.g.
    /// `building`, `ready`, `dirty`, `failed`, `resume`, `corrupt`).
    pub kind: String,
    pub state_json: String,
    pub generation: i64,
    pub updated_ms: i64,
}

pub(crate) fn index_state_map(r: &rusqlite::Row<'_>) -> StoreResult<IndexStateRow> {
    let workspace_raw: i64 = r.get(0)?;
    Ok(IndexStateRow {
        workspace_id: id_field(
            &format!("index_state workspace_id {workspace_raw}"),
            workspace_raw,
        )?,
        state_json: r.get(1)?,
        generation: r.get(2)?,
        updated_ms: r.get(3)?,
    })
}

pub(crate) fn index_state_log_map(r: &rusqlite::Row<'_>) -> StoreResult<IndexStateLogRow> {
    let workspace_raw: i64 = r.get(1)?;
    Ok(IndexStateLogRow {
        id: r.get(0)?,
        workspace_id: id_field(
            &format!("index_state_log workspace_id {workspace_raw}"),
            workspace_raw,
        )?,
        kind: r.get(2)?,
        state_json: r.get(3)?,
        generation: r.get(4)?,
        updated_ms: r.get(5)?,
    })
}

impl Store {
    /// Durable state row of one workspace's repository index (audits 30/64).
    /// `state_json` is an opaque JSON payload owned by `faktor-index`
    /// (protocol-agnostic, like every other TEXT payload here); `generation`
    /// is the numeric generation that row names (0 for NotStarted).
    pub fn index_state_get(&self, workspace_id: WorkspaceId) -> StoreResult<Option<IndexStateRow>> {
        let conn = self.read()?;
        let out = query_row_optional(
            &conn,
            "SELECT workspace_id, state_json, generation, updated_ms
             FROM index_state WHERE workspace_id = ?1",
            params![workspace_id.raw() as i64],
            index_state_map,
        )?;
        Ok(out)
    }

    /// Insert-or-replace the workspace's index state row AND append one
    /// journal row in a single transaction (the row and its journal entry
    /// are never torn apart). Used for corruption recovery and fresh-row
    /// seeding, where no expected value exists.
    pub fn index_state_put(
        &self,
        workspace_id: WorkspaceId,
        state_json: &str,
        generation: i64,
        kind: &str,
    ) -> StoreResult<()> {
        let state_json = state_json.to_owned();
        let kind = kind.to_owned();
        self.writer.execute("index_state_put", move |conn| {
            let now = now_ms();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO index_state(workspace_id, state_json, generation, updated_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(workspace_id) DO UPDATE SET
                state_json = excluded.state_json,
                generation = excluded.generation,
                updated_ms = excluded.updated_ms",
                params![workspace_id.raw() as i64, state_json, generation, now],
            )?;
            tx.execute(
            "INSERT INTO index_state_log(workspace_id, kind, state_json, generation, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![workspace_id.raw() as i64, kind, state_json, generation, now],
        )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Atomic compare-and-swap of the workspace's index state row: the row
    /// is replaced only when a row exists whose `(state_json, generation)`
    /// equal the expected pair exactly. The journal row is appended in the
    /// SAME transaction, so a legal state transition and its journal entry
    /// commit or fail together. Returns `Ok(false)` (writing nothing) when
    /// the row differs — the caller re-reads and re-decides. Two builders
    /// racing for the same generation therefore have exactly one winner.
    pub fn index_state_cas(
        &self,
        workspace_id: WorkspaceId,
        expected_state_json: &str,
        expected_generation: i64,
        new_state_json: &str,
        new_generation: i64,
        kind: &str,
    ) -> StoreResult<bool> {
        let expected_state_json = expected_state_json.to_owned();
        let new_state_json = new_state_json.to_owned();
        let kind = kind.to_owned();
        self.writer.execute("index_state_cas", move |conn| {
            let now = now_ms();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current: Option<(String, i64)> = tx
                .query_row(
                    "SELECT state_json, generation FROM index_state WHERE workspace_id = ?1",
                    params![workspace_id.raw() as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let matched = match current {
                Some((state_json, generation)) => {
                    state_json == expected_state_json && generation == expected_generation
                }
                None => false,
            };
            if !matched {
                return Ok(false); // rollback on drop; nothing written
            }
            tx.execute(
                "UPDATE index_state SET state_json = ?2, generation = ?3, updated_ms = ?4
             WHERE workspace_id = ?1",
                params![
                    workspace_id.raw() as i64,
                    new_state_json,
                    new_generation,
                    now
                ],
            )?;
            tx.execute(
            "INSERT INTO index_state_log(workspace_id, kind, state_json, generation, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                workspace_id.raw() as i64,
                kind,
                new_state_json,
                new_generation,
                now
            ],
        )?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// Append-only transition journal of one workspace (newest first,
    /// bounded page). Read side for recovery forensics and the adversarial
    /// "exactly one rebuild" assertions.
    pub fn index_state_log(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> StoreResult<Vec<IndexStateLogRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, kind, state_json, generation, updated_ms
             FROM index_state_log WHERE workspace_id = ?1
             ORDER BY id DESC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![workspace_id.raw() as i64, limit.max(0)])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(index_state_log_map(row)?);
        }
        Ok(out)
    }

    // ------------------------------------------------------- durable cost ledger
    // (P0-6/12 through attempt-identity accounting, schema v18: the
    // cost_reservation table + the task row's READ-ONLY monetary columns.
    // The task machine never writes these columns — upsert_task enumerates
    // its column list and get_task never selects them — so this section is
    // their ONLY writer and the ledger is the single monetary authority.
    // Every typed refusal leaves the row untouched. The v17 migration (index
    // 17) froze the status vocabulary to reserved | dispatched | settled |
    // refunded | uncertain and made refund-after-dispatch impossible at the
    // SQL level; attempt-keyed rows join provider_call by attempt_op_id.)
}

#[cfg(test)]
mod typed_ledger_tests {
    use super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        (dir, store)
    }

    #[test]
    fn index_state_roundtrip_cas_and_journal() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        // Unknown workspace -> no row.
        assert!(store.index_state_get(ws).unwrap().is_none());
        // Seed via put: row + journal entry in one transaction.
        store
            .index_state_put(ws, r#"{"state":"not_started"}"#, 0, "not_started")
            .unwrap();
        let row = store.index_state_get(ws).unwrap().unwrap();
        assert_eq!(row.state_json, r#"{"state":"not_started"}"#);
        assert_eq!(row.generation, 0);
        // Legal CAS: NotStarted -> Building{1}.
        assert!(store
            .index_state_cas(
                ws,
                r#"{"state":"not_started"}"#,
                0,
                r#"{"state":"building","generation":1}"#,
                1,
                "building",
            )
            .unwrap());
        // CAS with a STALE expected payload writes nothing and is false.
        assert!(
            !store
                .index_state_cas(
                    ws,
                    r#"{"state":"not_started"}"#,
                    0,
                    r#"{"state":"ready","generation":1}"#,
                    1,
                    "ready",
                )
                .unwrap(),
            "stale expected state must not match"
        );
        let row = store.index_state_get(ws).unwrap().unwrap();
        assert_eq!(row.state_json, r#"{"state":"building","generation":1}"#);
        // Journal is append-only and paged newest-first.
        let log = store.index_state_log(ws, 100).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].kind, "building");
        assert_eq!(log[1].kind, "not_started");
        assert_eq!(log[0].generation, 1);
        assert_eq!(log[1].generation, 0);
        // The row is REPLACED by a put (corruption recovery) + journaled.
        store
            .index_state_put(ws, "not json at all", 7, "corrupt")
            .unwrap();
        let row = store.index_state_get(ws).unwrap().unwrap();
        assert_eq!(row.state_json, "not json at all");
        assert_eq!(row.generation, 7);
        assert_eq!(store.index_state_log(ws, 100).unwrap()[0].kind, "corrupt");
    }

    #[test]
    fn index_state_unknown_workspace_cas_is_false_and_get_none() {
        let (_d, store) = tmp_store();
        // No workspace row exists: get is None, CAS false (writes nothing).
        assert!(store
            .index_state_get(WorkspaceId::new(42))
            .unwrap()
            .is_none());
        assert!(!store
            .index_state_cas(
                WorkspaceId::new(42),
                r#"{"state":"not_started"}"#,
                0,
                r#"{"state":"building","generation":1}"#,
                1,
                "building",
            )
            .unwrap());
        // put on a nonexistent workspace violates the FK loudly (the index
        // service only ever attaches workspaces that resolve in the store).
        let err = store.index_state_put(WorkspaceId::new(42), "{}", 0, "x");
        assert!(err.is_err(), "FK must reject unknown workspaces");
    }

    #[test]
    fn index_state_concurrent_cas_has_exactly_one_winner() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        store
            .index_state_put(ws, r#"{"state":"not_started"}"#, 0, "not_started")
            .unwrap();
        let store = std::sync::Arc::new(store);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                store
                    .index_state_cas(
                        ws,
                        r#"{"state":"not_started"}"#,
                        0,
                        r#"{"state":"building","generation":1}"#,
                        1,
                        "building",
                    )
                    .unwrap()
            }));
        }
        let winners = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1, "exactly one CAS winner per transition");
        let row = store.index_state_get(ws).unwrap().unwrap();
        assert_eq!(row.state_json, r#"{"state":"building","generation":1}"#);
        let log = store.index_state_log(ws, 100).unwrap();
        assert_eq!(log.len(), 2, "only the winner journals: {log:?}");
    }

    #[test]
    fn index_state_survives_reopen_and_migrates_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let ws: WorkspaceId;
        {
            let store = Store::open(dir.path(), true).unwrap();
            ws = store.create_workspace("/w").unwrap();
            store
                .index_state_put(ws, r#"{"state":"ready","generation":4}"#, 4, "ready")
                .unwrap();
        }
        // Reopen (daemon restart): the state row + its journal survive.
        let store = Store::open(dir.path(), true).unwrap();
        let row = store.index_state_get(ws).unwrap().unwrap();
        assert_eq!(row.state_json, r#"{"state":"ready","generation":4}"#);
        assert_eq!(row.generation, 4);
        assert_eq!(store.index_state_log(ws, 10).unwrap().len(), 1);
    }
}
