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
use std::process::ChildStdin;
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
    /// Buffered: the explicit `flush()` is the real pipe write, so a closed
    /// or broken child stdin fails the caller typed instead of leaving a
    /// pending entry to die on its deadline.
    stdin: BufWriter<ChildStdin>,
    next_id: u64,
    pending: HashMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>,
    /// False until [`McpServer::initialize`] observed the handshake reply:
    /// the reader bounds accumulated bytes by [`MAX_INITIAL_BYTES`] while
    /// false and by [`MAX_RESPONSE_BYTES`] afterwards.
    handshake_done: bool,
}

pub struct McpServer {
    name: String,
    conn: Arc<Mutex<Conn>>,
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
        let conn = Arc::new(Mutex::new(Conn {
            child_pid: spawned.child_pid,
            stdin: BufWriter::new(spawned.stdin),
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
            let wire = format!(
                "Content-Length: {}\r\n\r\n{}",
                request.to_string().len(),
                request
            );
            conn.stdin
                .write_all(wire.as_bytes())
                .map_err(|e| Error::new(ErrorKind::Network, format!("mcp write: {e}")))?;
            // Propagate the FLUSH failure BEFORE registering the pending
            // entry: a closed/broken child stdin is a typed transport error
            // NOW, never a call that waits out its deadline.
            conn.stdin.flush().map_err(|e| {
                Error::new(
                    ErrorKind::Network,
                    format!("mcp {method}: stdin flush failed: {e}"),
                )
            })?;
            conn.pending.insert(id.clone(), tx);
        }
        // Reader loop (see `read_loop`); here we wait with a deadline.
        match tokio::time::timeout(deadline, rx).await {
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
                // Clean up the pending entry.
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
        let pid = {
            let mut conn = recover_lock(&self.conn);
            let _ = conn.stdin.write_all(b"Content-Length: 0\r\n\r\n");
            let _ = conn.stdin.flush();
            conn.child_pid
        };
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
        let pid = {
            let mut conn = match self.conn.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            let _ = conn.stdin.write_all(b"Content-Length: 0\r\n\r\n");
            conn.child_pid
        };
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
