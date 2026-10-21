//! Real local Streamable HTTP MCP server fixture for the daemon runtime.
//! Each test speaks HTTP to a loopback server through the checked egress
//! client (explicit loopback allowlist); the vault is the only source of the
//! bearer credential.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use faktor_core::error::ErrorKind;
use faktor_foundation::customization::McpServerRecord;
use faktor_foundation::Foundation;
use faktor_mcp::remote::{AUTH_REQUIRED_CODE, SESSION_EXPIRED_CODE};
use faktor_provider::egress::CheckedHttpClient;
use faktor_security::destination::DestinationPolicy;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::McpRuntime;

#[derive(Default)]
struct State {
    token: Option<String>,
    sse_tools: AtomicBool,
    expire_next: AtomicBool,
    always_expire: AtomicBool,
    oversize: AtomicBool,
    slow_tools_ms: AtomicU64,
    session_seq: AtomicU64,
    initialize_count: AtomicUsize,
    delete_count: AtomicUsize,
    valid_sessions: Mutex<HashSet<String>>,
    headers: Mutex<Vec<(String, String)>>,
}

impl State {
    fn with_token(token: &str) -> Arc<Self> {
        Arc::new(Self {
            token: Some(token.to_string()),
            ..Self::default()
        })
    }

    fn sessions(&self) -> usize {
        self.valid_sessions.lock().unwrap().len()
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.headers.lock().unwrap().clone()
    }
}

struct Fixture {
    addr: std::net::SocketAddr,
}

impl Fixture {
    async fn start(state: Arc<State>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, state).await;
                });
            }
        });
        Self { addr }
    }

    fn endpoint(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    fn client(&self) -> CheckedHttpClient {
        CheckedHttpClient::with_policy_for_tests(
            DestinationPolicy::parse_lines([format!("http://127.0.0.1:{}", self.addr.port())])
                .expect("loopback policy"),
        )
    }
}

struct HttpRequest {
    method: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() > 256 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let path = request_line.next()?;
    if path != "/mcp" {
        return None;
    }
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        }
        headers.push((name, value));
    }
    while buffer.len() < header_end + content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    Some(HttpRequest {
        method,
        headers,
        body: String::from_utf8_lossy(&buffer[header_end..]).to_string(),
    })
}

async fn write_json(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, String)],
    body: &Value,
) {
    let body = body.to_string();
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&body);
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn write_empty(stream: &mut TcpStream, status: &str) {
    let out = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(out.as_bytes()).await;
}

