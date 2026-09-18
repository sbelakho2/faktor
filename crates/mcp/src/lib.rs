//! faktor-mcp — JSON-RPC Model Context Protocol client (spec §31).
//!
//! MCP processes are supervised like terminals: crashes, hangs, and garbage
//! output never destabilize the agent runtime. Every invocation has a
//! deadline; responses are bounded; the framing is Content-Length JSON-RPC.
//!
//! Audit P0-40 (unified process supervision): this crate NEVER constructs
//! its own process machinery. [`McpServer::connect`] receives the shared
//! [`faktor_terminal::ProcessSupervisor`] (the daemon wires ONE supervisor
//! for every server) and spawns the stdio MCP subprocess through
//! `spawn_detached_with_pipes`, so the child lands in the supervisor's
//! bounded registry with owner rows and the daemon-shutdown kill scope.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use faktor_core::error::{Error, ErrorKind};
use faktor_terminal::{EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};

/// Classified lock recovery for DERIVED state (caches, registries, rings,
/// process/ownership projections): a poisoned guard is recovered with the
/// poison flag cleared, so one panicking caller can never wedge later use.
/// The durable authority (store/journal/OS process state) remains the
/// source of truth; the recovered value is only ever a projection of it.
fn recover_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// The initialize-handshake read bound: while the handshake has not
/// completed, the reader accumulates at most this many bytes before it fails
/// the connection closed. A hostile server cannot force the client to buffer
/// a multi-megabyte frame before capabilities were even negotiated.
const MAX_INITIAL_BYTES: usize = 64 * 1024;
/// Internal (never wire) marker kind a reader-originated failure carries so
/// [`McpServer::call`] can surface the typed [`ErrorKind::Oversized`] instead
/// of a generic provider error.
const OVERSIZED_MARKER: &str = "oversized";

/// Stable [`ErrorKind::Provider`] code for [`is_delivery_unknown`]: a frame
/// whose stdin write had ALREADY STARTED when its deadline expired. It is
/// NEVER a clean, provably-not-delivered timeout.
pub const DELIVERY_UNKNOWN_CODE: &str = "delivery_unknown";

/// A timed-out stdin frame whose write had ALREADY STARTED when the deadline
/// expired: bytes may have reached the peer, so the request may still be
/// executed even though the client stopped waiting and discarded its result.
/// The Display text names that uncertainty explicitly; callers must not
/// blindly retry a request carrying an unknown external effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryUnknown {
    /// The operation label (`mcp <method>`, `lsp <method>`, ...).
    pub what: String,
}

impl std::fmt::Display for DeliveryUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: stdin write deadline exceeded AFTER the frame write had begun; \
             delivery is UNKNOWN — the peer may still execute the request and the \
             result was discarded (do not blindly retry)",
            self.what
        )
    }
}

impl std::error::Error for DeliveryUnknown {}

/// Mint the typed delivery-unknown error for `what`: the frame write had
/// begun, so the peer may still execute the request. `retryable` is FALSE —
/// a blind retry could double-apply the external effect.
pub fn delivery_unknown(what: &str) -> Error {
    Error::new(
        ErrorKind::Provider {
            code: DELIVERY_UNKNOWN_CODE.into(),
            retryable: false,
        },
        DeliveryUnknown { what: what.into() }.to_string(),
    )
}

/// True when `err` is the typed delivery-unknown outcome (match the stable
/// `ErrorKind::Provider` code, never the message): the peer may still execute
/// the timed-out request, so a retry is unsafe without verification.
pub fn is_delivery_unknown(err: &Error) -> bool {
    matches!(&err.kind, ErrorKind::Provider { code, .. } if code.as_str() == DELIVERY_UNKNOWN_CODE)
}

/// Per-frame write state shared between the enqueuing caller and the writer
/// thread. The caller may cancel exactly until the writer claims the frame
/// with [`FrameState::begin_write`]; after that the frame is irrevocable and
/// its delivery is unknown. Both transitions race on ONE CAS, so exactly one
/// of "skipped whole" / "write began" wins — never both, never neither.
pub struct FrameState {
    state: AtomicU8,
}

const FRAME_QUEUED: u8 = 0;
const FRAME_WRITING: u8 = 1;
const FRAME_CANCELLED: u8 = 2;

impl FrameState {
    pub fn queued() -> Self {
        Self {
            state: AtomicU8::new(FRAME_QUEUED),
        }
    }

