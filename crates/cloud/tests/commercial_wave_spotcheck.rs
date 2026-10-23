//! Independent spot checks for the commercial/billing wave.
//!
//! These do NOT re-run the wave's own regression files. They take a different
//! angle: a scratch ledger whose facts are appended in adversarial order and
//! then reconciled from BOTH the in-memory and SQLite authorities, with the
//! zero-actual translation exercised as the adapter documents it (a full
//! release, not a zero settle) and the closed-hold replay asserted at the
//! durable seam (`CreditAppendRefusal::ConsumeAlreadyClosed`), plus a wire
//! money-encoding check over the exact DTO values and a trial/cancel truth
//! table.

use std::sync::Arc;

use faktor_cloud::{
    fold_credits, fold_usage, BillingAccountId, BillingCapabilityState, BillingConfig,
    BillingStore, BillingStoreError, BillingTrialView, ControlPlaneError, CreditAppend,
    CreditAppendRefusal, CreditEntry, CreditEntryId, CreditKind, EntitlementService, ManualClock,
    MemoryBillingStore, OrganizationId, PlanConfig, ReconciliationState, SpendCategory,
    SqliteControlPlaneStore, SubscriptionStatus, UsageEvent, UsageEventId, UsageUnit,
    CAUSE_CURRENCY_NOT_CONFIGURED, CAUSE_INVOICES_NOT_AVAILABLE,
    CAUSE_PAYMENT_PROVIDER_NOT_CONFIGURED, CAUSE_SEAT_BILLING_NOT_MODELED,
    CAUSE_TAX_NOT_CONFIGURED, FEATURE_BYOK, FEATURE_CREDITS, FEATURE_MANAGED_PROVIDERS,
    LIMIT_MAX_ACTIVE_TASKS, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
};

fn org() -> OrganizationId {
    OrganizationId::try_new("org_spot").unwrap()
}

fn account() -> BillingAccountId {
    BillingAccountId::try_new("acct_spot").unwrap()
}

fn config() -> BillingConfig {
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
                (LIMIT_MAX_ACTIVE_TASKS.to_string(), 100),
                (
                    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD.to_string(),
                    1_000_000_000,
                ),
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
    EntitlementService::new_for_organization(store, clock, config(), Some(org()))
        .expect("spot-check config")
}

fn funded(store: Arc<dyn BillingStore>) -> (Arc<EntitlementService>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(1_000));
    let service = service_with(store, clock.clone());
    service
        .ensure_account(&org(), &account(), "spot", true)
        .unwrap();
    service
        .grant_credits(&org(), &account(), 1_000, "seed", Some("seed"))
        .unwrap();
    (service, clock)
}

fn cost_event(
    id: &str,
    cost: u64,
    category: SpendCategory,
    correction_of: Option<&str>,
    occurred_at_ms: i64,
) -> UsageEvent {
    UsageEvent {
        id: UsageEventId::try_new(id).unwrap(),
        organization_id: org(),
        billing_account_id: account(),
        task_id: 7,
        run_id: "1".into(),
        attempt_id: "11".into(),
        provider: "openai".into(),
        model: "m".into(),
        unit: UsageUnit::ProviderCostMicro,
        quantity: 1,
        provider_cost_micro: cost,
        source_operation: "session.cost_settle".into(),
        occurred_at_ms,
        reconciliation_state: ReconciliationState::Reconciled,
        correction_of: correction_of.map(|id| UsageEventId::try_new(id).unwrap()),
        category,
        source_key: format!("reservation:1:{id}"),
    }
}

fn token_event(id: &str, quantity: u64, occurred_at_ms: i64) -> UsageEvent {
    UsageEvent {
        id: UsageEventId::try_new(id).unwrap(),
        organization_id: org(),
        billing_account_id: account(),
        task_id: 7,
        run_id: "1".into(),
        attempt_id: "11".into(),
        provider: "openai".into(),
        model: "m".into(),
        unit: UsageUnit::InputTokens,
        quantity,
        provider_cost_micro: 0,
        source_operation: "session.cost_settle".into(),
        occurred_at_ms,
        reconciliation_state: ReconciliationState::Reconciled,
        correction_of: None,
        category: SpendCategory::Managed,
        source_key: format!("reservation:1:{id}"),
    }
}