async fn write_sse(stream: &mut TcpStream, events: &[Value]) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(head.as_bytes()).await;
    for event in events {
        let frame = format!("data: {event}\n\n");
        for part in frame.as_bytes().chunks(13) {
            let _ = stream
                .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                .await;
            let _ = stream.write_all(part).await;
            let _ = stream.write_all(b"\r\n").await;
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
}

fn session_of(request: &HttpRequest) -> Option<String> {
    request.header("mcp-session-id").map(str::to_string)
}

async fn serve(mut stream: TcpStream, state: Arc<State>) -> Option<()> {
    let request = read_request(&mut stream).await?;
    state
        .headers
        .lock()
        .unwrap()
        .extend(request.headers.iter().cloned());
    if request.method == "DELETE" {
        if let Some(session) = session_of(&request) {
            state.valid_sessions.lock().unwrap().remove(&session);
        }
        state.delete_count.fetch_add(1, Ordering::SeqCst);
        write_empty(&mut stream, "200 OK").await;
        return Some(());
    }
    let message: Value = serde_json::from_str(&request.body).ok()?;
    if let Some(expected) = &state.token {
        let authorized = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|value| value == expected);
        if !authorized {
            write_json(
                &mut stream,
                "401 Unauthorized",
                &[],
                &json!({"error": {"code": -32001, "message": "unauthorized"}}),
            )
            .await;
            return Some(());
        }
    }
    let id = message.get("id").cloned();
    let method = message
        .get("method")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if method == "initialize" {
        state.initialize_count.fetch_add(1, Ordering::SeqCst);
        let session = format!("sess-{}", state.session_seq.fetch_add(1, Ordering::SeqCst));
        state.valid_sessions.lock().unwrap().insert(session.clone());
        write_json(
            &mut stream,
            "200 OK",
            &[("Mcp-Session-Id", session)],
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {
                        "tools": {"listChanged": true},
                        "resources": {"subscribe": true},
                        "prompts": {"listChanged": false}
                    },
                    "serverInfo": {"name": "fixture", "version": "1.0"},
                    "instructions": "fixture instructions"
                }
            }),
        )
        .await;
        return Some(());
    }
    if let Some(session) = session_of(&request) {
        if !state.valid_sessions.lock().unwrap().contains(&session) {
            write_json(
                &mut stream,
                "404 Not Found",
                &[],
                &json!({"error": {"code": -32000, "message": "session expired"}}),
            )
            .await;
            return Some(());
        }
    }
    match method {
        "notifications/initialized" | "notifications/cancelled" => {
            write_empty(&mut stream, "202 Accepted").await;
        }
        "tools/list" => {
            if state.expire_next.swap(false, Ordering::SeqCst)
                || state.always_expire.load(Ordering::SeqCst)
            {
                if let Some(session) = session_of(&request) {
                    state.valid_sessions.lock().unwrap().remove(&session);
                }
                write_json(
                    &mut stream,
                    "404 Not Found",
                    &[],
                    &json!({"error": {"code": -32000, "message": "session expired"}}),
                )
                .await;
                return Some(());
            }
            let delay = state.slow_tools_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if state.oversize.load(Ordering::SeqCst) {
                write_json(
                    &mut stream,
                    "200 OK",
                    &[],
                    &json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"tools": [{"name": "x".repeat(8192), "description": ""}]}
                    }),
                )
                .await;
            } else if state.sse_tools.load(Ordering::SeqCst) {
                write_sse(
                    &mut stream,
                    &[
                        json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info"}}),
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"tools": [{"name": "echo", "description": "echoes", "inputSchema": {"type": "object"}}]}
                        }),
                    ],
                )
                .await;
            } else {
                write_json(
                    &mut stream,
                    "200 OK",
                    &[],
                    &json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"tools": [{"name": "echo", "description": "echoes", "inputSchema": {"type": "object"}}]}
                    }),
                )
                .await;
            }
        }
        "tools/call" => {
            write_json(
                &mut stream,
                "200 OK",
                &[],
                &json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"content": [{"type": "text", "text": "echo:"}], "isError": false}
                }),
            )
            .await;
        }
        "resources/list" => {
            write_json(
                &mut stream,
                "200 OK",
                &[],
                &json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"resources": [{"uri": "file:///a.txt", "name": "a"}]}
                }),
            )
            .await;
        }
        "resources/templates/list" => {
            write_json(
                &mut stream,
                "200 OK",
                &[],
                &json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"resourceTemplates": [{"uriTemplate": "file:///{path}", "name": "file"}]}
                }),
            )
            .await;
        }
        "prompts/list" => {
            write_json(
                &mut stream,
                "200 OK",
                &[],
                &json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"prompts": [{"name": "greet", "description": "greets"}]}
                }),
            )
            .await;
        }
        other => {
            write_json(
                &mut stream,
                "200 OK",
                &[],
                &json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("unknown method {other}")}
                }),
            )
            .await;
        }
    }
    Some(())
}

fn record(endpoint: &str, token_ref: Option<String>) -> McpServerRecord {
    McpServerRecord {
        id: "remote".into(),
        name: "remote".into(),
        command: String::new(),
        args: Vec::new(),
        env: Vec::new(),
        trust_origin: "user".into(),
        trusted: true,
        enabled: true,
        transport: "streamable_http".into(),
        endpoint: Some(endpoint.into()),
        auth_state: if token_ref.is_some() {
            "authorized".into()
        } else {
            "signed_out".into()
        },
        auth_account: None,
        auth_scopes: Vec::new(),
        auth_token_ref: token_ref,
        timeout_ms: Some(5_000),
        output_cap_bytes: Some(1024 * 1024),
        stderr_cap_bytes: None,
        revision: 1,
        created_ms: 0,
        updated_ms: 0,
    }
}

