//! faktor-lsp — workspace-scoped language server integration (spec §32).
//!
//! Language servers are workspace resources, not session resources: one
//! daemon shares `rust-analyzer`/`typescript-language-server`/`pyright`
//! across sessions on the same workspace. Requests are multiplexed with
//! per-request ids; heavy servers are unloaded after workspace inactivity.
//!
//! # LSP protocol correctness
//!
//! - The server process is owned by the REAL `WorkspaceId` of the workspace
//!   it serves — never a placeholder id — and runs with its working
//!   directory at the workspace root, which is also what `initialize`
//!   reports as `rootUri`.
//! - JSON-RPC requests and notifications are distinct paths: a request
//!   carries an id and waits for its response; a notification is
//!   fire-and-forget with no id and no pending entry, so a server that
//!   (correctly) never answers a notification cannot stall the client.
//! - The `initialized` notification is sent right after the `initialize`
//!   RESULT and before any `didOpen`. Lifecycle shutdown is a request with
//!   an awaited response, followed by the `exit` notification, followed by a
//!   bounded grace wait for the process; the kill is the fallback, never
//!   the first move.
//! - Stderr is drained continuously into a bounded byte ring (recent tail
//!   kept for diagnostics; total bytes counted unboundedly) so a verbose
//!   server can never block itself on a full stderr pipe.

use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use faktor_core::error::{Error, ErrorKind};
use faktor_core::id::WorkspaceId;
use faktor_terminal::{EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};

/// Re-exported delivery-unknown contract (owned by `faktor-mcp`, the framing/
/// writer sibling): a timed-out frame whose write had already begun can never
/// be reported as a clean, provably-not-delivered Timeout.
pub use faktor_mcp::{
    delivery_unknown, is_delivery_unknown, DeliveryUnknown, FrameGuard, FrameState,
};

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Bounded everything: at most this many requests may be awaiting responses.
const MAX_INFLIGHT_REQUESTS: usize = 1024;
/// Stderr diagnostics ring: bytes beyond the cap are dropped from the head.
const STDERR_RING_CAP: usize = 64 * 1024;
/// Deadlines of the graceful lifecycle (request → exit notification → wait).
const SHUTDOWN_REQUEST_MS: u64 = 5_000;
const EXIT_GRACE_MS: u64 = 2_000;

/// Classified lock recovery for the workspace-scoped LSP state: `conn`
/// (in-flight request map + child identity) and `clients`/`last_used` are
/// OWNERSHIP projections. A poisoned guard is recovered with the poison flag
/// cleared and RECONCILED against the durable process authority (the
/// supervisor's owner rows; the child pid was captured at spawn), so one
/// panicking caller can never orphan a server or wedge later calls.
fn recover_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// The REAL workspace root: the server's working directory AND the
    /// `rootUri` reported in `initialize` (file:// URI of this path).
    pub root: PathBuf,
}

/// Shared connection state; guarded so the reader thread, drain threads and
/// async request/notify paths never interleave a write. The stdin is owned
/// by the dedicated writer thread (see [`WriterHandle`]), NEVER written
/// while this mutex is held: the reader thread needs the mutex, so a
/// blocking write under it could deadlock the whole client.
struct LspConn {
    child_pid: u32,
    next_id: u64,
    pending: HashMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>,
}

/// Bound on the dedicated stdin writer's queue: a server that stops draining
/// must never grow client memory or block a caller unboundedly.
const WRITER_QUEUE_CAP: usize = 64;

/// Documented bound on the synchronous notification write+flush path; a
/// wedged server turns into a typed timeout instead of an unbounded block.
const NOTIFY_WRITE_TIMEOUT_MS: u64 = 5_000;

/// One queued stdin frame plus its completion channel.
enum WriterAck {
    /// Awaited by the async request path under a deadline.
    Async(tokio::sync::oneshot::Sender<std::io::Result<()>>),
    /// Awaited by the synchronous notification path with a bounded
    /// `recv_timeout` (never under the `conn` mutex).
    Sync(std::sync::mpsc::SyncSender<std::io::Result<()>>),
}

struct WriterMsg {
    bytes: Vec<u8>,
    ack: WriterAck,
    /// Shared with the enqueuing caller: the writer skips the frame iff the
    /// caller cancelled it before the writer's claim CAS.
    frame: Arc<FrameState>,
}

/// Handle to the per-client stdin writer thread (bounded FIFO queue). The
/// JoinHandle is shared (clones share one thread slot) so close/Drop can join
/// the thread bounded.
#[derive(Clone)]
struct WriterHandle {
    tx: std::sync::mpsc::SyncSender<WriterMsg>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl WriterHandle {
    fn take_thread(&self) -> Option<std::thread::JoinHandle<()>> {
        recover_lock(&self.thread).take()
    }
}

/// Spawn the writer thread owning the child's stdin. A blocked write (server
/// not draining) blocks only this thread; callers observe a typed refusal
/// (queue full) or a deadline-bounded completion wait. A frame cancelled
/// before the writer claims it is skipped whole — cancellation NEVER
/// reorders the remaining frames.
fn spawn_stdin_writer<W: Write + Send + 'static>(sink: W) -> WriterHandle {
    let (tx, rx) = std::sync::mpsc::sync_channel::<WriterMsg>(WRITER_QUEUE_CAP);
    let thread = std::thread::spawn(move || {
        let mut stdin = BufWriter::new(sink);
        while let Ok(msg) = rx.recv() {
            // The claim is the LAST instant cancellation is possible: after
            // this CAS the frame is irrevocable (bytes may reach the peer),
            // so a caller that loses the race must surface delivery-unknown,
            // never a clean Timeout.
            if !msg.frame.begin_write() {
                // Cancelled at the caller's deadline before the writer
                // touched the frame: skip it WHOLE. No byte can reach the
                // peer and the FIFO order of later frames is unchanged.
                let skipped = Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "frame cancelled before write",
                ));
                match msg.ack {
                    WriterAck::Async(ack) => {
                        let _ = ack.send(skipped);
                    }
                    WriterAck::Sync(ack) => {
                        let _ = ack.send(skipped);
                    }
                }
                continue;
            }
            let result = stdin.write_all(&msg.bytes).and_then(|()| stdin.flush());
            match msg.ack {
                WriterAck::Async(ack) => {
                    let _ = ack.send(result);
                }
                WriterAck::Sync(ack) => {
                    let _ = ack.send(result);
                }
            }
        }
    });
    WriterHandle {
        tx,
        thread: Arc::new(Mutex::new(Some(thread))),
    }
}

impl WriterHandle {
    /// Enqueue one wire frame and await its write+flush completion under
    /// `deadline_at` — no lock held.
    ///
    /// Deadline semantics: success means written+flushed; a clean
    /// [`ErrorKind::Timeout`] means the frame was cancelled BEFORE the
    /// writer began it (provably not delivered); a
    /// [`is_delivery_unknown`] error means the writer had already begun the
    /// write, so the server may still execute the request.
    async fn write_and_flush(
        &self,
        bytes: Vec<u8>,
        deadline_at: tokio::time::Instant,
        what: &str,
    ) -> Result<(), Error> {
        let frame = FrameState::guarded();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.enqueue(bytes, WriterAck::Async(ack_tx), frame.state().clone(), what)?;
        match tokio::time::timeout_at(deadline_at, ack_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            // The combined write+flush failure keeps the historical typed
            // wording the closed-stdin test pins.
            Ok(Ok(Err(e))) => Err(Error::new(
                ErrorKind::Network,
                format!("lsp write {what}: stdin write/flush failed: {e}"),
            )),
            Ok(Err(_)) => Err(Error::new(
                ErrorKind::Network,
                format!("lsp {what}: stdin writer dropped the completion"),
            )),
            Err(_) => {
                // Deadline hit with no ack. Race the writer for the frame:
                // winning the CAS proves the frame will be skipped whole
                // (clean Timeout = not delivered); losing it means the
                // writer had already begun, so the server may still execute
                // the request and the outcome must say so.
                if frame.state().cancel_before_write() {
                    Err(Error::timeout(format!(
                        "lsp {what}: stdin write exceeded its deadline (the server is not draining)"
                    )))
                } else {
                    Err(delivery_unknown(&format!("lsp {what}")))
                }
            }
        }
    }

