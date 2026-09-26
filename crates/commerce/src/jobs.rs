//! Deterministic bulk jobs (`docs/acquire.md` §12).
//!
//! A job is identified by its digest,
//! `BLAKE3(normalized request + enabled sources + profile identity +
//! freshness policy)`. Identical requests that are still active attach to
//! the same job; once a job is terminal, a new submission creates a new
//! job. Jobs do deterministic work only (no model calls, ever), are
//! restart-safe — every item outcome is committed durably before the next
//! item runs, so a crash mid-job resumes without re-running a completed
//! item — and hand large results to a CAS artifact through the injected
//! [`ArtifactStore`], returning a compact context result
//! ([`crate::result::CompactResult`], 4–12 KiB target, 64 KiB hard max).

use faktor_core::{SessionId, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::bom::{Bom, BomItem};
use crate::connector::{AcquireCtx, ProfileIdentity};
use crate::error::SourceError;
use crate::quantity::{NonZeroQuantity, MAX_ORDER_QUANTITY};
use crate::query::{
    normalize_query, DetailLevel, FreshnessMode, ProductRef, SourceSet, MAX_LIMIT, MAX_QUERY_BYTES,
};
use crate::result::{
    ArtifactStore, CompactResult, CompactStatus, ImportantEntry, ResultCounts,
    MAX_IMPORTANT_ENTRIES,
};
use crate::store::{CommerceStore, JobItemRow, JobRow, NormalizedPayload};
use crate::text::{AccountScope, Text};

/// Domain separator for job digests.
const JOB_DIGEST_DOMAIN: &[u8] = b"faktor-commerce.job-digest/v1\0";
/// Domain separator for job ids.
const JOB_ID_DOMAIN: &[u8] = b"faktor-commerce.job-id/v1\0";
/// Domain separator for job owner keys.
const JOB_OWNER_DOMAIN: &[u8] = b"faktor-commerce.job-owner/v1\0";
/// Domain separator for synthetic single-item job item keys.
const JOB_ITEM_DOMAIN: &[u8] = b"faktor-commerce.job-item/v1\0";
/// Domain separator for per-line BOM item keys.
const BOM_LINE_KEY_DOMAIN: &[u8] = b"faktor-commerce.bom-line-item/v1\0";

/// The per-line retry bound of a job. A transient, rate-limited or
/// quota-exhausted line is re-queued at most this many times within one job;
/// on the final failure the line is settled [`JobItemState::Failed`] with the
/// last typed error, so a permanently failing line can never consume work
/// forever and the rest of the job can still terminate.
pub const MAX_JOB_ITEM_ATTEMPTS: u64 = 5;

/// A job identifier (`job_` + 32 hex characters).
pub type JobId = String;

/// The owner of a commerce job, DERIVED BY THE RUNTIME from the
/// authenticated caller (the tool-run context's workspace/session plus the
/// session's account scope). It is never built from model-supplied tool
/// arguments: a tool factory cannot widen its own visibility.
///
/// The owner key binds three dimensions, so two sessions or two accounts in
/// the same workspace never share a job: the job digest includes the derived
/// key (active-job dedup can never cross-attach owners) and the durable job
/// row persists it (a status read by a different owner is a typed NotFound,
/// byte-identical to a missing job).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommercePrincipal {
    /// The workspace the caller is rooted in.
    pub workspace_id: WorkspaceId,
    /// The session that issued the call.
    pub session_id: SessionId,
    /// The account scope the caller acts under, when any.
    pub account_scope: Option<AccountScope>,
}

impl CommercePrincipal {
    /// Construct a principal from runtime-derived identity values.
    pub fn new(
        workspace_id: WorkspaceId,
        session_id: SessionId,
        account_scope: Option<AccountScope>,
    ) -> Self {
        Self {
            workspace_id,
            session_id,
            account_scope,
        }
    }

