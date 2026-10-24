use std::sync::{Arc, Barrier};

use faktor_payments::lease::{
    ConflictDomain, LeaseAcquisition, LeaseAmount, LeaseClaim, LeaseState, TerminalOutcome,
    UnknownReconciliation,
};
use faktor_payments::{
    FinancialResourceType, IdempotencyWrite, OperationClaimDraft, PaymentError, SqlitePaymentStore,
};
use rusqlite::{params, Connection};
use tempfile::TempDir;

const ORG: &str = "org_lease";

fn claim_draft(key: &str, digest: &str) -> OperationClaimDraft {
    OperationClaimDraft {
        organization_id: ORG.into(),
        idempotency_key: key.into(),
        operation: "pay_invoice".into(),
        resource_type: FinancialResourceType::Payment,
        resource_id: format!("pay_{key}"),
        request_digest: digest.into(),
        provider_idempotency_key: format!("{key}:charge"),
        context: "{}".into(),
        created_ms: 1_000,
    }
}

fn lease_claim(key: &str, digest: &str) -> LeaseClaim {
    LeaseClaim::new(key, key, digest)
}

fn invoice_domain(invoice_id: &str) -> ConflictDomain {
    ConflictDomain::InvoicePayment {
        invoice_id: invoice_id.into(),
    }
}

#[test]
fn same_claim_is_idempotent_and_amount_round_trips() {
    let store = SqlitePaymentStore::open_in_memory().unwrap();
    let domain = invoice_domain("inv_1");
    let resource = domain.logical_resource_id();
    let claim = lease_claim("key_1", "digest_1");
    let amount = LeaseAmount::new(2_500_000, "USD").unwrap();

    let first = store
        .try_acquire(ORG, &domain, &resource, &claim, Some(amount))
        .unwrap();
    let held = match first {
        LeaseAcquisition::Held(lease) => lease,
        other => panic!("expected the first acquisition to hold: {other:?}"),
    };
    assert_eq!(held.state, LeaseState::Held);
    assert_eq!(held.reserved_amount_micro, Some(2_500_000));
    assert_eq!(held.currency.as_deref(), Some("USD"));
    assert!(held.needs_reconciliation());

    let second = store
        .try_acquire(ORG, &domain, &resource, &claim, None)
        .unwrap();
    let replay = match second {
        LeaseAcquisition::AlreadyHeldBySameClaim(lease) => lease,
        other => panic!("expected same-claim idempotence: {other:?}"),
    };
    assert_eq!(replay.id, held.id);
    assert_eq!(replay.version, held.version);
    assert_eq!(
        store.leases_needing_reconciliation(ORG).unwrap().len(),
        1,
        "an idempotent acquisition never inserts a second row"
    );
}

#[test]
fn different_claims_and_keys_cannot_both_acquire() {
    let store = SqlitePaymentStore::open_in_memory().unwrap();
    let domain = invoice_domain("inv_1");
    let resource = domain.logical_resource_id();
    let first = store
        .try_acquire(
            ORG,
            &domain,
            &resource,
            &lease_claim("key_1", "digest_1"),
            None,
        )
        .unwrap();
    let held_id = match first {
        LeaseAcquisition::Held(lease) => lease.id,
        other => panic!("expected holder: {other:?}"),
    };

    for claim in [
        lease_claim("key_2", "digest_2"),
        LeaseClaim::new("claim_2", "key_1", "digest_1"),
        LeaseClaim::new("key_1", "key_different", "digest_1"),
    ] {
        match store
            .try_acquire(ORG, &domain, &resource, &claim, None)
            .unwrap()
        {
            LeaseAcquisition::Busy { holder } => {
                assert_eq!(holder.id, held_id);
                assert_eq!(holder.state, LeaseState::Held);
            }
            other => panic!("a competing claim must be busy: {other:?}"),
        }
    }
    assert_eq!(store.leases_needing_reconciliation(ORG).unwrap().len(), 1);

    let other_org = store
        .try_acquire(
            "org_other",
            &domain,
            &resource,
            &lease_claim("key_1", "digest_1"),
            None,
        )
        .unwrap();
    assert!(
        matches!(other_org, LeaseAcquisition::Held(_)),
        "the fence is organization-scoped"
    );
}