    /// Writer-side claim, immediately before the first write syscall.
    /// Returns false iff the caller cancelled the frame first (skip it
    /// whole: no byte ever reaches the peer).
    pub fn begin_write(&self) -> bool {
        self.state
            .compare_exchange(
                FRAME_QUEUED,
                FRAME_WRITING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Caller-side cancel at the deadline: true iff the frame was cancelled
    /// before the writer claimed it, i.e. it is guaranteed never to reach
    /// the peer (a clean not-delivered Timeout).
    pub fn cancel_before_write(&self) -> bool {
        self.state
            .compare_exchange(
                FRAME_QUEUED,
                FRAME_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// A fresh queued state wrapped in a drop guard: if the write future is
    /// dropped before it settles (an outer `timeout`/`select!` abandoned the
    /// call), the frame is cancelled while still QUEUED. A frame the writer
    /// already claimed is never touched — the guard's CAS can only succeed
    /// from QUEUED.
    pub fn guarded() -> FrameGuard {
        FrameGuard(Arc::new(Self::queued()))
    }
}

/// Drop guard for [`FrameState`]: see [`FrameState::guarded`]. Dropping it
/// after the write settled is a no-op (the state is already WRITING or
/// CANCELLED).
pub struct FrameGuard(Arc<FrameState>);

impl FrameGuard {
    pub fn state(&self) -> &Arc<FrameState> {
        &self.0
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        let _ = self.0.cancel_before_write();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResult {
    pub content: Vec<serde_json::Value>,
    pub is_error: bool,
}

struct Conn {
    child_pid: u32,
    next_id: u64,
    pending: HashMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>,
    /// False until [`McpServer::initialize`] observed the handshake reply:
    /// the reader bounds accumulated bytes by [`MAX_INITIAL_BYTES`] while
    /// false and by [`MAX_RESPONSE_BYTES`] afterwards.
    handshake_done: bool,
}

/// Bound on the dedicated stdin writer's queue: a child that stops draining
/// must never grow client memory or block a caller unboundedly — the queue
/// fills and further sends are typed refusals.
const WRITER_QUEUE_CAP: usize = 64;

/// One queued stdin frame plus the channel that reports its write+flush
/// outcome. The write itself happens on the dedicated writer thread, NEVER
/// on a tokio worker and NEVER under the `conn` mutex (the reader thread
/// needs that mutex, so holding it across a blocking write could deadlock).
struct WriterMsg {
    bytes: Vec<u8>,
    ack: tokio::sync::oneshot::Sender<std::io::Result<()>>,
    /// Shared with the enqueuing caller: the writer skips the frame iff the
    /// caller cancelled it before the writer's claim CAS.
    frame: Arc<FrameState>,
}

/// Handle to the per-server stdin writer thread (bounded queue; ordering is
/// the queue's FIFO order).
#[derive(Clone)]
struct WriterHandle {
    tx: std::sync::mpsc::SyncSender<WriterMsg>,
}

/// Spawn the writer thread owning the child's stdin. A blocked write (child
/// not draining) blocks only this thread; callers observe a typed refusal
/// (queue full) or a deadline-bounded ack wait. A frame cancelled before the
/// writer claims it is skipped whole — cancellation NEVER reorders the
/// remaining frames.
fn spawn_stdin_writer<W: Write + Send + 'static>(sink: W) -> WriterHandle {
    let (tx, rx) = std::sync::mpsc::sync_channel::<WriterMsg>(WRITER_QUEUE_CAP);
    std::thread::spawn(move || {
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
                let _ = msg.ack.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "frame cancelled before write",
                )));
                continue;
            }
            let result = stdin.write_all(&msg.bytes).and_then(|()| stdin.flush());
            let _ = msg.ack.send(result);
        }
    });
    WriterHandle { tx }
}

impl WriterHandle {
    /// Enqueue one wire frame and wait for its write+flush completion, both
    /// bounded by `deadline_at`. Enqueue is non-blocking (`try_send`): a
    /// full queue is a typed `Oversized` refusal, never a blocked caller.
    ///
    /// Deadline semantics: success means written+flushed; a clean
    /// [`ErrorKind::Timeout`] means the frame was cancelled BEFORE the
    /// writer began it (provably not delivered); a
    /// [`is_delivery_unknown`] error means the writer had already begun the
    /// write, so the peer may still execute the frame.
    async fn write_and_flush(
        &self,
        bytes: Vec<u8>,
        deadline_at: tokio::time::Instant,
        what: &str,
    ) -> Result<(), Error> {
        let frame = FrameState::guarded();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        match self.tx.try_send(WriterMsg {
            bytes,
            ack: ack_tx,
            frame: frame.state().clone(),
        }) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    format!("mcp {what}: stdin writer queue is full (the child is not draining)"),
                ));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                return Err(Error::new(
                    ErrorKind::Network,
                    format!("mcp {what}: stdin writer is gone"),
                ));
            }
        }
        match tokio::time::timeout_at(deadline_at, ack_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            // The combined write+flush failure keeps the historical typed
            // wording the closed-stdin test pins.
            Ok(Ok(Err(e))) => Err(Error::new(
                ErrorKind::Network,
                format!("mcp write {what}: stdin write/flush failed: {e}"),
            )),
            Ok(Err(_)) => Err(Error::new(
                ErrorKind::Network,
                format!("mcp {what}: stdin writer dropped the completion"),
            )),
            Err(_) => {
                // Deadline hit with no ack. Race the writer for the frame:
                // winning the CAS proves the frame will be skipped whole
                // (clean Timeout = not delivered); losing it means the
                // writer had already begun, so the peer may still execute
                // the request and the outcome must say so.
                if frame.state().cancel_before_write() {
                    Err(Error::timeout(format!(
                        "mcp {what}: stdin write exceeded its deadline (the child is not draining)"
                    )))
                } else {
                    Err(delivery_unknown(&format!("mcp {what}")))
                }
            }
        }
    }
}

pub struct McpServer {
    name: String,
    conn: Arc<Mutex<Conn>>,
    writer: WriterHandle,
    supervisor: Arc<ProcessSupervisor>,
}

