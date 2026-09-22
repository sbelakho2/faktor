//! Connector health, health tracking, and per-field extraction health.
//!
//! Two orthogonal records:
//!
//! * [`ConnectorHealth`] — the planner-facing state of one acquisition path
//!   (healthy, degraded, cooling down, rate limited, authentication or
//!   verification required, quota exhausted, unavailable). Time-based states
//!   recover on their own once their timestamp passes; the others must be
//!   cleared explicitly.
//! * [`ExtractionHealth`] — per (mechanism, field) outcome memory. Strategies
//!   report success / not-applicable / schema-mismatch / conflict and the
//!   runtime remembers which strategy works, without any model. This is the
//!   self-optimization of `docs/acquire.md` §8.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{AcquisitionError, VerificationKind};
use crate::mechanism::{AcquisitionMechanism, MECHANISM_PRECEDENCE};
use crate::request::RequestedField;

/// The planner-facing state of one acquisition path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "health", rename_all = "snake_case")]
pub enum ConnectorHealth {
    /// Fully usable.
    Healthy,
    /// Usable, but recent failures; the plan is flagged.
    Degraded {
        /// Why.
        reason: String,
    },
    /// Cooling down after transient failures until `until_ms`.
    CoolingDown {
        /// Until when.
        until_ms: u64,
    },
    /// Rate limited until `until_ms`.
    RateLimited {
        /// Until when.
        until_ms: u64,
    },
    /// Credentials must be supplied by a human.
    AuthenticationRequired,
    /// A human verification step is required.
    VerificationRequired {
        /// The kind of verification.
        kind: VerificationKind,
    },
    /// The quota window is exhausted until `reset_ms`.
    QuotaExhausted {
        /// When the window resets.
        reset_ms: u64,
    },
    /// Not usable until an operator fixes it.
    Unavailable,
}

impl ConnectorHealth {
    /// The stable wire label.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded { .. } => "degraded",
            Self::CoolingDown { .. } => "cooling_down",
            Self::RateLimited { .. } => "rate_limited",
            Self::AuthenticationRequired => "authentication_required",
            Self::VerificationRequired { .. } => "verification_required",
            Self::QuotaExhausted { .. } => "quota_exhausted",
            Self::Unavailable => "unavailable",
        }
    }

    /// Whether the path may be used at `now_ms`. Time-based states recover
    /// on their own; `AuthenticationRequired`, `VerificationRequired` and
    /// `Unavailable` stay unusable until explicitly cleared.
    pub fn usable_at(&self, now_ms: u64) -> bool {
        match self {
            Self::Healthy | Self::Degraded { .. } => true,
            Self::CoolingDown { until_ms } | Self::RateLimited { until_ms } => now_ms >= *until_ms,
            Self::QuotaExhausted { reset_ms } => now_ms >= *reset_ms,
            Self::AuthenticationRequired
            | Self::VerificationRequired { .. }
            | Self::Unavailable => false,
        }
    }

    /// Whether the path is degraded (usable but flagged).
    pub const fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded { .. })
    }

    /// The quota reset time, when this state is a quota exhaustion.
    pub const fn quota_reset_ms(&self) -> Option<u64> {
        match self {
            Self::QuotaExhausted { reset_ms } => Some(*reset_ms),
            _ => None,
        }
    }

    /// The verification kind, when this state demands verification.
    pub const fn verification_kind(&self) -> Option<VerificationKind> {
        match self {
            Self::VerificationRequired { kind } => Some(*kind),
            _ => None,
        }
    }
}

/// When a path is declared cooling down, and after how many consecutive
/// transient failures it degrades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthPolicy {
    /// Consecutive transient failures before `Degraded` becomes
    /// `CoolingDown`.
    pub degrade_threshold: u32,
    /// How long a cooldown lasts.
    pub cooldown_ms: u64,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            degrade_threshold: 3,
            cooldown_ms: 30_000,
        }
    }
}

/// A mutable health tracker: the one place that maps errors onto health and
/// restores throughput on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthTracker {
    /// The current state.
    pub health: ConnectorHealth,
    /// Consecutive transient failures.
    pub consecutive_transient: u32,
    /// The policy.
    pub policy: HealthPolicy,
}

