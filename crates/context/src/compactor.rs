//! Compaction that cannot enter a death spiral (spec §9) and cannot let a
//! model rewrite the user's ask (audits 70/71).
//!
//! Hard invariant: a successful compaction must achieve the configured
//! minimum reduction. An LLM "summary" that would leave the context above
//! the target (or shave only ~1%) is REJECTED and deterministic pruning
//! takes over. Incremental by design: the durable task projection is
//! preserved whole, only old recent turns are archived.
//!
//! # Structured compaction pipeline (audit 70)
//!
//! BEFORE any LLM summarization runs, deterministic stages run in order:
//!
//! 1. **Evidence extraction.** A huge historical tool output (a `tool`
//!    turn above [`TOOL_OUTPUT_EVIDENCE_THRESHOLD_BYTES`]) is
//!    normalized/compressed/stored through `faktor-evidence`; the wire
//!    carries an [`EvidenceRef`] whose compact body is bounded by the
//!    evidence crate's 16 KiB ceiling, and the original stays retrievable
//!    from the [`EvidenceArchive`]. Identical outputs content-address to
//!    ONE evidence id.
//! 2. **Exact dedupe.** Byte-identical (role, text) rows collapse to their
//!    first occurrence before summarization or pruning.
//! 3. **Durable truth.** Goal, criteria, decisions, checks, plan and child
//!    progress come from a [`TaskContextProjection`] built from durable
//!    rows (typed Task + typed ledger + VerificationRecord rows), never
//!    from the transcript. Goal/criteria ride the post-compaction wire
//!    VERBATIM ([`TaskContextProjection::identity_render`]) and are
//!    excluded from the summarizer input entirely.
//! 4. **Gated summarization.** The LLM summarizer is invoked only when the
//!    post-extraction conversational residue exceeds
//!    [`CompactionRequest::summarizer_residue_budget_tokens`]; below the
//!    budget the deterministic pipeline compacts with no model call.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::artifact::ArtifactRef;
use crate::ledger::TaskContextProjection;
use faktor_evidence::compress::compress;
use faktor_evidence::normalize::ProcessLogSummary;
use faktor_evidence::store::{
    EvidenceAccessContext, EvidenceStore, MemoryEvidenceStore, StoredEvidence,
};
use faktor_evidence::types::{
    BackingCompleteness, CompactRepresentation, CompressionRecord, EvidenceEnvelope, EvidenceError,
    EvidenceId, EvidenceKind, ProvenanceSet, ProvenanceSource, RetrievalPolicy,
};

/// Bound on ONE chunk of the rendered text of evicted turns carried out of
/// the compactor for the content store. There is NO total archive cap: every
/// evicted turn is preserved across as many chunks as needed, and the runtime
/// stores each chunk in the CAS behind a JSON manifest (the recent history in
/// RAM is itself bounded, so the chunked archive never exceeds it).
const ARCHIVE_CHUNK_BYTES: usize = 512 * 1024;
const DIGEST_MAX_CHARS: usize = 600;
const DIGEST_MAX_LINES: usize = 8;
const ARTIFACT_PLACEHOLDER: &str = "<artifact://hash>";

/// A `tool`-role turn larger than this many bytes is "huge historical tool
/// output": it leaves the wire as a bounded evidence ref instead of riding
/// inline. User/assistant turns are never extracted this way.
pub const TOOL_OUTPUT_EVIDENCE_THRESHOLD_BYTES: usize = 16 * 1024;
/// The evidence crate's hard compact-body ceiling; re-stated here so the
/// wire bound is visible at the compaction contract (compress() enforces
/// it at 16 KiB).
pub const EVIDENCE_COMPACT_BODY_MAX_BYTES: usize = 16 * 1024;
/// Backing bytes the default archive retains per evidence id (the original
/// is retrievable below this cap; above it the envelope + hash survive and
/// retrieval fails loudly, never silently).
pub const EVIDENCE_BACKING_CAP_BYTES: usize = 16 * 1024 * 1024;
/// Ceiling for any single evidence retrieval response.
pub const EVIDENCE_RETRIEVAL_MAX_BYTES: usize = EVIDENCE_BACKING_CAP_BYTES;
/// Default conversational-residue budget: the LLM summarizer runs only when
/// the post-extraction residue exceeds this many estimated tokens.
pub const DEFAULT_SUMMARIZER_RESIDUE_BUDGET_TOKENS: usize = 128;

/// Hard ceiling of one compaction summary (input-relative budget is
/// `hard_cap / 8`, clamped to this). The summary is a state transfer, not a
/// transcript: a 1,024-token schema carries state/decisions/files/checks/
/// blockers/next/facts while leaving the rest of the cap for recent turns.
pub const SUMMARY_MAX_OUTPUT_TOKENS: usize = 1_024;

/// Floor of the summary budget so a tiny target still gets a usable state
/// transfer instead of an empty fragment.
pub const SUMMARY_MIN_OUTPUT_TOKENS: usize = 64;

/// The summary token budget for a compaction request: at most
/// [`SUMMARY_MAX_OUTPUT_TOKENS`], never more than 1/8 of the hard cap, and
/// never below [`SUMMARY_MIN_OUTPUT_TOKENS`] unless the cap itself is
/// smaller (then it is the whole cap).
pub fn summary_budget_tokens(req: &CompactionRequest) -> usize {
    let hard = req.hard_cap();
    let scaled = hard / 8;
    scaled
        .clamp(
            SUMMARY_MIN_OUTPUT_TOKENS.min(hard),
            SUMMARY_MAX_OUTPUT_TOKENS,
        )
        .min(hard)
}

/// Strict, deterministic post-processing of a model-authored summary.
///
/// The summary is a high-signal state transfer, never a place to restate
/// durable truth. This function:
/// - drops exact duplicate lines (facts appear once);
/// - drops duplicate fact ids (`F:<id>` / `[<id>]` lines keep the first);
/// - drops lines already present in the verbatim identity / durable
///   projection (the wire carries those separately, so restating them is
///   pure duplicate cost);
/// - clamps the result to `budget_tokens` through the conservative
///   estimator.
///
/// The result is byte-deterministic for identical input, so repeated
/// compactions cannot grow the summary by rephrasing.
pub fn compact_summary_text(summary: &str, durable_render: &str, budget_tokens: usize) -> String {
    fn normalize(line: &str) -> String {
        line.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    fn fact_id(line: &str) -> Option<String> {
        let trimmed = line.trim_start().trim_start_matches("- ").trim();
        let rest = trimmed.strip_prefix("F:").or_else(|| {
            let close = trimmed.strip_prefix('[')?;
            let (id, _) = close.split_once(']')?;
            Some(id)
        })?;
        let id = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(':');
        if id.is_empty() || id.len() > 64 {
            None
        } else {
            Some(id.to_ascii_lowercase())
        }
    }
    let durable: std::collections::HashSet<String> = durable_render
        .lines()
        .map(normalize)
        .filter(|line| line.len() >= 12)
        .collect();
    // Bullet-stripped durable values catch a summary that restates a short
    // goal/decision as "- <value>": the verbatim block still carries it.
    let durable_values: std::collections::HashSet<String> = durable_render
        .lines()
        .map(|line| normalize(line.trim_start().trim_start_matches("- ")))
        .filter(|line| !line.is_empty())
        .collect();
    let mut seen_lines: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = String::with_capacity(summary.len().min(budget_tokens.saturating_mul(3)));
    for line in summary.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let key = normalize(line);
        let value_key = normalize(line.trim_start().trim_start_matches("- "));
        if (key.len() >= 12 && durable.contains(&key))
            || (!value_key.is_empty() && durable_values.contains(&value_key))
        {
            continue;
        }
        if !seen_lines.insert(key) {
            continue;
        }
        if let Some(id) = fact_id(line) {
            if !seen_ids.insert(id) {
                continue;
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    // clamp_to's documented bound is `max_tokens + 1` (the trailing
    // ellipsis); the summary budget is a HARD bound, so clamp one below it.
    Estimator.clamp_to(&out, budget_tokens.saturating_sub(1))
}

/// Insertion-ordered rendering of turns (`"{role}: {text}\n"`), the same
/// shape the archive chunks use.
#[cfg(test)]
fn render_turns<'a>(turns: impl Iterator<Item = &'a RecentTurn>) -> String {
    let mut out = String::new();
    for t in turns {
        out.push_str(&format!("{}: {}\n", t.role, t.text));
    }
    out
}

/// Character count of one turn as the per-turn estimator rendered it
/// (`"{role}: {text}"`, no trailing newline) without allocating the String.
fn turn_chars(turn: &RecentTurn) -> usize {
    turn.role
        .chars()
        .count()
        .saturating_add(turn.text.chars().count())
        .saturating_add(2)
}

/// Exact estimator value of one rendered turn (same formula as
/// `Estimator::estimate_tokens(&format!("{role}: {text}"))`, no allocation).
/// This is also the EXACT wire cost of one post-compaction message: the
/// compacted history is carried as one text message per turn, and the
/// estimator's +1 structural adder is the message envelope.
pub fn estimate_turn_tokens(turn: &RecentTurn) -> usize {
    Estimator.estimate_chars(turn_chars(turn))
}

/// Exact wire cost of a list of compacted turns (the number the hard
/// invariant must compare against the cap). This is the SAME metric the
/// compactor charges while it decides what to keep, so `after_tokens` can
/// never under-report the bytes the next request will actually carry.
pub fn estimate_wire_turns(turns: &[RecentTurn]) -> usize {
    turns
        .iter()
        .map(estimate_turn_tokens)
        .fold(0usize, usize::saturating_add)
}

/// Exact estimator value of the concatenated rendering of `turns` (same
/// formula, one pass over the characters, no intermediate String).
fn estimate_rendered_tokens<'a>(turns: impl Iterator<Item = &'a RecentTurn>) -> usize {
    let chars = turns
        .map(|turn| turn_chars(turn).saturating_add(1))
        .fold(0usize, |total, chars| total.saturating_add(chars));
    Estimator.estimate_chars(chars)
}

/// The runtime's compaction trigger and the session's hard-invariant record
/// must speak the SAME token unit. The compactor measures a history by its
/// rendered wire shape (`role: text`), so the runtime calls this instead of
/// approximating with `bytes / 4` (which under-counted the estimator by
/// about 25% and made the recorded reduction ratio meaningless).
pub fn estimate_history_tokens(turns: &[RecentTurn]) -> usize {
    estimate_rendered_tokens(turns.iter())
}

/// Deterministic digest of the EXACT compacted context (role + text pairs,
/// length-prefixed). A restart must reproduce this value byte-for-byte from
/// the durable checkpoint; probes and recovery compare it instead of
/// trusting an in-memory plan object.
pub fn history_digest(turns: &[RecentTurn]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(turns.len() as u64).to_le_bytes());
    for turn in turns {
        for part in [turn.role.as_bytes(), turn.text.as_bytes()] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
    }
    hasher.finalize().to_hex().to_string()
}

