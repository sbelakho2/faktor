//! The periodic installation/repository reconcile timer.
//!
//! Webhook-driven syncs converge on the same idempotent upserts, but a
//! missed or dropped webhook (provider outage, delivery gap, expired
//! token) would otherwise leave the durable rows stale forever. This module
//! adds a BOUNDED background reconcile timer on top of [`ScmSync`]:
//!
//! - **Single-flight.** Every sync this runner performs — timer passes and
//!   webhook-triggered passes alike — is gated by ONE in-flight slot. A
//!   caller that arrives while another sync runs COALESCES onto the leader:
//!   it awaits the same durable result instead of starting a parallel
//!   provider pass, so overlapping triggers can never multiply provider
//!   calls (and the upserts converge either way).
//! - **Bounded cadence.** The next attempt is `interval + deterministic
//!   jitter` (bounded by [`ReconcilePolicy::jitter_ms`]), so a fleet does
//!   not reconcile in lockstep. A failure schedules an exponential backoff
//!   capped at [`ReconcilePolicy::max_backoff_ms`]; the schedule never
//!   spins and never sleeps forever.
//! - **Typed journaling.** Every transition (started / completed /
//!   coalesced / failed with the stable [`ScmError::code`] and the next
//!   attempt instant) is appended to a BOUNDED ring buffer and mirrored to
//!   `tracing`, so a failing reconcile is diagnosable without unbounded
//!   state.
//! - **Disabled parity.** When no policy is configured the host builds no
//!   runner at all: the pre-existing webhook-only behavior is unchanged.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::error::ScmError;
use crate::github::Clock;
use crate::ids::ScmInstallationId;
use crate::sync::{ScmSync, SyncReport};

/// The shortest accepted reconcile interval.
pub const MIN_RECONCILE_INTERVAL_MS: i64 = 1_000;
/// The longest accepted reconcile interval (24h).
pub const MAX_RECONCILE_INTERVAL_MS: i64 = 86_400_000;
/// Hard bound on the configured cadence jitter (60s).
pub const MAX_RECONCILE_JITTER_MS: i64 = 60_000;
/// Hard ceiling on one failure backoff (24h).
pub const MAX_RECONCILE_BACKOFF_MS: i64 = 86_400_000;
/// The default reconcile interval (5min).
pub const DEFAULT_RECONCILE_INTERVAL_MS: i64 = 300_000;
/// The default cadence jitter (30s).
pub const DEFAULT_RECONCILE_JITTER_MS: i64 = 30_000;
/// The default failure-backoff ceiling (1h).
pub const DEFAULT_RECONCILE_MAX_BACKOFF_MS: i64 = 3_600_000;
/// Bound on the in-memory journal entries retained per runner.
pub const MAX_RECONCILE_JOURNAL: usize = 64;
/// Bound on one recorded journal error text.
pub const MAX_RECONCILE_ERROR_BYTES: usize = 512;
/// Domain separation of the deterministic cadence jitter.
const JITTER_DOMAIN: &[u8] = b"faktor-scm-reconcile-jitter:v1:";

/// The strict reconcile policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcilePolicy {
    /// Base cadence between two passes (`1s..=24h`).
    pub interval_ms: i64,
    /// Uniform bound of the per-pass cadence jitter (`0..=60s`, also
    /// capped by the interval).
    pub jitter_ms: i64,
    /// Ceiling of the exponential failure backoff (`interval..=24h`).
    pub max_backoff_ms: i64,
}

impl Default for ReconcilePolicy {
    fn default() -> Self {
        Self {
            interval_ms: DEFAULT_RECONCILE_INTERVAL_MS,
            jitter_ms: DEFAULT_RECONCILE_JITTER_MS,
            max_backoff_ms: DEFAULT_RECONCILE_MAX_BACKOFF_MS,
        }
    }
}