fn shuffled<T>(items: &mut [T], seed: u64) {
    let mut state = seed;
    for i in (1..items.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = (state as usize) % (i + 1);
        items.swap(i, j);
    }
}

fn close_hold_with_zero_actual(store: Arc<dyn BillingStore>) {
    let (service, _clock) = funded(store.clone());
    let organization = org();
    let acct = account();

    assert_eq!(
        service
            .consume_before_call(&organization, &acct, 400, None, "attempt", Some("hold"))
            .unwrap(),
        CreditAppend::Appended
    );
    let hold = service
        .credit_entry_id_by_idempotency_key(&organization, "hold")
        .unwrap()
        .expect("the hold entry");

    // The credit domain refuses zero entries by construction. The adapter's
    // documented translation of `actual == 0` is the FULL release.
    match service.settle_consume(&organization, &acct, &hold, 0, "zero") {
        Err(ControlPlaneError::Malformed(message)) => {
            assert!(message.contains("non-zero amount"), "{message}");
        }
        other => panic!("a zero settle is outside the credit domain: {other:?}"),
    }
    service
        .refund_consume(&organization, &acct, &hold, 400, "zero_actual")
        .unwrap();

    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.granted_micro, 1_000);
    assert_eq!(
        balance.consumed_micro, 400,
        "the gross hold stays the debit"
    );
    assert_eq!(balance.refunded_micro, 400, "the full hold is released");
    assert_eq!(balance.held_micro, 0);
    assert_eq!(balance.pending_consumes, 0);
    assert_eq!(
        balance.balance_micro().unwrap(),
        1_000,
        "a zero actual spends nothing and mints nothing"
    );

    // A replay of the record-before-call key is typed at the DURABLE seam:
    // it must never hand back a hold token for already-closed money.
    let replay_row = CreditEntry {
        id: CreditEntryId::try_new("crd_replay").unwrap(),
        organization: organization.clone(),
        billing_account_id: acct.clone(),
        kind: CreditKind::Consume,
        amount_micro: 400,
        reference: None,
        usage_event_id: None,
        reason: "attempt".into(),
        occurred_at_ms: 1_000,
        idempotency_key: Some("hold".into()),
    };
    match store.append_credit_entry(&replay_row) {
        Err(BillingStoreError::Credit(CreditAppendRefusal::ConsumeAlreadyClosed { reference })) => {
            assert_eq!(reference, hold.as_str())
        }
        other => panic!("expected ConsumeAlreadyClosed at the durable seam: {other:?}"),
    }
    // ... and the service surfaces it as the closed-hold conflict.
    match service.consume_before_call(&organization, &acct, 400, None, "attempt", Some("hold")) {
        Err(ControlPlaneError::Conflict(message)) => {
            assert!(message.contains("already closed"), "{message}");
        }
        other => panic!("a released hold cannot be replayed: {other:?}"),
    }
    assert_eq!(
        service.credit_balance(&organization).unwrap(),
        balance,
        "every refusal wrote nothing"
    );
}

/// (a) A zero actual releases the entire hold; the replay is
/// `ConsumeAlreadyClosed` at the store and a closed-hold conflict at the
/// service. Both authorities agree, and nothing is minted.
#[test]
fn zero_actual_release_is_typed_closed_on_both_stores() {
    close_hold_with_zero_actual(Arc::new(MemoryBillingStore::new()));
    close_hold_with_zero_actual(Arc::new(SqliteControlPlaneStore::open_in_memory().unwrap()));
}

