//! Egress broker tests over real loopback sockets: destination filtering,
//! accounting, upstream proxy selection and credential isolation, health and
//! shutdown. No external network is used.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use faktor_browser::{
    BrokerConfig, BrokerState, DestinationPolicy, EgressAddressPolicy, EgressBroker, HostPattern,
    ProxyCredentials, UpstreamProxy, UpstreamSelector,
};

async fn spawn_origin(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

/// A fake upstream proxy: records every `Proxy-Authorization` header it sees
/// and answers 200 with a fixed body.
async fn spawn_upstream(seen: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..read]).to_string();
                for line in head.lines() {
                    if line
                        .to_ascii_lowercase()
                        .starts_with("proxy-authorization:")
                    {
                        seen.lock().unwrap().push(line.to_string());
                    }
                }
                let body = "upstream-ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

async fn send_raw(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).to_string()
}

/// Read exactly one HTTP head (through `\r\n\r\n`) from a live stream.
async fn read_one_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let Ok(read) = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await
        else {
            break;
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => head.push(byte[0]),
        }
    }
    String::from_utf8_lossy(&head).to_string()
}

/// A fake upstream that records the full request head it received.
async fn spawn_upstream_full(seen: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let head = read_one_head(&mut stream).await;
                seen.lock().unwrap().push(head);
                let body = "upstream-ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

fn policy(first_party: Vec<HostPattern>) -> DestinationPolicy {
    // The integration origins below are loopback mock servers, so the
    // explicit loopback address-class rule is part of the test policy.
    DestinationPolicy::first_party_only(first_party).with_allow_loopback(true)
}

#[tokio::test]
async fn first_party_only_blocks_unknown_and_tracking_hosts() {
    let origin = spawn_origin("origin-ok").await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_blocked_hosts(vec![HostPattern::parse("*.tracker.test").unwrap()])
            .with_allowed_ports(vec![80, 443, origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .expect("broker starts");
    let allowed = send_raw(
        broker.addr(),
        &format!(
            "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(allowed.starts_with("HTTP/1.1 200 OK"), "{allowed}");
    assert!(allowed.ends_with("origin-ok"), "{allowed}");
    let tracking = send_raw(
        broker.addr(),
        "GET http://ads.tracker.test/pixel HTTP/1.1\r\nHost: ads.tracker.test\r\n\r\n",
    )
    .await;
    assert!(tracking.starts_with("HTTP/1.1 403"), "{tracking}");
    assert!(tracking.contains("explicitly_blocked"), "{tracking}");
    let unknown = send_raw(
        broker.addr(),
        "GET http://unknown.test/x HTTP/1.1\r\nHost: unknown.test\r\n\r\n",
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 403"), "{unknown}");
    assert!(unknown.contains("not_first_party"), "{unknown}");
    let accounting = broker.accounting();
    assert_eq!(accounting.requests_total, 3);
    assert_eq!(accounting.blocked_total, 2);
    assert!(accounting.per_host.contains_key("127.0.0.1"));
    assert!(accounting.bytes_down > 0);
    let health = broker.health();
    assert_eq!(health.state, BrokerState::Running);
    assert!(health.uptime_ms >= 0);
    broker.shutdown().await;
}

#[tokio::test]
async fn upstream_credentials_stay_on_the_broker_leg() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream(seen.clone()).await;
    let credentials = ProxyCredentials::new("broker-user", "broker-password");
    assert!(!format!("{credentials:?}").contains("broker-password"));
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![9, 80, 443]),
        upstream: UpstreamSelector::new(Some(
            UpstreamProxy::new("127.0.0.1", upstream.port())
                .with_address_policy(EgressAddressPolicy::LOCAL)
                .with_credentials(credentials),
        )),
        ..BrokerConfig::default()
    })
    .await
    .expect("broker starts");
    // The client (Chromium's role) sends no credentials and even tries to
    // smuggle its own Proxy-Authorization: it must be stripped.
    let response = send_raw(
        broker.addr(),
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Proxy-Authorization: Basic ZXZpbA==\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("upstream-ok"), "{response}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "exactly one upstream auth header: {seen:?}");
    assert!(seen[0].contains("Basic "), "{seen:?}");
    assert!(
        !seen[0].contains("ZXZpbA"),
        "client credentials were forwarded"
    );
    assert!(!response.contains("broker-password"));
    assert!(
        !response.contains("Basic "),
        "credentials leaked to the client"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn oversized_request_heads_are_refused() {
    let broker = EgressBroker::start(BrokerConfig {
        max_request_bytes: 1024,
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let huge = "x".repeat(8 * 1024);
    let response = send_raw(
        broker.addr(),
        &format!("GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Big: {huge}\r\n\r\n"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 431"), "{response}");
    assert!(broker.accounting().errors_total >= 1);
    broker.shutdown().await;
}

#[tokio::test]
async fn connect_tunnels_are_policy_checked_and_accounted() {
    let origin = spawn_origin("tunnel-ok").await;
    let broker =
        EgressBroker::start(BrokerConfig {
            policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
                .with_allowed_ports(vec![80, 443, origin.port()]),
            ..BrokerConfig::default()
        })
        .await
        .unwrap();
    let denied = send_raw(
        broker.addr(),
        "CONNECT tracker.test:443 HTTP/1.1\r\nHost: tracker.test:443\r\n\r\n",
    )
    .await;
    assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
    assert_eq!(broker.accounting().blocked_total, 1);

    let mut stream = TcpStream::connect(broker.addr()).await.unwrap();
    stream
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                origin.port(),
                origin.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("tunnel head timeout")
            .unwrap();
        if read == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    stream
        .write_all(b"GET /tunneled HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut body = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut body)).await;
    let body = String::from_utf8_lossy(&body).to_string();
    assert!(body.contains("tunnel-ok"), "{body}");
    assert_eq!(broker.accounting().tunnels_total, 1);
    broker.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_accepting_connections() {
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let addr = broker.addr();
    broker.shutdown().await;
    assert_eq!(broker.health().state, BrokerState::Stopped);
    // The listener is closed: connecting either fails or the connection is
    // immediately closed without a response.
    if let Ok(Ok(mut stream)) =
        tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(addr)).await
    {
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Err(_)),
            "a stopped broker must not serve"
        );
    }
}

#[tokio::test]
async fn broker_refuses_a_non_loopback_bind() {
    let config = BrokerConfig {
        bind: "0.0.0.0:0".parse().unwrap(),
        ..BrokerConfig::default()
    };
    assert!(EgressBroker::start(config).await.is_err());
}

#[tokio::test]
async fn allowlisted_host_on_a_non_policy_port_is_refused() {
    let origin = spawn_origin("port-ok").await;
    let broker =
        EgressBroker::start(BrokerConfig {
            policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
                .with_allowed_ports(vec![80, 443, origin.port()]),
            ..BrokerConfig::default()
        })
        .await
        .unwrap();
    // The explicitly allowlisted origin port is served (forward + CONNECT).
    let allowed = send_raw(
        broker.addr(),
        &format!(
            "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(allowed.starts_with("HTTP/1.1 200 OK"), "{allowed}");
    // The same first-party host on a port outside the policy is refused,
    // for both request forms, with the typed reason.
    let forward = send_raw(
        broker.addr(),
        "GET http://127.0.0.1:8443/x HTTP/1.1\r\nHost: 127.0.0.1:8443\r\n\r\n",
    )
    .await;
    assert!(forward.starts_with("HTTP/1.1 403"), "{forward}");
    assert!(forward.contains("port_not_allowed"), "{forward}");
    let connect = send_raw(
        broker.addr(),
        "CONNECT 127.0.0.1:8443 HTTP/1.1\r\nHost: 127.0.0.1:8443\r\n\r\n",
    )
    .await;
    assert!(connect.starts_with("HTTP/1.1 403"), "{connect}");
    assert!(connect.contains("port_not_allowed"), "{connect}");
    broker.shutdown().await;
}

#[tokio::test]
async fn destination_canonicalization_closes_spelling_tricks() {
    let origin = spawn_origin("canon-ok").await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("localhost").unwrap()])
            .with_allow_loopback(true)
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    // `localhost.` and `LOCALHOST` are the same canonical destination.
    let trailing = send_raw(
        broker.addr(),
        &format!(
            "GET http://LOCALHOST.:{}/x HTTP/1.1\r\nHost: LOCALHOST.\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(trailing.starts_with("HTTP/1.1 200 OK"), "{trailing}");
    // A decimal IP shorthand is ambiguous: refused, never resolved.
    let numeric = send_raw(
        broker.addr(),
        "GET http://2130706433/x HTTP/1.1\r\nHost: 2130706433\r\n\r\n",
    )
    .await;
    assert!(numeric.starts_with("HTTP/1.1 400"), "{numeric}");
    // A userinfo authority is rejected outright (never stripped into a
    // valid destination).
    let userinfo = send_raw(
        broker.addr(),
        &format!(
            "GET http://evil.test@localhost:{}/x HTTP/1.1\r\nHost: localhost\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(userinfo.starts_with("HTTP/1.1 400"), "{userinfo}");
    broker.shutdown().await;
}

#[tokio::test]
async fn smuggled_header_lines_and_connection_tokens_never_reach_the_upstream() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_full(seen.clone()).await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![9, 80, 443]),
        upstream: UpstreamSelector::new(Some(
            UpstreamProxy::new("127.0.0.1", upstream.port())
                .with_address_policy(EgressAddressPolicy::LOCAL),
        )),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    // Connection-listed tokens, the fixed hop-by-hop set and client-supplied
    // proxy credentials are all stripped.
    // `Transfer-Encoding: chunked` with its (empty) chunked body: the framing
    // metadata is consumed and canonicalized to one Content-Length, so the
    // upstream leg sees neither the header nor the chunk framing.
    let response = send_raw(
        broker.addr(),
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Connection: keep-alive, X-Custom-Hop\r\nX-Custom-Hop: value\r\n\
         Proxy-Authorization: Basic ZXZpbA==\r\nTransfer-Encoding: chunked\r\n\r\n\
         0\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    {
        let heads = seen.lock().unwrap().clone();
        assert_eq!(heads.len(), 1, "{heads:?}");
        let head = heads[0].to_ascii_lowercase();
        for stripped in [
            "x-custom-hop",
            "proxy-authorization",
            "transfer-encoding",
            "connection: keep-alive",
        ] {
            assert!(
                !head.contains(stripped),
                "{stripped} reached upstream: {head}"
            );
        }
    }
    // A bare LF inside a value can never become a new upstream header line:
    // the request is refused typed before any upstream leg.
    let smuggled = send_raw(
        broker.addr(),
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         X-Ignored: a\nProxy-Authorization: Basic ZXZpbA==\r\n\r\n",
    )
    .await;
    assert!(smuggled.starts_with("HTTP/1.1 400"), "{smuggled}");
    // An empty header name is malformed too.
    let empty = send_raw(
        broker.addr(),
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n: v\r\n\r\n",
    )
    .await;
    assert!(empty.starts_with("HTTP/1.1 400"), "{empty}");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "smuggled requests never reached upstream"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn stalled_tunnel_releases_the_connection_permit_within_bound() {
    // A black-hole origin: accepts the connection and never writes.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let black_hole = listener.local_addr().unwrap();
    let held: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
    let held_clone = held.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            held_clone.lock().unwrap().push(stream);
        }
    });
    let origin = spawn_origin("after-stall").await;
    let broker =
        EgressBroker::start(BrokerConfig {
            max_connections: 1,
            copy_idle_timeout_ms: 200,
            copy_max_ms: 5_000,
            policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
                .with_allowed_ports(vec![80, 443, black_hole.port(), origin.port()]),
            ..BrokerConfig::default()
        })
        .await
        .unwrap();
    // Tunnel one stalls forever (neither side sends).
    let mut stalled = TcpStream::connect(broker.addr()).await.unwrap();
    stalled
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                black_hole.port(),
                black_hole.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_one_head(&mut stalled).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // The only permit must come back within the idle bound: a second tunnel
    // is served while the first is still stalled.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut served = false;
    while std::time::Instant::now() < deadline {
        let Ok(Ok(mut second)) = tokio::time::timeout(
            Duration::from_millis(500),
            TcpStream::connect(broker.addr()),
        )
        .await
        else {
            continue;
        };
        let connect = format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        );
        if second.write_all(connect.as_bytes()).await.is_err() {
            continue;
        }
        let head = read_one_head(&mut second).await;
        if !head.starts_with("HTTP/1.1 200") {
            continue;
        }
        second
            .write_all(b"GET /tunneled HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .ok();
        let mut body = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), second.read_to_end(&mut body)).await;
        if String::from_utf8_lossy(&body).contains("after-stall") {
            served = true;
            break;
        }
    }
    assert!(
        served,
        "a stalled peer must release its max_connections permit within the idle bound"
    );
    let health = broker.health();
    assert!(
        health.accounting.errors_total >= 1,
        "the bounded close is observable: {health:?}"
    );
    assert!(
        health
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("idle timeout"),
        "typed close reason: {:?}",
        health.last_error
    );
    broker.shutdown().await;
}

// ---------------------------------------------------------------------------
// Helpers for the shutdown-ownership, framing and CONNECT-handshake tests.
// ---------------------------------------------------------------------------

/// A request as the fake upstream received it (head text plus, if the head
/// declared a Content-Length, exactly that many body bytes).
#[derive(Clone, Debug)]
struct RecordedRequest {
    head: String,
    body: Vec<u8>,
}

fn content_length_of(head: &str) -> Option<usize> {
    for line in head.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok();
            }
        }
    }
    None
}

fn count_occurrences(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

/// Fake upstream proxy that reads one request (head + declared body) and
/// records it.
async fn spawn_recording_upstream(seen: Arc<Mutex<Vec<RecordedRequest>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let head = read_one_head(&mut stream).await;
                let mut body = Vec::new();
                if let Some(length) = content_length_of(&head) {
                    body.resize(length, 0);
                    if stream.read_exact(&mut body).await.is_err() {
                        return;
                    }
                }
                seen.lock().unwrap().push(RecordedRequest { head, body });
                let response =
                    "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nupstream-ok";
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

/// Fake origin whose accepted tunnel sockets are read to EOF; `closed`
/// becomes true once the broker's socket to the origin is gone.
async fn spawn_closing_origin(closed: Arc<AtomicBool>, seen: Arc<Mutex<Vec<u8>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let closed = closed.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => seen.lock().unwrap().extend_from_slice(&buf[..n]),
                    }
                }
                closed.store(true, Ordering::SeqCst);
            });
        }
    });
    addr
}

