//! Persisted chunk embeddings for the repository index (spec §19/§20).
//!
//! Semantic retrieval must not re-embed an unchanged corpus on every
//! re-index. Chunk vectors are therefore durable index data, keyed by
//!
//! ```text
//! (content_hash, model_id, model_revision, dimension)
//! ```
//!
//! so that:
//!
//! - an unchanged chunk survives any number of BUILD generation swaps
//!   (index generation `g` → `g+1`) and daemon restarts — the key does not
//!   contain a path, mtime or generation, so rename/re-order/re-index all
//!   reuse the same vector;
//! - a changed chunk hashes differently and is the only text re-embedded;
//! - a model change or a model REVISION change invalidates every old vector
//!   (the identity is part of the key and old-identity records are pruned),
//!   and a dimension drift under the same identity invalidates the carried
//!   records too.
//!
//! Embeddings ride inside the durable generation envelope
//! ([`crate::generation::WorkspaceData::embeddings`]) — the same atomic
//! publish that makes a generation visible makes its vectors visible — and
//! are bounded on every axis: chunks per file, chunk bytes, embeds per
//! build, records and total components per workspace, and vector dimension.
//! The embedding source is an optional, synchronous seam
//! ([`EmbeddingSource`]); without one the index simply carries persisted
//! vectors forward (search can still use them) and never calls out.

use std::collections::BTreeMap;

use faktor_core::error::{Error, ErrorKind};
use faktor_core::id::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::WorkspaceIndex;

/// Format tag of the persisted embedding index; bumped on incompatible
/// shapes. A generation file carrying another tag loads as "no embeddings"
/// (loudly), never as garbage.
pub const EMBEDDING_FORMAT: u32 = 1;

/// Hard bound on one persisted vector's dimension (mirror of the provider
/// crate's `MAX_EMBEDDING_DIMENSIONS`: the index crate cannot depend on the
/// provider crate, so the bound is duplicated deliberately).
pub const MAX_EMBEDDING_DIMENSIONS: usize = 8_192;
/// Hard bound on persisted records per workspace (pruned deterministically).
pub const MAX_EMBEDDING_RECORDS: usize = 16_384;
/// Hard bound on the summed vector components persisted per workspace.
pub const MAX_EMBEDDING_COMPONENTS: usize = 32 * 1024 * 1024;
/// Target bytes of one embedded chunk (a file is split into windows this
/// size at newline/char boundaries).
pub const EMBEDDING_CHUNK_BYTES: usize = 2_048;
/// Chunks of one file that participate in embedding (the rest of a huge
/// file is lexical-only).
pub const MAX_EMBEDDING_CHUNKS_PER_FILE: usize = 64;
/// Missing chunks embedded in ONE build; beyond this the build publishes
/// without vectors for the remainder (next build resumes).
pub const MAX_EMBED_CHUNKS_PER_BUILD: usize = 1_024;
/// Total chunk TEXT bytes collected for embedding in one scan.
pub const MAX_EMBEDDING_SCAN_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// The synchronous embedding seam the build pipeline drives. Implementations
/// own batching/retries (the CLI's `ProviderEmbedder` already bounds batches
/// and follows the configured retry policy); the index validates every
/// response before persisting it and degrades (never fails the build) when
/// the source errors or answers a hostile shape. Every degradation is TYPED
/// and observable — the affected chunk count and the bounded provider error
/// are recorded in [`EmbeddingStats`] and persisted on the generation's
/// [`EmbeddingIndex::build_status`] (`Degraded`) — never a silent
/// lexical-only success.
pub trait EmbeddingSource: Send + Sync {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error>;
}

/// The configured embedding model identity. `revision` is the operator's
/// pin for a mutable model tag (a model pulled at a new revision must not
/// silently reuse old vectors); it is opaque to the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingModel {
    pub model_id: String,
    pub revision: String,
}

impl EmbeddingModel {
    pub fn new(model_id: impl Into<String>, revision: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            revision: revision.into(),
        }
    }
}

/// One persisted vector. The record repeats the full key so a serialized
/// store is self-describing and a key/record mismatch is detectable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingRecord {
    pub content_hash: String,
    pub model_id: String,
    pub model_revision: String,
    pub dimension: u32,
    pub vector: Vec<f32>,
}

/// Hard byte bound of the persisted degraded-build reason (a hostile
/// generation file cannot smuggle an unbounded string into memory or logs).
pub const EMBEDDING_BUILD_REASON_MAX_BYTES: usize = 512;

/// Why one build-time source call produced no vectors. Recorded (never
/// fatal): the build still publishes, but the outcome is typed and
/// observable instead of silently lexical-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedFailure {
    /// The source returned a typed error.
    Source {
        /// Provider code when the source surfaced one, else the error kind.
        code: String,
        /// Whether the source marked the failure retryable.
        retryable: bool,
        /// Bounded provider message.
        message: String,
    },
    /// The source panicked; the build catches it (the source is untrusted
    /// input to the build).
    Panicked,
}

impl EmbedFailure {
    /// Typed construction from the provider error, preserving the provider
    /// code/retryability when the error carries it.
    pub fn from_error(error: Error) -> Self {
        let (code, retryable) = match &error.kind {
            ErrorKind::Provider { code, retryable } => (code.clone(), *retryable),
            kind => (format!("{kind:?}"), error.retryable),
        };
        Self::Source {
            code,
            retryable,
            message: bounded_build_reason(&error.message),
        }
    }

    /// Bounded one-line description for stats/status/logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Source {
                code,
                retryable,
                message,
            } => format!("embedding source failed: code={code} retryable={retryable}: {message}"),
            Self::Panicked => "embedding source panicked".to_string(),
        }
    }
}

