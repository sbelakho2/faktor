//! Adversarial correctness regressions for the commercial billing seams:
//! account permission/existence at credit append, correction routing,
//! aggregate tenant isolation, rollup parity with the pure folds, closed
//! hold idempotency, and out-of-order correction ingestion.

use std::sync::Arc;

use faktor_cloud::billing::{
    FEATURE_BYOK, FEATURE_CREDITS, FEATURE_MANAGED_PROVIDERS, LIMIT_MAX_ACTIVE_TASKS,
    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD, LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK,
    LIMIT_MAX_TOKENS_PER_PERIOD,
};
use faktor_cloud::{
    fold_credits, fold_usage, BillingAccountId, BillingConfig, BillingStore, BillingStoreError,
    ControlPlaneError, CreditAppend, CreditAppendRefusal, CreditEntry, CreditEntryId, CreditKind,
    EntitlementService, ManualClock, MemoryBillingStore, OrganizationId, PlanConfig,
    ReconciliationState, SpendCategory, SqliteControlPlaneStore, UsageAppend, UsageEvent,
    UsageEventId, UsageUnit,
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
                (LIMIT_MAX_ACTIVE_TASKS.to_string(), 10),
                (LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK.to_string(), 8),
                (
                    LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD.to_string(),
                    1_000_000,
                ),
                (LIMIT_MAX_TOKENS_PER_PERIOD.to_string(), 10_000_000),
            ]
            .into_iter()
            .collect(),
        },
    );
    config.default_plan = Some("pro".into());
    config.managed_providers.insert("openai".into());
    config
}

fn service_with(store: Arc<dyn BillingStore>) -> Arc<EntitlementService> {
    EntitlementService::new(store, Arc::new(ManualClock::new(1_000)), configured())
        .expect("test config")
}

fn cost_event(
    id: &str,
    organization: &str,
    cost: u64,
    occurred_at_ms: i64,
    correction_of: Option<&str>,
) -> UsageEvent {
    UsageEvent {
        id: UsageEventId::try_new(id).unwrap(),
        organization_id: org(organization),
        billing_account_id: account("acct_1"),
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
        category: SpendCategory::Managed,
        source_key: format!("reservation:1:{id}"),
    }
}

fn credit(
    id: &str,
    organization: &str,
    kind: CreditKind,
    amount_micro: u64,
    reference: Option<&str>,
    idempotency_key: Option<&str>,
) -> CreditEntry {
    CreditEntry {
        id: CreditEntryId::try_new(id).unwrap(),
        organization: org(organization),
        billing_account_id: account("acct_1"),
        kind,
        amount_micro,
        reference: reference.map(|r| CreditEntryId::try_new(r).unwrap()),
        usage_event_id: None,
        reason: "audit regression".into(),
        occurred_at_ms: 1_000,
        idempotency_key: idempotency_key.map(str::to_string),
    }
}

