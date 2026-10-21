//! Rust authority for the serialized repository graph consumed by CI.
//!
//! `faktor-ci-tools impact validate` runs the SAME `faktor-impact`
//! `Graph::from_json` authority (schema, ordering, dangling edges,
//! reverse-edge bijection and BLAKE3 digest) and the revision binding the
//! selector used to reimplement in JavaScript. The verdict is a stable,
//! machine-readable JSON document; invalid, stale or unreadable graphs are a
//! typed refusal with a non-zero exit code.
//!
//! `impact seal` computes the canonical digest for fixture/generator use. It
//! validates the structure first, so it can never bless an inconsistent
//! graph.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use faktor_impact::{is_topology_manifest, Graph, GraphError, Language, NodeKind};
use serde_json::{json, Value};

const VERDICT_SCHEMA: &str = "faktor-impact-validation/v1";
const MAX_GRAPH_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_STDIN_BYTES: u64 = MAX_GRAPH_BYTES;

enum Failure {
    Usage(String),
    Refused { code: &'static str, detail: String },
    Io(String),
}

impl Failure {
    fn from_graph(error: &GraphError) -> Self {
        let code = match error {
            GraphError::Schema { .. } => "schema",
            GraphError::Oversized { .. } => "oversized",
            GraphError::Malformed(_) => "malformed",
            GraphError::DuplicateNode(_) => "duplicate_node",
            GraphError::DuplicateEdge(_) => "duplicate_edge",
            GraphError::DanglingEdge(_) => "dangling_edge",
            GraphError::ReverseMismatch(_) => "reverse_mismatch",
            GraphError::Unsorted(_) => "unsorted",
            GraphError::DigestMismatch { .. } => "digest_mismatch",
            GraphError::CorruptJson(_) => "corrupt_json",
        };
        Failure::Refused {
            code,
            detail: error.to_string(),
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("usage: faktor-ci-tools impact validate|seal ...");
        return 2;
    };
    let rest = &args[1..];
    match command {
        "validate" => validate_command(rest),
        "seal" => seal_command(rest),
        other => {
            eprintln!("faktor-ci-tools impact: unknown subcommand '{other}'");
            2
        }
    }
}

fn validate_command(args: &[String]) -> i32 {
    match validate(args) {
        Ok(verdict) => {
            println!("{verdict}");
            0
        }
        Err(Failure::Usage(message)) => {
            eprintln!("{message}");
            2
        }
        Err(Failure::Refused { code, detail }) => {
            println!("{}", refusal(None, code, &detail));
            3
        }
        Err(Failure::Io(message)) => {
            println!("{}", refusal(None, "io", &message));
            3
        }
    }
}

fn validate(args: &[String]) -> Result<String, Failure> {
    let mut path: Option<PathBuf> = None;
    let mut stdin = false;
    let mut head: Option<String> = None;
    let mut root: Option<PathBuf> = None;
    let mut json_only = false;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--stdin" => stdin = true,
            "--json" => json_only = true,
            "--head" => {
                index += 1;
                head = args.get(index).cloned();
            }
            "--root" => {
                index += 1;
                root = args.get(index).map(PathBuf::from);
            }
            other if other.starts_with('-') => {
                return Err(Failure::Usage(format!(
                    "impact validate: unknown option '{other}'"
                )));
            }
            other => path = Some(PathBuf::from(other)),
        }
        index += 1;
    }
    if !stdin && path.is_none() {
        return Err(Failure::Usage(
            "usage: faktor-ci-tools impact validate <graph.json> [--head SHA --root DIR] [--json]\n       faktor-ci-tools impact validate --stdin ...".into(),
        ));
    }
    let text = if stdin {
        read_stdin_bounded()?
    } else {
        read_bounded(path.as_deref().expect("checked above"))?
    };
    let shown_path = path.as_ref().map(|path| path.display().to_string());
    let graph = Graph::from_json(&text).map_err(|error| {
        let Failure::Refused { code, detail } = Failure::from_graph(&error) else {
            unreachable!()
        };
        Failure::Refused {
            code,
            detail: format!(
                "{} ({})",
                detail,
                shown_path.clone().unwrap_or_else(|| "<stdin>".into())
            ),
        }
    })?;
    if let Some(head) = head.as_deref() {
        let root = root.unwrap_or_else(|| PathBuf::from("."));
        if let Some((code, detail)) = binding_error(&graph, &root, head) {
            return Err(Failure::Refused { code, detail });
        }
    }
    if !json_only {
        // Machine-readable verdict is the default; --json is accepted for
        // callers that make the contract explicit.
    }
    Ok(json!({
        "schema": VERDICT_SCHEMA,
        "ok": true,
        "path": shown_path,
        "graph_schema": graph.schema,
        "revision": graph.revision,
        "digest": graph.digest,
        "nodes": graph.nodes.len(),
        "edges": graph.edges.len(),
        "reverse_edges": graph.reverse_edges.len(),
        "files": graph.files.len(),
    })
    .to_string())
}

