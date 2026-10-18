//! Memory V2 management authority (Rev8 S12/S66/K-10/K-11/O-09).
//!
//! This is the ONLY write path for user/project memory records: it enforces
//! identity dedupe, optimistic versions, tombstones (delete stops FUTURE
//! retrieval without pretending earlier prompt/artifact copies vanished),
//! restore, provenance and a bounded retention pass. Retrieval continues to
//! go through [`crate::render_for_context`], which excludes tombstones by
//! default — a deleted record can never be rendered into a prompt.

use std::sync::Arc;

use faktor_core::id::SessionId;
use faktor_store::Store;

use crate::{
    decode_store_row, MemoryFactCursor, MemoryFactV2, MemoryQuery, MemoryReader, MemoryScope,
    MemoryWriter, Provenance, StoreRepository, TypedMemoryValue, WorkspaceRepository,
    V2_KIND_PREFIX,
};

/// Hard page bound of one management read.
pub const MAX_MEMORY_RECORDS_PAGE: usize = 200;
/// Hard row-scan bound of one management read (paging is fundamental).
pub const MAX_MEMORY_SCAN_ROWS: usize = 4096;
/// Hard bound of one retention pass.
pub const MAX_MEMORY_RETENTION_ROWS: usize = 1000;
/// Hard bound of a memory policy revision value.
pub const MAX_MEMORY_POLICY_DAYS: u32 = 3_650;
/// Hard bound of the record count a policy may retain.
pub const MAX_MEMORY_POLICY_RECORDS: u32 = 100_000;

/// Typed management failures. The server maps these to HTTP codes; none of
/// them is a silent success.
#[derive(Debug)]
pub enum MemoryAdminError {
    NotFound,
    /// A create found a live record with the same identity and a DIFFERENT
    /// value. The caller must explicitly update with the current version.
    Conflict {
        current_version: u32,
        current_value_hash: String,
    },
    /// An update/delete/restore carried a stale expected version.
    VersionMismatch {
        expected: u32,
        actual: u32,
    },
    /// The record is deleted; explicit restore is required.
    Deleted {
        version: u32,
    },
    /// The record is not deleted, so restore is meaningless.
    NotDeleted {
        version: u32,
    },
    InvalidScope(String),
    Malformed(String),
    Oversized(String),
    /// The workspace memory policy's `max_records` would be exceeded by a
    /// new record. Dedupe/update/restore paths are never refused by it.
    PolicyLimit {
        scope: String,
        max_records: u32,
    },
    Store(faktor_store::StoreError),
}

impl MemoryAdminError {
    pub fn code(&self) -> &'static str {
        match self {
            MemoryAdminError::NotFound => "memory_not_found",
            MemoryAdminError::Conflict { .. } => "memory_conflict",
            MemoryAdminError::VersionMismatch { .. } => "memory_version_conflict",
            MemoryAdminError::Deleted { .. } => "memory_deleted",
            MemoryAdminError::NotDeleted { .. } => "memory_not_deleted",
            MemoryAdminError::InvalidScope(_) => "memory_scope_invalid",
            MemoryAdminError::Malformed(_) => "memory_malformed",
            MemoryAdminError::Oversized(_) => "memory_oversized",
            MemoryAdminError::PolicyLimit { .. } => "memory_policy_limit",
            MemoryAdminError::Store(_) => "memory_store_unavailable",
        }
    }
}

impl std::fmt::Display for MemoryAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryAdminError::NotFound => write!(f, "memory record not found"),
            MemoryAdminError::Conflict {
                current_version, ..
            } => write!(
                f,
                "a live memory with this identity exists at version {current_version}"
            ),
            MemoryAdminError::VersionMismatch { expected, actual } => write!(
                f,
                "memory version conflict: expected {expected}, current {actual}"
            ),
            MemoryAdminError::Deleted { version } => {
                write!(f, "memory record is deleted at version {version}")
            }
            MemoryAdminError::NotDeleted { version } => {
                write!(f, "memory record is live at version {version}")
            }
            MemoryAdminError::InvalidScope(message)
            | MemoryAdminError::Malformed(message)
            | MemoryAdminError::Oversized(message) => write!(f, "{message}"),
            MemoryAdminError::PolicyLimit { scope, max_records } => write!(
                f,
                "the {scope} memory policy limit of {max_records} records would be exceeded"
            ),
            MemoryAdminError::Store(error) => write!(f, "memory store error: {error}"),
        }
    }
}

impl std::error::Error for MemoryAdminError {}

impl From<faktor_store::StoreError> for MemoryAdminError {
    fn from(value: faktor_store::StoreError) -> Self {
        MemoryAdminError::Store(value)
    }
}

impl From<crate::MemoryError> for MemoryAdminError {
    fn from(value: crate::MemoryError) -> Self {
        match value {
            crate::MemoryError::Store(error) => MemoryAdminError::Store(error),
            crate::MemoryError::Malformed(message) => MemoryAdminError::Malformed(message),
            crate::MemoryError::Oversized(message) => MemoryAdminError::Oversized(message),
        }
    }
}

