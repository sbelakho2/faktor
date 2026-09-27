//! Generation-file codec and workspace fingerprints (audits 30/64).
//!
//! A published generation is one immutable, durable file:
//!
//! ```text
//! <data_root>/generations/<workspace>/gen-<g>.json
//! ```
//!
//! The file holds the generation envelope (format tag, workspace, built
//! generation, the workspace fingerprint the build saw, and the full
//! per-workspace inverted/symbol index). Builders write to
//! `<data_root>/scratch/<workspace>/gen-<g>-<nonce>.tmp`, fsync, and a
//! single rename publishes the file — a reader holding generation `g` never
//! observes a partial `g+1`, because the file only ever appears complete.
//!
//! The envelope is written with deterministic map order (BTreeMap), so
//! re-serializing a loaded generation produces byte-identical JSON — a
//! loaded generation can be re-persisted verbatim.

use std::collections::BTreeMap;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::coverage::{FingerprintCoverage, IndexCoverage, ScanCursor};
use crate::{FileEntry, Symbol, WorkspaceIndex};

/// Format tag of a generation file; bumped on incompatible envelope shapes.
/// v2 removed the never-read per-file `tokens`/`size` members (v1 files are
/// refused loudly and rebuilt, never partially decoded).
pub const GENERATION_FILE_FORMAT: u32 = 2;

/// One entry of a workspace fingerprint: the identity of a regular file the
/// walker saw (relative path, size, mtime ms). Compared entry-wise after
/// sorting; a difference means the filesystem changed and the generation is
/// stale.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FingerprintEntry {
    pub path: String,
    pub size: u64,
    pub modified_ms: i64,
}

/// The durable generation file: everything needed to serve generation `g`
/// after a restart without rescanning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationFile {
    pub format: u32,
    pub workspace: u64,
    pub generation: u64,
    pub built_ms: i64,
    /// Filesystem fingerprint the build was scanned against.
    pub fingerprint: Vec<FingerprintEntry>,
    pub data: WorkspaceData,
    /// Durable index coverage of this generation (audit 5): what the batch
    /// walker saw/indexed and whether the generation is complete. Absent on
    /// pre-coverage envelopes -> [`IndexCoverage::legacy_unknown`] (UNKNOWN
    /// completeness, never complete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<IndexCoverage>,
    /// Continuation cursor of the CONTENT batch walk (audit 5). `Some` while
    /// coverage is incomplete; `None` once the walk exhausted the tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ScanCursor>,
    /// Durable fingerprint coverage (audit 6): shard round state; a capped
    /// fingerprint is never fully clean. Absent -> [`FingerprintCoverage::legacy_unknown`]
    /// (never fully clean); a rebuild re-establishes the baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_coverage: Option<FingerprintCoverage>,
    /// In-progress fingerprint "next" map of an unfinished round: entries
    /// observed so far. Removals materialize only when the round completes
    /// (`fingerprint` becomes this map), so a capped round never drops a
    /// suffix it has not yet walked. Tagged with `fingerprint_next_epoch`:
    /// a resumed round continues its own map, a new round starts empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fingerprint_next: Vec<FingerprintEntry>,
    /// Epoch of [`GenerationFile::fingerprint_next`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_next_epoch: Option<u64>,
}

