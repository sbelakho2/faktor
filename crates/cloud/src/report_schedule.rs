//! The durable billing-report maintenance schedule: periodically reports the
//! folded usage of the CURRENT period through the vendor adapter, with
//! record-before-send idempotency and jittered exponential backoff — and a
//! BOUNDED catch-up for periods crossed while the schedule was down.
//!
//! Invariants:
//!
//! - **Record before send.** The period row is written durably BEFORE the
//!   first vendor call, and a vendor continuation cursor is persisted
//!   before the next page is sent. A crash therefore replays the exact same
//!   `(organization, period, cursor)` idempotency key and the vendor
//!   de-duplicates it;
//! - **Never double-report a period.** A period that completed is terminal
//!   (`reported`); a terminally refused one is `failed`. Both states are
//!   skipped forever (across restarts), so the schedule reports each period
//!   at most once;
//! - **Bounded catch-up.** When the schedule advances past one or more
//!   missed periods (downtime), it re-plans the most recent
//!   [`ReportPolicy::max_catch_up`] of them (oldest first) with the same
//!   record-before-send idempotency, then resumes the regular cadence. When
//!   the cap is exceeded, the OLDER periods are marked `skipped`
//!   permanently with a durable audit row (the skipped range and the cap
//!   are recorded in `last_error`) — a missed period is never silently
//!   dropped;
//! - **Retry per policy.** Only retryable failures (transport/429/5xx) are
//!   retried, up to `max_attempts`, each after a deterministic jittered
//!   exponential backoff; a final 4xx marks the period `failed` typed and
//!   is never retried. Backfill periods retry before the regular cadence
//!   resumes (strict oldest-first);
//! - **Bounded.** One vendor page per tick, one row per period, at most
//!   `max_attempts` retries and at most `max_catch_up` backfill periods per
//!   tick; the store prunes old period rows (open/skipped rows and audit
//!   rows are retained preferentially).

use std::sync::Arc;

use crate::billing_store::{
    BillingStore, ReportPeriodRow, ReportPeriodStatus, MAX_REPORT_PERIODS_PER_ORG,
};
use crate::error::ControlPlaneError;
use crate::ids::OrganizationId;
use crate::service::Clock;
use crate::vendor::{BillingVendorAdapter, ReportOutcome};

/// The shortest accepted maintenance interval.
pub const MIN_REPORT_INTERVAL_MS: i64 = 1_000;
/// The longest accepted maintenance interval (24h).
pub const MAX_REPORT_INTERVAL_MS: i64 = 86_400_000;
/// The shortest accepted reporting period (1min).
pub const MIN_REPORT_PERIOD_MS: i64 = 60_000;
/// The longest accepted reporting period (366 days).
pub const MAX_REPORT_PERIOD_MS: i64 = 366 * 86_400_000;
/// The hard ceiling on one retry backoff (24h).
pub const MAX_REPORT_MAX_BACKOFF_MS: i64 = 86_400_000;
/// The default catch-up window (24 missed periods).
pub const DEFAULT_REPORT_CATCH_UP: usize = 24;
/// Hard cap on the configured catch-up window. Bounded well below
/// [`MAX_REPORT_PERIODS_PER_ORG`] so the backfill rows plus the skip audit
/// row always survive the store's retention pruning.
pub const MAX_REPORT_CATCH_UP_PERIODS: usize = 256;
/// Domain separation of the deterministic backoff jitter.
const JITTER_DOMAIN: &[u8] = b"faktor-billing-report-jitter:v1:";
/// Bound on one recorded failure text.
const MAX_LAST_ERROR_BYTES: usize = 512;

/// The schedule's strict policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportPolicy {
    pub interval_ms: i64,
    pub period_ms: i64,
    pub max_attempts: u32,
    pub retry_base_ms: i64,
    pub max_backoff_ms: i64,
    /// Bound on the missed periods re-planned after a downtime gap
    /// (`0..=MAX_REPORT_CATCH_UP_PERIODS`; `0` = every missed period is
    /// immediately marked skipped with an audit row).
    pub max_catch_up: usize,
}

/// The period key of one instant: a stable printable bucket id
/// (`p<epoch>/<period>`), so the same period always derives the same
/// vendor idempotency key across restarts.
pub fn period_key(now_ms: i64, period_ms: i64) -> String {
    period_from_index(now_ms.div_euclid(period_ms.max(1)))
}

/// The period key of one period index.
pub fn period_from_index(index: i64) -> String {
    format!("p{index}")
}

/// The period index of one period key (`p<epoch>`), `None` for a foreign
/// or malformed key.
pub fn period_index(period: &str) -> Option<i64> {
    period.strip_prefix('p')?.parse::<i64>().ok()
}

