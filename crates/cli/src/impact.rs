//! Product surface for the repository relationship graph.
//!
//! `faktor impact graph` serializes the ONE graph document; `faktor impact
//! affected` runs the same O(V+E) query the semantic provider uses. The
//! daemon installs [`WorkspaceImpact`] on the semantic fallback, so the
//! product API and the CI selector consume one authority.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use faktor_core::id::WorkspaceId;
use faktor_fs::{FsEvent, WorkspaceFileService, WorkspaceHandle};
use faktor_impact::{
    AffectedOptions, BuildOptions, ChangedTarget, GenerationLimits, GraphBuilder, ImpactGraph,
    ImpactProcessOwner, ImpactSession, MaterialGeneration, RefreshMode, RefreshPolicy, Revision,
};
use faktor_semantic::fallback::{ImpactMetadata, ImpactMetadataResult, ImpactedFinding};
use faktor_semantic::{
    SemanticEntityId, SemanticEntityRef, SemanticError, WorkspacePath, MAX_ENTITY_REFS,
};

pub type RootResolver = Arc<dyn Fn(u64) -> Option<PathBuf> + Send + Sync>;

struct WorkspaceWatch {
    handle: WorkspaceHandle,
    events: Option<tokio::sync::broadcast::Receiver<FsEvent>>,
}

struct CachedGraph {
    root: PathBuf,
    session: ImpactSession,
    /// Whole-workspace generation authority for THIS cached graph. It is
    /// advanced only by the daemon watch (and the metadata reconciliation
    /// fallback), never by the paths a query happens to name.
    generation: MaterialGeneration,
    /// The generation the cached graph has completely applied.
    synced: u64,
    watch: Option<WorkspaceWatch>,
}

/// Semantic seam implementation: lazily builds the workspace graph and, on
/// every consult, brings it to the whole-workspace material generation before
/// seeding the query. `affected(changed = A)` therefore runs on a COMPLETE
/// current graph; out-of-band edits to other files are never ignored.
pub struct WorkspaceImpact {
    resolver: RootResolver,
    cache: Mutex<BTreeMap<u64, CachedGraph>>,
    /// The daemon's ONE workspace watch authority. Subscribing taps every
    /// event without consuming the IndexService's own receiver.
    workspaces: Arc<WorkspaceFileService>,
    policy: RefreshPolicy,
    limits: GenerationLimits,
}

impl WorkspaceImpact {
    pub fn new(resolver: RootResolver) -> Self {
        Self::with_workspaces(resolver, WorkspaceFileService::new())
    }

    pub fn with_workspaces(resolver: RootResolver, workspaces: Arc<WorkspaceFileService>) -> Self {
        Self {
            resolver,
            cache: Mutex::new(BTreeMap::new()),
            workspaces,
            policy: RefreshPolicy::default(),
            limits: GenerationLimits::default(),
        }
    }

