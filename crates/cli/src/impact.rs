//! Product surface for the repository relationship graph.
//!
//! `faktor impact graph` serializes the ONE graph document; `faktor impact
//! affected` runs the same O(V+E) query the semantic provider uses. The
//! daemon installs [`WorkspaceImpact`] on the semantic fallback, so the
//! product API and the CI selector consume one authority.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use faktor_core::id::WorkspaceId;
use faktor_impact::{
    AffectedOptions, BuildOptions, ChangedTarget, GraphBuilder, ImpactGraph, ImpactSession,
    Revision,
};
use faktor_semantic::fallback::{ImpactMetadata, ImpactMetadataResult, ImpactedFinding};
use faktor_semantic::{
    SemanticEntityId, SemanticEntityRef, SemanticError, WorkspacePath, MAX_ENTITY_REFS,
};

pub type RootResolver = Arc<dyn Fn(u64) -> Option<PathBuf> + Send + Sync>;

struct CachedGraph {
    root: PathBuf,
    session: ImpactSession,
}

/// Semantic seam implementation: lazily builds the workspace graph and
/// refreshes only files that changed since the previous consult.
pub struct WorkspaceImpact {
    resolver: RootResolver,
    cache: Mutex<BTreeMap<u64, CachedGraph>>,
}

impl WorkspaceImpact {
    pub fn new(resolver: RootResolver) -> Self {
        Self {
            resolver,
            cache: Mutex::new(BTreeMap::new()),
        }
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
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let workspace_key = workspace.raw();
        let needs_build = !cache
            .get(&workspace_key)
            .is_some_and(|entry| entry.root == root);
        if needs_build {
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
                },
            );
        }
        let entry = cache
            .get_mut(&workspace_key)
            .expect("cached graph was just inserted");
        // Always re-examine every changed path. The session compares per-path
        // content digests, so a repeated consult for unchanged bytes is a
        // cheap no-op, while a second real edit is never suppressed.
        let mut paths: Vec<String> = changed
            .iter()
            .map(|entity| entity.path.as_str().to_string())
            .collect();
        paths.sort();
        paths.dedup();
        if !paths.is_empty() {
            entry
                .session
                .refresh_material(&paths)
                .map_err(|error| SemanticError::Unavailable {
                    subsystem: "graph".to_string(),
                    reason: error.to_string(),
                })?;
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
    let git = faktor_impact::git::GitAdapter::new(supervisor.clone());
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
    let git = faktor_impact::git::GitAdapter::new(supervisor.clone());
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
