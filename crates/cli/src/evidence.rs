//! Production evidence provider (spec §20): the automatic evidence package
//! behind the daemon's context engine.
//!
//! The runtime calls `evidence_for(session, prompt)` before every reasoning
//! turn. This implementation resolves the session's workspace from the
//! durable store, indexes the workspace ONCE per workspace id (bounded scan;
//! the index is a memory-sidecar, never a session dependency), and asks the
//! search service for an evidence package from concepts of the prompt.
//!
//! Everything here is advisory for INFRASTRUCTURE failures (missing session,
//! unreadable root, hostile input, scan caps): those yield an empty evidence
//! list and never break the turn. A CONFIGURED semantic provider's failure is
//! different by contract: `fused`/`evidence_package` surface the search
//! crate's typed error unchanged, so this provider never silently replaces
//! semantic evidence with a lexical-only package.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use faktor_agent::{EvidenceProvider, EvidenceQuery};
use faktor_context::assembler::Evidence;
use faktor_core::error::{Error, ErrorKind};
use faktor_core::id::{SessionId, WorkspaceId};
use faktor_fs::rooted::{WalkBudget, WalkStep};
use faktor_fs::{WorkspaceFileService, WorkspaceHandle};
use faktor_index::{tokenize, WorkspaceIndex};
use faktor_search::SearchService;
use faktor_session::SessionManager;

/// Scan bounds (architecture §13 budgets): a workspace larger than this is
/// PARTIALLY indexed (deterministic walk order) — evidence stays bounded
/// even for hostile repos. The walk charges every entry against the
/// [`WalkBudget`]: entry/directory/depth ceilings and both byte ceilings
/// (file bytes seen, bytes actually read). Exhaustion is a typed
/// `Oversized` refusal the provider treats as a partial scan.
const SCAN_MAX_FILES: usize = 4_000;
const SCAN_MAX_DIRS: usize = 16_000;
const SCAN_MAX_BYTES: usize = 64 * 1024 * 1024;
const SCAN_MAX_FILE_BYTES: usize = 1_000_000;
/// Hard depth bound of the evidence walk (the root is depth 0): a hostile
/// tree that nests deeper is a partial scan, never an unbounded descent.
const SCAN_MAX_DEPTH: usize = 64;
const EVIDENCE_MAX_HITS: usize = 8;
const CONCEPT_MAX: usize = 16;
const CONCEPT_MIN_CHARS: usize = 4;

/// Directories that are never indexed (vcs metadata, dependency trees).
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".venv",
    "dist",
];

/// One index scan per workspace id per process; further prompts reuse it.
struct ScanState {
    scanned: HashSet<WorkspaceId>,
    failed: HashSet<WorkspaceId>,
}

/// Test-only scan seam hook type: fired with an entry's relative path after
/// the walker enumerated it and before the scan descends or reads it.
#[cfg(test)]
type ScanSeam = Arc<dyn Fn(&Path) + Send + Sync>;

pub struct RepoEvidence {
    session: Arc<SessionManager>,
    /// The anchored file authority the scan runs through: one
    /// [`WorkspaceHandle`] per scan, its rooted-directory walk and reads
    /// the ONLY path to workspace bytes. Never re-opened by absolute path.
    fs: Arc<WorkspaceFileService>,
    index: Arc<Mutex<WorkspaceIndex>>,
    search: SearchService,
    /// The CONFIGURED semantic embedder (`[embeddings]` resolution); `None`
    /// keeps retrieval lexical/symbol-only — an honest degradation, never a
    /// fabricated vector.
    embedder: Option<Arc<dyn faktor_search::Embedder>>,
    index_embedding: Option<(
        Arc<dyn faktor_index::EmbeddingSource>,
        faktor_index::EmbeddingModel,
    )>,
    scan: Mutex<ScanState>,
    scan_max_files: usize,
    scan_max_dirs: usize,
    scan_max_bytes: usize,
    scan_max_file_bytes: usize,
    /// Test seam: fired with an entry's relative path after the walker has
    /// enumerated (stat'ed) it and before the scan descends or reads it, so
    /// adversarial tests can swap the entry inside that exact window.
    #[cfg(test)]
    scan_seam: Option<ScanSeam>,
}

impl RepoEvidence {
    pub fn new(
        session: Arc<SessionManager>,
        embedder: Option<Arc<dyn faktor_search::Embedder>>,
    ) -> Self {
        Self::with_embedder_and_caps(
            session,
            embedder,
            SCAN_MAX_FILES,
            SCAN_MAX_DIRS,
            SCAN_MAX_BYTES,
            SCAN_MAX_FILE_BYTES,
        )
    }

