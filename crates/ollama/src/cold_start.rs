//! Cold-start bounds and warm-up for lazily loaded local models.
//!
//! Ollama loads a model on the first request, and a large local model can
//! spend minutes reading weights/prefilling context before any response
//! header exists. These bounds are configurable per instance and clamped by
//! the provider ceiling; the warm-up uses the same first-byte patience so a
//! cold-sensitive operation can load the model outside its measured window.

use super::*;

/// Documented cold-start bound for the response HEADERS of one streaming
/// `/api/chat` call. A local model is loaded lazily on the first request and
/// a large model can spend minutes reading weights/prefilling context before
/// the first byte exists; the generic 60 s transport default is a premature
/// failure for exactly this provider. Configurable per instance via
/// [`OllamaConfig::with_timeouts`]. Bounded by the operation deadline and
/// [`PROVIDER_CEILING_MS`].
pub const OLLAMA_COLD_FIRST_BYTE_TIMEOUT_MS: u64 = 300_000;
/// Documented stream read/idle bound after headers: a local model may think
/// or load additional context between frames. Configurable via
/// [`OllamaConfig::with_timeouts`].
pub const OLLAMA_STREAM_IDLE_TIMEOUT_MS: u64 = 120_000;

/// Stream hang controls: first-byte / idle bounds from the configured
/// cold-start policy. The OVERALL bound rides the operation deadline the
/// runtime stamped into `RequestMeta::deadline_ms`: `0` keeps streams
/// unbounded overall (defaults only), any positive value caps the stream's
/// whole lifetime at `min(deadline_ms, PROVIDER_CEILING_MS)`.
pub(super) fn stream_deadlines(
    request: &GenericAgentRequest,
    config: &OllamaConfig,
) -> StreamDeadlines {
    let (first_byte_ms, idle_ms) = config.stream_timeouts();
    let mut deadlines = StreamDeadlines {
        first_byte_ms,
        idle_ms,
        overall_ms: 0,
    };
    if request.meta.deadline_ms > 0 {
        deadlines.overall_ms = request.meta.deadline_ms.min(PROVIDER_CEILING_MS);
    }
    deadlines
}

impl OllamaConfig {
    /// Explicit cold-start/read bounds (milliseconds). Zero/None falls back
    /// to the documented defaults; values above [`PROVIDER_CEILING_MS`] are
    /// clamped by the stream guard.
    pub fn with_timeouts(
        mut self,
        first_byte_timeout_ms: Option<u64>,
        idle_timeout_ms: Option<u64>,
    ) -> Self {
        self.first_byte_timeout_ms = first_byte_timeout_ms.filter(|ms| *ms > 0);
        self.idle_timeout_ms = idle_timeout_ms.filter(|ms| *ms > 0);
        self
    }

    /// The effective first-byte and idle bounds for one stream.
    pub fn stream_timeouts(&self) -> (u64, u64) {
        (
            self.first_byte_timeout_ms
                .unwrap_or(OLLAMA_COLD_FIRST_BYTE_TIMEOUT_MS)
                .clamp(1, PROVIDER_CEILING_MS),
            self.idle_timeout_ms
                .unwrap_or(OLLAMA_STREAM_IDLE_TIMEOUT_MS)
                .clamp(1, PROVIDER_CEILING_MS),
        )
    }
}

