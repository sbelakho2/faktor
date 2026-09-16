//! Adversarial tests of the Wave 3 commercial metering domain (usage ledger,
//! entitlements, credits): append-only corrections, BYOK-vs-managed
//! accounting, credit exactness across crashes, the in-flight-transaction
//! invariant, quota refusals naming the exact limit, and org isolation.

use std::sync::Arc;

use faktor_cloud::billing::{
    ALL_LIMITS, FEATURE_BYOK, FEATURE_CREDITS, FEATURE_MANAGED_PROVIDERS, LIMIT_MAX_ACTIVE_TASKS,
    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD, LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK,
    LIMIT_MAX_TOKENS_PER_PERIOD, UNIT_PROVIDER_COST,
};
use faktor_cloud::{
    Admission, AdmissionBoundary, AdmissionRequest, BillingAccount, BillingAccountId,
    BillingConfig, BillingStore, CreditAppendRefusal, CreditEntry, CreditEntryId, CreditKind,
    DurableSpendRow, EntitlementExceeded, EntitlementService, InFlightKind, ManualClock,
    MemoryBillingStore, ObservedUsage, OrganizationId, PlanConfig, ReconciliationState,
    SpendCategory, SqliteControlPlaneStore, Subscription, SubscriptionId, SubscriptionStatus,
    UsageEvent, UsageEventId, UsageUnit, CAUSE_SUBSCRIPTION_ACTIVE,
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
            .balance_micro(),
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
            .balance_micro(),
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
    assert_eq!(balance.balance_micro(), 800);
    // Settling at the actual releases the hold exactly.
    service
        .settle_consume(&organization, &acct, &consume, 150, "actual")
        .unwrap();
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 0);
    assert_eq!(balance.consumed_micro, 150);
    assert_eq!(balance.balance_micro(), 850);
    // Refunds are exact: the unused 50 comes back, more is refused.
    service
        .refund_consume(&organization, &acct, &consume, 50, "unused")
        .unwrap();
    assert_eq!(
        service
            .credit_balance(&organization)
            .unwrap()
            .balance_micro(),
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
            .balance_micro(),
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
    assert_eq!(balance.balance_micro(), 600);
    // Reconciliation settles at the actual provider spend, exactly once.
    service
        .settle_consume(&organization, &acct, &consume_id, 260, "reconciled")
        .unwrap();
    assert_eq!(
        service
            .credit_balance(&organization)
            .unwrap()
            .balance_micro(),
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
            store.credit_balance(&organization).unwrap().balance_micro(),
            100
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
    assert_eq!(balance.balance_micro(), 4_600);
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
