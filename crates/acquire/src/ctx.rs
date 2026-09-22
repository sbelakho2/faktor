//! The acquisition context: deadline, cancellation, quota, health and the
//! injected transport.
//!
//! [`AcquireCtx`] is what a connector receives. It owns no HTTP client and no
//! browser: the checked egress transport is injected once, and every request
//! this crate makes goes through it. Deadline and cancellation are consulted
//! before and during every operation; quota and health are shared mutable
//! state behind poison-tolerant locks, so a panic in one caller can never
//! wedge an acquisition path.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::{Clock, Deadline};
use faktor_provider::egress::HttpTransport;

use crate::error::AcquisitionError;
use crate::health::{ConnectorHealth, HealthTracker};
use crate::http::DirectHttpPolicy;
use crate::quota::{QuotaState, QuotaWindow};

/// Lock a mutex, recovering from poisoning: a panic elsewhere must not turn
/// a quota counter into a permanent panic loop.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The effective-unbounded default quota of a context that did not install
/// one: a one-minute window with the maximum limit. Callers with a real
/// documented limit install it with [`AcquireCtx::with_quota`].
pub const DEFAULT_QUOTA_LIMIT: u32 = u32::MAX;

/// Everything one acquisition needs, injected.
#[derive(Clone)]
pub struct AcquireCtx {
    transport: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
    cancellation: CancellationToken,
    deadline_ms: Option<u64>,
    quota: Arc<Mutex<QuotaState>>,
    health: Arc<Mutex<HealthTracker>>,
    policy: DirectHttpPolicy,
}

impl AcquireCtx {
    /// Build a context over an injected transport and clock.
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        clock: Arc<dyn Clock>,
        cancellation: CancellationToken,
    ) -> Self {
        let now_ms = clamp_now(clock.now_ms());
        Self {
            transport,
            clock,
            cancellation,
            deadline_ms: None,
            quota: Arc::new(Mutex::new(QuotaState::new(
                QuotaWindow::per_minute(DEFAULT_QUOTA_LIMIT),
                now_ms,
            ))),
            health: Arc::new(Mutex::new(HealthTracker::default())),
            policy: DirectHttpPolicy::default(),
        }
    }

    /// Install an absolute deadline (Unix milliseconds).
    pub fn with_deadline_ms(mut self, deadline_ms: u64) -> Self {
        self.deadline_ms = Some(deadline_ms);
        self
    }

    /// Install a deadline from a core [`Deadline`].
    pub fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline_ms = Some(deadline.at_ms().max(0) as u64);
        self
    }

    /// Install a quota window.
    pub fn with_quota(mut self, quota: QuotaState) -> Self {
        self.quota = Arc::new(Mutex::new(quota));
        self
    }

    /// Install a health tracker.
    pub fn with_health(mut self, tracker: HealthTracker) -> Self {
        self.health = Arc::new(Mutex::new(tracker));
        self
    }

    /// Install the direct-HTTP bounds.
    pub fn with_policy(mut self, policy: DirectHttpPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The injected checked-egress transport.
    pub fn transport(&self) -> &Arc<dyn HttpTransport> {
        &self.transport
    }

    /// The injected clock.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The current instant (never negative).
    pub fn now_ms(&self) -> u64 {
        clamp_now(self.clock.now_ms())
    }

    /// The cancellation token.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// The absolute deadline, when one was installed.
    pub fn deadline_ms(&self) -> Option<u64> {
        self.deadline_ms
    }

    /// Milliseconds left until the deadline, or `None` when unbounded.
    pub fn remaining_ms(&self) -> Option<u64> {
        self.deadline_ms
            .map(|deadline| deadline.saturating_sub(self.now_ms()))
    }

    /// The direct-HTTP bounds.
    pub fn policy(&self) -> &DirectHttpPolicy {
        &self.policy
    }

    /// Fail fast when the work was cancelled or the deadline expired.
    pub fn check_active(&self) -> Result<(), AcquisitionError> {
        if self.cancellation.is_cancelled() {
            return Err(AcquisitionError::Cancelled);
        }
        if let Some(remaining) = self.remaining_ms() {
            if remaining == 0 {
                return Err(AcquisitionError::Deadline);
            }
        }
        Ok(())
    }

    /// Sleep for `delay_ms`, aborting on cancellation or deadline.
    pub async fn sleep_or_abort(&self, delay_ms: u64) -> Result<(), AcquisitionError> {
        self.check_active()?;
        if delay_ms == 0 {
            return Ok(());
        }
        let sleep = tokio::time::sleep(Duration::from_millis(delay_ms));
        tokio::pin!(sleep);
        match self.deadline_ms {
            Some(deadline) => {
                let remaining = deadline.saturating_sub(self.now_ms()).max(1);
                let until_deadline = tokio::time::sleep(Duration::from_millis(remaining));
                tokio::pin!(until_deadline);
                tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => Err(AcquisitionError::Cancelled),
                    _ = &mut until_deadline => Err(AcquisitionError::Deadline),
                    _ = &mut sleep => Ok(()),
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => Err(AcquisitionError::Cancelled),
                    _ = &mut sleep => Ok(()),
                }
            }
        }
    }

    /// The shared quota state.
    pub fn quota(&self) -> &Arc<Mutex<QuotaState>> {
        &self.quota
    }

    /// Run a closure against the quota state.
    pub fn with_quota_state<R>(&self, op: impl FnOnce(&mut QuotaState) -> R) -> R {
        op(&mut lock(&self.quota))
    }

    /// Consume one quota permit at the current instant.
    pub fn try_acquire_quota(&self) -> Result<(), AcquisitionError> {
        let now_ms = self.now_ms();
        lock(&self.quota).try_acquire(now_ms)
    }

    /// Record an upstream `Retry-After` (milliseconds).
    pub fn record_retry_after(&self, retry_after_ms: u64) {
        let now_ms = self.now_ms();
        lock(&self.quota).record_retry_after(retry_after_ms, now_ms);
    }

    /// The current health snapshot.
    pub fn health(&self) -> ConnectorHealth {
        lock(&self.health).health().clone()
    }

    /// Install a health state explicitly.
    pub fn set_health(&self, health: ConnectorHealth) {
        lock(&self.health).set(health);
    }

    /// Record a failure and update health.
    pub fn report_error(&self, err: &AcquisitionError) {
        let now_ms = self.now_ms();
        lock(&self.health).on_error(err, now_ms);
    }

    /// Record a success and restore health/throughput.
    pub fn report_success(&self) {
        lock(&self.health).on_success();
        lock(&self.quota).record_success();
    }

    /// Record a transient failure against the adaptive throttle.
    pub fn report_transient_throttle(&self) {
        lock(&self.quota).record_transient_failure();
    }
}

