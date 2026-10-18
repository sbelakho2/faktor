//! Shared context types (used by the wire planner and the compactor):
//! recent text turns and retrieved-evidence entries.
//!
//! The full budgeted context engine lives in [`crate::wire_plan`] — the
//! assembler's earlier engine duplicated its budget math and was never
//! wired into the agent; it is removed so one engine owns the budget.

/// One compacted/wire text turn: role + bounded text (the compactor and the
/// recent-history loaders both speak this shape).
///
/// `Serialize`/`Deserialize` exist for the DURABLE context checkpoint: an
/// accepted compaction persists exactly these turns (plus the source
/// high-water seq) so a restarted daemon rebuilds the same context instead
/// of replaying history that was already summarized.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecentTurn {
    pub role: String,
    pub text: String,
}

/// One retrieved-evidence hit (spec §20): the file, a snippet, and a score
/// for ordering. The wire planner renders high-score hits inline first.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub path: String,
    pub snippet: String,
    pub score: f64,
}