/// The bounded catch-up plan of one downtime gap: which missed periods are
/// backfilled (oldest first) and which older range is skipped-permanently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUpPlan {
    /// The missed period indices to backfill, oldest first (at most the
    /// configured cap).
    pub backfill: Vec<i64>,
    /// The inclusive `(first, last)` index range of missed periods that
    /// exceed the cap (older than the backfill window), `None` when every
    /// missed period fits.
    pub skipped: Option<(i64, i64)>,
}

/// Plan the catch-up of the missed periods strictly between `last_index`
/// (the newest period the schedule has recorded) and `current_index`.
///
/// The MOST RECENT `max_catch_up` missed periods are kept for backfill
/// (processed oldest first); everything older is the skipped range. The
/// plan is pure and bounded: it never enumerates more than `max_catch_up`
/// backfill indices.
pub fn catch_up_plan(last_index: i64, current_index: i64, max_catch_up: usize) -> CatchUpPlan {
    let first_missed = last_index.saturating_add(1);
    let last_missed = current_index.saturating_sub(1);
    if last_missed < first_missed {
        return CatchUpPlan {
            backfill: Vec::new(),
            skipped: None,
        };
    }
    let missed = (last_missed - first_missed + 1) as u128;
    let cap = max_catch_up as u128;
    if missed <= cap {
        return CatchUpPlan {
            backfill: (first_missed..=last_missed).collect(),
            skipped: None,
        };
    }
    // `missed > cap`, so the backfill window is exactly the cap-sized suffix
    // (the newest missed periods); everything older is the skipped range.
    let backfill_count = max_catch_up;
    let backfill_start = last_missed - backfill_count as i64 + 1;
    CatchUpPlan {
        backfill: (backfill_start..=last_missed).collect(),
        skipped: Some((first_missed, backfill_start.saturating_sub(1))),
    }
}

/// Deterministic jittered exponential backoff: the delay is in
/// `[base - base/4, base]` (bounded by `max_backoff_ms`), where the jitter
/// is a hash of the period and attempt — reproducible for tests, different
/// across periods/attempts so a fleet does not retry in lockstep.
pub fn backoff_ms(policy: &ReportPolicy, period: &str, attempts: u32) -> i64 {
    let shift = attempts.min(16);
    let base = policy
        .retry_base_ms
        .saturating_mul(1i64 << shift)
        .clamp(0, policy.max_backoff_ms.max(0));
    if base <= 0 {
        return 0;
    }
    let span = (base / 4).max(1);
    let jitter = jitter(policy.retry_base_ms, period, attempts) % span as u64;
    (base - span + jitter as i64).clamp(0, policy.max_backoff_ms.max(0))
}