    /// Test/operator seam: cap incremental breadth and the full-rebuild
    /// budget. The production defaults are the documented bounds.
    #[allow(dead_code)]
    pub fn with_refresh_policy(mut self, policy: RefreshPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Test/operator seam: bound the per-workspace dirty history.
    #[allow(dead_code)]
    pub fn with_generation_limits(mut self, limits: GenerationLimits) -> Self {
        self.limits = limits;
        self
    }

    fn open_watch(&self, workspace: WorkspaceId, root: &Path) -> Option<WorkspaceWatch> {
        let handle = self.workspaces.open(workspace, root.to_path_buf()).ok()?;
        let events = handle.subscribe();
        Some(WorkspaceWatch { handle, events })
    }

    /// Drain every queued watcher event. A lagged or closed tap is an
    /// explicit history loss, never "no changes"; only workspace-relevant
    /// paths (source languages and topology manifests) enter the dirty set.
    fn drain_watch(watch: &mut WorkspaceWatch) -> (Vec<String>, bool) {
        let Some(events) = watch.events.as_mut() else {
            return (Vec::new(), false);
        };
        let mut paths = Vec::new();
        let mut lost = false;
        loop {
            match events.try_recv() {
                Ok(event) => {
                    if event.workspace_id != watch.handle.workspace_id() {
                        continue;
                    }
                    if let Some(path) = relative_event_path(watch.handle.root(), &event.path) {
                        paths.push(path);
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    lost = true;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            }
        }
        (paths, lost)
    }

    /// Advance the generation from the watch, falling back to the bounded
    /// whole-workspace metadata reconciliation when no event was queued
    /// (dropped watch delivery, watcher-less roots, or an edit that landed
    /// before the event was flushed). The reconcile never parses files; the
    /// refresh hashes only the candidate paths.
    fn observe_changes(
        session: &mut ImpactSession,
        watch: &mut Option<WorkspaceWatch>,
        generation: &MaterialGeneration,
    ) -> Result<(), faktor_impact::GraphError> {
        if let Some(watch) = watch.as_mut() {
            let (paths, lost) = Self::drain_watch(watch);
            let paths: Vec<String> = paths
                .into_iter()
                .filter(|path| {
                    session.builder().workspace().crate_for_path(path).is_some()
                        || faktor_impact::is_topology_manifest(path)
                })
                .collect();
            if lost {
                generation.mark_history_lost();
            }
            if !paths.is_empty() {
                generation.observe(paths);
                return Ok(());
            }
            if lost {
                return Ok(());
            }
        }
        let scan = session.scan_material_changes()?;
        if scan.truncated {
            generation.mark_history_lost();
        } else if !scan.seeds.is_empty() {
            generation.observe(scan.seeds);
        }
        Ok(())
    }
}

fn relative_event_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    let relative = relative.to_string_lossy().replace('\\', "/");
    if relative.is_empty() {
        None
    } else {
        Some(relative)
    }
}

impl ImpactMetadata for WorkspaceImpact {
    fn identity(&self) -> String {
        "repository-graph".to_string()
    }

    fn affected(
        &self,
        workspace: WorkspaceId,
        changed: &[SemanticEntityRef],
        max_depth: u32,
        max_results: usize,
    ) -> Result<ImpactMetadataResult, SemanticError> {
        let root = (self.resolver)(workspace.raw()).ok_or_else(|| SemanticError::Unavailable {
            subsystem: "graph".to_string(),
            reason: format!("workspace {workspace} has no resolvable root"),
        })?;
        let root = root.canonicalize().unwrap_or(root);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let workspace_key = workspace.raw();
        let needs_build = !cache
            .get(&workspace_key)
            .is_some_and(|entry| entry.root == root);
        if needs_build {
            let watch = self.open_watch(workspace, &root);
            let session =
                ImpactSession::build_material_for_workspace(&root, workspace).map_err(|error| {
                    SemanticError::Unavailable {
                        subsystem: "graph".to_string(),
                        reason: error.to_string(),
                    }
                })?;
            cache.insert(
                workspace_key,
                CachedGraph {
                    root: root.clone(),
                    session,
                    generation: MaterialGeneration::with_limits(self.limits),
                    synced: 0,
                    watch,
                },
            );
        }
        let entry = cache
            .get_mut(&workspace_key)
            .expect("cached graph was just inserted");
        // The generation — never the query's changed paths — decides what is
        // stale. The watch advances it for out-of-band edits; the bounded
        // metadata reconciliation heals dropped events. `affected(changed=A)`
        // always runs after this catch-up, on a complete current graph.
        WorkspaceImpact::observe_changes(&mut entry.session, &mut entry.watch, &entry.generation)
            .map_err(|error| SemanticError::Unavailable {
            subsystem: "graph".to_string(),
            reason: error.to_string(),
        })?;
        let current = entry.generation.current();
        if entry.synced != current {
            let dirty = entry.generation.dirty_since(entry.synced);
            let report = entry
                .session
                .refresh_from_generation(&dirty, &self.policy)
                .map_err(|error| SemanticError::Unavailable {
                    subsystem: "graph".to_string(),
                    reason: error.to_string(),
                })?;
            // Advance the cached generation ONLY after a complete refresh.
            // A degraded (budget-refused) or truncated result keeps the old
            // marker so the next query retries and every answer stays typed.
            if report.mode != RefreshMode::Degraded && !report.degraded {
                entry.synced = report.generation;
            }
        }
        let targets: Vec<ChangedTarget> = changed
            .iter()
            .take(512)
            .map(|entity| ChangedTarget {
                path: entity.path.as_str().to_string(),
                symbol: symbol_of(&entity.entity_id),
            })
            .collect();
        let result = entry
            .session
            .affected(
                &targets,
                &AffectedOptions {
                    max_depth: max_depth.max(1),
                    max_results: max_results.clamp(1, MAX_ENTITY_REFS),
                    crate_tests: true,
                },
            )
            .map_err(|error| SemanticError::Unavailable {
                subsystem: "graph".to_string(),
                reason: error.to_string(),
            })?;
        let mut findings = Vec::new();
        for node in result.affected.iter().chain(result.tests.iter()) {
            let Some(path) = node.path.as_deref() else {
                continue;
            };
            let Ok(path) = WorkspacePath::parse(path) else {
                continue;
            };
            let Ok(entity_id) = SemanticEntityId::parse(&node.id) else {
                continue;
            };
            let reason = node
                .reasons
                .first()
                .map(|reason| {
                    format!(
                        "{} via {} ({:?})",
                        relation_name(reason.relation),
                        reason.via,
                        reason.provenance
                    )
                })
                .unwrap_or_else(|| "changed".to_string());
            let provenance = node
                .reasons
                .first()
                .map(|reason| format!("{:?}", reason.provenance).to_lowercase())
                .unwrap_or_else(|| "graph".to_string());
            findings.push(ImpactedFinding {
                entity: SemanticEntityRef::heuristic(workspace, path, entity_id),
                is_test: node.kind == faktor_impact::NodeKind::Test,
                reason,
                provenance,
            });
        }
        Ok(ImpactMetadataResult {
            findings,
            truncated: result.truncated,
            revision: result.revision.commit,
            degraded: result.degraded,
        })
    }
}

fn symbol_of(entity_id: &SemanticEntityId) -> Option<String> {
    let raw = entity_id.as_str();
    if !raw.contains('#') {
        return None;
    }
    let name = raw.rsplit('#').next()?.split('@').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn relation_name(relation: faktor_impact::ImpactRelation) -> &'static str {
    use faktor_impact::ImpactRelation as R;
    match relation {
        R::Changed => "changed",
        R::Imports => "imports",
        R::Uses => "uses",
        R::Defines => "defines",
        R::References => "references",
        R::Calls => "calls",
        R::Implements => "implements",
        R::Includes => "includes",
        R::Covers => "covers",
        R::Tests => "tests",
        R::DependsOn => "depends_on",
        R::TestTarget => "test_target",
    }
}

#[derive(Debug, Clone)]
pub struct GraphOptions {
    pub root: PathBuf,
    pub out: Option<PathBuf>,
    pub lsp: bool,
    pub max_files: Option<usize>,
    pub changed: Vec<String>,
    pub selector: bool,
    pub compact: bool,
}

pub async fn graph_command(
    options: GraphOptions,
    supervisor: Arc<faktor_terminal::ProcessSupervisor>,
) -> Result<(), String> {
    // The standalone impact CLI is the system semantic host: it has no
    // durable workspace registration, so its git/LSP children belong to the
    // daemon scope and no user session can reap them.
    let git = faktor_impact::git::GitAdapter::new(supervisor.clone(), ImpactProcessOwner::Daemon);
    let root = options.root.canonicalize().map_err(|e| e.to_string())?;
    let revision = match git.revision(&root).await {
        Ok(revision) => revision,
        Err(error) => {
            eprintln!("impact graph: git revision unavailable ({error}); using HEAD file");
            Revision::read_head(&root)
        }
    };
    let mut builder = GraphBuilder::new(&root, revision).map_err(|e| e.to_string())?;
    if let Some(max_files) = options.max_files {
        builder = builder.with_options(BuildOptions {
            max_files,
            ..BuildOptions::default()
        });
    }
    let mut graph = builder.build().map_err(|e| e.to_string())?;
    if options.lsp {
        let paths = if options.changed.is_empty() {
            graph.files.iter().map(|file| file.path.clone()).collect()
        } else {
            options.changed.clone()
        };
        let stats = faktor_impact::lsp::enrich_graph(
            &mut graph,
            &root,
            &paths,
            &faktor_impact::lsp::LspOptions::default(),
            &supervisor,
            ImpactProcessOwner::Daemon,
        )
        .map_err(|e| e.to_string())?;
        if stats.degraded {
            eprintln!(
                "impact graph: LSP enrichment degraded: {}",
                stats.note.unwrap_or_default()
            );
        }
    }
    let graph = if options.selector {
        graph.selector_projection().map_err(|e| e.to_string())?
    } else {
        graph
    };
    let text = if options.compact || options.selector {
        graph.to_json_compact().map_err(|e| e.to_string())?
    } else {
        graph.to_json_pretty().map_err(|e| e.to_string())?
    };
    write_output(options.out.as_deref(), &text)
}

#[derive(Debug, Clone)]
pub struct AffectedCommandOptions {
    pub root: PathBuf,
    pub out: Option<PathBuf>,
    pub lsp: bool,
    pub base: Option<String>,
    pub worktree: bool,
    pub values: Vec<String>,
    pub symbols: Vec<String>,
    pub max_depth: u32,
    pub max_results: usize,
    pub crate_tests: bool,
}

pub async fn affected_command(
    options: AffectedCommandOptions,
    supervisor: Arc<faktor_terminal::ProcessSupervisor>,
) -> Result<(), String> {
    let root = options.root.canonicalize().map_err(|e| e.to_string())?;
    let git = faktor_impact::git::GitAdapter::new(supervisor.clone(), ImpactProcessOwner::Daemon);
    let revision = git.revision(&root).await.map_err(|e| e.to_string())?;
    let mut changed = options.values;
    if changed.is_empty() {
        changed = git
            .changed_files(&root, options.base.as_deref(), options.worktree)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut targets: Vec<ChangedTarget> =
        changed.iter().cloned().map(ChangedTarget::file).collect();
    for symbol in &options.symbols {
        if let Some(path) = changed.first() {
            targets.push(ChangedTarget::symbol(path.clone(), symbol.clone()));
        }
    }
    if targets.is_empty() {
        return Err("impact affected: no changed files and no --changed-file/--symbol".into());
    }
    let mut builder = GraphBuilder::new(&root, revision).map_err(|e| e.to_string())?;
    let mut graph = builder.build().map_err(|e| e.to_string())?;
    if options.lsp {
        let paths: Vec<String> = targets.iter().map(|target| target.path.clone()).collect();
        let stats = faktor_impact::lsp::enrich_graph(
            &mut graph,
            &root,
            &paths,
            &faktor_impact::lsp::LspOptions::default(),
            &supervisor,
            ImpactProcessOwner::Daemon,
        )
        .map_err(|e| e.to_string())?;
        if stats.degraded {
            eprintln!(
                "impact affected: LSP enrichment degraded: {}",
                stats.note.unwrap_or_default()
            );
        }
    }
    let graph = ImpactGraph::new(graph).map_err(|e| e.to_string())?;
    let result = graph
        .affected(
            &targets,
            &AffectedOptions {
                max_depth: options.max_depth.max(1),
                max_results: options.max_results.clamp(1, 65_536),
                crate_tests: options.crate_tests,
            },
        )
        .map_err(|e| e.to_string())?;
    let text = result.to_json_pretty().map_err(|e| e.to_string())?;
    write_output(options.out.as_deref(), &text)
}

fn write_output(out: Option<&Path>, text: &str) -> Result<(), String> {
    match out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, format!("{text}\n")).map_err(|e| e.to_string())
        }
        None => {
            println!("{text}");
            Ok(())
        }
    }
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum ImpactAction {
    /// Build and serialize the repository relationship graph (the same
    /// document the native semantic provider and the CI selector consume).
    Graph {
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        out: Option<String>,
        /// Enrich changed/all files through rust-analyzer (targeted, bounded).
        #[arg(long)]
        lsp: bool,
        #[arg(long)]
        max_files: Option<usize>,
        /// Restrict LSP enrichment to these workspace-relative files.
        #[arg(long = "changed-file")]
        changed_file: Vec<String>,
        /// Emit the bounded selector projection (dependency/coverage/test
        /// edges) instead of the full caller graph.
        #[arg(long)]
        selector: bool,
        /// Serialize compact JSON (no pretty printing).
        #[arg(long)]
        compact: bool,
    },
    /// Query the impacted files/symbols/tests for changed files/symbols.
    Affected {
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        out: Option<String>,
        #[arg(long)]
        lsp: bool,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        worktree: bool,
        #[arg(long = "changed-file")]
        changed_file: Vec<String>,
        /// Symbol name (applied to the first changed file, or with
        /// --changed-file repeated, to the matching file).
        #[arg(long)]
        symbol: Vec<String>,
        #[arg(long, default_value_t = 6)]
        max_depth: u32,
        #[arg(long, default_value_t = 4096)]
        max_results: usize,
        /// Do not expand every test in an impacted crate; return only tests
        /// proven by coverage/call paths (live symbol queries).
        #[arg(long)]
        no_crate_tests: bool,
    },
}

pub async fn command(
    action: ImpactAction,
    supervisor: Arc<faktor_terminal::ProcessSupervisor>,
) -> Result<(), String> {
    match action {
        ImpactAction::Graph {
            root,
            out,
            lsp,
            max_files,
            changed_file,
            selector,
            compact,
        } => {
            graph_command(
                GraphOptions {
                    root: PathBuf::from(root),
                    out: out.map(PathBuf::from),
                    lsp,
                    max_files,
                    changed: changed_file,
                    selector,
                    compact,
                },
                supervisor,
            )
            .await
        }
        ImpactAction::Affected {
            root,
            out,
            lsp,
            base,
            worktree,
            changed_file,
            symbol,
            max_depth,
            max_results,
            no_crate_tests,
        } => {
            affected_command(
                AffectedCommandOptions {
                    root: PathBuf::from(root),
                    out: out.map(PathBuf::from),
                    lsp,
                    base,
                    worktree,
                    values: changed_file,
                    symbols: symbol,
                    max_depth,
                    max_results,
                    crate_tests: !no_crate_tests,
                },
                supervisor,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_semantic::SemanticProvider;
    use std::fs;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn workspace_impact_returns_real_tests_with_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\n");
        write(
            root,
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        write(
            root,
            "a/src/lib.rs",
            "pub fn answer() -> u32 { 42 }\n\npub fn caller() -> u32 { answer() }\n\n#[test]\nfn checks() { assert_eq!(caller(), 42); }\n",
        );
        let root_path = root.to_path_buf();
        let impact = WorkspaceImpact::new(Arc::new(move |_workspace| Some(root_path.clone())));
        let workspace = WorkspaceId::new(1);
        let changed = vec![SemanticEntityRef::heuristic(
            workspace,
            WorkspacePath::parse("a/src/lib.rs").unwrap(),
            SemanticEntityId::parse("symbol:a/src/lib.rs#answer@1").unwrap(),
        )];
        let result = impact.affected(workspace, &changed, 2, 100).unwrap();
        assert!(!result.findings.is_empty());
        assert!(
            result
                .findings
                .iter()
                .any(|finding| { finding.is_test && !finding.reason.is_empty() }),
            "findings: {result:?}"
        );
        assert!(result.findings.iter().any(|finding| finding
            .entity
            .entity_id
            .as_str()
            .contains("caller")));
        assert!(!result.degraded);
        let fallback =
            faktor_semantic::GenericSemanticFallback::default().with_impact(Arc::new(impact));
        let request = faktor_semantic::AffectedRequest {
            call: faktor_semantic::SemanticCall::new(
                faktor_core::OpId::new(1),
                faktor_core::SessionId::new(1),
                workspace,
                0,
                faktor_core::CancellationToken::new(),
            ),
            workspace,
            snapshot_id: faktor_semantic::SemanticSnapshotId::derive(
                workspace,
                "rev",
                &fallback.id(),
                fallback.version(),
                1,
            ),
            changed,
            max_depth: 2,
        };
        let envelope = block_on(fallback.affected(request)).unwrap();
        assert!(!envelope.payload.tests.is_empty());
        assert!(envelope
            .payload
            .provenance
            .iter()
            .any(|entry| entry.subsystem == faktor_semantic::types::SemanticSubsystem::Graph));
    }

    #[test]
    fn workspace_impact_observes_a_second_edit_to_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\n");
        write(
            root,
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        write(root, "a/src/lib.rs", "pub fn base() -> u32 { 1 }\n");
        let root_path = root.to_path_buf();
        let impact = WorkspaceImpact::new(Arc::new(move |_workspace| Some(root_path.clone())));
        let workspace = WorkspaceId::new(1);
        let changed = vec![SemanticEntityRef::heuristic(
            workspace,
            WorkspacePath::parse("a/src/lib.rs").unwrap(),
            SemanticEntityId::parse("file:a/src/lib.rs").unwrap(),
        )];

        write(
            root,
            "a/src/lib.rs",
            "pub fn base() -> u32 { 1 }\npub fn first_edit() -> u32 { base() }\n",
        );
        let first = impact.affected(workspace, &changed, 2, 100).unwrap();
        assert!(
            first.findings.iter().any(|finding| finding
                .entity
                .entity_id
                .as_str()
                .contains("first_edit")),
            "first edit must be indexed: {first:?}"
        );

        write(
            root,
            "a/src/lib.rs",
            "pub fn base() -> u32 { 1 }\npub fn second_edit() -> u32 { base() }\n",
        );
        let second = impact.affected(workspace, &changed, 2, 100).unwrap();
        assert!(
            second.findings.iter().any(|finding| finding
                .entity
                .entity_id
                .as_str()
                .contains("second_edit")),
            "a second edit to the same path must be observed: {second:?}"
        );
        assert!(
            !second.findings.iter().any(|finding| finding
                .entity
                .entity_id
                .as_str()
                .contains("first_edit")),
            "the first edit must not survive the second refresh: {second:?}"
        );
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use std::fs;
    use std::sync::Barrier;
    use std::thread;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"b\"]\n",
        );
        write(
            root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        );
        write(root, "app/src/lib.rs", "pub fn dispatch() -> u32 { 1 }\n");
        write(
            root,
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\n\n[dependencies]\napp = { path = \"../app\" }\n",
        );
        write(root, "b/src/lib.rs", "pub fn unrelated() -> u32 { 0 }\n");
        dir
    }

    fn app_change(workspace: WorkspaceId) -> Vec<SemanticEntityRef> {
        vec![SemanticEntityRef::heuristic(
            workspace,
            WorkspacePath::parse("app/src/lib.rs").unwrap(),
            SemanticEntityId::parse("file:app/src/lib.rs").unwrap(),
        )]
    }

    fn names(result: &ImpactMetadataResult) -> Vec<&str> {
        result
            .findings
            .iter()
            .map(|finding| finding.entity.entity_id.as_str())
            .collect()
    }

    #[test]
    fn out_of_band_edit_of_another_file_is_reflected_in_affected_query() {
        let dir = workspace();
        let root = dir.path().to_path_buf();
        let impact = WorkspaceImpact::new(Arc::new(move |_workspace| Some(root.clone())));
        let workspace = WorkspaceId::new(1);
        let changed = app_change(workspace);

        let first = impact.affected(workspace, &changed, 4, 100).unwrap();
        assert!(
            !names(&first).iter().any(|name| name.contains("target")),
            "b must not be impacted before its out-of-band edit: {first:?}"
        );

        // The query still names ONLY app/src/lib.rs. b is edited out of band
        // and must be picked up from the whole-workspace generation.
        write(
            dir.path(),
            "b/src/lib.rs",
            "use app::dispatch;\n\npub fn target() -> u32 { dispatch() }\n",
        );
        let second = impact.affected(workspace, &changed, 4, 100).unwrap();
        assert!(
            names(&second).iter().any(|name| name.contains("target")),
            "the out-of-band b -> app call edge must be reflected: {second:?}"
        );
        assert!(!second.degraded);
        assert!(second
            .findings
            .iter()
            .any(|finding| finding.reason.contains("calls")));
    }

    #[test]
    fn concurrent_queries_during_refresh_single_flight_one_consistent_graph() {
        let dir = workspace();
        let root = dir.path().to_path_buf();
        let impact = Arc::new(WorkspaceImpact::new(Arc::new(move |_workspace| {
            Some(root.clone())
        })));
        let workspace = WorkspaceId::new(1);
        let changed = app_change(workspace);
        impact.affected(workspace, &changed, 4, 100).unwrap();

        write(
            dir.path(),
            "b/src/lib.rs",
            "use app::dispatch;\n\npub fn target() -> u32 { dispatch() }\n",
        );
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let impact = impact.clone();
            let barrier = barrier.clone();
            let changed = changed.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                impact.affected(workspace, &changed, 4, 100).unwrap()
            }));
        }
        let results: Vec<ImpactMetadataResult> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        for result in &results {
            assert_eq!(
                result, &results[0],
                "every concurrent query must observe ONE consistent generation"
            );
            assert!(
                names(result).iter().any(|name| name.contains("target")),
                "every concurrent query must observe the refreshed graph: {result:?}"
            );
        }
    }

    #[test]
    fn rapid_multi_file_edits_are_refreshed_before_the_next_query() {
        let dir = workspace();
        let root = dir.path().to_path_buf();
        let impact = WorkspaceImpact::new(Arc::new(move |_workspace| Some(root.clone())));
        let workspace = WorkspaceId::new(1);
        let changed = app_change(workspace);
        impact.affected(workspace, &changed, 4, 100).unwrap();

        write(
            dir.path(),
            "app/src/lib.rs",
            "pub fn dispatch() -> u32 { 2 }\npub fn extra() -> u32 { 3 }\n",
        );
        write(
            dir.path(),
            "b/src/lib.rs",
            "use app::extra;\n\npub fn target() -> u32 { extra() }\n",
        );
        let result = impact.affected(workspace, &changed, 4, 100).unwrap();
        assert!(names(&result)
            .iter()
            .any(|name| name.contains("extra") || name.contains("target")));
        assert!(result
            .findings
            .iter()
            .any(|finding| finding.reason.contains("calls")));
    }
}
