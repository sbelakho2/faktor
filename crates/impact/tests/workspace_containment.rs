//! Adversarial workspace containment: declared members must never escape the
//! canonical workspace root, whether through `..`, an absolute path, a
//! symlinked member or a symlinked wildcard base.

use std::fs;
use std::path::{Path, PathBuf};

use faktor_impact::manifest::TopologyOptions;
use faktor_impact::{GraphBuilder, Revision};

const SENTINEL_CRATE: &str = "outside-sentinel-9c1f";
const SENTINEL_FN: &str = "outside_sentinel_9c1f";

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&repo).unwrap();
    write(
        &outside,
        "Cargo.toml",
        &format!("[package]\nname = \"{SENTINEL_CRATE}\"\nversion = \"0.1.0\"\n"),
    );
    write(
        &outside,
        "src/lib.rs",
        &format!("pub fn {SENTINEL_FN}() -> u32 {{ 1 }}\n"),
    );
    // A legitimate member proves a rejected hostile member does not abort
    // the whole workspace discovery.
    write(
        &repo,
        "good/Cargo.toml",
        "[package]\nname = \"good\"\nversion = \"0.1.0\"\n",
    );
    write(&repo, "good/src/lib.rs", "pub fn good() -> u32 { 1 }\n");
    (dir, repo, outside)
}

fn workspace_manifest(members: &[&str]) -> String {
    let list = members
        .iter()
        .map(|member| format!("\"{member}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[workspace]\nmembers = [{list}]\n")
}

fn assert_rejected(repo: &Path, outside: &Path, manifest: &str) -> Vec<String> {
    assert!(
        outside.join("src/lib.rs").is_file(),
        "the sentinel crate must exist for the test to be meaningful"
    );
    write(repo, "Cargo.toml", manifest);
    // Force the bounded manifest path so the refusal is deterministic (the
    // parser-only fallback is a supported production topology).
    let mut builder =
        GraphBuilder::new_with_topology(repo, Revision::default(), TopologyOptions::parser_only())
            .unwrap();
    let graph = builder
        .build()
        .expect("a hostile member must be refused or skipped, not abort discovery");
    let document = graph.to_json_compact().unwrap();
    assert!(
        !document.contains(SENTINEL_CRATE),
        "the outside crate name must never enter the graph: {document}"
    );
    assert!(
        !document.contains(SENTINEL_FN),
        "the outside sentinel function must never enter the graph"
    );
    assert!(
        graph
            .files
            .iter()
            .all(|file| !file.path.contains("outside") && !file.path.contains(SENTINEL_CRATE)),
        "no file outside the workspace may be indexed: {:?}",
        graph
            .files
            .iter()
            .map(|file| &file.path)
            .collect::<Vec<_>>()
    );
    assert!(
        graph.nodes.iter().any(|node| node.name == "good"),
        "the legitimate member must still be indexed"
    );
    assert!(
        !builder.workspace().skipped.is_empty(),
        "a rejected member must be reported as skipped"
    );
    builder.workspace().skipped.clone()
}

#[test]
fn parent_dir_member_and_wildcard_never_read_the_sibling() {
    let (_dir, repo, outside) = fixture();
    let skipped = assert_rejected(
        &repo,
        &outside,
        &workspace_manifest(&["good", "../outside", "../*"]),
    );
    assert!(
        skipped.iter().any(|entry| entry.contains("..")),
        "both parent traversal forms must be recorded: {skipped:?}"
    );
}

#[test]
fn absolute_member_is_refused() {
    let (_dir, repo, outside) = fixture();
    let absolute = outside.to_string_lossy().replace('\\', "\\\\");
    assert_rejected(&repo, &outside, &workspace_manifest(&["good", &absolute]));
}

#[cfg(unix)]
#[test]
fn symlinked_member_is_refused() {
    use std::os::unix::fs::symlink;
    let (_dir, repo, outside) = fixture();
    symlink(&outside, repo.join("linked")).unwrap();
    assert_rejected(&repo, &outside, &workspace_manifest(&["good", "linked"]));
}

#[cfg(unix)]
#[test]
fn member_path_through_a_symlinked_directory_is_refused() {
    use std::os::unix::fs::symlink;
    let (_dir, repo, outside) = fixture();
    fs::create_dir_all(repo.join("nest")).unwrap();
    symlink(&outside, repo.join("nest/linked")).unwrap();
    assert_rejected(
        &repo,
        &outside,
        &workspace_manifest(&["good", "nest/linked"]),
    );
}

#[cfg(unix)]
#[test]
fn symlinked_wildcard_base_is_refused_before_read_dir() {
    use std::os::unix::fs::symlink;
    let (_dir, repo, outside) = fixture();
    symlink(&outside, repo.join("crates")).unwrap();
    assert_rejected(&repo, &outside, &workspace_manifest(&["good", "crates/*"]));
}

#[test]
fn default_cargo_metadata_path_refuses_or_skips_the_parent_member() {
    let (_dir, repo, _outside) = fixture();
    write(
        &repo,
        "Cargo.toml",
        &workspace_manifest(&["good", "../outside"]),
    );
    let result =
        GraphBuilder::new(&repo, Revision::default()).and_then(|mut builder| builder.build());
    match result {
        Ok(graph) => {
            let document = graph.to_json_compact().unwrap();
            assert!(!document.contains(SENTINEL_CRATE));
            assert!(!document.contains(SENTINEL_FN));
            assert!(
                graph
                    .files
                    .iter()
                    .all(|file| !file.path.contains("outside")),
                "no outside source may be indexed: {:?}",
                graph
                    .files
                    .iter()
                    .map(|file| &file.path)
                    .collect::<Vec<_>>()
            );
            assert!(graph.nodes.iter().any(|node| node.name == "good"));
        }
        Err(error) => {
            // A typed refusal is also acceptable: the sentinel never produced
            // a graph document.
            assert!(!error.to_string().contains(SENTINEL_FN));
        }
    }
}