fn release_then_settle(store: Arc<dyn BillingStore>) {
    let (service, _clock) = funded(store);
    let organization = org();
    let acct = account();
    service
        .consume_before_call(&organization, &acct, 400, None, "attempt", Some("hold"))
        .unwrap();
    let hold = service
        .credit_entry_id_by_idempotency_key(&organization, "hold")
        .unwrap()
        .expect("the hold entry");
    service
        .refund_consume(&organization, &acct, &hold, 400, "pre_dispatch_refund")
        .unwrap();

    // Settling the cancelled hold would charge money that was already
    // returned: the durable seam refuses it typed.
    match service.settle_consume(&organization, &acct, &hold, 150, "late") {
        Err(ControlPlaneError::Conflict(message)) => {
            assert!(
                message.contains("released hold is never settled"),
                "{message}"
            );
        }
        other => panic!("a released hold can never be settled: {other:?}"),
    }
    // A second, differently-sized hold is settled once and then refunded;
    // a second settle is refused and cannot resurrect or grow the debit.
    service
        .consume_before_call(&organization, &acct, 300, None, "second", Some("hold2"))
        .unwrap();
    let hold2 = service
        .credit_entry_id_by_idempotency_key(&organization, "hold2")
        .unwrap()
        .expect("the second hold");
    service
        .settle_consume(&organization, &acct, &hold2, 250, "actual")
        .unwrap();
    service
        .refund_consume(&organization, &acct, &hold2, 100, "provider_refund")
        .unwrap();
    let before = service.credit_balance(&organization).unwrap();
    match service.settle_consume(&organization, &acct, &hold2, 251, "again") {
        Err(ControlPlaneError::Conflict(message)) => {
            assert!(message.contains("already settled"), "{message}");
        }
        other => panic!("a settled hold is settled exactly once: {other:?}"),
    }
    assert_eq!(
        service.credit_balance(&organization).unwrap(),
        before,
        "the refused second settle wrote nothing"
    );
    assert_eq!(
        before.balance_micro().unwrap(),
        1_000 - 250 + 100,
        "the settled actual minus the exact provider refund is the whole effect"
    );
}

/// (b) Refund-then-settle cannot mint managed spend: the released hold is
/// terminal, and an already-settled hold can never be settled again.
#[test]
fn refund_then_settle_never_mints_on_both_stores() {
    release_then_settle(Arc::new(MemoryBillingStore::new()));
    release_then_settle(Arc::new(SqliteControlPlaneStore::open_in_memory().unwrap()));
}