impl ReconcilePolicy {
    /// Strict validation; every bound is enforced before a timer exists.
    pub fn validate(&self) -> Result<(), ScmError> {
        if !(MIN_RECONCILE_INTERVAL_MS..=MAX_RECONCILE_INTERVAL_MS).contains(&self.interval_ms) {
            return Err(ScmError::Config(format!(
                "reconcile interval_ms must be {MIN_RECONCILE_INTERVAL_MS}..={MAX_RECONCILE_INTERVAL_MS}"
            )));
        }
        if self.jitter_ms < 0
            || self.jitter_ms > MAX_RECONCILE_JITTER_MS
            || self.jitter_ms > self.interval_ms
        {
            return Err(ScmError::Config(format!(
                "reconcile jitter_ms must be 0..={MAX_RECONCILE_JITTER_MS} and <= interval_ms"
            )));
        }
        if self.max_backoff_ms < self.interval_ms || self.max_backoff_ms > MAX_RECONCILE_BACKOFF_MS
        {
            return Err(ScmError::Config(format!(
                "reconcile max_backoff_ms must be interval_ms..={MAX_RECONCILE_BACKOFF_MS}"
            )));
        }
        Ok(())
    }
}

/// What one scheduled pass planned to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileKind {
    All,
    Installation(ScmInstallationId),
}

/// One typed journal entry (bounded; stable fields only, never a token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileEvent {
    /// The leader started a sync pass.
    Started { sequence: u64, kind: ReconcileKind },
    /// The leader's pass completed; `coalesced` counts the callers that
    /// shared this result instead of running their own pass.
    Completed {
        sequence: u64,
        report: SyncReport,
        coalesced: usize,
    },
    /// A caller coalesced onto an in-flight pass.
    Coalesced { sequence: u64 },
    /// The pass failed typed; the next attempt is scheduled after a bounded
    /// backoff.
    Failed {
        sequence: u64,
        code: &'static str,
        error: String,
        consecutive_failures: u32,
        next_attempt_at_ms: i64,
    },
}

impl ReconcileEvent {
    /// The stable machine code of one journal entry.
    pub const fn code(&self) -> &'static str {
        match self {
            ReconcileEvent::Started { .. } => "scm_reconcile_started",
            ReconcileEvent::Completed { .. } => "scm_reconcile_completed",
            ReconcileEvent::Coalesced { .. } => "scm_reconcile_coalesced",
            ReconcileEvent::Failed { .. } => "scm_reconcile_failed",
        }
    }
}

struct ReconcileState {
    sequence: u64,
    consecutive_failures: u32,
    next_attempt_at_ms: i64,
    journal: VecDeque<ReconcileEvent>,
}

/// The in-flight pass every caller shares (single-flight).
struct Flight {
    done: tokio::sync::Notify,
    result: Mutex<Option<Result<SyncReport, ScmError>>>,
}

/// The bounded reconcile runner over one [`ScmSync`].
pub struct ScmReconcile {
    sync: Arc<ScmSync>,
    organization: String,
    policy: ReconcilePolicy,
    clock: Arc<dyn Clock>,
    state: Mutex<ReconcileState>,
    flight: Mutex<Option<Arc<Flight>>>,
}