    /// The canonical owner key persisted on every job row:
    /// `BLAKE3(domain || workspace_id || session_id || account_scope)` with
    /// length-prefixed, presence-tagged fields. 64 hex characters.
    pub fn owner_key(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(JOB_OWNER_DOMAIN);
        hasher.update(&self.workspace_id.raw().to_le_bytes());
        hasher.update(&self.session_id.raw().to_le_bytes());
        match &self.account_scope {
            None => {
                hasher.update(&[0]);
            }
            Some(scope) => {
                hasher.update(&[1]);
                let value = scope.as_str();
                hasher.update(&(value.len() as u64).to_le_bytes());
                hasher.update(value.as_bytes());
            }
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// The deterministic state machine of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Accepted, not started.
    Queued,
    /// In progress.
    Running,
    /// Finished with a compact result.
    Completed,
    /// Finished with a typed failure.
    Failed,
    /// Cancelled before completion.
    Cancelled,
}

impl JobState {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse the wire label.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// True for terminal states.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// The legal transition table. Queued -> Running -> terminal; Running is
    /// idempotent (a resumed job re-enters it); anything out of a terminal
    /// state is refused.
    pub const fn can_transition_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Queued, Self::Queued)
                | (Self::Queued, Self::Running)
                | (Self::Queued, Self::Cancelled)
                | (Self::Running, Self::Running)
                | (Self::Running, Self::Completed)
                | (Self::Running, Self::Failed)
                | (Self::Running, Self::Cancelled)
                | (Self::Completed, Self::Completed)
                | (Self::Failed, Self::Failed)
                | (Self::Cancelled, Self::Cancelled)
        )
    }
}

/// The state of one job line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobItemState {
    /// Not acquired yet.
    Pending,
    /// An exact/strong match.
    Matched,
    /// An ambiguous match (never auto-selected).
    Ambiguous,
    /// No match.
    Unmatched,
    /// Settled with a typed failure that will not be retried.
    Failed,
}

impl JobItemState {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Matched => "matched",
            Self::Ambiguous => "ambiguous",
            Self::Unmatched => "unmatched",
            Self::Failed => "failed",
        }
    }

    /// Parse the wire label.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "matched" => Some(Self::Matched),
            "ambiguous" => Some(Self::Ambiguous),
            "unmatched" => Some(Self::Unmatched),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// True when the line must never be re-acquired.
    pub const fn is_settled(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// The work one job performs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "work", rename_all = "snake_case")]
pub enum JobWork {
    /// Discovery.
    Search {
        /// The free-text query.
        query: Text<{ MAX_QUERY_BYTES }>,
        /// The result limit.
        limit: u8,
    },
    /// One product inspection.
    Product {
        /// The reference.
        reference: ProductRef,
    },
    /// One quote.
    Quote {
        /// The reference.
        reference: ProductRef,
        /// The quantity.
        quantity: NonZeroQuantity,
        /// The requested packaging.
        packaging: Option<crate::packaging::PackagingType>,
        /// The requested variant.
        variant: Option<crate::text::VariantId>,
    },
    /// A bounded BOM.
    Bom {
        /// The lines.
        bom: Bom,
    },
}

impl JobWork {
    /// The stable kind label (`search`/`product`/`quote`/`bom`).
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Search { .. } => "search",
            Self::Product { .. } => "product",
            Self::Quote { .. } => "quote",
            Self::Bom { .. } => "bom",
        }
    }

    /// The normalized identity of this work (deterministic, display-free).
    pub fn canonical_identity(&self) -> String {
        match self {
            Self::Search { query, limit } => {
                format!("search\u{1}{}\u{1}{limit}", normalize_query(query.as_str()))
            }
            Self::Product { reference } => format!("product\u{1}{}", reference.identity_key()),
            Self::Quote {
                reference,
                quantity,
                packaging,
                variant,
            } => format!(
                "quote\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
                reference.identity_key(),
                quantity.get(),
                packaging.map(|p| p.as_str()).unwrap_or("-"),
                variant.as_ref().map(|v| v.as_str()).unwrap_or("-"),
            ),
            Self::Bom { bom } => format!("bom\u{1}{}", bom.digest()),
        }
    }

    /// The deterministic lines of this job.
    pub fn lines(&self) -> Vec<JobLine> {
        match self {
            Self::Search { query, .. } => vec![JobLine {
                item_key: synthetic_item_key(&self.canonical_identity()),
                ordinal: 0,
                query: Some(query.clone()),
                quantity: None,
                reference: None,
                variant: None,
                packaging: None,
            }],
            Self::Product { reference } => vec![JobLine {
                item_key: synthetic_item_key(&self.canonical_identity()),
                ordinal: 0,
                query: None,
                quantity: None,
                reference: Some(reference.clone()),
                variant: None,
                packaging: None,
            }],
            Self::Quote {
                reference,
                quantity,
                packaging,
                variant,
            } => vec![JobLine {
                item_key: synthetic_item_key(&self.canonical_identity()),
                ordinal: 0,
                query: None,
                quantity: Some(*quantity),
                reference: Some(reference.clone()),
                variant: variant.clone(),
                packaging: *packaging,
            }],
            Self::Bom { bom } => bom
                .items()
                .iter()
                .enumerate()
                .map(|(ordinal, item)| JobLine {
                    item_key: bom_line_key(ordinal as u32, item),
                    ordinal: ordinal as u32,
                    query: Some(item.q.clone()),
                    quantity: Some(item.qty),
                    reference: None,
                    variant: item.variant.clone(),
                    packaging: item.packaging,
                })
                .collect(),
        }
    }

    /// Re-validate the bounds (also used when loading a durable job).
    pub fn validate(&self) -> Result<(), SourceError> {
        match self {
            Self::Search { query, limit } => {
                if query.len() > MAX_QUERY_BYTES || !(1..=MAX_LIMIT).contains(limit) {
                    return Err(SourceError::InvalidRequest);
                }
                Ok(())
            }
            Self::Product { reference } => reference.validate().map_err(SourceError::from),
            Self::Quote {
                reference,
                quantity,
                ..
            } => {
                if quantity.get() > MAX_ORDER_QUANTITY {
                    return Err(SourceError::InvalidRequest);
                }
                reference.validate().map_err(SourceError::from)
            }
            Self::Bom { bom } => bom.validate().map_err(SourceError::from),
        }
    }
}