impl OllamaProvider {
    /// Load `model` (and run one trivial non-streaming completion) BEFORE a
    /// measured/cold-sensitive operation. Ollama loads a model lazily on the
    /// first request; on a large local model that first request can spend
    /// minutes reading weights before any response header exists. The bound
    /// is the configured cold-start first-byte timeout, so a warm-up is as
    /// patient as a real first request and never unbounded. Returns the
    /// observed warm-up wall time.
    pub async fn warm_up(&self, model: &str) -> Result<std::time::Duration, Error> {
        let started = std::time::Instant::now();
        let model = self.resolved_model_name(model);
        let url = format!("{}/api/chat", self.config.base_url);
        let mut body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Reply with ACK."}],
            "stream": false,
            "options": {"num_predict": 1}
        });
        if let Some(keep_alive) = &self.config.keep_alive {
            body["keep_alive"] = serde_json::json!(keep_alive);
        }
        let (first_byte_ms, idle_ms) = self.config.stream_timeouts();
        let call = execute_post_json(
            self.transport.as_ref(),
            &url,
            reqwest::header::HeaderMap::new(),
            &body,
        );
        let resp = match tokio::time::timeout(std::time::Duration::from_millis(first_byte_ms), call)
            .await
        {
            Ok(result) => result.map_err(egress_to_host_error)?,
            Err(_) => {
                return Err(Error::timeout(format!(
                    "ollama warm-up headers exceeded the {first_byte_ms} ms cold-start bound"
                )))
            }
        };
        if !resp.status().is_success() {
            return Err(Error::new(
                ErrorKind::Provider {
                    code: resp.status().as_u16().to_string(),
                    retryable: false,
                },
                format!("ollama warm-up returned {}", resp.status()),
            ));
        }
        let value: serde_json::Value =
            read_metadata_json("warm-up", resp, std::time::Duration::from_millis(idle_ms)).await?;
        if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
            return Err(Error::new(
                ErrorKind::Provider {
                    code: "warm_up".into(),
                    retryable: false,
                },
                format!("ollama warm-up failed: {error}"),
            ));
        }
        Ok(started.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{req, stream_chunks};
    use faktor_provider::egress::MockHttpTransport;
    use futures::StreamExt;

    /// A transport that delays the RESPONSE HEADERS by `delay` (the exact
    /// cold local-model shape: no bytes until the weights are loaded), then
    /// delegates to the canned mock response.
    struct DelayedHeaderTransport {
        delay: std::time::Duration,
        inner: Arc<MockHttpTransport>,
    }

    impl HttpTransport for DelayedHeaderTransport {
        fn execute(
            &self,
            req: reqwest::Request,
        ) -> futures::future::BoxFuture<
            '_,
            Result<CheckedResponse, faktor_provider::egress::EgressError>,
        > {
            let inner = self.inner.clone();
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                inner.execute(req).await
            })
        }
    }

    #[test]
    fn cold_start_defaults_are_documented_and_exceed_the_generic_header_default() {
        // The generic transport default (60 s) is a premature failure for a
        // lazily loaded local model. The adapter's documented cold-start
        // bound must be strictly larger, and configurable.
        let defaults = OllamaConfig::new(None);
        let (default_first, default_idle) = defaults.stream_timeouts();
        assert!(default_first > 60_000);
        assert!(default_idle >= 60_000);
        assert_eq!(
            defaults.stream_timeouts(),
            (
                OLLAMA_COLD_FIRST_BYTE_TIMEOUT_MS,
                OLLAMA_STREAM_IDLE_TIMEOUT_MS
            )
        );
        let configured = OllamaConfig::new(None).with_timeouts(Some(123_456), Some(65_432));
        assert_eq!(configured.stream_timeouts(), (123_456, 65_432));
        // Zero is not a way to disable the bound: documented defaults win.
        let zero = OllamaConfig::new(None).with_timeouts(Some(0), Some(0));
        assert_eq!(
            zero.stream_timeouts(),
            (
                OLLAMA_COLD_FIRST_BYTE_TIMEOUT_MS,
                OLLAMA_STREAM_IDLE_TIMEOUT_MS
            )
        );
    }

    /// SIMULATED COLD START (virtual time): headers arrive after 61 s — past
    /// the generic 60 s default that produced the live audit's
    /// `failed_recoverable`. The Ollama adapter's cold-start bound must wait
    /// for the real answer, and a configured shorter bound must still fire
    /// promptly/typed (never unbounded).
    #[tokio::test(start_paused = true)]
    async fn cold_start_header_delay_beyond_60s_is_not_a_premature_failure() {
        let body = r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#;
        let transport = Arc::new(DelayedHeaderTransport {
            delay: std::time::Duration::from_secs(61),
            inner: Arc::new(MockHttpTransport::new(200, body)),
        });
        let provider = OllamaProvider::new(
            OllamaConfig::new(Some("http://mock.invalid".into())),
            transport,
        );
        let mut request = req("cold-model");
        // No operation deadline: the adapter's own cold-start bound governs.
        request.meta.deadline_ms = 0;
        let started = tokio::time::Instant::now();
        let chunks = stream_chunks(provider.as_ref(), request).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_secs(61),
            "the delayed answer must be awaited, not cut at the old 60s default: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(OLLAMA_COLD_FIRST_BYTE_TIMEOUT_MS),
            "the cold-start wait must stay inside the documented bound: {elapsed:?}"
        );
        assert!(chunks
            .iter()
            .any(|chunk| matches!(chunk, ProviderChunk::Text { text } if text == "ok")));
        assert!(chunks
            .iter()
            .any(|chunk| matches!(chunk, ProviderChunk::Done)));

        // The bound is configurable and still yields a typed Timeout.
        let body = r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#;
        let transport = Arc::new(DelayedHeaderTransport {
            delay: std::time::Duration::from_secs(61),
            inner: Arc::new(MockHttpTransport::new(200, body)),
        });
        let provider = OllamaProvider::new(
            OllamaConfig::new(Some("http://mock.invalid".into()))
                .with_timeouts(Some(5_000), Some(10_000)),
            transport,
        );
        let mut request = req("cold-model");
        request.meta.deadline_ms = 0;
        let started = tokio::time::Instant::now();
        let mut stream = provider.stream(request);
        let first = stream
            .next()
            .await
            .expect("one terminal item")
            .expect_err("the configured header bound must fire");
        let elapsed = started.elapsed();
        assert_eq!(first.kind, ProviderErrorKind::Timeout, "{first:?}");
        assert!(first.message.contains("5000 ms"), "{first:?}");
        assert!(
            elapsed >= std::time::Duration::from_secs(5)
                && elapsed < std::time::Duration::from_secs(10),
            "the configured bound must be prompt and finite: {elapsed:?}"
        );
        assert!(stream.next().await.is_none(), "terminal after the timeout");
    }
}
