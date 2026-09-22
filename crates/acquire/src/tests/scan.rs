//! Static suites: the crate's non-test source carries no commerce
//! vocabulary, hard-codes no site names, and its dependency graph reaches no
//! model/agent crate. These are regression guards: a future edit that
//! reintroduces `price` or `faktor-agent` fails here first.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Domain/site vocabulary that must never appear in the generic runtime.
/// `order` is deliberately absent: it is legitimate English for ordering.
const FORBIDDEN_VOCABULARY: &[&str] = &[
    "sku",
    "mpn",
    "moq",
    "price",
    "pricing",
    "supplier",
    "manufacturer",
    "offer",
    "offers",
    "marketplace",
    "commerce",
    "bom",
    "rfq",
    "quote",
    "wholesale",
    "vendor",
    "seller",
    "reseller",
    "catalog",
    "alibaba",
    "1688",
    "lcsc",
    "mouser",
    "digikey",
    "taobao",
    "tmall",
];

/// Crate names that must never be reachable from `faktor-acquire`.
const FORBIDDEN_DEPENDENCIES: &[&str] = &[
    "faktor-agent",
    "faktor-router",
    "faktor-openai",
    "faktor-anthropic",
    "faktor-google",
    "faktor-ollama",
    "faktor-deepseek",
    "openai",
    "anthropic",
    "ollama",
];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("source directory readable") {
        let path = entry.expect("directory entry readable").path();
        if path.is_dir() {
            // The test suites necessarily name the forbidden vocabulary (this
            // file holds the list); the requirement is about the crate's
            // non-test source, which is a superset of its public API.
            if path.file_name().and_then(|name| name.to_str()) == Some("tests") {
                continue;
            }
            collect_sources(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Lowercased word tokens with camel-case and separator boundaries:
/// `MpnExact`, `mpn_exact` and `mpn-exact` all yield the token `mpn`.
fn tokens(text: &str) -> BTreeSet<String> {
    let mut spaced = String::with_capacity(text.len() + 16);
    let mut previous: Option<char> = None;
    for ch in text.chars() {
        if let Some(prev) = previous {
            if ch.is_ascii_uppercase() && (prev.is_ascii_lowercase() || prev.is_ascii_digit()) {
                spaced.push(' ');
            }
        }
        for lower in ch.to_lowercase() {
            spaced.push(lower);
        }
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            spaced.push(' ');
        }
        previous = Some(ch);
    }
    spaced.split_whitespace().map(str::to_string).collect()
}

#[test]
fn the_public_api_carries_no_commerce_vocabulary_and_no_site_names() {
    let mut sources = Vec::new();
    collect_sources(&manifest_dir().join("src"), &mut sources);
    assert!(
        sources.len() >= 10,
        "the scanner must actually see the crate sources"
    );
    let mut violations = Vec::new();
    for path in &sources {
        let text = fs::read_to_string(path).expect("source readable as UTF-8");
        for token in tokens(&text) {
            if FORBIDDEN_VOCABULARY.contains(&token.as_str()) {
                violations.push(format!("{}: {token}", path.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "commerce vocabulary leaked into the generic runtime: {violations:?}"
    );
}

fn lockfile_graph(lock: &str) -> BTreeMap<String, Vec<String>> {
    let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut in_dependencies = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            current = None;
            in_dependencies = false;
            continue;
        }
        if in_dependencies {
            if line == "]" {
                in_dependencies = false;
                continue;
            }
            let dep = line.trim_matches(|ch| ch == '"' || ch == ',' || ch == ' ');
            if let (Some(name), false) = (&current, dep.is_empty()) {
                let dep = dep.split(' ').next().unwrap_or_default().to_string();
                graph.entry(name.clone()).or_default().push(dep);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("name = ") {
            let name = rest.trim_matches('"').to_string();
            graph.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        if line == "dependencies = [" {
            in_dependencies = true;
        }
    }
    graph
}

fn reachable(graph: &BTreeMap<String, Vec<String>>, root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.to_string()];
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(dependencies) = graph.get(&name) {
            for dependency in dependencies {
                if !seen.contains(dependency) {
                    stack.push(dependency.clone());
                }
            }
        }
    }
    seen
}

#[test]
fn the_dependency_graph_reaches_no_model_or_agent_crate() {
    let lock_path = manifest_dir().join("../../Cargo.lock");
    let lock = fs::read_to_string(&lock_path).expect("workspace Cargo.lock readable");
    let graph = lockfile_graph(&lock);
    let dependencies = graph
        .get("faktor-acquire")
        .expect("faktor-acquire is in the workspace lockfile")
        .clone();
    assert!(
        dependencies.iter().any(|dep| dep == "faktor-provider"),
        "the checked egress authority must be a direct dependency: {dependencies:?}"
    );

    let reached = reachable(&graph, "faktor-acquire");
    let offenders: Vec<&String> = reached
        .iter()
        .filter(|name| FORBIDDEN_DEPENDENCIES.contains(&name.as_str()))
        .collect();
    assert!(
        offenders.is_empty(),
        "the acquisition runtime must not reach model/agent crates: {offenders:?}"
    );
    // Sanity: the walk is real (it must have reached the egress authority).
    assert!(reached.contains("faktor-provider"));
    assert!(reached.contains("faktor-core"));
}

#[test]
fn the_manifest_has_no_http_client_outside_dev_dependencies() {
    let manifest =
        fs::read_to_string(manifest_dir().join("Cargo.toml")).expect("manifest readable");
    let mut section = "";
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line;
            continue;
        }
        if section == "[dependencies]" {
            for (crate_name, forbidden) in [
                (
                    "reqwest",
                    "the direct-HTTP path must reuse the injected transport",
                ),
                (
                    "hyper",
                    "the direct-HTTP path must reuse the injected transport",
                ),
                (
                    "ureq",
                    "the direct-HTTP path must reuse the injected transport",
                ),
                ("faktor-agent", "no agent runtime"),
                ("faktor-router", "no model execution"),
            ] {
                assert!(
                    !line.starts_with(&format!("{crate_name} ")),
                    "{crate_name} must not be a runtime dependency ({forbidden}): {line}"
                );
            }
        }
    }
    assert!(
        manifest.contains("[dev-dependencies]"),
        "test doubles belong in dev-dependencies"
    );
}

#[test]
fn the_workspace_membership_is_declared() {
    let workspace =
        fs::read_to_string(manifest_dir().join("../../Cargo.toml")).expect("workspace readable");
    assert!(
        workspace.contains("\"crates/acquire\""),
        "faktor-acquire must be a workspace member"
    );
}
