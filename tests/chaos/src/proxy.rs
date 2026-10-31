//! Local chaos proxy between the daemon's provider transport and an
//! upstream provider endpoint (a real endpoint or the in-repo mock).
//!
//! Pure std + tokio TCP: no HTTP library, no new heavy dependency. One
//! connection is proxied sequentially (the provider transport issues one
//! request per connection — the mock/proxy answer `Connection: close`), so
//! every fault is applied to a fully deterministic byte sequence.
//!
//! Fault classes (HAR-01/HAR-03 transport chaos):
//!
//! | flag                | injection                                              |
//! |---------------------|--------------------------------------------------------|
//! | `kill_mid_sse`      | abrupt socket drop after N response BODY bytes (mid-SSE) |
//! | `truncate`          | clean write-side FIN after N response BODY bytes        |
//! | `reorder`           | split the body at a deterministic mid-frame offset and swap the halves |
//! | `delay_burst`       | one deterministic delay burst once N body bytes are out  |
//! | `rst_after`         | SO_LINGER=0 close (TCP RST) after N response BODY bytes  |
//! | `gzip_truncate`     | re-encode the body as gzip and truncate it mid-stream    |
//!
//! Determinism: every threshold, delay and swap index is a fixed function
//! of the parsed spec; [`FaultSpec::DEFAULT_SEED`] seeds the only derived
//! values, and the same spec replays the same fault sequence on every run.
//! `max_fires` makes a fault one-shot (the retry recovers) or persistent
//! (the retry budget must bound the attempts).
//!
//! Spec strings look like `kill_mid_sse:200` (`delay_burst:64:750` carries
//! its delay; `reorder:1` carries the frame index). The same strings are
//! accepted from the `FAKTOR_CHAOS_FAULT` environment variable through
//! [`FaultSpec::from_env`] so an operator can arm the proxy by flag.
// TEST-SEPARATION: oracle=independent-reference isolation=separate-process

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Fixed default seed: the derived values never vary between runs unless
/// the operator explicitly overrides them.
pub const DEFAULT_SEED: u64 = 0xFA70_2026;

/// One injectable transport fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultClass {
    KillMidSse,
    Truncate,
    Reorder,
    DelayBurst,
    RstAfter,
    GzipTruncate,
}

/// A parsed, fully deterministic fault script.
#[derive(Debug, Clone, Copy)]
pub struct FaultSpec {
    pub class: FaultClass,
    /// Response BODY byte threshold (or frame index for `reorder`).
    pub threshold: usize,
    /// Delay of the `delay_burst` class, milliseconds.
    pub delay_ms: u64,
    /// How many connections the fault may fire on (0 = every connection).
    pub max_fires: u32,
    pub seed: u64,
}

impl FaultSpec {
    pub const DEFAULT_SEED: u64 = DEFAULT_SEED;

    /// Parse `class:threshold[:delay_ms][:max_fires]`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.trim().split(':').collect();
        if parts.is_empty() || parts.len() > 4 {
            return Err(format!(
                "fault spec {text:?}: expected class[:threshold[:delay[:fires]]]"
            ));
        }
        let class = match parts[0] {
            "kill_mid_sse" => FaultClass::KillMidSse,
            "truncate" => FaultClass::Truncate,
            "reorder" => FaultClass::Reorder,
            "delay_burst" => FaultClass::DelayBurst,
            "rst_after" => FaultClass::RstAfter,
            "gzip_truncate" => FaultClass::GzipTruncate,
            other => return Err(format!("unknown fault class {other:?}")),
        };
        let threshold = if parts.len() > 1 && !parts[1].is_empty() {
            parts[1]
                .parse::<usize>()
                .map_err(|e| format!("bad threshold in {text:?}: {e}"))?
        } else {
            128
        };
        let delay_ms = if parts.len() > 2 && !parts[2].is_empty() {
            parts[2]
                .parse::<u64>()
                .map_err(|e| format!("bad delay in {text:?}: {e}"))?
        } else {
            0
        };
        let max_fires = if parts.len() > 3 && !parts[3].is_empty() {
            parts[3]
                .parse::<u32>()
                .map_err(|e| format!("bad fire count in {text:?}: {e}"))?
        } else {
            0
        };
        Ok(Self {
            class,
            threshold,
            delay_ms,
            max_fires,
            seed: DEFAULT_SEED,
        })
    }

    /// Read the spec from `FAKTOR_CHAOS_FAULT` (absent = `None`).
    pub fn from_env() -> Option<Result<Self, String>> {
        std::env::var("FAKTOR_CHAOS_FAULT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| Self::parse(&v))
    }

    fn fire_allowed(&self, fires_so_far: u32) -> bool {
        self.max_fires == 0 || fires_so_far < self.max_fires
    }
}

