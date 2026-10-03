//! Adversarial provider egress tests (task category 7, HTTP leg) plus the
//! live redirect/budget/gate behavior. Production entry points:
//! [`PolicyCheckedHttpTransport`], [`CheckedHttpClient`],
//! [`MockHttpTransport`], [`ResponseBudget`]/[`BudgetedBody`],
//! [`validate_provider_base_url`] and the `EgressError` → `ProviderError`
//! mapping.

use super::*;
use crate::egress::{
    execute_get, execute_post_json, validate_provider_base_url, BudgetComponent, BudgetedBody,
    CheckedHttpClient, EgressError, MockHttpTransport, OutboundScanConfig,
    PolicyCheckedHttpTransport, RawRequest, ResponseBudget, SafeUrlDiagnostic, UrlRejectReason,
};
use crate::transport::MAX_STREAM_BODY_BYTES;
use faktor_security::destination::{DeniedReason, DestinationPolicy, RuleMatch};
use faktor_security::network::EgressAddressPolicy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ------------------------------------------------------------- tiny server

#[derive(Clone, Copy)]
enum Mode {
    Immediate,
    /// Sleep before writing ANY byte (tests the head deadline).
    DelayHead(u64),
    /// Write the first `first` body bytes, sleep `gap_ms`, write the rest
    /// (tests idle/frame accounting).
    Split {
        first: usize,
        gap_ms: u64,
    },
    /// One byte every `per_byte_ms` (tests the total deadline).
    Drip {
        per_byte_ms: u64,
    },
}

#[derive(Clone)]
struct Route {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
    mode: Mode,
}

impl Route {
    fn ok(body: &str) -> Route {
        Route {
            status: 200,
            location: None,
            body: body.as_bytes().to_vec(),
            mode: Mode::Immediate,
        }
    }
    fn redirect(status: u16, location: &str) -> Route {
        Route {
            status,
            location: Some(location.to_string()),
            body: Vec::new(),
            mode: Mode::Immediate,
        }
    }
    fn with_mode(mut self, mode: Mode) -> Route {
        self.mode = mode;
        self
    }
}

#[derive(Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct Server {
    addr: std::net::SocketAddr,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    async fn start() -> Arc<Server> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test server bind");
        let addr = listener.local_addr().expect("local addr");
        let server = Arc::new(Server {
            addr,
            routes: Arc::new(Mutex::new(HashMap::new())),
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let accept_server = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let server = accept_server.clone();
                tokio::spawn(async move {
                    handle_conn(&mut socket, &server).await;
                });
            }
        });
        server
    }

    fn route(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn seen_paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.path.clone())
            .collect()
    }

    fn last_for(&self, path: &str) -> Option<Seen> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|s| s.path == path)
            .cloned()
    }
}

async fn handle_conn(socket: &mut TcpStream, server: &Arc<Server>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = match socket.read(&mut tmp).await {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
            break;
        }
    }
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();
    let mut content_length = 0usize;
    let mut headers = Vec::new();
    for line in lines {
        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_lowercase(), value.trim().to_string()));
        }
    }
    while buf.len() < header_end + content_length {
        let n = socket.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf
        .get(header_end..header_end + content_length)
        .unwrap_or(&[])
        .to_vec();
    server.seen.lock().unwrap().push(Seen {
        method,
        path: path.clone(),
        headers,
        body,
    });
    let route = server.routes.lock().unwrap().get(&path).cloned();
    let Some(route) = route else {
        let _ = socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        route.status,
        route.body.len()
    );
    if let Some(location) = &route.location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    head.push_str("\r\n");
    if let Mode::DelayHead(_) = route.mode {
        // Headers (and the declared length) arrive immediately; only the
        // BODY is delayed, which is exactly what the head deadline bounds.
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.flush().await;
    }
    match route.mode {
        Mode::DelayHead(ms) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let _ = socket.write_all(&route.body).await;
        }
        Mode::Split { first, gap_ms } => {
            let first = first.min(route.body.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&route.body[..first]).await;
            let _ = socket.flush().await;
            tokio::time::sleep(Duration::from_millis(gap_ms)).await;
            let _ = socket.write_all(&route.body[first..]).await;
        }
        Mode::Drip { per_byte_ms } => {
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.flush().await;
            for b in &route.body {
                let _ = socket.write_all(&[*b]).await;
                let _ = socket.flush().await;
                tokio::time::sleep(Duration::from_millis(per_byte_ms)).await;
            }
        }
        _ => {
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&route.body).await;
        }
    }
}