impl Default for HealthTracker {
    fn default() -> Self {
        Self::new(HealthPolicy::default())
    }
}

impl HealthTracker {
    /// A healthy tracker with the given policy.
    pub fn new(policy: HealthPolicy) -> Self {
        Self {
            health: ConnectorHealth::Healthy,
            consecutive_transient: 0,
            policy,
        }
    }

    /// The current health snapshot.
    pub fn health(&self) -> &ConnectorHealth {
        &self.health
    }

    /// Explicitly install a state (operator/administrative override).
    pub fn set(&mut self, health: ConnectorHealth) {
        self.health = health;
    }

    /// Success restores throughput: the counter resets and a time-based or
    /// degraded state clears to `Healthy`.
    pub fn on_success(&mut self) {
        self.consecutive_transient = 0;
        if matches!(
            self.health,
            ConnectorHealth::Degraded { .. }
                | ConnectorHealth::CoolingDown { .. }
                | ConnectorHealth::RateLimited { .. }
        ) {
            self.health = ConnectorHealth::Healthy;
        }
    }

    /// Map a typed failure onto health.
    pub fn on_error(&mut self, err: &AcquisitionError, now_ms: u64) {
        match err {
            AcquisitionError::NetworkTimeout | AcquisitionError::ApiUnavailable => {
                self.consecutive_transient = self.consecutive_transient.saturating_add(1);
                if self.consecutive_transient >= self.policy.degrade_threshold {
                    self.health = ConnectorHealth::CoolingDown {
                        until_ms: now_ms.saturating_add(self.policy.cooldown_ms),
                    };
                } else {
                    self.health = ConnectorHealth::Degraded {
                        reason: err.as_str().to_string(),
                    };
                }
            }
            AcquisitionError::BrowserCrashed => {
                self.consecutive_transient = self.consecutive_transient.saturating_add(1);
                self.health = ConnectorHealth::Degraded {
                    reason: err.as_str().to_string(),
                };
            }
            AcquisitionError::RateLimited { retry_after_ms } => {
                self.health = ConnectorHealth::RateLimited {
                    until_ms: now_ms.saturating_add(*retry_after_ms),
                };
            }
            AcquisitionError::QuotaExhausted { reset_ms } => {
                self.health = ConnectorHealth::QuotaExhausted {
                    reset_ms: *reset_ms,
                };
            }
            AcquisitionError::AuthenticationRequired => {
                self.health = ConnectorHealth::AuthenticationRequired;
            }
            AcquisitionError::VerificationRequired { kind } => {
                self.health = ConnectorHealth::VerificationRequired { kind: *kind };
            }
            AcquisitionError::BrowserUnavailable => {
                self.health = ConnectorHealth::Unavailable;
            }
            _ => {
                self.health = ConnectorHealth::Degraded {
                    reason: err.as_str().to_string(),
                };
            }
        }
    }
}

/// What one extraction strategy did on one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionOutcome {
    /// The field was extracted.
    Success,
    /// The strategy does not apply to this field/page.
    NotApplicable,
    /// The structure did not match the expected schema (drift).
    SchemaMismatch,
    /// Extractors produced conflicting values.
    Conflict,
}

/// The remembered record of one strategy on one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyRecord {
    /// Successful extractions.
    pub successes: u32,
    /// Not-applicable reports.
    pub not_applicable: u32,
    /// Schema mismatches.
    pub schema_mismatches: u32,
    /// Conflicts.
    pub conflicts: u32,
    /// The most recent outcome.
    pub last_outcome: ExtractionOutcome,
    /// When the record was last updated.
    pub last_observed_ms: u64,
}

impl StrategyRecord {
    /// The documented score: successes weigh 3, schema mismatches -2,
    /// conflicts -1, not-applicable 0.
    pub const fn score(&self) -> i64 {
        self.successes as i64 * 3 - self.schema_mismatches as i64 * 2 - self.conflicts as i64
    }

    /// A strategy proven bad for this field: it produced mismatches or
    /// conflicts and never a success.
    pub const fn is_proven_bad(&self) -> bool {
        self.successes == 0 && (self.schema_mismatches > 0 || self.conflicts > 0)
    }
}

