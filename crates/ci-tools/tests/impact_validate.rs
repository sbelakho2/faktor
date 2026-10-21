use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use faktor_impact::{GraphBuilder, Revision, TopologyOptions};
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

fn write_rel(root: &Path, relative: &str, text: &str) {
    write(&root.join(relative), text);
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

fn init_repo(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "validator@example.invalid"]);
    git(root, &["config", "user.name", "validator"]);
}

fn commit_all(root: &Path, message: &str) -> String {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", message]);
    git(root, &["rev-parse", "HEAD"])
}

/// The real producer's selector projection, bound to `commit`: the validator
/// tests therefore cover the published inventory, not a hand-written path
/// list that could drift from `faktor-impact`.
fn built_graph(root: &Path, commit: &str) -> Value {
    let mut builder = GraphBuilder::new_with_topology(
        root,
        Revision {
            commit: Some(commit.to_string()),
            tree: None,
            dirty: false,
        },
        TopologyOptions::parser_only(),
    )
    .unwrap();
    let graph = builder.build().unwrap();
    let graph = graph.selector_projection().unwrap();
    serde_json::to_value(&graph).unwrap()
}

fn legacy_graph(document: &Value) -> Value {
    let mut legacy = document.clone();
    legacy.as_object_mut().unwrap().remove("material_inputs");
    seal(&legacy)
}

fn validate_at(root: &Path, head: &str, document: &Value) -> (i32, Value) {
    run(
        &[
            "validate",
            "--stdin",
            "--json",
            "--root",
            root.to_str().unwrap(),
            "--head",
            head,
        ],
        Some(&document.to_string()),
    )
}

#[test]
fn root_package_manifest_and_lib_source_changes_are_stale() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write_rel(root, "package.json", r#"{"name":"root","private":true}"#);
    write_rel(root, "lib/a.ts", "export const a = 1;\n");
    let base = commit_all(root, "base");
    let document = built_graph(root, &base);
    let legacy = legacy_graph(&document);

    write_rel(root, "docs/note.md", "docs\n");
    let docs = commit_all(root, "docs only");
    let (code, verdict) = validate_at(root, &docs, &document);
    assert_eq!(code, 0, "{verdict}");

    write_rel(root, "lib/a.ts", "export const a = 2;\n");
    let changed = commit_all(root, "root lib source");
    for graph in [&document, &legacy] {
        let (code, verdict) = validate_at(root, &changed, graph);
        assert_eq!(code, 3, "{verdict}");
        assert_eq!(verdict["code"], "stale_revision", "{verdict}");
        assert!(
            verdict["error"].as_str().unwrap().contains("lib/a.ts"),
            "{verdict}"
        );
    }

    let manifest_document = built_graph(root, &changed);
    write_rel(
        root,
        "package.json",
        r#"{"name":"root","private":true,"version":"2"}"#,
    );
    let manifest = commit_all(root, "root manifest");
    for graph in [&manifest_document, &legacy_graph(&manifest_document)] {
        let (code, verdict) = validate_at(root, &manifest, graph);
        assert_eq!(code, 3, "{verdict}");
        assert_eq!(verdict["code"], "stale_revision", "{verdict}");
        assert!(
            verdict["error"].as_str().unwrap().contains("package.json"),
            "{verdict}"
        );
    }
}

#[test]
fn root_custom_rust_path_is_stale_after_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write_rel(
        root,
        "Cargo.toml",
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
    );
    write_rel(root, "custom/engine.rs", "pub fn engine() -> u32 { 1 }\n");
    let base = commit_all(root, "base");
    let document = built_graph(root, &base);
    let legacy = legacy_graph(&document);

    write_rel(root, "custom/engine.rs", "pub fn engine() -> u32 { 2 }\n");
    let changed = commit_all(root, "custom rust source");
    for graph in [&document, &legacy] {
        let (code, verdict) = validate_at(root, &changed, graph);
        assert_eq!(code, 3, "{verdict}");
        assert_eq!(verdict["code"], "stale_revision", "{verdict}");
        assert!(
            verdict["error"]
                .as_str()
                .unwrap()
                .contains("custom/engine.rs"),
            "{verdict}"
        );
    }
}

#[test]
fn manifestless_generic_repository_source_is_stale_after_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write_rel(root, "lib/engine.ts", "export const engine = 1;\n");
    let base = commit_all(root, "base");
    let document = built_graph(root, &base);
    let legacy = legacy_graph(&document);

    write_rel(root, "lib/engine.ts", "export const engine = 2;\n");
    let changed = commit_all(root, "generic source");
    for graph in [&document, &legacy] {
        let (code, verdict) = validate_at(root, &changed, graph);
        assert_eq!(code, 3, "{verdict}");
        assert_eq!(verdict["code"], "stale_revision", "{verdict}");
    }
}