/// One create/update/restore result. `deduped` means an identical live
/// record already existed (no write happened); `restored` means a tombstone
/// was revived under the same identity.
#[derive(Debug, Clone)]
pub struct MemoryWriteResult {
    pub record: MemoryFactV2,
    pub created: bool,
    pub deduped: bool,
    pub restored: bool,
}

/// One management page. `records` are newest-first by the V2 total order.
#[derive(Debug, Clone)]
pub struct MemoryRecordPage {
    pub records: Vec<MemoryFactV2>,
    pub size: usize,
    /// Resume cursor for the NEXT page (row order key, not the JSON
    /// `updated_ms` the caller supplied).
    pub cursor: Option<MemoryFactCursor>,
    /// The row cursor of the LAST returned record, even on the final page.
    pub last_cursor: Option<MemoryFactCursor>,
    pub has_more: bool,
    pub scanned: usize,
}

/// The durable receipt of a delete: future use stops at `at_revision`, while
/// earlier prompts/artifacts/logs are explicitly NOT claimed erased.
#[derive(Debug, Clone)]
pub struct MemoryDeleteReceipt {
    pub record: MemoryFactV2,
    pub deleted_value_hash: String,
    pub at_revision: u64,
    pub retained_prior_copies: bool,
}

/// One retention plan (dry-run and apply share the same selection).
#[derive(Debug, Clone)]
pub struct MemoryRetentionPlan {
    pub cutoff_ms: i64,
    pub records: Vec<MemoryFactV2>,
    pub scanned: usize,
    pub truncated: bool,
}

/// Which durable table owns a record: the per-session `memory_fact` table
/// or the workspace-shared `workspace_memory_fact` table (v37).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FactOwner {
    Session,
    Workspace,
}

/// The management authority over one session's V2 memory rows. Session
/// scope is private to that session; workspace/project scope is shared by
/// every session of ONE workspace through the separate workspace table.
pub struct MemoryRecordStore {
    store: Arc<Store>,
    session: SessionId,
    workspace: u64,
}

/// A validated create draft. Identity is `(scope, subject, predicate)`.
#[derive(Debug, Clone)]
pub struct NewMemoryRecord {
    pub scope: MemoryScope,
    pub subject: String,
    pub predicate: String,
    pub value: TypedMemoryValue,
    pub provenance: Provenance,
    pub confidence_ppm: u32,
    pub evidence: Vec<String>,
    pub source_revision: Option<String>,
}

impl MemoryRecordStore {
    pub fn new(store: Arc<Store>, session: SessionId) -> Self {
        let workspace = store
            .get_session(session)
            .ok()
            .flatten()
            .map(|row| row.workspace_id.raw())
            .unwrap_or(0);
        Self {
            store,
            session,
            workspace,
        }
    }

