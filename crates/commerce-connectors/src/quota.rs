//! The quota / rate-limit state connectors enforce before every call.
//!
//! Spec §10: "Respect documented API limits (Mouser 30/min, 1000/day; LCSC
//! 200/min, 1000/day baseline; DigiKey `X-RateLimit-Limit`/`X-RateLimit-
//! Remaining` headers)." This module is the acquire quota state those
//! limits live in:
//!
//! * each source has two local windows (minute and day) anchored at first
//!   use — deterministic, no wall-clock calendar math;
//! * a source's observed `X-RateLimit-Limit` / `X-RateLimit-Remaining`
//!   headers **override** the local caps for the governing budget, so a
//!   connector never relies only on hard-coded limits (DigiKey);
//! * an observed `429`/`Retry-After` pins the source until the stated reset;
//! * admission failures are the typed domain errors
//!   [`SourceError::RateLimited`] and [`SourceError::QuotaExhausted`], never
//!   a generic failure;
//! * a permit can be refunded only for transport failures that provably
//!   never left the machine — a request that may have reached the server is
//!   always counted (no quota evasion).
//!
//! The state is process-local and shared through `Arc<QuotaState>`; the
//! daemon owns the one instance per acquisition runtime.

use std::collections::BTreeMap;
use std::sync::Mutex;

use faktor_commerce::{ConnectorHealth, SourceError, SourceId};

/// One minute in milliseconds.
pub const MINUTE_MS: u64 = 60_000;
/// One day in milliseconds.
pub const DAY_MS: u64 = 86_400_000;

/// The local caps of one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuotaLimits {
    /// Requests per minute, when the source documents one.
    pub per_minute: Option<u32>,
    /// Requests per day, when the source documents one.
    pub per_day: Option<u32>,
}

impl QuotaLimits {
    /// Explicit caps.
    pub const fn new(per_minute: Option<u32>, per_day: Option<u32>) -> Self {
        Self {
            per_minute,
            per_day,
        }
    }