/// (c) A correction changes the amount, never the identity/category: a
/// managed cost can never be laundered into BYOK by a correction, and every
/// identity retag is refused before a row is written.
#[test]
fn corrections_cannot_retag_the_managed_fact() {
    type Retag = (&'static str, Box<dyn Fn(&mut UsageEvent)>);
    let store = Arc::new(MemoryBillingStore::new());
    let (service, _clock) = funded(store);
    let organization = org();
    let base = cost_event("uev_base", 500, SpendCategory::Managed, None, 10);
    service.record_usage(&base).unwrap();

    let retags: Vec<Retag> = vec![
        (
            "category",
            Box::new(|e: &mut UsageEvent| e.category = SpendCategory::Byok),
        ),
        (
            "provider",
            Box::new(|e: &mut UsageEvent| e.provider = "byok-provider".into()),
        ),
        (
            "model",
            Box::new(|e: &mut UsageEvent| e.model = "other".into()),
        ),
        ("task_id", Box::new(|e: &mut UsageEvent| e.task_id = 8)),
        (
            "run_id",
            Box::new(|e: &mut UsageEvent| e.run_id = "2".into()),
        ),
        (
            "attempt_id",
            Box::new(|e: &mut UsageEvent| e.attempt_id = "12".into()),
        ),
        (
            "unit",
            Box::new(|e: &mut UsageEvent| e.unit = UsageUnit::InputTokens),
        ),
    ];
    for (field, mutate) in retags {
        let mut correction = cost_event(
            "uev_retag",
            500,
            SpendCategory::Managed,
            Some("uev_base"),
            20,
        );
        mutate(&mut correction);
        // A token retag also has to zero the money to pass validate; the
        // identity guard must still fire (or the earlier value rule).
        if correction.unit == UsageUnit::InputTokens {
            correction.provider_cost_micro = 0;
            correction.quantity = 1;
            correction.category = SpendCategory::Managed;
        }
        match service.correct_usage(&correction) {
            Err(ControlPlaneError::Conflict(message)) => {
                assert!(
                    message.contains(field),
                    "the refusal must name {field:?}: {message}"
                );
            }
            other => panic!("retagging {field:?} must be refused: {other:?}"),
        }
        assert_eq!(
            service
                .usage_page(&organization, None, 10)
                .unwrap()
                .items
                .len(),
            1,
            "the refused {field} correction wrote nothing"
        );
    }

    // A value-only correction is legal and remains managed.
    let corrected = cost_event("uev_fix", 125, SpendCategory::Managed, Some("uev_base"), 20);
    service.correct_usage(&corrected).unwrap();
    let fold = service.fold(&organization).unwrap();
    assert_eq!(fold.totals.managed_cost_micro, 125);
    assert_eq!(fold.totals.byok_cost_micro, 0);
    // A second correction of the same base is a fork, never a second retag.
    let fork = cost_event("uev_fork", 1, SpendCategory::Managed, Some("uev_base"), 30);
    match service.correct_usage(&fork) {
        Err(ControlPlaneError::Conflict(message)) => {
            assert!(message.contains("already corrected"), "{message}");
        }
        other => panic!("double correction must be refused: {other:?}"),
    }
    // A correction of a foreign base is the same not-found as a missing one.
    let mut foreign = cost_event("uev_foreign", 5, SpendCategory::Managed, None, 10);
    foreign.organization_id = OrganizationId::try_new("org_other").unwrap();
    let foreign_correction =
        cost_event("uev_fc", 5, SpendCategory::Managed, Some("uev_foreign"), 20);
    assert!(matches!(
        service.correct_usage(&foreign_correction),
        Err(ControlPlaneError::NotFound(_))
    ));
}

/// (g) Both rollups equal the pure folds over an adversarial append order
/// (corrections before their bases, superseded corrections, an out-of-order
/// chain), and the credit rollup equals `fold_credits` after releases and
/// settles.
#[test]
fn rollups_match_the_pure_folds_under_adversarial_order() {
    let store = Arc::new(SqliteControlPlaneStore::open_in_memory().unwrap());
    let service = service_with(store.clone(), Arc::new(ManualClock::new(1_000)));
    let organization = org();
    service
        .ensure_account(&organization, &account(), "spot", true)
        .unwrap();

    let mut events = Vec::new();
    for i in 0..90u64 {
        let id = format!("uev_b{i}");
        let base = if i % 5 == 0 {
            token_event(&id, i + 1, (i as i64) % 7)
        } else {
            cost_event(
                &id,
                (i + 1) * 13,
                if i % 2 == 0 {
                    SpendCategory::Managed
                } else {
                    SpendCategory::Byok
                },
                None,
                (i as i64) % 7,
            )
        };
        events.push(base);
        if i % 3 == 0 && i % 5 != 0 {
            let correction = cost_event(
                &format!("uev_c{i}"),
                (i + 1) * 7,
                events[i as usize].category,
                Some(&id),
                (i as i64) % 11,
            );
            events.push(correction);
        }
    }
    // A correction that names another correction (the store seam tolerates
    // the chain; the winning-correction fold must still be exact).
    let chained = cost_event(
        "uev_chain",
        4_242,
        SpendCategory::Managed,
        Some("uev_c3"),
        99,
    );
    events.push(chained);

    let mut order: Vec<u64> = (0..events.len() as u64).collect();
    shuffled(&mut order, 0x5eed_5eed);
    for index in order {
        store.append_usage_event(&events[index as usize]).unwrap();
    }

    let rollup = store.usage_fold(&organization).unwrap();
    let pure = fold_usage(&organization, &events).unwrap();
    assert_eq!(rollup, pure, "SQLite usage rollup must equal the pure fold");
    assert_eq!(
        service.fold(&organization).unwrap(),
        pure,
        "the service must serve the same fold"
    );
    assert_eq!(
        service
            .entitlement_snapshot(&organization)
            .unwrap()
            .managed_spend_micro,
        pure.totals.managed_cost_micro
    );

    // Credit rollup vs pure fold across grants, holds, releases and settles.
    service
        .grant_credits(&organization, &account(), 100_000, "top", Some("top"))
        .unwrap();
    for i in 0..40u64 {
        let key = format!("hold-{i}");
        service
            .consume_before_call(&organization, &account(), 1_000, None, "call", Some(&key))
            .unwrap();
        let hold = service
            .credit_entry_id_by_idempotency_key(&organization, &key)
            .unwrap()
            .expect("the hold entry");
        if i % 2 == 0 {
            service
                .refund_consume(&organization, &account(), &hold, 1_000, "release")
                .unwrap();
        } else {
            service
                .settle_consume(&organization, &account(), &hold, 400 + i, "actual")
                .unwrap();
            if i % 4 == 1 {
                service
                    .refund_consume(&organization, &account(), &hold, 100, "provider refund")
                    .unwrap();
            }
        }
    }
    let entries: Vec<CreditEntry> = service
        .credit_entries_page(&organization, 0, 10_000)
        .unwrap()
        .into_iter()
        .map(|row| row.entry)
        .collect();
    let pure_credit = fold_credits(&entries).unwrap();
    assert_eq!(
        service.credit_balance(&organization).unwrap(),
        pure_credit,
        "SQLite credit rollup must equal the pure credit fold"
    );
}

/// (e) Every money value in the persisted DTOs is an exact decimal string
/// (a u64 micro amount never round-trips through a float), while counters
/// and timestamps stay JSON numbers.
#[test]
fn dto_money_values_are_exact_decimal_strings() {
    let store = Arc::new(MemoryBillingStore::new());
    let (service, _clock) = funded(store);
    let organization = org();
    // 312500 microUSD is exactly $0.3125; the previous lossy display read
    // it as $312,500 or truncated it.
    service
        .grant_credits(&organization, &account(), 312_500, "exact", Some("exact"))
        .unwrap();
    service
        .consume_before_call(
            &organization,
            &account(),
            312_500,
            None,
            "attempt",
            Some("exact-hold"),
        )
        .unwrap();
    let snapshot = service.entitlement_snapshot(&organization).unwrap();
    let mut wire = serde_json::to_value(&snapshot).unwrap();
    faktor_cloud::money::stringify_money_fields(&mut wire);
    assert_eq!(
        wire["credits"]["granted_micro"],
        serde_json::json!("313500")
    );
    assert_eq!(
        wire["credits"]["consumed_micro"],
        serde_json::json!("312500")
    );
    assert_eq!(wire["credits"]["held_micro"], serde_json::json!("312500"));
    assert_eq!(
        wire["limits"][LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD],
        serde_json::json!("1000000000")
    );
    assert_eq!(
        wire["limits"][LIMIT_MAX_ACTIVE_TASKS],
        serde_json::json!(100),
        "a non-money limit stays a number"
    );
    assert!(wire["total_tokens"].is_number());

    // The pure fold's money projection (what the usage route embeds) is
    // exact too once the native boundary applies the same encoder.
    let event = cost_event("uev_money", 312_500, SpendCategory::Managed, None, 10);
    service.store().append_usage_event(&event).unwrap();
    let fold = service.fold(&organization).unwrap();
    let mut usage = serde_json::to_value(&fold).unwrap();
    faktor_cloud::money::stringify_money_fields(&mut usage);
    assert_eq!(
        usage["totals"]["managed_cost_micro"],
        serde_json::json!("312500")
    );
    assert_eq!(
        usage["per_task"][0]["totals"]["provider_cost_micro"],
        serde_json::json!("312500")
    );
    assert_eq!(usage["totals"]["events"], serde_json::json!(1));
}

/// (f) The overview types every absent commercial capability as unavailable
/// with a stable cause; it never fabricates paid/invoiced/seated/taxed state.
/// Trial/cancel semantics are derived from the durable row, not a sweeper.
#[test]
fn overview_states_and_lifecycle_are_truthful() {
    let store = Arc::new(MemoryBillingStore::new());
    let clock = Arc::new(ManualClock::new(1_000));
    let service = service_with(store, clock.clone());
    let organization = org();
    service
        .ensure_account(&organization, &account(), "spot", true)
        .unwrap();

    let overview = service.overview(&organization).unwrap();
    assert_eq!(overview.plan_id.as_deref(), Some("pro"));
    assert!(overview.plan_found);
    assert!(!overview.subscription_active);
    assert_eq!(overview.trial.state, "none");
    for (state, cause, want) in [
        (
            overview.payment.state,
            overview.payment.cause,
            CAUSE_PAYMENT_PROVIDER_NOT_CONFIGURED,
        ),
        (
            overview.invoices.state,
            overview.invoices.cause,
            CAUSE_INVOICES_NOT_AVAILABLE,
        ),
        (
            overview.seats.state,
            overview.seats.cause,
            CAUSE_SEAT_BILLING_NOT_MODELED,
        ),
        (
            overview.tax.state,
            overview.tax.cause,
            CAUSE_TAX_NOT_CONFIGURED,
        ),
        (
            overview.currency.state,
            overview.currency.cause,
            CAUSE_CURRENCY_NOT_CONFIGURED,
        ),
    ] {
        assert_eq!(state, BillingCapabilityState::Unavailable);
        assert_eq!(cause, Some(want));
    }
    assert_eq!(
        overview.usage_export.state,
        BillingCapabilityState::Available
    );
    assert_eq!(overview.usage_export.cause, None);
    assert_eq!(overview.usage_export_formats, ["csv", "json"]);

    // An open-ended trial is never an entitlement.
    match service.change_subscription(&organization, "pro", SubscriptionStatus::Trialing, None) {
        Err(ControlPlaneError::Malformed(message)) => {
            assert!(message.contains("requires expires_ms"), "{message}");
        }
        other => panic!("an unbounded trial must be refused: {other:?}"),
    }
    let trial = SubscriptionStatus::Trialing;
    service
        .change_subscription(&organization, "pro", trial, Some(2_000))
        .unwrap();
    assert_eq!(
        service.overview(&organization).unwrap().trial.state,
        "active"
    );
    assert!(service.overview(&organization).unwrap().subscription_active);
    clock.advance(1_001);
    let expired = service.overview(&organization).unwrap();
    assert_eq!(
        expired.trial.state, "expired",
        "expiry is derived at read time"
    );
    assert!(!expired.subscription_active);
    assert_eq!(expired.trial.ends_ms, Some(2_000));
    // An open-ended ACTIVE subscription is legal; a canceled one is terminal.
    service
        .change_subscription(&organization, "pro", SubscriptionStatus::Active, None)
        .unwrap();
    assert!(service.overview(&organization).unwrap().subscription_active);
    let (canceled, replayed) = service.cancel_subscription(&organization).unwrap();
    assert_eq!(canceled.status, SubscriptionStatus::Canceled);
    assert!(!replayed);
    assert!(!service.overview(&organization).unwrap().subscription_active);
    assert_eq!(service.overview(&organization).unwrap().trial.state, "none");
    assert!(service.cancel_subscription(&organization).unwrap().1);
    match service.change_subscription(&organization, "pro", SubscriptionStatus::Canceled, None) {
        Err(ControlPlaneError::Malformed(message)) => {
            assert!(message.contains("cancel action"), "{message}");
        }
        other => panic!("change-to-canceled must be refused: {other:?}"),
    }
    // An unknown plan is refused, never resolved through the default.
    assert!(matches!(
        service.change_subscription(&organization, "absent", SubscriptionStatus::Active, None),
        Err(ControlPlaneError::Malformed(_))
    ));
    // A legacy trialing row with no expiry (only possible from an older
    // payload) is inactive, never an open-ended entitlement.
    let legacy = faktor_cloud::Subscription {
        id: faktor_cloud::SubscriptionId::try_new("sub_legacy").unwrap(),
        organization: organization.clone(),
        plan_id: "pro".into(),
        status: SubscriptionStatus::Trialing,
        started_ms: 0,
        expires_ms: None,
        updated_ms: 0,
    };
    assert!(!legacy.is_active_at(1_000));
    assert_eq!(BillingTrialView::of(Some(&legacy), 1_000).state, "expired");
}
