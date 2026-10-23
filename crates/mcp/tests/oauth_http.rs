//! Real local OAuth 2.0 + Streamable HTTP MCP fixture.
//!
//! One loopback HTTP/1.1 server is both the MCP resource server and the
//! authorization server: RFC 9728 protected-resource metadata, RFC 8414
//! authorization-server metadata, an authorization endpoint that binds a
//! code to the PKCE S256 challenge, a token endpoint (authorization_code,
//! refresh_token, client_credentials), RFC 7009 revocation, and a bearer
//! protected `/mcp` JSON-RPC endpoint. Every OAuth request goes through the
//! checked egress client with an explicit loopback allowlist.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use faktor_core::error::{Error, ErrorKind};
use faktor_mcp::oauth::{
    ClientCredentials, OAuthClient, OAuthTokenSet, PendingAuthorization, OAUTH_DENIED_CODE,
    OAUTH_EXPIRED_CODE, OAUTH_UNCONFIGURED_CODE,
};
use faktor_mcp::remote::{RemoteAuth, RemoteMcpConfig, RemoteMcpServer, SecretToken};
use faktor_provider::egress::{CheckedHttpClient, EgressAddressPolicy};
use faktor_security::destination::DestinationPolicy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const READ_SCOPE: &str = "mcp.read";
const WRITE_SCOPE: &str = "mcp.write";

#[derive(Clone)]
struct CodeEntry {
    challenge: String,
    redirect_uri: String,
    scope: String,
    used: bool,
}

#[derive(Clone)]
struct AccessEntry;

struct State {
    discovery: AtomicBool,
    corrupt_challenge: AtomicBool,
    wrong_scope: AtomicBool,
    client_credentials: AtomicBool,
    always_reject: AtomicBool,
    client_secret: Option<String>,
    code_seq: AtomicU64,
    access_seq: AtomicU64,
    refresh_seq: AtomicU64,
    codes: Mutex<HashMap<String, CodeEntry>>,
    access: Mutex<HashMap<String, AccessEntry>>,
    refresh: Mutex<HashMap<String, bool>>,
    bearer_headers: Mutex<Vec<String>>,
    refresh_requests: AtomicUsize,
    revoke_requests: AtomicUsize,
    initialize_requests: AtomicUsize,
    token_requests: AtomicUsize,
    access_issued: AtomicUsize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            discovery: AtomicBool::new(true),
            corrupt_challenge: AtomicBool::new(false),
            wrong_scope: AtomicBool::new(false),
            client_credentials: AtomicBool::new(false),
            always_reject: AtomicBool::new(false),
            client_secret: None,
            code_seq: AtomicU64::new(0),
            access_seq: AtomicU64::new(0),
            refresh_seq: AtomicU64::new(0),
            codes: Mutex::new(HashMap::new()),
            access: Mutex::new(HashMap::new()),
            refresh: Mutex::new(HashMap::new()),
            bearer_headers: Mutex::new(Vec::new()),
            refresh_requests: AtomicUsize::new(0),
            revoke_requests: AtomicUsize::new(0),
            initialize_requests: AtomicUsize::new(0),
            token_requests: AtomicUsize::new(0),
            access_issued: AtomicUsize::new(0),
        }
    }
}

impl State {
    fn expire_all_access(&self) {
        self.access.lock().unwrap().clear();
    }

    fn bearer_headers(&self) -> Vec<String> {
        self.bearer_headers.lock().unwrap().clone()
    }
}

struct Fixture {
    addr: std::net::SocketAddr,
    state: Arc<State>,
}

impl Fixture {
    async fn start() -> Self {
        let state = Arc::new(State::default());
        Self::start_with(state).await
    }

    async fn start_with(state: Arc<State>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = state.clone();
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
        Self { addr, state }
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

    fn oauth(&self) -> OAuthClient {
        OAuthClient::with_address_policy(EgressAddressPolicy::LOCAL)
            .with_timeout(Duration::from_secs(5))
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: String,
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
    let target = request_line.next()?.to_string();
    let (path, query) = target
        .split_once('?')
        .map(|(path, query)| (path.to_string(), query.to_string()))
        .unwrap_or((target, String::new()));
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
        path,
        query,
        headers,
        body: String::from_utf8_lossy(&buffer[header_end..]).to_string(),
    })
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, String)],
    body: &[u8],
) {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.flush().await;
}

async fn write_json(stream: &mut TcpStream, status: &str, body: &Value) {
    write_response(
        stream,
        status,
        &[("Content-Type", "application/json".into())],
        body.to_string().as_bytes(),
    )
    .await;
}