/// Truncate one diagnostic to [`EMBEDDING_BUILD_REASON_MAX_BYTES`] on a
/// char boundary.
pub fn bounded_build_reason(reason: &str) -> String {
    if reason.len() <= EMBEDDING_BUILD_REASON_MAX_BYTES {
        return reason.to_string();
    }
    let mut end = EMBEDDING_BUILD_REASON_MAX_BYTES;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

/// Persisted outcome of the embedding pass that produced an
/// [`EmbeddingIndex`]: enough to distinguish a complete build from one
/// that could not embed (provider error, affected count, skipped bound).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingBuildRecord {
    /// A source was configured when the pass ran.
    #[serde(default)]
    pub source_configured: bool,
    /// The source failed or answered a hostile shape.
    #[serde(default)]
    pub degraded: bool,
    /// Bounded provider error/diagnostic of the failure.
    #[serde(default)]
    pub reason: String,
    /// Referenced chunks that carry NO vector because of the failure.
    #[serde(default)]
    pub affected: usize,
    /// Vectors embedded in this pass.
    #[serde(default)]
    pub embedded: usize,
    /// Prior vectors carried forward untouched.
    #[serde(default)]
    pub carried: usize,
    /// Referenced chunks the per-build bound deferred to the next build.
    #[serde(default)]
    pub skipped: usize,
    /// Source calls made in this pass (retries live inside the source).
    #[serde(default)]
    pub calls: usize,
}

/// Typed view of [`EmbeddingBuildRecord`] for callers/health: the index
/// generation itself carries whether its vectors are complete, bounded by
/// the per-build cap, degraded by a source failure — or never attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingBuildStatus {
    /// No source configured when the pass ran: lexical/symbol-only by
    /// design, not a degradation.
    Unconfigured,
    /// Every referenced chunk carries a vector for the active identity.
    Complete { embedded: usize, carried: usize },
    /// No source failure: the per-build bound deferred `skipped` chunks to
    /// the next build, so this generation is intentionally (not
    /// accidentally) partial.
    Bounded {
        embedded: usize,
        carried: usize,
        skipped: usize,
    },
    /// The source failed or answered a hostile shape: `affected` referenced
    /// chunks carry no vector in this generation, and `reason` names the
    /// bounded provider error/diagnostic.
    Degraded {
        reason: String,
        affected: usize,
        embedded: usize,
        carried: usize,
        skipped: usize,
    },
}

impl EmbeddingBuildStatus {
    /// True only for an actual embed failure (never for the honest
    /// unconfigured/bounded outcomes).
    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded { .. })
    }
}

impl EmbeddingBuildRecord {
    pub fn status(&self) -> EmbeddingBuildStatus {
        if !self.source_configured {
            return EmbeddingBuildStatus::Unconfigured;
        }
        if self.degraded {
            return EmbeddingBuildStatus::Degraded {
                reason: self.reason.clone(),
                affected: self.affected,
                embedded: self.embedded,
                carried: self.carried,
                skipped: self.skipped,
            };
        }
        if self.skipped > 0 {
            return EmbeddingBuildStatus::Bounded {
                embedded: self.embedded,
                carried: self.carried,
                skipped: self.skipped,
            };
        }
        EmbeddingBuildStatus::Complete {
            embedded: self.embedded,
            carried: self.carried,
        }
    }
}

/// The per-workspace persisted embedding index: the ACTIVE identity plus
/// every record keyed by (content_hash, model_id, model_revision,
/// dimension).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingIndex {
    pub format: u32,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub model_revision: String,
    /// Active dimension; `0` = none observed yet.
    #[serde(default)]
    pub dimension: u32,
    /// Build-time embedding outcome that produced this index. Persisted
    /// with the generation so a build that could not embed is NEVER
    /// mistaken for a complete one by callers/health
    /// (see [`EmbeddingIndex::build_status`]). Raw bounded fields with
    /// serde defaults: an old or hostile generation can only produce a
    /// bounded status, never a load failure.
    #[serde(default)]
    pub build: EmbeddingBuildRecord,
    /// key = [`embedding_key`] of the record.
    #[serde(default)]
    pub records: BTreeMap<String, EmbeddingRecord>,
}

impl Default for EmbeddingIndex {
    fn default() -> Self {
        Self {
            format: EMBEDDING_FORMAT,
            model_id: String::new(),
            model_revision: String::new(),
            dimension: 0,
            build: EmbeddingBuildRecord::default(),
            records: BTreeMap::new(),
        }
    }
}

/// Canonical key of one record: the four tuple members joined with a unit
/// separator no model id can legally contain, so ids cannot collide with
/// each other across fields.
pub fn embedding_key(
    content_hash: &str,
    model_id: &str,
    model_revision: &str,
    dimension: usize,
) -> String {
    format!("{content_hash}\u{1f}{model_id}\u{1f}{model_revision}\u{1f}{dimension}")
}

/// Content hash of one chunk: BLAKE3 over the exact chunk text, hex. The
/// hash is path/generation independent (dedup across files and builds).
pub fn content_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// Deterministic bounded chunking: windows of at most
/// [`EMBEDDING_CHUNK_BYTES`], preferred to end on a newline, at most
/// [`MAX_EMBEDDING_CHUNKS_PER_FILE`] windows. Empty text yields no chunks.
pub fn chunk_text(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < text.len() && out.len() < MAX_EMBEDDING_CHUNKS_PER_FILE {
        let limit = (start + EMBEDDING_CHUNK_BYTES).min(text.len());
        let mut end = limit;
        // The window edge can fall mid-rune: retreat to the nearest
        // boundary, then prefer the last newline inside the window.
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            if let Some(pos) = text[start..end].rfind('\n') {
                let candidate = start + pos + 1;
                if candidate > start {
                    end = candidate;
                }
            }
        }
        if end <= start {
            // Defensive: never loop without progress (cannot happen for
            // valid UTF-8 and a non-empty window).
            end = limit.max(start + 1).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
        }
        out.push(&text[start..end]);
        start = end;
    }
    out
}

impl EmbeddingIndex {
    /// The typed build outcome recorded when this index was produced.
    /// Callers/health read this to distinguish a complete generation from
    /// one whose build could not embed (never a silent lexical-only swap).
    pub fn build_status(&self) -> EmbeddingBuildStatus {
        self.build.status()
    }