    /// Explicit workspace identity (tests/callers that already own it).
    pub fn with_workspace(store: Arc<Store>, session: SessionId, workspace: u64) -> Self {
        Self {
            store,
            session,
            workspace,
        }
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Map a requested scope to its owning table, refusing an arbitrary
    /// cross-session or cross-workspace id (K-11: no project may select a
    /// global memory location).
    fn owner_for_scope(&self, scope: &MemoryScope) -> Result<FactOwner, MemoryAdminError> {
        match scope {
            MemoryScope::Session(id) if *id == self.session.raw().to_string() => {
                Ok(FactOwner::Session)
            }
            MemoryScope::Workspace(id) | MemoryScope::Project(id)
                if self.workspace != 0 && *id == self.workspace.to_string() =>
            {
                Ok(FactOwner::Workspace)
            }
            other => Err(MemoryAdminError::InvalidScope(format!(
                "scope {}:{} is not writable from this session",
                other.kind(),
                other.id()
            ))),
        }
    }

    fn owner_scope(&self, owner: FactOwner) -> MemoryScope {
        match owner {
            FactOwner::Session => MemoryScope::session_scope(self.session),
            FactOwner::Workspace => MemoryScope::Workspace(self.workspace.to_string()),
        }
    }

    /// ONE indexed record by its durable id (never a table scan).
    pub fn get(&self, id: &str) -> Result<Option<MemoryFactV2>, MemoryAdminError> {
        Ok(self.get_raw(id)?.map(|(fact, _, _)| fact))
    }

    /// The record plus the EXACT stored JSON it was decoded from and the
    /// owning table. The raw bytes are the CAS token: an edit can never hash
    /// a newer value and then overwrite it. An id is globally derived from
    /// `(scope, subject, predicate)`, so a lookup can safely try the session
    /// table and then this session's workspace table.
    fn get_raw(
        &self,
        id: &str,
    ) -> Result<Option<(MemoryFactV2, String, FactOwner)>, MemoryAdminError> {
        if id.is_empty() || id.len() > 128 {
            return Err(MemoryAdminError::Malformed(
                "memory id must be 1..=128 bytes".into(),
            ));
        }
        for (owner, row) in [
            (
                FactOwner::Session,
                self.store.memory_fact_by_key(self.session, id)?,
            ),
            (
                FactOwner::Workspace,
                if self.workspace == 0 {
                    None
                } else {
                    self.store
                        .workspace_memory_fact_by_key(self.workspace, id)?
                },
            ),
        ] {
            let Some(row) = row else { continue };
            if !row.kind.starts_with(V2_KIND_PREFIX) {
                continue;
            }
            let raw = row.value.clone();
            let decoded = decode_store_row(row, &self.owner_scope(owner));
            if let Some(fact) = decoded.map(|d| d.fact).filter(|f| f.id == id) {
                return Ok(Some((fact, raw, owner)));
            }
        }
        Ok(None)
    }

    /// One bounded management page of ONE exact scope.
    pub fn list(
        &self,
        scope: &MemoryScope,
        include_deleted: bool,
        after: Option<&MemoryFactCursor>,
        limit: usize,
    ) -> Result<MemoryRecordPage, MemoryAdminError> {
        let owner = self.owner_for_scope(scope)?;
        let limit = limit.clamp(1, MAX_MEMORY_RECORDS_PAGE);
        let mut cursor = after.cloned();
        let mut records: Vec<MemoryFactV2> = Vec::with_capacity(limit);
        let mut scanned = 0usize;
        let mut more = false;
        let mut last_cursor: Option<MemoryFactCursor> = None;
        'pages: loop {
            let remaining = MAX_MEMORY_SCAN_ROWS.saturating_sub(scanned);
            if remaining == 0 {
                more = true;
                break;
            }
            let page = (limit.saturating_sub(records.len()).max(1))
                .min(remaining)
                .min(MAX_MEMORY_RECORDS_PAGE) as u64;
            let (rows, has_more) = match owner {
                FactOwner::Session => {
                    self.store
                        .memory_facts_v2_page(self.session, cursor.as_ref(), page)?
                }
                FactOwner::Workspace => {
                    self.store
                        .workspace_memory_facts_v2_page(self.workspace, cursor.as_ref(), page)?
                }
            };
            if rows.is_empty() {
                break;
            }
            scanned += rows.len();
            for row in &rows {
                cursor = Some((row.updated_ms, row.kind.clone(), row.key.clone()));
                if !row.kind.starts_with(V2_KIND_PREFIX) {
                    continue;
                }
                let Some(decoded) = decode_store_row(row.clone(), &self.owner_scope(owner)) else {
                    continue;
                };
                let fact = decoded.fact;
                if &fact.scope != scope || (!include_deleted && !fact.is_live()) {
                    continue;
                }
                records.push(fact);
                last_cursor = Some((row.updated_ms, row.kind.clone(), row.key.clone()));
                if records.len() >= limit {
                    more = true;
                    break 'pages;
                }
            }
            if !has_more {
                break;
            }
        }
        let cursor = if more { last_cursor.clone() } else { None };
        Ok(MemoryRecordPage {
            records,
            size: limit,
            cursor,
            last_cursor,
            has_more: more,
            scanned,
        })
    }

    /// Create one record with exact identity dedupe. A live identical value
    /// is a true no-op (`deduped`); a live DIFFERENT value is a typed
    /// conflict requiring an explicit versioned update; a tombstone is
    /// revived under the same identity.
    pub fn create(
        &self,
        draft: NewMemoryRecord,
        now_ms: i64,
    ) -> Result<MemoryWriteResult, MemoryAdminError> {
        self.create_bounded(draft, now_ms, u32::MAX)
    }

    /// Create with the S12 policy's `max_records` bound enforced before the
    /// insert. Dedupe, tombstone revive and updates are never refused: the
    /// limit applies only to a genuinely new record. The count is bounded by
    /// the same page/scan limits as management reads.
    pub fn create_bounded(
        &self,
        draft: NewMemoryRecord,
        now_ms: i64,
        max_records: u32,
    ) -> Result<MemoryWriteResult, MemoryAdminError> {
        let mut fact = MemoryFactV2::new(
            draft.scope.clone(),
            draft.subject,
            draft.predicate,
            draft.value.clone(),
            now_ms,
        )
        .with_provenance(draft.provenance)
        .with_confidence(draft.confidence_ppm)
        .with_evidence(draft.evidence);
        if let Some(revision) = draft.source_revision {
            fact = fact.with_source_revision(revision);
        }
        fact.validate()?;
        let owner = self.owner_for_scope(&fact.scope)?;
        let kind = fact.row_kind();
        if let Some((existing, raw, existing_owner)) = self.get_raw(&fact.id)? {
            if existing_owner != owner {
                return Err(MemoryAdminError::Conflict {
                    current_version: existing.version,
                    current_value_hash: revision_hash(&raw),
                });
            }
            if existing.is_live() {
                if same_value(&existing.value, &fact.value)? {
                    return Ok(MemoryWriteResult {
                        record: existing,
                        created: false,
                        deduped: true,
                        restored: false,
                    });
                }
                return Err(MemoryAdminError::Conflict {
                    current_version: existing.version,
                    current_value_hash: revision_hash(&raw),
                });
            }
            // Revive the tombstone under the SAME identity with the new
            // value/provenance; the delete remains in the prior revision.
            fact.created_ms = existing.created_ms;
            fact.version = existing.version.saturating_add(1);
            fact.previous = Some(Box::new(crate::MemoryRevision {
                version: existing.version,
                value: existing.value.clone(),
                updated_ms: existing.updated_ms,
            }));
            self.cas_write(existing_owner, &raw, &fact)?;
            return Ok(MemoryWriteResult {
                record: fact,
                created: false,
                deduped: false,
                restored: true,
            });
        }
        if max_records != u32::MAX {
            let count = self.count_scope(&fact.scope)?;
            if count >= u64::from(max_records) {
                return Err(MemoryAdminError::PolicyLimit {
                    scope: fact.scope.key(),
                    max_records,
                });
            }
        }
        let raw = serde_json::to_string(&fact)
            .map_err(|e| MemoryAdminError::Malformed(format!("memory serialization: {e}")))?;
        let inserted = match owner {
            FactOwner::Session => {
                self.store
                    .insert_memory_fact_if_absent(self.session, &kind, &fact.id, &raw)?
            }
            FactOwner::Workspace => self.store.insert_workspace_memory_fact_if_absent(
                self.workspace,
                &kind,
                &fact.id,
                &raw,
            )?,
        };
        if !inserted {
            // A concurrent create won: apply the same dedupe/conflict rule.
            return self.create_from_existing(fact, now_ms);
        }
        Ok(MemoryWriteResult {
            record: fact,
            created: true,
            deduped: false,
            restored: false,
        })
    }

