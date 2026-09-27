//! `runtime::media`: cohesive slice of the agent runtime.

use super::*;

impl AgentRuntime {
    /// Admission-time provider facts for media validation: the capabilities
    /// of `provider`/`model` plus the provider's per-image byte bound.
    /// `None` when the provider is not registered — admission callers fail
    /// closed (a typed refusal, never an assume-vision default).
    pub fn provider_media_caps(
        &self,
        provider: &str,
        model: &str,
    ) -> Option<(faktor_core::model::ModelCapabilities, usize)> {
        self.deps
            .providers
            .get(provider)
            .map(|p| (p.capabilities(model), p.max_image_bytes()))
    }

    /// Resolve the durable task's IMAGE and non-image DOCUMENT attachments
    /// into bounded in-memory media parts at REQUEST CONSTRUCTION and append
    /// them (input order preserved) to the newest user message carrying
    /// TEXT — the turn's own prompt — synthesizing one only when the history
    /// has none (e.g. after compaction dropped it). Non-document
    /// attachments (e.g. archives) are not model content and stay CAS-only.
    ///
    /// Every failure is typed and happens BEFORE any provider call:
    ///
    /// - an image against a model without [`ModelCapabilities::vision`]
    ///   refuses loudly (never a silently dropped image);
    /// - a document against a provider whose
    ///   [`faktor_provider::Provider::document_capable`] flag is false
    ///   refuses loudly (the vision-like document gate — never a silently
    ///   dropped document, never an invented text extraction);
    /// - the mime must be one of the provider's supported image/document
    ///   types;
    /// - the provider's per-part bounds and the request-wide media/document
    ///   bounds are checked from DURABLE sizes before any blob read;
    /// - a missing/tampered CAS blob is a typed error (the session layer
    ///   re-hashes every blob against its digest).
    ///
    /// The bytes exist only in this request: the durable task row keeps
    /// `AttachmentId`s, so re-attach re-resolves the byte-identical set.
    pub(crate) fn inject_attachment_media(
        &self,
        handle: &faktor_session::SessionHandle,
        history: &mut Vec<RequestMessage>,
        caps: &faktor_core::model::ModelCapabilities,
        provider: &dyn faktor_provider::Provider,
    ) -> faktor_core::Result<()> {
        let Some(task) = handle.get_task(handle.task_id()?)? else {
            return Ok(());
        };
        let images: Vec<faktor_core::attachment::AttachmentId> = task
            .attachments
            .iter()
            .filter(|a| a.is_image())
            .cloned()
            .collect();
        let documents: Vec<faktor_core::attachment::AttachmentId> = task
            .attachments
            .iter()
            .filter(|a| !a.is_image() && faktor_provider::is_supported_document_mime(&a.mime))
            .cloned()
            .collect();
        if images.is_empty() && documents.is_empty() {
            return Ok(());
        }
        let model = handle.model().unwrap_or_default();
        // Capability gates FIRST: nothing is read or resolved before every
        // gate that can refuse the whole set has passed.
        if !images.is_empty() && !caps.vision {
            return Err(Error::malformed(format!(
                "model {model} does not support vision, but task {} carries {} image attachment(s); remove them or select a vision model",
                task.task_id,
                images.len()
            )));
        }
        if !documents.is_empty() && !provider.document_capable(&model) {
            return Err(Error::malformed(format!(
                "the provider of model {model} does not support document input, but task {} carries {} document attachment(s); remove them or select a document-capable model",
                task.task_id,
                documents.len()
            )));
        }
        for id in &images {
            if !faktor_provider::is_supported_image_mime(&id.mime) {
                return Err(Error::new(
                    ErrorKind::Malformed,
                    format!(
                        "image attachment {} has unsupported mime {:?}; deliverable types: {}",
                        id.digest,
                        id.mime,
                        faktor_provider::SUPPORTED_IMAGE_MIMES.join(", ")
                    ),
                ));
            }
        }
        // The prompt message: the newest user message that carries text —
        // never a tool-result-only user message. Deduped per KIND if this
        // history was injected already in-process.
        let target = history
            .iter()
            .rposition(|m| {
                m.role == Role::User
                    && m.content.iter().any(|p| {
                        matches!(&p.kind, faktor_provider::ContentKind::Text { text } if !text.is_empty())
                    })
            })
            .or_else(|| history.iter().rposition(|m| m.role == Role::User));
        let has_images = target.is_some_and(|i| {
            history[i]
                .content
                .iter()
                .any(|p| matches!(p.kind, faktor_provider::ContentKind::ImageData { .. }))
        });
        let has_documents = target.is_some_and(|i| {
            history[i]
                .content
                .iter()
                .any(|p| matches!(p.kind, faktor_provider::ContentKind::FileData { .. }))
        });
        let need_images = !images.is_empty() && !has_images;
        let need_documents = !documents.is_empty() && !has_documents;
        if !need_images && !need_documents {
            return Ok(());
        }
        // Defense in depth behind admission: the provider bounds and the
        // request-wide bounds are re-checked here, from durable sizes.
        let resolved_images = if need_images {
            let per_image = provider
                .max_image_bytes()
                .min(faktor_provider::MAX_MEDIA_BYTES_HARD);
            handle.resolve_attachment_bytes(
                &images,
                per_image,
                faktor_provider::MAX_REQUEST_IMAGE_BYTES,
            )?
        } else {
            Vec::new()
        };
        let resolved_documents = if need_documents {
            let per_document = provider
                .max_document_bytes()
                .min(faktor_provider::MAX_MEDIA_BYTES_HARD);
            handle.resolve_document_bytes(
                &documents,
                per_document,
                faktor_provider::MAX_REQUEST_DOCUMENT_BYTES,
            )?
        } else {
            Vec::new()
        };
        let mut parts = Vec::with_capacity(resolved_images.len() + resolved_documents.len());
        for r in resolved_images {
            parts.push(ContentPart::image_data(&r.id.mime, r.bytes)?);
        }
        for r in resolved_documents {
            parts.push(ContentPart::file_data(
                &r.id.mime,
                r.id.filename.as_deref(),
                r.bytes,
            )?);
        }
        match target {
            Some(i) => history[i].content.extend(parts),
            None => history.push(RequestMessage {
                role: Role::User,
                content: parts,
            }),
        }
        Ok(())
    }
}
