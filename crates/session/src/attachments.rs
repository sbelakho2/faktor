//! Durable binary/image attachments: bytes in the CAS, typed metadata rows
//! in the store (`AttachmentId { digest, mime, filename, size }`, schema
//! v24).
//!
//! - `put_attachment` validates the mime/filename/size bounds BEFORE hashing
//!   or writing anything, streams the bytes into the CAS (content-addressed,
//!   dedupe + corruption detection come from the CAS), then persists one
//!   typed metadata row keyed `(session_id, digest)`.
//! - Dedupe by digest: an identical payload resolves to the FIRST-written
//!   metadata row — repeated uploads are idempotent and return a
//!   byte-identical [`AttachmentId`].
//! - `attachment`/`list_attachments` resolve the durable rows after a
//!   restart (never a process-local map); `attachment_bytes` re-verifies the
//!   CAS blob and refuses a metadata/blob size mismatch.
//!
//! The attachment is deliberately SEPARATE from the workspace-relative
//! `files` vocabulary: an attachment is bytes addressed by digest, never a
//! path. Model delivery resolves bytes HERE at request construction
//! ([`SessionHandle::resolve_attachment_bytes`] for images,
//! [`SessionHandle::resolve_document_bytes`] for non-image documents) into
//! `faktor_provider::ContentKind::ImageData` / `FileData` parts; the durable
//! task JSON stores only the `AttachmentId`, and the provider adapters own
//! their own wire encodings (base64 source for Anthropic/Google, data URL
//! for OpenAI, raw base64 for Ollama). Admission validates the media
//! mime/size against the chosen model's capabilities before any run/task row
//! exists.

use faktor_core::attachment::{
    validate_filename, validate_mime, AttachmentId, MAX_ATTACHMENTS_PER_TASK, MAX_ATTACHMENT_BYTES,
};
use faktor_core::error::{Error, ErrorKind};
use faktor_core::hash::FileHash;

use crate::handle::SessionHandle;
use crate::SessionError;

/// One durable attachment resolved to its verified bytes: the metadata row
/// plus the CAS bytes it addresses. Returned ONLY by
/// [`SessionHandle::resolve_attachment_bytes`] — the request-construction
/// read path — so the agent never handles bytes detached from their
/// identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAttachment {
    pub id: AttachmentId,
    pub bytes: Vec<u8>,
}

