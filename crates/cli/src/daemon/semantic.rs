//! Production adapters for the native semantic fallback.
//!
//! [`faktor_semantic::GenericSemanticFallback`] is intentionally dependency
//! free: it composes trait seams. This module is the daemon's REAL
//! composition of those seams over the authorities it already owns:
//!
//! - [`IndexService`] (durable symbol/file/lexical/embedding retrieval),
//! - [`LspManager`]/[`LspClient`] (rust-analyzer, TypeScript, pyright),
//! - tree-sitter parser facts (`faktor_index::parse_symbols`),
//! - git revision identity and changed-path diffs (`WorktreeManager`), and
//! - verification/test discovery (`faktor_verify` inventory + derivation),
//! - the repository relationship graph (`faktor_impact`).
//!
//! Every subsystem is optional at the workspace level: an absent index
//! generation, missing language server or non-git root is returned as a
//! typed [`SemanticError::Unavailable`], never an empty success. Results are
//! bounded and the fallback's revision-keyed cache prevents repeated
//! expensive lookups within one source revision. No subsystem requires a
//! network service or model inference.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use faktor_core::{CancellationToken, WorkspaceId};
use faktor_fs::WorkspaceHandle;
use faktor_git::WorktreeManager;
use faktor_index::{IndexService, Symbol as IndexSymbol, SymbolKind as IndexSymbolKind};
use faktor_lsp::{
    CallHierarchyItem, LspClient, LspConfig, LspManager, SemanticError as LspError,
    SemanticOptions, WorkspaceSymbol,
};
use faktor_search::{fuse, Embedder, Hit, SearchService};
use faktor_semantic::fallback::{
    GenericSemanticFallback, GitMetadata, IndexMetadata, LspMetadata, TreeSitterMetadata,
    VerificationMetadata,
};
use faktor_semantic::types::{
    BoxFuture, SemanticCheck, SemanticContextItem, SemanticDeltaChange, SemanticDeltaKind,
    SemanticEntityId, SemanticEntityRef, SemanticError, SemanticFidelity,
    SemanticFindingProvenance, SemanticSubsystem, WorkspacePath, MAX_ENTITY_REFS,
};
use faktor_session::SessionManager;
use faktor_terminal::{ProcessOwner, ProcessSupervisor};
use faktor_verify::inventory::{
    discover_repo_inventory_with_supervisor, InventoryBudget, RepoInventory,
};

/// The daemon authorities the production semantic fallback is composed over.
/// Built once in the graph and handed to [`crate::graph::semantic_registry`].
pub struct SemanticSources {
    pub index: Option<Arc<IndexService>>,
    pub session: Arc<SessionManager>,
    pub supervisor: Arc<ProcessSupervisor>,
    pub embedder: Option<Arc<dyn Embedder>>,
    /// The daemon's ONE workspace-scoped language-server manager. The
    /// semantic fallback and the native diagnostics/code-action routes share
    /// this instance, so one workspace never spawns two servers.
    pub lsp: Arc<LspManager>,
}

impl SemanticSources {
    /// The production composition: every native subsystem wired. A
    /// subsystem that cannot answer for a workspace reports typed
    /// unavailable at query time; it is never silently absent.
    pub fn fallback(&self) -> GenericSemanticFallback {
        let resolver_session = self.session.clone();
        let resolver: crate::impact::RootResolver = Arc::new(move |raw| {
            resolver_session
                .workspace_root(WorkspaceId::new(raw))
                .ok()
                .flatten()
        });
        let lsp = Arc::new(DaemonLspMetadata::new(
            self.lsp.clone(),
            self.session.clone(),
        ));
        let mut fallback = GenericSemanticFallback::default()
            .with_lsp(lsp)
            .with_tree_sitter(Arc::new(DaemonTreeSitterMetadata::new(
                self.session.clone(),
            )))
            .with_git(Arc::new(DaemonGitMetadata::new(
                Arc::new(WorktreeManager::new(self.supervisor.clone())),
                self.session.clone(),
                self.index.clone(),
            )))
            .with_verification(Arc::new(DaemonVerificationMetadata::new(
                self.session.clone(),
                self.supervisor.clone(),
            )))
            .with_impact(Arc::new(crate::impact::WorkspaceImpact::new(resolver)));
        if let Some(index) = &self.index {
            fallback = fallback.with_index(Arc::new(DaemonIndexMetadata::new(
                index.clone(),
                self.embedder.clone(),
            )));
        }
        fallback
    }
}

fn unavailable(subsystem: &str, reason: impl Into<String>) -> SemanticError {
    SemanticError::Unavailable {
        subsystem: subsystem.to_string(),
        reason: reason.into(),
    }
}

fn workspace_root(
    session: &SessionManager,
    workspace: WorkspaceId,
) -> Result<PathBuf, SemanticError> {
    match session.workspace_root(workspace) {
        Ok(Some(root)) => Ok(root),
        Ok(None) => Err(unavailable(
            "workspace",
            format!("workspace {workspace} has no durable root"),
        )),
        Err(error) => Err(unavailable(
            "workspace",
            format!("workspace {workspace} root read failed: {error}"),
        )),
    }
}