    #[cfg(test)]
    fn with_caps(
        session: Arc<SessionManager>,
        scan_max_files: usize,
        scan_max_dirs: usize,
        scan_max_bytes: usize,
        scan_max_file_bytes: usize,
        embedder: Option<Arc<dyn faktor_search::Embedder>>,
    ) -> Self {
        Self::with_embedder_and_caps(
            session,
            embedder,
            scan_max_files,
            scan_max_dirs,
            scan_max_bytes,
            scan_max_file_bytes,
        )
    }

    fn with_embedder_and_caps(
        session: Arc<SessionManager>,
        embedder: Option<Arc<dyn faktor_search::Embedder>>,
        scan_max_files: usize,
        scan_max_dirs: usize,
        scan_max_bytes: usize,
        scan_max_file_bytes: usize,
    ) -> Self {
        let index = Arc::new(Mutex::new(WorkspaceIndex::new()));
        let search = SearchService::new(index.clone(), embedder.clone());
        Self {
            session,
            fs: WorkspaceFileService::new(),
            index,
            search,
            index_embedding: embedder.clone().and_then(index_source_for_embedder),
            embedder,
            scan: Mutex::new(ScanState {
                scanned: HashSet::new(),
                failed: HashSet::new(),
            }),
            scan_max_files,
            scan_max_dirs,
            scan_max_bytes,
            scan_max_file_bytes,
            #[cfg(test)]
            scan_seam: None,
        }
    }

    /// Resolve the session's canonical workspace root (same source of truth
    /// as the tool runtime; P0-48 root re-pointing: a live shadow of the
    /// session resolves the evidence scan to the shadow root while a
    /// shadowed drive runs — evidence reflects the world the turn mutates).
    fn resolve_root(&self, session: SessionId) -> Option<(WorkspaceId, PathBuf)> {
        let handle = self.session.get_session(session).ok()??;
        let row = handle.row().ok()?;
        let root = self.session.resolve_workspace_root(session).ok()??;
        Some((row.workspace_id, root))
    }

    /// Deterministic bounded scan through the workspace's anchored
    /// authority ([`WorkspaceHandle::walk_bounded`]): the walker enumerates
    /// every directory through the fd anchored at open time and descends
    /// child-relative with strict no-follow semantics, so a directory entry
    /// swapped for an outside symlink mid-walk is a typed refusal and the
    /// walk never follows it out of the workspace. Every entry, directory,
    /// depth level, file byte seen and byte read is charged to the
    /// [`WalkBudget`]; exhaustion is the typed `Oversized` refusal the
    /// provider treats as a partial scan. Content is read through the SAME
    /// handle ([`WorkspaceHandle::read`]): no absolute path is ever used as
    /// authority, so an entry swapped after enumeration cannot inject an
    /// external file into the index. Skips vcs/dependency directories,
    /// binary and oversized files.
    fn scan_workspace(&self, ws: WorkspaceId, workspace: &WorkspaceHandle) -> Result<(), Error> {
        let mut budget = WalkBudget::new(
            self.scan_max_files,
            self.scan_max_dirs,
            SCAN_MAX_DEPTH,
            self.scan_max_bytes as u64,
            self.scan_max_bytes as u64,
        );
        let mut index = self.index.lock().unwrap();
        workspace.walk_bounded(
            Path::new(""),
            &mut budget,
            SKIP_DIRS,
            &mut |entry, _depth, budget| {
                // The test seam fires for EVERY enumerated entry (files and
                // directories) after the walker stat'ed it and before the
                // scan descends or reads it.
                #[cfg(test)]
                self.fire_scan_seam(&entry.rel);
                // Files only: symlinks (whatever they point at) and special
                // files are never read or indexed; directories are descended
                // by the walker (a swapped directory is refused there).
                if !entry.is_file() {
                    return Ok(WalkStep::Continue);
                }
                if entry.size > self.scan_max_file_bytes as u64 {
                    return Ok(WalkStep::Continue);
                }
                // The same-handle read: resolved from the open directory fd
                // with no-follow, so the pre-read `lstat` is never trusted
                // as authority. A vanished or swapped entry is skipped —
                // never read through a path string.
                let data = match workspace.read(&entry.rel, self.scan_max_file_bytes) {
                    Ok(data) => data,
                    Err(_) => return Ok(WalkStep::Continue),
                };
                budget.charge_read_bytes(data.bytes.len() as u64)?;
                // Binary sniff: NUL in the first 8 KiB → not text.
                if data.bytes.iter().take(8192).any(|b| *b == 0) {
                    return Ok(WalkStep::Continue);
                }
                let _ = index.index_file(ws, &entry.rel, &data.bytes, entry.modified_ms);
                Ok(WalkStep::Continue)
            },
        )
    }

