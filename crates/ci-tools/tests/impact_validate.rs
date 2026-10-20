use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_faktor-ci-tools")
}

fn run(args: &[&str], stdin: Option<&str>) -> (i32, Value) {
    let mut command = Command::new(binary());
    command
        .arg("impact")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .spawn()
        .expect("spawn validator");
    if let Some(text) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    let verdict: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "validator emitted no JSON verdict ({error}): stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap_or(-1), verdict)
}

fn fixture() -> Value {
    json!({
        "schema": "faktor-impact-graph/v1",
        "revision": { "commit": null, "tree": null, "dirty": false },
        "workspace": 1,
        "files": [],
        "nodes": [
            { "id": "crate:a", "kind": "crate", "name": "a", "path": "a", "provenance": "manifest" },
            { "id": "test:t.rs#t@1", "kind": "test", "name": "t", "path": "t.rs", "crate_name": "a", "provenance": "coverage" }
        ],
        "edges": [{ "from": "test:t.rs#t@1", "to": "crate:a", "kind": "tests", "provenance": "coverage" }],
        "reverse_edges": [{ "from": "crate:a", "to": "test:t.rs#t@1", "kind": "tests", "provenance": "coverage" }],
        "stats": {},
        "digest": ""
    })
}

fn seal(document: &Value) -> Value {
    let (code, sealed) = run(&["seal", "--stdin"], Some(&document.to_string()));
    assert_eq!(code, 0, "seal refused: {sealed}");
    sealed
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn valid_graph_is_accepted_and_invalid_variants_are_typed_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let sealed = seal(&fixture());
    let path = root.join("graph.json");
    write(&path, &sealed.to_string());

    let (code, verdict) = run(&["validate", path.to_str().unwrap(), "--json"], None);
    assert_eq!(code, 0, "{verdict}");
    assert_eq!(verdict["ok"], true);
    assert_eq!(verdict["schema"], "faktor-impact-validation/v1");
    assert_eq!(verdict["nodes"], 2);
    assert_eq!(verdict["edges"], 1);
    assert_eq!(verdict["digest"], sealed["digest"]);

    let (code, verdict) = run(
        &["validate", "--stdin", "--json"],
        Some(&sealed.to_string()),
    );
    assert_eq!(code, 0, "{verdict}");
    assert_eq!(verdict["ok"], true);

    let mut digest = sealed.clone();
    digest["nodes"][0]["name"] = json!("tampered");
    let (code, verdict) = run(
        &["validate", "--stdin", "--json"],
        Some(&digest.to_string()),
    );
    assert_eq!(code, 3);
    assert_eq!(verdict["ok"], false);
    assert_eq!(verdict["code"], "digest_mismatch", "{verdict}");

    let mut dangling = sealed.clone();
    dangling["nodes"].as_array_mut().unwrap().pop();
    let (code, verdict) = run(
        &["validate", "--stdin", "--json"],
        Some(&dangling.to_string()),
    );
    assert_eq!(code, 3);
    assert_eq!(verdict["code"], "dangling_edge", "{verdict}");

    let mut reverse = sealed.clone();
    reverse["reverse_edges"] = json!([]);
    let (code, verdict) = run(
        &["validate", "--stdin", "--json"],
        Some(&reverse.to_string()),
    );
    assert_eq!(code, 3);
    assert_eq!(verdict["code"], "reverse_mismatch", "{verdict}");

    let mut ordering = sealed.clone();
    ordering["nodes"].as_array_mut().unwrap().reverse();
    let (code, verdict) = run(
        &["validate", "--stdin", "--json"],
        Some(&ordering.to_string()),
    );
    assert_eq!(code, 3);
    assert_eq!(verdict["code"], "unsorted", "{verdict}");
}

#[test]
fn revision_binding_accepts_non_input_commits_and_refuses_real_graph_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "validator@example.invalid"]);
    git(root, &["config", "user.name", "validator"]);
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"pkg\"]\n",
    );
    write(
        &root.join("pkg/Cargo.toml"),
        "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n",
    );
    write(
        &root.join("pkg/src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    );
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    let base = git(root, &["rev-parse", "HEAD"]);

    let document = seal(&json!({
        "schema": "faktor-impact-graph/v1",
        "revision": { "commit": base, "tree": null, "dirty": false },
        "workspace": 1,
        "files": [],
        "nodes": [{ "id": "crate:pkg", "kind": "crate", "name": "pkg", "path": "pkg", "provenance": "manifest" }],
        "edges": [],
        "reverse_edges": [],
        "stats": {},
        "digest": ""
    }));

    write(&root.join("docs/note.md"), "docs\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "docs only"]);
    let docs_head = git(root, &["rev-parse", "HEAD"]);
    let (code, verdict) = run(
        &[
            "validate",
            "--stdin",
            "--json",
            "--root",
            root.to_str().unwrap(),
            "--head",
            &docs_head,
        ],
        Some(&document.to_string()),
    );
    assert_eq!(code, 0, "{verdict}");

    write(
        &root.join("pkg/src/lib.rs"),
        "pub fn value() -> u32 { 2 }\n",
    );
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "graph input"]);
    let code_head = git(root, &["rev-parse", "HEAD"]);
    let (code, verdict) = run(
        &[
            "validate",
            "--stdin",
            "--json",
            "--root",
            root.to_str().unwrap(),
            "--head",
            &code_head,
        ],
        Some(&document.to_string()),
    );
    assert_eq!(code, 3);
    assert_eq!(verdict["code"], "stale_revision", "{verdict}");

    let dirty = seal(&json!({
        "schema": "faktor-impact-graph/v1",
        "revision": { "commit": base, "tree": null, "dirty": true },
        "workspace": 1,
        "files": [],
        "nodes": [{ "id": "crate:pkg", "kind": "crate", "name": "pkg", "path": "pkg", "provenance": "manifest" }],
        "edges": [],
        "reverse_edges": [],
        "stats": {},
        "digest": ""
    }));
    let (code, verdict) = run(
        &[
            "validate",
            "--stdin",
            "--json",
            "--root",
            root.to_str().unwrap(),
            "--head",
            &code_head,
        ],
        Some(&dirty.to_string()),
    );
    assert_eq!(code, 3);
    assert_eq!(verdict["code"], "stale_dirty", "{verdict}");
}

#[test]
fn seal_refuses_a_structurally_invalid_document_instead_of_blessing_it() {
    let mut invalid = fixture();
    invalid["reverse_edges"] = json!([]);
    let mut child = Command::new(binary())
        .args(["impact", "seal", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(invalid.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("reverse edge mismatch"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
