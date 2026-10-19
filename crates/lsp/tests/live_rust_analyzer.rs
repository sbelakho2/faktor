//! Live semantic probe against the REAL `rust-analyzer` on a real Cargo
//! workspace.
//!
//! This is the production-truth test for the typed semantic surface:
//! definition, references (`includeDeclaration`), workspace symbols and the
//! three call-hierarchy requests must return real symbols/locations from the
//! actual server. The test skips (loudly) only when `rust-analyzer` is not
//! installed; on this machine it runs the binary found on PATH.
//!
//! Measured probes are printed: initialization time, cold vs cached
//! definition latency, and the exact locations/symbols observed.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use faktor_cas::Cas;
use faktor_core::id::WorkspaceId;
use faktor_lsp::{
    file_uri, CallHierarchyItem, LspConfig, LspManager, SemanticError, SemanticOptions, SymbolKind,
};
use faktor_terminal::ProcessSupervisor;
use tempfile::TempDir;

const SAMPLE_LIB: &str = r#"pub fn add_one(value: i64) -> i64 {
    value + 1
}

pub fn compute(input: i64) -> i64 {
    add_one(input)
}

pub struct Counter {
    pub value: i64,
}

impl Counter {
    pub fn bump(&mut self) {
        self.value = add_one(self.value);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add_one(1), 2);
    }
}
"#;

/// Resolve the REAL server binary. `rust-analyzer` on PATH may be a rustup
/// proxy that resolves the sample workspace's (absent) toolchain; `rustup
/// which` returns the concrete installed binary, so the live probe exercises
/// the actual server exactly as a configured absolute path would.
fn rust_analyzer_command() -> Option<String> {
    let available = std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !available {
        return None;
    }
    let resolved = std::process::Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty());
    Some(resolved.unwrap_or_else(|| "rust-analyzer".into()))
}

fn write_sample_workspace() -> (TempDir, PathBuf, PathBuf, String) {
    let dir = tempfile::tempdir().expect("temp workspace");
    let root = dir.path().join("lsp-live-sample");
    std::fs::create_dir_all(root.join("src")).expect("src dir");
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "lsp-live-sample"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .expect("Cargo.toml");
    let lib = root.join("src/lib.rs");
    std::fs::write(&lib, SAMPLE_LIB).expect("lib.rs");
    let uri = file_uri(&lib);
    (dir, root, lib, uri)
}

