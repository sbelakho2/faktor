//! Quota accounting: documented windows, reset accounting and Retry-After.
//!
//! A [`QuotaState`] is one rolling window for one acquisition path. It is
//! deliberately small and synchronous so it can live behind a `Mutex` in
//! [`crate::ctx::AcquireCtx`]:
//!
//! * the window resets when `now_ms >= window_started_ms + window.window_ms`;
//! * `try_acquire` is the only place that consumes budget, and it refuses
//!   with the typed `QuotaExhausted { reset_ms }` when the effective limit is
//!   reached — it also records the exhaustion so the refusal is stable until
//!   the reset;
//! * an upstream `Retry-After` is honored verbatim: `record_retry_after`
//!   installs a `RateLimited` gate that `try_acquire` surfaces as
//!   `RateLimited { retry_after_ms }` without consuming budget;
//! * adaptive throttling is integer-only: a transient failure divides the
//!   effective limit (bounded), a success restores it.

use serde::{Deserialize, Serialize};

use crate::error::AcquisitionError;

/// One documented quota window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Window length in milliseconds.
    pub window_ms: u64,
    /// Requests allowed per window.
    pub limit: u32,
}

impl QuotaWindow {
    /// A one-minute window with `limit` requests.
    pub const fn per_minute(limit: u32) -> Self {
        Self {
            window_ms: 60_000,
            limit,
        }
    }

    /// A one-hour window with `limit` requests.
    pub const fn per_hour(limit: u32) -> Self {
        Self {
            window_ms: 3_600_000,
            limit,
        }
    }

    /// A one-day window with `limit` requests.
    pub const fn per_day(limit: u32) -> Self {
        Self {
            window_ms: 86_400_000,
            limit,
        }
    }
}

/// The largest adaptive throttle divisor.
pub const MAX_THROTTLE_SCALE: u32 = 4;

/// Rolling-window quota state for one acquisition path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaState {
    /// The window definition.
    pub window: QuotaWindow,
    /// When the current window started.
    pub window_started_ms: u64,
    /// Requests consumed in the current window.
    pub used: u32,
    /// A `Retry-After` gate: no request before this instant.
    pub retry_at_ms: Option<u64>,
    /// A recorded exhaustion: no request before this instant.
    pub exhausted_until_ms: Option<u64>,
    /// Adaptive throttle divisor in `1..=MAX_THROTTLE_SCALE`.
    pub throttle_scale: u32,
}

impl QuotaState {
    /// A fresh window starting at `now_ms`.
    pub fn new(window: QuotaWindow, now_ms: u64) -> Self {
        Self {
            window,
            window_started_ms: now_ms,
            used: 0,
            retry_at_ms: None,
            exhausted_until_ms: None,
            throttle_scale: 1,
        }
    }

    /// Roll the window when it has elapsed. Returns whether a roll happened.
    /// A roll resets the budget, the exhaustion record and the Retry-After
    /// gate (the throttle scale is *not* reset: only success restores
    /// throughput).
    pub fn roll(&mut self, now_ms: u64) -> bool {
        let elapsed = now_ms.saturating_sub(self.window_started_ms);
        if elapsed < self.window.window_ms.max(1) {
            return false;
        }
        self.window_started_ms = now_ms;
        self.used = 0;
        self.exhausted_until_ms = None;
        self.retry_at_ms = None;
        true
    }

    /// When the current window resets.
    pub fn reset_at_ms(&self) -> u64 {
        self.window_started_ms.saturating_add(self.window.window_ms)
    }

    /// The effective limit after adaptive throttling (never 0).
    pub fn effective_limit(&self) -> u32 {
        let scale = self.throttle_scale.clamp(1, MAX_THROTTLE_SCALE);
        (self.window.limit / scale).max(1)
    }

    /// Remaining budget at `now_ms` without mutating the window.
    pub fn remaining_at(&self, now_ms: u64) -> u32 {
        if now_ms >= self.reset_at_ms() {
            return self.effective_limit();
        }
        self.effective_limit().saturating_sub(self.used)
    }

    /// Whether the path is currently exhausted at `now_ms`.
    pub fn is_exhausted_at(&self, now_ms: u64) -> bool {
        if let Some(until) = self.exhausted_until_ms {
            if now_ms < until {
                return true;
            }
        }
        self.remaining_at(now_ms) == 0
    }