/// Where the proxy forwards the request it accepted.
#[derive(Debug, Clone)]
pub enum Upstream {
    /// Plain TCP relay to a local HTTP endpoint (the in-repo mock, or any
    /// cleartext provider endpoint).
    Tcp(std::net::SocketAddr),
    /// The REAL provider leg for the live run: the daemon speaks cleartext
    /// HTTP to the loopback proxy (the production transport disables every
    /// environment proxy and carries no per-provider proxy/CA surface, so
    /// the TLS hop to the real endpoint is bridged by the system `curl`
    /// binary — one subprocess per request, config on stdin so no
    /// credential ever appears in the proxy's argv). The FAULT still lands
    /// between daemon and provider: the proxy governs the bytes the daemon
    /// observes. `work_dir` holds the per-request body file + curl log
    /// (hermetic temp directory).
    LiveHttps {
        base: String,
        work_dir: std::path::PathBuf,
    },
}

/// The opened upstream byte source: a relayed TCP stream or the live
/// curl subprocess's response stream (headers + dechunked body).
enum UpstreamIo {
    Tcp(TcpStream),
    Curl {
        stdout: tokio::process::ChildStdout,
        /// Kept alive for its side effect: `kill_on_drop` stops provider
        /// generation the moment this connection ends (cost bound).
        _child: tokio::process::Child,
    },
}

impl tokio::io::AsyncRead for UpstreamIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            UpstreamIo::Tcp(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            UpstreamIo::Curl { stdout, .. } => std::pin::Pin::new(stdout).poll_read(cx, buf),
        }
    }
}

/// Live counters of one proxy run (assertable, never derived from the SUT).
#[derive(Debug, Default)]
pub struct ProxyStats {
    pub connections: AtomicU64,
    /// `POST …/chat/completions` requests — the physical completion
    /// attempts (a `/models` probe never counts).
    pub chat_requests: AtomicU64,
    /// Connections on which the armed fault actually fired.
    pub faults_fired: AtomicU64,
    pub body_bytes_to_client: AtomicU64,
    /// Connections that ended before the fault threshold was reached.
    pub faults_starved: AtomicU64,
}

impl ProxyStats {
    pub fn faults_fired(&self) -> u64 {
        self.faults_fired.load(Ordering::SeqCst)
    }

