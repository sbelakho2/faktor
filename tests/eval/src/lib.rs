// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! faktor-tests-eval — HAR-14 fixture-repository generators and the offline
//! half of the HAR-09/HAR-17 eval harness.
//!
//! This crate owns the DETERMINISTIC, SEEDED generators for the fixture
//! repositories the eval programme needs:
//!
//! - a polyglot monorepo (Rust + TypeScript + Python + Go + Kotlin) with real
//!   cross-references between the language subtrees (call sites, subprocess/
//!   HTTP boundaries, tests sharing one cross-language constant) — HAR-14;
//! - a ~1,000,000-line legacy repo (generated, gitignored, regenerable;
//!   lives under `target/eval-fixtures/`) — HAR-14.
//!
//! and the OFFLINE measurements over them (retrieval/evidence ground truth
//! through `faktor-index`, bounded indexing time, `snapshot_tree` entry-bound
//! behavior), checked against the committed regression budgets in
//! `tests/eval/fixtures/budgets.json`. Budgets fail only on egregious
//! regression (measured > committed bound, where bound = max(3x baseline,
//! absolute floor)); they are never flaky wall-clocks.
//!
//! The REAL-PROVIDER conversation-quality suite (HAR-09/HAR-17) is driven by
//! `scripts/run-evals.mjs` against a throwaway daemon; its gate evaluator and
//! report template live in `tests/eval/fixtures/` next to the gate selftest
//! corpus. Nothing in this crate mutates session state or talks to a model.
//!
//! Generated corpora are written ONLY under a caller-provided root; the
//! committed tree contains generators, budgets and templates — never the
//! generated corpora.

pub mod budget;
pub mod legacy;
pub mod polyglot;
pub mod prng;

pub use budget::{load_budgets, Budget, Budgets};
pub use legacy::{generate_legacy, LEGACY_HUB_TOKEN, LEGACY_TARGET_LINES};
pub use polyglot::{
    fnv1a64, generate_polyglot, CorpusManifest, CrossrefManifest, FileRecord, SymbolGround,
    CROSSREF_TOKEN,
};
pub use prng::SplitMix64;

/// FNV-1a 64-bit offset basis (the well-known constants; the generator uses
/// them to seed every language subtree's shared test constant).
pub const FNV64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
pub const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;
