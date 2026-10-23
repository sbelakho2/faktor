//! Real billing periods: quota limits evaluate the CURRENT period only,
//! rollover deterministically resets them at the configured boundary, and
//! the all-time rollups survive for analytics. The clock is controllable
//! (no wall-clock ambiguity).

use std::sync::Arc;

use faktor_cloud::{
    AdmissionBoundary, AdmissionRequest, BillingAccountId, BillingConfig, EntitlementService,
    ManualClock, MemoryBillingStore, ObservedUsage, OrganizationId, PlanConfig, SpendCategory,
    Subscription, SubscriptionId, SubscriptionStatus, UsageEvent, UsageEventId, UsageUnit,
    FEATURE_MANAGED_PROVIDERS, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
    LIMIT_MAX_TOKENS_PER_PERIOD,
};

const T0: i64 = 1_700_000_000_000;
const PERIOD_MS: i64 = 24 * 60 * 60 * 1000;

fn org() -> OrganizationId {
    OrganizationId::try_new("org_period").unwrap()
}

fn account() -> BillingAccountId {
    BillingAccountId::try_new("acct_period").unwrap()
}

fn service() -> (Arc<EntitlementService>, Arc<ManualClock>) {
    let mut config = BillingConfig::default();
    config.plans.insert(
        "monthly".into(),
        PlanConfig {
            plan_id: "monthly".into(),
            features: [FEATURE_MANAGED_PROVIDERS.to_string()]
                .into_iter()
                .collect(),
            limits: [
                (LIMIT_MAX_TOKENS_PER_PERIOD.to_string(), 100u64),
                (LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD.to_string(), 500u64),
            ]
            .into_iter()
            .collect(),
            period_ms: Some(PERIOD_MS as u64),
        },
    );
    config.default_plan = Some("monthly".into());
    config.managed_providers.insert("managed".into());
    let clock = Arc::new(ManualClock::new(T0));
    let store = Arc::new(MemoryBillingStore::new());
    let service =
        EntitlementService::new_for_organization(store, clock.clone(), config, Some(org()))
            .unwrap();
    service
        .ensure_account(&org(), &account(), "primary", true)
        .unwrap();
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_period").unwrap(),
            organization: org(),
            plan_id: "monthly".into(),
            status: SubscriptionStatus::Active,
            started_ms: T0,
            expires_ms: None,
            updated_ms: T0,
        })
        .unwrap();
    (service, clock)
}

fn usage(id: &str, unit: UsageUnit, quantity: u64, at_ms: i64) -> UsageEvent {
    UsageEvent {
        id: UsageEventId::try_new(id).unwrap(),
        organization_id: org(),
        billing_account_id: account(),
        task_id: 7,
        run_id: "1".into(),
        attempt_id: "attempt".into(),
        provider: "managed".into(),
        model: "m".into(),
        unit,
        quantity: if unit == UsageUnit::ProviderCostMicro {
            1
        } else {
            quantity
        },
        provider_cost_micro: if unit == UsageUnit::ProviderCostMicro {
            quantity
        } else {
            0
        },
        source_operation: "test".into(),
        occurred_at_ms: at_ms,
        reconciliation_state: faktor_cloud::ReconciliationState::Reconciled,
        correction_of: None,
        category: SpendCategory::Managed,
        source_key: format!("test:{id}"),
    }
}

fn admission() -> AdmissionRequest {
    AdmissionRequest {
        boundary: AdmissionBoundary::NewTask,
        task_id: Some(7),
        provider: Some("managed".into()),
        observed: ObservedUsage::default(),
    }
}

fn provider_admission() -> AdmissionRequest {
    AdmissionRequest {
        boundary: AdmissionBoundary::NewProviderAttempt,
        task_id: Some(7),
        provider: Some("managed".into()),
        observed: ObservedUsage::default(),
    }
}