/// Fake origin that streams one byte every 10ms until the broker's socket
/// closes (which sets `closed`), keeping a copy genuinely mid-transfer.
async fn spawn_streaming_origin(closed: Arc<AtomicBool>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let closed = closed.clone();
            tokio::spawn(async move {
                let (mut read_half, mut write_half) = stream.into_split();
                let reader = tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match read_half.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    closed.store(true, Ordering::SeqCst);
                });
                let mut counter = 0u8;
                loop {
                    if write_half
                        .write_all(&[b'a'.wrapping_add(counter % 26)])
                        .await
                        .is_err()
                    {
                        break;
                    }
                    counter = counter.wrapping_add(1);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let _ = reader.await;
            });
        }
    });
    addr
}

/// Fake upstream proxy that answers each accepted CONNECT with the next
/// scripted response, then echoes whatever the broker tunnels.
async fn spawn_scripted_upstream(responses: Vec<&'static str>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let responses = Arc::new(responses);
    let counter = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let Some(response) = responses.get(index).copied() else {
                continue;
            };
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => head.push(byte[0]),
                    }
                }
                let _ = stream.write_all(response.as_bytes()).await;
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

fn upstream_config(upstream: SocketAddr) -> BrokerConfig {
    BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![9, 80, 443]),
        upstream: UpstreamSelector::new(Some(
            UpstreamProxy::new("127.0.0.1", upstream.port())
                // The mock upstream is local: its OWN leg needs the explicit
                // local rule (never inherited from the destination policy).
                .with_address_policy(EgressAddressPolicy::LOCAL),
        )),
        ..BrokerConfig::default()
    }
}