    /// Record the typed build outcome (the reason is bounded here so no
    /// construction path can persist an unbounded diagnostic).
    pub fn set_build(&mut self, mut build: EmbeddingBuildRecord) {
        build.reason = bounded_build_reason(&build.reason);
        self.build = build;
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The active vector for `content_hash`, when the active identity has a
    /// known dimension and the record exists.
    pub fn active_vector(&self, content_hash: &str) -> Option<&[f32]> {
        if self.dimension == 0 {
            return None;
        }
        let key = embedding_key(
            content_hash,
            &self.model_id,
            &self.model_revision,
            self.dimension as usize,
        );
        self.records
            .get(&key)
            .map(|record| record.vector.as_slice())
    }

    /// Any record for `content_hash` under the EXACT identity regardless of
    /// dimension (used to carry prior vectors forward before the model's
    /// dimension is re-confirmed).
    pub fn record_for_identity(
        &self,
        content_hash: &str,
        model_id: &str,
        model_revision: &str,
    ) -> Option<&EmbeddingRecord> {
        self.records.values().find(|record| {
            record.content_hash == content_hash
                && record.model_id == model_id
                && record.model_revision == model_revision
        })
    }

    /// Switch the active identity. A model or REVISION change invalidates
    /// every record of the old identity (never a silent reuse across
    /// revisions) and resets the active dimension; a no-op switch keeps
    /// everything.
    pub fn set_identity(&mut self, model_id: &str, model_revision: &str) {
        if self.model_id == model_id && self.model_revision == model_revision {
            return;
        }
        self.model_id = model_id.to_string();
        self.model_revision = model_revision.to_string();
        self.records.retain(|_, record| {
            record.model_id == self.model_id && record.model_revision == self.model_revision
        });
        self.dimension = self
            .records
            .values()
            .map(|record| record.dimension)
            .next()
            .unwrap_or(0);
    }

    /// Drop every record of the ACTIVE identity (dimension drift: the same
    /// identity answered a different dimension, so none of its old vectors
    /// are trustworthy).
    pub fn purge_active_identity(&mut self) {
        let model_id = self.model_id.clone();
        let revision = self.model_revision.clone();
        self.records.retain(|_, record| {
            !(record.model_id == model_id && record.model_revision == revision)
        });
        self.dimension = 0;
    }

    /// Insert one vector under the active identity, validating every bound.
    /// A dimension different from the established active dimension
    /// invalidates the identity's old records first.
    pub fn put(&mut self, content_hash: &str, vector: Vec<f32>) -> Result<(), Error> {
        if self.model_id.is_empty() {
            return Err(Error::new(
                ErrorKind::Malformed,
                "embedding index has no active model identity",
            ));
        }
        if content_hash.is_empty() {
            return Err(Error::new(
                ErrorKind::Malformed,
                "embedding content hash is empty",
            ));
        }
        let dimension = vector.len();
        if dimension == 0 {
            return Err(Error::new(
                ErrorKind::Malformed,
                "embedding vector is empty",
            ));
        }
        if dimension > MAX_EMBEDDING_DIMENSIONS {
            return Err(Error::new(
                ErrorKind::Oversized,
                format!(
                    "embedding vector of {dimension} dimensions exceeds MAX_EMBEDDING_DIMENSIONS ({MAX_EMBEDDING_DIMENSIONS})"
                ),
            ));
        }
        if vector.iter().any(|component| !component.is_finite()) {
            return Err(Error::new(
                ErrorKind::Malformed,
                "embedding vector carries a non-finite component",
            ));
        }
        if self.dimension != 0 && self.dimension as usize != dimension {
            tracing::warn!(
                model = %self.model_id,
                revision = %self.model_revision,
                old = self.dimension,
                new = dimension,
                "embedding dimension changed without a revision change; invalidating old vectors"
            );
            self.purge_active_identity();
        }
        if self.dimension == 0 {
            self.dimension = dimension as u32;
        }
        let key = embedding_key(
            content_hash,
            &self.model_id,
            &self.model_revision,
            dimension,
        );
        self.records.insert(
            key,
            EmbeddingRecord {
                content_hash: content_hash.to_string(),
                model_id: self.model_id.clone(),
                model_revision: self.model_revision.clone(),
                dimension: dimension as u32,
                vector,
            },
        );
        self.prune();
        Ok(())
    }

    /// Deterministic disk bound: drop lexicographically-first keys until
    /// both the record count and the summed component budget are met.
    fn prune(&mut self) {
        while self.records.len() > MAX_EMBEDDING_RECORDS {
            let Some(key) = self.records.keys().next().cloned() else {
                break;
            };
            self.records.remove(&key);
        }
        let mut components: usize = self.records.values().map(|r| r.vector.len()).sum();
        while components > MAX_EMBEDDING_COMPONENTS {
            let Some(key) = self.records.keys().next().cloned() else {
                break;
            };
            if let Some(removed) = self.records.remove(&key) {
                components = components.saturating_sub(removed.vector.len());
            }
        }
    }

    /// Validate a loaded (possibly hostile) payload: a wrong format tag
    /// loads as empty; invalid records are dropped loudly; the bounds are
    /// re-applied. The lexical index is unaffected either way.
    pub fn sanitize(mut self) -> Self {
        if self.format != EMBEDDING_FORMAT {
            tracing::warn!(
                format = self.format,
                "persisted embedding index format unsupported; ignoring its vectors"
            );
            return Self::default();
        }
        let before = self.records.len();
        self.records.retain(|key, record| {
            let dimension = record.vector.len();
            let valid = record.dimension > 0
                && record.dimension as usize == dimension
                && dimension <= MAX_EMBEDDING_DIMENSIONS
                && !record.content_hash.is_empty()
                && !record.model_id.is_empty()
                && record.vector.iter().all(|component| component.is_finite())
                && key
                    == &embedding_key(
                        &record.content_hash,
                        &record.model_id,
                        &record.model_revision,
                        record.dimension as usize,
                    );
            if !valid {
                tracing::warn!(key = %key, "dropping invalid persisted embedding record");
            }
            valid
        });
        if self.records.len() != before {
            tracing::warn!(
                dropped = before - self.records.len(),
                "persisted embedding index carried invalid records"
            );
        }
        // The active dimension must name a surviving record (or be zeroed).
        let active = self.dimension != 0
            && self.records.values().any(|record| {
                record.model_id == self.model_id
                    && record.model_revision == self.model_revision
                    && record.dimension == self.dimension
            });
        if !active {
            self.dimension = self
                .records
                .values()
                .find(|record| {
                    record.model_id == self.model_id && record.model_revision == self.model_revision
                })
                .map(|record| record.dimension)
                .unwrap_or(0);
        }
        // The persisted build record is informational but untrusted: bound
        // its reason so a hostile generation cannot smuggle an unbounded
        // string into memory or logs. A record of an unknown shape still
        // maps to a typed status — never a load failure.
        if self.build.reason.len() > EMBEDDING_BUILD_REASON_MAX_BYTES {
            self.build.reason = bounded_build_reason(&self.build.reason);
        }
        self.prune();
        self
    }
}

/// Outcome of one [`apply_embeddings`] pass (observability/tests). The
/// typed build outcome is also persisted onto the index itself
/// ([`EmbeddingIndex::build_status`]) so a degraded build survives the
/// generation swap.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmbeddingStats {
    /// Prior vectors carried forward unchanged (never re-embedded).
    pub carried: usize,
    /// Vectors embedded in this build.
    pub embedded: usize,
    /// Embedding source calls made (retries live inside the source).
    pub calls: usize,
    /// Chunks the per-build bound left for the next build.
    pub skipped: usize,
    /// The source failed or answered a hostile shape; the build published
    /// without (some) vectors.
    pub degraded: bool,
    /// Referenced chunks left WITHOUT a vector by the failure (`0` when the
    /// pass completed; the bound-deferred remainder is `skipped`).
    pub failed: usize,
    /// Bounded provider error/diagnostic when `degraded`.
    pub degraded_reason: Option<String>,
}