    pub fn connections(&self) -> u64 {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn chat_requests(&self) -> u64 {
        self.chat_requests.load(Ordering::SeqCst)
    }

    pub fn faults_starved(&self) -> u64 {
        self.faults_starved.load(Ordering::SeqCst)
    }
}

/// A running chaos proxy bound to an ephemeral loopback port.
pub struct ChaosProxy {
    pub addr: std::net::SocketAddr,
    pub stats: Arc<ProxyStats>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ChaosProxy {
    /// Start the proxy on `127.0.0.1:0`, forwarding to `upstream`.
    pub fn start(upstream: Upstream, spec: FaultSpec) -> Self {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let stats = Arc::new(ProxyStats::default());
        let stats_task = stats.clone();
        let thread = std::thread::Builder::new()
            .name("chaos-proxy".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("proxy runtime");
                rt.block_on(async move {
                    let listener = TcpListener::bind(("127.0.0.1", 0))
                        .await
                        .expect("proxy bind");
                    let addr = listener.local_addr().expect("proxy addr");
                    let _ = ready_tx.send(addr);
                    let fires: std::sync::Arc<AtomicU32> = std::sync::Arc::new(AtomicU32::new(0));
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            accepted = listener.accept() => {
                                let Ok((client, _)) = accepted else { break };
                                let idx = stats_task.connections.fetch_add(1, Ordering::SeqCst);
                                // One-shot accounting: a connection only
                                // consumes a fire while fires < max_fires;
                                // later connections pass through healthy so
                                // the transport's retry can recover.
                                let allow_fault =
                                    spec.fire_allowed(fires.load(Ordering::SeqCst));
                                let stats = stats_task.clone();
                                let fires = fires.clone();
                                let upstream = upstream.clone();
                                tokio::spawn(async move {
                                    // One connection is one bounded unit: a
                                    // stalled upstream can never leak a task.
                                    let outcome = tokio::time::timeout(
                                        Duration::from_secs(90),
                                        handle_connection(
                                            client, upstream, spec, stats, allow_fault, idx,
                                        ),
                                    )
                                    .await;
                                    if matches!(outcome, Ok(true)) && allow_fault {
                                        fires.fetch_add(1, Ordering::SeqCst);
                                    }
                                });
                            }
                        }
                    }
                });
            })
            .expect("proxy thread");
        let addr = ready_rx.recv().expect("proxy ready");
        Self {
            addr,
            stats,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    /// Stop accepting and drop every in-flight task (sockets close on
    /// drop). Bounded: the thread is joined, never leaked.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ChaosProxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

const READ_CHUNK: usize = 4096;

/// Sequential per-connection proxy: request → upstream, response → client
/// with the armed fault injected on the response body.
async fn handle_connection(
    client: TcpStream,
    upstream: Upstream,
    spec: FaultSpec,
    stats: Arc<ProxyStats>,
    allow_fault: bool,
    conn_idx: u64,
) -> bool {
    let _ = client.set_nodelay(true);
    let mut client = client;

    // 1. Read the whole request (head + Content-Length body).
    let request = match read_http_message(&mut client).await {
        Some(bytes) => bytes,
        None => return false,
    };
    // Faults target the COMPLETION stream only: a `/models` probe (or any
    // other route) always gets a healthy pipe, so an armed kill can never
    // burn its one fire on a non-SSE response.
    let request_head_end = find_head_end(&request).unwrap_or(request.len());
    let request_head = String::from_utf8_lossy(&request[..request_head_end]).to_string();
    let path = request_head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let is_chat = path.ends_with("/chat/completions");
    if is_chat {
        stats.chat_requests.fetch_add(1, Ordering::SeqCst);
    }
    let apply_fault = allow_fault && is_chat;

    // 2. Open the upstream: plain TCP relay, or the live TLS bridge.
    let mut upstream_io = match &upstream {
        Upstream::Tcp(addr) => {
            let mut stream = match TcpStream::connect(*addr).await {
                Ok(s) => s,
                Err(_) => return false,
            };
            let _ = stream.set_nodelay(true);
            if stream.write_all(&request).await.is_err() {
                return false;
            }
            if stream.flush().await.is_err() {
                return false;
            }
            UpstreamIo::Tcp(stream)
        }
        Upstream::LiveHttps { base, work_dir } => {
            match spawn_curl_upstream(base, work_dir, &request, conn_idx).await {
                Some(io) => io,
                None => return false,
            }
        }
    };

    // 3. Read the response head once; everything past it is BODY (the
    //    first read may already have pulled body bytes — they must count
    //    against the fault threshold, never be treated as head).
    let (head, carry) = match read_head(&mut upstream_io).await {
        Some(h) => h,
        None => return false,
    };
    let head = normalize_close_delimited(&head);
    let mut carry = carry;
    let mut carry_at = 0usize;

    // A connection past its one-shot fire budget — or any non-completion
    // route — is a healthy pipe: the transport's retry must find a working
    // endpoint again.
    if !apply_fault {
        if client.write_all(&head).await.is_err() {
            return false;
        }
        let mut rest = carry;
        let _ = upstream_io.read_to_end(&mut rest).await;
        if !rest.is_empty() {
            let _ = client.write_all(&rest).await;
            let _ = client.flush().await;
        }
        return false;
    }

    match spec.class {
        FaultClass::Reorder => {
            let mut rest = carry;
            let _ = upstream_io.read_to_end(&mut rest).await;
            let mut full = head.clone();
            full.extend_from_slice(&rest);
            let out = reorder_halves(&full, spec.threshold);
            let _ = client.write_all(&out).await;
            let _ = client.flush().await;
            stats.faults_fired.fetch_add(1, Ordering::SeqCst);
            true
        }
        FaultClass::GzipTruncate => {
            let mut rest = carry;
            let _ = upstream_io.read_to_end(&mut rest).await;
            let mut full = head.clone();
            full.extend_from_slice(&rest);
            let out = gzip_truncate_response(&full, spec.threshold, conn_idx, spec.seed);
            let _ = client.write_all(&out).await;
            let _ = client.flush().await;
            stats.faults_fired.fetch_add(1, Ordering::SeqCst);
            true
        }
        FaultClass::DelayBurst
        | FaultClass::KillMidSse
        | FaultClass::Truncate
        | FaultClass::RstAfter => {
            // Forward the head verbatim, then count BODY bytes. The
            // pending `carry` buffer is the ONLY byte source: bytes read
            // but not yet forwarded stay in it (a delay burst must later
            // resume the EXACT byte stream — never re-read past bytes).
            if client.write_all(&head).await.is_err() {
                return false;
            }
            let _ = client.flush().await;
            let mut body_seen: usize = 0;
            let mut fired = spec.threshold == 0;
            let mut buf = vec![0u8; READ_CHUNK];
            while !fired {
                if carry_at >= carry.len() {
                    let n = match upstream_io.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    carry.clear();
                    carry_at = 0;
                    carry.extend_from_slice(&buf[..n]);
                }
                let available = carry.len() - carry_at;
                let remaining_budget = spec.threshold.saturating_sub(body_seen);
                let take = available.min(remaining_budget);
                if take == 0 {
                    break;
                }
                if client
                    .write_all(&carry[carry_at..carry_at + take])
                    .await
                    .is_err()
                {
                    return false;
                }
                let _ = client.flush().await;
                body_seen += take;
                carry_at += take;
                stats
                    .body_bytes_to_client
                    .fetch_add(take as u64, Ordering::SeqCst);
                if body_seen >= spec.threshold {
                    fired = true;
                }
            }
            if !fired {
                // The upstream ended before the threshold: the armed fault
                // never fired on this connection (starved, counted).
                stats.faults_starved.fetch_add(1, Ordering::SeqCst);
                return false;
            }
            stats.faults_fired.fetch_add(1, Ordering::SeqCst);
            match spec.class {
                FaultClass::DelayBurst => {
                    // The bytes up to the threshold are already out; burst
                    // the remaining body after one deterministic delay.
                    if spec.delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(spec.delay_ms)).await;
                    }
                    let mut rest = carry[carry_at..].to_vec();
                    let _ = upstream_io.read_to_end(&mut rest).await;
                    if !rest.is_empty() {
                        let _ = client.write_all(&rest).await;
                        let _ = client.flush().await;
                        stats
                            .body_bytes_to_client
                            .fetch_add(rest.len() as u64, Ordering::SeqCst);
                    }
                }
                FaultClass::Truncate => {
                    // Clean FIN on the client-facing write side; the rest
                    // of the upstream body is discarded.
                    let _ = client.shutdown().await;
                }
                FaultClass::KillMidSse => {
                    // Abrupt drop: both sockets closed mid-stream (FIN both
                    // ways). Dropping the upstream also kills the live curl
                    // child (kill_on_drop), stopping provider generation.
                    drop(client);
                    drop(upstream_io);
                }
                FaultClass::RstAfter => {
                    rst_close(client).await;
                    drop(upstream_io);
                }
                _ => unreachable!(),
            }
            true
        }
    }
}

