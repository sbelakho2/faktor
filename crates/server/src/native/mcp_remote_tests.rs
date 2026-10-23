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
    slow_initialize_ms: AtomicU64,
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
        let delay = state.slow_initialize_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
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

fn client_for(fixtures: &[&Fixture]) -> CheckedHttpClient {
    CheckedHttpClient::with_policy_for_tests(
        DestinationPolicy::parse_lines(
            fixtures
                .iter()
                .map(|fixture| format!("http://127.0.0.1:{}", fixture.addr.port())),
        )
        .expect("loopback policy"),
    )
}

async fn wait_for_initialize(state: &State, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while state.initialize_count.load(Ordering::SeqCst) < expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "initialize was not observed within the bound"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_delete(state: &State) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while state.delete_count.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cancelled connection was never closed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
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
async fn slow_start_of_one_server_does_not_block_another() {
    let slow_state = Arc::new(State::default());
    slow_state.slow_initialize_ms.store(1_500, Ordering::SeqCst);
    let slow_fixture = Fixture::start(slow_state.clone()).await;
    let fast_state = Arc::new(State::default());
    let fast_fixture = Fixture::start(fast_state.clone()).await;
    let runtime = Arc::new(McpRuntime::with_remote_client(
        None,
        client_for(&[&slow_fixture, &fast_fixture]),
    ));
    let mut slow_record = record(&slow_fixture.endpoint(), None);
    slow_record.id = "slow-a".into();
    slow_record.name = "slow-a".into();
    let mut fast_record = record(&fast_fixture.endpoint(), None);
    fast_record.id = "fast-b".into();
    fast_record.name = "fast-b".into();

    let slow_task = tokio::spawn({
        let runtime = runtime.clone();
        let record = slow_record.clone();
        async move { runtime.start(&record, Vec::new(), None).await }
    });
    wait_for_initialize(&slow_state, 1).await;

    let started = std::time::Instant::now();
    let tools = tokio::time::timeout(
        Duration::from_secs(2),
        runtime.start(&fast_record, Vec::new(), None),
    )
    .await
    .expect("a fast server must not wait on a slow server's slot")
    .expect("the fast server starts");
    assert_eq!(tools[0]["name"], "echo");
    assert!(
        started.elapsed() < Duration::from_millis(1_000),
        "the fast start was serialized behind the slow handshake: {:?}",
        started.elapsed()
    );
    assert!(
        !slow_task.is_finished(),
        "the slow start must still be in flight while B completes"
    );
    let catalogs =
        tokio::time::timeout(Duration::from_millis(500), runtime.catalogs("fast-b", None))
            .await
            .expect("catalog reads stay short-locked")
            .expect("catalog read")
            .expect("started server");
    assert_eq!(catalogs["transport"], "streamable_http");

    tokio::time::timeout(Duration::from_secs(3), slow_task)
        .await
        .expect("the slow start completes")
        .unwrap()
        .expect("slow server starts");
    assert_eq!(slow_state.initialize_count.load(Ordering::SeqCst), 1);
    assert_eq!(fast_state.initialize_count.load(Ordering::SeqCst), 1);
    assert!(runtime.stop("slow-a").await);
    assert!(runtime.stop("fast-b").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_server_starts_share_one_connection() {
    let state = Arc::new(State::default());
    state.slow_initialize_ms.store(200, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let runtime = Arc::new(McpRuntime::with_remote_client(
        None,
        client_for(&[&fixture]),
    ));
    let mut record = record(&fixture.endpoint(), None);
    record.id = "shared".into();
    record.name = "shared".into();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let runtime = runtime.clone();
        let record = record.clone();
        tasks.push(tokio::spawn(async move {
            runtime.start(&record, Vec::new(), None).await
        }));
    }
    for task in tasks {
        let tools = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("single-flight start must not deadlock")
            .unwrap()
            .expect("every joined start succeeds");
        assert_eq!(tools[0]["name"], "echo");
    }
    assert_eq!(
        state.initialize_count.load(Ordering::SeqCst),
        1,
        "concurrent starts of one server must share one connection"
    );
    assert_eq!(runtime.health("shared").await, Some(true));
    assert!(runtime.stop("shared").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_start_clears_the_slot_and_a_retry_connects() {
    let state = State::with_token("right-token");
    let fixture = Fixture::start(state.clone()).await;
    let (_dir, foundation) = foundation();
    let runtime = Arc::new(McpRuntime::with_remote_client(
        None,
        client_for(&[&fixture]),
    ));
    let wrong = foundation
        .vault
        .store("mcp_oauth", "wrong MCP OAuth token", b"wrong-token")
        .unwrap();
    let mut record = record(&fixture.endpoint(), Some(wrong.handle));
    record.id = "retry".into();
    record.name = "retry".into();
    let error = runtime
        .start(&record, Vec::new(), Some(&foundation.vault))
        .await
        .expect_err("a wrong bearer token must fail");
    assert_eq!(auth_error_code(&error), AUTH_REQUIRED_CODE, "{error:?}");
    assert_eq!(
        runtime.health("retry").await,
        None,
        "a failed start must clear its slot"
    );

    let right = foundation
        .vault
        .store("mcp_oauth", "right MCP OAuth token", b"right-token")
        .unwrap();
    record.auth_token_ref = Some(right.handle);
    let tools = runtime
        .start(&record, Vec::new(), Some(&foundation.vault))
        .await
        .expect("a retry after a cleared failure connects");
    assert_eq!(tools[0]["name"], "echo");
    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.health("retry").await, Some(true));
    assert!(runtime.stop("retry").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_during_start_cancels_and_closes_without_deadlock() {
    let state = Arc::new(State::default());
    state.slow_initialize_ms.store(800, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let runtime = Arc::new(McpRuntime::with_remote_client(
        None,
        client_for(&[&fixture]),
    ));
    let mut record = record(&fixture.endpoint(), None);
    record.id = "stop-me".into();
    record.name = "stop-me".into();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let record = record.clone();
        async move { runtime.start(&record, Vec::new(), None).await }
    });
    wait_for_initialize(&state, 1).await;
    assert!(
        runtime.stop("stop-me").await,
        "a starting server is stoppable"
    );
    let error = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("stop must not deadlock the in-flight start")
        .unwrap()
        .expect_err("stop cancels the in-flight start");
    assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
    assert_eq!(runtime.health("stop-me").await, None);
    wait_for_delete(&state).await;
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

/// Independent final-wave probe: single-flight is per SERVER, and a failing
/// sibling cannot poison it. Eight concurrent starts of one healthy server
/// share exactly one initialize; a concurrent unauthorized start of another
/// server fails typed and leaves the healthy connection untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_sibling_never_poisons_eight_single_flight_starts() {
    let healthy_state = Arc::new(State::default());
    healthy_state
        .slow_initialize_ms
        .store(250, Ordering::SeqCst);
    let healthy_fixture = Fixture::start(healthy_state.clone()).await;
    let guarded_state = State::with_token("right-token");
    let guarded_fixture = Fixture::start(guarded_state.clone()).await;
    let (_dir, foundation) = foundation();
    let runtime = Arc::new(McpRuntime::with_remote_client(
        None,
        client_for(&[&healthy_fixture, &guarded_fixture]),
    ));
    let mut healthy_record = record(&healthy_fixture.endpoint(), None);
    healthy_record.id = "healthy".into();
    healthy_record.name = "healthy".into();
    let wrong = foundation
        .vault
        .store("mcp_oauth", "wrong MCP OAuth token", b"wrong-token")
        .unwrap();
    let mut guarded_record = record(&guarded_fixture.endpoint(), Some(wrong.handle));
    guarded_record.id = "guarded".into();
    guarded_record.name = "guarded".into();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let runtime = runtime.clone();
        let record = healthy_record.clone();
        tasks.push(tokio::spawn(async move {
            runtime.start(&record, Vec::new(), None).await
        }));
    }
    let failing = {
        let runtime = runtime.clone();
        let record = guarded_record.clone();
        tokio::spawn(async move {
            runtime
                .start(&record, Vec::new(), Some(&foundation.vault))
                .await
        })
    };

    for task in tasks {
        let tools = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("single-flight start must not deadlock")
            .unwrap()
            .expect("every healthy join succeeds");
        assert_eq!(tools[0]["name"], "echo");
    }
    let error = tokio::time::timeout(Duration::from_secs(5), failing)
        .await
        .expect("the failing sibling must not hang behind the healthy single-flight")
        .unwrap()
        .expect_err("the wrong bearer token must be refused");
    assert_eq!(auth_error_code(&error), AUTH_REQUIRED_CODE, "{error:?}");
    assert_eq!(
        healthy_state.initialize_count.load(Ordering::SeqCst),
        1,
        "eight concurrent starts of one server must initialize exactly once"
    );
    assert_eq!(
        guarded_state.initialize_count.load(Ordering::SeqCst),
        0,
        "an unauthorized connection never initializes"
    );
    assert_eq!(runtime.health("healthy").await, Some(true));
    assert_eq!(runtime.health("guarded").await, None);
    assert!(runtime.stop("healthy").await);
    assert!(!runtime.stop("guarded").await);
}