#[test]
fn deleted_and_renamed_inputs_are_stale() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write_rel(root, "lib/a.ts", "export const a = 1;\n");
    write_rel(root, "lib/b.ts", "export const b = 1;\n");
    let base = commit_all(root, "base");
    let document = built_graph(root, &base);
    let legacy = legacy_graph(&document);

    fs::remove_file(root.join("lib/a.ts")).unwrap();
    let deleted = commit_all(root, "delete input");
    for graph in [&document, &legacy] {
        let (code, verdict) = validate_at(root, &deleted, graph);
        assert_eq!(code, 3, "{verdict}");
        assert_eq!(verdict["code"], "stale_revision", "{verdict}");
        assert!(
            verdict["error"].as_str().unwrap().contains("lib/a.ts"),
            "{verdict}"
        );
    }

    write_rel(root, "lib/a.ts", "export const a = 1;\n");
    let restored = commit_all(root, "restore input");
    let renamed_document = built_graph(root, &restored);
    fs::rename(root.join("lib/b.ts"), root.join("lib/c.ts")).unwrap();
    let renamed = commit_all(root, "rename input");
    for graph in [&renamed_document, &legacy_graph(&renamed_document)] {
        let (code, verdict) = validate_at(root, &renamed, graph);
        assert_eq!(code, 3, "{verdict}");
        assert_eq!(verdict["code"], "stale_revision", "{verdict}");
        assert!(
            verdict["error"].as_str().unwrap().contains("lib/b.ts"),
            "{verdict}"
        );
    }
}

#[cfg(unix)]
#[test]
fn file_to_symlink_type_change_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write_rel(root, "lib/a.ts", "export const a = 1;\n");
    let base = commit_all(root, "base");
    let document = built_graph(root, &base);

    fs::remove_file(root.join("lib/a.ts")).unwrap();
    std::os::unix::fs::symlink("target.ts", root.join("lib/a.ts")).unwrap();
    let changed = commit_all(root, "type change");
    for graph in [&document, &legacy_graph(&document)] {
        let (code, verdict) = validate_at(root, &changed, graph);
        assert_eq!(code, 3, "{verdict}");
        assert_eq!(verdict["code"], "stale_revision", "{verdict}");
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_changed_path_fails_closed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("git");
    // A file name that cannot be decoded as UTF-8 (0xFF). APFS rejects such
    // names, so the raw git output is injected instead of creating the file.
    fs::write(&fake, "#!/bin/sh\nprintf 'lib/\\377.ts\\0'\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let document = seal(&json!({
        "schema": "faktor-impact-graph/v1",
        "revision": { "commit": "deadbeef", "tree": null, "dirty": false },
        "workspace": 1,
        "files": [],
        "nodes": [{ "id": "crate:a", "kind": "crate", "name": "a", "path": "a", "provenance": "manifest" }],
        "edges": [],
        "reverse_edges": [],
        "stats": {},
        "digest": ""
    }));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut search = std::ffi::OsString::from(&bin);
    search.push(":");
    search.push(path);
    let mut command = Command::new(binary());
    command
        .arg("impact")
        .arg("validate")
        .arg("--stdin")
        .arg("--json")
        .arg("--root")
        .arg(root)
        .arg("--head")
        .arg("new")
        .env("PATH", search)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(document.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let verdict: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(3), "{verdict}");
    assert_eq!(verdict["code"], "stale_path_undecodable", "{verdict}");
}

#[test]
fn unknown_repository_coverage_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write_rel(root, "plugin.rs", "pub fn plugin() -> u32 { 1 }\n");
    let base = commit_all(root, "base");
    let document = seal(&json!({
        "schema": "faktor-impact-graph/v1",
        "revision": { "commit": base, "tree": null, "dirty": false },
        "workspace": 1,
        "files": [],
        "nodes": [
            { "id": "test:t.rs#t@1", "kind": "test", "name": "t", "path": "t.rs", "crate_name": "a", "provenance": "coverage" }
        ],
        "edges": [],
        "reverse_edges": [],
        "stats": {},
        "digest": ""
    }));
    write_rel(root, "plugin.rs", "pub fn plugin() -> u32 { 2 }\n");
    let changed = commit_all(root, "unknown coverage");
    let (code, verdict) = validate_at(root, &changed, &document);
    assert_eq!(code, 3, "{verdict}");
    assert_eq!(verdict["code"], "stale_coverage_unknown", "{verdict}");
}

#[test]
fn inventory_without_units_binds_exact_inputs_but_refuses_new_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init_repo(root);
    write_rel(root, "lib/a.ts", "export const a = 1;\n");
    write_rel(root, "docs/note.md", "docs\n");
    let base = commit_all(root, "base");
    let digest = format!(
        "blake3:{}",
        blake3::hash(&fs::read(root.join("lib/a.ts")).unwrap()).to_hex()
    );
    let document = seal(&json!({
        "schema": "faktor-impact-graph/v1",
        "revision": { "commit": base, "tree": null, "dirty": false },
        "workspace": 1,
        "files": [],
        "nodes": [],
        "edges": [],
        "reverse_edges": [],
        "material_inputs": [{ "path": "lib/a.ts", "digest": digest }],
        "stats": {},
        "digest": ""
    }));

    write_rel(root, "docs/note.md", "docs changed\n");
    let docs = commit_all(root, "docs only");
    let (code, verdict) = validate_at(root, &docs, &document);
    assert_eq!(code, 0, "{verdict}");

    write_rel(root, "lib/a.ts", "export const a = 2;\n");
    let changed = commit_all(root, "exact input");
    let (code, verdict) = validate_at(root, &changed, &document);
    assert_eq!(code, 3, "{verdict}");
    assert_eq!(verdict["code"], "stale_revision", "{verdict}");

    write_rel(root, "lib/new.ts", "export const newSource = 1;\n");
    let new_source = commit_all(root, "new source");
    let (code, verdict) = validate_at(root, &new_source, &document);
    assert_eq!(code, 3, "{verdict}");
    assert_eq!(verdict["code"], "stale_coverage_unknown", "{verdict}");
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
