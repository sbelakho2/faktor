use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use faktor_impact::{
    AffectedOptions, BuildOptions, ChangedTarget, EdgeKind, GraphBuilder, GraphError, ImpactGraph,
    ImpactSession, NodeKind, Revision,
};

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn sample_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        r#"
[workspace]
members = ["alpha", "beta", "gamma"]
"#,
    );
    write(
        root,
        "alpha/Cargo.toml",
        r#"
[package]
name = "alpha"
version = "0.1.0"
"#,
    );
    write(
        root,
        "alpha/src/lib.rs",
        r#"
pub fn answer() -> u32 { 42 }

pub fn double(value: u32) -> u32 { answer() * 2 }

pub mod math;

#[cfg(test)]
mod tests {
    #[test]
    fn unit_answer_and_double() {
        assert_eq!(super::answer(), 42);
        assert_eq!(super::double(1), 84);
    }
}
"#,
    );
    write(
        root,
        "alpha/src/math.rs",
        r#"
pub fn helper(value: u32) -> u32 { super::answer() + value }
"#,
    );
    write(
        root,
        "beta/Cargo.toml",
        r#"
[package]
name = "beta"
version = "0.1.0"

[dependencies]
alpha = { path = "../alpha" }
"#,
    );
    write(
        root,
        "beta/src/lib.rs",
        r#"
use alpha::{answer, double};

pub fn use_answer() -> u32 { answer() + double(1) }
"#,
    );
    write(
        root,
        "beta/tests/integration.rs",
        r#"
use beta::use_answer;

#[test]
fn integration_answer_is_84() {
    assert_eq!(use_answer(), 84);
}
"#,
    );
    write(
        root,
        "gamma/Cargo.toml",
        r#"
[package]
name = "gamma"
version = "0.1.0"

[dependencies]
beta = { path = "../beta" }
"#,
    );
    write(
        root,
        "gamma/src/lib.rs",
        r#"
use beta::use_answer;

pub fn top() -> u32 { use_answer() + 1 }
"#,
    );
    write(
        root,
        "gamma/tests/top.rs",
        r#"
#[test]
fn top_works() {
    assert_eq!(gamma::top(), 85);
}
"#,
    );
    dir
}

fn build(root: &Path) -> ImpactGraph {
    let mut builder = GraphBuilder::new(
        root,
        Revision {
            commit: Some("test".into()),
            tree: Some("tree".into()),
            dirty: false,
        },
    )
    .unwrap();
    let graph = builder.build().unwrap();
    ImpactGraph::new(graph).unwrap()
}

#[test]
fn graph_has_files_symbols_tests_crates_and_relationship_edges() {
    let dir = sample_workspace();
    let graph = build(dir.path());
    let document = graph.graph();
    assert!(document
        .nodes
        .iter()
        .any(|node| node.kind == NodeKind::Crate && node.name == "alpha"));
    assert!(document.nodes.iter().any(
        |node| node.kind == NodeKind::File && node.path.as_deref() == Some("alpha/src/lib.rs")
    ));
    assert!(document
        .nodes
        .iter()
        .any(|node| node.kind == NodeKind::Symbol && node.name == "answer"));
    assert!(document
        .nodes
        .iter()
        .any(|node| node.kind == NodeKind::Test && node.name == "unit_answer_and_double"));
    assert!(document
        .edges
        .iter()
        .any(|edge| edge.kind == EdgeKind::Calls
            && edge.from.contains("double")
            && edge.to.contains("answer")));
    assert!(document
        .edges
        .iter()
        .any(|edge| edge.kind == EdgeKind::Covers));
    assert!(document
        .edges
        .iter()
        .any(|edge| edge.from == "crate:beta" && edge.to == "crate:alpha"));
    assert!(!document.reverse_edges.is_empty());
    assert!(document.validate().is_ok());
}

