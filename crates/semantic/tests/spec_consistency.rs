//! Normative spec consistency: the autonomous-engineering document must
//! describe the SAME per-operation quality model the code enforces.

use std::path::Path;

fn spec() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/autonomous-engineering.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn autonomous_engineering_spec_matches_per_operation_quality_model() {
    let raw = spec();
    let folded = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    for needle in [
        "per-operation",
        "fidelity",
        "completeness",
        "evidence dimensions",
        "provenance",
        "not limited to tier 0",
        "filesystem + lexical",
        "tree-sitter",
        "lsp / compiler",
        "dependency / call graph",
        "runtime + historical",
        "enriched external",
        "revision-bound local graph",
        "not a competing authoritative system model",
        "one selection gate",
        "tangerine",
        "kiwi",
        "optional",
    ] {
        assert!(
            folded.contains(needle),
            "the normative spec is missing {needle:?}; docs/specs/autonomous-engineering.md must describe the per-operation quality model"
        );
    }
    // The ladder remains bounded and the optionality rule is unchanged.
    assert!(raw.contains("0..=5"), "tiers must stay bounded 0..=5");
    assert!(
        folded.contains("never required"),
        "Tangerine/Kiwi must stay optional, never required"
    );
}