/// A BYOK-only (or disabled) account must never open a managed credit hold:
/// otherwise the call dispatches, usage ingestion refuses it, and the debit
/// is stranded. Unknown accounts must never receive money at all. The
/// configured managed flag is authoritative on daemon restart, not a
/// one-time seed.
#[test]
fn credit_append_requires_a_real_permitted_account_and_reconciles_the_config() {
    let service = service_with(Arc::new(MemoryBillingStore::new()));
    let organization = org("org_a");
    let byok = account("acct_byok");
    service
        .ensure_account(&organization, &byok, "byok", false)
        .unwrap();
    service
        .grant_credits(&organization, &byok, 1_000, "seed", None)
        .unwrap();

    let error = service
        .consume_before_call(&organization, &byok, 400, None, "call", Some("byok-hold"))
        .unwrap_err();
    assert!(
        matches!(&error, ControlPlaneError::Conflict(message) if message.contains("BYOK-only")),
        "a BYOK-only account cannot open a managed hold: {error:?}"
    );
    let after = service.credit_balance(&organization).unwrap();
    assert_eq!(after.granted_micro, 1_000);
    assert_eq!(after.consumed_micro, 0, "the refused hold wrote nothing");
    assert_eq!(after.held_micro, 0);
    assert_eq!(after.pending_consumes, 0);

    // A restart with `billing.managed = true` actually changes the durable
    // permission, and the same account may then hold.
    let reconciled = service
        .ensure_account(&organization, &byok, "byok", true)
        .unwrap();
    assert!(reconciled.managed, "config reconciliation, not a stale row");
    service
        .consume_before_call(
            &organization,
            &byok,
            400,
            None,
            "call",
            Some("managed-hold"),
        )
        .unwrap();
    assert_eq!(
        service.credit_balance(&organization).unwrap().held_micro,
        400
    );

    // Unknown accounts are refused before any entry is written.
    let missing = account("acct_missing");
    let error = service
        .grant_credits(&organization, &missing, 10, "grant", None)
        .unwrap_err();
    assert!(matches!(error, ControlPlaneError::NotFound(_)), "{error:?}");
    let error = service
        .consume_before_call(&organization, &missing, 10, None, "call", None)
        .unwrap_err();
    assert!(matches!(error, ControlPlaneError::NotFound(_)), "{error:?}");
    assert_eq!(
        service
            .credit_entries_page(&organization, 0, 100)
            .unwrap()
            .len(),
        2,
        "only the seed grant and the accepted managed hold"
    );
}

/// Disabled accounts can never open funding/spend, but must still be able to
/// close an existing hold (otherwise disabling an account strands money).
#[test]
fn disabled_accounts_cannot_open_but_can_close() {
    let store: Arc<dyn BillingStore> = Arc::new(MemoryBillingStore::new());
    let service = service_with(store.clone());
    let organization = org("org_a");
    let acct = account("acct_1");
    service
        .ensure_account(&organization, &acct, "acct", true)
        .unwrap();
    service
        .grant_credits(&organization, &acct, 1_000, "seed", None)
        .unwrap();
    service
        .consume_before_call(&organization, &acct, 400, None, "call", Some("hold"))
        .unwrap();
    let hold_id = service
        .credit_entry_id_by_idempotency_key(&organization, "hold")
        .unwrap()
        .expect("the hold id");

    let mut row = store
        .billing_account(&organization, &acct)
        .unwrap()
        .unwrap();
    row.disabled = true;
    store.put_billing_account(&row).unwrap();

    let error = service
        .grant_credits(&organization, &acct, 10, "grant", None)
        .unwrap_err();
    assert!(
        matches!(&error, ControlPlaneError::Conflict(m) if m.contains("disabled")),
        "{error:?}"
    );
    let error = service
        .consume_before_call(&organization, &acct, 10, None, "call", None)
        .unwrap_err();
    assert!(
        matches!(&error, ControlPlaneError::Conflict(m) if m.contains("disabled")),
        "{error:?}"
    );
    // Closing the pre-disable hold succeeds and releases exactly it.
    service
        .refund_consume(&organization, &acct, &hold_id, 400, "release")
        .unwrap();
    let balance = service.credit_balance(&organization).unwrap();
    assert_eq!(balance.held_micro, 0);
    assert_eq!(balance.pending_consumes, 0);
    assert_eq!(balance.balance_micro().unwrap(), 1_000);
}

/// `record_usage` is not a correction back door: corrections are only
/// accepted through the guarded `correct_usage` path, and the fold still
/// reports the base until that happens.
#[test]
fn record_usage_cannot_bypass_the_correction_guards() {
    let service = service_with(Arc::new(MemoryBillingStore::new()));
    let organization = org("org_a");
    service
        .ensure_account(&organization, &account("acct_1"), "acct", true)
        .unwrap();
    let base = cost_event("uev_base", "org_a", 100, 10, None);
    assert_eq!(service.record_usage(&base).unwrap(), UsageAppend::Appended);
    let correction = cost_event("uev_corr", "org_a", 60, 20, Some("uev_base"));

    let error = service.record_usage(&correction).unwrap_err();
    assert!(
        matches!(&error, ControlPlaneError::Malformed(m) if m.contains("correct_usage")),
        "a correction is refused outside the guarded path: {error:?}"
    );
    assert_eq!(
        service
            .usage_page(&organization, None, 10)
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        service
            .fold(&organization)
            .unwrap()
            .totals
            .provider_cost_micro,
        100
    );

    assert_eq!(
        service.correct_usage(&correction).unwrap(),
        UsageAppend::Appended
    );
    let folded = service.fold(&organization).unwrap();
    assert_eq!(folded.totals.provider_cost_micro, 60);
    assert_eq!(
        folded.totals.events, 1,
        "base superseded, correction counted"
    );
    assert_eq!(folded.totals.corrected_events, 2);
}