#[test]
fn changed_symbol_discovers_hand_verified_callers_and_tests() {
    let dir = sample_workspace();
    let graph = build(dir.path());
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
    for expected in ["double", "helper", "use_answer", "top"] {
        assert!(
            names.contains(&expected),
            "expected impacted caller {expected} in {names:?}"
        );
    }
    let tests: Vec<&str> = result.tests.iter().map(|node| node.name.as_str()).collect();
    assert!(
        tests.contains(&"unit_answer_and_double"),
        "expected unit test in {tests:?}"
    );
    assert!(
        tests.contains(&"integration_answer_is_84"),
        "expected integration test in {tests:?}"
    );
    assert!(result.tests.iter().all(|test| !test.reasons.is_empty()));
    assert!(result.stats.visited_edges > 0);
    assert!(!result.degraded);
}

#[test]
fn changed_file_is_conservative_across_dependent_crates() {
    let dir = sample_workspace();
    let graph = build(dir.path());
    let result = graph
        .affected_paths(&["alpha/src/lib.rs".into()], &AffectedOptions::default())
        .unwrap();
    assert!(result.crates.contains(&"alpha".to_string()));
    assert!(result.crates.contains(&"beta".to_string()));
    assert!(result.crates.contains(&"gamma".to_string()));
    assert!(result.tests.len() >= 3);
    assert!(result.files.contains(&"alpha/src/lib.rs".to_string()));
}

#[test]
fn incremental_refresh_reparses_only_changed_and_dependents() {
    let dir = sample_workspace();
    let mut session = ImpactSession::build(
        dir.path(),
        Revision {
            commit: Some("one".into()),
            tree: None,
            dirty: false,
        },
    )
    .unwrap();
    write(
        dir.path(),
        "alpha/src/extra.rs",
        "pub fn extra() -> u32 { crate::answer() }\n",
    );
    let stats = session
        .refresh(
            &["alpha/src/extra.rs".into()],
            Revision {
                commit: Some("two".into()),
                tree: None,
                dirty: false,
            },
        )
        .unwrap();
    assert_eq!(stats.files_changed, 1);
    assert!(stats.files_reparsed >= 1);
    assert!(stats.files_reparsed < stats.records);
    let result = session
        .affected(
            &[ChangedTarget::symbol("alpha/src/lib.rs", "answer")],
            &AffectedOptions {
                crate_tests: false,
                ..AffectedOptions::default()
            },
        )
        .unwrap();
    assert!(result.affected.iter().any(|node| node.name == "extra"));
    fs::remove_file(dir.path().join("alpha/src/extra.rs")).unwrap();
    session
        .refresh(
            &["alpha/src/extra.rs".into()],
            Revision {
                commit: Some("three".into()),
                tree: None,
                dirty: false,
            },
        )
        .unwrap();
    let result = session
        .affected(
            &[ChangedTarget::symbol("alpha/src/lib.rs", "answer")],
            &AffectedOptions {
                crate_tests: false,
                ..AffectedOptions::default()
            },
        )
        .unwrap();
    assert!(!result.affected.iter().any(|node| node.name == "extra"));
}

#[test]
fn graph_serialization_is_deterministic_and_digest_checked() {
    let dir = sample_workspace();
    let graph = build(dir.path());
    let first = graph.graph().to_json_pretty().unwrap();
    let second = build(dir.path()).graph().to_json_pretty().unwrap();
    assert_eq!(first, second);
    let parsed = faktor_impact::Graph::from_json(&first).unwrap();
    assert_eq!(parsed.digest, graph.graph().digest);
    let mut tampered = first.replace("\"answer\"", "\"answer2\"");
    if tampered == first {
        tampered = first.replace("alpha/src/lib.rs", "alpha/src/lib2.rs");
    }
    let error = faktor_impact::Graph::from_json(&tampered).unwrap_err();
    assert!(matches!(error, GraphError::DigestMismatch { .. }));
}