fn synthetic_item_key(canonical: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(JOB_ITEM_DOMAIN);
    hasher.update(&(canonical.len() as u64).to_le_bytes());
    hasher.update(canonical.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// The stable, per-line unique key of BOM line `ordinal`: the line's content
/// key bound to its requested position. Two duplicate (or
/// identically-normalizing) lines therefore remain two distinct durable job
/// items — the `(job_id, item_key)` primary key can never collapse requested
/// lines — while [`Bom::digest`] stays the content digest of the request.
pub fn bom_line_key(ordinal: u32, item: &BomItem) -> String {
    let content = item.key();
    let mut hasher = blake3::Hasher::new();
    hasher.update(BOM_LINE_KEY_DOMAIN);
    hasher.update(&ordinal.to_le_bytes());
    hasher.update(&(content.len() as u64).to_le_bytes());
    hasher.update(content.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// One deterministic job line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLine {
    /// The stable item key.
    pub item_key: String,
    /// The ordinal.
    pub ordinal: u32,
    /// The line query, when the work has one.
    pub query: Option<Text<{ MAX_QUERY_BYTES }>>,
    /// The requested quantity, when the work has one.
    pub quantity: Option<NonZeroQuantity>,
    /// The product reference, when the work has one.
    pub reference: Option<ProductRef>,
    /// The requested variant, when the work has one. For a BOM line this is
    /// the line's own selection, folded into the item key and the BOM digest.
    pub variant: Option<crate::text::VariantId>,
    /// The requested packaging, when the work has one.
    pub packaging: Option<crate::packaging::PackagingType>,
}

/// The full request of one job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommerceJobRequest {
    /// The work.
    pub work: JobWork,
    /// The requested source set.
    pub sources: SourceSet,
    /// The freshness policy.
    pub freshness: FreshnessMode,
    /// The detail level.
    pub detail: DetailLevel,
    /// The account scope (account data never crosses scopes).
    pub account: Option<AccountScope>,
}

impl CommerceJobRequest {
    /// Validate the bounds of the request.
    pub fn validate(&self) -> Result<(), SourceError> {
        self.work.validate()?;
        self.sources.validate().map_err(SourceError::from)
    }
}

/// A durable job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommerceJob {
    /// The job id.
    pub id: JobId,
    /// The request digest.
    pub digest: String,
    /// The state.
    pub state: JobState,
    /// The request.
    pub request: CommerceJobRequest,
}

/// The digest of a normalized job request:
/// `BLAKE3(normalized request + enabled sources + profile identity +
/// freshness policy + owner identity)`. `enabled_sources` is the sorted,
/// stable list of configured sources for the requested capability (never
/// health-filtered: a digest must not change because a breaker opened).
/// `owner` is the runtime-derived principal: the owner key is part of the
/// digest, so two sessions or accounts can never dedup onto one active job.
pub fn job_digest(
    request: &CommerceJobRequest,
    enabled_sources: &[crate::text::SourceId],
    profile: Option<&ProfileIdentity>,
    owner: &CommercePrincipal,
) -> String {
    fn put(hasher: &mut blake3::Hasher, tag: u8, value: Option<&str>) {
        hasher.update(&[tag]);
        match value {
            None => {
                hasher.update(&[0]);
            }
            Some(value) => {
                hasher.update(&[1]);
                hasher.update(&(value.len() as u64).to_le_bytes());
                hasher.update(value.as_bytes());
            }
        }
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(JOB_DIGEST_DOMAIN);
    put(&mut hasher, 1, Some(&request.work.canonical_identity()));
    let mut requested: Vec<String> = request
        .sources
        .as_slice()
        .iter()
        .map(|source| source.as_str().to_string())
        .collect();
    // The requested set is order-insensitive: the same sources in any order
    // are the same request and must attach to the same job.
    requested.sort();
    requested.dedup();
    put(
        &mut hasher,
        2,
        Some(&if requested.is_empty() {
            "auto".to_string()
        } else {
            requested.join(",")
        }),
    );
    let mut enabled: Vec<String> = enabled_sources
        .iter()
        .map(|source| source.as_str().to_string())
        .collect();
    enabled.sort();
    enabled.dedup();
    put(&mut hasher, 3, Some(&enabled.join(",")));
    put(
        &mut hasher,
        4,
        profile.map(|profile| profile.identity_key()).as_deref(),
    );
    put(&mut hasher, 5, Some(request.freshness.as_str()));
    put(&mut hasher, 6, Some(request.detail.as_str()));
    put(
        &mut hasher,
        7,
        request.account.as_ref().map(AccountScope::as_str),
    );
    put(&mut hasher, 8, Some(&owner.owner_key()));
    hasher.finalize().to_hex().to_string()
}

/// The outcome of a submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSubmitOutcome {
    /// The job id.
    pub job_id: JobId,
    /// True when an active job with the same digest absorbed the request.
    pub attached: bool,
}

/// Submit a job: attach to an active identical job OF THE SAME OWNER, else
/// create a new owner-keyed job (with its pending items) in one transaction.
pub fn submit_job(
    store: &CommerceStore,
    request: CommerceJobRequest,
    enabled_sources: &[crate::text::SourceId],
    profile: Option<&ProfileIdentity>,
    owner: &CommercePrincipal,
    now_ms: u64,
) -> Result<(CommerceJob, bool), SourceError> {
    request.validate()?;
    let owner_key = owner.owner_key();
    let digest = job_digest(&request, enabled_sources, profile, owner);
    if let Some(existing) = store.find_active_job(&digest, &owner_key)? {
        return Ok((existing, true));
    }
    let seq = store.job_count(None)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(JOB_ID_DOMAIN);
    hasher.update(digest.as_bytes());
    hasher.update(&now_ms.to_le_bytes());
    hasher.update(&seq.to_le_bytes());
    let job_id = format!("job_{}", &hasher.finalize().to_hex().to_string()[..32]);
    let job = CommerceJob {
        id: job_id.clone(),
        digest,
        state: JobState::Queued,
        request: request.clone(),
    };
    if !store.insert_job(&job, &owner_key, now_ms)? {
        // Two openers raced on the same id: converge on the durable row.
        let existing = store.job(&job_id)?.ok_or(SourceError::Store)?;
        return Ok((job_from_row(existing)?, true));
    }
    for line in request.work.lines() {
        store.upsert_job_item(&JobItemRow {
            job_id: job_id.clone(),
            item_key: line.item_key,
            ordinal: line.ordinal,
            state: JobItemState::Pending,
            attempts: 0,
            result: None,
            error_label: None,
            updated_ms: now_ms,
        })?;
    }
    Ok((job, false))
}

fn job_from_row(row: JobRow) -> Result<CommerceJob, SourceError> {
    let request = serde_json::from_str(row.request.as_str()).map_err(|_| SourceError::Store)?;
    Ok(CommerceJob {
        id: row.job_id,
        digest: row.digest,
        state: row.state,
        request,
    })
}

/// One completed line result, as persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobItemResult {
    /// The settled state.
    pub state: JobItemState,
    /// A bounded operator summary (never a raw body).
    pub label: Option<String>,
    /// The compact important entry for this line, when one exists.
    pub important: Option<ImportantEntry>,
    /// The bounded normalized detail.
    pub detail: NormalizedPayload,
    /// When the line was settled.
    pub observed_at_ms: u64,
}

