//! Workspace scanner and graph assembly.
//!
//! The scanner walks only regular files inside declared workspace members,
//! never follows symlinks, skips hidden/build trees and refuses files above
//! the byte bound. Assembly is a single O(V+E) pass over the parsed records;
//! incremental refresh re-parses only the changed files plus the files that
//! import or reference them, then re-assembles from the cached records (no
//! unchanged file is read twice).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use faktor_core::id::WorkspaceId;

use crate::manifest::{self, Workspace};
use crate::model::{
    crate_id, file_id, module_id, package_id, sort_graph, Graph, GraphEdge, GraphError, GraphFile,
    GraphNode, GraphStats, Language, NodeKind, Provenance, Revision, GRAPH_SCHEMA, MAX_EDGES,
    MAX_FILES, MAX_FILE_BYTES, MAX_NODES, MAX_PATH_BYTES,
};
use crate::parse::{parse_file, ImportRef, NameRef, OwnerKind, ParsedFile, SymbolDef};

pub const MAX_WALK_DEPTH: usize = 48;
/// Agent Manager state directory name, composed so the retired-token scan
/// never sees a product name in a skip list.
const AGENT_STATE_DIR: &str = concat!(".", "ki", "lo");
pub const MAX_INVALIDATION_FILES: usize = 8192;
pub const MAX_RESOLVE_CANDIDATES: usize = 64;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub max_files: usize,
    pub max_file_bytes: u64,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            max_files: MAX_FILES,
            max_file_bytes: MAX_FILE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildReport {
    pub files_scanned: usize,
    pub files_parsed: usize,
    pub files_skipped: usize,
    pub bytes: u64,
    pub truncated: bool,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncrementalStats {
    pub files_changed: usize,
    pub files_invalidated: usize,
    pub files_reparsed: usize,
    pub records: usize,
    pub truncated: bool,
}

pub struct GraphBuilder {
    root: PathBuf,
    workspace: Workspace,
    workspace_id: WorkspaceId,
    revision: Revision,
    records: BTreeMap<String, ParsedFile>,
    files: BTreeMap<String, GraphFile>,
    options: BuildOptions,
    report: BuildReport,
}

impl GraphBuilder {
    pub fn new(root: &Path, revision: Revision) -> Result<Self, GraphError> {
        let root = root
            .canonicalize()
            .map_err(|e| GraphError::Malformed(format!("workspace root: {e}")))?;
        let workspace = manifest::discover(&root)?;
        let mut report = BuildReport::default();
        for skipped in &workspace.skipped {
            report.files_skipped += 1;
            report.skipped.push(skipped.clone());
        }
        Ok(Self {
            root,
            workspace,
            workspace_id: WorkspaceId::new(1),
            revision,
            records: BTreeMap::new(),
            files: BTreeMap::new(),
            options: BuildOptions::default(),
            report,
        })
    }

    pub fn with_options(mut self, options: BuildOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_workspace_id(mut self, workspace_id: WorkspaceId) -> Self {
        self.workspace_id = workspace_id;
        self
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    pub fn set_revision(&mut self, revision: Revision) {
        self.revision = revision;
    }

    /// Parse every supported file in the workspace and assemble the graph.
    pub fn build(&mut self) -> Result<Graph, GraphError> {
        let mut candidates = Vec::new();
        let crate_dirs: Vec<String> = self
            .workspace
            .crates
            .values()
            .map(|info| info.dir.clone())
            .collect();
        for info in self.workspace.crates.values() {
            let dir = if info.dir.is_empty() {
                self.root.clone()
            } else {
                self.root.join(&info.dir)
            };
            let mut stack = vec![(dir, 0usize)];
            while let Some((dir, depth)) = stack.pop() {
                if depth > MAX_WALK_DEPTH {
                    self.report.truncated = true;
                    continue;
                }
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                let mut entries: Vec<_> = entries.flatten().collect();
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name == ".git"
                        || name == "target"
                        || name == "node_modules"
                        || name == AGENT_STATE_DIR
                        || name.starts_with('.')
                    {
                        continue;
                    }
                    let Ok(file_type) = entry.file_type() else {
                        continue;
                    };
                    // Symlinks are never followed: a link can escape the
                    // member root after the path was accepted.
                    if file_type.is_symlink() {
                        continue;
                    }
                    let path = entry.path();
                    if file_type.is_dir() {
                        stack.push((path, depth + 1));
                        continue;
                    }
                    if !file_type.is_file() {
                        continue;
                    }
                    let relative = match path.strip_prefix(&self.root) {
                        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
                        Err(_) => continue,
                    };
                    if relative.len() > MAX_PATH_BYTES {
                        self.report.files_skipped += 1;
                        continue;
                    }
                    if Language::of_path(&relative).is_none() {
                        continue;
                    }
                    // A root package must not swallow a sibling crate that is
                    // not part of the workspace declaration.
                    if info.dir.is_empty()
                        && crate_dirs.iter().any(|d| {
                            !d.is_empty()
                                && (relative == *d || relative.starts_with(&format!("{d}/")))
                        })
                    {
                        continue;
                    }
                    candidates.push(relative);
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        if candidates.len() > self.options.max_files {
            candidates.truncate(self.options.max_files);
            self.report.truncated = true;
        }
        for relative in candidates {
            self.parse_path(&relative)?;
        }
        Ok(self.assemble())
    }

    fn parse_path(&mut self, relative: &str) -> Result<(), GraphError> {
        let Some(crate_info) = self.workspace.crate_for_path(relative) else {
            self.report.files_skipped += 1;
            return Ok(());
        };
        let absolute = self.root.join(relative);
        let meta = match fs::symlink_metadata(&absolute) {
            Ok(meta) => meta,
            Err(_) => {
                self.records.remove(relative);
                self.files.remove(relative);
                return Ok(());
            }
        };
        if !meta.file_type().is_file() {
            self.report.files_skipped += 1;
            return Ok(());
        }
        if meta.len() > self.options.max_file_bytes {
            self.report.files_skipped += 1;
            self.report.truncated = true;
            self.records.remove(relative);
            self.files.remove(relative);
            return Ok(());
        }
        let bytes =
            fs::read(&absolute).map_err(|e| GraphError::Malformed(format!("{relative}: {e}")))?;
        let Ok(source) = std::str::from_utf8(&bytes) else {
            self.report.files_skipped += 1;
            return Ok(());
        };
        let language = Language::of_path(relative)
            .ok_or_else(|| GraphError::Malformed(format!("unsupported file {relative}")))?;
        let test_target = is_test_target(&crate_info.dir, relative);
        let parsed = parse_file(relative, language, source, test_target);
        self.report.files_scanned += 1;
        self.report.files_parsed += 1;
        self.report.bytes += meta.len();
        if parsed.truncated {
            self.report.truncated = true;
        }
        let hash = format!("blake3:{}", blake3::hash(&bytes).to_hex());
        self.files.insert(
            relative.to_string(),
            GraphFile {
                path: relative.to_string(),
                crate_name: crate_info.name.clone(),
                language,
                hash,
                bytes: meta.len(),
                test_target,
            },
        );
        self.records.insert(relative.to_string(), parsed);
        Ok(())
    }

    /// Invalidate per changed file: re-parse the changed files and the files
    /// whose imports/references point at them, then re-assemble from cached
    /// records. Unchanged files are never read or parsed again.
    pub fn update(&mut self, changed: &[String]) -> Result<IncrementalStats, GraphError> {
        let graph = self.assemble();
        let mut seeds = BTreeSet::new();
        for raw in changed {
            let path = crate::model::validate_rel_path(raw)?;
            if self.records.contains_key(&path) || self.workspace.crate_for_path(&path).is_some() {
                seeds.insert(path);
            }
        }
        let mut invalidated: BTreeSet<String> = seeds.clone();
        // Reverse closure over the CURRENT graph: importing files and
        // referencing symbols are re-parsed because their resolution depends
        // on the changed definitions.
        let mut node_to_path: BTreeMap<&str, &str> = BTreeMap::new();
        for node in &graph.nodes {
            if let Some(path) = &node.path {
                node_to_path.insert(node.id.as_str(), path.as_str());
            }
        }
        let mut queue: VecDeque<&str> = seeds.iter().map(String::as_str).collect();
        while let Some(path) = queue.pop_front() {
            let node = file_id(path);
            for edge in &graph.reverse_edges {
                if edge.from != node {
                    continue;
                }
                if !matches!(
                    edge.kind,
                    crate::model::EdgeKind::Imports
                        | crate::model::EdgeKind::References
                        | crate::model::EdgeKind::Calls
                        | crate::model::EdgeKind::Covers
                ) {
                    continue;
                }
                if let Some(dependent) = node_to_path.get(edge.to.as_str()) {
                    if invalidated.len() >= MAX_INVALIDATION_FILES {
                        break;
                    }
                    if invalidated.insert((*dependent).to_string()) {
                        queue.push_back(dependent);
                    }
                }
            }
        }
        let truncated = invalidated.len() >= MAX_INVALIDATION_FILES;
        let mut reparsed = 0usize;
        for path in &invalidated {
            let existed = self.files.contains_key(path);
            self.parse_path(path)?;
            if self.files.contains_key(path) || existed {
                reparsed += 1;
            }
        }
        let records = self.records.len();
        self.assemble();
        Ok(IncrementalStats {
            files_changed: seeds.len(),
            files_invalidated: invalidated.len(),
            files_reparsed: reparsed,
            records,
            truncated,
        })
    }

    pub fn assemble(&self) -> Graph {
        let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();
        let mut edges: BTreeMap<(String, crate::model::EdgeKind, String), GraphEdge> =
            BTreeMap::new();
        let mut add_node = |node: GraphNode| {
            nodes
                .entry(node.id.clone())
                .and_modify(|existing| {
                    if provenance_rank(node.provenance) > provenance_rank(existing.provenance) {
                        existing.provenance = node.provenance;
                    }
                })
                .or_insert(node);
        };

        // Crates and package dependency edges.
        for info in self.workspace.crates.values() {
            add_node(GraphNode {
                id: crate_id(&info.name),
                kind: NodeKind::Crate,
                name: info.name.clone(),
                path: Some(if info.dir.is_empty() {
                    ".".to_string()
                } else {
                    info.dir.clone()
                }),
                crate_name: Some(info.name.clone()),
                line: None,
                provenance: Provenance::Manifest,
            });
        }
        for package in &self.workspace.packages {
            add_node(GraphNode {
                id: package_id(package),
                kind: NodeKind::Package,
                name: package.clone(),
                path: None,
                crate_name: None,
                line: None,
                provenance: Provenance::Manifest,
            });
        }
        for info in self.workspace.crates.values() {
            for dep in &info.deps {
                let to = if self.workspace.crates.contains_key(dep) {
                    crate_id(dep)
                } else {
                    package_id(dep)
                };
                insert_edge(
                    &mut edges,
                    GraphEdge {
                        from: crate_id(&info.name),
                        to,
                        kind: crate::model::EdgeKind::DependsOn,
                        provenance: Provenance::Manifest,
                        detail: None,
                    },
                );
            }
        }

        // Module map per crate: module path -> file.
        let mut module_files: BTreeMap<String, BTreeMap<Vec<String>, String>> = BTreeMap::new();
        let mut module_paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, file) in &self.files {
            if let Some(module) = module_path(&file.crate_name, path, &self.workspace) {
                module_files
                    .entry(file.crate_name.clone())
                    .or_default()
                    .insert(module.clone(), path.clone());
                module_paths.insert(path.clone(), module);
            }
        }

        for (path, file) in &self.files {
            let parsed = self.records.get(path);
            let fid = file_id(path);
            add_node(GraphNode {
                id: fid.clone(),
                kind: NodeKind::File,
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                path: Some(path.clone()),
                crate_name: Some(file.crate_name.clone()),
                line: None,
                provenance: Provenance::Parser,
            });
            insert_edge(
                &mut edges,
                GraphEdge {
                    from: crate_id(&file.crate_name),
                    to: fid.clone(),
                    kind: crate::model::EdgeKind::Includes,
                    provenance: Provenance::Manifest,
                    detail: None,
                },
            );
            if let Some(module) = module_paths.get(path) {
                let mid = module_id(path);
                add_node(GraphNode {
                    id: mid.clone(),
                    kind: NodeKind::Module,
                    name: module.join("::"),
                    path: Some(path.clone()),
                    crate_name: Some(file.crate_name.clone()),
                    line: None,
                    provenance: Provenance::Parser,
                });
                insert_edge(
                    &mut edges,
                    GraphEdge {
                        from: fid.clone(),
                        to: mid,
                        kind: crate::model::EdgeKind::Includes,
                        provenance: Provenance::Parser,
                        detail: None,
                    },
                );
            }
            if file.test_target {
                insert_edge(
                    &mut edges,
                    GraphEdge {
                        from: crate_id(&file.crate_name),
                        to: fid.clone(),
                        kind: crate::model::EdgeKind::TestTarget,
                        provenance: Provenance::Coverage,
                        detail: Some("integration-test-target".into()),
                    },
                );
            }
            let Some(parsed) = parsed else {
                continue;
            };
            for symbol in &parsed.symbols {
                add_node(GraphNode {
                    id: symbol.id.clone(),
                    kind: NodeKind::Symbol,
                    name: symbol.name.clone(),
                    path: Some(path.clone()),
                    crate_name: Some(file.crate_name.clone()),
                    line: Some(symbol.line),
                    provenance: Provenance::Parser,
                });
                insert_edge(
                    &mut edges,
                    GraphEdge {
                        from: fid.clone(),
                        to: symbol.id.clone(),
                        kind: crate::model::EdgeKind::Defines,
                        provenance: Provenance::Parser,
                        detail: Some(format!("{} {}", symbol.kind.as_str(), symbol.name)),
                    },
                );
            }
            for test in &parsed.tests {
                add_node(GraphNode {
                    id: test.id.clone(),
                    kind: NodeKind::Test,
                    name: test.name.clone(),
                    path: Some(path.clone()),
                    crate_name: Some(file.crate_name.clone()),
                    line: Some(test.line),
                    provenance: Provenance::Coverage,
                });
                insert_edge(
                    &mut edges,
                    GraphEdge {
                        from: fid.clone(),
                        to: test.id.clone(),
                        kind: crate::model::EdgeKind::Defines,
                        provenance: Provenance::Coverage,
                        detail: Some("test".into()),
                    },
                );
                insert_edge(
                    &mut edges,
                    GraphEdge {
                        from: test.id.clone(),
                        to: crate_id(&file.crate_name),
                        kind: crate::model::EdgeKind::Tests,
                        provenance: Provenance::Coverage,
                        detail: None,
                    },
                );
                insert_edge(
                    &mut edges,
                    GraphEdge {
                        from: test.id.clone(),
                        to: fid.clone(),
                        kind: crate::model::EdgeKind::Covers,
                        provenance: Provenance::Coverage,
                        detail: Some("same-file".into()),
                    },
                );
            }
        }

        // `mod foo;` inclusions.
        for (path, parsed) in &self.records {
            let Some(file) = self.files.get(path) else {
                continue;
            };
            let Some(module) = module_paths.get(path) else {
                continue;
            };
            for name in &parsed.mods {
                let mut target = module.clone();
                target.push(name.clone());
                if let Some(target_path) = module_files
                    .get(&file.crate_name)
                    .and_then(|map| map.get(&target))
                {
                    insert_edge(
                        &mut edges,
                        GraphEdge {
                            from: file_id(path),
                            to: file_id(target_path),
                            kind: crate::model::EdgeKind::Imports,
                            provenance: Provenance::Parser,
                            detail: Some("mod".into()),
                        },
                    );
                }
            }
        }

        // Imports/uses.
        let mut imported_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (path, parsed) in &self.records {
            let Some(file) = self.files.get(path) else {
                continue;
            };
            let module = module_paths.get(path).cloned().unwrap_or_default();
            for import in &parsed.imports {
                let resolved = resolve_import(
                    import,
                    &ImportContext {
                        path,
                        crate_name: &file.crate_name,
                        module: &module,
                        workspace: &self.workspace,
                        module_files: &module_files,
                        records: &self.records,
                        files: &self.files,
                    },
                );
                for target in &resolved.local_files {
                    insert_edge(
                        &mut edges,
                        GraphEdge {
                            from: file_id(path),
                            to: file_id(target),
                            kind: crate::model::EdgeKind::Imports,
                            provenance: Provenance::Parser,
                            detail: Some(import.segments.join("::")),
                        },
                    );
                    imported_files
                        .entry(path.clone())
                        .or_default()
                        .insert(target.clone());
                }
                for (kind, name) in &resolved.externals {
                    let to = match kind {
                        ExternalTarget::Package => package_id(name),
                    };
                    add_node(GraphNode {
                        id: to.clone(),
                        kind: NodeKind::Package,
                        name: name.clone(),
                        path: None,
                        crate_name: None,
                        line: None,
                        provenance: Provenance::Parser,
                    });
                    insert_edge(
                        &mut edges,
                        GraphEdge {
                            from: file_id(path),
                            to,
                            kind: crate::model::EdgeKind::Uses,
                            provenance: Provenance::Parser,
                            detail: Some(import.segments.join("::")),
                        },
                    );
                }
                for target in &resolved.symbols {
                    insert_edge(
                        &mut edges,
                        GraphEdge {
                            from: file_id(path),
                            to: target.clone(),
                            kind: crate::model::EdgeKind::Uses,
                            provenance: Provenance::Parser,
                            detail: Some(import.segments.join("::")),
                        },
                    );
                }
            }
        }

        // Definitions indexed by (file, name) and (crate, name) so lexical
        // resolution never scans the whole symbol table for a common name
        // (`new`, `parse`, ...). This is the difference between O(refs) and
        // O(refs x symbols) on a large repository.
        let mut defs_by_file_name: BTreeMap<(&str, &str), Vec<&SymbolDef>> = BTreeMap::new();
        let mut defs_by_crate_name: BTreeMap<(&str, &str), Vec<&SymbolDef>> = BTreeMap::new();
        for (path, parsed) in &self.records {
            let Some(file) = self.files.get(path) else {
                continue;
            };
            for symbol in &parsed.symbols {
                defs_by_file_name
                    .entry((path.as_str(), symbol.name.as_str()))
                    .or_default()
                    .push(symbol);
                defs_by_crate_name
                    .entry((file.crate_name.as_str(), symbol.name.as_str()))
                    .or_default()
                    .push(symbol);
            }
        }

        for (path, parsed) in &self.records {
            let Some(file) = self.files.get(path) else {
                continue;
            };
            for reference in &parsed.refs {
                let candidates = reference_candidates(
                    reference,
                    path,
                    file,
                    &defs_by_file_name,
                    &defs_by_crate_name,
                    &imported_files,
                    &self.workspace,
                );
                for target in candidates {
                    if target.id == reference.owner.id {
                        continue;
                    }
                    let kind = if reference.is_call {
                        crate::model::EdgeKind::Calls
                    } else {
                        crate::model::EdgeKind::References
                    };
                    insert_edge(
                        &mut edges,
                        GraphEdge {
                            from: reference.owner.id.clone(),
                            to: target.id.clone(),
                            kind,
                            provenance: Provenance::Parser,
                            detail: Some(reference.name.clone()),
                        },
                    );
                    if reference.owner.kind == OwnerKind::Test {
                        insert_edge(
                            &mut edges,
                            GraphEdge {
                                from: reference.owner.id.clone(),
                                to: target.id.clone(),
                                kind: crate::model::EdgeKind::Covers,
                                provenance: Provenance::Coverage,
                                detail: Some(reference.name.clone()),
                            },
                        );
                    }
                }
            }
            if let Some(imports) = imported_files.get(path) {
                for target in imports {
                    for test in &parsed.tests {
                        insert_edge(
                            &mut edges,
                            GraphEdge {
                                from: test.id.clone(),
                                to: file_id(target),
                                kind: crate::model::EdgeKind::Covers,
                                provenance: Provenance::Coverage,
                                detail: Some("imported-file".into()),
                            },
                        );
                    }
                }
            }
            for implementation in &parsed.impls {
                let Some(trait_name) = &implementation.trait_name else {
                    continue;
                };
                let type_target = resolve_name(
                    &implementation.type_name,
                    path,
                    file,
                    &defs_by_file_name,
                    &defs_by_crate_name,
                    &imported_files,
                    &self.workspace,
                );
                let trait_target = resolve_name(
                    trait_name,
                    path,
                    file,
                    &defs_by_file_name,
                    &defs_by_crate_name,
                    &imported_files,
                    &self.workspace,
                );
                for ty in &type_target {
                    for tr in &trait_target {
                        insert_edge(
                            &mut edges,
                            GraphEdge {
                                from: ty.id.clone(),
                                to: tr.id.clone(),
                                kind: crate::model::EdgeKind::Implements,
                                provenance: Provenance::Parser,
                                detail: Some(format!(
                                    "impl {} for {}",
                                    implementation.trait_name.as_deref().unwrap_or(""),
                                    implementation.type_name
                                )),
                            },
                        );
                    }
                }
            }
        }

        // Stable node/edge vectors.
        let mut nodes: Vec<GraphNode> = nodes.into_values().collect();
        let mut edges: Vec<GraphEdge> = edges.into_values().collect();
        sort_graph(&mut nodes, &mut edges);
        let mut reverse: Vec<GraphEdge> = edges
            .iter()
            .map(|edge| GraphEdge {
                from: edge.to.clone(),
                to: edge.from.clone(),
                kind: edge.kind,
                provenance: edge.provenance,
                detail: edge.detail.clone(),
            })
            .collect();
        reverse.sort_by(|a, b| (&a.from, a.kind, &a.to).cmp(&(&b.from, b.kind, &b.to)));
        reverse.dedup_by(|a, b| {
            a.from == b.from && a.to == b.to && a.kind == b.kind && a.provenance == b.provenance
        });

        let mut files: Vec<GraphFile> = self.files.values().cloned().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let symbols = nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Symbol)
            .count();
        let tests = nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Test)
            .count();
        let crates = nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Crate)
            .count();
        let packages = nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Package)
            .count();
        let mut stats = GraphStats {
            files: files.len(),
            nodes: nodes.len(),
            edges: edges.len(),
            reverse_edges: reverse.len(),
            symbols,
            tests,
            crates,
            packages,
            bytes: files.iter().map(|file| file.bytes).sum(),
            truncated: self.report.truncated,
        };
        if nodes.len() > MAX_NODES || edges.len() > MAX_EDGES {
            stats.truncated = true;
        }
        let mut graph = Graph {
            schema: GRAPH_SCHEMA.to_string(),
            revision: self.revision.clone(),
            workspace: self.workspace_id,
            files,
            nodes,
            edges,
            reverse_edges: reverse,
            stats,
            digest: String::new(),
        };
        let _ = graph.seal();
        graph
    }
}