#[test]
fn quota_limits_read_the_current_period_and_reset_at_the_boundary() {
    let (service, clock) = service();
    let period = service.current_period(&org()).unwrap().unwrap();
    assert_eq!(period.start_ms, T0);
    assert_eq!(period.end_ms, T0 + PERIOD_MS);

    service
        .record_usage(&usage("u1", UsageUnit::InputTokens, 60, T0 + 10))
        .unwrap();
    let snapshot = service.entitlement_snapshot(&org()).unwrap();
    assert_eq!(snapshot.period_total_tokens, 60);
    assert_eq!(snapshot.total_tokens, 60, "all-time rollup kept");
    assert_eq!(snapshot.period.as_ref().unwrap().id, period.id);
    assert!(
        service.check_admission(&org(), &admission()).is_ok(),
        "60 < 100 is admitted in the same period"
    );

    service
        .record_usage(&usage("u2", UsageUnit::InputTokens, 50, T0 + 11))
        .unwrap();
    let denied = service.check_admission(&org(), &admission()).unwrap_err();
    assert_eq!(denied.limit, LIMIT_MAX_TOKENS_PER_PERIOD);
    assert_eq!(denied.observed, 110);

    // One millisecond before the boundary: still the same period.
    clock.advance(PERIOD_MS - 1);
    assert!(service.check_admission(&org(), &admission()).is_err());

    // Exactly at the boundary: a NEW period, quota reset.
    clock.advance(1);
    let next = service.current_period(&org()).unwrap().unwrap();
    assert_ne!(next.id, period.id);
    assert_eq!(next.start_ms, T0 + PERIOD_MS);
    let snapshot = service.entitlement_snapshot(&org()).unwrap();
    assert_eq!(snapshot.period_total_tokens, 0);
    assert_eq!(snapshot.total_tokens, 110, "all-time analytics unchanged");
    assert!(service.check_admission(&org(), &admission()).is_ok());
}

#[test]
fn managed_spend_quota_is_period_scoped_and_task_aggregates_are_keyed_by_period() {
    let (service, clock) = service();
    service
        .record_usage(&usage("c1", UsageUnit::ProviderCostMicro, 400, T0 + 1))
        .unwrap();
    let snapshot = service.entitlement_snapshot(&org()).unwrap();
    assert_eq!(snapshot.period_managed_spend_micro, 400);
    assert_eq!(snapshot.managed_spend_micro, 400);
    assert!(service
        .check_admission(&org(), &provider_admission())
        .is_ok());

    service
        .record_usage(&usage("c2", UsageUnit::ProviderCostMicro, 200, T0 + 2))
        .unwrap();
    let denied = service
        .check_admission(&org(), &provider_admission())
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD);

    let period = service.current_period(&org()).unwrap().unwrap();
    let fold = service.fold_in_period(&org(), &period.id).unwrap();
    assert_eq!(fold.totals.managed_cost_micro, 600);
    assert_eq!(fold.per_task.len(), 1);
    assert_eq!(fold.per_task[0].task_id, 7);
    assert_eq!(fold.per_task[0].totals.managed_cost_micro, 600);

    clock.advance(PERIOD_MS);
    let snapshot = service.entitlement_snapshot(&org()).unwrap();
    assert_eq!(snapshot.period_managed_spend_micro, 0);
    assert_eq!(snapshot.managed_spend_micro, 600, "all-time kept");
    assert!(service
        .check_admission(&org(), &provider_admission())
        .is_ok());
}

#[test]
fn corrections_stay_in_their_base_period() {
    let (service, clock) = service();
    let base = usage("b1", UsageUnit::InputTokens, 80, T0 + 5);
    service.record_usage(&base).unwrap();
    clock.advance(PERIOD_MS + 1);
    let mut correction = usage("corr", UsageUnit::InputTokens, 100, T0 + PERIOD_MS + 1);
    correction.correction_of = Some(base.id.clone());
    correction.reconciliation_state = faktor_cloud::ReconciliationState::Reconciled;
    service.correct_usage(&correction).unwrap();
    // A correction NEVER moves usage across a boundary: it inherits the
    // base event's period even though it arrived in the next period.
    let previous = faktor_cloud::BillingPeriod::derive(T0, PERIOD_MS, T0 + 5).unwrap();
    let current = service.current_period(&org()).unwrap().unwrap();
    assert_ne!(previous.id, current.id);
    let prev_fold = service.fold_in_period(&org(), &previous.id).unwrap();
    assert_eq!(prev_fold.totals.input_tokens, 100);
    assert_eq!(prev_fold.totals.managed_cost_micro, 0);
    assert_eq!(prev_fold.totals.corrected_events, 2);
    let current_fold = service.fold_in_period(&org(), &current.id).unwrap();
    assert_eq!(current_fold.totals.input_tokens, 0);
}