/// What one executor returns for one line.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOutcome {
    /// The settled state (`Matched`/`Ambiguous`/`Unmatched`).
    pub state: JobItemState,
    /// A bounded summary.
    pub label: Option<String>,
    /// The compact important entry, when the line needs one.
    pub important: Option<ImportantEntry>,
    /// The bounded normalized detail for the artifact.
    pub detail: NormalizedPayload,
}

impl ItemOutcome {
    /// Validate the state and persist the result payload.
    pub fn into_result(self, now_ms: u64) -> Result<JobItemResult, SourceError> {
        if !matches!(
            self.state,
            JobItemState::Matched | JobItemState::Ambiguous | JobItemState::Unmatched
        ) {
            return Err(SourceError::Store);
        }
        let label = match self.label {
            None => None,
            Some(label) => {
                // Bound, then scrub credential values, then re-bound and
                // refuse forbidden material.
                let bounded = Text::<256>::new(&label).map_err(|_| SourceError::Store)?;
                let scrubbed = crate::result::scrub_secrets(bounded.as_str());
                let bounded = Text::<256>::new(&scrubbed).map_err(|_| SourceError::Store)?;
                if crate::result::scan_forbidden(bounded.as_str()).is_err() {
                    return Err(SourceError::Store);
                }
                Some(bounded.as_str().to_string())
            }
        };
        Ok(JobItemResult {
            state: self.state,
            label,
            important: self.important,
            detail: self.detail,
            observed_at_ms: now_ms,
        })
    }
}