fn foundation() -> (tempfile::TempDir, Foundation) {
    let dir = tempfile::tempdir().unwrap();
    let foundation = Foundation::open(dir.path().join("foundation.db")).unwrap();
    (dir, foundation)
}

async fn start(
    fixture: &Fixture,
    foundation: &Foundation,
    record: McpServerRecord,
) -> (
    Arc<McpRuntime>,
    Result<Vec<Value>, faktor_core::error::Error>,
) {
    let runtime = Arc::new(McpRuntime::with_remote_client(None, fixture.client()));
    let result = runtime
        .start(&record, Vec::new(), Some(&foundation.vault))
        .await;
    (runtime, result)
}

fn auth_error_code(error: &faktor_core::error::Error) -> &str {
    match &error.kind {
        ErrorKind::Provider { code, .. } => code.as_str(),
        other => panic!("expected a typed provider error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vault_bearer_initializes_and_discovers_every_catalog() {
    let state = State::with_token("sk-vault-token");
    state.sse_tools.store(true, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let (_dir, foundation) = foundation();
    let secret = foundation
        .vault
        .store("mcp_oauth", "MCP OAuth token for remote", b"sk-vault-token")
        .unwrap();
    let record = record(&fixture.endpoint(), Some(secret.handle));
    let (runtime, result) = start(&fixture, &foundation, record.clone()).await;
    let tools = result.expect("SSE initialize/tools/list");
    assert_eq!(tools[0]["name"], "echo");
    assert!(state
        .headers()
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
            && value == "Bearer sk-vault-token"));
    let caps = runtime
        .capabilities("remote")
        .await
        .expect("negotiated caps");
    assert!(caps.tools && caps.resources && caps.prompts);
    let view = super::mcp_view(&record, Some(true), Some(tools.clone()), Some(caps));
    let rendered = view.to_string();
    assert!(
        !rendered.contains("sk-vault-token"),
        "the view leaked the bearer"
    );
    assert_eq!(view["remoteTransport"]["supported"], true);
    assert_eq!(view["capabilities"]["tools"]["supported"], true);
    let catalogs = runtime.catalogs("remote", None).await.unwrap().unwrap();
    assert_eq!(catalogs["transport"], "streamable_http");
    assert_eq!(catalogs["resources"][0]["uri"], "file:///a.txt");
    assert_eq!(catalogs["resourceTemplates"][0]["name"], "file");
    assert_eq!(catalogs["prompts"][0]["name"], "greet");
    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 1);
    assert!(runtime.stop("remote").await);
    assert_eq!(state.delete_count.load(Ordering::SeqCst), 1);
    assert_eq!(state.sessions(), 0, "DELETE terminates the session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_or_wrong_vault_token_is_typed_never_connected() {
    let state = State::with_token("sk-vault-token");
    let fixture = Fixture::start(state.clone()).await;
    let (_dir, foundation) = foundation();
    let (runtime, result) = start(&fixture, &foundation, record(&fixture.endpoint(), None)).await;
    let error = result.expect_err("an auth-required server must not start anonymously");
    assert_eq!(auth_error_code(&error), AUTH_REQUIRED_CODE, "{error:?}");
    assert!(!format!("{error:?}").contains("sk-vault-token"));
    assert_eq!(runtime.health("remote").await, None);

    let wrong = foundation
        .vault
        .store("mcp_oauth", "wrong MCP OAuth token", b"wrong-token")
        .unwrap();
    let (_runtime, result) = start(
        &fixture,
        &foundation,
        record(&fixture.endpoint(), Some(wrong.handle)),
    )
    .await;
    let error = result.expect_err("a wrong bearer token must be refused");
    assert_eq!(auth_error_code(&error), AUTH_REQUIRED_CODE, "{error:?}");
    assert!(!format!("{error:?}").contains("wrong-token"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_session_reinitializes_once_and_retries() {
    let state = State::with_token("sk-vault-token");
    let fixture = Fixture::start(state.clone()).await;
    let (_dir, foundation) = foundation();
    let secret = foundation
        .vault
        .store("mcp_oauth", "MCP OAuth token for remote", b"sk-vault-token")
        .unwrap();
    let (runtime, result) = start(
        &fixture,
        &foundation,
        record(&fixture.endpoint(), Some(secret.handle)),
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 1);
    state.expire_next.store(true, Ordering::SeqCst);
    let tools = runtime.tools("remote", None).await.unwrap().unwrap();
    assert_eq!(tools[0]["name"], "echo");
    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 2);
    let _ = runtime.stop("remote").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_session_expiry_is_typed_not_an_endless_retry() {
    let state = State::with_token("sk-vault-token");
    let fixture = Fixture::start(state.clone()).await;
    let (_dir, foundation) = foundation();
    let secret = foundation
        .vault
        .store("mcp_oauth", "MCP OAuth token for remote", b"sk-vault-token")
        .unwrap();
    let (runtime, result) = start(
        &fixture,
        &foundation,
        record(&fixture.endpoint(), Some(secret.handle)),
    )
    .await;
    assert!(result.is_ok());
    state.always_expire.store(true, Ordering::SeqCst);
    let error = runtime
        .tools("remote", None)
        .await
        .expect_err("a retried request that expires again is typed");
    assert_eq!(auth_error_code(&error), SESSION_EXPIRED_CODE, "{error:?}");
    assert_eq!(
        state.initialize_count.load(Ordering::SeqCst),
        2,
        "the client must re-initialize exactly once per call"
    );
    let _ = runtime.stop("remote").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_catalog_is_refused_at_the_configured_cap() {
    let state = State::with_token("sk-vault-token");
    state.oversize.store(true, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let (_dir, foundation) = foundation();
    let secret = foundation
        .vault
        .store("mcp_oauth", "MCP OAuth token for remote", b"sk-vault-token")
        .unwrap();
    let mut record = record(&fixture.endpoint(), Some(secret.handle));
    record.output_cap_bytes = Some(1024);
    let (runtime, result) = start(&fixture, &foundation, record).await;
    let error = result.expect_err("an oversized tools/list is refused");
    assert_eq!(error.kind, ErrorKind::Oversized, "{error:?}");
    assert_eq!(runtime.health("remote").await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_catalog_is_bounded_by_the_record_timeout() {
    let state = State::with_token("sk-vault-token");
    state.slow_tools_ms.store(2_000, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let (_dir, foundation) = foundation();
    let secret = foundation
        .vault
        .store("mcp_oauth", "MCP OAuth token for remote", b"sk-vault-token")
        .unwrap();
    let mut record = record(&fixture.endpoint(), Some(secret.handle));
    record.timeout_ms = Some(150);
    let started = std::time::Instant::now();
    let (runtime, result) = start(&fixture, &foundation, record).await;
    let error = result.expect_err("a slow remote catalog is a typed timeout");
    assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the record timeout must bound the call: {:?}",
        started.elapsed()
    );
    assert_eq!(runtime.health("remote").await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_surface_is_not_fabricated_for_a_stdio_record() {
    let (_dir, _foundation) = foundation();
    let stdio = McpServerRecord {
        transport: "stdio".into(),
        endpoint: None,
        auth_state: "signed_out".into(),
        ..record("http://127.0.0.1:1/mcp", None)
    };
    let view = super::mcp_view(&stdio, None, None, None);
    assert_eq!(view["remoteTransport"]["supported"], false);
    assert_eq!(view["remoteTransport"]["configured"], false);
    let remote = record("https://mcp.example.com/rpc", None);
    let view = super::mcp_view(&remote, None, None, None);
    assert_eq!(view["remoteTransport"]["supported"], true);
    assert_eq!(view["remoteTransport"]["configured"], true);
    assert_eq!(view["capabilities"], Value::Null);
}