impl std::fmt::Debug for ScmReconcile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScmReconcile")
            .field("organization", &self.organization)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl ScmReconcile {
    /// Build the runner with a validated policy. The first pass is scheduled
    /// one jittered interval after construction (the host runs its initial
    /// sync itself).
    pub fn new(
        sync: Arc<ScmSync>,
        organization: impl Into<String>,
        policy: ReconcilePolicy,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ScmError> {
        policy.validate()?;
        let organization = organization.into();
        if organization.is_empty() || organization.len() > 256 {
            return Err(ScmError::InvalidInput(
                "organization id must be 1..=256 bytes".into(),
            ));
        }
        let now = clock.now_ms();
        let next_attempt_at_ms = now.saturating_add(jittered_delay_ms(&policy, 0));
        Ok(Self {
            sync,
            organization,
            policy,
            clock,
            state: Mutex::new(ReconcileState {
                sequence: 0,
                consecutive_failures: 0,
                next_attempt_at_ms,
                journal: VecDeque::new(),
            }),
            flight: Mutex::new(None),
        })
    }

    pub fn policy(&self) -> &ReconcilePolicy {
        &self.policy
    }

    pub fn organization(&self) -> &str {
        &self.organization
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ReconcileState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_flight(&self) -> std::sync::MutexGuard<'_, Option<Arc<Flight>>> {
        self.flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn push_journal(&self, state: &mut ReconcileState, event: ReconcileEvent) {
        if state.journal.len() >= MAX_RECONCILE_JOURNAL {
            state.journal.pop_front();
        }
        state.journal.push_back(event);
    }

    /// The bounded journal snapshot (oldest first).
    pub fn journal(&self) -> Vec<ReconcileEvent> {
        self.lock_state().journal.iter().cloned().collect()
    }

    /// Milliseconds until the scheduled next attempt (`>= 0`).
    pub fn scheduled_delay_ms(&self) -> i64 {
        let next = self.lock_state().next_attempt_at_ms;
        next.saturating_sub(self.clock.now_ms()).max(0)
    }

    /// The instant the next attempt is scheduled for.
    pub fn next_attempt_at_ms(&self) -> i64 {
        self.lock_state().next_attempt_at_ms
    }

    /// One reconcile pass of the whole app (single-flight; coalesced callers
    /// share the leader's result).
    pub async fn sync_all(&self) -> Result<SyncReport, ScmError> {
        self.run_pass(ReconcileKind::All).await
    }

    /// One reconcile pass of a single installation (webhook path), sharing
    /// the SAME single-flight slot as [`Self::sync_all`].
    pub async fn sync_installation(
        &self,
        installation: ScmInstallationId,
    ) -> Result<SyncReport, ScmError> {
        self.run_pass(ReconcileKind::Installation(installation))
            .await
    }

    /// Sleep until the scheduled next attempt (recomputed each call, so a
    /// failure's backoff applies to the next sleep).
    pub async fn wait_next(&self) {
        let delay = self.scheduled_delay_ms().max(1) as u64;
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }

    /// The post-readiness timer loop: sleep to the scheduled instant, then
    /// run one single-flight pass. Failures are journaled, never fatal.
    pub async fn run(self: Arc<Self>) {
        loop {
            self.wait_next().await;
            let outcome = self.sync_all().await;
            if let Err(error) = outcome {
                tracing::warn!(
                    code = error.code(),
                    "scm reconcile pass failed (backoff scheduled): {error}"
                );
            }
        }
    }

    async fn run_pass(&self, kind: ReconcileKind) -> Result<SyncReport, ScmError> {
        // Single-flight: claim the in-flight slot or coalesce onto it.
        let (flight, leader) = {
            let mut slot = self.lock_flight();
            match &*slot {
                Some(flight) => (flight.clone(), false),
                None => {
                    let flight = Arc::new(Flight {
                        done: tokio::sync::Notify::new(),
                        result: Mutex::new(None),
                    });
                    *slot = Some(flight.clone());
                    (flight, true)
                }
            }
        };
        if !leader {
            let sequence = {
                let mut state = self.lock_state();
                let sequence = state.sequence;
                self.push_journal(&mut state, ReconcileEvent::Coalesced { sequence });
                sequence
            };
            tracing::debug!(
                sequence,
                "scm reconcile pass coalesced onto the in-flight sync"
            );
            return await_flight(&flight).await;
        }
        let sequence = {
            let mut state = self.lock_state();
            state.sequence = state.sequence.saturating_add(1);
            let sequence = state.sequence;
            self.push_journal(&mut state, ReconcileEvent::Started { sequence, kind });
            sequence
        };
        let outcome = match kind {
            ReconcileKind::All => self.sync.sync_all(&self.organization).await,
            ReconcileKind::Installation(installation) => {
                self.sync
                    .sync_installation(&self.organization, installation)
                    .await
            }
        };
        self.finish(flight, sequence, outcome)
    }

    fn finish(
        &self,
        flight: Arc<Flight>,
        sequence: u64,
        outcome: Result<SyncReport, ScmError>,
    ) -> Result<SyncReport, ScmError> {
        let now = self.clock.now_ms();
        {
            let mut state = self.lock_state();
            match &outcome {
                Ok(report) => {
                    state.consecutive_failures = 0;
                    state.next_attempt_at_ms =
                        now.saturating_add(jittered_delay_ms(&self.policy, state.sequence));
                    self.push_journal(
                        &mut state,
                        ReconcileEvent::Completed {
                            sequence,
                            report: report.clone(),
                            coalesced: 0,
                        },
                    );
                }
                Err(error) => {
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                    let backoff = failure_backoff_ms(&self.policy, state.consecutive_failures);
                    state.next_attempt_at_ms = now.saturating_add(backoff);
                    let event = ReconcileEvent::Failed {
                        sequence,
                        code: error.code(),
                        error: bounded_error(error),
                        consecutive_failures: state.consecutive_failures,
                        next_attempt_at_ms: state.next_attempt_at_ms,
                    };
                    self.push_journal(&mut state, event);
                }
            }
        }
        // Publish + release the in-flight slot BEFORE waking waiters, so a
        // waiter that arrives after the release observes the completed
        // result rather than a dangling flight.
        *flight.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome.clone());
        *self.lock_flight() = None;
        flight.done.notify_waiters();
        outcome
    }
}

async fn await_flight(flight: &Arc<Flight>) -> Result<SyncReport, ScmError> {
    loop {
        // Register the waiter BEFORE inspecting the result: a notification
        // that races the result store can never be lost.
        let notified = flight.done.notified();
        if let Some(result) = flight
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return result;
        }
        notified.await;
    }
}