/// The injected per-line acquisition seam. The real service implements it
/// over connectors; tests use a fake. Executors never create HTTP clients or
/// browsers themselves.
#[async_trait::async_trait]
pub trait JobItemExecutor: Send + Sync {
    /// Acquire one line deterministically.
    async fn execute(
        &self,
        ctx: &AcquireCtx,
        job: &CommerceJob,
        line: &JobLine,
    ) -> Result<ItemOutcome, SourceError>;
}

/// A job plus its compact result.
#[derive(Debug, Clone, PartialEq)]
pub struct JobOutcome {
    /// The current job.
    pub job: CommerceJob,
    /// The compact context result.
    pub compact: CompactResult,
}

/// A compact "still running" result at the caller's deadline.
pub fn running_outcome(job: &CommerceJob, now_ms: u64) -> JobOutcome {
    let _ = now_ms;
    JobOutcome {
        job: job.clone(),
        compact: CompactResult {
            status: CompactStatus::Running,
            lines: job.request.work.lines().len() as u64,
            matched: 0,
            ambiguous: 0,
            unmatched: 0,
            artifact: None,
            important: Vec::new(),
            truncated: false,
        },
    }
}

/// The compact result of a terminal job whose persisted compact JSON is
/// missing. A failed or cancelled job can be terminal without a compact
/// result (for example a cancellation between state transitions); it is
/// reported with its TRUE terminal [`CompactStatus`] and the typed
/// `last_error` diagnostic, never as a fabricated `running`. A `completed`
/// job always persists its compact result, so its absence is durable
/// corruption and a typed error.
pub fn terminal_outcome(
    job: &CommerceJob,
    counts: ResultCounts,
    last_error: Option<&str>,
) -> Result<JobOutcome, SourceError> {
    let status = match job.state {
        JobState::Failed => CompactStatus::Failed,
        JobState::Cancelled => CompactStatus::Cancelled,
        _ => return Err(SourceError::Store),
    };
    let mut important: Vec<ImportantEntry> = Vec::new();
    let diagnostic = last_error
        .map(bounded_diagnostic)
        .unwrap_or_else(|| job.state.as_str().to_string());
    if let Ok(entry) = ImportantEntry::new("job-error", &diagnostic) {
        important.push(entry);
    }
    Ok(JobOutcome {
        job: job.clone(),
        compact: CompactResult::new(status, counts, None, important)?,
    })
}