/// Normalize a forwarded response head to close-delimited framing: curl
/// dechunks the body it streams, so a `Transfer-Encoding: chunked` header
/// must never reach the daemon (the client would wait for chunk frames
/// that no longer exist). `Connection: close` is always present so EOF is
/// the body boundary; an upstream `Content-Length` (curl passes non-
/// chunked responses through unchanged) stays authoritative.
fn normalize_close_delimited(head: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let lines: Vec<&str> = text.split("\r\n").collect();
    let had_te = lines
        .iter()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:"));
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 1);
    let mut saw_connection = false;
    for line in &lines {
        let lower = line.to_ascii_lowercase();
        if line.trim().is_empty() {
            continue;
        }
        if lower.starts_with("transfer-encoding:") {
            continue;
        }
        if had_te && lower.starts_with("content-length:") {
            continue;
        }
        if lower.starts_with("connection:") {
            if !saw_connection {
                out.push("Connection: close".to_string());
                saw_connection = true;
            }
            continue;
        }
        out.push(line.to_string());
    }
    if !saw_connection {
        out.push("Connection: close".to_string());
    }
    let mut joined = out.join("\r\n");
    joined.push_str("\r\n\r\n");
    joined.into_bytes()
}

/// Open the live TLS leg: one bounded `curl` subprocess whose stdout IS the
/// upstream HTTP response (`-i` prepends the raw head; curl dechunks the
/// body). The request's `authorization` / `content-type` headers and the
/// body ride curl's CONFIG on stdin and a hermetic body file — never argv.
async fn spawn_curl_upstream(
    base: &str,
    work_dir: &std::path::Path,
    request: &[u8],
    conn_idx: u64,
) -> Option<UpstreamIo> {
    let head_end = find_head_end(request)?;
    let head = String::from_utf8_lossy(&request[..head_end]).to_string();
    let body = &request[head_end..];
    let request_line = head.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    let url = format!("{}{}", base.trim_end_matches('/'), target);

    std::fs::create_dir_all(work_dir).ok()?;
    let body_path = work_dir.join(format!("upstream-req-{conn_idx}.json"));
    std::fs::write(&body_path, body).ok()?;

    let mut config = format!("url = \"{url}\"\n");
    config.push_str("noproxy = \"*\"\n");
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "authorization" => config.push_str(&format!("header = \"authorization: {value}\"\n")),
            "content-type" => config.push_str(&format!("header = \"content-type: {value}\"\n")),
            _ => {}
        }
    }
    if !body.is_empty() {
        config.push_str(&format!("data-binary = \"@{}\"\n", body_path.display()));
    }

    let stderr_path = work_dir.join("curl.log");
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .ok()?;
    let mut child = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-N",
            "--no-buffer",
            "--http1.1",
            "-i",
            "--max-time",
            "80",
            "--config",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(stderr_file))
        .kill_on_drop(true)
        .current_dir(work_dir)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    use tokio::io::AsyncWriteExt as _;
    if stdin.write_all(config.as_bytes()).await.is_err() {
        return None;
    }
    if stdin.shutdown().await.is_err() {
        return None;
    }
    drop(stdin);
    let stdout = child.stdout.take()?;
    Some(UpstreamIo::Curl {
        stdout,
        _child: child,
    })
}