fn local_policy() -> DestinationPolicy {
    DestinationPolicy::parse_lines(["127.0.0.1"]).expect("loopback rule")
}

fn budget(
    head_ms: u64,
    idle_ms: u64,
    total_ms: u64,
    max_bytes: u64,
    frames: Option<u64>,
) -> ResponseBudget {
    ResponseBudget::from_millis(head_ms, idle_ms, total_ms, max_bytes, frames)
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn policy_gate_denies_before_any_connect_and_address_rules_hold() {
    let server = Server::start().await;
    server.route("/ok", Route::ok("hello"));

    // Empty allowlist: every destination denied before a connect is made.
    let denying = PolicyCheckedHttpTransport::with_policy_for_tests(DestinationPolicy::empty());
    for path in ["/ok", "/other"] {
        let err = execute_get(&denying, &server.url(path))
            .await
            .expect_err(&format!("empty policy must deny {path}"));
        assert!(
            matches!(err, EgressError::Denied { .. }),
            "denial variant for {path}: {err:?}"
        );
    }
    assert!(
        server.seen().is_empty(),
        "denied requests must never reach the server"
    );

    // Allowed destination reaches the server and the body is readable under
    // a budget.
    let allowing = PolicyCheckedHttpTransport::with_policy_for_tests(local_policy());
    let response = execute_get(&allowing, &server.url("/ok"))
        .await
        .expect("allowed loopback request");
    assert_eq!(response.status(), 200, "allowed status");
    let bytes = response
        .read_bytes(&budget(5_000, 5_000, 5_000, 1024, None))
        .await
        .expect("budgeted read");
    assert_eq!(bytes, b"hello", "budgeted body");
    assert_eq!(
        server.seen_paths(),
        vec!["/ok"],
        "server saw exactly one request"
    );

    // An explicitly NAMED literal loopback address is admitted even under
    // the external-only address rule (the operator named that exact
    // address; frozen by the existing literal/resolved test pair).
    let external = PolicyCheckedHttpTransport::try_with_policy_scan_and_addresses(
        local_policy(),
        None,
        EgressAddressPolicy::EXTERNAL,
    )
    .expect("transport build");
    execute_get(&external, &server.url("/ok"))
        .await
        .expect("an explicitly named literal loopback is admitted");

    // The SAME external rule refuses a HOSTNAME that resolves to loopback
    // (the rebinding vector), while LOCAL admits it explicitly.
    let port = server.addr.port();
    let hostname_rule = format!("http://allowed.example:{port}");
    let hostname_policy =
        DestinationPolicy::parse_lines([hostname_rule.as_str()]).expect("hostname rule");
    let name_url = format!("http://allowed.example:{port}/ok");
    let resolver = Arc::new(FixedResolver(vec![std::net::SocketAddr::new(
        "127.0.0.1".parse().unwrap(),
        port,
    )]));
    let rebinding = CheckedHttpClient::with_injected_resolver_for_tests(
        hostname_policy.clone(),
        EgressAddressPolicy::EXTERNAL,
        resolver.clone(),
        crate::resolver::DEFAULT_MAX_DNS_ANSWERS,
    );
    let err = rebinding
        .send_checked(rebinding.get(&name_url).unwrap())
        .await
        .expect_err("EXTERNAL must refuse a rebinding answer");
    match err {
        EgressError::AddressClassRefused { class, .. } => {
            assert_eq!(class.as_str(), "loopback", "refused class");
        }
        other => panic!("address-class refusal expected, got {other:?}"),
    }
    let local = CheckedHttpClient::with_injected_resolver_for_tests(
        hostname_policy,
        EgressAddressPolicy::LOCAL,
        resolver,
        crate::resolver::DEFAULT_MAX_DNS_ANSWERS,
    );
    let response = local
        .send_checked(local.get(&name_url).unwrap())
        .await
        .expect("LOCAL permits the resolved loopback");
    assert_eq!(response.status(), 200, "LOCAL name request status");

    // A non-http(s) scheme is refused by the transport seam.
    for url in ["ftp://127.0.0.1/x", "file:///etc/passwd"] {
        let err = execute_get(&allowing, url)
            .await
            .expect_err(&format!("{url} must be refused"));
        assert!(
            matches!(err, EgressError::UnsupportedScheme(_)),
            "scheme refusal for {url}: {err:?}"
        );
    }
    // URL tricks are refused before any connect.
    for url in [
        format!("http://user:pw@{}/ok", server.addr),
        format!("http://user@{}/ok", server.addr),
        format!("http://{}:0/ok", server.addr),
    ] {
        let result = execute_get(&allowing, &url).await;
        assert!(
            result.is_err(),
            "hostile URL {url:?} must be refused, got {:?}",
            result.map(|r| r.status())
        );
    }
    assert_eq!(
        server.seen().len(),
        3,
        "only the named-literal and LOCAL name requests reached the server"
    );
}

/// A deterministic resolver for the rebinding leg: always answers the fixed
/// address list (production uses the OS resolver through the same trait).
#[derive(Debug)]
struct FixedResolver(Vec<std::net::SocketAddr>);

impl crate::resolver::HostResolver for FixedResolver {
    fn resolve(
        &self,
        _host: &str,
        _port: u16,
    ) -> futures::future::BoxFuture<'_, Result<Vec<std::net::SocketAddr>, String>> {
        let addrs = self.0.clone();
        Box::pin(async move { Ok(addrs) })
    }
}

