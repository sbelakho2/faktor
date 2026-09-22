//! Small time helpers shared across the crate.

use std::time::Duration;

use faktor_core::time::{Clock, Deadline, SystemClock};

pub(crate) fn now_ms() -> i64 {
    SystemClock.now_ms()
}

/// Convert a [`Deadline`] into a tokio instant (saturating at "now" for an
/// already-expired deadline).
pub(crate) fn deadline_instant(deadline: Deadline) -> tokio::time::Instant {
    let remaining = deadline.at_ms().saturating_sub(now_ms()).max(0) as u64;
    tokio::time::Instant::now() + Duration::from_millis(remaining)
}

/// A deadline `ms` milliseconds from now.
pub(crate) fn deadline_in(ms: u64) -> Deadline {
    Deadline::at(now_ms().saturating_add(ms.min(i64::MAX as u64) as i64))
}