impl EmbeddingStats {
    fn mark_degraded(&mut self, affected: usize, reason: impl Into<String>) {
        self.degraded = true;
        self.failed = self.failed.saturating_add(affected);
        self.degraded_reason = Some(bounded_build_reason(&reason.into()));
    }

    fn build_record(&self, source_configured: bool) -> EmbeddingBuildRecord {
        EmbeddingBuildRecord {
            source_configured,
            degraded: self.degraded,
            reason: self.degraded_reason.clone().unwrap_or_default(),
            affected: self.failed,
            embedded: self.embedded,
            carried: self.carried,
            skipped: self.skipped,
            calls: self.calls,
        }
    }
}

/// Reuse prior vectors for every referenced chunk whose content hash
/// matches, embed the rest through `source` (when configured), and install
/// the result into `index`. Never fails the build: an embedding outage
/// degrades to lexical/symbol-only retrieval. The degradation is NEVER
/// silent: the typed outcome is recorded on the persisted index
/// ([`EmbeddingIndex::build_status`]) and logged loudly with the bounded
/// provider error and the affected chunk count.
///
/// `chunks` is the scanned chunk text per path (already bounded); `prior`
/// is the previously published embedding index (in memory or reloaded from
/// the newest generation file). Identical chunk text is embedded once.
pub fn apply_embeddings(
    index: &mut WorkspaceIndex,
    workspace: WorkspaceId,
    prior: Option<&EmbeddingIndex>,
    model: &EmbeddingModel,
    chunks: &[(String, Vec<String>)],
    source: Option<&dyn EmbeddingSource>,
) -> EmbeddingStats {
    let (stats, mut fresh) = apply_embeddings_pass(prior, model, chunks, source);
    let record = stats.build_record(source.is_some());
    if let EmbeddingBuildStatus::Degraded {
        reason,
        affected,
        skipped,
        ..
    } = record.status()
    {
        tracing::error!(
            workspace = workspace.raw(),
            affected,
            skipped,
            embedded = stats.embedded,
            carried = stats.carried,
            calls = stats.calls,
            "index embedding build DEGRADED: {} referenced chunks carry no vector ({reason}); the generation is published for lexical/symbol retrieval",
            affected
        );
    }
    fresh.set_build(record);
    index.replace_embeddings(workspace, fresh);
    stats
}