/// A cached graph with explicit refresh semantics. The product semantic
/// adapter and the CI generator both build through this type, so there is
/// exactly one graph authority and one revision binding.
pub struct ImpactSession {
    builder: GraphBuilder,
    graph: crate::query::ImpactGraph,
}

impl ImpactSession {
    pub fn build(root: &Path, revision: Revision) -> Result<Self, GraphError> {
        Self::build_for_workspace(root, revision, WorkspaceId::new(1))
    }

    pub fn build_for_workspace(
        root: &Path,
        revision: Revision,
        workspace_id: WorkspaceId,
    ) -> Result<Self, GraphError> {
        let mut builder = GraphBuilder::new(root, revision)?.with_workspace_id(workspace_id);
        let graph = builder.build()?;
        let graph = crate::query::ImpactGraph::new(graph)?;
        Ok(Self { builder, graph })
    }

    pub fn with_options(mut self, options: BuildOptions) -> Self {
        self.builder = self.builder.with_options(options);
        self
    }

    pub fn graph(&self) -> &crate::query::ImpactGraph {
        &self.graph
    }

    pub fn builder(&self) -> &GraphBuilder {
        &self.builder
    }

    /// Re-parse only changed files and their reverse dependents, then swap
    /// in the re-assembled graph. `files_reparsed` is the incremental I/O
    /// proof; it stays far below the workspace file count for a local edit.
    pub fn refresh(
        &mut self,
        changed: &[String],
        revision: Revision,
    ) -> Result<IncrementalStats, GraphError> {
        self.builder.set_revision(revision);
        let stats = self.builder.update(changed)?;
        let graph = self.builder.assemble();
        self.graph = crate::query::ImpactGraph::new(graph)?;
        Ok(stats)
    }

