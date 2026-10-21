//! Real local Streamable HTTP MCP server fixture: raw HTTP/1.1 on loopback
//! with bearer auth, session ids, JSON/SSE replies, expiry, stalls, oversize
//! replies and cancellation recording. Every client call goes through the
//! checked egress client with an explicit loopback allowlist.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use faktor_core::error::ErrorKind;
use faktor_mcp::remote::{
    RemoteMcpConfig, RemoteMcpServer, SecretToken, AUTH_FORBIDDEN_CODE, AUTH_REQUIRED_CODE,
    CAPABILITY_UNSUPPORTED_CODE, PROTOCOL_CODE, SESSION_EXPIRED_CODE,
};
use faktor_provider::egress::CheckedHttpClient;
use faktor_security::destination::DestinationPolicy;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct State {
    token: Option<String>,
    advertise_tools: AtomicBool,
    advertise_resources: AtomicBool,
    advertise_prompts: AtomicBool,
    sse_tools: AtomicBool,
    stall_sse: AtomicBool,
    bad_protocol: AtomicBool,
    slow_ms: AtomicU64,
    oversize: AtomicBool,
    expire_next: AtomicBool,
    always_expire: AtomicBool,
    session_seq: AtomicU64,
    initialize_count: AtomicUsize,
    resources_calls: AtomicUsize,
    valid_sessions: Mutex<HashSet<String>>,
    cancelled: Mutex<Vec<Value>>,
    seen_headers: Mutex<Vec<(String, String)>>,
}

impl State {
    fn advertised() -> Arc<Self> {
        let state = State::default();
        state.advertise_tools.store(true, Ordering::SeqCst);
        state.advertise_resources.store(true, Ordering::SeqCst);
        state.advertise_prompts.store(true, Ordering::SeqCst);
        Arc::new(state)
    }

    fn with_auth(token: &str) -> Arc<Self> {
        let state = Arc::try_unwrap(Self::advertised())
            .ok()
            .expect("unique state");
        let state = State {
            token: Some(token.to_string()),
            ..state
        };
        Arc::new(state)
    }

    fn cancellations(&self) -> Vec<Value> {
        self.cancelled.lock().unwrap().clone()
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.seen_headers.lock().unwrap().clone()
    }
}

struct Fixture {
    addr: std::net::SocketAddr,
}

