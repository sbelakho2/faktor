//! rust-analyzer/TypeScript LSP enrichment (feature `lsp`).
//!
//! The server is a supervised child (never `std::process::Command` here).
//! Enrichment is deliberately TARGETED: callers pass the changed files and
//! their small reverse dependency set, so a 10k-file graph build stays
//! linear; the LSP is used to confirm references, incoming calls and trait
//! implementations for the files that matter. Every LSP edge is tagged
//! [`Provenance::Lsp`] and upgrades the lexical edge with the same key.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_core::id::SessionId;
use faktor_terminal::{ProcessOwner, ProcessSupervisor, SpawnConfig};
use serde_json::{json, Value};

use crate::model::{
    file_id, symbol_id, Graph, GraphEdge, GraphError, GraphNode, NodeKind, Provenance,
};

const MAX_LSP_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct SymbolSlot {
    name: String,
    id: String,
    /// Lexical 1-based line of the existing node (or the LSP line + 1 for a
    /// node this enrichment will create).
    node_line: u32,
    lsp_line: u32,
    existing: bool,
}
type SymbolsByPath = BTreeMap<String, Vec<SymbolSlot>>;

#[derive(Debug, Clone)]
pub struct LspOptions {
    pub command: String,
    pub args: Vec<String>,
    pub timeout: Duration,
    /// Bounded settle after didOpen: rust-analyzer answers documentSymbol
    /// before name resolution/workspace loading completes.
    pub settle: Duration,
    pub max_files: usize,
    pub max_requests: usize,
    /// Total wall-clock budget for the enrichment pass. Once exhausted the
    /// pass stops and reports degradation; it never runs unbounded.
    pub max_duration: Duration,
}