fn clamp_now(now_ms: i64) -> u64 {
    now_ms.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::VerificationKind;
    use crate::health::HealthPolicy;
    use faktor_core::time::TestClock;
    use faktor_provider::egress::MockHttpTransport;

    fn ctx_with_clock(clock: Arc<TestClock>) -> AcquireCtx {
        AcquireCtx::new(
            Arc::new(MockHttpTransport::new(200, "{}")),
            clock,
            CancellationToken::new(),
        )
    }

    #[test]
    fn deadline_and_cancellation_fail_fast() {
        let clock = Arc::new(TestClock::new(1_000));
        let ctx = ctx_with_clock(clock.clone()).with_deadline_ms(2_000);
        assert!(ctx.check_active().is_ok());
        assert_eq!(ctx.remaining_ms(), Some(1_000));
        clock.set(2_000);
        assert_eq!(ctx.check_active().unwrap_err(), AcquisitionError::Deadline);

        let ctx = ctx_with_clock(clock.clone());
        ctx.cancellation().cancel();
        assert_eq!(ctx.check_active().unwrap_err(), AcquisitionError::Cancelled);
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_sleep() {
        let clock = Arc::new(TestClock::new(0));
        let ctx = ctx_with_clock(clock);
        let token = ctx.cancellation().clone();
        let sleeper = tokio::spawn(async move { ctx.sleep_or_abort(60_000).await });
        tokio::task::yield_now().await;
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), sleeper)
            .await
            .expect("sleep must abort")
            .unwrap();
        assert_eq!(result.unwrap_err(), AcquisitionError::Cancelled);
    }

    #[test]
    fn health_and_quota_are_shared_and_poison_tolerant() {
        let clock = Arc::new(TestClock::new(0));
        let ctx = ctx_with_clock(clock).with_health(HealthTracker::new(HealthPolicy {
            degrade_threshold: 1,
            cooldown_ms: 10,
        }));
        ctx.report_error(&AcquisitionError::VerificationRequired {
            kind: VerificationKind::Consent,
        });
        assert_eq!(
            ctx.health().verification_kind(),
            Some(VerificationKind::Consent)
        );
        ctx.set_health(ConnectorHealth::Healthy);
        ctx.report_success();
        assert_eq!(ctx.health(), ConnectorHealth::Healthy);

        // A panic while holding the quota lock must not wedge the context.
        let quota = ctx.quota().clone();
        let poisoned = std::thread::spawn(move || {
            let _guard = lock(&quota);
            panic!("simulated holder crash");
        });
        assert!(poisoned.join().is_err());
        assert!(ctx.try_acquire_quota().is_ok());
    }
}
