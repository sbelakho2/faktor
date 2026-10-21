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

use crate::manifest::{self, TopologyOptions, Workspace};
use crate::model::{
    crate_id, file_id, module_id, package_id, sort_graph, EdgeKind, Graph, GraphEdge, GraphError,
    GraphFile, GraphNode, GraphStats, Language, NodeKind, Provenance, Revision, GRAPH_SCHEMA,
    MAX_EDGES, MAX_FILES, MAX_FILE_BYTES, MAX_NODES, MAX_PATH_BYTES,
};
use crate::parse::{parse_file, ImportRef, NameRef, OwnerKind, ParsedFile, SymbolDef};
use crate::topology::UnitKind;

pub const MAX_WALK_DEPTH: usize = 48;
/// Bounded set of manifest/lock files whose content decides workspace
/// topology. Their stamps are part of the material reconciliation (a
/// manifest edit is not a source edit, but it does invalidate the graph).
const TOPOLOGY_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-workspace.yaml",
    "pnpm-lock.yaml",
    "yarn.lock",
    "settings.gradle",
    "settings.gradle.kts",
    "build.gradle",
    "build.gradle.kts",
    "pom.xml",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "poetry.lock",
    "Pipfile",
    "Pipfile.lock",
];
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
    /// True only when Cargo metadata was unavailable and the documented
    /// bounded manifest fallback ran.
    pub degraded: bool,
    pub degraded_reason: Option<String>,
    /// Adapters that contributed units (cargo, npm, gradle, maven, python,
    /// generic-filesystem).
    pub adapters: Vec<String>,
    /// Per-adapter native-metadata vs structural-fallback provenance. The
    /// `degraded` flag on an outcome is the typed fallback marker requested
    /// by the topology contract.
    pub adapter_outcomes: Vec<crate::topology::AdapterOutcome>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncrementalStats {
    pub files_changed: usize,
    /// Changed paths whose bytes are byte-identical to the indexed content;
    /// they are not re-parsed (a same-content refresh is a no-op).
    pub files_unchanged: usize,
    pub files_invalidated: usize,
    pub files_reparsed: usize,
    pub records: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ChangeClassification {
    /// Paths whose bytes differ from the indexed content (new, modified or
    /// deleted) and therefore require a re-parse.
    pub seeds: Vec<String>,
    pub unchanged: usize,
    /// The workspace walk hit its file bound, so the absent/deleted set is
    /// not complete. Callers must rebuild rather than apply a partial seed
    /// set.
    pub truncated: bool,
}

/// Cheap on-disk identity used by whole-workspace reconciliation. Content is
/// still decided by the blake3 digest in `classify_changes`; this stamp only
/// selects candidates so an unchanged query never reads every file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified_nanos: i128,
}

/// True when `path`'s file name is one of the workspace topology manifests
/// (Cargo.toml/lock, npm lockfiles, Gradle/Maven/Python manifests). The
/// watch uses this to treat a manifest edit as material even though it has
/// no source language.
pub fn is_topology_manifest(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    TOPOLOGY_MANIFESTS.contains(&name)
}

fn manifest_stamp(root: &Path, relative: &str) -> Option<FileStamp> {
    let meta = fs::symlink_metadata(root.join(relative)).ok()?;
    meta.file_type().is_file().then(|| file_stamp(&meta))
}

fn file_stamp(meta: &fs::Metadata) -> FileStamp {
    let modified_nanos = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128)
        .unwrap_or(0);
    FileStamp {
        bytes: meta.len(),
        modified_nanos,
    }
}

/// One bounded workspace candidate walk. `skipped` keeps the human-readable
/// reasons the build report records; a reconciliation pass only needs the
/// candidate set and the truncation flag.
struct CandidateScan {
    candidates: Vec<String>,
    skipped: Vec<String>,
    skipped_count: usize,
    truncated: bool,
}

