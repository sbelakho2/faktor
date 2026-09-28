//! `attachments`: cohesive slice of the mechanically decomposed parent module.

use super::*;

use faktor_core::attachment::AttachmentRef;

/// Hard ceiling on the references of ONE CAS blob in ONE session that
/// [`Store::attachment_refs_for_digest`] materializes. The digest route must
/// never materialize an unbounded reference set; beyond this bound the probe
/// is a typed [`StoreError::Oversized`] and callers must address one
/// reference by its surrogate row id.
pub const MAX_ATTACHMENT_REFS_PER_BLOB: usize = 64;

/// One CAS blob hash the store schema references (artifact rows by content
/// address, checkpoint rows by after-blob), with the referencing table and
/// row id. Doctor's dangling-reference scan compares these against the CAS;
/// the store itself never reads blob files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasHashRef {
    /// Table the reference originates from: `artifact` or `checkpoint`.
    pub source: &'static str,
    /// Row id inside that table.
    pub row_id: i64,
    /// The 64-hex BLAKE3 CAS hash the row references.
    pub hash: String,
}

/// Decode one attachment metadata tuple (`digest, mime, filename, size`
/// starting at `offset`) into its typed identity. A bad digest or a negative
/// size is a typed conversion failure, never a panic.
fn columns_to_attachment(r: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<AttachmentId> {
    let digest_raw: String = r.get(offset)?;
    let mime: String = r.get(offset + 1)?;
    let filename: Option<String> = r.get(offset + 2)?;
    let size: i64 = r.get(offset + 3)?;
    let digest = faktor_core::hash::FileHash::from_hex(&digest_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            offset,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("attachment digest {digest_raw:?} is not 32-byte hex"),
            )),
        )
    })?;
    if size < 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            offset + 3,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("attachment size {size} is negative"),
            )),
        ));
    }
    Ok(AttachmentId {
        digest,
        mime,
        filename,
        size: size as u64,
    })
}

/// Decode one `attachment` row (`digest, mime, filename, size` column order)
/// into its typed identity. A bad digest or a negative size is a typed
/// conversion failure, never a panic.
fn row_to_attachment(r: &rusqlite::Row<'_>) -> rusqlite::Result<AttachmentId> {
    columns_to_attachment(r, 0)
}

/// Decode one `attachment` row (`id, digest, mime, filename, size` column
/// order) into its typed REFERENCE identity. A non-positive surrogate id, a
/// bad digest or a negative size is a typed conversion failure.
fn row_to_attachment_ref(r: &rusqlite::Row<'_>) -> rusqlite::Result<AttachmentRef> {
    let id: i64 = r.get(0)?;
    if id <= 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("attachment ref id {id} is not a positive row id"),
            )),
        ));
    }
    let attachment = columns_to_attachment(r, 1)?;
    Ok(AttachmentRef {
        id: id as u64,
        digest: attachment.digest,
        mime: attachment.mime,
        filename: attachment.filename,
        size: attachment.size,
    })
}

/// One attachment reference PREPARED on the caller's thread for a writer job
/// (audit finding 3): the 64-hex digest text and every owned metadata string
/// are materialized BEFORE the command is enqueued, so the job body only
/// binds parameters. [`Self::prepare`] runs the structural validation first,
/// so a hostile identity can never reach a writer closure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PreparedAttachmentRef {
    digest_hex: String,
    mime: String,
    filename: Option<String>,
    size_i64: i64,
}

impl PreparedAttachmentRef {
    /// Validate and prepare one attachment identity for a writer job. The
    /// digest text conversion (`FileHash::to_hex`, an allocation) and every
    /// owned string happen HERE, on the caller's thread — never on the single
    /// writer owner thread (audit item 8).
    pub fn prepare(attachment: &AttachmentId) -> StoreResult<Self> {
        attachment.validate().map_err(|e| match e.kind {
            faktor_core::error::ErrorKind::Oversized => StoreError::Oversized(e.message),
            _ => StoreError::Malformed(e.message),
        })?;
        Ok(Self {
            digest_hex: attachment.digest.to_hex(),
            mime: attachment.mime.clone(),
            filename: attachment.filename.clone(),
            size_i64: attachment.size as i64,
        })
    }

    /// Reconstruct the typed identity. Caller-side only: it parses hex and
    /// allocates, so it must never run inside a writer closure.
    fn to_attachment_id(&self) -> StoreResult<AttachmentId> {
        let digest = faktor_core::hash::FileHash::from_hex(&self.digest_hex).ok_or_else(|| {
            StoreError::Malformed("prepared attachment digest is not 32-byte hex".into())
        })?;
        let size = u64::try_from(self.size_i64)
            .map_err(|_| StoreError::Malformed("prepared attachment size is negative".into()))?;
        Ok(AttachmentId {
            digest,
            mime: self.mime.clone(),
            filename: self.filename.clone(),
            size,
        })
    }

    /// The corrupt-row diagnostic for this reference, built on the caller's
    /// thread like every other writer input.
    fn not_found_message(&self, session_id: SessionId) -> String {
        format!(
            "attachment reference (session {session_id}, digest {}, mime {:?}, filename {:?}, size {}) not found after insert",
            self.digest_hex, self.mime, self.filename, self.size_i64
        )
    }
}

/// Insert-or-ignore every prepared reference into an open `IMMEDIATE`
/// transaction and resolve each durable row (SQL only: every bound value was
/// prepared by the caller). `not_found` must carry one caller-prepared
/// diagnostic per entry of `prepared`.
fn insert_prepared_refs(
    tx: &rusqlite::Transaction<'_>,
    session_id: SessionId,
    prepared: &[PreparedAttachmentRef],
    not_found: &mut [String],
) -> StoreResult<Vec<AttachmentRef>> {
    let mut out = Vec::with_capacity(prepared.len());
    for (index, attachment) in prepared.iter().enumerate() {
        tx.execute(
            "INSERT OR IGNORE INTO attachment(session_id, digest, mime, filename, size)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id.raw() as i64,
                attachment.digest_hex.as_str(),
                attachment.mime.as_str(),
                attachment.filename.as_deref(),
                attachment.size_i64,
            ],
        )?;
        let reference = tx
            .query_row(
                "SELECT id, digest, mime, filename, size FROM attachment
                 WHERE session_id = ?1 AND digest = ?2 AND mime = ?3
                   AND filename IS ?4 AND size = ?5
                 ORDER BY id ASC LIMIT 1",
                params![
                    session_id.raw() as i64,
                    attachment.digest_hex.as_str(),
                    attachment.mime.as_str(),
                    attachment.filename.as_deref(),
                    attachment.size_i64,
                ],
                row_to_attachment_ref,
            )
            .optional()?
            .ok_or_else(|| StoreError::Corrupt(vec![std::mem::take(&mut not_found[index])]))?;
        out.push(reference);
    }
    Ok(out)
}

/// One task row PREPARED on the caller's thread for
/// [`Store::seed_task_attachments_txn`]: every column value, JSON encoding
/// included, is materialized BEFORE the writer command is enqueued, so the
/// job body only binds parameters (audit item 8 / finding 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTaskWrite {
    row: TaskRow,
    criteria_json: String,
    plan_json: String,
    state_json: String,
    attachments_json: String,
}