fn origin(request: &HttpRequest) -> String {
    format!("http://{}", request.header("host").unwrap_or("127.0.0.1"))
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_map(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(name), decode(value))
        })
        .collect()
}

fn challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

async fn serve(mut stream: TcpStream, state: Arc<State>) -> Option<()> {
    let request = read_request(&mut stream).await?;
    let base = origin(&request);
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/.well-known/oauth-protected-resource")
        | ("GET", "/.well-known/oauth-protected-resource/mcp") => {
            if !state.discovery.load(Ordering::SeqCst) {
                write_response(&mut stream, "404 Not Found", &[], b"").await;
                return Some(());
            }
            write_json(
                &mut stream,
                "200 OK",
                &json!({
                    "resource": format!("{base}/mcp"),
                    "authorization_servers": [base],
                    "scopes_supported": [READ_SCOPE, WRITE_SCOPE]
                }),
            )
            .await;
        }
        ("GET", "/.well-known/oauth-authorization-server") => {
            if !state.discovery.load(Ordering::SeqCst) {
                write_response(&mut stream, "404 Not Found", &[], b"").await;
                return Some(());
            }
            let mut grants = vec!["authorization_code", "refresh_token"];
            if state.client_credentials.load(Ordering::SeqCst) {
                grants.push("client_credentials");
            }
            write_json(
                &mut stream,
                "200 OK",
                &json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                    "revocation_endpoint": format!("{base}/revoke"),
                    "scopes_supported": [READ_SCOPE, WRITE_SCOPE],
                    "code_challenge_methods_supported": ["S256"],
                    "grant_types_supported": grants,
                    "token_endpoint_auth_methods_supported": ["none", "client_secret_post"]
                }),
            )
            .await;
        }
        ("GET", "/authorize") => {
            let params = form_map(&request.query);
            let state_value = params.get("state").cloned().unwrap_or_default();
            let redirect = params.get("redirect_uri").cloned().unwrap_or_default();
            let scope = params.get("scope").cloned().unwrap_or_default();
            let challenge = params.get("code_challenge").cloned().unwrap_or_default();
            if params.get("response_type").map(String::as_str) != Some("code")
                || params.get("code_challenge_method").map(String::as_str) != Some("S256")
                || challenge.is_empty()
                || redirect.is_empty()
            {
                write_json(
                    &mut stream,
                    "400 Bad Request",
                    &json!({"error": "invalid_request"}),
                )
                .await;
                return Some(());
            }
            let seq = state.code_seq.fetch_add(1, Ordering::SeqCst);
            let code = format!("code-{seq}");
            let bound = if state.corrupt_challenge.load(Ordering::SeqCst) {
                "wrong-challenge".to_string()
            } else {
                challenge
            };
            state.codes.lock().unwrap().insert(
                code.clone(),
                CodeEntry {
                    challenge: bound,
                    redirect_uri: redirect.clone(),
                    scope,
                    used: false,
                },
            );
            let location = format!(
                "{redirect}?code={}&state={}",
                encode_component(&code),
                encode_component(&state_value)
            );
            write_response(&mut stream, "302 Found", &[("Location", location)], b"").await;
        }
        ("POST", "/token") => {
            state.token_requests.fetch_add(1, Ordering::SeqCst);
            let form = form_map(&request.body);
            if let Some(expected) = &state.client_secret {
                if form.get("client_secret") != Some(expected) {
                    write_json(
                        &mut stream,
                        "401 Unauthorized",
                        &json!({"error": "invalid_client"}),
                    )
                    .await;
                    return Some(());
                }
            }
            let grant = form.get("grant_type").map(String::as_str).unwrap_or("");
            match grant {
                "authorization_code" => {
                    let code = form.get("code").cloned().unwrap_or_default();
                    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                    let redirect = form.get("redirect_uri").cloned().unwrap_or_default();
                    let entry = {
                        let mut codes = state.codes.lock().unwrap();
                        codes.get_mut(&code).map(|entry| {
                            let valid = !entry.used
                                && entry.redirect_uri == redirect
                                && entry.challenge == challenge(&verifier);
                            if valid {
                                entry.used = true;
                            }
                            (valid, entry.scope.clone())
                        })
                    };
                    let Some((true, scope)) = entry else {
                        write_json(
                            &mut stream,
                            "400 Bad Request",
                            &json!({"error": "invalid_grant", "error_description": "code or PKCE verifier mismatch"}),
                        )
                        .await;
                        return Some(());
                    };
                    issue(&mut stream, &state, Some(scope)).await;
                }
                "refresh_token" => {
                    state.refresh_requests.fetch_add(1, Ordering::SeqCst);
                    let token = form.get("refresh_token").cloned().unwrap_or_default();
                    let valid = state
                        .refresh
                        .lock()
                        .unwrap()
                        .get(&token)
                        .copied()
                        .unwrap_or(false);
                    if !valid {
                        write_json(
                            &mut stream,
                            "400 Bad Request",
                            &json!({"error": "invalid_grant", "error_description": "refresh revoked"}),
                        )
                        .await;
                        return Some(());
                    }
                    issue(&mut stream, &state, None).await;
                }
                "client_credentials" => {
                    if !state.client_credentials.load(Ordering::SeqCst) {
                        write_json(
                            &mut stream,
                            "400 Bad Request",
                            &json!({"error": "unsupported_grant_type"}),
                        )
                        .await;
                        return Some(());
                    }
                    issue(&mut stream, &state, None).await;
                }
                _ => {
                    write_json(
                        &mut stream,
                        "400 Bad Request",
                        &json!({"error": "unsupported_grant_type"}),
                    )
                    .await;
                }
            }
        }
        ("POST", "/revoke") => {
            state.revoke_requests.fetch_add(1, Ordering::SeqCst);
            let form = form_map(&request.body);
            if let Some(token) = form.get("token") {
                state.access.lock().unwrap().remove(token);
                state.refresh.lock().unwrap().remove(token);
            }
            write_response(&mut stream, "200 OK", &[], b"").await;
        }
        ("POST", "/mcp") => serve_mcp(&mut stream, &state, &request).await,
        ("DELETE", "/mcp") => {
            if let Some(authorization) = request.header("authorization") {
                state
                    .bearer_headers
                    .lock()
                    .unwrap()
                    .push(authorization.to_string());
            }
            write_response(&mut stream, "200 OK", &[], b"").await;
        }
        _ => write_response(&mut stream, "404 Not Found", &[], b"").await,
    }
    Some(())
}