    /// Synchronous variant for the notification path, bounded by
    /// [`NOTIFY_WRITE_TIMEOUT_MS`]; never called while holding `conn`.
    /// Deadline semantics mirror [`WriterHandle::write_and_flush`]: a
    /// cancelled-before-write notification is a clean not-delivered Timeout,
    /// while one whose write had begun is delivery-unknown.
    fn write_and_flush_bounded(
        &self,
        bytes: Vec<u8>,
        bound: Duration,
        what: &str,
    ) -> Result<(), Error> {
        let frame = FrameState::guarded();
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        self.enqueue(bytes, WriterAck::Sync(ack_tx), frame.state().clone(), what)?;
        match ack_rx.recv_timeout(bound) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Error::new(
                ErrorKind::Network,
                format!("lsp notify {what}: {e}"),
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if frame.state().cancel_before_write() {
                    Err(Error::timeout(format!(
                        "lsp notify {what} exceeded its {} ms write bound",
                        bound.as_millis()
                    )))
                } else {
                    Err(delivery_unknown(&format!("lsp notify {what}")))
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Error::new(
                ErrorKind::Network,
                format!("lsp notify {what}: stdin writer is gone"),
            )),
        }
    }

    fn enqueue(
        &self,
        bytes: Vec<u8>,
        ack: WriterAck,
        frame: Arc<FrameState>,
        what: &str,
    ) -> Result<(), Error> {
        match self.tx.try_send(WriterMsg { bytes, ack, frame }) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Err(Error::new(
                ErrorKind::Oversized,
                format!("lsp {what}: stdin writer queue is full (the server is not draining)"),
            )),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(Error::new(
                ErrorKind::Network,
                format!("lsp {what}: stdin writer is gone"),
            )),
        }
    }
}

/// Bounded stderr tail in BYTES (recent tail kept for diagnostics). Total
/// bytes drained are counted separately — flooding is observable even
/// though the retained tail is capped.
#[derive(Debug, Default)]
struct StderrRing {
    bytes: VecDeque<u8>,
    total: u64,
    cap: usize,
}

impl StderrRing {
    fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(cap.min(4096)),
            total: 0,
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        if chunk.len() >= self.cap {
            // A single chunk larger than the cap: the ring IS this chunk's
            // tail (older bytes are gone — the cap is a hard bound).
            let keep = &chunk[chunk.len() - self.cap..];
            self.bytes.clear();
            self.bytes.extend(keep);
            return;
        }
        let over = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.cap);
        if over > 0 {
            // `over < self.bytes.len()`: chunk.len() < cap by the branch above.
            self.bytes.drain(..over);
        }
        self.bytes.extend(chunk);
    }

    fn tail_lossy(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<u8>>()).into_owned()
    }
}

/// Minimal file:// URI from an absolute path: unreserved RFC 3986
/// characters plus `/` and `:` pass through; everything else (spaces,
/// non-ASCII, ...) is percent-encoded per byte.
fn file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 8);
    out.push_str("file://");
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Idle bookkeeping hook: the manager installs it so every request AND
/// notification touches the workspace's last-used stamp.
type Activity = Arc<dyn Fn() + Send + Sync>;

pub struct LspClient {
    conn: Arc<Mutex<LspConn>>,
    writer: WriterHandle,
    supervisor: Arc<ProcessSupervisor>,
    /// Set when the server's stdout reached EOF (the server exited); new
    /// requests then fail fast instead of hanging until a deadline.
    exited: Arc<AtomicBool>,
    stderr: Arc<Mutex<StderrRing>>,
    activity: Option<Activity>,
    /// The stdout reader and stderr drain JoinHandles: consumed by the
    /// bounded join on shutdown/Drop (None = already joined).
    threads: Mutex<ClientThreads>,
}

#[derive(Default)]
struct ClientThreads {
    reader: Option<std::thread::JoinHandle<()>>,
    stderr: Option<std::thread::JoinHandle<()>>,
}

impl LspClient {
    async fn connect(
        cfg: &LspConfig,
        workspace: WorkspaceId,
        supervisor: Arc<ProcessSupervisor>,
        activity: Option<Activity>,
    ) -> Result<Arc<Self>, Error> {
        let proc_cfg = SpawnConfig {
            cmd: cfg.command.clone(),
            args: cfg.args.clone(),
            cwd: cfg.root.clone(),
            // One authority: the platform baseline (PATH/HOME/platform
            // bits) resolution needs; the daemon's full environment —
            // including secrets — never crosses.
            env: EnvSpec::default_baseline(),
            // The REAL workspace owns the daemon: never a placeholder id.
            owner: ProcessOwner::Workspace(workspace),
            capture: false,
            ..Default::default()
        };
        let spawned = supervisor
            .spawn_detached_with_pipes(proc_cfg)
            .map_err(|e| Error::new(ErrorKind::NotFound, format!("lsp spawn: {e}")))?;
        let writer = spawn_stdin_writer(spawned.stdin);
        let conn = Arc::new(Mutex::new(LspConn {
            child_pid: spawned.child_pid,
            next_id: 1,
            pending: HashMap::new(),
        }));
        let exited = Arc::new(AtomicBool::new(false));
        // Reader thread (stdout): incremental Content-Length framing;
        // responses are dispatched by id. EOF marks the server exited and
        // fails every pending request.
        let reader = {
            let conn2 = conn.clone();
            let exited2 = exited.clone();
            std::thread::spawn(move || read_loop(conn2, exited2, spawned.stdout))
        };
        // Stderr drain thread: a verbose server must never block itself on a
        // full stderr pipe; the bounded ring keeps the recent tail.
        let stderr = Arc::new(Mutex::new(StderrRing::new(STDERR_RING_CAP)));
        let stderr_thread = {
            let ring = stderr.clone();
            std::thread::spawn(move || stderr_drain(spawned.stderr, ring))
        };
        Ok(Arc::new(Self {
            conn,
            writer,
            supervisor,
            exited,
            stderr,
            activity,
            threads: Mutex::new(ClientThreads {
                reader: Some(reader),
                stderr: Some(stderr_thread),
            }),
        }))
    }