fn refusal(path: Option<&Path>, code: &str, detail: &str) -> String {
    json!({
        "schema": VERDICT_SCHEMA,
        "ok": false,
        "code": code,
        "error": detail,
        "path": path.map(|path| path.display().to_string()),
    })
    .to_string()
}

fn seal_command(args: &[String]) -> i32 {
    let mut path: Option<PathBuf> = None;
    let mut stdin = false;
    let mut out: Option<PathBuf> = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--stdin" => stdin = true,
            "--out" => {
                index += 1;
                out = args.get(index).map(PathBuf::from);
            }
            other if other.starts_with('-') => {
                eprintln!("impact seal: unknown option '{other}'");
                return 2;
            }
            other => path = Some(PathBuf::from(other)),
        }
        index += 1;
    }
    if !stdin && path.is_none() {
        eprintln!("usage: faktor-ci-tools impact seal <graph.json> [--out FILE] | --stdin");
        return 2;
    }
    let text = if stdin {
        match read_stdin_bounded() {
            Ok(text) => text,
            Err(Failure::Io(error)) => {
                eprintln!("{error}");
                return 3;
            }
            Err(_) => return 3,
        }
    } else {
        match read_bounded(path.as_deref().expect("checked above")) {
            Ok(text) => text,
            Err(Failure::Io(error)) => {
                eprintln!("{error}");
                return 3;
            }
            Err(_) => return 3,
        }
    };
    let mut graph: Graph = match serde_json::from_str(&text) {
        Ok(graph) => graph,
        Err(error) => {
            eprintln!("impact seal: unreadable graph: {error}");
            return 3;
        }
    };
    if let Err(error) = graph.validate() {
        eprintln!("impact seal: {error}");
        return 3;
    }
    if let Err(error) = graph.seal() {
        eprintln!("impact seal: {error}");
        return 3;
    }
    let text = match serde_json::to_string(&graph) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("impact seal: serialize: {error}");
            return 3;
        }
    };
    match out {
        Some(path) => {
            if let Err(error) = std::fs::write(&path, format!("{text}\n")) {
                eprintln!("impact seal: {}: {error}", path.display());
                return 3;
            }
        }
        None => println!("{text}"),
    }
    0
}

fn read_bounded(path: &Path) -> Result<String, Failure> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| Failure::Io(format!("{}: {error}", path.display())))?;
    if !meta.file_type().is_file() {
        return Err(Failure::Io(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > MAX_GRAPH_BYTES {
        return Err(Failure::Refused {
            code: "oversized",
            detail: format!(
                "{} exceeds the {MAX_GRAPH_BYTES}-byte graph bound",
                path.display()
            ),
        });
    }
    std::fs::read(path)
        .map_err(|error| Failure::Io(format!("{}: {error}", path.display())))
        .and_then(|bytes| {
            String::from_utf8(bytes)
                .map_err(|error| Failure::Io(format!("{} is not UTF-8: {error}", path.display())))
        })
}

fn read_stdin_bounded() -> Result<String, Failure> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_STDIN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Failure::Io(format!("stdin: {error}")))?;
    if bytes.len() as u64 > MAX_STDIN_BYTES {
        return Err(Failure::Refused {
            code: "oversized",
            detail: format!("stdin exceeds the {MAX_STDIN_BYTES}-byte graph bound"),
        });
    }
    String::from_utf8(bytes).map_err(|error| Failure::Io(format!("stdin is not UTF-8: {error}")))
}