impl PreparedTaskWrite {
    /// Serialize every `TaskRow` column on the caller's thread. Failures are
    /// typed and happen before any write.
    pub fn prepare(row: TaskRow) -> StoreResult<Self> {
        let task_id = row.task_id;
        let malformed = |what: &str, e: serde_json::Error| {
            StoreError::Malformed(format!("task {task_id} {what} json: {e}"))
        };
        // Preparation BEFORE enqueueing: every serialized TaskRow column.
        let criteria_json = serde_json::to_string(&row.acceptance_criteria)
            .map_err(|e| malformed("acceptance_criteria", e))?;
        let plan_json = serde_json::to_string(&row.plan).map_err(|e| malformed("plan", e))?;
        // In-process constructed enum (see `upsert_task`).
        let state_json = serde_json::to_string(&row.state)
            .expect("in-process TaskState serialization cannot fail");
        let attachments_json =
            serde_json::to_string(&row.attachments).map_err(|e| malformed("attachments", e))?;
        Ok(Self {
            row,
            criteria_json,
            plan_json,
            state_json,
            attachments_json,
        })
    }
}

/// The caller-read task row prepared for one seed PATCH: the raw CAS columns
/// (`revision`, canonical `state` JSON, canonical `attachments` JSON) exactly
/// as the caller read them, plus the typed row itself for the idempotent
/// return. Serialization happens HERE, on the caller's thread — the writer
/// closure compares raw text and never decodes an `AgentState` or a
/// `Vec<AttachmentId>` on the single writer owner (audit finding 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTaskExpectation {
    row: TaskRow,
    revision_i64: i64,
    state_json: String,
    attachments_json: String,
}

impl PreparedTaskExpectation {
    /// Prepare the raw CAS columns of the row the caller read. The revision
    /// uses the schema's lossless two's-complement cast; the state and the
    /// attachment set are serialized once, caller-side.
    pub fn prepare(row: TaskRow) -> StoreResult<Self> {
        let task_id = row.task_id;
        // In-process constructed enum (see `upsert_task`).
        let state_json = serde_json::to_string(&row.state)
            .expect("in-process TaskState serialization cannot fail");
        let attachments_json = serde_json::to_string(&row.attachments)
            .map_err(|e| StoreError::Malformed(format!("task {task_id} attachments json: {e}")))?;
        Ok(Self {
            revision_i64: row.revision.raw() as i64,
            row,
            state_json,
            attachments_json,
        })
    }
}

/// Typed refusal of [`Store::seed_task_attachments_txn`]: the transaction
/// re-read the task row and one of the caller's expectations did not hold.
/// Every refusal leaves ZERO durable writes — reference rows included — so
/// the seed either lands atomically or the durable state is untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedTaskAttachmentRefusal {
    /// The PATCH path expected an existing row; none exists.
    TaskMissing { task_id: TaskId },
    /// The CREATE path expected no row; one already exists.
    TaskExists { task_id: TaskId },
    /// The durable revision differs from the caller's expectation: the row
    /// moved on since the caller's read.
    RevisionMismatch {
        expected: TaskRevision,
        actual: TaskRevision,
    },
    /// The row is terminal: a frozen row can never gain references.
    Terminal { state: TaskState },
    /// The durable state text or attachment text differs from the caller's
    /// prepared expectation: a concurrent mutation replaced the row. The
    /// refusal is payload-free because recovering the typed cause would
    /// require decoding the raw row INSIDE the writer job; callers with the
    /// typed expectation re-read the row on their own thread instead.
    StateOrSetChanged,
    /// The CREATE path's prepared state cannot seed a task.
    IllegalCreateState { state: TaskState },
}

/// The caller-validated target of one atomic seed. Resolving the `Option`
/// parameters BEFORE enqueueing keeps the writer closure free of unwraps.
enum SeedMode {
    Patch {
        task_id: TaskId,
        expectation: Box<PreparedTaskExpectation>,
    },
    Create,
}

impl Store {
    /// Artifact rows reference CAS hashes; the blob itself lives in the CAS.
    pub fn put_artifact(
        &self,
        session_id: SessionId,
        kind: &str,
        cas_hash: &str,
        summary: &str,
        size: i64,
    ) -> StoreResult<i64> {
        let kind = kind.to_owned();
        let cas_hash = cas_hash.to_owned();
        let summary = summary.to_owned();
        // The timestamp is acquired on the CALLER's thread (never inside the
        // writer job): writer closures stay SQL-only.
        let created_ms = now_ms();
        self.writer.execute("put_artifact", move |conn| {
            conn.execute(
            "INSERT OR IGNORE INTO artifact(session_id, kind, cas_hash, summary, created_ms, size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id.raw() as i64,
                kind,
                cas_hash,
                summary,
                created_ms,
                size
            ],
        )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn artifact(&self, cas_hash: &str) -> StoreResult<Option<(String, String)>> {
        let conn = self.read()?;
        let out = conn
            .query_row(
                "SELECT summary, kind FROM artifact WHERE cas_hash = ?1",
                params![cas_hash],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        Ok(out)
    }

    // -------------------------------------------------------------- attachments

    /// Persist one binary/image attachment REFERENCE and return the durable
    /// row matching its FULL metadata (`digest` + `mime` + `filename` +
    /// `size`), never another reference's metadata. The CAS blob is keyed by
    /// `digest` alone and may back many references; the surrogate row id is
    /// internal (the metadata index is `(session_id, digest, mime,
    /// COALESCE(filename,''), size)`). An exact-metadata re-upload is
    /// idempotent and returns the SAME row. The caller (session layer)
    /// validates the id before this write; the store only persists/reads the
    /// typed row.
    pub fn put_attachment(
        &self,
        session_id: SessionId,
        attachment: &AttachmentId,
    ) -> StoreResult<AttachmentId> {
        self.put_attachment_ref(session_id, attachment)
            .map(|reference| reference.attachment())
    }

    /// [`Self::put_attachment`] returning the first-class REFERENCE identity
    /// (surrogate `id` plus the exact metadata), so a caller can address this
    /// exact reference later even when the CAS blob backs several.
    pub fn put_attachment_ref(
        &self,
        session_id: SessionId,
        attachment: &AttachmentId,
    ) -> StoreResult<AttachmentRef> {
        // Preparation BEFORE enqueueing: validation, digest text and owned
        // metadata (audit finding 3) — the writer closure only binds values.
        let prepared = PreparedAttachmentRef::prepare(attachment)?;
        let not_found = prepared.not_found_message(session_id);
        self.writer.execute("put_attachment_ref", move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO attachment(session_id, digest, mime, filename, size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id.raw() as i64,
                    prepared.digest_hex.as_str(),
                    prepared.mime.as_str(),
                    prepared.filename.as_deref(),
                    prepared.size_i64,
                ],
            )?;
            conn.query_row(
                "SELECT id, digest, mime, filename, size FROM attachment
                 WHERE session_id = ?1 AND digest = ?2 AND mime = ?3
                   AND filename IS ?4 AND size = ?5
                 ORDER BY id ASC LIMIT 1",
                params![
                    session_id.raw() as i64,
                    prepared.digest_hex.as_str(),
                    prepared.mime.as_str(),
                    prepared.filename.as_deref(),
                    prepared.size_i64,
                ],
                row_to_attachment_ref,
            )
            .optional()?
            .ok_or_else(|| StoreError::Corrupt(vec![not_found]))
        })
    }

    /// Insert every requested reference in ONE writer job/transaction:
    /// either all rows of `attachments` become durable or the transaction
    /// rolls back and NONE do (duplicate ids inside the batch dedupe
    /// idempotently). Returns each reference in input order. The caller
    /// verifies every CAS blob and validates every id BEFORE calling; the
    /// store only persists the typed rows.
    pub fn inherit_attachment_refs(
        &self,
        session_id: SessionId,
        attachments: &[AttachmentId],
    ) -> StoreResult<Vec<AttachmentRef>> {
        if attachments.len() > faktor_core::attachment::MAX_ATTACHMENTS_PER_TASK {
            return Err(StoreError::Oversized(format!(
                "{} inherited attachment references exceed MAX_ATTACHMENTS_PER_TASK ({})",
                attachments.len(),
                faktor_core::attachment::MAX_ATTACHMENTS_PER_TASK
            )));
        }
        // Preparation BEFORE enqueueing: validation, digest text and owned
        // metadata per reference (audit finding 3) — no `to_hex()` or string
        // formatting escapes into the writer closure.
        let mut prepared = Vec::with_capacity(attachments.len());
        let mut not_found = Vec::with_capacity(attachments.len());
        for attachment in attachments {
            let reference = PreparedAttachmentRef::prepare(attachment)?;
            not_found.push(reference.not_found_message(session_id));
            prepared.push(reference);
        }
        self.writer.execute("inherit_attachment_refs", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut not_found = not_found;
            let out = insert_prepared_refs(&tx, session_id, &prepared, &mut not_found)?;
            tx.commit()?;
            Ok(out)
        })
    }