    /// REQUEST path: carries an id, registers a pending entry, awaits the
    /// response (bounded by `deadline`). A server that never answers is a
    /// timeout — never a hang.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        deadline: Duration,
    ) -> Result<serde_json::Value, Error> {
        if self.exited.load(Ordering::SeqCst) {
            return Err(Error::new(
                ErrorKind::Network,
                format!("lsp server exited before {method}"),
            ));
        }
        // ONE deadline covers the write AND the response wait: a server that
        // stops draining can never extend the request past it.
        let deadline_at = tokio::time::Instant::now() + deadline;
        let (id, request) = {
            let mut conn = recover_lock(&self.conn);
            if conn.pending.len() >= MAX_INFLIGHT_REQUESTS {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    "too many in-flight lsp requests",
                ));
            }
            let id = conn.next_id;
            conn.next_id += 1;
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            });
            (id.to_string(), request)
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut conn = recover_lock(&self.conn);
            // Register BEFORE the frame can reach the server: a fast response
            // is dispatched by the reader thread, which only takes this mutex
            // briefly (never across a blocking write).
            conn.pending.insert(id.clone(), tx);
        }
        let wire = format!(
            "Content-Length: {}\r\n\r\n{}",
            request.to_string().len(),
            request
        );
        // The write+flush runs on the dedicated writer thread; this await is
        // bounded by the same deadline and holds NO lock. On failure the
        // pending entry is dropped either way — a cancelled frame can never
        // be answered, and a delivery-unknown frame is no longer awaited
        // (its late response is discarded) — while the ERROR itself carries
        // the difference: clean Timeout vs typed delivery-unknown.
        if let Err(e) = self
            .writer
            .write_and_flush(wire.into_bytes(), deadline_at, method)
            .await
        {
            recover_lock(&self.conn).pending.remove(&id);
            return Err(e);
        }
        if let Some(a) = &self.activity {
            a();
        }
        match tokio::time::timeout_at(deadline_at, rx).await {
            Ok(Ok(v)) => {
                if let Some(err) = v.get("error") {
                    return Err(Error::new(
                        ErrorKind::Provider {
                            code: "lsp".into(),
                            retryable: false,
                        },
                        format!("lsp {method}: {err}"),
                    ));
                }
                Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
            }
            Ok(Err(_)) => Err(Error::new(ErrorKind::Network, "lsp connection dropped")),
            Err(_) => {
                recover_lock(&self.conn).pending.remove(&id);
                Err(Error::timeout(format!(
                    "lsp {method} exceeded {}ms",
                    deadline.as_millis()
                )))
            }
        }
    }

    /// NOTIFICATION path: fire-and-forget. No id, no pending entry, no
    /// response wait — the LSP server is NOT supposed to answer a
    /// notification, and the client must not act as if one were coming.
    /// Notifications to an already-exited server are dropped silently
    /// (their delivery is best-effort by design); a live-server write
    /// failure surfaces loudly.
    fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), Error> {
        if self.exited.load(Ordering::SeqCst) {
            return Ok(());
        }
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let wire = format!(
            "Content-Length: {}\r\n\r\n{}",
            notification.to_string().len(),
            notification
        );
        // The write+flush runs on the dedicated writer thread (never under
        // `conn`), bounded so a wedged server becomes a typed timeout.
        if let Err(e) = self.writer.write_and_flush_bounded(
            wire.into_bytes(),
            Duration::from_millis(NOTIFY_WRITE_TIMEOUT_MS),
            method,
        ) {
            if self.exited.load(Ordering::SeqCst) {
                return Ok(()); // the server died mid-write; nothing to notify
            }
            return Err(e);
        }
        if let Some(a) = &self.activity {
            a();
        }
        Ok(())
    }

    /// `initialize` REQUEST with the REAL workspace root as `rootUri`
    /// (spec §3.15: servers resolve project roots from this URI).
    pub async fn initialize(&self, root: &Path) -> Result<serde_json::Value, Error> {
        self.request(
            "initialize",
            serde_json::json!({
                "processId": null,
                "rootUri": file_uri(root),
                "capabilities": {},
            }),
            Duration::from_secs(10),
        )
        .await
    }

    /// `initialized` NOTIFICATION — must arrive right after the initialize
    /// RESULT and before any `didOpen` (spec §3.15).
    pub fn notify_initialized(&self) -> Result<(), Error> {
        self.notify("initialized", serde_json::json!({}))
    }

    /// `textDocument/didOpen` NOTIFICATION: the server must never answer
    /// it, so this returns as soon as the frame is written — no response
    /// wait, no timeout.
    pub fn did_open(&self, uri: &str, text: &str) -> Result<(), Error> {
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_of(uri),
                    "version": 1,
                    "text": text,
                }
            }),
        )
    }

    pub async fn document_symbols(&self, uri: &str) -> Result<Vec<serde_json::Value>, Error> {
        let result = self
            .request(
                "textDocument/documentSymbol",
                serde_json::json!({ "textDocument": { "uri": uri } }),
                Duration::from_secs(10),
            )
            .await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    /// Graceful LSP lifecycle: `shutdown` REQUEST (response awaited),
    /// `exit` NOTIFICATION, then a bounded grace wait for the process; the
    /// group kill is the fallback if the server does not exit on its own.
    pub async fn shutdown(&self) -> Result<(), Error> {
        // (1) shutdown REQUEST: await the server's response (bounded). A
        // failing/absent response is tolerated: exit+kill still proceed.
        let _ = self
            .request(
                "shutdown",
                serde_json::Value::Null,
                Duration::from_millis(SHUTDOWN_REQUEST_MS),
            )
            .await;
        // (2) exit NOTIFICATION: fire-and-forget.
        let _ = self.notify("exit", serde_json::Value::Null);
        // (3) bounded grace: poll for the process to exit; only then kill.
        let pid = self.conn.lock().map(|c| c.child_pid).unwrap_or(0);
        if pid != 0 && self.supervisor.pid_alive(pid) {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(EXIT_GRACE_MS);
            while tokio::time::Instant::now() < deadline && self.supervisor.pid_alive(pid) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            if self.supervisor.pid_alive(pid) {
                tracing::warn!(
                    "lsp server {pid} did not exit within {}ms of exit; killing",
                    EXIT_GRACE_MS
                );
                let _ = self.supervisor.kill_child_pid(pid, 500);
            }
        }
        self.exited.store(true, Ordering::SeqCst);
        // Bounded join: the kill (or the server's own exit) closes stdout and
        // stderr, so both drain threads reach EOF and end well inside the
        // bound. Nothing here is left detached silently.
        self.join_threads(Duration::from_millis(1_000));
        Ok(())
    }

    /// Consume and join the reader, stderr and writer threads with a hard
    /// bound each; a thread still alive at its deadline is reported.
    fn join_threads(&self, bound: Duration) {
        let (reader, stderr) = {
            let mut threads = recover_lock(&self.threads);
            (threads.reader.take(), threads.stderr.take())
        };
        let writer = self.writer.take_thread();
        for (name, handle) in [("reader", reader), ("stderr", stderr), ("writer", writer)] {
            let Some(handle) = handle else { continue };
            if !faktor_mcp::join_thread_bounded(handle, bound) {
                tracing::warn!(thread = name, "lsp thread did not stop within the bound");
            }
        }
    }

    /// True once every dedicated thread was consumed by a bounded join
    /// (test/diagnostic accessor).
    #[cfg(test)]
    fn threads_joined(&self) -> bool {
        let threads = recover_lock(&self.threads);
        threads.reader.is_none() && threads.stderr.is_none()
    }

    /// Recent stderr tail (bounded; lossy UTF-8) for diagnostics.
    pub fn stderr_tail(&self) -> String {
        recover_lock(&self.stderr).tail_lossy()
    }

    /// Total stderr bytes drained (never bounded) — proves a flood was
    /// actually drained and never blocked the server.
    pub fn stderr_total_bytes(&self) -> u64 {
        recover_lock(&self.stderr).total
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.exited.store(true, Ordering::SeqCst);
        let pid = self.conn.lock().map(|c| c.child_pid).unwrap_or(0);
        if pid != 0 {
            let _ = self.supervisor.kill_child_pid(pid, 200);
        }
        // Bounded joins: the kill closes the pipes, so the threads end; a
        // thread still alive at the deadline is reported, never implicitly
        // detached without a trace.
        self.join_threads(Duration::from_millis(500));
    }
}

fn language_of(uri: &str) -> String {
    let ext = uri.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "rust".into(),
        "py" => "python".into(),
        "ts" | "tsx" => "typescript".into(),
        "js" | "jsx" => "javascript".into(),
        "go" => "go".into(),
        _ => "plaintext".into(),
    }
}

/// One workspace's registry row: either a READY client or an in-flight start
/// gate. Concurrent `start()` calls for the same workspace single-flight on
/// the gate, so exactly ONE server process is ever spawned and the loser
/// callers adopt the winner's client.
enum ClientSlot {
    Starting(Arc<tokio::sync::Mutex<()>>),
    Ready(Arc<LspClient>),
}

/// Workspace-scoped registry: servers are shared per workspace and unloaded
/// on idle. `last_used` is the single idle clock: it is touched when a
/// server is STARTED and on EVERY request/notification after that (the
/// manager installs the touch hook on each client), so an actively-used old
/// server is never unloaded while a merely-created one is.
pub struct LspManager {
    supervisor: Arc<ProcessSupervisor>,
    clients: Mutex<HashMap<WorkspaceId, ClientSlot>>,
    last_used: Arc<Mutex<HashMap<WorkspaceId, i64>>>,
}