    fn create_from_existing(
        &self,
        fact: MemoryFactV2,
        _now_ms: i64,
    ) -> Result<MemoryWriteResult, MemoryAdminError> {
        let Some((existing, raw, _)) = self.get_raw(&fact.id)? else {
            return Err(MemoryAdminError::Store(faktor_store::StoreError::Conflict(
                "concurrent memory create disappeared".into(),
            )));
        };
        if existing.is_live() && same_value(&existing.value, &fact.value)? {
            return Ok(MemoryWriteResult {
                record: existing,
                created: false,
                deduped: true,
                restored: false,
            });
        }
        Err(MemoryAdminError::Conflict {
            current_version: existing.version,
            current_value_hash: revision_hash(&raw),
        })
    }

    /// Edit one live record. `expected_version` is mandatory: a stale editor
    /// never clobbers a newer value.
    pub fn update(
        &self,
        id: &str,
        expected_version: u32,
        value: TypedMemoryValue,
        now_ms: i64,
    ) -> Result<MemoryWriteResult, MemoryAdminError> {
        let (mut fact, raw, owner) = self.get_raw(id)?.ok_or(MemoryAdminError::NotFound)?;
        if fact.deleted.is_some() {
            return Err(MemoryAdminError::Deleted {
                version: fact.version,
            });
        }
        check_version(fact.version, expected_version)?;
        fact.previous = Some(Box::new(crate::MemoryRevision {
            version: fact.version,
            value: fact.value.clone(),
            updated_ms: fact.updated_ms,
        }));
        fact.value = value;
        fact.version = fact.version.saturating_add(1);
        fact.updated_ms = now_ms.max(fact.updated_ms);
        fact.validate()?;
        self.cas_write(owner, &raw, &fact)?;
        Ok(MemoryWriteResult {
            record: fact,
            created: false,
            deduped: false,
            restored: false,
        })
    }

    /// Delete one live record from FUTURE retrieval (tombstone). Earlier
    /// prompt/artifact copies are retained and disclosed by the receipt.
    pub fn delete(
        &self,
        id: &str,
        expected_version: u32,
        reason: Option<String>,
        at_revision: u64,
        now_ms: i64,
    ) -> Result<MemoryDeleteReceipt, MemoryAdminError> {
        let (mut fact, raw, owner) = self.get_raw(id)?.ok_or(MemoryAdminError::NotFound)?;
        if fact.deleted.is_some() {
            return Err(MemoryAdminError::Deleted {
                version: fact.version,
            });
        }
        check_version(fact.version, expected_version)?;
        let deleted_value_hash = revision_hash(
            &serde_json::to_string(&fact.value)
                .map_err(|e| MemoryAdminError::Malformed(format!("memory value: {e}")))?,
        );
        fact.previous = Some(Box::new(crate::MemoryRevision {
            version: fact.version,
            value: fact.value.clone(),
            updated_ms: fact.updated_ms,
        }));
        fact.deleted = Some(crate::MemoryDeletion {
            deleted_ms: now_ms,
            reason,
            at_revision,
        });
        fact.version = fact.version.saturating_add(1);
        fact.updated_ms = now_ms.max(fact.updated_ms);
        fact.validate()?;
        self.cas_write(owner, &raw, &fact)?;
        Ok(MemoryDeleteReceipt {
            record: fact,
            deleted_value_hash,
            at_revision,
            retained_prior_copies: true,
        })
    }

    /// Revive a tombstoned record (S12 restore). The prior value stays
    /// visible as the previous revision.
    pub fn restore(
        &self,
        id: &str,
        expected_version: u32,
        now_ms: i64,
    ) -> Result<MemoryWriteResult, MemoryAdminError> {
        let (mut fact, raw, owner) = self.get_raw(id)?.ok_or(MemoryAdminError::NotFound)?;
        if fact.deleted.is_none() {
            return Err(MemoryAdminError::NotDeleted {
                version: fact.version,
            });
        }
        check_version(fact.version, expected_version)?;
        fact.deleted = None;
        fact.version = fact.version.saturating_add(1);
        fact.updated_ms = now_ms.max(fact.updated_ms);
        fact.validate()?;
        self.cas_write(owner, &raw, &fact)?;
        Ok(MemoryWriteResult {
            record: fact,
            created: false,
            deduped: false,
            restored: true,
        })
    }