impl McpServer {
    /// Connect: spawn the server process through the given (shared)
    /// supervisor and perform the initialize handshake with a bounded
    /// timeout. The supervisor owns the child's process group, registry
    /// row and reaping; [`McpServer::close`] / `Drop` kill the group.
    pub async fn connect(
        cfg: McpConfig,
        supervisor: Arc<ProcessSupervisor>,
    ) -> Result<Arc<Self>, Error> {
        // One environment authority: the daemon's platform baseline
        // (PATH/HOME/platform bits, deny-set applied on resolve) plus the
        // server's configured entries (which override by name). The child
        // is env-cleared first — the daemon's full environment never
        // crosses, and secret-shaped names are dropped by the deny-set.
        let mut entries = EnvSpec::default_baseline().resolve();
        entries.extend(cfg.env.iter().map(|(k, v)| (k.into(), v.into())));
        let proc_cfg = SpawnConfig {
            cmd: cfg.command.clone(),
            args: cfg.args.clone(),
            cwd: std::env::temp_dir(),
            env: EnvSpec::Explicit(entries),
            owner: ProcessOwner::Daemon,
            capture: false,
            ..Default::default()
        };
        let spawned = supervisor
            .spawn_detached_with_pipes(proc_cfg)
            .map_err(|e| Error::new(ErrorKind::NotFound, format!("mcp spawn: {e}")))?;
        let writer = spawn_stdin_writer(spawned.stdin);
        let conn = Arc::new(Mutex::new(Conn {
            child_pid: spawned.child_pid,
            next_id: 1,
            pending: HashMap::new(),
            handshake_done: false,
        }));
        // Reader thread: incremental Content-Length framing; responses are
        // dispatched by id; EOF/kill cleans up. The thread owns stdout, so
        // blocking reads can never stall the runtime.
        {
            let conn2 = conn.clone();
            std::thread::spawn(move || {
                read_loop(conn2, spawned.stdout);
            });
        }
        let server = Arc::new(Self {
            name: cfg.name.clone(),
            conn,
            writer,
            supervisor,
        });
        server.initialize().await?;
        Ok(server)
    }

    fn next_request(&self, method: &str, params: serde_json::Value) -> (String, serde_json::Value) {
        let mut conn = recover_lock(&self.conn);
        let id = conn.next_id;
        conn.next_id += 1;
        (
            id.to_string(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }),
        )
    }

    /// Send a request and wait for its response (bounded by deadline).
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        deadline: Duration,
    ) -> Result<serde_json::Value, Error> {
        // ONE deadline covers the write AND the response wait: a child that
        // stops draining can never extend the call past it.
        let deadline_at = tokio::time::Instant::now() + deadline;
        let (id, request) = self.next_request(method, params);
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut conn = recover_lock(&self.conn);
            if conn.pending.len() > 256 {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    "too many in-flight MCP requests",
                ));
            }
            // Register BEFORE the frame can reach the child: a fast response
            // is dispatched by the reader thread, which only needs this
            // mutex briefly (never across a blocking write).
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
        // Reader loop (see `read_loop`); here we wait with the same deadline.
        match tokio::time::timeout_at(deadline_at, rx).await {
            Ok(Ok(response)) => {
                if let Some(err) = response.get("error") {
                    // Reader-originated failures are INTERNAL (the error
                    // object is minted by `read_loop`, never parsed from the
                    // server's own frame): the oversized marker keeps the
                    // typed bound failure instead of degrading it to a
                    // generic provider error.
                    let kind = if err
                        .get("data")
                        .and_then(|data| data.get("faktor_kind"))
                        .and_then(|kind| kind.as_str())
                        == Some(OVERSIZED_MARKER)
                    {
                        ErrorKind::Oversized
                    } else {
                        ErrorKind::Provider {
                            code: "mcp".into(),
                            retryable: false,
                        }
                    };
                    return Err(Error::new(kind, format!("mcp {method}: {err}")));
                }
                Ok(response
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null))
            }
            Ok(Err(_)) => Err(Error::new(
                ErrorKind::Network,
                format!("mcp {method}: connection dropped"),
            )),
            Err(_elapsed) => {
                // Clean up the pending entry (brief lock; the writer thread
                // and the reader thread never hold it across blocking I/O).
                recover_lock(&self.conn).pending.remove(&id);
                Err(Error::timeout(format!(
                    "mcp {method} exceeded {}ms",
                    deadline.as_millis()
                )))
            }
        }
    }

    async fn initialize(&self) -> Result<(), Error> {
        let result = self
            .call(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "faktor-plus", "version": "0.1.0" },
                }),
                Duration::from_secs(10),
            )
            .await?;
        let _ = result;
        // The handshake reply was observed: from here the reader applies the
        // full response bound instead of the initial handshake bound.
        recover_lock(&self.conn).handshake_done = true;
        // Notify initialized (fire and forget; never blocks the runtime).
        let _ = self
            .call(
                "notifications/initialized",
                serde_json::json!({}),
                Duration::from_secs(2),
            )
            .await;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpTool>, Error> {
        let result = self
            .call("tools/list", serde_json::json!({}), Duration::from_secs(10))
            .await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for t in tools {
            out.push(McpTool {
                name: t
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: t
                    .get("description")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            });
        }
        Ok(out)
    }

    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
        deadline: Duration,
    ) -> Result<McpResult, Error> {
        let result = self
            .call(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": args }),
                deadline,
            )
            .await?;
        Ok(McpResult {
            content: result
                .get("content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default(),
            is_error: result
                .get("isError")
                .and_then(|e| e.as_bool())
                .unwrap_or(false),
        })
    }

    pub async fn close(&self) -> Result<(), Error> {
        let pid = recover_lock(&self.conn).child_pid;
        // Best-effort empty frame, bounded: a wedged child must not extend
        // shutdown past the kill that follows.
        let _ = self
            .writer
            .write_and_flush(
                b"Content-Length: 0\r\n\r\n".to_vec(),
                tokio::time::Instant::now() + Duration::from_millis(500),
                "close",
            )
            .await;
        // Kill via the supervisor (process-group aware).
        let _ = self.supervisor.kill_child_pid(pid, 1000);
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_alive(&self) -> bool {
        let pid = recover_lock(&self.conn).child_pid;
        self.supervisor.pid_alive(pid)
    }
}

impl Drop for McpServer {
    /// Zero orphans: dropping the client kills the server process.
    fn drop(&mut self) {
        let pid = match self.conn.lock() {
            Ok(c) => c.child_pid,
            Err(_) => return,
        };
        // Best-effort empty frame (never blocking Drop on a wedged child);
        // the ack receiver is dropped with the message, which the writer
        // tolerates. The kill below is the guarantee.
        let (ack, _) = tokio::sync::oneshot::channel();
        let _ = self.writer.tx.try_send(WriterMsg {
            bytes: b"Content-Length: 0\r\n\r\n".to_vec(),
            ack,
            frame: Arc::new(FrameState::queued()),
        });
        let _ = self.supervisor.kill_child_pid(pid, 300);
    }
}

/// Incremental frame reader: accumulates bytes, parses Content-Length
/// frames, dispatches responses by id, and drops pending senders on EOF.
fn read_loop(conn: Arc<Mutex<Conn>>, stdout: std::process::ChildStdout) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // Read one chunk.
        let mut chunk = [0u8; 8192];
        use std::io::Read;
        let n = match reader.read(&mut chunk) {
            Ok(0) => break, // EOF: server died or closed
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&chunk[..n]);
        if recover_lock(&conn).handshake_done {
            if buf.len() > MAX_RESPONSE_BYTES {
                break; // hostile server: stop reading
            }
        } else if buf.len() > MAX_INITIAL_BYTES {
            // The INITIAL handshake read is bounded separately: fail every
            // pending call with the typed limit (never a multi-megabyte
            // buffer before capabilities are negotiated) and stop reading.
            let mut guard = recover_lock(&conn);
            for (_, tx) in guard.pending.drain() {
                let _ = tx.send(serde_json::json!({
                    "error": {
                        "code": -32002,
                        "message": format!(
                            "the initial handshake exceeds the {MAX_INITIAL_BYTES}-byte MAX_INITIAL_BYTES bound"
                        ),
                        "data": { "faktor_kind": OVERSIZED_MARKER },
                    }
                }));
            }
            return;
        }
        // Parse as many complete frames as available.
        loop {
            if buf.is_empty() {
                break; // fully drained: wait for the next chunk
            }
            match parse_frame(&buf) {
                Ok(Some((consumed, value))) => {
                    buf.drain(..consumed);
                    // Servers echo the request id verbatim: the client sends
                    // NUMERIC ids, so a numeric echo must match the string
                    // pending key ("1" == 1). (Latent bug: numeric echoes
                    // never matched and every successful call timed out.)
                    let id = value.get("id").and_then(|i| match i {
                        serde_json::Value::String(s) => Some(s.clone()),
                        serde_json::Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    });
                    let is_notification =
                        value.get("method").is_some() && value.get("id").is_none();
                    if let Some(id) = id {
                        let mut guard = recover_lock(&conn);
                        if let Some(tx) = guard.pending.remove(&id) {
                            let _ = tx.send(value);
                        }
                    } else if is_notification {
                        // Unsolicited notifications are ignored.
                    }
                }
                Ok(None) => break, // incomplete frame: wait for more bytes
                Err(_) => {
                    // Garbage on the wire: drop everything pending (the
                    // server is broken) and stop reading.
                    let mut guard = recover_lock(&conn);
                    for (_, tx) in guard.pending.drain() {
                        let _ = tx.send(serde_json::json!({"error": {"code": -32700, "message": "parse error"}}));
                    }
                    return;
                }
            }
        }
    }
    // EOF: fail all pending requests.
    let mut guard = recover_lock(&conn);
    for (_, tx) in guard.pending.drain() {
        let _ = tx.send(serde_json::json!({"error": {"code": -32000, "message": "server closed"}}));
    }
}

