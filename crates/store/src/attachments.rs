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
        let attachment = attachment.to_owned();
        self.writer.execute("put_attachment_ref", move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO attachment(session_id, digest, mime, filename, size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id.raw() as i64,
                    attachment.digest.to_hex(),
                    attachment.mime.as_str(),
                    attachment.filename.as_deref(),
                    attachment.size as i64,
                ],
            )?;
            conn.query_row(
                "SELECT id, digest, mime, filename, size FROM attachment
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
                row_to_attachment_ref,
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::Corrupt(vec![format!(
                    "attachment reference (session {session_id}, digest {}, mime {:?}, filename {:?}, size {}) not found after insert",
                    attachment.digest, attachment.mime, attachment.filename, attachment.size
                )])
            })
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
        let attachments = attachments.to_vec();
        self.writer.execute("inherit_attachment_refs", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut out = Vec::with_capacity(attachments.len());
            for attachment in &attachments {
                tx.execute(
                    "INSERT OR IGNORE INTO attachment(session_id, digest, mime, filename, size)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        session_id.raw() as i64,
                        attachment.digest.to_hex(),
                        attachment.mime.as_str(),
                        attachment.filename.as_deref(),
                        attachment.size as i64,
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
                            attachment.digest.to_hex(),
                            attachment.mime.as_str(),
                            attachment.filename.as_deref(),
                            attachment.size as i64,
                        ],
                        row_to_attachment_ref,
                    )
                    .optional()?
                    .ok_or_else(|| {
                        StoreError::Corrupt(vec![format!(
                            "attachment reference (session {session_id}, digest {}, mime {:?}, filename {:?}, size {}) not found after insert",
                            attachment.digest, attachment.mime, attachment.filename, attachment.size
                        )])
                    })?;
                out.push(reference);
            }
            tx.commit()?;
            Ok(out)
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
}