async fn send_raw_bytes(addr: SocketAddr, request: &[u8], half_close: bool) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    if half_close {
        stream.shutdown().await.unwrap();
    }
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).to_string()
}

async fn wait_for_flag(flag: &AtomicBool, bound: Duration) -> bool {
    let deadline = std::time::Instant::now() + bound;
    while std::time::Instant::now() < deadline {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    flag.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// P1: broker-owned connection tasks and honest terminal state.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_owns_the_established_tunnel_and_reports_terminal_after_drain() {
    let closed = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin = spawn_closing_origin(closed.clone(), seen).await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let mut client = TcpStream::connect(broker.addr()).await.unwrap();
    client
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                origin.port(),
                origin.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_one_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(broker.health().state, BrokerState::Running);
    assert_eq!(broker.health().active_connections, 1);

    // The first poll issues the cancel and moves Running -> Draining without
    // yielding to the accept task (current-thread runtime), making the
    // transition observable deterministically.
    let waker = futures_util::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    let mut shutdown = Box::pin(broker.shutdown());
    assert!(Future::poll(shutdown.as_mut(), &mut context).is_pending());
    assert_eq!(broker.health().state, BrokerState::Draining);

    let started = std::time::Instant::now();
    let joined = tokio::time::timeout(Duration::from_secs(2), shutdown).await;
    assert!(joined.is_ok(), "shutdown must return within the bound");
    assert!(started.elapsed() < Duration::from_secs(2));
    let health = broker.health();
    assert_eq!(health.state, BrokerState::Stopped);
    assert_eq!(health.active_connections, 0);

    // Zero broker-owned sockets: the fake origin saw its socket close and
    // the tunnel client observes EOF.
    assert!(
        wait_for_flag(&closed, Duration::from_secs(2)).await,
        "the origin must observe the broker socket closing"
    );
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "the tunnel client must observe close: {read:?}"
    );
}