    /// Plan (or execute) a bounded retention pass over TOMBSTONES older than
    /// `cutoff_ms` in one exact scope. Live records are never selected.
    pub fn retention(
        &self,
        scope: &MemoryScope,
        cutoff_ms: i64,
        limit: usize,
        apply: bool,
    ) -> Result<MemoryRetentionPlan, MemoryAdminError> {
        let limit = limit.clamp(1, MAX_MEMORY_RETENTION_ROWS);
        let mut selected: Vec<MemoryFactV2> = Vec::new();
        let mut scanned = 0usize;
        let mut truncated = false;
        let mut cursor: Option<MemoryFactCursor> = None;
        loop {
            if selected.len() >= limit || scanned >= MAX_MEMORY_SCAN_ROWS {
                truncated = true;
                break;
            }
            let page = self.list(scope, true, cursor.as_ref(), MAX_MEMORY_RECORDS_PAGE)?;
            scanned = scanned.saturating_add(page.scanned);
            for fact in &page.records {
                if let Some(deleted) = &fact.deleted {
                    if deleted.deleted_ms < cutoff_ms && selected.len() < limit {
                        selected.push(fact.clone());
                    }
                }
            }
            if !page.has_more {
                break;
            }
            cursor = page.cursor;
        }
        if apply {
            for fact in &selected {
                match self.owner_for_scope(&fact.scope)? {
                    FactOwner::Session => {
                        self.store
                            .delete_memory_fact(self.session, &fact.row_kind(), &fact.id)?;
                    }
                    FactOwner::Workspace => {
                        self.store.delete_workspace_memory_fact(
                            self.workspace,
                            &fact.row_kind(),
                            &fact.id,
                        )?;
                    }
                }
            }
        }
        Ok(MemoryRetentionPlan {
            cutoff_ms,
            records: selected,
            scanned,
            truncated,
        })
    }

    fn cas_write(
        &self,
        owner: FactOwner,
        expected_raw: &str,
        next: &MemoryFactV2,
    ) -> Result<(), MemoryAdminError> {
        let expected = revision_hash(expected_raw);
        let value = serde_json::to_string(next)
            .map_err(|e| MemoryAdminError::Malformed(format!("memory serialization: {e}")))?;
        let written = match owner {
            FactOwner::Session => self.store.compare_and_swap_memory_fact(
                self.session,
                &next.row_kind(),
                &next.id,
                &expected,
                &value,
            )?,
            FactOwner::Workspace => self.store.compare_and_swap_workspace_memory_fact(
                self.workspace,
                &next.row_kind(),
                &next.id,
                &expected,
                &value,
            )?,
        };
        if written.is_none() {
            let current = self.get(&next.id)?;
            return Err(match current {
                Some(current) => MemoryAdminError::VersionMismatch {
                    expected: next.version.saturating_sub(1),
                    actual: current.version,
                },
                None => MemoryAdminError::NotFound,
            });
        }
        Ok(())
    }

    /// Exact number of managed V2 records in one scope. Session rows are
    /// filtered to the `memory_v2:` namespace (turn/checkpoint facts live in
    /// the same table); the workspace table holds only V2 rows.
    pub fn count_scope(&self, scope: &MemoryScope) -> Result<u64, MemoryAdminError> {
        match self.owner_for_scope(scope)? {
            FactOwner::Session => Ok(
                u64::try_from(self.store.memory_fact_count_v2(self.session)?).unwrap_or(u64::MAX),
            ),
            FactOwner::Workspace => Ok(u64::try_from(
                self.store.workspace_memory_fact_count(self.workspace)?,
            )
            .unwrap_or(u64::MAX)),
        }
    }

    /// A bounded retrieval probe for tests/evidence: render-eligible rows of
    /// the session scope must exclude every tombstone.
    pub fn live_scope_records(
        &self,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemoryFactV2>, MemoryAdminError> {
        let query = MemoryQuery::typed(scope.clone());
        let bounded = match self.owner_for_scope(scope)? {
            FactOwner::Session => {
                let repo = StoreRepository::new(self.store.clone(), self.session);
                repo.facts_bounded(&query, limit, crate::MAX_BOUNDED_BYTES)?
            }
            FactOwner::Workspace => {
                let repo = WorkspaceRepository::new(self.store.clone(), self.workspace);
                repo.facts_bounded(&query, limit, crate::MAX_BOUNDED_BYTES)?
            }
        };
        Ok(bounded.facts)
    }
}

fn check_version(actual: u32, expected: u32) -> Result<(), MemoryAdminError> {
    if actual == expected {
        Ok(())
    } else {
        Err(MemoryAdminError::VersionMismatch { expected, actual })
    }
}

fn same_value(a: &TypedMemoryValue, b: &TypedMemoryValue) -> Result<bool, MemoryAdminError> {
    let a = serde_json::to_value(a)
        .map_err(|e| MemoryAdminError::Malformed(format!("memory value encoding: {e}")))?;
    let b = serde_json::to_value(b)
        .map_err(|e| MemoryAdminError::Malformed(format!("memory value encoding: {e}")))?;
    Ok(a == b)
}

/// Canonical revision hash of one stored value (the CAS token the UI
/// receives as `revisionHash`).
pub fn revision_hash(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

// ---------------------------------------------------------------- policy

/// New-memory capture policy (K-10). Failure learning is a SEPARATE switch
/// and is never reinterpreted as general auto-memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCapture {
    Off,
    Manual,
    Controlled,
}

impl AutoCapture {
    pub fn as_str(self) -> &'static str {
        match self {
            AutoCapture::Off => "off",
            AutoCapture::Manual => "manual",
            AutoCapture::Controlled => "controlled",
        }
    }
}