/// The pure usage fold refuses a mixed-tenant event array rather than
/// attributing foreign spend to the requested organization.
#[test]
fn fold_usage_refuses_foreign_organization_events() {
    let base = cost_event("uev_a", "org_a", 100, 10, None);
    let foreign = cost_event("uev_b", "org_b", 900, 10, None);
    let error = fold_usage(&org("org_a"), &[base, foreign]).unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("uev_b"), "{rendered}");
    assert!(rendered.contains("org_b"), "{rendered}");
}

/// Out-of-order append (correction before its base) must produce the SAME
/// totals as the append-order-independent pure fold: exactly one effective
/// event, the correction's cost, and both rows marked corrected.
#[test]
fn usage_rollup_matches_the_pure_fold_for_correction_before_base() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");
    let base = cost_event("uev_base", "org_a", 100, 10, None);
    let correction = cost_event("uev_corr", "org_a", 60, 20, Some("uev_base"));
    // Adversarial append order: the correction is durable first.
    store.append_usage_event(&correction).unwrap();
    store.append_usage_event(&base).unwrap();

    let pure = fold_usage(&organization, &[base, correction]).unwrap();
    let rollup = store.usage_fold(&organization).unwrap();
    assert_eq!(rollup.totals, pure.totals);
    assert_eq!(rollup.per_task, pure.per_task);
    assert_eq!(rollup.totals.provider_cost_micro, 60);
    assert_eq!(rollup.totals.events, 1);
    assert_eq!(rollup.totals.corrected_events, 2);
    assert_eq!(store.usage_totals(&organization).unwrap(), pure.totals);
}

/// The SQLite credit rollup is not allowed to disagree with `fold_credits`:
/// releasing a pending hold must drop BOTH `held_micro` and
/// `pending_consumes`, and a settle-then-refund must keep the settled debit.
#[test]
fn credit_rollup_matches_the_pure_fold_for_release_and_refund() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");

    let released = vec![
        credit("crd_g", "org_a", CreditKind::Grant, 1_000, None, None),
        credit(
            "crd_c",
            "org_a",
            CreditKind::Consume,
            400,
            None,
            Some("hold"),
        ),
        credit(
            "crd_r",
            "org_a",
            CreditKind::Refund,
            400,
            Some("crd_c"),
            None,
        ),
    ];
    for entry in &released {
        store.append_credit_entry(entry).unwrap();
    }
    let pure = fold_credits(&released).unwrap();
    let rollup = store.credit_balance(&organization).unwrap();
    assert_eq!(rollup, pure);
    assert_eq!(rollup.held_micro, 0);
    assert_eq!(rollup.pending_consumes, 0);
    assert_eq!(rollup.refunded_micro, 400);
    assert_eq!(rollup.balance_micro().unwrap(), 1_000);

    // A settled hold: the refund funds the balance but must never resurrect
    // a hold or change the settled debit.
    let settled = vec![
        credit("crd_g2", "org_a", CreditKind::Grant, 1_000, None, None),
        credit("crd_c2", "org_a", CreditKind::Consume, 400, None, None),
        credit(
            "crd_s2",
            "org_a",
            CreditKind::Settle,
            150,
            Some("crd_c2"),
            None,
        ),
        credit(
            "crd_r2",
            "org_a",
            CreditKind::Refund,
            50,
            Some("crd_c2"),
            None,
        ),
    ];
    for entry in &settled {
        store.append_credit_entry(entry).unwrap();
    }
    let mut all = released.clone();
    all.extend(settled.iter().cloned());
    let pure = fold_credits(&all).unwrap();
    let rollup = store.credit_balance(&organization).unwrap();
    assert_eq!(rollup, pure);
    assert_eq!(rollup.consumed_micro, 550);
    assert_eq!(rollup.held_micro, 0);
    assert_eq!(rollup.pending_consumes, 0);
    assert_eq!(rollup.balance_micro().unwrap(), 1_900);
}

