//! Commercial billing period arithmetic (pure; no clock and no storage).
//!
//! A billing period is a deterministic half-open window `[start_ms, end_ms)`
//! derived from the subscription anchor and the plan's configured period
//! length. There is no calendar/timezone inference here: the same
//! `(anchor_ms, period_ms, at_ms)` triple always yields the same period id,
//! so a quota decision, a settlement outbox row and a usage rollup can never
//! disagree about which period a fact belongs to.

use serde::{Deserialize, Serialize};

/// The period length used when a plan does not configure one (30 days).
pub const DEFAULT_BILLING_PERIOD_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Hard upper bound on a configured period length (10 years).
pub const MAX_BILLING_PERIOD_MS: i64 = 10 * 366 * 24 * 60 * 60 * 1000;
/// The id of the honest "no subscription/plan configured" bucket. Usage is
/// never dropped: an unconfigured organization folds into this bucket (the
/// admission gate still denies it for lack of a plan).
pub const UNCONFIGURED_PERIOD_ID: &str = "all-time";
/// Bound on a derived period id (`bp_<start>_<end>` is far below this).
pub const MAX_BILLING_PERIOD_ID_BYTES: usize = 64;

/// One deterministic billing period.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BillingPeriod {
    pub id: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

impl BillingPeriod {
    /// The all-time bucket used before a subscription/plan exists.
    pub fn unconfigured() -> Self {
        Self {
            id: UNCONFIGURED_PERIOD_ID.to_string(),
            start_ms: 0,
            end_ms: i64::MAX,
        }
    }

    pub fn is_unconfigured(&self) -> bool {
        self.id == UNCONFIGURED_PERIOD_ID
    }

    /// Derive the period containing `at_ms` from a subscription anchor and a
    /// positive period length. Returns `None` for a non-positive/oversized
    /// period or an anchor outside the non-negative epoch domain; callers
    /// refuse typed rather than fabricating a period.
    pub fn derive(anchor_ms: i64, period_ms: i64, at_ms: i64) -> Option<Self> {
        if anchor_ms < 0 || period_ms <= 0 || period_ms > MAX_BILLING_PERIOD_MS {
            return None;
        }
        // i128 keeps the index arithmetic exact even for extreme clocks.
        let elapsed = i128::from(at_ms) - i128::from(anchor_ms);
        let index = if elapsed <= 0 {
            0
        } else {
            elapsed / i128::from(period_ms)
        };
        let start = i128::from(anchor_ms) + index * i128::from(period_ms);
        let end = start + i128::from(period_ms);
        let start_ms = i64::try_from(start).ok()?;
        let end_ms = i64::try_from(end).ok()?;
        let id = format!("bp_{start_ms}_{end_ms}");
        if id.len() > MAX_BILLING_PERIOD_ID_BYTES {
            return None;
        }
        Some(Self {
            id,
            start_ms,
            end_ms,
        })
    }

    /// The period containing `at_ms`, or the honest all-time bucket when no
    /// period is configured.
    pub fn derive_or_unconfigured(anchor_ms: Option<i64>, period_ms: i64, at_ms: i64) -> Self {
        anchor_ms
            .and_then(|anchor| Self::derive(anchor, period_ms, at_ms))
            .unwrap_or_else(Self::unconfigured)
    }

    /// Whether `at_ms` lies inside this half-open window.
    pub fn contains(&self, at_ms: i64) -> bool {
        self.start_ms <= at_ms && at_ms < self.end_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_is_deterministic_and_rolls_over_at_the_boundary() {
        let anchor = 1_000;
        let period = 100;
        let first = BillingPeriod::derive(anchor, period, 1_050).unwrap();
        assert_eq!((first.start_ms, first.end_ms), (1_000, 1_100));
        assert!(first.contains(1_000) && first.contains(1_099) && !first.contains(1_100));
        let same = BillingPeriod::derive(anchor, period, 1_099).unwrap();
        assert_eq!(first, same, "every instant inside a period shares its id");
        let next = BillingPeriod::derive(anchor, period, 1_100).unwrap();
        assert_eq!((next.start_ms, next.end_ms), (1_100, 1_200));
        assert_ne!(first.id, next.id);
        let before = BillingPeriod::derive(anchor, period, 1).unwrap();
        assert_eq!((before.start_ms, before.end_ms), (1_000, 1_100));
    }

    #[test]
    fn hostile_period_inputs_are_refused_not_fabricated() {
        assert!(BillingPeriod::derive(-1, 100, 0).is_none());
        assert!(BillingPeriod::derive(0, 0, 0).is_none());
        assert!(BillingPeriod::derive(0, -1, 0).is_none());
        assert!(BillingPeriod::derive(0, MAX_BILLING_PERIOD_MS + 1, 0).is_none());
        assert!(BillingPeriod::derive(0, 1, i64::MAX).is_none());
        assert_eq!(
            BillingPeriod::derive_or_unconfigured(None, 100, 0),
            BillingPeriod::unconfigured()
        );
    }
}