/// The versioned S12 memory policy. Defaults are the safe repository
/// default: manual saves only, failure learning on, 90-day tombstone
/// retention, session+workspace scopes, no cross-project user memory.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MemoryPolicy {
    pub revision: u64,
    pub auto_capture: AutoCapture,
    pub retention_days: u32,
    pub max_records: u32,
    pub allow_session_scope: bool,
    pub allow_workspace_scope: bool,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            revision: 0,
            auto_capture: AutoCapture::Manual,
            retention_days: 90,
            max_records: 5_000,
            allow_session_scope: true,
            allow_workspace_scope: true,
        }
    }
}

impl MemoryPolicy {
    pub fn validate(&self) -> Result<(), MemoryAdminError> {
        if self.retention_days > MAX_MEMORY_POLICY_DAYS {
            return Err(MemoryAdminError::Oversized(format!(
                "retentionDays exceeds {MAX_MEMORY_POLICY_DAYS}"
            )));
        }
        if self.max_records == 0 || self.max_records > MAX_MEMORY_POLICY_RECORDS {
            return Err(MemoryAdminError::Oversized(format!(
                "maxRecords must be 1..={MAX_MEMORY_POLICY_RECORDS}"
            )));
        }
        if !self.allow_session_scope && !self.allow_workspace_scope {
            return Err(MemoryAdminError::Malformed(
                "at least one memory scope must remain enabled".into(),
            ));
        }
        Ok(())
    }
}

/// The store-backed policy authority. `scope` is an opaque namespace
/// (`workspace`) and `scope_id` its durable id; the server resolves both
/// from the session row, never from request text.
pub struct MemoryPolicyStore {
    store: Arc<Store>,
}

impl MemoryPolicyStore {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub fn get(&self, scope: &str, scope_id: i64) -> Result<MemoryPolicy, MemoryAdminError> {
        match self.store.memory_policy_get(scope, scope_id)? {
            Some(row) => {
                let mut policy: MemoryPolicy = serde_json::from_str(&row.policy_json)
                    .map_err(|e| MemoryAdminError::Malformed(format!("memory policy row: {e}")))?;
                policy.revision = u64::try_from(row.revision).unwrap_or(0);
                policy.validate()?;
                Ok(policy)
            }
            None => Ok(MemoryPolicy::default()),
        }
    }

    /// Compare-and-swap the policy. `expected_revision = 0` creates it.
    pub fn put_cas(
        &self,
        scope: &str,
        scope_id: i64,
        expected_revision: u64,
        mut policy: MemoryPolicy,
        now_ms: i64,
    ) -> Result<MemoryPolicy, MemoryAdminError> {
        policy.revision = expected_revision;
        policy.validate()?;
        let json = serde_json::to_string(&policy)
            .map_err(|e| MemoryAdminError::Malformed(format!("memory policy: {e}")))?;
        let expected = i64::try_from(expected_revision).map_err(|_| {
            MemoryAdminError::Malformed("policy revision exceeds the signed range".into())
        })?;
        let Some(row) = self
            .store
            .memory_policy_cas(scope, scope_id, expected, &json, now_ms)?
        else {
            return Err(MemoryAdminError::Conflict {
                current_version: self.get(scope, scope_id)?.revision.min(u32::MAX as u64) as u32,
                current_value_hash: String::new(),
            });
        };
        policy.revision = u64::try_from(row.revision).unwrap_or(expected_revision + 1);
        Ok(policy)
    }
}

/// Scope validation helpers shared by the native surface. A workspace scope
/// carries the session's OWN durable workspace id; a session scope carries
/// the session id. Nothing else is writable from a project screen (K-11).
pub fn scope_from_request(
    scope: &str,
    session: SessionId,
    workspace: u64,
) -> Result<MemoryScope, MemoryAdminError> {
    match scope {
        "session" => Ok(MemoryScope::Session(session.raw().to_string())),
        "workspace" | "project" => Ok(MemoryScope::Workspace(workspace.to_string())),
        other => Err(MemoryAdminError::InvalidScope(format!(
            "unsupported memory scope {other:?}; expected session or workspace"
        ))),
    }
}