/// `interval + deterministic jitter` in `[interval, interval + jitter_ms]`.
/// The jitter is a hash of the policy and the scheduling sequence: stable
/// for tests, uncorrelated across passes and replicas.
pub fn jittered_delay_ms(policy: &ReconcilePolicy, sequence: u64) -> i64 {
    let interval = policy.interval_ms.max(0);
    let jitter_bound = policy.jitter_ms.max(0);
    if jitter_bound == 0 {
        return interval;
    }
    let jitter = hash(&[&policy.interval_ms.to_le_bytes(), &sequence.to_le_bytes()])
        % (jitter_bound as u64 + 1);
    interval.saturating_add(jitter as i64)
}

/// Bounded exponential failure backoff: `interval * 2^(failures-1)` plus
/// jitter, clamped to `max_backoff_ms`. Never zero, never unbounded.
pub fn failure_backoff_ms(policy: &ReconcilePolicy, consecutive_failures: u32) -> i64 {
    let shift = consecutive_failures.saturating_sub(1).min(16);
    let base = policy
        .interval_ms
        .saturating_mul(1i64 << shift)
        .clamp(policy.interval_ms.max(0), policy.max_backoff_ms.max(0));
    let jitter_bound = policy.jitter_ms.max(0);
    let jitter = if jitter_bound == 0 {
        0
    } else {
        (hash(&[
            &policy.interval_ms.to_le_bytes(),
            &consecutive_failures.to_le_bytes(),
            b"backoff",
        ]) % (jitter_bound as u64 + 1)) as i64
    };
    base.saturating_add(jitter)
        .clamp(policy.interval_ms.max(0), policy.max_backoff_ms.max(0))
}

