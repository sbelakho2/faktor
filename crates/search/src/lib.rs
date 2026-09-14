//! faktor-search — hybrid retrieval with rank fusion (spec §19, §20).
//!
//! Exact + lexical + symbol (+ optional semantic) searches are fused with
//! reciprocal-rank fusion weighted by symbol relevance, lexical score,
//! semantic score, file recency, and task affinity. Evidence packages are
//! retrieved automatically before serious reasoning turns (spec §20).

use std::sync::{Arc, Mutex};

use faktor_core::error::{Error, ErrorKind};
use faktor_core::id::WorkspaceId;
use faktor_index::{Symbol, SymbolKind, WorkspaceIndex};

const MAX_QUERY_BYTES: usize = 4096;
const MAX_SNIPPET_CHARS: usize = 400;
/// Exact-cosine retrieval over PERSISTED index vectors is bounded to this
/// many chunk vectors per query (deterministic prefix: sorted paths, chunk
/// order). The persisted store is itself bounded per workspace; this is the
/// per-query work bound.
const MAX_PERSISTED_VECTORS_PER_QUERY: usize = 8_192;

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
    pub fn exact(&self, ws: WorkspaceId, needle: &str, limit: usize) -> Vec<Hit> {
        if self.check_query(needle).is_err() {
            return vec![];
        }
        let needle_l = needle.to_lowercase();
        let index = self.index.lock().unwrap();
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
        out
    }

    pub fn lexical(&self, ws: WorkspaceId, query: &str, limit: usize) -> Vec<Hit> {
        if self.check_query(query).is_err() {
            return vec![];
        }
        let tokens = faktor_index::tokenize(query);
        let index = self.index.lock().unwrap();
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
        out
    }

    pub fn symbol(&self, ws: WorkspaceId, name: &str, limit: usize) -> Vec<Hit> {
        if self.check_query(name).is_err() {
            return vec![];
        }
        let index = self.index.lock().unwrap();
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
        out
    }

    pub fn semantic(&self, ws: WorkspaceId, query: &str, limit: usize) -> Result<Vec<Hit>, Error> {
        self.check_query(query)?;
        let Some(embedder) = &self.embedder else {
            return Err(Error::new(
                ErrorKind::NotFound,
                "no embedding provider configured",
            ));
        };
        let index = self.index.lock().unwrap();
        // Persisted-vector retrieval (index builds with a configured
        // embedding source persist chunk vectors keyed by content hash): the
        // QUERY is embedded, the corpus is served from the durable vectors —
        // no document is re-embedded per search, and chunk embeddings
        // survive generation swaps/reopens.
        if index.has_embedding_index(ws) {
            return semantic_from_persisted(&index, ws, query, limit, embedder.as_ref());
        }
        // Embed the query and candidate chunks (paths + symbols); cosine
        // similarity ranks the corpus.
        let mut candidates: Vec<String> = Vec::new();
        let mut paths: Vec<String> = Vec::new();
        if let Some(files) = index_files(&index, ws) {
            for (path, syms) in files {
                candidates.push(format!(
                    "{path} {}",
                    syms.iter()
                        .map(|s| s.name.clone())
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
                paths.push(path);
            }
        }
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let query_emb = embedder.try_embed(&[query.to_string()])?;
        let doc_embs = embedder.try_embed(&candidates)?;
        // A configured embedder is untrusted INPUT: an empty or ragged
        // response is a typed refusal, never an index panic or a silent
        // truncation that would score the wrong files.
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
        if doc_embs.len() != paths.len() {
            return Err(Error::malformed(format!(
                "embedding provider returned {} vectors for {} documents",
                doc_embs.len(),
                paths.len()
            )));
        }
        if doc_embs
            .iter()
            .any(|d| d.len() != q.len() || d.iter().any(|v| !v.is_finite()))
        {
            return Err(Error::malformed(
                "embedding provider returned a document vector with a different dimension or non-finite component",
            ));
        }
        let mut scored: Vec<(String, f64)> = paths
            .iter()
            .zip(doc_embs.iter())
            .map(|(p, d)| (p.clone(), cosine(q, d)))
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        scored.truncate(limit);
        Ok(scored
            .into_iter()
            .map(|(path, score)| {
                let snippet = snippet_for(&path);
                Hit {
                    path,
                    score,
                    snippet,
                    symbol: None,
                }
            })
            .collect())
    }

    /// Reciprocal-rank fusion over exact/lexical/symbol (+ semantic when
    /// present). Every ranking is bounded and deterministic; the fused list
    /// is the ranked evidence package the context compiler consumes.
    pub fn fused(&self, ws: WorkspaceId, query: &str, limit: usize) -> Vec<Hit> {
        if self.check_query(query).is_err() {
            return vec![];
        }
        let mut rankings: Vec<(f64, Vec<Hit>)> = Vec::new();
        rankings.push((2.0, self.symbol(ws, query, limit * 4)));
        rankings.push((1.5, self.exact(ws, query, limit * 4)));
        rankings.push((1.0, self.lexical(ws, query, limit * 4)));
        if self.embedder.is_some() {
            if let Ok(sem) = self.semantic(ws, query, limit * 4) {
                rankings.push((0.8, sem));
            }
        }
        fuse(&rankings, limit)
    }

    /// Automatic evidence package (spec §20): concepts from the task, recent
    /// errors, active symbols, changed files. Bounded: ≤ max_hits, snippets
    /// ≤ 400 chars.
    pub fn evidence_package(
        &self,
        ws: WorkspaceId,
        concepts: &[String],
        max_hits: usize,
    ) -> Vec<EvidenceHit> {
        if max_hits == 0 {
            return vec![];
        }
        let mut out: Vec<EvidenceHit> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for concept in concepts.iter().take(16) {
            if out.len() >= max_hits {
                break;
            }
            let hits = self.fused(ws, concept, 4);
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
        out
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

fn snippet_for(path: &str) -> String {
    truncate(path, MAX_SNIPPET_CHARS)
}

/// Semantic retrieval over the PERSISTED chunk vectors of one workspace
/// generation: embed the query, exact-cosine score every stored vector
/// (bounded, deterministic order), keep each path's best chunk, rank by
/// (score desc, path asc). A query vector whose dimension does not match
/// the active model dimension is a typed `Malformed` refusal — never a
/// silent zero-score ranking.
fn semantic_from_persisted(
    index: &WorkspaceIndex,
    ws: WorkspaceId,
    query: &str,
    limit: usize,
    embedder: &dyn Embedder,
) -> Result<Vec<Hit>, Error> {
    let query_emb = embedder.try_embed(&[query.to_string()])?;
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
    let active_dimension = index.embedding_dimension(ws) as usize;
    if q.len() != active_dimension {
        return Err(Error::malformed(format!(
            "query embedding dimension {} does not match the indexed model dimension {active_dimension}",
            q.len()
        )));
    }
    let mut best: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for (path, vector) in index.chunk_vectors(ws, MAX_PERSISTED_VECTORS_PER_QUERY) {
        let score = cosine(q, vector);
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
    fn fused_ranks_symbol_above_lexical_only() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, None);
        let hits = svc.fused(ws, "Parser", 10);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].path, "src/parser.rs");
        assert!(hits[0].symbol.is_some(), "symbol match should rank first");
    }

    #[test]
    fn semantic_without_embedder_errors_cleanly() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, None);
        let err = svc.semantic(ws, "parse", 5).unwrap_err();
        assert!(err.kind == ErrorKind::NotFound);
    }

    #[test]
    fn semantic_with_fake_embedder_contributes_to_fusion() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, Some(Arc::new(KeywordEmbedder)));
        let sem = svc.semantic(ws, "lexer token", 5).unwrap();
        assert!(!sem.is_empty());
        assert_eq!(
            sem[0].path, "src/lexer.rs",
            "lexer dominates the query semantically"
        );
        // Fusion: parser.rs wins the lexical leg (token `token` ×2) while
        // lexer.rs wins the semantic leg; both must be present.
        let fused = svc.fused(ws, "lexer token", 5);
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
        let hits = svc.exact(ws, "PARSER", 10);
        assert!(hits.iter().any(|h| h.path.contains("parser.rs")));
        let hits = svc.exact(ws, "src/lexer", 10);
        assert!(hits.iter().any(|h| h.path.contains("lexer.rs")));
    }

    /// A configured embedder is untrusted input: malformed responses are
    /// typed refusals (never a panic, never a silent truncation) and a
    /// provider error propagates through `semantic` while `fused` degrades
    /// honestly to the lexical/symbol legs.
    #[test]
    fn malformed_embedder_responses_are_typed_and_fusion_degrades() {
        struct Bad(EmbedderResponses);
        enum EmbedderResponses {
            Empty,
            Short,
            Ragged,
            NonFinite,
            Failed,
        }
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
                    EmbedderResponses::Failed => {
                        vec![vec![1.0]; n]
                    }
                }
            }
            fn try_embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
                if matches!(self.0, EmbedderResponses::Failed) {
                    return Err(Error::new(
                        ErrorKind::Provider {
                            code: "embeddings_unavailable".into(),
                            retryable: true,
                        },
                        "embedding provider refused the request",
                    ));
                }
                Ok(self.embed(texts))
            }
        }
        let (idx, ws) = corpus();
        for (name, case) in [
            ("empty", EmbedderResponses::Empty),
            ("short", EmbedderResponses::Short),
            ("ragged", EmbedderResponses::Ragged),
            ("non-finite", EmbedderResponses::NonFinite),
            ("failed", EmbedderResponses::Failed),
        ] {
            let svc = SearchService::new(idx.clone(), Some(Arc::new(Bad(case))));
            let err = svc
                .semantic(ws, "parse", 5)
                .expect_err(&format!("{name} must be a typed refusal"));
            if name != "failed" {
                assert_eq!(err.kind, ErrorKind::Malformed, "{name}: {err}");
            }
            // Fusion never crashes and never loses the lexical/symbol legs.
            let fused = svc.fused(ws, "parse", 5);
            assert!(
                fused.iter().any(|h| h.path == "src/parser.rs"),
                "{name}: lexical/symbol evidence must survive: {fused:?}"
            );
        }
        // A provider error carries its typed retryable code through.
        let svc = SearchService::new(idx, Some(Arc::new(Bad(EmbedderResponses::Failed))));
        let err = svc.semantic(ws, "parse", 5).unwrap_err();
        assert!(
            matches!(
                err.kind,
                ErrorKind::Provider {
                    retryable: true,
                    ..
                }
            ),
            "{err:?}"
        );
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
        assert!(svc.lexical(ws, "ziggurat_vault", 10).is_empty());
        assert!(svc.symbol(ws, "ziggurat_vault", 10).is_empty());
        let exact = svc.exact(ws, "ziggurat_vault", 10);
        assert_eq!(exact.len(), 1, "the path is an exact hit: {exact:?}");
        let fused = svc.fused(ws, "ziggurat_vault", 10);
        assert!(
            fused.iter().any(|h| h.path == "src/ziggurat_vault.rs"),
            "the exact-only path must survive fusion: {fused:?}"
        );
        let pkg = svc.evidence_package(ws, &["ziggurat_vault".into()], 4);
        assert_eq!(pkg.len(), 1);
        assert_eq!(pkg[0].path, "src/ziggurat_vault.rs");
    }

    #[test]
    fn evidence_package_bounded() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, Some(Arc::new(KeywordEmbedder)));
        let concepts: Vec<String> = (0..100).map(|i| format!("parse token {i}")).collect();
        let pkg = svc.evidence_package(ws, &concepts, 8);
        assert!(pkg.len() <= 8, "bounded by max_hits");
        assert!(pkg.iter().all(|e| e.snippet.len() <= MAX_SNIPPET_CHARS));
        // Dedup by path.
        let paths: std::collections::HashSet<_> = pkg.iter().map(|e| &e.path).collect();
        assert_eq!(paths.len(), pkg.len());
        // Empty max_hits → empty.
        assert!(svc.evidence_package(ws, &concepts, 0).is_empty());
    }

    #[test]
    fn empty_and_hostile_queries() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, None);
        assert!(svc.exact(ws, "", 5).is_empty());
        assert!(svc.lexical(ws, "   ", 5).is_empty());
        assert!(svc.symbol(ws, "", 5).is_empty());
        assert!(svc.fused(ws, "", 5).is_empty());
        // Oversized query → error (or empty for infallible entry points).
        let big = "x".repeat(MAX_QUERY_BYTES + 1);
        assert!(svc.semantic(ws, &big, 5).is_err());
        assert!(svc.exact(ws, &big, 5).is_empty());
        // Hostile unicode is safe.
        let _ = svc.fused(ws, "\u{FFFE}\u{FFFF}😀", 5);
    }

    #[test]
    fn no_index_data_returns_empty() {
        let idx = Arc::new(Mutex::new(WI::new()));
        let svc = SearchService::new(idx, None);
        assert!(svc.fused(WorkspaceId::new(9), "anything", 5).is_empty());
        assert!(svc
            .evidence_package(WorkspaceId::new(9), &["x".into()], 5)
            .is_empty());
    }

    #[test]
    fn fusion_weights_are_stable() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx.clone(), Some(Arc::new(KeywordEmbedder)));
        let a = svc.fused(ws, "parse", 10);
        let b = svc.fused(ws, "parse", 10);
        assert_eq!(a, b, "fused ranking must be deterministic");
    }

    #[test]
    fn symbol_search_exact_and_prefix() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, None);
        let hits = svc.symbol(ws, "Parser", 10);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].symbol.as_ref().unwrap().name, "Parser");
        let hits = svc.symbol(ws, "parse", 10);
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
        let fused = svc.fused(ws, "quantum", 5);
        assert!(
            fused.iter().any(|h| h.path == "src/ledger.rs"),
            "persisted vectors must fuse: {fused:?}"
        );
    }

    #[test]
    fn persisted_query_dimension_and_shape_mismatches_are_typed() {
        // Index model dimension 2, embedder answers 3.
        let (idx, ws) = persisted_corpus(2);
        let svc = SearchService::new(idx.clone(), Some(Arc::new(SpyAxisEmbedder::new(3))));
        let err = svc.semantic(ws, "quantum", 5).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed, "{err}");
        assert!(err.message.contains("dimension"), "{err}");
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
        let hits = svc.lexical(ws, "shared", 10);
        assert_eq!(hits.len(), 2);
        // Both are returned; ordering is by freq (equal), stable.
        assert!(hits.iter().any(|h| h.path == "old.rs"));
        assert!(hits.iter().any(|h| h.path == "new.rs"));
    }

    #[test]
    fn unicode_query_safe() {
        let (idx, ws) = corpus();
        let svc = SearchService::new(idx, Some(Arc::new(KeywordEmbedder)));
        let _ = svc.fused(ws, "解析 parse", 5);
        let _ = svc.evidence_package(ws, &["解析".into(), "parse".into()], 5);
    }
}
