//! faktor-impact — the repository relationship graph authority.
//!
//! One graph document (files, modules, symbols, tests, crates, packages and
//! their import/use/reference/call/implementation/coverage/dependency edges)
//! is built here and consumed by BOTH the native semantic `affected()`
//! operation and the CI change selector. There is no second heuristic graph:
//! consumers either read this document or fall back conservatively.
//!
//! Layers:
//! * [`model`] — typed schema, deterministic ordering, digest validation.
//! * [`manifest`] — supervised `cargo metadata` topology plus the bounded
//!   Cargo-unavailable fallback.
//! * [`topology`] — neutral npm/Gradle/Maven/Python/generic adapters.
//! * [`parse`] — bounded Rust/JS/TS/Python/JVM lexical parser.
//! * [`build`] — workspace scanner, graph assembly, incremental invalidation.
//! * [`query`] — O(V+E) impacted-set traversal with reasons/provenance.
//! * `git` (feature) — revision identity and changed-file adapter through
//!   the daemon's supervised git owner.
//! * `lsp` (feature) — rust-analyzer references/call-hierarchy/implementation
//!   enrichment through the ONE process supervisor.

pub mod build;
pub mod generation;
pub mod manifest;
pub mod model;
pub mod parse;
pub mod query;
pub mod topology;

#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "lsp")]
pub mod lsp;

/// Who owns an impact semantic subprocess (git or LSP).
///
/// These children are workspace or system resources, NEVER session
/// resources: a placeholder session id made ending session 1 kill semantic
/// subprocesses shared by every session. The two legitimate scopes are the
/// REAL workspace (workspace teardown reaps the child) and the standalone
/// daemon/CLI host that has no durable workspace registry entry.
#[cfg(any(feature = "git", feature = "lsp"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpactProcessOwner {
    /// Workspace-scoped semantic child: `ProcessOwner::Workspace(id)`.
    Workspace(faktor_core::id::WorkspaceId),
    /// The standalone system/daemon host: no user session owns the child.
    Daemon,
}

#[cfg(any(feature = "git", feature = "lsp"))]
impl ImpactProcessOwner {
    pub fn process_owner(self) -> faktor_terminal::ProcessOwner {
        match self {
            Self::Workspace(workspace) => faktor_terminal::ProcessOwner::Workspace(workspace),
            Self::Daemon => faktor_terminal::ProcessOwner::Daemon,
        }
    }
}

#[cfg(any(feature = "git", feature = "lsp"))]
impl From<faktor_core::id::WorkspaceId> for ImpactProcessOwner {
    fn from(workspace: faktor_core::id::WorkspaceId) -> Self {
        Self::Workspace(workspace)
    }
}

#[cfg(feature = "lsp")]
pub use lsp::{enrich_graph, LspOptions, LspStats};

pub use build::{
    is_topology_manifest, BuildOptions, BuildReport, GraphBuilder, ImpactSession, IncrementalStats,
    RefreshMode, RefreshPolicy, RefreshReport,
};
pub use generation::{
    DirtyPaths, GenerationLimits, MaterialGeneration, DEFAULT_MAX_DIRTY_PATHS,
    DEFAULT_MAX_GENERATIONS,
};
pub use manifest::{CrateInfo, TopologyOptions, Workspace};
pub use model::{
    crate_id, file_id, module_id, package_id, symbol_id, test_id, EdgeKind, Graph, GraphEdge,
    GraphError, GraphFile, GraphNode, GraphStats, Language, MaterialInput, NodeKind, Provenance,
    Revision, GRAPH_SCHEMA, MAX_EDGES, MAX_FILES, MAX_FILE_BYTES, MAX_INPUTS, MAX_NODES,
    MAX_PATH_BYTES, MAX_SYMBOL_BYTES,
};
pub use parse::{parse_file, ParsedFile};
pub use query::{
    AffectedOptions, AffectedResult, ChangedTarget, ImpactGraph, ImpactReason, ImpactRelation,
    ImpactedNode, QueryStats, DEFAULT_MAX_DEPTH, DEFAULT_MAX_RESULTS,
};
pub use topology::{
    AdapterKind, AdapterMode, AdapterOutcome, NativeProbe, Unit, UnitKind as TopologyUnitKind,
};
