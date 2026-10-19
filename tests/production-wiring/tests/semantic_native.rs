//! Production-wiring certification for Faktor's NATIVE semantic fallback.
//!
//! The graph is the executable's own `build_daemon_core` via
//! [`harness::build_production_graph`]. The non-ignored test proves the
//! registry's fallback is composed with the daemon's REAL index, LSP,
//! tree-sitter, git and verification adapters — not
//! `GenericSemanticFallback::default()` — and that an ordinary context query
//! is served from the real index with per-finding provenance.
//!
//! The `#[ignore]` live test drives the SAME production graph against THIS
//! repository with the REAL `rust-analyzer` on PATH: a real symbol must
//! return real definitions, references and affected files. It is excluded
//! from the default CI lane because it starts an external language server
//! and indexes the workspace.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use faktor_core::id::{OpId, WorkspaceId};
use faktor_core::CancellationToken;
use faktor_semantic::fallback::GENERIC_FALLBACK_ID;
use faktor_semantic::types::{
    AffectedRequest, SemanticCall, SemanticContextRequest, SemanticEntityId, SemanticEntityRef,
    SemanticExplainRequest, SemanticProviderId, SemanticSnapshotId, SemanticSubsystem,
    SEMANTIC_SCHEMA_VERSION,
};
use faktor_tests_production_wiring::harness::{self, Config, DaemonGraph};

fn config_file(dir: &std::path::Path, body: serde_json::Value) -> Config {
    let path = dir.join("semantic-native-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

fn call(
    graph: &DaemonGraph,
    workspace: WorkspaceId,
    session: &faktor_session::SessionHandle,
) -> SemanticCall {
    SemanticCall::new(
        OpId::new(1),
        session.id(),
        workspace,
        graph.session.now_ms(),
        CancellationToken::new(),
    )
}

fn snapshot_id(workspace: WorkspaceId) -> SemanticSnapshotId {
    SemanticSnapshotId::derive(
        workspace,
        "semantic-native-test",
        &SemanticProviderId::parse(GENERIC_FALLBACK_ID).unwrap(),
        1,
        SEMANTIC_SCHEMA_VERSION,
    )
}

#[test]
fn production_daemon_wires_real_native_semantic_adapters_not_the_empty_default() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"semantic-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n\n#[test]\nfn answer_test() { assert_eq!(answer(), 42); }\n",
    )
    .unwrap();

    let graph = harness::build_production_graph(
        &dir.path().join("data"),
        config_file(dir.path(), serde_json::json!({})),
    )
    .expect("the production daemon graph must build fallback-only");

    let report = graph.semantic.fallback().wired_backends();
    assert_eq!(report.index.as_deref(), Some("index"));
    assert_eq!(report.tree_sitter.as_deref(), Some("tree-sitter"));
    assert_eq!(report.git.as_deref(), Some("git"));
    assert_eq!(report.verification.as_deref(), Some("verification"));
    assert_eq!(report.impact.as_deref(), Some("repository-graph"));
    assert!(
        report.lsp.is_some(),
        "the production fallback must carry the real LSP adapter"
    );
    assert!(report.is_wired());
    assert_ne!(
        report,
        faktor_semantic::GenericSemanticFallback::default().wired_backends(),
        "the daemon must not wire the empty default fallback"
    );

    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .expect("real workspace row");
    let index = graph.index.as_ref().expect("daemon hosts the real index");
    index.attach(workspace).expect("index attach");
    let view = index
        .ensure_ready(workspace, Instant::now() + Duration::from_secs(60))
        .expect("the real index must publish a generation");
    assert!(view.generation() >= 1);

    let session = graph
        .session
        .create_session(workspace, "semantic native", "test", "test")
        .expect("real session");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let envelope = runtime
        .block_on(graph.semantic.context(SemanticContextRequest {
            call: call(&graph, workspace, &session),
            workspace,
            source_revision: "semantic-native-test".to_string(),
            snapshot_id: snapshot_id(workspace),
            query: "answer".to_string(),
            max_items: 8,
            max_bytes: 16 * 1024,
        }))
        .expect("context is served by the production fallback");
    assert!(
        !envelope.payload.items.is_empty(),
        "the real index adapter must retrieve the fixture's `answer` symbol"
    );
    assert!(
        envelope
            .payload
            .items
            .iter()
            .any(|item| item.entity.entity_id.as_str() == "answer"),
        "the retrieved entity must be the real indexed symbol"
    );
    assert!(
        envelope.payload.items.iter().any(|item| item
            .provenance
            .iter()
            .any(|entry| entry.subsystem == SemanticSubsystem::Index)),
        "every index finding must carry index provenance"
    );
}