/// Abortive close: the kernel emits TCP RST instead of FIN (tokio's stable
/// `SO_LINGER=0` wrapper — no unsafe in this harness).
async fn rst_close(stream: TcpStream) {
    let _ = stream.set_zero_linger();
    drop(stream);
}

/// Read one HTTP/1.x message (head + Content-Length body, if any) from a
/// socket that carries exactly one request.
async fn read_http_message(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    let head_end = loop {
        if let Some(pos) = find_head_end(&out) {
            break pos;
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&out[..head_end]).to_string();
    let content_length = parse_content_length(&head)?;
    while out.len() < head_end + content_length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Some(out)
}

/// Read the response head (through `\r\n\r\n`). Returns the head BYTES ONLY
/// plus any body bytes the first read already pulled past the boundary —
/// those must be counted against the fault threshold, never mistaken for
/// head.
async fn read_head<R: tokio::io::AsyncRead + Unpin>(stream: &mut R) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::with_capacity(512);
    let mut buf = [0u8; 1024];
    loop {
        if let Some(pos) = find_head_end(&out) {
            let carry = out.split_off(pos);
            return Some((out, carry));
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > 64 * 1024 {
            return None;
        }
    }
}

fn find_head_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

fn parse_content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            return rest.trim().parse::<usize>().ok();
        }
    }
    // No body (e.g. GET).
    Some(0)
}