#[test]
fn hostile_paths_oversized_files_and_cycles_are_bounded() {
    let dir = sample_workspace();
    write(
        dir.path(),
        "alpha/src/huge.rs",
        &"pub fn huge() {}\n".repeat(100_000),
    );
    write(
        dir.path(),
        "beta/src/cycle.rs",
        "use crate::use_answer; pub fn cycle() -> u32 { use_answer() }\n",
    );
    let mut builder = GraphBuilder::new(dir.path(), Revision::default()).unwrap();
    builder = builder.with_options(BuildOptions {
        max_files: 10_000,
        max_file_bytes: 1024,
    });
    let graph = builder.build().unwrap();
    let graph = ImpactGraph::new(graph).unwrap();
    assert!(graph.graph().stats.truncated);
    assert!(!graph
        .graph()
        .files
        .iter()
        .any(|file| file.path.ends_with("huge.rs")));
    let error = graph
        .affected(
            &[ChangedTarget::file("alpha/../beta/src/lib.rs")],
            &AffectedOptions::default(),
        )
        .unwrap_err();
    assert!(matches!(error, GraphError::Malformed(_)));
}

#[test]
fn this_repository_symbol_change_finds_real_callers_and_tests() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let revision = Revision::read_head(&root);
    assert!(revision.is_bound(), "this test requires a git checkout");
    let mut builder = GraphBuilder::new(&root, revision).unwrap();
    let graph = builder.build().unwrap();
    let graph = ImpactGraph::new(graph).unwrap();
    assert!(graph.graph().files.len() > 100);
    let result = graph
        .affected(
            &[ChangedTarget::symbol(
                "crates/semantic/src/types.rs",
                "parse",
            )],
            &AffectedOptions {
                crate_tests: false,
                max_results: 8192,
                ..AffectedOptions::default()
            },
        )
        .unwrap();
    let test_names: Vec<&str> = result.tests.iter().map(|node| node.name.as_str()).collect();
    assert!(
        test_names.contains(&"hostile_entity_ids_rejected_with_exact_typed_reason_at_the_boundary"),
        "expected the hand-verified semantic test in {test_names:?}"
    );
    assert!(
        result.affected.iter().any(|node| node.name == "parse"),
        "impacted callers should include parse references"
    );
    assert!(result.stats.visited_edges > 0);
    assert!(result.affected.iter().any(|node| !node.reasons.is_empty()));
}