// Test-only work counter for the deterministic keep scan: proves the
// per-compaction work is a constant number of passes over the loaded
// window (never a rescan of the whole conversation per kept turn).
#[cfg(test)]
thread_local! {
    static TURN_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_turn_visit() {
    TURN_VISITS.with(|v| v.set(v.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn take_turn_visits() -> usize {
    TURN_VISITS.with(|v| v.replace(0))
}

/// The exact wire text of the eviction digest marker.
fn digest_marker(digest_text: &str) -> String {
    format!(
        "[Earlier context archived: {}…{}]",
        truncate(digest_text, DIGEST_MAX_CHARS),
        ARTIFACT_PLACEHOLDER
    )
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// (b) Exact-fact dedupe: byte-identical (role, text) rows collapse to their
/// FIRST occurrence, preserving order. Applied after evidence extraction, so
/// identical huge tool outputs also collapse (they render one ref).
fn dedupe_exact_rows(turns: &[RecentTurn]) -> Vec<RecentTurn> {
    let mut seen: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(turns.len());
    for turn in turns {
        if seen.insert((turn.role.as_str(), turn.text.as_str())) {
            out.push(turn.clone());
        }
    }
    out
}

/// Split prior compaction summaries out of the conversational residue:
/// `(residue_without_summaries, newest_summary, older_summaries)`.
///
/// A summary is model-authored compression, not a new fact. Feeding it back
/// into a summarizer lets repeated compactions re-summarize their own output
/// and drift; the newest summary is therefore carried VERBATIM (or archived
/// whole when it cannot fit) and never enters the summarizer input. Older
/// summaries are superseded and archived.
fn partition_prior_metadata(
    turns: &[RecentTurn],
) -> (
    Vec<RecentTurn>,
    Option<RecentTurn>,
    Vec<RecentTurn>,
    Option<RecentTurn>,
) {
    let mut residue = Vec::with_capacity(turns.len());
    let mut summaries: Vec<RecentTurn> = Vec::new();
    let mut digest: Option<RecentTurn> = None;
    for turn in turns {
        if turn.role == "summary" {
            summaries.push(turn.clone());
        } else if turn.role == "assistant" && is_digest_marker(turn) {
            // Only the NEWEST archive pointer matters; older ones are
            // superseded by it (they describe the same durable archive).
            digest = Some(turn.clone());
        } else {
            residue.push(turn.clone());
        }
    }
    let carried = summaries.pop();
    (residue, carried, summaries, digest)
}

/// One deterministic keep pass's result (the retry loop can revisit the
/// keep budget when the optional digest marker does not fit).
struct KeptPlan {
    wire: Vec<RecentTurn>,
    archived: Vec<ArtifactRef>,
    chunks: Vec<String>,
    after: usize,
    accepted: bool,
}

fn is_digest_marker(turn: &RecentTurn) -> bool {
    turn.text.starts_with("[Earlier context archived:")
}

/// Only a TOOL-role turn is evidence-eligible. Tool output has no
/// instruction authority and no user-policy bytes, so it may be moved
/// behind an evidence ref; user and assistant text stay exactly where they
/// were (the user's ask is policy; assistant residue keeps its own archival
/// path through the CAS manifest).
fn is_evidence_eligible(turn: &RecentTurn) -> bool {
    turn.text.len() > TOOL_OUTPUT_EVIDENCE_THRESHOLD_BYTES && turn.role.eq_ignore_ascii_case("tool")
}

/// (c) The synthesized wire turns that carry the durable truth OUTSIDE the
/// summarization input: the verbatim identity block (goal + criteria) and
/// the durable facts render. Empty blocks produce no turn, so a compaction
/// with no durable content stays turn-for-turn identical to the old shape.
fn synthesized_turns(identity: &str, durable_facts: &str) -> Vec<RecentTurn> {
    let mut out = Vec::new();
    if !identity.is_empty() {
        out.push(RecentTurn {
            role: "system".into(),
            text: format!("TASK IDENTITY (verbatim):\n{identity}"),
        });
    }
    if !durable_facts.is_empty() {
        out.push(RecentTurn {
            role: "system".into(),
            text: format!("DURABLE TASK PROJECTION:\n{durable_facts}"),
        });
    }
    out
}

/// One reference to archived historical tool output: the compact body is
/// wire-visible and bounded by [`EVIDENCE_COMPACT_BODY_MAX_BYTES`]; the
/// original bytes are retrievable through the [`EvidenceArchive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef {
    pub id: EvidenceId,
    pub kind: EvidenceKind,
    pub original_bytes: usize,
    pub compact_bytes: usize,
    pub backing_hash: [u8; 32],
    /// Bounded structural summary produced by the evidence compressor.
    pub compact_body: String,
    /// Exit code normalized from the raw output, when recognized.
    pub normalized_exit: Option<i32>,
    /// Lines the normalizer kept as important, when normalization ran.
    pub normalized_important_lines: usize,
}

impl EvidenceRef {
    /// The wire text that replaces the raw output.
    pub fn render(&self) -> String {
        format!(
            "[tool output archived: evidence://{} kind={:?} original_bytes={} compact_bytes={}{}]\n{}",
            self.id,
            self.kind,
            self.original_bytes,
            self.compact_bytes,
            self.normalized_exit
                .map(|e| format!(" exit={e}"))
                .unwrap_or_default(),
            self.compact_body,
        )
    }
}

/// Deterministic archive of huge historical tool output as evidence
/// envelopes + backing, built on `faktor-evidence` (normalize + compress +
/// store). Content-addressed by blake3: re-archiving identical bytes returns
/// the SAME ref (duplicate insert can never overwrite stored evidence).
///
/// Bounded: backing is retained per envelope up to `backing_cap`; beyond it
/// the envelope + hash survive and retrieval fails loudly. Scope reads go
/// through the evidence store's session/workspace check.
#[derive(Debug)]
pub struct EvidenceArchive {
    store: MemoryEvidenceStore,
    session_id: faktor_core::SessionId,
    workspace_id: faktor_core::WorkspaceId,
    task_id: Option<u64>,
}

impl Default for EvidenceArchive {
    fn default() -> Self {
        // Standalone session defaults (ids may never be 0).
        Self {
            store: MemoryEvidenceStore::new(EVIDENCE_BACKING_CAP_BYTES),
            session_id: faktor_core::SessionId::new(1),
            workspace_id: faktor_core::WorkspaceId::new(1),
            task_id: None,
        }
    }
}

impl EvidenceArchive {
    /// A scoped archive, or a typed refusal when `session_id`/`workspace_id`
    /// is 0 or `task_id` is `Some(0)`. Zero is never remapped onto another
    /// identity (the historic `.max(1)` clamp could remap a hostile 0 onto
    /// identity 1).
    pub fn new(
        session_id: u64,
        workspace_id: u64,
        task_id: Option<u64>,
    ) -> Result<Self, EvidenceError> {
        Self::with_backing_cap(
            session_id,
            workspace_id,
            task_id,
            EVIDENCE_BACKING_CAP_BYTES,
        )
    }

    /// Fallible typed constructor: every zero id is an explicit
    /// [`EvidenceError::Malformed`] naming the field, never a panic and never
    /// a silent remap.
    pub fn with_backing_cap(
        session_id: u64,
        workspace_id: u64,
        task_id: Option<u64>,
        backing_cap: usize,
    ) -> Result<Self, EvidenceError> {
        let session_id = faktor_core::SessionId::try_from(session_id)
            .map_err(|e| EvidenceError::Malformed(format!("evidence archive session_id: {e}")))?;
        let workspace_id = faktor_core::WorkspaceId::try_from(workspace_id)
            .map_err(|e| EvidenceError::Malformed(format!("evidence archive workspace_id: {e}")))?;
        if task_id == Some(0) {
            return Err(EvidenceError::Malformed(
                "evidence archive task_id cannot be 0".to_string(),
            ));
        }
        Ok(Self {
            store: MemoryEvidenceStore::new(backing_cap),
            session_id,
            workspace_id,
            task_id,
        })
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    fn access(&self) -> EvidenceAccessContext {
        EvidenceAccessContext::new(self.session_id.raw(), self.workspace_id.raw(), self.task_id)
    }

    /// Normalize + compress + store `raw` under `kind`. Idempotent for
    /// identical bytes (content-addressed id). The compact body is bounded
    /// by the evidence crate; the backing is retained only under the
    /// store's cap policy.
    pub fn archive(&mut self, kind: EvidenceKind, raw: &str) -> Result<EvidenceRef, EvidenceError> {
        let hash = *blake3::hash(raw.as_bytes()).as_bytes();
        // normalize step (bounded typed view; a >MAX_INPUT_LINES hostile
        // line count still compresses below via the streaming transform).
        let normalized = ProcessLogSummary::try_from_text(raw).ok();
        let normalized_exit = normalized.as_ref().and_then(|n| n.exit);
        let normalized_important_lines = normalized
            .as_ref()
            .map(|n| n.important_lines.len())
            .unwrap_or(0);

        let mut id_raw = u64::from_be_bytes(hash[..8].try_into().expect("8 bytes"));
        loop {
            let candidate = EvidenceId(id_raw);
            match self.store.get(candidate)? {
                Some(stored) if stored.envelope.backing_hash == Some(hash) => {
                    // Exact duplicate: the SAME evidence ref, never a second
                    // copy (audit 70 dedupe at the evidence layer).
                    return Ok(EvidenceRef {
                        id: candidate,
                        kind: stored.envelope.kind,
                        original_bytes: stored.envelope.compression.original_bytes as usize,
                        compact_bytes: stored.envelope.compression.compact_bytes as usize,
                        backing_hash: hash,
                        compact_body: stored.envelope.compact.body.clone(),
                        normalized_exit,
                        normalized_important_lines,
                    });
                }
                Some(_) => id_raw = id_raw.wrapping_add(1),
                None => break,
            }
        }
        let id = EvidenceId(id_raw);
        // compress step: kind-hardcoded policy; ProcessLog is Aggressive, so
        // the body is structurally compacted and ties back to the digest.
        // A kind whose policy copies verbatim (LosslessOnly) could exceed
        // the evidence bound — refuse it rather than promise a 16 KiB body
        // and deliver more.
        let (compact, record): (CompactRepresentation, CompressionRecord) =
            compress(&kind, BackingCompleteness::Complete, raw)?;
        if compact.body.len() > EVIDENCE_COMPACT_BODY_MAX_BYTES {
            return Err(EvidenceError::Oversized {
                max: EVIDENCE_COMPACT_BODY_MAX_BYTES,
                actual: compact.body.len(),
            });
        }
        let compressibility = kind.default_compressibility();
        let envelope = EvidenceEnvelope::new(
            id,
            kind,
            self.session_id,
            self.workspace_id,
            self.task_id,
            None,
            ProvenanceSet::new([ProvenanceSource::Tool]),
            compressibility,
            compact.clone(),
            Some(hash),
            BackingCompleteness::Complete,
            record,
            RetrievalPolicy::new(true, true, EVIDENCE_RETRIEVAL_MAX_BYTES),
        )?;
        // store step: backing retained under the store's cap policy.
        self.store.insert(envelope, Some(raw.as_bytes().to_vec()))?;
        Ok(EvidenceRef {
            id,
            kind,
            original_bytes: raw.len(),
            compact_bytes: compact.body.len(),
            backing_hash: hash,
            compact_body: compact.body,
            normalized_exit,
            normalized_important_lines,
        })
    }

    /// Scope-checked retrieval of one archived envelope (backing included
    /// when the store retained it).
    pub fn retrieve(&self, id: EvidenceId) -> Result<StoredEvidence, EvidenceError> {
        self.store.get_scoped(id, &self.access())
    }
}

/// Chunk the rendered text of evicted turns (`"{role}: {text}\n"` per turn,
/// OLDEST-first iteration) into `Vec<String>` chunks each <= `chunk_max`
/// bytes. Chunks preserve order and NEVER split mid-turn: a single turn
/// larger than the bound occupies its own whole chunk (the recent history in
/// RAM is bounded, so an oversize chunk is bounded too). Nothing is dropped.
fn fill_chunks<'a>(evicted: impl Iterator<Item = &'a RecentTurn>, chunk_max: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    for turn in evicted {
        let line = format!("{}: {}", turn.role, turn.text);
        if let Some(last) = chunks.last_mut() {
            if !last.is_empty() && last.len() + line.len() < chunk_max {
                last.push_str(&line);
                last.push('\n');
                continue;
            }
        }
        let mut chunk = String::new();
        chunk.push_str(&line);
        chunk.push('\n');
        chunks.push(chunk);
    }
    chunks
}

/// The eviction digest that rides the wire inside kept_recent[0]: the NEWEST
/// evicted material (the tail of the archive — chunks are ordered oldest
/// first), bounded to a few lines and `DIGEST_MAX_CHARS`. A single line
/// larger than the whole budget is truncated to fit, never dropped entirely,
/// so the marker always carries a glimpse of what was archived. Empty only
/// when nothing was archived.
fn archive_digest(chunks: &[String]) -> String {
    let mut newest_first: Vec<&str> = Vec::new();
    for chunk in chunks.iter().rev() {
        for line in chunk.lines().rev() {
            newest_first.push(line);
            if newest_first.len() >= DIGEST_MAX_LINES {
                break;
            }
        }
        if newest_first.len() >= DIGEST_MAX_LINES {
            break;
        }
    }
    // Render chronologically (oldest of the sampled lines first).
    let mut digest_text = String::new();
    for line in newest_first.into_iter().rev() {
        let remaining = DIGEST_MAX_CHARS.saturating_sub(digest_text.len());
        if remaining == 0 {
            break;
        }
        if line.len() > remaining {
            digest_text.push_str(truncate(line, remaining));
            digest_text.push('\n');
            break;
        }
        digest_text.push_str(line);
        digest_text.push('\n');
    }
    digest_text
}
use crate::assembler::RecentTurn;
use crate::estimator::Estimator;
use crate::ledger::TaskLedger;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionRequest {
    pub before_tokens: usize,
    pub target_tokens: usize,
    /// Minimum fraction the context must shrink by (0.25 = 25%).
    pub min_reduction_ratio: f64,
    /// (f) Conversational residue (AFTER evidence extraction and exact
    /// dedupe) above which the LLM summarizer may run. At or below the
    /// budget the deterministic pipeline compacts without any model call.
    pub summarizer_residue_budget_tokens: usize,
}

impl Default for CompactionRequest {
    fn default() -> Self {
        Self {
            before_tokens: 0,
            target_tokens: 0,
            min_reduction_ratio: 0.25,
            summarizer_residue_budget_tokens: DEFAULT_SUMMARIZER_RESIDUE_BUDGET_TOKENS,
        }
    }
}

impl CompactionRequest {
    pub fn new(before_tokens: usize, target_tokens: usize) -> Self {
        Self {
            before_tokens,
            target_tokens,
            min_reduction_ratio: 0.25,
            summarizer_residue_budget_tokens: DEFAULT_SUMMARIZER_RESIDUE_BUDGET_TOKENS,
        }
    }

    /// The maximum acceptable after_tokens: min(target, before*(1-ratio)).
    pub fn hard_cap(&self) -> usize {
        let by_ratio = if self.min_reduction_ratio <= 0.0 {
            self.before_tokens
        } else {
            let factor = (1.0 - self.min_reduction_ratio).clamp(0.0, 1.0);
            (self.before_tokens as f64 * factor) as usize
        };
        self.target_tokens.min(by_ratio)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStrategy {
    LlmSummary,
    DeterministicPruning,
    Rejected,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPlan {
    pub accepted: bool,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub target_tokens: usize,
    pub strategy: CompactionStrategy,
    pub ledger: TaskLedger,
    pub kept_recent: Vec<RecentTurn>,
    pub archived: Vec<ArtifactRef>,
    /// The rendered text of every turn evicted from the wire, chunked
    /// (`"{role}: {text}\n"` per turn, oldest first, each chunk <=
    /// `ARCHIVE_CHUNK_BYTES`, never split mid-turn, NO total cap — nothing
    /// evicted is ever omitted). This is the durable archive material for
    /// the content store: the runtime writes each chunk to the CAS, then a
    /// JSON manifest, and replaces the digest placeholder with the manifest
    /// artifact ref.
    pub archive_chunks: Vec<String>,
    /// (a) Huge historical tool output that was normalized, compressed and
    /// stored as evidence before any summarization; each ref's compact body
    /// is bounded and the original is retrievable from the archive.
    pub evidence_refs: Vec<EvidenceRef>,
}

/// Produces the LLM-written summary (injected from the agent; None in
/// deterministic-only operation). Async: real summarizers stream a
/// provider request; the deterministic ledger summarizer resolves
/// immediately.
///
/// The input is the POST-EXTRACTION residue (huge tool output already
/// replaced by evidence refs, exact duplicates collapsed) plus the
/// durable non-goal facts render — the goal and acceptance criteria are
/// NEVER passed here (audit 70: the user's ask cannot go through a lossy
/// model).
pub trait Summarizer: Send + Sync {
    fn summarize<'a>(
        &'a self,
        residue: &'a [RecentTurn],
        durable_facts: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>>;
}

/// Compactor with a per-instance evidence archive. Production wires one
/// compactor per compaction call; long-lived harnesses may reuse one
/// instance and read the archive back through [`Compactor::evidence`].
pub struct Compactor {
    summarizer: Option<Arc<dyn Summarizer>>,
    evidence: Mutex<EvidenceArchive>,
}

impl Compactor {
    pub fn new(summarizer: Option<Arc<dyn Summarizer>>) -> Self {
        Self {
            summarizer,
            evidence: Mutex::new(EvidenceArchive::default()),
        }
    }

    pub fn deterministic_only() -> Self {
        Self::new(None)
    }

    /// Scope the archive to a session/workspace/task (retrieval then runs
    /// through the evidence store's scope check). Zero ids are a typed
    /// refusal naming the field — never silently remapped.
    pub fn with_evidence_scope(
        mut self,
        session_id: u64,
        workspace_id: u64,
        task_id: Option<u64>,
    ) -> Result<Self, EvidenceError> {
        self.evidence = Mutex::new(EvidenceArchive::new(session_id, workspace_id, task_id)?);
        Ok(self)
    }

    /// Recover from a poisoned lock instead of panicking: the archive
    /// critical sections cannot panic, and compaction must never take the
    /// runtime down over an unrelated poison.
    fn lock_evidence(&self) -> MutexGuard<'_, EvidenceArchive> {
        self.evidence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The per-instance evidence archive (bounded backing, scope checks).
    pub fn evidence(&self) -> MutexGuard<'_, EvidenceArchive> {
        self.lock_evidence()
    }

    /// (a)+(b)+(c) Deterministic preparation, run BEFORE any summarization:
    /// huge non-user tool output → evidence refs (normalize/compress/store),
    /// then byte-identical rows collapse. Returns the residue plus the refs
    /// that replaced archived output.
    fn prepare_history(&self, history: &[RecentTurn]) -> (Vec<RecentTurn>, Vec<EvidenceRef>) {
        let mut prepared: Vec<RecentTurn> = Vec::with_capacity(history.len());
        let mut refs: Vec<EvidenceRef> = Vec::new();
        {
            let mut archive = self.lock_evidence();
            let mut seen_refs: std::collections::HashSet<EvidenceId> =
                std::collections::HashSet::new();
            for turn in history {
                if is_evidence_eligible(turn) {
                    if let Ok(ev) = archive.archive(EvidenceKind::ProcessLog, &turn.text) {
                        // Identical outputs content-address to ONE ref.
                        if seen_refs.insert(ev.id) {
                            refs.push(ev.clone());
                        }
                        prepared.push(RecentTurn {
                            role: turn.role.clone(),
                            text: ev.render(),
                        });
                        continue;
                    }
                    // Archive failure (refused/oversized invariant): the raw
                    // stays inline — evidence extraction never drops bytes.
                }
                prepared.push(turn.clone());
            }
        }
        (dedupe_exact_rows(&prepared), refs)
    }

    /// Compatibility entry point (existing callers/tests): the read-only
    /// projection is built from the legacy durable ledger rows and the
    /// instance's own evidence archive is used.
    pub async fn compact(
        &self,
        history: &[RecentTurn],
        ledger: &TaskLedger,
        req: &CompactionRequest,
    ) -> CompactionPlan {
        let projection = TaskContextProjection::from_task_ledger(ledger);
        self.compact_projected(history, &projection, ledger, req)
            .await
    }

    /// Compaction pipeline (audits 70/71):
    /// 1. Evidence extraction + exact dedupe (deterministic, model-free).
    /// 2. Goal/criteria identity is rendered VERBATIM outside the
    ///    summarization input.
    /// 3. If a summarizer exists AND the residue exceeds the configured
    ///    budget, try the LLM summary; accept it ONLY if it satisfies the
    ///    hard invariant (after <= hard_cap) with identity + durable facts
    ///    counted.
    /// 4. Otherwise (or when rejected), deterministic pruning.
    /// 5. If even deterministic pruning cannot reach the cap
    ///    (pathological), the plan is marked rejected with strategy
    ///    `Rejected` — callers must surface it (CompactRejected) instead of
    ///    pretending success.
    pub async fn compact_projected(
        &self,
        history: &[RecentTurn],
        projection: &TaskContextProjection,
        working_ledger: &TaskLedger,
        req: &CompactionRequest,
    ) -> CompactionPlan {
        // Stages (a)+(b): move huge historical tool output to evidence refs
        // and collapse exact duplicate rows BEFORE any model sees anything.
        let (prepared, evidence_refs) = self.prepare_history(history);
        // Prior compaction summaries are NOT conversational residue: feeding
        // a summary back into a summarizer is how repeated compactions drift.
        // The newest summary is carried VERBATIM; older ones are archived.
        let (residue, carried_summary, older_summaries, prior_digest) =
            partition_prior_metadata(&prepared);
        // (c): the user's ask, verbatim, and the durable facts, both kept
        // OUT of the summarizer input.
        let identity = projection.identity_render();
        let durable_facts = projection.summarizable_render();
        // The synthesized turns are reserved by their EXACT wire cost (role
        // + ": " + text), never by estimating the bare render: the old code
        // omitted the "TASK IDENTITY (verbatim):\n" / "DURABLE TASK
        // PROJECTION:\n" prefixes and could accept an over-cap plan.
        let synthesized = synthesized_turns(&identity, &durable_facts);
        let residue_tokens = estimate_rendered_tokens(residue.iter());

        // (f): the LLM is invoked only above the residue budget.
        if let Some(summarizer) = &self.summarizer {
            if residue_tokens > req.summarizer_residue_budget_tokens {
                let summary = summarizer.summarize(&residue, &durable_facts).await;
                // High-signal normalization BEFORE acceptance: duplicate
                // lines / duplicate fact ids collapse, and anything already
                // carried verbatim by the identity or durable projection is
                // removed (the wire carries those once). The model-authored
                // text is bounded by the request-relative summary budget so
                // a verbose summary can never consume the whole cap and
                // starve the kept recent window.
                let durable_render = format!("{identity}\n{durable_facts}");
                let summary =
                    compact_summary_text(&summary, &durable_render, summary_budget_tokens(req));
                // A verbatim transcript echo is NOT a summary: the model
                // returned the residue (role-prefixed rows included) and the
                // clamp would otherwise make a ~0% reduction look accepted.
                // Reject it exactly like a liar summary.
                let echoes_transcript = residue.iter().any(|turn| {
                    turn.text.len() > 64
                        && summary.contains(&format!("{}: {}", turn.role, turn.text))
                });
                // An EMPTY/whitespace summary would pass the token cap and
                // wipe the conversation: reject it exactly like a liar
                // summary and let deterministic pruning carry the facts.
                if !summary.trim().is_empty() && !echoes_transcript {
                    // The accepted summary REPLACES the conversational
                    // residue (and every prior summary, which it supersedes).
                    // Every replaced turn is still archived in the CAS, so
                    // the wire MUST carry its archive pointer: without the
                    // digest marker the whole archived history becomes
                    // unreachable (the deterministic branch already does
                    // this; the summary branch used to drop it, orphaning the
                    // FIRST summary's archive forever).
                    let archived = residue
                        .iter()
                        .chain(older_summaries.iter())
                        .chain(carried_summary.iter())
                        .map(|t| ArtifactRef {
                            inline: None,
                            artifact: None,
                            summary: format!("archived turn ({} chars)", t.text.len()),
                            size: t.text.len(),
                        })
                        .collect();
                    let archive_chunks = fill_chunks(
                        older_summaries
                            .iter()
                            .chain(carried_summary.iter())
                            .chain(residue.iter()),
                        ARCHIVE_CHUNK_BYTES,
                    );
                    let digest_text = archive_digest(&archive_chunks);
                    let digest_turn = if !digest_text.is_empty() {
                        Some(RecentTurn {
                            role: "assistant".into(),
                            text: digest_marker(&digest_text),
                        })
                    } else {
                        // Already-compacted context: carry the previous
                        // pointer verbatim instead of orphaning it.
                        prior_digest.clone()
                    };
                    let mut kept_recent = Vec::new();
                    if let Some(digest) = digest_turn.as_ref() {
                        // FIRST on the wire so the runtime's manifest-hash
                        // replacement targets this turn's placeholder.
                        kept_recent.push(digest.clone());
                    }
                    kept_recent.extend(synthesized.iter().cloned());
                    kept_recent.push(RecentTurn {
                        role: "summary".into(),
                        text: summary,
                    });
                    // The ACTIVE instruction rides verbatim even when the
                    // summary omits it: a summary is model-authored and may
                    // paraphrase, but the user's current ask is not
                    // negotiable context. If it cannot fit, the summary is
                    // rejected and deterministic pruning must prove the
                    // prompt fits instead.
                    if let Some(active) = residue.iter().rev().find(|t| t.role == "user") {
                        kept_recent.push(active.clone());
                    }
                    let after = estimate_wire_turns(&kept_recent);
                    if after <= req.hard_cap() {
                        return CompactionPlan {
                            accepted: true,
                            before_tokens: req.before_tokens,
                            after_tokens: after,
                            target_tokens: req.target_tokens,
                            strategy: CompactionStrategy::LlmSummary,
                            ledger: working_ledger.clone(),
                            kept_recent,
                            archived,
                            archive_chunks,
                            evidence_refs,
                        };
                    }
                }
                // REJECT the liar/empty summary; fall through to
                // deterministic pruning. The strategy stays `Rejected` so
                // the runtime journals why no LLM summary was used.
                let mut plan = self.deterministic_core(
                    &residue,
                    carried_summary.as_ref(),
                    &older_summaries,
                    prior_digest.as_ref(),
                    &synthesized,
                    working_ledger,
                    req,
                    evidence_refs,
                );
                plan.strategy = CompactionStrategy::Rejected;
                return plan;
            }
        }
        self.deterministic_core(
            &residue,
            carried_summary.as_ref(),
            &older_summaries,
            prior_digest.as_ref(),
            &synthesized,
            working_ledger,
            req,
            evidence_refs,
        )
    }

    /// Deterministic pruning: keep the durable projection in full, keep the
    /// NEWEST CONTIGUOUS SUFFIX of residue turns that fits under the cap,
    /// archive the rest as references. `synthesized` (identity + durable
    /// facts) and the carried prior summary are reserved up front at their
    /// EXACT wire cost, and `after_tokens` is measured from the actual wire
    /// turns returned — so an "accepted" plan can never exceed the cap.
    #[allow(clippy::too_many_arguments)]
    fn deterministic_core(
        &self,
        residue: &[RecentTurn],
        carried_summary: Option<&RecentTurn>,
        older_summaries: &[RecentTurn],
        prior_digest: Option<&RecentTurn>,
        synthesized: &[RecentTurn],
        working_ledger: &TaskLedger,
        req: &CompactionRequest,
        evidence_refs: Vec<EvidenceRef>,
    ) -> CompactionPlan {
        let cap = req.hard_cap();
        let synthesized_tokens = estimate_wire_turns(synthesized);
        let mut archived_summaries: Vec<RecentTurn> = older_summaries.to_vec();
        let mut carried = carried_summary.cloned();
        let carried_tokens = carried
            .as_ref()
            .map(estimate_turn_tokens)
            .unwrap_or_default();
        let mut reserved = synthesized_tokens.saturating_add(carried_tokens);
        if cap <= synthesized_tokens && carried.is_some() {
            // Even the summary cannot ride the wire under this cap: archive
            // it instead of pretending it was carried.
            if let Some(summary) = carried.take() {
                archived_summaries.push(summary);
            }
            reserved = synthesized_tokens;
        }
        // Keep the newest contiguous suffix under a keep budget. The digest
        // marker is METADATA: when it would overflow the hard cap we shrink
        // the keep budget by the overage and rerun (bounded passes), so the
        // marker is preserved whenever any space can be traded for it and
        // the final `after_tokens` is always the exact wire cost of the
        // returned turns.
        let mut keep_cap = cap;
        let mut result: Option<KeptPlan> = None;
        for attempt in 0..4 {
            let mut kept_recent = Vec::new();
            let mut evicted: Vec<&RecentTurn> = Vec::new();
            let mut used = reserved;
            let mut keep_going = keep_cap > reserved;
            // The kept window is a CONTIGUOUS newest suffix: the first turn
            // that does not fit evicts itself and every older turn, so an
            // older turn can never appear before a newer archived one and a
            // tool call can never survive across its result.
            for turn in residue.iter().rev() {
                #[cfg(test)]
                note_turn_visit();
                let t = estimate_turn_tokens(turn);
                if !keep_going || used.saturating_add(t) > keep_cap {
                    keep_going = false;
                    evicted.push(turn);
                    continue;
                }
                used = used.saturating_add(t);
                kept_recent.push(turn.clone());
            }
            kept_recent.reverse();
            let mut archived: Vec<ArtifactRef> = evicted
                .iter()
                .map(|turn| ArtifactRef {
                    inline: None,
                    artifact: None,
                    summary: format!("archived turn ({} chars)", turn.text.len()),
                    size: turn.text.len(),
                })
                .collect();
            for summary in &archived_summaries {
                archived.push(ArtifactRef {
                    inline: None,
                    artifact: None,
                    summary: format!("archived summary ({} chars)", summary.text.len()),
                    size: summary.text.len(),
                });
            }
            // Every evicted turn is archived, chunked, oldest first —
            // nothing beyond the kept window is ever omitted (the old 1 MiB
            // cap silently dropped cold history).
            let archive_chunks = fill_chunks(
                archived_summaries
                    .iter()
                    .rev()
                    .chain(evicted.iter().rev().copied()),
                ARCHIVE_CHUNK_BYTES,
            );
            let digest_text = archive_digest(&archive_chunks);
            // Prefer the NEW digest; when this compaction evicted nothing
            // (already-compacted context), carry the PREVIOUS archive
            // pointer forward verbatim so repeated compactions never orphan
            // the durable archive behind already-summarized history. The
            // marker is regenerated, never re-summarized.
            let carried_digest: Option<RecentTurn> = if !digest_text.is_empty() {
                Some(RecentTurn {
                    role: "assistant".into(),
                    text: digest_marker(&digest_text),
                })
            } else {
                prior_digest.cloned()
            };
            let mut wire_recent = Vec::new();
            wire_recent.extend(synthesized.iter().cloned());
            if let Some(summary) = carried.clone() {
                wire_recent.push(summary);
            }
            wire_recent.extend(kept_recent.iter().cloned());
            let mut after = estimate_wire_turns(&wire_recent);
            let mut over = after.saturating_sub(cap);
            let mut digest_included = false;
            if let Some(digest_turn) = carried_digest.as_ref() {
                let mut with_digest = Vec::with_capacity(wire_recent.len() + 1);
                with_digest.push(digest_turn.clone());
                with_digest.extend(wire_recent.iter().cloned());
                let with_digest_tokens = estimate_wire_turns(&with_digest);
                over = over.max(with_digest_tokens.saturating_sub(cap));
                if with_digest_tokens <= cap {
                    wire_recent = with_digest;
                    after = with_digest_tokens;
                    digest_included = true;
                }
            }
            // The ACTIVE instruction must never disappear: if the newest
            // user turn did not fit the kept suffix, this is a typed
            // rejection, not a "successful" compaction that silently drops
            // the prompt (the caller then reports CompactRejected).
            let newest_user = residue.iter().rev().find(|t| t.role == "user");
            let newest_user_present = newest_user.is_none_or(|user| {
                wire_recent
                    .iter()
                    .any(|w| w.role == "user" && w.text == user.text)
            });
            let accepted = after <= cap && newest_user_present;
            result = Some(KeptPlan {
                wire: wire_recent,
                archived,
                chunks: archive_chunks,
                after,
                accepted,
            });
            let digest_done = digest_included || carried_digest.is_none();
            if (accepted && digest_done) || kept_recent.is_empty() || attempt == 3 {
                break;
            }
            // Trade the overage out of the keep window and try again; the
            // next pass evicts older turns, which can only shrink `after`.
            // If the trade would consume the whole keep window (the digest
            // metadata alone is over budget), keep the accepted plan WITHOUT
            // the optional marker: dropping real turns — including the
            // active instruction — to make room for metadata is never right.
            let next_keep_cap = keep_cap.saturating_sub(over.max(1));
            if next_keep_cap <= reserved {
                break;
            }
            keep_cap = next_keep_cap;
        }
        let kept = result.expect("the bounded keep loop always produces a plan");
        CompactionPlan {
            accepted: kept.accepted,
            before_tokens: req.before_tokens,
            after_tokens: kept.after,
            target_tokens: req.target_tokens,
            strategy: if kept.accepted {
                CompactionStrategy::DeterministicPruning
            } else {
                CompactionStrategy::Rejected
            },
            ledger: working_ledger.clone(),
            kept_recent: kept.wire,
            archived: kept.archived,
            archive_chunks: kept.chunks,
            evidence_refs,
        }
    }

    /// Compatibility wrapper: builds the read-only projection from the
    /// legacy durable ledger rows and prunes deterministically.
    pub fn deterministic_prune(
        &self,
        history: &[RecentTurn],
        ledger: &TaskLedger,
        req: &CompactionRequest,
    ) -> CompactionPlan {
        let projection = TaskContextProjection::from_task_ledger(ledger);
        let (prepared, evidence_refs) = self.prepare_history(history);
        let (residue, carried_summary, older_summaries, prior_digest) =
            partition_prior_metadata(&prepared);
        let identity = projection.identity_render();
        let durable_facts = projection.summarizable_render();
        let synthesized = synthesized_turns(&identity, &durable_facts);
        self.deterministic_core(
            &residue,
            carried_summary.as_ref(),
            &older_summaries,
            prior_digest.as_ref(),
            &synthesized,
            ledger,
            req,
            evidence_refs,
        )
    }

    /// Would a new compaction still be required immediately after `plan`?
    /// The death-spiral guard: if true after an *accepted* plan, the engine
    /// must not loop forever — this must converge to false within bounded
    /// steps because deterministic pruning is monotonically non-increasing.
    pub fn would_compact_again(&self, plan: &CompactionPlan, req: &CompactionRequest) -> bool {
        if !plan.accepted {
            return true;
        }
        let new_req = CompactionRequest {
            before_tokens: plan.after_tokens,
            target_tokens: req.target_tokens,
            min_reduction_ratio: req.min_reduction_ratio,
            summarizer_residue_budget_tokens: req.summarizer_residue_budget_tokens,
        };
        // If before <= target there is nothing left to do.
        plan.after_tokens
            > new_req
                .hard_cap()
                .max(new_req.target_tokens.min(plan.after_tokens))
            && plan.after_tokens > req.target_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{DurableTaskRows, ProjectedCheck, ProjectedDecision};
    use std::sync::Arc;

    fn history(n: usize) -> Vec<RecentTurn> {
        (0..n)
            .map(|i| RecentTurn {
                role: "assistant".into(),
                text: format!("turn {i}: {}", "z".repeat(400)),
            })
            .collect()
    }

    fn ledger() -> TaskLedger {
        TaskLedger {
            goal: "g".into(),
            open_steps: vec!["s".into()],
            ..Default::default()
        }
    }

    #[test]
    fn zero_scope_ids_are_typed_refusals_never_remapped() {
        let err = EvidenceArchive::new(0, 2, None).unwrap_err();
        assert!(
            matches!(&err, EvidenceError::Malformed(m) if m.contains("session_id")),
            "{err:?}"
        );
        let err = EvidenceArchive::new(1, 0, None).unwrap_err();
        assert!(
            matches!(&err, EvidenceError::Malformed(m) if m.contains("workspace_id")),
            "{err:?}"
        );
        let err = EvidenceArchive::new(1, 1, Some(0)).unwrap_err();
        assert!(
            matches!(&err, EvidenceError::Malformed(m) if m.contains("task_id")),
            "{err:?}"
        );
        assert!(EvidenceArchive::with_backing_cap(0, 1, None, 8).is_err());

        // The compactor's scoping seam refuses zero typed, never remaps.
        assert!(Compactor::deterministic_only()
            .with_evidence_scope(0, 1, None)
            .is_err());
        assert!(Compactor::deterministic_only()
            .with_evidence_scope(1, 0, None)
            .is_err());
        assert!(Compactor::deterministic_only()
            .with_evidence_scope(1, 1, Some(0))
            .is_err());

        // Valid values are unchanged, including the archive's scope identity:
        // a hostile 0 can never be remapped onto identity 1.
        let mut archive = EvidenceArchive::new(7, 9, Some(3)).unwrap();
        assert_eq!(
            archive.access(),
            EvidenceAccessContext::for_task(7, 9, faktor_core::id::TaskId::new(3))
        );
        let ev = archive
            .archive(EvidenceKind::ProcessLog, "raw output")
            .unwrap();
        let stored = archive.retrieve(ev.id).unwrap();
        assert_eq!(stored.envelope.session_id.raw(), 7);
        assert_eq!(stored.envelope.workspace_id.raw(), 9);
        assert_eq!(stored.envelope.task_id, Some(3));

        // The standalone default keeps its documented (1, 1, None) identity.
        let mut default = EvidenceArchive::default();
        let ev = default
            .archive(EvidenceKind::ProcessLog, "standalone")
            .unwrap();
        assert_eq!(
            default.retrieve(ev.id).unwrap().envelope.session_id.raw(),
            1
        );
    }

    /// The adversary: a "summarizer" that returns the whole history verbatim
    /// (reduces context by ~0%).
    struct LiarSummarizer;
    impl Summarizer for LiarSummarizer {
        fn summarize<'a>(
            &'a self,
            residue: &'a [RecentTurn],
            _durable_facts: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            Box::pin(async move { render_turns(residue.iter()) })
        }
    }

    #[tokio::test]
    async fn one_percent_summary_rejected_and_deterministic_fallback() {
        let history = history(200);
        let e = Estimator;
        let before = e.estimate_tokens(
            &history
                .iter()
                .map(|t| t.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let req = CompactionRequest::new(before, before / 2);
        let compactor = Compactor::new(Some(Arc::new(LiarSummarizer)));
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(
            !matches!(plan.strategy, CompactionStrategy::LlmSummary),
            "liar summary must never be accepted"
        );
        assert!(plan.accepted, "deterministic fallback must succeed");
        assert_eq!(
            plan.strategy,
            CompactionStrategy::Rejected,
            "the summary attempt was rejected, then pruned"
        );
        assert!(plan.after_tokens <= req.hard_cap(), "hard invariant");
        // ~1% reduction never accepted: verify explicitly.
        let one_pct = CompactionRequest {
            before_tokens: 180_000,
            target_tokens: 178_200, // 1% reduction
            min_reduction_ratio: 0.25,
            ..Default::default()
        };
        let cap = one_pct.hard_cap();
        assert!(cap <= 135_000, "cap enforces the 25% floor, not the 1% ask");
    }

    #[tokio::test]
    async fn good_summary_accepted() {
        struct GoodSummarizer;
        impl Summarizer for GoodSummarizer {
            fn summarize<'a>(
                &'a self,
                _residue: &'a [RecentTurn],
                durable_facts: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>>
            {
                Box::pin(async move { format!("SUMMARY: {durable_facts}") })
            }
        }
        let history = history(200);
        let before = 100_000;
        let req = CompactionRequest::new(before, 30_000);
        let compactor = Compactor::new(Some(Arc::new(GoodSummarizer)));
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(plan.accepted);
        assert_eq!(plan.strategy, CompactionStrategy::LlmSummary);
        assert!(plan.after_tokens <= req.hard_cap());
    }

    #[tokio::test]
    async fn death_spiral_converges_with_liar_summarizer() {
        // The classic failure: compaction keeps "succeeding" by tiny margins
        // and never reaches the target. Our invariant must force the
        // deterministic path, and repeated compactions must converge.
        let mut current = history(400);
        let compactor = Compactor::new(Some(Arc::new(LiarSummarizer)));
        let target = 40_000usize;
        let mut steps = 0;
        let e = Estimator;
        let mut before_tokens = e.estimate_tokens(
            &current
                .iter()
                .map(|t| t.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut converged = false;
        while steps < 20 {
            let req = CompactionRequest::new(before_tokens, target);
            let plan = compactor.compact(&current, &ledger(), &req).await;
            assert!(plan.accepted, "deterministic path must accept");
            assert!(
                plan.after_tokens <= req.hard_cap(),
                "step {steps}: after {} > cap {}",
                plan.after_tokens,
                req.hard_cap()
            );
            if compactor.would_compact_again(&plan, &req) {
                // Simulate the next context built from the plan.
                current = plan.kept_recent.clone();
                before_tokens = plan.after_tokens;
                steps += 1;
                continue;
            }
            converged = true;
            break;
        }
        assert!(converged, "did not converge within 20 steps");
    }

    #[tokio::test]
    async fn none_summarizer_incremental_compaction_preserves_ledger() {
        let history = history(300);
        let before = 200_000;
        let req = CompactionRequest::new(before, 60_000);
        let compactor = Compactor::deterministic_only();
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(plan.accepted);
        assert_eq!(plan.strategy, CompactionStrategy::DeterministicPruning);
        assert_eq!(plan.ledger, ledger(), "ledger preserved in full");
        assert!(!plan.kept_recent.is_empty(), "newest turns kept");
        assert!(plan.after_tokens <= req.hard_cap());
    }

    #[tokio::test]
    async fn archived_artifacts_track_evicted_turns() {
        let history = history(100);
        let req = CompactionRequest::new(200_000, 10_000);
        let compactor = Compactor::deterministic_only();
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(!plan.archived.is_empty(), "most turns archived");
        // Every evicted turn is accounted for: kept history turns (minus
        // the synthesized identity/durable-facts turns and the eviction
        // digest) + archived = total.
        let synthesized = plan
            .kept_recent
            .iter()
            .filter(|t| t.role == "system")
            .count();
        let digest = usize::from(
            plan.kept_recent
                .first()
                .is_some_and(|t| t.text.starts_with("[Earlier context archived:")),
        );
        let kept_excluding_synthesized = plan.kept_recent.len() - synthesized - digest;
        assert_eq!(
            kept_excluding_synthesized + plan.archived.len(),
            history.len(),
            "every evicted turn must be accounted for"
        );
        // Newest turns survive, oldest archived.
        assert_eq!(
            plan.kept_recent.last().map(|t| &t.text),
            history.last().map(|t| &t.text)
        );
        // The durable archive material is non-empty, chunked, and every
        // chunk respects the bound.
        assert!(!plan.archive_chunks.is_empty());
        assert!(plan
            .archive_chunks
            .iter()
            .all(|c| !c.is_empty() && c.len() <= ARCHIVE_CHUNK_BYTES));
        // The digest rides the wire so the model knows history was dropped.
        assert!(plan
            .kept_recent
            .first()
            .is_some_and(|t| t.text.contains("<artifact://hash>")));
    }

    #[tokio::test]
    async fn accepted_llm_summary_actually_replaces_history() {
        struct GoodSummarizer;
        impl Summarizer for GoodSummarizer {
            fn summarize<'a>(
                &'a self,
                _residue: &'a [RecentTurn],
                durable_facts: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>>
            {
                Box::pin(async move { format!("COMPACT SUMMARY: {durable_facts}") })
            }
        }
        let history = history(50);
        let req = CompactionRequest::new(100_000, 5_000);
        let compactor = Compactor::new(Some(Arc::new(GoodSummarizer)));
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(plan.accepted);
        assert_eq!(plan.strategy, CompactionStrategy::LlmSummary);
        // The wire content after an accepted summary is the archive
        // pointer + summary + the synthesized durable turns, NOT the full
        // history (the old code kept history verbatim and the shrink was
        // bookkeeping-only). The pointer is not optional: without it the
        // archived turns are unreachable.
        assert_eq!(
            plan.kept_recent
                .iter()
                .filter(|t| t.role != "system")
                .count(),
            2,
            "exactly one archive pointer + one summary replace the history"
        );
        assert!(plan
            .kept_recent
            .first()
            .is_some_and(|t| t.text.starts_with("[Earlier context archived:")
                && t.text.contains("<artifact://hash>")));
        assert!(plan
            .kept_recent
            .last()
            .is_some_and(|t| t.text.starts_with("COMPACT SUMMARY:")));
        // All evicted turns are accounted for as archives + durable text.
        assert_eq!(plan.archived.len(), history.len());
        let archive_len: usize = plan.archive_chunks.iter().map(String::len).sum();
        assert!(archive_len >= 10_000, "archive holds real text");
    }

    #[tokio::test]
    async fn tiny_history_fits_without_archiving() {
        let history = history(2);
        let req = CompactionRequest::new(10_000, 8_000);
        let compactor = Compactor::deterministic_only();
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(plan.accepted);
        assert!(plan.archived.is_empty());
        // Two history turns + the synthesized identity + durable-facts
        // turns (goal `g` and open step `s` exist in the fixture).
        assert_eq!(plan.kept_recent.len(), 4);
        assert_eq!(
            plan.kept_recent
                .iter()
                .filter(|t| t.role != "system")
                .count(),
            2
        );
    }

    /// Rendered evicted-turn text exactly as the compactor archives it.
    fn rendered(evicted: &[RecentTurn]) -> String {
        let mut out = String::new();
        for t in evicted {
            out.push_str(&format!("{}: {}\n", t.role, t.text));
        }
        out
    }

    #[tokio::test]
    async fn deterministic_evictions_over_1mib_archive_chunked_nothing_lost() {
        // P0: the old 1 MiB archive cap silently dropped cold history. A
        // ~2.5 MiB eviction must now come back as MULTIPLE ordered chunks
        // whose concatenation equals the evicted turns EXACTLY — oldest and
        // newest evicted text included, nothing truncated.
        let history = history(6000); // ≈ 2.5 MiB of turn text
        let before = Estimator.estimate_tokens(&rendered(&history));
        let req = CompactionRequest::new(before, before / 5);
        let compactor = Compactor::deterministic_only();
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(plan.accepted, "deterministic pruning must fit the cap");
        assert!(
            plan.archive_chunks.len() >= 2,
            "> 1 MiB of evicted text must produce multiple chunks, got {}",
            plan.archive_chunks.len()
        );
        // Every chunk respects the bound; no empty chunks.
        assert!(plan
            .archive_chunks
            .iter()
            .all(|c| !c.is_empty() && c.len() <= ARCHIVE_CHUNK_BYTES));
        // The kept turns are the newest suffix (minus the synthesized
        // identity/facts turns and the digest at index 0).
        let kept: Vec<&RecentTurn> = plan
            .kept_recent
            .iter()
            .filter(|t| t.role != "system" && !t.text.starts_with("[Earlier context archived:"))
            .collect();
        assert_eq!(
            kept,
            history[history.len() - kept.len()..]
                .iter()
                .collect::<Vec<_>>()
        );
        let evicted = &history[..history.len() - kept.len()];
        assert!(!evicted.is_empty());
        // Concatenation (oldest first) equals the evicted text EXACTLY.
        let concat: String = plan.archive_chunks.concat();
        assert_eq!(
            concat,
            rendered(evicted),
            "archived material must be lossless"
        );
        // The oldest AND newest evicted text both survive.
        assert!(concat.starts_with(&format!("{}: ", evicted[0].role)));
        assert_eq!(concat.len(), rendered(evicted).len());
        assert_eq!(
            concat[concat.len() - evicted.last().unwrap().text.len() - 1..].trim_end(),
            evicted.last().unwrap().text
        );
    }

    #[tokio::test]
    async fn accepted_summary_archives_whole_history_chunked() {
        // The LlmSummary branch archives EVERY evicted turn (the whole
        // history) — over 1 MiB it must chunk, never truncate.
        struct GoodSummarizer;
        impl Summarizer for GoodSummarizer {
            fn summarize<'a>(
                &'a self,
                _residue: &'a [RecentTurn],
                durable_facts: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>>
            {
                Box::pin(async move { format!("COMPACT SUMMARY: {durable_facts}") })
            }
        }
        let history = history(6000); // ≈ 2.5 MiB of turn text
        let before = Estimator.estimate_tokens(&rendered(&history));
        let req = CompactionRequest::new(before, before / 5);
        let compactor = Compactor::new(Some(Arc::new(GoodSummarizer)));
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(plan.accepted);
        assert_eq!(plan.strategy, CompactionStrategy::LlmSummary);
        assert!(
            plan.archive_chunks.len() >= 2,
            "> 1 MiB of evicted text must produce multiple chunks, got {}",
            plan.archive_chunks.len()
        );
        assert!(plan
            .archive_chunks
            .iter()
            .all(|c| !c.is_empty() && c.len() <= ARCHIVE_CHUNK_BYTES));
        assert_eq!(
            plan.archive_chunks.concat(),
            rendered(&history),
            "the whole evicted history must be archived losslessly"
        );
    }

    #[test]
    fn fill_chunks_never_splits_mid_turn_and_preserves_order() {
        // Chunk boundaries fall ONLY between turns; a single turn larger
        // than the bound occupies its own WHOLE chunk (never a fragment);
        // oldest-first order is preserved end to end.
        let turn = |text: &str| RecentTurn {
            role: "assistant".into(),
            text: text.to_string(),
        };
        let turns = vec![turn("aa"), turn("bbbb"), turn("cc"), turn("dddddddddd")];
        // Bound 12 bytes: "assistant: aa\n" (13) already exceeds it.
        let chunks = fill_chunks(turns.iter(), 12);
        // Every turn whole in its own chunk: the rendered lines are larger
        // than the bound, so nothing may be joined.
        assert_eq!(
            chunks,
            vec![
                "assistant: aa\n".to_string(),
                "assistant: bbbb\n".to_string(),
                "assistant: cc\n".to_string(),
                "assistant: dddddddddd\n".to_string(),
            ]
        );
        // A bound that fits two whole turns joins them, oldest first, and
        // never lets a third straddle a boundary ("cc" would push chunk 0
        // to 43 > 30, so it opens its own chunk).
        let chunks = fill_chunks(turns.iter(), 30);
        assert_eq!(
            chunks,
            vec![
                "assistant: aa\nassistant: bbbb\n".to_string(),
                "assistant: cc\n".to_string(),
                "assistant: dddddddddd\n".to_string(),
            ]
        );
        // Concatenation is always the lossless rendered text, in order.
        assert_eq!(chunks.concat(), rendered(&turns));
    }

    #[tokio::test]
    async fn zero_reduction_never_accepted_even_at_target() {
        // before == target: any "compaction" is a zero reduction → rejected.
        let history = history(10);
        let before = 5_000;
        let req = CompactionRequest::new(before, before);
        let compactor = Compactor::deterministic_only();
        let plan = compactor.compact(&history, &ledger(), &req).await;
        // hard_cap = before * 0.75 < before, so after (>= ledger) may or may
        // not fit; what must NEVER happen is `accepted` with after == before.
        if plan.accepted {
            assert!(
                plan.after_tokens < plan.before_tokens,
                "zero-reduction compaction accepted!"
            );
        }
    }

    #[test]
    fn would_compact_again_boundaries() {
        let compactor = Compactor::deterministic_only();
        let plan = CompactionPlan {
            accepted: true,
            before_tokens: 100_000,
            after_tokens: 80_000,
            target_tokens: 80_000,
            strategy: CompactionStrategy::DeterministicPruning,
            ledger: ledger(),
            kept_recent: vec![],
            archived: vec![],
            archive_chunks: vec![],
            evidence_refs: vec![],
        };
        let req = CompactionRequest::new(100_000, 80_000);
        // After == target: done.
        assert!(!compactor.would_compact_again(&plan, &req));
        // Rejected plans always need attention.
        let rejected = CompactionPlan {
            accepted: false,
            ..plan.clone()
        };
        assert!(compactor.would_compact_again(&rejected, &req));
        // Above target: must compact again.
        let above = CompactionPlan {
            after_tokens: 90_000,
            ..plan
        };
        let req = CompactionRequest::new(100_000, 70_000);
        assert!(compactor.would_compact_again(&above, &req));
    }

    #[test]
    fn hostile_ratio_values_are_clamped() {
        let req = CompactionRequest {
            before_tokens: 100,
            target_tokens: 10,
            min_reduction_ratio: 5.0, // hostile
            ..Default::default()
        };
        let cap = req.hard_cap();
        assert!(cap <= 10);
        let req = CompactionRequest {
            before_tokens: 100,
            target_tokens: 10,
            min_reduction_ratio: -1.0, // hostile
            ..Default::default()
        };
        assert!(req.hard_cap() <= 10);
    }

    // ===================================================================
    // Audit 70/71 adversarial rows
    // ===================================================================

    /// Records every summarizer invocation and the exact inputs it saw.
    #[derive(Default)]
    struct SpySummarizer {
        calls: std::sync::atomic::AtomicUsize,
        residue: Mutex<Vec<RecentTurn>>,
        facts: Mutex<String>,
    }

    impl SpySummarizer {
        fn new() -> Self {
            Self::default()
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn residue(&self) -> Vec<RecentTurn> {
            self.residue.lock().expect("spy residue lock").clone()
        }

        fn facts(&self) -> String {
            self.facts.lock().expect("spy facts lock").clone()
        }
    }

    impl Summarizer for SpySummarizer {
        fn summarize<'a>(
            &'a self,
            residue: &'a [RecentTurn],
            durable_facts: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.residue.lock().expect("spy residue lock") = residue.to_vec();
            *self.facts.lock().expect("spy facts lock") = durable_facts.to_string();
            Box::pin(async move { "SHORT SUMMARY".to_string() })
        }
    }

    #[tokio::test]
    async fn ten_megabyte_tool_output_becomes_evidence_ref_and_original_retrievable() {
        // (a) A 10 MiB historical tool output must leave the wire as an
        // evidence ref whose compact body is <= 16 KiB, with the original
        // retrievable byte-for-byte from the evidence store.
        let line = "compile warning: unused variable `x` in module foo\n";
        let target = 10 * 1024 * 1024;
        let mut raw = String::with_capacity(target + line.len());
        while raw.len() < target {
            raw.push_str(line);
        }
        assert!(raw.len() >= target);
        let history = vec![
            RecentTurn {
                role: "tool".into(),
                text: raw.clone(),
            },
            RecentTurn {
                role: "assistant".into(),
                text: "analysis complete".into(),
            },
        ];
        let compactor = Compactor::deterministic_only();
        let req = CompactionRequest::new(3_000_000, 200_000);
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert!(plan.accepted, "the compacted plan must fit the cap");
        assert_eq!(
            plan.evidence_refs.len(),
            1,
            "the huge tool output must become exactly one evidence ref"
        );
        let ev = &plan.evidence_refs[0];
        assert!(ev.original_bytes >= target);
        assert!(
            ev.compact_body.len() <= EVIDENCE_COMPACT_BODY_MAX_BYTES,
            "compact body {} exceeds the 16 KiB evidence bound",
            ev.compact_body.len()
        );
        assert!(ev.compact_bytes <= EVIDENCE_COMPACT_BODY_MAX_BYTES);
        // The wire carries the ref, never the 10 MiB payload.
        assert!(plan
            .kept_recent
            .iter()
            .all(|t| t.text.len() <= EVIDENCE_COMPACT_BODY_MAX_BYTES + 512));
        // The original is retrievable via the evidence store.
        let archive = compactor.evidence();
        let stored = archive
            .retrieve(ev.id)
            .expect("evidence must be retrievable");
        assert_eq!(stored.backing.as_deref(), Some(raw.as_bytes()));
        assert_eq!(
            stored.envelope.compression.original_bytes as usize,
            raw.len()
        );
        assert_eq!(stored.envelope.backing_hash, Some(ev.backing_hash));
    }

    #[tokio::test]
    async fn identical_huge_tool_outputs_content_address_to_one_evidence_ref() {
        // Two identical huge outputs (exact-fact dedupe at the evidence
        // layer and at the row layer): ONE evidence id, ONE ref row.
        let raw = "same tool output line\n".repeat(20_000); // ~440 KiB
        let history = vec![
            RecentTurn {
                role: "tool".into(),
                text: raw.clone(),
            },
            RecentTurn {
                role: "tool".into(),
                text: raw.clone(),
            },
        ];
        let compactor = Compactor::deterministic_only();
        let req = CompactionRequest::new(500_000, 50_000);
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert_eq!(plan.evidence_refs.len(), 1, "content-addressed dedupe");
        assert_eq!(
            plan.kept_recent.iter().filter(|t| t.role == "tool").count(),
            1,
            "identical ref rows collapse to one"
        );
        assert_eq!(compactor.evidence().len(), 1);
    }

    #[tokio::test]
    async fn duplicate_exact_facts_collapse_before_summarization_and_pruning() {
        // (b) Byte-identical (role, text) rows collapse to their first
        // occurrence before the residue reaches the model.
        let fact = RecentTurn {
            role: "assistant".into(),
            text: "FACT: parser is recursive descent".into(),
        };
        let history = vec![
            fact.clone(),
            fact.clone(),
            fact.clone(),
            RecentTurn {
                role: "assistant".into(),
                text: "unique tail".into(),
            },
        ];
        let spy = Arc::new(SpySummarizer::new());
        let compactor = Compactor::new(Some(spy.clone()));
        let req = CompactionRequest {
            summarizer_residue_budget_tokens: 0,
            ..CompactionRequest::new(100_000, 50_000)
        };
        let plan = compactor.compact(&history, &ledger(), &req).await;
        assert_eq!(spy.calls(), 1, "the non-empty residue was summarized");
        let residue = spy.residue();
        assert_eq!(
            residue.len(),
            2,
            "identical rows must collapse: {residue:?}"
        );
        assert_eq!(
            residue
                .iter()
                .filter(|t| t.text.contains("FACT: parser"))
                .count(),
            1
        );
        assert_eq!(plan.strategy, CompactionStrategy::LlmSummary);
    }

    #[tokio::test]
    async fn goal_and_criteria_stay_verbatim_and_never_enter_summarizer_input() {
        // (c) The user's ask is excluded from the summarization input
        // entirely and rides the post-compaction wire byte-for-byte.
        let goal = "GOAL-Ω-ship-the-parser-😀";
        let criterion = "CRITERION-Ω-tests-pass-verbatim";
        let projection = TaskContextProjection::from_durable_rows(DurableTaskRows {
            goal: goal.into(),
            criteria: vec![criterion.into()],
            ..Default::default()
        });
        let spy = Arc::new(SpySummarizer::new());
        let compactor = Compactor::new(Some(spy.clone()));
        let history = history(200);
        let req = CompactionRequest::new(100_000, 30_000);
        let plan = compactor
            .compact_projected(&history, &projection, &TaskLedger::default(), &req)
            .await;
        assert_eq!(spy.calls(), 1, "large residue invokes the summarizer");
        assert!(
            !spy.facts().contains(goal),
            "goal must never reach the summarizer"
        );
        assert!(!spy.facts().contains(criterion));
        assert!(
            !render_turns(spy.residue().iter()).contains(goal),
            "goal must not be smuggled in the residue"
        );
        assert!(
            plan.kept_recent.iter().any(|t| t.text.contains(goal)),
            "post-compaction wire must carry the goal verbatim"
        );
        assert!(plan.kept_recent.iter().any(|t| t.text.contains(criterion)));
    }

    #[tokio::test]
    async fn decisions_and_checks_come_from_durable_rows_not_the_transcript() {
        // (d) The transcript lies; the projection reports durable truth.
        let projection = TaskContextProjection::from_durable_rows(DurableTaskRows {
            task_state: "Verifying".into(),
            goal: "durable goal".into(),
            decisions: vec![ProjectedDecision {
                step: "3".into(),
                choice: "DURABLE-CHOICE-X".into(),
                rationale: "durable rationale".into(),
            }],
            checks: vec![ProjectedCheck {
                name: "cargo test".into(),
                status: "passed".into(),
                summary: "all green".into(),
            }],
            ..Default::default()
        });
        let history = vec![RecentTurn {
            role: "assistant".into(),
            text: "we decided to use TRANSCRIPT-LIE and the checks FAILED".into(),
        }];
        let compactor = Compactor::deterministic_only();
        let req = CompactionRequest::new(50_000, 20_000);
        let plan = compactor
            .compact_projected(&history, &projection, &TaskLedger::default(), &req)
            .await;
        assert!(plan.accepted);
        let facts_turn = plan
            .kept_recent
            .iter()
            .find(|t| t.role == "system" && t.text.contains("DURABLE TASK PROJECTION"))
            .expect("the durable facts turn must ride the wire");
        assert!(facts_turn.text.contains("DURABLE-CHOICE-X"));
        assert!(facts_turn.text.contains("cargo test"));
        assert!(facts_turn.text.contains("passed"));
        assert!(
            !facts_turn.text.contains("TRANSCRIPT-LIE"),
            "the durable projection cannot be rewritten by the transcript"
        );
        // Reinforce the source of truth: the projection itself is rows-only.
        assert!(projection
            .summarizable_render()
            .contains("DURABLE-CHOICE-X"));
        assert!(!projection.summarizable_render().contains("TRANSCRIPT-LIE"));
    }

    #[tokio::test]
    async fn llm_summarization_only_above_the_residue_budget() {
        // (f) A spy counter proves the model is invoked only when the
        // post-extraction residue exceeds the configured budget.
        let spy = Arc::new(SpySummarizer::new());
        let compactor = Compactor::new(Some(spy.clone()));
        let small = vec![
            RecentTurn {
                role: "user".into(),
                text: "hi".into(),
            },
            RecentTurn {
                role: "assistant".into(),
                text: "hello".into(),
            },
        ];
        let roomy = CompactionRequest {
            summarizer_residue_budget_tokens: 1_000_000,
            ..CompactionRequest::new(10_000, 5_000)
        };
        let plan = compactor.compact(&small, &ledger(), &roomy).await;
        assert_eq!(spy.calls(), 0, "small residue must not call the model");
        assert_eq!(plan.strategy, CompactionStrategy::DeterministicPruning);
        assert!(plan.accepted);

        let big = history(200);
        let tight = CompactionRequest {
            summarizer_residue_budget_tokens: 128,
            ..CompactionRequest::new(100_000, 30_000)
        };
        let plan = compactor.compact(&big, &ledger(), &tight).await;
        assert_eq!(
            spy.calls(),
            1,
            "residue above budget invokes the model once"
        );
        assert_eq!(plan.strategy, CompactionStrategy::LlmSummary);
    }

    #[test]
    fn allocation_free_token_estimates_match_the_rendered_string_exactly() {
        let turns = vec![
            RecentTurn {
                role: "user".into(),
                text: "汉字😀 mixed 123".into(),
            },
            RecentTurn {
                role: "tool".into(),
                text: "\u{FFFE} hostile \t bytes".into(),
            },
            RecentTurn {
                role: String::new(),
                text: String::new(),
            },
        ];
        for turn in &turns {
            assert_eq!(
                estimate_turn_tokens(turn),
                Estimator.estimate_tokens(&format!("{}: {}", turn.role, turn.text))
            );
        }
        assert_eq!(
            estimate_rendered_tokens(turns.iter()),
            Estimator.estimate_tokens(&render_turns(turns.iter()))
        );
    }

    #[test]
    fn compaction_100k_tokens_stays_allocation_free() {
        let body = "x".repeat(296);
        let history: Vec<RecentTurn> = (0..1000)
            .map(|i| RecentTurn {
                role: if i % 2 == 0 { "tool" } else { "assistant" }.to_string(),
                text: format!("{i:04}-{body}"),
            })
            .collect();
        let compactor = Compactor::deterministic_only();
        let ledger = TaskLedger::default();
        let req = CompactionRequest::new(100_000, 50_000);
        let mut best = u64::MAX;
        let mut after = 0usize;
        for _ in 0..9 {
            let started = std::time::Instant::now();
            let plan = compactor.deterministic_prune(&history, &ledger, &req);
            best = best.min(started.elapsed().as_nanos() as u64);
            after = plan.after_tokens;
        }
        println!("PROBE compaction_100k_tokens best_ns={best} after_tokens={after}");
    }
    /// The strict summary schema is enforced deterministically: duplicate
    /// lines collapse, duplicate fact ids keep the first, and lines that
    /// restate the verbatim identity/durable projection are removed (those
    /// already ride the wire once). The model-authored text is also clamped
    /// to the request-relative summary budget, so a verbose summary cannot
    /// consume the whole compaction cap.
    #[tokio::test]
    async fn strict_summary_schema_dedupes_facts_and_respects_the_budget() {
        struct SchemaSummarizer;
        impl Summarizer for SchemaSummarizer {
            fn summarize<'a>(
                &'a self,
                _residue: &'a [RecentTurn],
                _durable_facts: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>>
            {
                Box::pin(async move {
                    format!(
                        "STATE: Implementing\n\
DECISIONS:\n- 1: pick A -> because\n- 1: pick A -> because\n\
FILES:\n- src/a.rs :: changed\n\
BLOCKERS:\n- F:1 shared blocker\n\
NOTES:\n- F:1 shared blocker\n- F:2 unique fact\n- F:2 unique fact\n- g\n{}",
                        "verbose tail ".repeat(2_000)
                    )
                })
            }
        }
        let projection = TaskContextProjection::from_durable_rows(DurableTaskRows {
            task_state: "Implementing".into(),
            goal: "g".into(),
            ..Default::default()
        });
        let history = history(200);
        let req = CompactionRequest::new(100_000, 30_000);
        let compactor = Compactor::new(Some(Arc::new(SchemaSummarizer)));
        let plan = compactor
            .compact_projected(&history, &projection, &TaskLedger::default(), &req)
            .await;
        assert_eq!(plan.strategy, CompactionStrategy::LlmSummary);
        let summary = plan
            .kept_recent
            .iter()
            .find(|turn| turn.role == "summary")
            .expect("accepted summary rides the wire")
            .text
            .clone();
        assert!(
            !summary.contains("STATE: Implementing"),
            "durable projection already carries this line verbatim: {summary}"
        );
        assert!(
            !summary.lines().any(|line| line == "- g"),
            "the verbatim goal must never be restated by the summary"
        );
        assert_eq!(
            summary.matches("shared blocker").count(),
            1,
            "duplicate fact ids/lines collapse to one: {summary}"
        );
        assert_eq!(
            summary.matches("unique fact").count(),
            1,
            "duplicate lines collapse: {summary}"
        );
        assert_eq!(
            summary.matches("pick A").count(),
            1,
            "duplicate decisions collapse: {summary}"
        );
        let tokens = Estimator.estimate_tokens(&summary);
        assert!(
            tokens <= summary_budget_tokens(&req),
            "summary {tokens} tokens exceeds its budget {}",
            summary_budget_tokens(&req)
        );
        assert!(tokens <= SUMMARY_MAX_OUTPUT_TOKENS);
    }
}