/// Pure JSON-RPC framing parser (Content-Length headers), unit-tested
/// adversarially without any process.
pub fn parse_frame(bytes: &[u8]) -> Result<Option<(usize, serde_json::Value)>, String> {
    // Returns Ok(Some((consumed, message))) or Ok(None) when incomplete.
    if bytes.is_empty() {
        return Ok(None); // empty is incomplete, never a parse error
    }
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("response exceeds 16MB bound".into());
    }
    let header_end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no header terminator")?;
    let header = std::str::from_utf8(&bytes[..header_end]).map_err(|_| "header not utf8")?;
    let mut content_length: Option<usize> = None;
    for line in header.split("\r\n") {
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = Some(
                v.trim()
                    .parse::<usize>()
                    .map_err(|_| "bad Content-Length")?,
            );
        }
    }
    let content_length = content_length.ok_or("missing Content-Length")?;
    if content_length > MAX_RESPONSE_BYTES {
        return Err("declared Content-Length exceeds bound".into());
    }
    let body_start = header_end + 4;
    let total = body_start + content_length;
    if bytes.len() < total {
        return Ok(None); // incomplete
    }
    let body = std::str::from_utf8(&bytes[body_start..total]).map_err(|_| "body not utf8")?;
    let value = serde_json::from_str(body).map_err(|e| format!("invalid jsonrpc: {e}"))?;
    Ok(Some((total, value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn framing_roundtrip() {
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        let body = msg.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let (consumed, parsed) = parse_frame(frame.as_bytes()).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(parsed, msg);
        // Multiple frames concatenated: parse consumes exactly the first.
        let double = format!("{frame}{frame}");
        let (consumed, _) = parse_frame(double.as_bytes()).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
    }

    #[test]
    fn framing_garbage_rejected() {
        // An empty buffer is incomplete (a fully-drained reader must not
        // treat it as wire garbage — that bug dropped healthy frames).
        assert!(parse_frame(b"").unwrap().is_none(), "empty is incomplete");
        // 3 of 5 declared bytes: incomplete frame, not an error.
        assert!(
            parse_frame(b"Content-Length: 5\r\n\r\nhel")
                .unwrap()
                .is_none(),
            "incomplete is None not error"
        );
        assert!(parse_frame(b"Content-Length: -1\r\n\r\n").is_err());
        assert!(parse_frame(b"Content-Length: 99999999999999999999\r\n\r\n").is_err());
        assert!(parse_frame(b"Content-Length: abc\r\n\r\n").is_err());
        let mismatched = format!("Content-Length: 5\r\n\r\n{}\r\n\r\n", "x".repeat(100));
        assert!(
            parse_frame(mismatched.as_bytes()).is_err(),
            "mismatched length"
        );
        assert!(
            parse_frame(b"Content-Length: 5\r\n\r\n\xff\xfe\x00\x01\x02").is_err(),
            "invalid utf8 body"
        );
        let big = format!("Content-Length: {}\r\n\r\n", MAX_RESPONSE_BYTES + 1);
        assert!(parse_frame(big.as_bytes()).is_err(), "declared size bound");
    }

    #[test]
    fn framing_bad_json_rejected() {
        let body = "{not json";
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        assert!(parse_frame(frame.as_bytes()).is_err());
    }

    #[tokio::test]
    async fn close_terminates_process() {
        // Use a real MCP-ish server: a python one-liner that reads one frame
        // and exits. Skip gracefully if python3 is absent.
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 missing; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let script = r#"
import sys
line = sys.stdin.readline()
while line:
    line = sys.stdin.readline()
sys.exit(0)
"#;
        let cfg = McpConfig {
            name: "mock".into(),
            command: "python3".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
        };
        // connect() will time out on initialize (the mock never answers);
        // verify the process is killed and no zombie remains.
        let server = McpServer::connect(cfg, sup.clone()).await;
        assert!(server.is_err(), "mock never initializes");
        // The supervisor must have reaped the child (no zombie).
        std::thread::sleep(Duration::from_millis(300));
        assert!(sup.reap().is_empty() || sup.registered() == 0);
    }

    #[tokio::test]
    async fn garbage_server_is_malformed_not_hang() {
        // A server that emits garbage on stdout: framing must fail fast.
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let script = "import sys, time\nwhile True:\n    sys.stdout.write('garbage\\n')\n    sys.stdout.flush()\n    time.sleep(0.01)\n";
        let cfg = McpConfig {
            name: "garbage".into(),
            command: "python3".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
        };
        let server = McpServer::connect(cfg, sup.clone()).await;
        // Either initialize fails (timeout → error) or the server is dead;
        // never a hang.
        let _ = server;
    }

    #[test]
    fn response_bound_is_enforced() {
        // A hostile 17MB frame is rejected by the parser.
        let body = "x".repeat(MAX_RESPONSE_BYTES + 1);
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let r = parse_frame(frame.as_bytes());
        assert!(r.is_err());
    }

    /// The INITIAL handshake read is bounded by `MAX_INITIAL_BYTES`, not the
    /// 16MB response cap: a server that answers `initialize` with a huge
    /// frame is refused with the typed bound error NAMING the limit (and the
    /// child is killed — no orphan), never buffered.
    #[tokio::test]
    async fn oversized_initial_handshake_is_refused_typed_and_bounded() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 missing; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let pad = MAX_INITIAL_BYTES * 2;
        let script = format!(
            r#"
import sys, time
body = ('{{"jsonrpc":"2.0","id":1,"result":{{"pad":"' + 'x' * {pad} + '"}}}}').encode()
sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
sys.stdout.buffer.write(body)
sys.stdout.buffer.flush()
while True:
    time.sleep(0.05)
"#
        );
        let cfg = McpConfig {
            name: "oversized-init".into(),
            command: "python3".into(),
            args: vec!["-c".into(), script],
            env: vec![],
        };
        let err = match McpServer::connect(cfg, sup.clone()).await {
            Ok(_) => panic!("an oversized initial handshake must refuse the connection"),
            Err(err) => err,
        };
        assert_eq!(err.kind, ErrorKind::Oversized, "{err:?}");
        assert!(
            err.message.contains(&MAX_INITIAL_BYTES.to_string()),
            "the typed error must name the limit: {err:?}"
        );
        assert!(
            err.message.contains("MAX_INITIAL_BYTES"),
            "the typed error must name the bound: {err:?}"
        );
        // Zero orphans: the refused connection's child is killed by Drop.
        std::thread::sleep(Duration::from_millis(300));
        assert!(sup.reap().is_empty() || sup.registered() == 0);
    }

    /// A child that answers exactly one frame, then closes its stdin and
    /// keeps running: the NEXT call must fail with the typed transport
    /// error BEFORE any pending entry exists. The old ignored-flush path
    /// registered the request and turned the failure into a deadline
    /// timeout instead.
    #[tokio::test]
    async fn closed_stdin_surfaces_typed_error_without_pending_entry() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 missing; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let script = r#"
import json, os, sys, time

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

msg = read_msg()
body = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {}}).encode("utf-8")
sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
sys.stdout.buffer.write(body)
sys.stdout.buffer.flush()
# Close the read end of the pipe: os.close on the FD — closing the sys.stdin
# wrapper would NOT close the descriptor.
os.close(sys.stdin.fileno())
while True:
    time.sleep(0.05)
