//! Rev8 long-task durability rows: compaction operations, memory policy,
//! instruction bindings and the memory-record CAS/read seams.
//!
//! Every row here is explicitly bounded by its caller; these methods do the
//! durable state transitions only (no task/session policy).

use super::*;

/// One durable compaction operation (S10/K-03/K-04). `state` and `phase` are
/// validated by the session layer before they reach this store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionOperationRow {
    pub session_id: SessionId,
    pub operation_id: String,
    pub state: String,
    pub phase: String,
    pub progress_ppm: i64,
    pub expected_revision: Option<i64>,
    pub manifest_revision: Option<i64>,
    pub before_tokens: Option<i64>,
    pub after_tokens: Option<i64>,
    pub target_tokens: i64,
    pub min_reduction_ppm: i64,
    pub strategy: Option<String>,
    pub result_json: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// The mutable part of one compaction operation transition.
#[derive(Debug, Clone, Copy)]
pub struct CompactionOperationUpdate<'a> {
    pub state: &'a str,
    pub phase: &'a str,
    pub progress_ppm: i64,
    pub before_tokens: Option<i64>,
    pub after_tokens: Option<i64>,
    pub target_tokens: Option<i64>,
    pub manifest_revision: Option<i64>,
    pub strategy: Option<&'a str>,
    pub result_json: Option<&'a str>,
    pub error_code: Option<&'a str>,
    pub error_message: Option<&'a str>,
}

/// One durable memory policy row (S12/O-09), scoped by an opaque
/// `(scope, scope_id)` pair the memory authority owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPolicyRow {
    pub scope: String,
    pub scope_id: i64,
    pub revision: i64,
    pub policy_json: String,
    pub updated_ms: i64,
}

/// The instruction epoch/bundle captured at admission (I-11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionBindingRow {
    pub session_id: SessionId,
    pub instruction_epoch: String,
    pub bundle_hash: String,
    pub turn_op_id: Option<i64>,
    pub captured_ms: i64,
}

impl Store {
    // ------------------------------------------------- memory record reads