/// A replayed record-before-call key must only replay an OPEN hold. Once the
/// hold is settled or released, the replay is a typed conflict (never a
/// fresh hold token that would let the caller dispatch an unpaid call).
#[test]
fn replaying_a_closed_hold_is_refused_on_both_stores() {
    let sqlite_dir = tempfile::tempdir().unwrap();
    for store in [
        Arc::new(MemoryBillingStore::new()) as Arc<dyn BillingStore>,
        Arc::new(SqliteControlPlaneStore::open(&sqlite_dir.path().join("b.db")).unwrap())
            as Arc<dyn BillingStore>,
    ] {
        let service = service_with(store);
        let organization = org("org_a");
        let acct = account("acct_1");
        service
            .ensure_account(&organization, &acct, "acct", true)
            .unwrap();
        service
            .grant_credits(&organization, &acct, 1_000, "seed", None)
            .unwrap();
        assert_eq!(
            service
                .consume_before_call(&organization, &acct, 400, None, "call", Some("hold"))
                .unwrap(),
            CreditAppend::Appended
        );
        let hold = service
            .credit_entry_id_by_idempotency_key(&organization, "hold")
            .unwrap()
            .unwrap();
        service
            .refund_consume(&organization, &acct, &hold, 400, "release")
            .unwrap();
        let before = service.credit_balance(&organization).unwrap();

        let replay = service
            .consume_before_call(&organization, &acct, 400, None, "call", Some("hold"))
            .unwrap_err();
        assert!(
            matches!(&replay, ControlPlaneError::Conflict(m) if m.contains("closed")),
            "a released hold cannot be replayed: {replay:?}"
        );
        assert_eq!(service.credit_balance(&organization).unwrap(), before);
        assert_eq!(
            service
                .credit_entries_page(&organization, 0, 100)
                .unwrap()
                .len(),
            3
        );

        // Same for a settled hold.
        service
            .consume_before_call(&organization, &acct, 200, None, "call", Some("hold2"))
            .unwrap();
        let hold2 = service
            .credit_entry_id_by_idempotency_key(&organization, "hold2")
            .unwrap()
            .unwrap();
        service
            .settle_consume(&organization, &acct, &hold2, 150, "actual")
            .unwrap();
        let before = service.credit_balance(&organization).unwrap();
        let replay = service
            .consume_before_call(&organization, &acct, 200, None, "call", Some("hold2"))
            .unwrap_err();
        assert!(
            matches!(&replay, ControlPlaneError::Conflict(m) if m.contains("closed")),
            "a settled hold cannot be replayed: {replay:?}"
        );
        assert_eq!(service.credit_balance(&organization).unwrap(), before);
    }
}

/// The store also refuses to settle a hold that was released (the service
/// path is not the only seam), and the credit rollup stays exact.
#[test]
fn settle_after_release_is_refused_at_the_store_and_on_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");
    for entry in [
        credit("crd_g", "org_a", CreditKind::Grant, 1_000, None, None),
        credit("crd_c", "org_a", CreditKind::Consume, 400, None, None),
        credit(
            "crd_r",
            "org_a",
            CreditKind::Refund,
            400,
            Some("crd_c"),
            None,
        ),
    ] {
        store.append_credit_entry(&entry).unwrap();
    }
    let before = store.credit_balance(&organization).unwrap();
    let error = store
        .append_credit_entry(&credit(
            "crd_s",
            "org_a",
            CreditKind::Settle,
            400,
            Some("crd_c"),
            None,
        ))
        .unwrap_err();
    assert!(
        matches!(
            error,
            BillingStoreError::Credit(CreditAppendRefusal::SettleAfterRefund { .. })
        ),
        "{error:?}"
    );
    assert_eq!(store.credit_balance(&organization).unwrap(), before);
    assert_eq!(before.balance_micro().unwrap(), 1_000);
}

