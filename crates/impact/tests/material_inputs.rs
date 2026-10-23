//! The graph publishes the exact content inputs it was derived from, so the
//! CI validator never has to guess which source/manifests can make it stale.

use std::fs;
use std::path::Path;

use faktor_impact::{
    GraphBuilder, ImpactSession, MaterialGeneration, RefreshMode, RefreshPolicy, Revision,
    TopologyOptions,
};

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

fn material_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.toml", "[workspace]\nmembers = [\"app\"]\n");
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(root, "app/src/lib.rs", "pub fn engine() -> u32 { 1 }\n");
    write(
        root,
        "app/src/extra.rs",
        "pub fn extra() -> u32 { engine() }\n",
    );
    dir
}

fn session_inputs(session: &ImpactSession) -> Vec<(String, String)> {
    session
        .graph()
        .graph()
        .material_inputs
        .iter()
        .map(|input| (input.path.clone(), input.digest.clone()))
        .collect()
}

fn source_hashes(session: &ImpactSession) -> Vec<(String, String)> {
    session
        .graph()
        .graph()
        .files
        .iter()
        .map(|file| (file.path.clone(), file.hash.clone()))
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

#[test]
fn manifest_only_change_changes_the_material_revision_and_rebuilds() {
    let dir = material_workspace();
    let root = dir.path();
    let mut session = ImpactSession::build_material(root).unwrap();
    let before = session.graph().graph().revision.clone();
    let before_sources = source_hashes(&session);
    assert!(
        before
            .tree
            .as_deref()
            .is_some_and(|tree| tree.contains("#generation:1")),
        "the baseline is a generation-1 material revision: {before:?}"
    );

    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.2.0\"\n\n[features]\ndefault = []\n",
    );
    let scan = session.scan_material_changes().unwrap();
    assert!(
        scan.seeds.iter().any(|path| path == "app/Cargo.toml"),
        "the manifest stamp must be dirty: {scan:?}"
    );
    let generation = MaterialGeneration::new();
    generation.observe(scan.seeds);
    let report = session
        .refresh_from_generation(&generation.dirty_since(0), &RefreshPolicy::default())
        .unwrap();
    assert_eq!(report.mode, RefreshMode::Rebuilt, "{report:?}");
    assert!(
        session
            .graph()
            .graph()
            .revision
            .tree
            .as_deref()
            .is_some_and(|tree| tree.contains("#generation:2")),
        "the rebuild must advance the material generation"
    );

    let fresh = ImpactSession::build_material(root).unwrap();
    let fresh_revision = fresh.graph().graph().revision.clone();
    assert!(
        fresh_revision
            .tree
            .as_deref()
            .is_some_and(|tree| tree.contains("#generation:1")),
        "a restart starts at generation 1: {fresh_revision:?}"
    );
    assert_ne!(
        fresh_revision, before,
        "a manifest-only edit must change the generation-1 material digest"
    );
    assert_eq!(
        source_hashes(&fresh),
        before_sources,
        "the manifest-only edit must not change source content"
    );
    assert!(fresh
        .graph()
        .graph()
        .material_inputs
        .iter()
        .any(|input| input.path == "app/Cargo.toml"));
}

#[test]
fn lockfile_only_change_changes_the_material_revision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"root","version":"1.0.0","private":true}"#,
    );
    write(
        root,
        "package-lock.json",
        r#"{"name":"root","version":"1.0.0","lockfileVersion":3,"packages":{"":{"name":"root","version":"1.0.0"}}}"#,
    );
    write(root, "lib/a.ts", "export const a = 1;\n");
    let before = ImpactSession::build_material(root).unwrap();
    assert!(
        session_inputs(&before)
            .iter()
            .any(|(path, _)| path == "package-lock.json"),
        "the lockfile must be a material input"
    );

    write(
        root,
        "package-lock.json",
        r#"{"name":"root","version":"1.0.1","lockfileVersion":3,"packages":{"":{"name":"root","version":"1.0.1"}}}"#,
    );
    let after = ImpactSession::build_material(root).unwrap();
    assert_ne!(
        after.graph().graph().revision,
        before.graph().graph().revision,
        "a lockfile-only edit must change the generation-1 material digest"
    );
    assert_eq!(
        source_hashes(&after),
        source_hashes(&before),
        "no source changed"
    );
}

#[test]
fn restart_over_identical_inputs_preserves_material_identity() {
    let dir = material_workspace();
    let root = dir.path();
    let first = ImpactSession::build_material(root).unwrap();
    let second = ImpactSession::build_material(root).unwrap();
    assert_eq!(
        first.graph().graph().revision,
        second.graph().graph().revision,
        "a restart over identical content must reproduce the material revision"
    );
    assert_eq!(
        first.graph().graph().digest,
        second.graph().graph().digest,
        "the sealed graph digest must also be reproducible"
    );
    assert_eq!(session_inputs(&first), session_inputs(&second));
}

#[test]
fn deleted_and_renamed_inputs_change_the_material_revision() {
    let dir = material_workspace();
    let root = dir.path();
    let baseline = ImpactSession::build_material(root).unwrap();
    let baseline_revision = baseline.graph().graph().revision.clone();
    assert!(session_inputs(&baseline)
        .iter()
        .any(|(path, _)| path == "app/src/extra.rs"));

    fs::remove_file(root.join("app/src/extra.rs")).unwrap();
    let deleted = ImpactSession::build_material(root).unwrap();
    assert_ne!(
        deleted.graph().graph().revision,
        baseline_revision,
        "a deleted input must change the material revision"
    );
    assert!(
        !session_inputs(&deleted)
            .iter()
            .any(|(path, _)| path == "app/src/extra.rs"),
        "the deleted input must leave the inventory"
    );

    write(
        root,
        "app/src/extra.rs",
        "pub fn extra() -> u32 { engine() }\n",
    );
    let restored = ImpactSession::build_material(root).unwrap();
    assert!(session_inputs(&restored)
        .iter()
        .any(|(path, _)| path == "app/src/extra.rs"));

    fs::rename(root.join("app/src/extra.rs"), root.join("app/src/moved.rs")).unwrap();
    let renamed = ImpactSession::build_material(root).unwrap();
    assert_ne!(
        renamed.graph().graph().revision,
        restored.graph().graph().revision,
        "a renamed input must change the material revision"
    );
    let inputs = session_inputs(&renamed);
    assert!(
        inputs.iter().any(|(path, _)| path == "app/src/moved.rs"),
        "the renamed input must enter the inventory"
    );
    assert!(
        !inputs.iter().any(|(path, _)| path == "app/src/extra.rs"),
        "the old input path must leave the inventory"
    );
}