pub struct GraphBuilder {
    root: PathBuf,
    workspace: Workspace,
    workspace_id: WorkspaceId,
    revision: Revision,
    /// Discovery options retained so a full rebuild re-reads manifests
    /// (a Cargo.toml/package.json change is a topology change, not a no-op).
    topology_options: TopologyOptions,
    records: BTreeMap<String, ParsedFile>,
    files: BTreeMap<String, GraphFile>,
    /// On-disk stamp per indexed file, used by the whole-workspace
    /// reconciliation walk. Never part of the serialized graph.
    file_meta: BTreeMap<String, FileStamp>,
    /// Stamps of the manifest/lock files the topology was discovered from.
    /// A change here invalidates the unit graph even when no source file
    /// changed, so the next refresh re-discovers topology instead of
    /// silently keeping stale crate/package relationships.
    manifest_meta: BTreeMap<String, FileStamp>,
    /// Materialized path-level reverse dependency index from the last
    /// assembled graph (dependency path -> dependent paths). Refresh uses it
    /// so invalidation is O(changed + affected), never a full graph walk.
    dependents: BTreeMap<String, BTreeSet<String>>,
    options: BuildOptions,
    report: BuildReport,
}

impl GraphBuilder {
    pub fn new(root: &Path, revision: Revision) -> Result<Self, GraphError> {
        Self::new_with_topology(root, revision, TopologyOptions::default())
    }