#[tokio::test]
async fn response_budget_enforces_each_component() {
    let server = Server::start().await;
    server.route("/head", Route::ok("body").with_mode(Mode::DelayHead(5_000)));
    server.route(
        "/idle",
        Route::ok("abcdef").with_mode(Mode::Split {
            first: 3,
            gap_ms: 60_000,
        }),
    );
    server.route(
        "/frames",
        Route::ok("abcdef").with_mode(Mode::Split {
            first: 3,
            gap_ms: 5,
        }),
    );
    server.route(
        "/drip",
        Route::ok("0123456789").with_mode(Mode::Drip { per_byte_ms: 30 }),
    );
    server.route("/bytes", Route::ok(&"x".repeat(100)));
    let transport = PolicyCheckedHttpTransport::with_policy_for_tests(local_policy());

    // Head: no first body byte inside the head window.
    let response = execute_get(&transport, &server.url("/head")).await.unwrap();
    let err = response
        .read_bytes(&budget(50, 5_000, 5_000, 1024, None))
        .await
        .expect_err("head budget must fire");
    match err {
        EgressError::ResponseBudgetExceeded { component, .. } => {
            assert_eq!(component, BudgetComponent::Head, "head component");
        }
        other => panic!("head budget variant: {other:?}"),
    }

    // Idle: silence between chunks.
    let response = execute_get(&transport, &server.url("/idle")).await.unwrap();
    let err = response
        .read_bytes(&budget(5_000, 50, 5_000, 1024, None))
        .await
        .expect_err("idle budget must fire");
    match err {
        EgressError::ResponseBudgetExceeded { component, .. } => {
            assert_eq!(component, BudgetComponent::Idle, "idle component");
        }
        other => panic!("idle budget variant: {other:?}"),
    }

    // Total: a slow drip outlives the overall deadline.
    let response = execute_get(&transport, &server.url("/drip")).await.unwrap();
    let err = response
        .read_bytes(&budget(5_000, 5_000, 60, 1024, None))
        .await
        .expect_err("total budget must fire");
    match err {
        EgressError::ResponseBudgetExceeded { component, .. } => {
            assert_eq!(component, BudgetComponent::Total, "total component");
        }
        other => panic!("total budget variant: {other:?}"),
    }

    // Bytes: the chunk that would cross the cap is refused.
    let response = execute_get(&transport, &server.url("/bytes"))
        .await
        .unwrap();
    let err = response
        .read_bytes(&budget(5_000, 5_000, 5_000, 10, None))
        .await
        .expect_err("byte cap must fire");
    match err {
        EgressError::ResponseBudgetExceeded { component, .. } => {
            assert_eq!(component, BudgetComponent::Bytes, "bytes component");
        }
        other => panic!("bytes budget variant: {other:?}"),
    }

    // Frames: a body arriving in two chunks exceeds a one-frame cap.
    let response = execute_get(&transport, &server.url("/frames"))
        .await
        .unwrap();
    let err = response
        .read_bytes(&budget(5_000, 5_000, 5_000, 1024, Some(1)))
        .await
        .expect_err("frame cap must fire");
    match err {
        EgressError::ResponseBudgetExceeded { component, .. } => {
            assert_eq!(component, BudgetComponent::Frames, "frames component");
        }
        other => panic!("frames budget variant: {other:?}"),
    }

    // A generous budget reads the whole body.
    let response = execute_get(&transport, &server.url("/bytes"))
        .await
        .unwrap();
    let bytes = response
        .read_bytes(&budget(5_000, 5_000, 5_000, 1024, None))
        .await
        .expect("generous budget");
    assert_eq!(bytes.len(), 100, "full body under a generous budget");

    // An unbound reader refuses before any read.
    let raw = reqwest::Response::from(
        http::Response::builder()
            .status(200)
            .body(reqwest::Body::from("x"))
            .expect("mock response"),
    );
    let mut unbound = BudgetedBody::unbudgeted(raw);
    assert!(
        matches!(
            unbound.next_chunk().await,
            Err(EgressError::UnboundResponseBody)
        ),
        "unbound budget is a typed refusal"
    );

    // ResponseBudget zero fields are caller data, not a silent unbounded
    // read: a zero head window fires immediately on a delayed server.
    let response = execute_get(&transport, &server.url("/head")).await.unwrap();
    let zero = ResponseBudget::from_millis(0, 0, 0, 10_000, None);
    match response.read_bytes(&zero).await {
        Err(EgressError::ResponseBudgetExceeded { component, .. }) => {
            assert!(
                matches!(component, BudgetComponent::Head | BudgetComponent::Total),
                "a zero budget must fire head/total, got {component:?}"
            );
        }
        other => panic!("zero budget must fire: {other:?}"),
    }
}