    /// Fire the test-only scan seam for one enumerated entry (no-op outside
    /// `#[cfg(test)]` builds).
    #[cfg(test)]
    fn fire_scan_seam(&self, rel: &Path) {
        if let Some(seam) = &self.scan_seam {
            seam(rel);
        }
    }

    #[cfg(test)]
    fn set_scan_seam(&mut self, seam: ScanSeam) {
        self.scan_seam = Some(seam);
    }

    /// Concepts from the retrieval signal (spec §20): the prompt's own
    /// words first, then basename tokens of the changed files (so edited
    /// files rank for follow-up), then failure keywords. Bounded, deduped.
    fn concepts(query: &EvidenceQuery) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let push_tokens = |text: &str, out: &mut Vec<String>, seen: &mut HashSet<String>| {
            for tok in tokenize(text).into_iter().take(512) {
                if tok.len() < CONCEPT_MIN_CHARS || !seen.insert(tok.clone()) {
                    continue;
                }
                out.push(tok);
                if out.len() >= CONCEPT_MAX {
                    return;
                }
            }
        };
        push_tokens(&query.prompt, &mut out, &mut seen);
        if out.len() < CONCEPT_MAX {
            for f in query.changed_files.iter().take(16) {
                let base = f.rsplit('/').next().unwrap_or(f.as_str());
                push_tokens(base, &mut out, &mut seen);
                if out.len() >= CONCEPT_MAX {
                    break;
                }
            }
        }
        if out.len() < CONCEPT_MAX {
            push_tokens(&query.failures.join(" "), &mut out, &mut seen);
        }
        out
    }
}

impl RepoEvidence {
    /// The session ended: drop this workspace's scan state AND index
    /// postings so memory stays bounded across session churn. Any transient
    /// scan handle registered for the workspace is closed too.
    pub fn forget_workspace(&self, ws: WorkspaceId) {
        self.index.lock().unwrap().remove_workspace(ws);
        self.fs.close(ws);
        let mut scan = self.scan.lock().unwrap();
        scan.scanned.remove(&ws);
        scan.failed.remove(&ws);
    }
}

impl RepoEvidence {
    /// The synchronous evidence body behind the async trait method (audit
    /// 14/26). The runtime polls the boxed trait future on a blocking-pool
    /// thread under a hard wall budget; this inherent method keeps the
    /// crate's own tests on the historical synchronous shape (the scan is
    /// CPU/fs-bound and the assertions here are about scan semantics, not
    /// async dispatch).
    ///
    /// Fallible by contract for the SEMANTIC leg: a configured provider's
    /// typed failure propagates instead of degrading to lexical-only hits.
    /// Infrastructure failures (unknown session, unreadable root, scan caps)
    /// remain the documented empty advisory package.
    fn evidence_sync(
        &self,
        session: SessionId,
        query: &EvidenceQuery,
    ) -> Result<Vec<Evidence>, Error> {
        let Some((ws, root)) = self.resolve_root(session) else {
            return Ok(vec![]);
        };
        {
            let scan = self.scan.lock().unwrap();
            if scan.failed.contains(&ws) {
                return Ok(vec![]);
            }
            if scan.scanned.contains(&ws) {
                return self.evidence_package(ws, query);
            }
        }
        // The scan runs through one anchored WorkspaceHandle; the handle is
        // closed once the scan is done (its watcher is scan-scoped here).
        let result = match self.fs.open(ws, root) {
            Ok(workspace) => {
                let outcome = self.scan_workspace(ws, &workspace);
                self.fs.close(ws);
                outcome
            }
            Err(e) => Err(e),
        };
        let mut scan = self.scan.lock().unwrap();
        match result {
            // Budget exhaustion is a PARTIAL scan: the entries verified
            // before the cap stay indexed (deterministic walk order) and
            // the workspace reads as scanned.
            Err(e) if e.kind == ErrorKind::Oversized => {
                scan.scanned.insert(ws);
            }
            // Infrastructure failure (unknown root, hostile swap, workspace
            // re-pointed mid-scan): advisory empty package, never a broken
            // turn and never a rescan storm.
            Err(_) => {
                scan.failed.insert(ws);
            }
            Ok(()) => {
                scan.scanned.insert(ws);
            }
        }
        drop(scan);
        self.evidence_package(ws, query)
    }
}