/// The selector's documented revision binding: a graph may be committed one
/// commit after the revision it describes, and stays valid while every later
/// change is outside the graph inputs. The Rust authority owns this decision
/// so the JavaScript consumer never reimplements it.
fn binding_error(graph: &Graph, root: &Path, head: &str) -> Option<(&'static str, String)> {
    let revision = &graph.revision;
    let commit = revision
        .commit
        .as_deref()
        .filter(|commit| !commit.is_empty())?;
    if commit == head {
        return None;
    }
    if revision.dirty {
        return Some((
            "stale_dirty",
            format!("impact graph is stale: revision {commit} was built from a dirty tree, not HEAD {head}"),
        ));
    }
    match changed_inputs(graph, root, commit, head) {
        Ok(inputs) if inputs.is_empty() => None,
        Ok(inputs) => Some((
            "stale_revision",
            format!(
                "impact graph is stale: revision {commit} != HEAD {head} (graph inputs changed: {})",
                inputs.into_iter().take(3).collect::<Vec<_>>().join(", ")
            ),
        )),
        Err((code, error)) => Some((
            code,
            format!("impact graph is stale: revision {commit} != HEAD {head} ({error})"),
        )),
    }
}

/// Local units the graph actually scans. A root unit (`path: "."`) covers the
/// whole repository; other units cover their directory. A graph that declares
/// no unit cannot prove any changed path inert, so that is a typed refusal
/// rather than a fresh verdict.
struct Coverage {
    root: bool,
    dirs: Vec<String>,
}

impl Coverage {
    fn from_graph(graph: &Graph) -> Self {
        let mut root = false;
        let mut dirs = Vec::new();
        for node in &graph.nodes {
            if !matches!(node.kind, NodeKind::Crate | NodeKind::Package) {
                continue;
            }
            let Some(path) = node.path.as_deref() else {
                continue;
            };
            let path = path.strip_prefix("./").unwrap_or(path);
            let path = path.trim_end_matches('/');
            if path.is_empty() || path == "." {
                root = true;
            } else {
                dirs.push(path.to_string());
            }
        }
        dirs.sort();
        dirs.dedup();
        Self { root, dirs }
    }

    fn known(&self) -> bool {
        self.root || !self.dirs.is_empty()
    }