#[tokio::test]
async fn redirect_method_and_body_semantics_are_exact() {
    let server = Server::start().await;
    for status in [301u16, 302, 303, 307, 308] {
        server.route(
            &format!("/r{status}"),
            Route::redirect(status, &format!("/d{status}")),
        );
        server.route(&format!("/d{status}"), Route::ok("landed"));
    }
    let client = CheckedHttpClient::with_policy_for_tests(local_policy());

    // GET is preserved through every redirect status.
    for status in [301u16, 302, 303, 307, 308] {
        let response = client
            .send_checked(client.get(&server.url(&format!("/r{status}"))).unwrap())
            .await
            .unwrap_or_else(|e| panic!("GET redirect {status}: {e}"));
        assert_eq!(response.status(), 200, "GET final status for {status}");
        let seen = server
            .last_for(&format!("/d{status}"))
            .unwrap_or_else(|| panic!("dest for {status} not reached"));
        assert_eq!(seen.method, "GET", "GET method preserved via {status}");
        assert!(seen.body.is_empty(), "GET body empty via {status}");
    }

    // POST semantics: 301/302/303 become GET without a body; 307/308
    // preserve method and body.
    for (status, want_method, want_body) in [
        (301u16, "GET", ""),
        (302, "GET", ""),
        (303, "GET", ""),
        (307, "POST", "payload"),
        (308, "POST", "payload"),
    ] {
        let response = client
            .send_checked(
                client
                    .post(&server.url(&format!("/r{status}")))
                    .unwrap()
                    .body("payload"),
            )
            .await
            .unwrap_or_else(|e| panic!("POST redirect {status}: {e}"));
        assert_eq!(response.status(), 200, "POST final status for {status}");
        let seen = server
            .last_for(&format!("/d{status}"))
            .unwrap_or_else(|| panic!("dest for {status} not reached"));
        assert_eq!(seen.method, want_method, "method after {status} redirect");
        assert_eq!(
            String::from_utf8_lossy(&seen.body),
            want_body,
            "body after {status} redirect"
        );
    }
}