#[test]
fn terminal_release_allows_a_new_acquire() {
    let store = SqlitePaymentStore::open_in_memory().unwrap();
    let draft = claim_draft("key_terminal", "digest_terminal");
    assert!(matches!(
        store.claim_operation(&draft).unwrap(),
        faktor_payments::ClaimAcquisition::Acquired
    ));
    let domain = invoice_domain("inv_terminal");
    let resource = domain.logical_resource_id();
    let claim = LeaseClaim::from_draft(&draft);
    let lease = match store
        .try_acquire(ORG, &domain, &resource, &claim, None)
        .unwrap()
    {
        LeaseAcquisition::Held(lease) => lease,
        other => panic!("expected holder: {other:?}"),
    };

    assert!(matches!(
        store.release_terminal(ORG, &lease.id, TerminalOutcome::Failed),
        Err(PaymentError::Conflict(_))
    ));

    store
        .record_atomic(
            &IdempotencyWrite {
                organization_id: ORG.into(),
                key: draft.idempotency_key.clone(),
                operation: "pay_invoice".into(),
                request_hash: draft.request_digest.clone(),
                response: "{}".into(),
                created_ms: 2_000,
            },
            |_| Ok(()),
        )
        .unwrap();

    let terminal = store
        .release_terminal(ORG, &lease.id, TerminalOutcome::Succeeded)
        .unwrap();
    assert_eq!(terminal.state, LeaseState::Terminal);
    assert!(terminal.released_ms.is_some());
    assert!(!terminal.needs_reconciliation());
    assert!(store.active_for(ORG, &domain, &resource).unwrap().is_none());

    let next = store
        .try_acquire(
            ORG,
            &domain,
            &resource,
            &lease_claim("key_next", "digest_next"),
            None,
        )
        .unwrap();
    let next = match next {
        LeaseAcquisition::Held(lease) => lease,
        other => panic!("a terminal release must free the fence: {other:?}"),
    };
    assert_ne!(next.id, terminal.id);
    assert_eq!(
        store.lease(ORG, &terminal.id).unwrap().unwrap().state,
        LeaseState::Terminal,
        "the released history row is retained"
    );
    assert!(matches!(
        store.release_terminal(ORG, &terminal.id, TerminalOutcome::Succeeded),
        Err(PaymentError::IllegalState(_))
    ));
}

#[test]
fn unknown_cannot_be_released_without_reconciliation() {
    let store = SqlitePaymentStore::open_in_memory().unwrap();
    let draft = claim_draft("key_unknown", "digest_unknown");
    assert!(matches!(
        store.claim_operation(&draft).unwrap(),
        faktor_payments::ClaimAcquisition::Acquired
    ));
    let domain = invoice_domain("inv_unknown");
    let resource = domain.logical_resource_id();
    let claim = LeaseClaim::from_draft(&draft);
    let lease = match store
        .try_acquire(ORG, &domain, &resource, &claim, None)
        .unwrap()
    {
        LeaseAcquisition::Held(lease) => lease,
        other => panic!("expected holder: {other:?}"),
    };

    store
        .mark_claim_dispatched(ORG, &draft.idempotency_key, 2_000)
        .unwrap();
    store
        .mark_claim_unknown(ORG, &draft.idempotency_key, 3_000)
        .unwrap();

    assert!(matches!(
        store.renew(ORG, &lease.id),
        Err(PaymentError::Conflict(_))
    ));
    assert!(matches!(
        store.release_terminal(ORG, &lease.id, TerminalOutcome::Succeeded),
        Err(PaymentError::Conflict(_))
    ));
    assert!(matches!(
        store.release_no_effect(ORG, &lease.id),
        Err(PaymentError::Conflict(_))
    ));
    assert!(matches!(
        store.reconcile_unknown(ORG, &lease.id, UnknownReconciliation::NoEffect),
        Err(PaymentError::Conflict(_))
    ));
    assert_eq!(
        store
            .active_for(ORG, &domain, &resource)
            .unwrap()
            .unwrap()
            .id,
        lease.id,
        "an unknown outcome never silently frees the fence"
    );

    store
        .fail_unknown_claim(
            ORG,
            &draft.idempotency_key,
            "payment_provider_error",
            "provider reconciliation found no effect",
            4_000,
        )
        .unwrap();
    let released = store.release_no_effect(ORG, &lease.id).unwrap();
    assert_eq!(released.state, LeaseState::Released);
    assert!(released.released_ms.is_some());
    assert!(store.active_for(ORG, &domain, &resource).unwrap().is_none());

    let next = store
        .try_acquire(
            ORG,
            &domain,
            &resource,
            &lease_claim("key_retry", "digest_retry"),
            None,
        )
        .unwrap();
    assert!(matches!(next, LeaseAcquisition::Held(_)));
}

