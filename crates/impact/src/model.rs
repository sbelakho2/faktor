//! The serialized repository relationship graph: one deterministic document
//! shared by the native semantic authority and the CI change selector.
//!
//! The graph is a directed multigraph. Every forward edge has an explicit
//! kind and provenance; reverse edges are materialized in the same document
//! so consumers never invent a second traversal model. Nodes and edges are
//! sorted by their stable ids, which makes the wire bytes a pure function of
//! the repository content and the revision binding.

use std::collections::BTreeSet;

use faktor_core::id::WorkspaceId;
use serde::{Deserialize, Serialize};

pub const GRAPH_SCHEMA: &str = "faktor-impact-graph/v1";
pub const MAX_PATH_BYTES: usize = 4096;
pub const MAX_SYMBOL_BYTES: usize = 256;
pub const MAX_NODES: usize = 2_000_000;
pub const MAX_EDGES: usize = 8_000_000;
pub const MAX_FILES: usize = 200_000;
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Revision identity. A graph without a commit is explicitly unbound (for
/// synthetic fixtures); CI refuses an unbound graph at selection time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub commit: Option<String>,
    pub tree: Option<String>,
    #[serde(default)]
    pub dirty: bool,
}

impl Revision {
    pub fn is_bound(&self) -> bool {
        self.commit.as_deref().is_some_and(|c| !c.is_empty())
    }

    /// No commit, tree or dirty flag: the caller did not observe a repository
    /// revision. The impact authority replaces this with a material revision
    /// derived from the scanned file digests.
    pub fn is_unspecified(&self) -> bool {
        self.commit.is_none() && self.tree.is_none() && !self.dirty
    }

    /// Identity of an uncommitted material state: the digest of every scanned
    /// file plus a generation that advances only when content changes. This
    /// is a real dirty-worktree revision, not `Revision::default()`.
    pub fn material(digest: &str, generation: u64) -> Self {
        Self {
            commit: None,
            tree: Some(format!("{digest}#generation:{generation}")),
            dirty: true,
        }
    }
}

impl Revision {
    /// Read the commit identity without spawning git. `tree` stays `None`
    /// here (the CLI adapter supplies it through the supervised git owner);
    /// unborn/non-repository checkouts return an unbound revision.
    pub fn read_head(root: &std::path::Path) -> Self {
        let Some(git_dir) = git_dir(root) else {
            return Self::default();
        };
        let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else {
            return Self::default();
        };
        let head = head.trim();
        let commit = if let Some(reference) = head.strip_prefix("ref: ") {
            let common = common_dir(&git_dir);
            let direct = common.join(reference);
            if let Ok(value) = std::fs::read_to_string(&direct) {
                Some(value.trim().to_string())
            } else {
                std::fs::read_to_string(common.join("packed-refs"))
                    .ok()
                    .and_then(|packed| {
                        packed.lines().find_map(|line| {
                            let mut parts = line.split_whitespace();
                            match (parts.next(), parts.next()) {
                                (Some(oid), Some(name)) if name == reference => {
                                    Some(oid.to_string())
                                }
                                _ => None,
                            }
                        })
                    })
            }
        } else if head.len() == 40 && head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Some(head.to_string())
        } else {
            None
        };
        Self {
            commit,
            tree: None,
            dirty: false,
        }
    }
}

fn git_dir(root: &std::path::Path) -> Option<std::path::PathBuf> {
    let dot = root.join(".git");
    let metadata = std::fs::symlink_metadata(&dot).ok()?;
    if metadata.is_dir() {
        return Some(dot);
    }
    if metadata.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let path = text.trim().strip_prefix("gitdir: ")?.trim();
        let path = std::path::PathBuf::from(path);
        return Some(if path.is_absolute() {
            path
        } else {
            root.join(path)
        });
    }
    None
}

