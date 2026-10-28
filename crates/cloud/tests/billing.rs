// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Adversarial tests of the Wave 3 commercial metering domain (usage ledger,
//! entitlements, credits): append-only corrections, BYOK-vs-managed
//! accounting, credit exactness across crashes, the in-flight-transaction
//! invariant, quota refusals naming the exact limit, and org isolation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_cloud::billing::{
    task_id_text, CreditLedgerError, ALL_LIMITS, FEATURE_BYOK, FEATURE_CREDITS,
    FEATURE_MANAGED_PROVIDERS, LIMIT_MAX_ACTIVE_TASKS, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
    LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK, LIMIT_MAX_TOKENS_PER_PERIOD,
    LIMIT_MIN_CREDIT_BALANCE_MICRO, UNIT_PROVIDER_COST,
};
use faktor_cloud::{
    Admission, AdmissionBoundary, AdmissionRequest, BillingAccount, BillingAccountId,
    BillingCapabilityState, BillingConfig, BillingStore, BillingStoreError, ControlPlaneError,
    CreditAppend, CreditAppendRefusal, CreditEntry, CreditEntryId, CreditKind, DurableSpendRow,
    EntitlementExceeded, EntitlementService, InFlightKind, ManualClock, MemoryBillingStore,
    ObservedUsage, OrganizationId, PlanConfig, ReconciliationState, SpendCategory,
    SqliteControlPlaneStore, Subscription, SubscriptionId, SubscriptionStatus, UsageEvent,
    UsageEventId, UsageUnit, CAUSE_CURRENCY_NOT_CONFIGURED, CAUSE_INVOICES_NOT_AVAILABLE,
    CAUSE_LEDGER_OVERFLOW, CAUSE_PAYMENT_PROVIDER_NOT_CONFIGURED, CAUSE_PLAN,
    CAUSE_SEAT_BILLING_NOT_MODELED, CAUSE_SUBSCRIPTION_ACTIVE, CAUSE_TAX_NOT_CONFIGURED,
};

fn org(id: &str) -> OrganizationId {
    OrganizationId::try_new(id).unwrap()
}

fn account(id: &str) -> BillingAccountId {
    BillingAccountId::try_new(id).unwrap()
}

fn configured() -> BillingConfig {
    let mut config = BillingConfig::default();
    config.plans.insert(
        "pro".into(),
        PlanConfig {
            period_ms: None,
            plan_id: "pro".into(),
            features: [
                FEATURE_MANAGED_PROVIDERS.to_string(),
                FEATURE_BYOK.to_string(),
                FEATURE_CREDITS.to_string(),
            ]
            .into_iter()
            .collect(),
            limits: [
                (LIMIT_MAX_ACTIVE_TASKS.to_string(), 1),
                (LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK.to_string(), 4),
                (LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD.to_string(), 1_000),
                (LIMIT_MAX_TOKENS_PER_PERIOD.to_string(), 100_000),
            ]
            .into_iter()
            .collect(),
        },
    );
    config.default_plan = Some("pro".into());
    config.managed_providers.insert("openai".into());
    config
}

fn service_with(store: Arc<dyn BillingStore>, clock: Arc<ManualClock>) -> Arc<EntitlementService> {
    EntitlementService::new(store, clock, configured()).expect("the test config is valid")
}

fn subscribe(service: &EntitlementService, organization: &OrganizationId, expires_ms: Option<i64>) {
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_1").unwrap(),
            organization: organization.clone(),
            plan_id: "pro".into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms,
            updated_ms: 0,
        })
        .unwrap();
}

fn spend_row(
    reservation: i64,
    cost: u64,
    tokens_in: u64,
    tokens_out: u64,
    cache: u64,
) -> DurableSpendRow {
    DurableSpendRow {
        organization_id: "org_a".into(),
        session_id: 1,
        task_id: 7,
        reservation_id: reservation,
        attempt_id: format!("attempt-{reservation}"),
        provider: "openai".into(),
        model: "m".into(),
        input_tokens: tokens_in,
        output_tokens: tokens_out,
        cache_read_tokens: cache,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        provider_cost_micro: cost,
        provider_reported_micro: cost,
        price_authority: Some(faktor_core::model::PriceAuthority::Exact),
        state: "settled".into(),
        occurred_at_ms: 1000 + reservation,
        source_operation: "session.cost_reservation.settled".into(),
    }
}

fn usage_event(id: &str) -> UsageEvent {
    UsageEvent {
        id: UsageEventId::try_new(id).unwrap(),
        organization_id: org("org_a"),
        billing_account_id: account("acct_1"),
        task_id: 7,
        run_id: "1".into(),
        attempt_id: "attempt-x".into(),
        provider: "openai".into(),
        model: "m".into(),
        unit: UsageUnit::ProviderCostMicro,
        quantity: 1,
        provider_cost_micro: 500,
        source_operation: "session.cost_reservation.settled".into(),
        occurred_at_ms: 1,
        reconciliation_state: ReconciliationState::Reconciled,
        correction_of: None,
        category: SpendCategory::Managed,
        source_key: format!("manual:{id}"),
    }
}

fn credit_entry(
    id: &str,
    kind: CreditKind,
    amount_micro: u64,
    reference: Option<&str>,
) -> CreditEntry {
    CreditEntry {
        id: CreditEntryId::try_new(id).unwrap(),
        organization: org("org_a"),
        billing_account_id: account("acct_1"),
        kind,
        amount_micro,
        reference: reference.map(|r| CreditEntryId::try_new(r).unwrap()),
        usage_event_id: None,
        reason: "boundary".into(),
        occurred_at_ms: 1,
        idempotency_key: None,
    }
}

#[test]
fn corrections_are_new_events_the_base_row_is_never_mutated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let service = service_with(
        Arc::new(SqliteControlPlaneStore::open(&path).unwrap()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    service
        .ensure_account(&organization, &account("acct_1"), "acct", true)
        .unwrap();
    let base = usage_event("uev_base");
    assert!(matches!(
        service.record_usage(&base).unwrap(),
        faktor_cloud::UsageAppend::Appended
    ));
    let before = service
        .usage_event_of(&organization, "uev_base")
        .unwrap()
        .unwrap();

    let mut correction = usage_event("uev_fix");
    correction.correction_of = Some(base.id.clone());
    correction.provider_cost_micro = 125;
    correction.reconciliation_state = ReconciliationState::Reconciled;
    assert!(matches!(
        service.correct_usage(&correction).unwrap(),
        faktor_cloud::UsageAppend::Appended
    ));
    // The base row is byte-identical after the correction (never mutated).
    let after = service
        .usage_event_of(&organization, "uev_base")
        .unwrap()
        .unwrap();
    assert_eq!(before, after, "corrections never mutate the corrected row");
    // The fold uses the correction exactly once.
    let fold = service.fold(&organization).unwrap();
    assert_eq!(fold.totals.provider_cost_micro, 125);
    assert_eq!(fold.totals.managed_cost_micro, 125);
    assert_eq!(fold.totals.events, 1);
    assert_eq!(fold.totals.corrected_events, 2, "base + correction");
    // A second correction of the SAME base is a fork: refused loudly.
    let mut fork = usage_event("uev_fork");
    fork.correction_of = Some(base.id.clone());
    fork.provider_cost_micro = 99;
    fork.reconciliation_state = ReconciliationState::Reconciled;
    assert!(matches!(
        service.correct_usage(&fork).unwrap_err(),
        faktor_cloud::ControlPlaneError::Conflict(_)
    ));
    // A correction of a correction is refused.
    let mut chain = usage_event("uev_chain");
    chain.correction_of = Some(correction.id.clone());
    chain.reconciliation_state = ReconciliationState::Reconciled;
    assert!(matches!(
        service.correct_usage(&chain).unwrap_err(),
        faktor_cloud::ControlPlaneError::Conflict(_)
    ));
    // A self-correction never validates.
    let mut self_fix = usage_event("uev_self");
    self_fix.correction_of = Some(self_fix.id.clone());
    self_fix.reconciliation_state = ReconciliationState::Reconciled;
    assert!(service.correct_usage(&self_fix).is_err());
    // A correction naming a foreign organization's event is a typed 404.
    service
        .store()
        .put_billing_account(&BillingAccount {
            id: account("acct_b"),
            organization: org("org_b"),
            name: "b".into(),
            managed: true,
            created_ms: 0,
            disabled: false,
        })
        .unwrap();
    let mut foreign = usage_event("uev_foreign");
    foreign.organization_id = org("org_b");
    foreign.billing_account_id = account("acct_b");
    foreign.correction_of = Some(base.id.clone());
    foreign.reconciliation_state = ReconciliationState::Reconciled;
    assert!(matches!(
        service.correct_usage(&foreign).unwrap_err(),
        faktor_cloud::ControlPlaneError::NotFound(_)
    ));
}

#[test]
fn correction_winner_is_latest_by_time_then_id() {
    let base = usage_event("uev_base");
    // A correction with a later time but an id that sorts BEFORE the other
    // correction's id: the TIME must decide, exactly like
    // `UsageEvent::effective_state`.
    let mut early = usage_event("uev_z");
    early.correction_of = Some(base.id.clone());
    early.provider_cost_micro = 60;
    early.occurred_at_ms = 1_000;
    early.reconciliation_state = ReconciliationState::Reconciled;
    let mut late = usage_event("uev_a");
    late.correction_of = Some(base.id.clone());
    late.provider_cost_micro = 99;
    late.occurred_at_ms = 2_000;
    late.reconciliation_state = ReconciliationState::Reconciled;

    let events = vec![base.clone(), early.clone(), late.clone()];
    assert_eq!(
        UsageEvent::effective_state(&base, &events),
        late.reconciliation_state,
        "the effective state and the fold must agree on the winner"
    );
    let fold = faktor_cloud::fold_usage(&org("org_a"), &events).unwrap();
    assert_eq!(
        fold.totals.provider_cost_micro, 99,
        "the later correction wins even though its id sorts first"
    );
    assert_eq!(fold.totals.managed_cost_micro, 99);
    assert_eq!(
        fold.totals.events, 1,
        "only the winning correction contributes to the totals"
    );
    assert_eq!(
        fold.totals.corrected_events, 3,
        "base + both corrections are accounted as corrected"
    );
}

#[test]
fn byok_usage_is_recorded_but_never_debits_credits() {
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    service
        .grant_credits(&organization, &acct, 1_000, "seed", Some("g1"))
        .unwrap();
    // A managed consume debits (record-before-call).
    let consume = match service
        .consume_before_call(
            &organization,
            &acct,
            200,
            None,
            "attempt",
            Some("consume-1"),
        )
        .unwrap()
    {
        faktor_cloud::CreditAppend::Appended => service
            .credit_entries_page(&organization, 0, 10)
            .unwrap()
            .into_iter()
            .find(|row| row.entry.kind == CreditKind::Consume)
            .map(|row| row.entry.id)
            .unwrap(),
        other => panic!("expected an appended consume, got {other:?}"),
    };
    assert_eq!(
        service
            .credit_balance(&organization)
            .unwrap()
            .balance_micro()
            .unwrap(),
        800
    );
    // A BYOK spend row is recorded as usage and debits NOTHING.
    let before_entries = service
        .credit_entries_page(&organization, 0, 100)
        .unwrap()
        .len();
    let report = service
        .ingest_spend_row(
            &organization,
            &acct,
            &spend_row(1, 300, 10, 20, 3),
            SpendCategory::Byok,
        )
        .unwrap();
    assert!(report.appended >= 2, "tokens + cost rows: {report:?}");
    assert_eq!(
        service
            .credit_entries_page(&organization, 0, 100)
            .unwrap()
            .len(),
        before_entries,
        "BYOK ingestion never appends a credit entry"
    );
    assert_eq!(
        service
            .credit_balance(&organization)
            .unwrap()
            .balance_micro()
            .unwrap(),
        800
    );
    let fold = service.fold(&organization).unwrap();
    assert_eq!(fold.totals.byok_cost_micro, 300);
    assert_eq!(fold.totals.managed_cost_micro, 0);
    // A replay of the same durable row appends NOTHING (idempotent source
    // keys), so a crash-recovery re-read can never double count.
    let replay = service
        .ingest_spend_row(
            &organization,
            &acct,
            &spend_row(1, 300, 10, 20, 3),
            SpendCategory::Byok,
        )
        .unwrap();
    assert_eq!(replay.appended, 0);
    assert_eq!(replay.duplicates, report.appended);
    assert_eq!(
        service.fold(&organization).unwrap().totals.byok_cost_micro,
        300
    );
    // A BYOK-only account refuses managed writes (category enforced on write).
    let byok_only = account("acct_byok");
    service
        .ensure_account(&organization, &byok_only, "byok", false)
        .unwrap();
    let mut managed = usage_event("uev_managed");
    managed.billing_account_id = byok_only.clone();
    assert!(service.record_usage(&managed).is_err());
    // The consume is left pending: a crash after the debit holds the credits.
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 200);
    assert_eq!(balance.pending_consumes, 1);
    assert_eq!(balance.balance_micro().unwrap(), 800);
    // Settling at the actual releases the hold exactly.
    service
        .settle_consume(&organization, &acct, &consume, 150, "actual")
        .unwrap();
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 0);
    assert_eq!(balance.consumed_micro, 150);
    assert_eq!(balance.balance_micro().unwrap(), 850);
    // Refunds are exact: the unused 50 comes back, more is refused.
    service
        .refund_consume(&organization, &acct, &consume, 50, "unused")
        .unwrap();
    assert_eq!(
        service
            .credit_balance(&organization)
            .unwrap()
            .balance_micro()
            .unwrap(),
        900
    );
    assert!(matches!(
        service
            .refund_consume(&organization, &acct, &consume, 101, "more than spent")
            .unwrap_err(),
        faktor_cloud::ControlPlaneError::Conflict(_)
    ));
    // A second settle of the same consume is refused (exactly once).
    assert!(matches!(
        service
            .settle_consume(&organization, &acct, &consume, 10, "again")
            .unwrap_err(),
        faktor_cloud::ControlPlaneError::Conflict(_)
    ));
    // An overdrawing consume writes NOTHING.
    assert!(matches!(
        service
            .consume_before_call(&organization, &acct, 10_000, None, "too big", Some("over"))
            .unwrap_err(),
        faktor_cloud::ControlPlaneError::Conflict(_)
    ));
    assert_eq!(
        service
            .credit_balance(&organization)
            .unwrap()
            .balance_micro()
            .unwrap(),
        900
    );
}