#[tokio::test]
async fn redirect_hop_limit_location_tricks_and_stream_bodies() {
    let server = Server::start().await;
    // A 10-redirect chain lands; an 11-redirect chain is refused and the
    // over-limit hop is never requested.
    for i in 0..=9 {
        server.route(
            &format!("/c{i}"),
            Route::redirect(302, &format!("/c{}", i + 1)),
        );
    }
    server.route("/c10", Route::ok("chain-ok"));
    for i in 0..=10 {
        server.route(
            &format!("/h{i}"),
            Route::redirect(302, &format!("/h{}", i + 1)),
        );
    }
    server.route("/h11", Route::ok("never"));
    // Hostile Location values.
    server.route("/u", {
        let mut r = Route::ok("");
        r.status = 302;
        r.location = Some(format!("http://user:pw@{}/ok", server.addr));
        r
    });
    server.route("/scheme", {
        let mut r = Route::ok("");
        r.status = 302;
        r.location = Some("file:///etc/passwd".into());
        r
    });
    server.route("/meta", {
        let mut r = Route::ok("");
        r.status = 302;
        r.location = Some("http://169.254.169.254/latest/meta-data/".into());
        r
    });
    server.route("/r307", Route::redirect(307, "/d307"));
    server.route("/d307", Route::ok("streamed"));
    server.route("/ok", Route::ok("ok"));

    let client = CheckedHttpClient::with_policy_for_tests(local_policy());

    // Exactly 10 hops are allowed.
    let response = client
        .send_checked(client.get(&server.url("/c0")).unwrap())
        .await
        .expect("10-hop chain must be allowed");
    assert_eq!(response.status(), 200, "10-hop final status");
    assert!(
        server.seen_paths().contains(&"/c10".to_string()),
        "the 10th hop target was requested"
    );

    // An 11th redirect is refused before the over-limit hop is sent.
    let err = client
        .send_checked(client.get(&server.url("/h0")).unwrap())
        .await
        .expect_err("11-hop chain must refuse");
    assert!(
        matches!(err, EgressError::TooManyRedirects { .. }),
        "TooManyRedirects variant: {err:?}"
    );
    assert!(
        !server.seen_paths().contains(&"/h11".to_string()),
        "the over-limit hop must never be requested"
    );

    // Location with userinfo is refused typed.
    let err = client
        .send_checked(client.get(&server.url("/u")).unwrap())
        .await
        .expect_err("userinfo Location must refuse");
    match err {
        EgressError::UnparseableUrl(UrlRejectReason::RedirectUserinfoNotAllowed) => {}
        other => panic!("userinfo Location variant: {other:?}"),
    }

    // A non-http(s) Location is refused.
    let err = client
        .send_checked(client.get(&server.url("/scheme")).unwrap())
        .await
        .expect_err("file:// Location must refuse");
    assert!(
        matches!(err, EgressError::UnsupportedScheme(_)),
        "scheme Location variant: {err:?}"
    );

    // A Location into a special-range literal is denied by the address gate.
    let err = client
        .send_checked(client.get(&server.url("/meta")).unwrap())
        .await
        .expect_err("metadata Location must refuse");
    assert!(
        matches!(
            err,
            EgressError::Denied { .. } | EgressError::AddressClassRefused { .. }
        ),
        "metadata Location variant: {err:?}"
    );

    // A 307 with a non-replayable stream body is refused, never replayed
    // body-less.
    let stream = futures::stream::iter(vec![Ok::<Vec<u8>, std::io::Error>(b"streamed".to_vec())]);
    let err = client
        .send_checked(
            client
                .post(&server.url("/r307"))
                .unwrap()
                .body(reqwest::Body::wrap_stream(stream)),
        )
        .await
        .expect_err("stream body through 307 must refuse");
    assert!(
        matches!(err, EgressError::RedirectBodyNotReplayable { .. }),
        "stream 307 variant: {err:?}"
    );
}