/// Deterministic SSE frame reorder: the response body is split `tail`
/// bytes from its END (a cut that lands INSIDE the terminal `data: [DONE]`
/// frame for the mock's fixed body) and the two halves are swapped, so the
/// client observes the frames out of order. Every body byte is preserved
/// exactly once — only the order changes — and the cut is a pure function
/// of the body length and `tail`, never of TCP chunk boundaries.
fn reorder_halves(response: &[u8], tail: usize) -> Vec<u8> {
    let Some(split) = find_head_end(response) else {
        return response.to_vec();
    };
    let (head, body) = response.split_at(split);
    if body.len() < 4 {
        return response.to_vec();
    }
    let tail = tail.max(1).min(body.len() - 1);
    let cut = body.len() - tail;
    let mut out = head.to_vec();
    out.extend_from_slice(&body[cut..]);
    out.extend_from_slice(&body[..cut]);
    out
}

/// Rewrite the response as `Content-Encoding: gzip` with a stored-block
/// (uncompressed DEFLATE) gzip body, truncated at `truncate_at` bytes —
/// always strictly inside the gzip stream so the client observes a
/// corrupted/truncated gzip body. The proxy owns the encoding here, so no
/// compression library is needed and the bytes are byte-identical for a
/// given input.
fn gzip_truncate_response(
    response: &[u8],
    truncate_at: usize,
    conn_idx: u64,
    seed: u64,
) -> Vec<u8> {
    let Some(split) = find_head_end(response) else {
        return response.to_vec();
    };
    let (head, body) = response.split_at(split);
    let gzip_body = gzip_store(body);
    // Deterministic per connection: the cut lands inside the stream.
    let jitter = ((seed.wrapping_mul(conn_idx.wrapping_add(1)) >> 3) % 7) as usize;
    let cut = truncate_at
        .max(1)
        .min(gzip_body.len().saturating_sub(1 + jitter));
    let cut = cut.max(1).min(gzip_body.len().saturating_sub(1));
    let mut head_text = String::from_utf8_lossy(head).to_string();
    // Replace/append the encoding + length headers; drop any
    // transfer-encoding so Content-Length is authoritative.
    let mut kept: Vec<String> = Vec::new();
    for line in head_text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("content-length:")
            || lower.starts_with("content-encoding:")
            || lower.starts_with("transfer-encoding:")
            || lower.starts_with("connection:")
        {
            continue;
        }
        if lower.starts_with("http/") {
            kept.push(line.to_string());
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        kept.push(line.to_string());
    }
    head_text = kept.join("\r\n");
    head_text.push_str("\r\nContent-Encoding: gzip\r\nContent-Length: ");
    head_text.push_str(&cut.to_string());
    head_text.push_str("\r\nConnection: close\r\n\r\n");
    let mut out = head_text.into_bytes();
    out.extend_from_slice(&gzip_body[..cut]);
    out
}