#[tokio::test]
async fn shutdown_interrupts_a_copy_mid_transfer_within_bound() {
    let closed = Arc::new(AtomicBool::new(false));
    let origin = spawn_streaming_origin(closed.clone()).await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let mut client = TcpStream::connect(broker.addr()).await.unwrap();
    client
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                origin.port(),
                origin.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_one_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // Drain the tunnel so the copy is genuinely transferring when shutdown
    // fires.
    let eof = Arc::new(AtomicBool::new(false));
    let eof_flag = eof.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        eof_flag.store(true, Ordering::SeqCst);
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    let joined = tokio::time::timeout(Duration::from_secs(2), broker.shutdown()).await;
    assert!(joined.is_ok(), "shutdown must return within the bound");
    assert!(started.elapsed() < Duration::from_secs(2));
    let health = broker.health();
    assert_eq!(health.state, BrokerState::Stopped);
    assert_eq!(health.active_connections, 0);
    assert!(
        wait_for_flag(&closed, Duration::from_secs(2)).await,
        "the origin must observe the broker socket closing"
    );
    assert!(
        wait_for_flag(&eof, Duration::from_secs(2)).await,
        "the tunnel client must observe close"
    );
}

#[tokio::test]
async fn dropping_the_handle_aborts_connections_within_bound() {
    let closed = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin = spawn_closing_origin(closed.clone(), seen).await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let mut client = TcpStream::connect(broker.addr()).await.unwrap();
    client
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                origin.port(),
                origin.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_one_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(broker.health().active_connections, 1);

    drop(broker);
    assert!(
        wait_for_flag(&closed, Duration::from_secs(2)).await,
        "Drop must abort the tunnel and close the broker-owned socket"
    );
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "{read:?}");
}