#[test]
fn credit_ledger_survives_a_crash_with_the_hold_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let organization = org("org_a");
    let acct = account("acct_1");
    let consume_id;
    {
        let service = service_with(
            Arc::new(SqliteControlPlaneStore::open(&path).unwrap()),
            Arc::new(ManualClock::new(1_000)),
        );
        service
            .ensure_account(&organization, &acct, "acct", true)
            .unwrap();
        service
            .grant_credits(&organization, &acct, 1_000, "seed", None)
            .unwrap();
        // Record-before-call: the debit lands BEFORE the provider call.
        service
            .consume_before_call(&organization, &acct, 400, None, "attempt", None)
            .unwrap();
        consume_id = service
            .credit_entries_page(&organization, 0, 10)
            .unwrap()
            .into_iter()
            .find(|row| row.entry.kind == CreditKind::Consume)
            .map(|row| row.entry.id)
            .unwrap();
        // Crash here: no settle was written.
    }
    let service = service_with(
        Arc::new(SqliteControlPlaneStore::open(&path).unwrap()),
        Arc::new(ManualClock::new(2_000)),
    );
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 400, "the hold survives the crash");
    assert_eq!(balance.pending_consumes, 1);
    assert_eq!(balance.balance_micro().unwrap(), 600);
    // Reconciliation settles at the actual provider spend, exactly once.
    service
        .settle_consume(&organization, &acct, &consume_id, 260, "reconciled")
        .unwrap();
    assert_eq!(
        service
            .credit_balance(&organization)
            .unwrap()
            .balance_micro()
            .unwrap(),
        740
    );
    // Replaying the settle after another crash is the same typed refusal.
    assert!(service
        .settle_consume(&organization, &acct, &consume_id, 260, "replay")
        .is_err());
    // Unknown references are typed refusals, never silent accepts.
    let bogus = CreditEntryId::try_new("crd_missing").unwrap();
    assert!(matches!(
        service
            .refund_consume(&organization, &acct, &bogus, 1, "ghost")
            .unwrap_err(),
        faktor_cloud::ControlPlaneError::Conflict(_)
    ));
}

#[test]
fn expired_subscription_mid_transaction_finishes_then_denies_the_next_admission() {
    let clock = Arc::new(ManualClock::new(1_000));
    let service = service_with(Arc::new(MemoryBillingStore::new()), clock.clone());
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    service
        .grant_credits(&organization, &acct, 10_000, "seed", None)
        .unwrap();
    subscribe(&service, &organization, Some(2_000));
    // A first task admission is admitted while the subscription is live.
    assert_eq!(
        service
            .check_admission(
                &organization,
                &AdmissionRequest {
                    observed: ObservedUsage {
                        active_tasks: 0,
                        ..Default::default()
                    },
                    ..AdmissionRequest::boundary(AdmissionBoundary::NewTask)
                }
            )
            .unwrap(),
        Admission::Admitted
    );
    // The integration transaction opens.
    let in_flight = service
        .begin_in_flight(&organization, InFlightKind::Integration, "run-1")
        .unwrap();
    // The subscription expires DURING the transaction.
    clock.advance(5_000);
    let snapshot = service.entitlement_snapshot(&organization).unwrap();
    assert!(!snapshot.subscription_active);
    // The transaction continues: continuation boundaries are never gated.
    assert_eq!(
        service
            .check_admission(
                &organization,
                &AdmissionRequest::boundary(AdmissionBoundary::IntegrationContinuation)
            )
            .unwrap(),
        Admission::InFlightContinuation
    );
    assert_eq!(
        service
            .check_admission(
                &organization,
                &AdmissionRequest::boundary(AdmissionBoundary::RollbackContinuation)
            )
            .unwrap(),
        Admission::InFlightContinuation
    );
    // The transaction finishes (the in-flight row closes exactly once).
    assert!(service
        .finish_in_flight(&organization, &in_flight.id)
        .unwrap());
    assert!(!service
        .finish_in_flight(&organization, &in_flight.id)
        .unwrap());
    // The NEXT admission is denied, naming the exact cause.
    let denied: EntitlementExceeded = service
        .check_admission(
            &organization,
            &AdmissionRequest::boundary(AdmissionBoundary::NewTask),
        )
        .expect_err("an expired subscription denies the next admission");
    assert_eq!(denied.limit, CAUSE_SUBSCRIPTION_ACTIVE);
    assert_eq!(denied.boundary, AdmissionBoundary::NewTask);
    // The in-flight transaction is closed: no stale continuation bypass.
    assert!(service
        .entitlement_snapshot(&organization)
        .unwrap()
        .in_flight
        .is_empty());
}

#[test]
fn quota_exhaustion_at_admission_names_the_exact_limit() {
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    subscribe(&service, &organization, None);
    // One live task against max_active_tasks = 1: the next admission is
    // refused naming THAT limit with the observed value.
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest {
                observed: ObservedUsage {
                    active_tasks: 1,
                    ..Default::default()
                },
                ..AdmissionRequest::boundary(AdmissionBoundary::NewTask)
            },
        )
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MAX_ACTIVE_TASKS);
    assert_eq!(denied.limit_value, Some(1));
    assert_eq!(denied.observed, 1);
    assert_eq!(denied.boundary, AdmissionBoundary::NewTask);
    // Child spawn and provider attempt have their own exact limits.
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest {
                observed: ObservedUsage {
                    provider_attempts_of_task: 4,
                    ..Default::default()
                },
                ..AdmissionRequest::boundary(AdmissionBoundary::NewProviderAttempt)
            },
        )
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK);
    // A managed attempt whose estimate would cross the managed-spend limit
    // is refused naming the spend limit (the folded managed spend is read
    // from the ledger, never trusted from the caller).
    let report = service
        .ingest_spend_row(
            &organization,
            &acct,
            &spend_row(1, 1_000, 1, 1, 0),
            SpendCategory::Managed,
        )
        .unwrap();
    assert!(report.appended >= 1);
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest {
                provider: Some("openai".into()),
                observed: ObservedUsage {
                    estimated_provider_cost_micro: 1,
                    ..Default::default()
                },
                ..AdmissionRequest::boundary(AdmissionBoundary::NewProviderAttempt)
            },
        )
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD);
    // Token exhaustion too.
    let mut big = spend_row(2, 0, 200_000, 0, 0);
    big.provider_cost_micro = 0;
    big.provider_reported_micro = 0;
    service
        .ingest_spend_row(&organization, &acct, &big, SpendCategory::Byok)
        .unwrap();
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest::boundary(AdmissionBoundary::NewChildSpawn),
        )
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MAX_TOKENS_PER_PERIOD);
    // A missing plan (config gap) denies naming the plan, never a guess.
    let empty = EntitlementService::new(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
        BillingConfig::default(),
    )
    .unwrap();
    let denied = empty
        .check_admission(
            &org("org_x"),
            &AdmissionRequest::boundary(AdmissionBoundary::NewTask),
        )
        .unwrap_err();
    assert_eq!(denied.limit, "plan");
    assert!(ALL_LIMITS.contains(&LIMIT_MAX_ACTIVE_TASKS));
    assert!(ALL_LIMITS.contains(&denied.limit.as_str()) || denied.limit == "plan");
}

#[test]
fn configured_min_credit_balance_floor_is_enforced_at_admission() {
    let mut config = configured();
    config
        .plans
        .get_mut("pro")
        .expect("pro plan")
        .limits
        .insert(LIMIT_MIN_CREDIT_BALANCE_MICRO.to_string(), 500);
    let service = EntitlementService::new(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
        config,
    )
    .expect("the floored config is valid");
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    subscribe(&service, &organization, None);

    // Free balance 0 < configured floor 500: the NEW admission is refused
    // naming the exact limit and both numbers.
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest::boundary(AdmissionBoundary::NewTask),
        )
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MIN_CREDIT_BALANCE_MICRO);
    assert_eq!(denied.limit_value, Some(500));
    assert_eq!(denied.observed, 0);
    assert_eq!(denied.boundary, AdmissionBoundary::NewTask);

    // Balance 600 >= 500: admitted.
    service
        .grant_credits(&organization, &acct, 600, "seed", Some("g1"))
        .unwrap();
    assert_eq!(
        service
            .check_admission(
                &organization,
                &AdmissionRequest::boundary(AdmissionBoundary::NewTask)
            )
            .unwrap(),
        Admission::Admitted
    );

    // A record-before-call hold that drops the FREE balance below the floor
    // denies the next gated boundary — while a continuation of the in-flight
    // transaction is never interrupted.
    service
        .consume_before_call(&organization, &acct, 200, None, "hold", Some("c1"))
        .unwrap();
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest::boundary(AdmissionBoundary::NewChildSpawn),
        )
        .unwrap_err();
    assert_eq!(denied.limit, LIMIT_MIN_CREDIT_BALANCE_MICRO);
    assert_eq!(denied.limit_value, Some(500));
    assert_eq!(denied.observed, 400);
    assert_eq!(
        service
            .check_admission(
                &organization,
                &AdmissionRequest::boundary(AdmissionBoundary::RollbackContinuation)
            )
            .unwrap(),
        Admission::InFlightContinuation
    );
}

#[test]
fn org_isolation_and_unit_vocabulary_are_exact() {
    for unit in [
        UsageUnit::InputTokens,
        UsageUnit::OutputTokens,
        UsageUnit::CacheReadTokens,
        UsageUnit::CacheWriteTokens,
        UsageUnit::ReasoningTokens,
        UsageUnit::ProviderCostMicro,
    ] {
        assert_eq!(UsageUnit::parse(unit.as_str()), Some(unit));
    }
    assert_eq!(UNIT_PROVIDER_COST, UsageUnit::ProviderCostMicro.as_str());
    for category in [SpendCategory::Managed, SpendCategory::Byok] {
        assert_eq!(SpendCategory::parse(category.as_str()), Some(category));
    }
    // A foreign organization's rows are invisible to the fold and the page.
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    for (organization, account_id, task) in [("org_a", "acct_a", 1u64), ("org_b", "acct_b", 2u64)] {
        let organization = org(organization);
        let account_id = account(account_id);
        service
            .ensure_account(&organization, &account_id, "acct", true)
            .unwrap();
        let mut event = usage_event(&format!("uev_{task}"));
        event.organization_id = organization.clone();
        event.billing_account_id = account_id;
        event.task_id = task;
        event.source_key = format!("manual:task-{task}");
        service.record_usage(&event).unwrap();
    }
    let fold_a = service.fold(&org("org_a")).unwrap();
    assert_eq!(fold_a.totals.provider_cost_micro, 500);
    assert_eq!(fold_a.per_task.len(), 1);
    assert_eq!(fold_a.per_task[0].task_id, 1);
    let page_a = service.usage_page(&org("org_a"), None, 10).unwrap();
    assert_eq!(page_a.items.len(), 1);
    assert_eq!(page_a.items[0].event.task_id, 1);
    assert!(service
        .usage_page(&org("org_missing"), None, 10)
        .unwrap()
        .items
        .is_empty());
    // Hostile cursors are typed refusals (never a silent page from zero).
    assert!(service.usage_page(&org("org_a"), Some("-1"), 10).is_err());
    assert!(service
        .usage_page(&org("org_a"), Some("not-a-cursor"), 10)
        .is_err());
    // A credit entry on a foreign account is still scoped by organization.
    assert_eq!(
        service.credit_balance(&org("org_missing")).unwrap(),
        Default::default()
    );
}

