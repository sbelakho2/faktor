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
//! * [`manifest`] — Cargo workspace/package dependency adapter.
//! * [`parse`] — bounded Rust/JS/TS lexical parser.
//! * [`build`] — workspace scanner, graph assembly, incremental invalidation.
//! * [`query`] — O(V+E) impacted-set traversal with reasons/provenance.
//! * `git` (feature) — revision identity and changed-file adapter through
//!   the daemon's supervised git owner.
//! * `lsp` (feature) — rust-analyzer references/call-hierarchy/implementation
//!   enrichment through the ONE process supervisor.

pub mod build;
pub mod manifest;
pub mod model;
pub mod parse;
pub mod query;

#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "lsp")]
pub mod lsp;
#[cfg(feature = "lsp")]
pub use lsp::{enrich_graph, LspOptions, LspStats};

pub use build::{BuildOptions, BuildReport, GraphBuilder, ImpactSession, IncrementalStats};
pub use manifest::{CrateInfo, Workspace};
pub use model::{
    crate_id, file_id, module_id, package_id, symbol_id, test_id, EdgeKind, Graph, GraphEdge,
    GraphError, GraphFile, GraphNode, GraphStats, Language, NodeKind, Provenance, Revision,
    GRAPH_SCHEMA, MAX_EDGES, MAX_FILES, MAX_FILE_BYTES, MAX_NODES, MAX_PATH_BYTES,
    MAX_SYMBOL_BYTES,
};
pub use parse::{parse_file, ParsedFile};
pub use query::{
    AffectedOptions, AffectedResult, ChangedTarget, ImpactGraph, ImpactReason, ImpactRelation,
    ImpactedNode, QueryStats, DEFAULT_MAX_DEPTH, DEFAULT_MAX_RESULTS,
};