    fn covers(&self, graph: &Graph, path: &str) -> bool {
        if self.root {
            return true;
        }
        if self.dirs.iter().any(|dir| {
            path == dir
                || path
                    .strip_prefix(dir.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        }) {
            return true;
        }
        graph.files.iter().any(|file| file.path == path)
            || graph.material_inputs.iter().any(|input| input.path == path)
    }
}

/// Compare the revision delta against the graph's published material input
/// inventory. Paths whose HEAD bytes still hash to the recorded digest did
/// not change the graph content; deleted inputs, renamed sources, new sources
/// under a covered unit and manifest changes do. Without an inventory the
/// supported-extension/manifest rule is applied anywhere in the graph-covered
/// repository (never "unknown means fresh").
fn changed_inputs(
    graph: &Graph,
    root: &Path,
    base: &str,
    head: &str,
) -> Result<Vec<String>, (&'static str, String)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=ACMRDT",
            "-z",
            &format!("{base}..{head}"),
            "--",
        ])
        .output()
        .map_err(|error| {
            (
                "stale_diff_unavailable",
                format!("diff unavailable: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err((
            "stale_diff_unavailable",
            format!(
                "diff unavailable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    if output.stdout.len() > MAX_GIT_BYTES {
        return Err((
            "stale_diff_unavailable",
            "changed-path list exceeded the bound".into(),
        ));
    }
    let mut changed = BTreeSet::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let Ok(path) = std::str::from_utf8(raw) else {
            // Undecodable paths cannot be classified against canonical graph
            // paths; treating them as inert is the fail-open bug.
            return Err((
                "stale_path_undecodable",
                format!("changed path is not valid UTF-8: {}", raw.escape_ascii()),
            ));
        };
        let path = path.replace('\\', "/");
        if path.is_empty() {
            continue;
        }
        if path.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return Err((
                "stale_path_undecodable",
                format!("changed path carries control characters: {path:?}"),
            ));
        }
        changed.insert(path);
    }
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let coverage = Coverage::from_graph(graph);
    let inventory: BTreeMap<&str, &str> = graph
        .material_inputs
        .iter()
        .map(|input| (input.path.as_str(), input.digest.as_str()))
        .collect();
    let has_inventory = !inventory.is_empty();
    let mut material = BTreeSet::new();
    let mut verify = BTreeSet::new();
    for path in &changed {
        if inventory.contains_key(path.as_str()) {
            verify.insert(path.clone());
            continue;
        }
        let potential = Language::of_path(path).is_some() || is_topology_manifest(path);
        if !potential {
            continue;
        }
        if !coverage.known() {
            // No published inventory entry and no declared unit: the graph
            // cannot prove this path is inert, so refuse instead of guessing.
            return Err((
                "stale_coverage_unknown",
                format!(
                    "repository coverage is unknown: graph declares no local unit, so {path} cannot be proven inert"
                ),
            ));
        }
        // Legacy graph: conservative anywhere in the covered repository.
        // Published inventory: a new source inside a unit is material; a
        // manifest anywhere is material because it can add/move a unit.
        let material_now = if has_inventory {
            is_topology_manifest(path) || coverage.covers(graph, path)
        } else {
            true
        };
        if material_now {
            material.insert(path.clone());
        }
    }
    if !verify.is_empty() {
        let digests = head_digests(root, head, &verify)?;
        for path in verify {
            let unchanged = inventory
                .get(path.as_str())
                .zip(digests.get(&path))
                .is_some_and(|(recorded, actual)| *recorded == actual.as_str());
            if !unchanged {
                material.insert(path);
            }
        }
    }
    Ok(material.into_iter().collect())
}

/// BLAKE3 digests of `head:path` blobs, read through one bounded git child.
/// Missing objects (deleted inputs) and non-blob paths stay absent so the
/// caller treats them as material, never as unchanged.
fn head_digests(
    root: &Path,
    head: &str,
    paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>, (&'static str, String)> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            (
                "stale_diff_unavailable",
                format!("head content unavailable: {error}"),
            )
        })?;
    let input: String = paths
        .iter()
        .map(|path| format!("{head}:{path}\n"))
        .collect();
    let Some(mut stdin) = child.stdin.take() else {
        return Err((
            "stale_diff_unavailable",
            "head content unavailable: no git stdin".into(),
        ));
    };
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut stdout = Vec::new();
    if let Some(out) = child.stdout.take() {
        out.take(MAX_GIT_BYTES as u64 + 1)
            .read_to_end(&mut stdout)
            .map_err(|error| {
                (
                    "stale_diff_unavailable",
                    format!("head content unavailable: {error}"),
                )
            })?;
    }
    let status = child.wait().map_err(|error| {
        (
            "stale_diff_unavailable",
            format!("head content unavailable: {error}"),
        )
    })?;
    let _ = writer.join();
    if stdout.len() > MAX_GIT_BYTES {
        return Err((
            "stale_diff_unavailable",
            "head content exceeded the bound".into(),
        ));
    }
    if !status.success() {
        return Err((
            "stale_diff_unavailable",
            "head content unavailable: git cat-file failed".into(),
        ));
    }
    let mut digests = BTreeMap::new();
    let mut cursor = 0usize;
    for path in paths {
        let rest = stdout.get(cursor..).ok_or_else(|| {
            (
                "stale_diff_unavailable",
                "head content is truncated".to_string(),
            )
        })?;
        let newline = rest.iter().position(|byte| *byte == b'\n').ok_or_else(|| {
            (
                "stale_diff_unavailable",
                "head content header is truncated".to_string(),
            )
        })?;
        let header = std::str::from_utf8(&rest[..newline]).map_err(|_| {
            (
                "stale_diff_unavailable",
                "head content header is not UTF-8".to_string(),
            )
        })?;
        cursor += newline + 1;
        let mut fields = header.split(' ');
        let _object = fields.next();
        let kind = fields.next().unwrap_or_default();
        if kind == "missing" {
            continue;
        }
        let size: usize = fields
            .next()
            .and_then(|raw| raw.parse().ok())
            .ok_or_else(|| {
                (
                    "stale_diff_unavailable",
                    "head content header is malformed".to_string(),
                )
            })?;
        let end = cursor.checked_add(size).ok_or_else(|| {
            (
                "stale_diff_unavailable",
                "head content size overflows".to_string(),
            )
        })?;
        if end > stdout.len() || stdout.get(end) != Some(&b'\n') {
            return Err((
                "stale_diff_unavailable",
                "head content is truncated".to_string(),
            ));
        }
        if kind == "blob" {
            digests.insert(
                path.clone(),
                format!("blake3:{}", blake3::hash(&stdout[cursor..end]).to_hex()),
            );
        }
        cursor = end + 1;
    }
    Ok(digests)
}

#[allow(dead_code)]
fn _value(_value: Value) {}