fn hash(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in JITTER_DOMAIN
        .iter()
        .copied()
        .chain(parts.iter().flat_map(|part| part.iter().copied()))
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn bounded_error(error: &ScmError) -> String {
    let message = error.to_string();
    if message.len() <= MAX_RECONCILE_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_RECONCILE_ERROR_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{IssueRef, PullRequestRef, RemoteRef, RepositoryRef};
    use crate::provider::{
        BranchSpec, CommentTarget, PullRequestSpec, ScmBranch, ScmComment, ScmInstallation,
        ScmIssue, ScmProvider, ScmPullRequest, ScmRemoteRef, ScmRepository, ScmReviewEvent,
    };
    use crate::store::{MemoryScmStore, ScmStore};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A provider whose `list_installations` optionally blocks until
    /// released and that counts every provider entry (the single-flight
    /// observable).
    struct SlowProvider {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        calls: AtomicUsize,
        fail: AtomicUsize,
        block: AtomicUsize,
    }

    impl SlowProvider {
        fn new() -> Self {
            Self {
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                calls: AtomicUsize::new(0),
                fail: AtomicUsize::new(0),
                block: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ScmProvider for SlowProvider {
        fn provider_name(&self) -> &'static str {
            "slow"
        }
        async fn repository(&self, _: &RepositoryRef) -> Result<ScmRepository, ScmError> {
            Err(ScmError::NotFound("slow".into()))
        }
        async fn issue(&self, _: &IssueRef) -> Result<ScmIssue, ScmError> {
            Err(ScmError::NotFound("slow".into()))
        }
        async fn remote_ref(&self, _: &RemoteRef) -> Result<Option<ScmRemoteRef>, ScmError> {
            Ok(None)
        }
        async fn create_or_reconcile_branch(
            &self,
            _: &crate::ids::ExternalOperationId,
            _: &BranchSpec,
        ) -> Result<ScmBranch, ScmError> {
            Err(ScmError::Forbidden("slow".into()))
        }
        async fn create_or_reconcile_pull_request(
            &self,
            _: &crate::ids::ExternalOperationId,
            _: &PullRequestSpec,
        ) -> Result<ScmPullRequest, ScmError> {
            Err(ScmError::Forbidden("slow".into()))
        }
        async fn comment(
            &self,
            _: &crate::ids::ExternalOperationId,
            _: &CommentTarget,
            _: &str,
        ) -> Result<ScmComment, ScmError> {
            Err(ScmError::Forbidden("slow".into()))
        }
        async fn review_events(&self, _: &PullRequestRef) -> Result<Vec<ScmReviewEvent>, ScmError> {
            Ok(Vec::new())
        }
        async fn list_installations(&self) -> Result<Vec<ScmInstallation>, ScmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_waiters();
            if self.fail.load(Ordering::SeqCst) > 0 {
                return Err(ScmError::Transport("provider down".into()));
            }
            if self.block.load(Ordering::SeqCst) > 0 {
                self.release.notified().await;
            }
            Ok(Vec::new())
        }
        async fn list_repositories(
            &self,
            _: ScmInstallationId,
        ) -> Result<Vec<ScmRepository>, ScmError> {
            Ok(Vec::new())
        }
    }

    fn runner(
        provider: Arc<SlowProvider>,
        clock: Arc<dyn Clock>,
        policy: ReconcilePolicy,
    ) -> Arc<ScmReconcile> {
        let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
        let sync = Arc::new(ScmSync::new(provider, store, clock.clone()));
        Arc::new(ScmReconcile::new(sync, "org:alpha", policy, clock).unwrap())
    }

    /// Concurrent triggers share ONE provider pass (single-flight): the
    /// waiter observes the leader's exact result.
    #[tokio::test]
    async fn overlapping_reconciles_coalesce_onto_the_in_flight_sync() {
        let provider = Arc::new(SlowProvider::new());
        provider.block.store(1, Ordering::SeqCst);
        let clock: Arc<dyn Clock> = Arc::new(crate::github::ManualClock::new(1_000));
        let reconcile = runner(provider.clone(), clock, ReconcilePolicy::default());
        // Release the provider from the test thread once the waiter joined.
        let entered = provider.entered.notified();
        tokio::pin!(entered);
        let leader_reconcile = reconcile.clone();
        let leader = tokio::spawn(async move { leader_reconcile.sync_all().await });
        entered.await;
        let waiter_reconcile = reconcile.clone();
        let waiter = tokio::spawn(async move { waiter_reconcile.sync_all().await });
        // Let the waiter claim/coalesce before the leader completes.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        provider.release.notify_waiters();
        let leader_report = leader.await.unwrap().unwrap();
        let waiter_report = waiter.await.unwrap().unwrap();
        assert_eq!(leader_report, waiter_report);
        assert_eq!(
            provider.calls(),
            1,
            "a coalesced caller must not start a second provider pass"
        );
        let journal = reconcile.journal();
        assert!(journal
            .iter()
            .any(|event| event.code() == "scm_reconcile_coalesced"));
        assert!(journal
            .iter()
            .any(|event| event.code() == "scm_reconcile_started"));
        assert!(journal
            .iter()
            .any(|event| event.code() == "scm_reconcile_completed"));
    }

    /// The timer fires at the configured interval (time-controlled) and not
    /// a moment earlier.
    #[tokio::test(start_paused = true)]
    async fn timer_triggers_a_sync_at_the_configured_interval() {
        let provider = Arc::new(SlowProvider::new());
        let clock: Arc<dyn Clock> = Arc::new(crate::github::SystemClock);
        let policy = ReconcilePolicy {
            interval_ms: 60_000,
            jitter_ms: 0,
            max_backoff_ms: 600_000,
        };
        let reconcile = runner(provider.clone(), clock, policy);
        let timer = tokio::spawn(reconcile.clone().run());
        tokio::task::yield_now().await;
        assert_eq!(provider.calls(), 0, "nothing may run before the interval");
        tokio::time::advance(Duration::from_millis(59_000)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(provider.calls(), 0, "the interval has not elapsed");
        tokio::time::advance(Duration::from_millis(2_000)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(provider.calls(), 1, "one pass at the configured interval");
        tokio::time::advance(Duration::from_millis(60_000)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(provider.calls(), 2, "the cadence continues");
        timer.abort();
    }

    /// Failures schedule a bounded, strictly growing backoff and journal the
    /// typed cause; the schedule never exceeds `max_backoff_ms`.
    #[tokio::test]
    async fn failures_back_off_bounded_with_typed_journaling() {
        let provider = Arc::new(SlowProvider::new());
        provider.fail.store(1, Ordering::SeqCst);
        let clock = Arc::new(crate::github::ManualClock::new(1_000));
        let policy = ReconcilePolicy {
            interval_ms: 10_000,
            jitter_ms: 0,
            max_backoff_ms: 40_000,
        };
        let reconcile = runner(provider.clone(), clock.clone(), policy);
        let mut previous = 0i64;
        for round in 1..=8u32 {
            let error = reconcile.sync_all().await.unwrap_err();
            assert_eq!(error.code(), "scm_transport");
            let delay = reconcile.scheduled_delay_ms();
            assert!(
                delay >= previous || round == 1,
                "{round}: {delay} < {previous}"
            );
            assert!(
                delay <= policy.max_backoff_ms,
                "{round}: {delay} exceeds the bound"
            );
            previous = delay;
            clock.advance(delay);
        }
        // The last backoff hit the ceiling: it is bounded, not exponential
        // without limit.
        assert_eq!(previous, policy.max_backoff_ms);
        let failures: Vec<u32> = reconcile
            .journal()
            .iter()
            .filter_map(|event| match event {
                ReconcileEvent::Failed {
                    code,
                    consecutive_failures,
                    ..
                } => {
                    assert_eq!(*code, "scm_transport");
                    Some(*consecutive_failures)
                }
                _ => None,
            })
            .collect();
        assert_eq!(failures, (1..=8).collect::<Vec<_>>());
    }

    /// A success resets the failure state; later failures back off from the
    /// base again (no compounding across healthy runs).
    #[tokio::test]
    async fn success_resets_the_failure_backoff() {
        let provider = Arc::new(SlowProvider::new());
        provider.fail.store(1, Ordering::SeqCst);
        let clock = Arc::new(crate::github::ManualClock::new(1_000));
        let policy = ReconcilePolicy {
            interval_ms: 10_000,
            jitter_ms: 0,
            max_backoff_ms: 40_000,
        };
        let reconcile = runner(provider.clone(), clock.clone(), policy);
        reconcile.sync_all().await.unwrap_err();
        assert_eq!(reconcile.scheduled_delay_ms(), 10_000);
        provider.fail.store(0, Ordering::SeqCst);
        provider.release.notify_waiters();
        reconcile.sync_all().await.unwrap();
        assert_eq!(reconcile.scheduled_delay_ms(), 10_000);
    }

    #[test]
    fn cadence_jitter_and_backoff_bounds_are_enforced() {
        let policy = ReconcilePolicy {
            interval_ms: 60_000,
            jitter_ms: 5_000,
            max_backoff_ms: 600_000,
        };
        policy.validate().unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for sequence in 0..64u64 {
            let delay = jittered_delay_ms(&policy, sequence);
            assert!((60_000..=65_000).contains(&delay), "{sequence}: {delay}");
            seen.insert(delay);
        }
        assert!(seen.len() > 1, "jitter must not be constant");
        for failures in 1..12u32 {
            let backoff = failure_backoff_ms(&policy, failures);
            assert!(
                (60_000..=600_000).contains(&backoff),
                "{failures}: {backoff}"
            );
        }
        // Hostile policies are refused, never clamped silently.
        assert!(ReconcilePolicy {
            interval_ms: 999,
            ..policy
        }
        .validate()
        .is_err());
        assert!(ReconcilePolicy {
            jitter_ms: 60_001,
            ..policy
        }
        .validate()
        .is_err());
        assert!(ReconcilePolicy {
            max_backoff_ms: 59_999,
            ..policy
        }
        .validate()
        .is_err());
        assert!(ReconcilePolicy {
            interval_ms: 1_000,
            jitter_ms: 2_000,
            max_backoff_ms: 1_000,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn journal_is_bounded() {
        let provider = Arc::new(SlowProvider::new());
        let clock: Arc<dyn Clock> = Arc::new(crate::github::ManualClock::new(0));
        let reconcile = runner(provider, clock, ReconcilePolicy::default());
        for _ in 0..(MAX_RECONCILE_JOURNAL + 32) {
            let mut state = reconcile.lock_state();
            let sequence = state.sequence;
            reconcile.push_journal(&mut state, ReconcileEvent::Coalesced { sequence });
        }
        assert_eq!(reconcile.journal().len(), MAX_RECONCILE_JOURNAL);
    }
}