#[tokio::test]
async fn repeated_and_concurrent_shutdowns_are_idempotent() {
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    tokio::join!(broker.shutdown(), broker.shutdown());
    broker.shutdown().await;
    broker.shutdown().await;
    let health = broker.health();
    assert_eq!(health.state, BrokerState::Stopped);
    assert_eq!(health.active_connections, 0);
}

// ---------------------------------------------------------------------------
// P1: HTTP/1 request-body framing on the forward path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chunked_request_bodies_are_decoded_to_one_canonical_content_length() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_recording_upstream(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();
    let response = send_raw(
        broker.addr(),
        "POST http://127.0.0.1:9/upload HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let head = seen[0].head.to_ascii_lowercase();
    assert!(!head.contains("transfer-encoding"), "{head}");
    assert_eq!(count_occurrences(&head, "content-length:"), 1, "{head}");
    assert!(head.contains("content-length: 11"), "{head}");
    assert_eq!(seen[0].body, b"hello world");
    broker.shutdown().await;
}

#[tokio::test]
async fn ambiguous_or_conflicting_framing_is_refused_before_upstream() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_recording_upstream(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();
    let cases = [
        (
            "cl+te",
            "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "400",
        ),
        (
            "unequal duplicate cl",
            "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Length: 4\r\nContent-Length: 5\r\n\r\nBODY",
            "400",
        ),
        (
            "non-decimal cl",
            "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Length: 4x\r\n\r\nBODY",
            "400",
        ),
        (
            "unsupported coding",
            "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Transfer-Encoding: gzip, chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            "501",
        ),
        (
            "duplicate chunked coding",
            "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "501",
        ),
        (
            "short cl leaves residue",
            "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Length: 3\r\n\r\nabcdef",
            "400",
        ),
        (
            "pipelined second request",
            "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Length: 4\r\n\r\nBODYGET http://127.0.0.1:9/second HTTP/1.1\r\n\
             Host: 127.0.0.1\r\n\r\n",
            "400",
        ),
    ];
    for (name, request, expected) in cases {
        let response = send_raw(broker.addr(), request).await;
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected}")),
            "{name}: {response}"
        );
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "no ambiguous/conflicting request may reach the upstream"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn equal_duplicate_content_length_is_canonicalized_to_one() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_recording_upstream(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();
    let response = send_raw(
        broker.addr(),
        "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Length: 4\r\nContent-Length: 4\r\n\r\nBODY",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let head = seen[0].head.to_ascii_lowercase();
    assert_eq!(count_occurrences(&head, "content-length:"), 1, "{head}");
    assert!(head.contains("content-length: 4"), "{head}");
    assert_eq!(seen[0].body, b"BODY");
    broker.shutdown().await;
}

#[tokio::test]
async fn overlong_content_length_is_typed_without_hanging() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_recording_upstream(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();
    // Declared longer than supplied, then half-close: typed 400, no hang.
    let response = send_raw_bytes(
        broker.addr(),
        b"POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
          Content-Length: 100\r\n\r\nabc",
        true,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");

    // Declared beyond the configured body bound: typed 413 before any read.
    let tight = EgressBroker::start(BrokerConfig {
        max_request_body_bytes: 16,
        ..upstream_config(upstream)
    })
    .await
    .unwrap();
    let response = send_raw(
        tight.addr(),
        "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Length: 64\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    assert!(seen.lock().unwrap().is_empty());
    tight.shutdown().await;
    broker.shutdown().await;
}

#[tokio::test]
async fn chunk_extensions_and_trailers_are_consumed_and_malformed_framing_refused() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_recording_upstream(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();
    let response = send_raw(
        broker.addr(),
        "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Transfer-Encoding: chunked\r\n\r\n5;a=b\r\nhello\r\n0\r\nX-Trailer: v\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].body, b"hello");
        let head = seen[0].head.to_ascii_lowercase();
        assert!(head.contains("content-length: 5"), "{head}");
        assert!(!head.contains("x-trailer"), "{head}");
    }
    // Invalid chunk size.
    let response = send_raw(
        broker.addr(),
        "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Transfer-Encoding: chunked\r\n\r\nzz\r\nhello\r\n0\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    // A bare LF is never a chunk-size terminator.
    let response = send_raw(
        broker.addr(),
        "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Transfer-Encoding: chunked\r\n\r\n5\nhello\r\n0\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    // Chunk data not followed by CRLF.
    let response = send_raw(
        broker.addr(),
        "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Transfer-Encoding: chunked\r\n\r\n5\r\nhelloxx0\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "only the well-formed request may reach the upstream"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn oversized_chunked_body_is_typed_at_the_bound() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_recording_upstream(seen.clone()).await;
    let broker = EgressBroker::start(BrokerConfig {
        max_request_body_bytes: 8,
        ..upstream_config(upstream)
    })
    .await
    .unwrap();
    let response = send_raw(
        broker.addr(),
        "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Transfer-Encoding: chunked\r\n\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    assert!(seen.lock().unwrap().is_empty());
    broker.shutdown().await;
}

#[tokio::test]
async fn expect_100_continue_is_answered_locally_and_not_forwarded() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_recording_upstream(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();
    let response = send_raw(
        broker.addr(),
        "POST http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Expect: 100-continue\r\nContent-Length: 4\r\n\r\nBODY",
    )
    .await;
    assert!(response.contains("HTTP/1.1 100 Continue"), "{response}");
    assert!(response.contains("HTTP/1.1 200 OK"), "{response}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        !seen[0].head.to_ascii_lowercase().contains("expect"),
        "{}",
        seen[0].head
    );
    assert_eq!(seen[0].body, b"BODY");
    broker.shutdown().await;
}

// ---------------------------------------------------------------------------
// P2: exact head bound and strict upstream CONNECT status parsing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_head_bound_is_exact_and_terminator_overage_is_typed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = listener.local_addr().unwrap().port();
    drop(listener);
    let bound = 128usize;
    let broker = EgressBroker::start(BrokerConfig {
        max_request_bytes: bound,
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![closed_port]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let prefix =
        format!("GET http://127.0.0.1:{closed_port}/x HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pad: ");
    let suffix = "\r\n\r\n";
    let exact_pad = bound - prefix.len() - suffix.len();
    assert!(exact_pad > 2);
    // Exactly at the bound: accepted (the destination connect may fail, but
    // the head itself is never typed 431).
    let exact = format!("{prefix}{}{suffix}", "a".repeat(exact_pad));
    assert_eq!(exact.len(), bound);
    let accepted = send_raw(broker.addr(), &exact).await;
    assert!(!accepted.starts_with("HTTP/1.1 431"), "{accepted}");
    // One byte over the bound.
    let over = format!("{prefix}{}{suffix}", "a".repeat(exact_pad + 1));
    assert_eq!(over.len(), bound + 1);
    let refused = send_raw(broker.addr(), &over).await;
    assert!(refused.starts_with("HTTP/1.1 431"), "{refused}");
    // Head content exactly at the bound, terminator landing in the overage:
    // still typed 431, never accepted.
    let crossing = format!("{prefix}{}{suffix}", "a".repeat(bound - prefix.len() - 2));
    assert_eq!(crossing.len(), bound + 2);
    let refused = send_raw(broker.addr(), &crossing).await;
    assert!(refused.starts_with("HTTP/1.1 431"), "{refused}");
    broker.shutdown().await;
}

#[tokio::test]
async fn upstream_connect_status_lines_are_parsed_strictly() {
    // Every one of these contains the substring " 200" but is not a strict
    // `HTTP/1.x SP 200` status line (the last is a real non-200).
    let responses = vec![
        "HTTP/1.1 2000 OK\r\n\r\n",
        "XD 200 OK\r\n\r\n",
        "HTTP/9 200 OK\r\n\r\n",
        "HTTP/1.1 20 OK\r\n\r\n",
        "HTTP/1.1 200OK\r\n\r\n",
        "HTTP/1.1  200 OK\r\n\r\n",
        "HTTP/1.1 407 Proxy Authentication Required\r\n\r\n",
    ];
    let upstream = spawn_scripted_upstream(responses).await;
    let broker = EgressBroker::start(BrokerConfig {
        upstream: UpstreamSelector::new(Some(
            UpstreamProxy::new("127.0.0.1", upstream.port())
                .with_address_policy(EgressAddressPolicy::LOCAL),
        )),
        ..upstream_config(upstream)
    })
    .await
    .unwrap();
    for _ in 0..7 {
        let response = send_raw(
            broker.addr(),
            "CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    }
    assert_eq!(
        broker.accounting().tunnels_total,
        0,
        "no tunnel may be authorized on a malformed or non-200 status"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn upstream_connect_accepts_exact_200_and_relays_early_tunnel_bytes() {
    let responses = vec![
        "HTTP/1.0 200 Connection Established\r\n\r\n",
        "HTTP/1.1 200 OK\r\n\r\nEARLY-DATA",
    ];
    let upstream = spawn_scripted_upstream(responses).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();
    // HTTP/1.0 exact 200: tunnel established, echo works.
    let mut first = TcpStream::connect(broker.addr()).await.unwrap();
    first
        .write_all(b"CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")
        .await
        .unwrap();
    let head = read_one_head(&mut first).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    first.write_all(b"ping").await.unwrap();
    let mut echoed = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(2), first.read_exact(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&echoed, b"ping");
    // HTTP/1.1 exact 200 with post-header bytes: early tunnel bytes are
    // delivered to the client intact, then the tunnel still carries data.
    let mut second = TcpStream::connect(broker.addr()).await.unwrap();
    second
        .write_all(b"CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")
        .await
        .unwrap();
    let head = read_one_head(&mut second).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let mut early = [0u8; 10];
    tokio::time::timeout(Duration::from_secs(2), second.read_exact(&mut early))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&early, b"EARLY-DATA");
    second.write_all(b"ping").await.unwrap();
    let mut echoed = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(2), second.read_exact(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&echoed, b"ping");
    assert_eq!(broker.accounting().tunnels_total, 2);
    broker.shutdown().await;
}

// ---------------------------------------------------------------------------
// Adversarial: CONNECT kind policy, Host ownership, userinfo rejection.
// ---------------------------------------------------------------------------

/// Fake origin that records the request head it received. `bind` may be
/// `127.0.0.1` or `[::1]`.
async fn spawn_recording_origin(bind: &str, seen: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = TcpListener::bind((bind, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let head = read_one_head(&mut stream).await;
                seen.lock().unwrap().push(head);
                let body = "origin-ok";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn connect_tunnels_require_an_https_capable_policy() {
    let origin = spawn_origin("http-only").await;
    // HTTP-only policy: forward HTTP to the allowlisted host works, a
    // CONNECT tunnel to the same host/port is a typed refusal because the
    // request kind is part of the policy decision.
    let broker = EgressBroker::start(BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allow_loopback(true)
            .with_allow_schemes(vec!["http".to_string()])
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let forward = send_raw(
        broker.addr(),
        &format!(
            "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(forward.starts_with("HTTP/1.1 200"), "{forward}");
    let connect = send_raw(
        broker.addr(),
        &format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            origin.port(),
            origin.port()
        ),
    )
    .await;
    assert!(connect.starts_with("HTTP/1.1 403"), "{connect}");
    assert!(connect.contains("connect_not_allowed"), "{connect}");
    assert_eq!(
        broker.accounting().tunnels_total,
        0,
        "no tunnel may be established under an HTTP-only policy"
    );
    let health = broker.health();
    assert!(health.accounting.blocked_total >= 1);
    broker.shutdown().await;
}

#[tokio::test]
async fn literal_and_resolved_loopback_need_the_explicit_address_rule() {
    let origin = spawn_origin("literal-ok").await;
    // A literal 127.0.0.1 destination is NOT a bypass: without the explicit
    // address rule it is refused by the SAME class vetting a resolved name
    // gets, before any socket connects.
    let broker = EgressBroker::start(BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let refused = send_raw(
        broker.addr(),
        &format!(
            "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(refused.starts_with("HTTP/1.1 502"), "{refused}");
    assert!(
        refused.contains("loopback"),
        "the refusal names the address class: {refused}"
    );
    broker.shutdown().await;

    // The explicit loopback address rule admits the literal...
    let broker = EgressBroker::start(BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allow_loopback(true)
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let response = send_raw(
        broker.addr(),
        &format!(
            "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    broker.shutdown().await;

    // ...and the policy names the NAME `localhost`, which resolves onto
    // loopback: without the explicit address rule that connect is refused
    // after the one resolution, before any socket connects. The rule is per
    // address class, never per spelling.
    let broker = EgressBroker::start(BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("localhost").unwrap()])
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let response = send_raw(
        broker.addr(),
        &format!(
            "GET http://localhost:{}/x HTTP/1.1\r\nHost: localhost\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(
        response.contains("loopback"),
        "the refusal names the address class: {response}"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn userinfo_targets_are_rejected_for_forward_and_connect() {
    let origin = spawn_origin("no-userinfo").await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allow_loopback(true)
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    for request in [
        format!(
            "GET http://user:pass@127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
        // Userinfo naming the allowed host must not be sanitized into it.
        format!(
            "GET http://evil.test@127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
        format!(
            "CONNECT user:pass@127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
        format!(
            "CONNECT attacker.test@127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
    ] {
        let response = send_raw(broker.addr(), &request).await;
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "{request} => {response}"
        );
    }
    assert_eq!(broker.accounting().tunnels_total, 0);
    broker.shutdown().await;
}

#[tokio::test]
async fn broker_strips_and_synthesizes_exactly_one_canonical_host_header() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin = spawn_recording_origin("127.0.0.1", seen.clone()).await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allow_loopback(true)
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    // Conflicting and duplicate Host headers: none may survive; exactly one
    // canonical Host is synthesized from the policy-checked destination.
    let response = send_raw(
        broker.addr(),
        &format!(
            "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: evil.test:80\r\nHost: 127.0.0.1\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let heads = seen.lock().unwrap().clone();
    assert_eq!(heads.len(), 1, "{heads:?}");
    let head = &heads[0];
    let lower = head.to_ascii_lowercase();
    assert_eq!(count_occurrences(&lower, "host:"), 1, "{head}");
    assert!(!lower.contains("evil.test"), "{head}");
    assert!(
        head.contains(&format!("Host: 127.0.0.1:{}", origin.port())),
        "{head}"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn ipv6_authorities_are_bracketed_in_the_synthesized_host() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin = spawn_recording_origin("::1", seen.clone()).await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: DestinationPolicy::first_party_only(vec![HostPattern::parse("[::1]").unwrap()])
            .with_allow_loopback(true)
            .with_allowed_ports(vec![origin.port()]),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    let response = send_raw(
        broker.addr(),
        &format!(
            "GET http://[::1]:{}/x HTTP/1.1\r\nHost: [::1]\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let heads = seen.lock().unwrap().clone();
    assert_eq!(heads.len(), 1, "{heads:?}");
    let head = &heads[0];
    assert_eq!(
        count_occurrences(&head.to_ascii_lowercase(), "host:"),
        1,
        "{head}"
    );
    assert!(
        head.contains(&format!("Host: [::1]:{}", origin.port())),
        "{head}"
    );
    broker.shutdown().await;
}

#[tokio::test]
async fn upstream_proxy_mode_also_owns_the_host_header() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_recording_upstream(seen.clone()).await;
    let broker = EgressBroker::start(upstream_config(upstream))
        .await
        .unwrap();
    // Absolute-form request line through an upstream proxy: the client's
    // duplicate/conflicting Host headers are still stripped and replaced by
    // the canonical authority of the policy-checked target.
    let response = send_raw(
        broker.addr(),
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: attacker.example\r\nHost: 127.0.0.1:9\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let head = &seen[0].head;
    let lower = head.to_ascii_lowercase();
    assert_eq!(count_occurrences(&lower, "host:"), 1, "{head}");
    assert!(!lower.contains("attacker.example"), "{head}");
    assert!(head.contains("Host: 127.0.0.1:9"), "{head}");
    // The absolute-form request line still names the checked destination.
    assert!(
        head.starts_with("GET http://127.0.0.1:9/"),
        "absolute-form target preserved: {head}"
    );
    broker.shutdown().await;
}