/// Per (mechanism, field) extraction outcome memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionHealth {
    records: BTreeMap<AcquisitionMechanism, BTreeMap<RequestedField, StrategyRecord>>,
}

impl ExtractionHealth {
    /// No records.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one outcome.
    pub fn record(
        &mut self,
        mechanism: AcquisitionMechanism,
        field: RequestedField,
        outcome: ExtractionOutcome,
        now_ms: u64,
    ) {
        let record = self
            .records
            .entry(mechanism)
            .or_default()
            .entry(field)
            .or_insert(StrategyRecord {
                successes: 0,
                not_applicable: 0,
                schema_mismatches: 0,
                conflicts: 0,
                last_outcome: outcome,
                last_observed_ms: now_ms,
            });
        match outcome {
            ExtractionOutcome::Success => record.successes = record.successes.saturating_add(1),
            ExtractionOutcome::NotApplicable => {
                record.not_applicable = record.not_applicable.saturating_add(1)
            }
            ExtractionOutcome::SchemaMismatch => {
                record.schema_mismatches = record.schema_mismatches.saturating_add(1)
            }
            ExtractionOutcome::Conflict => record.conflicts = record.conflicts.saturating_add(1),
        }
        record.last_outcome = outcome;
        record.last_observed_ms = now_ms;
    }

    /// The record of one strategy on one field.
    pub fn record_of(
        &self,
        mechanism: AcquisitionMechanism,
        field: RequestedField,
    ) -> Option<&StrategyRecord> {
        self.records.get(&mechanism)?.get(&field)
    }

    /// The score of one strategy on one field (0 when unknown).
    pub fn score(&self, mechanism: AcquisitionMechanism, field: RequestedField) -> i64 {
        self.record_of(mechanism, field)
            .map(StrategyRecord::score)
            .unwrap_or(0)
    }

    /// Whether a strategy is proven bad for every one of the requested
    /// fields (used by the planner to skip a mechanism).
    pub fn proven_bad_for_all(
        &self,
        mechanism: AcquisitionMechanism,
        fields: &[RequestedField],
    ) -> bool {
        !fields.is_empty()
            && fields.iter().all(|field| {
                self.record_of(mechanism, *field)
                    .is_some_and(StrategyRecord::is_proven_bad)
            })
    }

    /// The remembered best strategy for a field: highest score (> 0), ties
    /// broken by the documented mechanism precedence. Deterministic.
    pub fn best_strategy(&self, field: RequestedField) -> Option<AcquisitionMechanism> {
        let mut best: Option<(i64, AcquisitionMechanism)> = None;
        for mechanism in MECHANISM_PRECEDENCE {
            let score = self.score(mechanism, field);
            if score <= 0 {
                continue;
            }
            match best {
                Some((best_score, _)) if best_score >= score => {}
                _ => best = Some((score, mechanism)),
            }
        }
        best.map(|(_, mechanism)| mechanism)
    }