/// The bounded work of one build pass: carries matching prior vectors into
/// a FRESH identity-scoped index (records of a different model/revision can
/// never survive, and records of unreferenced chunks cannot linger), embeds
/// the missing chunks and records the typed outcome. Returns the stats plus
/// the index the caller installs — degraded or not, the build publishes.
fn apply_embeddings_pass(
    prior: Option<&EmbeddingIndex>,
    model: &EmbeddingModel,
    chunks: &[(String, Vec<String>)],
    source: Option<&dyn EmbeddingSource>,
) -> (EmbeddingStats, EmbeddingIndex) {
    let mut stats = EmbeddingStats::default();
    let mut fresh = EmbeddingIndex::default();
    fresh.set_identity(&model.model_id, &model.revision);

    // Referenced chunks: hash -> text (first occurrence wins, so duplicates
    // across files share one vector).
    let mut referenced: BTreeMap<String, String> = BTreeMap::new();
    for (_, texts) in chunks {
        for text in texts {
            let hash = content_hash(text);
            referenced.entry(hash).or_insert_with(|| text.clone());
        }
    }

    // Carry matching prior vectors forward (same model/revision, whichever
    // dimension they had); a revision switch already purged them in
    // `set_identity`.
    if let Some(prior) = prior {
        for hash in referenced.keys() {
            if let Some(record) = prior.record_for_identity(hash, &model.model_id, &model.revision)
            {
                if record.vector.is_empty()
                    || record.vector.len() > MAX_EMBEDDING_DIMENSIONS
                    || record.vector.iter().any(|component| !component.is_finite())
                {
                    continue;
                }
                if fresh.put(hash, record.vector.clone()).is_ok() {
                    stats.carried += 1;
                }
            }
        }
    }

    let mut missing: Vec<(String, String)> = referenced
        .iter()
        .filter(|(hash, _)| fresh.active_vector(hash).is_none())
        .map(|(hash, text)| (hash.clone(), text.clone()))
        .collect();
    if missing.len() > MAX_EMBED_CHUNKS_PER_BUILD {
        stats.skipped += missing.len() - MAX_EMBED_CHUNKS_PER_BUILD;
        missing.truncate(MAX_EMBED_CHUNKS_PER_BUILD);
    }
    let Some(source) = source else {
        return (stats, fresh);
    };
    if missing.is_empty() {
        return (stats, fresh);
    }

    let carried_dimension = fresh.dimension;
    match embed_batch(source, &missing, &mut stats) {
        Ok(vectors) => {
            let dimension = match vectors.first().map(|v| v.len()) {
                Some(d) if d > 0 && d <= MAX_EMBEDDING_DIMENSIONS => d,
                _ => {
                    stats.mark_degraded(
                        missing.len(),
                        "embedding source answered a zero/invalid dimension",
                    );
                    return (stats, fresh);
                }
            };
            let uniform = vectors
                .iter()
                .all(|v| v.len() == dimension && v.iter().all(|c| c.is_finite()));
            if !uniform {
                stats.mark_degraded(
                    missing.len(),
                    "embedding source answered a ragged/non-finite batch",
                );
                return (stats, fresh);
            }
            if carried_dimension != 0 && carried_dimension as usize != dimension {
                // Same identity answered a NEW dimension: every carried
                // vector is stale. Invalidate and re-embed the whole
                // referenced set in one bounded follow-up call.
                tracing::warn!(
                    old = carried_dimension,
                    new = dimension,
                    "embedding dimension changed; re-embedding the corpus"
                );
                fresh.purge_active_identity();
                stats.carried = 0;
                let mut all: Vec<(String, String)> = referenced
                    .iter()
                    .map(|(hash, text)| (hash.clone(), text.clone()))
                    .collect();
                if all.len() > MAX_EMBED_CHUNKS_PER_BUILD {
                    stats.skipped += all.len() - MAX_EMBED_CHUNKS_PER_BUILD;
                    all.truncate(MAX_EMBED_CHUNKS_PER_BUILD);
                }
                let vectors = match embed_batch(source, &all, &mut stats) {
                    Ok(vectors) => vectors,
                    Err(failure) => {
                        stats.mark_degraded(all.len(), failure.describe());
                        return (stats, fresh);
                    }
                };
                let dimension = vectors.first().map(|v| v.len()).unwrap_or(0);
                let uniform = dimension > 0
                    && dimension <= MAX_EMBEDDING_DIMENSIONS
                    && vectors.iter().all(|v| {
                        v.len() == dimension && v.iter().all(|component| component.is_finite())
                    });
                if !uniform {
                    stats.mark_degraded(
                        all.len(),
                        "embedding source answered an invalid re-embed batch",
                    );
                    return (stats, fresh);
                }
                stats.embedded += store_vectors(&mut fresh, &all, vectors);
            } else {
                stats.embedded += store_vectors(&mut fresh, &missing, vectors);
            }
        }
        Err(failure) => stats.mark_degraded(missing.len(), failure.describe()),
    }
    (stats, fresh)
}

/// One bounded source call: `Err` (degraded) on any error/refusal, never a
/// panic — the source is untrusted input to the build.
fn embed_batch(
    source: &dyn EmbeddingSource,
    batch: &[(String, String)],
    stats: &mut EmbeddingStats,
) -> Result<Vec<Vec<f32>>, EmbedFailure> {
    let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
    stats.calls += 1;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.embed(&texts))) {
        Ok(Ok(vectors)) => Ok(vectors),
        Ok(Err(error)) => {
            let failure = EmbedFailure::from_error(error);
            tracing::warn!(
                "embedding source failed (build degrades to lexical/symbol): {}",
                failure.describe()
            );
            Err(failure)
        }
        Err(_) => {
            tracing::warn!("embedding source panicked (build degrades to lexical/symbol)");
            Err(EmbedFailure::Panicked)
        }
    }
}

