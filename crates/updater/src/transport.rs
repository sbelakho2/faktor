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
        let transport_err = |detail: String| UpdateError::Transport {
            artifact: url.to_string(),
            detail,
        };
        let builder = self
            .client
            .get(url)
            .map_err(|e| transport_err(format!("checked URL refused: {e}")))?;
        let mut response = self
            .client
            .send_checked(builder)
            .await
            .map_err(|e| transport_err(format!("request refused/failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(transport_err(format!("HTTP status {status}")));
        }
        let mut streamed: u64 = 0;
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|e| transport_err(format!("body stream: {e}")))?;
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
}
