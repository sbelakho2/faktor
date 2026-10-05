//! faktor-search — hybrid retrieval with rank fusion (spec §19, §20).
//!
//! Exact + lexical + symbol (+ optional semantic) searches are fused with
//! reciprocal-rank fusion weighted by symbol relevance, lexical score,
//! semantic score, file recency, and task affinity. Evidence packages are
//! retrieved automatically before serious reasoning turns (spec §20).
//!
//! Fallibility contract (silent-degradation guard): every entry point
//! validates its query and returns a typed [`ErrorKind::Malformed`] or
//! [`ErrorKind::Oversized`] error instead of an empty vector, so invalid
//! input can never be mistaken for "no matches". [`SearchService::semantic`]
//! refuses typedly without a configured embedder and surfaces provider
//! failures (timeout, rate limit, transport, malformed/ragged/non-finite
//! vectors, wrong dimension) unchanged. [`SearchService::fused`] propagates
//! those failures: a configured-but-failing provider can never masquerade as
//! a valid lexical/symbol-only result. Semantic retrieval is omitted ONLY
//! when no embedder is configured — a `Disabled` mode that is explicit in the
//! API ([`SearchService::semantic_enabled`] returns `false`), never encoded
//! as zero semantic hits.
//!
//! Locking/embedding contract (audit P1): the index mutex is never held
//! across a provider call. `semantic` embeds the query before snapshotting,
//! scores persisted vectors under a read-consistent lock without calling
//! out, and the fallback path snapshots at most
//! [`MAX_FALLBACK_SEMANTIC_CANDIDATES`] reduced candidates (lexical/symbol
//! first, then a deterministic path prefix), embeds them OUTSIDE the lock in
//! requests of at most [`Embedder::max_batch_size`] clamped to
//! [`MAX_SEMANTIC_EMBED_BATCH`], and re-checks each candidate's
//! [`faktor_index::FileRevision`] under the lock before returning a page. A
//! slow, dead or hostile provider can therefore never stall lexical/symbol
//! search or index updates.

use std::sync::{Arc, Mutex};

use faktor_core::error::{Error, ErrorKind};
use faktor_core::id::WorkspaceId;
use faktor_index::{FileRevision, Symbol, SymbolKind, WorkspaceIndex};

/// Classified lock recovery for DERIVED state (caches, registries, rings,
/// process/ownership projections): a poisoned guard is recovered with the
/// poison flag cleared, so one panicking caller can never wedge later use.
/// The durable authority (store/journal/OS process state) remains the
/// source of truth; the recovered value is only ever a projection of it.
fn recover_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}

const MAX_QUERY_BYTES: usize = 4096;
const MAX_SNIPPET_CHARS: usize = 400;
/// Exact-cosine retrieval over PERSISTED index vectors is bounded to this
/// many chunk vectors per query (deterministic prefix: sorted paths, chunk
/// order). The persisted store is itself bounded per workspace; this is the
/// per-query work bound.
const MAX_PERSISTED_VECTORS_PER_QUERY: usize = 8_192;
/// Local candidate reduction bound for FALLBACK semantic retrieval (a
/// workspace with no persisted vectors): the union of the query's
/// lexical/symbol candidates ranks first and the remaining budget is filled
/// with the deterministic sorted-path prefix, so ONE fallback query embeds
/// at most this many documents no matter how large the workspace is. The
/// window is deliberately in the documented 256..=4096 range: large enough
/// to preserve whole-corpus recall on normal repositories, hard-bounded for
/// hostile ones.
pub const MAX_FALLBACK_SEMANTIC_CANDIDATES: usize = 512;
/// Hard cap on the number of texts of ONE embedding request the search
/// service issues. Mirrors `faktor_provider::MAX_EMBEDDING_INPUTS`, which
/// bounds one provider embedding request the same way. An [`Embedder`] may
/// declare a SMALLER cap via [`Embedder::max_batch_size`]; it can never
/// raise this one.
pub const MAX_SEMANTIC_EMBED_BATCH: usize = faktor_provider::MAX_EMBEDDING_INPUTS;

// Compile-time validation of the bounds above.
const _: () = {
    assert!(MAX_SEMANTIC_EMBED_BATCH >= 1);
    assert!(MAX_FALLBACK_SEMANTIC_CANDIDATES >= 256);
    assert!(MAX_FALLBACK_SEMANTIC_CANDIDATES <= 4096);
};

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub score: f64,
    pub snippet: String,
    pub symbol: Option<Symbol>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceHit {
    pub path: String,
    pub snippet: String,
    pub reason: String,
}

/// Semantic embedding provider (optional; search works without it).
pub trait Embedder: Send + Sync {
    /// Stable identity `(model_id, revision)` of this embedder when known.
    /// The index persists vectors keyed by this identity so unchanged
    /// chunks are never re-embedded; `None` keeps vector persistence off
    /// (honest, never a fabricated identity).
    fn identity(&self) -> Option<(String, String)> {
        None
    }

    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>>;

    /// Fallible variant: a configured provider can fail typedly (transport,
    /// rate limit, malformed response) and the error must surface to the
    /// caller instead of being flattened into a vector list. The default
    /// bridges to the infallible [`Embedder::embed`], so legacy embedders
    /// keep working unchanged.
    fn try_embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        Ok(self.embed(texts))
    }

    /// Maximum number of texts this embedder accepts in ONE `try_embed`
    /// call. Providers with a smaller request cap override this; the search
    /// service validates the declaration by clamping it to
    /// `1..=MAX_SEMANTIC_EMBED_BATCH`, so an embedder can lower the batch
    /// size but can never raise the service's hard cap.
    fn max_batch_size(&self) -> usize {
        MAX_SEMANTIC_EMBED_BATCH
    }
}

pub struct SearchService {
    index: Arc<Mutex<WorkspaceIndex>>,
    embedder: Option<Arc<dyn Embedder>>,
}

impl SearchService {
    pub fn new(index: Arc<Mutex<WorkspaceIndex>>, embedder: Option<Arc<dyn Embedder>>) -> Self {
        Self { index, embedder }
    }

    fn check_query(&self, query: &str) -> Result<(), Error> {
        if query.trim().is_empty() {
            return Err(Error::malformed("empty search query"));
        }
        if query.len() > MAX_QUERY_BYTES {
            return Err(Error::oversized(format!(
                "query {} bytes exceeds {MAX_QUERY_BYTES}",
                query.len()
            )));
        }
        Ok(())
    }