/// gzip member over stored (BTYPE=00) DEFLATE blocks + CRC32 + ISIZE.
fn gzip_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff]);
    // Stored blocks cap at u16::MAX bytes; an empty payload still needs one
    // final empty block to be a valid stream.
    let blocks: Vec<&[u8]> = if data.is_empty() {
        vec![&[][..]]
    } else {
        data.chunks(u16::MAX as usize).collect()
    };
    for (index, chunk) in blocks.iter().enumerate() {
        let is_last = index + 1 == blocks.len();
        let len = chunk.len() as u16;
        let nlen = !len;
        out.push(if is_last { 0x01 } else { 0x00 });
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    // Table-driven CRC-32 (IEEE), generated once per call site cost is
    // negligible for mock-sized bodies.
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut crc = i as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
        *slot = crc;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc = table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    // TEST-SEPARATION: oracle=independent-reference isolation=pure-unit
    use super::*;

    #[test]
    fn spec_parsing_is_deterministic_and_rejects_hostile_input() {
        let a = FaultSpec::parse("kill_mid_sse:200").expect("parse");
        let b = FaultSpec::parse("kill_mid_sse:200").expect("parse");
        assert_eq!(a.class, b.class);
        assert_eq!(a.threshold, b.threshold);
        assert_eq!(a.seed, FaultSpec::DEFAULT_SEED);
        assert!(FaultSpec::parse("drop_tables:1").is_err());
        assert!(FaultSpec::parse("kill_mid_sse:not-a-number").is_err());
        let burst = FaultSpec::parse("delay_burst:64:750").expect("burst");
        assert_eq!(burst.delay_ms, 750);
        assert_eq!(burst.class, FaultClass::DelayBurst);
    }

    #[test]
    fn gzip_store_round_trips_through_a_stored_block() {
        // Independent reference: the stored-block layout is checked by
        // hand-built bytes, not by re-running the encoder.
        let payload = b"data: {\"text\":\"hi\"}\n\n";
        let gz = gzip_store(payload);
        assert_eq!(&gz[..3], &[0x1f, 0x8b, 0x08], "gzip magic");
        // BFINAL=1, BTYPE=00 (stored) as the first block header byte.
        assert_eq!(gz[10] & 0x07, 0x01, "final stored block");
        let len = u16::from_le_bytes([gz[11], gz[12]]);
        assert_eq!(len as usize, payload.len(), "stored block length");
        let nlen = u16::from_le_bytes([gz[13], gz[14]]);
        assert_eq!(!len, nlen, "NLEN is the ones-complement of LEN");
        assert_eq!(&gz[15..15 + payload.len()], payload, "stored bytes");
        // ISIZE (last 4 bytes) is the uncompressed length.
        let isize = u32::from_le_bytes(gz[gz.len() - 4..].try_into().unwrap());
        assert_eq!(isize as usize, payload.len(), "ISIZE");
    }

    #[test]
    fn frame_reorder_swaps_deterministic_halves_and_preserves_bytes() {
        let body = "data: one\n\ndata: two\n\ndata: [DONE]\n\n";
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}");
        let out = reorder_halves(response.as_bytes(), 5);
        let text = String::from_utf8(out.clone()).expect("ascii");
        let (head, swapped) = text.split_once("\r\n\r\n").expect("head");
        assert!(head.contains("200 OK"), "head untouched");
        // The last 5 bytes (`NE]\n\n`) move in front of the rest; the cut
        // lands inside the terminal `data: [DONE]` frame.
        let fixture_swapped = "NE]\n\ndata: one\n\ndata: two\n\ndata: [DO";
        assert_eq!(swapped, fixture_swapped, "byte-exact swap");
        // Every body byte preserved exactly once — only order changed.
        let mut from_fixture: Vec<u8> = body.bytes().collect();
        let mut from_swap: Vec<u8> = swapped.bytes().collect();
        from_fixture.sort_unstable();
        from_swap.sort_unstable();
        assert_eq!(from_fixture, from_swap, "byte multiset identical");
        // Pure function: the same input re-encodes to the same bytes.
        assert_eq!(reorder_halves(response.as_bytes(), 5), out);
    }
}