#[test]
fn sqlite_billing_store_roundtrip_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let organization = org("org_a");
    {
        let service = service_with(
            Arc::new(SqliteControlPlaneStore::open(&path).unwrap()),
            Arc::new(ManualClock::new(1_000)),
        );
        let acct = account("acct_1");
        service
            .ensure_account(&organization, &acct, "acct", true)
            .unwrap();
        subscribe(&service, &organization, None);
        service
            .grant_credits(&organization, &acct, 500, "seed", Some("k1"))
            .unwrap();
        service
            .begin_in_flight(&organization, InFlightKind::Completion, "run-9")
            .unwrap();
        service
            .ingest_spend_row(
                &organization,
                &acct,
                &spend_row(11, 42, 5, 6, 0),
                SpendCategory::Managed,
            )
            .unwrap();
    }
    let service = service_with(
        Arc::new(SqliteControlPlaneStore::open(&path).unwrap()),
        Arc::new(ManualClock::new(2_000)),
    );
    let snapshot = service.entitlement_snapshot(&organization).unwrap();
    assert!(snapshot.plan_found);
    assert!(snapshot.subscription_active);
    assert_eq!(snapshot.credits.granted_micro, 500);
    assert_eq!(snapshot.managed_spend_micro, 42);
    assert_eq!(
        snapshot.in_flight.len(),
        1,
        "the open transaction is durable"
    );
    // Idempotent provisioning + idempotent grant replay.
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    assert!(matches!(
        service
            .grant_credits(&organization, &acct, 500, "seed", Some("k1"))
            .unwrap(),
        faktor_cloud::CreditAppend::Duplicate
    ));
    assert_eq!(
        service.credit_balance(&organization).unwrap().granted_micro,
        500
    );
    // A conflicting reuse of the same key is refused.
    assert!(matches!(
        service
            .grant_credits(&organization, &acct, 999, "other", Some("k1"))
            .unwrap_err(),
        faktor_cloud::ControlPlaneError::Conflict(_)
    ));
    // Append-only store surface: a duplicate source key is a no-op.
    let acct = account("acct_1");
    let row = spend_row(11, 42, 5, 6, 0);
    let report = service
        .ingest_spend_row(&organization, &acct, &row, SpendCategory::Managed)
        .unwrap();
    assert_eq!(report.appended, 0);
    assert!(report.duplicates > 0);
}

#[test]
fn hostile_page_limit_is_refused_typed_without_wrapping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let store = SqliteControlPlaneStore::open(&path).unwrap();
    // Only 64-bit `usize` can hold a limit above SQLite's signed domain
    // (on 32-bit every `usize` fits `i64`, so no refusal is expressible).
    if i64::try_from(usize::MAX).is_ok() {
        return;
    }
    let organization = org("org_a");
    assert!(store
        .billing_accounts(&organization, None, 10)
        .unwrap()
        .is_empty());
    assert!(store
        .credit_entries(&organization, 0, 10)
        .unwrap()
        .is_empty());
    assert!(store.report_periods(&organization, 10).unwrap().is_empty());
    for err in [
        store
            .billing_accounts(&organization, None, usize::MAX)
            .unwrap_err(),
        store
            .credit_entries(&organization, 0, usize::MAX)
            .unwrap_err(),
        store.report_periods(&organization, usize::MAX).unwrap_err(),
    ] {
        assert!(
            matches!(err, BillingStoreError::Malformed(_)),
            "the upper half of usize is refused typed, never wrapped: {err:?}"
        );
    }
}

#[test]
fn credit_refusals_are_typed_and_write_nothing() {
    // The memory store and SQLite store must agree on the exact refusal
    // vocabulary (a divergence would be an accounting hole).
    let mem = MemoryBillingStore::new();
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("b.sqlite")).unwrap();
    let organization = org("org_a");
    let acct = account("acct_1");
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        let grant = CreditEntry {
            id: CreditEntryId::try_new("crd_g").unwrap(),
            organization: organization.clone(),
            billing_account_id: acct.clone(),
            kind: CreditKind::Grant,
            amount_micro: 100,
            reference: None,
            usage_event_id: None,
            reason: "seed".into(),
            occurred_at_ms: 1,
            idempotency_key: None,
        };
        assert!(store.append_credit_entry(&grant).is_ok());
        let missing = CreditEntry {
            id: CreditEntryId::try_new("crd_missing_ref").unwrap(),
            organization: organization.clone(),
            billing_account_id: acct.clone(),
            kind: CreditKind::Settle,
            amount_micro: 1,
            reference: Some(CreditEntryId::try_new("crd_ghost").unwrap()),
            usage_event_id: None,
            reason: "ghost".into(),
            occurred_at_ms: 1,
            idempotency_key: None,
        };
        assert_eq!(
            store.append_credit_entry(&missing).unwrap_err(),
            faktor_cloud::BillingStoreError::Credit(CreditAppendRefusal::UnknownReference(
                "crd_ghost".into()
            )),
            "both stores refuse the unknown reference identically"
        );
        let over = CreditEntry {
            id: CreditEntryId::try_new("crd_over").unwrap(),
            organization: organization.clone(),
            billing_account_id: acct.clone(),
            kind: CreditKind::Consume,
            amount_micro: 101,
            reference: None,
            usage_event_id: None,
            reason: "over".into(),
            occurred_at_ms: 1,
            idempotency_key: None,
        };
        assert!(matches!(
            store.append_credit_entry(&over).unwrap_err(),
            faktor_cloud::BillingStoreError::Credit(
                CreditAppendRefusal::InsufficientCredits { .. }
            )
        ));
        assert_eq!(
            store
                .credit_balance(&organization)
                .unwrap()
                .balance_micro()
                .unwrap(),
            100
        );
    }
}

#[test]
fn credit_idempotency_identity_includes_account_and_usage_event_on_both_stores() {
    // The same idempotency key is only "the same request" when kind, amount,
    // reference, billing_account_id AND usage_event_id all match. A different
    // account or usage event under one key is a typed conflict, never a silent
    // Duplicate that would drop a second account's ledger entry.
    let mem = MemoryBillingStore::new();
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("b.sqlite")).unwrap();
    let organization = org("org_a");
    let entry = |id: &str, account_id: &str, event: Option<&str>| CreditEntry {
        id: CreditEntryId::try_new(id).unwrap(),
        organization: organization.clone(),
        billing_account_id: account(account_id),
        kind: CreditKind::Grant,
        amount_micro: 100,
        reference: None,
        usage_event_id: event.map(|e| UsageEventId::try_new(e).unwrap()),
        reason: "seed".into(),
        occurred_at_ms: 1,
        idempotency_key: Some("same-request-key".into()),
    };
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        assert_eq!(
            store
                .append_credit_entry(&entry("crd_1", "acct_1", None))
                .unwrap(),
            CreditAppend::Appended
        );
        // Byte-identical request (account and usage event included): replay.
        assert_eq!(
            store
                .append_credit_entry(&entry("crd_1b", "acct_1", None))
                .unwrap(),
            CreditAppend::Duplicate
        );
        // Same key, DIFFERENT account: a conflict, not a duplicate.
        assert_eq!(
            store
                .append_credit_entry(&entry("crd_2", "acct_2", None))
                .unwrap_err(),
            BillingStoreError::Credit(CreditAppendRefusal::IdempotencyConflict(
                "same-request-key".into()
            )),
            "a different billing account under one key must conflict"
        );
        // Same key, DIFFERENT usage event: also a conflict.
        assert_eq!(
            store
                .append_credit_entry(&entry("crd_3", "acct_1", Some("uev_1")))
                .unwrap_err(),
            BillingStoreError::Credit(CreditAppendRefusal::IdempotencyConflict(
                "same-request-key".into()
            )),
            "a different usage event under one key must conflict"
        );
        assert_eq!(
            store.credit_balance(&organization).unwrap().granted_micro,
            100,
            "the refused requests wrote nothing"
        );
    }
}

#[test]
fn managed_usage_reconciliation_pending_to_reconciled_is_durable_and_append_only() {
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    service
        .grant_credits(&organization, &acct, 5_000, "seed", None)
        .unwrap();
    // Record-before-call: the managed attempt's usage row is PENDING (the
    // reservation estimate) and the credit consume holds the same estimate.
    let mut pending = usage_event("uev_pending");
    pending.reconciliation_state = ReconciliationState::Pending;
    pending.provider_cost_micro = 400;
    assert!(matches!(
        service.record_usage(&pending).unwrap(),
        faktor_cloud::UsageAppend::Appended
    ));
    service
        .consume_before_call(
            &organization,
            &acct,
            400,
            Some(&pending.id),
            "attempt",
            None,
        )
        .unwrap();
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 400);
    assert_eq!(balance.balance_micro().unwrap(), 4_600);
    let fold = service.fold(&organization).unwrap();
    assert_eq!(
        fold.totals.provider_cost_micro, 400,
        "the pending row folds"
    );
    assert_eq!(
        service
            .usage_event_of(&organization, "uev_pending")
            .unwrap()
            .unwrap()
            .event
            .reconciliation_state,
        ReconciliationState::Pending
    );
    // Reconcile at the provider's actual: a NEW correction event; the
    // original row stays byte-identical (pending), the fold uses the
    // correction, and the credit hold settles at the actual.
    let mut actual = usage_event("uev_actual");
    actual.correction_of = Some(pending.id.clone());
    actual.provider_cost_micro = 260;
    actual.reconciliation_state = ReconciliationState::Reconciled;
    service.correct_usage(&actual).unwrap();
    let base = service
        .usage_event_of(&organization, "uev_pending")
        .unwrap()
        .unwrap();
    assert_eq!(
        base.event.reconciliation_state,
        ReconciliationState::Pending
    );
    let all = service
        .usage_page(&organization, None, 10)
        .unwrap()
        .items
        .into_iter()
        .map(|row| row.event)
        .collect::<Vec<_>>();
    assert_eq!(
        UsageEvent::effective_state(&base.event, &all),
        ReconciliationState::Reconciled,
        "the effective state transitions durably via the correction"
    );
    let fold = service.fold(&organization).unwrap();
    assert_eq!(fold.totals.provider_cost_micro, 260);
    assert_eq!(fold.totals.corrected_events, 2);
    // A correction that tries to be PENDING is refused up front.
    let mut bad = usage_event("uev_bad");
    bad.correction_of = Some(base.event.id.clone());
    bad.reconciliation_state = ReconciliationState::Pending;
    assert!(service.correct_usage(&bad).is_err());
}

// ------------------------------------------------- P1 task-id identity

/// Create a pre-v6 database: a full current-schema database is opened first
/// so every other table exists at its final shape, then `usage_event` is
/// rolled back to the OLD schema (`task_id INTEGER NOT NULL`, the lossy
/// projection) and the ladder cursor is set to v5. `rows` are
/// `(legacy INTEGER column value, event id, payload JSON)`; entity columns
/// other than id/payload are constants (the v6 finalizer and the read paths
/// only need id + payload).
fn legacy_task_id_db(path: &std::path::Path, rows: &[(i64, String, String)]) {
    drop(SqliteControlPlaneStore::open(path).unwrap());
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "DROP TABLE usage_event;
         -- A true v5 database predates the v8/v9 rollups: drop them so the
         -- replayed ladder recreates the tables EXACTLY as that version did
         -- (the v10 column then arrives through its own migration step).
         DROP TABLE IF EXISTS billing_usage_total;
         DROP TABLE IF EXISTS billing_usage_task;
         DROP TABLE IF EXISTS billing_usage_period;
         DROP TABLE IF EXISTS billing_usage_period_task;
         CREATE TABLE usage_event (
            id TEXT PRIMARY KEY,
            organization_id TEXT NOT NULL,
            event_seq INTEGER NOT NULL,
            source_key TEXT NOT NULL,
            billing_account_id TEXT NOT NULL,
            task_id INTEGER NOT NULL,
            run_id TEXT NOT NULL,
            attempt_id TEXT NOT NULL,
            category TEXT NOT NULL,
            correction_of TEXT,
            occurred_at_ms INTEGER NOT NULL,
            reconciliation_state TEXT NOT NULL,
            payload TEXT NOT NULL,
            UNIQUE (organization_id, source_key)
         );
         CREATE INDEX idx_usage_event_org_task
            ON usage_event(organization_id, task_id, event_seq);
         PRAGMA user_version = 5;",
    )
    .unwrap();
    for (seq, (legacy, id, payload)) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO usage_event
                (id, organization_id, event_seq, source_key, billing_account_id, task_id,
                 run_id, attempt_id, category, correction_of, occurred_at_ms,
                 reconciliation_state, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                id,
                "org_a",
                seq as i64 + 1,
                format!("manual:legacy:{id}"),
                "acct_1",
                legacy,
                "1",
                "attempt-x",
                "managed",
                None::<String>,
                1i64,
                "reconciled",
                payload,
            ],
        )
        .unwrap();
    }
}

fn legacy_payload(task_id: u64) -> String {
    let mut event = usage_event(&format!("uev_{task_id:016x}"));
    event.task_id = task_id;
    event.source_key = format!("manual:legacy:{task_id:016x}");
    serde_json::to_string(&event).unwrap()
}