impl Fixture {
    async fn start(state: Arc<State>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = state;
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = shared.clone();
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
    let path = request_line.next()?.to_string();
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
    let body = String::from_utf8_lossy(&buffer[header_end..]).to_string();
    Some(HttpRequest {
        method,
        headers,
        body,
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
    let _ = stream.flush().await;
}

async fn write_sse(stream: &mut TcpStream, events: &[Value]) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(head.as_bytes()).await;
    for event in events {
        let frame = format!("data: {event}\n\n");
        for part in frame.as_bytes().chunks(17) {
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
    let _ = stream.flush().await;
}

fn session_of(request: &HttpRequest) -> Option<String> {
    request.header("mcp-session-id").map(str::to_string)
}

async fn serve(mut stream: TcpStream, state: Arc<State>) -> Option<()> {
    let request = read_request(&mut stream).await?;
    {
        let mut headers = state.seen_headers.lock().unwrap();
        headers.extend(request.headers.iter().cloned());
    }
    if request.method == "DELETE" {
        if let Some(session) = session_of(&request) {
            state.valid_sessions.lock().unwrap().remove(&session);
        }
        write_empty(&mut stream, "200 OK").await;
        return Some(());
    }
    let message: Value = serde_json::from_str(&request.body).ok()?;
    if let Some(expected) = &state.token {
        match request.header("authorization") {
            None => {
                write_json(
                    &mut stream,
                    "401 Unauthorized",
                    &[],
                    &json!({"error": {"code": -32001, "message": "unauthorized"}}),
                )
                .await;
                return Some(());
            }
            Some(value) if value.strip_prefix("Bearer ") != Some(expected.as_str()) => {
                write_json(
                    &mut stream,
                    "403 Forbidden",
                    &[],
                    &json!({"error": {"code": -32002, "message": "forbidden"}}),
                )
                .await;
                return Some(());
            }
            Some(_) => {}
        }
    }
    if let Some(session) = session_of(&request) {
        let valid = state.valid_sessions.lock().unwrap().contains(&session);
        if !valid {
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
    let id = message.get("id").cloned();
    let method = message
        .get("method")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    match method {
        "initialize" => {
            state.initialize_count.fetch_add(1, Ordering::SeqCst);
            let session = format!("sess-{}", state.session_seq.fetch_add(1, Ordering::SeqCst));
            state.valid_sessions.lock().unwrap().insert(session.clone());
            let mut capabilities = json!({});
            if state.advertise_tools.load(Ordering::SeqCst) {
                capabilities["tools"] = json!({"listChanged": true});
            }
            if state.advertise_resources.load(Ordering::SeqCst) {
                capabilities["resources"] = json!({"subscribe": true});
            }
            if state.advertise_prompts.load(Ordering::SeqCst) {
                capabilities["prompts"] = json!({"listChanged": false});
            }
            write_json(
                &mut stream,
                "200 OK",
                &[("Mcp-Session-Id", session)],
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": if state.bad_protocol.load(Ordering::SeqCst) {
                            "1999-01-01"
                        } else {
                            "2025-06-18"
                        },
                        "capabilities": capabilities,
                        "serverInfo": {"name": "fixture", "version": "1.0"},
                        "instructions": "fixture instructions"
                    }
                }),
            )
            .await;
        }
        "notifications/initialized" => write_empty(&mut stream, "202 Accepted").await,
        "notifications/cancelled" => {
            state
                .cancelled
                .lock()
                .unwrap()
                .push(message.get("params").cloned().unwrap_or(Value::Null));
            write_empty(&mut stream, "202 Accepted").await;
        }
        _ => {
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
            let result = match method {
                "tools/list" => {
                    if state.stall_sse.load(Ordering::SeqCst) {
                        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(head.as_bytes()).await;
                        let notification = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info"}});
                        let frame = format!("data: {notification}\n\n");
                        let _ = stream
                            .write_all(format!("{:x}\r\n", frame.len()).as_bytes())
                            .await;
                        let _ = stream.write_all(frame.as_bytes()).await;
                        let _ = stream.write_all(b"\r\n").await;
                        let _ = stream.flush().await;
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        return Some(());
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
                        return Some(());
                    }
                    json!({"tools": [{"name": "echo", "description": "echoes", "inputSchema": {"type": "object"}}]})
                }
                "tools/call" => {
                    let name = message
                        .get("params")
                        .and_then(|params| params.get("name"))
                        .and_then(|name| name.as_str())
                        .unwrap_or("");
                    match name {
                        "echo" => json!({
                            "content": [{"type": "text", "text": "echo:"}],
                            "isError": false
                        }),
                        "slow" => {
                            tokio::time::sleep(Duration::from_millis(
                                state.slow_ms.load(Ordering::SeqCst),
                            ))
                            .await;
                            json!({"content": [{"type": "text", "text": "late"}]})
                        }
                        "big" => json!({
                            "content": [{"type": "text", "text": "x".repeat(16 * 1024)}]
                        }),
                        "error" => {
                            write_json(
                                &mut stream,
                                "200 OK",
                                &[],
                                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "bad tool args"}}),
                            )
                            .await;
                            return Some(());
                        }
                        _ => json!({"content": [], "isError": true}),
                    }
                }
                "resources/list" => {
                    state.resources_calls.fetch_add(1, Ordering::SeqCst);
                    json!({"resources": [{"uri": "file:///a.txt", "name": "a"}]})
                }
                "resources/templates/list" => {
                    json!({"resourceTemplates": [{"uriTemplate": "file:///{path}", "name": "file"}]})
                }
                "resources/read" => {
                    let uri = message
                        .get("params")
                        .and_then(|params| params.get("uri"))
                        .and_then(|uri| uri.as_str())
                        .unwrap_or("");
                    json!({"contents": [{"uri": uri, "text": "body"}]})
                }
                "prompts/list" => json!({"prompts": [{"name": "greet", "description": "greets"}]}),
                "prompts/get" => {
                    json!({"description": "greet", "messages": [{"role": "user", "content": {"type": "text", "text": "hi"}}]})
                }
                other => {
                    write_json(
                        &mut stream,
                        "200 OK",
                        &[],
                        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {other}")}}),
                    )
                    .await;
                    return Some(());
                }
            };
            if state.oversize.load(Ordering::SeqCst) {
                write_json(
                    &mut stream,
                    "200 OK",
                    &[],
                    &json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "x".repeat(8192), "description": ""}]}}),
                )
                .await;
                return Some(());
            }
            write_json(
                &mut stream,
                "200 OK",
                &[],
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            )
            .await;
        }
    }
    Some(())
}

fn client_for(addr: std::net::SocketAddr) -> CheckedHttpClient {
    CheckedHttpClient::with_policy_for_tests(
        DestinationPolicy::parse_lines([format!("http://127.0.0.1:{}", addr.port())])
            .expect("loopback policy"),
    )
}

fn config(endpoint: &str) -> RemoteMcpConfig {
    let mut config = RemoteMcpConfig::new("fixture", endpoint);
    config.timeout = Duration::from_secs(5);
    config.output_cap_bytes = 1024 * 1024;
    config
}