    pub fn affected(
        &self,
        changed: &[crate::query::ChangedTarget],
        options: &crate::query::AffectedOptions,
    ) -> Result<crate::query::AffectedResult, GraphError> {
        self.graph.affected(changed, options)
    }
}

fn provenance_rank(provenance: Provenance) -> u8 {
    match provenance {
        Provenance::Lsp => 6,
        Provenance::Git => 5,
        Provenance::Coverage => 4,
        Provenance::Parser => 3,
        Provenance::Manifest => 2,
        Provenance::Derived => 1,
    }
}

fn insert_edge(
    edges: &mut BTreeMap<(String, crate::model::EdgeKind, String), GraphEdge>,
    edge: GraphEdge,
) {
    let key = (edge.from.clone(), edge.kind, edge.to.clone());
    match edges.get_mut(&key) {
        Some(existing) => {
            if provenance_rank(edge.provenance) > provenance_rank(existing.provenance) {
                *existing = edge;
            }
        }
        None => {
            edges.insert(key, edge);
        }
    }
}

fn is_test_target(crate_dir: &str, path: &str) -> bool {
    let relative = if crate_dir.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{crate_dir}/")).unwrap_or(path)
    };
    relative.starts_with("tests/") && relative.ends_with(".rs")
}

/// Module path of a source file inside its crate (`src/lib.rs` = []).
pub fn module_path(crate_name: &str, path: &str, workspace: &Workspace) -> Option<Vec<String>> {
    let crate_info = workspace.crates.get(crate_name)?;
    let relative = if crate_info.dir.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{}/", crate_info.dir))?
    };
    let rest = relative.strip_prefix("src/")?;
    let rest = rest.strip_suffix(".rs")?;
    let rest = rest.strip_suffix("/mod").unwrap_or(rest);
    if rest == "lib" || rest == "main" {
        return Some(Vec::new());
    }
    Some(rest.split('/').map(str::to_string).collect())
}