impl EvidenceProvider for RepoEvidence {
    fn evidence_for(
        &self,
        session: SessionId,
        query: EvidenceQuery,
    ) -> futures::future::BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
        Box::pin(async move { self.evidence_sync(session, &query) })
    }

    fn forget(&self, workspace: WorkspaceId) {
        self.forget_workspace(workspace);
    }

    /// The CONFIGURED embedder also reaches the agent's index-backed
    /// evidence assembly through this seam (the agent asks its evidence
    /// provider for the embedder instead of hardcoding `None`).
    fn embedder(&self) -> Option<Arc<dyn faktor_search::Embedder>> {
        self.embedder.clone()
    }

    fn index_embedding(
        &self,
    ) -> Option<(
        Arc<dyn faktor_index::EmbeddingSource>,
        faktor_index::EmbeddingModel,
    )> {
        self.index_embedding.clone()
    }
}

impl RepoEvidence {
    fn evidence_package(
        &self,
        ws: WorkspaceId,
        query: &EvidenceQuery,
    ) -> Result<Vec<Evidence>, Error> {
        if query.prompt.len() > 512 * 1024 {
            return Ok(vec![]);
        }
        let concepts = Self::concepts(query);
        if concepts.is_empty() {
            return Ok(vec![]);
        }
        let hits = self
            .search
            .evidence_package(ws, &concepts, EVIDENCE_MAX_HITS)?;
        Ok(hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| Evidence {
                path: h.path,
                snippet: h.snippet,
                score: 1.0 / (1.0 + i as f64),
            })
            .collect())
    }
}

/// When the configured embedder is also an index `EmbeddingSource` (the
/// CLI's `ProviderEmbedder`), reuse it for build-time vector persistence
/// with a stable `(model, revision)` identity. A pure query-time embedder
/// yields `None` (honest: builds stay lexical/symbol-only).
fn index_source_for_embedder(
    embedder: Arc<dyn faktor_search::Embedder>,
) -> Option<(
    Arc<dyn faktor_index::EmbeddingSource>,
    faktor_index::EmbeddingModel,
)> {
    let (id, rev) = embedder.identity()?;
    let source: Arc<dyn faktor_index::EmbeddingSource> =
        std::sync::Arc::new(SearchEmbedderIndexSource { embedder });
    Some((source, faktor_index::EmbeddingModel::new(&id, &rev)))
}

/// Thin adapter: the `EmbeddingSource` impl forwards to the search
/// embedder (the CLI type implements both; this keeps the seam object-safe
/// in the agent crate without depending on the CLI).
struct SearchEmbedderIndexSource {
    embedder: Arc<dyn faktor_search::Embedder>,
}

impl faktor_index::EmbeddingSource for SearchEmbedderIndexSource {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, faktor_core::Error> {
        Ok(self.embedder.embed(texts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_session::{ShadowRow, ShadowRowState};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn manager() -> Arc<SessionManager> {
        // The dir must outlive the manager (per-call connections reopen the
        // db file); leak it for the process lifetime.
        static KEEP: std::sync::Mutex<Vec<TempDir>> = std::sync::Mutex::new(Vec::new());
        let dir = TempDir::new().unwrap();
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        KEEP.lock().unwrap().push(dir);
        m
    }

    /// Register the workspace + a session on it; returns the session id.
    fn registered_session(m: &Arc<SessionManager>, root: &Path) -> SessionId {
        let ws = m.create_workspace(root.to_str().unwrap()).unwrap();
        m.create_session(ws, "t", "p", "m").unwrap().id()
    }

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    #[test]
    fn evidence_finds_symbols_in_workspace() {
        let root = TempDir::new().unwrap();
        write(
            root.path(),
            "src/lib.rs",
            b"pub fn balance_account() -> i64 { 42 }\n",
        );
        let m = manager();
        let ev = RepoEvidence::new(m.clone(), None);
        let sid = registered_session(&m, root.path());
        let evidence = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "fix balance_account".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            evidence.iter().any(|e| e.path.ends_with("lib.rs")),
            "evidence must surface the file defining the concept: {evidence:?}"
        );
    }

    /// Embedder for the fusion E2E: the concept tokens (`quantum`, `zebra`)
    /// are semantically near `reconcile`/`ledger` — none of them appear in
    /// the corpus, so lexical/symbol/exact search can never match.
    struct ConceptAxisEmbedder;
    impl faktor_search::Embedder for ConceptAxisEmbedder {
        fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
            texts
                .iter()
                .map(|t| {
                    let l = t.to_lowercase();
                    vec![
                        if l.contains("quantum") || l.contains("ledger") || l.contains("reconcile")
                        {
                            1.0
                        } else {
                            0.0
                        },
                        if l.contains("zebra") { 1.0 } else { 0.0 },
                    ]
                })
                .collect()
        }
    }