/// Bound one typed diagnostic to the `important`-entry value limit on a char
/// boundary.
fn bounded_diagnostic(value: &str) -> String {
    if value.len() <= crate::result::MAX_IMPORTANT_VALUE_BYTES {
        return value.to_string();
    }
    let mut end = crate::result::MAX_IMPORTANT_VALUE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Advance one job deterministically. Every settled line is committed
/// durably before the next line runs; a re-entry after a crash skips settled
/// lines and never repeats their external work (a line that was in flight
/// when the process died is retried, which is the documented at-least-once
/// boundary of any external effect).
pub async fn advance_job(
    store: &CommerceStore,
    ctx: &AcquireCtx,
    job_id: &str,
    executor: &dyn JobItemExecutor,
    artifacts: &dyn ArtifactStore,
    now_ms: u64,
) -> Result<JobOutcome, SourceError> {
    let row = store.job(job_id)?.ok_or(SourceError::Store)?;
    // A legacy ownerless row is unusable: it cannot be attributed to any
    // principal, so it must never execute or be reported. The v3 migration
    // already terminalized active ownerless rows; this is defense in depth.
    if row.owner_key.is_none() {
        tracing::warn!(
            job_id,
            "commerce job refused: ownerless legacy row has no principal"
        );
        return Err(SourceError::Store);
    }
    if row.state.is_terminal() {
        if let Some(compact_json) = row.compact_json.clone() {
            let compact: CompactResult =
                serde_json::from_str(&compact_json).map_err(|_| SourceError::Store)?;
            return Ok(JobOutcome {
                job: job_from_row(row)?,
                compact,
            });
        }
        let counts = ResultCounts {
            matched: row.matched,
            ambiguous: row.ambiguous,
            unmatched: row.unmatched,
        };
        let last_error = row.last_error.clone();
        return terminal_outcome(&job_from_row(row)?, counts, last_error.as_deref());
    }
    store.update_job_state(job_id, JobState::Running, now_ms)?;
    let job = job_from_row(store.job(job_id)?.ok_or(SourceError::Store)?)?;
    let lines = job.request.work.lines();
    let mut items = store.job_items(job_id)?;
    if items.is_empty() {
        for line in &lines {
            store.upsert_job_item(&JobItemRow {
                job_id: job_id.to_string(),
                item_key: line.item_key.clone(),
                ordinal: line.ordinal,
                state: JobItemState::Pending,
                attempts: 0,
                result: None,
                error_label: None,
                updated_ms: now_ms,
            })?;
        }
        items = store.job_items(job_id)?;
    }

    for item in &items {
        if item.state.is_settled() {
            continue;
        }
        let Some(line) = lines.iter().find(|line| line.item_key == item.item_key) else {
            // A durable item without a matching request line is corruption:
            // settle it unmatched rather than retrying forever.
            store.upsert_job_item(&JobItemRow {
                job_id: job_id.to_string(),
                item_key: item.item_key.clone(),
                ordinal: item.ordinal,
                state: JobItemState::Unmatched,
                attempts: item.attempts,
                result: Some(NormalizedPayload::from_json(
                    "{\"state\":\"unmatched\",\"detail\":null}".to_string(),
                )?),
                error_label: Some("orphaned_item".to_string()),
                updated_ms: now_ms,
            })?;
            continue;
        };
        ctx.check()?;
        match ctx.run_bounded(executor.execute(ctx, &job, line)).await {
            Ok(outcome) => {
                let result = outcome.into_result(now_ms)?;
                let payload = NormalizedPayload::from_serializable(&result)?;
                let state = result.state;
                store.upsert_job_item(&JobItemRow {
                    job_id: job_id.to_string(),
                    item_key: item.item_key.clone(),
                    ordinal: item.ordinal,
                    state,
                    attempts: item.attempts,
                    result: Some(payload),
                    error_label: None,
                    updated_ms: now_ms,
                })?;
            }
            Err(error) => {
                let class = error.retry_class();
                let permanent = matches!(
                    class,
                    crate::error::SourceRetryClass::Permanent
                        | crate::error::SourceRetryClass::HumanRequired
                );
                if permanent {
                    let payload = NormalizedPayload::from_serializable(&JobItemResult {
                        state: JobItemState::Failed,
                        label: None,
                        important: None,
                        detail: NormalizedPayload::from_json(
                            "{\"error\":\"acquisition_failed\"}".to_string(),
                        )?,
                        observed_at_ms: now_ms,
                    })?;
                    store.upsert_job_item(&JobItemRow {
                        job_id: job_id.to_string(),
                        item_key: item.item_key.clone(),
                        ordinal: item.ordinal,
                        state: JobItemState::Failed,
                        attempts: item.attempts,
                        result: Some(payload),
                        error_label: Some(error.as_str().to_string()),
                        updated_ms: now_ms,
                    })?;
                    continue;
                }
                if matches!(class, crate::error::SourceRetryClass::Aborted) {
                    // Cancellation/deadline is a job-level abort, not a
                    // per-line failure: the line stays pending, consumes no
                    // retry budget and returns a running outcome.
                    store.upsert_job_item(&JobItemRow {
                        job_id: job_id.to_string(),
                        item_key: item.item_key.clone(),
                        ordinal: item.ordinal,
                        state: JobItemState::Pending,
                        attempts: item.attempts,
                        result: None,
                        error_label: Some(error.as_str().to_string()),
                        updated_ms: now_ms,
                    })?;
                    let fresh = store.job(job_id)?.ok_or(SourceError::Store)?;
                    return Ok(running_outcome(&job_from_row(fresh)?, now_ms));
                }
                // Transient / rate-limited / quota-exhausted: consume one
                // attempt. Below the documented bound the line stays pending
                // (nothing completed is re-acquired on the next pass); at the
                // bound the line is settled `Failed` with the LAST typed
                // error, so a permanently failing line stops consuming work
                // and the job can terminate.
                let attempts = item.attempts.saturating_add(1);
                if attempts >= MAX_JOB_ITEM_ATTEMPTS {
                    let payload = NormalizedPayload::from_serializable(&JobItemResult {
                        state: JobItemState::Failed,
                        label: None,
                        important: None,
                        detail: NormalizedPayload::from_json(format!(
                            "{{\"error\":\"{}\",\"attempts\":{attempts}}}",
                            error.as_str()
                        ))?,
                        observed_at_ms: now_ms,
                    })?;
                    store.upsert_job_item(&JobItemRow {
                        job_id: job_id.to_string(),
                        item_key: item.item_key.clone(),
                        ordinal: item.ordinal,
                        state: JobItemState::Failed,
                        attempts,
                        result: Some(payload),
                        error_label: Some(error.as_str().to_string()),
                        updated_ms: now_ms,
                    })?;
                    continue;
                }
                store.upsert_job_item(&JobItemRow {
                    job_id: job_id.to_string(),
                    item_key: item.item_key.clone(),
                    ordinal: item.ordinal,
                    state: JobItemState::Pending,
                    attempts,
                    result: None,
                    error_label: Some(error.as_str().to_string()),
                    updated_ms: now_ms,
                })?;
                let fresh = store.job(job_id)?.ok_or(SourceError::Store)?;
                return Ok(running_outcome(&job_from_row(fresh)?, now_ms));
            }
        }
    }

    // Every line settled: assemble counts, important entries and the CAS
    // artifact, then finish the job.
    let items = store.job_items(job_id)?;
    let mut counts = ResultCounts::default();
    let mut important: Vec<ImportantEntry> = Vec::new();
    for item in &items {
        match item.state {
            JobItemState::Matched => counts.matched += 1,
            JobItemState::Ambiguous => counts.ambiguous += 1,
            JobItemState::Unmatched | JobItemState::Failed => counts.unmatched += 1,
            JobItemState::Pending => {
                return Ok(running_outcome(&job, now_ms));
            }
        }
        if matches!(
            item.state,
            JobItemState::Ambiguous | JobItemState::Unmatched | JobItemState::Failed
        ) {
            if important.len() >= MAX_IMPORTANT_ENTRIES {
                continue;
            }
            let label = item
                .error_label
                .clone()
                .unwrap_or_else(|| item.state.as_str().to_string());
            let key = format!("line-{:04}", item.ordinal + 1);
            let value = format!("{} {}", item.item_key, label);
            if let Ok(entry) = ImportantEntry::new(&key, &value) {
                important.push(entry);
            }
        }
    }
    let bytes = assemble_artifact(&job, &items)?;
    let artifact_ref = artifacts.put(&bytes).await?;
    let compact = CompactResult::new(
        CompactStatus::Completed,
        counts,
        Some(artifact_ref),
        important,
    )?;
    let compact_json = compact.to_bounded_json()?;
    store.finish_job(
        job_id,
        JobState::Completed,
        counts,
        compact.artifact.as_ref(),
        &compact_json,
        None,
        now_ms,
    )?;
    let fresh = store.job(job_id)?.ok_or(SourceError::Store)?;
    Ok(JobOutcome {
        job: job_from_row(fresh)?,
        compact,
    })
}

/// Assemble the bounded artifact body: one JSON line per job item with its
/// normalized detail. Forbidden material (markup, cookies, tokens, headers)
/// refuses the whole artifact.
fn assemble_artifact(job: &CommerceJob, items: &[JobItemRow]) -> Result<Vec<u8>, SourceError> {
    #[derive(Serialize)]
    struct ArtifactLine<'a> {
        key: &'a str,
        ordinal: u32,
        state: &'a str,
        detail: Option<&'a str>,
    }
    #[derive(Serialize)]
    struct ArtifactBody<'a> {
        job: &'a str,
        digest: &'a str,
        kind: &'a str,
        lines: Vec<ArtifactLine<'a>>,
    }
    let mut lines = Vec::new();
    let details: Vec<Option<String>> = items
        .iter()
        .map(|item| {
            item.result
                .as_ref()
                .map(|payload| crate::result::scrub_secrets(payload.as_str()))
        })
        .collect();
    for (item, detail) in items.iter().zip(&details) {
        lines.push(ArtifactLine {
            key: &item.item_key,
            ordinal: item.ordinal,
            state: item.state.as_str(),
            detail: detail.as_deref(),
        });
    }
    let body = ArtifactBody {
        job: &job.id,
        digest: &job.digest,
        kind: job.request.work.kind(),
        lines,
    };
    let bytes = serde_json::to_vec(&body).map_err(|_| SourceError::Store)?;
    if bytes.len() > crate::result::MAX_ARTIFACT_BYTES {
        return Err(SourceError::ResponseTooLarge);
    }
    if let Err(forbidden) = crate::result::scan_forbidden_bytes(&bytes) {
        tracing::warn!(
            kind = forbidden.kind,
            at = forbidden.at,
            "commerce job artifact refused: forbidden material"
        );
        return Err(SourceError::Store);
    }
    Ok(bytes)
}