#[tokio::test]
async fn cross_origin_redirects_strip_credentials() {
    let origin_a = Server::start().await;
    let origin_b = Server::start().await;
    origin_a.route("/cross", Route::redirect(302, &origin_b.url("/dest")));
    origin_a.route("/same", Route::redirect(302, "/dest"));
    origin_a.route("/dest", Route::ok("same"));
    origin_b.route("/dest", Route::ok("cross"));
    let client = CheckedHttpClient::with_policy_for_tests(local_policy());

    // Cross-origin (different port): credentials are stripped.
    client
        .send_checked(
            client
                .get(&origin_a.url("/cross"))
                .unwrap()
                .header("authorization", "Bearer cross-origin-token")
                .header("cookie", "session=abc")
                .header("x-api-key", "not-a-credential-header-for-strip"),
        )
        .await
        .expect("cross-origin redirect");
    let seen = origin_b.last_for("/dest").expect("origin B request");
    assert!(
        seen.header("authorization").is_none(),
        "authorization must be stripped cross-origin: {:?}",
        seen.headers
    );
    assert!(
        seen.header("cookie").is_none(),
        "cookie must be stripped cross-origin"
    );
    assert!(
        seen.header("x-api-key").is_some(),
        "non-listed headers are not stripped"
    );

    // Same origin: credentials are kept.
    client
        .send_checked(
            client
                .get(&origin_a.url("/same"))
                .unwrap()
                .header("authorization", "Bearer same-origin-token")
                .header("cookie", "session=abc"),
        )
        .await
        .expect("same-origin redirect");
    let seen = origin_a
        .seen()
        .into_iter()
        .rev()
        .find(|s| s.path == "/dest")
        .expect("same-origin dest");
    assert_eq!(
        seen.header("authorization"),
        Some("Bearer same-origin-token"),
        "same-origin authorization is kept"
    );
    assert_eq!(
        seen.header("cookie"),
        Some("session=abc"),
        "same-origin cookie is kept"
    );
}

#[tokio::test]
async fn outbound_secret_scan_blocks_before_connect() {
    let server = Server::start().await;
    server.route("/v1/chat", Route::ok("{\"ok\":true}"));
    let transport = PolicyCheckedHttpTransport::with_policy_and_scan_for_tests(
        local_policy(),
        Some(OutboundScanConfig::default()),
    );
    let url = server.url("/v1/chat");

    // A body carrying a default-pattern secret is blocked and never sent.
    let secret_body = serde_json::json!({
        "prompt": format!("leak sk-{}", "Q".repeat(24)),
    });
    let err = execute_post_json(
        &transport,
        &url,
        reqwest::header::HeaderMap::new(),
        &secret_body,
    )
    .await
    .expect_err("secret body must be blocked");
    match err {
        EgressError::SecretBlocked { kinds } => {
            assert!(
                kinds.iter().any(|k| k == "openai_key"),
                "canonical kind only, got {kinds:?}"
            );
        }
        other => panic!("SecretBlocked variant: {other:?}"),
    }
    assert!(
        server.seen().is_empty(),
        "a blocked body must never reach the server"
    );

    // A clean body is sent and answered.
    let clean = serde_json::json!({"prompt": "hello"});
    let response = execute_post_json(&transport, &url, reqwest::header::HeaderMap::new(), &clean)
        .await
        .expect("clean body");
    assert_eq!(response.status(), 200, "clean body status");
    assert_eq!(server.seen().len(), 1, "clean body reached the server");

    // A configured absolute cap fails closed for an oversized body.
    let capped = PolicyCheckedHttpTransport::with_policy_and_scan_for_tests(
        local_policy(),
        Some(OutboundScanConfig {
            policy: faktor_security::payload::ScanPolicy {
                max_payload_bytes: Some(4),
                overlap_max: 4096,
            },
            block_on_secret: true,
            registry: None,
        }),
    );
    let err = execute_post_json(
        &capped,
        &url,
        reqwest::header::HeaderMap::new(),
        &serde_json::json!({"prompt": "far too large for the cap"}),
    )
    .await
    .expect_err("oversized body must fail closed");
    assert!(
        matches!(err, EgressError::BodyTooLarge { .. }),
        "BodyTooLarge variant: {err:?}"
    );
}