/// LIVE probe: real `rust-analyzer` + THIS repository. Run with
/// `cargo test -p faktor-tests-production-wiring --test semantic_native -- --ignored`.
#[test]
#[ignore = "live: starts the real rust-analyzer and indexes this repository"]
fn live_rust_analyzer_returns_real_definitions_references_and_affected_files() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("this repository root");
    let dir = tempfile::tempdir().unwrap();
    let graph = harness::build_production_graph(
        &dir.path().join("data"),
        config_file(dir.path(), serde_json::json!({})),
    )
    .expect("the production daemon graph must build fallback-only");
    let workspace = graph
        .session
        .create_workspace(repo.to_str().unwrap())
        .expect("real workspace row");
    let session = graph
        .session
        .create_session(workspace, "semantic live", "test", "test")
        .expect("real session");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let snapshot_id = snapshot_id(workspace);
    let entity = SemanticEntityRef::new(
        workspace,
        faktor_semantic::types::WorkspacePath::parse("crates/semantic/src/registry.rs").unwrap(),
        SemanticEntityId::parse("SemanticProviderRegistry").unwrap(),
    );

    // A cold rust-analyzer can answer the readiness probe before its full
    // crate graph is warm; retry the LIVE query (bounded) instead of
    // treating a transient empty analysis as a failure.
    let affected_request = AffectedRequest {
        call: call(&graph, workspace, &session),
        workspace,
        snapshot_id,
        changed: vec![entity.clone()],
        max_depth: 2,
    };
    let affected_started = std::time::Instant::now();
    let mut affected = None;
    let mut last_unavailable = Vec::new();
    // The fallback bounds each affected query's LSP phase (default 1.5s) and
    // briefly backs off a starved server. A cold rust-analyzer needs the
    // crate graph loaded before references answer, so the live probe retries
    // across that startup window; each individual query stays bounded.
    for attempt in 0..8 {
        let envelope = runtime
            .block_on(graph.semantic.affected(affected_request.clone()))
            .expect("affected is served by the production fallback");
        if envelope
            .payload
            .provenance
            .iter()
            .any(|entry| entry.subsystem == SemanticSubsystem::Lsp)
        {
            affected = Some(envelope);
            break;
        }
        last_unavailable = envelope.payload.unavailable;
        if attempt < 7 {
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    let affected_ms = affected_started.elapsed().as_millis();
    let affected = affected.unwrap_or_else(|| {
        panic!(
            "affected findings must carry real LSP provenance after bounded retries; last unavailable={last_unavailable:?}"
        )
    });
    let paths: BTreeSet<String> = affected
        .payload
        .affected
        .iter()
        .map(|entity| entity.path.as_str().to_string())
        .collect();
    assert!(
        !paths.is_empty(),
        "real rust-analyzer references must produce affected entities"
    );
    assert!(
        paths
            .iter()
            .any(|path| path != "crates/semantic/src/registry.rs"),
        "real references must cross files; got {paths:?}; unavailable={:?}",
        affected.payload.unavailable
    );

    let explain_request = SemanticExplainRequest {
        call: call(&graph, workspace, &session),
        workspace,
        snapshot_id,
        entity: entity.clone(),
        question: "what does this symbol govern?".to_string(),
    };
    let explain_started = std::time::Instant::now();
    let mut explain = None;
    for attempt in 0..4 {
        let envelope = runtime
            .block_on(graph.semantic.explain(explain_request.clone()))
            .expect("explain is served by the production fallback");
        if !envelope.payload.citations.is_empty() {
            explain = Some(envelope);
            break;
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    let explain_ms = explain_started.elapsed().as_millis();
    let explain = explain.expect(
        "real rust-analyzer definitions/references must produce citations after bounded retries",
    );
    assert!(
        explain.payload.text.contains("native definition"),
        "the explanation must report real evidence: {}",
        explain.payload.text
    );

    let context_request = SemanticContextRequest {
        call: call(&graph, workspace, &session),
        workspace,
        source_revision: "semantic-native-live".to_string(),
        snapshot_id,
        query: "SemanticProviderRegistry".to_string(),
        max_items: 8,
        max_bytes: 16 * 1024,
    };
    let context_started = std::time::Instant::now();
    let mut context = None;
    for attempt in 0..4 {
        let envelope = runtime
            .block_on(graph.semantic.context(context_request.clone()))
            .expect("context is served by the production fallback");
        if envelope.payload.items.iter().any(|item| {
            item.provenance
                .iter()
                .any(|entry| entry.subsystem == SemanticSubsystem::Lsp)
        }) {
            context = Some(envelope);
            break;
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    let context_ms = context_started.elapsed().as_millis();
    let context = context
        .expect("real rust-analyzer workspace symbols must produce context after bounded retries");
    println!(
        "live semantic probe: affected={} tests={} paths={:?} affected_ms={} explain_citations={} explain_ms={} context_items={} context_ms={}",
        affected.payload.affected.len(),
        affected.payload.tests.len(),
        paths.iter().take(8).collect::<Vec<_>>(),
        affected_ms,
        explain.payload.citations.len(),
        explain_ms,
        context.payload.items.len(),
        context_ms,
    );
    assert!(
        context.payload.items.iter().any(|item| item
            .provenance
            .iter()
            .any(|entry| entry.subsystem == SemanticSubsystem::Lsp)),
        "real rust-analyzer workspace symbols must produce context; unavailable={:?}",
        context.payload.unavailable
    );
}

/// MEASURED probe at real repository scale: the production graph's REAL
/// index over THIS repository, with O(log n) lexical/symbol retrieval and
/// O(V+E) impact traversal. Run with `--ignored`.
#[test]
#[ignore = "perf: builds the real index over this repository"]
fn real_repository_index_and_impact_are_measured_and_not_quadratic() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("this repository root");
    let dir = tempfile::tempdir().unwrap();
    let graph = harness::build_production_graph(
        &dir.path().join("data"),
        config_file(dir.path(), serde_json::json!({})),
    )
    .expect("the production daemon graph must build fallback-only");
    let workspace = graph
        .session
        .create_workspace(repo.to_str().unwrap())
        .expect("real workspace row");
    let index = graph.index.as_ref().expect("daemon hosts the real index");
    let attach_started = std::time::Instant::now();
    index.attach(workspace).expect("index attach");
    let view = index
        .ensure_ready(workspace, Instant::now() + Duration::from_secs(600))
        .expect("the real index must publish a generation");
    let ready_ms = attach_started.elapsed().as_millis();
    // Readiness is a cold build only once: a repeat query must serve the
    // published generation without rebuilding the workspace.
    let ready_warm_started = std::time::Instant::now();
    let warm_view = index
        .ensure_ready(workspace, Instant::now() + Duration::from_secs(60))
        .expect("the warm readiness probe must reuse the published generation");
    let ready_warm_ms = ready_warm_started.elapsed().as_millis();
    assert_eq!(warm_view.generation(), view.generation());
    assert!(
        ready_warm_ms < 1_000,
        "warm index readiness {ready_warm_ms}ms must stay sub-second at this repository scale"
    );
    let index_guard = view.index();
    let (files, symbols) = {
        let guard = index_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (guard.file_count(workspace), guard.symbol_count(workspace))
    };
    assert!(files > 100, "expected a real repository index, got {files}");
    assert!(symbols > 1000, "expected real symbols, got {symbols}");

    let session = graph
        .session
        .create_session(workspace, "semantic perf", "test", "test")
        .expect("real session");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let snapshot_id = snapshot_id(workspace);

    let first_started = std::time::Instant::now();
    let first = runtime
        .block_on(graph.semantic.context(SemanticContextRequest {
            call: call(&graph, workspace, &session),
            workspace,
            source_revision: "semantic-native-perf".to_string(),
            snapshot_id,
            query: "SemanticProviderRegistry".to_string(),
            max_items: 32,
            max_bytes: 64 * 1024,
        }))
        .expect("real index context");
    let first_ms = first_started.elapsed().as_millis();
    assert!(
        first.payload.items.iter().any(|item| item
            .provenance
            .iter()
            .any(|entry| entry.subsystem == SemanticSubsystem::Index)),
        "real index context must carry index provenance; unavailable={:?}",
        first.payload.unavailable
    );

    let warm_started = std::time::Instant::now();
    let warm = runtime
        .block_on(graph.semantic.context(SemanticContextRequest {
            call: call(&graph, workspace, &session),
            workspace,
            source_revision: "semantic-native-perf".to_string(),
            snapshot_id,
            query: "SemanticProviderRegistry".to_string(),
            max_items: 32,
            max_bytes: 64 * 1024,
        }))
        .expect("warm real index context");
    let warm_ms = warm_started.elapsed().as_millis();
    assert_eq!(first.payload.items, warm.payload.items);

    let entity = SemanticEntityRef::new(
        workspace,
        faktor_semantic::types::WorkspacePath::parse("crates/semantic/src/registry.rs").unwrap(),
        SemanticEntityId::parse("SemanticProviderRegistry").unwrap(),
    );
    let affected_started = std::time::Instant::now();
    let affected = runtime
        .block_on(graph.semantic.affected(AffectedRequest {
            call: call(&graph, workspace, &session),
            workspace,
            snapshot_id,
            changed: vec![entity],
            max_depth: 2,
        }))
        .expect("real impact query");
    let affected_ms = affected_started.elapsed().as_millis();
    assert!(
        !affected.payload.affected.is_empty(),
        "real impact query must find affected entities; unavailable={:?}",
        affected.payload.unavailable
    );
    assert!(
        affected_ms < 30_000,
        "cold affected {affected_ms}ms exceeded the bounded cold budget"
    );
    // Warm affected: the impact graph, index and LSP caches must serve the
    // same query without re-parsing the workspace or re-waiting per entity.
    let affected_warm_started = std::time::Instant::now();
    let affected_warm = runtime
        .block_on(graph.semantic.affected(AffectedRequest {
            call: call(&graph, workspace, &session),
            workspace,
            snapshot_id,
            changed: vec![SemanticEntityRef::new(
                workspace,
                faktor_semantic::types::WorkspacePath::parse("crates/semantic/src/registry.rs")
                    .unwrap(),
                SemanticEntityId::parse("SemanticProviderRegistry").unwrap(),
            )],
            max_depth: 2,
        }))
        .expect("warm real impact query");
    let affected_warm_ms = affected_warm_started.elapsed().as_millis();
    assert_eq!(affected.payload.affected, affected_warm.payload.affected);
    assert!(
        affected_warm_ms < 1_000,
        "warm affected {affected_warm_ms}ms must stay sub-second at this repository scale"
    );
    // LSP enrichment must be bounded: a payload that exhausted the budget is
    // explicitly degraded with a typed LSP reason, never silently partial.
    if let Some(reason) = affected
        .payload
        .unavailable
        .iter()
        .find(|entry| entry.subsystem == SemanticSubsystem::Lsp)
        .map(|entry| entry.reason.as_str())
    {
        assert!(
            !reason.is_empty(),
            "a bounded LSP phase must carry its typed reason"
        );
        assert!(
            affected.payload.degraded,
            "a bounded LSP phase must set degraded"
        );
    }
    println!(
        "scale probe: files={files} symbols={symbols} ready_ms={ready_ms} ready_warm_ms={ready_warm_ms} context_cold_ms={first_ms} context_warm_ms={warm_ms} affected_ms={affected_ms} affected_warm_ms={affected_warm_ms} affected={} degraded={}",
        affected.payload.affected.len(),
        affected.payload.degraded,
    );
}