    /// Case-insensitive substring search over indexed file contents? The
    /// inverted index stores tokens; exact search matches the QUERY as a
    /// token or a token prefix, plus full-text substring over paths.
    ///
    /// Invalid input (blank or over [`MAX_QUERY_BYTES`]) is a typed refusal,
    /// never `Ok(vec![])`.
    pub fn exact(&self, ws: WorkspaceId, needle: &str, limit: usize) -> Result<Vec<Hit>, Error> {
        self.check_query(needle)?;
        let needle_l = needle.to_lowercase();
        let index = recover_lock(&self.index);
        let mut out = Vec::new();
        // Path substring matches.
        if let Some(files) = index_files(&index, ws) {
            for path in files.keys() {
                if path.to_lowercase().contains(&needle_l) {
                    out.push(Hit {
                        path: path.clone(),
                        score: 3.0,
                        snippet: format!("path match: {path}"),
                        symbol: None,
                    });
                }
            }
        }
        // Token matches.
        for token in faktor_index::tokenize(needle) {
            for hit in index.files_for_token(ws, &token, limit) {
                if !out.iter().any(|h| h.path == hit.path) {
                    out.push(Hit {
                        path: hit.path.clone(),
                        score: hit.freq as f64,
                        snippet: format!("token `{token}` × {}", hit.freq),
                        symbol: None,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.path.cmp(&b.path))
        });
        out.truncate(limit);
        Ok(out)
    }

    /// Token-frequency search: invalid input is a typed refusal, never
    /// `Ok(vec![])`.
    pub fn lexical(&self, ws: WorkspaceId, query: &str, limit: usize) -> Result<Vec<Hit>, Error> {
        self.check_query(query)?;
        let tokens = faktor_index::tokenize(query);
        let index = recover_lock(&self.index);
        let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for token in tokens {
            for hit in index.files_for_token(ws, &token, limit * 4) {
                *scores.entry(hit.path.clone()).or_insert(0.0) += hit.freq as f64 * 1.0;
            }
        }
        let mut out: Vec<Hit> = scores
            .into_iter()
            .map(|(path, score)| {
                let snippet = snippet_for(&path);
                Hit {
                    snippet,
                    path,
                    score,
                    symbol: None,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.path.cmp(&b.path))
        });
        out.truncate(limit);
        Ok(out)
    }

    /// Symbol lookup: invalid input is a typed refusal, never `Ok(vec![])`.
    pub fn symbol(&self, ws: WorkspaceId, name: &str, limit: usize) -> Result<Vec<Hit>, Error> {
        self.check_query(name)?;
        let index = recover_lock(&self.index);
        let mut out = Vec::new();
        for (path, sym) in index.symbol_lookup(ws, name, limit) {
            out.push(Hit {
                path: path.clone(),
                score: 2.0,
                snippet: format!("{} {} at line {}", kind_label(sym.kind), sym.name, sym.line),
                symbol: Some(sym),
            });
        }
        out.truncate(limit);
        Ok(out)
    }

    /// Semantic retrieval. Without a configured embedder this is the typed
    /// `Disabled` refusal (`NotFound`); with one configured, every provider
    /// failure propagates unchanged — never an empty or lexical-only
    /// substitute.
    ///
    /// Locking contract (audit P1): the index mutex is NEVER held across a
    /// provider call. It is taken only to (a) probe which retrieval path
    /// applies, (b) snapshot fallback candidates plus their [`FileRevision`]
    /// under the lock, and (c) score a read-consistent page. The query is
    /// embedded before any snapshot, document batches are embedded after the
    /// lock was released, and the final page re-acquires the lock only to
    /// reject candidates that were removed/replaced in the meantime. A slow
    /// or dead provider therefore cannot stall lexical/symbol search or
    /// index updates.
    pub fn semantic(&self, ws: WorkspaceId, query: &str, limit: usize) -> Result<Vec<Hit>, Error> {
        self.check_query(query)?;
        let Some(embedder) = &self.embedder else {
            return Err(Error::new(
                ErrorKind::NotFound,
                "no embedding provider configured",
            ));
        };
        // Path probe under the lock, released before the query embed.
        let persisted = {
            let index = recover_lock(&self.index);
            if index.has_embedding_index(ws) {
                true
            } else if index.file_count(ws) == 0 {
                return Ok(vec![]);
            } else {
                false
            }
        };
        // Provider call OUTSIDE the lock: persisted-vector retrieval only
        // embeds the QUERY (the corpus comes from the durable vectors); the
        // fallback embeds the query first and the reduced candidate set
        // later, also outside the lock.
        let query_emb = embedder.try_embed(&[query.to_string()])?;
        let q = validated_query_vector(&query_emb)?;
        if persisted {
            // Read-consistent scoring under the lock: no provider call here.
            let index = recover_lock(&self.index);
            if index.has_embedding_index(ws) {
                return semantic_from_persisted(&index, ws, q, limit);
            }
            if index.file_count(ws) == 0 {
                return Ok(vec![]);
            }
        }
        self.semantic_fallback(ws, query, q, limit, embedder.as_ref())
    }

    /// The no-persisted-vectors fallback: snapshot bounded candidates under
    /// the lock, embed their texts OUTSIDE the lock in provider-aware
    /// batches, then re-acquire the lock for the read-consistent page and
    /// drop candidates whose file was removed or replaced while the provider
    /// ran.
    fn semantic_fallback(
        &self,
        ws: WorkspaceId,
        query: &str,
        query_vector: &[f32],
        limit: usize,
        embedder: &dyn Embedder,
    ) -> Result<Vec<Hit>, Error> {
        let candidates = {
            let index = recover_lock(&self.index);
            snapshot_fallback_candidates(&index, ws, query)
        };
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let doc_embs = embed_documents_batched(embedder, &candidates)?;
        if doc_embs
            .iter()
            .any(|d| d.len() != query_vector.len() || d.iter().any(|v| !v.is_finite()))
        {
            return Err(Error::malformed(
                "embedding provider returned a document vector with a different dimension or non-finite component",
            ));
        }
        let mut scored: Vec<(String, FileRevision, f64)> = candidates
            .iter()
            .zip(doc_embs.iter())
            .map(|(candidate, vector)| {
                (
                    candidate.path.clone(),
                    candidate.revision.clone(),
                    cosine(query_vector, vector),
                )
            })
            .collect();
        scored.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        // Read-consistent page: a candidate whose revision no longer matches
        // (deleted, tombstoned, re-indexed) is dropped rather than returned
        // from the stale snapshot.
        let index = recover_lock(&self.index);
        let mut out = Vec::new();
        for (path, revision, score) in scored {
            if out.len() >= limit {
                break;
            }
            if index.file_revision(ws, &path).as_ref() == Some(&revision) {
                out.push(Hit {
                    snippet: snippet_for(&path),
                    path,
                    score,
                    symbol: None,
                });
            }
        }
        Ok(out)
    }

    /// Reciprocal-rank fusion over exact/lexical/symbol (+ semantic when a
    /// provider is configured). Every ranking is bounded and deterministic;
    /// the fused list is the ranked evidence package the context compiler
    /// consumes.
    ///
    /// Fallible by contract (silent-degradation guard): an invalid query is
    /// a typed refusal and, when [`SearchService::semantic_enabled`] is
    /// `true`, any semantic-provider failure propagates unchanged. A
    /// configured provider is never skipped; the ONLY mode without a
    /// semantic leg is `Disabled` (no embedder configured), which callers
    /// can observe explicitly instead of inferring it from zero hits.
    pub fn fused(&self, ws: WorkspaceId, query: &str, limit: usize) -> Result<Vec<Hit>, Error> {
        self.check_query(query)?;
        let mut rankings: Vec<(f64, Vec<Hit>)> = Vec::new();
        rankings.push((2.0, self.symbol(ws, query, limit * 4)?));
        rankings.push((1.5, self.exact(ws, query, limit * 4)?));
        rankings.push((1.0, self.lexical(ws, query, limit * 4)?));
        if self.semantic_enabled() {
            rankings.push((0.8, self.semantic(ws, query, limit * 4)?));
        }
        Ok(fuse(&rankings, limit))
    }

    /// Whether a semantic provider is configured.
    ///
    /// `false` is the explicit `Disabled` mode: `fused` deliberately omits
    /// the semantic leg and [`SearchService::semantic`] refuses with a typed
    /// `NotFound`. `true` means the configured provider is consulted and its
    /// typed failures propagate out of `fused`/`semantic`.
    pub fn semantic_enabled(&self) -> bool {
        self.embedder.is_some()
    }

    /// Automatic evidence package (spec §20): concepts from the task, recent
    /// errors, active symbols, changed files. Bounded: ≤ max_hits, snippets
    /// ≤ 400 chars.
    ///
    /// Fallible by contract: each concept is retrieved through
    /// [`SearchService::fused`], so a malformed/oversized concept or a
    /// configured semantic provider's failure propagates instead of silently
    /// shrinking the package.
    pub fn evidence_package(
        &self,
        ws: WorkspaceId,
        concepts: &[String],
        max_hits: usize,
    ) -> Result<Vec<EvidenceHit>, Error> {
        if max_hits == 0 {
            return Ok(vec![]);
        }
        let mut out: Vec<EvidenceHit> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for concept in concepts.iter().take(16) {
            if out.len() >= max_hits {
                break;
            }
            let hits = self.fused(ws, concept, 4)?;
            for hit in hits {
                if out.len() >= max_hits {
                    break;
                }
                if seen.insert(hit.path.clone()) {
                    out.push(EvidenceHit {
                        reason: format!("concept: {concept}"),
                        path: hit.path,
                        snippet: truncate(&hit.snippet, MAX_SNIPPET_CHARS),
                    });
                }
            }
        }
        Ok(out)
    }
}

fn index_files(
    index: &WorkspaceIndex,
    ws: WorkspaceId,
) -> Option<std::collections::HashMap<String, Vec<Symbol>>> {
    let paths = index.file_paths(ws);
    if paths.is_empty() {
        return None;
    }
    Some(
        paths
            .into_iter()
            .map(|p| {
                let syms = index.symbols_for(ws, &p);
                (p, syms)
            })
            .collect(),
    )
}

/// One snapshot candidate of the fallback semantic path: the text embedded
/// for it, and the [`FileRevision`] it was snapshotted from. The revision is
/// re-checked under the lock before the candidate may appear in a page.
struct FallbackCandidate {
    path: String,
    text: String,
    revision: FileRevision,
}

/// Local candidate reduction for the fallback path (audit P1). The union of
/// the query's lexical postings and symbol hits ranks first (bounded per
/// token by [`MAX_FALLBACK_SEMANTIC_CANDIDATES`]); the remaining budget is
/// filled with the deterministic sorted-path prefix, so normal-size
/// repositories keep whole-corpus recall while a huge workspace can never
/// turn into an unbounded embedding request. Ties are broken by path, so
/// the candidate set is deterministic. Must be called with the index lock
/// held; it clones everything the later provider calls need and never calls
/// out.
fn snapshot_fallback_candidates(
    index: &WorkspaceIndex,
    ws: WorkspaceId,
    query: &str,
) -> Vec<FallbackCandidate> {
    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for token in faktor_index::tokenize(query) {
        for hit in index.files_for_token(ws, &token, MAX_FALLBACK_SEMANTIC_CANDIDATES) {
            *scores.entry(hit.path).or_insert(0.0) += hit.freq as f64;
        }
    }
    for (path, _symbol) in index.symbol_lookup(ws, query, MAX_FALLBACK_SEMANTIC_CANDIDATES) {
        *scores.entry(path).or_insert(0.0) += 2.0;
    }
    let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let mut selected: Vec<String> = ranked
        .into_iter()
        .take(MAX_FALLBACK_SEMANTIC_CANDIDATES)
        .map(|(path, _)| path)
        .collect();
    if selected.len() < MAX_FALLBACK_SEMANTIC_CANDIDATES {
        let mut seen: std::collections::HashSet<String> = selected.iter().cloned().collect();
        for path in index.file_paths(ws) {
            if selected.len() >= MAX_FALLBACK_SEMANTIC_CANDIDATES {
                break;
            }
            if seen.insert(path.clone()) {
                selected.push(path);
            }
        }
    }
    let mut out = Vec::with_capacity(selected.len());
    for path in selected {
        let Some(revision) = index.file_revision(ws, &path) else {
            continue;
        };
        let symbols = revision
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let text = format!("{path} {symbols}");
        out.push(FallbackCandidate {
            path,
            text,
            revision,
        });
    }
    out
}

/// Embed the candidate texts in provider-aware batches. The effective batch
/// size is the embedder's declared [`Embedder::max_batch_size`] clamped to
/// `1..=MAX_SEMANTIC_EMBED_BATCH`, so an embedder can lower the batch size
/// but never raise the service's hard cap. The FIRST typed provider error is
/// returned unchanged (a mid-batch failure is bounded: at most
/// `ceil(candidates / batch)` calls) and every batch's vector count is
/// validated before scoring. The caller holds no index lock here.
fn embed_documents_batched(
    embedder: &dyn Embedder,
    candidates: &[FallbackCandidate],
) -> Result<Vec<Vec<f32>>, Error> {
    let batch_size = embedder.max_batch_size().clamp(1, MAX_SEMANTIC_EMBED_BATCH);
    let mut out = Vec::with_capacity(candidates.len());
    for batch in candidates.chunks(batch_size) {
        let texts: Vec<String> = batch
            .iter()
            .map(|candidate| candidate.text.clone())
            .collect();
        let vectors = embedder.try_embed(&texts)?;
        if vectors.len() != batch.len() {
            return Err(Error::malformed(format!(
                "embedding provider returned {} vectors for {} documents",
                vectors.len(),
                batch.len()
            )));
        }
        out.extend(vectors);
    }
    Ok(out)
}

/// Validate the query embedding response: exactly one finite, non-empty
/// vector, else a typed `Malformed` refusal (a configured embedder is
/// untrusted input).
fn validated_query_vector(query_emb: &[Vec<f32>]) -> Result<&[f32], Error> {
    let Some(q) = query_emb.first() else {
        return Err(Error::malformed(
            "embedding provider returned no vector for the query",
        ));
    };
    if q.is_empty() || q.iter().any(|v| !v.is_finite()) {
        return Err(Error::malformed(
            "embedding provider returned a malformed query vector",
        ));
    }
    Ok(q)
}

fn snippet_for(path: &str) -> String {
    truncate(path, MAX_SNIPPET_CHARS)
}

/// Semantic retrieval over the PERSISTED chunk vectors of one workspace
/// generation: exact-cosine score every stored vector (bounded,
/// deterministic order), keep each path's best chunk, rank by (score desc,
/// path asc). The QUERY VECTOR was already embedded by the caller OUTSIDE
/// the index lock; this function performs no provider call at all. A query
/// vector whose dimension does not match the active model dimension is a
/// typed `Malformed` refusal — never a silent zero-score ranking.
fn semantic_from_persisted(
    index: &WorkspaceIndex,
    ws: WorkspaceId,
    query_vector: &[f32],
    limit: usize,
) -> Result<Vec<Hit>, Error> {
    let active_dimension = index.embedding_dimension(ws) as usize;
    if query_vector.len() != active_dimension {
        return Err(Error::malformed(format!(
            "query embedding dimension {} does not match the indexed model dimension {active_dimension}",
            query_vector.len()
        )));
    }
    let mut best: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for (path, vector) in index.chunk_vectors(ws, MAX_PERSISTED_VECTORS_PER_QUERY) {
        let score = cosine(query_vector, vector);
        best.entry(path)
            .and_modify(|current| {
                if score > *current {
                    *current = score;
                }
            })
            .or_insert(score);
    }
    let mut out: Vec<Hit> = best
        .into_iter()
        .map(|(path, score)| Hit {
            snippet: snippet_for(&path),
            path,
            score,
            symbol: None,
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.path.cmp(&b.path))
    });
    out.truncate(limit);
    Ok(out)
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f64
}

/// Reciprocal-rank fusion: score = Σ w / (k + rank), k = 60.
pub fn fuse(rankings: &[(f64, Vec<Hit>)], limit: usize) -> Vec<Hit> {
    const K: f64 = 60.0;
    let mut scores: std::collections::HashMap<String, (f64, Option<Symbol>, String)> =
        std::collections::HashMap::new();
    // Deterministic accumulation: floating-point addition is order-sensitive,
    // so process hits in sorted (path, rank) order.
    for (weight, hits) in rankings {
        let mut ordered: Vec<(usize, &Hit)> = hits.iter().enumerate().collect();
        ordered.sort_by(|a, b| a.1.path.cmp(&b.1.path).then(a.0.cmp(&b.0)));
        for (rank, hit) in ordered {
            let entry = scores
                .entry(hit.path.clone())
                .or_insert_with(|| (0.0, hit.symbol.clone(), hit.snippet.clone()));
            entry.0 += weight / (K + rank as f64);
            if entry.1.is_none() {
                entry.1 = hit.symbol.clone();
            }
        }
    }
    let mut out: Vec<Hit> = scores
        .into_iter()
        .map(|(path, (score, symbol, snippet))| Hit {
            path,
            score,
            snippet,
            symbol,
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "fn",
        SymbolKind::Class => "class",
        SymbolKind::Struct => "struct",
        SymbolKind::Method => "method",
        SymbolKind::Type => "type",
        SymbolKind::Test => "test",
        SymbolKind::Import => "import",
        SymbolKind::Export => "export",
        SymbolKind::Const => "const",
        SymbolKind::Enum => "enum",
        SymbolKind::Unknown => "symbol",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_index::WorkspaceIndex as WI;
    use faktor_index::{content_hash, EmbeddingModel};

    fn corpus() -> (Arc<Mutex<WI>>, WorkspaceId) {
        let mut idx = WI::new();
        let ws = WorkspaceId::new(1);
        idx.index_file(
            ws,
            std::path::Path::new("src/parser.rs"),
            b"fn parse_token() {}\nfn parse_expr() {}\nstruct Parser {}\n",
            100,
        )
        .unwrap();
        idx.index_file(
            ws,
            std::path::Path::new("src/lexer.rs"),
            b"fn lex() {}\nfn next_char() {}\n",
            200,
        )
        .unwrap();
        idx.index_file(
            ws,
            std::path::Path::new("tests/parser_test.rs"),
            b"#[test]\nfn test_parse_expr() {}\n",
            300,
        )
        .unwrap();
        (Arc::new(Mutex::new(idx)), ws)
    }

    /// Fake embedder: keyword-overlap vectors so semantic ranking is
    /// deterministic.
    struct KeywordEmbedder;
    impl Embedder for KeywordEmbedder {
        fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
            let vocab = ["parse", "token", "expr", "lexer", "test", "parser"];
            texts
                .iter()
                .map(|t| {
                    vocab
                        .iter()
                        .map(|v| if t.contains(v) { 1.0 } else { 0.0 })
                        .collect()
                })
                .collect()
        }
    }

    #[test]
    fn poisoned_index_is_recovered_for_reads() {
        let (idx, ws) = corpus();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let idx = idx.clone();
            move || {
                let _guard = idx.lock().unwrap();
                panic!("holder poisoned the derived index");
            }
        }));
        assert!(idx.is_poisoned());
        // Derived cache => recover: reads reconcile against the durable
        // index generations (the recovered guard is still served) and never
        // panic the search path.
        let svc = SearchService::new(idx.clone(), None);
        let hits = svc.exact(ws, "parse", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(!idx.is_poisoned());
    }