/// The four boundary ids that must never alias:
/// `i64::MAX`, `i64::MAX + 1` (`0x8000_0000_0000_0000`), `u64::MAX - 1` and
/// `u64::MAX` — three of which the old `min(i64::MAX as u64) as i64`
/// projection collapsed onto one indexed value. Every id must round-trip
/// exactly and a task-filtered page must return exactly its own event,
/// identically on both backends.
#[test]
fn task_id_boundaries_roundtrip_and_never_cross_match_on_both_backends() {
    let mem = MemoryBillingStore::new();
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("b.sqlite")).unwrap();
    let organization = org("org_a");
    let boundaries = [
        1u64,
        i64::MAX as u64,
        i64::MAX as u64 + 1,
        u64::MAX - 1,
        u64::MAX,
    ];
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        for id in boundaries {
            let mut event = usage_event(&format!("uev_{id:016x}"));
            event.task_id = id;
            event.source_key = format!("manual:boundary:{id:016x}");
            assert_eq!(
                store.append_usage_event(&event).unwrap(),
                faktor_cloud::UsageAppend::Appended,
                "task {id:#x} appends"
            );
        }
        for id in boundaries {
            let page = store
                .usage_events_of_task(&organization, id, 0, 100)
                .unwrap();
            let found: Vec<String> = page
                .iter()
                .map(|row| row.event.id.as_str().to_string())
                .collect();
            assert_eq!(
                found,
                vec![format!("uev_{id:016x}")],
                "task {id:#x} must see exactly its own row"
            );
            assert_eq!(page[0].event.task_id, id);
        }
        // The defect's exact aliasing pair: the query for i64::MAX + 1 must
        // never return the i64::MAX row.
        let high = store
            .usage_events_of_task(&organization, i64::MAX as u64 + 1, 0, 100)
            .unwrap();
        assert_eq!(high.len(), 1);
        assert_ne!(high[0].event.id.as_str(), format!("uev_{:016x}", i64::MAX));
        assert_eq!(high[0].event.task_id, i64::MAX as u64 + 1);
        // Ids that were never written never match (no clamping aliases them
        // onto a written high/high boundary).
        for unwritten in [i64::MAX as u64 - 1, u64::MAX - 2] {
            assert!(
                store
                    .usage_events_of_task(&organization, unwritten, 0, 100)
                    .unwrap()
                    .is_empty(),
                "unwritten task {unwritten:#x} matches nothing"
            );
        }
        // The unfiltered page returns every payload identity exactly:
        // storage does not mutate the identity the read paths report.
        let all = store.usage_events(&organization, 0, 100).unwrap();
        let seen: Vec<u64> = all.iter().map(|row| row.event.task_id).collect();
        assert_eq!(seen, boundaries.to_vec());
    }
}

/// Legacy v5 rows migrate to the exact reversible encoding — including the
/// previously CLAMPED high ids, recovered from the payload (the only
/// authority for ids above `i64::MAX`, which SQLite JSON cannot parse
/// exactly) — and the index travels with the rebuilt column.
#[test]
fn legacy_integer_task_ids_migrate_to_exact_reversible_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let ids = [
        1u64,
        7,
        i64::MAX as u64,
        i64::MAX as u64 + 1,
        u64::MAX - 1,
        u64::MAX,
    ];
    let payloads: Vec<String> = ids.iter().map(|id| legacy_payload(*id)).collect();
    let rows: Vec<(i64, String, String)> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            // The old write path: everything above i64::MAX clamped.
            let legacy = if *id > i64::MAX as u64 {
                i64::MAX
            } else {
                *id as i64
            };
            (legacy, format!("uev_{id:016x}"), payloads[i].clone())
        })
        .collect();
    legacy_task_id_db(&path, &rows);

    let store = SqliteControlPlaneStore::open(&path).unwrap();
    for id in ids {
        let page = store
            .usage_events_of_task(&org("org_a"), id, 0, 100)
            .unwrap();
        assert_eq!(page.len(), 1, "task {id:#x} must have exactly its own row");
        assert_eq!(page[0].event.id.as_str(), format!("uev_{id:016x}"));
        assert_eq!(page[0].event.task_id, id, "clamped ids recover exactly");
    }
    let all = store.usage_events(&org("org_a"), 0, 100).unwrap();
    let seen: Vec<u64> = all.iter().map(|row| row.event.task_id).collect();
    assert_eq!(seen, ids.to_vec());
    drop(store);

    // The committed schema is the canonical text encoding; the staging
    // table is gone and the (org, task_id, event_seq) index still serves
    // the task filter.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert!(version >= 6, "the ladder advanced past v6: {version}");
    for id in ids {
        let hex: String = conn
            .query_row(
                "SELECT task_id FROM usage_event WHERE id = ?1",
                rusqlite::params![format!("uev_{id:016x}")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hex, task_id_text(id), "canonical 16-hex text for {id:#x}");
    }
    let staging: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'usage_event_v6_task_id_carry'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        staging, 0,
        "the staging table is dropped after finalization"
    );
    let plan: Vec<String> = conn
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT event_seq, payload FROM usage_event
             WHERE organization_id = ?1 AND task_id = ?2 AND event_seq > ?3
             ORDER BY event_seq LIMIT ?4",
        )
        .unwrap()
        .query_map(
            rusqlite::params!["org_a", task_id_text(u64::MAX), 0i64, 10i64],
            |r| r.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("idx_usage_event_org_task")),
        "the rebuilt task index still serves the filter: {plan:?}"
    );
    drop(conn);
    // A second open (carry table gone) is a no-op: the identities stay
    // exactly recoverable across restarts.
    let store = SqliteControlPlaneStore::open(&path).unwrap();
    for id in ids {
        let page = store
            .usage_events_of_task(&org("org_a"), id, 0, 100)
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].event.task_id, id);
    }
}

/// A legacy row that cannot be recovered EXACTLY refuses the migration
/// typed (no guessing, no partial conversion): the v6 transaction rolls
/// back, the ladder stays at v5, and the legacy row is untouched.
#[test]
fn unrecoverable_legacy_task_ids_refuse_the_migration_without_guessing() {
    let cases: Vec<(&str, i64, String, &str)> = vec![
        (
            "payload disagrees with an exact stored id",
            9,
            legacy_payload(5),
            "disagrees with the exact stored id",
        ),
        (
            "clamped sentinel with a lower payload id",
            i64::MAX,
            legacy_payload(5),
            "clamped sentinel",
        ),
        (
            "unreadable payload",
            7,
            "{ this is not json".to_string(),
            "payload is unreadable",
        ),
        ("zero id", 0, legacy_payload(0), "not a valid non-zero id"),
    ];
    for (name, legacy, payload, needle) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("billing.db");
        legacy_task_id_db(&path, &[(legacy, "uev_legacy".to_string(), payload)]);
        let err = SqliteControlPlaneStore::open(&path)
            .err()
            .unwrap_or_else(|| panic!("{name}: the migration must refuse"));
        let message = err.to_string();
        assert!(message.contains(needle), "{name}: {message}");
        assert!(
            message.contains("refusing to guess") || message.contains("unreadable"),
            "{name}: the refusal is explicit: {message}"
        );
        // Nothing migrated, nothing lost.
        let conn = rusqlite::Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 5, "{name}: the ladder did not advance");
        let stored: i64 = conn
            .query_row("SELECT task_id FROM usage_event LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, legacy, "{name}: the legacy row is untouched");
    }
}

// --------------------------------------------- P1 credit amount domain

/// `i64::MAX` micro-units is the largest legal credit amount and it stays
/// EXACT end to end: the JSON payload, the signed SQL column and every
/// balance/read projection report the same value, and the fold's boundary
/// arithmetic (grant + refund - settled consume) neither saturates nor
/// clamps.
#[test]
fn credit_amount_i64_max_is_exact_in_payload_and_sql_on_both_backends() {
    let max = i64::MAX as u64;
    let mem = MemoryBillingStore::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let sqlite = SqliteControlPlaneStore::open(&path).unwrap();
    let organization = org("org_a");
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        for (id, kind, reference) in [
            ("crd_max", CreditKind::Grant, None),
            ("crd_use", CreditKind::Consume, None),
            ("crd_settle", CreditKind::Settle, Some("crd_use")),
            ("crd_refund", CreditKind::Refund, Some("crd_use")),
        ] {
            assert_eq!(
                store
                    .append_credit_entry(&credit_entry(id, kind, max, reference))
                    .unwrap(),
                CreditAppend::Appended,
                "{id} at i64::MAX appends"
            );
        }
        let balance = store.credit_balance(&organization).unwrap();
        assert_eq!(balance.granted_micro, max);
        assert_eq!(balance.consumed_micro, max);
        assert_eq!(balance.refunded_micro, max);
        assert_eq!(balance.held_micro, 0);
        assert_eq!(
            balance.balance_micro().unwrap(),
            max,
            "grant + refund - settled consume at i64::MAX stays exact"
        );
        let page = store.credit_entries(&organization, 0, 10).unwrap();
        assert_eq!(page.len(), 4);
        for row in &page {
            assert_eq!(row.entry.amount_micro, max, "the read path is exact");
        }
    }
    // SQL column == payload, bit for bit: no clamp exists anywhere.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let (stored, payload): (i64, String) = conn
        .query_row(
            "SELECT amount_micro, payload FROM credit_entry WHERE id = 'crd_max'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        stored,
        i64::MAX,
        "the column holds the exact boundary value"
    );
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        value.get("amount_micro").and_then(|v| v.as_u64()),
        Some(max),
        "the payload holds the same exact value"
    );
    assert_eq!(u64::try_from(stored).unwrap(), max);
}

/// The settle guard must cover only the DELTA above the pending hold. The
/// hold is already inside `balance_micro()` (an unsettled consume counts as
/// consumed), so adding it back double-counts it and lets a settle spend
/// free + hold. Both backends must refuse identically, write nothing, and
/// leave no hidden debt (a saturated balance would hide it as 0).
#[test]
fn settle_delta_never_double_counts_the_pending_hold_on_both_backends() {
    let mem = MemoryBillingStore::new();
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        // grant 1000, hold 400 -> free 600.
        assert_eq!(
            store
                .append_credit_entry(&credit_entry("crd_g", CreditKind::Grant, 1_000, None))
                .unwrap(),
            CreditAppend::Appended
        );
        assert_eq!(
            store
                .append_credit_entry(&credit_entry("crd_hold", CreditKind::Consume, 400, None))
                .unwrap(),
            CreditAppend::Appended
        );
        let balance = store.credit_balance(&organization).unwrap();
        assert_eq!(balance.held_micro, 400);
        assert_eq!(balance.balance_micro().unwrap(), 600, "free = 600");
        // The exploit: settle 1400 = hold 400 + free 600 + 400 double-counted.
        // The delta above the hold is 1000, the free balance is 600: refused.
        assert_eq!(
            store
                .append_credit_entry(&credit_entry(
                    "crd_s1400",
                    CreditKind::Settle,
                    1_400,
                    Some("crd_hold")
                ))
                .unwrap_err(),
            BillingStoreError::Credit(CreditAppendRefusal::InsufficientCredits {
                available: 600,
                requested: 1_000,
            }),
            "free + hold must never be spendable"
        );
        // One micro above the hold is refused by exactly one micro.
        assert_eq!(
            store
                .append_credit_entry(&credit_entry(
                    "crd_s1001",
                    CreditKind::Settle,
                    1_001,
                    Some("crd_hold")
                ))
                .unwrap_err(),
            BillingStoreError::Credit(CreditAppendRefusal::InsufficientCredits {
                available: 600,
                requested: 601,
            })
        );
        // Nothing was written by either refusal.
        assert_eq!(store.credit_entries(&organization, 0, 10).unwrap().len(), 2);
        // Exactly the hold plus the free balance (1000) is allowed.
        assert_eq!(
            store
                .append_credit_entry(&credit_entry(
                    "crd_s1000",
                    CreditKind::Settle,
                    1_000,
                    Some("crd_hold")
                ))
                .unwrap(),
            CreditAppend::Appended
        );
        let balance = store.credit_balance(&organization).unwrap();
        assert_eq!(balance.granted_micro, 1_000);
        assert_eq!(balance.consumed_micro, 1_000);
        assert_eq!(balance.refunded_micro, 0);
        assert_eq!(balance.held_micro, 0);
        assert_eq!(
            balance.balance_micro().unwrap(),
            0,
            "the accepted settle spent exactly the funded credits: no hidden debt"
        );
        // The corrected ledger holds no phantom credit: one more consume is
        // refused, and the consume is settled exactly once.
        assert!(matches!(
            store
                .append_credit_entry(&credit_entry("crd_c1", CreditKind::Consume, 1, None))
                .unwrap_err(),
            BillingStoreError::Credit(CreditAppendRefusal::InsufficientCredits {
                available: 0,
                requested: 1,
            })
        ));
        assert_eq!(
            store
                .append_credit_entry(&credit_entry(
                    "crd_s1001b",
                    CreditKind::Settle,
                    1_001,
                    Some("crd_hold")
                ))
                .unwrap_err(),
            BillingStoreError::Credit(CreditAppendRefusal::AlreadySettled {
                reference: "crd_hold".into(),
            })
        );
    }
}