fn parse_path(path: &str) -> Result<WorkspacePath, SemanticError> {
    WorkspacePath::parse(path)
}

fn file_entity_id(path: &str) -> String {
    let digest = blake3::hash(path.as_bytes()).to_hex();
    format!("file.{}", &digest[..16])
}

fn bounded_id(text: &str) -> String {
    let trimmed: String = text.chars().take(200).collect();
    if trimmed.is_empty() {
        "entity".to_string()
    } else {
        trimmed
    }
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

pub struct DaemonIndexMetadata {
    index: Arc<IndexService>,
    embedder: Option<Arc<dyn Embedder>>,
}

impl DaemonIndexMetadata {
    pub fn new(index: Arc<IndexService>, embedder: Option<Arc<dyn Embedder>>) -> Self {
        Self { index, embedder }
    }

    fn revision_label(view: &faktor_index::IndexView) -> String {
        format!("gen:{}:{}", view.generation(), view.content_identity())
    }

    fn hit_items(
        workspace: WorkspaceId,
        view: &faktor_index::IndexView,
        hits: Vec<Hit>,
        limit: usize,
    ) -> Vec<SemanticContextItem> {
        let revision = Self::revision_label(view);
        let source = format!("index:generation:{}", view.generation());
        let mut items = Vec::new();
        for hit in hits.into_iter().take(limit) {
            let Ok(path) = parse_path(&hit.path) else {
                continue;
            };
            let (entity_id, kind) = match &hit.symbol {
                Some(symbol) => (bounded_id(&symbol.name), Some(symbol.kind)),
                None => (file_entity_id(&hit.path), None),
            };
            let Ok(entity_id) = SemanticEntityId::parse(&entity_id) else {
                continue;
            };
            let entity = SemanticEntityRef::new(workspace, path, entity_id);
            let excerpt = match kind {
                Some(kind) => format!("{} {:?} at {}: {}", hit.path, kind, hit.path, hit.snippet),
                None => format!("{}: {}", hit.path, hit.snippet),
            };
            let relevance_bps = ((hit.score.clamp(0.0, 100.0) / 100.0) * 10_000.0) as u32;
            items.push(SemanticContextItem {
                provenance: vec![SemanticFindingProvenance::new(
                    entity.entity_id.as_str(),
                    SemanticSubsystem::Index,
                    &source,
                    SemanticFidelity::Heuristic,
                    Some(revision.clone()),
                )],
                entity,
                relevance_bps: relevance_bps.min(10_000),
                excerpt,
            });
        }
        items
    }
}

impl IndexMetadata for DaemonIndexMetadata {
    fn identity(&self) -> String {
        "index".to_string()
    }

    fn snapshot_revision(&self, workspace: WorkspaceId) -> Option<String> {
        self.index
            .view(workspace)
            .map(|view| Self::revision_label(&view))
    }

    fn entity_count(&self, workspace: WorkspaceId) -> u64 {
        let Some(view) = self.index.view(workspace) else {
            return 0;
        };
        let index = view.index();
        let guard = index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (guard.file_count(workspace) + guard.symbol_count(workspace)) as u64
    }

    fn context_items(
        &self,
        workspace: WorkspaceId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SemanticContextItem>, SemanticError> {
        let view = self.index.view(workspace).ok_or_else(|| {
            unavailable("index", "no published index generation for this workspace")
        })?;
        let search = SearchService::new(view.index(), self.embedder.clone());
        let hits = match search.fused(workspace, query, limit) {
            Ok(hits) => hits,
            Err(_) => {
                let mut rankings = Vec::new();
                if let Ok(symbols) = search.symbol(workspace, query, limit.saturating_mul(4)) {
                    rankings.push((2.0, symbols));
                }
                if let Ok(exact) = search.exact(workspace, query, limit.saturating_mul(4)) {
                    rankings.push((1.5, exact));
                }
                if let Ok(lexical) = search.lexical(workspace, query, limit.saturating_mul(4)) {
                    rankings.push((1.0, lexical));
                }
                fuse(&rankings, limit)
            }
        };
        Ok(Self::hit_items(workspace, &view, hits, limit))
    }

    fn impacted_entities(
        &self,
        workspace: WorkspaceId,
        changed: &[SemanticEntityRef],
        _max_depth: u32,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let view = self.index.view(workspace).ok_or_else(|| {
            unavailable("index", "no published index generation for this workspace")
        })?;
        let index = view.index();
        let guard = index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = Vec::new();
        for entity in changed.iter().take(MAX_ENTITY_REFS) {
            if out.len() >= MAX_ENTITY_REFS {
                break;
            }
            let token = entity.entity_id.as_str().to_lowercase();
            for hit in guard.files_for_token(workspace, &token, 64) {
                if out.len() >= MAX_ENTITY_REFS {
                    break;
                }
                let Ok(path) = parse_path(&hit.path) else {
                    continue;
                };
                let id = file_entity_id(&hit.path);
                if let Ok(id) = SemanticEntityId::parse(&id) {
                    out.push(SemanticEntityRef::new(workspace, path.clone(), id));
                }
                for symbol in guard.symbols_for(workspace, &hit.path).into_iter().take(32) {
                    if out.len() >= MAX_ENTITY_REFS {
                        break;
                    }
                    if let Ok(id) = SemanticEntityId::parse(&bounded_id(&symbol.name)) {
                        out.push(SemanticEntityRef::new(workspace, path.clone(), id));
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out.truncate(MAX_ENTITY_REFS);
        Ok(out)
    }

    fn declarations(
        &self,
        workspace: WorkspaceId,
        entity: &SemanticEntityRef,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let view = self.index.view(workspace).ok_or_else(|| {
            unavailable("index", "no published index generation for this workspace")
        })?;
        let index = view.index();
        let guard = index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = Vec::new();
        for (path, symbol) in guard.symbol_lookup(workspace, entity.entity_id.as_str(), 32) {
            let Ok(path) = parse_path(&path) else {
                continue;
            };
            if let Ok(id) = SemanticEntityId::parse(&bounded_id(&symbol.name)) {
                out.push(SemanticEntityRef::new(workspace, path, id));
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn test_entities(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let Some(view) = self.index.view(workspace) else {
            return Err(unavailable(
                "index",
                "no published index generation for this workspace",
            ));
        };
        let index = view.index();
        let guard = index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = Vec::new();
        for (path, symbol) in guard.test_symbols(workspace, MAX_ENTITY_REFS) {
            let Ok(path) = parse_path(&path) else {
                continue;
            };
            if let Ok(id) = SemanticEntityId::parse(&bounded_id(&symbol.name)) {
                out.push(SemanticEntityRef::new(workspace, path, id));
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tree-sitter
// ---------------------------------------------------------------------------

const MAX_PARSER_FILE_BYTES: usize = 2 * 1024 * 1024;

pub struct DaemonTreeSitterMetadata {
    session: Arc<SessionManager>,
}

impl DaemonTreeSitterMetadata {
    pub fn new(session: Arc<SessionManager>) -> Self {
        Self { session }
    }

    fn symbols(
        &self,
        workspace: WorkspaceId,
        path: &WorkspacePath,
    ) -> Result<Vec<IndexSymbol>, SemanticError> {
        let root = workspace_root(&self.session, workspace)?;
        let handle = WorkspaceHandle::open_scoped(workspace, root)
            .map_err(|error| unavailable("tree_sitter", format!("workspace open: {error}")))?;
        let data = handle
            .read(Path::new(path.as_str()), MAX_PARSER_FILE_BYTES)
            .map_err(|error| unavailable("tree_sitter", format!("read {path}: {error}")))?;
        let text = String::from_utf8(data.bytes)
            .map_err(|_| unavailable("tree_sitter", format!("{path} is not UTF-8")))?;
        Ok(faktor_index::parse_symbols(Path::new(path.as_str()), &text))
    }
}

impl TreeSitterMetadata for DaemonTreeSitterMetadata {
    fn identity(&self) -> String {
        "tree-sitter".to_string()
    }

    fn outline(
        &self,
        workspace: WorkspaceId,
        path: &WorkspacePath,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let symbols = self.symbols(workspace, path)?;
        let mut out = Vec::new();
        for symbol in symbols.into_iter().take(MAX_ENTITY_REFS) {
            if symbol.name.is_empty() {
                continue;
            }
            if let Ok(id) = SemanticEntityId::parse(&bounded_id(&symbol.name)) {
                out.push(SemanticEntityRef::new(workspace, path.clone(), id));
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn tests_in(
        &self,
        workspace: WorkspaceId,
        path: &WorkspacePath,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let symbols = self.symbols(workspace, path)?;
        let mut out = Vec::new();
        for symbol in symbols
            .into_iter()
            .filter(|symbol| symbol.kind == IndexSymbolKind::Test)
            .take(MAX_ENTITY_REFS)
        {
            if let Ok(id) = SemanticEntityId::parse(&bounded_id(&symbol.name)) {
                out.push(SemanticEntityRef::new(workspace, path.clone(), id));
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

pub struct DaemonGitMetadata {
    worktrees: Arc<WorktreeManager>,
    session: Arc<SessionManager>,
    index: Option<Arc<IndexService>>,
}

impl DaemonGitMetadata {
    pub fn new(
        worktrees: Arc<WorktreeManager>,
        session: Arc<SessionManager>,
        index: Option<Arc<IndexService>>,
    ) -> Self {
        Self {
            worktrees,
            session,
            index,
        }
    }

    fn changes_for_path(
        &self,
        workspace: WorkspaceId,
        path: &str,
        kind: SemanticDeltaKind,
    ) -> Vec<SemanticDeltaChange> {
        let Some(index) = &self.index else {
            return file_change(workspace, path, kind);
        };
        let Some(view) = index.view(workspace) else {
            return file_change(workspace, path, kind);
        };
        let Ok(rel) = WorkspacePath::parse(path) else {
            return Vec::new();
        };
        let index = view.index();
        let guard = index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let symbols = guard.symbols_for(workspace, path);
        if symbols.is_empty() {
            return vec![SemanticDeltaChange {
                entity: SemanticEntityRef::new(
                    workspace,
                    rel,
                    SemanticEntityId::parse(&file_entity_id(path))
                        .expect("generated file entity id is valid"),
                ),
                kind,
                old_hash: None,
                new_hash: None,
                provenance: Vec::new(),
            }];
        }
        symbols
            .into_iter()
            .take(MAX_ENTITY_REFS)
            .filter_map(|symbol| {
                let id = SemanticEntityId::parse(&bounded_id(&symbol.name)).ok()?;
                Some(SemanticDeltaChange {
                    entity: SemanticEntityRef::new(workspace, rel.clone(), id),
                    kind,
                    old_hash: None,
                    new_hash: None,
                    provenance: Vec::new(),
                })
            })
            .collect()
    }
}

fn file_change(
    workspace: WorkspaceId,
    path: &str,
    kind: SemanticDeltaKind,
) -> Vec<SemanticDeltaChange> {
    let Ok(rel) = WorkspacePath::parse(path) else {
        return Vec::new();
    };
    let Ok(id) = SemanticEntityId::parse(&file_entity_id(path)) else {
        return Vec::new();
    };
    vec![SemanticDeltaChange {
        entity: SemanticEntityRef::new(workspace, rel, id),
        kind,
        old_hash: None,
        new_hash: None,
        provenance: Vec::new(),
    }]
}

impl GitMetadata for DaemonGitMetadata {
    fn identity(&self) -> String {
        "git".to_string()
    }

    fn revision<'a>(
        &'a self,
        workspace: WorkspaceId,
    ) -> BoxFuture<'a, Result<Option<String>, SemanticError>> {
        Box::pin(async move {
            let root = workspace_root(&self.session, workspace)?;
            self.worktrees
                .head_sha(&root, ProcessOwner::Workspace(workspace))
                .await
                .map_err(|error| unavailable("git", format!("HEAD read: {error}")))
        })
    }

    fn changed_entities<'a>(
        &'a self,
        workspace: WorkspaceId,
        from_revision: &'a str,
        to_revision: &'a str,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticDeltaChange>, SemanticError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(SemanticError::Cancelled {
                    provider: "git".to_string(),
                });
            }
            let root = workspace_root(&self.session, workspace)?;
            let output = self
                .worktrees
                .git_read(
                    &root,
                    &[
                        "diff",
                        "--name-status",
                        "-z",
                        "--no-renames",
                        from_revision,
                        to_revision,
                    ],
                    ProcessOwner::Workspace(workspace),
                )
                .await
                .map_err(|error| unavailable("git", format!("git diff: {error}")))?;
            let mut parts = output.split('\0');
            let mut changes = Vec::new();
            while let Some(status) = parts.next() {
                if status.is_empty() {
                    continue;
                }
                let Some(path) = parts.next() else {
                    break;
                };
                if path.is_empty() || changes.len() >= MAX_ENTITY_REFS {
                    continue;
                }
                let kind = match status.as_bytes().first().copied() {
                    Some(b'A') => SemanticDeltaKind::Added,
                    Some(b'D') => SemanticDeltaKind::Removed,
                    _ => SemanticDeltaKind::Modified,
                };
                changes.extend(self.changes_for_path(workspace, path, kind));
                if changes.len() >= MAX_ENTITY_REFS {
                    changes.truncate(MAX_ENTITY_REFS);
                    break;
                }
            }
            Ok(changes)
        })
    }
}

// ---------------------------------------------------------------------------
// Language server
// ---------------------------------------------------------------------------

const LSP_START_TIMEOUT: Duration = Duration::from_secs(60);
const LSP_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const LSP_READY_TIMEOUT: Duration = Duration::from_secs(45);
const LSP_READY_POLL: Duration = Duration::from_millis(500);
const MAX_LSP_FILE_BYTES: usize = 2 * 1024 * 1024;

pub struct DaemonLspMetadata {
    manager: Arc<LspManager>,
    session: Arc<SessionManager>,
    label: Mutex<Option<String>>,
    /// True once the server has answered at least one workspace-symbol
    /// probe (syntactic search may be ready before crate analysis).
    symbols_ready: AtomicBool,
    /// True once a definition probe at a real source position returned a
    /// location: compiler semantics (crate graph) are loaded, so reference
    /// and call-hierarchy requests can be trusted.
    semantic_ready: AtomicBool,
    /// Monotonic document version for full-text didChange syncs.
    version: AtomicI64,
}

impl DaemonLspMetadata {
    pub fn new(manager: Arc<LspManager>, session: Arc<SessionManager>) -> Self {
        Self {
            manager,
            session,
            label: Mutex::new(None),
            symbols_ready: AtomicBool::new(false),
            semantic_ready: AtomicBool::new(false),
            version: AtomicI64::new(1),
        }
    }

    fn label(&self) -> String {
        self.label
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| "lsp".to_string())
    }

    async fn client(&self, workspace: WorkspaceId) -> Result<Arc<LspClient>, SemanticError> {
        if let Ok(client) = self.manager.client(workspace).await {
            return Ok(client);
        }
        let root = workspace_root(&self.session, workspace)?;
        let config = lsp_config_for(&root).ok_or_else(|| {
            unavailable(
                "lsp",
                format!(
                    "no language server configured/installed for {}",
                    root.display()
                ),
            )
        })?;
        *self
            .label
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(format!("lsp:{}", config.name));
        match tokio::time::timeout(LSP_START_TIMEOUT, self.manager.start(workspace, config)).await {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(error)) => Err(unavailable("lsp", format!("server start: {error}"))),
            Err(_) => Err(unavailable(
                "lsp",
                format!("server start exceeded {}s", LSP_START_TIMEOUT.as_secs()),
            )),
        }
    }

    fn open_document(
        &self,
        workspace: WorkspaceId,
        entity: &SemanticEntityRef,
    ) -> Result<(Arc<PathBuf>, String, String), SemanticError> {
        let root = workspace_root(&self.session, workspace)?;
        let handle = WorkspaceHandle::open_scoped(workspace, root.clone())
            .map_err(|error| unavailable("lsp", format!("workspace open: {error}")))?;
        let data = handle
            .read(Path::new(entity.path.as_str()), MAX_LSP_FILE_BYTES)
            .map_err(|error| unavailable("lsp", format!("read {}: {error}", entity.path)))?;
        let text = String::from_utf8(data.bytes)
            .map_err(|_| unavailable("lsp", format!("{} is not UTF-8", entity.path)))?;
        let uri = file_uri(&root.join(entity.path.as_str()));
        Ok((Arc::new(root), uri, text))
    }

    /// Idempotent full-text document sync: didOpen the first time (version
    /// 1), didChange with a monotonic version afterwards. Both clear the
    /// client's revision-keyed semantic cache, which is required for the
    /// readiness probe to observe fresh results.
    fn sync_document(
        &self,
        client: &LspClient,
        uri: &str,
        text: &str,
    ) -> Result<(), SemanticError> {
        if client.document_revision(uri).is_some() {
            let version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
            client
                .did_change(uri, text, version)
                .map_err(|error| unavailable("lsp", format!("didChange: {error}")))
        } else {
            client
                .did_open_version(uri, text, 1)
                .map_err(|error| unavailable("lsp", format!("didOpen: {error}")))
        }
    }

    /// Wait (bounded) until the server answers workspace-symbol probes. A
    /// freshly initialized server returns empty results while it loads the
    /// project; using those without this wait would falsely report "no
    /// references". Every probe is a distinct cache key (prefixes) so the
    /// client's revision-keyed cache cannot pin the pre-ready empty answer.
    async fn wait_ready(
        &self,
        client: &LspClient,
        probe: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), SemanticError> {
        if self.symbols_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let deadline = Instant::now() + LSP_READY_TIMEOUT;
        let candidates = readiness_probes(probe);
        let mut index = 0usize;
        loop {
            if cancellation.is_cancelled() {
                return Err(SemanticError::Cancelled {
                    provider: "lsp".to_string(),
                });
            }
            let query = &candidates[index % candidates.len()];
            let result = client
                .workspace_symbols_with(
                    query,
                    SemanticOptions::new()
                        .with_timeout(Duration::from_secs(10))
                        .with_cancellation(cancellation),
                )
                .await;
            match result {
                Ok(symbols) if !symbols.items.is_empty() || symbols.truncated => {
                    self.symbols_ready.store(true, Ordering::Release);
                    return Ok(());
                }
                Ok(_) => {}
                Err(LspError::Unsupported { .. }) => {
                    // The server simply does not advertise workspace
                    // symbols; the actual capability-gated request decides.
                    self.symbols_ready.store(true, Ordering::Release);
                    return Ok(());
                }
                Err(LspError::Cancelled { .. }) => {
                    return Err(SemanticError::Cancelled {
                        provider: "lsp".to_string(),
                    });
                }
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                // Bounded: proceed, and let the actual request return its
                // own typed result rather than fabricating readiness.
                self.symbols_ready.store(true, Ordering::Release);
                return Ok(());
            }
            index += 1;
            tokio::time::sleep(LSP_READY_POLL).await;
        }
    }

    /// Wait (bounded) for crate analysis: a fresh server can answer
    /// workspace symbols syntactically while definitions/references still
    /// return empty. A non-empty definition at a REAL source position is
    /// the strongest available readiness signal. Each retry re-syncs the
    /// document (new version), which clears the client's revision-keyed
    /// cache so a cached pre-ready empty definition cannot pin the wait.
    async fn wait_semantic_ready(
        &self,
        client: &LspClient,
        uri: &str,
        position: faktor_lsp::Position,
        text: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), SemanticError> {
        if self.semantic_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let deadline = Instant::now() + LSP_READY_TIMEOUT;
        loop {
            if cancellation.is_cancelled() {
                return Err(SemanticError::Cancelled {
                    provider: "lsp".to_string(),
                });
            }
            self.sync_document(client, uri, text)?;
            let options = SemanticOptions::new()
                .with_timeout(Duration::from_secs(10))
                .with_cancellation(cancellation);
            match client.definition_with(uri, position, options).await {
                Ok(result) if !result.items.is_empty() => {
                    self.semantic_ready.store(true, Ordering::Release);
                    return Ok(());
                }
                Ok(_) => {}
                Err(LspError::Unsupported { .. }) => {
                    self.semantic_ready.store(true, Ordering::Release);
                    return Ok(());
                }
                Err(LspError::Cancelled { .. }) => {
                    return Err(SemanticError::Cancelled {
                        provider: "lsp".to_string(),
                    });
                }
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                self.semantic_ready.store(true, Ordering::Release);
                return Ok(());
            }
            tokio::time::sleep(LSP_READY_POLL).await;
        }
    }

    fn entity_for_location(
        &self,
        workspace: WorkspaceId,
        root: &Path,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<SemanticEntityRef> {
        let rel = uri_to_relative(root, uri)?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let path = WorkspacePath::parse(&rel).ok()?;
        let id = format!(
            "loc:{}:{}",
            line.saturating_add(1),
            character.saturating_add(1)
        );
        let id = SemanticEntityId::parse(&id).ok()?;
        Some(SemanticEntityRef::new(workspace, path, id))
    }

    fn symbol_entity(
        &self,
        workspace: WorkspaceId,
        root: &Path,
        symbol: &WorkspaceSymbol,
    ) -> Option<SemanticEntityRef> {
        let rel = uri_to_relative(root, &symbol.location.uri)?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let path = WorkspacePath::parse(&rel).ok()?;
        let id = SemanticEntityId::parse(&bounded_id(&symbol.name)).ok()?;
        Some(SemanticEntityRef::new(workspace, path, id))
    }

    fn call_entity(
        &self,
        workspace: WorkspaceId,
        root: &Path,
        item: &CallHierarchyItem,
    ) -> Option<SemanticEntityRef> {
        let rel = uri_to_relative(root, &item.uri)?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let path = WorkspacePath::parse(&rel).ok()?;
        let id = SemanticEntityId::parse(&bounded_id(&item.name)).ok()?;
        Some(SemanticEntityRef::new(workspace, path, id))
    }

    async fn definitions(
        &self,
        workspace: WorkspaceId,
        entity: &SemanticEntityRef,
        cancellation: CancellationToken,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        let client = self.client(workspace).await?;
        let (root, uri, text) = self.open_document(workspace, entity)?;
        let position = locate_symbol(&text, entity.entity_id.as_str()).ok_or_else(|| {
            unavailable(
                "lsp",
                format!(
                    "entity {} has no source position in {}",
                    entity.entity_id, entity.path
                ),
            )
        })?;
        self.sync_document(&client, &uri, &text)?;
        self.wait_ready(&client, entity.entity_id.as_str(), &cancellation)
            .await?;
        let options = SemanticOptions::new()
            .with_timeout(LSP_REQUEST_TIMEOUT)
            .with_cancellation(&cancellation);
        let result =
            with_content_modified_retry(|| client.definition_with(&uri, position, options))
                .await
                .map_err(map_lsp_error)?;
        let mut out = Vec::new();
        for location in result.items {
            if let Some(entity) = self.entity_for_location(
                workspace,
                &root,
                &location.uri,
                location.selection_range.start.line,
                location.selection_range.start.character,
            ) {
                out.push(entity);
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

#[allow(clippy::result_large_err)]
async fn with_content_modified_retry<T, F, Fut>(mut call: F) -> Result<T, LspError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, LspError>>,
{
    let mut attempts = 0u32;
    loop {
        match call().await {
            Err(LspError::ContentModified { .. }) if attempts < 50 => {
                // The server can report this while it applies our didOpen/
                // didChange or finishes project loading. It is retryable;
                // retry for a bounded 10s window instead of surfacing a
                // transient as "no references".
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            other => return other,
        }
    }
}

fn map_lsp_error(error: LspError) -> SemanticError {
    match error {
        LspError::Cancelled { .. } => SemanticError::Cancelled {
            provider: "lsp".to_string(),
        },
        LspError::Timeout { .. } => SemanticError::ProviderTimeout {
            provider: "lsp".to_string(),
        },
        other => unavailable("lsp", other.to_string()),
    }
}

impl LspMetadata for DaemonLspMetadata {
    fn identity(&self) -> String {
        self.label()
    }

    fn available(&self, workspace: WorkspaceId) -> bool {
        match self.session.workspace_root(workspace) {
            Ok(Some(root)) => lsp_config_for(&root).is_some(),
            _ => false,
        }
    }

    fn definitions<'a>(
        &'a self,
        workspace: WorkspaceId,
        entity: &'a SemanticEntityRef,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
        Box::pin(self.definitions(workspace, entity, cancellation))
    }

    fn references<'a>(
        &'a self,
        workspace: WorkspaceId,
        entity: &'a SemanticEntityRef,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
        Box::pin(async move {
            let client = self.client(workspace).await?;
            let (root, uri, text) = self.open_document(workspace, entity)?;
            let position = locate_symbol(&text, entity.entity_id.as_str()).ok_or_else(|| {
                unavailable(
                    "lsp",
                    format!(
                        "entity {} has no source position in {}",
                        entity.entity_id, entity.path
                    ),
                )
            })?;
            self.sync_document(&client, &uri, &text)?;
            self.wait_semantic_ready(&client, &uri, position, &text, &cancellation)
                .await?;
            let options = SemanticOptions::new()
                .with_timeout(LSP_REQUEST_TIMEOUT)
                .with_cancellation(&cancellation);
            let result = with_content_modified_retry(|| {
                client.references_with(&uri, position, false, options)
            })
            .await
            .map_err(map_lsp_error)?;
            let mut out = Vec::new();
            for location in result.items {
                if let Some(entity) = self.entity_for_location(
                    workspace,
                    &root,
                    &location.uri,
                    location.selection_range.start.line,
                    location.selection_range.start.character,
                ) {
                    out.push(entity);
                }
            }
            out.sort();
            out.dedup();
            Ok(out)
        })
    }

    fn workspace_symbols<'a>(
        &'a self,
        workspace: WorkspaceId,
        query: &'a str,
        limit: usize,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
        Box::pin(async move {
            let client = self.client(workspace).await?;
            let root = workspace_root(&self.session, workspace)?;
            self.wait_ready(&client, query, &cancellation).await?;
            let options = SemanticOptions::new()
                .with_timeout(LSP_REQUEST_TIMEOUT)
                .with_cancellation(&cancellation);
            let result =
                with_content_modified_retry(|| client.workspace_symbols_with(query, options))
                    .await
                    .map_err(map_lsp_error)?;
            let mut out = Vec::new();
            for symbol in result.items.into_iter().take(limit) {
                if let Some(entity) = self.symbol_entity(workspace, &root, &symbol) {
                    out.push(entity);
                }
            }
            out.sort();
            out.dedup();
            Ok(out)
        })
    }

    fn call_hierarchy<'a>(
        &'a self,
        workspace: WorkspaceId,
        entity: &'a SemanticEntityRef,
        incoming: bool,
        max_depth: u32,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<SemanticEntityRef>, SemanticError>> {
        Box::pin(async move {
            let client = self.client(workspace).await?;
            let (root, uri, text) = self.open_document(workspace, entity)?;
            let position = locate_symbol(&text, entity.entity_id.as_str()).ok_or_else(|| {
                unavailable(
                    "lsp",
                    format!(
                        "entity {} has no source position in {}",
                        entity.entity_id, entity.path
                    ),
                )
            })?;
            self.sync_document(&client, &uri, &text)?;
            self.wait_semantic_ready(&client, &uri, position, &text, &cancellation)
                .await?;
            let options = SemanticOptions::new()
                .with_timeout(LSP_REQUEST_TIMEOUT)
                .with_cancellation(&cancellation);
            let prepared = with_content_modified_retry(|| {
                client.prepare_call_hierarchy_with(&uri, position, options)
            })
            .await
            .map_err(map_lsp_error)?;
            let mut out = Vec::new();
            let mut frontier = prepared.items;
            let mut depth = 0u32;
            while depth < max_depth && !frontier.is_empty() && out.len() < MAX_ENTITY_REFS {
                let mut next = Vec::new();
                for item in frontier {
                    if out.len() >= MAX_ENTITY_REFS {
                        break;
                    }
                    if let Some(found) = self.call_entity(workspace, &root, &item) {
                        out.push(found);
                    }
                    if incoming {
                        let calls = with_content_modified_retry(|| {
                            client.incoming_calls_with(&item, options)
                        })
                        .await
                        .map_err(map_lsp_error)?;
                        next.extend(calls.items.into_iter().map(|call| call.from));
                    } else {
                        let calls = with_content_modified_retry(|| {
                            client.outgoing_calls_with(&item, options)
                        })
                        .await
                        .map_err(map_lsp_error)?;
                        next.extend(calls.items.into_iter().map(|call| call.to));
                    }
                }
                frontier = next;
                depth += 1;
            }
            out.sort();
            out.dedup();
            Ok(out)
        })
    }
}

fn readiness_probes(probe: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !probe.is_empty() {
        out.push(probe.to_string());
        let mut chars = probe.chars();
        let half: String = chars.by_ref().take(probe.chars().count() / 2).collect();
        if half.len() >= 2 && half != probe {
            out.push(half);
        }
        if let Some(short) = probe.chars().next() {
            out.push(short.to_string());
        }
    }
    out.push("a".to_string());
    out.sort();
    out.dedup();
    out
}

fn lsp_config_for(root: &Path) -> Option<LspConfig> {
    faktor_lsp::config_for_root(root)
}

fn file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 8);
    out.push_str("file://");
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn uri_to_relative(root: &Path, uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    let decoded = percent_decode(raw)?;
    let path = PathBuf::from(decoded);
    let rel = path.strip_prefix(root).ok()?;
    if rel.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(rel.to_path_buf())
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = *bytes.get(i + 1)?;
            let lo = *bytes.get(i + 2)?;
            out.push((hexval(hi)? << 4) | hexval(lo)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hexval(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn locate_symbol(text: &str, entity_id: &str) -> Option<faktor_lsp::Position> {
    let mut candidates = vec![entity_id.to_string()];
    if let Some(last) = entity_id.rsplit("::").next() {
        if last != entity_id {
            candidates.push(last.to_string());
        }
    }
    if let Some(last) = entity_id.rsplit('.').next() {
        if last != entity_id {
            candidates.push(last.to_string());
        }
    }
    for (line_number, line) in text.lines().take(200_000).enumerate() {
        for candidate in &candidates {
            if candidate.is_empty() {
                continue;
            }
            let Some(byte_index) = find_identifier(line, candidate) else {
                continue;
            };
            let character = line[..byte_index].encode_utf16().count() as u32;
            return Some(faktor_lsp::Position::new(line_number as u32, character));
        }
    }
    None
}

fn find_identifier(line: &str, needle: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(offset) = line[start..].find(needle) {
        let index = start + offset;
        let before = line[..index].chars().next_back();
        let after = line[index + needle.len()..].chars().next();
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        if !before.is_some_and(ident) && !after.is_some_and(ident) {
            return Some(index);
        }
        start = index + needle.len().max(1);
        if start >= line.len() {
            break;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Verification / test discovery
// ---------------------------------------------------------------------------

const MAX_DISCOVERED_CHECKS: usize = 256;

pub struct DaemonVerificationMetadata {
    session: Arc<SessionManager>,
    supervisor: Arc<ProcessSupervisor>,
    cache: Mutex<BTreeMap<WorkspaceId, (String, Discovery)>>,
}

#[derive(Clone, Default)]
struct Discovery {
    checks: Vec<SemanticCheck>,
    tests: Vec<SemanticEntityRef>,
}

impl DaemonVerificationMetadata {
    pub fn new(session: Arc<SessionManager>, supervisor: Arc<ProcessSupervisor>) -> Self {
        Self {
            session,
            supervisor,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    fn discover(&self, workspace: WorkspaceId) -> Result<Discovery, SemanticError> {
        let root = workspace_root(&self.session, workspace)?;
        let revision = head_revision(&root);
        if let Some(revision) = &revision {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((cached_revision, discovery)) = cache.get(&workspace) {
                if cached_revision == revision {
                    return Ok(discovery.clone());
                }
            }
        }
        let inventory = discover_repo_inventory_with_supervisor(
            &root,
            &InventoryBudget::default(),
            &self.supervisor,
        );
        let discovery = discovery_from_inventory(workspace, &root, &inventory);
        if let Some(revision) = revision {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cache.len() >= 32 && !cache.contains_key(&workspace) {
                if let Some(oldest) = cache.keys().next().copied() {
                    cache.remove(&oldest);
                }
            }
            cache.insert(workspace, (revision, discovery.clone()));
        }
        Ok(discovery)
    }
}

fn discovery_from_inventory(
    workspace: WorkspaceId,
    root: &Path,
    inventory: &RepoInventory,
) -> Discovery {
    // The MULTI-COMPONENT profile is the production verification surface;
    // the legacy single-`ProjectType` detection is a frozen compatibility
    // API that production callers must not use.
    let profile = faktor_verify::derive::detect_project_profile(root, &inventory.files);
    let changed: Vec<PathBuf> = inventory.files.iter().map(PathBuf::from).collect();
    let specs = faktor_verify::derive::derive_checks(&profile, &changed)
        .or_else(|_| {
            let bounded: Vec<PathBuf> = changed.iter().take(128).cloned().collect();
            faktor_verify::derive::derive_checks(&profile, &bounded)
        })
        .unwrap_or_default();
    let checks = specs
        .into_iter()
        .take(MAX_DISCOVERED_CHECKS)
        .map(|check| {
            let mut command = check.program.to_string_lossy().into_owned();
            for arg in &check.args {
                command.push(' ');
                command.push_str(&arg.to_string_lossy());
            }
            let command: String = command.chars().take(512).collect();
            SemanticCheck {
                name: check.id,
                // DISCOVERED, not executed. The native semantic metadata
                // pass never claims a passing verdict it did not observe.
                passed: false,
                detail: format!(
                    "discovered {:?} check `{command}` (not executed by the semantic metadata pass)",
                    check.kind
                ),
                provenance: Vec::new(),
            }
        })
        .collect();
    let mut tests = Vec::new();
    for path in inventory.files.iter().take(MAX_ENTITY_REFS) {
        let name = path.rsplit('/').next().unwrap_or(path);
        let stem = name.split('.').next().unwrap_or(name);
        if path.starts_with("tests/")
            || path.contains("/tests/")
            || stem.contains("test")
            || stem.ends_with("_spec")
        {
            if let Ok(path) = WorkspacePath::parse(path) {
                if let Ok(id) = SemanticEntityId::parse(&file_entity_id(path.as_str())) {
                    tests.push(SemanticEntityRef::new(workspace, path, id));
                }
            }
        }
    }
    tests.sort();
    tests.dedup();
    Discovery { checks, tests }
}

impl VerificationMetadata for DaemonVerificationMetadata {
    fn identity(&self) -> String {
        "verification".to_string()
    }

    fn checks(&self, workspace: WorkspaceId) -> Result<Vec<SemanticCheck>, SemanticError> {
        Ok(self.discover(workspace)?.checks)
    }

    fn test_entities(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<SemanticEntityRef>, SemanticError> {
        Ok(self.discover(workspace)?.tests)
    }
}

fn head_revision(root: &Path) -> Option<String> {
    let git = root.join(".git");
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref: ") else {
        return Some(head.to_string());
    };
    if let Ok(value) = std::fs::read_to_string(git.join(reference)) {
        return Some(value.trim().to_string());
    }
    let packed = std::fs::read_to_string(git.join("packed-refs")).ok()?;
    for line in packed.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let oid = parts.next()?;
        let name = parts.next()?;
        if name == reference {
            return Some(oid.to_string());
        }
    }
    None
}