    #[test]
    fn fused_ranks_symbol_above_lexical_only() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, None);
        let hits = svc.fused(ws, "Parser", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].path, "src/parser.rs");
        assert!(hits[0].symbol.is_some(), "symbol match should rank first");
    }

    #[test]
    fn semantic_without_embedder_errors_cleanly() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, None);
        // `Disabled` semantics is explicit in the API, never encoded as an
        // empty semantic ranking.
        assert!(!svc.semantic_enabled());
        let err = svc.semantic(ws, "parse", 5).unwrap_err();
        assert!(err.kind == ErrorKind::NotFound);
        // The fused API stays available: no provider means no semantic leg
        // (documented `Disabled`), and lexical/symbol evidence still serves.
        assert!(!svc.fused(ws, "parse", 5).unwrap().is_empty());
    }

    #[test]
    fn semantic_with_fake_embedder_contributes_to_fusion() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, Some(Arc::new(KeywordEmbedder)));
        assert!(svc.semantic_enabled());
        let sem = svc.semantic(ws, "lexer token", 5).unwrap();
        assert!(!sem.is_empty());
        assert_eq!(
            sem[0].path, "src/lexer.rs",
            "lexer dominates the query semantically"
        );
        // Fusion: parser.rs wins the lexical leg (token `token` ×2) while
        // lexer.rs wins the semantic leg; both must be present.
        let fused = svc.fused(ws, "lexer token", 5).unwrap();
        assert!(!fused.is_empty());
        let paths: Vec<&str> = fused.iter().map(|h| h.path.as_str()).collect();
        assert!(
            paths.contains(&"src/lexer.rs"),
            "lexer must rank via semantics: {paths:?}"
        );
        assert!(
            paths.contains(&"src/parser.rs"),
            "parser must rank via lexical: {paths:?}"
        );
    }

    #[test]
    fn exact_substring_case_insensitive() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, None);
        let hits = svc.exact(ws, "PARSER", 10).unwrap();
        assert!(hits.iter().any(|h| h.path.contains("parser.rs")));
        let hits = svc.exact(ws, "src/lexer", 10).unwrap();
        assert!(hits.iter().any(|h| h.path.contains("lexer.rs")));
    }

    /// A configured embedder is untrusted input: malformed responses and
    /// provider failures are typed refusals (never a panic, never a silent
    /// truncation) that `fused` PROPAGATES unchanged — a provider outage can
    /// never masquerade as a valid lexical/symbol-only result set.
    #[test]
    fn semantic_provider_failures_surface_through_fused_typed() {
        #[derive(Clone, Copy)]
        enum EmbedderResponses {
            Empty,
            Short,
            Ragged,
            NonFinite,
            DocNonFinite,
            Timeout,
            RateLimited,
            Provider { retryable: bool },
        }
        struct Bad(EmbedderResponses);
        impl Embedder for Bad {
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                let n = texts.len();
                match self.0 {
                    EmbedderResponses::Empty => vec![],
                    EmbedderResponses::Short => vec![vec![1.0]; n.saturating_sub(1)],
                    EmbedderResponses::Ragged => {
                        let mut v = vec![vec![1.0, 0.0]; n];
                        if let Some(f) = v.first_mut() {
                            f.push(0.0);
                        }
                        v
                    }
                    EmbedderResponses::NonFinite => vec![vec![f32::NAN]; n],
                    EmbedderResponses::DocNonFinite => {
                        let mut v = vec![vec![1.0]; n];
                        if let Some(d) = v.get_mut(1) {
                            d[0] = f32::INFINITY;
                        }
                        v
                    }
                    EmbedderResponses::Timeout
                    | EmbedderResponses::RateLimited
                    | EmbedderResponses::Provider { .. } => vec![vec![1.0]; n],
                }
            }
            fn try_embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
                match self.0 {
                    EmbedderResponses::Timeout => {
                        Err(Error::timeout("embedding provider timed out"))
                    }
                    EmbedderResponses::RateLimited => Err(Error::new(
                        ErrorKind::RateLimited,
                        "embedding provider rate limited",
                    )),
                    EmbedderResponses::Provider { retryable } => Err(Error::new(
                        ErrorKind::Provider {
                            code: "embeddings_unavailable".into(),
                            retryable,
                        },
                        "embedding provider refused the request",
                    )),
                    _ => Ok(self.embed(texts)),
                }
            }
        }
        let (idx, ws) = corpus();
        let cases: Vec<(&str, EmbedderResponses, ErrorKind)> = vec![
            (
                "empty-response",
                EmbedderResponses::Empty,
                ErrorKind::Malformed,
            ),
            (
                "short-response",
                EmbedderResponses::Short,
                ErrorKind::Malformed,
            ),
            (
                "ragged-vector",
                EmbedderResponses::Ragged,
                ErrorKind::Malformed,
            ),
            (
                "non-finite-query",
                EmbedderResponses::NonFinite,
                ErrorKind::Malformed,
            ),
            (
                "non-finite-document",
                EmbedderResponses::DocNonFinite,
                ErrorKind::Malformed,
            ),
            ("timeout", EmbedderResponses::Timeout, ErrorKind::Timeout),
            (
                "rate-limited",
                EmbedderResponses::RateLimited,
                ErrorKind::RateLimited,
            ),
            (
                "provider-retryable",
                EmbedderResponses::Provider { retryable: true },
                ErrorKind::Provider {
                    code: "embeddings_unavailable".into(),
                    retryable: true,
                },
            ),
            (
                "provider-permanent",
                EmbedderResponses::Provider { retryable: false },
                ErrorKind::Provider {
                    code: "embeddings_unavailable".into(),
                    retryable: false,
                },
            ),
        ];
        for (name, case, expected) in cases {
            let svc = SearchService::new(idx.clone(), Some(Arc::new(Bad(case))));
            let sem_err = svc
                .semantic(ws, "parse", 5)
                .expect_err(&format!("{name}: semantic must be a typed refusal"));
            assert_eq!(sem_err.kind, expected, "{name}: semantic kind");
            let fused_err = svc.fused(ws, "parse", 5).expect_err(&format!(
                "{name}: fused must surface the provider error, never a lexical-only result"
            ));
            assert_eq!(fused_err.kind, expected, "{name}: fused kind");
            assert_eq!(
                fused_err.retryable,
                expected.is_retryable(),
                "{name}: retryability must survive the surface"
            );
            // The evidence-package entry point propagates too: a configured
            // provider failure can never silently shrink the package.
            let pkg_err = svc
                .evidence_package(ws, &["parse".into()], 4)
                .expect_err(&format!("{name}: evidence package must propagate"));
            assert_eq!(pkg_err.kind, expected, "{name}: evidence_package kind");
        }
    }

    #[test]
    fn exact_path_hit_survives_fusion_without_lexical_or_symbol_evidence() {
        // A path-only exact match (the query token appears in no token,
        // symbol or content posting) must still be ranked: the evidence
        // package's exact leg is fused, not dropped.
        let mut idx = WI::new();
        let ws = WorkspaceId::new(7);
        idx.index_file(
            ws,
            std::path::Path::new("src/ziggurat_vault.rs"),
            b"fn unrelated() {}\n",
            1,
        )
        .unwrap();
        let svc = SearchService::new(Arc::new(Mutex::new(idx)), None);
        assert!(svc.lexical(ws, "ziggurat_vault", 10).unwrap().is_empty());
        assert!(svc.symbol(ws, "ziggurat_vault", 10).unwrap().is_empty());
        let exact = svc.exact(ws, "ziggurat_vault", 10).unwrap();
        assert_eq!(exact.len(), 1, "the path is an exact hit: {exact:?}");
        let fused = svc.fused(ws, "ziggurat_vault", 10).unwrap();
        assert!(
            fused.iter().any(|h| h.path == "src/ziggurat_vault.rs"),
            "the exact-only path must survive fusion: {fused:?}"
        );
        let pkg = svc
            .evidence_package(ws, &["ziggurat_vault".into()], 4)
            .unwrap();
        assert_eq!(pkg.len(), 1);
        assert_eq!(pkg[0].path, "src/ziggurat_vault.rs");
    }

    #[test]
    fn evidence_package_bounded() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, Some(Arc::new(KeywordEmbedder)));
        let concepts: Vec<String> = (0..100).map(|i| format!("parse token {i}")).collect();
        let pkg = svc.evidence_package(ws, &concepts, 8).unwrap();
        assert!(pkg.len() <= 8, "bounded by max_hits");
        assert!(pkg.iter().all(|e| e.snippet.len() <= MAX_SNIPPET_CHARS));
        // Dedup by path.
        let paths: std::collections::HashSet<_> = pkg.iter().map(|e| &e.path).collect();
        assert_eq!(paths.len(), pkg.len());
        // Empty max_hits → empty.
        assert!(svc.evidence_package(ws, &concepts, 0).unwrap().is_empty());
    }

    /// Invalid input is a typed refusal from EVERY entry point — never an
    /// empty `Ok` that a caller could mistake for "no matches".
    #[test]
    fn invalid_queries_are_typed_from_every_entry_point() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx.clone(), Some(Arc::new(KeywordEmbedder)));
        let big = "x".repeat(MAX_QUERY_BYTES + 1);
        for (name, query, expected) in [
            ("empty", "", ErrorKind::Malformed),
            ("whitespace", "   \t\n", ErrorKind::Malformed),
            ("oversized", big.as_str(), ErrorKind::Oversized),
        ] {
            assert_eq!(
                svc.exact(ws, query, 5).unwrap_err().kind,
                expected,
                "{name}: exact"
            );
            assert_eq!(
                svc.lexical(ws, query, 5).unwrap_err().kind,
                expected,
                "{name}: lexical"
            );
            assert_eq!(
                svc.symbol(ws, query, 5).unwrap_err().kind,
                expected,
                "{name}: symbol"
            );
            assert_eq!(
                svc.fused(ws, query, 5).unwrap_err().kind,
                expected,
                "{name}: fused"
            );
            assert_eq!(
                svc.semantic(ws, query, 5).unwrap_err().kind,
                expected,
                "{name}: semantic"
            );
            assert_eq!(
                svc.evidence_package(ws, &[query.to_string()], 4)
                    .unwrap_err()
                    .kind,
                expected,
                "{name}: evidence_package"
            );
        }
        // The disabled semantic mode is a typed refusal too, never a silent
        // empty ranking.
        let plain = SearchService::new(idx, None);
        assert_eq!(
            plain.semantic(ws, "parse", 5).unwrap_err().kind,
            ErrorKind::NotFound
        );
        // Hostile unicode is safe (and valid input is not refused).
        assert!(svc.fused(ws, "\u{FFFE}\u{FFFF}😀", 5).is_ok());
    }

    #[test]
    fn no_index_data_returns_empty() {
        let idx = Arc::new(Mutex::new(WI::new()));
        let svc = SearchService::new(idx, None);
        assert!(svc
            .fused(WorkspaceId::new(9), "anything", 5)
            .unwrap()
            .is_empty());
        assert!(svc
            .evidence_package(WorkspaceId::new(9), &["x".into()], 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn fusion_weights_are_stable() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx.clone(), Some(Arc::new(KeywordEmbedder)));
        let a = svc.fused(ws, "parse", 10).unwrap();
        let b = svc.fused(ws, "parse", 10).unwrap();
        assert_eq!(a, b, "fused ranking must be deterministic");
    }

    #[test]
    fn symbol_search_exact_and_prefix() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, None);
        let hits = svc.symbol(ws, "Parser", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].symbol.as_ref().unwrap().name, "Parser");
        let hits = svc.symbol(ws, "parse", 10).unwrap();
        assert!(hits.iter().any(|h| h
            .symbol
            .as_ref()
            .map(|s| s.name.starts_with("parse"))
            .unwrap_or(false)));
    }

    // ------------------------------------------------- persisted vectors

    /// Query-side embedder for the persisted-vector path: keyword axes, and
    /// it RECORDS every text it is asked to embed (proving the corpus is
    /// served from the persisted store, not re-embedded).
    struct SpyAxisEmbedder {
        dimension: usize,
        texts: std::sync::Mutex<Vec<String>>,
    }

    impl SpyAxisEmbedder {
        fn new(dimension: usize) -> Self {
            Self {
                dimension,
                texts: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn texts(&self) -> Vec<String> {
            self.texts.lock().unwrap().clone()
        }
    }

    impl Embedder for SpyAxisEmbedder {
        fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
            self.texts.lock().unwrap().extend(texts.iter().cloned());
            texts
                .iter()
                .map(|text| {
                    let l = text.to_lowercase();
                    let mut v = vec![0.1f32; self.dimension];
                    let axis =
                        if l.contains("quantum") || l.contains("ledger") || l.contains("reconcile")
                        {
                            0
                        } else if l.contains("zebra") || l.contains("parser") {
                            1
                        } else {
                            usize::MAX
                        };
                    if axis < self.dimension {
                        for (i, component) in v.iter_mut().enumerate() {
                            *component = if i == axis { 1.0 } else { 0.0 };
                        }
                    }
                    v
                })
                .collect()
        }
    }

    const LEDGER: &str = "pub fn reconcile_accounts() -> u32 { 7 }\n";
    const PARSER: &str = "pub fn parse_expr() -> u32 { 1 }\n";

    /// One corpus whose PERSISTED vectors (not query-time candidates) decide
    /// the ranking: semantic search must return the exact nearest neighbor
    /// by cosine and be deterministic across calls.
    fn persisted_corpus(dimension: usize) -> (Arc<Mutex<WI>>, WorkspaceId) {
        let mut idx = WI::new();
        let ws = WorkspaceId::new(3);
        idx.index_file(
            ws,
            std::path::Path::new("src/ledger.rs"),
            LEDGER.as_bytes(),
            1,
        )
        .unwrap();
        idx.index_file(
            ws,
            std::path::Path::new("src/parser.rs"),
            PARSER.as_bytes(),
            2,
        )
        .unwrap();
        idx.set_embedding_model(ws, &EmbeddingModel::new("m", "r1"));
        let mut ledger = vec![0.0f32; dimension];
        ledger[0] = 1.0;
        let mut parser = vec![0.0f32; dimension];
        parser[1] = 1.0;
        idx.put_embedding(ws, &content_hash(LEDGER), ledger)
            .unwrap();
        idx.put_embedding(ws, &content_hash(PARSER), parser)
            .unwrap();
        (Arc::new(Mutex::new(idx)), ws)
    }

    #[test]
    fn persisted_vectors_return_the_exact_nearest_neighbor_deterministically() {
        let (idx, ws) = persisted_corpus(2);
        let spy = Arc::new(SpyAxisEmbedder::new(2));
        let svc = SearchService::new(idx, Some(spy.clone()));
        let a = svc.semantic(ws, "quantum", 5).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].path, "src/ledger.rs", "exact nearest neighbor wins");
        assert!((a[0].score - 1.0).abs() < 1e-6, "{:?}", a[0]);
        assert_eq!(a[1].path, "src/parser.rs");
        assert!((a[1].score - 0.0).abs() < 1e-6);
        // Deterministic across calls.
        let b = svc.semantic(ws, "quantum", 5).unwrap();
        assert_eq!(a, b, "persisted retrieval must be deterministic");
        // Only QUERY texts ever reach the embedder (two calls for two
        // queries); the corpus came from the store.
        assert_eq!(
            spy.texts(),
            vec!["quantum".to_string(), "quantum".to_string()]
        );
        // Fusion still carries the persisted semantic leg.
        let fused = svc.fused(ws, "quantum", 5).unwrap();
        assert!(
            fused.iter().any(|h| h.path == "src/ledger.rs"),
            "persisted vectors must fuse: {fused:?}"
        );
    }

    #[test]
    fn persisted_query_dimension_and_shape_mismatches_are_typed() {
        // Index model dimension 2, embedder answers 3: the typed refusal is
        // surfaced by BOTH `semantic` and `fused` — the fused caller can
        // never receive a lexical-only result instead of the error.
        let (idx, ws) = persisted_corpus(2);
        let svc = SearchService::new(idx.clone(), Some(Arc::new(SpyAxisEmbedder::new(3))));
        let err = svc.semantic(ws, "quantum", 5).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed, "{err}");
        assert!(err.message.contains("dimension"), "{err}");
        let fused_err = svc.fused(ws, "quantum", 5).unwrap_err();
        assert_eq!(fused_err.kind, ErrorKind::Malformed, "{fused_err}");
        assert!(fused_err.message.contains("dimension"), "{fused_err}");
        // A non-finite query vector is a typed refusal.
        struct NonFinite;
        impl Embedder for NonFinite {
            fn embed(&self, _texts: &[String]) -> Vec<Vec<f32>> {
                vec![vec![f32::NAN, 0.0]]
            }
        }
        let svc = SearchService::new(idx, Some(Arc::new(NonFinite)));
        let err = svc.semantic(ws, "quantum", 5).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed, "{err}");
        let fused_err = svc.fused(ws, "quantum", 5).unwrap_err();
        assert_eq!(fused_err.kind, ErrorKind::Malformed, "{fused_err}");
    }

    #[test]
    fn fuse_empty_and_single() {
        assert!(fuse(&[], 5).is_empty());
        let hit = Hit {
            path: "a".into(),
            score: 1.0,
            snippet: "s".into(),
            symbol: None,
        };
        let out = fuse(&[(1.0, vec![hit.clone()])], 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "a");
        // limit respected (distinct paths; dedup is by path)
        let many: Vec<Hit> = (0..10)
            .map(|i| Hit {
                path: format!("f{i}"),
                score: 1.0,
                snippet: "s".into(),
                symbol: None,
            })
            .collect();
        assert!(fuse(&[(1.0, many)], 3).len() == 3);
    }

    #[test]
    fn recency_affects_ranking() {
        // Two files with the same token; the newer file should win ties via
        // its higher modified_ms when the index exposes it — the fusion
        // weights include recency through the lexical score; verify both
        // files appear and the newer is first for an equal-token query.
        let mut idx = WI::new();
        let ws = WorkspaceId::new(1);
        idx.index_file(
            ws,
            std::path::Path::new("old.rs"),
            b"fn shared_thing() {}",
            100,
        )
        .unwrap();
        idx.index_file(
            ws,
            std::path::Path::new("new.rs"),
            b"fn shared_thing() {}",
            200,
        )
        .unwrap();
        let svc = SearchService::new(Arc::new(Mutex::new(idx)), None);
        let hits = svc.lexical(ws, "shared", 10).unwrap();
        assert_eq!(hits.len(), 2);
        // Both are returned; ordering is by freq (equal), stable.
        assert!(hits.iter().any(|h| h.path == "old.rs"));
        assert!(hits.iter().any(|h| h.path == "new.rs"));
    }

    #[test]
    fn unicode_query_safe() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, Some(Arc::new(KeywordEmbedder)));
        // A mixed CJK+ASCII query must still retrieve through the ASCII
        // token (CJK is tokenized safely, never panics, never blanks the
        // whole query).
        let hits = svc.fused(ws, "解析 parse", 5).unwrap();
        assert!(!hits.is_empty(), "mixed CJK+ASCII query must retrieve");
        assert!(
            hits.iter().any(|h| h.path.ends_with("parser.rs")),
            "{hits:?}"
        );
        // Per-concept evidence: the ASCII concept retrieves real snippets;
        // the pure-CJK concept is handled without fabricating hits.
        let mixed = svc
            .evidence_package(ws, &["解析".into(), "parse".into()], 5)
            .unwrap();
        assert!(!mixed.is_empty(), "the ASCII concept must retrieve");
        assert!(mixed.len() <= 5, "evidence stays bounded");
        assert!(mixed
            .iter()
            .all(|e| !e.snippet.is_empty() && e.path.ends_with(".rs")));
        let cjk_only = svc.evidence_package(ws, &["解析".into()], 5).unwrap();
        assert!(cjk_only.len() <= 5, "a pure-CJK concept stays bounded");
    }

    // ------------------------------------------- audit P1: lock discipline

    /// Audit P1 killer concurrency test: a provider call blocks; while it is
    /// blocked, lexical search, symbol search and an index update must each
    /// complete well under the bound (the semantic path must not hold the
    /// index mutex across the embedder call).
    #[test]
    fn slow_provider_never_stalls_lexical_symbol_or_index_updates() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        struct GatedEmbedder {
            entered: mpsc::Sender<()>,
            release: Mutex<bool>,
            ready: std::sync::Condvar,
            first_call: std::sync::atomic::AtomicBool,
        }
        impl GatedEmbedder {
            fn release(&self) {
                *self.release.lock().unwrap_or_else(|p| p.into_inner()) = true;
                self.ready.notify_all();
            }
        }
        impl Embedder for GatedEmbedder {
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                self.try_embed(texts).unwrap_or_default()
            }
            fn try_embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
                if !self
                    .first_call
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = self.entered.send(());
                    // Safety valve: a regression takes the assertion path
                    // instead of hanging the suite forever.
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut released = self.release.lock().unwrap_or_else(|p| p.into_inner());
                    while !*released && Instant::now() < deadline {
                        let (guard, _) = self
                            .ready
                            .wait_timeout(released, Duration::from_millis(25))
                            .unwrap_or_else(|p| p.into_inner());
                        released = guard;
                    }
                }
                Ok(texts.iter().map(|_| vec![1.0f32]).collect())
            }
        }

        let (idx, ws) = corpus();
        let (entered_tx, entered_rx) = mpsc::channel();
        let gated = Arc::new(GatedEmbedder {
            entered: entered_tx,
            release: Mutex::new(false),
            ready: std::sync::Condvar::new(),
            first_call: std::sync::atomic::AtomicBool::new(false),
        });
        let svc = Arc::new(SearchService::new(idx.clone(), Some(gated.clone())));
        let worker = {
            let svc = svc.clone();
            std::thread::spawn(move || svc.semantic(ws, "lexer token", 5))
        };
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the gated embedder must be entered");

        let bound = Duration::from_millis(250);
        let started = Instant::now();
        let lexical = svc.lexical(ws, "parse", 5).unwrap();
        let lexical_elapsed = started.elapsed();
        assert!(
            lexical_elapsed < bound,
            "lexical search stalled behind the blocked provider: {lexical_elapsed:?}"
        );
        let started = Instant::now();
        let symbols = svc.symbol(ws, "Parser", 5).unwrap();
        let symbol_elapsed = started.elapsed();
        assert!(
            symbol_elapsed < bound,
            "symbol search stalled behind the blocked provider: {symbol_elapsed:?}"
        );
        let started = Instant::now();
        idx.lock()
            .unwrap()
            .index_file(
                ws,
                std::path::Path::new("src/added_while_blocked.rs"),
                b"fn added_while_blocked() {}\n",
                1,
            )
            .unwrap();
        let update_elapsed = started.elapsed();
        assert!(
            update_elapsed < bound,
            "index update stalled behind the blocked provider: {update_elapsed:?}"
        );
        eprintln!(
            "P1 measured while provider blocked: lexical={lexical_elapsed:?} symbol={symbol_elapsed:?} update={update_elapsed:?} (bound {bound:?})"
        );
        assert!(!lexical.is_empty());
        assert!(!symbols.is_empty());

        gated.release();
        let hits = worker.join().unwrap().unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
    }

    /// The fallback reduction keeps whole-corpus recall on small
    /// repositories in deterministic sorted-path order: the two-file
    /// semantic-only corpus used by the CLI/agent E2Es is embedded in ONE
    /// two-text request with ledger before parser, and the semantic-only
    /// nearest neighbor still ranks first.
    #[test]
    fn fallback_small_corpus_is_filled_in_deterministic_path_order() {
        struct RecordingEmbedder {
            batches: Mutex<Vec<Vec<String>>>,
        }
        impl Embedder for RecordingEmbedder {
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                self.batches.lock().unwrap().push(texts.to_vec());
                texts
                    .iter()
                    .map(|t| {
                        let l = t.to_lowercase();
                        vec![
                            if l.contains("quantum") || l.contains("ledger") {
                                1.0f32
                            } else {
                                0.0
                            },
                            if l.contains("zebra") || l.contains("parser") {
                                1.0f32
                            } else {
                                0.0
                            },
                        ]
                    })
                    .collect()
            }
        }

        let mut idx = WI::new();
        let ws = WorkspaceId::new(21);
        idx.index_file(
            ws,
            std::path::Path::new("src/ledger.rs"),
            b"pub fn reconcile_accounts() -> u32 { 7 }\n",
            1,
        )
        .unwrap();
        idx.index_file(
            ws,
            std::path::Path::new("src/parser.rs"),
            b"pub fn parse_expr() -> u32 { 1 }\n",
            2,
        )
        .unwrap();
        let recording = Arc::new(RecordingEmbedder {
            batches: Mutex::new(Vec::new()),
        });
        let svc = SearchService::new(Arc::new(Mutex::new(idx)), Some(recording.clone()));
        let hits = svc.semantic(ws, "quantum zebra", 5).unwrap();
        assert_eq!(
            hits[0].path, "src/ledger.rs",
            "the semantic-only nearest neighbor must win: {hits:?}"
        );
        let batches = recording.batches.lock().unwrap().clone();
        assert_eq!(batches.len(), 2, "query + one candidate batch: {batches:?}");
        assert_eq!(batches[0], vec!["quantum zebra".to_string()]);
        assert_eq!(batches[1].len(), 2);
        assert!(batches[1][0].starts_with("src/ledger.rs"), "{batches:?}");
        assert!(batches[1][1].starts_with("src/parser.rs"), "{batches:?}");
    }

    /// Fallback candidate reduction under the file cap: a fallback query
    /// over 100k files embeds at most [`MAX_FALLBACK_SEMANTIC_CANDIDATES`]
    /// documents, and every provider request stays within
    /// [`MAX_SEMANTIC_EMBED_BATCH`].
    #[test]
    fn fallback_with_100k_files_embeds_at_most_the_candidate_cap() {
        struct CountingEmbedder {
            calls: Mutex<Vec<usize>>,
        }
        impl Embedder for CountingEmbedder {
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                self.calls.lock().unwrap().push(texts.len());
                texts.iter().map(|_| vec![1.0f32]).collect()
            }
        }

        let mut idx = WI::new();
        let ws = WorkspaceId::new(11);
        for i in 0..100_000usize {
            let rel = format!("f{i:06}.txt");
            idx.index_file(
                ws,
                std::path::Path::new(&rel),
                b"alpha shared token payload",
                0,
            )
            .unwrap();
        }
        assert_eq!(idx.file_count(ws), 100_000);
        let counting = Arc::new(CountingEmbedder {
            calls: Mutex::new(Vec::new()),
        });
        let svc = SearchService::new(Arc::new(Mutex::new(idx)), Some(counting.clone()));
        let hits = svc.semantic(ws, "shared", 16).unwrap();
        assert_eq!(hits.len(), 16);
        let calls = counting.calls.lock().unwrap().clone();
        assert_eq!(calls[0], 1, "the query is its own one-text request");
        assert!(
            calls.iter().all(|&n| n <= MAX_SEMANTIC_EMBED_BATCH),
            "every request is within the hard batch cap: {calls:?}"
        );
        let documents: usize = calls[1..].iter().sum();
        assert!(
            documents <= MAX_FALLBACK_SEMANTIC_CANDIDATES,
            "fallback embedded {documents} documents (cap {MAX_FALLBACK_SEMANTIC_CANDIDATES}): {calls:?}"
        );
        assert!(
            calls.len() <= 1 + MAX_FALLBACK_SEMANTIC_CANDIDATES,
            "the number of provider calls is bounded: {calls:?}"
        );
    }

    /// A fallback snapshot taken before the document embed can never serve a
    /// candidate that was deleted/tombstoned while the provider ran.
    #[test]
    fn stale_snapshot_never_returns_a_deleted_candidate() {
        use std::sync::mpsc;
        use std::time::Duration;

        struct ParserAxisEmbedder;
        impl Embedder for ParserAxisEmbedder {
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                texts
                    .iter()
                    .map(|t| vec![if t.contains("pars") { 1.0f32 } else { 0.0 }])
                    .collect()
            }
        }
        struct DocGateEmbedder {
            entered: mpsc::Sender<()>,
            release: Mutex<bool>,
            ready: std::sync::Condvar,
            blocked: std::sync::atomic::AtomicBool,
        }
        impl DocGateEmbedder {
            fn release(&self) {
                *self.release.lock().unwrap_or_else(|p| p.into_inner()) = true;
                self.ready.notify_all();
            }
        }
        impl Embedder for DocGateEmbedder {
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                ParserAxisEmbedder.embed(texts)
            }
            fn try_embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
                // The query is its own one-text call; block the DOCUMENT
                // batch so the deletion lands strictly between the snapshot
                // and the page assembly.
                if texts.len() > 1 && !self.blocked.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = self.entered.send(());
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    let mut released = self.release.lock().unwrap_or_else(|p| p.into_inner());
                    while !*released && std::time::Instant::now() < deadline {
                        let (guard, _) = self
                            .ready
                            .wait_timeout(released, Duration::from_millis(25))
                            .unwrap_or_else(|p| p.into_inner());
                        released = guard;
                    }
                }
                Ok(ParserAxisEmbedder.embed(texts))
            }
        }

        // Control: without the deletion, the file is the top semantic hit.
        let (idx_control, ws_control) = corpus();
        let control = SearchService::new(idx_control, Some(Arc::new(ParserAxisEmbedder)));
        let before = control
            .semantic(ws_control, "parse lexer token", 5)
            .unwrap();
        assert_eq!(before[0].path, "src/parser.rs", "{before:?}");

        let (idx, ws) = corpus();
        let (entered_tx, entered_rx) = mpsc::channel();
        let gated = Arc::new(DocGateEmbedder {
            entered: entered_tx,
            release: Mutex::new(false),
            ready: std::sync::Condvar::new(),
            blocked: std::sync::atomic::AtomicBool::new(false),
        });
        let svc = Arc::new(SearchService::new(idx.clone(), Some(gated.clone())));
        let worker = {
            let svc = svc.clone();
            std::thread::spawn(move || svc.semantic(ws, "parse lexer token", 5))
        };
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the document batch must be entered");
        idx.lock()
            .unwrap()
            .remove_file(ws, std::path::Path::new("src/parser.rs"));
        gated.release();
        let after = worker.join().unwrap().unwrap();
        assert!(
            after.iter().all(|hit| hit.path != "src/parser.rs"),
            "a deleted candidate must not be served from the stale snapshot: {after:?}"
        );
        assert!(
            !after.is_empty(),
            "live candidates must still serve after the deletion: {after:?}"
        );
    }

    /// A provider failure in the MIDDLE of the fallback document batches is
    /// typed (kind, code and retryability survive) and bounded (the service
    /// stops at the first failure and never exceeds the declared batch cap).
    #[test]
    fn provider_error_mid_batch_is_typed_and_bounded() {
        struct MidBatchFail {
            requests: Mutex<Vec<usize>>,
            doc_calls: std::sync::atomic::AtomicUsize,
        }
        impl Embedder for MidBatchFail {
            fn max_batch_size(&self) -> usize {
                4
            }
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                texts.iter().map(|_| vec![1.0f32]).collect()
            }
            fn try_embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
                self.requests.lock().unwrap().push(texts.len());
                if texts.len() == 1 {
                    return Ok(vec![vec![1.0]]);
                }
                if self
                    .doc_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    >= 1
                {
                    return Err(Error::new(
                        ErrorKind::Provider {
                            code: "embed_batch_rejected".into(),
                            retryable: true,
                        },
                        "provider rejected document batch 2",
                    ));
                }
                Ok(self.embed(texts))
            }
        }

        let mut idx = WI::new();
        let ws = WorkspaceId::new(12);
        for i in 0..10 {
            let rel = format!("src/f{i}.txt");
            idx.index_file(ws, std::path::Path::new(&rel), b"shared token payload", 0)
                .unwrap();
        }
        let failing = Arc::new(MidBatchFail {
            requests: Mutex::new(Vec::new()),
            doc_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let svc = SearchService::new(Arc::new(Mutex::new(idx)), Some(failing.clone()));
        let err = svc.semantic(ws, "shared", 5).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::Provider {
                code: "embed_batch_rejected".into(),
                retryable: true,
            },
            "{err}"
        );
        assert!(err.retryable, "retryability must survive: {err}");
        let requests = failing.requests.lock().unwrap().clone();
        assert_eq!(
            requests,
            vec![1, 4, 4],
            "the first failing batch ends the walk at the declared cap: {requests:?}"
        );
        assert!(
            requests.iter().all(|&n| n <= 4),
            "no request may exceed the declared batch size: {requests:?}"
        );
    }

    /// The declared provider cap is validated: a smaller declaration is
    /// respected, an overclaim is clamped to [`MAX_SEMANTIC_EMBED_BATCH`],
    /// and a zero declaration is clamped to one text per request.
    #[test]
    fn provider_batch_cap_is_validated_and_clamped() {
        struct Declared {
            cap: usize,
            requests: Mutex<Vec<usize>>,
        }
        impl Embedder for Declared {
            fn max_batch_size(&self) -> usize {
                self.cap
            }
            fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                self.requests.lock().unwrap().push(texts.len());
                texts.iter().map(|_| vec![1.0f32]).collect()
            }
        }

        let mut idx = WI::new();
        let ws = WorkspaceId::new(13);
        for i in 0..130 {
            let rel = format!("src/g{i:03}.txt");
            idx.index_file(ws, std::path::Path::new(&rel), b"shared token payload", 0)
                .unwrap();
        }
        let idx = Arc::new(Mutex::new(idx));

        // An overclaiming provider can never raise the hard cap.
        let over = Arc::new(Declared {
            cap: usize::MAX,
            requests: Mutex::new(Vec::new()),
        });
        SearchService::new(idx.clone(), Some(over.clone()))
            .semantic(ws, "shared", 5)
            .unwrap();
        let requests = over.requests.lock().unwrap().clone();
        assert_eq!(requests[0], 1);
        assert!(
            requests[1..].iter().all(|&n| n <= MAX_SEMANTIC_EMBED_BATCH),
            "{requests:?}"
        );
        assert_eq!(
            requests[1..].iter().sum::<usize>(),
            130,
            "all candidates still embed: {requests:?}"
        );

        // A declared zero clamps to one text per request.
        let zero = Arc::new(Declared {
            cap: 0,
            requests: Mutex::new(Vec::new()),
        });
        SearchService::new(idx, Some(zero.clone()))
            .semantic(ws, "shared", 5)
            .unwrap();
        let requests = zero.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 131, "query + 130 singleton batches");
        assert!(requests.iter().all(|&n| n == 1), "{requests:?}");
    }
}