fn jitter(salt: i64, period: &str, attempts: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in JITTER_DOMAIN
        .iter()
        .copied()
        .chain(salt.to_le_bytes())
        .chain(period.as_bytes().iter().copied())
        .chain(attempts.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// What one maintenance tick did (bounded; the loop logs it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// Nothing was due (the period is terminal, or it is inside its retry
    /// backoff).
    Idle,
    /// One vendor page was accepted and the period is complete.
    Reported {
        period: String,
        attempts: u32,
        idempotent_replay: bool,
    },
    /// One vendor page was accepted; the vendor asked for another page and
    /// the continuation cursor is durably recorded for the next tick.
    Continued { period: String, next_cursor: String },
    /// A retryable failure was recorded; the period retries after the
    /// recorded instant.
    RetryScheduled {
        period: String,
        attempts: u32,
        next_attempt_at_ms: i64,
        error: String,
    },
    /// A final refusal (or exhausted attempts) was recorded; the period is
    /// terminally failed and never retried.
    Failed { period: String, error: String },
}

/// The report schedule over one durable billing store + vendor adapter.
pub struct BillingReportRunner {
    store: Arc<dyn BillingStore>,
    adapter: BillingVendorAdapter,
    organization: OrganizationId,
    policy: ReportPolicy,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for BillingReportRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingReportRunner")
            .field("organization", &self.organization)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl BillingReportRunner {
    pub fn new(
        store: Arc<dyn BillingStore>,
        adapter: BillingVendorAdapter,
        organization: OrganizationId,
        policy: ReportPolicy,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            adapter,
            organization,
            policy,
            clock,
        }
    }

    pub fn policy(&self) -> &ReportPolicy {
        &self.policy
    }

    /// Run one bounded tick: at most ONE vendor page (a backfill period
    /// first, oldest first, then the current period). A downtime gap is
    /// planned — and its skipped audit row written — before the page is
    /// sent, so the backfill is durable (record-before-send) too.
    pub async fn tick(&self) -> Result<TickOutcome, ControlPlaneError> {
        let now = self.clock.now_ms();
        let period_ms = self.policy.period_ms.max(1);
        let current_index = now.div_euclid(period_ms);
        let current_period = period_from_index(current_index);

        let known = self
            .store
            .report_periods(&self.organization, MAX_REPORT_PERIODS_PER_ORG)
            .map_err(ControlPlaneError::from)?;
        let last_index = known
            .iter()
            .filter_map(|row| period_index(&row.period))
            .max();
        let mut rows = known;
        if let Some(last_index) = last_index {
            if current_index > last_index.saturating_add(1) {
                self.plan_catch_up(last_index, current_index, now)?;
                rows = self
                    .store
                    .report_periods(&self.organization, MAX_REPORT_PERIODS_PER_ORG)
                    .map_err(ControlPlaneError::from)?;
            }
        }

        // Strict oldest-first: an outstanding backfill period (any period
        // strictly older than the current one) is reported before the
        // regular cadence. The OLDEST outstanding period owns the tick: if
        // it is inside its retry backoff the tick idles, so periods are
        // never reported out of order.
        let mut outstanding: Vec<(i64, ReportPeriodRow)> = rows
            .iter()
            .filter(|row| row.status == ReportPeriodStatus::Open)
            .filter_map(|row| {
                period_index(&row.period)
                    .filter(|index| *index < current_index)
                    .map(|index| (index, row.clone()))
            })
            .collect();
        outstanding.sort_by_key(|(index, _)| *index);
        if let Some((_, row)) = outstanding.into_iter().next() {
            if row.next_attempt_at_ms > now {
                return Ok(TickOutcome::Idle);
            }
            return self.report_row(row.period.clone(), row, now).await;
        }

        // Regular cadence: the current period.
        let row = match self
            .store
            .report_period(&self.organization, &current_period)
            .map_err(ControlPlaneError::from)?
        {
            Some(row) => row,
            None => {
                // Record-before-send: the period row is durable BEFORE any
                // vendor call, so a crash between the vendor write and the
                // acknowledgement replays the same idempotency key.
                let row = ReportPeriodRow {
                    organization_id: self.organization.as_str().to_string(),
                    period: current_period.clone(),
                    next_cursor: None,
                    status: ReportPeriodStatus::Open,
                    attempts: 0,
                    last_error: None,
                    first_seen_ms: now,
                    reported_at_ms: None,
                    next_attempt_at_ms: now,
                    skipped_at_ms: None,
                };
                self.store
                    .put_report_period(&row)
                    .map_err(ControlPlaneError::from)?;
                row
            }
        };
        if row.status != ReportPeriodStatus::Open || row.next_attempt_at_ms > now {
            return Ok(TickOutcome::Idle);
        }
        self.report_row(current_period, row, now).await
    }

    /// Plan one downtime gap: write the bounded backfill rows (record
    /// before send) and the skipped-range audit row. Idempotent: a row that
    /// already exists (from a pre-crash plan) is never rewritten, so a
    /// restart mid-backfill neither re-sends nor re-audits.
    fn plan_catch_up(
        &self,
        last_index: i64,
        current_index: i64,
        now: i64,
    ) -> Result<(), ControlPlaneError> {
        let plan = catch_up_plan(last_index, current_index, self.policy.max_catch_up);
        if let Some((first, last)) = plan.skipped {
            let anchor = period_from_index(first);
            if self
                .store
                .report_period(&self.organization, &anchor)
                .map_err(ControlPlaneError::from)?
                .is_none()
            {
                let count = last.saturating_sub(first).saturating_add(1);
                let message = format!(
                    "skipped {count} missed periods {anchor}..{}: the {}-period catch-up cap \
                     was exceeded; these periods are permanently skipped (audit only, never \
                     reported)",
                    period_from_index(last),
                    self.policy.max_catch_up
                );
                let row = ReportPeriodRow {
                    organization_id: self.organization.as_str().to_string(),
                    period: anchor,
                    next_cursor: None,
                    status: ReportPeriodStatus::Skipped,
                    attempts: 0,
                    last_error: Some(bounded_text(&message)),
                    first_seen_ms: now,
                    reported_at_ms: None,
                    next_attempt_at_ms: now,
                    skipped_at_ms: Some(now),
                };
                self.store
                    .put_report_period(&row)
                    .map_err(ControlPlaneError::from)?;
                tracing::warn!(
                    first_period = first,
                    last_period = last,
                    skipped_periods = count,
                    "billing report catch-up cap exceeded; older periods marked skipped with a \
                     durable audit row"
                );
            }
        }
        for index in plan.backfill {
            let period = period_from_index(index);
            if self
                .store
                .report_period(&self.organization, &period)
                .map_err(ControlPlaneError::from)?
                .is_some()
            {
                continue;
            }
            let row = ReportPeriodRow {
                organization_id: self.organization.as_str().to_string(),
                period,
                next_cursor: None,
                status: ReportPeriodStatus::Open,
                attempts: 0,
                last_error: None,
                first_seen_ms: now,
                reported_at_ms: None,
                next_attempt_at_ms: now,
                skipped_at_ms: None,
            };
            self.store
                .put_report_period(&row)
                .map_err(ControlPlaneError::from)?;
        }
        Ok(())
    }

    /// Report ONE page of one due period.
    async fn report_row(
        &self,
        period: String,
        mut row: ReportPeriodRow,
        now: i64,
    ) -> Result<TickOutcome, ControlPlaneError> {
        match self
            .adapter
            .report_period(&self.organization, &period, row.next_cursor.as_deref())
            .await
        {
            Ok(outcome) => self.record_page(&period, &mut row, &outcome, now),
            Err(error) => self.record_failure(&period, &mut row, &error, now),
        }
    }

    fn record_page(
        &self,
        period: &str,
        row: &mut ReportPeriodRow,
        outcome: &ReportOutcome,
        now: i64,
    ) -> Result<TickOutcome, ControlPlaneError> {
        match &outcome.next_cursor {
            // Persist the continuation cursor BEFORE the next page is sent:
            // a crash after this point replays page N+1's own key, and a
            // crash before it replays page N's key (both de-duplicated).
            Some(next_cursor) => {
                row.next_cursor = Some(next_cursor.clone());
                row.attempts = 0;
                row.next_attempt_at_ms = now;
                self.store
                    .put_report_period(row)
                    .map_err(ControlPlaneError::from)?;
                Ok(TickOutcome::Continued {
                    period: period.to_string(),
                    next_cursor: next_cursor.clone(),
                })
            }
            None => {
                row.next_cursor = None;
                row.status = ReportPeriodStatus::Reported;
                row.attempts = 0;
                row.last_error = None;
                row.reported_at_ms = Some(now);
                self.store
                    .put_report_period(row)
                    .map_err(ControlPlaneError::from)?;
                Ok(TickOutcome::Reported {
                    period: period.to_string(),
                    attempts: outcome.attempts,
                    idempotent_replay: outcome.idempotent_replay,
                })
            }
        }
    }

    fn record_failure(
        &self,
        period: &str,
        row: &mut ReportPeriodRow,
        error: &ControlPlaneError,
        now: i64,
    ) -> Result<TickOutcome, ControlPlaneError> {
        row.attempts = row.attempts.saturating_add(1);
        let message = bounded_error(error);
        let retryable = matches!(error, ControlPlaneError::Backend(_));
        if retryable && row.attempts < self.policy.max_attempts {
            let next_attempt_at_ms =
                now.saturating_add(backoff_ms(&self.policy, period, row.attempts));
            row.last_error = Some(message.clone());
            row.next_attempt_at_ms = next_attempt_at_ms;
            self.store
                .put_report_period(row)
                .map_err(ControlPlaneError::from)?;
            return Ok(TickOutcome::RetryScheduled {
                period: period.to_string(),
                attempts: row.attempts,
                next_attempt_at_ms,
                error: message,
            });
        }
        row.status = ReportPeriodStatus::Failed;
        row.last_error = Some(message.clone());
        self.store
            .put_report_period(row)
            .map_err(ControlPlaneError::from)?;
        Ok(TickOutcome::Failed {
            period: period.to_string(),
            error: message,
        })
    }

    /// The post-readiness maintenance loop: one bounded tick per interval
    /// (missed ticks are skipped, never replayed in a burst — downtime
    /// crossing is handled by the durable bounded catch-up instead).
    pub async fn run(self: Arc<Self>) {
        let interval_ms = self.policy().interval_ms.max(1) as u64;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match self.tick().await {
                Ok(TickOutcome::Idle) => {}
                Ok(outcome) => tracing::info!("billing report schedule: {outcome:?}"),
                Err(e) => tracing::warn!("billing report schedule tick failed: {e}"),
            }
        }
    }
}

fn bounded_error(error: &ControlPlaneError) -> String {
    bounded_text(&error.to_string())
}

fn bounded_text(message: &str) -> String {
    if message.len() <= MAX_LAST_ERROR_BYTES {
        return message.to_string();
    }
    let mut end = MAX_LAST_ERROR_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}