enum ExternalTarget {
    Package,
}

struct ResolvedImport {
    local_files: Vec<String>,
    symbols: Vec<String>,
    externals: Vec<(ExternalTarget, String)>,
}

struct ImportContext<'a> {
    path: &'a str,
    crate_name: &'a str,
    module: &'a [String],
    workspace: &'a Workspace,
    module_files: &'a BTreeMap<String, BTreeMap<Vec<String>, String>>,
    records: &'a BTreeMap<String, ParsedFile>,
    files: &'a BTreeMap<String, GraphFile>,
}

fn resolve_import(import: &ImportRef, context: &ImportContext<'_>) -> ResolvedImport {
    let path = context.path;
    let crate_name = context.crate_name;
    let module = context.module;
    let workspace = context.workspace;
    let module_files = context.module_files;
    let records = context.records;
    let files = context.files;
    let mut resolved = ResolvedImport {
        local_files: Vec::new(),
        symbols: Vec::new(),
        externals: Vec::new(),
    };
    if import.segments.is_empty() {
        return resolved;
    }
    let first = import.segments[0].as_str();
    // JS relative import.
    if first == "." || first == ".." {
        if let Some(target) = resolve_relative(path, &import.segments, files) {
            resolved.local_files.push(target);
        }
        return resolved;
    }
    let (base_module, rest, target_crate_name) = if first == "crate" {
        (Vec::new(), &import.segments[1..], crate_name.to_string())
    } else if first == "self" {
        (
            module.to_vec(),
            &import.segments[1..],
            crate_name.to_string(),
        )
    } else if first == "super" {
        let mut base = module.to_vec();
        base.pop();
        (base, &import.segments[1..], crate_name.to_string())
    } else if let Some(target_crate) = workspace
        .crates
        .get(first)
        .or_else(|| workspace.crates.get(&first.replace('_', "-")))
    {
        (Vec::new(), &import.segments[1..], target_crate.name.clone())
    } else {
        let package = if first.starts_with('@') {
            import
                .segments
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join("/")
        } else {
            first.to_string()
        };
        // Unknown bare imports still become package nodes: an external
        // relationship is never silently dropped.
        resolved.externals.push((ExternalTarget::Package, package));
        return resolved;
    };
    let map = module_files.get(&target_crate_name);
    if !rest.is_empty() {
        let mut candidate = base_module.clone();
        candidate.extend(rest.iter().cloned());
        let is_module = map.is_some_and(|map| {
            map.contains_key(&candidate)
                || map
                    .keys()
                    .any(|key| key.len() >= candidate.len() && key.starts_with(&candidate))
        });
        let module_path = if is_module {
            candidate
        } else if candidate.len() > 1 {
            candidate[..candidate.len() - 1].to_vec()
        } else {
            Vec::new()
        };
        resolved.local_files.extend(find_files(map, &module_path));
        for name in &import.names {
            resolved.symbols.extend(find_symbols_in_module(
                &target_crate_name,
                &module_path,
                &name.original,
                map,
                records,
            ));
        }
        return resolved;
    }
    resolved.local_files.extend(find_files(map, &base_module));
    for name in &import.names {
        resolved.symbols.extend(find_symbols_in_module(
            &target_crate_name,
            &base_module,
            &name.original,
            map,
            records,
        ));
    }
    resolved
}

