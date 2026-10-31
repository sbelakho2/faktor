// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Offline (no daemon, no model) HAR-14 fixture tests: generator
//! determinism, cross-language retrieval/evidence ground truth through the
//! real `faktor-index`, entry-bounded `snapshot_tree` behavior, and the
//! committed regression budgets.
//!
//! The 1,000,000-line probe is in `fixtures_scale.rs` (`#[ignore]`d manual
//! lane, registered in scripts/certification/ignored-tests.json).

use std::fs;
use std::path::Path;
use std::time::Instant;

use faktor_core::error::ErrorKind;
use faktor_core::id::WorkspaceId;
use faktor_fs::snapshot_tree;
use faktor_index::WorkspaceIndex;
use faktor_tests_eval::polyglot::language_root_of;
use faktor_tests_eval::{
    generate_legacy, generate_polyglot, load_budgets, CROSSREF_TOKEN, LEGACY_HUB_TOKEN,
};

/// Small legacy corpus for the offline lane (the 1M probe is #[ignore]d).
const LEGACY_SMALL_LINES: u64 = 20_000;

fn workspace_target() -> std::path::PathBuf {
    // tests/eval -> workspace root -> target/eval-fixtures (gitignored).
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/eval-fixtures")
}

fn budgets() -> faktor_tests_eval::Budgets {
    load_budgets(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/budgets.json"))
        .expect("committed budgets fixture loads")
}

/// Evidence query over a literal identifier: tokenize it the way the index
/// does (alnum runs) and intersect the per-token postings — the AND query a
/// retrieval/evidence surface actually answers (underscores split).
fn and_query(index: &WorkspaceIndex, ws: WorkspaceId, literal: &str) -> Vec<String> {
    let mut parts: Vec<String> = literal
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| p.len() >= 2)
        .map(str::to_string)
        .collect();
    parts.sort();
    parts.dedup();
    let mut result: Vec<String> = index
        .files_for_token(ws, &parts[0], 100_000)
        .into_iter()
        .map(|h| h.path)
        .collect();
    for part in &parts[1..] {
        let hits: std::collections::BTreeSet<String> = index
            .files_for_token(ws, part, 100_000)
            .into_iter()
            .map(|h| h.path)
            .collect();
        result.retain(|p| hits.contains(p));
    }
    result.sort();
    result
}

/// Index every manifest file into a fresh WorkspaceIndex (the same read
/// path retrieval/evidence queries use).
fn index_manifest_files(
    root: &Path,
    manifest: &faktor_tests_eval::CorpusManifest,
) -> WorkspaceIndex {
    let ws = WorkspaceId::new(0xE7A1);
    let mut index = WorkspaceIndex::new();
    for (i, file) in manifest.files.iter().enumerate() {
        let bytes = fs::read(root.join(&file.path)).expect("fixture file reads");
        index
            .index_file(ws, Path::new(&file.path), &bytes, i as i64)
            .expect("fixture file indexes");
    }
    index
}

#[test]
fn polyglot_generation_is_deterministic_for_a_seed() {
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let dir_c = tempfile::tempdir().expect("tempdir c");
    let a = generate_polyglot(dir_a.path(), 0xC0FFEE).expect("generate a");
    let b = generate_polyglot(dir_b.path(), 0xC0FFEE).expect("generate b");
    let c = generate_polyglot(dir_c.path(), 0xC0FFEE ^ 1).expect("generate c");
    // Same seed: identical manifests (paths, line counts, content hashes).
    assert_eq!(a, b, "same seed must produce byte-identical corpora");
    // Different seed: the seeded fillers/attachment marker must diverge.
    assert_ne!(
        a.crossref.attachment_marker, c.crossref.attachment_marker,
        "attachment marker is seed-derived"
    );
    assert_ne!(
        a.files.iter().map(|f| &f.blake3).collect::<Vec<_>>(),
        c.files.iter().map(|f| &f.blake3).collect::<Vec<_>>(),
        "different seeds must produce different content"
    );
}