#[test]
fn reconciled_unknown_terminal_outcome_releases_the_fence() {
    let store = SqlitePaymentStore::open_in_memory().unwrap();
    let draft = claim_draft("key_reconciled", "digest_reconciled");
    assert!(matches!(
        store.claim_operation(&draft).unwrap(),
        faktor_payments::ClaimAcquisition::Acquired
    ));
    let domain = invoice_domain("inv_reconciled");
    let resource = domain.logical_resource_id();
    let lease = match store
        .try_acquire(
            ORG,
            &domain,
            &resource,
            &LeaseClaim::from_draft(&draft),
            None,
        )
        .unwrap()
    {
        LeaseAcquisition::Held(lease) => lease,
        other => panic!("expected holder: {other:?}"),
    };
    store
        .mark_claim_dispatched(ORG, &draft.idempotency_key, 2_000)
        .unwrap();
    store
        .mark_claim_unknown(ORG, &draft.idempotency_key, 3_000)
        .unwrap();
    assert!(matches!(
        store.reconcile_unknown(
            ORG,
            &lease.id,
            UnknownReconciliation::Terminal(TerminalOutcome::Succeeded)
        ),
        Err(PaymentError::Conflict(_))
    ));

    store
        .resolve_unknown_claim(ORG, &draft.idempotency_key, "{}", 4_000)
        .unwrap();
    let resolved = store
        .reconcile_unknown(
            ORG,
            &lease.id,
            UnknownReconciliation::Terminal(TerminalOutcome::Succeeded),
        )
        .unwrap();
    assert_eq!(resolved.state, LeaseState::Terminal);
    assert!(store.active_for(ORG, &domain, &resource).unwrap().is_none());
    let next = store
        .try_acquire(
            ORG,
            &domain,
            &resource,
            &lease_claim("key_after_reconcile", "digest_after_reconcile"),
            None,
        )
        .unwrap();
    assert!(matches!(next, LeaseAcquisition::Held(_)));
}

#[test]
fn partial_unique_index_admits_exactly_one_held_lease_under_concurrency() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("payment.db");
    let stores: Vec<SqlitePaymentStore> = (0..4)
        .map(|_| SqlitePaymentStore::open(&path).unwrap())
        .collect();
    let barrier = Arc::new(Barrier::new(stores.len()));
    let domain = invoice_domain("inv_race");
    let resource = domain.logical_resource_id();

    let handles: Vec<_> = stores
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let barrier = barrier.clone();
            let domain = domain.clone();
            let resource = resource.clone();
            std::thread::spawn(move || {
                let claim = LeaseClaim::new(
                    format!("claim_{index}"),
                    format!("key_{index}"),
                    format!("digest_{index}"),
                );
                barrier.wait();
                store
                    .try_acquire(ORG, &domain, &resource, &claim, None)
                    .unwrap()
            })
        })
        .collect();

    let results: Vec<LeaseAcquisition> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let mut held = Vec::new();
    let mut busy = Vec::new();
    for result in results {
        match result {
            LeaseAcquisition::Held(lease) => held.push(lease),
            LeaseAcquisition::Busy { holder } => busy.push(holder),
            other => panic!("unexpected concurrent acquisition: {other:?}"),
        }
    }
    assert_eq!(held.len(), 1, "exactly one concurrent acquirer wins");
    assert_eq!(busy.len(), 3);
    assert!(busy.iter().all(|holder| holder.id == held[0].id));
    let store = SqlitePaymentStore::open(&path).unwrap();
    assert_eq!(store.leases_needing_reconciliation(ORG).unwrap().len(), 1);
}

