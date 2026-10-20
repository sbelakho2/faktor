//! O(V+E) impacted-set query over the shared graph.
//!
//! Seeds are changed files/symbols. Traversal follows the materialized
//! reverse adjacency (callers, importers, covering tests, dependent crates)
//! with a depth bound, records one shortest-path reason per discovered node,
//! and never scans an edge that is not incident to a visited node. Results
//! are sorted and capped; truncation is explicit, never silent.

use std::collections::{BTreeSet, HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::model::{
    EdgeKind, Graph, GraphError, GraphNode, NodeKind, Provenance, Revision, MAX_PATH_BYTES,
};

pub const DEFAULT_MAX_RESULTS: usize = 4096;
pub const MAX_RESULTS: usize = 65_536;
pub const DEFAULT_MAX_DEPTH: u32 = 6;
pub const MAX_DEPTH: u32 = 64;
pub const MAX_REASONS_PER_NODE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedTarget {
    pub path: String,
    pub symbol: Option<String>,
}

impl ChangedTarget {
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            symbol: None,
        }
    }

    pub fn symbol(path: impl Into<String>, symbol: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            symbol: Some(symbol.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedOptions {
    pub max_depth: u32,
    pub max_results: usize,
    /// When a crate is reached, conservatively include its test nodes (and
    /// the tests of dependent crates). Symbol-level live queries disable this
    /// to assert an exact hand-verified caller/test set.
    pub crate_tests: bool,
}

impl Default for AffectedOptions {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_results: DEFAULT_MAX_RESULTS,
            crate_tests: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactRelation {
    Changed,
    Imports,
    Uses,
    Defines,
    References,
    Calls,
    Implements,
    Includes,
    Covers,
    Tests,
    DependsOn,
    TestTarget,
}

impl From<EdgeKind> for ImpactRelation {
    fn from(kind: EdgeKind) -> Self {
        match kind {
            EdgeKind::Imports => Self::Imports,
            EdgeKind::Uses => Self::Uses,
            EdgeKind::Defines => Self::Defines,
            EdgeKind::References => Self::References,
            EdgeKind::Calls => Self::Calls,
            EdgeKind::Implements => Self::Implements,
            EdgeKind::Includes => Self::Includes,
            EdgeKind::Covers => Self::Covers,
            EdgeKind::Tests => Self::Tests,
            EdgeKind::DependsOn => Self::DependsOn,
            EdgeKind::TestTarget => Self::TestTarget,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactReason {
    pub relation: ImpactRelation,
    pub via: String,
    pub provenance: Provenance,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactedNode {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crate_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub distance: u32,
    pub reasons: Vec<ImpactReason>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryStats {
    pub visited_nodes: usize,
    pub visited_edges: usize,
    pub discovered: usize,
    pub max_frontier: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffectedResult {
    pub schema: String,
    pub revision: Revision,
    pub changed: Vec<String>,
    pub affected: Vec<ImpactedNode>,
    pub tests: Vec<ImpactedNode>,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    pub crates: Vec<String>,
    pub packages: Vec<String>,
    pub truncated: bool,
    pub degraded: bool,
    pub notes: Vec<String>,
    pub stats: QueryStats,
    /// Blake3 over the canonical result with `digest` empty. Lets a CI
    /// consumer prove it executed exactly this selection.
    pub digest: String,
}

const AFFECTED_SCHEMA: &str = "faktor-impact-affected/v1";

impl AffectedResult {
    pub fn seal(&mut self) -> Result<(), GraphError> {
        let mut copy = self.clone();
        copy.digest.clear();
        let bytes =
            serde_json::to_vec(&copy).map_err(|e| GraphError::CorruptJson(e.to_string()))?;
        self.digest = format!("blake3:{}", blake3::hash(&bytes).to_hex());
        Ok(())
    }

    pub fn to_json_pretty(&self) -> Result<String, GraphError> {
        serde_json::to_string_pretty(self).map_err(|e| GraphError::CorruptJson(e.to_string()))
    }
}

pub struct ImpactGraph {
    graph: Graph,
    node_index: HashMap<String, usize>,
    out_edges: HashMap<String, Vec<usize>>,
    in_edges: HashMap<String, Vec<usize>>,
    /// Workspace-relative unit dir -> (unit name, graph node id).
    crate_by_dir: HashMap<String, (String, String)>,
    tests_by_crate: HashMap<String, Vec<String>>,
    defs_by_name: HashMap<String, Vec<String>>,
}

impl ImpactGraph {
    pub fn new(graph: Graph) -> Result<Self, GraphError> {
        graph.validate()?;
        let mut node_index = HashMap::with_capacity(graph.nodes.len());
        let mut out_edges: HashMap<String, Vec<usize>> = HashMap::new();
        let mut in_edges: HashMap<String, Vec<usize>> = HashMap::new();
        let mut crate_by_dir: HashMap<String, (String, String)> = HashMap::new();
        let mut tests_by_crate: HashMap<String, Vec<String>> = HashMap::new();
        let mut defs_by_name: HashMap<String, Vec<String>> = HashMap::new();
        for (index, node) in graph.nodes.iter().enumerate() {
            node_index.insert(node.id.clone(), index);
            match node.kind {
                // Cargo crates and local non-Cargo packages are both units:
                // the graph keeps their node namespace (crate:/package:) but
                // path ownership is identical.
                NodeKind::Crate | NodeKind::Package => {
                    if let Some(dir) = &node.path {
                        // Normalize the one trailing slash the historical
                        // matcher accepted; the longest matching ancestor
                        // wins at lookup time (root "." is the fallback).
                        let dir = dir.trim_end_matches('/');
                        crate_by_dir
                            .entry(dir.to_string())
                            .or_insert_with(|| (node.name.clone(), node.id.clone()));
                    }
                }
                NodeKind::Test => {
                    if let Some(crate_name) = &node.crate_name {
                        tests_by_crate
                            .entry(crate_name.clone())
                            .or_default()
                            .push(node.id.clone());
                    }
                }
                NodeKind::Symbol => {
                    defs_by_name
                        .entry(node.name.clone())
                        .or_default()
                        .push(node.id.clone());
                }
                _ => {}
            }
        }
        for tests in tests_by_crate.values_mut() {
            tests.sort();
            tests.dedup();
        }
        for defs in defs_by_name.values_mut() {
            defs.sort();
            defs.dedup();
        }
        for (index, edge) in graph.edges.iter().enumerate() {
            out_edges.entry(edge.from.clone()).or_default().push(index);
        }
        for (index, edge) in graph.reverse_edges.iter().enumerate() {
            in_edges.entry(edge.from.clone()).or_default().push(index);
        }
        Ok(Self {
            graph,
            node_index,
            out_edges,
            in_edges,
            crate_by_dir,
            tests_by_crate,
            defs_by_name,
        })
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub fn into_graph(self) -> Graph {
        self.graph
    }

    pub fn crate_for_path(&self, path: &str) -> Option<&str> {
        self.unit_for_path(path).map(|(name, _)| name)
    }

    fn unit_for_path(&self, path: &str) -> Option<(&str, &str)> {
        let path = path.trim_start_matches("./");
        // Longest-prefix lookup in O(path depth): the exact path (a crate
        // dir may equal it) then every slash-delimited ancestor, deepest
        // first. The historical vector scan allocated two `format!` strings
        // per crate per changed path (O(files x crates) allocations on a
        // whole-workspace seed); the map keeps the SAME winner without
        // allocating.
        if let Some((name, id)) = self.crate_by_dir.get(path) {
            return Some((name.as_str(), id.as_str()));
        }
        for (index, _) in path.match_indices('/').rev() {
            if let Some((name, id)) = self.crate_by_dir.get(&path[..index]) {
                return Some((name.as_str(), id.as_str()));
            }
        }
        self.crate_by_dir
            .get(".")
            .map(|(name, id)| (name.as_str(), id.as_str()))
    }

    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.node_index
            .get(id)
            .map(|index| &self.graph.nodes[*index])
    }

    pub fn affected(
        &self,
        changed: &[ChangedTarget],
        options: &AffectedOptions,
    ) -> Result<AffectedResult, GraphError> {
        if changed.len() > 100_000 {
            return Err(GraphError::Oversized {
                what: "changed targets",
                max: 100_000,
                actual: changed.len(),
            });
        }
        let mut targets = changed.to_vec();
        targets.sort_by(|a, b| (&a.path, &a.symbol).cmp(&(&b.path, &b.symbol)));
        let changed = &targets[..];
        let max_depth = options.max_depth.clamp(1, MAX_DEPTH);
        let max_results = options.max_results.clamp(1, MAX_RESULTS);
        let max_reasons = MAX_REASONS_PER_NODE;

        let mut best: HashMap<String, (u32, Vec<ImpactReason>)> = HashMap::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut changed_paths = BTreeSet::new();
        let mut notes = Vec::new();
        let mut degraded = false;

        let seed = |id: String,
                    reason: ImpactReason,
                    best: &mut HashMap<String, (u32, Vec<ImpactReason>)>,
                    queue: &mut VecDeque<String>| {
            match best.get_mut(&id) {
                Some((distance, reasons)) => {
                    if reason.depth < *distance {
                        *distance = reason.depth;
                        reasons.clear();
                    }
                    if reasons.len() < max_reasons && !reasons.contains(&reason) {
                        reasons.push(reason);
                        reasons.sort_by(|a, b| {
                            (a.depth, a.relation, &a.via).cmp(&(b.depth, b.relation, &b.via))
                        });
                    }
                }
                None => {
                    let depth = reason.depth;
                    best.insert(id.clone(), (depth, vec![reason]));
                    queue.push_back(id);
                }
            }
        };

        for target in changed {
            let path = crate::model::validate_rel_path(&target.path)?;
            if path.len() > MAX_PATH_BYTES {
                return Err(GraphError::Oversized {
                    what: "changed path",
                    max: MAX_PATH_BYTES,
                    actual: path.len(),
                });
            }
            changed_paths.insert(path.clone());
            let file = crate::model::file_id(&path);
            let mut seeded = false;
            if let Some(symbol) = &target.symbol {
                crate::model::validate_symbol(symbol)?;
                if let Some(ids) = self.defs_by_name.get(symbol) {
                    for id in ids {
                        if self
                            .node(id)
                            .and_then(|node| node.path.as_deref())
                            .is_some_and(|node_path| node_path == path)
                        {
                            seed(
                                id.clone(),
                                ImpactReason {
                                    relation: ImpactRelation::Changed,
                                    via: format!("{path}#{symbol}"),
                                    provenance: Provenance::Git,
                                    depth: 0,
                                    detail: Some("changed symbol".into()),
                                },
                                &mut best,
                                &mut queue,
                            );
                            seeded = true;
                        }
                    }
                }
                if !seeded {
                    notes.push(format!(
                        "symbol {symbol} was not indexed in {path}; falling back to the file"
                    ));
                }
            }
            if let Some(node) = self.node(&file) {
                seed(
                    node.id.clone(),
                    ImpactReason {
                        relation: ImpactRelation::Changed,
                        via: path.clone(),
                        provenance: Provenance::Git,
                        depth: 0,
                        detail: Some("changed file".into()),
                    },
                    &mut best,
                    &mut queue,
                );
                seeded = true;
                // Symbols and tests defined by the changed file are seeds
                // too; a file-level edit impacts its own declarations.
                if let Some(indices) = self.out_edges.get(&file) {
                    for index in indices {
                        let edge = &self.graph.edges[*index];
                        if edge.kind == EdgeKind::Defines {
                            seed(
                                edge.to.clone(),
                                ImpactReason {
                                    relation: ImpactRelation::Defines,
                                    via: file.clone(),
                                    provenance: Provenance::Parser,
                                    depth: 1,
                                    detail: None,
                                },
                                &mut best,
                                &mut queue,
                            );
                        }
                    }
                }
                if let Some((_, unit_id)) = self.unit_for_path(&path) {
                    let id = unit_id.to_string();
                    if self.node(&id).is_some() {
                        seed(
                            id,
                            ImpactReason {
                                relation: ImpactRelation::Includes,
                                via: file.clone(),
                                provenance: Provenance::Manifest,
                                depth: 1,
                                detail: None,
                            },
                            &mut best,
                            &mut queue,
                        );
                    }
                }
            }
            if !seeded {
                // A new file is not indexed yet: seed its crate and mark the
                // result degraded so no caller mistakes the miss for "nothing
                // is impacted".
                degraded = true;
                if let Some((_, unit_id)) = self.unit_for_path(&path) {
                    let id = unit_id.to_string();
                    if self.node(&id).is_some() {
                        seed(
                            id,
                            ImpactReason {
                                relation: ImpactRelation::Changed,
                                via: path.clone(),
                                provenance: Provenance::Git,
                                depth: 0,
                                detail: Some("unindexed file".into()),
                            },
                            &mut best,
                            &mut queue,
                        );
                    }
                } else {
                    notes.push(format!(
                        "changed path {path} is outside every workspace crate"
                    ));
                }
            }
        }

        let mut visited_edges = 0usize;
        let mut max_frontier = queue.len();
        while let Some(id) = queue.pop_front() {
            let depth = best.get(&id).map(|(depth, _)| *depth).unwrap_or(u32::MAX);
            if depth >= max_depth {
                continue;
            }
            let Some(node) = self.node(&id) else {
                continue;
            };
            max_frontier = max_frontier.max(queue.len());
            let crate_expansion =
                matches!(node.kind, NodeKind::Crate | NodeKind::Package) && options.crate_tests;
            if crate_expansion {
                if let Some(tests) = self.tests_by_crate.get(&node.name) {
                    for test in tests {
                        seed(
                            test.clone(),
                            ImpactReason {
                                relation: ImpactRelation::Tests,
                                via: id.clone(),
                                provenance: Provenance::Coverage,
                                depth: depth + 1,
                                detail: Some("crate test expansion".into()),
                            },
                            &mut best,
                            &mut queue,
                        );
                    }
                }
            }
            if let Some(indices) = self.in_edges.get(&id) {
                for index in indices {
                    visited_edges += 1;
                    let edge = &self.graph.reverse_edges[*index];
                    // Reverse edge: edge.from == impacted node, edge.to ==
                    // the dependent/caller/importer to mark.
                    if matches!(node.kind, NodeKind::Crate | NodeKind::Package)
                        && !matches!(edge.kind, EdgeKind::DependsOn | EdgeKind::Tests)
                    {
                        // Crate impact propagates to dependent crates and
                        // their tests; it must not mark every file in the
                        // crate (that would be an O(files) false closure).
                        continue;
                    }
                    if node.kind == NodeKind::File && matches!(edge.kind, EdgeKind::Defines) {
                        // Symbols in the changed file are already seeded.
                        continue;
                    }
                    seed(
                        edge.to.clone(),
                        ImpactReason {
                            relation: edge.kind.into(),
                            via: id.clone(),
                            provenance: edge.provenance,
                            depth: depth + 1,
                            detail: edge.detail.clone(),
                        },
                        &mut best,
                        &mut queue,
                    );
                }
            }
        }

        let discovered = best.len();
        // Tests get a guaranteed share of the bound: a caller that asks for a
        // bounded impact set must never receive only symbols/crates because
        // their ids sort first.
        let mut test_ids: Vec<String> = Vec::new();
        let mut other_ids: Vec<String> = Vec::new();
        for id in best.keys() {
            match self.node(id).map(|node| node.kind) {
                Some(NodeKind::Test) => test_ids.push(id.clone()),
                _ => other_ids.push(id.clone()),
            }
        }
        test_ids.sort();
        other_ids.sort();
        let test_budget = if test_ids.is_empty() {
            0
        } else {
            (max_results / 2).max(1).min(test_ids.len())
        };
        let other_budget = max_results.saturating_sub(test_budget);
        let truncated = test_ids.len() > test_budget || other_ids.len() > other_budget;
        test_ids.truncate(test_budget);
        other_ids.truncate(other_budget);
        let mut affected = Vec::new();
        let mut tests = Vec::new();
        let mut files = BTreeSet::new();
        let mut symbols = BTreeSet::new();
        let mut crates = BTreeSet::new();
        let mut packages = BTreeSet::new();
        let mut ids = test_ids;
        ids.extend(other_ids);
        ids.sort();
        for id in &ids {
            let Some((distance, mut reasons)) = best.get(id).cloned() else {
                continue;
            };
            let Some(node) = self.node(id) else {
                continue;
            };
            reasons
                .sort_by(|a, b| (a.depth, a.relation, &a.via).cmp(&(b.depth, b.relation, &b.via)));
            reasons.dedup();
            let result = ImpactedNode {
                id: node.id.clone(),
                kind: node.kind,
                name: node.name.clone(),
                path: node.path.clone(),
                crate_name: node.crate_name.clone(),
                line: node.line,
                distance,
                reasons,
            };
            match node.kind {
                NodeKind::Test => tests.push(result),
                NodeKind::File => {
                    if let Some(path) = &node.path {
                        files.insert(path.clone());
                    }
                    affected.push(result);
                }
                NodeKind::Symbol => {
                    symbols.insert(node.name.clone());
                    affected.push(result);
                }
                NodeKind::Crate => {
                    crates.insert(node.name.clone());
                    affected.push(result);
                }
                NodeKind::Package => {
                    packages.insert(node.name.clone());
                    affected.push(result);
                }
                _ => affected.push(result),
            }
        }
        for path in &changed_paths {
            files.insert(path.clone());
        }
        let mut result = AffectedResult {
            schema: AFFECTED_SCHEMA.to_string(),
            revision: self.graph.revision.clone(),
            changed: changed_paths.into_iter().collect(),
            affected,
            tests,
            files: files.into_iter().collect(),
            symbols: symbols.into_iter().collect(),
            crates: crates.into_iter().collect(),
            packages: packages.into_iter().collect(),
            truncated,
            degraded: degraded || self.graph.stats.truncated,
            notes,
            stats: QueryStats {
                visited_nodes: best.len(),
                visited_edges,
                discovered,
                max_frontier,
            },
            digest: String::new(),
        };
        result.seal()?;
        Ok(result)
    }

    pub fn affected_paths(
        &self,
        changed: &[String],
        options: &AffectedOptions,
    ) -> Result<AffectedResult, GraphError> {
        let targets: Vec<ChangedTarget> =
            changed.iter().cloned().map(ChangedTarget::file).collect();
        self.affected(&targets, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        GraphEdge, GraphFile, GraphNode, GraphStats, Language, Revision, GRAPH_SCHEMA,
    };

    fn node(id: &str, name: &str, kind: NodeKind, path: &str) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            kind,
            name: name.to_string(),
            path: Some(path.to_string()),
            crate_name: Some("alpha".into()),
            line: Some(1),
            provenance: Provenance::Parser,
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind) -> GraphEdge {
        GraphEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind,
            provenance: Provenance::Parser,
            detail: None,
        }
    }

    fn fixture() -> Graph {
        let mut nodes = vec![
            node("crate:alpha", "alpha", NodeKind::Crate, "."),
            node("crate:beta", "beta", NodeKind::Crate, "beta"),
            node("file:src/lib.rs", "lib.rs", NodeKind::File, "src/lib.rs"),
            node(
                "symbol:src/lib.rs#answer@1",
                "answer",
                NodeKind::Symbol,
                "src/lib.rs",
            ),
            node("file:src/user.rs", "user.rs", NodeKind::File, "src/user.rs"),
            node(
                "symbol:src/user.rs#caller@1",
                "caller",
                NodeKind::Symbol,
                "src/user.rs",
            ),
            node(
                "test:src/user.rs#checks@5",
                "checks",
                NodeKind::Test,
                "src/user.rs",
            ),
        ];
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        let mut edges = vec![
            edge("crate:alpha", "crate:beta", EdgeKind::DependsOn),
            edge("crate:alpha", "file:src/lib.rs", EdgeKind::Includes),
            edge(
                "file:src/lib.rs",
                "symbol:src/lib.rs#answer@1",
                EdgeKind::Defines,
            ),
            edge(
                "file:src/user.rs",
                "symbol:src/user.rs#caller@1",
                EdgeKind::Defines,
            ),
            edge(
                "file:src/user.rs",
                "test:src/user.rs#checks@5",
                EdgeKind::Defines,
            ),
            edge(
                "symbol:src/user.rs#caller@1",
                "symbol:src/lib.rs#answer@1",
                EdgeKind::Calls,
            ),
            edge(
                "test:src/user.rs#checks@5",
                "symbol:src/lib.rs#answer@1",
                EdgeKind::Covers,
            ),
            edge("test:src/user.rs#checks@5", "crate:alpha", EdgeKind::Tests),
        ];
        edges.sort_by(|a, b| (&a.from, a.kind, &a.to).cmp(&(&b.from, b.kind, &b.to)));
        let reverse = edges
            .iter()
            .map(|edge| GraphEdge {
                from: edge.to.clone(),
                to: edge.from.clone(),
                kind: edge.kind,
                provenance: edge.provenance,
                detail: edge.detail.clone(),
            })
            .collect::<Vec<_>>();
        let mut reverse = reverse;
        reverse.sort_by(|a, b| (&a.from, a.kind, &a.to).cmp(&(&b.from, b.kind, &b.to)));
        let mut graph = Graph {
            schema: GRAPH_SCHEMA.to_string(),
            revision: Revision {
                commit: Some("abc".into()),
                tree: Some("def".into()),
                dirty: false,
            },
            workspace: faktor_core::id::WorkspaceId::new(1),
            files: vec![GraphFile {
                path: "src/lib.rs".into(),
                crate_name: "alpha".into(),
                language: Language::Rust,
                hash: "x".into(),
                bytes: 1,
                test_target: false,
            }],
            nodes,
            edges,
            reverse_edges: reverse,
            stats: GraphStats::default(),
            digest: String::new(),
        };
        graph.seal().unwrap();
        graph
    }

    #[test]
    fn symbol_change_finds_callers_and_tests_with_reasons() {
        let graph = ImpactGraph::new(fixture()).unwrap();
        let result = graph
            .affected(
                &[ChangedTarget::symbol("src/lib.rs", "answer")],
                &AffectedOptions {
                    crate_tests: false,
                    ..AffectedOptions::default()
                },
            )
            .unwrap();
        assert!(result.symbols.iter().any(|symbol| symbol == "caller"));
        assert!(result.tests.iter().any(|test| test.id.contains("checks")));
        let caller = result
            .affected
            .iter()
            .find(|node| node.name == "caller")
            .unwrap();
        assert!(caller
            .reasons
            .iter()
            .any(|reason| reason.relation == ImpactRelation::Calls));
    }

    #[test]
    fn crate_expansion_is_conservative_but_bounded() {
        let graph = ImpactGraph::new(fixture()).unwrap();
        let result = graph
            .affected(
                &[ChangedTarget::file("src/user.rs")],
                &AffectedOptions::default(),
            )
            .unwrap();
        assert!(result.tests.iter().any(|test| test.id.contains("checks")));
        assert!(result.crates.iter().any(|name| name == "alpha"));
        assert!(result.stats.visited_edges <= graph.graph.edges.len() * 2);
    }

    #[test]
    fn bounded_results_always_reserve_a_test_slot() {
        let graph = ImpactGraph::new(fixture()).unwrap();
        let result = graph
            .affected(
                &[ChangedTarget::file("src/user.rs")],
                &AffectedOptions {
                    max_results: 1,
                    ..AffectedOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.tests.len(), 1);
        assert!(result.truncated);
    }

    #[test]
    fn path_traversal_is_refused() {
        let graph = ImpactGraph::new(fixture()).unwrap();
        let error = graph
            .affected(
                &[ChangedTarget::file("../escape.rs")],
                &AffectedOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(error, GraphError::Malformed(_)));
    }

    #[test]
    fn crate_lookup_prefers_the_longest_matching_ancestor() {
        let crates = vec![
            node("crate:alpha", "alpha", NodeKind::Crate, "."),
            node("crate:beta", "beta", NodeKind::Crate, "beta"),
            node("crate:nested", "nested", NodeKind::Crate, "beta/nested/"),
        ];
        let mut nodes = crates;
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        let mut graph = Graph {
            schema: GRAPH_SCHEMA.to_string(),
            revision: Revision::default(),
            workspace: faktor_core::id::WorkspaceId::new(1),
            files: vec![],
            nodes,
            edges: vec![],
            reverse_edges: vec![],
            stats: GraphStats::default(),
            digest: String::new(),
        };
        graph.seal().unwrap();
        let graph = ImpactGraph::new(graph).unwrap();
        assert_eq!(
            graph.crate_for_path("beta/nested/src/lib.rs"),
            Some("nested")
        );
        assert_eq!(graph.crate_for_path("./beta/src/lib.rs"), Some("beta"));
        assert_eq!(graph.crate_for_path("README.md"), Some("alpha"));
        assert_eq!(graph.crate_for_path("beta"), Some("beta"));
    }

    /// Bounded probe (no soak): 20k lookups across 500 crates. The historical
    /// scan allocated two formatted prefixes per crate per lookup
    /// (O(paths x crates) allocations); the map probes only the path's
    /// ancestors. The ceiling is generous for a debug build but far below
    /// the quadratic path.
    #[test]
    #[ignore = "[perf] impact crate lookup at 500 crates / 20k paths"]
    fn perf_crate_lookup_is_linear_in_path_depth() {
        let mut nodes: Vec<GraphNode> = (0..500)
            .map(|i| {
                node(
                    &format!("crate:c{i:03}"),
                    &format!("c{i:03}"),
                    NodeKind::Crate,
                    &format!("crates/c{i:03}"),
                )
            })
            .collect();
        nodes.push(node("crate:root", "root", NodeKind::Crate, "."));
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        let mut graph = Graph {
            schema: GRAPH_SCHEMA.to_string(),
            revision: Revision::default(),
            workspace: faktor_core::id::WorkspaceId::new(1),
            files: vec![],
            nodes,
            edges: vec![],
            reverse_edges: vec![],
            stats: GraphStats::default(),
            digest: String::new(),
        };
        graph.seal().unwrap();
        let graph = ImpactGraph::new(graph).unwrap();
        let paths: Vec<String> = (0..20_000)
            .map(|i| format!("crates/c{:03}/src/f{}.rs", i % 500, i))
            .collect();
        let started = std::time::Instant::now();
        for (i, path) in paths.iter().enumerate() {
            assert_eq!(
                graph.crate_for_path(path),
                Some(format!("c{:03}", i % 500).as_str())
            );
        }
        let elapsed = started.elapsed();
        // LEGACY: the sorted vector + two `format!` allocations per crate.
        let mut legacy_dirs: Vec<(String, String)> = (0..500)
            .map(|i| (format!("crates/c{i:03}"), format!("c{i:03}")))
            .collect();
        legacy_dirs.push((".".to_string(), "root".to_string()));
        legacy_dirs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let legacy_started = std::time::Instant::now();
        for path in &paths {
            let found = legacy_dirs
                .iter()
                .find(|(dir, _)| dir == "." || path == dir || path.starts_with(&format!("{dir}/")))
                .map(|(_, name)| name.as_str());
            assert!(found.is_some());
        }
        let legacy = legacy_started.elapsed();
        println!("[perf] 20k crate lookups over 500 crates: map {elapsed:?} legacy {legacy:?}");
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "20k lookups took {elapsed:?}"
        );
        assert!(
            elapsed * 5 < legacy,
            "map lookup {elapsed:?} must beat the legacy scan {legacy:?} by 5x"
        );
    }
}