#[test]
fn polyglot_cross_language_retrieval_ground_truth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate_polyglot(dir.path(), 42).expect("generate");
    let ws = WorkspaceId::new(0xE7A1);

    // The committed manifest ground truth itself: the crossref token files
    // span all five language subtrees.
    let mut roots_seen: Vec<&str> = manifest
        .crossref
        .token_files
        .iter()
        .filter_map(|f| language_root_of(f))
        .collect();
    roots_seen.sort_unstable();
    roots_seen.dedup();
    assert_eq!(
        roots_seen,
        vec!["go/", "kotlin/", "python/", "rust/", "ts/"],
        "manifest token_files must span all five languages"
    );

    // The REAL index answers the same evidence query across languages.
    let index = index_manifest_files(dir.path(), &manifest);
    let hits = and_query(&index, ws, CROSSREF_TOKEN);
    let mut hit_roots: Vec<&str> = hits.iter().filter_map(|f| language_root_of(f)).collect();
    hit_roots.sort_unstable();
    hit_roots.dedup();
    assert_eq!(
        hit_roots,
        vec!["go/", "kotlin/", "python/", "rust/", "ts/"],
        "the lexical index must return cross-language hits for the crossref token"
    );
    let mut expected: Vec<String> = manifest.crossref.token_files.clone();
    expected.sort();
    assert_eq!(
        hits, expected,
        "the evidence query must return exactly the manifest ground truth"
    );

    // Symbol ground truth the tree-sitter extractor covers (rust/ts/python);
    // go/kotlin are asserted through the token index above.
    for symbol in &manifest.crossref.symbols {
        match symbol.language.as_str() {
            "rust" | "typescript" | "python" => {
                let hits = index.symbol_lookup(ws, &symbol.name, 32);
                assert!(
                    hits.iter().any(|(path, _)| path == &symbol.path),
                    "symbol '{}' must resolve to {} (got {:?})",
                    symbol.name,
                    symbol.path,
                    hits.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>()
                );
            }
            _ => {}
        }
    }

    // The attachment marker is unique to the Rust core file (+ the README
    // that documents it as ground truth); no other subtree may carry it.
    let marker_hits = and_query(&index, ws, &manifest.crossref.attachment_marker);
    assert_eq!(
        marker_hits,
        vec![
            "README.md".to_string(),
            "rust/telemetry_core/src/lib.rs".to_string()
        ],
        "attachment marker must stay unique to the rust core + its ground-truth doc"
    );
}

#[test]
fn polyglot_snapshot_tree_is_entry_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = generate_polyglot(dir.path(), 42).expect("generate");
    let file_count = manifest.files.len();
    let snap = snapshot_tree(dir.path(), 10_000).expect("full snapshot within cap");
    assert_eq!(
        snap.len(),
        file_count + 1, // + manifest.json
        "snapshot covers every file when the cap allows it"
    );
    // Entry-bound behavior: a cap below the tree size is a typed Oversized
    // refusal, never a silent truncation and never an unbounded walk.
    let err = snapshot_tree(dir.path(), 3).expect_err("cap below tree size must refuse");
    assert_eq!(err.kind, ErrorKind::Oversized);
}

#[test]
fn legacy_small_corpus_is_line_exact_entry_bounded_and_retrievable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let started = Instant::now();
    let manifest = generate_legacy(dir.path(), 7, LEGACY_SMALL_LINES).expect("generate");
    let generate_ms = started.elapsed().as_millis() as u64;
    assert_eq!(manifest.total_lines, LEGACY_SMALL_LINES);
    let sum: u64 = manifest.files.iter().map(|f| f.lines).sum();
    assert_eq!(sum, LEGACY_SMALL_LINES, "manifest line accounting is exact");

    // Entry-bound snapshot behavior at small scale: cap below the tree is a
    // typed refusal; a sufficient cap is a complete deterministic manifest.
    let refused = snapshot_tree(dir.path(), 4).expect_err("tiny cap must refuse");
    assert_eq!(refused.kind, ErrorKind::Oversized);
    let started = Instant::now();
    let snap = snapshot_tree(dir.path(), 10_000).expect("full snapshot");
    let snapshot_ms = started.elapsed().as_millis() as u64;
    assert_eq!(snap.len(), manifest.files.len() + 1); // + manifest.json

    // Retrieval ground truth still holds on the legacy shape (AND query over
    // the hub identifier must equal the manifest's hub-token ground truth).
    let index = index_manifest_files(dir.path(), &manifest);
    let ws = WorkspaceId::new(0xE7A1);
    let hub_hits = and_query(&index, ws, LEGACY_HUB_TOKEN);
    let mut expected_hub: Vec<String> = manifest.crossref.token_files.clone();
    expected_hub.sort();
    assert_eq!(
        hub_hits, expected_hub,
        "hub evidence query must return exactly the manifest ground truth"
    );
    assert!(
        hub_hits.len() >= 4,
        "hub docs + hub-citing modules expected (got {})",
        hub_hits.len()
    );
    let anchor = index.symbol_lookup(ws, "legacy_hub_anchor", 8);
    assert!(
        anchor
            .iter()
            .any(|(p, _)| p == "modules/mod0000/src/mod0000.rs"),
        "anchor symbol must resolve in mod0000"
    );

    let budgets = budgets();
    budgets
        .check("legacy_small_generate_ms", generate_ms)
        .expect("legacy small generation stays within its committed bound");
    budgets
        .check("legacy_small_snapshot_ms", snapshot_ms)
        .expect("legacy small snapshot stays within its committed bound");
}