impl MemoryWriter for MemoryRecordStore {
    fn put(&self, fact: &MemoryFactV2) -> Result<(), crate::MemoryError> {
        let raw = serde_json::to_string(fact)
            .map_err(|e| crate::MemoryError::Malformed(format!("memory serialization: {e}")))?;
        match &fact.scope {
            MemoryScope::Workspace(_) | MemoryScope::Project(_) => {
                self.owner_for_scope(&fact.scope).map_err(|_| {
                    crate::MemoryError::Malformed(
                        "workspace memory scope does not belong to this workspace".into(),
                    )
                })?;
                self.store
                    .upsert_workspace_memory_fact(self.workspace, &fact.row_kind(), &fact.id, &raw)
                    .map_err(crate::MemoryError::Store)
            }
            MemoryScope::Session(_) | MemoryScope::Task(_) => self
                .store
                .upsert_memory_fact(self.session, &fact.row_kind(), &fact.id, &raw)
                .map_err(crate::MemoryError::Store),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::id::WorkspaceId;
    use faktor_store::Store;

    fn fixture() -> (tempfile::TempDir, Arc<Store>, SessionId, WorkspaceId) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ws = store.create_workspace("/w").unwrap();
        let session = store.create_session(ws, "t", "p", "m").unwrap();
        (dir, store, session.id, ws)
    }

    fn draft(scope: &MemoryScope, value: &str) -> NewMemoryRecord {
        NewMemoryRecord {
            scope: scope.clone(),
            subject: "project".into(),
            predicate: "test_command".into(),
            value: TypedMemoryValue::Text(value.into()),
            provenance: Provenance::user(1_000),
            confidence_ppm: 1_000_000,
            evidence: Vec::new(),
            source_revision: None,
        }
    }

    #[test]
    fn create_dedupes_exact_identity_and_conflicts_on_drift() {
        let (_d, store, session, _ws) = fixture();
        let records = MemoryRecordStore::new(store, session);
        let scope = MemoryScope::session_scope(session);
        let first = records.create(draft(&scope, "cargo test"), 1_000).unwrap();
        assert!(first.created);
        assert!(!first.deduped);
        let second = records.create(draft(&scope, "cargo test"), 2_000).unwrap();
        assert!(
            !second.created && second.deduped,
            "identical value is a no-op"
        );
        assert_eq!(second.record.version, 1, "dedupe never bumps the version");
        let error = records
            .create(draft(&scope, "cargo nextest"), 3_000)
            .unwrap_err();
        match error {
            MemoryAdminError::Conflict {
                current_version, ..
            } => assert_eq!(current_version, 1),
            other => panic!("expected a typed conflict, got {other:?}"),
        }
    }

    #[test]
    fn update_delete_restore_are_versioned_and_bound_before_retrieval() {
        let (_d, store, session, _ws) = fixture();
        let records = MemoryRecordStore::new(store, session);
        let scope = MemoryScope::session_scope(session);
        let created = records.create(draft(&scope, "one"), 1_000).unwrap();
        let id = created.record.id.clone();
        let updated = records
            .update(&id, 1, TypedMemoryValue::Text("two".into()), 2_000)
            .unwrap();
        assert_eq!(updated.record.version, 2);
        assert_eq!(
            updated.record.previous.as_ref().unwrap().version,
            1,
            "the prior value remains inspectable"
        );
        let _ = records
            .update(&id, 1, TypedMemoryValue::Text("stale".into()), 3_000)
            .unwrap_err();
        let receipt = records
            .delete(&id, 2, Some("user request".into()), 9, 4_000)
            .unwrap();
        assert!(receipt.retained_prior_copies);
        assert_eq!(receipt.record.version, 3);
        assert!(records.get(&id).unwrap().unwrap().deleted.is_some());
        assert!(
            records.live_scope_records(&scope, 10).unwrap().is_empty(),
            "a tombstone is never retrieve-eligible"
        );
        let restored = records.restore(&id, 3, 5_000).unwrap();
        assert_eq!(restored.record.version, 4);
        assert_eq!(records.live_scope_records(&scope, 10).unwrap().len(), 1);
        assert!(
            records.restore(&id, 4, 6_000).is_err(),
            "live cannot restore"
        );
    }

    #[test]
    fn workspace_scope_is_shared_within_one_workspace_and_never_across() {
        let (dir, store, session, ws) = fixture();
        let sibling = store.create_session(ws, "sibling", "p", "m").unwrap();
        let other_ws = store.create_workspace("/other").unwrap();
        let other = store.create_session(other_ws, "other", "p", "m").unwrap();
        let a = MemoryRecordStore::new(store.clone(), session);
        let b = MemoryRecordStore::new(store.clone(), sibling.id);
        let outsider = MemoryRecordStore::new(store, other.id);
        let scope = MemoryScope::Workspace(ws.raw().to_string());

        let created = a.create(draft(&scope, "shared-value"), 1_000).unwrap();
        let id = created.record.id.clone();
        // Sibling session of the SAME workspace sees the durable record.
        let seen = b.get(&id).unwrap().expect("workspace memory is shared");
        assert_eq!(seen.version, 1);
        assert_eq!(
            b.live_scope_records(&scope, 10).unwrap().len(),
            1,
            "retrieval-eligible workspace memory crosses sessions"
        );
        // An edit from the sibling is durable for the creator too.
        let edited = b
            .update(&id, 1, TypedMemoryValue::Text("edited".into()), 2_000)
            .unwrap();
        assert_eq!(edited.record.version, 2);
        assert_eq!(a.get(&id).unwrap().unwrap().version, 2);
        // Another workspace can neither read the id nor name this scope.
        assert!(outsider.get(&id).unwrap().is_none());
        assert!(matches!(
            outsider.live_scope_records(&scope, 10),
            Err(MemoryAdminError::InvalidScope(_))
        ));
        // A session cannot address a sibling's private session scope.
        assert!(matches!(
            b.live_scope_records(&MemoryScope::session_scope(session), 10),
            Err(MemoryAdminError::InvalidScope(_))
        ));
        // A delete from the creator stops future retrieval for the sibling.
        a.delete(&id, 2, Some("user".into()), 7, 3_000).unwrap();
        assert!(b.live_scope_records(&scope, 10).unwrap().is_empty());
        assert!(b.get(&id).unwrap().unwrap().deleted.is_some());
        let _ = dir;
    }

    #[test]
    fn paging_uses_the_row_cursor_not_the_caller_timestamp() {
        let (_d, store, session, _ws) = fixture();
        let records = MemoryRecordStore::new(store, session);
        let scope = MemoryScope::session_scope(session);
        let mut ids = Vec::new();
        for index in 0..7 {
            let mut draft = draft(&scope, &format!("value-{index}"));
            draft.predicate = format!("predicate-{index}");
            ids.push(records.create(draft, 1).unwrap().record.id);
        }
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = records.list(&scope, false, cursor.as_ref(), 2).unwrap();
            seen.extend(page.records.iter().map(|f| f.id.clone()));
            if !page.has_more {
                break;
            }
            cursor = page.cursor;
            assert!(
                cursor.is_some(),
                "a page with more must expose its row cursor"
            );
        }
        seen.sort();
        let mut expected = ids;
        expected.sort();
        assert_eq!(seen, expected, "paging never skips or duplicates a row");
    }

