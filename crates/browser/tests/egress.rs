//! Egress broker tests over real loopback sockets: destination filtering,
//! accounting, upstream proxy selection and credential isolation, health and
//! shutdown. No external network is used.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use faktor_browser::{
    BrokerConfig, BrokerState, DestinationPolicy, EgressBroker, HostPattern, ProxyCredentials,
    UpstreamProxy, UpstreamSelector,
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
    DestinationPolicy::first_party_only(first_party)
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
            UpstreamProxy::new("127.0.0.1", upstream.port()).with_credentials(credentials),
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
    // A userinfo host is never the destination.
    let userinfo = send_raw(
        broker.addr(),
        &format!(
            "GET http://evil.test@localhost:{}/x HTTP/1.1\r\nHost: localhost\r\n\r\n",
            origin.port()
        ),
    )
    .await;
    assert!(userinfo.starts_with("HTTP/1.1 200 OK"), "{userinfo}");
    broker.shutdown().await;
}

#[tokio::test]
async fn smuggled_header_lines_and_connection_tokens_never_reach_the_upstream() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn_upstream_full(seen.clone()).await;
    let broker = EgressBroker::start(BrokerConfig {
        policy: policy(vec![HostPattern::parse("127.0.0.1").unwrap()])
            .with_allowed_ports(vec![9, 80, 443]),
        upstream: UpstreamSelector::new(Some(UpstreamProxy::new("127.0.0.1", upstream.port()))),
        ..BrokerConfig::default()
    })
    .await
    .unwrap();
    // Connection-listed tokens, the fixed hop-by-hop set and client-supplied
    // proxy credentials are all stripped.
    let response = send_raw(
        broker.addr(),
        "GET http://127.0.0.1:9/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Connection: keep-alive, X-Custom-Hop\r\nX-Custom-Hop: value\r\n\
         Proxy-Authorization: Basic ZXZpbA==\r\nTransfer-Encoding: chunked\r\n\r\n",
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