#[test]
fn ten_thousand_files_build_and_query_are_linear() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let crates = 100usize;
    let files_per_crate = 100usize;
    let mut members = Vec::new();
    for crate_index in 0..crates {
        let name = format!("k{crate_index:03}");
        members.push(format!("\"{name}\""));
        let dependency = if crate_index > 0 {
            format!(
                "[dependencies]\nk{:03} = {{ path = \"../k{:03}\" }}\n",
                crate_index - 1,
                crate_index - 1
            )
        } else {
            String::new()
        };
        write(
            root,
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{dependency}"),
        );
        // cargo metadata requires a target; the fixture measures scanning,
        // so one empty library per crate is enough.
        write(root, &format!("{name}/src/lib.rs"), "");
        for file_index in 0..files_per_crate {
            let next = (file_index + 1) % files_per_crate;
            let text = if crate_index == 0 {
                format!(
                    "pub fn f{file_index}() -> u32 {{ {file_index} }}\npub fn g{file_index}() -> u32 {{ f{next}() }}\n"
                )
            } else {
                format!(
                    "use k{:03}::g{};\npub fn f{file_index}() -> u32 {{ g{}() }}\npub fn g{file_index}() -> u32 {{ f{next}() }}\n",
                    crate_index - 1,
                    file_index,
                    file_index
                )
            };
            write(root, &format!("{name}/src/f{file_index}.rs"), &text);
        }
    }
    write(
        root,
        "Cargo.toml",
        &format!("[workspace]\nmembers = [{}]\n", members.join(", ")),
    );
    let started = Instant::now();
    let mut session = ImpactSession::build(root, Revision::default()).unwrap();
    let build_ms = started.elapsed().as_millis();
    let graph = session.graph();
    assert!(graph.graph().files.len() >= 10_000);
    assert!(graph.graph().stats.symbols >= 20_000);
    let query_started = Instant::now();
    let result = graph
        .affected(
            &[ChangedTarget::symbol("k000/src/f0.rs", "f0")],
            &AffectedOptions {
                crate_tests: false,
                max_results: 1000,
                ..AffectedOptions::default()
            },
        )
        .unwrap();
    let query_ms = query_started.elapsed().as_millis();
    let edges = graph.graph().edges.len();
    // Whole-workspace seed: every incident edge is traversed once, proving
    // the query is a linear pass (not an O(n^2) pairwise scan).
    let all_files: Vec<String> = graph
        .graph()
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let bulk_started = Instant::now();
    let bulk = graph
        .affected_paths(&all_files, &AffectedOptions::default())
        .unwrap();
    let bulk_ms = bulk_started.elapsed().as_millis();
    println!(
        "impact scale: files={} symbols={} edges={} build_ms={build_ms} query_ms={query_ms} visited_edges={} discovered={} bulk_ms={bulk_ms} bulk_visited_edges={}",
        graph.graph().files.len(),
        graph.graph().stats.symbols,
        edges,
        result.stats.visited_edges,
        result.stats.discovered,
        bulk.stats.visited_edges
    );
    assert!(build_ms < 120_000, "build took {build_ms}ms");
    assert!(query_ms < 2_000, "query took {query_ms}ms");
    assert!(bulk_ms < 5_000, "bulk query took {bulk_ms}ms");
    assert!(
        result.stats.visited_edges <= edges,
        "query touched more edges than exist"
    );
    assert!(
        bulk.stats.visited_edges > edges / 2,
        "bulk query should traverse most of the graph"
    );
    assert!(
        bulk.stats.visited_edges <= edges,
        "each reverse edge traversed at most once"
    );
    assert!(result.affected.iter().any(|node| node.name == "g99"));

    // Incremental refresh at 10k files must be O(changed): one edited file
    // plus its reverse dependency, never a rescan or reparse of the
    // workspace, and never a full-graph invalidation walk.
    let changed_path = "k000/src/f0.rs".to_string();
    let original = fs::read_to_string(root.join(&changed_path)).unwrap();
    fs::write(root.join(&changed_path), format!("// edited\n{original}")).unwrap();
    let refresh_started = Instant::now();
    let refresh_stats = session
        .refresh_material(std::slice::from_ref(&changed_path))
        .unwrap();
    let refresh_ms = refresh_started.elapsed().as_millis();
    assert_eq!(refresh_stats.files_changed, 1);
    // The edited symbol is re-exported through the 100-crate chain, so the
    // reverse closure is at most one file per crate — never the 10k-file
    // workspace.
    // The fixture chains every crate to the previous one, so the reverse
    // closure reaches one file per crate (100), not the 10k-file workspace.
    assert!(
        refresh_stats.files_reparsed <= 128,
        "refresh reparsed {} files at 10k files: {refresh_stats:?}",
        refresh_stats.files_reparsed
    );
    assert!(
        refresh_stats.files_reparsed < session.graph().graph().files.len() / 10,
        "refresh must stay far below a full workspace parse: {refresh_stats:?}"
    );
    let rewritten = fs::read_to_string(root.join(&changed_path)).unwrap();
    fs::write(root.join(&changed_path), &rewritten).unwrap();
    let noop_started = Instant::now();
    let noop = session.refresh_material(&[changed_path]).unwrap();
    let noop_ms = noop_started.elapsed().as_millis();
    assert_eq!(noop.files_changed, 0);
    assert_eq!(noop.files_reparsed, 0);
    println!(
        "impact incremental: refresh_ms={refresh_ms} files_reparsed={} noop_ms={noop_ms} files_unchanged={}",
        refresh_stats.files_reparsed, noop.files_unchanged
    );
    assert!(
        refresh_ms < 30_000,
        "incremental refresh took {refresh_ms}ms"
    );
    assert!(noop_ms < 2_000, "no-op refresh took {noop_ms}ms");
}