/// Monetary aggregation is CHECKED on both backends: an aggregate that
/// leaves the u64 domain is a typed overflow (never a saturated total that
/// would hide debits/funding), and a ledger whose debits exceed its funding
/// is typed corruption (never a saturated 0 balance hiding debt). The
/// refused append writes nothing, and the read path refuses too.
#[test]
fn credit_aggregate_overflow_and_hidden_debt_are_typed_never_saturated() {
    let max = i64::MAX as u64;
    let mem = MemoryBillingStore::new();
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        assert_eq!(
            store
                .append_credit_entry(&credit_entry("crd_o1", CreditKind::Grant, max, None))
                .unwrap(),
            CreditAppend::Appended
        );
        assert_eq!(
            store
                .append_credit_entry(&credit_entry("crd_o2", CreditKind::Grant, max, None))
                .unwrap(),
            CreditAppend::Appended
        );
        // 2 * i64::MAX still fits u64 exactly (u64::MAX - 1): no saturation,
        // no error at the boundary.
        assert_eq!(
            store.credit_balance(&organization).unwrap().granted_micro,
            u64::MAX - 1
        );
        // The third boundary grant would overflow the aggregate: refused
        // typed BEFORE the write, so the ledger never becomes unreadable.
        match store
            .append_credit_entry(&credit_entry("crd_o3", CreditKind::Grant, max, None))
            .unwrap_err()
        {
            BillingStoreError::Ledger(CreditLedgerError::Overflow {
                field: "granted_micro",
                left,
                right,
            }) => {
                assert_eq!(left, u64::MAX - 1);
                assert_eq!(right, max);
            }
            other => panic!("expected a typed granted_micro overflow, got {other:?}"),
        }
        // The refused append wrote nothing and the ledger still reads
        // exactly (no saturation, no unreadable state).
        assert_eq!(
            store.credit_entries(&organization, 0, 10).unwrap().len(),
            2,
            "the overflowing append wrote nothing"
        );
        assert_eq!(
            store.credit_balance(&organization).unwrap().granted_micro,
            u64::MAX - 1
        );
        // A refund can overflow FUNDING even when the refund itself is
        // exact: consume max against the two grants, then refund max (the
        // guard allows it: it is the full hold) — the funded aggregate would
        // leave u64, so it is refused typed before the write.
        assert_eq!(
            store
                .append_credit_entry(&credit_entry("crd_o_use", CreditKind::Consume, max, None))
                .unwrap(),
            CreditAppend::Appended
        );
        assert_eq!(
            store
                .credit_balance(&organization)
                .unwrap()
                .balance_micro()
                .unwrap(),
            max,
            "free = funding (u64::MAX - 1) minus the hold (i64::MAX)"
        );
        match store
            .append_credit_entry(&credit_entry(
                "crd_o_refund",
                CreditKind::Refund,
                max,
                Some("crd_o_use"),
            ))
            .unwrap_err()
        {
            BillingStoreError::Ledger(CreditLedgerError::Overflow {
                field: "funded_micro",
                ..
            }) => {}
            other => panic!("expected a typed funded_micro overflow, got {other:?}"),
        }
        // Nothing was written by the refused refund: the exact hold and the
        // free balance stay intact.
        let balance = store.credit_balance(&organization).unwrap();
        assert_eq!(balance.refunded_micro, 0);
        assert_eq!(balance.consumed_micro, max);
        assert_eq!(balance.balance_micro().unwrap(), max);
        assert_eq!(store.credit_entries(&organization, 0, 10).unwrap().len(), 3);
    }
    // Raw corrupt ledgers (planted through SQL, bypassing the guards): the
    // typed corruption surfaces at every arithmetic boundary, never as a
    // saturated number.
    fn plant(path: &std::path::Path, rows: &[CreditEntry]) {
        drop(SqliteControlPlaneStore::open(path).unwrap());
        let conn = rusqlite::Connection::open(path).unwrap();
        for (index, entry) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO credit_entry
                    (id, organization_id, entry_seq, billing_account_id, kind, reference,
                     usage_event_id, idempotency_key, amount_micro, occurred_at_ms, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    entry.id.as_str(),
                    "org_a",
                    index as i64 + 1,
                    "acct_1",
                    entry.kind.as_str(),
                    entry.reference.as_ref().map(|r| r.as_str()),
                    None::<String>,
                    None::<String>,
                    i64::try_from(entry.amount_micro).unwrap(),
                    1i64,
                    serde_json::to_string(entry).unwrap(),
                ],
            )
            .unwrap();
        }
    }
    // A pre-existing aggregate overflow (three boundary grants planted
    // through SQL) refuses the READ typed: never a saturated u64::MAX total.
    let overflow_path = dir.path().join("overflow.db");
    plant(
        &overflow_path,
        &[
            credit_entry("crd_x1", CreditKind::Grant, max, None),
            credit_entry("crd_x2", CreditKind::Grant, max, None),
            credit_entry("crd_x3", CreditKind::Grant, max, None),
        ],
    );
    let store = SqliteControlPlaneStore::open(&overflow_path).unwrap();
    match store.credit_balance(&organization).unwrap_err() {
        BillingStoreError::Ledger(CreditLedgerError::Overflow {
            field: "granted_micro",
            ..
        }) => {}
        other => panic!("the read path must refuse the overflow typed, got {other:?}"),
    }
    // Debt: a consume of 500 against a grant of 100.
    let debt_path = dir.path().join("debt.db");
    plant(
        &debt_path,
        &[
            credit_entry("crd_dg", CreditKind::Grant, 100, None),
            credit_entry("crd_dc", CreditKind::Consume, 500, None),
        ],
    );
    let store = SqliteControlPlaneStore::open(&debt_path).unwrap();
    // The raw fold is readable (the fields are the ledger), but the DERIVED
    // free balance refuses typed: never a saturated 0 that hides the debt.
    match store
        .credit_balance(&organization)
        .unwrap()
        .balance_micro()
        .unwrap_err()
    {
        CreditLedgerError::Corrupt { detail } => {
            assert!(detail.contains("500"), "the debits are named: {detail}");
            assert!(detail.contains("100"), "the funding is named: {detail}");
            assert!(detail.contains("400"), "the hidden debt is named: {detail}");
        }
        other => panic!("expected typed hidden-debt corruption, got {other:?}"),
    }
    // Refund-over-spend: 600 refunded against a SETTLED 500 consume. The
    // plain balance still folds, but the refund guard refuses typed instead
    // of masking the excess to a zero refundable amount. (Over-releasing a
    // still-PENDING hold is itself corruption and is refused on read.)
    let refund_path = dir.path().join("refund.db");
    plant(
        &refund_path,
        &[
            credit_entry("crd_rg", CreditKind::Grant, 1_000, None),
            credit_entry("crd_rc", CreditKind::Consume, 500, None),
            credit_entry("crd_rs", CreditKind::Settle, 500, Some("crd_rc")),
            credit_entry("crd_rr", CreditKind::Refund, 600, Some("crd_rc")),
        ],
    );
    let store = SqliteControlPlaneStore::open(&refund_path).unwrap();
    assert_eq!(
        store
            .credit_balance(&organization)
            .unwrap()
            .balance_micro()
            .unwrap(),
        1_100
    );
    match store
        .append_credit_entry(&credit_entry(
            "crd_rr2",
            CreditKind::Refund,
            1,
            Some("crd_rc"),
        ))
        .unwrap_err()
    {
        BillingStoreError::Ledger(CreditLedgerError::Corrupt { detail }) => {
            assert!(detail.contains("600"), "the refunds are named: {detail}");
            assert!(detail.contains("500"), "the spend is named: {detail}");
        }
        other => panic!("expected typed refund-over-spend corruption, got {other:?}"),
    }
    assert_eq!(
        store.credit_entries(&organization, 0, 10).unwrap().len(),
        4,
        "the corrupt-ledger append wrote nothing"
    );
}

/// Every out-of-domain amount (`i64::MAX + 1`, `u64::MAX`) is refused TYPED
/// at every entry point — entry validation, credit grant/consume/settle/
/// refund, and the store append on both backends — naming the field
/// `amount_micro` and the `i64::MAX` limit, and writes nothing. The zero
/// rules are unchanged.
#[test]
fn credit_amounts_above_i64_max_are_refused_typed_at_every_entry_point() {
    let max = i64::MAX as u64;
    fn refused(err: ControlPlaneError, over: u64) {
        match err {
            ControlPlaneError::Malformed(msg) => {
                assert!(msg.contains("amount_micro"), "names the field: {msg}");
                assert!(
                    msg.contains(&(i64::MAX as u64).to_string()),
                    "names the limit: {msg}"
                );
                assert!(msg.contains(&over.to_string()), "names the value: {msg}");
            }
            other => panic!("expected Malformed naming amount_micro, got {other:?}"),
        }
    }
    let mem = MemoryBillingStore::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let sqlite = SqliteControlPlaneStore::open(&path).unwrap();
    let organization = org("org_a");
    let acct = account("acct_1");
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    // A real consume to aim settle/refund at.
    service
        .grant_credits(&organization, &acct, 10, "seed", Some("seed"))
        .unwrap();
    service
        .consume_before_call(&organization, &acct, 5, None, "hold", Some("hold"))
        .unwrap();
    let consume = service
        .credit_entry_id_by_idempotency_key(&organization, "hold")
        .unwrap()
        .unwrap();
    for over in [max + 1, u64::MAX] {
        // 1. The constructor/validation boundary.
        match credit_entry("crd_bad", CreditKind::Grant, over, None).validate() {
            Err(e) => refused(e, over),
            Ok(()) => panic!("validate must refuse {over}"),
        }
        // 2. Every service entry point.
        refused(
            service
                .grant_credits(&organization, &acct, over, "grant", None)
                .unwrap_err(),
            over,
        );
        refused(
            service
                .consume_before_call(&organization, &acct, over, None, "consume", None)
                .unwrap_err(),
            over,
        );
        refused(
            service
                .settle_consume(&organization, &acct, &consume, over, "settle")
                .unwrap_err(),
            over,
        );
        refused(
            service
                .refund_consume(&organization, &acct, &consume, over, "refund")
                .unwrap_err(),
            over,
        );
        // 3. The store append boundary on both backends.
        for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
            match store
                .append_credit_entry(&credit_entry(
                    "crd_store_bad",
                    CreditKind::Grant,
                    over,
                    None,
                ))
                .unwrap_err()
            {
                BillingStoreError::Malformed(msg) => {
                    assert!(msg.contains("amount_micro"), "{msg}");
                    assert!(msg.contains(&max.to_string()), "{msg}");
                }
                other => panic!("the store must refuse {over} typed, got {other:?}"),
            }
        }
    }
    // Nothing was written by any refused call.
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.granted_micro, 10);
    assert_eq!(balance.consumed_micro, 5);
    assert_eq!(balance.held_micro, 5);
    assert_eq!(balance.refunded_micro, 0);
    assert_eq!(balance.balance_micro().unwrap(), 5);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM credit_entry", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "refused store writes wrote nothing");
    drop(conn);
    // The zero rules are unchanged at both boundaries.
    match credit_entry("crd_zero", CreditKind::Grant, 0, None).validate() {
        Err(ControlPlaneError::Malformed(msg)) => {
            assert!(msg.contains("non-zero amount"), "{msg}")
        }
        other => panic!("zero must stay refused, got {other:?}"),
    }
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        match store
            .append_credit_entry(&credit_entry("crd_zero", CreditKind::Grant, 0, None))
            .unwrap_err()
        {
            BillingStoreError::Malformed(msg) => assert!(msg.contains("non-zero amount"), "{msg}"),
            other => panic!("the store must refuse zero typed, got {other:?}"),
        }
    }
}

/// A legacy `credit_entry` row written by the old clamping writer (stored
/// `i64::MAX` sentinel with a payload above the domain bound) is surfaced
/// TYPED at every read/mutation boundary: never silently reinterpreted as
/// `i64::MAX`, never folded into a balance, and never mistaken for a replayed
/// request. A legacy row whose payload is exactly `i64::MAX` (the legal
/// boundary) keeps reading exactly. No in-place migration rewrites these
/// rows: the true amount has no exact in-domain value, so the documented
/// decision is refusal with the row named (the v6 task-id precedent).
#[test]
fn legacy_clamped_credit_amounts_are_surfaced_typed_and_never_reinterpreted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let organization = org("org_a");
    // Create the full current schema, then plant the legacy rows through raw
    // SQL exactly as the old writer stored them.
    drop(SqliteControlPlaneStore::open(&path).unwrap());
    let exact = serde_json::to_string(&credit_entry(
        "crd_exact",
        CreditKind::Grant,
        i64::MAX as u64,
        None,
    ))
    .unwrap();
    let clamped = serde_json::to_string(&credit_entry(
        "crd_clamped",
        CreditKind::Grant,
        u64::MAX,
        None,
    ))
    .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    for (seq, id, amount, key, payload) in [
        (1i64, "crd_exact", i64::MAX, None, exact),
        (2i64, "crd_clamped", i64::MAX, Some("legacy-key"), clamped),
    ] {
        conn.execute(
            "INSERT INTO credit_entry
                (id, organization_id, entry_seq, billing_account_id, kind, reference,
                 usage_event_id, idempotency_key, amount_micro, occurred_at_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                id,
                "org_a",
                seq,
                "acct_1",
                "grant",
                None::<String>,
                None::<String>,
                key,
                amount,
                1i64,
                payload,
            ],
        )
        .unwrap();
    }
    drop(conn);

    let store = SqliteControlPlaneStore::open(&path).unwrap();
    let refuse = |err: BillingStoreError| {
        assert!(
            matches!(err, BillingStoreError::Malformed(_)),
            "the legacy row is refused typed: {err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("crd_clamped"), "{message}");
        assert!(message.contains(&i64::MAX.to_string()), "{message}");
        assert!(message.contains(&u64::MAX.to_string()), "{message}");
        assert!(message.contains("clamped"), "{message}");
    };
    refuse(store.credit_balance(&organization).unwrap_err());
    refuse(store.credit_entries(&organization, 0, 10).unwrap_err());
    refuse(
        store
            .credit_entry_by_idempotency_key(&organization, "legacy-key")
            .unwrap_err(),
    );
    // A replay of the legacy idempotency key with the boundary amount is NOT
    // accepted as a Duplicate of the clamped row.
    let mut replay = credit_entry("crd_replay", CreditKind::Grant, i64::MAX as u64, None);
    replay.idempotency_key = Some("legacy-key".into());
    refuse(store.append_credit_entry(&replay).unwrap_err());
    // A fresh legal append is refused too: the balance cannot be trusted
    // while the clamped row exists.
    refuse(
        store
            .append_credit_entry(&credit_entry("crd_new", CreditKind::Grant, 1, None))
            .unwrap_err(),
    );
    // The legacy row is untouched (append-only: nothing rewrote it).
    let conn = rusqlite::Connection::open(&path).unwrap();
    let stored: i64 = conn
        .query_row(
            "SELECT amount_micro FROM credit_entry WHERE id = 'crd_clamped'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, i64::MAX, "the legacy row is never rewritten");
    // Removing the out-of-domain row makes the ledger readable again: the
    // legal `i64::MAX` boundary row reads exactly.
    conn.execute("DELETE FROM credit_entry WHERE id = 'crd_clamped'", [])
        .unwrap();
    drop(conn);
    let balance = store.credit_balance(&organization).unwrap();
    assert_eq!(balance.granted_micro, i64::MAX as u64);
    let page = store.credit_entries(&organization, 0, 10).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].entry.amount_micro, i64::MAX as u64);
}