#[test]
fn restart_keeps_held_leases_as_reconciliation_obligations() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("payment.db");
    let draft = claim_draft("key_crash", "digest_crash");
    let domain = invoice_domain("inv_crash");
    let resource = domain.logical_resource_id();
    let lease_id;
    {
        let store = SqlitePaymentStore::open(&path).unwrap();
        assert!(matches!(
            store.claim_operation(&draft).unwrap(),
            faktor_payments::ClaimAcquisition::Acquired
        ));
        store
            .mark_claim_dispatched(ORG, &draft.idempotency_key, 2_000)
            .unwrap();
        let lease = match store
            .try_acquire(
                ORG,
                &domain,
                &resource,
                &LeaseClaim::from_draft(&draft),
                None,
            )
            .unwrap()
        {
            LeaseAcquisition::Held(lease) => lease,
            other => panic!("expected holder: {other:?}"),
        };
        lease_id = lease.id;
    }

    let store = SqlitePaymentStore::open(&path).unwrap();
    let claims = store.recover_claims(3_000).unwrap();
    assert_eq!(claims.unknown, 1);
    let recovery = store.recover_leases().unwrap();
    assert_eq!(recovery.held, 1);
    assert_eq!(recovery.needs_reconciliation, 1);
    let held = store.leases_needing_reconciliation(ORG).unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].id, lease_id);
    assert_eq!(held[0].state, LeaseState::Held);
    assert_eq!(
        store
            .active_for(ORG, &domain, &resource)
            .unwrap()
            .unwrap()
            .id,
        lease_id
    );
    assert!(matches!(
        store.release_no_effect(ORG, &lease_id),
        Err(PaymentError::Conflict(_))
    ));
    assert_eq!(
        store.recover_leases().unwrap().held,
        1,
        "recovery never frees a held lease"
    );
}