/// A convenience constructor for the common BOM submission.
pub fn bom_request(
    bom: Bom,
    sources: SourceSet,
    freshness: FreshnessMode,
    detail: DetailLevel,
    account: Option<AccountScope>,
) -> CommerceJobRequest {
    CommerceJobRequest {
        work: JobWork::Bom { bom },
        sources,
        freshness,
        detail,
        account,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(workspace: u64, session: u64, scope: Option<&str>) -> CommercePrincipal {
        CommercePrincipal::new(
            WorkspaceId::new(workspace),
            SessionId::new(session),
            scope.map(|value| AccountScope::new(value).expect("scope")),
        )
    }

    #[test]
    fn owner_key_is_deterministic_and_separates_every_identity_axis() {
        let base = principal(1, 1, None);
        assert_eq!(base.owner_key(), base.owner_key());
        assert_eq!(base.owner_key().len(), 64);
        assert!(base.owner_key().chars().all(|c| c.is_ascii_hexdigit()));

        let other_workspace = principal(2, 1, None);
        let other_session = principal(1, 2, None);
        let scoped = principal(1, 1, Some("acct-a"));
        let other_scope = principal(1, 1, Some("acct-b"));
        let keys = [
            base.owner_key(),
            other_workspace.owner_key(),
            other_session.owner_key(),
            scoped.owner_key(),
            other_scope.owner_key(),
        ];
        let distinct: std::collections::BTreeSet<&String> = keys.iter().collect();
        assert_eq!(
            distinct.len(),
            keys.len(),
            "workspace, session and account scope must each separate owner keys"
        );
        assert_eq!(
            principal(1, 1, Some("acct-a")).owner_key(),
            scoped.owner_key(),
            "the same identity derives the same key"
        );
    }
}