"#;
        let cfg = McpConfig {
            name: "closes-stdin".into(),
            command: "python3".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
        };
        let server = McpServer::connect(cfg, sup.clone()).await.unwrap();
        let t0 = std::time::Instant::now();
        let err = server
            .call("tools/list", serde_json::json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Network, "{err:?}");
        assert!(
            err.message.contains("flush failed") || err.message.contains("mcp write"),
            "the typed transport error must name the failed write/flush: {err:?}"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "a closed stdin must fail fast, never wait out the deadline: {:?}",
            t0.elapsed()
        );
        assert!(
            server.conn.lock().unwrap().pending.is_empty(),
            "no pending entry may be registered for a failed write"
        );
    }

    /// A child that answers the handshake and then STOPS draining stdin: a
    /// large write would previously block the async caller under the `conn`
    /// mutex while the reader thread waited on the same mutex — a deadlock
    /// that no request deadline could cover. The dedicated writer thread +
    /// bounded queue must turn it into a bounded typed error, with the mutex
    /// and reader still usable.
    ///
    /// Delivery semantics: the writer had already CLAIMED this single frame
    /// (the queue was empty, so it went straight from the FIFO to the pipe
    /// and blocked), so the deadline must NOT masquerade as a clean
    /// not-delivered Timeout — the child may still execute it once it
    /// resumes draining.
    #[tokio::test]
    async fn a_child_that_stops_draining_is_delivery_unknown_not_a_deadlock() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 missing; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let script = r#"
