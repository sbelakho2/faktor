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
/// After the last observed progress/status activity, wait this long for the
/// server to emit another signal before declaring the workspace settled.
/// rust-analyzer emits `experimental/serverStatus` when the client opts in;
/// this quiet window is the bounded fallback for servers that only stream
/// `$/progress`.
const QUIESCENCE_QUIET_WINDOW: Duration = Duration::from_secs(5);
/// Bounded retries for LSP `ContentModified` (-32801) and `ServerCancelled`
/// (-32802) responses while rust-analyzer applies didOpen/didChange or
/// finishes loading. The whole request still obeys `LspOptions::timeout`.
const TRANSIENT_RETRY_LIMIT: u32 = 50;
/// Delay between transient LSP retries.
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(200);
/// Per-probe bound: one reference query must not consume the whole
/// readiness window while the server is still loading.
const READINESS_PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Budget reserved after readiness for the actual references / call
/// hierarchy / implementations queries.
const QUERY_PHASE_RESERVE: Duration = Duration::from_secs(90);

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
    /// Bounded wait for rust-analyzer workspace loading/indexing to become
    /// quiescent after didOpen. The server reports `experimental/serverStatus`
    /// and `$/progress`; when those signals show active work the wait is
    /// extended adaptively up to `max_duration`. `settle` is therefore the
    /// QUIET/settle window, not a fixed sleep.
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
            timeout: Duration::from_secs(60),
            settle: Duration::from_secs(8),
            max_files: 512,
            max_requests: 8192,
            // Documented whole-pass bound, including readiness. A cold,
            // large workspace (this repository: ~1k Rust files/88 crates)
            // needs several minutes before rust-analyzer answers references;
            // the functional probe returns as soon as one is queryable, and
            // the query phase gets at least `max_duration / 4` (>= 90s).
            max_duration: Duration::from_secs(600),
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
    /// Requests issued by the actual relationship queries (never the
    /// readiness probes, which have their own bound and counter).
    pub requests: usize,
    /// Reference probes used to establish functional readiness.
    pub readiness_probes: usize,
    /// Wall-clock spent waiting for the first queryable reference.
    pub readiness_wait_ms: u64,
    /// True when the server explicitly reported (or progress settled to)
    /// quiescence during readiness.
    pub quiescent: bool,
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
    /// In-flight `$/progress` tokens (work-done progress). RA's indexing
    /// passes are begin/end-delimited; quiescence requires this set empty.
    progress_tokens: BTreeSet<String>,
    /// True once ANY progress/status signal has been observed, so an empty
    /// signal set after a quiet window is meaningful (and a server that
    /// never opts into progress is not mistaken for a settled one).
    saw_quiescence_signal: bool,
    /// Last `experimental/serverStatus.quiescent` value, when the server
    /// reports one. `Some(true)` means workspace loading/indexing finished.
    server_quiescent: Option<bool>,
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
            progress_tokens: BTreeSet::new(),
            saw_quiescence_signal: false,
            server_quiescent: None,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, GraphError> {
        self.request_bounded(method, params, self.timeout)
    }

    /// One request bounded by `timeout` (the whole transient-retry sequence
    /// for this call, not each attempt).
    fn request_bounded(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, GraphError> {
        let deadline = Instant::now() + timeout;
        let mut retries = 0u32;
        loop {
            let id = self.next_id;
            self.next_id += 1;
            write_frame(
                &mut self.stdin,
                &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            )?;
            match self.await_response(method, id, deadline, timeout) {
                Ok(value) => return Ok(value),
                Err(RequestOutcome::Transient(error)) => {
                    // rust-analyzer reports ContentModified while it applies
                    // our didOpen/didChange or finishes workspace loading.
                    // Retrying is required for a cold large repository; the
                    // request deadline still bounds the whole sequence.
                    if retries >= TRANSIENT_RETRY_LIMIT {
                        return Err(GraphError::Malformed(format!(
                            "lsp {method}: transient {error} persisted after {retries} retries"
                        )));
                    }
                    retries += 1;
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(GraphError::Malformed(format!(
                            "lsp {method} exceeded {}ms while retrying {error}",
                            timeout.as_millis()
                        )));
                    }
                    std::thread::sleep(
                        TRANSIENT_RETRY_DELAY.min(deadline.saturating_duration_since(now)),
                    );
                }
                Err(RequestOutcome::Fatal(error)) => {
                    return Err(GraphError::Malformed(format!("lsp {method}: {error}")));
                }
                Err(RequestOutcome::Dead(tail)) => {
                    return Err(GraphError::Malformed(format!(
                        "lsp server exited before {method}: {tail}"
                    )));
                }
            }
        }
    }

    /// Wait for the response with `id`, answering server->client requests so
    /// the language server never blocks on an unsupported feature.
    fn await_response(
        &mut self,
        _method: &str,
        id: u64,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Value, RequestOutcome> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(RequestOutcome::Fatal(format!(
                    "exceeded {}ms",
                    timeout.as_millis()
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
                            if is_transient_lsp_error(error) {
                                return Err(RequestOutcome::Transient(error.to_string()));
                            }
                            return Err(RequestOutcome::Fatal(error.to_string()));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    if message.get("id").is_some() && message.get("method").is_some() {
                        let _ = write_frame(
                            &mut self.stdin,
                            &json!({
                                "jsonrpc":"2.0",
                                "id":message.get("id").cloned().unwrap_or(Value::Null),
                                "result":Value::Null
                            }),
                        );
                    } else if message.get("method").and_then(Value::as_str)
                        == Some("experimental/serverStatus")
                    {
                        self.observe_server_status(&message);
                    } else if message.get("method").and_then(Value::as_str) == Some("$/progress") {
                        self.observe_progress(&message);
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(RequestOutcome::Dead(self.stderr_tail()));
                }
            }
        }
    }

    fn observe_server_status(&mut self, message: &Value) {
        if let Some(quiescent) = message
            .pointer("/params/quiescent")
            .and_then(Value::as_bool)
        {
            self.saw_quiescence_signal = true;
            self.server_quiescent = Some(quiescent);
        }
    }

    fn observe_progress(&mut self, message: &Value) {
        let Some(kind) = message
            .pointer("/params/value/kind")
            .and_then(Value::as_str)
        else {
            return;
        };
        let token = message
            .pointer("/params/token")
            .map(|token| match token {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        self.saw_quiescence_signal = true;
        match kind {
            "begin" => {
                self.server_quiescent = Some(false);
                self.progress_tokens.insert(token);
            }
            "end" => {
                self.progress_tokens.remove(&token);
            }
            "report" => {
                self.server_quiescent = Some(false);
            }
            _ => {}
        }
    }

    /// Drain server notifications until rust-analyzer reports quiescence.
    ///
    /// The server tells us the truth in `experimental/serverStatus`
    /// (`quiescent: true` only after loading/indexing settled). When a
    /// server only streams `$/progress`, the wait continues while any
    /// work-done token is open and ends after [`QUIESCENCE_QUIET_WINDOW`] of
    /// silence once every token closed. `settle` bounds the wait BEFORE the
    /// first signal (a server that never opts in cannot stall us); once
    /// activity is observed the wait may extend to `overall_deadline`
    /// (`max_duration` in [`enrich_graph`]) and never beyond it.
    ///
    /// Returns `(waited, observed_signal, quiescent)`.
    fn wait_quiescent(
        &mut self,
        settle: Duration,
        overall_deadline: Instant,
    ) -> (Duration, bool, bool) {
        let started = Instant::now();
        let mut deadline = (started + settle).min(overall_deadline);
        let mut quiet_since: Option<Instant> = None;
        let mut quiescent = false;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(250)))
            {
                Ok(message) => {
                    let method = message.get("method").and_then(Value::as_str);
                    if std::env::var_os("FAKTOR_IMPACT_DEBUG").is_some() {
                        eprintln!(
                            "impact lsp notification: {method:?} {}",
                            message
                                .pointer("/params/value/title")
                                .or_else(|| message.pointer("/params/value/kind"))
                                .map(|value| value.to_string())
                                .unwrap_or_default()
                        );
                    }
                    if method == Some("experimental/serverStatus") {
                        self.observe_server_status(&message);
                        quiet_since = None;
                        deadline = overall_deadline;
                        if message
                            .pointer("/params/quiescent")
                            .and_then(Value::as_bool)
                            == Some(true)
                        {
                            quiescent = true;
                            break;
                        }
                    } else if method == Some("$/progress") {
                        self.observe_progress(&message);
                        quiet_since = None;
                        deadline = overall_deadline;
                    } else if let Some(server_id) = message.get("id") {
                        if method.is_some() {
                            let _ = write_frame(
                                &mut self.stdin,
                                &json!({"jsonrpc":"2.0","id":server_id,"result":Value::Null}),
                            );
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.saw_quiescence_signal && self.progress_tokens.is_empty() {
                        match quiet_since {
                            None => quiet_since = Some(now),
                            Some(since) if now.duration_since(since) >= QUIESCENCE_QUIET_WINDOW => {
                                quiescent = true;
                                break;
                            }
                            Some(_) => {}
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        (started.elapsed(), self.saw_quiescence_signal, quiescent)
    }

    fn active_progress(&self) -> usize {
        self.progress_tokens.len()
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
                    // Opt in to the server's own readiness signals: without
                    // these rust-analyzer sends NEITHER `$/progress` nor
                    // `experimental/serverStatus`, and a cold large workspace
                    // looks exactly like "no references" after any fixed sleep.
                    "window": {"workDoneProgress": true},
                    "workspace": {"configuration": true},
                    "experimental": {"serverStatusNotification": true},
                    "textDocument": {
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                        "references": {},
                        "callHierarchy": {},
                        "implementation": {},
                        "definition": {}
                    }
                },
                // Impact enrichment needs stable source semantics, not
                // macro-expanded diagnostics: skipping build-script/proc-macro
                // compilation keeps a large workspace's cold start bounded
                // (rust-analyzer otherwise spends minutes in "Building
                // compile-time-deps" before ANY reference is answerable).
                // Lexical edges still cover generated/macro code; these LSP
                // edges are the precision add-on for real source items.
                "initializationOptions": {
                    "cargo": {"buildScripts": {"enable": false}},
                    "procMacro": {"enable": false},
                    "checkOnSave": false
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
        self.references_bounded(path, line, character, self.timeout)
    }

    fn references_bounded(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
        timeout: Duration,
    ) -> Result<Vec<LspLocation>, GraphError> {
        let result = self.request_bounded(
            "textDocument/references",
            json!({
                "textDocument": {"uri": path_to_uri(path)},
                "position": {"line": line, "character": character},
                "context": {"includeDeclaration": false}
            }),
            timeout,
        )?;
        Ok(parse_locations(&result))
    }

    /// Functional readiness probe: a language server is semantically ready
    /// for `probes` exactly when a reference query at a real declaration
    /// position returns at least one caller. Unlike a workspace-wide
    /// quiescence wait (rust-analyzer can keep building/analysing unrelated
    /// crates for many minutes on a large repository), this is the evidence
    /// the caller actually needs, so the pass proceeds as soon as it exists.
    ///
    /// Bounded by `readiness_deadline`; stops early when the server reports
    /// `quiescent: true` (then empty references are the truthful answer, not
    /// "still loading"). Returns `(waited, requests, ready)`.
    fn wait_functionally_ready(
        &mut self,
        probes: &[ReadinessProbe],
        readiness_deadline: Instant,
        probe_timeout: Duration,
    ) -> (Duration, usize, bool) {
        let started = Instant::now();
        let mut requests = 0usize;
        if probes.is_empty() {
            return (Duration::ZERO, requests, false);
        }
        let mut cursor = 0usize;
        let mut interval = Duration::from_millis(500);
        loop {
            let probe = &probes[cursor % probes.len()];
            cursor += 1;
            if Instant::now() >= readiness_deadline {
                return (started.elapsed(), requests, false);
            }
            requests += 1;
            if let Ok(locations) = self.references_bounded(
                &probe.path,
                probe.line,
                probe.character,
                probe_timeout.min(readiness_deadline.saturating_duration_since(Instant::now())),
            ) {
                if !locations.is_empty() {
                    if std::env::var_os("FAKTOR_IMPACT_DEBUG").is_some() {
                        eprintln!(
                            "impact lsp: readiness probe {}:{} returned {} references after {}ms",
                            probe.name,
                            probe.line + 1,
                            locations.len(),
                            started.elapsed().as_millis()
                        );
                    }
                    return (started.elapsed(), requests, true);
                }
            }
            // A server that already reported quiescence has finished
            // analysis: retrying the same probes cannot discover more.
            if self.server_quiescent == Some(true) {
                return (started.elapsed(), requests, false);
            }
            let now = Instant::now();
            if now >= readiness_deadline {
                return (started.elapsed(), requests, false);
            }
            std::thread::sleep(interval.min(readiness_deadline.saturating_duration_since(now)));
            interval = (interval * 2).min(Duration::from_secs(5));
        }
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

/// Outcome of waiting for one LSP response.
enum RequestOutcome {
    /// The server reported a retryable transient state (`ContentModified`,
    /// `ServerCancelled`) while it loads or applies edits.
    Transient(String),
    /// A terminal protocol/server error.
    Fatal(String),
    /// The server process exited / closed the frame channel.
    Dead(String),
}

/// LSP error codes that mean "not ready yet, retry": `ContentModified`
/// (-32801) and `ServerCancelled` (-32802).
fn is_transient_lsp_error(error: &Value) -> bool {
    let code = error.get("code").and_then(Value::as_i64);
    if matches!(code, Some(-32801) | Some(-32802)) {
        return true;
    }
    error
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            message.contains("content modified")
                || message.contains("server cancelled")
                || message.contains("server canceled")
        })
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
struct ReadinessProbe {
    path: PathBuf,
    line: u32,
    character: u32,
    name: String,
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
    // loading; references/call hierarchy are empty until semantic analysis
    // is ready. Waiting for the workspace to become globally QUIESCENT is
    // wrong on a large repository: rust-analyzer may keep building and
    // analysing unrelated crates for many minutes after the changed files
    // are already answerable. The functional readiness probe is the truth
    // the caller needs: proceed as soon as a real declaration returns real
    // callers. The probe drains `$/progress` and `experimental/serverStatus`
    // notifications and stops early on genuine quiescence; the whole wait
    // is bounded and reserves budget for the actual query phase.
    if stats.files_opened > 0 {
        let overall_deadline = started + options.max_duration;
        let query_reserve = options
            .max_duration
            .checked_div(4)
            .unwrap_or(Duration::ZERO)
            .max(QUERY_PHASE_RESERVE);
        let readiness_deadline = overall_deadline
            .checked_sub(query_reserve)
            .unwrap_or(overall_deadline);
        let probes = readiness_probes(root, &file_symbols);
        let (waited, probe_requests, ready) =
            session.wait_functionally_ready(&probes, readiness_deadline, READINESS_PROBE_TIMEOUT);
        stats.readiness_probes = probe_requests;
        stats.readiness_wait_ms = waited.as_millis().min(u64::MAX as u128) as u64;
        stats.quiescent = session.server_quiescent == Some(true);
        if std::env::var_os("FAKTOR_IMPACT_DEBUG").is_some() {
            eprintln!(
                "impact lsp: readiness waited={}ms requests={probe_requests} ready={ready} server_quiescent={:?} active_progress={} settle={}ms",
                waited.as_millis(),
                session.server_quiescent,
                session.active_progress(),
                options.settle.as_millis()
            );
        }
        if !ready && session.server_quiescent.is_none() {
            // Keep the signal wait as a bounded fallback for servers that
            // only stream progress (no serverStatus capability).
            let (extra, observed, _) = session.wait_quiescent(
                options.settle.min(Duration::from_secs(10)),
                overall_deadline,
            );
            if std::env::var_os("FAKTOR_IMPACT_DEBUG").is_some() {
                eprintln!(
                    "impact lsp: fallback quiescence waited={}ms observed={observed}",
                    extra.as_millis()
                );
            }
        }
        if !ready {
            stats.note = Some(format!(
                "no reference became queryable within the readiness window ({}ms); continuing with a bounded query budget",
                waited.as_millis()
            ));
        }
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
    graph.merge_enrichment(nodes, edges);
    Ok(stats)
}

/// Candidate declaration positions for the functional readiness probe.
/// Prefer real (non-test) callables; a test-only file still contributes its
/// callables as a last resort. Bounded so a generated file cannot flood the
/// probe with thousands of requests.
fn readiness_probes(
    root: &Path,
    file_symbols: &BTreeMap<String, Vec<LspSymbol>>,
) -> Vec<ReadinessProbe> {
    const MAX_PROBES: usize = 8;
    let mut primary = Vec::new();
    let mut fallback = Vec::new();
    for (relative, symbols) in file_symbols {
        let absolute = root.join(relative);
        for symbol in symbols {
            if !is_callable(symbol.kind) || symbol.name.is_empty() {
                continue;
            }
            let probe = ReadinessProbe {
                path: absolute.clone(),
                line: symbol.line,
                character: symbol.character,
                name: symbol.name.clone(),
            };
            if looks_like_test(&symbol.name) {
                fallback.push(probe);
            } else {
                primary.push(probe);
            }
        }
    }
    primary.extend(fallback);
    primary.truncate(MAX_PROBES);
    primary
}

fn looks_like_test(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("test_")
        || lower.ends_with("_test")
        || lower.ends_with("_tests")
        || lower.contains("test_")
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
