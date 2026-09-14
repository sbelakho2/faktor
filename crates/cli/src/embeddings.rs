//! The configured semantic embedder: the runtime half of the additive
//! `[embeddings]` config section. It resolves the SELECTED provider from the
//! daemon registry and maps the provider's synchronous embedding call onto
//! the search crate's [`faktor_search::Embedder`] seam.
//!
//! Invariants:
//!
//! - inputs are split into batches bounded by BOTH the provider request
//!   count cap and the summed byte cap, so a large workspace never becomes
//!   one unbounded embedding request;
//! - an oversized single input is a typed refusal, never a silent
//!   truncation (a truncated input would embed different semantics);
//! - every provider error is a typed [`faktor_core::Error`] with the
//!   provider's retryability and native code preserved, and retries happen
//!   ONLY per the configured [`RetryPolicy`];
//! - a malformed provider response (wrong vector count/dimension) is a
//!   typed `Malformed` error before anything scores it.

use std::sync::Arc;
use std::time::Duration;

use faktor_core::error::{Error, ErrorKind};
use faktor_core::retry::RetryPolicy;
use faktor_provider::{
    EmbeddingRequest, EmbeddingResponse, Provider, ProviderError, ProviderErrorKind,
    MAX_EMBEDDING_INPUTS, MAX_EMBEDDING_INPUT_BYTES, MAX_EMBEDDING_TOTAL_INPUT_BYTES,
};

/// Sleeper seam: production sleeps for the policy backoff, tests inject a
/// no-op so retry assertions stay instant.
pub type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

pub struct ProviderEmbedder {
    provider: Arc<dyn Provider>,
    model: String,
    retry: RetryPolicy,
    sleeper: Sleeper,
}

impl ProviderEmbedder {
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>, retry: RetryPolicy) -> Self {
        Self {
            provider,
            model: model.into(),
            retry,
            sleeper: Arc::new(std::thread::sleep),
        }
    }

    /// Test seam: replace the backoff sleeper (assertions on retry counts
    /// must not pay the real delay).
    #[cfg(test)]
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// Split the inputs into provider-legal batches: every batch respects
    /// the count cap and the summed byte cap. A single input over the
    /// per-input cap is a typed `Oversized` refusal.
    fn batches(&self, texts: &[String]) -> Result<Vec<Vec<String>>, Error> {
        let mut batches: Vec<Vec<String>> = Vec::new();
        let mut current: Vec<String> = Vec::new();
        let mut current_bytes: usize = 0;
        for text in texts {
            if text.len() > MAX_EMBEDDING_INPUT_BYTES {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    format!(
                        "embedding input of {} bytes exceeds the provider cap ({MAX_EMBEDDING_INPUT_BYTES})",
                        text.len()
                    ),
                ));
            }
            let would_exceed_count = current.len() + 1 > MAX_EMBEDDING_INPUTS;
            let would_exceed_bytes =
                current_bytes.saturating_add(text.len()) > MAX_EMBEDDING_TOTAL_INPUT_BYTES;
            if !current.is_empty() && (would_exceed_count || would_exceed_bytes) {
                batches.push(std::mem::take(&mut current));
                current_bytes = 0;
            }
            current_bytes = current_bytes.saturating_add(text.len());
            current.push(text.clone());
        }
        if !current.is_empty() {
            batches.push(current);
        }
        Ok(batches)
    }

    fn call_with_retry(&self, req: EmbeddingRequest) -> Result<EmbeddingResponse, Error> {
        let max_attempts = self.retry.max_attempts.max(1);
        for attempt in 0..max_attempts {
            if attempt > 0 {
                (self.sleeper)(self.retry.next_delay(attempt - 1));
            }
            match self.provider.embed(req.clone()) {
                Ok(response) => return Ok(response),
                Err(e) => {
                    let rate_limited = matches!(e.kind, ProviderErrorKind::RateLimited);
                    if self.retry.should_retry(attempt, e.retryable, rate_limited) {
                        continue;
                    }
                    return Err(map_provider_error(e));
                }
            }
        }
        Err(Error::new(
            ErrorKind::Internal,
            "embedding retry loop ended without a terminal outcome",
        ))
    }
}