    /// The configured-embedder wiring of the production evidence provider:
    /// semantic fusion finds the item lexical/symbol search misses; without
    /// an embedder the same prompt yields NO evidence (explicit `Disabled`
    /// semantics, no crash, no invented item); a FAILING embedder surfaces
    /// its typed error — never a silent lexical-only package.
    #[test]
    fn configured_embedder_fuses_semantically_and_absence_degrades_honestly() {
        let root = TempDir::new().unwrap();
        write(
            root.path(),
            "src/ledger.rs",
            b"pub fn reconcile_accounts() -> u32 { 7 }\n",
        );
        let m = manager();
        let sid = registered_session(&m, root.path());
        let query = EvidenceQuery {
            prompt: "quantum zebra".into(),
            ..Default::default()
        };

        let with = RepoEvidence::new(m.clone(), Some(Arc::new(ConceptAxisEmbedder)))
            .evidence_sync(sid, &query)
            .unwrap();
        assert!(
            with.iter().any(|e| e.path.ends_with("ledger.rs")),
            "the configured embedder must fuse the semantic-only item: {with:?}"
        );

        let without = RepoEvidence::new(m.clone(), None)
            .evidence_sync(sid, &query)
            .unwrap();
        assert!(
            without.is_empty(),
            "without an embedder no semantic evidence may be invented: {without:?}"
        );

        struct FailingEmbedder;
        impl faktor_search::Embedder for FailingEmbedder {
            fn embed(&self, _texts: &[String]) -> Vec<Vec<f32>> {
                vec![]
            }
            fn try_embed(
                &self,
                _texts: &[String],
            ) -> Result<Vec<Vec<f32>>, faktor_core::error::Error> {
                Err(faktor_core::error::Error::new(
                    faktor_core::error::ErrorKind::Network,
                    "embedding backend down",
                ))
            }
        }
        let failing = RepoEvidence::new(m.clone(), Some(Arc::new(FailingEmbedder)))
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "reconcile_accounts".into(),
                    ..Default::default()
                },
            )
            .expect_err("a provider failure must surface, never degrade to lexical-only");
        assert_eq!(
            failing.kind,
            faktor_core::error::ErrorKind::Network,
            "{failing}"
        );
        assert!(
            failing.retryable,
            "transport errors stay retryable: {failing}"
        );
    }

    #[test]
    fn scan_is_capped_and_never_hangs() {
        let root = TempDir::new().unwrap();
        for i in 0..200 {
            write(
                root.path(),
                &format!("f{i}.rs"),
                format!("pub fn fx{i}() {{}}\n").as_bytes(),
            );
        }
        let m = manager();
        let ev = RepoEvidence::with_caps(m.clone(), 25, 10_000, 64 * 1024 * 1024, 1_000_000, None);
        // The scan cap must not panic, must terminate, and must bound work.
        let sid = registered_session(&m, root.path());
        let evidence = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "fx199".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(evidence.len() <= 8);
        // fx199 may or may not be indexed (25 < 200) — but the call returns.
        assert!(evidence.len() <= EVIDENCE_MAX_HITS);
    }

    #[test]
    fn skips_vcs_and_junk() {
        let root = TempDir::new().unwrap();
        write(root.path(), "src/app.rs", b"fn payments() {}\n");
        write(root.path(), ".git/config", b"fn payments() {}\n");
        write(root.path(), "target/debug/x.rs", b"fn payments() {}\n");
        write(
            root.path(),
            "node_modules/lib.js",
            b"function payments() {}\n",
        );
        let m = manager();
        let ev = RepoEvidence::new(m.clone(), None);
        let sid = registered_session(&m, root.path());
        let evidence = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "payments".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(evidence.len(), 1);
        assert!(evidence[0].path.ends_with("src/app.rs"));
    }

    #[test]
    fn evidence_resolves_from_the_live_shadow_of_a_shadowed_session() {
        // P0-48 root re-pointing: while the session carries a live shadow
        // row, the evidence scan reads the SHADOW root (files present only
        // there are findable, user-checkout-only files are not); a plain
        // session keeps today's stored-root behavior byte-identically.
        let user = TempDir::new().unwrap();
        let shadow = TempDir::new().unwrap();
        std::fs::write(
            shadow.path().join("onlyshadow.rs"),
            b"pub fn shadow_only_symbol() {}\n",
        )
        .unwrap();
        std::fs::write(
            user.path().join("useronly.rs"),
            b"pub fn user_only_symbol() {}\n",
        )
        .unwrap();
        let m = manager();
        let ws = m.create_workspace(user.path().to_str().unwrap()).unwrap();
        let sid = m.create_session(ws, "shadowed", "p", "m").unwrap().id();
        let ev = RepoEvidence::new(m.clone(), None);
        // Plain session: the user checkout is the evidence root (a shadow
        // row exists for NO session; shadow-only files are never seen).
        let out = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "user_only_symbol".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            out.iter().any(|e| e.path.ends_with("useronly.rs")),
            "plain sessions scan the stored root: {out:?}"
        );
        assert!(
            !out.iter().any(|e| e.path.contains("onlyshadow.rs")),
            "no shadow is ever consulted on a plain session"
        );
        // A live shadow row (the exact durable row ShadowRoots writes):
        // the evidence root re-points to the shadow.
        let row = ShadowRow {
            session_id: sid.raw(),
            shadow_id: "sh-test".into(),
            base_root: user.path().to_str().unwrap().into(),
            root: shadow.path().to_str().unwrap().into(),
            state: ShadowRowState::Active,
            base_entries: 0,
            base_bytes: 0,
            created_ms: 1,
        };
        m.put_shadow_row(sid, &row).unwrap();
        // A fresh evidence provider: per-workspace scan caches are process
        // state, so the re-pointed scan needs a clean provider instance.
        let ev = RepoEvidence::new(m.clone(), None);
        let out = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "shadow_only_symbol".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            out.iter().any(|e| e.path.ends_with("onlyshadow.rs")),
            "evidence must read the shadow root while the shadow is live: {out:?}"
        );
        assert!(
            !out.iter().any(|e| e.path.contains("useronly.rs")),
            "the user checkout is not the evidence root of a shadowed session"
        );
        // Retire the shadow: the stored root is authoritative again.
        let mut retired = row;
        retired.state = ShadowRowState::Integrated;
        m.put_shadow_row(sid, &retired).unwrap();
        let ev = RepoEvidence::new(m.clone(), None);
        let out = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "user_only_symbol".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            out.iter().any(|e| e.path.ends_with("useronly.rs")),
            "a retired shadow never keeps re-pointing evidence"
        );
    }

    #[test]
    fn hostile_workspace_never_panics() {
        let root = TempDir::new().unwrap();
        let mut bin = vec![0u8; 64];
        bin.extend([1u8; 64]);
        write(root.path(), "bin.dat", &bin);
        let m = manager();
        let ev = RepoEvidence::new(m.clone(), None);
        // Root that is a file, missing root, unknown session.
        let sid = registered_session(&m, root.path());
        let _ = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "payments".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let unknown = SessionId::new(99_999);
        assert!(ev
            .evidence_sync(
                unknown,
                &EvidenceQuery {
                    prompt: "payments".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .is_empty());
        // A session whose store root vanished.
        let missing = TempDir::new().unwrap();
        write(missing.path(), "x.rs", b"fn alpha() {}\n");
        let sid2 = registered_session(&m, missing.path());
        std::fs::remove_dir_all(missing.path()).unwrap();
        let out = ev
            .evidence_sync(
                sid2,
                &EvidenceQuery {
                    prompt: "alpha".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(out.len() <= 8);
    }

    #[test]
    fn hostile_prompt_is_bounded() {
        let root = TempDir::new().unwrap();
        write(root.path(), "src/app.rs", b"fn payments() {}\n");
        let m = manager();
        let ev = RepoEvidence::new(m.clone(), None);
        let sid = registered_session(&m, root.path());
        // 1 MiB prompt: bounded token extraction, bounded output.
        let huge = "a".repeat(1024 * 1024);
        let evidence = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: huge,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(evidence.len() <= EVIDENCE_MAX_HITS);
        // Empty prompt → nothing.
        assert!(ev
            .evidence_sync(sid, &EvidenceQuery::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn binary_and_oversized_files_skipped() {
        let root = TempDir::new().unwrap();
        write(root.path(), "big.rs", &[b'x'; 2_000_000]);
        write(root.path(), "good.rs", b"fn target_fn() {}\n");
        let m = manager();
        let ev = RepoEvidence::with_caps(m.clone(), 100, 10_000, 64 * 1024 * 1024, 1_000, None);
        let sid = registered_session(&m, root.path());
        let evidence = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "target_fn".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(evidence.len(), 1);
        assert!(evidence[0].path.ends_with("good.rs"));
    }

    #[test]
    fn changed_file_signal_drives_retrieval_without_prompt_keyword() {
        // Spec §20: retrieval must not depend on the model's wording — a
        // prompt with NO keyword still surfaces evidence for the file the
        // task changed (basename tokens are concepts too).
        let root = TempDir::new().unwrap();
        write(
            root.path(),
            "src/payments_ledger.rs",
            b"pub fn settle_payments() -> i64 { 7 }\n",
        );
        let m = manager();
        let ev = RepoEvidence::new(m.clone(), None);
        let sid = registered_session(&m, root.path());
        let evidence = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "continue the task please".into(),
                    changed_files: vec!["src/payments_ledger.rs".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            evidence
                .iter()
                .any(|e| e.path.ends_with("payments_ledger.rs")),
            "changed-file signal must drive retrieval: {evidence:?}"
        );
    }

    #[tokio::test]
    async fn forget_drops_index_and_rescans_later() {
        // Spec §21: a closed session's index is dropped; a later session on
        // the same workspace rescans (a file created after the first scan is
        // only discoverable after the forget).
        let root = TempDir::new().unwrap();
        write(root.path(), "src/one.rs", b"pub fn alpha_fn() {}\n");
        let m = manager();
        let ev = RepoEvidence::new(m.clone(), None);
        let sid = registered_session(&m, root.path());
        // First scan: alpha_fn found.
        let evidence = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "alpha_fn".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(evidence.len(), 1);
        let ws = ev.resolve_root(sid).unwrap().0;
        // The workspace is dropped: scan state AND postings.
        ev.forget_workspace(ws);
        // New file after the forget.
        write(root.path(), "src/two.rs", b"pub fn beta_fn() {}\n");
        let evidence = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "beta_fn".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            evidence.iter().any(|e| e.path.ends_with("two.rs")),
            "the rescanned index must see the new file: {evidence:?}"
        );
    }

    // ------------------------------------------------------------------
    // P1-D adversarial containment: a swap inside the walker's
    // enumeration -> descent / stat -> read windows must never let an
    // external planted secret reach the index or the evidence package.
    // ------------------------------------------------------------------

    /// The seam fires once per enumerated entry; only the first firing on
    /// `rel` performs the swap, so the hook is idempotent under a walk that
    /// re-enumerates.
    #[cfg(unix)]
    fn once_seam(
        rel: &'static str,
        swap: impl Fn() + Send + Sync + 'static,
    ) -> (Arc<AtomicBool>, ScanSeam) {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_seam = fired.clone();
        let hook: ScanSeam = Arc::new(move |seen: &Path| {
            if seen == Path::new(rel) && !fired_seam.swap(true, Ordering::SeqCst) {
                swap();
            }
        });
        (fired, hook)
    }

    /// P1-D: a directory SWAPPED for an outside symlink after the walker
    /// enumerated (stat'ed) it and before the descent must abort the walk
    /// loudly — the outside tree is never entered, and its planted secret
    /// never enters the index or the evidence output. (The old private
    /// walker queued the absolute path and `read_dir`'d it later; the swap
    /// would have descended into the external directory.)
    #[cfg(unix)]
    #[test]
    fn dir_swap_after_enumeration_never_indexes_outside_secret() {
        let root = TempDir::new().unwrap();
        write(root.path(), "a/keep.rs", b"pub fn keep_symbol() {}\n");
        let outside = TempDir::new().unwrap();
        std::fs::write(
            outside.path().join("secret.rs"),
            b"pub fn zebrasecret_marker() {}\n",
        )
        .unwrap();
        let m = manager();
        let mut ev = RepoEvidence::new(m.clone(), None);
        let sid = registered_session(&m, root.path());
        let root_path = root.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        let (fired, seam) = once_seam("a", move || {
            std::fs::remove_dir_all(root_path.join("a")).unwrap();
            std::os::unix::fs::symlink(&outside_path, root_path.join("a")).unwrap();
        });
        ev.set_scan_seam(seam);

        let out = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "zebrasecret_marker".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            fired.load(Ordering::SeqCst),
            "the seam must fire on the enumerated directory"
        );
        assert!(
            out.is_empty(),
            "the outside secret must never be evidence: {out:?}"
        );
        let ws = ev.resolve_root(sid).unwrap().0;
        let index = ev.index.lock().unwrap();
        assert!(
            index.file_paths(ws).iter().all(|p| !p.contains("secret")),
            "the outside path must never enter the index: {:?}",
            index.file_paths(ws)
        );
        assert!(
            index.files_for_token(ws, "zebrasecret", 8).is_empty(),
            "the outside secret token must never enter the index"
        );
        assert!(
            outside.path().join("secret.rs").exists(),
            "the outside tree is untouched"
        );
    }

    /// P1-D: a file SWAPPED for an outside symlink after the walker stat'ed
    /// it and before the read must never be read through the planted path:
    /// the handle-relative no-follow read refuses, the entry is skipped, and
    /// the secret never enters the index or the evidence output.
    #[cfg(unix)]
    #[test]
    fn file_swap_after_stat_before_read_never_indexes_outside_secret() {
        let root = TempDir::new().unwrap();
        write(root.path(), "note.rs", b"pub fn inside_symbol() {}\n");
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("secret.rs");
        std::fs::write(&outside_file, b"pub fn zebrasecret_marker() {}\n").unwrap();
        let m = manager();
        let mut ev = RepoEvidence::new(m.clone(), None);
        let sid = registered_session(&m, root.path());
        let root_path = root.path().to_path_buf();
        let link_target = outside_file.clone();
        let (fired, seam) = once_seam("note.rs", move || {
            std::fs::remove_file(root_path.join("note.rs")).unwrap();
            std::os::unix::fs::symlink(&link_target, root_path.join("note.rs")).unwrap();
        });
        ev.set_scan_seam(seam);

        let out = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "zebrasecret_marker".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            fired.load(Ordering::SeqCst),
            "the seam must fire on the enumerated file"
        );
        assert!(
            out.is_empty(),
            "the outside secret must never be evidence: {out:?}"
        );
        let ws = ev.resolve_root(sid).unwrap().0;
        let index = ev.index.lock().unwrap();
        assert!(
            index.files_for_token(ws, "zebrasecret", 8).is_empty(),
            "the swapped-in external content must never be indexed"
        );
        assert!(
            index.files_for_token(ws, "inside", 8).is_empty(),
            "the swapped-away original must not be indexed either: {:?}",
            index.file_paths(ws)
        );
        assert!(outside_file.exists(), "the outside file is untouched");
    }

    /// P1-D: the evidence caps map onto the typed [`WalkBudget`] — entry,
    /// file-byte and depth exhaustion are `Oversized` refusals naming the
    /// budget (never a silent truncation), and the turn-level path degrades
    /// them to the documented PARTIAL scan (bounded output).
    #[test]
    fn scan_budget_exhaustion_is_typed_and_partial() {
        let root = TempDir::new().unwrap();
        for i in 0..50 {
            write(
                root.path(),
                &format!("f{i:02}.rs"),
                b"pub fn budget_probe() {}\n",
            );
        }
        let m = manager();
        let sid = registered_session(&m, root.path());

        // Entry budget: 5 entries for 50 files.
        let ev = RepoEvidence::with_caps(m.clone(), 5, 10_000, 64 * 1024 * 1024, 1_000_000, None);
        let (ws, root_path) = ev.resolve_root(sid).unwrap();
        let handle = ev.fs.open(ws, root_path).unwrap();
        let err = ev.scan_workspace(ws, &handle).unwrap_err();
        ev.fs.close(ws);
        assert_eq!(err.kind, ErrorKind::Oversized, "{err:?}");
        assert!(err.message.contains("walk budget"), "{err}");
        // The provider treats exhaustion as a partial scan: no turn error.
        let out = ev
            .evidence_sync(
                sid,
                &EvidenceQuery {
                    prompt: "budget_probe".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(out.len() <= EVIDENCE_MAX_HITS);

        // File-byte budget: 10 bytes total while each file is 24 bytes.
        let ev = RepoEvidence::with_caps(m.clone(), 4_000, 10_000, 10, 1_000_000, None);
        let (ws, root_path) = ev.resolve_root(sid).unwrap();
        let handle = ev.fs.open(ws, root_path).unwrap();
        let err = ev.scan_workspace(ws, &handle).unwrap_err();
        ev.fs.close(ws);
        assert_eq!(err.kind, ErrorKind::Oversized, "{err:?}");
        assert!(err.message.contains("file budget"), "{err}");

        // Depth budget: a tree nested past SCAN_MAX_DEPTH is refused typed.
        let deep = TempDir::new().unwrap();
        let deep_rel: String = (0..SCAN_MAX_DEPTH + 2).map(|i| format!("d{i}/")).collect();
        write(
            deep.path(),
            &format!("{deep_rel}x.rs"),
            b"pub fn deep_probe() {}\n",
        );
        let sid2 = registered_session(&m, deep.path());
        let ev = RepoEvidence::new(m.clone(), None);
        let (ws2, root2) = ev.resolve_root(sid2).unwrap();
        let handle = ev.fs.open(ws2, root2).unwrap();
        let err = ev.scan_workspace(ws2, &handle).unwrap_err();
        ev.fs.close(ws2);
        assert_eq!(err.kind, ErrorKind::Oversized, "{err:?}");
        assert!(err.message.contains("depth budget"), "{err}");
    }
}