#[tokio::test]
async fn mock_transport_and_raw_request_contracts() {
    let mock = MockHttpTransport::new(429, "slow down");
    let response = execute_get(&mock, "https://api.example.test/v1/x")
        .await
        .expect("mock executes");
    assert_eq!(response.status(), 429, "mock status preserved");
    let bytes = response
        .read_bytes(&budget(1_000, 1_000, 1_000, 1024, None))
        .await
        .expect("mock body");
    assert_eq!(bytes, b"slow down", "mock body");
    assert_eq!(
        mock.requests(),
        vec![(
            "GET".to_string(),
            "https://api.example.test/v1/x".to_string()
        )],
        "mock records method+url"
    );
    assert_eq!(mock.request_count(), 1, "mock count");

    let denying = MockHttpTransport::denying(EgressError::BodyTooLarge { limit_bytes: 7 });
    let err = execute_get(&denying, "https://api.example.test/v1/x")
        .await
        .expect_err("denying mock");
    assert!(
        matches!(err, EgressError::BodyTooLarge { limit_bytes: 7 }),
        "deny variant passthrough: {err:?}"
    );
    assert_eq!(
        denying.request_count(),
        1,
        "the execution attempt is recorded even when denied"
    );

    // RawRequest Debug never echoes query/fragment/userinfo.
    let raw = RawRequest::new("POST", "https://api.example.test/v1/x?key=supersecret#frag")
        .header("authorization", "Bearer raw-secret-token")
        .header("content-type", "application/json");
    let debug = format!("{raw:?}");
    assert!(
        !debug.contains("supersecret"),
        "RawRequest Debug leaks the query: {debug}"
    );
    assert!(
        !debug.contains("raw-secret-token"),
        "RawRequest Debug leaks a credential header: {debug}"
    );
    assert!(
        debug.contains("api.example.test"),
        "RawRequest Debug names the host: {debug}"
    );
}

#[test]
fn provider_base_url_validation_matrix() {
    let accepted = [
        "https://api.openai.com",
        "https://api.openai.com/v1",
        "http://127.0.0.1:11434",
        "http://localhost:8080/v1/",
        "https://[::1]:8443",
    ];
    for raw in accepted {
        assert!(
            validate_provider_base_url(raw).is_ok(),
            "base URL {raw:?} must be accepted"
        );
    }
    let rejected = [
        ("", "parse"),
        ("not a url", "parse"),
        ("ftp://api.example.com", "scheme"),
        ("ws://api.example.com", "scheme"),
        ("file:///etc/passwd", "scheme"),
        ("https://user:pw@api.example.com", "userinfo"),
        ("https://token@api.example.com", "userinfo"),
        ("https://:443", "parse"),
        ("https://", "parse"),
        ("https://api.example.com:0", "port 0"),
    ];
    for (raw, needle) in rejected {
        let err = validate_provider_base_url(raw)
            .expect_err(&format!("base URL {raw:?} must be refused"));
        let rendered = err.to_string().to_lowercase();
        assert!(
            rendered.contains(needle),
            "refusal for {raw:?} must mention {needle:?}: {rendered}"
        );
        assert!(
            !rendered.contains(raw) || raw.is_empty(),
            "refusal must not echo the hostile input {raw:?}: {rendered}"
        );
    }
}