/// The pure credit fold is defensive on its own: over-refund of a settled
/// consume, a duplicate settle, and a dangling reference are all typed
/// corruption, never a silently inflated balance.
#[test]
fn credit_fold_refuses_over_refund_duplicate_settle_and_dangling_reference() {
    let grant = credit("crd_g", "org_a", CreditKind::Grant, 1_000, None, None);
    let consume = credit("crd_c", "org_a", CreditKind::Consume, 400, None, None);
    let settle = credit(
        "crd_s",
        "org_a",
        CreditKind::Settle,
        150,
        Some("crd_c"),
        None,
    );

    let over = vec![
        grant.clone(),
        consume.clone(),
        settle.clone(),
        credit(
            "crd_r",
            "org_a",
            CreditKind::Refund,
            151,
            Some("crd_c"),
            None,
        ),
    ];
    match fold_credits(&over).unwrap_err() {
        faktor_cloud::billing::CreditLedgerError::Corrupt { detail } => {
            assert!(detail.contains("151") && detail.contains("150"), "{detail}");
        }
        other => panic!("expected corruption, got {other:?}"),
    }

    let duplicate = vec![
        grant.clone(),
        consume.clone(),
        settle.clone(),
        credit(
            "crd_s2",
            "org_a",
            CreditKind::Settle,
            100,
            Some("crd_c"),
            None,
        ),
    ];
    match fold_credits(&duplicate).unwrap_err() {
        faktor_cloud::billing::CreditLedgerError::Corrupt { detail } => {
            assert!(detail.contains("settled more than once"), "{detail}");
        }
        other => panic!("expected corruption, got {other:?}"),
    }

    let dangling = vec![
        grant,
        credit(
            "crd_r2",
            "org_a",
            CreditKind::Refund,
            10,
            Some("crd_absent"),
            None,
        ),
    ];
    match fold_credits(&dangling).unwrap_err() {
        faktor_cloud::billing::CreditLedgerError::Corrupt { detail } => {
            assert!(detail.contains("crd_absent"), "{detail}");
        }
        other => panic!("expected corruption, got {other:?}"),
    }
}

/// The store refuses a malformed credit entry (a settle with no reference)
/// typed — never an `expect` panic inside the balance guards — and writes
/// nothing on either backend.
#[test]
fn credit_store_refuses_malformed_entries_instead_of_panicking() {
    let mut bad = credit("crd_s", "org_a", CreditKind::Settle, 10, None, None);
    bad.id = CreditEntryId::try_new("crd_s").unwrap();
    let mem = MemoryBillingStore::new();
    match mem.append_credit_entry(&bad).unwrap_err() {
        BillingStoreError::Malformed(message) => {
            assert!(message.contains("must name the consume"), "{message}");
        }
        other => panic!("expected a typed malformed refusal, got {other:?}"),
    }
    assert!(mem.credit_entries(&org("org_a"), 0, 10).unwrap().is_empty());

    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    match sqlite.append_credit_entry(&bad).unwrap_err() {
        BillingStoreError::Malformed(message) => {
            assert!(message.contains("must name the consume"), "{message}");
        }
        other => panic!("expected a typed malformed refusal, got {other:?}"),
    }
    assert!(sqlite
        .credit_entries(&org("org_a"), 0, 10)
        .unwrap()
        .is_empty());
}

