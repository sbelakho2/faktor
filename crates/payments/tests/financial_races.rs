// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir,separate-process
mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use faktor_cloud::Clock;
use faktor_payments::{
    CancelRequest, ChangePlanRequest, ChangeSeatsRequest, ClaimAcquisition, ConflictDomain,
    FinancialResourceType, InvoiceStatus, LeaseAcquisition, LeaseClaim, LeaseState,
    OperationClaimDraft, OperationClaimState, PaymentError, PaymentProvider, PaymentStatus,
    RefundOutcome, RefundStatus, SqlitePaymentStore, SubscriptionState, TerminalOutcome,
    UnknownReconciliation, WebhookDisposition,
};
use support::*;

const ITERATIONS: usize = 20;
const FULL: i64 = 10_000;

macro_rules! check {
    ($violations:expr, $condition:expr, $($arg:tt)*) => {
        if !$condition {
            $violations.push(format!($($arg)*));
        }
    };
}

fn terminal_or_absent(
    violations: &mut Vec<String>,
    fx: &RaceFixture,
    operation: &str,
    iteration: usize,
) {
    for claim in fx.store.operation_claims(ORG).unwrap() {
        if claim.operation == operation {
            check!(
                violations,
                claim.state == OperationClaimState::Succeeded
                    || claim.state == OperationClaimState::Failed,
                "iteration {iteration}: {operation} claim {} ended {} after the race",
                claim.idempotency_key,
                claim.state.as_str()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_pay_invoice_with_different_keys_charges_once() {
    let mut violations = Vec::new();
    for iteration in 0..ITERATIONS {
        let fx = RaceFixture::new();
        let subscription = fx.subscribe().await;
        let method = fx.add_method().await;
        let invoice = fx.issue_invoice(&subscription.id).await;
        check!(
            violations,
            invoice.total_minor == FULL,
            "iteration {iteration}: invoice total {} is not the $100 fixture",
            invoice.total_minor
        );

        let gate = fx.provider.block_before(CallKind::Charge);
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let futures = vec![
            fx.pay_future(
                invoice.id.clone(),
                method.id.clone(),
                "pay-a",
                start.clone(),
            ),
            fx.pay_future(
                invoice.id.clone(),
                method.id.clone(),
                "pay-b",
                start.clone(),
            ),
        ];
        let results = run_race(gate, 2, futures).await;

        let charges = fx.ledger.committed(CallKind::Charge);
        check!(
            violations,
            charges.len() == 1,
            "iteration {iteration}: two different-key pay_invoice calls made {} provider charges: {:?}",
            charges.len(),
            charges
                .iter()
                .map(|call| (&call.idempotency_key, call.amount_minor))
                .collect::<Vec<_>>()
        );
        check!(
            violations,
            fx.ledger.committed_total_minor(CallKind::Charge) == FULL,
            "iteration {iteration}: provider charged {} minor units instead of {FULL}",
            fx.ledger.committed_total_minor(CallKind::Charge)
        );
        let successes = results.iter().filter(|(_, result)| result.is_ok()).count();
        check!(
            violations,
            successes == 1,
            "iteration {iteration}: {successes} pay_invoice calls succeeded; results {:?}",
            results
                .iter()
                .map(|(key, result)| (
                    key.as_str(),
                    result.as_ref().map(|_| ()).map_err(|e| e.to_string())
                ))
                .collect::<Vec<_>>()
        );
        for (key, result) in &results {
            if let Err(error) = result {
                if !is_typed_refusal(error) {
                    violations.push(format!(
                        "iteration {iteration}: pay_invoice {key} failed untyped: {error:?}"
                    ));
                }
            }
        }

        let payments = fx.payments();
        check!(
            violations,
            payments.len() == 1,
            "iteration {iteration}: {} local payment rows were recorded for one invoice",
            payments.len()
        );
        check!(
            violations,
            payments
                .iter()
                .map(|payment| payment.amount_minor)
                .sum::<i64>()
                == FULL,
            "iteration {iteration}: recorded payments total {} minor units",
            payments
                .iter()
                .map(|payment| payment.amount_minor)
                .sum::<i64>()
        );
        if charges.len() == 1 {
            check!(
                violations,
                charges[0].amount_minor == Some(FULL),
                "iteration {iteration}: provider charge amount {:?} is not {FULL}",
                charges[0].amount_minor
            );
            if let (Some(object_id), Some(payment)) =
                (charges[0].object_id.as_deref(), payments.first())
            {
                check!(
                    violations,
                    object_id == payment.provider_payment_id,
                    "iteration {iteration}: local payment binds provider id {} instead of the single charge {object_id}",
                    payment.provider_payment_id
                );
            }
        }
        let charge_keys: BTreeSet<&str> = charges
            .iter()
            .map(|call| call.idempotency_key.as_str())
            .collect();
        check!(
            violations,
            charge_keys.len() == charges.len(),
            "iteration {iteration}: provider charge keys were reused: {charge_keys:?}"
        );
        if let Some((success_key, _)) = results.iter().find(|(_, result)| result.is_ok()) {
            if charges.len() == 1 {
                check!(
                    violations,
                    charges[0].idempotency_key == format!("{success_key}:charge"),
                    "iteration {iteration}: accepted pay_invoice {success_key} used provider key {}",
                    charges[0].idempotency_key
                );
            }
        }

        let stored = fx.invoice(&invoice.id);
        check!(
            violations,
            stored.amount_paid_minor == FULL,
            "iteration {iteration}: invoice paid {} minor units after a $100 charge",
            stored.amount_paid_minor
        );
        check!(
            violations,
            stored.status == InvoiceStatus::Paid,
            "iteration {iteration}: invoice status is {} after settlement",
            stored.status.as_str()
        );
        terminal_or_absent(&mut violations, &fx, "pay_invoice", iteration);
        let domain = ConflictDomain::InvoicePayment {
            invoice_id: invoice.id.clone(),
        };
        if let Some(held) = fx.store.active_for(ORG, &domain, &invoice.id).unwrap() {
            violations.push(format!(
                "iteration {iteration}: pay_invoice fence {} is still held after a terminal outcome",
                held.id
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "simultaneous pay_invoice violations ({} over {} iterations):\n{}",
        violations.len(),
        ITERATIONS,
        violations.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_full_refunds_with_different_keys_refund_once() {
    let mut violations = Vec::new();
    for iteration in 0..ITERATIONS {
        let fx = RaceFixture::new();
        let (invoice, payment) = fx.paid_payment().await;
        check!(
            violations,
            payment.status == PaymentStatus::Succeeded,
            "iteration {iteration}: fixture payment is {}",
            payment.status.as_str()
        );

        let gate = fx.provider.block_before(CallKind::Refund);
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let futures = vec![
            fx.refund_future(
                payment.id.clone(),
                FULL,
                "refund-full-a".into(),
                start.clone(),
            ),
            fx.refund_future(
                payment.id.clone(),
                FULL,
                "refund-full-b".into(),
                start.clone(),
            ),
        ];
        let results = run_race(gate, 2, futures).await;
        let committed = fx.ledger.committed(CallKind::Refund);
        check!(
            violations,
            committed.len() == 1,
            "iteration {iteration}: two different-key full refunds made {} provider refunds: {:?}",
            committed.len(),
            committed
                .iter()
                .map(|call| (&call.idempotency_key, call.amount_minor))
                .collect::<Vec<_>>()
        );
        check!(
            violations,
            fx.ledger.committed_total_minor(CallKind::Refund) == FULL,
            "iteration {iteration}: provider refunded {} minor units instead of {FULL}",
            fx.ledger.committed_total_minor(CallKind::Refund)
        );
        let successes = results.iter().filter(|(_, result)| result.is_ok()).count();
        check!(
            violations,
            successes == 1,
            "iteration {iteration}: {successes} full refunds succeeded"
        );
        for (key, result) in &results {
            if let Err(error) = result {
                if !is_typed_refusal(error) {
                    violations.push(format!(
                        "iteration {iteration}: refund {key} failed untyped: {error:?}"
                    ));
                }
            }
        }
        if let Some((success_key, _)) = results.iter().find(|(_, result)| result.is_ok()) {
            if committed.len() == 1 {
                check!(
                    violations,
                    committed[0].idempotency_key == format!("{success_key}:refund"),
                    "iteration {iteration}: accepted refund did not map to its provider key: {}",
                    committed[0].idempotency_key
                );
            }
        }

        let refunds = fx.refunds(&payment.id);
        check!(
            violations,
            refunds.len() == 1,
            "iteration {iteration}: {} local refund rows were recorded",
            refunds.len()
        );
        if let Some(refund) = refunds.first() {
            check!(
                violations,
                refund.status == RefundStatus::Succeeded,
                "iteration {iteration}: local refund is {}",
                refund.status.as_str()
            );
            check!(
                violations,
                refund.credit_note_id.is_some(),
                "iteration {iteration}: settled refund has no credit note"
            );
            check!(
                violations,
                refund.amount_minor == FULL,
                "iteration {iteration}: local refund amount is {}",
                refund.amount_minor
            );
        }
        let payments = fx.payments();
        if let Some(payment) = payments.first() {
            check!(
                violations,
                payment.refunded_minor == FULL,
                "iteration {iteration}: payment refunded {} minor units",
                payment.refunded_minor
            );
            check!(
                violations,
                payment.status == PaymentStatus::Refunded,
                "iteration {iteration}: payment status is {} after a full refund",
                payment.status.as_str()
            );
        }
        let stored = fx.invoice(&invoice.id);
        check!(
            violations,
            stored.amount_refunded_minor == FULL,
            "iteration {iteration}: invoice refunded {} minor units",
            stored.amount_refunded_minor
        );
        terminal_or_absent(&mut violations, &fx, "refund", iteration);
        let domain = ConflictDomain::PaymentRefund {
            payment_id: payment.id.clone(),
        };
        if let Some(held) = fx.store.active_for(ORG, &domain, &payment.id).unwrap() {
            violations.push(format!(
                "iteration {iteration}: refund fence {} is still held after a terminal outcome",
                held.id
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "simultaneous full-refund violations ({} over {} iterations):\n{}",
        violations.len(),
        ITERATIONS,
        violations.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_partial_refunds_never_exceed_collected() {
    const PARTIALS: usize = 8;
    const PARTIAL: i64 = 3_000;
    let mut violations = Vec::new();
    for iteration in 0..ITERATIONS {
        let fx = RaceFixture::new();
        let (invoice, payment) = fx.paid_payment().await;
        let gate = fx.provider.block_before(CallKind::Refund);
        let start = Arc::new(tokio::sync::Barrier::new(PARTIALS));
        let futures = (0..PARTIALS)
            .map(|index| {
                fx.refund_future(
                    payment.id.clone(),
                    PARTIAL,
                    format!("refund-part-{index}"),
                    start.clone(),
                )
            })
            .collect();
        let results = run_race(gate, PARTIALS, futures).await;

        let committed = fx.ledger.committed(CallKind::Refund);
        let provider_total: i64 = committed
            .iter()
            .map(|call| call.amount_minor.unwrap_or(0))
            .sum();
        check!(
            violations,
            provider_total <= FULL,
            "iteration {iteration}: provider refunded {provider_total} minor units from {FULL} collected across {} partial refunds",
            committed.len()
        );
        let successes: Vec<&(String, Result<RefundOutcome, PaymentError>)> = results
            .iter()
            .filter(|(_, result)| result.is_ok())
            .collect();
        let accepted_total: i64 = successes
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .map(|outcome| outcome.refund.amount_minor)
            .sum();
        check!(
            violations,
            provider_total == accepted_total,
            "iteration {iteration}: provider refund total {provider_total} differs from accepted {accepted_total}"
        );
        check!(
            violations,
            committed.len() == successes.len(),
            "iteration {iteration}: {} provider refunds but {} accepted local refunds",
            committed.len(),
            successes.len()
        );
        // The provider ledger is the independent oracle: its committed keys
        // are the expectation, the SUT's accepted outcomes are the observation.
        let provider_keys: BTreeSet<String> = committed
            .iter()
            .map(|call| call.idempotency_key.clone())
            .collect();
        let accepted_keys: BTreeSet<String> = successes
            .iter()
            .map(|(key, _)| format!("{key}:refund"))
            .collect();
        check!(
            violations,
            accepted_keys == provider_keys,
            "iteration {iteration}: accepted keys {accepted_keys:?} do not match provider oracle keys {provider_keys:?}"
        );
        for (key, result) in &results {
            if let Err(error) = result {
                if !is_typed_refusal(error) {
                    violations.push(format!(
                        "iteration {iteration}: partial refund {key} failed untyped: {error:?}"
                    ));
                }
            }
        }

        let refunds = fx.refunds(&payment.id);
        check!(
            violations,
            refunds.len() == successes.len(),
            "iteration {iteration}: {} accepted outcomes but {} persisted refund rows",
            successes.len(),
            refunds.len()
        );
        let mut credit_notes = BTreeSet::new();
        for refund in &refunds {
            check!(
                violations,
                refund.status == RefundStatus::Succeeded,
                "iteration {iteration}: accepted partial refund {} is {}",
                refund.id,
                refund.status.as_str()
            );
            if let Some(note_id) = &refund.credit_note_id {
                let note = fx
                    .store
                    .credit_note(ORG, note_id)
                    .unwrap()
                    .expect("credit note row is missing");
                credit_notes.insert(note.number);
            } else {
                violations.push(format!(
                    "iteration {iteration}: accepted partial refund {} has no credit note",
                    refund.id
                ));
            }
        }
        check!(
            violations,
            credit_notes.len() == successes.len(),
            "iteration {iteration}: {} accepted partial refunds produced {} unique credit notes",
            successes.len(),
            credit_notes.len()
        );

        // Provider ledger truth owns the expected status; never derive it
        // from the SUT's own accepted outcomes.
        let expected_status = if provider_total == FULL {
            PaymentStatus::Refunded
        } else if provider_total > 0 {
            PaymentStatus::PartiallyRefunded
        } else {
            PaymentStatus::Succeeded
        };
        let payments = fx.payments();
        if let Some(payment) = payments.first() {
            check!(
                violations,
                payment.refunded_minor == provider_total,
                "iteration {iteration}: payment refunded {} but provider oracle total is {provider_total}",
                payment.refunded_minor
            );
            check!(
                violations,
                payment.status == expected_status,
                "iteration {iteration}: payment status {} does not match provider oracle total {provider_total}",
                payment.status.as_str()
            );
        }
        let stored = fx.invoice(&invoice.id);
        check!(
            violations,
            stored.amount_refunded_minor == provider_total,
            "iteration {iteration}: invoice refunded {} but provider oracle total is {provider_total}",
            stored.amount_refunded_minor
        );
        terminal_or_absent(&mut violations, &fx, "refund", iteration);
        let domain = ConflictDomain::PaymentRefund {
            payment_id: payment.id.clone(),
        };
        if let Some(held) = fx.store.active_for(ORG, &domain, &payment.id).unwrap() {
            violations.push(format!(
                "iteration {iteration}: partial-refund fence {} is still held",
                held.id
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "concurrent partial-refund violations ({} over {} iterations):\n{}",
        violations.len(),
        ITERATIONS,
        violations.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_subscriptions_with_different_keys_create_one_external_subscription() {
    let mut violations = Vec::new();
    for iteration in 0..ITERATIONS {
        let fx = RaceFixture::new();
        let gate = fx.provider.block_before(CallKind::Subscribe);
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let futures = vec![
            fx.subscribe_future("subscribe-a".into(), start.clone()),
            fx.subscribe_future("subscribe-b".into(), start.clone()),
        ];
        let results = run_race(gate, 2, futures).await;

        let committed = fx.ledger.committed(CallKind::Subscribe);
        check!(
            violations,
            committed.len() == 1,
            "iteration {iteration}: two different-key subscribe calls created {} external subscriptions: {:?}",
            committed.len(),
            committed
                .iter()
                .map(|call| (&call.idempotency_key, call.quantity))
                .collect::<Vec<_>>()
        );
        let successes = results.iter().filter(|(_, result)| result.is_ok()).count();
        check!(
            violations,
            successes == 1,
            "iteration {iteration}: {successes} subscribe calls succeeded"
        );
        for (key, result) in &results {
            if let Err(error) = result {
                if !is_typed_refusal(error) {
                    violations.push(format!(
                        "iteration {iteration}: subscribe {key} failed untyped: {error:?}"
                    ));
                }
            }
        }
        if let Some((success_key, _)) = results.iter().find(|(_, result)| result.is_ok()) {
            if committed.len() == 1 {
                check!(
                    violations,
                    committed[0].idempotency_key == format!("{success_key}:subscribe"),
                    "iteration {iteration}: accepted subscribe {success_key} used provider key {}",
                    committed[0].idempotency_key
                );
            }
        }
        let provider_objects = ["subscribe-a:subscribe", "subscribe-b:subscribe"]
            .into_iter()
            .filter(|key| {
                fx.store
                    .provider_object(fx.provider.name(), "subscription", ORG, key)
                    .unwrap()
                    .is_some()
            })
            .count();
        check!(
            violations,
            provider_objects == 1,
            "iteration {iteration}: {provider_objects} durable provider subscription objects exist"
        );
        match fx.store.subscription(ORG).unwrap() {
            Some(subscription) => {
                if committed.len() == 1 {
                    check!(
                        violations,
                        committed[0].object_id.as_deref()
                            == Some(subscription.provider_subscription_id.as_str()),
                        "iteration {iteration}: local subscription binds {} but the single provider subscription is {:?}",
                        subscription.provider_subscription_id,
                        committed[0].object_id
                    );
                }
                check!(
                    violations,
                    subscription.state.is_live(),
                    "iteration {iteration}: surviving subscription is {}",
                    subscription.state.as_str()
                );
            }
            None => violations.push(format!(
                "iteration {iteration}: no local subscription survived a successful subscribe race"
            )),
        }
        terminal_or_absent(&mut violations, &fx, "subscribe", iteration);
        let domain = ConflictDomain::Subscription {
            organization_id: ORG.into(),
        };
        if let Some(held) = fx.store.active_for(ORG, &domain, ORG).unwrap() {
            violations.push(format!(
                "iteration {iteration}: subscription fence {} is still held",
                held.id
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "simultaneous subscribe violations ({} over {} iterations):\n{}",
        violations.len(),
        ITERATIONS,
        violations.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_plan_and_seat_changes_never_lose_an_update() {
    let mut violations = Vec::new();
    for iteration in 0..ITERATIONS {
        let fx = RaceFixture::new();
        let initial = fx.subscribe().await;
        let gate = fx.provider.block_before(CallKind::ChangePlan);
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let futures: Vec<RaceStep> = vec![
            fx.change_plan_step(initial.id.clone(), "team", "plan-change", start.clone()),
            fx.change_seats_step(initial.id.clone(), 3, "seat-change", start.clone()),
        ];
        let results = run_race(gate, 2, futures).await;
        let plan = results
            .iter()
            .find(|(key, _)| key == "plan-change")
            .expect("plan result is missing");
        let seats = results
            .iter()
            .find(|(key, _)| key == "seat-change")
            .expect("seat result is missing");
        for (key, result) in [plan, seats] {
            if let Err(error) = result {
                if !is_typed_refusal(error) {
                    violations.push(format!(
                        "iteration {iteration}: {key} failed untyped: {error:?}"
                    ));
                }
            }
        }
        let plan_ok = plan.1.is_ok();
        let seats_ok = seats.1.is_ok();
        check!(
            violations,
            plan_ok || seats_ok,
            "iteration {iteration}: both plan and seat changes failed"
        );

        let subscription = fx.subscription();
        let provider = fx.provider_subscription(&subscription.provider_subscription_id);
        if plan_ok && seats_ok {
            check!(
                violations,
                subscription.plan_id == "team" && subscription.quantity == 3,
                "iteration {iteration}: both changes succeeded but the durable subscription is plan {} x {} (lost update)",
                subscription.plan_id,
                subscription.quantity
            );
        } else if plan_ok {
            check!(
                violations,
                subscription.plan_id == "team" && subscription.quantity == 1,
                "iteration {iteration}: plan-only success left plan {} x {}",
                subscription.plan_id,
                subscription.quantity
            );
            check!(
                violations,
                matches!(&seats.1, Err(error) if is_typed_refusal(error)),
                "iteration {iteration}: seat change did not fail with a typed refusal"
            );
        } else if seats_ok {
            check!(
                violations,
                subscription.plan_id == "pro" && subscription.quantity == 3,
                "iteration {iteration}: seat-only success left plan {} x {}",
                subscription.plan_id,
                subscription.quantity
            );
            check!(
                violations,
                matches!(&plan.1, Err(error) if is_typed_refusal(error)),
                "iteration {iteration}: plan change did not fail with a typed refusal"
            );
        }

        let changes = fx.ledger.committed(CallKind::ChangePlan);
        let successes = usize::from(plan_ok) + usize::from(seats_ok);
        check!(
            violations,
            changes.len() == successes,
            "iteration {iteration}: {successes} accepted changes produced {} external changes",
            changes.len()
        );
        let mut expected_change_keys = BTreeSet::new();
        if plan_ok {
            expected_change_keys.insert("plan-change:change-plan");
        }
        if seats_ok {
            expected_change_keys.insert("seat-change:seats");
        }
        let actual_change_keys: BTreeSet<&str> = changes
            .iter()
            .map(|call| call.idempotency_key.as_str())
            .collect();
        check!(
            violations,
            actual_change_keys == expected_change_keys,
            "iteration {iteration}: accepted changes {expected_change_keys:?} used provider keys {actual_change_keys:?}"
        );
        for change in &changes {
            if let Some(object_id) = &change.object_id {
                check!(
                    violations,
                    object_id == &subscription.provider_subscription_id,
                    "iteration {iteration}: external change targeted {object_id} but the durable subscription is {}",
                    subscription.provider_subscription_id
                );
            }
        }
        check!(
            violations,
            provider.status == "active",
            "iteration {iteration}: provider subscription status {} after plan/seat changes",
            provider.status
        );
        check!(
            violations,
            provider.quantity == subscription.quantity,
            "iteration {iteration}: provider quantity {} differs from durable plan {} quantity {} (plan_ok={plan_ok} seats_ok={seats_ok}, changes={:?}, claims={:?})",
            provider.quantity,
            subscription.plan_id,
            subscription.quantity,
            changes
                .iter()
                .map(|call| (&call.idempotency_key, call.quantity, call.object_id.as_deref()))
                .collect::<Vec<_>>(),
            fx.store
                .operation_claims(ORG)
                .unwrap()
                .into_iter()
                .filter(|claim| claim.operation == "change_plan" || claim.operation == "change_seats")
                .map(|claim| (claim.operation, claim.idempotency_key, claim.state.as_str()))
                .collect::<Vec<_>>()
        );
        check!(
            violations,
            subscription.provider_item_id == provider.item_id
                && subscription.provider_product_id == provider.product_id
                && subscription.provider_price_id == provider.price_id,
            "iteration {iteration}: durable provider item identity {:?}/{:?}/{:?} contradicts provider {:?}/{:?}/{:?}",
            subscription.provider_item_id,
            subscription.provider_product_id,
            subscription.provider_price_id,
            provider.item_id,
            provider.product_id,
            provider.price_id
        );
        terminal_or_absent(&mut violations, &fx, "change_plan", iteration);
        terminal_or_absent(&mut violations, &fx, "change_seats", iteration);
        let domain = ConflictDomain::SubscriptionMutation {
            subscription_id: initial.id.clone(),
        };
        if let Some(held) = fx.store.active_for(ORG, &domain, &initial.id).unwrap() {
            violations.push(format!(
                "iteration {iteration}: subscription mutation fence {} is still held",
                held.id
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "concurrent plan/seat violations ({} over {} iterations):\n{}",
        violations.len(),
        ITERATIONS,
        violations.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cancel_and_upgrade_never_resurrect_the_subscription() {
    let mut violations = Vec::new();
    for iteration in 0..ITERATIONS {
        let fx = RaceFixture::new();
        let initial = fx.subscribe().await;
        let cancel_gate = fx.provider.block_before(CallKind::Cancel);
        let upgrade_gate = fx.provider.block_before(CallKind::ChangePlan);
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let futures: Vec<RaceStep> = vec![
            fx.cancel_step(initial.id.clone(), "cancel-race", start.clone()),
            fx.change_plan_step(initial.id.clone(), "team", "upgrade-race", start.clone()),
        ];
        let results = run_race_multi(vec![cancel_gate, upgrade_gate], 2, futures).await;
        let cancel = results
            .iter()
            .find(|(key, _)| key == "cancel-race")
            .expect("cancel result is missing");
        let upgrade = results
            .iter()
            .find(|(key, _)| key == "upgrade-race")
            .expect("upgrade result is missing");
        for (key, result) in [cancel, upgrade] {
            if let Err(error) = result {
                if !is_typed_refusal(error) {
                    violations.push(format!(
                        "iteration {iteration}: {key} failed untyped: {error:?}"
                    ));
                }
            }
        }
        let cancel_ok = cancel.1.is_ok();
        let upgrade_ok = upgrade.1.is_ok();
        check!(
            violations,
            cancel_ok || upgrade_ok,
            "iteration {iteration}: both cancel and upgrade failed"
        );

        let subscription = fx.subscription();
        let provider = fx.provider_subscription(&subscription.provider_subscription_id);
        if cancel_ok && upgrade_ok {
            check!(
                violations,
                subscription.state == SubscriptionState::Canceled,
                "iteration {iteration}: both cancel and upgrade succeeded but the subscription ended {} (resurrected)",
                subscription.state.as_str()
            );
        }
        if cancel_ok {
            check!(
                violations,
                subscription.state == SubscriptionState::Canceled,
                "iteration {iteration}: cancel succeeded but the subscription is {}",
                subscription.state.as_str()
            );
            check!(
                violations,
                provider.status == "canceled",
                "iteration {iteration}: cancel succeeded but the provider reports {}",
                provider.status
            );
            check!(
                violations,
                subscription.canceled_ms.is_some(),
                "iteration {iteration}: canceled subscription has no canceled_ms"
            );
        }
        if !cancel_ok {
            check!(
                violations,
                upgrade_ok,
                "iteration {iteration}: cancel failed without an accepted upgrade"
            );
            check!(
                violations,
                subscription.state == SubscriptionState::Active,
                "iteration {iteration}: failed cancel left subscription {}",
                subscription.state.as_str()
            );
            check!(
                violations,
                subscription.plan_id == "team",
                "iteration {iteration}: accepted upgrade left plan {}",
                subscription.plan_id
            );
            check!(
                violations,
                provider.status == "active",
                "iteration {iteration}: accepted upgrade left provider status {}",
                provider.status
            );
        }
        check!(
            violations,
            fx.ledger.committed(CallKind::Cancel).len() == usize::from(cancel_ok),
            "iteration {iteration}: cancel outcomes and external cancel calls disagree"
        );
        check!(
            violations,
            fx.ledger.committed(CallKind::ChangePlan).len() == usize::from(upgrade_ok),
            "iteration {iteration}: upgrade outcomes and external upgrade calls disagree"
        );
        let mut expected_keys = BTreeSet::new();
        if cancel_ok {
            expected_keys.insert("cancel-race:cancel".to_string());
        }
        if upgrade_ok {
            expected_keys.insert("upgrade-race:change-plan".to_string());
        }
        let actual_keys: BTreeSet<String> = fx
            .ledger
            .committed(CallKind::Cancel)
            .into_iter()
            .chain(fx.ledger.committed(CallKind::ChangePlan))
            .map(|call| call.idempotency_key)
            .collect();
        check!(
            violations,
            actual_keys == expected_keys,
            "iteration {iteration}: accepted cancel/upgrade {expected_keys:?} used provider keys {actual_keys:?}"
        );
        terminal_or_absent(&mut violations, &fx, "cancel", iteration);
        terminal_or_absent(&mut violations, &fx, "change_plan", iteration);
        let domain = ConflictDomain::SubscriptionMutation {
            subscription_id: initial.id.clone(),
        };
        if let Some(held) = fx.store.active_for(ORG, &domain, &initial.id).unwrap() {
            violations.push(format!(
                "iteration {iteration}: cancel/upgrade fence {} is still held",
                held.id
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "concurrent cancel/upgrade violations ({} over {} iterations):\n{}",
        violations.len(),
        ITERATIONS,
        violations.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_settlement_during_in_flight_refund_is_ordered_and_idempotent() {
    let fx = RaceFixture::new();
    let subscription = fx.subscribe().await;
    let method = fx.add_method().await;
    let invoice = fx.issue_invoice(&subscription.id).await;
    fx.provider.set_charge_status("processing");
    let charge = fx
        .service
        .pay_invoice(
            &org(),
            pay_request(&invoice.id, &method.id, "pay-processing"),
        )
        .await
        .unwrap();
    assert_eq!(charge.payment.status, PaymentStatus::Processing);
    assert_eq!(charge.invoice.amount_paid_minor, 0);

    let refund_gate = fx.provider.block_before(CallKind::Refund);
    let service = fx.service.clone();
    let organization = org();
    let payment_id = charge.payment.id.clone();
    let provider_payment_id = charge.payment.provider_payment_id.clone();
    let refund_task = tokio::spawn(async move {
        service
            .refund(
                &organization,
                refund_request(&payment_id, 4_000, "refund-during-webhook"),
            )
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        fx.provider.wait_before(CallKind::Refund, 1),
    )
    .await
    .expect("the outbound refund did not reach the provider boundary");

    let settled = fx
        .deliver_payment_succeeded(
            &provider_payment_id,
            invoice.total_minor,
            "evt_payment_during_refund",
        )
        .await;
    assert_eq!(settled.disposition, WebhookDisposition::Applied);
    let duplicate = fx
        .deliver_payment_succeeded(
            &provider_payment_id,
            invoice.total_minor,
            "evt_payment_during_refund",
        )
        .await;
    assert_eq!(duplicate.disposition, WebhookDisposition::Duplicate);
    let settled_invoice = fx.invoice(&invoice.id);
    assert_eq!(settled_invoice.amount_paid_minor, FULL);
    assert_eq!(settled_invoice.status, InvoiceStatus::Paid);

    refund_gate.open();
    let refund = refund_task.await.unwrap().unwrap();
    assert_eq!(refund.refund.amount_minor, 4_000);
    assert_eq!(refund.refund.status, RefundStatus::Succeeded);
    assert!(refund.credit_note.is_some());

    let payments = fx.payments();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].amount_minor, FULL);
    assert_eq!(payments[0].refunded_minor, 4_000);
    assert_eq!(payments[0].status, PaymentStatus::PartiallyRefunded);
    let final_invoice = fx.invoice(&invoice.id);
    assert_eq!(final_invoice.amount_paid_minor, FULL);
    assert_eq!(final_invoice.amount_refunded_minor, 4_000);
    assert_eq!(final_invoice.status, InvoiceStatus::Paid);
    let refunds = fx.refunds(&charge.payment.id);
    assert_eq!(refunds.len(), 1);
    assert_eq!(refunds[0].status, RefundStatus::Succeeded);
    assert_eq!(fx.ledger.committed(CallKind::Refund).len(), 1);
    assert_eq!(fx.ledger.committed_total_minor(CallKind::Refund), 4_000);
    let refund_domain = ConflictDomain::PaymentRefund {
        payment_id: charge.payment.id.clone(),
    };
    assert!(fx
        .store
        .active_for(ORG, &refund_domain, &charge.payment.id)
        .unwrap()
        .is_none());

    let late = fx
        .deliver_payment_succeeded(
            &provider_payment_id,
            invoice.total_minor,
            "evt_payment_during_refund",
        )
        .await;
    assert_eq!(late.disposition, WebhookDisposition::Duplicate);
    // The explicit field assertions above are the oracle; do NOT compare a
    // second SUT read back to the first SUT read (self-oracle).
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_provider_commit_reconciles_without_a_second_charge() {
    let fx = RaceFixture::new();
    let subscription = fx.subscribe().await;
    let method = fx.add_method().await;
    let invoice = fx.issue_invoice(&subscription.id).await;
    let after_commit = fx.provider.block_after_commit(CallKind::Charge);
    let service = fx.service.clone();
    let organization = org();
    let invoice_id = invoice.id.clone();
    let method_id = method.id.clone();
    let task = tokio::spawn(async move {
        service
            .pay_invoice(
                &organization,
                pay_request(&invoice_id, &method_id, "crash-pay"),
            )
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        fx.provider.wait_after_commit(CallKind::Charge, 1),
    )
    .await
    .expect("the charge did not commit before the simulated crash");
    let dispatched = fx.claim("crash-pay").expect("claim is missing");
    assert_eq!(dispatched.state, OperationClaimState::Dispatched);
    assert_eq!(fx.ledger.committed(CallKind::Charge).len(), 1);
    assert_eq!(fx.payments().len(), 0);
    task.abort();
    let _ = task.await;
    after_commit.open();

    let store = Arc::new(SqlitePaymentStore::open(&fx.path).unwrap());
    let provider = Arc::new(LedgerProvider::new(
        store.clone(),
        fx.clock.clone(),
        WEBHOOK_SECRET,
        fx.ledger.clone(),
    ));
    let restarted = fx.service_over(store.clone(), provider);
    let recovered = store.operation_claim(ORG, "crash-pay").unwrap().unwrap();
    assert_eq!(recovered.state, OperationClaimState::Unknown);
    let crash_domain = ConflictDomain::InvoicePayment {
        invoice_id: invoice.id.clone(),
    };
    let held = store
        .active_for(ORG, &crash_domain, &invoice.id)
        .unwrap()
        .expect("the acquired payment fence must survive the restart");
    assert_eq!(held.state, LeaseState::Held);
    assert_eq!(fx.payments().len(), 0);
    assert_eq!(restarted.payments(&org(), 50).unwrap().len(), 0);
    assert_eq!(
        restarted
            .invoice(&org(), &invoice.id)
            .unwrap()
            .amount_paid_minor,
        0
    );

    let duplicate = restarted
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "crash-pay"))
        .await;
    assert!(matches!(duplicate, Err(PaymentError::Conflict(_))));
    assert_eq!(fx.ledger.committed(CallKind::Charge).len(), 1);

    let resolved = restarted.reconcile_unknown_claims(&org()).await.unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].state, OperationClaimState::Succeeded);
    assert_eq!(fx.ledger.committed(CallKind::Charge).len(), 1);
    assert_eq!(
        fx.ledger.committed(CallKind::Charge)[0].idempotency_key,
        "crash-pay:charge"
    );
    let reconciles = fx.ledger.of_kind(CallKind::Reconcile);
    assert_eq!(reconciles.len(), 1);
    assert_eq!(reconciles[0].idempotency_key, "crash-pay:charge");
    let charges = fx.ledger.committed(CallKind::Charge);
    let payments = restarted.payments(&org(), 50).unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].amount_minor, FULL);
    assert_eq!(
        Some(payments[0].provider_payment_id.as_str()),
        charges[0].object_id.as_deref()
    );
    let paid = restarted.invoice(&org(), &invoice.id).unwrap();
    assert_eq!(paid.amount_paid_minor, FULL);
    assert_eq!(paid.status, InvoiceStatus::Paid);

    let replay = restarted
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "crash-pay"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.payment.id, payments[0].id);
    assert_eq!(fx.ledger.committed(CallKind::Charge).len(), 1);
    assert!(store
        .active_for(ORG, &crash_domain, &invoice.id)
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replaying_the_same_key_is_idempotent() {
    for iteration in 0..ITERATIONS {
        let fx = RaceFixture::new();
        let (invoice, payment) = fx.paid_payment().await;
        let pay = fx
            .service
            .pay_invoice(
                &org(),
                pay_request(
                    &invoice.id,
                    &payment.payment_method_id.clone().unwrap(),
                    "pay-invoice",
                ),
            )
            .await
            .unwrap();
        assert!(
            pay.replayed,
            "iteration {iteration}: same-key pay was not replayed"
        );
        assert_eq!(pay.payment.id, payment.id);
        assert_eq!(fx.ledger.committed(CallKind::Charge).len(), 1);
        assert_eq!(fx.payments().len(), 1);

        let first = fx
            .service
            .refund(&org(), refund_request(&payment.id, 2_500, "refund-replay"))
            .await
            .unwrap();
        assert!(!first.replayed);
        let second = fx
            .service
            .refund(&org(), refund_request(&payment.id, 2_500, "refund-replay"))
            .await
            .unwrap();
        assert!(
            second.replayed,
            "iteration {iteration}: same-key refund was not replayed"
        );
        assert_eq!(second.refund.id, first.refund.id);
        assert_eq!(fx.ledger.committed(CallKind::Refund).len(), 1);
        assert_eq!(fx.ledger.committed_total_minor(CallKind::Refund), 2_500);
        let refunds = fx.refunds(&payment.id);
        assert_eq!(refunds.len(), 1);
        assert_eq!(refunds[0].amount_minor, 2_500);
        assert!(refunds[0].credit_note_id.is_some());
        assert_eq!(fx.invoice(&invoice.id).amount_paid_minor, FULL);
        assert_eq!(fx.invoice(&invoice.id).amount_refunded_minor, 2_500);
        let changed = fx
            .service
            .refund(&org(), refund_request(&payment.id, 3_000, "refund-replay"))
            .await;
        assert!(matches!(changed, Err(PaymentError::IdempotencyConflict(_))));
        assert_eq!(fx.ledger.committed(CallKind::Refund).len(), 1);
    }
}

#[tokio::test]
async fn fence_release_is_refused_until_terminal_or_no_effect() {
    let fx = RaceFixture::new();
    let now = fx.clock.now_ms();
    let draft = OperationClaimDraft {
        organization_id: ORG.into(),
        idempotency_key: "fence-no-effect".into(),
        operation: "pay_invoice".into(),
        resource_type: FinancialResourceType::Payment,
        resource_id: "pay_fence".into(),
        request_digest: "a".repeat(64),
        provider_idempotency_key: "fence-no-effect:charge".into(),
        context: "{}".into(),
        created_ms: now,
    };
    assert_eq!(
        fx.store.claim_operation(&draft).unwrap(),
        ClaimAcquisition::Acquired
    );
    let domain = ConflictDomain::InvoicePayment {
        invoice_id: "inv_fence".into(),
    };
    let resource = domain.logical_resource_id();
    let lease = match fx
        .store
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
        other => panic!("the first claim must hold the fence: {other:?}"),
    };
    match fx
        .store
        .try_acquire(
            ORG,
            &domain,
            &resource,
            &LeaseClaim::new("fence-other", "fence-other", "b".repeat(64)),
            None,
        )
        .unwrap()
    {
        LeaseAcquisition::Busy { holder } => assert_eq!(holder.id, lease.id),
        other => panic!("a different claim must not acquire the held fence: {other:?}"),
    }
    for (label, attempt) in [
        (
            "terminal",
            fx.store
                .release_terminal(ORG, &lease.id, TerminalOutcome::Succeeded),
        ),
        ("no-effect", fx.store.release_no_effect(ORG, &lease.id)),
        (
            "reconcile",
            fx.store
                .reconcile_unknown(ORG, &lease.id, UnknownReconciliation::NoEffect),
        ),
    ] {
        assert!(
            matches!(attempt, Err(PaymentError::Conflict(_))),
            "a premature {label} release of a claimed fence must be refused"
        );
    }
    assert_eq!(
        fx.store.lease(ORG, &lease.id).unwrap().unwrap().state,
        LeaseState::Held
    );

    fx.store
        .mark_claim_dispatched(ORG, "fence-no-effect", now + 1)
        .unwrap();
    fx.store
        .mark_claim_unknown(ORG, "fence-no-effect", now + 2)
        .unwrap();
    assert!(matches!(
        fx.store.renew(ORG, &lease.id),
        Err(PaymentError::Conflict(_))
    ));
    for (label, attempt) in [
        (
            "terminal",
            fx.store
                .release_terminal(ORG, &lease.id, TerminalOutcome::Succeeded),
        ),
        ("no-effect", fx.store.release_no_effect(ORG, &lease.id)),
        (
            "reconcile",
            fx.store
                .reconcile_unknown(ORG, &lease.id, UnknownReconciliation::NoEffect),
        ),
    ] {
        assert!(
            matches!(attempt, Err(PaymentError::Conflict(_))),
            "an unknown fence must refuse a {label} release until reconciliation"
        );
    }
    assert_eq!(
        fx.store
            .active_for(ORG, &domain, &resource)
            .unwrap()
            .unwrap()
            .id,
        lease.id
    );

    fx.store
        .fail_unknown_claim(
            ORG,
            "fence-no-effect",
            "payment_provider_error",
            "provider reconciliation found no effect",
            now + 3,
        )
        .unwrap();
    let released = fx.store.release_no_effect(ORG, &lease.id).unwrap();
    assert_eq!(released.state, LeaseState::Released);
    assert!(released.released_ms.is_some());
    assert!(fx
        .store
        .active_for(ORG, &domain, &resource)
        .unwrap()
        .is_none());
    assert!(matches!(
        fx.store.release_no_effect(ORG, &lease.id),
        Err(PaymentError::IllegalState(_))
    ));

    let terminal_draft = OperationClaimDraft {
        idempotency_key: "fence-terminal".into(),
        resource_id: "pay_fence_terminal".into(),
        provider_idempotency_key: "fence-terminal:charge".into(),
        ..draft.clone()
    };
    assert_eq!(
        fx.store.claim_operation(&terminal_draft).unwrap(),
        ClaimAcquisition::Acquired
    );
    let terminal_lease = match fx
        .store
        .try_acquire(
            ORG,
            &domain,
            &resource,
            &LeaseClaim::from_draft(&terminal_draft),
            None,
        )
        .unwrap()
    {
        LeaseAcquisition::Held(lease) => lease,
        other => panic!("a terminal test claim must acquire the freed fence: {other:?}"),
    };
    fx.store
        .mark_claim_dispatched(ORG, "fence-terminal", now + 4)
        .unwrap();
    fx.store
        .mark_claim_unknown(ORG, "fence-terminal", now + 5)
        .unwrap();
    fx.store
        .resolve_unknown_claim(ORG, "fence-terminal", "{}", now + 6)
        .unwrap();
    let terminal = fx
        .store
        .release_terminal(ORG, &terminal_lease.id, TerminalOutcome::Succeeded)
        .unwrap();
    assert_eq!(terminal.state, LeaseState::Terminal);
    assert!(!terminal.needs_reconciliation());
    assert!(fx
        .store
        .active_for(ORG, &domain, &resource)
        .unwrap()
        .is_none());
    assert!(matches!(
        fx.store
            .release_terminal(ORG, &terminal_lease.id, TerminalOutcome::Succeeded),
        Err(PaymentError::IllegalState(_))
    ));

    let held_draft = OperationClaimDraft {
        idempotency_key: "fence-held".into(),
        resource_id: "pay_fence_held".into(),
        provider_idempotency_key: "fence-held:charge".into(),
        ..draft
    };
    assert_eq!(
        fx.store.claim_operation(&held_draft).unwrap(),
        ClaimAcquisition::Acquired
    );
    let held_lease = match fx
        .store
        .try_acquire(
            ORG,
            &domain,
            &resource,
            &LeaseClaim::from_draft(&held_draft),
            None,
        )
        .unwrap()
    {
        LeaseAcquisition::Held(lease) => lease,
        other => panic!("the held recovery claim must acquire the fence: {other:?}"),
    };
    fx.store
        .mark_claim_dispatched(ORG, "fence-held", now + 7)
        .unwrap();
    let recovery = fx.store.recover_leases().unwrap();
    assert!(recovery.held >= 1);
    assert_eq!(recovery.needs_reconciliation, recovery.held);
    let recovered = fx
        .store
        .lease(ORG, &held_lease.id)
        .unwrap()
        .expect("startup recovery must retain the held lease");
    assert_eq!(recovered.state, LeaseState::Held);
    assert!(recovered.version > held_lease.version);
    assert!(fx
        .store
        .active_for(ORG, &domain, &resource)
        .unwrap()
        .is_some());
    fx.store
        .mark_claim_unknown(ORG, "fence-held", now + 8)
        .unwrap();
    fx.store
        .fail_unknown_claim(
            ORG,
            "fence-held",
            "payment_provider_error",
            "provider reconciliation found no effect",
            now + 9,
        )
        .unwrap();
    fx.store.release_no_effect(ORG, &held_lease.id).unwrap();
    assert!(fx
        .store
        .leases_needing_reconciliation(ORG)
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_during_an_in_flight_cancel_is_refused() {
    let mut violations = Vec::new();
    for iteration in 0..ITERATIONS {
        let fx = RaceFixture::new();
        let initial = fx.subscribe().await;
        let cancel_gate = fx.provider.block_before(CallKind::Cancel);
        let service = fx.service.clone();
        let organization = org();
        let subscription_id = initial.id.clone();
        let cancel_task = tokio::spawn(async move {
            service
                .cancel(
                    &organization,
                    CancelRequest {
                        subscription_id,
                        at_period_end: false,
                        idempotency_key: "cancel-in-flight".into(),
                    },
                )
                .await
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            fx.provider.wait_before(CallKind::Cancel, 1),
        )
        .await
        .expect("the cancel did not reach the provider boundary");

        let upgrade = tokio::time::timeout(
            Duration::from_secs(5),
            fx.service.change_plan(
                &org(),
                ChangePlanRequest {
                    subscription_id: initial.id.clone(),
                    plan_id: "team".into(),
                    interval: None,
                    idempotency_key: "upgrade-during-cancel".into(),
                },
            ),
        )
        .await
        .expect("the upgrade blocked behind the in-flight cancel instead of refusing");
        match upgrade {
            Err(error) if is_typed_refusal(&error) => {}
            Err(error) => violations.push(format!(
                "iteration {iteration}: upgrade during in-flight cancel failed untyped: {error:?}"
            )),
            Ok(_) => violations.push(format!(
                "iteration {iteration}: upgrade was accepted while cancel was in flight"
            )),
        }
        cancel_gate.open();
        let canceled = cancel_task.await.unwrap().unwrap();
        check!(
            violations,
            canceled.subscription.state == SubscriptionState::Canceled,
            "iteration {iteration}: in-flight cancel ended {}",
            canceled.subscription.state.as_str()
        );
        let subscription = fx.subscription();
        check!(
            violations,
            subscription.state == SubscriptionState::Canceled,
            "iteration {iteration}: durable subscription is {} after cancel",
            subscription.state.as_str()
        );
        check!(
            violations,
            subscription.plan_id == "pro",
            "iteration {iteration}: refused upgrade changed the plan to {}",
            subscription.plan_id
        );
        let provider = fx.provider_subscription(&subscription.provider_subscription_id);
        check!(
            violations,
            provider.status == "canceled",
            "iteration {iteration}: provider subscription is {} after a canceled outcome",
            provider.status
        );
        check!(
            violations,
            fx.ledger.committed(CallKind::Cancel).len() == 1,
            "iteration {iteration}: cancel made {} provider calls",
            fx.ledger.committed(CallKind::Cancel).len()
        );
        if let Some(cancel_call) = fx.ledger.committed(CallKind::Cancel).first() {
            check!(
                violations,
                cancel_call.idempotency_key == "cancel-in-flight:cancel",
                "iteration {iteration}: cancel used provider key {}",
                cancel_call.idempotency_key
            );
        }
        check!(
            violations,
            fx.ledger.committed(CallKind::ChangePlan).is_empty(),
            "iteration {iteration}: refused upgrade still reached the provider"
        );
        let domain = ConflictDomain::SubscriptionMutation {
            subscription_id: initial.id.clone(),
        };
        if let Some(held) = fx.store.active_for(ORG, &domain, &initial.id).unwrap() {
            violations.push(format!(
                "iteration {iteration}: subscription mutation fence {} stayed held after cancel",
                held.id
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "cancel-in-flight violations ({} over {} iterations):\n{}",
        violations.len(),
        ITERATIONS,
        violations.join("\n")
    );
}

#[derive(Clone, Copy, Debug)]
enum FenceMutation {
    ChangePlan,
    ChangeSeats,
    Cancel,
}

impl FenceMutation {
    const ALL: [FenceMutation; 3] = [
        FenceMutation::ChangePlan,
        FenceMutation::ChangeSeats,
        FenceMutation::Cancel,
    ];

    const fn label(self) -> &'static str {
        match self {
            FenceMutation::ChangePlan => "change_plan",
            FenceMutation::ChangeSeats => "change_seats",
            FenceMutation::Cancel => "cancel",
        }
    }

    const fn call_kind(self) -> CallKind {
        match self {
            FenceMutation::ChangePlan | FenceMutation::ChangeSeats => CallKind::ChangePlan,
            FenceMutation::Cancel => CallKind::Cancel,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payment_webhooks_never_bypass_a_subscription_mutation_fence() {
    for mutation in FenceMutation::ALL {
        for failed_webhook in [false, true] {
            let fx = RaceFixture::new();
            let subscription = fx.subscribe().await;
            let method = fx.add_method().await;
            let invoice = fx.issue_invoice(&subscription.id).await;
            let provider_payment_id = if failed_webhook {
                fx.provider.set_charge_status("pending");
                let pending = fx
                    .service
                    .pay_invoice(
                        &org(),
                        pay_request(&invoice.id, &method.id, "race-pending-payment"),
                    )
                    .await
                    .unwrap();
                assert_eq!(pending.payment.status, PaymentStatus::Pending);
                pending.payment.provider_payment_id
            } else {
                format!("tr_{}_fenced", mutation.label())
            };
            let mut past_due = fx.store.subscription(ORG).unwrap().unwrap();
            past_due.state = SubscriptionState::PastDue;
            past_due.dunning_attempts = 1;
            past_due.grace_end_ms = Some(fx.clock.now_ms() + 60_000);
            fx.store.put_subscription(&past_due).unwrap();

            let kind = mutation.call_kind();
            let gate = fx.provider.block_before(kind);
            let service = fx.service.clone();
            let subscription_id = subscription.id.clone();
            let task = tokio::spawn(async move {
                match mutation {
                    FenceMutation::ChangePlan => service
                        .change_plan(
                            &org(),
                            ChangePlanRequest {
                                subscription_id,
                                plan_id: "team".into(),
                                interval: None,
                                idempotency_key: "race-fence-change-plan".into(),
                            },
                        )
                        .await
                        .map(|_| ()),
                    FenceMutation::ChangeSeats => service
                        .change_seats(
                            &org(),
                            ChangeSeatsRequest {
                                subscription_id,
                                quantity: 2,
                                idempotency_key: "race-fence-change-seats".into(),
                            },
                        )
                        .await
                        .map(|_| ()),
                    FenceMutation::Cancel => service
                        .cancel(
                            &org(),
                            CancelRequest {
                                subscription_id,
                                at_period_end: false,
                                idempotency_key: "race-fence-cancel".into(),
                            },
                        )
                        .await
                        .map(|_| ()),
                }
            });
            tokio::time::timeout(Duration::from_secs(5), fx.provider.wait_before(kind, 1))
                .await
                .expect("the subscription mutation did not reach the provider boundary");

            let event_id = format!(
                "evt_{}_{}",
                mutation.label(),
                if failed_webhook {
                    "failed"
                } else {
                    "succeeded"
                }
            );
            let (headers, body) = if failed_webhook {
                fx.payment_failed_webhook(&provider_payment_id, &invoice.id, &event_id)
            } else {
                fx.payment_succeeded_webhook_for_invoice(
                    &provider_payment_id,
                    invoice.total_minor,
                    &invoice.id,
                    &event_id,
                )
            };
            let delivered = fx
                .service
                .handle_webhook(&headers, &body, fx.clock.now_ms())
                .await;

            let during = fx.store.subscription(ORG).unwrap().unwrap();
            if failed_webhook {
                assert!(
                    matches!(delivered, Err(PaymentError::Conflict(_))),
                    "{} payment.failed applied inside a held mutation fence: {delivered:?}",
                    mutation.label()
                );
                assert_eq!(
                    during,
                    past_due,
                    "{} payment.failed wrote dunning over a held mutation fence",
                    mutation.label()
                );
                assert_eq!(
                    fx.invoice(&invoice.id).status,
                    InvoiceStatus::Open,
                    "{} deferred payment.failed recorded an invoice effect",
                    mutation.label()
                );
            } else {
                let outcome = delivered.unwrap();
                assert_eq!(
                    outcome.disposition,
                    WebhookDisposition::Applied,
                    "{} payment.succeeded was not applied",
                    mutation.label()
                );
                assert_eq!(
                    during,
                    past_due,
                    "{} payment.succeeded overwrote the fenced subscription",
                    mutation.label()
                );
                assert_eq!(
                    fx.invoice(&invoice.id).status,
                    InvoiceStatus::Paid,
                    "{} payment.succeeded did not settle its invoice",
                    mutation.label()
                );
            }
            assert!(
                fx.ledger.committed(kind).is_empty(),
                "{} webhook reached the provider or completed the blocked mutation",
                mutation.label()
            );

            gate.open();
            task.await.unwrap().unwrap();
            assert_eq!(fx.ledger.committed(kind).len(), 1);

            let final_invoice = fx.invoice(&invoice.id);
            let final_subscription = fx.store.subscription(ORG).unwrap().unwrap();
            if failed_webhook {
                let retried = fx
                    .service
                    .handle_webhook(&headers, &body, fx.clock.now_ms())
                    .await
                    .unwrap();
                assert_eq!(
                    retried.disposition,
                    WebhookDisposition::Applied,
                    "{} deferred payment.failed was not applied exactly once on retry",
                    mutation.label()
                );
                assert_eq!(
                    fx.invoice(&invoice.id).status,
                    InvoiceStatus::PastDue,
                    "{} retried payment.failed did not dunn the invoice",
                    mutation.label()
                );
                assert_eq!(fx.payments()[0].status, PaymentStatus::Failed);
                let after = fx.store.subscription(ORG).unwrap().unwrap();
                match mutation {
                    FenceMutation::Cancel => {
                        assert_eq!(after.state, SubscriptionState::Canceled);
                        assert_eq!(
                            after.dunning_attempts, past_due.dunning_attempts,
                            "a canceled subscription must not accrue dunning after the fence"
                        );
                    }
                    FenceMutation::ChangePlan | FenceMutation::ChangeSeats => {
                        assert_eq!(after.state, SubscriptionState::Canceled);
                        assert_eq!(after.dunning_attempts, past_due.dunning_attempts + 1);
                        assert_eq!(after.canceled_ms, Some(fx.clock.now_ms()));
                    }
                }
                assert_eq!(after.grace_end_ms, past_due.grace_end_ms);
            } else {
                assert_eq!(
                    final_invoice.status,
                    InvoiceStatus::Paid,
                    "{} payment.succeeded did not remain settled",
                    mutation.label()
                );
                match mutation {
                    FenceMutation::ChangePlan => {
                        assert_eq!(final_subscription.plan_id, "team");
                        assert_eq!(final_subscription.state, SubscriptionState::PastDue);
                    }
                    FenceMutation::ChangeSeats => {
                        assert_eq!(final_subscription.quantity, 2);
                        assert_eq!(final_subscription.state, SubscriptionState::PastDue);
                    }
                    FenceMutation::Cancel => {
                        assert_eq!(final_subscription.state, SubscriptionState::Canceled);
                    }
                }
                assert_eq!(
                    final_subscription.dunning_attempts,
                    past_due.dunning_attempts,
                    "{} payment.succeeded reset dunning across the fence",
                    mutation.label()
                );
                assert_eq!(final_subscription.grace_end_ms, past_due.grace_end_ms);
            }
            let domain = ConflictDomain::SubscriptionMutation {
                subscription_id: subscription.id.clone(),
            };
            assert!(
                fx.store
                    .active_for(ORG, &domain, &subscription.id)
                    .unwrap()
                    .is_none(),
                "{} left the subscription mutation fence held",
                mutation.label()
            );
        }
    }
}