#[test]
fn polyglot_indexing_and_snapshot_stay_within_committed_budgets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let started = Instant::now();
    let manifest = generate_polyglot(dir.path(), 42).expect("generate");
    let generate_ms = started.elapsed().as_millis() as u64;

    let started = Instant::now();
    let index = index_manifest_files(dir.path(), &manifest);
    let index_ms = started.elapsed().as_millis() as u64;
    assert!(index.file_count(WorkspaceId::new(0xE7A1)) > 0);

    let started = Instant::now();
    let snap = snapshot_tree(dir.path(), 10_000).expect("snapshot");
    let snapshot_ms = started.elapsed().as_millis() as u64;
    assert_eq!(snap.len(), manifest.files.len() + 1);

    let budgets = budgets();
    budgets
        .check("polyglot_generate_ms", generate_ms)
        .expect("polyglot generation stays within its committed bound");
    budgets
        .check("polyglot_index_ms", index_ms)
        .expect("polyglot indexing stays within its committed bound");
    budgets
        .check("polyglot_snapshot_ms", snapshot_ms)
        .expect("polyglot snapshot stays within its committed bound");
    // Print the measurements so a baseline refresh can copy them.
    println!(
        "measured: polyglot_generate_ms={generate_ms} polyglot_index_ms={index_ms} polyglot_snapshot_ms={snapshot_ms}"
    );
}

#[test]
fn committed_budgets_are_well_formed() {
    let budgets = budgets();
    assert!(!budgets.budgets.is_empty());
    for (key, budget) in &budgets.budgets {
        assert!(
            budget.bound_ms >= budget.baseline_ms,
            "budget '{key}': bound must not be below the recorded baseline"
        );
        assert!(
            budget.bound_ms >= budget.baseline_ms.saturating_mul(3) || budget.bound_ms >= 1_000,
            "budget '{key}': bound must be 3x baseline or an absolute floor"
        );
    }
    for required in [
        "polyglot_generate_ms",
        "polyglot_index_ms",
        "polyglot_snapshot_ms",
        "legacy_small_generate_ms",
        "legacy_small_snapshot_ms",
        "legacy_1m_generate_ms",
        "legacy_1m_index_ms",
        "legacy_1m_snapshot_full_ms",
        "legacy_1m_snapshot_refuse_ms",
    ] {
        assert!(
            budgets.budgets.contains_key(required),
            "budgets.json is missing required key '{required}'"
        );
    }
}

#[test]
fn polyglot_workspace_target_renders_deterministically_for_the_runner() {
    // The runner regenerates the committed-seed corpus under target/ (never
    // inside the repo tree); prove the runner's default seed is stable by
    // generating twice into temp dirs and comparing manifests.
    let a_dir = tempfile::tempdir().expect("tempdir a");
    let b_dir = tempfile::tempdir().expect("tempdir b");
    let a = generate_polyglot(a_dir.path(), 42).expect("generate a");
    let b = generate_polyglot(b_dir.path(), 42).expect("generate b");
    assert_eq!(a.files, b.files);
    let _ = workspace_target(); // path shape exercised; written only by bin/runner
}