/// A correction may name an existing correction at the durable store seam.
/// The rollup must match the pure fold exactly: the superseded correction's
/// marker is counted ONCE (not again when it stops contributing).
#[test]
fn usage_rollup_matches_fold_when_a_correction_names_a_correction() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap();
    let organization = org("org_a");
    let base = cost_event("uev_base", "org_a", 100, 10, None);
    let first = cost_event("uev_c1", "org_a", 60, 20, Some("uev_base"));
    let second = cost_event("uev_c2", "org_a", 30, 30, Some("uev_c1"));
    for event in [&base, &first, &second] {
        store.append_usage_event(event).unwrap();
    }
    let pure = fold_usage(&organization, &[base, first, second]).unwrap();
    let rollup = store.usage_fold(&organization).unwrap();
    assert_eq!(rollup.totals, pure.totals);
    assert_eq!(rollup.per_task, pure.per_task);
    assert_eq!(rollup.totals.provider_cost_micro, 30);
    assert_eq!(rollup.totals.events, 1);
    assert_eq!(rollup.totals.corrected_events, 3);
}

/// A correction may zero a provider-cost fact (the provider refunded or the
/// charge was void): zero is legal ONLY as a correction, and the base is
/// fully superseded (no leftover cost, one effective event).
#[test]
fn a_zero_value_correction_supersedes_the_charged_base() {
    let service = service_with(Arc::new(MemoryBillingStore::new()));
    let organization = org("org_a");
    service
        .ensure_account(&organization, &account("acct_1"), "acct", true)
        .unwrap();
    let base = cost_event("uev_base", "org_a", 100, 10, None);
    service.record_usage(&base).unwrap();
    let zero = cost_event("uev_zero", "org_a", 0, 20, Some("uev_base"));
    assert_eq!(service.correct_usage(&zero).unwrap(), UsageAppend::Appended);
    let folded = service.fold(&organization).unwrap();
    assert_eq!(folded.totals.provider_cost_micro, 0);
    assert_eq!(folded.totals.events, 1);
    assert_eq!(folded.totals.corrected_events, 2);

    // A non-correction zero-cost row stays refused.
    let plain = cost_event("uev_plain", "org_a", 0, 30, None);
    let error = service.record_usage(&plain).unwrap_err();
    assert!(
        matches!(&error, ControlPlaneError::Malformed(m) if m.contains("non-zero cost")),
        "{error:?}"
    );
}

/// An in-flight transaction id is organization-scoped: a second tenant can
/// never overwrite the durable row (which would inject its organization
/// into the owner's protected-transaction view).
#[test]
fn in_flight_ids_are_never_overwritten_across_tenants() {
    use faktor_cloud::{InFlightKind, InFlightTxn, InFlightTxnId};

    let dir = tempfile::tempdir().unwrap();
    for store in [
        Arc::new(MemoryBillingStore::new()) as Arc<dyn BillingStore>,
        Arc::new(SqliteControlPlaneStore::open(&dir.path().join("b.db")).unwrap())
            as Arc<dyn BillingStore>,
    ] {
        let id = InFlightTxnId::try_new("ifx_shared").unwrap();
        let txn_a = InFlightTxn {
            id: id.clone(),
            organization: org("org_a"),
            kind: InFlightKind::Integration,
            reference: "merge-a".into(),
            started_ms: 10,
            ended_ms: None,
        };
        let txn_b = InFlightTxn {
            id: id.clone(),
            organization: org("org_b"),
            kind: InFlightKind::Completion,
            reference: "merge-b".into(),
            started_ms: 20,
            ended_ms: None,
        };
        store.begin_in_flight(&txn_a).unwrap();
        let error = store.begin_in_flight(&txn_b).unwrap_err();
        assert!(
            matches!(&error, BillingStoreError::Malformed(m) if m.contains("another organization")),
            "{error:?}"
        );
        let a = store.in_flight(&org("org_a")).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].organization, org("org_a"));
        assert_eq!(a[0].reference, "merge-a");
        assert_eq!(a[0].ended_ms, None);
        assert!(store.in_flight(&org("org_b")).unwrap().is_empty());
    }
}