/// Store validated vectors into the fresh index, skipping none/failing ones
/// individually; the count actually persisted is returned.
fn store_vectors(
    index: &mut EmbeddingIndex,
    batch: &[(String, String)],
    vectors: Vec<Vec<f32>>,
) -> usize {
    let mut stored = 0usize;
    for ((hash, _), vector) in batch.iter().zip(vectors) {
        if index.put(hash, vector).is_ok() {
            stored += 1;
        }
    }
    stored
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn hash_of(text: &str) -> String {
        content_hash(text)
    }

    #[test]
    fn chunking_is_deterministic_bounded_and_unicode_safe() {
        assert!(chunk_text("").is_empty());
        let short = "fn alpha() {}";
        assert_eq!(chunk_text(short), vec![short]);
        // A file over the window splits at newlines, each chunk ≤ the cap.
        let line = "let value = compute_something();\n";
        let long: String = line.repeat(400);
        let chunks = chunk_text(&long);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= EMBEDDING_CHUNK_BYTES));
        assert_eq!(chunks.concat(), long, "chunking is lossless");
        // Multi-byte runes at a window edge never split mid-rune.
        let uni = "é".repeat(4_000);
        let chunks = chunk_text(&uni);
        assert!(chunks.iter().all(|c| c.chars().all(|ch| ch == 'é')));
        assert_eq!(chunks.concat(), uni);
        // Hostile: a single file of many windows is truncated at the cap.
        let huge = "x".repeat(EMBEDDING_CHUNK_BYTES * (MAX_EMBEDDING_CHUNKS_PER_FILE + 10));
        assert_eq!(chunk_text(&huge).len(), MAX_EMBEDDING_CHUNKS_PER_FILE);
    }

    #[test]
    fn records_are_keyed_by_the_full_tuple_and_revisions_invalidate() {
        let mut idx = EmbeddingIndex::default();
        idx.set_identity("m", "r1");
        idx.put(&hash_of("a"), vec![1.0, 0.0]).unwrap();
        assert_eq!(idx.dimension, 2);
        assert_eq!(idx.active_vector(&hash_of("a")), Some(&[1.0, 0.0][..]));
        // A different revision is a different key AND purges the old one.
        idx.set_identity("m", "r2");
        assert!(idx.is_empty(), "revision change invalidates old vectors");
        assert_eq!(idx.dimension, 0);
        // A different model likewise.
        idx.put(&hash_of("a"), vec![1.0, 2.0, 3.0]).unwrap();
        idx.set_identity("other", "r2");
        assert!(idx.is_empty());
        // Hostile shapes are typed refusals, never persisted.
        idx.set_identity("m", "r1");
        assert!(idx.put("h", vec![]).is_err());
        assert!(idx.put("h", vec![f32::NAN]).is_err());
        assert!(idx
            .put("h", vec![0.0; MAX_EMBEDDING_DIMENSIONS + 1])
            .is_err());
        assert!(idx.put("", vec![1.0]).is_err());
        assert!(idx.is_empty());
    }

    #[test]
    fn dimension_drift_purges_the_identity_and_re_record_is_the_new_dimension() {
        let mut idx = EmbeddingIndex::default();
        idx.set_identity("m", "r1");
        idx.put(&hash_of("a"), vec![1.0, 0.0]).unwrap();
        idx.put(&hash_of("b"), vec![0.0, 1.0]).unwrap();
        assert_eq!(idx.len(), 2);
        idx.put(&hash_of("c"), vec![1.0, 0.0, 0.0]).unwrap();
        assert_eq!(idx.dimension, 3);
        assert_eq!(idx.len(), 1, "old dimension records were invalidated");
        assert!(idx.active_vector(&hash_of("a")).is_none());
        assert_eq!(idx.active_vector(&hash_of("c")), Some(&[1.0, 0.0, 0.0][..]));
    }

    #[test]
    fn pruning_is_bounded_and_deterministic() {
        let mut idx = EmbeddingIndex::default();
        idx.set_identity("m", "r");
        let over = MAX_EMBEDDING_RECORDS + 10;
        for i in 0..over {
            idx.put(&format!("hash-{i:08}"), vec![1.0; 2]).unwrap();
        }
        assert_eq!(idx.len(), MAX_EMBEDDING_RECORDS);
        // Deterministic: the first (lexicographic) keys were dropped.
        assert!(idx.active_vector("hash-00000000").is_none());
        assert!(idx
            .active_vector(&format!("hash-{:08}", over - 1))
            .is_some());
    }

    #[test]
    fn sanitize_drops_hostile_persisted_shapes() {
        let mut evil = EmbeddingIndex::default();
        evil.records.insert(
            "not-a-key".into(),
            EmbeddingRecord {
                content_hash: "h".into(),
                model_id: "m".into(),
                model_revision: "r".into(),
                dimension: 2,
                vector: vec![1.0, 2.0],
            },
        );
        evil.records.insert(
            embedding_key("h2", "m", "r", 2),
            EmbeddingRecord {
                content_hash: "h2".into(),
                model_id: "m".into(),
                model_revision: "r".into(),
                dimension: 2,
                vector: vec![f32::INFINITY, 0.0],
            },
        );
        evil.records.insert(
            embedding_key("h3", "m", "r", 2),
            EmbeddingRecord {
                content_hash: "h3".into(),
                model_id: "m".into(),
                model_revision: "r".into(),
                dimension: 9,
                vector: vec![0.0, 0.0],
            },
        );
        evil.model_id = "m".into();
        evil.model_revision = "r".into();
        evil.dimension = 2;
        let clean = evil.sanitize();
        assert!(clean.is_empty(), "{clean:?}");
        assert_eq!(clean.dimension, 0);
        // A wrong format tag degrades to no embeddings, loudly.
        let future = EmbeddingIndex {
            format: 999,
            ..EmbeddingIndex::default()
        };
        assert!(future.sanitize().is_empty());
    }

    #[test]
    fn workspace_embedding_index_requires_the_active_identity() {
        let ws = WorkspaceId::new(1);
        let mut index = WorkspaceIndex::new();
        // A partial/hostile store: records exist, but the active identity
        // names nothing usable — search must NOT take the persisted path.
        let mut partial = EmbeddingIndex::default();
        partial.records.insert(
            embedding_key("h", "old", "r0", 2),
            EmbeddingRecord {
                content_hash: "h".into(),
                model_id: "old".into(),
                model_revision: "r0".into(),
                dimension: 2,
                vector: vec![1.0, 0.0],
            },
        );
        index.replace_embeddings(ws, partial);
        assert!(!index.has_embedding_index(ws));
        // Activating the matching identity makes the store usable again.
        index.set_embedding_model(ws, &EmbeddingModel::new("old", "r0"));
        assert!(index.has_embedding_index(ws));
        assert_eq!(index.embedding_dimension(ws), 2);
    }

    // ------------------------------------------------------------ build pass

    /// Spy source: counts calls, records every text batch, answers from a
    /// keyword axis (so assertions know which chunks were requested).
    struct SpySource {
        calls: std::sync::atomic::AtomicUsize,
        texts: std::sync::Mutex<Vec<String>>,
        dimension: usize,
        fail: std::sync::atomic::AtomicBool,
    }

    impl SpySource {
        fn new(dimension: usize) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                texts: std::sync::Mutex::new(Vec::new()),
                dimension,
                fail: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn texts(&self) -> Vec<String> {
            self.texts.lock().unwrap().clone()
        }
    }

    impl EmbeddingSource for SpySource {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::new(ErrorKind::Network, "embedding backend down"));
            }
            self.texts.lock().unwrap().extend(texts.iter().cloned());
            Ok(texts
                .iter()
                .map(|text| {
                    (0..self.dimension)
                        .map(|axis| {
                            if text.bytes().fold(0u64, |acc, b| acc.wrapping_add(b as u64))
                                % (axis as u64 + 3)
                                == 0
                            {
                                1.0
                            } else {
                                0.5
                            }
                        })
                        .collect()
                })
                .collect())
        }
    }

    fn index_one(ws: WorkspaceId, rel: &str, text: &str) -> WorkspaceIndex {
        let mut index = WorkspaceIndex::new();
        index
            .index_file(ws, Path::new(rel), text.as_bytes(), 0)
            .unwrap();
        index
    }

    #[test]
    fn unchanged_chunks_are_carried_and_changed_chunks_reembed() {
        let ws = WorkspaceId::new(1);
        let alpha = "pub fn alpha() -> i64 { 1 }\n";
        let beta = "pub fn beta() -> i64 { 2 }\n";
        let mut index = index_one(ws, "src/a.rs", alpha);
        index
            .index_file(ws, Path::new("src/b.rs"), beta.as_bytes(), 0)
            .unwrap();
        let model = EmbeddingModel::new("m", "r1");
        let chunks: Vec<(String, Vec<String>)> = vec![
            ("src/a.rs".into(), vec![alpha.into()]),
            ("src/b.rs".into(), vec![beta.into()]),
        ];
        let source = SpySource::new(3);
        let stats = apply_embeddings(&mut index, ws, None, &model, &chunks, Some(&source));
        assert_eq!(stats.carried, 0);
        assert_eq!(stats.embedded, 2);
        assert_eq!(stats.calls, 1, "one bounded batch per build");
        assert!(index.has_embedding_index(ws));
        let first_hash = hash_of(alpha);

        // Re-index: only the changed chunk is missing; the unchanged one is
        // carried from the prior index WITHOUT a source call.
        let changed = "pub fn alpha() -> i64 { 99 }\n";
        let chunks2: Vec<(String, Vec<String>)> = vec![
            ("src/a.rs".into(), vec![changed.into()]),
            ("src/b.rs".into(), vec![beta.into()]),
        ];
        let prior = index.embedding_index(ws).cloned().unwrap();
        let stats = apply_embeddings(
            &mut index,
            ws,
            Some(&prior),
            &model,
            &chunks2,
            Some(&source),
        );
        assert_eq!(stats.carried, 1, "the unchanged chunk is reused");
        assert_eq!(stats.embedded, 1, "only the changed chunk re-embeds");
        assert_eq!(source.calls(), 2);
        assert_eq!(
            source.texts(),
            vec![alpha.to_string(), beta.to_string(), changed.to_string()],
            "the second batch carries ONLY the changed chunk"
        );
        // The unchanged vector is byte-identical across the rebuild.
        assert!(index.embedding_vector(ws, &first_hash).is_none());
        assert!(index.embedding_vector(ws, &hash_of(beta)).is_some());
    }

    #[test]
    fn source_failure_degrades_without_failing_the_build() {
        let ws = WorkspaceId::new(1);
        let text = "pub fn gamma() {}\n";
        let mut index = index_one(ws, "src/c.rs", text);
        let source = SpySource::new(2);
        source.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let stats = apply_embeddings(
            &mut index,
            ws,
            None,
            &EmbeddingModel::new("m", "r"),
            &[("src/c.rs".into(), vec![text.into()])],
            Some(&source),
        );
        assert!(stats.degraded);
        assert_eq!(stats.embedded, 0);
        assert_eq!(stats.failed, 1, "the affected chunk is counted");
        assert!(stats.degraded_reason.is_some());
        assert!(!index.has_embedding_index(ws));
        // The typed outcome is persisted on the generation: not a clean
        // success.
        assert!(index
            .embedding_index(ws)
            .expect("the build records its outcome")
            .build_status()
            .is_degraded());
        // The lexical index is untouched by the embedding outage.
        assert_eq!(index.files_for_token(ws, "gamma", 10).len(), 1);
    }

    /// A source that panics on every call: hostile input to the build.
    struct PanickingSource;

    impl EmbeddingSource for PanickingSource {
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
            panic!("embedding backend exploded");
        }
    }

    #[test]
    fn source_failure_is_a_typed_persisted_degraded_status_with_affected_count() {
        let ws = WorkspaceId::new(1);
        let alpha = "pub fn alpha() {}\n";
        let beta = "pub fn beta() {}\n";
        let mut index = index_one(ws, "src/a.rs", alpha);
        index
            .index_file(ws, Path::new("src/b.rs"), beta.as_bytes(), 0)
            .unwrap();
        let source = SpySource::new(2);
        source.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let chunks = vec![
            ("src/a.rs".into(), vec![alpha.into()]),
            ("src/b.rs".into(), vec![beta.into()]),
        ];
        let model = EmbeddingModel::new("m", "r1");
        let stats = apply_embeddings(&mut index, ws, None, &model, &chunks, Some(&source));
        assert!(stats.degraded);
        assert_eq!(stats.failed, 2, "every referenced chunk was affected");
        let described = stats.degraded_reason.as_deref().unwrap();
        assert!(described.contains("Network"), "{described}");
        assert!(described.contains("embedding backend down"), "{described}");
        // The generation itself carries the typed status: callers/health can
        // tell a degraded build from a complete one without reading logs.
        match index
            .embedding_index(ws)
            .expect("the build records its outcome")
            .build_status()
        {
            EmbeddingBuildStatus::Degraded {
                reason,
                affected,
                embedded,
                carried,
                skipped,
            } => {
                assert_eq!(affected, 2);
                assert_eq!(embedded, 0);
                assert_eq!(carried, 0);
                assert_eq!(skipped, 0);
                assert!(reason.contains("embedding backend down"), "{reason}");
            }
            other => panic!("a failed embed build must not look complete: {other:?}"),
        }
    }

    #[test]
    fn recovery_rebuild_clears_the_degraded_status() {
        let ws = WorkspaceId::new(1);
        let text = "pub fn gamma() {}\n";
        let mut index = index_one(ws, "src/c.rs", text);
        let source = SpySource::new(2);
        source.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let chunks = vec![("src/c.rs".into(), vec![text.into()])];
        let model = EmbeddingModel::new("m", "r1");
        let failed = apply_embeddings(&mut index, ws, None, &model, &chunks, Some(&source));
        assert!(failed.degraded);
        assert!(index
            .embedding_index(ws)
            .unwrap()
            .build_status()
            .is_degraded());
        // Rebuild with a healthy provider: the same referenced corpus now
        // embeds and the generation is Complete — the degrade is not sticky.
        source
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let prior = index.embedding_index(ws).cloned().unwrap();
        let healed = apply_embeddings(&mut index, ws, Some(&prior), &model, &chunks, Some(&source));
        assert!(!healed.degraded);
        assert_eq!(healed.embedded, 1);
        assert_eq!(healed.failed, 0);
        assert!(healed.degraded_reason.is_none());
        assert_eq!(
            index.embedding_index(ws).unwrap().build_status(),
            EmbeddingBuildStatus::Complete {
                embedded: 1,
                carried: 0
            },
            "a healthy rebuild must clear the degraded status"
        );
        assert!(index.has_embedding_index(ws));
    }

    #[test]
    fn panicking_source_is_degraded_not_propagated() {
        let ws = WorkspaceId::new(1);
        let text = "pub fn delta() {}\n";
        let mut index = index_one(ws, "src/d.rs", text);
        let chunks = vec![("src/d.rs".into(), vec![text.into()])];
        let stats = apply_embeddings(
            &mut index,
            ws,
            None,
            &EmbeddingModel::new("m", "r1"),
            &chunks,
            Some(&PanickingSource),
        );
        assert!(stats.degraded);
        assert_eq!(stats.failed, 1);
        assert_eq!(
            stats.degraded_reason.as_deref(),
            Some("embedding source panicked")
        );
        match index.embedding_index(ws).unwrap().build_status() {
            EmbeddingBuildStatus::Degraded {
                reason, affected, ..
            } => {
                assert_eq!(affected, 1);
                assert_eq!(reason, "embedding source panicked");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn build_status_survives_serialization_and_hostile_reasons_are_bounded() {
        let mut index = EmbeddingIndex::default();
        index.set_identity("m", "r");
        index.set_build(EmbeddingBuildRecord {
            source_configured: true,
            degraded: true,
            reason: "x".repeat(EMBEDDING_BUILD_REASON_MAX_BYTES * 4),
            affected: 3,
            embedded: 1,
            carried: 2,
            skipped: 4,
            calls: 1,
        });
        let json = serde_json::to_string(&index).unwrap();
        let loaded: EmbeddingIndex = serde_json::from_str(&json).unwrap();
        let loaded = loaded.sanitize();
        match loaded.build_status() {
            EmbeddingBuildStatus::Degraded {
                reason,
                affected,
                embedded,
                carried,
                skipped,
            } => {
                assert_eq!(affected, 3);
                assert_eq!(embedded, 1);
                assert_eq!(carried, 2);
                assert_eq!(skipped, 4);
                assert!(
                    reason.len() <= EMBEDDING_BUILD_REASON_MAX_BYTES,
                    "a hostile reason must be bounded: {}",
                    reason.len()
                );
            }
            other => panic!("{other:?}"),
        }
        // An old generation without the build field loads as Unconfigured,
        // never as a load failure.
        let old: EmbeddingIndex = serde_json::from_str(
            r#"{"format":1,"model_id":"m","model_revision":"r","dimension":0,"records":{}}"#,
        )
        .unwrap();
        assert_eq!(old.build_status(), EmbeddingBuildStatus::Unconfigured);
        // The bound-deferred outcome is typed as intentional, not degraded.
        let bounded = EmbeddingBuildRecord {
            source_configured: true,
            skipped: 5,
            ..EmbeddingBuildRecord::default()
        };
        assert_eq!(
            bounded.status(),
            EmbeddingBuildStatus::Bounded {
                embedded: 0,
                carried: 0,
                skipped: 5
            }
        );
        assert!(!bounded.status().is_degraded());
        assert!(!EmbeddingBuildRecord::default().status().is_degraded());
    }

    #[test]
    fn dimension_drift_reembeds_the_whole_referenced_set() {
        let ws = WorkspaceId::new(1);
        let alpha = "pub fn alpha() {}\n";
        let beta = "pub fn beta() {}\n";
        let mut index = index_one(ws, "src/a.rs", alpha);
        index
            .index_file(ws, Path::new("src/b.rs"), beta.as_bytes(), 0)
            .unwrap();
        let model = EmbeddingModel::new("m", "r1");
        let chunks = vec![
            ("src/a.rs".into(), vec![alpha.into()]),
            ("src/b.rs".into(), vec![beta.into()]),
        ];
        let source = SpySource::new(2);
        apply_embeddings(&mut index, ws, None, &model, &chunks, Some(&source));
        let prior = index.embedding_index(ws).cloned().unwrap();
        assert_eq!(prior.dimension, 2);
        // Same identity, new dimension. A changed chunk is needed to observe
        // it: the source answer (dim 3) conflicts with the carried dim 2, so
        // the carried vector is invalidated and the WHOLE referenced set is
        // re-embedded in the follow-up call.
        let changed = "pub fn alpha() -> Option<()> { None }\n";
        let chunks_drift = vec![
            ("src/a.rs".into(), vec![changed.into()]),
            ("src/b.rs".into(), vec![beta.into()]),
        ];
        let source3 = SpySource::new(3);
        let stats = apply_embeddings(
            &mut index,
            ws,
            Some(&prior),
            &model,
            &chunks_drift,
            Some(&source3),
        );
        assert_eq!(
            stats.carried, 0,
            "the stale-dimension carry was invalidated"
        );
        assert_eq!(stats.embedded, 2, "the whole referenced set re-embedded");
        assert_eq!(source3.calls(), 2, "missing batch + full re-embed");
        assert!(source3.texts().contains(&changed.to_string()));
        assert!(source3.texts().contains(&beta.to_string()));
        assert_eq!(index.embedding_dimension(ws), 3);
        assert!(index.embedding_vector(ws, &hash_of(changed)).is_some());
        assert!(index.embedding_vector(ws, &hash_of(beta)).is_some());
    }
}