    /// Newest `memory_fact` row with this key for the session. The
    /// `(session_id, key)` index makes this one bounded lookup; a V2
    /// memory id is the row key, so every management screen read is indexed.
    pub fn memory_fact_by_key(
        &self,
        session_id: SessionId,
        key: &str,
    ) -> StoreResult<Option<MemoryFactRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT kind, key, value, updated_ms FROM memory_fact
             WHERE session_id = ?1 AND key = ?2
             ORDER BY updated_ms DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64, key])?;
        match rows.next()? {
            Some(row) => Ok(Some(MemoryFactRow {
                kind: row.get(0)?,
                key: row.get(1)?,
                value: row.get(2)?,
                updated_ms: row.get(3)?,
            })),
            None => Ok(None),
        }
    }

    /// Insert one memory fact ONLY when `(session, kind, key)` is absent.
    /// Returns false when a row already exists (the caller re-reads and
    /// applies its dedupe/conflict rule instead of overwriting).
    pub fn insert_memory_fact_if_absent(
        &self,
        session_id: SessionId,
        kind: &str,
        key: &str,
        value: &str,
    ) -> StoreResult<bool> {
        let kind = kind.to_owned();
        let key = key.to_owned();
        let value = value.to_owned();
        let stamp = self.fact_timestamp();
        self.writer
            .execute("insert_memory_fact_if_absent", move |conn| {
                let changed = conn.execute(
                    "INSERT OR IGNORE INTO memory_fact(session_id, kind, key, value, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![session_id.raw() as i64, kind, key, value, stamp],
                )?;
                Ok(changed == 1)
            })
    }

    /// Compare-and-swap one memory fact on the EXACT current stored bytes.
    /// `None` means absent OR changed (the caller re-reads and reports the
    /// typed revision conflict); the comparison and the write share one
    /// transaction, so two concurrent edits can never both win.
    pub fn compare_and_swap_memory_fact(
        &self,
        session_id: SessionId,
        kind: &str,
        key: &str,
        expected_value_hash: &str,
        new_value: &str,
    ) -> StoreResult<Option<MemoryFactRow>> {
        let stored: Option<String> = {
            let conn = self.read()?;
            conn.query_row(
                "SELECT value FROM memory_fact
                 WHERE session_id = ?1 AND kind = ?2 AND key = ?3",
                params![session_id.raw() as i64, kind, key],
                |r| r.get(0),
            )
            .optional()?
        };
        let Some(stored) = stored else {
            return Ok(None);
        };
        // The digest is computed on the caller's thread; the writer closure
        // stays SQL-only. The UPDATE compares the exact stored bytes, so a
        // concurrent change between this read and the write loses the CAS
        // (zero rows) instead of overwriting it.
        let actual = blake3::hash(stored.as_bytes()).to_hex().to_string();
        if actual != expected_value_hash {
            return Ok(None);
        }
        let kind_owned = kind.to_owned();
        let key_owned = key.to_owned();
        let new_value_owned = new_value.to_owned();
        let (kind_in, key_in, value_in) = (
            kind_owned.clone(),
            key_owned.clone(),
            new_value_owned.clone(),
        );
        let stamp = self.fact_timestamp();
        let changed = self
            .writer
            .execute("compare_and_swap_memory_fact", move |conn| {
                let changed = conn.execute(
                    "UPDATE memory_fact SET value = ?4, updated_ms = ?5
                     WHERE session_id = ?1 AND kind = ?2 AND key = ?3 AND value = ?6",
                    params![
                        session_id.raw() as i64,
                        &kind_in,
                        &key_in,
                        &value_in,
                        stamp,
                        &stored
                    ],
                )?;
                Ok(changed == 1)
            })?;
        if !changed {
            return Ok(None);
        }
        Ok(Some(MemoryFactRow {
            kind: kind_owned,
            key: key_owned,
            value: new_value_owned,
            updated_ms: stamp,
        }))
    }

    // ------------------------------------------- workspace memory records

    /// Newest workspace-scoped memory row with this key. Workspace memory is
    /// shared by every session of ONE workspace (Rev8 S12/K-10/K-11); it is
    /// stored outside `memory_fact` so cross-project reads are impossible.
    pub fn workspace_memory_fact_by_key(
        &self,
        workspace_id: u64,
        key: &str,
    ) -> StoreResult<Option<MemoryFactRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT kind, key, value, updated_ms FROM workspace_memory_fact
             WHERE workspace_id = ?1 AND key = ?2
             ORDER BY updated_ms DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![workspace_id as i64, key])?;
        match rows.next()? {
            Some(row) => Ok(Some(MemoryFactRow {
                kind: row.get(0)?,
                key: row.get(1)?,
                value: row.get(2)?,
                updated_ms: row.get(3)?,
            })),
            None => Ok(None),
        }
    }

    /// Upsert one workspace memory row (the V2 writer path).
    pub fn upsert_workspace_memory_fact(
        &self,
        workspace_id: u64,
        kind: &str,
        key: &str,
        value: &str,
    ) -> StoreResult<()> {
        let kind = kind.to_owned();
        let key = key.to_owned();
        let value = value.to_owned();
        let stamp = self.fact_timestamp();
        self.writer
            .execute("upsert_workspace_memory_fact", move |conn| {
                conn.execute(
                    "INSERT INTO workspace_memory_fact(workspace_id, kind, key, value, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(workspace_id, kind, key) DO UPDATE SET
                    value = excluded.value,
                    updated_ms = excluded.updated_ms",
                    params![workspace_id as i64, kind, key, value, stamp],
                )?;
                Ok(())
            })
    }

    /// Insert a workspace memory row ONLY when `(workspace, kind, key)` is
    /// absent. Returns false when a row already exists.
    pub fn insert_workspace_memory_fact_if_absent(
        &self,
        workspace_id: u64,
        kind: &str,
        key: &str,
        value: &str,
    ) -> StoreResult<bool> {
        let kind = kind.to_owned();
        let key = key.to_owned();
        let value = value.to_owned();
        let stamp = self.fact_timestamp();
        self.writer
            .execute("insert_workspace_memory_fact_if_absent", move |conn| {
                let changed = conn.execute(
                    "INSERT OR IGNORE INTO workspace_memory_fact(
                        workspace_id, kind, key, value, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![workspace_id as i64, kind, key, value, stamp],
                )?;
                Ok(changed == 1)
            })
    }

    /// Compare-and-swap one workspace memory row on the EXACT stored bytes.
    pub fn compare_and_swap_workspace_memory_fact(
        &self,
        workspace_id: u64,
        kind: &str,
        key: &str,
        expected_value_hash: &str,
        new_value: &str,
    ) -> StoreResult<Option<MemoryFactRow>> {
        let stored: Option<String> = {
            let conn = self.read()?;
            conn.query_row(
                "SELECT value FROM workspace_memory_fact
                 WHERE workspace_id = ?1 AND kind = ?2 AND key = ?3",
                params![workspace_id as i64, kind, key],
                |r| r.get(0),
            )
            .optional()?
        };
        let Some(stored) = stored else {
            return Ok(None);
        };
        // The digest is computed on the caller's thread; the writer closure
        // stays SQL-only. The UPDATE compares the exact stored bytes, so a
        // concurrent change loses the CAS (zero rows) instead of overwriting.
        let actual = blake3::hash(stored.as_bytes()).to_hex().to_string();
        if actual != expected_value_hash {
            return Ok(None);
        }
        let kind_owned = kind.to_owned();
        let key_owned = key.to_owned();
        let new_value_owned = new_value.to_owned();
        let (kind_in, key_in, value_in) = (
            kind_owned.clone(),
            key_owned.clone(),
            new_value_owned.clone(),
        );
        let stamp = self.fact_timestamp();
        let changed =
            self.writer
                .execute("compare_and_swap_workspace_memory_fact", move |conn| {
                    let changed = conn.execute(
                        "UPDATE workspace_memory_fact SET value = ?4, updated_ms = ?5
                 WHERE workspace_id = ?1 AND kind = ?2 AND key = ?3 AND value = ?6",
                        params![
                            workspace_id as i64,
                            &kind_in,
                            &key_in,
                            &value_in,
                            stamp,
                            &stored
                        ],
                    )?;
                    Ok(changed == 1)
                })?;
        if !changed {
            return Ok(None);
        }
        Ok(Some(MemoryFactRow {
            kind: kind_owned,
            key: key_owned,
            value: new_value_owned,
            updated_ms: stamp,
        }))
    }

    /// Newest-first bounded read of ONE workspace's memory rows executed on
    /// the WRITER connection: the coherence fallback for a pooled reader
    /// that is still inside a pre-write WAL snapshot. The query is the same
    /// indexed page and stays bounded; it is used only when the cached read
    /// returns an empty page, so the steady-state cost is unchanged.
    pub fn workspace_memory_facts_fresh(
        &self,
        workspace_id: u64,
        limit: u64,
    ) -> StoreResult<Vec<MemoryFactRow>> {
        let limit = limit.clamp(1, 500) as i64;
        self.writer
            .execute("workspace_memory_facts_fresh", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT kind, key, value, updated_ms FROM workspace_memory_fact
                     WHERE workspace_id = ?1
                     ORDER BY updated_ms DESC, kind DESC, key DESC LIMIT ?2",
                )?;
                let mut rows = stmt.query(params![workspace_id as i64, limit])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(MemoryFactRow {
                        kind: row.get(0)?,
                        key: row.get(1)?,
                        value: row.get(2)?,
                        updated_ms: row.get(3)?,
                    });
                }
                Ok(out)
            })
    }

    /// Newest-first bounded page over ONE workspace's memory rows.
    pub fn workspace_memory_facts_page(
        &self,
        workspace_id: u64,
        after: Option<&MemoryFactCursor>,
        limit: u64,
    ) -> StoreResult<(Vec<MemoryFactRow>, bool)> {
        let conn = self.read()?;
        let mut sql = String::from(
            "SELECT kind, key, value, updated_ms FROM workspace_memory_fact
             WHERE workspace_id = ?1",
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(5);
        params.push(rusqlite::types::Value::Integer(workspace_id as i64));
        if let Some((ms, kind, key)) = after {
            sql.push_str(
                " AND (updated_ms < ?2 OR (updated_ms = ?2 AND (kind < ?3 OR (kind = ?3 AND key < ?4))))",
            );
            params.push(rusqlite::types::Value::Integer(*ms));
            params.push(rusqlite::types::Value::Text(kind.clone()));
            params.push(rusqlite::types::Value::Text(key.clone()));
        }
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

    /// Newest-first page of ONE workspace fact kind.
    pub fn workspace_memory_facts_by_kind_page(
        &self,
        workspace_id: u64,
        kind: &str,
        after: Option<&MemoryFactCursor>,
        limit: u64,
    ) -> StoreResult<(Vec<MemoryFactRow>, bool)> {
        let conn = self.read()?;
        let mut sql = String::from(
            "SELECT kind, key, value, updated_ms FROM workspace_memory_fact
             WHERE workspace_id = ?1 AND kind = ?2",
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(6);
        params.push(rusqlite::types::Value::Integer(workspace_id as i64));
        params.push(rusqlite::types::Value::Text(kind.to_string()));
        if let Some((ms, kind, key)) = after {
            sql.push_str(
                " AND (updated_ms < ?3 OR (updated_ms = ?3 AND (kind < ?4 OR (kind = ?4 AND key < ?5))))",
            );
            params.push(rusqlite::types::Value::Integer(*ms));
            params.push(rusqlite::types::Value::Text(kind.clone()));
            params.push(rusqlite::types::Value::Text(key.clone()));
        }
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

    pub fn delete_workspace_memory_fact(
        &self,
        workspace_id: u64,
        kind: &str,
        key: &str,
    ) -> StoreResult<usize> {
        let kind = kind.to_owned();
        let key = key.to_owned();
        self.writer
            .execute("delete_workspace_memory_fact", move |conn| {
                Ok(conn.execute(
                    "DELETE FROM workspace_memory_fact
                 WHERE workspace_id = ?1 AND kind = ?2 AND key = ?3",
                    params![workspace_id as i64, &kind, &key],
                )?)
            })
    }

    /// Count of managed V2 memory rows for one session (the `memory_v2:`
    /// namespace only; turn/checkpoint facts share the table).
    pub fn memory_fact_count_v2(&self, session_id: SessionId) -> StoreResult<i64> {
        let conn = self.read()?;
        let out = conn.query_row(
            "SELECT COUNT(*) FROM memory_fact
             WHERE session_id = ?1 AND kind LIKE 'memory_v2:%'",
            params![session_id.raw() as i64],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(out)
    }

    pub fn workspace_memory_fact_count(&self, workspace_id: u64) -> StoreResult<i64> {
        let conn = self.read()?;
        let out = conn.query_row(
            "SELECT COUNT(*) FROM workspace_memory_fact WHERE workspace_id = ?1",
            params![workspace_id as i64],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(out)
    }

    // ---------------------------------------------- compaction operations

    /// Insert the PREPARED row. Returns false when the operation already
    /// exists (idempotent replay), never a duplicate row.
    pub fn insert_compaction_operation(&self, row: CompactionOperationRow) -> StoreResult<bool> {
        self.writer
            .execute("insert_compaction_operation", move |conn| {
                let changed = conn.execute(
                    "INSERT OR IGNORE INTO compaction_operation(
                        session_id, operation_id, state, phase, progress_ppm,
                        expected_revision, manifest_revision, before_tokens,
                        after_tokens, target_tokens, min_reduction_ppm, strategy,
                        result_json, error_code, error_message, created_ms, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                             ?13, ?14, ?15, ?16, ?17)",
                    params![
                        row.session_id.raw() as i64,
                        row.operation_id,
                        row.state,
                        row.phase,
                        row.progress_ppm,
                        row.expected_revision,
                        row.manifest_revision,
                        row.before_tokens,
                        row.after_tokens,
                        row.target_tokens,
                        row.min_reduction_ppm,
                        row.strategy,
                        row.result_json,
                        row.error_code,
                        row.error_message,
                        row.created_ms,
                        row.updated_ms
                    ],
                )?;
                Ok(changed == 1)
            })
    }

    pub fn compaction_operation(
        &self,
        session_id: SessionId,
        operation_id: &str,
    ) -> StoreResult<Option<CompactionOperationRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, operation_id, state, phase, progress_ppm,
                    expected_revision, manifest_revision, before_tokens,
                    after_tokens, target_tokens, min_reduction_ppm, strategy,
                    result_json, error_code, error_message, created_ms, updated_ms
             FROM compaction_operation
             WHERE session_id = ?1 AND operation_id = ?2",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64, operation_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(compaction_operation_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn latest_compaction_operation(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<CompactionOperationRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, operation_id, state, phase, progress_ppm,
                    expected_revision, manifest_revision, before_tokens,
                    after_tokens, target_tokens, min_reduction_ppm, strategy,
                    result_json, error_code, error_message, created_ms, updated_ms
             FROM compaction_operation
             WHERE session_id = ?1
             ORDER BY created_ms DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64])?;
        match rows.next()? {
            Some(row) => Ok(Some(compaction_operation_row(row)?)),
            None => Ok(None),
        }
    }

    /// Advance a non-terminal operation. Terminal rows are immutable (a
    /// late phase write can never resurrect a completed refusal).
    pub fn update_compaction_operation(
        &self,
        session_id: SessionId,
        operation_id: &str,
        update: CompactionOperationUpdate<'_>,
    ) -> StoreResult<Option<CompactionOperationRow>> {
        let operation_id = operation_id.to_owned();
        let state = update.state.to_owned();
        let phase = update.phase.to_owned();
        let strategy = update.strategy.map(str::to_owned);
        let result_json = update.result_json.map(str::to_owned);
        let error_code = update.error_code.map(str::to_owned);
        let error_message = update.error_message.map(str::to_owned);
        let progress_ppm = update.progress_ppm;
        let before_tokens = update.before_tokens;
        let after_tokens = update.after_tokens;
        let target_tokens = update.target_tokens;
        let manifest_revision = update.manifest_revision;
        let now = now_ms();
        self.writer
            .execute("update_compaction_operation", move |conn| {
                let tx = conn.unchecked_transaction()?;
                let changed = tx.execute(
                    "UPDATE compaction_operation SET
                        state = ?3, phase = ?4, progress_ppm = ?5,
                        before_tokens = COALESCE(?6, before_tokens),
                        after_tokens = COALESCE(?7, after_tokens),
                        target_tokens = COALESCE(?8, target_tokens),
                        manifest_revision = COALESCE(?9, manifest_revision),
                        strategy = COALESCE(?10, strategy),
                        result_json = COALESCE(?11, result_json),
                        error_code = ?12, error_message = ?13, updated_ms = ?14
                     WHERE session_id = ?1 AND operation_id = ?2
                       AND state NOT IN ('applied','refused','cancelled','failed','effect_unknown')",
                    params![
                        session_id.raw() as i64,
                        operation_id,
                        state,
                        phase,
                        progress_ppm,
                        before_tokens,
                        after_tokens,
                        target_tokens,
                        manifest_revision,
                        strategy,
                        result_json,
                        error_code,
                        error_message,
                        now
                    ],
                )?;
                if changed == 0 {
                    tx.commit()?;
                    return Ok(None);
                }
                let row = tx
                    .query_row(
                        "SELECT session_id, operation_id, state, phase, progress_ppm,
                                expected_revision, manifest_revision, before_tokens,
                                after_tokens, target_tokens, min_reduction_ppm, strategy,
                                result_json, error_code, error_message, created_ms, updated_ms
                         FROM compaction_operation
                         WHERE session_id = ?1 AND operation_id = ?2",
                        params![session_id.raw() as i64, operation_id],
                        compaction_operation_row,
                    )
                    .optional()?;
                tx.commit()?;
                Ok(row)
            })
    }

    /// Mark a non-terminal operation as `cancelling` (the executor observes
    /// it before every commit and in a real concurrent cancel).
    pub fn request_compaction_cancel(
        &self,
        session_id: SessionId,
        operation_id: &str,
    ) -> StoreResult<Option<CompactionOperationRow>> {
        let operation_id = operation_id.to_owned();
        let now = now_ms();
        self.writer
            .execute("request_compaction_cancel", move |conn| {
                let tx = conn.unchecked_transaction()?;
                let changed = tx.execute(
                    "UPDATE compaction_operation SET state = 'cancelling', phase = 'cancelling',
                        updated_ms = ?3
                     WHERE session_id = ?1 AND operation_id = ?2
                       AND state NOT IN ('applied','refused','cancelled','failed','effect_unknown')",
                    params![session_id.raw() as i64, operation_id, now],
                )?;
                if changed == 0 {
                    tx.commit()?;
                    return Ok(None);
                }
                let row = tx
                    .query_row(
                        "SELECT session_id, operation_id, state, phase, progress_ppm,
                                expected_revision, manifest_revision, before_tokens,
                                after_tokens, target_tokens, min_reduction_ppm, strategy,
                                result_json, error_code, error_message, created_ms, updated_ms
                         FROM compaction_operation
                         WHERE session_id = ?1 AND operation_id = ?2",
                        params![session_id.raw() as i64, operation_id],
                        compaction_operation_row,
                    )
                    .optional()?;
                tx.commit()?;
                Ok(row)
            })
    }

    /// Bound the op history per session: terminal rows older than the newest
    /// `keep` are pruned. Non-terminal rows are never evicted.
    pub fn prune_compaction_operations_keep_newest(
        &self,
        session_id: SessionId,
        keep: usize,
    ) -> StoreResult<usize> {
        let keep = i64::try_from(keep).unwrap_or(i64::MAX).max(1);
        self.writer
            .execute("prune_compaction_operations", move |conn| {
                let deleted = conn.execute(
                    "DELETE FROM compaction_operation
                     WHERE session_id = ?1
                       AND state IN ('applied','refused','cancelled','failed','effect_unknown')
                       AND operation_id NOT IN (
                         SELECT operation_id FROM compaction_operation
                         WHERE session_id = ?1
                         ORDER BY created_ms DESC LIMIT ?2
                       )",
                    params![session_id.raw() as i64, keep],
                )?;
                Ok(deleted)
            })
    }

    // --------------------------------------------------- memory policy

    pub fn memory_policy_get(
        &self,
        scope: &str,
        scope_id: i64,
    ) -> StoreResult<Option<MemoryPolicyRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT scope, scope_id, revision, policy_json, updated_ms
             FROM memory_policy WHERE scope = ?1 AND scope_id = ?2",
        )?;
        let mut rows = stmt.query(params![scope, scope_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(MemoryPolicyRow {
                scope: row.get(0)?,
                scope_id: row.get(1)?,
                revision: row.get(2)?,
                policy_json: row.get(3)?,
                updated_ms: row.get(4)?,
            })),
            None => Ok(None),
        }
    }

    /// Compare-and-swap the policy row. `expected_revision = 0` means "no
    /// row yet"; any other value must equal the current revision. Returns
    /// `None` on a stale revision (the caller reports the typed conflict).
    pub fn memory_policy_cas(
        &self,
        scope: &str,
        scope_id: i64,
        expected_revision: i64,
        policy_json: &str,
        now_ms: i64,
    ) -> StoreResult<Option<MemoryPolicyRow>> {
        let scope = scope.to_owned();
        let policy_json = policy_json.to_owned();
        self.writer.execute("memory_policy_cas", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let current: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM memory_policy WHERE scope = ?1 AND scope_id = ?2",
                    params![&scope, scope_id],
                    |r| r.get(0),
                )
                .optional()?;
            let next = match current {
                None if expected_revision == 0 => 1,
                Some(revision) if revision == expected_revision => revision.saturating_add(1),
                _ => {
                    tx.commit()?;
                    return Ok(None);
                }
            };
            tx.execute(
                "INSERT INTO memory_policy(scope, scope_id, revision, policy_json, updated_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(scope, scope_id) DO UPDATE SET
                        revision = excluded.revision,
                        policy_json = excluded.policy_json,
                        updated_ms = excluded.updated_ms",
                params![&scope, scope_id, next, &policy_json, now_ms],
            )?;
            tx.commit()?;
            Ok(Some(MemoryPolicyRow {
                scope,
                scope_id,
                revision: next,
                policy_json,
                updated_ms: now_ms,
            }))
        })
    }

    // ---------------------------------------------- instruction binding

    pub fn instruction_binding_get(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Option<InstructionBindingRow>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, instruction_epoch, bundle_hash, turn_op_id, captured_ms
             FROM instruction_binding WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64])?;
        match rows.next()? {
            Some(row) => Ok(Some(InstructionBindingRow {
                session_id,
                instruction_epoch: row.get(1)?,
                bundle_hash: row.get(2)?,
                turn_op_id: row.get(3)?,
                captured_ms: row.get(4)?,
            })),
            None => Ok(None),
        }
    }

    pub fn put_instruction_binding(&self, row: InstructionBindingRow) -> StoreResult<()> {
        self.writer.execute("put_instruction_binding", move |conn| {
            conn.execute(
                "INSERT INTO instruction_binding(session_id, instruction_epoch, bundle_hash, turn_op_id, captured_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id) DO UPDATE SET
                    instruction_epoch = excluded.instruction_epoch,
                    bundle_hash = excluded.bundle_hash,
                    turn_op_id = excluded.turn_op_id,
                    captured_ms = excluded.captured_ms",
                params![
                    row.session_id.raw() as i64,
                    row.instruction_epoch,
                    row.bundle_hash,
                    row.turn_op_id,
                    row.captured_ms
                ],
            )?;
            Ok(())
        })
    }
}

fn compaction_operation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CompactionOperationRow> {
    Ok(CompactionOperationRow {
        session_id: SessionId::new(row.get::<_, i64>(0)? as u64),
        operation_id: row.get(1)?,
        state: row.get(2)?,
        phase: row.get(3)?,
        progress_ppm: row.get(4)?,
        expected_revision: row.get(5)?,
        manifest_revision: row.get(6)?,
        before_tokens: row.get(7)?,
        after_tokens: row.get(8)?,
        target_tokens: row.get(9)?,
        min_reduction_ppm: row.get(10)?,
        strategy: row.get(11)?,
        result_json: row.get(12)?,
        error_code: row.get(13)?,
        error_message: row.get(14)?,
        created_ms: row.get(15)?,
        updated_ms: row.get(16)?,
    })
}