impl Default for LspOptions {
    fn default() -> Self {
        Self {
            command: std::env::var("FAKTOR_RUST_ANALYZER")
                .unwrap_or_else(|_| "rust-analyzer".into()),
            args: Vec::new(),
            timeout: Duration::from_secs(30),
            settle: Duration::from_secs(8),
            max_files: 512,
            max_requests: 8192,
            max_duration: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LspStats {
    pub files_opened: usize,
    pub symbols: usize,
    pub references: usize,
    pub calls: usize,
    pub implementations: usize,
    pub requests: usize,
    pub degraded: bool,
    pub note: Option<String>,
}

struct LspSession {
    stdin: std::process::ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
    timeout: Duration,
    supervisor: Arc<ProcessSupervisor>,
    pid: u32,
    stderr: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl LspSession {
    fn stderr_tail(&self) -> String {
        let guard = self
            .stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&guard).trim().to_string()
    }
}

impl LspSession {
    fn start(
        root: &Path,
        options: &LspOptions,
        supervisor: Arc<ProcessSupervisor>,
    ) -> Result<Self, GraphError> {
        let spawned = supervisor
            .spawn_detached_with_pipes(SpawnConfig {
                cmd: options.command.clone(),
                args: options.args.clone(),
                cwd: root.to_path_buf(),
                env: faktor_terminal::EnvSpec::toolchain(),
                owner: ProcessOwner::Session(SessionId::new(1)),
                ..Default::default()
            })
            .map_err(|e| GraphError::Malformed(format!("lsp spawn: {e}")))?;
        let stdout = spawned.stdout;
        let mut stderr = spawned.stderr;
        let stderr_buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr_sink = stderr_buffer.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(size) = stderr.read(&mut chunk) {
                if size == 0 {
                    break;
                }
                let mut guard = stderr_sink
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let drain_to = guard
                    .len()
                    .saturating_add(size)
                    .saturating_sub(16 * 1024)
                    .min(guard.len());
                if drain_to > 0 {
                    guard.drain(..drain_to);
                }
                guard.extend_from_slice(&chunk[..size]);
            }
        });
        let (tx, rx) = channel();
        std::thread::spawn(move || read_frames(stdout, tx));
        Ok(Self {
            stdin: spawned.stdin,
            rx,
            next_id: 1,
            timeout: options.timeout,
            supervisor,
            pid: spawned.child_pid,
            stderr: stderr_buffer,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, GraphError> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(GraphError::Malformed(format!(
                    "lsp {method} exceeded {}ms",
                    self.timeout.as_millis()
                )));
            }
            match self
                .rx
                .recv_timeout(deadline.saturating_duration_since(now))
            {
                Ok(message) => {
                    if message.get("id").and_then(Value::as_u64) == Some(id)
                        && message.get("method").is_none()
                    {
                        if let Some(error) = message.get("error") {
                            return Err(GraphError::Malformed(format!("lsp {method}: {error}")));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    // Server -> client request: answer with null so the
                    // server never blocks waiting on an unsupported feature.
                    if let Some(server_id) = message.get("id") {
                        if message.get("method").is_some() {
                            let _ = write_frame(
                                &mut self.stdin,
                                &json!({"jsonrpc":"2.0","id":server_id,"result":Value::Null}),
                            );
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    let tail = self.stderr_tail();
                    return Err(GraphError::Malformed(format!(
                        "lsp server exited before {method}: {tail}"
                    )));
                }
            }
        }
    }

    /// Drain server notifications until rust-analyzer reports quiescence
    /// (`experimental/serverStatus.quiescent`) or the bounded settle window
    /// elapses. This is what distinguishes "still indexing" from "no
    /// references"; a fixed sleep either flakes or wastes time.
    fn wait_quiescent(&mut self, max: Duration) {
        let deadline = Instant::now() + max;
        let mut saw_end = false;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(message) => {
                    let method = message.get("method").and_then(Value::as_str);
                    if std::env::var_os("FAKTOR_IMPACT_DEBUG").is_some() {
                        eprintln!("impact lsp notification: {method:?}");
                    }
                    if method == Some("experimental/serverStatus")
                        && message
                            .pointer("/params/quiescent")
                            .and_then(Value::as_bool)
                            == Some(true)
                    {
                        return;
                    }
                    if method == Some("$/progress")
                        && message
                            .pointer("/params/value/kind")
                            .and_then(Value::as_str)
                            == Some("end")
                    {
                        saw_end = true;
                    }
                    if let Some(server_id) = message.get("id") {
                        if method.is_some() {
                            let _ = write_frame(
                                &mut self.stdin,
                                &json!({"jsonrpc":"2.0","id":server_id,"result":Value::Null}),
                            );
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if saw_end {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), GraphError> {
        write_frame(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","method":method,"params":params}),
        )
    }

    fn initialize(&mut self, root: &Path) -> Result<(), GraphError> {
        let uri = path_to_uri(root);
        self.request(
            "initialize",
            json!({
                "processId": Value::Null,
                "rootUri": uri,
                "workspaceFolders": [{"uri": uri, "name": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "workspace".into())}],
                "capabilities": {
                    "textDocument": {
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                        "references": {},
                        "callHierarchy": {},
                        "implementation": {}
                    }
                }
            }),
        )?;
        self.notify("initialized", json!({}))
    }

    fn did_open(&mut self, path: &Path, language: &str, text: &str) -> Result<(), GraphError> {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": path_to_uri(path), "languageId": language, "version": 1, "text": text}}),
        )
    }

    fn references(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, GraphError> {
        let result = self.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": path_to_uri(path)},
                "position": {"line": line, "character": character},
                "context": {"includeDeclaration": false}
            }),
        )?;
        Ok(parse_locations(&result))
    }

    fn implementations(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, GraphError> {
        let result = self.request(
            "textDocument/implementation",
            json!({
                "textDocument": {"uri": path_to_uri(path)},
                "position": {"line": line, "character": character}
            }),
        )?;
        Ok(parse_locations(&result))
    }

    fn prepare_call_hierarchy(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<Value>, GraphError> {
        let result = self.request(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument": {"uri": path_to_uri(path)},
                "position": {"line": line, "character": character}
            }),
        )?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    fn incoming_calls(&mut self, item: &Value) -> Result<Vec<Value>, GraphError> {
        let result = self.request("callHierarchy/incomingCalls", json!({"item": item}))?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    fn document_symbols(&mut self, path: &Path) -> Result<Vec<LspSymbol>, GraphError> {
        let result = self.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": path_to_uri(path)}}),
        )?;
        let mut out = Vec::new();
        if let Some(items) = result.as_array() {
            for item in items {
                parse_document_symbol(item, &mut out, 0);
            }
        }
        Ok(out)
    }
}

impl Drop for LspSession {
    fn drop(&mut self) {
        let _ = self.supervisor.kill_child_pid(self.pid, 500);
    }
}

fn write_frame(stdin: &mut std::process::ChildStdin, value: &Value) -> Result<(), GraphError> {
    let body = value.to_string();
    if body.len() > MAX_LSP_FRAME_BYTES {
        return Err(GraphError::Oversized {
            what: "lsp frame",
            max: MAX_LSP_FRAME_BYTES,
            actual: body.len(),
        });
    }
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n{}", body.len(), body).as_bytes())
        .and_then(|_| stdin.flush())
        .map_err(|e| GraphError::Malformed(format!("lsp write: {e}")))
}

fn read_frames(mut stdout: std::process::ChildStdout, tx: std::sync::mpsc::Sender<Value>) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > MAX_LSP_FRAME_BYTES {
            break;
        }
        while let Some(header_end) = find(&buffer, b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
            let mut length = None;
            for line in header.split("\r\n") {
                if let Some(value) = line.strip_prefix("Content-Length:") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
            let Some(length) = length else {
                buffer.drain(..header_end + 4);
                continue;
            };
            if buffer.len() < header_end + 4 + length {
                break;
            }
            let body = buffer[header_end + 4..header_end + 4 + length].to_vec();
            buffer.drain(..header_end + 4 + length);
            if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                if tx.send(value).is_err() {
                    return;
                }
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Debug, Clone)]
struct LspLocation {
    uri: String,
    line: u32,
}

#[derive(Debug, Clone)]
struct LspSymbol {
    name: String,
    kind: u32,
    line: u32,
    character: u32,
}

fn parse_document_symbol(value: &Value, out: &mut Vec<LspSymbol>, depth: usize) {
    if depth > 64 {
        return;
    }
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return;
    }
    let kind = value.get("kind").and_then(Value::as_u64).unwrap_or(0) as u32;
    let range = value.get("range").or_else(|| {
        value
            .get("location")
            .and_then(|location| location.get("range"))
    });
    let selection = value.get("selectionRange").or(range);
    let line = selection
        .and_then(|range| range.get("start"))
        .and_then(|start| start.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let character = selection
        .and_then(|range| range.get("start"))
        .and_then(|start| start.get("character"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    out.push(LspSymbol {
        name,
        kind,
        line,
        character,
    });
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            parse_document_symbol(child, out, depth + 1);
        }
    }
}

fn parse_locations(value: &Value) -> Vec<LspLocation> {
    let mut out = Vec::new();
    let Some(items) = value.as_array() else {
        return out;
    };
    for item in items {
        let uri = item
            .get("uri")
            .or_else(|| item.get("targetUri"))
            .and_then(Value::as_str);
        let range = item
            .get("range")
            .or_else(|| item.get("targetSelectionRange"))
            .or_else(|| item.get("targetRange"));
        if let (Some(uri), Some(range)) = (uri, range) {
            out.push(LspLocation {
                uri: uri.to_string(),
                line: range
                    .get("start")
                    .and_then(|start| start.get("line"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
            });
        }
    }
    out
}

/// Enrich `graph` with rust-analyzer relationships for `paths` (workspace
/// relative). The lexical graph is the fallback for every request that fails.
pub fn enrich_graph(
    graph: &mut Graph,
    root: &Path,
    paths: &[String],
    options: &LspOptions,
    supervisor: &Arc<ProcessSupervisor>,
) -> Result<LspStats, GraphError> {
    let mut stats = LspStats::default();
    let selected: Vec<String> = paths
        .iter()
        .filter(|path| path.ends_with(".rs"))
        .take(options.max_files)
        .cloned()
        .collect();
    if selected.is_empty() {
        return Ok(stats);
    }
    let started = Instant::now();
    let expired = || started.elapsed() > options.max_duration;
    let mut session = match LspSession::start(root, options, supervisor.clone()) {
        Ok(session) => session,
        Err(error) => {
            stats.degraded = true;
            stats.note = Some(error.to_string());
            return Ok(stats);
        }
    };
    if let Err(error) = session.initialize(root) {
        stats.degraded = true;
        stats.note = Some(error.to_string());
        return Ok(stats);
    }
    stats.requests = session.next_id.saturating_sub(1) as usize;

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let existing_paths: BTreeSet<String> =
        graph.files.iter().map(|file| file.path.clone()).collect();
    let mut symbols_by_path: SymbolsByPath = BTreeMap::new();
    let mut file_symbols: BTreeMap<String, Vec<LspSymbol>> = BTreeMap::new();

    for relative in &selected {
        if expired() {
            stats.degraded = true;
            break;
        }
        let absolute = root.join(relative);
        let Ok(bytes) = std::fs::read(&absolute) else {
            continue;
        };
        if bytes.len() > 4 * 1024 * 1024 {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if !existing_paths.contains(relative) {
            continue;
        }
        if session.did_open(&absolute, "rust", text).is_err() {
            stats.degraded = true;
            continue;
        }
        stats.files_opened += 1;
        if stats.requests >= options.max_requests {
            stats.degraded = true;
            break;
        }
        let Ok(symbols) = session.document_symbols(&absolute) else {
            stats.degraded = true;
            continue;
        };
        stats.requests += 1;
        stats.symbols += symbols.len();
        symbols_by_path.insert(relative.clone(), symbol_slots(graph, relative, &symbols));
        file_symbols.insert(relative.clone(), symbols);
    }

    // rust-analyzer answers documentSymbol before the crate graph finished
    // loading; references/call hierarchy are empty until the workspace is
    // quiescent. A short bounded settle avoids treating "still loading" as
    // "no relationships".
    if stats.files_opened > 0 {
        session.wait_quiescent(options.settle);
    }

    // Add LSP-discovered symbols (methods, fields, associated items) that the
    // lexical parser did not emit, and remember their ids for edge owners.
    for (relative, symbols) in &file_symbols {
        let slots = symbols_by_path.get(relative).cloned().unwrap_or_default();
        for symbol in symbols {
            if !is_callable(symbol.kind) {
                continue;
            }
            let found = slots
                .iter()
                .find(|slot| slot.name == symbol.name && slot.node_line == symbol.line + 1);
            if found.is_none_or(|slot| !slot.existing) {
                let id = found
                    .map(|slot| slot.id.clone())
                    .unwrap_or_else(|| symbol_id(relative, &symbol.name, symbol.line + 1));
                nodes.push(GraphNode {
                    id: id.clone(),
                    kind: NodeKind::Symbol,
                    name: symbol.name.clone(),
                    path: Some(relative.clone()),
                    crate_name: graph
                        .files
                        .iter()
                        .find(|file| &file.path == relative)
                        .map(|file| file.crate_name.clone()),
                    line: Some(symbol.line + 1),
                    provenance: Provenance::Lsp,
                });
                edges.push(GraphEdge {
                    from: file_id(relative),
                    to: id,
                    kind: crate::model::EdgeKind::Defines,
                    provenance: Provenance::Lsp,
                    detail: Some("lsp-symbol".into()),
                });
            }
        }
    }

    for (relative, symbols) in &file_symbols {
        if stats.requests >= options.max_requests || expired() {
            stats.degraded = true;
            break;
        }
        let slots = symbols_by_path.get(relative).cloned().unwrap_or_default();
        for symbol in symbols {
            if expired() {
                stats.degraded = true;
                break;
            }
            if !is_callable(symbol.kind) {
                continue;
            }
            let Some(slot) = slots
                .iter()
                .find(|slot| slot.name == symbol.name && slot.node_line == symbol.line + 1)
            else {
                continue;
            };
            let target_id = &slot.id;
            let absolute = root.join(relative);
            if std::env::var_os("FAKTOR_IMPACT_DEBUG").is_some() {
                eprintln!(
                    "impact lsp: {} {} line={} char={}",
                    relative, symbol.name, symbol.line, symbol.character
                );
            }
            if let Ok(locations) = session.references(&absolute, symbol.line, symbol.character) {
                stats.requests += 1;
                for location in locations {
                    let Some(path) = uri_to_relative(&location.uri, root) else {
                        continue;
                    };
                    if !existing_paths.contains(&path) {
                        continue;
                    }
                    let owner = owner_at(&symbols_by_path, &path, location.line)
                        .unwrap_or_else(|| file_id(&path));
                    if owner == *target_id {
                        continue;
                    }
                    edges.push(GraphEdge {
                        from: owner,
                        to: target_id.clone(),
                        kind: crate::model::EdgeKind::References,
                        provenance: Provenance::Lsp,
                        detail: Some(symbol.name.clone()),
                    });
                    stats.references += 1;
                }
            } else {
                stats.degraded = true;
            }
            if stats.requests >= options.max_requests {
                stats.degraded = true;
                break;
            }
            if let Ok(items) =
                session.prepare_call_hierarchy(&absolute, symbol.line, symbol.character)
            {
                stats.requests += 1;
                for item in items {
                    if stats.requests >= options.max_requests {
                        stats.degraded = true;
                        break;
                    }
                    let Ok(incoming) = session.incoming_calls(&item) else {
                        stats.degraded = true;
                        continue;
                    };
                    stats.requests += 1;
                    for call in incoming {
                        let Some(from) = call.get("from") else {
                            continue;
                        };
                        let Some(uri) = from.get("uri").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(path) = uri_to_relative(uri, root) else {
                            continue;
                        };
                        if !existing_paths.contains(&path) {
                            continue;
                        }
                        let line = from
                            .get("selectionRange")
                            .or_else(|| from.get("range"))
                            .and_then(|range| range.get("start"))
                            .and_then(|start| start.get("line"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32;
                        let name = from.get("name").and_then(Value::as_str).unwrap_or("");
                        let caller = owner_at_named(&symbols_by_path, &path, line, name)
                            .unwrap_or_else(|| file_id(&path));
                        if caller == *target_id {
                            continue;
                        }
                        edges.push(GraphEdge {
                            from: caller,
                            to: target_id.clone(),
                            kind: crate::model::EdgeKind::Calls,
                            provenance: Provenance::Lsp,
                            detail: Some(symbol.name.clone()),
                        });
                        stats.calls += 1;
                    }
                }
            }
            if symbol.kind == 11 || symbol.kind == 5 {
                // Interface (trait) or class: implementations.
                if let Ok(locations) =
                    session.implementations(&absolute, symbol.line, symbol.character)
                {
                    stats.requests += 1;
                    for location in locations {
                        let Some(path) = uri_to_relative(&location.uri, root) else {
                            continue;
                        };
                        if !existing_paths.contains(&path) {
                            continue;
                        }
                        let implementor = owner_at(&symbols_by_path, &path, location.line)
                            .unwrap_or_else(|| file_id(&path));
                        edges.push(GraphEdge {
                            from: implementor,
                            to: target_id.clone(),
                            kind: crate::model::EdgeKind::Implements,
                            provenance: Provenance::Lsp,
                            detail: Some(symbol.name.clone()),
                        });
                        stats.implementations += 1;
                    }
                }
            }
        }
    }
    stats.requests = stats
        .requests
        .max(session.next_id.saturating_sub(1) as usize);
    graph.merge_enrichment(nodes, edges);
    Ok(stats)
}

fn symbol_slots(graph: &Graph, relative: &str, symbols: &[LspSymbol]) -> Vec<SymbolSlot> {
    let mut out = Vec::new();
    for symbol in symbols {
        if !is_callable(symbol.kind) {
            continue;
        }
        let line = symbol.line + 1;
        let existing = graph
            .nodes
            .iter()
            .find(|node| {
                node.kind == NodeKind::Symbol
                    && node.name == symbol.name
                    && node.path.as_deref() == Some(relative)
                    && node.line == Some(line)
            })
            .map(|node| node.id.clone());
        let id = existing
            .clone()
            .unwrap_or_else(|| symbol_id(relative, &symbol.name, line));
        out.push(SymbolSlot {
            name: symbol.name.clone(),
            id,
            node_line: line,
            lsp_line: symbol.line,
            existing: existing.is_some(),
        });
    }
    out
}

fn is_callable(kind: u32) -> bool {
    matches!(kind, 5 | 6 | 9 | 12)
}

fn owner_at(symbols: &SymbolsByPath, path: &str, line: u32) -> Option<String> {
    let slots = symbols.get(path)?;
    slots
        .iter()
        .filter(|slot| slot.lsp_line <= line && slot.node_line > 0)
        .max_by_key(|slot| slot.lsp_line)
        .map(|slot| slot.id.clone())
}

fn owner_at_named(symbols: &SymbolsByPath, path: &str, line: u32, name: &str) -> Option<String> {
    let slots = symbols.get(path)?;
    if name.is_empty() {
        return owner_at(symbols, path, line);
    }
    slots
        .iter()
        .find(|slot| slot.name == name && slot.lsp_line <= line)
        .or_else(|| slots.iter().find(|slot| slot.name == name))
        .map(|slot| slot.id.clone())
}

fn path_to_uri(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let text = absolute.to_string_lossy().replace('\\', "/");
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b':' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("file://{encoded}")
}

fn uri_to_relative(uri: &str, root: &Path) -> Option<String> {
    let raw = uri.strip_prefix("file://")?;
    let mut decoded = Vec::new();
    let bytes = raw.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let path = PathBuf::from(String::from_utf8(decoded).ok()?);
    let canonical = path.canonicalize().ok()?;
    let root = root.canonicalize().ok()?;
    canonical
        .strip_prefix(root)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}