/// Adversarial: an authoritative usage fold that leaves the `u64` domain
/// refuses TYPED at every call site — the service fold, the entitlement
/// snapshot and the admission gate — and the gate's decision is EXPLICIT
/// and fail-closed (`ledger_overflow:<field>`), never a saturated total
/// admitted as under-quota. The exact boundary below the overflow stays
/// readable and exact.
#[test]
fn usage_overflow_is_typed_at_the_fold_snapshot_and_admission_decision() {
    let max = i64::MAX as u64;
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    subscribe(&service, &organization, None);
    // Two boundary managed events reach u64::MAX - 1 exactly: no saturation
    // and no error AT the boundary.
    for index in 0..2u64 {
        let mut event = usage_event(&format!("uev_o{index}"));
        event.provider_cost_micro = max;
        service.record_usage(&event).unwrap();
    }
    let snapshot = service.entitlement_snapshot(&organization).unwrap();
    assert_eq!(
        snapshot.managed_spend_micro,
        u64::MAX - 1,
        "the boundary stays exact"
    );
    // The third boundary event overflows the authoritative money aggregate.
    let mut third = usage_event("uev_o2");
    third.provider_cost_micro = max;
    service.record_usage(&third).unwrap();
    match service.fold(&organization) {
        Err(ControlPlaneError::Ledger(CreditLedgerError::Overflow {
            field: "provider_cost_micro",
            left,
            right,
        })) => {
            assert_eq!(left, u64::MAX - 1);
            assert_eq!(right, max);
        }
        other => panic!("expected the typed fold overflow, got {other:?}"),
    }
    // The derived snapshot refuses the same typed way: never a saturated
    // total, never a silently readable spend number.
    match service.entitlement_snapshot(&organization) {
        Err(ControlPlaneError::Ledger(CreditLedgerError::Overflow {
            field: "provider_cost_micro",
            ..
        })) => {}
        other => panic!("expected the typed snapshot overflow, got {other:?}"),
    }
    // The admission decision is EXPLICIT and documented: the overflow is
    // treated exactly like an exhausted quota (fail closed), names the
    // overflowed field, and admits nothing.
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest {
                provider: Some("openai".into()),
                observed: ObservedUsage {
                    estimated_provider_cost_micro: 1,
                    ..Default::default()
                },
                ..AdmissionRequest::boundary(AdmissionBoundary::NewProviderAttempt)
            },
        )
        .unwrap_err();
    assert_eq!(
        denied.limit,
        format!("{CAUSE_LEDGER_OVERFLOW}:provider_cost_micro")
    );
    assert_eq!(denied.limit_value, None);
    assert_eq!(denied.observed, u64::MAX - 1);
    assert_eq!(denied.boundary, AdmissionBoundary::NewProviderAttempt);
    // The in-flight invariant is untouched: a continuation is still admitted
    // while the ledger refuses (an overflow never interrupts a transaction).
    assert_eq!(
        service
            .check_admission(
                &organization,
                &AdmissionRequest::boundary(AdmissionBoundary::RollbackContinuation)
            )
            .unwrap(),
        Admission::InFlightContinuation
    );
    // The token-quota numerator is checked the same way: three token buckets
    // each at the boundary overflow the sum, and the refusal names the exact
    // bucket that crossed it.
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_t");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    subscribe(&service, &organization, None);
    for (index, unit) in [
        UsageUnit::InputTokens,
        UsageUnit::OutputTokens,
        UsageUnit::CacheReadTokens,
    ]
    .into_iter()
    .enumerate()
    {
        let mut event = usage_event(&format!("uev_t{index}"));
        event.organization_id = organization.clone();
        event.billing_account_id = acct.clone();
        event.unit = unit;
        event.quantity = max;
        event.provider_cost_micro = 0;
        service.record_usage(&event).unwrap();
    }
    match service.entitlement_snapshot(&organization) {
        Err(ControlPlaneError::Ledger(CreditLedgerError::Overflow {
            field: "cache_read_tokens",
            ..
        })) => {}
        other => panic!("expected the typed token-numerator overflow, got {other:?}"),
    }
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest::boundary(AdmissionBoundary::NewChildSpawn),
        )
        .unwrap_err();
    assert_eq!(
        denied.limit,
        format!("{CAUSE_LEDGER_OVERFLOW}:cache_read_tokens")
    );
}

/// Adversarial: the configured `default_plan` belongs to the configured
/// `[billing] organization` ALONE. A foreign/unprovisioned organization gets
/// an unentitled snapshot (`plan_found: false`, no plan id, empty
/// features/limits) and the admission gate refuses it typed naming the plan;
/// an active subscription always wins, regardless of organization.
#[test]
fn the_configured_default_plan_is_scoped_to_the_configured_organization() {
    let mut config = configured();
    config.plans.insert(
        "team".into(),
        PlanConfig {
            period_ms: None,
            plan_id: "team".into(),
            features: [FEATURE_BYOK.to_string()].into_iter().collect(),
            limits: [(LIMIT_MAX_ACTIVE_TASKS.to_string(), 9)]
                .into_iter()
                .collect(),
        },
    );
    let configured_organization = org("org_configured");
    let foreign_organization = org("org_foreign");
    let service = EntitlementService::new_for_organization(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
        config,
        Some(configured_organization.clone()),
    )
    .unwrap();

    // The configured organization resolves the configured default plan.
    let snapshot = service
        .entitlement_snapshot(&configured_organization)
        .unwrap();
    assert_eq!(snapshot.plan_id.as_deref(), Some("pro"));
    assert!(snapshot.plan_found);
    assert!(!snapshot.features.is_empty());
    assert!(!snapshot.limits.is_empty());

    // A foreign/unprovisioned organization never inherits it: no plan, no
    // features, no limits, and the gate refuses TYPED (never a fabricated
    // entitlement).
    let snapshot = service.entitlement_snapshot(&foreign_organization).unwrap();
    assert_eq!(snapshot.plan_id, None, "no fabricated plan id");
    assert!(!snapshot.plan_found, "no fabricated plan");
    assert!(snapshot.features.is_empty(), "no inherited features");
    assert!(snapshot.limits.is_empty(), "no inherited limits");
    assert!(!snapshot.subscription_active);
    let denied = service
        .check_admission(
            &foreign_organization,
            &AdmissionRequest::boundary(AdmissionBoundary::NewTask),
        )
        .unwrap_err();
    assert_eq!(denied.limit, CAUSE_PLAN);
    assert_eq!(denied.boundary, AdmissionBoundary::NewTask);

    // A subscription ALWAYS wins regardless of organization: the foreign
    // organization's own active subscription resolves its own plan (not the
    // configured default).
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_foreign").unwrap(),
            organization: foreign_organization.clone(),
            plan_id: "team".into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: 0,
        })
        .unwrap();
    let snapshot = service.entitlement_snapshot(&foreign_organization).unwrap();
    assert_eq!(snapshot.plan_id.as_deref(), Some("team"));
    assert!(snapshot.plan_found);
    assert_eq!(snapshot.limit(LIMIT_MAX_ACTIVE_TASKS), Some(9));

    // And a subscription beats the default on the configured organization
    // too.
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_configured").unwrap(),
            organization: configured_organization.clone(),
            plan_id: "team".into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: 0,
        })
        .unwrap();
    let snapshot = service
        .entitlement_snapshot(&configured_organization)
        .unwrap();
    assert_eq!(snapshot.plan_id.as_deref(), Some("team"));
    assert!(snapshot.plan_found);
    assert_eq!(snapshot.limit(LIMIT_MAX_ACTIVE_TASKS), Some(9));
}

/// The account overview is derived from durable rows and reports every
/// capability this build does not implement as a TYPED unavailable state: no
/// fake paid/invoice/seat/tax/currency state is ever inferred from a plan.
#[test]
fn overview_is_typed_and_trials_are_bounded_and_derived() {
    let root = tempfile::tempdir().unwrap();
    let store: Arc<dyn BillingStore> =
        Arc::new(SqliteControlPlaneStore::open(&root.path().join("billing.db")).unwrap());
    let clock = Arc::new(ManualClock::new(1_000));
    let service = EntitlementService::new_for_organization(
        store,
        clock.clone(),
        configured(),
        Some(org("org_overview")),
    )
    .unwrap();
    let organization = org("org_overview");
    service
        .ensure_account(&organization, &account("acct"), "Local", true)
        .unwrap();

    let overview = service.overview(&organization).unwrap();
    assert_eq!(
        overview.account.as_ref().map(|a| a.id.clone()),
        Some(account("acct"))
    );
    assert!(overview.plan_found);
    assert!(!overview.subscription_active, "no subscription row yet");
    assert_eq!(overview.trial.state, "none");
    assert_eq!(overview.payment.state, BillingCapabilityState::Unavailable);
    assert_eq!(
        overview.payment.cause,
        Some(CAUSE_PAYMENT_PROVIDER_NOT_CONFIGURED)
    );
    assert_eq!(overview.invoices.cause, Some(CAUSE_INVOICES_NOT_AVAILABLE));
    assert_eq!(overview.seats.cause, Some(CAUSE_SEAT_BILLING_NOT_MODELED));
    assert_eq!(overview.tax.cause, Some(CAUSE_TAX_NOT_CONFIGURED));
    assert_eq!(overview.currency.cause, Some(CAUSE_CURRENCY_NOT_CONFIGURED));
    assert_eq!(
        overview.usage_export.state,
        BillingCapabilityState::Available
    );
    assert_eq!(overview.usage_export_formats, ["csv", "json"]);

    // A trial is time-bounded by expires_ms and derives active -> expired.
    let (trial, replayed) = service
        .change_subscription(
            &organization,
            "pro",
            SubscriptionStatus::Trialing,
            Some(1_500),
        )
        .unwrap();
    assert!(!replayed);
    assert_eq!(trial.status, SubscriptionStatus::Trialing);
    assert_eq!(trial.started_ms, 1_000);
    let overview = service.overview(&organization).unwrap();
    assert_eq!(overview.trial.state, "active");
    assert!(overview.subscription_active);
    clock.advance(1_000);
    let overview = service.overview(&organization).unwrap();
    assert_eq!(overview.trial.state, "expired");
    assert!(
        !overview.subscription_active,
        "a stale trialing row is inactive"
    );
    assert_eq!(overview.trial.ends_ms, Some(1_500));

    // An open-ended trial is refused (it would be an unbounded entitlement).
    let err = service
        .change_subscription(&organization, "pro", SubscriptionStatus::Trialing, None)
        .unwrap_err();
    assert!(err.to_string().contains("requires expires_ms"), "{err}");

    // A legacy trialing row WITHOUT an expiry (possible only in an older
    // payload) is never presented as active: an open-ended trial is not an
    // entitlement. The store refuses it on write now; the DERIVATION still
    // fails closed if such a row ever appears.
    let legacy_trial = Subscription {
        id: SubscriptionId::try_new("sub_legacy_trial").unwrap(),
        organization: organization.clone(),
        plan_id: "pro".into(),
        status: SubscriptionStatus::Trialing,
        started_ms: 1_000,
        expires_ms: None,
        updated_ms: 1_000,
    };
    assert!(!legacy_trial.is_active_at(1_000));
    assert_eq!(
        faktor_cloud::BillingTrialView::of(Some(&legacy_trial), 1_000).state,
        "expired"
    );
}

