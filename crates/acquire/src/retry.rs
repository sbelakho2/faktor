//! Bounded, cancellation-aware retries.
//!
//! Only the transient class is retried automatically; authentication,
//! verification, rate-limit and quota failures surface immediately (the
//! runtime never sleeps through a limit it must respect). Backoff is
//! exponential with bounded jitter, and the jitter is derived
//! deterministically from `(seed, attempt)` so a test can assert exact
//! bounds while production seeds differ per call site.

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::ctx::AcquireCtx;
use crate::error::AcquisitionError;

/// The bounded retry policy of one acquisition path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquisitionRetryPolicy {
    /// Total attempts (1 = no retry).
    pub max_attempts: u32,
    /// First backoff in milliseconds.
    pub base_backoff_ms: u64,
    /// Hard cap on one backoff.
    pub max_backoff_ms: u64,
    /// Jitter as a percentage of the exponential delay (0..=100).
    pub jitter_percent: u32,
    /// The jitter seed (deterministic in tests, varied in production).
    pub seed: u64,
}

impl Default for AcquisitionRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_backoff_ms: 250,
            max_backoff_ms: 5_000,
            jitter_percent: 25,
            seed: 0,
        }
    }
}

impl AcquisitionRetryPolicy {
    /// Whether a retry follows `attempt` failures (0-based). Bounded by
    /// `max_attempts` and gated on the transient class only.
    pub fn should_retry(&self, attempt: u32, err: &AcquisitionError) -> bool {
        attempt.saturating_add(1) < self.max_attempts && err.is_transient()
    }

    /// The backoff before retry `attempt` (0-based), in milliseconds:
    /// `min(base * 2^attempt, max)` scaled by a deterministic jitter in
    /// `[100 - jitter_percent, 100 + jitter_percent]` percent, then capped at
    /// `max_backoff_ms` and floored at 1.
    pub fn backoff_ms(&self, attempt: u32) -> u64 {
        let cap = self.max_backoff_ms.max(1);
        let exponential = self
            .base_backoff_ms
            .saturating_mul(2u64.saturating_pow(attempt))
            .min(cap);
        let jitter = self.jitter_percent.min(100) as u64;
        if jitter == 0 {
            return exponential.max(1);
        }
        let span = jitter.saturating_mul(2).saturating_add(1);
        let offset = (jitter_offset(self.seed, attempt) % span) as i64 - jitter as i64;
        let scaled = (exponential as i128) * (100 + offset as i128) / 100;
        (scaled.max(1).min(cap as i128)) as u64
    }
}

fn jitter_offset(seed: u64, attempt: u32) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed.to_le_bytes());
    hasher.update(&attempt.to_le_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(bytes)
}

/// Run `op` with bounded, transient-only retries. The sleep between attempts
/// aborts on cancellation and deadline, so a retry can never outlive the
/// caller's budget.
pub async fn run_with_retry<T, F, Fut>(
    ctx: &AcquireCtx,
    policy: &AcquisitionRetryPolicy,
    mut op: F,
) -> Result<T, AcquisitionError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, AcquisitionError>>,
{
    let mut attempt = 0u32;
    loop {
        ctx.check_active()?;
        match op(attempt).await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if !policy.should_retry(attempt, &err) {
                    return Err(err);
                }
                let delay = policy.backoff_ms(attempt);
                tracing::debug!(
                    attempt,
                    delay_ms = delay,
                    class = ?err.retry_class(),
                    "retrying transient acquisition failure"
                );
                ctx.sleep_or_abort(delay).await?;
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::VerificationKind;

    #[test]
    fn only_transient_failures_are_retried() {
        let policy = AcquisitionRetryPolicy {
            max_attempts: 5,
            ..Default::default()
        };
        assert!(policy.should_retry(0, &AcquisitionError::NetworkTimeout));
        assert!(policy.should_retry(0, &AcquisitionError::ApiUnavailable));
        for err in [
            AcquisitionError::AuthenticationRequired,
            AcquisitionError::VerificationRequired {
                kind: VerificationKind::Challenge,
            },
            AcquisitionError::RateLimited { retry_after_ms: 10 },
            AcquisitionError::QuotaExhausted { reset_ms: 10 },
            AcquisitionError::Cancelled,
            AcquisitionError::Deadline,
            AcquisitionError::EgressUnavailable,
        ] {
            assert!(!policy.should_retry(0, &err), "{err} must not be retried");
        }
    }

    #[test]
    fn attempts_are_bounded() {
        let policy = AcquisitionRetryPolicy {
            max_attempts: 3,
            ..Default::default()
        };
        assert!(policy.should_retry(0, &AcquisitionError::NetworkTimeout));
        assert!(policy.should_retry(1, &AcquisitionError::NetworkTimeout));
        assert!(!policy.should_retry(2, &AcquisitionError::NetworkTimeout));
        assert!(!policy.should_retry(99, &AcquisitionError::NetworkTimeout));
        let no_retry = AcquisitionRetryPolicy {
            max_attempts: 1,
            ..Default::default()
        };
        assert!(!no_retry.should_retry(0, &AcquisitionError::NetworkTimeout));
    }

    #[test]
    fn backoff_is_exponential_bounded_and_jittered() {
        let policy = AcquisitionRetryPolicy {
            max_attempts: 10,
            base_backoff_ms: 100,
            max_backoff_ms: 1_000,
            jitter_percent: 25,
            seed: 7,
        };
        for attempt in 0..12u32 {
            let delay = policy.backoff_ms(attempt);
            let exponential = 100u64
                .saturating_mul(2u64.saturating_pow(attempt))
                .min(1_000);
            let low = (exponential * 75 / 100).max(1);
            assert!(delay <= 1_000, "attempt {attempt}: {delay} exceeds the cap");
            assert!(
                delay >= low,
                "attempt {attempt}: {delay} below the jitter floor {low}"
            );
        }
        assert_eq!(policy.backoff_ms(20), 1_000, "the cap applies exactly");
        let exact = AcquisitionRetryPolicy {
            jitter_percent: 0,
            ..policy
        };
        assert_eq!(exact.backoff_ms(0), 100);
        assert_eq!(exact.backoff_ms(1), 200);
        assert_eq!(exact.backoff_ms(2), 400);
        assert_eq!(exact.backoff_ms(9), 1_000);
    }

    #[test]
    fn jitter_varies_with_the_seed_but_is_deterministic() {
        let a = AcquisitionRetryPolicy {
            seed: 1,
            ..Default::default()
        };
        let b = AcquisitionRetryPolicy { seed: 2, ..a };
        assert_eq!(a.backoff_ms(3), a.backoff_ms(3), "same seed is stable");
        let differs = (0..8).any(|attempt| a.backoff_ms(attempt) != b.backoff_ms(attempt));
        assert!(differs, "different seeds must decorrelate the jitter");
    }
}