async fn issue(stream: &mut TcpStream, state: &State, requested_scope: Option<String>) {
    let access_seq = state.access_seq.fetch_add(1, Ordering::SeqCst);
    let refresh_seq = state.refresh_seq.fetch_add(1, Ordering::SeqCst);
    let access = format!("access-{access_seq}");
    let refresh = format!("refresh-{refresh_seq}");
    let scope = if state.wrong_scope.load(Ordering::SeqCst) {
        READ_SCOPE.to_string()
    } else {
        requested_scope.unwrap_or_else(|| format!("{READ_SCOPE} {WRITE_SCOPE}"))
    };
    state
        .access
        .lock()
        .unwrap()
        .insert(access.clone(), AccessEntry);
    state.refresh.lock().unwrap().insert(refresh.clone(), true);
    state.access_issued.fetch_add(1, Ordering::SeqCst);
    write_json(
        stream,
        "200 OK",
        &json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": refresh,
            "scope": scope
        }),
    )
    .await;
}

async fn serve_mcp(stream: &mut TcpStream, state: &State, request: &HttpRequest) {
    let authorized = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| {
            !state.always_reject.load(Ordering::SeqCst)
                && state.access.lock().unwrap().contains_key(*token)
        });
    let Some(token) = authorized else {
        write_response(
            stream,
            "401 Unauthorized",
            &[(
                "WWW-Authenticate",
                format!(
                    "Bearer resource_metadata=\"http://{}/.well-known/oauth-protected-resource\", scope=\"{READ_SCOPE}\"",
                    request.header("host").unwrap_or("127.0.0.1")
                ),
            )],
            b"",
        )
        .await;
        return;
    };
    state
        .bearer_headers
        .lock()
        .unwrap()
        .push(format!("Bearer {token}"));
    let Ok(message) = serde_json::from_str::<Value>(&request.body) else {
        write_json(stream, "400 Bad Request", &json!({"error": "bad json"})).await;
        return;
    };
    let id = message.get("id").cloned();
    match message.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => {
            state.initialize_requests.fetch_add(1, Ordering::SeqCst);
            write_json(
                stream,
                "200 OK",
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": "oauth-fixture", "version": "1.0"}
                    }
                }),
            )
            .await;
        }
        "notifications/initialized" => {
            write_response(stream, "202 Accepted", &[], b"").await;
        }
        "tools/list" => {
            write_json(
                stream,
                "200 OK",
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"tools": [{"name": "echo", "description": "echoes", "inputSchema": {"type": "object"}}]}
                }),
            )
            .await;
        }
        other => {
            write_json(
                stream,
                "200 OK",
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("unknown method {other}")}
                }),
            )
            .await;
        }
    }
}

fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

async fn http_get(addr: std::net::SocketAddr, url: &str) -> String {
    let parsed = reqwest::Url::parse(url).expect("authorization URL");
    let target = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_string(),
    };
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        parsed.authority()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);
    text.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("location:"))
        .map(|line| line["location:".len()..].trim().to_string())
        .unwrap_or_else(|| panic!("authorization endpoint returned no redirect: {text}"))
}

fn parameter(url: &str, name: &str) -> String {
    let parsed = reqwest::Url::parse(url).expect("callback URL");
    parsed
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("callback has no {name}: {url}"))
}

struct TestAuth {
    client: OAuthClient,
    bundle: tokio::sync::Mutex<OAuthTokenSet>,
    refreshes: Arc<AtomicUsize>,
}

impl RemoteAuth for TestAuth {
    fn token<'a>(&'a self) -> futures::future::BoxFuture<'a, Result<SecretToken, Error>> {
        Box::pin(async move {
            let bundle = self.bundle.lock().await;
            Ok(bundle.access_token())
        })
    }

    fn refresh<'a>(&'a self) -> futures::future::BoxFuture<'a, Result<SecretToken, Error>> {
        Box::pin(async move {
            let mut bundle = self.bundle.lock().await;
            let next = self.client.refresh(&bundle).await?;
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            let token = next.access_token();
            *bundle = next;
            Ok(token)
        })
    }
}

fn test_auth(fixture: &Fixture, tokens: OAuthTokenSet) -> Arc<TestAuth> {
    Arc::new(TestAuth {
        client: fixture.oauth(),
        bundle: tokio::sync::Mutex::new(tokens),
        refreshes: Arc::new(AtomicUsize::new(0)),
    })
}

async fn connect_mcp(fixture: &Fixture, auth: Arc<TestAuth>) -> Arc<RemoteMcpServer> {
    let mut config = RemoteMcpConfig::new("oauth-fixture", fixture.endpoint());
    config.timeout = Duration::from_secs(5);
    config.output_cap_bytes = 1024 * 1024;
    config.auth = Some(auth);
    RemoteMcpServer::connect_with_client(config, fixture.client())
        .await
        .expect("MCP initialize carries the OAuth bearer")
}