    /// Strategies ordered by descending score, ties by precedence. Only
    /// strategies with a positive score are returned.
    pub fn ranked_strategies(&self, field: RequestedField) -> Vec<AcquisitionMechanism> {
        let mut ranked: Vec<(i64, AcquisitionMechanism)> = MECHANISM_PRECEDENCE
            .into_iter()
            .map(|mechanism| (self.score(mechanism, field), mechanism))
            .filter(|(score, _)| *score > 0)
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        ranked.into_iter().map(|(_, mechanism)| mechanism).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_based_health_recovers_and_human_states_do_not() {
        assert!(!ConnectorHealth::CoolingDown { until_ms: 100 }.usable_at(99));
        assert!(ConnectorHealth::CoolingDown { until_ms: 100 }.usable_at(100));
        assert!(!ConnectorHealth::QuotaExhausted { reset_ms: 5 }.usable_at(4));
        assert!(ConnectorHealth::QuotaExhausted { reset_ms: 5 }.usable_at(5));
        assert!(!ConnectorHealth::AuthenticationRequired.usable_at(u64::MAX));
        assert!(!ConnectorHealth::VerificationRequired {
            kind: VerificationKind::Challenge
        }
        .usable_at(u64::MAX));
        assert!(!ConnectorHealth::Unavailable.usable_at(u64::MAX));
    }

    #[test]
    fn tracker_degrades_then_cools_down_and_recovers_on_success() {
        let mut tracker = HealthTracker::new(HealthPolicy {
            degrade_threshold: 2,
            cooldown_ms: 500,
        });
        tracker.on_error(&AcquisitionError::NetworkTimeout, 1_000);
        assert!(tracker.health().is_degraded());
        tracker.on_error(&AcquisitionError::NetworkTimeout, 1_000);
        assert_eq!(
            tracker.health(),
            &ConnectorHealth::CoolingDown { until_ms: 1_500 }
        );
        assert!(!tracker.health().usable_at(1_499));
        assert!(tracker.health().usable_at(1_500));
        tracker.on_success();
        assert_eq!(tracker.health(), &ConnectorHealth::Healthy);
        assert_eq!(tracker.consecutive_transient, 0);
    }

    #[test]
    fn tracker_maps_quota_and_human_states() {
        let mut tracker = HealthTracker::default();
        tracker.on_error(
            &AcquisitionError::RateLimited {
                retry_after_ms: 250,
            },
            10,
        );
        assert_eq!(
            tracker.health(),
            &ConnectorHealth::RateLimited { until_ms: 260 }
        );
        tracker.on_error(&AcquisitionError::QuotaExhausted { reset_ms: 900 }, 10);
        assert_eq!(tracker.health().quota_reset_ms(), Some(900));
        tracker.on_error(
            &AcquisitionError::VerificationRequired {
                kind: VerificationKind::Challenge,
            },
            10,
        );
        assert_eq!(
            tracker.health().verification_kind(),
            Some(VerificationKind::Challenge)
        );
        tracker.on_error(&AcquisitionError::AuthenticationRequired, 10);
        assert_eq!(tracker.health(), &ConnectorHealth::AuthenticationRequired);
    }

    #[test]
    fn extraction_health_remembers_the_best_strategy() {
        let mut health = ExtractionHealth::new();
        health.record(
            AcquisitionMechanism::DirectHttp,
            RequestedField::Availability,
            ExtractionOutcome::SchemaMismatch,
            1,
        );
        assert!(health
            .record_of(
                AcquisitionMechanism::DirectHttp,
                RequestedField::Availability
            )
            .unwrap()
            .is_proven_bad());
        health.record(
            AcquisitionMechanism::BrowserNetwork,
            RequestedField::Availability,
            ExtractionOutcome::Success,
            2,
        );
        assert_eq!(
            health.best_strategy(RequestedField::Availability),
            Some(AcquisitionMechanism::BrowserNetwork)
        );
        assert!(health.proven_bad_for_all(
            AcquisitionMechanism::DirectHttp,
            &[RequestedField::Availability]
        ));
        assert!(!health.proven_bad_for_all(
            AcquisitionMechanism::BrowserNetwork,
            &[RequestedField::Availability]
        ));
    }

    #[test]
    fn ties_break_by_precedence_and_not_applicable_never_wins() {
        let mut health = ExtractionHealth::new();
        health.record(
            AcquisitionMechanism::Dom,
            RequestedField::Descriptive,
            ExtractionOutcome::Success,
            1,
        );
        health.record(
            AcquisitionMechanism::OfficialApi,
            RequestedField::Descriptive,
            ExtractionOutcome::Success,
            1,
        );
        health.record(
            AcquisitionMechanism::EmbeddedState,
            RequestedField::Descriptive,
            ExtractionOutcome::NotApplicable,
            1,
        );
        assert_eq!(
            health.best_strategy(RequestedField::Descriptive),
            Some(AcquisitionMechanism::OfficialApi)
        );
        assert_eq!(
            health.ranked_strategies(RequestedField::Descriptive),
            vec![AcquisitionMechanism::OfficialApi, AcquisitionMechanism::Dom]
        );
        assert_eq!(health.best_strategy(RequestedField::BulkSet), None);
    }
}