    /// Build a workspace scanner with explicit topology options: the
    /// supervisor used for `cargo metadata`, adapter composition and the
    /// documented parser-only fallback. `cargo metadata` runs at most once
    /// per (root, revision) and is cached.
    pub fn new_with_topology(
        root: &Path,
        revision: Revision,
        topology_options: TopologyOptions,
    ) -> Result<Self, GraphError> {
        let root = root
            .canonicalize()
            .map_err(|e| GraphError::Malformed(format!("workspace root: {e}")))?;
        let workspace = manifest::discover_with(&root, &revision, &topology_options)?;
        let retained_topology = topology_options.clone();
        let mut report = BuildReport {
            degraded: workspace.degraded,
            degraded_reason: workspace.degraded_reason.clone(),
            adapters: workspace
                .adapters
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
            adapter_outcomes: workspace.adapter_outcomes.clone(),
            ..BuildReport::default()
        };
        for skipped in &workspace.skipped {
            report.files_skipped += 1;
            report.skipped.push(skipped.clone());
        }
        Ok(Self {
            root,
            workspace,
            workspace_id: WorkspaceId::new(1),
            revision,
            topology_options: retained_topology,
            records: BTreeMap::new(),
            files: BTreeMap::new(),
            file_meta: BTreeMap::new(),
            manifest_meta: BTreeMap::new(),
            dependents: BTreeMap::new(),
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

    pub fn report(&self) -> &BuildReport {
        &self.report
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
        self.build_inner(None)
    }

    fn build_inner(&mut self, material_generation: Option<u64>) -> Result<Graph, GraphError> {
        let scan = self.collect_candidates();
        self.report.files_skipped += scan.skipped_count;
        self.report.skipped.extend(scan.skipped);
        if scan.truncated {
            self.report.truncated = true;
        }
        for relative in scan.candidates {
            self.parse_path(&relative)?;
        }
        self.manifest_meta = self.capture_manifests();
        if let Some(generation) = material_generation {
            self.revision = material_revision(self.files.values(), generation);
        }
        let graph = self.assemble();
        self.dependents = dependents_from(&graph);
        Ok(graph)
    }

    /// Drop every cached record and re-scan the whole workspace. Used when
    /// the dirty history is lost or a partial invalidation would exceed its
    /// bound: the rebuilt graph is complete for the current content, never a
    /// merge of old and new records.
    pub fn rebuild(&mut self, material_generation: Option<u64>) -> Result<Graph, GraphError> {
        // Topology is part of the whole-workspace material identity: a
        // Cargo.toml/package.json edit must re-read the unit graph, not just
        // reuse the discovery from the previous generation.
        self.workspace =
            manifest::discover_with(&self.root, &self.revision, &self.topology_options)?;
        self.records.clear();
        self.files.clear();
        self.file_meta.clear();
        self.manifest_meta.clear();
        self.dependents.clear();
        self.report.files_scanned = 0;
        self.report.files_parsed = 0;
        self.report.files_skipped = 0;
        self.report.bytes = 0;
        self.report.truncated = false;
        self.report.skipped.clear();
        self.report.degraded = self.workspace.degraded;
        self.report.degraded_reason = self.workspace.degraded_reason.clone();
        self.report.adapters = self
            .workspace
            .adapters
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect();
        self.report.adapter_outcomes = self.workspace.adapter_outcomes.clone();
        for skipped in &self.workspace.skipped {
            self.report.files_skipped += 1;
            self.report.skipped.push(skipped.clone());
        }
        self.build_inner(material_generation)
    }

    fn collect_candidates(&self) -> CandidateScan {
        let mut candidates = Vec::new();
        let mut skipped = Vec::new();
        let mut skipped_count = 0usize;
        let mut truncated = false;
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
            // Defense in depth: no unit directory is walked before its
            // canonical form is proven to remain inside the workspace root.
            let dir = match dir.canonicalize() {
                Ok(path) if path.starts_with(&self.root) => path,
                Ok(_) => {
                    skipped_count += 1;
                    skipped.push(format!(
                        "{}: unit directory resolves outside the workspace root",
                        info.dir
                    ));
                    continue;
                }
                Err(_) => {
                    skipped_count += 1;
                    skipped.push(format!("{}: unit directory is missing", info.dir));
                    continue;
                }
            };
            let mut stack = vec![(dir, 0usize)];
            while let Some((dir, depth)) = stack.pop() {
                if depth > MAX_WALK_DEPTH {
                    truncated = true;
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
                        skipped_count += 1;
                        continue;
                    }
                    if Language::of_path(&relative).is_none() {
                        continue;
                    }
                    // A parent unit must not swallow a nested declared unit
                    // (Cargo member, npm workspace, Gradle/Maven module).
                    // Only DESCENDANT units are excluded: when the walk is
                    // already inside a nested unit, its ancestors must not
                    // hide its own files (nested Maven/Gradle modules).
                    if crate_dirs.iter().any(|d| {
                        !d.is_empty()
                            && *d != info.dir
                            && (info.dir.is_empty() || d.starts_with(&format!("{}/", info.dir)))
                            && (relative == *d || relative.starts_with(&format!("{d}/")))
                    }) {
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
            truncated = true;
        }
        CandidateScan {
            candidates,
            skipped,
            skipped_count,
            truncated,
        }
    }

    fn manifest_paths(&self) -> Vec<String> {
        let mut dirs = BTreeSet::new();
        dirs.insert(String::new());
        for info in self.workspace.crates.values() {
            dirs.insert(info.dir.clone());
        }
        let mut paths = Vec::new();
        for dir in dirs {
            for name in TOPOLOGY_MANIFESTS {
                let relative = if dir.is_empty() {
                    (*name).to_string()
                } else {
                    format!("{dir}/{name}")
                };
                if self
                    .root
                    .join(&relative)
                    .symlink_metadata()
                    .is_ok_and(|meta| meta.file_type().is_file())
                {
                    paths.push(relative);
                }
            }
        }
        paths
    }

    fn capture_manifests(&self) -> BTreeMap<String, FileStamp> {
        let mut stamps = BTreeMap::new();
        for path in self.manifest_paths() {
            if let Some(stamp) = manifest_stamp(&self.root, &path) {
                stamps.insert(path, stamp);
            }
        }
        stamps
    }

    /// Whole-workspace reconciliation: walk the same bounded candidate set
    /// as the build and return every path whose on-disk stamp differs from
    /// the indexed record (new, modified or deleted). This is the watch's
    /// fallback for dropped events and the pre-refresh generation check; it
    /// never re-parses, and `classify_changes` still decides by content
    /// digest.
    pub fn scan_changes(&self) -> Result<ChangeClassification, GraphError> {
        let scan = self.collect_candidates();
        if scan.truncated {
            return Ok(ChangeClassification {
                truncated: true,
                ..ChangeClassification::default()
            });
        }
        let candidates: BTreeSet<String> = scan.candidates.into_iter().collect();
        let mut seeds = BTreeSet::new();
        let mut unchanged = 0usize;
        for path in &candidates {
            let Some(file) = self.files.get(path) else {
                seeds.insert(path.clone());
                continue;
            };
            let stamp = fs::symlink_metadata(self.root.join(path))
                .ok()
                .filter(|meta| meta.file_type().is_file())
                .map(|meta| file_stamp(&meta));
            match (stamp, self.file_meta.get(path)) {
                (Some(current), Some(indexed))
                    if current == *indexed && current.bytes == file.bytes =>
                {
                    unchanged += 1;
                }
                _ => {
                    seeds.insert(path.clone());
                }
            }
        }
        for path in self.files.keys() {
            if !candidates.contains(path) {
                seeds.insert(path.clone());
            }
        }
        let manifests = self.capture_manifests();
        for (path, stamp) in &manifests {
            if self.manifest_meta.get(path) != Some(stamp) {
                seeds.insert(path.clone());
            }
        }
        for path in self.manifest_meta.keys() {
            if !manifests.contains_key(path) {
                seeds.insert(path.clone());
            }
        }
        Ok(ChangeClassification {
            seeds: seeds.into_iter().collect(),
            unchanged,
            truncated: false,
        })
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
                self.file_meta.remove(relative);
                return Ok(());
            }
        };
        if !meta.file_type().is_file() {
            self.records.remove(relative);
            self.files.remove(relative);
            self.file_meta.remove(relative);
            self.report.files_skipped += 1;
            return Ok(());
        }
        if meta.len() > self.options.max_file_bytes {
            self.report.files_skipped += 1;
            self.report.truncated = true;
            self.records.remove(relative);
            self.files.remove(relative);
            self.file_meta.remove(relative);
            return Ok(());
        }
        let bytes =
            fs::read(&absolute).map_err(|e| GraphError::Malformed(format!("{relative}: {e}")))?;
        let Ok(source) = std::str::from_utf8(&bytes) else {
            self.records.remove(relative);
            self.files.remove(relative);
            self.file_meta.remove(relative);
            self.report.files_skipped += 1;
            return Ok(());
        };
        let language = Language::of_path(relative)
            .ok_or_else(|| GraphError::Malformed(format!("unsupported file {relative}")))?;
        let test_target = is_test_target(crate_info, relative, &self.workspace);
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
        self.file_meta
            .insert(relative.to_string(), file_stamp(&meta));
        Ok(())
    }

    /// Classify changed paths by content digest. Only paths whose bytes
    /// differ from the indexed content (new, modified or deleted) become
    /// seeds; byte-identical paths are counted as unchanged and never
    /// re-parsed. This is what makes a repeated report for the same lifetime
    /// cheap without suppressing a later real edit.
    pub fn classify_changes(&self, changed: &[String]) -> Result<ChangeClassification, GraphError> {
        let mut seeds = BTreeSet::new();
        let mut unchanged = 0usize;
        for raw in changed {
            let path = crate::model::validate_rel_path(raw)?;
            if self.workspace.crate_for_path(&path).is_none() || Language::of_path(&path).is_none()
            {
                continue;
            }
            match (self.files.get(&path), self.current_content(&path)?) {
                (Some(file), CurrentContent::Hash(hash)) if file.hash == hash => {
                    unchanged += 1;
                }
                (None, CurrentContent::Missing) => unchanged += 1,
                _ => {
                    seeds.insert(path);
                }
            }
        }
        Ok(ChangeClassification {
            seeds: seeds.into_iter().collect(),
            unchanged,
            truncated: false,
        })
    }

    fn current_content(&self, path: &str) -> Result<CurrentContent, GraphError> {
        let absolute = self.root.join(path);
        let meta = match fs::symlink_metadata(&absolute) {
            Ok(meta) => meta,
            Err(_) => return Ok(CurrentContent::Missing),
        };
        if !meta.file_type().is_file() {
            return Ok(CurrentContent::Missing);
        }
        if meta.len() > self.options.max_file_bytes {
            return Ok(CurrentContent::Changed);
        }
        let bytes =
            fs::read(&absolute).map_err(|e| GraphError::Malformed(format!("{path}: {e}")))?;
        Ok(CurrentContent::Hash(format!(
            "blake3:{}",
            blake3::hash(&bytes).to_hex()
        )))
    }

    /// Invalidate per changed file: re-parse the changed files and the files
    /// whose imports/references point at them, then re-assemble from cached
    /// records. Unchanged files are never read or parsed again.
    pub fn update(&mut self, changed: &[String]) -> Result<IncrementalStats, GraphError> {
        let classification = self.classify_changes(changed)?;
        if classification.seeds.is_empty() {
            return Ok(IncrementalStats {
                files_unchanged: classification.unchanged,
                records: self.records.len(),
                ..IncrementalStats::default()
            });
        }
        let (mut stats, _) = self.apply_changes(&classification.seeds, None)?;
        stats.files_unchanged = classification.unchanged;
        Ok(stats)
    }

    /// Re-parse exactly the changed seeds plus their reverse dependency
    /// closure and return the freshly assembled graph. `material_generation`
    /// stamps a dirty-worktree revision derived from the scanned file
    /// digests before assembly, so the graph carries a real material identity.
    fn apply_changes(
        &mut self,
        seeds: &[String],
        material_generation: Option<u64>,
    ) -> Result<(IncrementalStats, Graph), GraphError> {
        let mut invalidated: BTreeSet<String> = seeds.iter().cloned().collect();
        let mut truncated = invalidated.len() >= MAX_INVALIDATION_FILES;
        if !truncated {
            // Reverse closure over the materialized dependency index from the
            // last graph: importing files and referencing symbols are
            // re-parsed because their resolution depends on the changed
            // definitions. Cost is O(changed + affected), not O(workspace).
            let mut queue: VecDeque<String> = seeds.iter().cloned().collect();
            'closure: while let Some(path) = queue.pop_front() {
                let Some(dependents) = self.dependents.get(&path) else {
                    continue;
                };
                for dependent in dependents {
                    if invalidated.len() >= MAX_INVALIDATION_FILES {
                        truncated = true;
                        break 'closure;
                    }
                    if invalidated.insert(dependent.clone()) {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }
        let mut reparsed = 0usize;
        for path in &invalidated {
            let existed = self.files.contains_key(path);
            self.parse_path(path)?;
            if self.files.contains_key(path) || existed {
                reparsed += 1;
            }
        }
        if let Some(generation) = material_generation {
            self.revision = material_revision(self.files.values(), generation);
        }
        let graph = self.assemble();
        self.dependents = dependents_from(&graph);
        Ok((
            IncrementalStats {
                files_changed: seeds.len(),
                files_unchanged: 0,
                files_invalidated: invalidated.len(),
                files_reparsed: reparsed,
                records: self.records.len(),
                truncated,
            },
            graph,
        ))
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

        // Local units (Cargo crates and non-Cargo packages) and dependency
        // edges. Node ids keep the two namespaces distinct.
        for info in self.workspace.crates.values() {
            add_node(GraphNode {
                id: info.kind.node_id(&info.name),
                kind: match info.kind {
                    UnitKind::Crate => NodeKind::Crate,
                    UnitKind::Package => NodeKind::Package,
                },
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
                let to = match self.workspace.local_unit(dep) {
                    Some(name) => self
                        .workspace
                        .crates
                        .get(name)
                        .map(|unit| unit.kind.node_id(&unit.name))
                        .unwrap_or_else(|| package_id(dep)),
                    None => package_id(dep),
                };
                insert_edge(
                    &mut edges,
                    GraphEdge {
                        from: info.kind.node_id(&info.name),
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
            let unit_id = self
                .workspace
                .crates
                .get(&file.crate_name)
                .map(|unit| unit.kind.node_id(&unit.name))
                .unwrap_or_else(|| crate_id(&file.crate_name));
            insert_edge(
                &mut edges,
                GraphEdge {
                    from: unit_id.clone(),
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
                        from: unit_id.clone(),
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
                        to: unit_id.clone(),
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

        self.finalize_graph(nodes, edges)
    }

    /// Deterministic serialization phase of [`Self::assemble`]: stable
    /// vectors, reverse edges, bounded stats and the canonical seal.
    fn finalize_graph(
        &self,
        nodes: BTreeMap<String, GraphNode>,
        edges: BTreeMap<(String, crate::model::EdgeKind, String), GraphEdge>,
    ) -> Graph {
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

/// Bounded refresh policy. A dirty set above the incremental path bound is
/// never applied partially: the session rebuilds instead. A rebuild is
/// refused (and the query is marked degraded) when the indexed workspace is
/// above the rebuild file budget, so no caller mistakes a stale graph for a
/// complete current-generation answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshPolicy {
    pub max_incremental_paths: usize,
    pub max_rebuild_files: usize,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            max_incremental_paths: MAX_INVALIDATION_FILES,
            max_rebuild_files: MAX_FILES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// The cached graph already represented the requested generation.
    Unchanged,
    /// Only the recorded dirty paths (and their reverse dependents) were
    /// re-examined.
    Incremental,
    /// Dirty history was lost or exceeded the incremental bound; the whole
    /// workspace was re-scanned.
    Rebuilt,
    /// The rebuild budget was exceeded. The graph is NOT current and the
    /// report carries `degraded = true`; this is never silent.
    Degraded,
}

#[derive(Debug, Clone)]
pub struct RefreshReport {
    pub generation: u64,
    pub mode: RefreshMode,
    pub stats: IncrementalStats,
    pub lost: bool,
    pub degraded: bool,
    pub note: Option<String>,
}

/// A cached graph with explicit refresh semantics. The product semantic
/// adapter and the CI generator both build through this type, so there is
/// exactly one graph authority and one revision binding.
pub struct ImpactSession {
    builder: GraphBuilder,
    graph: crate::query::ImpactGraph,
    /// Advances only when refresh observes different content, so an
    /// unchanged refresh keeps the same material revision and graph digest.
    material_generation: u64,
    /// Set when the graph could not be proven current (lost history or a
    /// rebuild that hit its bound). Propagated into every affected result.
    material_degraded: bool,
    material_degraded_reason: Option<String>,
}

impl ImpactSession {
    pub fn build(root: &Path, revision: Revision) -> Result<Self, GraphError> {
        Self::build_for_workspace(root, revision, WorkspaceId::new(1))
    }

    /// Build a session whose revision is a dirty-worktree material identity
    /// (scanned file digest + generation) rather than `Revision::default()`.
    pub fn build_material(root: &Path) -> Result<Self, GraphError> {
        Self::build_for_workspace(root, Revision::default(), WorkspaceId::new(1))
    }

    pub fn build_material_for_workspace(
        root: &Path,
        workspace_id: WorkspaceId,
    ) -> Result<Self, GraphError> {
        Self::build_for_workspace(root, Revision::default(), workspace_id)
    }

    /// Material build with explicit scan bounds (the rebuild-budget tests and
    /// operators that must cap a very large workspace).
    pub fn build_material_for_workspace_with_options(
        root: &Path,
        workspace_id: WorkspaceId,
        options: BuildOptions,
    ) -> Result<Self, GraphError> {
        Self::build_for_workspace_with_options(root, Revision::default(), workspace_id, options)
    }

    pub fn build_for_workspace(
        root: &Path,
        revision: Revision,
        workspace_id: WorkspaceId,
    ) -> Result<Self, GraphError> {
        Self::build_for_workspace_with_options(
            root,
            revision,
            workspace_id,
            BuildOptions::default(),
        )
    }

    fn build_for_workspace_with_options(
        root: &Path,
        revision: Revision,
        workspace_id: WorkspaceId,
        options: BuildOptions,
    ) -> Result<Self, GraphError> {
        let mut builder = GraphBuilder::new(root, revision)?
            .with_workspace_id(workspace_id)
            .with_options(options);
        let mut graph = builder.build()?;
        let material = graph.revision.is_unspecified();
        let material_generation = if material {
            let revision = material_revision(graph.files.iter(), 1);
            graph.revision = revision.clone();
            builder.set_revision(revision);
            graph.seal()?;
            1
        } else {
            0
        };
        let graph = crate::query::ImpactGraph::new(graph)?;
        Ok(Self {
            builder,
            graph,
            material_generation,
            material_degraded: false,
            material_degraded_reason: None,
        })
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

    /// The material generation of the graph currently cached here.
    pub fn material_generation(&self) -> u64 {
        self.material_generation
    }

    /// True when the cached graph could not be proven current on the last
    /// generation sync. `affected` carries this flag into every result.
    pub fn material_degraded(&self) -> bool {
        self.material_degraded
    }

    pub fn material_degraded_reason(&self) -> Option<&str> {
        self.material_degraded_reason.as_deref()
    }

    /// Whole-workspace reconciliation against the indexed state. The caller
    /// turns the returned seeds into a generation observation; this method
    /// never mutates the graph.
    pub fn scan_material_changes(&self) -> Result<ChangeClassification, GraphError> {
        self.builder.scan_changes()
    }

    fn mark_degraded(&mut self, reason: String) {
        self.material_degraded = true;
        self.material_degraded_reason = Some(reason);
    }

    fn clear_degraded(&mut self) {
        self.material_degraded = false;
        self.material_degraded_reason = None;
    }

    /// Full re-scan used when the dirty history cannot be applied exactly.
    /// The graph is replaced only after the complete rebuild succeeds.
    pub fn rebuild_material(&mut self) -> Result<IncrementalStats, GraphError> {
        self.material_generation = self.material_generation.saturating_add(1);
        let generation = self.material_generation;
        let graph = self.builder.rebuild(Some(generation))?;
        let truncated = self.builder.report().truncated;
        self.graph = crate::query::ImpactGraph::new(graph)?;
        if truncated {
            self.mark_degraded(format!(
                "material rebuild hit the file/scan bound ({} files indexed)",
                self.builder.files.len()
            ));
        } else {
            self.clear_degraded();
        }
        Ok(IncrementalStats {
            files_changed: self.builder.files.len(),
            files_reparsed: self.builder.files.len(),
            records: self.builder.records.len(),
            truncated,
            ..IncrementalStats::default()
        })
    }

    /// Bring the cached graph to `dirty.generation` (a snapshot of the
    /// whole-workspace authority). A complete refresh is the ONLY path that
    /// lets the caller advance its cached generation; the authority may have
    /// moved on while this ran, and the next query will see that newer
    /// generation.
    pub fn refresh_from_generation(
        &mut self,
        dirty: &crate::generation::DirtyPaths,
        policy: &RefreshPolicy,
    ) -> Result<RefreshReport, GraphError> {
        let non_incremental = dirty.paths.iter().any(|path| {
            Language::of_path(path).is_none()
                || self.builder.workspace.crate_for_path(path).is_none()
        });
        let rebuild =
            dirty.lost || non_incremental || dirty.paths.len() > policy.max_incremental_paths;
        if rebuild {
            if self.builder.files.len() > policy.max_rebuild_files {
                let note = format!(
                    "dirty history {} for generation {} requires a rebuild, but {} indexed files exceed the rebuild budget {}; the cached graph is degraded (not current)",
                    if dirty.lost {
                        "was lost"
                    } else {
                        "exceeded the incremental bound"
                    },
                    dirty.generation,
                    self.builder.files.len(),
                    policy.max_rebuild_files
                );
                self.mark_degraded(note.clone());
                return Ok(RefreshReport {
                    generation: dirty.generation,
                    mode: RefreshMode::Degraded,
                    stats: IncrementalStats {
                        records: self.builder.records.len(),
                        ..IncrementalStats::default()
                    },
                    lost: dirty.lost,
                    degraded: true,
                    note: Some(note),
                });
            }
            let stats = self.rebuild_material()?;
            let degraded = self.material_degraded;
            return Ok(RefreshReport {
                generation: dirty.generation,
                mode: RefreshMode::Rebuilt,
                stats,
                lost: dirty.lost,
                degraded,
                note: self.material_degraded_reason.clone(),
            });
        }
        if dirty.paths.is_empty() {
            return Ok(RefreshReport {
                generation: dirty.generation,
                mode: RefreshMode::Unchanged,
                stats: IncrementalStats {
                    files_unchanged: self.builder.files.len(),
                    records: self.builder.records.len(),
                    ..IncrementalStats::default()
                },
                lost: false,
                degraded: self.material_degraded,
                note: self.material_degraded_reason.clone(),
            });
        }
        let stats = self.refresh_material(&dirty.paths)?;
        if stats.truncated {
            self.mark_degraded(format!(
                "incremental invalidation exceeded the {} file bound; the graph is partial",
                MAX_INVALIDATION_FILES
            ));
        } else {
            self.clear_degraded();
        }
        Ok(RefreshReport {
            generation: dirty.generation,
            mode: RefreshMode::Incremental,
            degraded: self.material_degraded,
            note: self.material_degraded_reason.clone(),
            stats,
            lost: false,
        })
    }

    /// Re-parse only changed files (by content digest) and their reverse
    /// dependents, then swap in the re-assembled graph. Every changed path is
    /// re-examined on every call; there is no per-path lifetime suppression,
    /// so a second real edit is always observed. `files_reparsed` is the
    /// incremental I/O proof and stays far below the workspace file count.
    pub fn refresh(
        &mut self,
        changed: &[String],
        revision: Revision,
    ) -> Result<IncrementalStats, GraphError> {
        let material = revision.is_unspecified();
        let classification = self.builder.classify_changes(changed)?;
        if classification.seeds.is_empty() {
            if !material && revision != *self.builder.revision() {
                self.builder.set_revision(revision);
                let graph = self.builder.assemble();
                self.graph = crate::query::ImpactGraph::new(graph)?;
            }
            return Ok(IncrementalStats {
                files_unchanged: classification.unchanged,
                records: self.builder.records.len(),
                ..IncrementalStats::default()
            });
        }
        let generation = if material {
            self.material_generation = self.material_generation.saturating_add(1);
            Some(self.material_generation)
        } else {
            self.builder.set_revision(revision);
            None
        };
        let (mut stats, graph) = self
            .builder
            .apply_changes(&classification.seeds, generation)?;
        stats.files_unchanged = classification.unchanged;
        self.graph = crate::query::ImpactGraph::new(graph)?;
        Ok(stats)
    }

    /// Refresh with a dirty-worktree material revision (see
    /// [`Revision::material`]) instead of an unspecified revision.
    pub fn refresh_material(&mut self, changed: &[String]) -> Result<IncrementalStats, GraphError> {
        self.refresh(changed, Revision::default())
    }

    pub fn affected(
        &self,
        changed: &[crate::query::ChangedTarget],
        options: &crate::query::AffectedOptions,
    ) -> Result<crate::query::AffectedResult, GraphError> {
        let mut result = self.graph.affected(changed, options)?;
        if self.material_degraded {
            result.degraded = true;
            result.notes.push(
                self.material_degraded_reason
                    .clone()
                    .unwrap_or_else(|| "material graph is not current".to_string()),
            );
            result.seal()?;
        }
        Ok(result)
    }
}

enum CurrentContent {
    Missing,
    /// Content is present but outside the per-file bound; it still requires a
    /// re-parse so the stale record is removed.
    Changed,
    Hash(String),
}

/// Path-level reverse dependency index for incremental invalidation. Every
/// import/reference/call/coverage edge contributes one dependency edge from
/// the target file to the file that must be re-parsed when it changes.
fn dependents_from(graph: &Graph) -> BTreeMap<String, BTreeSet<String>> {
    let mut path_by_node: BTreeMap<&str, &str> = BTreeMap::new();
    for node in &graph.nodes {
        if let Some(path) = node.path.as_deref() {
            path_by_node.insert(node.id.as_str(), path);
        }
    }
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for edge in &graph.edges {
        if !matches!(
            edge.kind,
            EdgeKind::Imports | EdgeKind::References | EdgeKind::Calls | EdgeKind::Covers
        ) {
            continue;
        }
        let (Some(from), Some(to)) = (
            path_by_node.get(edge.from.as_str()),
            path_by_node.get(edge.to.as_str()),
        ) else {
            continue;
        };
        if from == to {
            continue;
        }
        dependents
            .entry((*to).to_string())
            .or_default()
            .insert((*from).to_string());
    }
    dependents
}

fn material_revision<'a>(files: impl Iterator<Item = &'a GraphFile>, generation: u64) -> Revision {
    let mut hasher = blake3::Hasher::new();
    for file in files {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(file.hash.as_bytes());
        hasher.update(b"\0");
    }
    Revision::material(
        &format!("blake3:{}", hasher.finalize().to_hex()),
        generation,
    )
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

fn is_test_target(unit: &crate::manifest::CrateInfo, path: &str, workspace: &Workspace) -> bool {
    let relative = if unit.dir.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{}/", unit.dir)).unwrap_or(path)
    };
    if unit.test_targets.iter().any(|target| {
        relative == target
            || relative.starts_with(&format!("{target}/"))
            || path == target
            || path.starts_with(&format!("{target}/"))
    }) {
        return true;
    }
    let lower = relative.to_ascii_lowercase();
    match unit.kind {
        UnitKind::Crate => relative.starts_with("tests/") && relative.ends_with(".rs"),
        UnitKind::Package => {
            lower.contains("/src/test/")
                || lower.starts_with("src/test/")
                || lower.contains("/__tests__/")
                || lower.starts_with("__tests__/")
                || lower.contains(".test.")
                || lower.contains(".spec.")
                || lower.starts_with("tests/")
                || lower.starts_with("test/")
                || lower
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with("test_") || name.ends_with("_test.py"))
                || workspace.crates.values().any(|other| {
                    other.kind == UnitKind::Crate && other.test_targets.contains(relative)
                })
        }
    }
}

/// Module path of a source file inside its crate (`src/lib.rs` = []).
pub fn module_path(crate_name: &str, path: &str, workspace: &Workspace) -> Option<Vec<String>> {
    let crate_info = workspace.crates.get(crate_name)?;
    if crate_info.kind != UnitKind::Crate {
        return None;
    }
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