impl LspManager {
    pub fn new(supervisor: Arc<ProcessSupervisor>) -> Self {
        Self {
            supervisor,
            clients: Mutex::new(HashMap::new()),
            last_used: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The touch hook installed on every client: each request/notification
    /// refreshes this workspace's idle stamp through the manager's own
    /// bookkeeping map.
    fn activity_hook(&self, workspace: WorkspaceId) -> Activity {
        let last_used = self.last_used.clone();
        Arc::new(move || {
            if let Ok(mut m) = last_used.lock() {
                m.insert(workspace, now_ms());
            }
        })
    }

    /// Start (or adopt) the workspace's server with SINGLE-FLIGHT semantics:
    /// a placeholder gate is inserted under the registry lock, exactly one
    /// caller connects, and every concurrent caller blocks on that gate and
    /// adopts the same [`Arc<LspClient>`]. A failed start rolls the
    /// placeholder back, so no unreachable server is ever left running.
    pub async fn start(
        &self,
        workspace: WorkspaceId,
        cfg: LspConfig,
    ) -> Result<Arc<LspClient>, Error> {
        loop {
            enum Step {
                Ready(Arc<LspClient>),
                Wait(Arc<tokio::sync::Mutex<()>>),
                Lead(Arc<tokio::sync::Mutex<()>>),
            }
            let step = {
                let mut clients = recover_lock(&self.clients);
                match clients.get(&workspace) {
                    Some(ClientSlot::Ready(client)) => Step::Ready(client.clone()),
                    Some(ClientSlot::Starting(gate)) => Step::Wait(gate.clone()),
                    None => {
                        let gate = Arc::new(tokio::sync::Mutex::new(()));
                        clients.insert(workspace, ClientSlot::Starting(gate.clone()));
                        Step::Lead(gate)
                    }
                }
            };
            match step {
                Step::Ready(client) => {
                    // Reuse IS use: refresh the idle stamp.
                    self.touch(workspace);
                    return Ok(client);
                }
                Step::Wait(gate) => {
                    // The leader either publishes a Ready client or rolls its
                    // placeholder back; either way, re-inspect afterwards.
                    let _guard = gate.lock().await;
                    continue;
                }
                Step::Lead(gate) => {
                    // Hold the gate across connect+initialize: waiters block
                    // until the outcome is published, never spawn a second
                    // server.
                    let _guard = gate.lock().await;
                    let outcome = self.connect_and_initialize(workspace, &cfg).await;
                    return match outcome {
                        Ok(client) => {
                            // Publish ONLY if our placeholder is still there
                            // (no await happens under this scope; a concurrent
                            // shutdown that removed it wins and the fresh
                            // client is shut down, never orphaned).
                            let published = {
                                let mut clients = recover_lock(&self.clients);
                                let ours = matches!(
                                    clients.get(&workspace),
                                    Some(ClientSlot::Starting(current)) if Arc::ptr_eq(current, &gate)
                                );
                                if ours {
                                    clients.insert(workspace, ClientSlot::Ready(client.clone()));
                                }
                                ours
                            };
                            if published {
                                self.touch(workspace);
                                Ok(client)
                            } else {
                                let _ = client.shutdown().await;
                                Err(Error::new(
                                    ErrorKind::Cancelled,
                                    format!(
                                        "lsp start for workspace {workspace} was superseded by shutdown"
                                    ),
                                ))
                            }
                        }
                        Err(e) => {
                            let mut clients = recover_lock(&self.clients);
                            let ours = matches!(
                                clients.get(&workspace),
                                Some(ClientSlot::Starting(current)) if Arc::ptr_eq(current, &gate)
                            );
                            if ours {
                                clients.remove(&workspace);
                            }
                            drop(clients);
                            Err(e)
                        }
                    };
                }
            }
        }
    }

    /// Connect one client and run its initialize handshake.
    async fn connect_and_initialize(
        &self,
        workspace: WorkspaceId,
        cfg: &LspConfig,
    ) -> Result<Arc<LspClient>, Error> {
        let client = LspClient::connect(
            cfg,
            workspace,
            self.supervisor.clone(),
            Some(self.activity_hook(workspace)),
        )
        .await?;
        // Handshake: initialize REQUEST (awaited) → initialized NOTIFICATION
        // (fire-and-forget, before any didOpen).
        client.initialize(&cfg.root).await?;
        client.notify_initialized()?;
        Ok(client)
    }

    pub async fn client(&self, workspace: WorkspaceId) -> Result<Arc<LspClient>, Error> {
        let clients = recover_lock(&self.clients);
        match clients.get(&workspace) {
            Some(ClientSlot::Ready(client)) => Ok(client.clone()),
            // An in-flight start is not a usable client yet: callers either
            // wait through `start()` (single-flight) or observe not-found.
            _ => Err(Error::not_found(format!(
                "no LSP for workspace {workspace}"
            ))),
        }
    }

    /// Graceful teardown: shutdown request → exit notification → bounded
    /// exit wait (kill only as the fallback). A start racing this shutdown
    /// sees its placeholder gone and shuts the fresh client down itself.
    pub async fn shutdown(&self, workspace: WorkspaceId) -> Result<(), Error> {
        let slot = recover_lock(&self.clients).remove(&workspace);
        if let Some(ClientSlot::Ready(client)) = slot {
            client.shutdown().await?;
        }
        recover_lock(&self.last_used).remove(&workspace);
        Ok(())
    }

    pub fn active(&self) -> Vec<WorkspaceId> {
        let mut v: Vec<WorkspaceId> = recover_lock(&self.clients).keys().copied().collect();
        v.sort_by_key(|w| w.raw());
        v
    }

    fn touch(&self, workspace: WorkspaceId) {
        if let Ok(mut m) = self.last_used.lock() {
            m.insert(workspace, now_ms());
        }
    }

    /// Unload servers idle for longer than `idle_ms` (spec §21/§32).
    /// Synchronous by contract (daemon sweep): idle servers are killed
    /// directly — the graceful shutdown request/exit dance is the
    /// interactive teardown path, not the idle sweep's.
    pub fn unload_idle(&self, idle_ms: i64) -> Vec<WorkspaceId> {
        let now = now_ms();
        let stale: Vec<WorkspaceId> = recover_lock(&self.last_used)
            .iter()
            .filter(|(_, t)| now - **t > idle_ms)
            .map(|(w, _)| *w)
            .collect();
        for w in &stale {
            match recover_lock(&self.clients).remove(w) {
                Some(ClientSlot::Ready(client)) => {
                    let pid = client.conn.lock().map(|c| c.child_pid).unwrap_or(0);
                    if pid != 0 {
                        let _ = self.supervisor.kill_child_pid(pid, 200);
                    }
                }
                // An in-flight start: its leader observes the placeholder is
                // gone and shuts the fresh client down itself (no orphan).
                Some(ClientSlot::Starting(_)) | None => {}
            }
            recover_lock(&self.last_used).remove(w);
        }
        stale
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Stdout reader (own thread): incremental Content-Length framing; responses
/// are dispatched by id; EOF fails every pending request and marks the
/// server exited. The thread owns stdout, so blocking reads can never stall
/// the async runtime.
fn read_loop(
    conn: Arc<Mutex<LspConn>>,
    exited: Arc<AtomicBool>,
    stdout: std::process::ChildStdout,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    use std::io::Read;
    loop {
        let mut chunk = [0u8; 8192];
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_RESPONSE_BYTES {
            break;
        }
        loop {
            match faktor_mcp::parse_frame(&buf) {
                Ok(Some((consumed, value))) => {
                    buf.drain(..consumed);
                    let id = value
                        .get("id")
                        .and_then(|i| i.as_u64())
                        .map(|i| i.to_string());
                    if let Some(id) = id {
                        let mut guard = recover_lock(&conn);
                        if let Some(tx) = guard.pending.remove(&id) {
                            let _ = tx.send(value);
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    let mut guard = recover_lock(&conn);
                    for (_, tx) in guard.pending.drain() {
                        let _ = tx.send(serde_json::json!({"error": {"code": -32700, "message": "parse error"}}));
                    }
                    exited.store(true, Ordering::SeqCst);
                    return;
                }
            }
        }
    }
    exited.store(true, Ordering::SeqCst);
    let mut guard = recover_lock(&conn);
    for (_, tx) in guard.pending.drain() {
        let _ = tx.send(serde_json::json!({"error": {"code": -32000, "message": "server closed"}}));
    }
}

/// Stderr drain (own thread): read everything into the bounded ring — an
/// unread stderr pipe would let a verbose server block itself on a full
/// pipe and stall every request.
fn stderr_drain(mut stderr: std::process::ChildStderr, ring: Arc<Mutex<StderrRing>>) {
    use std::io::Read;
    let mut buf = [0u8; 8192];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => recover_lock(&ring).push(&buf[..n]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const MOCK: &str = r#"
import json, os, sys, threading, time

def send(obj):
    body = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

def read_msg():
    cl = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            cl = int(line.split(b":")[1])
    if cl == 0:
        return {}
    return json.loads(sys.stdin.buffer.read(cl))

def log(msg):
    sys.stderr.write(msg + "\n")
    sys.stderr.flush()

mode = sys.argv[1]
expected_root = sys.argv[2]
expected_uri = sys.argv[3]

if mode == "flood":
    def flood_loop():
        chunk = b"z" * 8192
        while True:
            sys.stderr.buffer.write(chunk)
            sys.stderr.buffer.flush()
    threading.Thread(target=flood_loop, daemon=True).start()

initialized_seen = False
didopen_seen = False

def check(cond, what):
    log(("OK " if cond else "FAIL ") + what)

while True:
    msg = read_msg()
    if msg is None:
        break
    if "method" not in msg:
        continue
    m = msg["method"]
    if m == "initialize":
        p = msg.get("params", {})
        check(p.get("rootUri") == expected_uri, "initialize rootUri")
        check(os.path.realpath(os.getcwd()) == os.path.realpath(expected_root), "initialize cwd")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "capabilities": {},
            "serverInfo": {"name": "mock-lsp", "version": "1"},
        }})
        log("SENT initialize result")
    elif m == "initialized":
        initialized_seen = True
        log("GOT initialized")
    elif m == "textDocument/didOpen":
        didopen_seen = True
        check(initialized_seen, "didOpen after initialized")
        log("GOT didOpen")
        if mode == "stops-draining":
            # Stop reading stdin: the next large write fills the pipe and
            # blocks the writer thread; the client must return a typed
            # deadline error instead of deadlocking on the conn mutex.
            log("STOPPED draining")
            while True:
                time.sleep(0.05)
        if mode == "close-stdin":
            # Close our read end of the pipe and keep running: the client's
            # NEXT write/flush must fail typed, not time out. (`os.close` —
            # closing the sys.stdin wrapper would NOT close the fd.)
            os.close(sys.stdin.fileno())
            log("CLOSED stdin")
            while True:
                time.sleep(0.05)
    elif m == "textDocument/documentSymbol":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": []})
    elif m == "shutdown":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
        log("SENT shutdown result")
    elif m == "exit":
        log("GOT exit")
        break
log("mock exiting")
"#;

    fn python_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// A real language-server stand-in (written to a temp file at test
    /// time): speaks Content-Length JSON-RPC on stdio, checks the handshake
    /// contract on behalf of the "server side" and reports via stderr
    /// markers that the client drains into its bounded ring.
    fn mock_server_script() -> std::path::PathBuf {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lsp_mock.py");
        std::fs::write(&path, MOCK).unwrap();
        std::mem::forget(dir); // the temp file lives for the test process
        path
    }

    fn supervisor() -> (tempfile::TempDir, Arc<ProcessSupervisor>) {
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        (dir, ProcessSupervisor::new(cas))
    }

    fn cfg_with(root: PathBuf, mode: &str, expected_uri: &str) -> (LspConfig, String) {
        let script = mock_server_script();
        (
            LspConfig {
                name: "mock".into(),
                command: "python3".into(),
                args: vec![
                    script.to_str().unwrap().into(),
                    mode.into(),
                    root.to_str().unwrap().into(),
                    expected_uri.into(),
                ],
                root,
            },
            script.to_str().unwrap().to_string(),
        )
    }

    /// Poll the client's stderr ring until `needle` appears (markers are
    /// written by the mock and drained asynchronously).
    async fn wait_for_stderr(client: &LspClient, needle: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let tail = client.stderr_tail();
            if tail.contains(needle) || std::time::Instant::now() > deadline {
                return tail;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn language_of_mapping() {
        assert_eq!(language_of("file:///x/main.rs"), "rust");
        assert_eq!(language_of("file:///x/a.py"), "python");
        assert_eq!(language_of("file:///x/a.ts"), "typescript");
        assert_eq!(language_of("file:///x/a.go"), "go");
        assert_eq!(language_of("file:///x/a.xyz"), "plaintext");
    }

    #[test]
    fn file_uri_encodes_and_roundtrips() {
        assert_eq!(file_uri(Path::new("/a/b/c.rs")), "file:///a/b/c.rs");
        assert_eq!(file_uri(Path::new("/a b/ü.rs")), "file:///a%20b/%C3%BC.rs");
    }

    #[test]
    fn stderr_ring_caps_bytes_and_keeps_the_tail() {
        let mut ring = StderrRing::new(16);
        ring.push(b"abcdefghijklmnop"); // exactly the cap
        assert_eq!(ring.bytes.len(), 16);
        ring.push(b"q"); // one byte over
        assert_eq!(ring.bytes.len(), 16, "hard byte cap");
        assert_eq!(
            ring.tail_lossy(),
            "bcdefghijklmnopq",
            "head dropped, tail kept"
        );
        assert_eq!(ring.total, 17, "total is never bounded");
        // A chunk bigger than the whole ring replaces it by its own tail.
        ring.push(&b"x".repeat(64));
        assert_eq!(ring.bytes.len(), 16);
        assert_eq!(ring.total, 81);
        assert_eq!(ring.tail_lossy(), "x".repeat(16));
    }

    #[tokio::test]
    async fn missing_server_is_not_found_and_manager_survives() {
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let mgr = LspManager::new(sup);
        let result = mgr
            .start(
                WorkspaceId::new(1),
                LspConfig {
                    name: "ghost".into(),
                    command: "/nonexistent-lsp".into(),
                    args: vec![],
                    root: dir.path().to_path_buf(),
                },
            )
            .await;
        // connect() returns Err(NotFound) OR times out (either is a clean
        // failure — never a panic, never a zombie).
        if let Err(e) = result {
            assert!(e.kind == ErrorKind::NotFound, "{e:?}");
        }
        // The manager remains usable.
        assert!(mgr.active().is_empty());
        assert!(mgr.client(WorkspaceId::new(1)).await.is_err());
    }

    #[tokio::test]
    async fn idle_unload_removes_clients() {
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let mgr = LspManager::new(sup);
        // No server started → idle unload is a no-op.
        assert!(mgr.unload_idle(1).is_empty());
        assert!(mgr.active().is_empty());
    }

    #[test]
    fn framing_reuse() {
        // The LSP crate reuses faktor-mcp's framing; verify a valid frame.
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": null}).to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let (consumed, v) = faktor_mcp::parse_frame(frame.as_bytes()).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn hostile_frames_never_panic() {
        // Empty and truncated frames are "need more", never errors; garbage
        // headers are typed errors. None may panic.
        assert!(matches!(faktor_mcp::parse_frame(b""), Ok(None)));
        assert!(matches!(
            faktor_mcp::parse_frame(b"Content-Length: 5\r\n\r\n"),
            Ok(None)
        ));
        assert!(faktor_mcp::parse_frame(b"\x00\x01").is_err());
        assert!(faktor_mcp::parse_frame(b"Content-Length: x\r\n\r\n").is_err());
        // A declared length past the 16 MiB bound is refused before any
        // allocation; a valid frame still parses (non-vacuity control).
        assert!(faktor_mcp::parse_frame(b"Content-Length: 999999999999\r\n\r\n").is_err());
        let (consumed, value) = faktor_mcp::parse_frame(b"Content-Length: 2\r\n\r\n{}")
            .unwrap()
            .unwrap();
        assert_eq!(consumed, 23);
        assert_eq!(value, serde_json::json!({}));
    }

    /// (i) connect sends initialize with the REAL rootUri and cwd, receives
    /// the result, sends the initialized NOTIFICATION (server asserts it
    /// receives initialized before any didOpen); the process is owned by the
    /// REAL workspace id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_carries_real_root_and_owner() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let mgr = LspManager::new(sup.clone());
        let real_ws = WorkspaceId::new(42); // never 1: the owner must be real
        let (cfg, _script) = cfg_with(root.clone(), "plain", &expected_uri);
        let client = tokio::time::timeout(Duration::from_secs(15), mgr.start(real_ws, cfg))
            .await
            .expect("start timeout")
            .expect("start failed");

        // The spawned daemon is owned by the REAL workspace id.
        let alive = sup.alive();
        assert!(
            alive
                .iter()
                .any(|h| h.owner == ProcessOwner::Workspace(real_ws)),
            "server must be owned by the real workspace: {alive:?}"
        );
        assert!(
            !alive
                .iter()
                .any(|h| h.owner == ProcessOwner::Workspace(WorkspaceId::new(1))),
            "never a placeholder workspace-1 owner"
        );

        // didOpen is a notification; the server-side assertions (rootUri,
        // cwd, initialized-before-didOpen) surface as stderr markers.
        client
            .did_open(&format!("{expected_uri}/main.rs"), "fn main() {}")
            .unwrap();
        let symbols = tokio::time::timeout(
            Duration::from_secs(10),
            client.document_symbols(&format!("{expected_uri}/main.rs")),
        )
        .await
        .expect("documentSymbol timeout")
        .expect("documentSymbol failed");
        assert!(symbols.is_empty());

        let tail = wait_for_stderr(&client, "mock exiting").await;
        assert!(
            tail.contains("OK initialize rootUri"),
            "rootUri must be the REAL workspace root: {tail}"
        );
        assert!(
            tail.contains("OK initialize cwd"),
            "cwd must be the REAL workspace root: {tail}"
        );
        assert!(
            tail.contains("OK didOpen after initialized"),
            "initialized must precede didOpen: {tail}"
        );
        mgr.shutdown(real_ws).await.unwrap();
    }

    /// (ii) didOpen is a NOTIFICATION: the stand-in sends NO response and
    /// the client must NOT time out or error (the old request path waited
    /// for a response that is not coming by design).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn did_open_notification_never_waits_for_a_response() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let mgr = LspManager::new(sup);
        let (cfg, _script) = cfg_with(root.clone(), "plain", &expected_uri);
        let client =
            tokio::time::timeout(Duration::from_secs(15), mgr.start(WorkspaceId::new(9), cfg))
                .await
                .expect("start timeout")
                .expect("start failed");
        let t0 = std::time::Instant::now();
        client
            .did_open(&format!("{expected_uri}/a.rs"), "fn a() {}")
            .unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "a notification must return immediately, took {elapsed:?}"
        );
        // The server processed it (no response was ever expected).
        let tail = wait_for_stderr(&client, "GOT didOpen").await;
        assert!(!tail.contains("FAIL didOpen after initialized"), "{tail}");
        mgr.shutdown(WorkspaceId::new(9)).await.unwrap();
    }

    /// (vi) a child that closes its stdin after a notification: the NEXT
    /// request must fail with the typed transport error immediately and must
    /// NOT be registered in the pending map. The old ignored-flush path
    /// registered it and only surfaced the failure as a deadline timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_stdin_request_fails_typed_and_registers_no_pending() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let mgr = LspManager::new(sup);
        let ws = WorkspaceId::new(41);
        let (cfg, _script) = cfg_with(root.clone(), "close-stdin", &expected_uri);
        let client = tokio::time::timeout(Duration::from_secs(15), mgr.start(ws, cfg))
            .await
            .expect("start timeout")
            .expect("start failed");
        // The notification itself succeeds (the child is still reading).
        client
            .did_open(&format!("{expected_uri}/a.rs"), "fn a() {}")
            .unwrap();
        let tail = wait_for_stderr(&client, "CLOSED stdin").await;
        assert!(
            tail.contains("CLOSED stdin"),
            "the mock must have closed its stdin: {tail}"
        );
        // The next REQUEST hits the closed pipe: typed transport error, fast
        // (never the 10s deadline), with no pending entry left behind.
        let t0 = std::time::Instant::now();
        let err = client
            .document_symbols(&format!("{expected_uri}/a.rs"))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Network, "{err:?}");
        assert!(
            err.message.contains("flush failed") || err.message.contains("lsp write"),
            "the typed transport error must name the failed write/flush: {err:?}"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "a closed stdin must fail fast, never time out: {:?}",
            t0.elapsed()
        );
        assert!(
            client.conn.lock().unwrap().pending.is_empty(),
            "no pending entry may be registered for a failed write"
        );
        mgr.shutdown(ws).await.unwrap();
    }

    /// A server that answers the handshake, then STOPS draining stdin: a
    /// large request would previously block the async caller under the
    /// `conn` mutex while the reader thread waited on the same mutex — a
    /// deadlock no deadline could cover. The dedicated writer thread must
    /// turn it into a bounded typed error, leaving the mutex and reader
    /// thread usable.
    ///
    /// Delivery semantics: the writer had already CLAIMED this single frame
    /// (the queue was empty, so it went straight from the FIFO to the pipe
    /// and blocked), so the deadline must NOT masquerade as a clean
    /// not-delivered Timeout — the server may still execute it on resume.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stops_draining_server_is_delivery_unknown_not_a_deadlock() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let mgr = LspManager::new(sup);
        let ws = WorkspaceId::new(77);
        let (cfg, _script) = cfg_with(root.clone(), "stops-draining", &expected_uri);
        let client = tokio::time::timeout(Duration::from_secs(15), mgr.start(ws, cfg))
            .await
            .expect("start timeout")
            .expect("start failed");
        client
            .did_open(&format!("{expected_uri}/a.rs"), "fn a() {}")
            .unwrap();
        let tail = wait_for_stderr(&client, "STOPPED draining").await;
        assert!(tail.contains("STOPPED draining"), "{tail}");
        let payload = "x".repeat(4 * 1024 * 1024);
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            client.request(
                "textDocument/documentSymbol",
                serde_json::json!({ "pad": payload }),
                Duration::from_millis(300),
            ),
        )
        .await
        .expect("a non-draining server must not hang the caller")
        .unwrap_err();
        assert!(
            is_delivery_unknown(&err),
            "a claimed-but-unacked write must be delivery-unknown, never a clean Timeout: {err:?}"
        );
        assert_ne!(err.kind, ErrorKind::Timeout, "{err:?}");
        assert!(!err.retryable, "{err:?}");
        assert!(
            err.message.contains("may still execute"),
            "the Display text must name the peer-execution risk: {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the request deadline must cover the write: {:?}",
            started.elapsed()
        );
        assert!(
            client.conn.lock().unwrap().pending.is_empty(),
            "the timed-out call must not leave a pending entry"
        );
        // The conn mutex and reader thread remain usable (no deadlock).
        assert!(client.conn.lock().is_ok());
        drop(mgr);
    }

    /// Deterministic fake server pipe: the FIRST write blocks until the gate
    /// opens, then every write is appended to a byte log (delivered frames
    /// can be parsed back and ordered). Later writes pass straight through —
    /// a fake server that drains later.
    struct LspGatedSink {
        byte_log: Arc<Mutex<Vec<u8>>>,
        entered: Arc<std::sync::atomic::AtomicBool>,
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl Write for LspGatedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.entered.store(true, Ordering::SeqCst);
            let (open, cv) = &*self.gate;
            let mut guard = open.lock().unwrap();
            while !*guard {
                guard = cv.wait(guard).unwrap();
            }
            drop(guard);
            self.byte_log.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn release_lsp_gate(gate: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
        let (open, cv) = &**gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }

    fn lsp_logical_frame(id: u64, method: &str) -> Vec<u8> {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method}).to_string();
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    fn lsp_delivered_ids(bytes: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let (consumed, value) = faktor_mcp::parse_frame(rest)
                .expect("recorded bytes must be complete frames")
                .expect("recorded bytes must hold complete frames");
            out.push(value["id"].as_u64().expect("frames carry numeric ids"));
            rest = &rest[consumed..];
        }
        out
    }

    struct LspFakeSink {
        writer: WriterHandle,
        byte_log: Arc<Mutex<Vec<u8>>>,
        entered: Arc<std::sync::atomic::AtomicBool>,
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    fn lsp_sink() -> LspFakeSink {
        let byte_log = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let writer = spawn_stdin_writer(LspGatedSink {
            byte_log: byte_log.clone(),
            entered: entered.clone(),
            gate: gate.clone(),
        });
        LspFakeSink {
            writer,
            byte_log,
            entered,
            gate,
        }
    }

    async fn await_lsp_entered(entered: &Arc<std::sync::atomic::AtomicBool>) {
        let t0 = std::time::Instant::now();
        while !entered.load(Ordering::SeqCst) {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "the writer never reached the fake server"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Deterministic request path: the claimed frame is delivery-unknown; the
    /// queued frames are cleanly cancelled and never reach the fake server;
    /// a live frame keeps FIFO order across the skipped ones.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_timeout_is_clean_only_when_cancelled_before_write() {
        let sink = lsp_sink();
        let head = tokio::spawn({
            let writer = sink.writer.clone();
            async move {
                writer
                    .write_and_flush(
                        lsp_logical_frame(1, "initialize"),
                        tokio::time::Instant::now() + Duration::from_millis(150),
                        "initialize",
                    )
                    .await
            }
        });
        await_lsp_entered(&sink.entered).await;
        for id in 2..=3u64 {
            let err = sink
                .writer
                .write_and_flush(
                    lsp_logical_frame(id, "queued"),
                    tokio::time::Instant::now() + Duration::from_millis(50),
                    "queued",
                )
                .await
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::Timeout, "frame {id}: {err:?}");
            assert!(!is_delivery_unknown(&err), "frame {id}: {err:?}");
        }
        let head_err = tokio::time::timeout(Duration::from_secs(5), head)
            .await
            .expect("head must settle")
            .expect("head task must not panic")
            .unwrap_err();
        assert!(
            is_delivery_unknown(&head_err) && !head_err.retryable,
            "the claimed request must be delivery-unknown: {head_err:?}"
        );
        let tail = tokio::spawn({
            let writer = sink.writer.clone();
            async move {
                writer
                    .write_and_flush(
                        lsp_logical_frame(4, "tail"),
                        tokio::time::Instant::now() + Duration::from_secs(10),
                        "tail",
                    )
                    .await
            }
        });
        release_lsp_gate(&sink.gate);
        let tail_result = tokio::time::timeout(Duration::from_secs(5), tail)
            .await
            .expect("tail must settle")
            .expect("tail task must not panic");
        assert!(tail_result.is_ok(), "{tail_result:?}");
        let ids = lsp_delivered_ids(&sink.byte_log.lock().unwrap().clone());
        assert_eq!(
            ids,
            vec![1, 4],
            "cancelled requests must be skipped whole without reordering"
        );
    }

    /// Deterministic notification path (synchronous bounded writer): a
    /// notification queued behind a blocked frame is a clean not-delivered
    /// Timeout and never reaches the fake server; a notification whose write
    /// had begun is delivery-unknown; the live one keeps its FIFO place.
    #[test]
    fn notification_timeout_is_clean_only_when_cancelled_before_write() {
        let sink = lsp_sink();

        // Head notification: claimed by the writer, blocked in the fake
        // server. Its write BEGAN, so its bounded timeout must be
        // delivery-unknown.
        let (head_tx, head_rx) = std::sync::mpsc::channel();
        let head_writer = sink.writer.clone();
        let head_thread = std::thread::spawn(move || {
            let result = head_writer.write_and_flush_bounded(
                lsp_logical_frame(1, "didOpen"),
                Duration::from_millis(150),
                "didOpen",
            );
            let _ = head_tx.send(result);
        });
        let t0 = std::time::Instant::now();
        while !sink.entered.load(Ordering::SeqCst) {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }

        // Queued notification: still in the FIFO when its bound expires =>
        // cancelled whole, clean Timeout, never delivered.
        let queued = sink
            .writer
            .write_and_flush_bounded(
                lsp_logical_frame(2, "didChange"),
                Duration::from_millis(50),
                "didChange",
            )
            .unwrap_err();
        assert_eq!(queued.kind, ErrorKind::Timeout, "{queued:?}");
        assert!(!is_delivery_unknown(&queued), "{queued:?}");

        let head_err = head_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("head notification must settle")
            .expect_err("the claimed notification cannot be a clean success");
        assert!(
            is_delivery_unknown(&head_err) && !head_err.retryable,
            "a claimed notification must be delivery-unknown: {head_err:?}"
        );
        let _ = head_thread.join();

        // Live notification after the cancelled one: delivered, FIFO kept.
        let (tail_tx, tail_rx) = std::sync::mpsc::channel();
        let tail_writer = sink.writer.clone();
        let tail_thread = std::thread::spawn(move || {
            let _ = tail_tx.send(tail_writer.write_and_flush_bounded(
                lsp_logical_frame(3, "didSave"),
                Duration::from_secs(10),
                "didSave",
            ));
        });
        release_lsp_gate(&sink.gate);
        let tail_result = tail_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("tail notification must settle");
        assert!(tail_result.is_ok(), "{tail_result:?}");
        let _ = tail_thread.join();
        let ids = lsp_delivered_ids(&sink.byte_log.lock().unwrap().clone());
        assert_eq!(
            ids,
            vec![1, 3],
            "cancelled notifications must be skipped whole without reordering"
        );
    }

    /// The bounded FIFO still refuses with the typed `Oversized` error on
    /// both writer paths once full (never a blocked caller).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lsp_full_writer_queue_is_a_typed_oversized_refusal() {
        let sink = lsp_sink();
        let head = tokio::spawn({
            let writer = sink.writer.clone();
            async move {
                writer
                    .write_and_flush(
                        lsp_logical_frame(1, "initialize"),
                        tokio::time::Instant::now() + Duration::from_secs(30),
                        "initialize",
                    )
                    .await
            }
        });
        await_lsp_entered(&sink.entered).await;
        for id in 2..(2 + WRITER_QUEUE_CAP as u64) {
            let (ack_tx, _rx) = std::sync::mpsc::sync_channel(1);
            sink.writer
                .tx
                .try_send(WriterMsg {
                    bytes: lsp_logical_frame(id, "queued"),
                    ack: WriterAck::Sync(ack_tx),
                    frame: Arc::new(FrameState::queued()),
                })
                .expect("the bounded FIFO must accept exactly CAP frames");
        }
        let async_err = sink
            .writer
            .write_and_flush(
                lsp_logical_frame(999, "overflow"),
                tokio::time::Instant::now() + Duration::from_millis(200),
                "overflow",
            )
            .await
            .unwrap_err();
        assert_eq!(async_err.kind, ErrorKind::Oversized, "{async_err:?}");
        let sync_err = sink
            .writer
            .write_and_flush_bounded(
                lsp_logical_frame(998, "overflow-notify"),
                Duration::from_millis(200),
                "overflow-notify",
            )
            .unwrap_err();
        assert_eq!(sync_err.kind, ErrorKind::Oversized, "{sync_err:?}");
        release_lsp_gate(&sink.gate);
        let _ = tokio::time::timeout(Duration::from_secs(10), head).await;
    }

    /// (iii) shutdown REQUEST receives a response, then the exit
    /// notification arrives, then the process exits within the grace —
    /// kill is never the first move.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_request_then_exit_notification_within_grace() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let mgr = LspManager::new(sup.clone());
        let ws = WorkspaceId::new(13);
        let (cfg, _script) = cfg_with(root.clone(), "plain", &expected_uri);
        let client = tokio::time::timeout(Duration::from_secs(15), mgr.start(ws, cfg))
            .await
            .expect("start timeout")
            .expect("start failed");
        let pid = client.conn.lock().unwrap().child_pid;
        assert!(sup.pid_alive(pid), "server must be alive before shutdown");
        let t0 = std::time::Instant::now();
        mgr.shutdown(ws).await.unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(EXIT_GRACE_MS + 2_000),
            "process must exit within the bounded grace: {elapsed:?}"
        );
        assert!(!sup.pid_alive(pid), "server must have exited on its own");
        // The mock answered shutdown and only then received exit — proving
        // the request/response/notification ordering, not a blind kill.
        let tail = wait_for_stderr(&client, "GOT exit").await;
        assert!(
            tail.contains("SENT shutdown result"),
            "shutdown response must be awaited: {tail}"
        );
        assert!(
            tail.contains("GOT exit"),
            "exit notification must arrive: {tail}"
        );
        assert!(
            tail.contains("mock exiting"),
            "process exits after exit: {tail}"
        );
        assert!(mgr.active().is_empty());
        assert!(
            client.threads_joined(),
            "shutdown must join the reader, stderr and writer threads"
        );
    }

    /// F4: concurrent starts for one workspace single-flight to EXACTLY one
    /// child process; every caller adopts the winner's handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_starts_single_flight_to_one_child() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let mgr = Arc::new(LspManager::new(sup.clone()));
        let ws = WorkspaceId::new(31);
        let (cfg, _script) = cfg_with(root.clone(), "plain", &expected_uri);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let mgr = mgr.clone();
            let cfg = cfg.clone();
            tasks.push(tokio::spawn(async move { mgr.start(ws, cfg).await }));
        }
        let mut clients = Vec::new();
        for task in tasks {
            clients.push(
                tokio::time::timeout(Duration::from_secs(15), task)
                    .await
                    .expect("join timeout")
                    .expect("task panicked")
                    .expect("start failed"),
            );
        }
        for client in clients.iter().skip(1) {
            assert!(
                Arc::ptr_eq(&clients[0], client),
                "every concurrent caller must adopt the winner's handle"
            );
        }
        assert_eq!(
            sup.registered(),
            1,
            "exactly ONE child process may be spawned by concurrent starts"
        );
        clients.clear();
        tokio::time::timeout(Duration::from_secs(10), mgr.shutdown(ws))
            .await
            .expect("shutdown timeout")
            .expect("shutdown failed");
        assert!(mgr.active().is_empty());
    }

    /// F4: a failed start rolls its placeholder back, no orphan child is
    /// left, and the manager stays usable for a later start.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_start_failure_leaves_no_orphan_and_rolls_back() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let mgr = Arc::new(LspManager::new(sup.clone()));
        let ws = WorkspaceId::new(32);
        let cfg = LspConfig {
            name: "dies".into(),
            command: "python3".into(),
            args: vec!["-c".into(), "import sys; sys.exit(0)".into()],
            root: dir.path().to_path_buf(),
        };
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let mgr = mgr.clone();
            let cfg = cfg.clone();
            tasks.push(tokio::spawn(async move { mgr.start(ws, cfg).await }));
        }
        for task in tasks {
            let outcome = tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("join timeout")
                .expect("task panicked");
            assert!(outcome.is_err(), "a dead server cannot initialize");
        }
        assert!(
            mgr.active().is_empty(),
            "a failed start must roll its placeholder back"
        );
        // Let the exited children be reaped; none may remain registered.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sup.registered() > 0 && std::time::Instant::now() < deadline {
            sup.reap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        sup.reap();
        assert_eq!(sup.registered(), 0, "no orphaned server process may remain");
        assert!(mgr.client(ws).await.is_err());
    }

    /// F7: Drop (no graceful shutdown) kills the child and joins the
    /// dedicated threads within the bound; the supervisor registry drains.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_joins_threads_and_leaves_no_child() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let (cfg, _script) = cfg_with(root.clone(), "plain", &expected_uri);
        let direct = LspClient::connect(&cfg, WorkspaceId::new(35), sup.clone(), None)
            .await
            .expect("connect");
        let pid = direct.conn.lock().unwrap().child_pid;
        assert!(sup.pid_alive(pid));
        drop(direct);
        assert!(!sup.pid_alive(pid), "Drop must terminate the child");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sup.registered() > 0 && std::time::Instant::now() < deadline {
            sup.reap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        sup.reap();
        assert_eq!(sup.registered(), 0, "Drop must leave no lingering child");
    }

    /// (iv) a stand-in flooding stderr with megabytes keeps running (the
    /// bounded ring drains the pipe — no deadlock) and the client still
    /// answers requests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stderr_flood_is_drained_bounded_and_never_blocks() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let mgr = LspManager::new(sup);
        let ws = WorkspaceId::new(21);
        let (cfg, _script) = cfg_with(root.clone(), "flood", &expected_uri);
        let client = tokio::time::timeout(Duration::from_secs(15), mgr.start(ws, cfg))
            .await
            .expect("start timeout")
            .expect("start failed");
        // Let the flood run; the client must keep answering requests.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while client.stderr_total_bytes() < 2 * 1024 * 1024 && std::time::Instant::now() < deadline
        {
            let symbols = tokio::time::timeout(
                Duration::from_secs(5),
                client.document_symbols("file:///x.rs"),
            )
            .await
            .expect("request timeout while flooding")
            .expect("request failed while flooding");
            assert!(symbols.is_empty());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let total = client.stderr_total_bytes();
        assert!(
            total >= 2 * 1024 * 1024,
            "stderr must actually be drained, only {total} bytes"
        );
        let tail = client.stderr_tail();
        assert!(
            tail.len() <= STDERR_RING_CAP,
            "retained tail must be bounded: {}",
            tail.len()
        );
        mgr.shutdown(ws).await.unwrap();
    }

    /// (v) idle: a server created and left idle is unloaded after the
    /// manager's idle period; an OLD server that was actively used right
    /// before the sweep is NOT unloaded (touch-on-use proven).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_unload_ignores_recently_used_old_servers() {
        if !python_available() {
            eprintln!("python3 missing; skipping");
            return;
        }
        let idle_ms = 250i64;
        let root_dir = tempdir().unwrap();
        let root = root_dir.path().to_path_buf();
        let expected_uri = file_uri(&root);
        let (_d, sup) = supervisor();
        let mgr = LspManager::new(sup.clone());
        let idle_ws = WorkspaceId::new(30);
        let (cfg_idle, _s) = cfg_with(root.clone(), "plain", &expected_uri);
        let _idle_client =
            tokio::time::timeout(Duration::from_secs(15), mgr.start(idle_ws, cfg_idle))
                .await
                .expect("start timeout")
                .expect("start failed");
        // Let the idle server age past the idle period...
        tokio::time::sleep(Duration::from_millis((idle_ms + 150) as u64)).await;
        // ...then start a second server and let IT age too.
        let used_ws = WorkspaceId::new(31);
        let (cfg_used, _s2) = cfg_with(root.clone(), "plain", &expected_uri);
        let used_client =
            tokio::time::timeout(Duration::from_secs(15), mgr.start(used_ws, cfg_used))
                .await
                .expect("start timeout")
                .expect("start failed");
        tokio::time::sleep(Duration::from_millis((idle_ms + 150) as u64)).await;
        // BOTH creation stamps are now stale. Actively use the old server:
        // the request must refresh its idle stamp.
        used_client
            .document_symbols("file:///x.rs")
            .await
            .expect("request on the old-but-used server failed");
        let unloaded = mgr.unload_idle(idle_ms);
        assert!(
            unloaded.contains(&idle_ws),
            "created-and-idle server must be unloaded: {unloaded:?}"
        );
        assert!(
            !unloaded.contains(&used_ws),
            "an old server used right before the sweep must survive: {unloaded:?}"
        );
        assert_eq!(mgr.active(), vec![used_ws]);
        // The idle server's process is gone; the used one's is alive.
        let alive_pids: Vec<u32> = sup.alive().iter().map(|h| h.pid).collect();
        assert_eq!(
            alive_pids.len(),
            1,
            "only the used server survives: {alive_pids:?}"
        );
        mgr.shutdown(used_ws).await.unwrap();
        assert!(mgr.active().is_empty());
    }
}