/// Zero-based line + UTF-16 character of the `occurrence`-th `needle`
/// (the sample is ASCII, so bytes == UTF-16 units).
fn position_of(text: &str, needle: &str, occurrence: usize) -> faktor_lsp::Position {
    let index = text
        .match_indices(needle)
        .nth(occurrence)
        .unwrap_or_else(|| panic!("needle {needle:?} occurrence {occurrence} not found"))
        .0;
    let prefix = &text[..index];
    let line = prefix.matches('\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |offset| offset + 1);
    faktor_lsp::Position::new(line, (index - line_start) as u32)
}

async fn start(
    root: &Path,
    command: String,
) -> (TempDir, LspManager, std::sync::Arc<faktor_lsp::LspClient>) {
    let cas_dir = tempfile::tempdir().expect("cas dir");
    let cas = Cas::open(cas_dir.path().join("cas")).expect("cas");
    let manager = LspManager::new(ProcessSupervisor::new(std::sync::Arc::new(cas)));
    let started = Instant::now();
    let client = tokio::time::timeout(
        Duration::from_secs(180),
        manager.start(
            WorkspaceId::new(77),
            LspConfig {
                name: "rust-analyzer".into(),
                command,
                args: Vec::new(),
                root: root.to_path_buf(),
            },
        ),
    )
    .await
    .expect("rust-analyzer initialize must finish within 180s")
    .expect("rust-analyzer must start");
    eprintln!("live: rust-analyzer initialized in {:?}", started.elapsed());
    (cas_dir, manager, client)
}

/// Wait for rust-analyzer's background indexing: a cold server legitimately
/// answers `null`/empty (or `ContentModified`) until the crate graph is
/// loaded. A read-only query is safe to reissue.
async fn poll_semantic<T, F, Fut>(
    what: &str,
    client: &faktor_lsp::LspClient,
    mut call: F,
    accept: impl Fn(&T) -> bool,
) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SemanticError>>,
{
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last_error = None;
    loop {
        match call().await {
            Ok(value) if accept(&value) => return value,
            Ok(_) => {}
            Err(error) => last_error = Some(error),
        }
        if Instant::now() >= deadline {
            panic!(
                "rust-analyzer never satisfied {what}: {last_error:?}\nstderr:\n{}",
                client.stderr_tail()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn poll_definition(
    client: &faktor_lsp::LspClient,
    uri: &str,
    position: faktor_lsp::Position,
) -> faktor_lsp::DefinitionResult {
    poll_semantic(
        "definition",
        client,
        || {
            client.definition_with(
                uri,
                position,
                SemanticOptions::new().with_timeout(Duration::from_secs(60)),
            )
        },
        |result| !result.items.is_empty(),
    )
    .await
}

async fn poll_workspace_symbol(
    client: &faktor_lsp::LspClient,
    name: &str,
) -> faktor_lsp::WorkspaceSymbols {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last_error: Option<SemanticError> = None;
    loop {
        match client
            .workspace_symbols_with(
                name,
                SemanticOptions::new().with_timeout(Duration::from_secs(60)),
            )
            .await
        {
            Ok(result) if result.items.iter().any(|symbol| symbol.name == name) => {
                return result;
            }
            Ok(_) => {}
            Err(error) => last_error = Some(error),
        }
        if Instant::now() >= deadline {
            panic!("rust-analyzer never indexed workspace symbol {name:?}: {last_error:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn hierarchy_item<'a>(items: &'a [CallHierarchyItem], name: &str) -> &'a CallHierarchyItem {
    items
        .iter()
        .find(|item| item.name == name)
        .unwrap_or_else(|| panic!("call hierarchy item {name:?} not found: {items:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_rust_analyzer_semantics() {
    let Some(command) = rust_analyzer_command() else {
        eprintln!("rust-analyzer not installed; skipping the live semantic probe");
        return;
    };
    let (_dir, root, _lib, uri) = write_sample_workspace();
    let (_cas_dir, manager, client) = start(&root, command).await;

    let caps = client.capabilities();
    eprintln!("live: capabilities = {caps:?}");
    assert!(caps.definition, "rust-analyzer must advertise definitions");
    assert!(caps.references, "rust-analyzer must advertise references");
    assert!(
        caps.workspace_symbol,
        "rust-analyzer must advertise workspace symbols"
    );
    assert!(
        caps.call_hierarchy,
        "rust-analyzer must advertise call hierarchy"
    );
    assert_eq!(caps.server_name.as_deref(), Some("rust-analyzer"));

    client.did_open(&uri, SAMPLE_LIB).expect("didOpen");
    assert!(client.document_revision(&uri).is_some());

    let add_one_decl = position_of(SAMPLE_LIB, "add_one", 0);
    let add_one_call = position_of(SAMPLE_LIB, "add_one", 1);
    let compute_decl = position_of(SAMPLE_LIB, "compute", 0);
    eprintln!(
        "live: add_one decl at {:?}, call at {:?}, compute decl at {:?}",
        add_one_decl, add_one_call, compute_decl
    );

    let definition = poll_definition(&client, &uri, add_one_call).await;
    assert_eq!(definition.items.len(), 1, "{definition:?}");
    let target = &definition.items[0];
    assert_eq!(target.uri, uri, "definition must land in the sample file");
    assert_eq!(
        target.selection_range.start.line, add_one_decl.line,
        "definition must select the add_one name: {target:?}"
    );
    eprintln!(
        "live: definition -> {}:{}",
        target.uri, target.selection_range.start.line
    );

    let cold_stats = client.semantic_cache_stats();
    let started = Instant::now();
    let cached = client
        .definition_with(
            &uri,
            add_one_call,
            SemanticOptions::new().with_timeout(Duration::from_secs(30)),
        )
        .await
        .expect("cached definition");
    let warm = started.elapsed();
    assert_eq!(cached, definition);
    let warm_stats = client.semantic_cache_stats();
    assert!(
        warm_stats.hits > cold_stats.hits,
        "the warm call must be a measured cache hit: {cold_stats:?} -> {warm_stats:?}"
    );
    eprintln!(
        "live: cached definition latency = {warm:?} (hits {} -> {})",
        cold_stats.hits, warm_stats.hits
    );
    assert!(
        warm < Duration::from_millis(100),
        "cache hit too slow: {warm:?}"
    );

    let with_decl = poll_semantic(
        "references(includeDeclaration=true)",
        &client,
        || {
            client.references_with(
                &uri,
                add_one_decl,
                true,
                SemanticOptions::new().with_timeout(Duration::from_secs(60)),
            )
        },
        |result| !result.items.is_empty(),
    )
    .await;
    assert!(
        with_decl.items.iter().any(|location| location.uri == uri
            && location.range.start.line <= add_one_decl.line
            && location.range.end.line >= add_one_decl.line),
        "includeDeclaration=true must include the declaration: {with_decl:?}"
    );
    let without_decl = poll_semantic(
        "references(includeDeclaration=false)",
        &client,
        || {
            client.references_with(
                &uri,
                add_one_decl,
                false,
                SemanticOptions::new().with_timeout(Duration::from_secs(60)),
            )
        },
        |result| !result.items.is_empty(),
    )
    .await;
    assert!(
        without_decl.items.len() < with_decl.items.len(),
        "includeDeclaration=false must exclude the declaration: {without_decl:?} vs {with_decl:?}"
    );
    assert!(
        without_decl
            .items
            .iter()
            .all(|location| location.uri != uri || location.range.start.line != add_one_decl.line),
        "includeDeclaration=false must not return the declaration: {without_decl:?}"
    );
    eprintln!(
        "live: references with declaration = {}, without = {}",
        with_decl.items.len(),
        without_decl.items.len()
    );

    let symbols = poll_workspace_symbol(&client, "Counter").await;
    let counter = symbols
        .items
        .iter()
        .find(|symbol| symbol.name == "Counter")
        .expect("Counter symbol");
    assert_eq!(counter.kind, SymbolKind::Struct, "{counter:?}");
    assert_eq!(counter.location.uri, uri);
    assert!(counter.location.range.is_some());
    eprintln!(
        "live: workspace symbol Counter -> {:?} {:?}",
        counter.kind, counter.location.range
    );

    let add_one_items = poll_semantic(
        "prepareCallHierarchy(add_one)",
        &client,
        || {
            client.prepare_call_hierarchy_with(
                &uri,
                add_one_decl,
                SemanticOptions::new().with_timeout(Duration::from_secs(60)),
            )
        },
        |result| result.items.iter().any(|item| item.name == "add_one"),
    )
    .await;
    let add_one_item = hierarchy_item(&add_one_items.items, "add_one");
    let incoming = poll_semantic(
        "incomingCalls(add_one)",
        &client,
        || {
            client.incoming_calls_with(
                add_one_item,
                SemanticOptions::new().with_timeout(Duration::from_secs(60)),
            )
        },
        |result| {
            result
                .items
                .iter()
                .any(|call| call.from.name.contains("compute"))
        },
    )
    .await;
    assert!(
        incoming
            .items
            .iter()
            .any(|call| call.from.name.contains("compute")),
        "add_one must have compute as an incoming caller: {incoming:?}"
    );
    eprintln!(
        "live: incoming calls of add_one = {:?}",
        incoming
            .items
            .iter()
            .map(|call| call.from.name.as_str())
            .collect::<Vec<_>>()
    );

    let compute_items = poll_semantic(
        "prepareCallHierarchy(compute)",
        &client,
        || {
            client.prepare_call_hierarchy_with(
                &uri,
                compute_decl,
                SemanticOptions::new().with_timeout(Duration::from_secs(60)),
            )
        },
        |result| result.items.iter().any(|item| item.name == "compute"),
    )
    .await;
    let compute_item = hierarchy_item(&compute_items.items, "compute");
    let outgoing = poll_semantic(
        "outgoingCalls(compute)",
        &client,
        || {
            client.outgoing_calls_with(
                compute_item,
                SemanticOptions::new().with_timeout(Duration::from_secs(60)),
            )
        },
        |result| result.items.iter().any(|call| call.to.name == "add_one"),
    )
    .await;
    assert!(
        outgoing.items.iter().any(|call| call.to.name == "add_one"),
        "compute must call add_one: {outgoing:?}"
    );
    eprintln!(
        "live: outgoing calls of compute = {:?}",
        outgoing
            .items
            .iter()
            .map(|call| call.to.name.as_str())
            .collect::<Vec<_>>()
    );

    manager
        .shutdown(WorkspaceId::new(77))
        .await
        .expect("shutdown");
}