    #[test]
    fn retention_only_removes_old_tombstones_in_the_exact_scope() {
        let (_d, store, session, ws) = fixture();
        let records = MemoryRecordStore::new(store, session);
        let session_scope = MemoryScope::session_scope(session);
        let workspace_scope = MemoryScope::Workspace(ws.raw().to_string());
        let deleted = records.create(draft(&session_scope, "old"), 1).unwrap();
        let live = records.create(draft(&workspace_scope, "keep"), 1).unwrap();
        records
            .delete(&deleted.record.id, 1, None, 2, 1_000)
            .unwrap();
        let plan = records.retention(&session_scope, 2_000, 10, false).unwrap();
        assert_eq!(plan.records.len(), 1);
        assert_eq!(plan.records[0].id, deleted.record.id);
        assert!(
            plan.records.iter().all(|r| r.id != live.record.id),
            "live records are never candidates"
        );
        let applied = records.retention(&session_scope, 2_000, 10, true).unwrap();
        assert_eq!(applied.records.len(), 1);
        assert!(records.get(&deleted.record.id).unwrap().is_none());
        assert!(records.get(&live.record.id).unwrap().is_some());
    }

    #[test]
    fn workspace_scope_is_isolated_from_another_workspace() {
        let (_d, store, session, ws) = fixture();
        let other_ws = store.create_workspace("/other").unwrap();
        let other = store.create_session(other_ws, "o", "p", "m").unwrap();
        let records = MemoryRecordStore::new(store.clone(), session);
        let other_records = MemoryRecordStore::new(store, other.id);
        let scope = MemoryScope::Workspace(ws.raw().to_string());
        let other_scope = MemoryScope::Workspace(other_ws.raw().to_string());
        records.create(draft(&scope, "mine"), 1).unwrap();
        let mut second = draft(&scope, "mine2");
        second.predicate = "second_command".into();
        records.create(second, 2).unwrap();
        let page = other_records.list(&other_scope, false, None, 50).unwrap();
        assert!(page.records.is_empty(), "another workspace sees nothing");
        assert!(
            matches!(
                other_records.list(&scope, false, None, 50),
                Err(MemoryAdminError::InvalidScope(_))
            ),
            "a foreign scope id is a typed refusal, never a read capability"
        );
    }

    #[test]
    fn policy_cas_is_durable_and_stale_writers_conflict() {
        let (_d, store, _session, ws) = fixture();
        let policies = MemoryPolicyStore::new(store);
        let scope_id = ws.raw() as i64;
        let first = policies.get("workspace", scope_id).unwrap();
        assert_eq!(first.revision, 0);
        assert_eq!(first.auto_capture, AutoCapture::Manual);
        let mut next = first.clone();
        next.auto_capture = AutoCapture::Controlled;
        next.retention_days = 30;
        let saved = policies
            .put_cas("workspace", scope_id, 0, next.clone(), 1_000)
            .unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(
            policies.get("workspace", scope_id).unwrap().auto_capture,
            AutoCapture::Controlled
        );
        assert!(policies
            .put_cas("workspace", scope_id, 0, next, 2_000)
            .is_err());
        let invalid = MemoryPolicy {
            max_records: 0,
            ..MemoryPolicy::default()
        };
        assert!(invalid.validate().is_err());
    }
}