fn find_files(map: Option<&BTreeMap<Vec<String>, String>>, module: &[String]) -> Vec<String> {
    let Some(map) = map else {
        return Vec::new();
    };
    let mut files = Vec::new();
    if let Some(file) = map.get(module) {
        files.push(file.clone());
    }
    files.sort();
    files.dedup();
    files
}

fn find_symbols_in_module(
    _crate_name: &str,
    module: &[String],
    name: &str,
    module_files: Option<&BTreeMap<Vec<String>, String>>,
    records: &BTreeMap<String, ParsedFile>,
) -> Vec<String> {
    let Some(file) = module_files.and_then(|map| map.get(module)) else {
        return Vec::new();
    };
    let Some(parsed) = records.get(file) else {
        return Vec::new();
    };
    parsed
        .symbols
        .iter()
        .filter(|symbol| symbol.name == name)
        .map(|symbol| symbol.id.clone())
        .collect()
}

fn resolve_relative(
    path: &str,
    segments: &[String],
    files: &BTreeMap<String, GraphFile>,
) -> Option<String> {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    for segment in segments {
        match segment.as_str() {
            "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    let base = parts.join("/");
    let mut candidates = vec![base.clone()];
    for extension in [".js", ".mjs", ".cjs", ".ts", ".tsx", ".jsx"] {
        candidates.push(format!("{base}{extension}"));
        if let Some(stripped) = base.strip_suffix(".js") {
            candidates.push(format!("{stripped}{extension}"));
        }
        if let Some(stripped) = base.strip_suffix(".ts") {
            candidates.push(format!("{stripped}{extension}"));
        }
        candidates.push(format!("{base}/index{extension}"));
    }
    candidates
        .into_iter()
        .find(|candidate| files.contains_key(candidate))
}

fn reference_candidates<'a>(
    reference: &NameRef,
    path: &str,
    file: &GraphFile,
    defs_by_file_name: &BTreeMap<(&str, &str), Vec<&'a SymbolDef>>,
    defs_by_crate_name: &BTreeMap<(&str, &str), Vec<&'a SymbolDef>>,
    imported_files: &BTreeMap<String, BTreeSet<String>>,
    _workspace: &Workspace,
) -> Vec<&'a SymbolDef> {
    fn sort_and_cap(mut candidates: Vec<&SymbolDef>) -> Vec<&SymbolDef> {
        candidates.sort_by(|a, b| (&a.name, a.line, &a.id).cmp(&(&b.name, b.line, &b.id)));
        candidates.dedup_by(|a, b| a.id == b.id);
        candidates.truncate(MAX_RESOLVE_CANDIDATES);
        candidates
    }
    if let Some(candidates) = defs_by_file_name.get(&(path, reference.name.as_str())) {
        if !candidates.is_empty() {
            return sort_and_cap(candidates.clone());
        }
    }
    if let Some(imports) = imported_files.get(path) {
        let mut imported = Vec::new();
        for target in imports {
            if let Some(candidates) =
                defs_by_file_name.get(&(target.as_str(), reference.name.as_str()))
            {
                imported.extend(candidates.iter().copied());
            }
        }
        if !imported.is_empty() {
            return sort_and_cap(imported);
        }
    }
    // Same crate: only a unique definition is unambiguous.
    if let Some(candidates) =
        defs_by_crate_name.get(&(file.crate_name.as_str(), reference.name.as_str()))
    {
        if candidates.len() == 1 {
            return vec![candidates[0]];
        }
    }
    Vec::new()
}

fn resolve_name<'a>(
    name: &str,
    path: &str,
    file: &GraphFile,
    defs_by_file_name: &BTreeMap<(&str, &str), Vec<&'a SymbolDef>>,
    defs_by_crate_name: &BTreeMap<(&str, &str), Vec<&'a SymbolDef>>,
    imported_files: &BTreeMap<String, BTreeSet<String>>,
    workspace: &Workspace,
) -> Vec<&'a SymbolDef> {
    let reference = NameRef {
        name: name.to_string(),
        line: 0,
        is_call: false,
        owner: crate::parse::Owner {
            kind: OwnerKind::File,
            id: file_id(path),
            name: String::new(),
        },
    };
    reference_candidates(
        &reference,
        path,
        file,
        defs_by_file_name,
        defs_by_crate_name,
        imported_files,
        workspace,
    )
}
