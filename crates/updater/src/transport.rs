//! Artifact download through the CHECKED transport.
//!
//! [`ArtifactFetcher`] is the seam: the daemon wires
//! [`CheckedHttpFetcher`], which streams a GET through
//! `faktor_provider::egress::CheckedHttpClient` — the same policy-checked
//! choke point every provider call uses (destination policy checked on the
//! final request URL before any connect, no resolve-then-connect split).
//! Tests inject fakes; nothing in this crate opens a socket on its own.
//!
//! Downloads are bounded by the caller's byte limit: the declared size is
//! checked up front and the stream aborts the moment the limit is crossed
//! (never an unbounded read into memory or disk).

use faktor_provider::egress::CheckedHttpClient;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::error::UpdateError;

/// Stream one artifact URL into `sink`, refusing bodies larger than
/// `max_bytes`. Returns the number of bytes streamed. Implementations MUST
/// NOT buffer the whole artifact in memory.
#[async_trait::async_trait]
pub trait ArtifactFetcher: Send + Sync {
    async fn fetch_to(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
    ) -> Result<u64, UpdateError>;
}

/// The real transport: every download goes through the daemon's checked
/// HTTP client.
#[derive(Debug, Clone)]
pub struct CheckedHttpFetcher {
    client: CheckedHttpClient,
}

impl CheckedHttpFetcher {
    /// Wrap an already-built checked client (the daemon graph builds ONE such
    /// client for provider egress; the updater reuses it, so a download can
    /// never bypass the destination policy's choke point).
    pub fn new(client: CheckedHttpClient) -> Self {
        CheckedHttpFetcher { client }
    }
}

#[async_trait::async_trait]
impl ArtifactFetcher for CheckedHttpFetcher {
    async fn fetch_to(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
    ) -> Result<u64, UpdateError> {
        self.fetch_to_with(
            url,
            max_bytes,
            sink,
            std::time::Duration::from_millis(ARTIFACT_HEAD_TIMEOUT_MS),
            std::time::Duration::from_millis(ARTIFACT_IDLE_TIMEOUT_MS),
        )
        .await
    }
}

/// Documented wall-clock bound for the response HEAD of one artifact
/// download. The shared checked client only bounds connect, so a host that
/// accepts and then never answers would otherwise block the update forever.
pub const ARTIFACT_HEAD_TIMEOUT_MS: u64 = 30_000;

/// Documented idle bound between artifact body chunks. The body is
/// byte-bounded by the caller's `max_bytes`, so an idle-bounded drip cannot
/// run forever: every chunk must arrive inside this window and the total
/// byte count is capped.
pub const ARTIFACT_IDLE_TIMEOUT_MS: u64 = 60_000;

impl CheckedHttpFetcher {
    /// Bounded form of [`ArtifactFetcher::fetch_to`]; production passes the
    /// documented constants, tests pass short bounds against stalled hosts.
    async fn fetch_to_with(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
        head_bound: std::time::Duration,
        idle_bound: std::time::Duration,
    ) -> Result<u64, UpdateError> {
        let transport_err = |detail: String| UpdateError::Transport {
            artifact: url.to_string(),
            detail,
        };
        let builder = self
            .client
            .get(url)
            .map_err(|e| transport_err(format!("checked URL refused: {e}")))?;
        let mut response =
            match tokio::time::timeout(head_bound, self.client.send_checked(builder)).await {
                Ok(result) => {
                    result.map_err(|e| transport_err(format!("request refused/failed: {e}")))?
                }
                Err(_) => {
                    return Err(transport_err(format!(
                        "no response headers within the {} ms bound",
                        head_bound.as_millis()
                    )))
                }
            };
        let status = response.status();
        if !status.is_success() {
            return Err(transport_err(format!("HTTP status {status}")));
        }
        let mut streamed: u64 = 0;
        loop {
            let chunk = match tokio::time::timeout(idle_bound, response.chunk()).await {
                Ok(result) => result.map_err(|e| transport_err(format!("body stream: {e}")))?,
                Err(_) => {
                    return Err(transport_err(format!(
                        "artifact body stalled beyond the {} ms idle bound",
                        idle_bound.as_millis()
                    )))
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            streamed = streamed.saturating_add(chunk.len() as u64);
            if streamed > max_bytes {
                return Err(UpdateError::ArtifactTooLarge {
                    artifact: url.to_string(),
                    max_bytes,
                });
            }
            sink.write_all(&chunk)
                .await
                .map_err(|e| transport_err(format!("staging write: {e}")))?;
        }
        Ok(streamed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_non_http_url_is_refused_before_any_connect() {
        let fetcher = CheckedHttpFetcher::new(CheckedHttpClient::with_policy(None));
        let mut sink = Vec::new();
        let err = fetcher
            .fetch_to("file:///etc/passwd", 1024, &mut sink)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "transport");
    }

    /// One raw HTTP/1.1 server: reads the request head, optionally writes
    /// `head`, then stalls with the connection open.
    async fn stalling_server(head: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            if !head.is_empty() {
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.flush().await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        format!("http://{addr}/artifact.bin")
    }

    #[tokio::test]
    async fn a_stalled_artifact_host_is_cut_off_at_the_bounds() {
        let fetcher = CheckedHttpFetcher::new(CheckedHttpClient::with_policy(None));
        let head_bound = std::time::Duration::from_millis(100);
        let idle_bound = std::time::Duration::from_millis(100);

        // No headers at all: the head bound fires.
        let url = stalling_server("").await;
        let mut sink = Vec::new();
        let started = std::time::Instant::now();
        let err = fetcher
            .fetch_to_with(&url, 1024, &mut sink, head_bound, idle_bound)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "transport");
        assert!(err.to_string().contains("headers"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        // Headers then a stalled body: the idle bound fires.
        let url = stalling_server("HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n").await;
        let mut sink = Vec::new();
        let started = std::time::Instant::now();
        let err = fetcher
            .fetch_to_with(&url, 1024, &mut sink, head_bound, idle_bound)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "transport");
        assert!(err.to_string().contains("stalled"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
