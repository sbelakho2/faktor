//! The graph publishes the exact content inputs it was derived from, so the
//! CI validator never has to guess which source/manifests can make it stale.

use std::fs;
use std::path::Path;

use faktor_impact::{GraphBuilder, Revision, TopologyOptions};

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn build(root: &Path) -> (faktor_impact::Graph, faktor_impact::BuildReport) {
    let mut builder =
        GraphBuilder::new_with_topology(root, Revision::default(), TopologyOptions::parser_only())
            .unwrap();
    let graph = builder.build().unwrap();
    (graph, builder.report().clone())
}

fn input_paths(graph: &faktor_impact::Graph) -> Vec<&str> {
    graph
        .material_inputs
        .iter()
        .map(|input| input.path.as_str())
        .collect()
}

#[test]
fn root_package_and_lib_source_are_published_as_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"root","private":true}"#);
    write(root, "lib/a.ts", "export const a = 1;\n");
    write(root, "docs/readme.md", "docs\n");
    let (graph, report) = build(root);
    assert!(
        input_paths(&graph).contains(&"package.json"),
        "root manifest must be a published input: {:?}",
        input_paths(&graph)
    );
    assert!(
        input_paths(&graph).contains(&"lib/a.ts"),
        "root lib source must be a published input: {:?}",
        input_paths(&graph)
    );
    assert!(
        !input_paths(&graph).contains(&"docs/readme.md"),
        "docs are not graph content"
    );
    assert_eq!(
        report.material_inputs, graph.material_inputs,
        "the build report exposes the same inventory the graph publishes"
    );
    let projected = graph.selector_projection().unwrap();
    assert_eq!(
        projected.material_inputs, graph.material_inputs,
        "the selector projection must carry the inventory"
    );
}

#[test]
fn root_custom_rust_path_outside_src_is_published() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
    );
    write(root, "custom/engine.rs", "pub fn engine() -> u32 { 1 }\n");
    let (graph, _) = build(root);
    assert!(
        input_paths(&graph).contains(&"custom/engine.rs"),
        "non-src Rust source under the root unit must be a published input: {:?}",
        input_paths(&graph)
    );
    assert!(
        input_paths(&graph).contains(&"Cargo.toml"),
        "root Rust manifest must be a published input"
    );
}

#[test]
fn inventory_is_canonical_ordered_and_matches_file_digests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "lib/a.ts", "export const a = 1;\n");
    write(root, "lib/b.ts", "export const b = 1;\n");
    let (graph, _) = build(root);
    assert!(graph.material_inputs.len() >= 2);
    graph.validate().unwrap();

    let mut unsorted = graph.clone();
    unsorted.material_inputs.reverse();
    assert!(
        matches!(
            unsorted.validate(),
            Err(faktor_impact::GraphError::Unsorted(_))
        ),
        "an unsorted inventory must be refused typed: {:?}",
        unsorted.validate()
    );

    let mut digest = graph.clone();
    digest.material_inputs[0].digest = "not-a-digest".into();
    match digest.validate() {
        Err(faktor_impact::GraphError::Malformed(message)) => assert!(
            message.contains("invalid digest"),
            "an invalid inventory digest must name the digest: {message}"
        ),
        other => panic!("expected Malformed invalid digest, got {other:?}"),
    }

    let mut missing = graph.clone();
    missing
        .material_inputs
        .retain(|input| input.path != "lib/a.ts");
    match missing.validate() {
        Err(faktor_impact::GraphError::Malformed(message)) => assert!(
            message.contains("does not publish graph file"),
            "an omitted graph file must name the missing input: {message}"
        ),
        other => panic!("expected Malformed missing input, got {other:?}"),
    }
}

#[test]
fn manifestless_generic_repository_sources_are_published() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "lib/engine.ts", "export const engine = 1;\n");
    let (graph, _) = build(root);
    assert!(
        input_paths(&graph).contains(&"lib/engine.ts"),
        "generic repository source outside src/ must be a published input: {:?}",
        input_paths(&graph)
    );
}