impl SessionHandle {
    /// Store a bounded attachment: validate bounds/shape, put the bytes into
    /// the CAS, persist the typed metadata row. Returns the (possibly
    /// pre-existing) canonical [`AttachmentId`]. Hostile mime/filename shapes
    /// and oversized payloads are typed refusals before any write.
    pub fn put_attachment(
        &self,
        mime: &str,
        filename: Option<&str>,
        bytes: &[u8],
    ) -> faktor_core::Result<AttachmentId> {
        let canonical_mime = mime.trim().to_ascii_lowercase();
        validate_mime(&canonical_mime)?;
        if let Some(name) = filename {
            validate_filename(name)?;
        }
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(SessionError::Oversized(format!(
                "attachment of {} bytes exceeds MAX_ATTACHMENT_BYTES ({MAX_ATTACHMENT_BYTES})",
                bytes.len()
            ))
            .into());
        }
        let digest = self.manager.cas().put(bytes).map_err(SessionError::from)?;
        let computed = AttachmentId {
            digest,
            mime: canonical_mime,
            filename: filename.map(str::to_string),
            size: bytes.len() as u64,
        };
        let stored = self
            .manager
            .store()
            .put_attachment(self.id, &computed)
            .map_err(crate::map_store_err)?;
        if stored.size != computed.size {
            return Err(SessionError::Malformed(format!(
                "attachment {digest} row size {} does not match the {} uploaded bytes (durable row corruption)",
                stored.size,
                computed.size
            ))
            .into());
        }
        Ok(stored)
    }

    /// Copy one already-admitted attachment into THIS session (the
    /// orchestrated-child inheritance path): the id is structurally
    /// validated, the CAS blob at its digest is verified to EXIST and hash
    /// correctly (`Cas::verify_now`, streamed — no bytes materialize), and
    /// the typed metadata row is written idempotently for this session. The
    /// child's request construction then re-verifies the blob like any
    /// other attachment. A missing/tampered blob is a typed refusal, never
    /// an inherited phantom row.
    pub fn inherit_attachment(&self, id: &AttachmentId) -> faktor_core::Result<AttachmentId> {
        id.validate()?;
        self.manager
            .cas()
            .verify_now(&id.digest.to_hex())
            .map_err(SessionError::from)?;
        let stored = self
            .manager
            .store()
            .put_attachment(self.id, id)
            .map_err(crate::map_store_err)?;
        if stored != *id {
            return Err(Error::new(
                ErrorKind::Malformed,
                format!(
                    "inherited attachment {} conflicts with an existing row of session {}",
                    id.digest, self.id
                ),
            ));
        }
        Ok(stored)
    }

    /// Resolve one durable attachment row by its digest (restart-safe).
    pub fn attachment(&self, digest: FileHash) -> faktor_core::Result<Option<AttachmentId>> {
        self.manager
            .store()
            .attachment(self.id, digest)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// The session's durable attachment rows, bounded page.
    pub fn list_attachments(&self, limit: usize) -> faktor_core::Result<Vec<AttachmentId>> {
        self.manager
            .store()
            .list_attachments(self.id, limit)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Read the attachment bytes back, verifying the CAS blob against the
    /// durable metadata: the tracked size is checked before any I/O and the
    /// decoded byte count must equal the row's `size` (a mismatch is loud,
    /// never served as the attachment).
    pub fn attachment_bytes(
        &self,
        id: &AttachmentId,
        max_bytes: usize,
    ) -> faktor_core::Result<Vec<u8>> {
        if id.size > max_bytes as u64 {
            return Err(SessionError::Oversized(format!(
                "attachment {} is {} bytes, limit {max_bytes}",
                id.digest, id.size
            ))
            .into());
        }
        let bytes = self
            .manager
            .cas()
            .get_verified_now(id.digest)
            .map_err(SessionError::from)?;
        if bytes.len() as u64 != id.size {
            return Err(SessionError::Malformed(format!(
                "attachment {} decoded to {} bytes, metadata says {}",
                id.digest,
                bytes.len(),
                id.size
            ))
            .into());
        }
        Ok(bytes)
    }

    /// Resolve and validate one attachment SET at admission time: the count
    /// is bounded by [`MAX_ATTACHMENTS_PER_TASK`], every id is structurally
    /// validated (hostile mime/filename/size are typed refusals), and every
    /// digest must resolve to a byte-identical durable row of THIS session.
    /// Admission never fabricates an attachment: an unknown digest is a
    /// typed `NotFound`, a mismatched durable row a typed `Malformed`.
    ///
    /// Media policy lives ABOVE this layer: provider admission validates the
    /// image mime/size against the chosen model's capabilities, and request
    /// construction resolves the bytes.
    pub fn resolve_attachments(&self, ids: &[AttachmentId]) -> faktor_core::Result<()> {
        if ids.len() > MAX_ATTACHMENTS_PER_TASK {
            return Err(Error::new(
                ErrorKind::Oversized,
                format!(
                    "{} attachments exceed MAX_ATTACHMENTS_PER_TASK ({MAX_ATTACHMENTS_PER_TASK})",
                    ids.len()
                ),
            ));
        }
        for id in ids {
            id.validate()?;
            match self.attachment(id.digest)? {
                Some(stored) if stored == *id => {}
                Some(stored) => {
                    return Err(Error::new(
                        ErrorKind::Malformed,
                        format!(
                            "attachment {} does not match the durable row (requested {} bytes / {} mime, stored {} bytes / {} mime)",
                            id.digest, id.size, id.mime, stored.size, stored.mime
                        ),
                    ));
                }
                None => {
                    return Err(Error::new(
                        ErrorKind::NotFound,
                        format!(
                            "attachment {} is not stored in session {}",
                            id.digest, self.id
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Resolve a bounded attachment SET to verified bytes (the
    /// REQUEST-CONSTRUCTION read path). Order is the input order. Every id
    /// is re-validated and must resolve byte-identically
    /// ([`Self::resolve_attachments`]); each blob must fit `max_bytes_each`
    /// and the running total must fit `max_total_bytes` — both checked from
    /// the DURABLE size before any read, then re-verified against the
    /// decoded blob (`attachment_bytes`). A missing or tampered CAS blob is
    /// a typed error: no caller ever receives bytes that do not hash to
    /// their attachment digest.
    pub fn resolve_attachment_bytes(
        &self,
        ids: &[AttachmentId],
        max_bytes_each: usize,
        max_total_bytes: usize,
    ) -> faktor_core::Result<Vec<ResolvedAttachment>> {
        self.resolve_attachments(ids)?;
        let mut total: u64 = 0;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if id.size > max_bytes_each as u64 {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    format!(
                        "attachment {} is {} bytes, per-attachment limit {max_bytes_each}",
                        id.digest, id.size
                    ),
                ));
            }
            total = total.saturating_add(id.size);
            if total > max_total_bytes as u64 {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    format!(
                        "attachment set totals {total} bytes, exceeding the {max_total_bytes} byte request bound"
                    ),
                ));
            }
            let bytes = self.attachment_bytes(id, max_bytes_each)?;
            out.push(ResolvedAttachment {
                id: id.clone(),
                bytes,
            });
        }
        Ok(out)
    }

    /// Resolve a bounded DOCUMENT attachment SET to verified bytes (the
    /// request-construction read path for non-image documents). Identical
    /// guarantees to [`Self::resolve_attachment_bytes`] — re-validated ids,
    /// byte-identical durable rows, durable-size bounds before any read —
    /// plus the document path's own structural rules: an IMAGE row passed to
    /// the document path is a typed refusal (the two delivery paths never
    /// cross), and a zero-byte row is refused (it can never be a valid
    /// document part). An empty set resolves to an empty list.
    pub fn resolve_document_bytes(
        &self,
        ids: &[AttachmentId],
        max_bytes_each: usize,
        max_total_bytes: usize,
    ) -> faktor_core::Result<Vec<ResolvedAttachment>> {
        for id in ids {
            if id.is_image() {
                return Err(Error::new(
                    ErrorKind::Malformed,
                    format!(
                        "attachment {} has image mime {:?} and cannot ride the document delivery path",
                        id.digest, id.mime
                    ),
                ));
            }
            if id.size == 0 {
                return Err(Error::new(
                    ErrorKind::Malformed,
                    format!(
                        "attachment {} is zero bytes and can never be a valid document part",
                        id.digest
                    ),
                ));
            }
        }
        self.resolve_attachment_bytes(ids, max_bytes_each, max_total_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::tests::{session, test_manager};
    use crate::SessionManager;

    #[test]
    fn upload_is_durable_typed_deduped_and_survives_reopen() {
        let (dir, m) = test_manager();
        let s = session(&m);
        let bytes = b"durable attachment bytes".to_vec();
        let first = s
            .put_attachment("Image/PNG", Some("shot.png"), &bytes)
            .unwrap();
        assert_eq!(first.mime, "image/png", "mime is canonicalized");
        assert_eq!(first.size, bytes.len() as u64);
        assert!(first.is_image());
        // Dedupe by digest: same bytes (even with different metadata) return
        // the FIRST durable row byte-identically.
        let again = s
            .put_attachment("application/pdf", Some("other.pdf"), &bytes)
            .unwrap();
        assert_eq!(again, first, "dedupe by digest returns the stored id");
        assert_eq!(s.list_attachments(16).unwrap(), vec![first.clone()]);
        // Bytes resolve from the CAS.
        assert_eq!(s.attachment_bytes(&first, 1 << 20).unwrap(), bytes);
        // Bounded reads refuse before I/O.
        let err = s.attachment_bytes(&first, 4).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized);

        // Reopen the REAL store/cas from disk: the typed row resolves
        // identically and the bytes still verify.
        drop(s);
        drop(m);
        let m2 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let h = m2
            .get_session(faktor_core::id::SessionId::new(1))
            .unwrap()
            .unwrap();
        assert_eq!(h.attachment(first.digest).unwrap(), Some(first.clone()));
        assert_eq!(h.list_attachments(16).unwrap(), vec![first.clone()]);
        assert_eq!(h.attachment_bytes(&first, 1 << 20).unwrap(), bytes);
        // An unknown digest is an honest absence, never a phantom.
        assert_eq!(h.attachment(FileHash::from([9; 32])).unwrap(), None);
    }

    /// The document delivery path: byte-identical resolution of a small PDF
    /// fixture, path crossing and zero-byte refusals, and the durable
    /// bounds — while the image path is untouched.
    #[test]
    fn document_bytes_resolve_byte_exactly_and_crosses_are_refused() {
        let (_dir, m) = test_manager();
        let s = session(&m);
        // A small byte-exact PDF fixture.
        let pdf = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
        let doc = s
            .put_attachment("application/pdf", Some("spec.pdf"), &pdf)
            .unwrap();
        let text = s
            .put_attachment("text/plain", Some("notes.txt"), b"plain text\n")
            .unwrap();
        let resolved = s
            .resolve_document_bytes(&[doc.clone(), text.clone()], 1 << 20, 1 << 20)
            .unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].id, doc);
        assert_eq!(resolved[0].bytes, pdf, "the PDF bytes lower byte-exactly");
        assert_eq!(resolved[1].bytes, b"plain text\n");
        // An empty set is fine and reads nothing.
        assert!(s
            .resolve_document_bytes(&[], 1 << 20, 1 << 20)
            .unwrap()
            .is_empty());

        // An IMAGE row can never ride the document path.
        let img = s
            .put_attachment("image/png", Some("shot.png"), b"\x89PNG")
            .unwrap();
        let err = s
            .resolve_document_bytes(std::slice::from_ref(&img), 1 << 20, 1 << 20)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Malformed);
        assert!(err.message.contains("document delivery path"), "{err}");

        // A zero-byte row is refused before any read.
        let empty = s
            .put_attachment("application/pdf", Some("empty.pdf"), b"")
            .unwrap();
        let err = s
            .resolve_document_bytes(&[empty], 1 << 20, 1 << 20)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Malformed);
        assert!(err.message.contains("zero bytes"), "{err}");

        // Durable bounds apply from the metadata BEFORE any blob read.
        let err = s
            .resolve_document_bytes(std::slice::from_ref(&doc), 4, 1 << 20)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized);
        let err = s
            .resolve_document_bytes(&[doc, text], 1 << 20, 4)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized);

        // The image path still resolves the image byte-identically.
        assert_eq!(
            s.resolve_attachment_bytes(&[img], 1 << 20, 1 << 20)
                .unwrap()[0]
                .bytes,
            b"\x89PNG"
        );
    }

    #[test]
    fn hostile_attachments_are_typed_refusals_before_any_write() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let payload = b"payload".to_vec();
        for mime in [
            "",
            "image",
            "image/",
            "/png",
            "image/png/extra",
            "bad mime/type",
        ] {
            let err = s.put_attachment(mime, None, &payload).unwrap_err();
            assert!(
                matches!(
                    err.kind,
                    faktor_core::ErrorKind::Malformed | faktor_core::ErrorKind::Oversized
                ),
                "mime {mime:?} => {:?}",
                err.kind
            );
        }
        for name in ["../secrets", "a/b.png", "a\\b.png", ".", "..", "bad\0name"] {
            let err = s
                .put_attachment("image/png", Some(name), &payload)
                .unwrap_err();
            assert!(
                matches!(
                    err.kind,
                    faktor_core::ErrorKind::Malformed | faktor_core::ErrorKind::Oversized
                ),
                "filename {name:?} => {:?}",
                err.kind
            );
        }
        // Oversized payloads are refused BEFORE hashing/writing anything.
        let big = vec![0u8; MAX_ATTACHMENT_BYTES as usize + 1];
        let err = s.put_attachment("image/png", None, &big).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized);
        // Nothing durable was left behind by any hostile attempt.
        assert!(s.list_attachments(16).unwrap().is_empty());
    }

    #[test]
    fn corrupted_digest_metadata_mismatch_is_loud() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let id = s
            .put_attachment("application/octet-stream", None, b"1234")
            .unwrap();
        // A forged id naming the real digest but a different size can never
        // be served as the attachment.
        let forged = AttachmentId {
            size: id.size + 1,
            ..id.clone()
        };
        let err = s.attachment_bytes(&forged, 1 << 20).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Malformed);
    }

    #[test]
    fn child_inherits_attachment_rows_only_from_verified_blobs() {
        let (_d, m) = test_manager();
        let parent = session(&m);
        let child = session(&m);
        let id = parent
            .put_attachment("image/png", Some("shot.png"), b"\x89PNG-data")
            .unwrap();
        // A phantom digest (no CAS blob) can never be inherited.
        let phantom = AttachmentId {
            digest: FileHash::from([42; 32]),
            ..id.clone()
        };
        let err = child.inherit_attachment(&phantom).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::NotFound);
        assert_eq!(
            child.attachment(phantom.digest).unwrap(),
            None,
            "no phantom row may land in the child session"
        );
        // Real inheritance writes the row; bytes resolve in the child only
        // after the blob verified.
        assert_eq!(child.inherit_attachment(&id).unwrap(), id);
        assert_eq!(
            child.attachment_bytes(&id, 1 << 20).unwrap(),
            b"\x89PNG-data"
        );
        // Idempotent re-inheritance (crash re-attach path).
        assert_eq!(child.inherit_attachment(&id).unwrap(), id);
        assert_eq!(child.list_attachments(16).unwrap(), vec![id]);
    }

    #[test]
    fn set_read_preserves_order_and_refuses_bounds_before_reading() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let first = s
            .put_attachment("image/png", Some("a.png"), b"\x89PNG-a")
            .unwrap();
        let second = s
            .put_attachment("image/jpeg", Some("b.jpg"), b"\xff\xd8-b")
            .unwrap();
        let resolved = s
            .resolve_attachment_bytes(&[first.clone(), second.clone()], 1 << 20, 1 << 20)
            .unwrap();
        assert_eq!(
            resolved.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![first.clone(), second.clone()],
            "input order is preserved exactly"
        );
        assert_eq!(resolved[0].bytes, b"\x89PNG-a");
        assert_eq!(resolved[1].bytes, b"\xff\xd8-b");
        // A per-attachment bound is typed BEFORE any blob is read.
        let err = s
            .resolve_attachment_bytes(&[first.clone(), second.clone()], 3, 1 << 20)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized);
        // A total bound is typed as well.
        let err = s
            .resolve_attachment_bytes(&[first.clone(), second.clone()], 1 << 20, 8)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized);
        // An unknown digest never materializes bytes.
        let unknown = AttachmentId {
            digest: FileHash::from([6; 32]),
            ..first.clone()
        };
        let err = s
            .resolve_attachment_bytes(&[unknown], 1 << 20, 1 << 20)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::NotFound);
    }

    #[test]
    fn missing_or_tampered_cas_blob_is_a_typed_refusal() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let id = s
            .put_attachment("image/png", Some("shot.png"), b"\x89PNG-real")
            .unwrap();
        let blob = m.cas().root().join(id.digest.cas_path());
        // Missing blob: a typed absence, never fabricated bytes.
        std::fs::remove_file(&blob).unwrap();
        let err = s
            .resolve_attachment_bytes(std::slice::from_ref(&id), 1 << 20, 1 << 20)
            .unwrap_err();
        assert!(
            matches!(
                err.kind,
                faktor_core::ErrorKind::NotFound | faktor_core::ErrorKind::Store
            ),
            "missing blob => {:?}",
            err.kind
        );
        // Tampered blob (same length, different bytes): the CAS re-hash is
        // loud — the attachment is never served as its digest claims.
        std::fs::write(&blob, b"\x89PNG-evil").unwrap();
        let err = s
            .resolve_attachment_bytes(std::slice::from_ref(&id), 1 << 20, 1 << 20)
            .unwrap_err();
        assert!(
            matches!(
                err.kind,
                faktor_core::ErrorKind::Malformed | faktor_core::ErrorKind::Store
            ),
            "tampered blob => {:?}",
            err.kind
        );
    }

    #[test]
    fn admission_resolution_requires_durable_byte_identical_rows() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let stored = s
            .put_attachment("application/pdf", Some("spec.pdf"), b"%PDF-1.4")
            .unwrap();
        // A stored, byte-identical set resolves cleanly (dedupe included).
        s.resolve_attachments(std::slice::from_ref(&stored))
            .unwrap();
        s.resolve_attachments(&[]).unwrap();
        // An unknown digest is a typed absence, never a phantom admission.
        let unknown = AttachmentId {
            digest: FileHash::from([7; 32]),
            ..stored.clone()
        };
        let err = s.resolve_attachments(&[unknown]).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::NotFound);
        // A mismatched durable row (same digest, different metadata) is a
        // typed refusal: admission trusts the durable row, not the request.
        let mismatched = AttachmentId {
            mime: "text/plain".into(),
            ..stored.clone()
        };
        let err = s.resolve_attachments(&[mismatched]).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Malformed);
        // Structurally hostile ids are refused before any store read.
        let hostile = AttachmentId {
            filename: Some("../secrets".into()),
            ..stored.clone()
        };
        let err = s.resolve_attachments(&[hostile]).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Malformed);
        // The count bound is typed and inclusive.
        let many = vec![stored; MAX_ATTACHMENTS_PER_TASK + 1];
        let err = s.resolve_attachments(&many).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized);
    }
}