async fn connect(fixture: &Fixture, config: RemoteMcpConfig) -> Arc<RemoteMcpServer> {
    RemoteMcpServer::connect_with_client(config, client_for(fixture.addr))
        .await
        .expect("initialize handshake")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_capabilities_discovery_invoke_and_sse() {
    let state = State::advertised();
    state.sse_tools.store(true, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;

    assert_eq!(server.session_id().await.as_deref(), Some("sess-0"));
    let caps = server.capabilities().await;
    assert!(caps.tools && caps.resources && caps.prompts);
    assert!(caps.tools_list_changed);
    assert!(caps.resources_subscribe);
    assert_eq!(server.server_info().await["name"], "fixture");
    assert_eq!(
        server.instructions().await.as_deref(),
        Some("fixture instructions")
    );

    let tools = server.list_tools().await.expect("SSE tools/list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let resources = server.list_resources().await.unwrap();
    assert_eq!(resources[0]["uri"], "file:///a.txt");
    let templates = server.list_resource_templates().await.unwrap();
    assert_eq!(templates[0]["name"], "file");
    let prompts = server.list_prompts().await.unwrap();
    assert_eq!(prompts[0]["name"], "greet");
    let result = server
        .call_tool("echo", json!({"text": "hi"}), Duration::from_secs(2))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content[0]["text"], "echo:");
    let read = server.read_resource("file:///a.txt").await.unwrap();
    assert_eq!(read["contents"][0]["text"], "body");
    let prompt = server
        .get_prompt("greet", Some(json!({"name": "Ada"})))
        .await
        .unwrap();
    assert_eq!(prompt["messages"][0]["role"], "user");

    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 1);
    let headers = state.headers();
    assert!(headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("accept") && value.contains("text/event-stream")
    }));
    assert!(headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("mcp-protocol-version") && value == "2025-06-18"
    }));
    assert!(headers
        .iter()
        .any(|(name, value)| { name.eq_ignore_ascii_case("mcp-session-id") && value == "sess-0" }));
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_required_and_denied_are_typed() {
    let state = State::with_auth("sk-fixture-token");
    let fixture = Fixture::start(state.clone()).await;
    let error = match RemoteMcpServer::connect_with_client(
        config(&fixture.endpoint()),
        client_for(fixture.addr),
    )
    .await
    {
        Ok(_) => panic!("an auth-required server must not connect without a token"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind,
        ErrorKind::Provider {
            code: AUTH_REQUIRED_CODE.into(),
            retryable: false
        },
        "{error:?}"
    );
    assert!(!format!("{error:?}").contains("sk-fixture-token"));

    let mut wrong = config(&fixture.endpoint());
    wrong.token = Some(SecretToken::new("wrong-token"));
    let error = match RemoteMcpServer::connect_with_client(wrong, client_for(fixture.addr)).await {
        Ok(_) => panic!("a wrong bearer token must be refused"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind,
        ErrorKind::Provider {
            code: AUTH_FORBIDDEN_CODE.into(),
            retryable: false
        },
        "{error:?}"
    );

    let mut authorized = config(&fixture.endpoint());
    authorized.token = Some(SecretToken::new("sk-fixture-token"));
    let server = connect(&fixture, authorized).await;
    let headers = state.headers();
    assert!(headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && value == "Bearer sk-fixture-token"
    }));
    assert!(!format!("{server:?}").contains("sk-fixture-token"));
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn absent_capability_is_typed_and_never_sent() {
    let state = Arc::new(State::default());
    state.advertise_tools.store(true, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;
    let error = server.list_resources().await.unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::Provider {
            code: CAPABILITY_UNSUPPORTED_CODE.into(),
            retryable: false
        },
        "{error:?}"
    );
    assert_eq!(
        state.resources_calls.load(Ordering::SeqCst),
        0,
        "an unadvertised capability must not be probed"
    );
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_sends_cancellation_and_returns_typed_timeout() {
    let state = State::advertised();
    state.slow_ms.store(2_000, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;
    let started = std::time::Instant::now();
    let error = server
        .call_tool("slow", json!({}), Duration::from_millis(250))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the call must not wait for the server's sleep: {:?}",
        started.elapsed()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while state.cancellations().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let cancelled = state.cancellations();
    assert_eq!(
        cancelled.len(),
        1,
        "the client must send notifications/cancelled"
    );
    assert!(cancelled[0]["requestId"].is_number(), "{cancelled:?}");
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_sse_body_is_timed_out_and_cancelled() {
    let state = State::advertised();
    state.stall_sse.store(true, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let mut cfg = config(&fixture.endpoint());
    cfg.timeout = Duration::from_millis(400);
    let server = connect(&fixture, cfg).await;
    let started = std::time::Instant::now();
    let error = server.list_tools().await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the SSE body budget must bound the read: {:?}",
        started.elapsed()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while state.cancellations().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        state.cancellations().len(),
        1,
        "a body-read timeout must send notifications/cancelled"
    );
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_response_is_refused_within_the_cap() {
    let state = State::advertised();
    state.oversize.store(true, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let mut cfg = config(&fixture.endpoint());
    cfg.output_cap_bytes = 1024;
    let server = connect(&fixture, cfg).await;
    let error = server.list_tools().await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Oversized, "{error:?}");
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_expiry_reinitializes_once_and_retries() {
    let state = State::advertised();
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;
    assert_eq!(server.session_id().await.as_deref(), Some("sess-0"));
    state.expire_next.store(true, Ordering::SeqCst);
    let tools = server.list_tools().await.expect("reconnect retry succeeds");
    assert_eq!(tools.len(), 1);
    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 2);
    assert_eq!(server.session_id().await.as_deref(), Some("sess-1"));
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_session_expiry_is_typed_not_an_endless_retry() {
    let state = State::advertised();
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;
    state.always_expire.store(true, Ordering::SeqCst);
    let error = server.list_tools().await.unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::Provider {
            code: SESSION_EXPIRED_CODE.into(),
            retryable: true
        },
        "{error:?}"
    );
    assert_eq!(
        state.initialize_count.load(Ordering::SeqCst),
        2,
        "the client must re-initialize exactly once per call"
    );
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreachable_endpoint_is_a_typed_transport_error_never_fake_ready() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut config = config(&format!("http://{addr}/mcp"));
    config.timeout = Duration::from_secs(2);
    let error = match RemoteMcpServer::connect_with_client(config, client_for(addr)).await {
        Ok(_) => panic!("a closed port must not produce a ready session"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::Network, "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caller_config_bounds_cannot_exceed_the_transport_ceiling() {
    let state = State::advertised();
    let fixture = Fixture::start(state.clone()).await;
    let mut cap = config(&fixture.endpoint());
    cap.output_cap_bytes = 64 * 1024 * 1024 + 1;
    let error = match RemoteMcpServer::connect_with_client(cap, client_for(fixture.addr)).await {
        Ok(_) => panic!("an unbounded output cap must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::Oversized, "{error:?}");

    let mut timeout = config(&fixture.endpoint());
    timeout.timeout = Duration::from_secs(601);
    let error = match RemoteMcpServer::connect_with_client(timeout, client_for(fixture.addr)).await
    {
        Ok(_) => panic!("an unbounded timeout must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_request_is_refused_before_egress() {
    let state = State::advertised();
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;
    let error = server
        .call_tool(
            "echo",
            json!({"text": "x".repeat(5 * 1024 * 1024)}),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Oversized, "{error:?}");
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_protocol_version_is_typed_not_faked() {
    let state = State::advertised();
    state.bad_protocol.store(true, Ordering::SeqCst);
    let fixture = Fixture::start(state.clone()).await;
    let error = match RemoteMcpServer::connect_with_client(
        config(&fixture.endpoint()),
        client_for(fixture.addr),
    )
    .await
    {
        Ok(_) => panic!("an unsupported protocol version must not produce a session"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind,
        ErrorKind::Provider {
            code: PROTOCOL_CODE.into(),
            retryable: false
        },
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jsonrpc_server_error_is_typed() {
    let state = State::advertised();
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;
    let error = server
        .call_tool("error", json!({}), Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(
        matches!(error.kind, ErrorKind::Provider { .. }),
        "{error:?}"
    );
    assert!(error.message.contains("bad tool args"), "{error:?}");
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_sequential_calls_are_bounded_and_linear() {
    let state = State::advertised();
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;
    let started = std::time::Instant::now();
    for _ in 0..100 {
        let tools = server.list_tools().await.expect("tools/list");
        assert_eq!(tools.len(), 1);
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "100 sequential MCP calls took {elapsed:?}"
    );
    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 1);
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_hundred_concurrent_calls_share_one_session() {
    let state = State::advertised();
    let fixture = Fixture::start(state.clone()).await;
    let server = connect(&fixture, config(&fixture.endpoint())).await;
    let started = std::time::Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..100_u32 {
        let server = server.clone();
        tasks.spawn(async move {
            server
                .call_tool(
                    "echo",
                    json!({"text": format!("{index}")}),
                    Duration::from_secs(5),
                )
                .await
        });
    }
    let mut completed = 0_usize;
    while let Some(result) = tasks.join_next().await {
        result
            .expect("task joins")
            .expect("concurrent tool call succeeds");
        completed += 1;
    }
    assert_eq!(completed, 100);
    assert_eq!(
        state.initialize_count.load(Ordering::SeqCst),
        1,
        "concurrent calls must share the one initialized session"
    );
    let elapsed = started.elapsed();
    eprintln!("perf MCP: 100 concurrent calls in {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(10),
        "100 concurrent MCP calls took {elapsed:?}"
    );
    server.close().await;
}