/// A source-key replay is idempotent only for the SAME fact: a different
/// fact under an existing key is a typed collision, never a silent drop.
#[test]
fn usage_source_key_replays_are_fact_checked() {
    let dir = tempfile::tempdir().unwrap();
    let stores: [Arc<dyn BillingStore>; 2] = [
        Arc::new(MemoryBillingStore::new()),
        Arc::new(SqliteControlPlaneStore::open(&dir.path().join("billing.db")).unwrap()),
    ];
    for (index, store) in stores.into_iter().enumerate() {
        let organization = org(&format!("org_{index}"));
        let event = cost_event("uev_1", organization.as_str(), 100, 10, None);
        assert_eq!(
            store.append_usage_event(&event).unwrap(),
            UsageAppend::Appended
        );
        let mut replay = event.clone();
        replay.id = UsageEventId::try_new("uev_replayed").unwrap();
        assert_eq!(
            store.append_usage_event(&replay).unwrap(),
            UsageAppend::Duplicate,
            "the same durable fact replays"
        );
        let mut collision = replay.clone();
        collision.provider_cost_micro = 999;
        let error = store.append_usage_event(&collision).unwrap_err();
        assert!(
            matches!(&error, BillingStoreError::Malformed(m) if m.contains("different fact")),
            "{error:?}"
        );
        let rows = store.usage_events(&organization, 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.provider_cost_micro, 100);
    }
}

/// The pure credit fold never mixes tenants or account ledgers: a foreign
/// organization or a settle/refund across accounts is typed corruption.
#[test]
fn credit_fold_refuses_mixed_organizations_and_cross_account_references() {
    let mut foreign = credit("crd_g2", "org_b", CreditKind::Grant, 10, None, None);
    foreign.organization = org("org_b");
    let mixed = vec![
        credit("crd_g", "org_a", CreditKind::Grant, 100, None, None),
        foreign,
    ];
    match fold_credits(&mixed).unwrap_err() {
        faktor_cloud::billing::CreditLedgerError::Corrupt { detail } => {
            assert!(detail.contains("org_b"), "{detail}");
        }
        other => panic!("expected mixed-org corruption, got {other:?}"),
    }

    let consume = credit("crd_c", "org_a", CreditKind::Consume, 100, None, None);
    let mut refund = credit(
        "crd_r",
        "org_a",
        CreditKind::Refund,
        50,
        Some("crd_c"),
        None,
    );
    refund.billing_account_id = account("acct_other");
    let cross = vec![
        credit("crd_g", "org_a", CreditKind::Grant, 100, None, None),
        consume,
        refund,
    ];
    match fold_credits(&cross).unwrap_err() {
        faktor_cloud::billing::CreditLedgerError::Corrupt { detail } => {
            assert!(
                detail.contains("cross") || detail.contains("account"),
                "{detail}"
            );
        }
        other => panic!("expected cross-account corruption, got {other:?}"),
    }
}

/// A trialing subscription without an expiry is an unbounded trial; the
/// store seam refuses it typed (and refuses to read such a corrupt row).
#[test]
fn subscription_store_refuses_an_unbounded_trial() {
    use faktor_cloud::{Subscription, SubscriptionId, SubscriptionStatus};

    let dir = tempfile::tempdir().unwrap();
    for store in [
        Arc::new(MemoryBillingStore::new()) as Arc<dyn BillingStore>,
        Arc::new(SqliteControlPlaneStore::open(&dir.path().join("b.db")).unwrap())
            as Arc<dyn BillingStore>,
    ] {
        let bad = Subscription {
            id: SubscriptionId::try_new("sub_bad").unwrap(),
            organization: org("org_a"),
            plan_id: "pro".into(),
            status: SubscriptionStatus::Trialing,
            started_ms: 10,
            expires_ms: None,
            updated_ms: 10,
        };
        let error = store.put_subscription(&bad).unwrap_err();
        assert!(
            matches!(&error, BillingStoreError::Malformed(m) if m.contains("trialing")),
            "{error:?}"
        );
        assert!(store.subscription(&org("org_a")).unwrap().is_none());
    }
}