/// Preserve the provider's native code + retryability on the core error, so
/// callers (search fusion, journals) can act on the exact class.
fn map_provider_error(e: ProviderError) -> Error {
    let kind = match e.kind {
        ProviderErrorKind::Network => ErrorKind::Network,
        ProviderErrorKind::Timeout => ErrorKind::Timeout,
        ProviderErrorKind::RateLimited => ErrorKind::RateLimited,
        ProviderErrorKind::Cancelled => ErrorKind::Cancelled,
        ProviderErrorKind::BadRequest | ProviderErrorKind::Auth | ProviderErrorKind::Server => {
            ErrorKind::Provider {
                code: e
                    .code
                    .clone()
                    .unwrap_or_else(|| format!("{:?}", e.kind).to_lowercase()),
                retryable: e.retryable,
            }
        }
        ProviderErrorKind::Malformed => ErrorKind::Malformed,
    };
    let mut mapped = Error::new(kind, format!("embeddings: {}", e.message));
    if let Some(code) = e.code {
        // Keep the native code reachable for journals without changing the
        // typed kind shape.
        if !mapped.message.contains(&code) {
            mapped.message = format!("{} [{code}]", mapped.message);
        }
    }
    mapped
}

impl faktor_search::Embedder for ProviderEmbedder {
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        // Best-effort infallible bridge; the fallible seam carries the
        // typed error and is what the search service uses.
        self.try_embed(texts).unwrap_or_default()
    }

    fn try_embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for batch in self.batches(texts)? {
            let expected = batch.len();
            let req = EmbeddingRequest::new(self.model.clone(), batch)
                .map_err(|e| Error::new(e.kind.clone(), format!("embeddings: {}", e.message)))?;
            let response = self.call_with_retry(req)?;
            response.validate_for(expected).map_err(|e| {
                Error::new(ErrorKind::Malformed, format!("embeddings: {}", e.message))
            })?;
            out.extend(response.vectors);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::model::ModelCapabilities;
    use faktor_core::retry::RetryClass;
    use faktor_provider::FakeProvider;
    use faktor_search::Embedder as _;

    fn one(value: f32) -> EmbeddingResponse {
        EmbeddingResponse::new(vec![vec![value]]).unwrap()
    }

    fn policy(max_attempts: u32, class: RetryClass) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay_ms: 1,
            max_delay_ms: 1,
            jitter: 0.0,
            class,
        }
    }

    fn noop_sleeper() -> Sleeper {
        Arc::new(|_: Duration| {})
    }

    /// Provider errors are typed and retried ONLY per the injected policy:
    /// a retryable class recovers on the scripted success, a terminal
    /// (non-retryable) error returns immediately, and exhaustion surfaces
    /// the last typed error.
    #[test]
    fn provider_errors_are_typed_and_retried_per_policy() {
        let fake = Arc::new(
            FakeProvider::new("p", ModelCapabilities::default()).with_embeddings(
                "m",
                vec![
                    Err(ProviderError::new(ProviderErrorKind::Network, "reset")),
                    Err(ProviderError::new(ProviderErrorKind::RateLimited, "429")),
                    Ok(one(0.5)),
                ],
            ),
        );
        let embedder = ProviderEmbedder::new(fake.clone(), "m", policy(3, RetryClass::Always))
            .with_sleeper(noop_sleeper());
        let out = embedder.try_embed(&["x".into()]).unwrap();
        assert_eq!(out, vec![vec![0.5]]);
        assert_eq!(
            fake.embedding_call_count(),
            3,
            "two failures then success = three provider calls"
        );
        assert_eq!(fake.embedding_requests().len(), 3);

        // Terminal error (BadRequest) is not retried even with attempts left.
        let fake = Arc::new(
            FakeProvider::new("p", ModelCapabilities::default()).with_embeddings(
                "m",
                vec![Err(ProviderError::new(
                    ProviderErrorKind::BadRequest,
                    "hostile",
                ))],
            ),
        );
        let embedder = ProviderEmbedder::new(fake.clone(), "m", policy(3, RetryClass::Always))
            .with_sleeper(noop_sleeper());
        let err = embedder.try_embed(&["x".into()]).unwrap_err();
        assert!(
            matches!(
                err.kind,
                ErrorKind::Provider {
                    retryable: false,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            fake.embedding_call_count(),
            1,
            "terminal errors never retry"
        );

        // Exhaustion surfaces the last typed error after exactly
        // `max_attempts` provider calls.
        let fake = Arc::new(
            FakeProvider::new("p", ModelCapabilities::default()).with_embeddings(
                "m",
                vec![
                    Err(ProviderError::new(ProviderErrorKind::Network, "e1")),
                    Err(ProviderError::new(ProviderErrorKind::Network, "e2")),
                    Err(ProviderError::new(ProviderErrorKind::Network, "e3")),
                ],
            ),
        );
        let embedder = ProviderEmbedder::new(fake.clone(), "m", policy(3, RetryClass::Always))
            .with_sleeper(noop_sleeper());
        let err = embedder.try_embed(&["x".into()]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Network);
        assert_eq!(fake.embedding_call_count(), 3);

        // The policy CLASS decides retryability: a Network-class policy does
        // not retry a rate-limit error.
        let fake = Arc::new(
            FakeProvider::new("p", ModelCapabilities::default()).with_embeddings(
                "m",
                vec![Err(ProviderError::new(
                    ProviderErrorKind::RateLimited,
                    "429",
                ))],
            ),
        );
        let embedder = ProviderEmbedder::new(fake.clone(), "m", policy(5, RetryClass::Network))
            .with_sleeper(noop_sleeper());
        let err = embedder.try_embed(&["x".into()]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::RateLimited);
        assert_eq!(fake.embedding_call_count(), 1);
    }

    /// Large corpora are split into provider-legal batches (count and byte
    /// caps) with the input order preserved; an oversized single input is a
    /// typed refusal before any provider call.
    #[test]
    fn inputs_are_batched_and_oversized_inputs_refuse_typed() {
        let fake = Arc::new(
            FakeProvider::new("p", ModelCapabilities::default()).with_embeddings(
                "m",
                vec![
                    Ok(EmbeddingResponse::new(vec![vec![1.0]; MAX_EMBEDDING_INPUTS]).unwrap()),
                    Ok(
                        EmbeddingResponse::new(vec![vec![2.0]; 100 - MAX_EMBEDDING_INPUTS])
                            .unwrap(),
                    ),
                ],
            ),
        );
        let embedder = ProviderEmbedder::new(fake.clone(), "m", policy(1, RetryClass::Network));
        let texts: Vec<String> = (0..100).map(|i| format!("t{i}")).collect();
        let out = embedder.try_embed(&texts).unwrap();
        assert_eq!(out.len(), 100, "every input gets exactly one vector");
        let requests = fake.embedding_requests();
        assert_eq!(requests.len(), 2, "two batches at 64+36");
        assert_eq!(requests[0].inputs.len(), MAX_EMBEDDING_INPUTS);
        assert_eq!(requests[1].inputs.len(), 100 - MAX_EMBEDDING_INPUTS);
        assert_eq!(requests[0].inputs[0], "t0");
        assert_eq!(requests[1].inputs[0], "t64");

        // A single input over the per-input cap is refused typed, with zero
        // provider calls.
        let fake = Arc::new(
            FakeProvider::new("p", ModelCapabilities::default()).with_embeddings("m", vec![]),
        );
        let embedder = ProviderEmbedder::new(fake.clone(), "m", policy(1, RetryClass::Network));
        let oversized = "x".repeat(MAX_EMBEDDING_INPUT_BYTES + 1);
        let err = embedder.try_embed(&[oversized]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Oversized);
        assert_eq!(fake.embedding_call_count(), 0);
    }
}