/// Deterministically ordered per-workspace index payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceData {
    /// rel path (sorted) -> file entry.
    pub files: BTreeMap<String, StoredFile>,
    /// token -> (rel path -> freq), sorted.
    pub postings: BTreeMap<String, BTreeMap<String, u32>>,
    /// symbol name (lowercase) -> sorted hit list.
    pub symbols: BTreeMap<String, Vec<StoredSymbolHit>>,
    pub token_count: u64,
    /// Persisted chunk embeddings keyed by (content_hash, model_id,
    /// model_revision, dimension). Additive: generations written before
    /// embeddings existed decode to an empty store.
    #[serde(default)]
    pub embeddings: crate::EmbeddingIndex,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredFile {
    pub symbols: Vec<Symbol>,
    pub modified_ms: i64,
    /// Chunk content hashes (additive; see
    /// [`crate::embedding::chunk_text`]).
    #[serde(default)]
    pub chunks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSymbolHit {
    pub path: String,
    pub symbol: Symbol,
}

impl GenerationFile {
    /// Capture the per-workspace maps of `index` (which must hold exactly
    /// this workspace's data — the service's per-workspace indexes satisfy
    /// that) into a deterministic envelope.
    pub fn capture(
        workspace: u64,
        generation: u64,
        index: &WorkspaceIndex,
        fingerprint: Vec<FingerprintEntry>,
    ) -> Self {
        // Legacy/plain capture: a complete generation with no continuation
        // state (the shape pre-coverage callers and tests expect).
        Self::capture_with_coverage(
            workspace,
            generation,
            index,
            fingerprint,
            IndexCoverage::complete(),
            None,
            FingerprintCoverage::complete(),
            Vec::new(),
            None,
        )
    }

    /// [`GenerationFile::capture`] carrying the durable coverage, content
    /// continuation cursor and fingerprint shard state (audit 5/6).
    #[allow(clippy::too_many_arguments)]
    pub fn capture_with_coverage(
        workspace: u64,
        generation: u64,
        index: &WorkspaceIndex,
        fingerprint: Vec<FingerprintEntry>,
        coverage: IndexCoverage,
        cursor: Option<ScanCursor>,
        fingerprint_coverage: FingerprintCoverage,
        fingerprint_next: Vec<FingerprintEntry>,
        fingerprint_next_epoch: Option<u64>,
    ) -> Self {
        let mut files = BTreeMap::new();
        if let Some(map) = index.files.get(&crate::WorkspaceId::new(workspace)) {
            for (path, e) in map {
                files.insert(
                    path.clone(),
                    StoredFile {
                        symbols: e.symbols.clone(),
                        modified_ms: e.modified_ms,
                        chunks: e.chunks.clone(),
                    },
                );
            }
        }
        let mut postings = BTreeMap::new();
        if let Some(map) = index.postings.get(&crate::WorkspaceId::new(workspace)) {
            for (tok, m) in map {
                postings.insert(
                    tok.clone(),
                    m.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                );
            }
        }
        let mut symbols = BTreeMap::new();
        if let Some(map) = index.symbols.get(&crate::WorkspaceId::new(workspace)) {
            for (name, hits) in map {
                let mut list: Vec<StoredSymbolHit> = hits
                    .iter()
                    .map(|(path, symbol)| StoredSymbolHit {
                        path: path.clone(),
                        symbol: symbol.clone(),
                    })
                    .collect();
                list.sort_by(|a, b| a.path.cmp(&b.path).then(a.symbol.line.cmp(&b.symbol.line)));
                symbols.insert(name.clone(), list);
            }
        }
        Self {
            format: GENERATION_FILE_FORMAT,
            workspace,
            generation,
            built_ms: now_ms(),
            fingerprint,
            data: WorkspaceData {
                files,
                postings,
                symbols,
                token_count: index.token_count() as u64,
                embeddings: index
                    .embedding_index(crate::WorkspaceId::new(workspace))
                    .cloned()
                    .unwrap_or_default()
                    .sanitize(),
            },
            coverage: Some(coverage),
            cursor,
            fingerprint_coverage: Some(fingerprint_coverage),
            fingerprint_next,
            fingerprint_next_epoch,
        }
    }

    /// The generation's durable coverage. A pre-coverage envelope without a
    /// record is LEGACY-UNKNOWN (incomplete): its completeness was never
    /// established, so it must never be reported complete.
    pub fn coverage(&self) -> IndexCoverage {
        self.coverage
            .clone()
            .unwrap_or_else(IndexCoverage::legacy_unknown)
    }

    /// The generation's fingerprint coverage. A pre-coverage envelope
    /// without a record is LEGACY-UNKNOWN (incomplete, never fully clean).
    pub fn fingerprint_coverage(&self) -> FingerprintCoverage {
        self.fingerprint_coverage
            .clone()
            .unwrap_or_else(FingerprintCoverage::legacy_unknown)
    }

    /// Workspace CONTENT identity of this generation: a BLAKE3 digest over
    /// the canonical envelope members that define content + fingerprint
    /// (workspace, generation, fingerprint, data) with `built_ms` and the
    /// coverage/continuation bookkeeping excluded, so re-serializing an
    /// unchanged generation yields the same identity. Evidence packages
    /// carry this value (audit 16).
    pub fn identity(&self) -> String {
        let mut canonical = self.clone();
        canonical.built_ms = 0;
        canonical.coverage = None;
        canonical.cursor = None;
        canonical.fingerprint_coverage = None;
        canonical.fingerprint_next = Vec::new();
        canonical.fingerprint_next_epoch = None;
        match canonical.to_bytes() {
            Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
            // Unreachable for plain data; a hostile envelope cannot reach
            // this path either (serialization of decoded values is total).
            Err(_) => "identity-unavailable".to_string(),
        }
    }

    /// Fingerprint-only identity: BLAKE3 over the canonical fingerprint list.
    pub fn fingerprint_identity(&self) -> String {
        let mut canonical = self.fingerprint.clone();
        canonical.sort();
        match serde_json::to_vec(&canonical) {
            Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
            Err(_) => "fingerprint-identity-unavailable".to_string(),
        }
    }

    /// Materialize a fresh single-workspace in-memory index from the
    /// envelope. A corrupt or version-skewed file is a typed error — the
    /// caller fails loudly (durable Failed), never serves a silent empty
    /// index.
    pub fn materialize(&self) -> Result<WorkspaceIndex, String> {
        if self.format != GENERATION_FILE_FORMAT {
            return Err(format!(
                "generation file format {} unsupported (expected {GENERATION_FILE_FORMAT})",
                self.format
            ));
        }
        // Decode validates this too; re-check here so a directly constructed
        // (non-decoded) envelope can never panic either.
        let ws = crate::WorkspaceId::try_from(self.workspace).map_err(|e| {
            format!(
                "generation file workspace {} is malformed: {e}",
                self.workspace
            )
        })?;
        let mut idx = WorkspaceIndex::new();
        let mut files = HashMap::new();
        for (path, f) in &self.data.files {
            files.insert(
                path.clone(),
                FileEntry {
                    symbols: f.symbols.clone(),
                    modified_ms: f.modified_ms,
                    chunks: f.chunks.clone(),
                },
            );
        }
        idx.files.insert(ws, files);
        let mut postings = HashMap::new();
        for (tok, map) in &self.data.postings {
            postings.insert(
                tok.clone(),
                map.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            );
        }
        idx.postings.insert(ws, postings);
        let mut symbols: HashMap<String, Vec<(String, Symbol)>> = HashMap::new();
        for (name, hits) in &self.data.symbols {
            symbols.insert(
                name.clone(),
                hits.iter()
                    .map(|h| (h.path.clone(), h.symbol.clone()))
                    .collect(),
            );
        }
        idx.symbols.insert(ws, symbols);
        // The unique-token cap is enforced at index time; a generation
        // claiming more is corrupt, never a silently accepted counter that
        // would disable the cap after a reload.
        let token_count = usize::try_from(self.data.token_count).map_err(|_| {
            format!(
                "generation token_count {} does not fit this platform",
                self.data.token_count
            )
        })?;
        if token_count > crate::MAX_UNIQUE_TOKENS {
            return Err(format!(
                "generation token_count {token_count} exceeds the {} unique-token cap",
                crate::MAX_UNIQUE_TOKENS
            ));
        }
        idx.token_count = token_count;
        // Persisted vectors are hostile input like everything else in the
        // envelope: invalid shapes are dropped loudly, bounds re-applied.
        idx.embeddings
            .insert(ws, self.data.embeddings.clone().sanitize());
        Ok(idx)
    }

    /// Serialize deterministically (sorted maps; stable field order).
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|e| format!("generation encode: {e}"))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let file: Self =
            serde_json::from_slice(bytes).map_err(|e| format!("generation decode: {e}"))?;
        if file.format != GENERATION_FILE_FORMAT {
            return Err(format!(
                "generation file format {} unsupported (expected {GENERATION_FILE_FORMAT})",
                file.format
            ));
        }
        if file.workspace == 0 {
            return Err(
                "generation file workspace 0 is malformed: workspace ids cannot be 0".to_string(),
            );
        }
        Ok(file)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SymbolKind, WorkspaceId};
    use std::path::Path;

    const SRC: &str = "pub fn alpha() {}\npub struct Beta {}\n";

    fn sample() -> (WorkspaceIndex, u64) {
        let mut idx = WorkspaceIndex::new();
        idx.index_file(
            WorkspaceId::new(7),
            Path::new("src/a.rs"),
            SRC.as_bytes(),
            123,
        )
        .unwrap();
        idx.index_file(
            WorkspaceId::new(7),
            Path::new("src/b.py"),
            b"def gamma(): pass",
            456,
        )
        .unwrap();
        (idx, 7)
    }

    #[test]
    fn envelope_roundtrip_materializes_identical_index() {
        let (idx, ws) = sample();
        let fp = vec![FingerprintEntry {
            path: "src/a.rs".into(),
            size: SRC.len() as u64,
            modified_ms: 123,
        }];
        let env = GenerationFile::capture(ws, 3, &idx, fp);
        let bytes = env.to_bytes().unwrap();
        let back = GenerationFile::from_bytes(&bytes).unwrap();
        assert_eq!(back, env);
        assert_eq!(back.generation, 3);
        assert_eq!(back.data.files.len(), 2);

        let mat = back.materialize().unwrap();
        // The materialized index serves the exact same lookups.
        assert!(!mat
            .files_for_token(WorkspaceId::new(7), "alpha", 10)
            .is_empty());
        assert!(!mat
            .files_for_token(WorkspaceId::new(7), "gamma", 10)
            .is_empty());
        let syms = mat.symbols_in(WorkspaceId::new(7), Path::new("src/a.rs"));
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"Beta"));
        assert_eq!(
            syms.iter().find(|s| s.name == "Beta").unwrap().kind,
            SymbolKind::Struct
        );
        // Determinism: re-serializing a loaded generation is byte-identical
        // modulo the capture timestamp (built_ms is wall-clock).
        let mut again = GenerationFile::capture(ws, 3, &mat, back.fingerprint);
        again.built_ms = back.built_ms;
        assert_eq!(again.to_bytes().unwrap(), bytes);
    }

    #[test]
    fn hostile_token_count_is_refused_at_materialize() {
        // The unique-token cap is an index-time invariant; a persisted
        // envelope claiming more is corrupt and must refuse loudly (it
        // would otherwise disable the cap for every later index_file).
        let (idx, ws) = sample();
        let mut env = GenerationFile::capture(ws, 1, &idx, vec![]);
        env.data.token_count = (crate::MAX_UNIQUE_TOKENS + 1) as u64;
        let err = env.materialize().unwrap_err();
        assert!(err.contains("unique-token cap"), "{err}");
        // Exactly at the cap is still admissible.
        env.data.token_count = crate::MAX_UNIQUE_TOKENS as u64;
        assert!(env.materialize().is_ok());
    }

    #[test]
    fn hostile_envelope_fails_loudly() {
        for evil in [
            b"not json".as_slice(),
            b"{}".as_slice(),
            br#"{"format":999,"generation":1}"#,
            br#"{"format":1,"workspace":7,"generation":3,"data":[]}"#,
        ] {
            assert!(GenerationFile::from_bytes(evil).is_err(), "{evil:?}");
        }
    }

    #[test]
    fn zero_workspace_is_refused_at_decode_and_never_panics_in_materialize() {
        // A serde-decoded generation file claiming workspace 0 must be a
        // typed refusal at decode (never a WorkspaceId::new panic later).
        let (idx, ws) = sample();
        let mut hostile = GenerationFile::capture(ws, 3, &idx, vec![]);
        hostile.workspace = 0;
        let bytes = hostile.to_bytes().unwrap();
        let err = GenerationFile::from_bytes(&bytes).unwrap_err();
        assert!(err.contains("workspace"), "{err}");
        // Even a directly constructed (non-decoded) envelope refuses in
        // materialize instead of panicking.
        let err = hostile.materialize().unwrap_err();
        assert!(err.contains("workspace"), "{err}");
        // Valid envelopes still round-trip and materialize unchanged.
        let valid = GenerationFile::capture(ws, 3, &idx, vec![]);
        let bytes = valid.to_bytes().unwrap();
        let back = GenerationFile::from_bytes(&bytes).unwrap();
        assert_eq!(back.workspace, ws);
        assert!(back.materialize().is_ok());
    }

    #[test]
    fn capture_sorts_deterministically_regardless_of_insertion_order() {
        let (a, ws) = sample();
        // Same content inserted in the opposite order must capture the same
        // envelope bytes.
        let mut b = WorkspaceIndex::new();
        b.index_file(
            WorkspaceId::new(7),
            Path::new("src/b.py"),
            b"def gamma(): pass",
            456,
        )
        .unwrap();
        b.index_file(
            WorkspaceId::new(7),
            Path::new("src/a.rs"),
            SRC.as_bytes(),
            123,
        )
        .unwrap();
        let mut ea = GenerationFile::capture(ws, 1, &a, vec![]);
        let eb = GenerationFile::capture(ws, 1, &b, vec![]);
        // The envelope embeds built_ms as wall-clock (see the round-trip
        // test above): the ORDERING claim is byte-equality modulo the
        // capture timestamp, so normalize it before comparing (a
        // millisecond boundary between the two captures is not an ordering
        // difference).
        ea.built_ms = eb.built_ms;
        assert_eq!(ea.to_bytes().unwrap(), eb.to_bytes().unwrap());
    }

    #[test]
    fn coverage_and_continuation_roundtrip_and_legacy_defaults_are_unknown() {
        use crate::coverage::{FingerprintCoverage, IndexCoverage, ScanCursor, ScanFrame};
        let (idx, ws) = sample();
        let mut cov = IndexCoverage::empty();
        cov.files_seen = 3;
        cov.files_indexed = 2;
        cov.bytes_indexed = 42;
        cov.complete = false;
        cov.truncated_reason = Some("batch_files".into());
        let cursor = ScanCursor {
            frames: vec![ScanFrame {
                dir: String::new(),
                after: "src/a.rs".into(),
            }],
        };
        let fp = FingerprintCoverage::round(3);
        let env = GenerationFile::capture_with_coverage(
            ws,
            1,
            &idx,
            vec![],
            cov.clone(),
            Some(cursor.clone()),
            fp.clone(),
            vec![],
            None,
        );
        let bytes = env.to_bytes().unwrap();
        let back = GenerationFile::from_bytes(&bytes).unwrap();
        assert_eq!(back.coverage(), cov);
        assert_eq!(back.cursor, Some(cursor));
        assert_eq!(back.fingerprint_coverage(), fp);
        // Identity is stable across re-serialization (built_ms excluded) and
        // changes when content changes.
        let mut reserialized = back.clone();
        reserialized.built_ms += 1;
        assert_eq!(reserialized.identity(), back.identity());
        assert_eq!(
            reserialized.fingerprint_identity(),
            back.fingerprint_identity()
        );
        let mut other = back.clone();
        other.fingerprint.push(FingerprintEntry {
            path: "z.rs".into(),
            size: 1,
            modified_ms: 1,
        });
        assert_ne!(other.identity(), back.identity());
        assert_ne!(other.fingerprint_identity(), back.fingerprint_identity());
        // Pre-coverage envelope (no coverage members) decodes as
        // LEGACY-UNKNOWN: never complete, fallback-eligible on misses.
        let legacy = GenerationFile::capture(ws, 2, &idx, vec![]);
        let mut value: serde_json::Value =
            serde_json::from_slice(&legacy.to_bytes().unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("coverage");
        value.as_object_mut().unwrap().remove("cursor");
        value
            .as_object_mut()
            .unwrap()
            .remove("fingerprint_coverage");
        let decoded = GenerationFile::from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(!decoded.coverage().complete);
        assert!(decoded.coverage().is_legacy_unknown());
        assert!(decoded.coverage().needs_fallback_on_miss());
        assert!(decoded.cursor.is_none());
        assert!(!decoded.fingerprint_coverage().complete);
        assert!(decoded.fingerprint_coverage().is_legacy_unknown());
        assert_eq!(
            decoded.coverage().truncated_reason.as_deref(),
            Some("legacy_unknown")
        );
    }

    #[test]
    fn fingerprint_order_is_canonical() {
        let mut fp = vec![
            FingerprintEntry {
                path: "z.rs".into(),
                size: 1,
                modified_ms: 1,
            },
            FingerprintEntry {
                path: "a.rs".into(),
                size: 2,
                modified_ms: 2,
            },
        ];
        fp.sort();
        let (idx, ws) = sample();
        let env = GenerationFile::capture(ws, 1, &idx, fp.clone());
        assert_eq!(env.fingerprint[0].path, "a.rs");
    }
}