    /// Land a task's attachment reference set AND its task-row mutation in
    /// ONE `BEGIN IMMEDIATE` transaction (audit finding 2). The callers
    /// (session layer) keep validation/policy; this command enforces the
    /// already-decided conditional write:
    ///
    /// * `patch == Some(expectation)` — PATCH: the row must exist; its raw
    ///   `revision`, `state` and `attachments` columns must equal the
    ///   caller-prepared expectation (the row the caller read), whose state
    ///   must be non-terminal. Then every reference is inserted-or-ignored
    ///   and (unless the desired set is already durable) the row's
    ///   `attachments`/`revision`/`updated_ms` are taken from
    ///   `prepared_task`.
    /// * `patch == None` — CREATE: no row may exist for the prepared row's
    ///   identity; every reference is inserted-or-ignored, then the prepared
    ///   row (revision 1) is inserted.
    ///
    /// A failed condition returns `Ok(Err(`[`SeedTaskAttachmentRefusal`]`))`
    /// with ZERO durable writes — reference rows included: the transaction
    /// rolls back whole. `prepared_task.row.attachments` must equal the
    /// prepared reference set (validated caller-side), so a returned row can
    /// never point at references that were not inserted.
    ///
    /// The writer closure is SQL-ONLY (audit finding 1): the CAS compares the
    /// raw `revision`/`state`/`attachments` text against the caller-prepared
    /// expectation and never decodes a `TaskRow`, `AgentState` or
    /// `Vec<AttachmentId>` on the single writer owner.
    pub fn seed_task_attachments_txn(
        &self,
        session_id: SessionId,
        patch: Option<PreparedTaskExpectation>,
        attachments: &[PreparedAttachmentRef],
        prepared_task: PreparedTaskWrite,
    ) -> StoreResult<std::result::Result<TaskRow, SeedTaskAttachmentRefusal>> {
        if attachments.len() > faktor_core::attachment::MAX_ATTACHMENTS_PER_TASK {
            return Err(StoreError::Oversized(format!(
                "{} seeded attachment references exceed MAX_ATTACHMENTS_PER_TASK ({})",
                attachments.len(),
                faktor_core::attachment::MAX_ATTACHMENTS_PER_TASK
            )));
        }
        // Preparation BEFORE enqueueing: own the batch so the writer closure
        // is `'static` and only binds prepared values.
        let attachments = attachments.to_vec();
        if prepared_task.row.session_id != session_id {
            return Err(StoreError::Malformed(format!(
                "seed task write names session {}, not {session_id}",
                prepared_task.row.session_id
            )));
        }
        // The task row's list and the reference batch are ONE set: mismatch
        // would orphan references or leave the row pointing at absent ones.
        let desired: Vec<AttachmentId> = attachments
            .iter()
            .map(PreparedAttachmentRef::to_attachment_id)
            .collect::<StoreResult<_>>()?;
        if desired != prepared_task.row.attachments {
            return Err(StoreError::Malformed(
                "seed task write attachments must equal the inserted reference set".into(),
            ));
        }
        let task_identity = prepared_task.row.task_id;
        let mode = match patch {
            Some(expectation) => {
                if expectation.row.session_id != session_id {
                    return Err(StoreError::Malformed(format!(
                        "seed patch names session {}, not {session_id}",
                        expectation.row.session_id
                    )));
                }
                if expectation.row.task_id != task_identity {
                    return Err(StoreError::Malformed(format!(
                        "seed patch names task {}, not {task_identity}",
                        expectation.row.task_id
                    )));
                }
                let new_revision = expectation
                    .row
                    .revision
                    .checked_next()
                    .ok_or_else(|| StoreError::Malformed("task revision overflow".into()))?;
                if prepared_task.row.revision != new_revision {
                    return Err(StoreError::Malformed(
                        "a seed patch must write exactly the next revision".into(),
                    ));
                }
                // Caller-prepared terminal protection: the expectation IS the
                // row the caller read, so its typed state is available on this
                // thread. A durable transition to terminal after the read
                // changes the raw state text and refuses below.
                if expectation.row.state.is_terminal() {
                    return Ok(Err(SeedTaskAttachmentRefusal::Terminal {
                        state: expectation.row.state,
                    }));
                }
                SeedMode::Patch {
                    task_id: task_identity,
                    expectation: Box::new(expectation),
                }
            }
            None => {
                if prepared_task.row.revision != TaskRevision::new(1) {
                    return Err(StoreError::Malformed(
                        "a seeded task row starts at revision 1".into(),
                    ));
                }
                SeedMode::Create
            }
        };
        // Preparation BEFORE enqueueing: one caller-side corrupt diagnostic per
        // reference, the vanished-row conflict text and the corrupt-revision
        // diagnostic (the closure formats nothing).
        let mut not_found: Vec<String> = attachments
            .iter()
            .map(|attachment| attachment.not_found_message(session_id))
            .collect();
        let vanished =
            format!("task {session_id}/{task_identity} vanished between validation and write");
        let corrupt_revision =
            format!("task {session_id}/{task_identity} revision: not a positive revision");
        self.writer
            .execute("seed_task_attachments_txn", move |conn| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let prepared = prepared_task;
                match mode {
                    SeedMode::Patch {
                        task_id,
                        expectation,
                    } => {
                        // SELECT ONLY the raw CAS columns: no row decode runs on
                        // the writer owner (audit finding 1).
                        let raw = tx
                            .query_row(
                                "SELECT revision, state, attachments FROM task
                                 WHERE session_id = ?1 AND task_id = ?2",
                                params![session_id.raw() as i64, task_id.raw() as i64],
                                |r| {
                                    Ok((
                                        r.get::<_, i64>(0)?,
                                        r.get::<_, String>(1)?,
                                        r.get::<_, String>(2)?,
                                    ))
                                },
                            )
                            .optional()?;
                        let Some((raw_revision, raw_state, raw_attachments)) = raw else {
                            return Ok(Err(SeedTaskAttachmentRefusal::TaskMissing { task_id }));
                        };
                        // The durable state moved (a terminal transition lands
                        // here): the refusal is raw and payload-free, because
                        // recovering the typed state would decode inside the
                        // writer job.
                        if raw_state != expectation.state_json {
                            return Ok(Err(SeedTaskAttachmentRefusal::StateOrSetChanged));
                        }
                        if raw_revision != expectation.revision_i64 {
                            let actual = TaskRevision::try_from(raw_revision as u64)
                                .map_err(|_| StoreError::Corrupt(vec![corrupt_revision.clone()]))?;
                            return Ok(Err(SeedTaskAttachmentRefusal::RevisionMismatch {
                                expected: expectation.row.revision,
                                actual,
                            }));
                        }
                        if raw_attachments != expectation.attachments_json {
                            return Ok(Err(SeedTaskAttachmentRefusal::StateOrSetChanged));
                        }
                        let _ =
                            insert_prepared_refs(&tx, session_id, &attachments, &mut not_found)?;
                        if raw_attachments == prepared.attachments_json {
                            // Idempotent re-seed: the reference rows converge,
                            // the task row (and its revision) is not touched.
                            // The caller-provided typed row is returned as-is.
                            tx.commit()?;
                            return Ok(Ok(expectation.row));
                        }
                        let updated = tx.execute(
                            "UPDATE task SET attachments = ?3, revision = ?4, updated_ms = ?5
                             WHERE session_id = ?1 AND task_id = ?2 AND revision = ?6
                               AND state = ?7 AND attachments = ?8",
                            params![
                                session_id.raw() as i64,
                                task_id.raw() as i64,
                                prepared.attachments_json.as_str(),
                                prepared.row.revision.raw() as i64,
                                prepared.row.updated_ms,
                                expectation.revision_i64,
                                expectation.state_json.as_str(),
                                expectation.attachments_json.as_str(),
                            ],
                        )?;
                        if updated != 1 {
                            return Err(StoreError::Conflict(vanished));
                        }
                        tx.commit()?;
                        Ok(Ok(prepared.row))
                    }
                    SeedMode::Create => {
                        let exists: Option<i64> = tx
                            .query_row(
                                "SELECT 1 FROM task WHERE session_id = ?1 AND task_id = ?2 LIMIT 1",
                                params![session_id.raw() as i64, task_identity.raw() as i64],
                                |r| r.get(0),
                            )
                            .optional()?;
                        if exists.is_some() {
                            return Ok(Err(SeedTaskAttachmentRefusal::TaskExists {
                                task_id: task_identity,
                            }));
                        }
                        if !prepared.row.state.is_creatable() {
                            return Ok(Err(SeedTaskAttachmentRefusal::IllegalCreateState {
                                state: prepared.row.state,
                            }));
                        }
                        let _ =
                            insert_prepared_refs(&tx, session_id, &attachments, &mut not_found)?;
                        tx.execute(
                            "INSERT INTO task(task_id, session_id, goal, acceptance_criteria, plan,
                                              max_tokens, max_turns, spent_tokens, spent_turns,
                                              state, created_ms, updated_ms, revision, attachments)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                            params![
                                prepared.row.task_id.raw() as i64,
                                session_id.raw() as i64,
                                prepared.row.goal.as_str(),
                                prepared.criteria_json.as_str(),
                                prepared.plan_json.as_str(),
                                prepared.row.max_tokens.map(|m| m as i64),
                                prepared.row.max_turns.map(|m| m as i64),
                                prepared.row.spent_tokens.min(i64::MAX as u64) as i64,
                                prepared.row.spent_turns.min(i64::MAX as u32) as i64,
                                prepared.state_json.as_str(),
                                prepared.row.created_ms,
                                prepared.row.updated_ms,
                                prepared.row.revision.raw() as i64,
                                prepared.attachments_json.as_str(),
                            ],
                        )?;
                        tx.commit()?;
                        Ok(Ok(prepared.row))
                    }
                }
            })
    }

    /// Resolve ONE durable attachment reference by its surrogate row id,
    /// scoped to the session (a foreign/unknown/out-of-range id is `None`,
    /// never a cross-session read).
    pub fn attachment_ref(
        &self,
        session_id: SessionId,
        ref_id: u64,
    ) -> StoreResult<Option<AttachmentRef>> {
        let Ok(ref_id) = i64::try_from(ref_id) else {
            return Ok(None);
        };
        let conn = self.read()?;
        let out = conn
            .query_row(
                "SELECT id, digest, mime, filename, size FROM attachment
                 WHERE session_id = ?1 AND id = ?2",
                params![session_id.raw() as i64, ref_id],
                row_to_attachment_ref,
            )
            .optional()?;
        Ok(out)
    }

    /// EVERY attachment reference of THIS session that shares one CAS blob
    /// `digest`, in deterministic insert (`id`) order — bounded by
    /// [`MAX_ATTACHMENT_REFS_PER_BLOB`]: beyond the bound the probe is a
    /// typed [`StoreError::Oversized`], never an unbounded materialization.
    pub fn attachment_refs_for_digest(
        &self,
        session_id: SessionId,
        digest: faktor_core::hash::FileHash,
    ) -> StoreResult<Vec<AttachmentRef>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, digest, mime, filename, size FROM attachment
             WHERE session_id = ?1 AND digest = ?2 ORDER BY id ASC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![
            session_id.raw() as i64,
            digest.to_hex(),
            (MAX_ATTACHMENT_REFS_PER_BLOB + 1) as i64
        ])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            if out.len() == MAX_ATTACHMENT_REFS_PER_BLOB {
                return Err(StoreError::Oversized(format!(
                    "attachment digest {digest} in session {session_id} has more than MAX_ATTACHMENT_REFS_PER_BLOB ({MAX_ATTACHMENT_REFS_PER_BLOB}) references; resolve one by its surrogate ref id"
                )));
            }
            out.push(row_to_attachment_ref(r)?);
        }
        Ok(out)
    }

    /// Resolve the EXACT attachment reference: `Some` only when a row of THIS
    /// session matches all of `digest` + `mime` + `filename` + `size`.
    /// `None` covers both "no such digest" and "digest exists with other
    /// metadata"; callers that must distinguish the two use
    /// [`Self::attachments_by_digest`].
    pub fn attachment_row(
        &self,
        session_id: SessionId,
        attachment: &AttachmentId,
    ) -> StoreResult<Option<AttachmentId>> {
        let conn = self.read()?;
        let out = conn
            .query_row(
                "SELECT digest, mime, filename, size FROM attachment
                 WHERE session_id = ?1 AND digest = ?2 AND mime = ?3
                   AND filename IS ?4 AND size = ?5
                 ORDER BY id ASC LIMIT 1",
                params![
                    session_id.raw() as i64,
                    attachment.digest.to_hex(),
                    attachment.mime.as_str(),
                    attachment.filename.as_deref(),
                    attachment.size as i64,
                ],
                row_to_attachment,
            )
            .optional()?;
        Ok(out)
    }

    /// EVERY attachment reference of THIS session that shares one CAS blob
    /// `digest`, in deterministic insert (`id`) order. The caller owns the
    /// bound: this materializes the durable rows for one digest, so a trust
    /// boundary should prefer the exact [`Self::attachment_row`] lookup and
    /// use this only for existence/diagnostics (or a caller-side limit).
    pub fn attachments_by_digest(
        &self,
        session_id: SessionId,
        digest: faktor_core::hash::FileHash,
    ) -> StoreResult<Vec<AttachmentId>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT digest, mime, filename, size FROM attachment
             WHERE session_id = ?1 AND digest = ?2 ORDER BY id ASC",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64, digest.to_hex()])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_attachment(r)?);
        }
        Ok(out)
    }

    /// Resolve ONE durable attachment row by digest (restart-safe: reads the
    /// typed metadata, never a process-local map). When several references
    /// share a blob the returned row is DETERMINISTIC: the lowest surrogate
    /// id (the first-inserted reference). Callers needing a specific
    /// reference use [`Self::attachment_row`].
    pub fn attachment(
        &self,
        session_id: SessionId,
        digest: faktor_core::hash::FileHash,
    ) -> StoreResult<Option<AttachmentId>> {
        let conn = self.read()?;
        let out = conn
            .query_row(
                "SELECT digest, mime, filename, size FROM attachment
                 WHERE session_id = ?1 AND digest = ?2
                 ORDER BY id ASC LIMIT 1",
                params![session_id.raw() as i64, digest.to_hex()],
                row_to_attachment,
            )
            .optional()?;
        Ok(out)
    }

    /// The session's durable attachment metadata, deterministic digest order
    /// (ties broken by insert id), a bounded page of at most `limit` rows.
    pub fn list_attachments(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> StoreResult<Vec<AttachmentId>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT digest, mime, filename, size FROM attachment
             WHERE session_id = ?1 ORDER BY digest ASC, id ASC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64, limit as i64])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_attachment(r)?);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- worktrees

    /// Every CAS blob hash the store schema references — `artifact.cas_hash`
    /// rows and `checkpoint.after_cas_hash` rows — with the referencing
    /// table and row id, for the doctor dangling-reference scan. The store
    /// never reads blob files; existence is verified by the caller against
    /// the CAS (the CLI's doctor owns both handles).
    pub fn cas_hash_references(&self) -> StoreResult<Vec<CasHashRef>> {
        let conn = self.read()?;
        let mut out = Vec::new();
        {
            let mut stmt = conn.prepare("SELECT id, cas_hash FROM artifact")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                out.push(CasHashRef {
                    source: "artifact",
                    row_id: row.get(0)?,
                    hash: row.get(1)?,
                });
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT id, after_cas_hash FROM checkpoint WHERE after_cas_hash IS NOT NULL",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                out.push(CasHashRef {
                    source: "checkpoint",
                    row_id: row.get(0)?,
                    hash: row.get(1)?,
                });
            }
        }
        Ok(out)
    }
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
    fn artifact_hash_unique_across_sessions() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s1 = store.create_session(ws, "a", "p", "m").unwrap();
        let s2 = store.create_session(ws, "b", "p", "m").unwrap();
        store
            .put_artifact(s1.id, "command_output", "hash1", "sum", 10)
            .unwrap();
        store
            .put_artifact(s2.id, "command_output", "hash1", "sum", 10)
            .unwrap();
        let a = store.artifact("hash1").unwrap().unwrap();
        assert_eq!(a.0, "sum");
        assert_eq!(store.artifact("nope").unwrap(), None);
    }

    #[test]
    fn giant_payload_roundtrip_via_cas_hash_reference() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let blob_hash = format!("{:064x}", 7);
        let big = serde_json::json!({"blob_hash": blob_hash});
        store
            .put_artifact(
                s.id,
                "tool_output",
                big["blob_hash"].as_str().unwrap(),
                "300MB compiler log",
                300_000_000,
            )
            .unwrap();
        assert_eq!(
            store
                .artifact(big["blob_hash"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .1,
            "tool_output"
        );
        // A different hash is not found.
        assert_eq!(store.artifact(&"0".repeat(63)).unwrap(), None);
    }

    #[test]
    fn attachment_references_are_keyed_by_full_metadata_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s1 = store.create_session(ws, "att", "p", "m").unwrap();
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "other", "p", "m").unwrap();
        let digest = faktor_core::hash::FileHash::from([7; 32]);
        let png = AttachmentId::new(digest, "image/png", Some("shot.png"), 123).unwrap();
        let first = store.put_attachment(s1.id, &png).unwrap();
        assert_eq!(first, png);
        // Identical bytes under a different MIME is a DISTINCT reference: it
        // must keep its own metadata, never inherit the first row's.
        let pdf = AttachmentId::new(digest, "application/pdf", Some("shot.png"), 123).unwrap();
        let second = store.put_attachment(s1.id, &pdf).unwrap();
        assert_eq!(second, pdf);
        assert_ne!(second, first);
        assert_eq!(second.mime, "application/pdf");
        // Identical bytes under a different filename is a DISTINCT reference:
        // a rename/re-select is repairable.
        let renamed = AttachmentId::new(digest, "image/png", Some("renamed.png"), 123).unwrap();
        let third = store.put_attachment(s1.id, &renamed).unwrap();
        assert_eq!(third, renamed);
        assert_ne!(third, first);
        // An EXACT-metadata re-upload is the only dedupe: idempotent, returns
        // the same reference.
        assert_eq!(store.put_attachment(s1.id, &pdf).unwrap(), second);
        assert_eq!(store.put_attachment(s1.id, &png).unwrap(), first);
        // Exact lookup resolves each reference's OWN metadata.
        assert_eq!(
            store.attachment_row(s1.id, &png).unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            store.attachment_row(s1.id, &pdf).unwrap(),
            Some(second.clone())
        );
        assert_eq!(
            store.attachment_row(s1.id, &renamed).unwrap(),
            Some(third.clone())
        );
        // A digest-only lookup is deterministic: the lowest id (first insert).
        assert_eq!(
            store.attachment(s1.id, digest).unwrap(),
            Some(first.clone())
        );
        // Every reference to the one blob, in insert order.
        assert_eq!(
            store.attachments_by_digest(s1.id, digest).unwrap(),
            vec![first.clone(), second.clone(), third.clone()]
        );
        assert_eq!(
            store.list_attachments(s1.id, 10).unwrap(),
            vec![first.clone(), second.clone(), third.clone()],
            "listing is deterministic when several references share a digest"
        );
        // A reference whose metadata is absent is None even when the digest
        // exists (the distinct-from-absent distinction lives in the session
        // layer).
        let other_digest = faktor_core::hash::FileHash::from([8; 32]);
        let absent = AttachmentId::new(other_digest, "image/png", Some("shot.png"), 123).unwrap();
        assert_eq!(store.attachment_row(s1.id, &absent).unwrap(), None);
        // Session scope: another session never sees any of the references.
        assert!(store.attachment(s2.id, digest).unwrap().is_none());
        assert!(store
            .attachments_by_digest(s2.id, digest)
            .unwrap()
            .is_empty());
        // Reopen: every reference resolves identically from disk.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(
            store.attachment_row(s1.id, &pdf).unwrap(),
            Some(second.clone())
        );
        assert_eq!(
            store.attachments_by_digest(s1.id, digest).unwrap(),
            vec![first, second, third]
        );
    }

    #[test]
    fn attachment_refs_are_id_addressed_and_session_scoped() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s1 = store.create_session(ws, "att", "p", "m").unwrap();
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "other", "p", "m").unwrap();
        let digest = faktor_core::hash::FileHash::from([21; 32]);
        let png = AttachmentId::new(digest, "image/png", Some("shot.png"), 123).unwrap();
        let pdf = AttachmentId::new(digest, "application/pdf", None, 123).unwrap();
        let first = store.put_attachment_ref(s1.id, &png).unwrap();
        assert_eq!(first.attachment(), png);
        assert!(first.id >= 1, "a durable row id is a positive surrogate");
        let second = store.put_attachment_ref(s1.id, &pdf).unwrap();
        assert_eq!(second.attachment(), pdf);
        assert_ne!(second.id, first.id, "each reference keeps its own row id");
        // An exact-metadata re-upload returns the SAME reference row.
        assert_eq!(store.put_attachment_ref(s1.id, &png).unwrap(), first);
        // Id-addressed resolution returns exactly that reference.
        assert_eq!(
            store.attachment_ref(s1.id, first.id).unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            store.attachment_ref(s1.id, second.id).unwrap(),
            Some(second.clone())
        );
        // Unknown, zero, out-of-range and foreign ids are honest absences.
        assert_eq!(store.attachment_ref(s1.id, second.id + 1).unwrap(), None);
        assert_eq!(store.attachment_ref(s1.id, 0).unwrap(), None);
        assert_eq!(store.attachment_ref(s1.id, u64::MAX).unwrap(), None);
        assert_eq!(store.attachment_ref(s2.id, first.id).unwrap(), None);
        // The bounded digest probe sees both references in insert order.
        assert_eq!(
            store.attachment_refs_for_digest(s1.id, digest).unwrap(),
            vec![first, second]
        );
        assert!(store
            .attachment_refs_for_digest(s2.id, digest)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn attachment_refs_for_digest_is_bounded_and_typed_oversized() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "att", "p", "m").unwrap();
        let digest = faktor_core::hash::FileHash::from([22; 32]);
        for index in 0..MAX_ATTACHMENT_REFS_PER_BLOB {
            let id =
                AttachmentId::new(digest, "image/png", Some(&format!("f{index}.png")), 1).unwrap();
            store.put_attachment_ref(s.id, &id).unwrap();
        }
        assert_eq!(
            store
                .attachment_refs_for_digest(s.id, digest)
                .unwrap()
                .len(),
            MAX_ATTACHMENT_REFS_PER_BLOB,
            "exactly the bound still materializes"
        );
        let extra = AttachmentId::new(digest, "image/png", Some("overflow.png"), 1).unwrap();
        store.put_attachment_ref(s.id, &extra).unwrap();
        let err = store
            .attachment_refs_for_digest(s.id, digest)
            .expect_err("one over the bound must be a typed Oversized");
        assert!(matches!(err, StoreError::Oversized(_)), "{err:?}");
        // The id-addressed probes stay exact and available beyond the bound.
        assert_eq!(
            store
                .attachment_ref(s.id, 1)
                .unwrap()
                .unwrap()
                .filename
                .as_deref(),
            Some("f0.png")
        );
    }

    #[test]
    fn inherit_attachment_refs_is_one_transaction_all_or_nothing() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "att", "p", "m").unwrap();
        let digest_a = faktor_core::hash::FileHash::from([23; 32]);
        let digest_b = faktor_core::hash::FileHash::from([24; 32]);
        let a = AttachmentId::new(digest_a, "image/png", Some("a.png"), 3).unwrap();
        let b = AttachmentId::new(digest_b, "application/pdf", Some("b.pdf"), 4).unwrap();
        // The success path inserts both references in input order.
        let refs = store
            .inherit_attachment_refs(s.id, &[a.clone(), b.clone()])
            .unwrap();
        assert_eq!(
            refs.iter().map(|r| r.attachment()).collect::<Vec<_>>(),
            vec![a.clone(), b.clone()]
        );
        // A hostile trigger aborts the SECOND insert mid-batch: the whole
        // transaction rolls back and no partial reference set survives.
        store
            .writer
            .execute("plant_hostile_trigger", move |conn| {
                conn.execute(
                    "CREATE TRIGGER attachment_boom BEFORE INSERT ON attachment
                     WHEN NEW.mime = 'text/plain'
                     BEGIN SELECT RAISE(ABORT, 'hostile mid-batch failure'); END",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let c = AttachmentId::new(
            faktor_core::hash::FileHash::from([25; 32]),
            "image/png",
            Some("c.png"),
            5,
        )
        .unwrap();
        let hostile = AttachmentId::new(
            faktor_core::hash::FileHash::from([26; 32]),
            "text/plain",
            Some("d.txt"),
            6,
        )
        .unwrap();
        let before = store.list_attachments(s.id, 32).unwrap();
        let err = store
            .inherit_attachment_refs(s.id, &[c.clone(), hostile])
            .expect_err("the hostile second insert must abort the transaction");
        assert!(matches!(err, StoreError::Sqlite(_)), "{err:?}");
        assert_eq!(
            store.list_attachments(s.id, 32).unwrap(),
            before,
            "no partial reference set may survive a rolled-back batch"
        );
        assert_eq!(store.attachment_row(s.id, &c).unwrap(), None);
        // The bound is enforced before any write.
        let many = vec![c; faktor_core::attachment::MAX_ATTACHMENTS_PER_TASK + 1];
        let err = store
            .inherit_attachment_refs(s.id, &many)
            .expect_err("over-bound batch");
        assert!(matches!(err, StoreError::Oversized(_)), "{err:?}");
    }

    #[test]
    fn cas_hash_references_lists_artifact_and_checkpoint_after_blobs() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_artifact(s.id, "command_output", &"ab".repeat(32), "sum", 10)
            .unwrap();
        store
            .put_artifact(s.id, "command_output", &"cd".repeat(32), "sum", 10)
            .unwrap();
        // Checkpoint with after blob (referenced) and without (not listed).
        store
            .put_checkpoint(s.id, 1, "a.rs", "b", "a", Some(&"ef".repeat(32)))
            .unwrap();
        store
            .put_checkpoint(s.id, 2, "b.rs", "b", "a", None)
            .unwrap();
        let refs = store.cas_hash_references().unwrap();
        assert_eq!(refs.len(), 3);
        assert!(refs
            .iter()
            .any(|r| r.source == "artifact" && r.hash == "ab".repeat(32)));
        assert!(refs
            .iter()
            .any(|r| r.source == "artifact" && r.hash == "cd".repeat(32)));
        assert!(refs
            .iter()
            .any(|r| r.source == "checkpoint" && r.hash == "ef".repeat(32)));
        assert!(!refs
            .iter()
            .any(|r| r.row_id == 2 && r.source == "checkpoint"));
    }

    // -------------------------------------------------- actor batch surface tests

    fn task_seed_row(
        session_id: SessionId,
        task_id: TaskId,
        state: TaskState,
        revision: u64,
        attachments: Vec<AttachmentId>,
    ) -> TaskRow {
        TaskRow {
            task_id,
            session_id,
            goal: "seed target".into(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments,
            max_tokens: None,
            max_turns: None,
            spent_tokens: 0,
            spent_turns: 0,
            state,
            revision: TaskRevision::new(revision),
            created_ms: 10,
            updated_ms: 10,
        }
    }

    fn prepared_refs(ids: &[AttachmentId]) -> Vec<PreparedAttachmentRef> {
        ids.iter()
            .map(|id| PreparedAttachmentRef::prepare(id).unwrap())
            .collect()
    }

    fn listed_sorted(store: &Store, session_id: SessionId) -> Vec<AttachmentId> {
        let mut listed = store.list_attachments(session_id, 16).unwrap();
        listed.sort_by_key(|id| id.digest.to_hex());
        listed
    }

    #[test]
    fn prepared_attachment_refs_validate_and_materialize_digest_text() {
        let digest = faktor_core::hash::FileHash::from([31; 32]);
        let id = AttachmentId::new(digest, "image/png", Some("a.png"), 12).unwrap();
        let prepared = PreparedAttachmentRef::prepare(&id).unwrap();
        assert_eq!(prepared.digest_hex, digest.to_hex());
        assert_eq!(prepared.mime, "image/png");
        assert_eq!(prepared.filename.as_deref(), Some("a.png"));
        assert_eq!(prepared.size_i64, 12);
        assert_eq!(prepared.to_attachment_id().unwrap(), id);
        // Hostile identities are typed refusals BEFORE any writer job exists.
        let traversal = AttachmentId {
            filename: Some("../secrets".into()),
            ..id.clone()
        };
        assert!(matches!(
            PreparedAttachmentRef::prepare(&traversal),
            Err(StoreError::Malformed(_))
        ));
        let oversized = AttachmentId {
            size: faktor_core::attachment::MAX_ATTACHMENT_BYTES + 1,
            ..id
        };
        assert!(matches!(
            PreparedAttachmentRef::prepare(&oversized),
            Err(StoreError::Oversized(_))
        ));
    }

    #[test]
    fn seed_task_attachments_txn_create_lands_references_and_the_row_together() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "seed", "p", "m").unwrap();
        let a = AttachmentId::new(
            faktor_core::hash::FileHash::from([32; 32]),
            "image/png",
            Some("a.png"),
            3,
        )
        .unwrap();
        let b = AttachmentId::new(
            faktor_core::hash::FileHash::from([33; 32]),
            "application/pdf",
            Some("b.pdf"),
            4,
        )
        .unwrap();
        let task_id = TaskId::new(1);
        let refs = prepared_refs(&[a.clone(), b.clone()]);
        let row = task_seed_row(
            s.id,
            task_id,
            TaskState::Pending,
            1,
            vec![a.clone(), b.clone()],
        );
        let write = PreparedTaskWrite::prepare(row.clone()).unwrap();
        let outcome = store
            .seed_task_attachments_txn(s.id, None, &refs, write)
            .unwrap()
            .expect("the create path must land");
        assert_eq!(outcome, row);
        assert_eq!(store.get_task(s.id, task_id).unwrap().unwrap(), row);
        let mut expected = vec![a.clone(), b.clone()];
        expected.sort_by_key(|id| id.digest.to_hex());
        assert_eq!(
            listed_sorted(&store, s.id),
            expected,
            "both references are durable in the same transaction as the row"
        );
        // A second create for the same identity refuses typed and writes
        // nothing at all.
        let write = PreparedTaskWrite::prepare(row.clone()).unwrap();
        let refusal = store
            .seed_task_attachments_txn(s.id, None, &refs, write)
            .unwrap()
            .expect_err("an existing row must refuse the create");
        assert_eq!(refusal, SeedTaskAttachmentRefusal::TaskExists { task_id });
        assert_eq!(store.get_task(s.id, task_id).unwrap().unwrap(), row);
        assert_eq!(listed_sorted(&store, s.id).len(), 2);
    }

    #[test]
    fn seed_task_attachments_txn_stale_conditions_refuse_with_zero_writes() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "seed", "p", "m").unwrap();
        let a = AttachmentId::new(
            faktor_core::hash::FileHash::from([34; 32]),
            "image/png",
            Some("a.png"),
            3,
        )
        .unwrap();
        let b = AttachmentId::new(
            faktor_core::hash::FileHash::from([35; 32]),
            "application/pdf",
            Some("b.pdf"),
            4,
        )
        .unwrap();
        let task_id = TaskId::new(1);
        let refs = prepared_refs(&[a.clone(), b.clone()]);
        let desired = vec![a.clone(), b.clone()];
        // Seed a non-terminal row at revision 5 with NO attachments.
        store
            .upsert_task(&task_seed_row(s.id, task_id, TaskState::Running, 5, vec![]))
            .unwrap();
        // Stale revision: the caller read revision 4, the row is at 5; the
        // transaction refuses before the first insert.
        let stale = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            4,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            5,
            desired.clone(),
        ))
        .unwrap();
        let refusal = store
            .seed_task_attachments_txn(s.id, Some(stale), &refs, write)
            .unwrap()
            .expect_err("a stale revision must refuse");
        assert_eq!(
            refusal,
            SeedTaskAttachmentRefusal::RevisionMismatch {
                expected: TaskRevision::new(4),
                actual: TaskRevision::new(5),
            }
        );
        assert!(listed_sorted(&store, s.id).is_empty());
        // Stale attachment set at the SAME revision: the raw attachment text
        // no longer matches the caller's expectation, so the seed refuses
        // payload-free and writes nothing.
        let stale = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            5,
            vec![a.clone()],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            6,
            desired.clone(),
        ))
        .unwrap();
        let refusal = store
            .seed_task_attachments_txn(s.id, Some(stale), &refs, write)
            .unwrap()
            .expect_err("a stale attachment set must refuse");
        assert_eq!(refusal, SeedTaskAttachmentRefusal::StateOrSetChanged);
        assert!(listed_sorted(&store, s.id).is_empty());
        // A concurrent transition to terminal changes the raw state text and
        // refuses payload-free (the caller recovers the typed state with its
        // own re-read), with zero reference rows.
        store
            .upsert_task(&task_seed_row(
                s.id,
                task_id,
                TaskState::Cancelled,
                5,
                vec![],
            ))
            .unwrap();
        let stale = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            5,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            6,
            desired.clone(),
        ))
        .unwrap();
        let refusal = store
            .seed_task_attachments_txn(s.id, Some(stale), &refs, write)
            .unwrap()
            .expect_err("a concurrent terminal transition must refuse the seed");
        assert_eq!(refusal, SeedTaskAttachmentRefusal::StateOrSetChanged);
        assert!(listed_sorted(&store, s.id).is_empty());
        // A caller-prepared expectation that IS terminal is refused BEFORE
        // enqueueing (caller-side terminal protection, never a decode on the
        // writer owner).
        let terminal = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Cancelled,
            5,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            6,
            desired.clone(),
        ))
        .unwrap();
        let refusal = store
            .seed_task_attachments_txn(s.id, Some(terminal), &refs, write)
            .unwrap()
            .expect_err("a terminal expectation must refuse the seed");
        assert_eq!(
            refusal,
            SeedTaskAttachmentRefusal::Terminal {
                state: TaskState::Cancelled
            }
        );
        assert!(listed_sorted(&store, s.id).is_empty());
        assert_eq!(
            store.get_task(s.id, task_id).unwrap().unwrap().attachments,
            Vec::new()
        );
        // A corrupt raw revision (zero) on a non-terminal row is a typed
        // Corrupt, never a panic.
        store
            .upsert_task(&task_seed_row(s.id, task_id, TaskState::Running, 7, vec![]))
            .unwrap();
        store
            .writer
            .execute("plant_zero_revision", move |conn| {
                conn.execute(
                    "UPDATE task SET revision = 0 WHERE session_id = ?1 AND task_id = ?2",
                    params![s.id.raw() as i64, task_id.raw() as i64],
                )?;
                Ok(())
            })
            .unwrap();
        let stale = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            1,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            2,
            desired.clone(),
        ))
        .unwrap();
        let err = store
            .seed_task_attachments_txn(s.id, Some(stale), &refs, write)
            .expect_err("a zero raw revision is corruption");
        assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
        assert!(listed_sorted(&store, s.id).is_empty());
        let missing = TaskId::new(9);
        // Patch on a missing row.
        let stale = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            missing,
            TaskState::Running,
            1,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            missing,
            TaskState::Running,
            2,
            desired.clone(),
        ))
        .unwrap();
        let refusal = store
            .seed_task_attachments_txn(s.id, Some(stale), &refs, write)
            .unwrap()
            .expect_err("a missing patch target must refuse");
        assert_eq!(
            refusal,
            SeedTaskAttachmentRefusal::TaskMissing { task_id: missing }
        );
        assert!(listed_sorted(&store, s.id).is_empty());
        // The reference batch must equal the prepared row's list.
        let expectation = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            5,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            6,
            vec![a.clone()],
        ))
        .unwrap();
        let err = store
            .seed_task_attachments_txn(s.id, Some(expectation), &refs, write)
            .expect_err("a mismatched desired set is caller misconfiguration");
        assert!(matches!(err, StoreError::Malformed(_)), "{err:?}");
        assert!(listed_sorted(&store, s.id).is_empty());
        // The bound is enforced before any write.
        let expectation = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            5,
            vec![],
        ))
        .unwrap();
        let many = vec![refs[0].clone(); faktor_core::attachment::MAX_ATTACHMENTS_PER_TASK + 1];
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            6,
            desired,
        ))
        .unwrap();
        let err = store
            .seed_task_attachments_txn(s.id, Some(expectation), &many, write)
            .expect_err("an over-bound seed batch must refuse");
        assert!(matches!(err, StoreError::Oversized(_)), "{err:?}");
        assert!(listed_sorted(&store, s.id).is_empty());
    }

    #[test]
    fn seed_task_attachments_txn_patch_bumps_once_and_reseed_is_idempotent() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "seed", "p", "m").unwrap();
        let a = AttachmentId::new(
            faktor_core::hash::FileHash::from([36; 32]),
            "image/png",
            Some("a.png"),
            3,
        )
        .unwrap();
        let b = AttachmentId::new(
            faktor_core::hash::FileHash::from([37; 32]),
            "application/pdf",
            Some("b.pdf"),
            4,
        )
        .unwrap();
        let task_id = TaskId::new(1);
        store
            .upsert_task(&task_seed_row(s.id, task_id, TaskState::Running, 3, vec![]))
            .unwrap();
        let refs = prepared_refs(&[a.clone(), b.clone()]);
        // First patch: revision 3 -> 4, both references land. The expectation
        // is the row the caller read.
        let expectation = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            3,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            4,
            vec![a.clone(), b.clone()],
        ))
        .unwrap();
        let outcome = store
            .seed_task_attachments_txn(s.id, Some(expectation), &refs, write)
            .unwrap()
            .expect("the patch path must land");
        assert_eq!(outcome.revision, TaskRevision::new(4));
        assert_eq!(outcome.attachments, vec![a.clone(), b.clone()]);
        let stored = store.get_task(s.id, task_id).unwrap().unwrap();
        assert_eq!(stored.revision, TaskRevision::new(4));
        assert_eq!(stored.attachments, vec![a.clone(), b.clone()]);
        // Idempotent re-seed: the same desired set converges; the reference
        // rows are re-proven and the revision does NOT bump.
        let expectation = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            4,
            vec![a.clone(), b.clone()],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            5,
            vec![a.clone(), b.clone()],
        ))
        .unwrap();
        let outcome = store
            .seed_task_attachments_txn(s.id, Some(expectation), &refs, write)
            .unwrap()
            .expect("the idempotent re-seed must succeed");
        assert_eq!(
            outcome.revision,
            TaskRevision::new(4),
            "a no-op re-seed must not bump the revision"
        );
        assert_eq!(
            store.get_task(s.id, task_id).unwrap().unwrap().revision,
            TaskRevision::new(4)
        );
        assert_eq!(listed_sorted(&store, s.id).len(), 2);
    }

    #[test]
    fn seed_task_attachments_txn_hostile_task_update_rolls_back_references() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "seed", "p", "m").unwrap();
        let a = AttachmentId::new(
            faktor_core::hash::FileHash::from([38; 32]),
            "image/png",
            Some("a.png"),
            3,
        )
        .unwrap();
        let task_id = TaskId::new(1);
        store
            .upsert_task(&task_seed_row(s.id, task_id, TaskState::Running, 1, vec![]))
            .unwrap();
        store
            .writer
            .execute("plant_hostile_task_update", move |conn| {
                conn.execute(
                    "CREATE TRIGGER task_update_boom BEFORE UPDATE ON task
                     BEGIN SELECT RAISE(ABORT, 'hostile task patch failure'); END",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let refs = prepared_refs(std::slice::from_ref(&a));
        let expectation = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            1,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            2,
            vec![a.clone()],
        ))
        .unwrap();
        let err = store
            .seed_task_attachments_txn(s.id, Some(expectation), &refs, write)
            .expect_err("the hostile task patch must abort the whole transaction");
        assert!(matches!(err, StoreError::Sqlite(_)), "{err:?}");
        assert!(
            listed_sorted(&store, s.id).is_empty(),
            "the reference inserted before the failing patch must roll back"
        );
        let stored = store.get_task(s.id, task_id).unwrap().unwrap();
        assert_eq!(stored.revision, TaskRevision::new(1));
        assert_eq!(stored.attachments, Vec::new());
        // Remove the hostile trigger: the same seed now converges.
        store
            .writer
            .execute("drop_hostile_task_update", move |conn| {
                conn.execute("DROP TRIGGER task_update_boom", [])?;
                Ok(())
            })
            .unwrap();
        let expectation = PreparedTaskExpectation::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            1,
            vec![],
        ))
        .unwrap();
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Running,
            2,
            vec![a.clone()],
        ))
        .unwrap();
        let outcome = store
            .seed_task_attachments_txn(s.id, Some(expectation), &refs, write)
            .unwrap()
            .expect("the retried seed must land");
        assert_eq!(outcome.revision, TaskRevision::new(2));
        assert_eq!(listed_sorted(&store, s.id), vec![a]);
    }

    #[test]
    fn seed_task_attachments_txn_hostile_reference_insert_leaves_no_task_row() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "seed", "p", "m").unwrap();
        let a = AttachmentId::new(
            faktor_core::hash::FileHash::from([39; 32]),
            "image/png",
            Some("a.png"),
            3,
        )
        .unwrap();
        let hostile = AttachmentId::new(
            faktor_core::hash::FileHash::from([40; 32]),
            "text/plain",
            Some("d.txt"),
            4,
        )
        .unwrap();
        store
            .writer
            .execute("plant_hostile_attachment_insert", move |conn| {
                conn.execute(
                    "CREATE TRIGGER attachment_boom BEFORE INSERT ON attachment
                     WHEN NEW.mime = 'text/plain'
                     BEGIN SELECT RAISE(ABORT, 'hostile reference insert'); END",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let task_id = TaskId::new(1);
        let refs = prepared_refs(&[a.clone(), hostile.clone()]);
        let write = PreparedTaskWrite::prepare(task_seed_row(
            s.id,
            task_id,
            TaskState::Pending,
            1,
            vec![a, hostile],
        ))
        .unwrap();
        let err = store
            .seed_task_attachments_txn(s.id, None, &refs, write)
            .expect_err("the hostile reference insert must abort the create");
        assert!(matches!(err, StoreError::Sqlite(_)), "{err:?}");
        assert!(
            store.get_task(s.id, task_id).unwrap().is_none(),
            "a failed create must not leave the task row"
        );
        assert!(listed_sorted(&store, s.id).is_empty());
    }
}