async fn complete_flow(
    fixture: &Fixture,
    requested_scopes: Option<Vec<String>>,
) -> Result<OAuthTokenSet, Error> {
    let client = fixture.oauth();
    let discovery = client.discover(&fixture.endpoint(), None).await?;
    let pending = client.begin_authorization(
        &discovery,
        &ClientCredentials {
            client_id: "fixture-client".into(),
            client_secret: None,
        },
        "http://127.0.0.1:9/callback",
        requested_scopes,
        "remote",
        Some("operator".into()),
    )?;
    let callback = http_get(fixture.addr, pending.authorization_url()).await;
    pending.complete(&callback).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovery_pkce_exchange_bearer_and_capability_scopes() {
    let fixture = Fixture::start().await;
    let client = fixture.oauth();

    let challenge = client
        .probe_challenge(&fixture.endpoint())
        .await
        .expect("challenge probe")
        .expect("bearer challenge");
    assert!(challenge
        .resource_metadata
        .as_deref()
        .unwrap()
        .ends_with("/.well-known/oauth-protected-resource"));
    assert_eq!(challenge.scope.as_deref(), Some(READ_SCOPE));

    let discovery = client
        .discover(&fixture.endpoint(), Some(&challenge))
        .await
        .expect("metadata discovery");
    assert_eq!(discovery.resource, fixture.endpoint());
    assert_eq!(
        discovery.authorization_server.issuer,
        format!("http://{}", fixture.addr)
    );
    assert!(discovery
        .authorization_server
        .code_challenge_methods_supported
        .iter()
        .any(|method| method == "S256"));
    assert!(discovery.scopes.contains(&READ_SCOPE.to_string()));

    let tokens = complete_flow(&fixture, Some(vec![READ_SCOPE.to_string()]))
        .await
        .expect("authorization code + PKCE exchange");
    assert!(tokens.scopes_cover(&[READ_SCOPE.to_string()]));
    assert!(tokens.has_refresh());
    assert!(tokens.expires_at_ms.is_some());

    let auth = test_auth(&fixture, tokens);
    let server = connect_mcp(&fixture, auth).await;
    let tools = server.list_tools().await.expect("tools/list");
    assert_eq!(tools[0].name, "echo");
    let bearer = fixture.state.bearer_headers();
    assert!(
        bearer
            .iter()
            .any(|value| value.starts_with("Bearer access-")),
        "{bearer:?}"
    );
    assert!(!format!("{server:?}").contains("access-"));
    server.close().await;
    let after_close = fixture.state.bearer_headers();
    assert!(
        after_close
            .last()
            .is_some_and(|value| value.starts_with("Bearer access-")),
        "session DELETE must carry the same OAuth bearer: {after_close:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pkce_mismatch_is_a_typed_refusal_not_a_token() {
    let fixture = Fixture::start().await;
    fixture
        .state
        .corrupt_challenge
        .store(true, Ordering::SeqCst);
    let error = complete_flow(&fixture, Some(vec![READ_SCOPE.to_string()]))
        .await
        .expect_err("a verifier that does not match the challenge must fail");
    assert_eq!(error_code(&error), OAUTH_DENIED_CODE, "{error:?}");
    assert_eq!(
        fixture.state.access_issued.load(Ordering::SeqCst),
        0,
        "no token may be issued on a PKCE mismatch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_csrf_and_replay_are_refused() {
    let fixture = Fixture::start().await;
    let client = fixture.oauth();
    let discovery = client.discover(&fixture.endpoint(), None).await.unwrap();
    let credentials = ClientCredentials {
        client_id: "fixture-client".into(),
        client_secret: None,
    };

    let first = client
        .begin_authorization(
            &discovery,
            &credentials,
            "http://127.0.0.1:9/callback",
            None,
            "remote",
            None,
        )
        .unwrap();
    let callback = http_get(fixture.addr, first.authorization_url()).await;
    let code = parameter(&callback, "code");
    let error = first
        .complete_with_params(&code, "not-the-flow-state")
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), OAUTH_DENIED_CODE, "{error:?}");

    let second = client
        .begin_authorization(
            &discovery,
            &credentials,
            "http://127.0.0.1:9/callback",
            None,
            "remote",
            None,
        )
        .unwrap();
    let callback = http_get(fixture.addr, second.authorization_url()).await;
    let code = parameter(&callback, "code");
    let state = parameter(&callback, "state");
    let tokens = second
        .complete_with_params(&code, &state)
        .await
        .expect("the correct state succeeds");
    assert!(tokens.has_refresh());
    let replay = second
        .complete_with_params(&code, &state)
        .await
        .unwrap_err();
    assert_eq!(error_code(&replay), OAUTH_DENIED_CODE, "{replay:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_scope_is_a_typed_denial() {
    let fixture = Fixture::start().await;
    fixture.state.wrong_scope.store(true, Ordering::SeqCst);
    let error = complete_flow(
        &fixture,
        Some(vec![READ_SCOPE.to_string(), WRITE_SCOPE.to_string()]),
    )
    .await
    .expect_err("a narrower grant than the requested capability scopes must be refused");
    assert_eq!(error_code(&error), OAUTH_DENIED_CODE, "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unconfigured_metadata_is_typed() {
    let fixture = Fixture::start().await;
    fixture.state.discovery.store(false, Ordering::SeqCst);
    let error = fixture
        .oauth()
        .discover(&fixture.endpoint(), None)
        .await
        .expect_err("a resource with no metadata must not fabricate OAuth");
    assert_eq!(error_code(&error), OAUTH_UNCONFIGURED_CODE, "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiry_401_refreshes_once_and_retries() {
    let fixture = Fixture::start().await;
    let tokens = complete_flow(&fixture, Some(vec![READ_SCOPE.to_string()]))
        .await
        .unwrap();
    let auth = test_auth(&fixture, tokens);
    let refreshes = auth.refreshes.clone();
    let server = connect_mcp(&fixture, auth).await;
    server.list_tools().await.expect("first request");
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);

    fixture.state.expire_all_access();
    let tools = server
        .list_tools()
        .await
        .expect("401 -> refresh -> retry succeeds");
    assert_eq!(tools[0].name, "echo");
    assert_eq!(
        refreshes.load(Ordering::SeqCst),
        1,
        "the client must refresh exactly once after a 401"
    );
    assert_eq!(fixture.state.refresh_requests.load(Ordering::SeqCst), 1);

    server
        .list_tools()
        .await
        .expect("refreshed credential works");
    assert_eq!(
        refreshes.load(Ordering::SeqCst),
        1,
        "a successful retry must not refresh again"
    );
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_credential_rejected_after_refresh_is_typed_expired_and_retried_once() {
    let fixture = Fixture::start().await;
    let tokens = complete_flow(&fixture, Some(vec![READ_SCOPE.to_string()]))
        .await
        .unwrap();
    let auth = test_auth(&fixture, tokens);
    let refreshes = auth.refreshes.clone();
    let server = connect_mcp(&fixture, auth).await;
    fixture.state.always_reject.store(true, Ordering::SeqCst);
    let error = server
        .list_tools()
        .await
        .expect_err("a fresh token the server still rejects must not loop");
    assert_eq!(error_code(&error), OAUTH_EXPIRED_CODE, "{error:?}");
    assert_eq!(
        refreshes.load(Ordering::SeqCst),
        1,
        "exactly one forced refresh precedes the typed failure"
    );
    assert_eq!(fixture.state.refresh_requests.load(Ordering::SeqCst), 1);
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revocation_is_remote_and_the_refresh_becomes_expired() {
    let fixture = Fixture::start().await;
    let tokens = complete_flow(&fixture, Some(vec![READ_SCOPE.to_string()]))
        .await
        .unwrap();
    assert!(tokens.has_refresh());
    let outcome = fixture
        .oauth()
        .revoke(&tokens)
        .await
        .expect("revocation request");
    assert!(outcome.remote, "{outcome:?}");
    assert_eq!(
        fixture.state.revoke_requests.load(Ordering::SeqCst),
        2,
        "both the refresh token and the access token are revoked"
    );
    let error = fixture
        .oauth()
        .refresh(&tokens)
        .await
        .expect_err("a revoked refresh token cannot mint a new access token");
    assert_eq!(error_code(&error), OAUTH_EXPIRED_CODE, "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_credentials_are_supported_when_advertised() {
    let state = Arc::new(State {
        client_credentials: AtomicBool::new(true),
        client_secret: Some("client-secret-fixture".into()),
        ..State::default()
    });
    let fixture = Fixture::start_with(state).await;
    let discovery = fixture
        .oauth()
        .discover(&fixture.endpoint(), None)
        .await
        .unwrap();
    let credentials = ClientCredentials {
        client_id: "machine-client".into(),
        client_secret: Some(SecretToken::new("client-secret-fixture")),
    };
    let tokens = fixture
        .oauth()
        .client_credentials(
            &discovery,
            &credentials,
            Some(vec![READ_SCOPE.to_string()]),
            "remote",
            Some("machine".into()),
        )
        .await
        .expect("client_credentials exchange");
    assert!(tokens.scopes_cover(&[READ_SCOPE.to_string()]));
    let mut expired = tokens.clone();
    expired.expires_at_ms = Some(0);
    let reminted = fixture
        .oauth()
        .refresh(&expired)
        .await
        .expect("an expired client_credentials bundle is re-minted");
    assert!(reminted.scopes_cover(&[READ_SCOPE.to_string()]));
    assert!(
        reminted.has_refresh(),
        "the fixture still issues a refresh token"
    );
    let auth = test_auth(&fixture, tokens);
    let server = connect_mcp(&fixture, auth).await;
    assert_eq!(server.list_tools().await.unwrap()[0].name, "echo");
    server.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_credentials_unsupported_is_typed_unconfigured() {
    let fixture = Fixture::start().await;
    let discovery = fixture
        .oauth()
        .discover(&fixture.endpoint(), None)
        .await
        .unwrap();
    let error = fixture
        .oauth()
        .client_credentials(
            &discovery,
            &ClientCredentials {
                client_id: "machine-client".into(),
                client_secret: Some(SecretToken::new("secret")),
            },
            None,
            "remote",
            None,
        )
        .await
        .expect_err("client_credentials must not be attempted when not advertised");
    assert_eq!(error_code(&error), OAUTH_UNCONFIGURED_CODE, "{error:?}");
}

fn error_code(error: &Error) -> &str {
    match &error.kind {
        ErrorKind::Provider { code, .. } => code.as_str(),
        other => panic!("expected a typed provider error, got {other:?}"),
    }
}

// Keep the unused-import gate honest: these types are part of the public
// fixture surface even when a test does not touch them directly.
const _: fn() = || {
    let _: Option<PendingAuthorization> = None;
    let _: Option<Value> = None;
};