import json, sys, time

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

msg = read_msg()
body = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {}}).encode("utf-8")
sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
sys.stdout.buffer.write(body)
sys.stdout.buffer.flush()
# STOP draining stdin: the next large write fills the pipe and blocks.
while True:
    time.sleep(0.05)
"#;
        let cfg = McpConfig {
            name: "stops-draining".into(),
            command: "python3".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
        };
        let server = McpServer::connect(cfg, sup.clone()).await.unwrap();
        let payload = "x".repeat(4 * 1024 * 1024);
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            server.call(
                "tools/call",
                serde_json::json!({ "pad": payload }),
                Duration::from_millis(300),
            ),
        )
        .await
        .expect("a non-draining child must not hang the caller")
        .unwrap_err();
        assert!(
            is_delivery_unknown(&err),
            "a claimed-but-unacked write must be delivery-unknown, never a clean Timeout: {err:?}"
        );
        assert_ne!(err.kind, ErrorKind::Timeout, "{err:?}");
        assert!(
            !err.retryable,
            "delivery-unknown must never be blindly retryable: {err:?}"
        );
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
            server.conn.lock().unwrap().pending.is_empty(),
            "the timed-out call must not leave a pending entry"
        );
        // The conn mutex and reader thread remain usable (no deadlock).
        let _ = server.is_alive();
    }

    /// A deterministic stand-in for a stalled child pipe: the FIRST write
    /// blocks until the gate opens, then every write is appended to a byte
    /// log (so delivered frames can be parsed back and ordered). Later writes
    /// pass straight through once released — a fake child that drains later.
    struct GatedSink {
        byte_log: Arc<Mutex<Vec<u8>>>,
        entered: Arc<std::sync::atomic::AtomicBool>,
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl Write for GatedSink {
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

    fn release_gate(gate: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
        let (open, cv) = &**gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }

    /// One logical wire frame for id `id`.
    fn logical_frame(id: u64, method: &str) -> Vec<u8> {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method}).to_string();
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    /// Parse the sink's byte log back into delivered frame ids.
    fn delivered_ids(bytes: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let (consumed, value) = parse_frame(rest)
                .expect("recorded bytes must be complete frames")
                .expect("recorded bytes must hold complete frames");
            out.push(value["id"].as_u64().expect("frames carry numeric ids"));
            rest = &rest[consumed..];
        }
        out
    }

    /// Adversarial core of the delivery fix, deterministic (no process):
    /// - the frame the writer CLAIMED before the deadline is flagged
    ///   delivery-unknown (it is written when the fake child resumes);
    /// - frames still queued behind it are cancelled at their deadline and
    ///   skipped WHOLE — a clean Timeout each, and they never appear on the
    ///   child's stdin;
    /// - a non-cancelled frame enqueued between cancelled ones keeps FIFO
    ///   order;
    /// - everything completes in bounded time (the outer timeout fails the
    ///   test instead of hanging).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_frames_are_cancelled_whole_or_flagged_delivery_unknown() {
        let byte_log = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let writer = spawn_stdin_writer(GatedSink {
            byte_log: byte_log.clone(),
            entered: entered.clone(),
            gate: gate.clone(),
        });

        // Frame 1 is claimed immediately and blocks inside the fake child.
        let head = tokio::spawn({
            let writer = writer.clone();
            async move {
                writer
                    .write_and_flush(
                        logical_frame(1, "head"),
                        tokio::time::Instant::now() + Duration::from_millis(150),
                        "head",
                    )
                    .await
            }
        });
        let t0 = std::time::Instant::now();
        while !entered.load(Ordering::SeqCst) {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "the writer never reached the fake child"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Frames 2..=4 queue behind the blocked head: their deadlines cancel
        // them before the writer can claim them => clean, not-delivered
        // Timeouts.
        for id in 2..=4u64 {
            let err = writer
                .write_and_flush(
                    logical_frame(id, "burst"),
                    tokio::time::Instant::now() + Duration::from_millis(50),
                    "burst",
                )
                .await
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::Timeout, "frame {id}: {err:?}");
            assert!(
                !is_delivery_unknown(&err),
                "queued frame {id} must be provably not delivered: {err:?}"
            );
        }

        // The head's deadline expired while the writer held it: that MUST be
        // delivery-unknown, not a clean Timeout.
        let head_err = tokio::time::timeout(Duration::from_secs(5), head)
            .await
            .expect("head write must settle in bounded time")
            .expect("head task must not panic")
            .unwrap_err();
        assert!(
            is_delivery_unknown(&head_err) && !head_err.retryable,
            "the claimed frame must be delivery-unknown: {head_err:?}"
        );

        // Frame 5 is enqueued AFTER the cancelled frames but with a live
        // deadline: it must be written in order once the child resumes, and
        // the three cancelled frames must be skipped whole around it.
        let tail = tokio::spawn({
            let writer = writer.clone();
            async move {
                writer
                    .write_and_flush(
                        logical_frame(5, "tail"),
                        tokio::time::Instant::now() + Duration::from_secs(10),
                        "tail",
                    )
                    .await
            }
        });
        release_gate(&gate);
        let tail_result = tokio::time::timeout(Duration::from_secs(5), tail)
            .await
            .expect("tail write must settle")
            .expect("tail task must not panic");
        assert!(tail_result.is_ok(), "{tail_result:?}");

        let recorded = byte_log.lock().unwrap().clone();
        let ids = delivered_ids(&recorded);
        assert_eq!(
            ids,
            vec![1, 5],
            "only the claimed frame and the live tail may reach the child; \
             cancelled frames are skipped without reordering: {ids:?}"
        );
        assert!(
            !ids.contains(&2) && !ids.contains(&3) && !ids.contains(&4),
            "a cancelled-before-write frame must never appear on stdin: {ids:?}"
        );
    }

    /// A blocked writer + a full bounded FIFO: the next send is the typed
    /// `Oversized` refusal (never a blocked caller), including after the
    /// fix. Bounded time by construction (`try_send`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_writer_queue_is_a_typed_oversized_refusal() {
        let byte_log = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let writer = spawn_stdin_writer(GatedSink {
            byte_log,
            entered: entered.clone(),
            gate: gate.clone(),
        });
        // Occupy the writer with a frame it claims and blocks on.
        let head = tokio::spawn({
            let writer = writer.clone();
            async move {
                writer
                    .write_and_flush(
                        logical_frame(1, "head"),
                        tokio::time::Instant::now() + Duration::from_secs(30),
                        "head",
                    )
                    .await
            }
        });
        let t0 = std::time::Instant::now();
        while !entered.load(Ordering::SeqCst) {
            assert!(t0.elapsed() < Duration::from_secs(5));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Fill the bounded FIFO exactly (the writer already took the head).
        for id in 2..(2 + WRITER_QUEUE_CAP as u64) {
            let (ack, _rx) = tokio::sync::oneshot::channel();
            writer
                .tx
                .try_send(WriterMsg {
                    bytes: logical_frame(id, "queued"),
                    ack,
                    frame: Arc::new(FrameState::queued()),
                })
                .expect("the bounded FIFO must accept exactly CAP frames");
        }
        let err = writer
            .write_and_flush(
                logical_frame(999, "overflow"),
                tokio::time::Instant::now() + Duration::from_millis(200),
                "overflow",
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Oversized, "{err:?}");
        assert!(!is_delivery_unknown(&err), "{err:?}");
        release_gate(&gate);
        let _ = tokio::time::timeout(Duration::from_secs(10), head).await;
    }

    /// An abandoned write future (an outer timeout/`select!` dropped it) must
    /// not leave its frame in the FIFO to execute later: the drop guard
    /// cancels it while still queued, so it never reaches the fake child and
    /// the remaining live frame keeps FIFO order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandoned_write_future_never_executes_its_frame() {
        let byte_log = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let writer = spawn_stdin_writer(GatedSink {
            byte_log: byte_log.clone(),
            entered: entered.clone(),
            gate: gate.clone(),
        });
        let head = tokio::spawn({
            let writer = writer.clone();
            async move {
                writer
                    .write_and_flush(
                        logical_frame(1, "head"),
                        tokio::time::Instant::now() + Duration::from_secs(30),
                        "head",
                    )
                    .await
            }
        });
        let t0 = std::time::Instant::now();
        while !entered.load(Ordering::SeqCst) {
            assert!(t0.elapsed() < Duration::from_secs(5));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Enqueue a frame behind the blocked head, poll it exactly once (the
        // enqueue), then drop the still-pending future — exactly what an
        // outer timeout/`select!` does.
        use futures::FutureExt;
        let abandoned = writer
            .write_and_flush(
                logical_frame(2, "abandoned"),
                tokio::time::Instant::now() + Duration::from_secs(30),
                "abandoned",
            )
            .now_or_never();
        assert!(
            abandoned.is_none(),
            "the abandoned write must still have been pending (enqueued)"
        );
        // A live frame enqueued after the abandoned one.
        let tail = tokio::spawn({
            let writer = writer.clone();
            async move {
                writer
                    .write_and_flush(
                        logical_frame(3, "tail"),
                        tokio::time::Instant::now() + Duration::from_secs(10),
                        "tail",
                    )
                    .await
            }
        });
        release_gate(&gate);
        let tail_result = tokio::time::timeout(Duration::from_secs(5), tail)
            .await
            .expect("tail must settle")
            .expect("tail must not panic");
        assert!(tail_result.is_ok(), "{tail_result:?}");
        let head_result = tokio::time::timeout(Duration::from_secs(5), head)
            .await
            .expect("head must settle")
            .expect("head must not panic");
        assert!(head_result.is_ok(), "{head_result:?}");
        let ids = delivered_ids(&byte_log.lock().unwrap().clone());
        assert_eq!(
            ids,
            vec![1, 3],
            "an abandoned queued frame must be skipped whole: {ids:?}"
        );
    }

    /// The audit's process-level scenario: a real child stalls (never drains)
    /// while a large frame blocks the writer; a burst then times out. On
    /// resume the child must have executed ONLY the blocking frame plus a
    /// live-deadline frame that kept its FIFO position — every cancelled
    /// burst frame is skipped whole (clean Timeout, never delivered), and
    /// the client never deadlocks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stalled_child_burst_is_cleanly_cancelled_and_never_executed() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 missing; skipping");
            return;
        }
        let script = r#"