    /// No local caps (header-driven only).
    pub const fn unlimited() -> Self {
        Self {
            per_minute: None,
            per_day: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Window {
    started_ms: u64,
    used: u32,
}

impl Window {
    fn new(now_ms: u64) -> Self {
        Self {
            started_ms: now_ms,
            used: 0,
        }
    }

    fn roll(&mut self, now_ms: u64, length_ms: u64) {
        if now_ms >= self.started_ms.saturating_add(length_ms) {
            self.started_ms = now_ms;
            self.used = 0;
        }
    }

    fn reset_at_ms(&self, length_ms: u64) -> u64 {
        self.started_ms.saturating_add(length_ms)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct HeaderBudget {
    limit: Option<u32>,
    remaining: Option<u32>,
    reset_at_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct SourceQuota {
    limits: QuotaLimits,
    minute: Window,
    day: Window,
    header: HeaderBudget,
}

/// A public read-only view of one source's quota state (for `doctor` /
/// status surfaces and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaSnapshot {
    /// Requests used in the current minute window.
    pub minute_used: u32,
    /// The minute cap, when one is installed.
    pub minute_limit: Option<u32>,
    /// When the minute window resets.
    pub minute_reset_at_ms: u64,
    /// Requests used in the current day window.
    pub day_used: u32,
    /// The day cap, when one is installed.
    pub day_limit: Option<u32>,
    /// When the day window resets.
    pub day_reset_at_ms: u64,
    /// The last observed `X-RateLimit-Limit`.
    pub header_limit: Option<u32>,
    /// The last observed `X-RateLimit-Remaining`.
    pub header_remaining: Option<u32>,
    /// When the observed header budget resets, when known.
    pub header_reset_at_ms: Option<u64>,
}

/// An admitted request. Must be released when the transport failure
/// provably never left the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaPermit {
    key: String,
    charged_minute: bool,
    charged_day: bool,
    charged_header: bool,
}

/// The shared quota state.
pub struct QuotaState {
    inner: Mutex<BTreeMap<String, SourceQuota>>,
}

impl Default for QuotaState {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaState {
    /// An empty state.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// Install (or replace) the local caps of one source.
    pub fn register_limits(&self, source: &SourceId, limits: QuotaLimits) {
        let mut inner = self.lock();
        let entry = inner
            .entry(source.as_str().to_string())
            .or_insert_with(|| SourceQuota {
                limits,
                minute: Window::new(0),
                day: Window::new(0),
                header: HeaderBudget::default(),
            });
        entry.limits = limits;
    }

    /// Try to admit one request for `source`.
    pub fn try_acquire(&self, source: &SourceId, now_ms: u64) -> Result<QuotaPermit, SourceError> {
        let key = source.as_str().to_string();
        let mut inner = self.lock();
        let entry = inner.entry(key.clone()).or_insert_with(|| SourceQuota {
            limits: QuotaLimits::unlimited(),
            minute: Window::new(now_ms),
            day: Window::new(now_ms),
            header: HeaderBudget::default(),
        });
        entry.minute.roll(now_ms, MINUTE_MS);
        entry.day.roll(now_ms, DAY_MS);

        // 1. The observed header budget is authoritative when the source
        //    states it (DigiKey). `remaining == 0` pins the source until the
        //    observed reset (or one minute when the source states none).
        if let Some(remaining) = entry.header.remaining {
            if remaining == 0 {
                let reset_at = entry
                    .header
                    .reset_at_ms
                    .unwrap_or_else(|| now_ms.saturating_add(MINUTE_MS));
                if now_ms < reset_at {
                    return Err(SourceError::RateLimited {
                        retry_after_ms: reset_at - now_ms,
                    });
                }
                entry.header.remaining = entry.header.limit.or(Some(1));
                entry.header.reset_at_ms = None;
            }
        }

        // 2. Local per-minute cap.
        if let Some(limit) = entry.limits.per_minute {
            if entry.minute.used >= limit {
                return Err(SourceError::RateLimited {
                    retry_after_ms: entry.minute.reset_at_ms(MINUTE_MS).saturating_sub(now_ms),
                });
            }
        }

        // 3. Local per-day cap.
        if let Some(limit) = entry.limits.per_day {
            if entry.day.used >= limit {
                return Err(SourceError::QuotaExhausted {
                    reset_ms: entry.day.reset_at_ms(DAY_MS),
                });
            }
        }

        let charged_minute = entry.limits.per_minute.is_some();
        let charged_day = entry.limits.per_day.is_some();
        let charged_header = entry.header.remaining.is_some();
        entry.minute.used = entry.minute.used.saturating_add(1);
        entry.day.used = entry.day.used.saturating_add(1);
        if let Some(remaining) = entry.header.remaining.as_mut() {
            *remaining = remaining.saturating_sub(1);
        }
        Ok(QuotaPermit {
            key,
            charged_minute,
            charged_day,
            charged_header,
        })
    }

    /// Refund a permit for a request that never left the machine.
    pub fn release(&self, permit: QuotaPermit) {
        let mut inner = self.lock();
        if let Some(entry) = inner.get_mut(&permit.key) {
            if permit.charged_minute {
                entry.minute.used = entry.minute.used.saturating_sub(1);
            }
            if permit.charged_day {
                entry.day.used = entry.day.used.saturating_sub(1);
            }
            if permit.charged_header {
                if let Some(remaining) = entry.header.remaining.as_mut() {
                    *remaining = remaining.saturating_add(1);
                }
            }
        }
    }

    /// Consume one response's rate-limit headers (spec §10: never rely only
    /// on hard-coded limits).
    pub fn observe_headers(
        &self,
        source: &SourceId,
        limit: Option<u32>,
        remaining: Option<u32>,
        reset_at_ms: Option<u64>,
        now_ms: u64,
    ) {
        let mut inner = self.lock();
        let entry = inner
            .entry(source.as_str().to_string())
            .or_insert_with(|| SourceQuota {
                limits: QuotaLimits::unlimited(),
                minute: Window::new(now_ms),
                day: Window::new(now_ms),
                header: HeaderBudget::default(),
            });
        if let Some(limit) = limit {
            entry.header.limit = Some(limit);
        }
        if let Some(remaining) = remaining {
            entry.header.remaining = Some(remaining);
            if remaining == 0 {
                entry.header.reset_at_ms =
                    reset_at_ms.or_else(|| Some(now_ms.saturating_add(MINUTE_MS)));
            }
        } else if let Some(reset_at_ms) = reset_at_ms {
            entry.header.reset_at_ms = Some(reset_at_ms);
        }
    }

    /// Record a `429`/`Retry-After`: pins the source until the stated reset.
    pub fn observe_rate_limited(
        &self,
        source: &SourceId,
        retry_after_ms: Option<u64>,
        now_ms: u64,
    ) {
        let retry_after_ms = retry_after_ms.unwrap_or(MINUTE_MS).min(DAY_MS);
        self.observe_headers(
            source,
            None,
            Some(0),
            Some(now_ms.saturating_add(retry_after_ms)),
            now_ms,
        );
    }

    /// A read-only snapshot of one source.
    pub fn snapshot(&self, source: &SourceId, now_ms: u64) -> QuotaSnapshot {
        let key = source.as_str().to_string();
        let mut inner = self.lock();
        let entry = inner.entry(key).or_insert_with(|| SourceQuota {
            limits: QuotaLimits::unlimited(),
            minute: Window::new(now_ms),
            day: Window::new(now_ms),
            header: HeaderBudget::default(),
        });
        entry.minute.roll(now_ms, MINUTE_MS);
        entry.day.roll(now_ms, DAY_MS);
        QuotaSnapshot {
            minute_used: entry.minute.used,
            minute_limit: entry.limits.per_minute,
            minute_reset_at_ms: entry.minute.reset_at_ms(MINUTE_MS),
            day_used: entry.day.used,
            day_limit: entry.limits.per_day,
            day_reset_at_ms: entry.day.reset_at_ms(DAY_MS),
            header_limit: entry.header.limit,
            header_remaining: entry.header.remaining,
            header_reset_at_ms: entry.header.reset_at_ms,
        }
    }

    /// The planner-facing health of one source right now.
    pub fn health(&self, source: &SourceId, now_ms: u64) -> ConnectorHealth {
        match self.try_acquire_probe(source, now_ms) {
            Ok(()) => ConnectorHealth::Healthy,
            Err(error) => ConnectorHealth::from_error(&error),
        }
    }

    /// Probe admission without consuming it.
    fn try_acquire_probe(&self, source: &SourceId, now_ms: u64) -> Result<(), SourceError> {
        let snapshot = self.snapshot(source, now_ms);
        if let Some(remaining) = snapshot.header_remaining {
            if remaining == 0 {
                let reset_at = snapshot
                    .header_reset_at_ms
                    .unwrap_or_else(|| now_ms.saturating_add(MINUTE_MS));
                if now_ms < reset_at {
                    return Err(SourceError::RateLimited {
                        retry_after_ms: reset_at - now_ms,
                    });
                }
            }
        }
        if let Some(limit) = snapshot.minute_limit {
            if snapshot.minute_used >= limit {
                return Err(SourceError::RateLimited {
                    retry_after_ms: snapshot.minute_reset_at_ms.saturating_sub(now_ms),
                });
            }
        }
        if let Some(limit) = snapshot.day_limit {
            if snapshot.day_used >= limit {
                return Err(SourceError::QuotaExhausted {
                    reset_ms: snapshot.day_reset_at_ms,
                });
            }
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, SourceQuota>> {
        // Poison-tolerant: quota accounting must never panic the runtime.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl std::fmt::Debug for QuotaState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("QuotaState")
            .field("sources", &inner.len())
            .finish()
    }
}

/// Parse a `Retry-After` header: delta-seconds or an HTTP date. Only the
/// delta-seconds form is honored; the date form degrades to a conservative
/// one-minute wait (documented). Anything hostile is ignored.
pub fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 32 {
        return None;
    }
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.saturating_mul(1000).min(DAY_MS));
    }
    Some(MINUTE_MS)
}

/// A label for one quota scope (spec §15 multi-scope rate keys). The
/// connector-side state is per source today; the label keeps the future
/// scopes explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QuotaScope {
    /// The whole source.
    Source,
    /// One account of the source.
    Account,
    /// One browser profile.
    Profile,
    /// One egress identity.
    Egress,
    /// One host.
    Host,
    /// One operation class.
    OperationClass,
}

impl QuotaScope {
    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Account => "account",
            Self::Profile => "profile",
            Self::Egress => "egress",
            Self::Host => "host",
            Self::OperationClass => "operation_class",
        }
    }
}
