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
    ) -> futures::future::BoxFuture<'_, Result<CheckedResponse, faktor_provider::egress::EgressError>>
    {
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