import json, sys, time

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

def send(obj):
    body = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

executed = []
while True:
    msg = read_msg()
    if msg is None:
        break
    method = msg.get("method")
    if method is None:
        continue
    if method == "stall":
        # Reset the transcript to the post-handshake baseline, answer, then
        # STOP draining stdin for 1.5s: the large frame behind this response
        # fills the pipe and blocks the client's writer.
        executed = ["stall"]
        if "id" in msg:
            send({"jsonrpc": "2.0", "id": msg["id"], "result": {"executed": executed}})
        time.sleep(1.5)
        continue
    executed.append(method)
    if "id" in msg:
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"executed": executed}})
"#;
        let dir = tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let sup = ProcessSupervisor::new(cas);
        let cfg = McpConfig {
            name: "stall-burst".into(),
            command: "python3".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
        };
        let server = tokio::time::timeout(
            Duration::from_secs(15),
            McpServer::connect(cfg, sup.clone()),
        )
        .await
        .expect("connect must not hang")
        .unwrap();

        // Prove the child is alive and responsive before stalling it.
        let stall = server
            .call("stall", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect("stall ack");
        assert_eq!(stall["executed"], serde_json::json!(["stall"]));

        // The large frame claims the writer and blocks in the pipe while the
        // child sleeps.
        let big = "x".repeat(4 * 1024 * 1024);
        let big_task = tokio::spawn({
            let server = server.clone();
            async move {
                server
                    .call(
                        "tools/call",
                        serde_json::json!({ "pad": big }),
                        Duration::from_secs(30),
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        // A live-deadline frame enqueued BETWEEN the blocked head and the
        // cancelling burst: it must survive and keep its FIFO position.
        let keep_task = tokio::spawn({
            let server = server.clone();
            async move {
                server
                    .call("mcp_keep", serde_json::json!({}), Duration::from_secs(30))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Burst: all queued behind the blocked head, all deadlines far below
        // the child's 1.5s stall. Each must be a CLEAN not-delivered Timeout
        // — the old code let them sit in the FIFO and execute on resume.
        let mut burst = Vec::new();
        for i in 0..8u32 {
            let server = server.clone();
            burst.push(tokio::spawn(async move {
                server
                    .call(
                        &format!("mcp_burst_{i}"),
                        serde_json::json!({}),
                        Duration::from_millis(250),
                    )
                    .await
            }));
        }
        for handle in burst {
            let err = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("burst call must settle in bounded time")
                .expect("burst task must not panic")
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
            assert!(
                !is_delivery_unknown(&err),
                "a queued burst frame must be provably not delivered: {err:?}"
            );
        }

        // The child resumes and the child-side transcript proves exactly
        // which frames were executed, in order.
        let big_result = tokio::time::timeout(Duration::from_secs(30), big_task)
            .await
            .expect("the blocked frame must complete after resume")
            .expect("big task must not panic")
            .expect("the big frame must be delivered");
        assert_eq!(
            big_result["executed"],
            serde_json::json!(["stall", "tools/call"]),
            "cancelled burst frames must never be executed by the child"
        );
        let keep_result = tokio::time::timeout(Duration::from_secs(10), keep_task)
            .await
            .expect("the live frame must complete")
            .expect("keep task must not panic")
            .expect("the live frame must be delivered");
        assert_eq!(
            keep_result["executed"],
            serde_json::json!(["stall", "tools/call", "mcp_keep"]),
            "a non-cancelled frame between cancelled frames keeps FIFO order"
        );
        let after = server
            .call("mcp_after", serde_json::json!({}), Duration::from_secs(10))
            .await
            .expect("the connection must remain usable");
        assert_eq!(
            after["executed"],
            serde_json::json!(["stall", "tools/call", "mcp_keep", "mcp_after"])
        );
        assert!(server.conn.lock().unwrap().pending.is_empty());
        let _ = server.close().await;
    }

    /// P0-40 static certification: no production path in this crate
    /// constructs its own process `Command` or its own `ProcessSupervisor`
    /// — the stdio MCP subprocess is spawned exclusively through the
    /// supervisor handed into [`McpServer::connect`] (the daemon passes one
    /// shared supervisor for every server). Scans the production section of
    /// this file (cut at the first `#[cfg(test)]`), wave-17 egress style.
    #[test]
    fn production_paths_never_construct_a_command_or_supervisor() {
        const MARKERS: [&str; 3] = ["Command::new", "Command::spawn", "ProcessSupervisor::new"];
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
        // Stable-snapshot read (parallel sessions may edit this file while
        // the full workspace suite runs; scanning a half-written file
        // produced phantom offenders). Never scan an unstable file.
        let mut source = std::fs::read_to_string(&path).unwrap();
        let mut stable = false;
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_millis(15));
            if let Ok(second) = std::fs::read_to_string(&path) {
                if second == source {
                    stable = true;
                    break;
                }
                source = second;
            }
        }
        assert!(
            stable,
            "src/lib.rs kept changing while the spawn-authority scan ran \
             (concurrent writer); refusing to scan an unstable file"
        );
        let production = source.split("#[cfg(test)]").next().unwrap();
        let mut offenders: Vec<String> = Vec::new();
        for (idx, line) in production.lines().enumerate() {
            if MARKERS.iter().any(|m| line.contains(m)) {
                offenders.push(format!("{}: {}", idx + 1, line.trim()));
            }
        }
        assert!(
            offenders.is_empty(),
            "MCP must spawn subprocesses only through the shared supervisor:\n  {}",
            offenders.join("\n  ")
        );
    }
}
