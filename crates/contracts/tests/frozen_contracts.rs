//! End-to-end witnesses for the frozen-contract gate:
//!
//! * the real tree passes against the compiled vocabularies;
//! * a scratch copy with a renamed and a reordered variant makes `check` fail;
//! * `write` is byte-deterministic across independent processes (and hash
//!   seeds) and removes stale frozen files.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_faktor-contracts")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/contracts has a parent")
        .parent()
        .expect("crates/contracts lives in the workspace")
        .to_path_buf()
}

fn run(root: &Path, command: &str) -> std::process::Output {
    Command::new(bin())
        .arg(command)
        .arg("--root")
        .arg(root)
        .output()
        .expect("faktor-contracts must run")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn copy_contracts(root: &Path) -> PathBuf {
    let source = repo_root().join("docs/contracts");
    let target = root.join("docs/contracts");
    std::fs::create_dir_all(&target).expect("scratch contracts dir");
    for entry in std::fs::read_dir(&source).expect("read docs/contracts") {
        let entry = entry.expect("dir entry");
        std::fs::copy(entry.path(), target.join(entry.file_name())).expect("copy frozen file");
    }
    target
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("frozen file")).expect("json")
}

fn write_json(path: &Path, value: &serde_json::Value) {
    let mut text = serde_json::to_string_pretty(value).expect("serialize");
    text.push('\n');
    std::fs::write(path, text).expect("write frozen file");
}

fn file_map(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("dir entry");
        out.insert(
            entry.file_name().to_string_lossy().to_string(),
            std::fs::read(entry.path()).expect("read file"),
        );
    }
    out
}

#[test]
fn real_tree_frozen_contracts_match_the_compiled_vocabularies() {
    let root = repo_root();
    let output = run(&root, "check");
    assert!(
        output.status.success(),
        "check failed:\n{}{}",
        stdout(&output),
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("contracts: PASS"),
        "missing PASS line: {text}"
    );
    let digest_output = run(&root, "digest");
    assert!(digest_output.status.success(), "{}", stderr(&digest_output));
    let digest = stdout(&digest_output);
    let digest = digest.trim();
    assert!(digest.starts_with("sha256:"), "bad digest: {digest}");
    assert!(text.contains(digest), "check must name the digest {digest}");
}

#[test]
fn check_fails_on_a_renamed_and_reordered_variant_in_a_scratch_copy() {
    let temp = tempfile::tempdir().expect("temp dir");
    let contracts = copy_contracts(temp.path());

    // Rename one wire variant: the compiled vocabulary still says
    // `compact_rejected`.
    let event_path = contracts.join("event-kind.json");
    let mut event = read_json(&event_path);
    let variants = event["variants"].as_array_mut().expect("variants array");
    let renamed = variants
        .iter_mut()
        .find(|variant| variant["wire"] == "compact_rejected")
        .expect("CompactRejected present");
    renamed["wire"] = serde_json::json!("compaction_rejected");
    write_json(&event_path, &event);

    // Reorder two variants: the compiled declaration order is unchanged.
    let error_path = contracts.join("error-kind.json");
    let mut errors = read_json(&error_path);
    let variants = errors["variants"].as_array_mut().expect("variants array");
    variants.swap(0, 1);
    write_json(&error_path, &errors);

    let output = run(temp.path(), "check");
    assert!(
        !output.status.success(),
        "a mutated frozen copy must fail the check:\n{}{}",
        stdout(&output),
        stderr(&output)
    );
    let text = stderr(&output);
    for expected in ["event-kind.json", "error-kind.json", "first divergence"] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
}

#[test]
fn write_is_byte_deterministic_across_processes_and_removes_stale_files() {
    let first = tempfile::tempdir().expect("temp dir");
    let second = tempfile::tempdir().expect("temp dir");
    let first_output = run(first.path(), "write");
    let second_output = run(second.path(), "write");
    assert!(first_output.status.success(), "{}", stderr(&first_output));
    assert!(second_output.status.success(), "{}", stderr(&second_output));
    let first_files = file_map(&first.path().join("docs/contracts"));
    let second_files = file_map(&second.path().join("docs/contracts"));
    assert!(!first_files.is_empty(), "write produced no files");
    assert_eq!(
        first_files, second_files,
        "two independent generation runs must produce byte-identical files"
    );

    // A stale frozen file is removed by write and refused by check.
    let stale = first.path().join("docs/contracts/stale-contract.json");
    std::fs::write(&stale, b"{}\n").expect("write stale file");
    let drift = run(first.path(), "check");
    assert!(
        !drift.status.success(),
        "the stale file must fail the check"
    );
    assert!(stderr(&drift).contains("stale-contract.json"));
    let rewrite = run(first.path(), "write");
    assert!(rewrite.status.success(), "{}", stderr(&rewrite));
    assert!(!stale.exists(), "write must remove stale frozen files");
}