#[test]
fn safe_url_diagnostic_never_carries_credentials_or_raw_path_bytes() {
    let diagnostic = SafeUrlDiagnostic::from_raw(
        "https://user:password@api.example.com:8443/a/b/c?api_key=supersecret#frag",
    );
    let rendered = format!("{diagnostic}");
    assert_eq!(diagnostic.scheme(), "https", "scheme");
    assert_eq!(diagnostic.host(), "api.example.com", "host");
    assert_eq!(diagnostic.port(), 8443, "port");
    assert_eq!(diagnostic.path_segments(), 3, "segment count only");
    assert!(diagnostic.query_present(), "query presence flag");
    assert!(
        !rendered.contains("password") && !rendered.contains("supersecret"),
        "diagnostic leaks credentials: {rendered}"
    );
    assert!(
        !rendered.contains("/a/b/c"),
        "diagnostic leaks path bytes: {rendered}"
    );
    assert!(
        !rendered.contains("frag"),
        "diagnostic leaks the fragment: {rendered}"
    );
    // from_raw with unparseable input stays credential-free too.
    let hostile = SafeUrlDiagnostic::from_raw("https://user:pw@[not-an-ip]/?token=abc");
    let rendered = format!("{hostile:?}");
    assert!(
        !rendered.contains("pw") && !rendered.contains("token=abc"),
        "unparseable diagnostic leaks: {rendered}"
    );
}

#[test]
fn egress_error_to_provider_error_mapping_is_retry_exact() {
    let diag = SafeUrlDiagnostic::from_raw("https://api.example.com/x");
    // Transport failures are retryable Network.
    let mapped = ProviderError::from(EgressError::Transport("connect reset".into()));
    assert_eq!(mapped.kind, ProviderErrorKind::Network, "transport kind");
    assert!(mapped.retryable, "transport is retryable");

    // Stalled head/idle/total reads are retryable Network.
    for component in [
        BudgetComponent::Head,
        BudgetComponent::Idle,
        BudgetComponent::Total,
    ] {
        let mapped = ProviderError::from(EgressError::ResponseBudgetExceeded {
            url: diag.clone(),
            component,
            limit: 10,
        });
        assert_eq!(
            mapped.kind,
            ProviderErrorKind::Network,
            "{component:?} kind"
        );
        assert!(mapped.retryable, "{component:?} is retryable");
    }
    // Byte/frame breaches are terminal BadRequest.
    for component in [BudgetComponent::Bytes, BudgetComponent::Frames] {
        let mapped = ProviderError::from(EgressError::ResponseBudgetExceeded {
            url: diag.clone(),
            component,
            limit: 10,
        });
        assert_eq!(
            mapped.kind,
            ProviderErrorKind::BadRequest,
            "{component:?} kind"
        );
        assert!(!mapped.retryable, "{component:?} is terminal");
    }
    // Malformed JSON is terminal Malformed.
    let mapped = ProviderError::from(EgressError::MalformedResponse {
        detail: "expected value".into(),
    });
    assert_eq!(mapped.kind, ProviderErrorKind::Malformed, "malformed kind");
    assert!(!mapped.retryable, "malformed is terminal");
    // A denied destination is terminal BadRequest: never retried.
    let mapped = ProviderError::from(EgressError::Denied {
        url: diag.clone(),
        reason: DeniedReason {
            rule_fired: None,
            matched: RuleMatch::None,
        },
    });
    assert_eq!(mapped.kind, ProviderErrorKind::BadRequest, "denied kind");
    assert!(!mapped.retryable, "denied is terminal");
    // Secret-blocked and oversized bodies are terminal too.
    for err in [
        EgressError::SecretBlocked {
            kinds: vec!["openai_key".into()],
        },
        EgressError::BodyTooLarge { limit_bytes: 1 },
        EgressError::ClientBuild {
            detail: "tls backend".into(),
        },
        EgressError::UnboundResponseBody,
        EgressError::UnsupportedScheme("ftp".into()),
    ] {
        let mapped = ProviderError::from(err.clone());
        assert!(!mapped.retryable, "{err:?} must be terminal, got retryable");
    }
    // The stream body cap is a fixed frozen bound.
    assert_eq!(
        MAX_STREAM_BODY_BYTES,
        64 * 1024 * 1024,
        "provider body cap is frozen"
    );
}