    /// Consume one request. Returns the typed refusal without consuming
    /// budget when a gate is active.
    pub fn try_acquire(&mut self, now_ms: u64) -> Result<(), AcquisitionError> {
        self.roll(now_ms);
        if let Some(until) = self.exhausted_until_ms {
            if now_ms < until {
                return Err(AcquisitionError::QuotaExhausted { reset_ms: until });
            }
            self.exhausted_until_ms = None;
        }
        if let Some(at) = self.retry_at_ms {
            if now_ms < at {
                return Err(AcquisitionError::RateLimited {
                    retry_after_ms: at - now_ms,
                });
            }
            self.retry_at_ms = None;
        }
        if self.used >= self.effective_limit() {
            let reset_ms = self.reset_at_ms();
            self.exhausted_until_ms = Some(reset_ms);
            return Err(AcquisitionError::QuotaExhausted { reset_ms });
        }
        self.used = self.used.saturating_add(1);
        Ok(())
    }

    /// Honor an upstream `Retry-After` (milliseconds): every `try_acquire`
    /// before `now_ms + retry_after_ms` refuses with `RateLimited` and the
    /// remaining wait. The gate never exceeds the window reset.
    pub fn record_retry_after(&mut self, retry_after_ms: u64, now_ms: u64) {
        let at = now_ms.saturating_add(retry_after_ms);
        self.retry_at_ms = Some(at.min(self.reset_at_ms()).max(now_ms));
    }

    /// A transient failure reduces throughput (adaptive throttling).
    pub fn record_transient_failure(&mut self) {
        self.throttle_scale = self
            .throttle_scale
            .saturating_add(1)
            .min(MAX_THROTTLE_SCALE);
    }

    /// A success restores throughput one step.
    pub fn record_success(&mut self) {
        self.throttle_scale = self.throttle_scale.saturating_sub(1).max(1);
    }
}

/// Parse a `Retry-After` header value in delta-seconds. HTTP-date values are
/// not guessed at: they return `None` and the caller applies its documented
/// default.
pub fn parse_retry_after_seconds(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u64>().ok().map(|s| s.saturating_mul(1_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_reset_accounts_exactly() {
        let mut quota = QuotaState::new(QuotaWindow::per_minute(2), 0);
        assert_eq!(quota.remaining_at(0), 2);
        quota.try_acquire(0).unwrap();
        quota.try_acquire(1).unwrap();
        assert_eq!(
            quota.try_acquire(2).unwrap_err(),
            AcquisitionError::QuotaExhausted { reset_ms: 60_000 }
        );
        // Refusal is stable until the reset even after budget appears free.
        assert!(quota.is_exhausted_at(59_999));
        assert_eq!(quota.remaining_at(59_999), 0);
        // Window rolls at the boundary and budget returns.
        quota.try_acquire(60_000).unwrap();
        assert_eq!(quota.used, 1);
        assert!(!quota.is_exhausted_at(60_001));
    }

    #[test]
    fn retry_after_is_honored_and_never_consumes_budget() {
        let mut quota = QuotaState::new(QuotaWindow::per_minute(5), 0);
        quota.record_retry_after(5_000, 1_000);
        assert_eq!(
            quota.try_acquire(1_500).unwrap_err(),
            AcquisitionError::RateLimited {
                retry_after_ms: 4_500
            }
        );
        assert_eq!(quota.used, 0, "a gated attempt never consumes budget");
        quota.try_acquire(6_000).unwrap();
        assert_eq!(quota.used, 1);
    }

    #[test]
    fn retry_after_is_clamped_to_the_window_reset() {
        let mut quota = QuotaState::new(QuotaWindow::per_minute(1), 0);
        quota.record_retry_after(600_000, 0);
        assert_eq!(quota.retry_at_ms, Some(60_000));
    }

    #[test]
    fn adaptive_throttling_is_bounded_and_restored_by_success() {
        let mut quota = QuotaState::new(QuotaWindow::per_minute(8), 0);
        for _ in 0..10 {
            quota.record_transient_failure();
        }
        assert_eq!(quota.throttle_scale, MAX_THROTTLE_SCALE);
        assert_eq!(quota.effective_limit(), 2);
        quota.record_success();
        assert_eq!(quota.throttle_scale, 3);
        for _ in 0..10 {
            quota.record_success();
        }
        assert_eq!(quota.throttle_scale, 1);
        assert_eq!(quota.effective_limit(), 8);
    }

    #[test]
    fn retry_after_parsing_rejects_hostile_values() {
        assert_eq!(parse_retry_after_seconds("7"), Some(7_000));
        assert_eq!(parse_retry_after_seconds(" 12 "), Some(12_000));
        assert_eq!(parse_retry_after_seconds("-1"), None);
        assert_eq!(parse_retry_after_seconds("1.5"), None);
        assert_eq!(
            parse_retry_after_seconds("Wed, 21 Oct 2015 07:28:00 GMT"),
            None
        );
        assert_eq!(parse_retry_after_seconds(""), None);
        assert_eq!(
            parse_retry_after_seconds("999999999999999999999999"),
            None,
            "overflow is refused, not saturated"
        );
    }
}