#[test]
fn v3_migration_backfills_in_flight_claims_as_held_leases() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("payment.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE payment_operation_claim (
                organization_id TEXT NOT NULL,
                idempotency_key TEXT NOT NULL,
                operation TEXT NOT NULL,
                resource_type TEXT NOT NULL,
                resource_id TEXT NOT NULL,
                request_digest TEXT NOT NULL,
                provider_idempotency_key TEXT NOT NULL,
                state TEXT NOT NULL,
                context TEXT NOT NULL,
                response TEXT,
                failure_code TEXT,
                failure_message TEXT,
                created_ms INTEGER NOT NULL,
                dispatched_ms INTEGER,
                resolved_ms INTEGER,
                updated_ms INTEGER NOT NULL,
                PRIMARY KEY (organization_id, idempotency_key)
            );
            CREATE TABLE payment_provider_object (
                provider TEXT NOT NULL,
                kind TEXT NOT NULL,
                organization_id TEXT NOT NULL,
                idempotency_key TEXT NOT NULL,
                object_id TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                PRIMARY KEY (provider, kind, organization_id, idempotency_key)
            );
            PRAGMA user_version = 3;",
        )
        .unwrap();
        let insert = |key: &str,
                      operation: &str,
                      resource: &str,
                      state: &str,
                      context: &str,
                      created_ms: i64| {
            conn.execute(
                "INSERT INTO payment_operation_claim \
                 (organization_id, idempotency_key, operation, resource_type, resource_id, \
                  request_digest, provider_idempotency_key, state, context, created_ms, updated_ms) \
                 VALUES (?1, ?2, ?3, 'payment', ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![
                    ORG,
                    key,
                    operation,
                    resource,
                    format!("digest_{key}"),
                    format!("provider_{key}"),
                    state,
                    context,
                    created_ms
                ],
            )
            .unwrap();
        };
        insert(
            "pay-key",
            "pay_invoice",
            "pay_1",
            "dispatched",
            "{\"request\":{\"invoice_id\":\"inv_1\"},\"currency\":\"USD\"}",
            10,
        );
        insert(
            "ref-key",
            "refund",
            "re_1",
            "unknown",
            "{\"request\":{\"payment_id\":\"pay_1\"}}",
            20,
        );
        insert(
            "issue-key",
            "issue_invoice",
            "inv_2",
            "claimed",
            "{\"request\":{\"subscription_id\":\"sub_1\"},\"invoice\":{\"period_start_ms\":100,\
             \"period_end_ms\":200,\"currency\":\"EUR\"}}",
            30,
        );
        insert(
            "pay-duplicate",
            "pay_invoice",
            "pay_2",
            "unknown",
            "{\"request\":{\"invoice_id\":\"inv_1\"},\"currency\":\"USD\"}",
            40,
        );
        insert(
            "subscribe-key",
            "subscribe",
            "sub_new",
            "dispatched",
            "{}",
            50,
        );
        insert("change-key", "change_plan", "sub_1", "dispatched", "{}", 60);
        insert(
            "done-key",
            "pay_invoice",
            "pay_done",
            "succeeded",
            "{\"request\":{\"invoice_id\":\"inv_done\"}}",
            70,
        );
    }

    let store = SqlitePaymentStore::open(&path).unwrap();
    let payment_domain = invoice_domain("inv_1");
    let payment = store
        .active_for(ORG, &payment_domain, &payment_domain.logical_resource_id())
        .unwrap()
        .expect("an in-flight payment is fenced after migration");
    assert_eq!(payment.state, LeaseState::Held);
    assert_eq!(payment.idempotency_key, "pay-key");
    assert_eq!(payment.operation_claim_id, "pay-key");
    assert_eq!(payment.currency.as_deref(), Some("USD"));
    assert_eq!(payment.reserved_amount_micro, None);

    let refund_domain = ConflictDomain::PaymentRefund {
        payment_id: "pay_1".into(),
    };
    let refund = store
        .active_for(ORG, &refund_domain, &refund_domain.logical_resource_id())
        .unwrap()
        .expect("an in-flight refund is fenced after migration");
    assert_eq!(refund.idempotency_key, "ref-key");

    let issue_domain = ConflictDomain::InvoiceIssue {
        subscription_id: "sub_1".into(),
        period: "100-200".into(),
    };
    let issue = store
        .active_for(ORG, &issue_domain, &issue_domain.logical_resource_id())
        .unwrap()
        .expect("an in-flight invoice issue is fenced after migration");
    assert_eq!(issue.idempotency_key, "issue-key");
    assert_eq!(issue.currency.as_deref(), Some("EUR"));

    let subscription_domain = ConflictDomain::Subscription {
        organization_id: ORG.into(),
    };
    let subscription = store
        .active_for(
            ORG,
            &subscription_domain,
            &subscription_domain.logical_resource_id(),
        )
        .unwrap()
        .expect("an in-flight subscription creation is fenced after migration");
    assert_eq!(subscription.idempotency_key, "subscribe-key");

    let mutation_domain = ConflictDomain::SubscriptionMutation {
        subscription_id: "sub_1".into(),
    };
    let mutation = store
        .active_for(
            ORG,
            &mutation_domain,
            &mutation_domain.logical_resource_id(),
        )
        .unwrap()
        .expect("an in-flight subscription mutation is fenced after migration");
    assert_eq!(mutation.idempotency_key, "change-key");

    let terminal_domain = invoice_domain("inv_done");
    assert!(store
        .active_for(
            ORG,
            &terminal_domain,
            &terminal_domain.logical_resource_id()
        )
        .unwrap()
        .is_none());
    assert_eq!(store.leases_needing_reconciliation(ORG).unwrap().len(), 5);
    drop(store);

    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, faktor_payments::store::PAYMENT_SCHEMA_VERSION);
}
