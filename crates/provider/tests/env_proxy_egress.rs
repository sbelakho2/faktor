//! Adversarial egress test: a configured environment proxy must never carry
//! checked outbound traffic.
//!
//! The checked production client is built while `HTTP_PROXY`/`HTTPS_PROXY`
//! (and `ALL_PROXY`) point at a fake loopback proxy. The fake proxy records
//! every accepted connection; the checked request must still reach the
//! vetted loopback destination directly, with the fake proxy recording ZERO
//! connections. A default `reqwest::Client` is used as the control: its
//! automatic system-proxy inheritance is expected to dial the fake proxy, so
//! a zero count for the checked client cannot be explained by the env proxy
//! being ignored in this process.
//!
//! This lives in `tests/` because `std::env::set_var` is process-global: an
//! integration-test binary gets its own process, so the mutated proxy
//! environment can never race the crate's parallel unit tests.

use faktor_provider::egress::{execute_get, PolicyCheckedHttpTransport, ResponseBudget};
use faktor_security::destination::DestinationPolicy;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Process-global proxy environment variables owned by this test.
const PROXY_ENV_VARS: [&str; 9] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "REQUEST_METHOD",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

/// Sets the given env vars and removes the rest of [`PROXY_ENV_VARS`],
/// restoring every original value on drop (even on panic). The test is
/// single-threaded by construction (one `#[tokio::test]` in this binary).
struct ProxyEnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl ProxyEnvGuard {
    /// Install `sets`, and clear every other proxy-related variable so the
    /// fake proxy is the only routing candidate the environment offers.
    fn install(sets: &[(&'static str, &str)]) -> Self {
        let mut saved = Vec::new();
        for name in PROXY_ENV_VARS {
            saved.push((name, std::env::var_os(name)));
            std::env::remove_var(name);
        }
        for (name, value) in sets {
            std::env::set_var(name, value);
        }
        Self { saved }
    }
}

impl Drop for ProxyEnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A fake HTTP proxy: counts accepted connections and answers promptly with
/// a 502 so a default client cannot hang. It never forwards anywhere.
struct FakeProxy {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
}

impl FakeProxy {
    async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("fake proxy bind");
        let addr = listener.local_addr().expect("fake proxy addr");
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ =
                        tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { addr, connections }
    }

    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn reset_connection_count(&self) {
        self.connections.store(0, Ordering::SeqCst);
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// A minimal loopback HTTP destination that answers every request `hello`
/// and counts the requests it served.
async fn start_destination() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("destination bind");
    let addr = listener.local_addr().expect("destination addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut seen = Vec::new();
                loop {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                    )
                    .await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (addr, hits)
}

#[tokio::test]
async fn env_proxy_is_never_used_for_checked_egress() {
    let proxy = FakeProxy::start().await;
    let (dest_addr, dest_hits) = start_destination().await;
    let proxy_url = proxy.url();

    // The checked transport is built while the environment advertises a
    // proxy for every scheme; NO_PROXY is cleared so nothing exempts the
    // destination from the env proxy.
    let _env = ProxyEnvGuard::install(&[
        ("HTTP_PROXY", &proxy_url),
        ("HTTPS_PROXY", &proxy_url),
        ("ALL_PROXY", &proxy_url),
    ]);

    // Control: a default reqwest client inherits the system proxy and dials
    // the fake proxy (which answers 502). This proves the env proxy IS
    // honored in this process, so the checked client's zero count below is
    // real evidence, not a silently ignored variable.
    let control_url = format!("http://{dest_addr}/control");
    let control = reqwest::Client::builder()
        .build()
        .expect("control client build");
    let control_response =
        tokio::time::timeout(Duration::from_secs(10), control.get(&control_url).send())
            .await
            .expect("control request must not hang")
            .expect("the fake proxy answers 502");
    assert_eq!(
        control_response.status(),
        502,
        "the control request must be answered BY the fake env proxy"
    );
    assert_eq!(
        proxy.connection_count(),
        1,
        "the env proxy must be honored by a default client"
    );
    assert_eq!(
        dest_hits.load(Ordering::SeqCst),
        0,
        "the proxied control request must not reach the destination directly"
    );
    // The control proved the environment proxy is honored; reset the
    // counter so the checked phase below must record ZERO connections.
    proxy.reset_connection_count();

    // Checked egress: the production checked transport (literal loopback is
    // explicitly allowlisted) must dial the vetted destination directly.
    let policy = DestinationPolicy::parse_lines(["127.0.0.1"]).expect("loopback allowlist");
    let transport =
        PolicyCheckedHttpTransport::try_with_policy(policy).expect("checked transport build");
    let url = format!("http://{dest_addr}/vetted");
    let response = execute_get(&transport, &url)
        .await
        .expect("checked request must reach the vetted destination");
    assert_eq!(response.status(), 200, "destination status");
    let bytes = response
        .read_bytes(&ResponseBudget::for_timeout(Duration::from_secs(5), 1024))
        .await
        .expect("budgeted body read");
    assert_eq!(&bytes, b"hello", "body read from the vetted destination");

    assert_eq!(
        proxy.connection_count(),
        0,
        "checked egress must attempt ZERO connections through the env proxy"
    );
    assert_eq!(
        dest_hits.load(Ordering::SeqCst),
        1,
        "the checked request went straight to the vetted destination"
    );
}
