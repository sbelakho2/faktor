use crate::util::{read_json, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn parse_listed(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_suffix(": test") {
            if !name.is_empty() && !name.chars().any(char::is_whitespace) {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn rust_units(manifest: &Value) -> Vec<(String, String)> {
    let mut units = Vec::new();
    let Some(entries) = manifest.get("entries").and_then(Value::as_array) else {
        return units;
    };
    for entry in entries {
        let Some(capabilities) = entry.get("capabilities").and_then(Value::as_array) else {
            continue;
        };
        for row in capabilities {
            if row.get("class").and_then(Value::as_str) == Some("gap")
                || row.get("delegate").is_some()
            {
                continue;
            }
            if let Some(unit) = row.get("unit").and_then(Value::as_str) {
                if !unit.is_empty() {
                    let id = row
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string();
                    units.push((id, unit.to_string()));
                }
            }
        }
    }
    units
}

pub fn check(
    manifest_path: &Path,
    listed_text: &str,
    snapshot_path: &Path,
    run_metadata: bool,
) -> Result<(Vec<String>, usize)> {
    let mut problems = Vec::new();
    let listed = parse_listed(listed_text);
    if listed.is_empty() {
        problems.push(
            "compiled-test-inventory-empty: no libtest lines on stdin (run with `cargo test -- --list`)"
                .to_string(),
        );
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for name in &listed {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    let manifest = read_json(manifest_path)?;
    for (capability, unit) in rust_units(&manifest) {
        let exact = counts.get(&unit).copied().unwrap_or(0);
        let suffix = counts
            .iter()
            .filter(|(name, count)| **count > 0 && name.ends_with(&format!("::{unit}")))
            .count();
        let total = if exact > 0 { exact } else { suffix };
        if total != 1 {
            problems.push(format!(
                "uncompiled-unit: {capability} proof {unit:?} appears {total} time(s) in the compiled test inventory (need exactly 1)"
            ));
        }
    }
    if run_metadata {
        let root = std::env::current_dir().map_err(|error| format!("cwd: {error}"))?;
        let output = Command::new("cargo")
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .current_dir(&root)
            .output()
            .map_err(|error| format!("cargo metadata: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "cargo metadata failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let metadata: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("cargo metadata JSON: {error}"))?;
        let mut live: Vec<String> = metadata
            .get("packages")
            .and_then(Value::as_array)
            .map(|packages| {
                packages
                    .iter()
                    .filter(|package| package.get("source").is_none_or(Value::is_null))
                    .filter_map(|package| package.get("name").and_then(Value::as_str))
                    .filter(|name| !name.starts_with("faktor-tests-"))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        live.sort();
        let snapshot = read_json(snapshot_path)?;
        let mut snapshot_crates: Vec<String> = snapshot
            .get("crates")
            .and_then(Value::as_array)
            .map(|crates| {
                crates
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        snapshot_crates.sort();
        if snapshot_crates != live {
            problems.push(format!(
                "stale-crate-snapshot: tests/production-crates.json does not match cargo metadata (snapshot {} vs live {}); run `node scripts/check-invariant-coverage.mjs --refresh-crates`",
                snapshot_crates.len(),
                live.len()
            ));
        }
    }
    Ok((problems, listed.len()))
}

fn selftest() -> i32 {
    let mut failures = 0;
    let mut check_case = |name: &str, ok: bool| {
        if ok {
            println!("selftest ok: {name}");
        } else {
            eprintln!("selftest FAIL: {name}");
            failures += 1;
        }
    };
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("selftest: tempdir: {error}");
            return 1;
        }
    };
    let manifest_path = dir.path().join("manifest.json");
    let manifest = serde_json::json!({
        "entries": [{
            "crate": "faktor-alpha",
            "class": "release-critical",
            "capabilities": [{"id": "alpha.one", "unit": "proof_one", "mutation": "INV-A"}]
        }],
        "surfaces": []
    });
    std::fs::write(&manifest_path, serde_json::to_string(&manifest).unwrap()).unwrap();
    let snapshot_path = dir.path().join("snapshot.json");
    std::fs::write(&snapshot_path, r#"{"crates":["faktor-alpha"]}"#).unwrap();
    let good = "runtime::tests::proof_one: test\nruntime::tests::other: test\n";
    let (problems, count) = check(&manifest_path, good, &snapshot_path, false).unwrap();
    check_case("a listed proof passes", problems.is_empty() && count == 2);
    let (problems, _) = check(
        &manifest_path,
        "runtime::tests::other: test\n",
        &snapshot_path,
        false,
    )
    .unwrap();
    check_case(
        "an unlisted proof fails",
        problems
            .iter()
            .any(|problem| problem.contains("uncompiled-unit")),
    );
    let (problems, _) = check(
        &manifest_path,
        "a::proof_one: test\nb::proof_one: test\n",
        &snapshot_path,
        false,
    )
    .unwrap();
    check_case(
        "a duplicated proof fails",
        problems
            .iter()
            .any(|problem| problem.contains("uncompiled-unit")),
    );
    let (problems, _) = check(&manifest_path, "", &snapshot_path, false).unwrap();
    check_case(
        "an empty inventory fails",
        problems
            .iter()
            .any(|problem| problem.contains("inventory-empty")),
    );
    if failures > 0 {
        eprintln!("check-capability-tests-compiled selftest: FAIL ({failures})");
        return 1;
    }
    println!("check-capability-tests-compiled selftest: PASS");
    0
}

pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("selftest") {
        return selftest();
    }
    let mut manifest = PathBuf::from("tests/invariant-coverage.json");
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--manifest" && index + 1 < args.len() {
            manifest = PathBuf::from(&args[index + 1]);
            index += 2;
        } else {
            index += 1;
        }
    }
    let listed = match crate::util::read_stdin() {
        Ok(text) => text,
        Err(error) => {
            eprintln!("capability-tests-compiled: {error}");
            return 1;
        }
    };
    match check(
        &manifest,
        &listed,
        Path::new("tests/production-crates.json"),
        true,
    ) {
        Ok((problems, count)) if problems.is_empty() => {
            println!(
                "capability-tests-compiled: PASS ({count} compiled tests listed; every proven unit enumerated)"
            );
            0
        }
        Ok((problems, _)) => {
            for problem in problems {
                eprintln!("capability-tests-compiled: {problem}");
            }
            1
        }
        Err(error) => {
            eprintln!("capability-tests-compiled: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_libtest_lines() {
        let listed = parse_listed("a::b: test\nnoise\na::c: bench\n");
        assert_eq!(listed, vec!["a::b"]);
    }

    #[test]
    fn empty_inventory_is_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("m.json");
        std::fs::write(&manifest, r#"{"entries":[]}"#).unwrap();
        let snapshot = dir.path().join("s.json");
        std::fs::write(&snapshot, r#"{"crates":[]}"#).unwrap();
        let (problems, _) = check(&manifest, "", &snapshot, false).unwrap();
        assert!(problems[0].contains("inventory-empty"));
    }
}