/// Plan changes are durable and immediate: a downgrade/upgrade rewrites the
/// SAME row, replaying the same request never bumps `updated_ms`, and cancel
/// denies the next admission without touching the plan row.
#[test]
fn plan_change_and_cancel_are_durable_idempotent_effects() {
    let store: Arc<dyn BillingStore> = Arc::new(MemoryBillingStore::new());
    let clock = Arc::new(ManualClock::new(10_000));
    let mut config = configured();
    config.plans.insert(
        "team".into(),
        PlanConfig {
            period_ms: None,
            plan_id: "team".into(),
            features: [FEATURE_MANAGED_PROVIDERS.to_string()]
                .into_iter()
                .collect(),
            limits: [(LIMIT_MAX_ACTIVE_TASKS.to_string(), 9)]
                .into_iter()
                .collect(),
        },
    );
    let service = EntitlementService::new_for_organization(
        store,
        clock.clone(),
        config,
        Some(org("org_change")),
    )
    .unwrap();
    let organization = org("org_change");
    service
        .ensure_account(&organization, &account("acct"), "Local", true)
        .unwrap();

    let (upgraded, replayed) = service
        .change_subscription(&organization, "team", SubscriptionStatus::Active, None)
        .unwrap();
    assert!(!replayed);
    assert_eq!(upgraded.started_ms, 10_000);
    assert_eq!(
        service
            .entitlement_snapshot(&organization)
            .unwrap()
            .plan_id
            .as_deref(),
        Some("team")
    );

    // The same request is a replay: no new row, no timestamp drift.
    clock.advance(50);
    let (same, replayed) = service
        .change_subscription(&organization, "team", SubscriptionStatus::Active, None)
        .unwrap();
    assert!(replayed);
    assert_eq!(same.updated_ms, upgraded.updated_ms);

    // A real downgrade rewrites the row and is visible at the next read.
    clock.advance(50);
    let (downgraded, replayed) = service
        .change_subscription(&organization, "pro", SubscriptionStatus::Active, None)
        .unwrap();
    assert!(!replayed);
    assert_eq!(downgraded.id, upgraded.id, "the organization keeps one row");
    assert_eq!(
        service
            .entitlement_snapshot(&organization)
            .unwrap()
            .plan_id
            .as_deref(),
        Some("pro")
    );

    // Cancellation is a durable effect; the next NEW admission is refused
    // with the subscription cause, and the plan row remains for history.
    let (canceled, replayed) = service.cancel_subscription(&organization).unwrap();
    assert!(!replayed);
    assert_eq!(canceled.status, SubscriptionStatus::Canceled);
    let snapshot = service.entitlement_snapshot(&organization).unwrap();
    assert!(!snapshot.subscription_active);
    assert_eq!(snapshot.plan_id.as_deref(), Some("pro"));
    let denied = service
        .check_admission(
            &organization,
            &AdmissionRequest::boundary(AdmissionBoundary::NewTask),
        )
        .unwrap_err();
    assert_eq!(denied.limit, CAUSE_SUBSCRIPTION_ACTIVE);
    assert!(
        service.cancel_subscription(&organization).unwrap().1,
        "replay"
    );

    // Unknown plans are refused, and `change` cannot smuggle in `canceled`.
    let err = service
        .change_subscription(&organization, "absent", SubscriptionStatus::Active, None)
        .unwrap_err();
    assert!(err.to_string().contains("not defined"), "{err}");
    let err = service
        .change_subscription(&organization, "pro", SubscriptionStatus::Canceled, None)
        .unwrap_err();
    assert!(err.to_string().contains("cancel action"), "{err}");
}

/// Adversarial: refunding a PENDING hold must cancel it, and the cancelled
/// consume can never then be settled. The old sequence (refund the hold,
/// then settle the actual) returned the credits AND charged the spend — free
/// managed usage. Both stores must refuse identically, write nothing, and
/// the fold must report no phantom hold.
#[test]
fn a_refunded_pending_hold_is_cancelled_and_never_settled_on_both_backends() {
    let mem = MemoryBillingStore::new();
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        assert_eq!(
            store
                .append_credit_entry(&credit_entry("crd_g", CreditKind::Grant, 1_000, None))
                .unwrap(),
            CreditAppend::Appended
        );
        assert_eq!(
            store
                .append_credit_entry(&credit_entry("crd_h", CreditKind::Consume, 400, None))
                .unwrap(),
            CreditAppend::Appended
        );
        // A partial release of a pending hold is refused: it would leave a
        // half-released hold a later settle could charge again.
        assert_eq!(
            store
                .append_credit_entry(&credit_entry(
                    "crd_partial",
                    CreditKind::Refund,
                    100,
                    Some("crd_h")
                ))
                .unwrap_err(),
            BillingStoreError::Credit(CreditAppendRefusal::PendingHoldRefundNotFull {
                reference: "crd_h".into(),
                refundable: 400,
                requested: 100,
            })
        );
        assert_eq!(
            store.credit_entries(&organization, 0, 10).unwrap().len(),
            2,
            "the partial refusal wrote nothing"
        );
        // Releasing the FULL hold is the documented pre-dispatch refund.
        assert_eq!(
            store
                .append_credit_entry(&credit_entry(
                    "crd_full",
                    CreditKind::Refund,
                    400,
                    Some("crd_h")
                ))
                .unwrap(),
            CreditAppend::Appended
        );
        let balance = store.credit_balance(&organization).unwrap();
        assert_eq!(balance.held_micro, 0, "the cancelled hold is released");
        assert_eq!(balance.pending_consumes, 0);
        assert_eq!(
            balance.balance_micro().unwrap(),
            1_000,
            "a released hold returns the credits"
        );
        // The exploit: settle the SAME consume after its refund. Refused
        // typed, nothing written, the balance unchanged.
        assert_eq!(
            store
                .append_credit_entry(&credit_entry(
                    "crd_settle_after",
                    CreditKind::Settle,
                    400,
                    Some("crd_h")
                ))
                .unwrap_err(),
            BillingStoreError::Credit(CreditAppendRefusal::SettleAfterRefund {
                reference: "crd_h".into(),
            })
        );
        assert_eq!(
            store.credit_entries(&organization, 0, 10).unwrap().len(),
            3,
            "the refused settle wrote nothing"
        );
        let balance = store.credit_balance(&organization).unwrap();
        assert_eq!(balance.refunded_micro, 400);
        assert_eq!(balance.consumed_micro, 400);
        assert_eq!(balance.balance_micro().unwrap(), 1_000);
    }
}

/// Adversarial: the same exploit through the service API (`consume_before_call`
/// -> `refund_consume` -> `settle_consume`) is refused typed and leaves the
/// append-only ledger byte-identical after the refusal.
#[test]
fn service_refund_then_settle_never_mints_credit() {
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    service
        .grant_credits(&organization, &acct, 1_000, "seed", None)
        .unwrap();
    let consume = match service
        .consume_before_call(&organization, &acct, 400, None, "attempt", Some("hold-1"))
        .unwrap()
    {
        CreditAppend::Appended => service
            .credit_entries_page(&organization, 0, 10)
            .unwrap()
            .into_iter()
            .find(|row| row.entry.kind == CreditKind::Consume)
            .map(|row| row.entry.id)
            .unwrap(),
        other => panic!("expected an appended consume, got {other:?}"),
    };
    service
        .refund_consume(&organization, &acct, &consume, 400, "pre_dispatch")
        .unwrap();
    let before = service.credit_entries_page(&organization, 0, 100).unwrap();
    let err = service
        .settle_consume(&organization, &acct, &consume, 400, "late settle")
        .unwrap_err();
    assert!(
        matches!(err, ControlPlaneError::Conflict(ref message) if message.contains("released hold")),
        "the refusal names the released hold: {err:?}"
    );
    let after = service.credit_entries_page(&organization, 0, 100).unwrap();
    assert_eq!(before, after, "a refused settle never mutates the ledger");
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 0);
    assert_eq!(balance.pending_consumes, 0);
    assert_eq!(balance.balance_micro().unwrap(), 1_000);
}

/// Adversarial: a correction changes the corrected VALUE, never the base
/// fact. Retagging managed spend as BYOK, moving it to another task/run/
/// account/provider/model, or changing its unit is refused typed with the
/// field named, and the fold stays byte-identical after the refusal.
#[test]
fn corrections_cannot_retag_the_base_fact_or_launder_managed_spend() {
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    let acct = account("acct_1");
    let other = account("acct_2");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    service
        .ensure_account(&organization, &other, "other", true)
        .unwrap();
    let base = usage_event("uev_base");
    service.record_usage(&base).unwrap();
    let fold_before = service.fold(&organization).unwrap();

    type Mutation = Box<dyn Fn(&mut UsageEvent)>;
    let mut attempts: Vec<(&str, Mutation)> = vec![
        (
            "category",
            Box::new(|event| event.category = SpendCategory::Byok),
        ),
        ("task_id", Box::new(|event| event.task_id = 8)),
        ("run_id", Box::new(|event| event.run_id = "2".into())),
        (
            "attempt_id",
            Box::new(|event| event.attempt_id = "other".into()),
        ),
        (
            "provider",
            Box::new(|event| event.provider = "anthropic".into()),
        ),
        ("model", Box::new(|event| event.model = "m2".into())),
        (
            "billing_account_id",
            Box::new(|event| event.billing_account_id = account("acct_2")),
        ),
        (
            "unit",
            Box::new(|event| {
                event.unit = UsageUnit::InputTokens;
                event.quantity = 2;
                event.provider_cost_micro = 0;
            }),
        ),
    ];
    for (field, mutate) in attempts.drain(..) {
        let mut correction = usage_event(&format!("uev_fix_{field}"));
        correction.correction_of = Some(base.id.clone());
        correction.reconciliation_state = ReconciliationState::Reconciled;
        mutate(&mut correction);
        let err = service.correct_usage(&correction).unwrap_err();
        assert!(
            matches!(&err, ControlPlaneError::Conflict(message) if message.contains(field)),
            "the {field} retag must be a typed conflict naming it: {err:?}"
        );
        assert_eq!(
            service.fold(&organization).unwrap(),
            fold_before,
            "a refused correction never changes the managed/BYOK split ({field})"
        );
    }
    // Control: a legitimate correction of the AMOUNT is still accepted and
    // supersedes the base in the fold.
    let mut correction = usage_event("uev_fix_amount");
    correction.correction_of = Some(base.id.clone());
    correction.provider_cost_micro = 125;
    correction.reconciliation_state = ReconciliationState::Reconciled;
    assert_eq!(
        service.correct_usage(&correction).unwrap(),
        faktor_cloud::UsageAppend::Appended
    );
    let fold = service.fold(&organization).unwrap();
    assert_eq!(fold.totals.managed_cost_micro, 125);
    assert_eq!(fold.totals.byok_cost_micro, 0);
}

/// Adversarial: credits can only ever be appended to an account that really
/// belongs to the organization; a settle/refund can never cross accounts; a
/// disabled account cannot open new funding/spend but can still close holds.
#[test]
fn credit_appends_are_account_bound_and_cross_account_references_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let mem = MemoryBillingStore::new();
    let organization = org("org_a");
    let foreign_org = org("org_b");
    // An account id is globally unique: a second organization can never
    // silently take over the row (which would alias its ledger identity).
    let shared = BillingAccount {
        id: account("acct_shared"),
        organization: organization.clone(),
        name: "shared".into(),
        managed: true,
        created_ms: 0,
        disabled: false,
    };
    let mut foreign = shared.clone();
    foreign.organization = foreign_org.clone();
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        store.put_billing_account(&shared).unwrap();
        let err = store.put_billing_account(&foreign).unwrap_err();
        assert!(
            matches!(err, BillingStoreError::Malformed(ref message) if message.contains("already owned")),
            "a cross-organization account takeover is refused typed: {err:?}"
        );
        assert_eq!(
            store
                .billing_account(&organization, &shared.id)
                .unwrap()
                .unwrap()
                .organization,
            organization
        );
    }

    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let acct = account("acct_1");
    let other = account("acct_2");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    service
        .ensure_account(&organization, &other, "other", true)
        .unwrap();
    service
        .ensure_account(&foreign_org, &account("acct_b"), "b", true)
        .unwrap();
    // Unknown account: refuses BEFORE any ledger append.
    let missing = account("acct_missing");
    assert!(matches!(
        service
            .grant_credits(&organization, &missing, 10, "ghost", None)
            .unwrap_err(),
        ControlPlaneError::NotFound(_)
    ));
    assert!(service
        .credit_entries_page(&organization, 0, 10)
        .unwrap()
        .is_empty());
    // A foreign organization's account is invisible (typed 404).
    assert!(matches!(
        service
            .grant_credits(&organization, &account("acct_b"), 10, "foreign", None)
            .unwrap_err(),
        ControlPlaneError::NotFound(_)
    ));
    assert!(service
        .credit_entries_page(&organization, 0, 10)
        .unwrap()
        .is_empty());
    // A disabled account can neither be granted nor consume.
    let disabled = account("acct_disabled");
    service
        .store()
        .put_billing_account(&BillingAccount {
            id: disabled.clone(),
            organization: organization.clone(),
            name: "disabled".into(),
            managed: true,
            created_ms: 0,
            disabled: true,
        })
        .unwrap();
    assert!(matches!(
        service
            .grant_credits(&organization, &disabled, 10, "disabled", None)
            .unwrap_err(),
        ControlPlaneError::Conflict(_)
    ));
    assert!(matches!(
        service
            .consume_before_call(&organization, &disabled, 10, None, "disabled", None)
            .unwrap_err(),
        ControlPlaneError::Conflict(_)
    ));
    // Fund acct_1, hold 400, then try to close the hold through acct_2.
    service
        .grant_credits(&organization, &acct, 1_000, "seed", None)
        .unwrap();
    let consume = match service
        .consume_before_call(&organization, &acct, 400, None, "attempt", Some("hold-x"))
        .unwrap()
    {
        CreditAppend::Appended => service
            .credit_entries_page(&organization, 0, 10)
            .unwrap()
            .into_iter()
            .find(|row| row.entry.kind == CreditKind::Consume)
            .map(|row| row.entry.id)
            .unwrap(),
        other => panic!("expected an appended consume, got {other:?}"),
    };
    let before = service.credit_entries_page(&organization, 0, 100).unwrap();
    let err = service
        .settle_consume(&organization, &other, &consume, 400, "cross-account")
        .unwrap_err();
    assert!(
        matches!(err, ControlPlaneError::Conflict(ref message) if message.contains("cross accounts")),
        "cross-account settle is refused typed: {err:?}"
    );
    assert_eq!(
        service.credit_entries_page(&organization, 0, 100).unwrap(),
        before,
        "the cross-account refusal wrote nothing"
    );
    // The account that owns the hold still closes it.
    service
        .settle_consume(&organization, &acct, &consume, 250, "actual")
        .unwrap();
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.consumed_micro, 250);
    assert_eq!(balance.balance_micro().unwrap(), 750);
}

