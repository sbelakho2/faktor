//! `attachments`: cohesive slice of the mechanically decomposed parent module.

use super::*;

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
        self.writer.execute("put_artifact", move |conn| {
            conn.execute(
            "INSERT OR IGNORE INTO artifact(session_id, kind, cas_hash, summary, created_ms, size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id.raw() as i64,
                kind,
                cas_hash,
                summary,
                now_ms(),
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

    /// Persist one binary/image attachment's typed metadata (schema v24).
    /// Dedupe IS the primary key `(session_id, digest)`: an identical digest
    /// is a no-op and the FIRST-written row is read back and returned, so
    /// repeated uploads of the same bytes resolve to one byte-identical
    /// `AttachmentId`. The caller (session layer) validates the id before
    /// this write; the store only persists/reads the typed row.
    pub fn put_attachment(
        &self,
        session_id: SessionId,
        attachment: &AttachmentId,
    ) -> StoreResult<AttachmentId> {
        let attachment = attachment.to_owned();
        self.writer.execute("put_attachment", move |conn| {
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
            let stored = conn.query_row(
                "SELECT digest, mime, filename, size FROM attachment
             WHERE session_id = ?1 AND digest = ?2",
                params![session_id.raw() as i64, attachment.digest.to_hex()],
                |r| {
                    let digest: String = r.get(0)?;
                    let mime: String = r.get(1)?;
                    let filename: Option<String> = r.get(2)?;
                    let size: i64 = r.get(3)?;
                    let digest =
                        faktor_core::hash::FileHash::from_hex(&digest).ok_or_else(|| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("attachment digest {digest:?} is not 32-byte hex"),
                                )),
                            )
                        })?;
                    if size < 0 {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            3,
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
                },
            )?;
            Ok(stored)
        })
    }

    /// Resolve one durable attachment row by its digest (restart-safe: reads
    /// the typed metadata, never a process-local map).
    pub fn attachment(
        &self,
        session_id: SessionId,
        digest: faktor_core::hash::FileHash,
    ) -> StoreResult<Option<AttachmentId>> {
        let conn = self.read()?;
        let out = conn
            .query_row(
                "SELECT digest, mime, filename, size FROM attachment
                 WHERE session_id = ?1 AND digest = ?2",
                params![session_id.raw() as i64, digest.to_hex()],
                |r| {
                    let digest_raw: String = r.get(0)?;
                    let mime: String = r.get(1)?;
                    let filename: Option<String> = r.get(2)?;
                    let size: i64 = r.get(3)?;
                    let digest =
                        faktor_core::hash::FileHash::from_hex(&digest_raw).ok_or_else(|| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("attachment digest {digest_raw:?} is not 32-byte hex"),
                                )),
                            )
                        })?;
                    if size < 0 {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            3,
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
                },
            )
            .optional()?;
        Ok(out)
    }

    /// The session's durable attachment metadata, newest digest order (a
    /// bounded, deterministic page: at most `limit` rows).
    pub fn list_attachments(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> StoreResult<Vec<AttachmentId>> {
        let conn = self.read()?;
        let mut stmt = conn.prepare(
            "SELECT digest, mime, filename, size FROM attachment
             WHERE session_id = ?1 ORDER BY digest ASC LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![session_id.raw() as i64, limit as i64])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let digest_raw: String = r.get(0)?;
            let Some(digest) = faktor_core::hash::FileHash::from_hex(&digest_raw) else {
                return Err(StoreError::Corrupt(vec![format!(
                    "attachment digest {digest_raw:?} is not 32-byte hex"
                )]));
            };
            let size: i64 = r.get(3)?;
            if size < 0 {
                return Err(StoreError::Corrupt(vec![format!(
                    "attachment size {size} is negative"
                )]));
            }
            out.push(AttachmentId {
                digest,
                mime: r.get(1)?,
                filename: r.get(2)?,
                size: size as u64,
            });
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
    fn durable_attachment_rows_dedupe_by_digest_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s1 = store.create_session(ws, "att", "p", "m").unwrap();
        let ws2 = store.create_workspace("/w2").unwrap();
        let s2 = store.create_session(ws2, "other", "p", "m").unwrap();
        let digest = faktor_core::hash::FileHash::from([7; 32]);
        let id = AttachmentId::new(digest, "image/png", Some("shot.png"), 123).unwrap();
        let first = store.put_attachment(s1.id, &id).unwrap();
        assert_eq!(first, id);
        // Dedupe by digest: a second write of the same digest is a no-op and
        // the FIRST row's metadata is authoritative (never rewritten).
        let conflicting = AttachmentId::new(digest, "application/pdf", Some("other"), 123).unwrap();
        assert_eq!(store.put_attachment(s1.id, &conflicting).unwrap(), first);
        assert_eq!(
            store.attachment(s1.id, digest).unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            store.list_attachments(s1.id, 10).unwrap(),
            vec![first.clone()]
        );
        // Session scope: another session never sees the row.
        assert!(store.attachment(s2.id, digest).unwrap().is_none());
        // Reopen: the typed row resolves identically from disk.
        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert_eq!(store.attachment(s1.id, digest).unwrap(), Some(first));
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
