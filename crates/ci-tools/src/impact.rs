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

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use faktor_impact::{Graph, GraphError};
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
    match changed_inputs(root, commit, head) {
        Ok(inputs) if inputs.is_empty() => None,
        Ok(inputs) => Some((
            "stale_revision",
            format!(
                "impact graph is stale: revision {commit} != HEAD {head} (graph inputs changed: {})",
                inputs.into_iter().take(3).collect::<Vec<_>>().join(", ")
            ),
        )),
        Err(error) => Some((
            "stale_diff_unavailable",
            format!("impact graph is stale: revision {commit} != HEAD {head} (diff unavailable: {error})"),
        )),
    }
}

fn changed_inputs(root: &Path, base: &str, head: &str) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=ACMRD",
            "-z",
            &format!("{base}..{head}"),
            "--",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.stdout.len() > MAX_GIT_BYTES {
        return Err("changed-path list exceeded the bound".into());
    }
    let mut inputs = BTreeSet::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        let Ok(path) = std::str::from_utf8(raw) else {
            continue;
        };
        let path = path.replace('\\', "/");
        if path.is_empty() {
            continue;
        }
        if graph_input(root, &path) {
            inputs.insert(path);
        }
    }
    Ok(inputs.into_iter().collect())
}

fn graph_input(root: &Path, path: &str) -> bool {
    const EXACT: &[&str] = &[
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "package.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "settings.gradle",
        "settings.gradle.kts",
        "build.gradle",
        "build.gradle.kts",
        "pom.xml",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "poetry.lock",
    ];
    if EXACT.contains(&path) {
        return true;
    }
    if path.ends_with("/Cargo.toml")
        || path.ends_with("/package.json")
        || path.ends_with("/build.gradle")
        || path.ends_with("/build.gradle.kts")
        || path.ends_with("/settings.gradle")
        || path.ends_with("/settings.gradle.kts")
        || path.ends_with("/pom.xml")
        || path.ends_with("/pyproject.toml")
        || path.ends_with("/setup.py")
        || path.ends_with("/setup.cfg")
    {
        return true;
    }
    const SOURCE: &[&str] = &[
        ".rs", ".js", ".mjs", ".cjs", ".jsx", ".ts", ".tsx", ".mts", ".cts", ".py", ".pyi",
        ".java", ".kt", ".kts",
    ];
    if !SOURCE.iter().any(|extension| path.ends_with(extension)) {
        return false;
    }
    // A source file belongs to the graph only when the directory that owns
    // it declares a unit manifest. Scripts and programme metadata under
    // `tests/programme/` are not graph inputs even though they live below a
    // directory named `tests`.
    const UNIT_MANIFESTS: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "build.gradle",
        "build.gradle.kts",
        "pom.xml",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
    ];
    let mut current = Path::new(path).parent();
    while let Some(relative) = current {
        if relative.as_os_str().is_empty() {
            break;
        }
        for manifest in UNIT_MANIFESTS {
            if root.join(relative).join(manifest).is_file() {
                return true;
            }
        }
        current = relative.parent();
    }
    // Repository-root packages (`src/...` with a root manifest) are inputs;
    // root-level scripts are not.
    path.starts_with("src/")
}

#[allow(dead_code)]
fn _value(_value: Value) {}