fn common_dir(git_dir: &std::path::Path) -> std::path::PathBuf {
    let Ok(text) = std::fs::read_to_string(git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let path = std::path::PathBuf::from(text.trim());
    if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Python,
    Java,
    Kotlin,
}

impl Language {
    pub fn of_path(path: &str) -> Option<Self> {
        let ext = path.rsplit('.').next()?;
        match ext {
            "rs" => Some(Self::Rust),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "ts" | "tsx" | "mts" | "cts" => Some(Self::TypeScript),
            "py" | "pyi" => Some(Self::Python),
            "java" => Some(Self::Java),
            "kt" | "kts" => Some(Self::Kotlin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Module,
    Symbol,
    Test,
    Crate,
    Package,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// File -> file module inclusion (`mod foo;`) or JS relative import.
    Imports,
    /// File -> crate/package external use (`use faktor_core::...`).
    Uses,
    /// File -> symbol/test definition.
    Defines,
    /// Symbol/file -> symbol identifier use.
    References,
    /// Symbol/file -> symbol call target.
    Calls,
    /// Type symbol -> trait symbol (`impl Trait for Type`).
    Implements,
    /// File -> module-node structural inclusion.
    Includes,
    /// Test -> covered symbol/file.
    Covers,
    /// Test / test-target file -> crate.
    Tests,
    /// Crate -> crate/package dependency.
    DependsOn,
    /// Crate -> integration test target file.
    TestTarget,
}

impl EdgeKind {
    pub const ALL: [EdgeKind; 11] = [
        EdgeKind::Imports,
        EdgeKind::Uses,
        EdgeKind::Defines,
        EdgeKind::References,
        EdgeKind::Calls,
        EdgeKind::Implements,
        EdgeKind::Includes,
        EdgeKind::Covers,
        EdgeKind::Tests,
        EdgeKind::DependsOn,
        EdgeKind::TestTarget,
    ];
}

/// Where an edge/node came from. LSP results supersede parser guesses for
/// the same edge; the parser's lexical edges remain for files the server did
/// not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Parser,
    Manifest,
    Lsp,
    Git,
    Coverage,
    Derived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphFile {
    pub path: String,
    pub crate_name: String,
    pub language: Language,
    pub hash: String,
    pub bytes: u64,
    #[serde(default)]
    pub test_target: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNode {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crate_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub provenance: Provenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GraphStats {
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
    pub reverse_edges: usize,
    pub symbols: usize,
    pub tests: usize,
    pub crates: usize,
    pub packages: usize,
    pub bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    pub schema: String,
    pub revision: Revision,
    pub workspace: WorkspaceId,
    pub files: Vec<GraphFile>,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Every forward edge materialized in the opposite direction. Consumers
    /// may use either view, but both are produced by the same authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverse_edges: Vec<GraphEdge>,
    pub stats: GraphStats,
    /// Blake3 over the canonical document with `digest` empty.
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    Schema {
        got: String,
    },
    Oversized {
        what: &'static str,
        max: usize,
        actual: usize,
    },
    Malformed(String),
    DuplicateNode(String),
    DuplicateEdge(String),
    DanglingEdge(String),
    ReverseMismatch(String),
    Unsorted(String),
    DigestMismatch {
        expected: String,
        actual: String,
    },
    CorruptJson(String),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Schema { got } => write!(f, "unsupported impact graph schema {got:?}"),
            Self::Oversized { what, max, actual } => {
                write!(f, "{what} exceeds bound: {actual} > {max}")
            }
            Self::Malformed(detail) => write!(f, "malformed impact graph: {detail}"),
            Self::DuplicateNode(id) => write!(f, "duplicate impact node {id}"),
            Self::DuplicateEdge(id) => write!(f, "duplicate impact edge {id}"),
            Self::DanglingEdge(id) => write!(f, "impact edge references unknown node: {id}"),
            Self::ReverseMismatch(id) => write!(f, "impact reverse edge mismatch: {id}"),
            Self::Unsorted(id) => write!(f, "impact graph is not deterministically ordered: {id}"),
            Self::DigestMismatch { expected, actual } => {
                write!(f, "impact graph digest mismatch: document says {expected} but canonical bytes hash to {actual}")
            }
            Self::CorruptJson(detail) => write!(f, "impact graph is unreadable: {detail}"),
        }
    }
}

impl std::error::Error for GraphError {}

pub fn validate_rel_path(raw: &str) -> Result<String, GraphError> {
    if raw.is_empty() {
        return Err(GraphError::Malformed("empty workspace path".into()));
    }
    if raw.len() > MAX_PATH_BYTES {
        return Err(GraphError::Oversized {
            what: "path",
            max: MAX_PATH_BYTES,
            actual: raw.len(),
        });
    }
    let normalized = raw.replace('\\', "/").replace("//", "/");
    let trimmed = normalized.trim_start_matches("./");
    if trimmed.starts_with('/') {
        return Err(GraphError::Malformed(format!(
            "absolute path is not workspace-relative: {raw:?}"
        )));
    }
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(GraphError::Malformed(format!(
                "path escapes or is not canonical: {raw:?}"
            )));
        }
        if segment.contains('\0') || segment.contains('\n') || segment.contains('\r') {
            return Err(GraphError::Malformed(format!(
                "path carries control characters: {raw:?}"
            )));
        }
    }
    Ok(trimmed.to_string())
}

pub fn validate_symbol(raw: &str) -> Result<(), GraphError> {
    if raw.is_empty() || raw.len() > MAX_SYMBOL_BYTES {
        return Err(GraphError::Oversized {
            what: "symbol",
            max: MAX_SYMBOL_BYTES,
            actual: raw.len(),
        });
    }
    if raw.chars().any(char::is_control) {
        return Err(GraphError::Malformed(format!(
            "symbol carries control characters: {raw:?}"
        )));
    }
    Ok(())
}

pub fn file_id(path: &str) -> String {
    format!("file:{path}")
}

pub fn module_id(path: &str) -> String {
    format!("module:{path}")
}

pub fn symbol_id(path: &str, name: &str, line: u32) -> String {
    format!("symbol:{path}#{name}@{line}")
}

pub fn test_id(path: &str, name: &str, line: u32) -> String {
    format!("test:{path}#{name}@{line}")
}

pub fn crate_id(name: &str) -> String {
    format!("crate:{name}")
}

pub fn package_id(name: &str) -> String {
    format!("package:{name}")
}

impl Graph {
    /// Validate structural invariants and deterministic ordering. Consumer
    /// entry points call this before trusting a serialized graph.
    pub fn validate(&self) -> Result<(), GraphError> {
        if self.schema != GRAPH_SCHEMA {
            return Err(GraphError::Schema {
                got: self.schema.clone(),
            });
        }
        if self.nodes.len() > MAX_NODES {
            return Err(GraphError::Oversized {
                what: "nodes",
                max: MAX_NODES,
                actual: self.nodes.len(),
            });
        }
        if self.edges.len() > MAX_EDGES {
            return Err(GraphError::Oversized {
                what: "edges",
                max: MAX_EDGES,
                actual: self.edges.len(),
            });
        }
        let mut ids = BTreeSet::new();
        let mut last: Option<&str> = None;
        for node in &self.nodes {
            validate_node_id(&node.id)?;
            if last.is_some_and(|prev| prev >= node.id.as_str()) {
                return Err(GraphError::Unsorted(node.id.clone()));
            }
            last = Some(&node.id);
            if !ids.insert(node.id.as_str()) {
                return Err(GraphError::DuplicateNode(node.id.clone()));
            }
        }
        let mut edges = BTreeSet::new();
        let mut last: Option<(String, EdgeKind, String)> = None;
        for edge in &self.edges {
            if !ids.contains(edge.from.as_str()) {
                return Err(GraphError::DanglingEdge(edge.from.clone()));
            }
            if !ids.contains(edge.to.as_str()) {
                return Err(GraphError::DanglingEdge(edge.to.clone()));
            }
            let key = (edge.from.clone(), edge.kind, edge.to.clone());
            if last.as_ref().is_some_and(|prev| prev >= &key) {
                return Err(GraphError::Unsorted(format!(
                    "{} -> {} ({:?})",
                    edge.from, edge.to, edge.kind
                )));
            }
            last = Some(key.clone());
            if !edges.insert(key) {
                return Err(GraphError::DuplicateEdge(format!(
                    "{} -> {}",
                    edge.from, edge.to
                )));
            }
        }
        let mut reverse = BTreeSet::new();
        for edge in &self.reverse_edges {
            let key = (edge.from.clone(), edge.kind, edge.to.clone());
            let forward = (edge.to.clone(), edge.kind, edge.from.clone());
            if !edges.contains(&forward) {
                return Err(GraphError::ReverseMismatch(format!("{key:?}")));
            }
            reverse.insert(key);
        }
        for (from, kind, to) in &edges {
            if !reverse.contains(&(to.clone(), *kind, from.clone())) {
                return Err(GraphError::ReverseMismatch(format!(
                    "{from} -> {to} ({kind:?})"
                )));
            }
        }
        Ok(())
    }

    /// Deterministic canonical bytes: the digest field is cleared, the
    /// document is serialized once, then the digest is computed over exactly
    /// those bytes.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, GraphError> {
        let mut copy = self.clone();
        copy.digest.clear();
        serde_json::to_vec(&copy).map_err(|e| GraphError::CorruptJson(e.to_string()))
    }

    pub fn compute_digest(&self) -> Result<String, GraphError> {
        Ok(format!(
            "blake3:{}",
            blake3::hash(&self.canonical_bytes()?).to_hex()
        ))
    }

    pub fn seal(&mut self) -> Result<(), GraphError> {
        self.digest = self.compute_digest()?;
        Ok(())
    }

    pub fn from_json(text: &str) -> Result<Self, GraphError> {
        let graph: Graph =
            serde_json::from_str(text).map_err(|e| GraphError::CorruptJson(e.to_string()))?;
        graph.validate()?;
        let expected = graph.compute_digest()?;
        if graph.digest != expected {
            return Err(GraphError::DigestMismatch {
                expected: graph.digest,
                actual: expected,
            });
        }
        Ok(graph)
    }

    pub fn to_json_pretty(&self) -> Result<String, GraphError> {
        serde_json::to_string_pretty(self).map_err(|e| GraphError::CorruptJson(e.to_string()))
    }

    pub fn to_json_compact(&self) -> Result<String, GraphError> {
        serde_json::to_string(self).map_err(|e| GraphError::CorruptJson(e.to_string()))
    }

    /// The change-selector projection: crate/package dependency edges, test
    /// target edges and file-level coverage. Caller/reference edges and
    /// symbol nodes stay in the native graph (the product `affected` API
    /// serves them); the CI selector gets a bounded document whose size is a
    /// function of crates, files and tests, not of every call site.
    pub fn selector_projection(&self) -> Result<Self, GraphError> {
        use std::collections::BTreeMap;
        let mut file_by_path: BTreeMap<&str, &str> = BTreeMap::new();
        let mut node_path: BTreeMap<&str, &str> = BTreeMap::new();
        for node in &self.nodes {
            if let Some(path) = node.path.as_deref() {
                node_path.insert(node.id.as_str(), path);
                if node.kind == NodeKind::File {
                    file_by_path.insert(path, node.id.as_str());
                }
            }
        }
        let mut edges: Vec<GraphEdge> = Vec::new();
        for edge in &self.edges {
            match edge.kind {
                EdgeKind::DependsOn | EdgeKind::Tests | EdgeKind::TestTarget => {
                    edges.push(edge.clone());
                }
                EdgeKind::Covers => {
                    let Some(path) = node_path.get(edge.to.as_str()) else {
                        continue;
                    };
                    let Some(file) = file_by_path.get(path) else {
                        continue;
                    };
                    let mut cover = edge.clone();
                    cover.to = (*file).to_string();
                    edges.push(cover);
                }
                _ => {}
            }
        }
        edges.sort_by(|a, b| (&a.from, a.kind, &a.to).cmp(&(&b.from, b.kind, &b.to)));
        edges.dedup_by(|a, b| a.from == b.from && a.kind == b.kind && a.to == b.to);
        let mut copy = self.clone();
        copy.nodes.retain(|node| {
            matches!(
                node.kind,
                NodeKind::Crate | NodeKind::Package | NodeKind::File | NodeKind::Test
            )
        });
        copy.edges = edges;
        copy.rebuild_reverse();
        copy.recount();
        copy.stats.truncated = self.stats.truncated;
        copy.seal()?;
        Ok(copy)
    }

    /// Rebuild the materialized reverse edges from the forward edges.
    pub fn rebuild_reverse(&mut self) {
        let mut reverse: Vec<GraphEdge> = self
            .edges
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
        self.reverse_edges = reverse;
    }

    /// Recompute node/edge counters after an additive enrichment pass or a
    /// projection.
    pub fn recount(&mut self) {
        self.stats.nodes = self.nodes.len();
        self.stats.edges = self.edges.len();
        self.stats.reverse_edges = self.reverse_edges.len();
        self.stats.files = self.files.len();
        self.stats.symbols = self
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Symbol)
            .count();
        self.stats.tests = self
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Test)
            .count();
        self.stats.crates = self
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Crate)
            .count();
        self.stats.packages = self
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Package)
            .count();
        self.stats.bytes = self.files.iter().map(|file| file.bytes).sum();
    }

    /// Deterministic merge of externally enriched nodes/edges (LSP). Existing
    /// parser edges are upgraded in place when the new provenance is
    /// stronger.
    pub fn merge_enrichment(&mut self, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) {
        for node in nodes {
            match self
                .nodes
                .binary_search_by(|existing| existing.id.cmp(&node.id))
            {
                Ok(index) => {
                    if rank(node.provenance) > rank(self.nodes[index].provenance) {
                        self.nodes[index] = node;
                    }
                }
                Err(index) => self.nodes.insert(index, node),
            }
        }
        for edge in edges {
            let key = |candidate: &GraphEdge| {
                (candidate.from.clone(), candidate.kind, candidate.to.clone())
            };
            let needle = key(&edge);
            match self
                .edges
                .binary_search_by(|existing| key(existing).cmp(&needle))
            {
                Ok(index) => {
                    if rank(edge.provenance) > rank(self.edges[index].provenance) {
                        self.edges[index] = edge;
                    }
                }
                Err(index) => self.edges.insert(index, edge),
            }
        }
        self.rebuild_reverse();
        self.recount();
        let _ = self.seal();
    }

    pub fn crate_names(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Crate)
            .map(|n| n.name.clone())
            .collect()
    }
}