/// Adversarial: a durable spend row is never attributed to another tenant.
#[test]
fn ingestion_refuses_a_row_naming_a_foreign_organization() {
    let service = service_with(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    let mut row = spend_row(1, 100, 1, 1, 0);
    row.organization_id = "org_b".into();
    let err = service
        .ingest_spend_row(&organization, &acct, &row, SpendCategory::Managed)
        .unwrap_err();
    assert!(
        matches!(err, ControlPlaneError::Malformed(ref message) if message.contains("another tenant")),
        "a foreign row is refused typed: {err:?}"
    );
    assert!(service
        .usage_page(&organization, None, 10)
        .unwrap()
        .items
        .is_empty());
}

/// Adversarial concurrency: racing consumes can never jointly overdraw, and
/// a racing settle/refund pair for one hold has exactly one winner (never
/// both, which would both charge and release the same money).
#[test]
fn concurrent_consumes_and_settle_refund_races_are_exact() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");
    store
        .append_credit_entry(&credit_entry("crd_g", CreditKind::Grant, 1_000, None))
        .unwrap();
    let outcomes: Vec<BillingStoreError> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for index in 0..8u32 {
            let store = &store;
            handles.push(scope.spawn(move || {
                store
                    .append_credit_entry(&credit_entry(
                        &format!("crd_race_{index}"),
                        CreditKind::Consume,
                        400,
                        None,
                    ))
                    .map(|_| ())
            }));
        }
        handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap().err())
            .collect()
    });
    assert_eq!(outcomes.len(), 6, "exactly two 400-micro holds fit 1000");
    for refusal in &outcomes {
        assert!(
            matches!(
                refusal,
                BillingStoreError::Credit(CreditAppendRefusal::InsufficientCredits { .. })
            ),
            "every losing consume is a typed insufficient-credit refusal: {refusal:?}"
        );
    }
    let balance = store.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 800);
    assert_eq!(balance.balance_micro().unwrap(), 200, "never overdrawn");
    let held = store.credit_entries(&organization, 0, 100).unwrap();
    assert_eq!(held.len(), 3);
    let mut consume_ids: Vec<_> = held
        .iter()
        .filter(|row| row.entry.kind == CreditKind::Consume)
        .map(|row| row.entry.id.clone())
        .collect();
    assert_eq!(consume_ids.len(), 2);
    let settle_target = consume_ids.pop().unwrap();
    let refund_target = consume_ids.pop().unwrap();

    // Two settles of the SAME hold: exactly one wins; a double settle is
    // refused typed (never charges the actual twice).
    let settles: Vec<_> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for index in 0..2u32 {
            let store = &store;
            let target = settle_target.as_str();
            handles.push(scope.spawn(move || {
                store.append_credit_entry(&credit_entry(
                    &format!("crd_race_settle_{index}"),
                    CreditKind::Settle,
                    400,
                    Some(target),
                ))
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert_eq!(
        settles.iter().filter(|outcome| outcome.is_ok()).count(),
        1,
        "exactly one settle wins: {settles:?}"
    );

    // Two FULL refunds of the OTHER pending hold: exactly one wins; the
    // second is the typed `PendingHoldRefundNotFull` (a released hold cannot
    // be released again).
    let refunds: Vec<_> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for index in 0..2u32 {
            let store = &store;
            let target = refund_target.as_str();
            handles.push(scope.spawn(move || {
                store.append_credit_entry(&credit_entry(
                    &format!("crd_race_refund_{index}"),
                    CreditKind::Refund,
                    400,
                    Some(target),
                ))
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert_eq!(
        refunds.iter().filter(|outcome| outcome.is_ok()).count(),
        1,
        "exactly one full release wins: {refunds:?}"
    );
    let balance = store.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 0, "no phantom holds after the races");
    assert_eq!(balance.pending_consumes, 0);
    assert_eq!(balance.consumed_micro, 800, "settled+released holds");
    assert_eq!(balance.refunded_micro, 400);
    assert_eq!(balance.balance_micro().unwrap(), 600, "never minted");
    assert_eq!(
        store.credit_entries(&organization, 0, 100).unwrap().len(),
        5
    );
    // The cancelled hold can never be settled afterwards.
    assert!(matches!(
        store
            .append_credit_entry(&credit_entry(
                "crd_race_late_settle",
                CreditKind::Settle,
                400,
                Some(refund_target.as_str()),
            ))
            .unwrap_err(),
        BillingStoreError::Credit(CreditAppendRefusal::SettleAfterRefund { .. })
    ));
}

/// A commercial mutation idempotency key is request-bound on BOTH stores:
/// the same canonical request replays the recorded response, a different
/// request conflicts without mutating, and the key is tenant-scoped.
#[test]
fn commercial_mutation_claim_replays_only_the_same_request() {
    let org_a = org("org_mutation_a");
    let org_b = org("org_mutation_b");
    let store = MemoryBillingStore::new();
    assert_eq!(
        store
            .claim_billing_mutation(&org_a, "k", "req-1", 0)
            .unwrap(),
        faktor_cloud::BillingMutationClaim::Fresh
    );
    store
        .finish_billing_mutation(&org_a, "k", "resp-1")
        .unwrap();
    assert_eq!(
        store
            .claim_billing_mutation(&org_a, "k", "req-1", 1)
            .unwrap(),
        faktor_cloud::BillingMutationClaim::Replay {
            response: Some("resp-1".into())
        }
    );
    assert!(matches!(
        store
            .claim_billing_mutation(&org_a, "k", "req-2", 2)
            .unwrap(),
        faktor_cloud::BillingMutationClaim::Conflict { .. }
    ));
    // Tenant-scoped: another organization's identical key is a fresh claim.
    assert_eq!(
        store
            .claim_billing_mutation(&org_b, "k", "req-1", 3)
            .unwrap(),
        faktor_cloud::BillingMutationClaim::Fresh
    );
}

/// Adversarial: caller-chosen commercial idempotency keys are BOUNDED per
/// organization. The newest keys keep their replay record; the oldest are
/// pruned (a fresh claim), so an owner cannot grow the durable table without
/// bound. Both stores keep the newest key replayed.
#[test]
fn commercial_mutation_idempotency_keys_are_bounded_per_organization() {
    let dir = tempfile::tempdir().unwrap();
    let mem = MemoryBillingStore::new();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");
    for store in [&mem as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        for index in 0..(faktor_cloud::MAX_BILLING_MUTATIONS_PER_ORG + 5) {
            assert_eq!(
                store
                    .claim_billing_mutation(
                        &organization,
                        &format!("key_{index}"),
                        &format!("request_{index}"),
                        index as i64,
                    )
                    .unwrap(),
                faktor_cloud::BillingMutationClaim::Fresh
            );
        }
        let newest = faktor_cloud::MAX_BILLING_MUTATIONS_PER_ORG + 4;
        assert!(matches!(
            store
                .claim_billing_mutation(
                    &organization,
                    &format!("key_{newest}"),
                    &format!("request_{newest}"),
                    0,
                )
                .unwrap(),
            faktor_cloud::BillingMutationClaim::Replay { response: None }
        ));
        // The oldest key was pruned by the bound: a fresh claim again.
        assert_eq!(
            store
                .claim_billing_mutation(&organization, "key_0", "request_0", 0)
                .unwrap(),
            faktor_cloud::BillingMutationClaim::Fresh
        );
    }
}

/// The in-memory store is an embedded-host backend: appending N usage rows
/// must stay O(N log N), never rescan every prior row per append (the old
/// source-key/correction lookup was O(N^2) and made 1M-row ingestion
/// impractical). 50k rows is large enough to separate the two growth curves
/// while staying a bounded CI probe.
#[test]
fn memory_usage_append_50k_is_not_quadratic() {
    let store = MemoryBillingStore::new();
    let started = Instant::now();
    for index in 0..50_000u64 {
        let mut event = usage_event(&format!("uev_scale_{index}"));
        event.task_id = 1 + (index % 200);
        event.occurred_at_ms = index as i64;
        assert!(matches!(
            store.append_usage_event(&event).unwrap(),
            faktor_cloud::UsageAppend::Appended
        ));
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "50k memory usage appends took {elapsed:?}; the append path regressed to a full scan"
    );
    let rows = store
        .usage_events_of_task(&org("org_a"), 7, 0, 250)
        .unwrap();
    assert_eq!(rows.len(), 250, "the task index stays exact");
    assert!(rows
        .windows(2)
        .all(|pair| pair[0].event_seq < pair[1].event_seq));
}

/// Credit appends validate against the incremental balance/index; folding
/// the whole ledger per append was O(N^2). 20k grants is a bounded guard
/// that also checks the final balance is byte-for-byte the grant sum.
#[test]
fn memory_credit_append_20k_keeps_the_balance_incremental() {
    let store = MemoryBillingStore::new();
    let started = Instant::now();
    for index in 0..20_000u64 {
        let mut entry = credit_entry(&format!("crd_scale_{index}"), CreditKind::Grant, 3, None);
        entry.occurred_at_ms = index as i64;
        assert!(matches!(
            store.append_credit_entry(&entry).unwrap(),
            CreditAppend::Appended
        ));
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "20k memory credit appends took {elapsed:?}; the append path regressed to a full fold"
    );
    let balance = store.credit_balance(&org("org_a")).unwrap();
    assert_eq!(balance.granted_micro, 60_000);
    assert_eq!(balance.balance_micro().unwrap(), 60_000);
    assert_eq!(
        store
            .credit_entries(&org("org_a"), 0, 25_000)
            .unwrap()
            .len(),
        20_000
    );
}

/// Both backends must refuse a reused durable id: SQLite enforces the
/// primary key, and the in-memory store must not silently overwrite the row
/// (which would leave the old source-key/index entry pointing at new data).
#[test]
fn memory_store_refuses_reused_usage_and_credit_ids_like_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("billing.db");
    let sqlite = SqliteControlPlaneStore::open(&path).unwrap();
    let memory = MemoryBillingStore::new();
    let organization = org("org_a");
    for store in [&memory as &dyn BillingStore, &sqlite as &dyn BillingStore] {
        let first = usage_event("uev_reused");
        store.append_usage_event(&first).unwrap();
        let mut second = first.clone();
        second.source_key = "manual:other".into();
        assert!(
            store.append_usage_event(&second).is_err(),
            "a reused usage id must be refused, not overwrite the row"
        );

        let grant = credit_entry("crd_reused", CreditKind::Grant, 10, None);
        assert_eq!(
            store.append_credit_entry(&grant).unwrap(),
            CreditAppend::Appended
        );
        let mut changed = grant.clone();
        changed.amount_micro = 11;
        assert!(
            store.append_credit_entry(&changed).is_err(),
            "a reused credit id must be refused, not overwrite the row"
        );
        assert_eq!(
            store.credit_balance(&organization).unwrap().granted_micro,
            10,
            "the first credit row is untouched"
        );
    }
}

/// Unknown pricing must never ingest as a settled $0: the call is recorded
/// as an UNPRICED call (a distinct usage unit), the fold reports it beside
/// the cost totals, and a genuinely zero-priced authority (LocalZero) is not
/// conflated with it.
#[test]
fn unknown_pricing_is_recorded_as_an_unpriced_call_never_a_settled_zero() {
    let service = service_with(
        Arc::new(SqliteControlPlaneStore::open_in_memory().unwrap()),
        Arc::new(ManualClock::new(1_000)),
    );
    let organization = org("org_a");
    let acct = account("acct_unpriced_managed");
    service
        .ensure_account(&organization, &acct, "managed", true)
        .unwrap();

    let mut unpriced = spend_row(11, 0, 10, 20, 0);
    unpriced.provider_cost_micro = 0;
    unpriced.provider_reported_micro = 0;
    unpriced.price_authority = None; // no pricing authority could price it
    let report = service
        .ingest_spend_row(&organization, &acct, &unpriced, SpendCategory::Managed)
        .unwrap();
    assert_eq!(
        report.unpriced, 1,
        "the unpriced settlement is counted: {report:?}"
    );
    let fold = service.fold(&organization).unwrap();
    assert_eq!(fold.totals.unpriced_calls, 1);
    assert_eq!(
        fold.totals.provider_cost_micro, 0,
        "unknown pricing contributes no cost at all — never a fake settled zero"
    );
    assert_eq!(
        fold.totals.input_tokens, 10,
        "the token truth is still ingested beside the unpriced marker"
    );

    // A LocalZero authority is a MEASURED $0, not unknown pricing.
    let mut local = spend_row(12, 0, 5, 5, 0);
    local.provider_cost_micro = 0;
    local.provider_reported_micro = 0;
    local.price_authority = Some(faktor_core::model::PriceAuthority::LocalZero);
    let report = service
        .ingest_spend_row(&organization, &acct, &local, SpendCategory::Managed)
        .unwrap();
    assert_eq!(
        report.unpriced, 0,
        "a measured local zero is priced: {report:?}"
    );
    let fold = service.fold(&organization).unwrap();
    assert_eq!(
        fold.totals.unpriced_calls, 1,
        "the local zero adds no unpriced call"
    );
    assert_eq!(fold.totals.input_tokens, 15);

    // Replay of the same durable rows appends nothing (idempotent).
    let replay = service
        .ingest_spend_row(&organization, &acct, &unpriced, SpendCategory::Managed)
        .unwrap();
    assert_eq!(replay.appended, 0);
    assert_eq!(
        replay.unpriced, 1,
        "a replayed unpriced row is reported, not re-appended"
    );
    assert_eq!(
        service.fold(&organization).unwrap().totals.unpriced_calls,
        1
    );
}
