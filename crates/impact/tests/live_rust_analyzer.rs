#![cfg(feature = "lsp")]

use std::path::PathBuf;

use faktor_impact::{
    AffectedOptions, ChangedTarget, GraphBuilder, ImpactGraph, LspOptions, Revision,
};

static LIVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn sample_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fixtures/impact-sample")
}

fn rust_files() -> Vec<String> {
    vec![
        "alpha/src/lib.rs".to_string(),
        "beta/src/lib.rs".to_string(),
        "beta/tests/answer.rs".to_string(),
    ]
}

/// Live rust-analyzer proof on a real (small) Cargo workspace: a real symbol
/// change discovers hand-verified callers and the integration test.
///
/// This is a long live test (ignored by default):
/// `cargo test -p faktor-impact --features lsp --test live_rust_analyzer -- --ignored`
#[test]
#[ignore = "[fault] live rust-analyzer sample workspace"]
fn fault_live_rust_analyzer_relationships() {
    let _guard = LIVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = sample_root();
    assert!(root.join("Cargo.toml").exists());
    let mut builder = GraphBuilder::new(&root, Revision::default()).unwrap();
    let mut graph = builder.build().unwrap();
    let supervisor = std::sync::Arc::new(faktor_terminal::ProcessSupervisor::try_shared().unwrap());
    let stats = faktor_impact::lsp::enrich_graph(
        &mut graph,
        &root,
        &rust_files(),
        &LspOptions {
            timeout: std::time::Duration::from_secs(90),
            settle: std::time::Duration::from_secs(20),
            ..LspOptions::default()
        },
        &supervisor,
    )
    .unwrap();
    assert!(!stats.degraded, "lsp degraded: {:?}", stats.note);
    assert!(stats.files_opened >= 3);
    assert!(
        stats.references + stats.calls > 0,
        "no LSP relationships: {stats:?}"
    );
    let graph = ImpactGraph::new(graph).unwrap();
    let result = graph
        .affected(
            &[ChangedTarget::symbol("alpha/src/lib.rs", "answer")],
            &AffectedOptions {
                crate_tests: false,
                ..AffectedOptions::default()
            },
        )
        .unwrap();
    let names: Vec<&str> = result
        .affected
        .iter()
        .map(|node| node.name.as_str())
        .collect();
    assert!(names.contains(&"double"), "callers: {names:?}");
    assert!(names.contains(&"call_answer"), "callers: {names:?}");
    let tests: Vec<&str> = result.tests.iter().map(|node| node.name.as_str()).collect();
    assert!(tests.contains(&"answer_is_42"), "tests: {tests:?}");
    assert!(result.affected.iter().any(|node| node
        .reasons
        .iter()
        .any(|reason| { reason.provenance == faktor_impact::Provenance::Lsp })));
}

/// Live rust-analyzer proof on THIS repository: enriching the real impact
/// crate discovers caller references from the CLI and test files.
#[test]
#[ignore = "[fault] live rust-analyzer on this repository"]
fn fault_live_rust_analyzer_this_repository() {
    let _guard = LIVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut builder = GraphBuilder::new(&root, Revision::read_head(&root)).unwrap();
    let mut graph = builder.build().unwrap();
    let supervisor = std::sync::Arc::new(faktor_terminal::ProcessSupervisor::try_shared().unwrap());
    let stats = faktor_impact::lsp::enrich_graph(
        &mut graph,
        &root,
        &[
            "crates/impact/src/query.rs".into(),
            "crates/impact/src/lib.rs".into(),
        ],
        &LspOptions {
            timeout: std::time::Duration::from_secs(20),
            settle: std::time::Duration::from_secs(60),
            max_requests: 2048,
            max_duration: std::time::Duration::from_secs(240),
            ..LspOptions::default()
        },
        &supervisor,
    )
    .unwrap();
    assert!(
        stats.references + stats.calls > 0,
        "no LSP relationships (degraded={}): {stats:?}",
        stats.degraded
    );
    let graph = ImpactGraph::new(graph).unwrap();
    let result = graph
        .affected(
            &[ChangedTarget::symbol(
                "crates/impact/src/query.rs",
                "affected",
            )],
            &AffectedOptions {
                crate_tests: false,
                max_results: 8192,
                ..AffectedOptions::default()
            },
        )
        .unwrap();
    assert!(
        result.affected.iter().any(|node| node
            .path
            .as_deref()
            .is_some_and(|path| path.contains("cli/src/impact.rs"))),
        "expected the CLI consumer in {:?}",
        result
            .affected
            .iter()
            .filter_map(|node| node.path.clone())
            .collect::<Vec<_>>()
    );
}