fn validate_node_id(id: &str) -> Result<(), GraphError> {
    if id.len() > MAX_PATH_BYTES + MAX_SYMBOL_BYTES + 32 {
        return Err(GraphError::Oversized {
            what: "node id",
            max: MAX_PATH_BYTES + MAX_SYMBOL_BYTES + 32,
            actual: id.len(),
        });
    }
    if id.chars().any(char::is_control) {
        return Err(GraphError::Malformed(format!(
            "node id carries control characters: {id:?}"
        )));
    }
    let Some((kind, rest)) = id.split_once(':') else {
        return Err(GraphError::Malformed(format!(
            "node id has no kind: {id:?}"
        )));
    };
    if !matches!(
        kind,
        "file" | "module" | "symbol" | "test" | "crate" | "package"
    ) {
        return Err(GraphError::Malformed(format!(
            "node id has unknown kind {kind:?}: {id:?}"
        )));
    }
    if rest.is_empty() {
        return Err(GraphError::Malformed(format!(
            "node id has empty name: {id:?}"
        )));
    }
    Ok(())
}

pub fn sort_graph(nodes: &mut [GraphNode], edges: &mut [GraphEdge]) {
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    edges.sort_by(|a, b| (&a.from, a.kind, &a.to).cmp(&(&b.from, b.kind, &b.to)));
}

fn rank(provenance: Provenance) -> u8 {
    match provenance {
        Provenance::Lsp => 6,
        Provenance::Git => 5,
        Provenance::Coverage => 4,
        Provenance::Parser => 3,
        Provenance::Manifest => 2,
        Provenance::Derived => 1,
    }
}
