// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! HAR-14 scale probe: the full ~1,000,000-line legacy corpus (generation,
//! indexing, entry-bounded snapshot) against the committed regression
//! budgets in `tests/eval/fixtures/budgets.json`.
//!
//! `#[ignore]`d manual lane (tag `[perf]`): never part of the default PR
//! run; execute explicitly with
//! `cargo test -p faktor-tests-eval --release --test fixtures_scale -- --ignored --nocapture`.
//! Registered in `scripts/certification/ignored-tests.json`.

use std::fs;
use std::path::Path;
use std::time::Instant;

use faktor_core::error::ErrorKind;
use faktor_core::id::WorkspaceId;
use faktor_fs::snapshot_tree;
use faktor_index::WorkspaceIndex;
use faktor_tests_eval::{generate_legacy, load_budgets, LEGACY_HUB_TOKEN, LEGACY_TARGET_LINES};

fn workspace_target() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/eval-fixtures")
}

/// Writes the probe's measurements next to the certification artifacts so a
/// baseline refresh can copy them (gitignored target/ tree).
fn record_measurements(measurements: &serde_json::Value) {
    let out_dir = workspace_target().join("../certification/evals");
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("fixtures_scale: cannot write measurement dir: {e}");
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = out_dir.join(format!("fixtures-scale-{stamp}.json"));
    match serde_json::to_string_pretty(measurements) {
        Ok(text) => {
            if let Err(e) = fs::write(&path, text) {
                eprintln!("fixtures_scale: cannot write {}: {e}", path.display());
            } else {
                println!("fixtures_scale: measurements -> {}", path.display());
            }
        }
        Err(e) => eprintln!("fixtures_scale: serialize: {e}"),
    }
}

#[test]
#[ignore = "[perf] manual 1M-line fixture corpus probe: generation + indexing + entry-bounded snapshot against committed budgets; run via `cargo test -p faktor-tests-eval --release --test fixtures_scale -- --ignored` (scripts/run-evals.mjs fixtures), never the PR lane"]
fn legacy_one_million_line_corpus_generation_indexing_and_bounds() {
    let root = workspace_target().join("legacy-1m");
    let budgets =
        load_budgets(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/budgets.json"))
            .expect("committed budgets fixture loads");

    // (1) Generation: exact one-million lines, seeded, regenerable.
    let started = Instant::now();
    let manifest = generate_legacy(&root, 42, LEGACY_TARGET_LINES).expect("generate legacy 1m");
    let generate_ms = started.elapsed().as_millis() as u64;
    assert_eq!(
        manifest.total_lines, LEGACY_TARGET_LINES,
        "the corpus must contain exactly one million lines"
    );
    let sum: u64 = manifest.files.iter().map(|f| f.lines).sum();
    assert_eq!(sum, LEGACY_TARGET_LINES, "manifest accounting is exact");
    assert!(
        manifest.files.len() >= 5_000,
        "expected >= 5000 files, got {}",
        manifest.files.len()
    );

    // (2) Entry-bounded snapshot behavior at 1M scale: a cap below the tree
    // size is a typed Oversized refusal (bounded work), never an unbounded
    // walk; a sufficient cap is a complete manifest.
    let started = Instant::now();
    let refused = snapshot_tree(&root, 1_024).expect_err("1024-entry cap must refuse 5k files");
    let refuse_ms = started.elapsed().as_millis() as u64;
    assert_eq!(refused.kind, ErrorKind::Oversized);

    let started = Instant::now();
    let snap = snapshot_tree(&root, 100_000).expect("full 1m snapshot within cap");
    let snapshot_full_ms = started.elapsed().as_millis() as u64;
    assert_eq!(snap.len(), manifest.files.len() + 1); // + manifest.json

    // (3) Bounded indexing: every file through the real WorkspaceIndex, then
    // a scale retrieval query for the hub token (ground truth from manifest).
    let ws = WorkspaceId::new(0xE7A1_1000);
    let started = Instant::now();
    let mut index = WorkspaceIndex::new();
    for (i, file) in manifest.files.iter().enumerate() {
        let bytes = fs::read(root.join(&file.path)).expect("legacy file reads");
        index
            .index_file(ws, Path::new(&file.path), &bytes, i as i64)
            .expect("legacy file indexes");
    }
    let index_ms = started.elapsed().as_millis() as u64;
    assert_eq!(
        index.file_count(ws),
        manifest.files.len(),
        "every corpus file is indexed"
    );
    // AND evidence query over the hub identifier (the index tokenizes on
    // non-alnum, so the literal splits into faktor/eval/legacy/hub).
    let mut parts: Vec<String> = LEGACY_HUB_TOKEN
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| p.len() >= 2)
        .map(str::to_string)
        .collect();
    parts.sort();
    parts.dedup();
    let mut hub_hits: Vec<String> = index
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
        hub_hits.retain(|p| hits.contains(p));
    }
    hub_hits.sort();
    let mut expected_hub: Vec<String> = manifest.crossref.token_files.clone();
    expected_hub.sort();
    assert_eq!(
        hub_hits, expected_hub,
        "scale retrieval must return exactly the manifest's hub-token ground truth"
    );
    let anchor = index.symbol_lookup(ws, "legacy_hub_anchor", 8);
    assert!(
        anchor
            .iter()
            .any(|(p, _)| p == "modules/mod0000/src/mod0000.rs"),
        "anchor symbol resolves at scale"
    );

    // (4) Regression budgets: fail only on egregious regression.
    budgets
        .check("legacy_1m_generate_ms", generate_ms)
        .expect("1m generation within committed bound");
    budgets
        .check("legacy_1m_index_ms", index_ms)
        .expect("1m indexing within committed bound");
    budgets
        .check("legacy_1m_snapshot_full_ms", snapshot_full_ms)
        .expect("full snapshot within committed bound");
    budgets
        .check("legacy_1m_snapshot_refuse_ms", refuse_ms)
        .expect("capped refusal within committed bound");

    println!(
        "measured: legacy_1m_generate_ms={generate_ms} legacy_1m_index_ms={index_ms} \
         legacy_1m_snapshot_full_ms={snapshot_full_ms} legacy_1m_snapshot_refuse_ms={refuse_ms}"
    );
    record_measurements(&serde_json::json!({
        "schema": "faktor-eval-fixtures-scale/v1",
        "generated_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        "files": manifest.files.len(),
        "lines": manifest.total_lines,
        "measured_ms": {
            "legacy_1m_generate_ms": generate_ms,
            "legacy_1m_index_ms": index_ms,
            "legacy_1m_snapshot_full_ms": snapshot_full_ms,
            "legacy_1m_snapshot_refuse_ms": refuse_ms,
        },
    }));
}
