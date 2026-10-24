//! Durable fencing for subscription, plan, seat, cancel and period-invoice
//! mutations.
//!
//! The operation claims serialize retries of one idempotency key. These
//! helpers add the resource fence: `Subscription{org}` for creation,
//! `SubscriptionMutation{subscription_id}` for every change/cancel and
//! `InvoiceIssue{subscription_id,period}` for a period invoice. A fence is
//! acquired before any provider call and stays occupied through
//! `dispatched`/`unknown`; it is released only after a terminal claim outcome
//! or a reconciliation that proved no provider effect.

use rusqlite::params;

use crate::error::PaymentError;
use crate::lease::{
    ConflictDomain, FinancialMutationLease, LeaseAcquisition, LeaseClaim, TerminalOutcome,
};
use crate::model::{OperationClaimState, PaymentSubscription};
use crate::store::SqlitePaymentStore;

/// Acquire the one fence for a domain/resource. A different claimant gets a
/// typed conflict and never reaches the provider.
pub(crate) fn acquire(
    store: &SqlitePaymentStore,
    organization_id: &str,
    domain: &ConflictDomain,
    resource: &str,
    idempotency_key: &str,
    request_digest: &str,
) -> Result<FinancialMutationLease, PaymentError> {
    let claim = LeaseClaim::new(idempotency_key, idempotency_key, request_digest);
    match store.try_acquire(organization_id, domain, resource, &claim, None)? {
        LeaseAcquisition::Held(lease) | LeaseAcquisition::AlreadyHeldBySameClaim(lease) => {
            Ok(lease)
        }
        LeaseAcquisition::Busy { holder } => Err(PaymentError::Conflict(format!(
            "{} is fenced by operation {}; retry after it reaches a terminal outcome",
            domain.as_str(),
            holder.idempotency_key
        ))),
    }
}

/// Remove a claim that was inserted for a fence that could not be acquired.
/// The provider was never called, so the same key may retry.
pub(crate) fn abandon_claim(
    store: &SqlitePaymentStore,
    organization_id: &str,
    idempotency_key: &str,
) -> Result<(), PaymentError> {
    store.tx(|tx| {
        tx.execute(
            "DELETE FROM payment_operation_claim \
             WHERE organization_id = ?1 AND idempotency_key = ?2 AND state = 'claimed'",
            params![organization_id, idempotency_key],
        )
        .map_err(|error| PaymentError::Store(format!("abandon fenced operation claim: {error}")))?;
        Ok(())
    })
}

/// Release a fence exactly when its claim is terminal. An in-flight or
/// unknown claim keeps the fence occupied for reconciliation.
pub(crate) fn settle(
    store: &SqlitePaymentStore,
    organization_id: &str,
    lease: &FinancialMutationLease,
    idempotency_key: &str,
) -> Result<(), PaymentError> {
    match claim_state(store, organization_id, idempotency_key)? {
        Some(OperationClaimState::Succeeded) => release_terminal(store, organization_id, lease),
        Some(OperationClaimState::Failed) => release_no_effect(store, organization_id, lease),
        _ => Ok(()),
    }
}

fn owns_invoice_payment_lease(lease: &FinancialMutationLease) -> bool {
    matches!(
        lease.conflict_domain,
        ConflictDomain::InvoicePayment { .. } | ConflictDomain::PaymentRefund { .. }
    )
}
fn claim_state(
    store: &SqlitePaymentStore,
    organization_id: &str,
    idempotency_key: &str,
) -> Result<Option<OperationClaimState>, PaymentError> {
    Ok(store
        .operation_claim(organization_id, idempotency_key)?
        .map(|claim| claim.state))
}

fn release_terminal(
    store: &SqlitePaymentStore,
    organization_id: &str,
    lease: &FinancialMutationLease,
) -> Result<(), PaymentError> {
    match store.release_terminal(organization_id, &lease.id, TerminalOutcome::Succeeded) {
        Ok(_) | Err(PaymentError::NotFound(_)) | Err(PaymentError::IllegalState(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

fn release_no_effect(
    store: &SqlitePaymentStore,
    organization_id: &str,
    lease: &FinancialMutationLease,
) -> Result<(), PaymentError> {
    match store.release_no_effect(organization_id, &lease.id) {
        Ok(_) | Err(PaymentError::NotFound(_)) | Err(PaymentError::IllegalState(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Release fences left held by a crash after a terminal commit, and orphan
/// fences whose claim was discarded at startup (no provider effect exists).
pub(crate) fn release_resolved(
    store: &SqlitePaymentStore,
    organization_id: &str,
) -> Result<usize, PaymentError> {
    let mut released = 0;
    for lease in store.leases_needing_reconciliation(organization_id)? {
        if owns_invoice_payment_lease(&lease) {
            continue;
        }
        match claim_state(store, organization_id, &lease.idempotency_key)? {
            Some(OperationClaimState::Succeeded) => {
                release_terminal(store, organization_id, &lease)?;
                released += 1;
            }
            Some(OperationClaimState::Failed) => {
                release_no_effect(store, organization_id, &lease)?;
                released += 1;
            }
            Some(_) => {}
            None => {
                release_orphan(store, organization_id, &lease)?;
                released += 1;
            }
        }
    }
    Ok(released)
}

/// Startup recovery for every organization: a fence whose claim is terminal
/// or absent has no unresolved provider effect and is released; every other
/// held fence stays occupied and needs reconciliation.
pub(crate) fn recover_leases_after_claim_recovery(
    store: &SqlitePaymentStore,
) -> Result<usize, PaymentError> {
    let held: Vec<(String, String, String)> = store.tx(|tx| {
        let mut statement = tx
            .prepare(
                "SELECT organization_id, id, idempotency_key \
                 FROM financial_mutation_lease WHERE state = 'held' \
                 ORDER BY acquired_ms, id",
            )
            .map_err(|error| PaymentError::Store(format!("read held leases: {error}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| PaymentError::Store(format!("read held leases: {error}")))?;
        let mut leases = Vec::new();
        for row in rows {
            leases.push(
                row.map_err(|error| PaymentError::Store(format!("read held leases: {error}")))?,
            );
        }
        Ok(leases)
    })?;
    let mut released = 0;
    for (organization_id, lease_id, idempotency_key) in held {
        let lease = store.lease(&organization_id, &lease_id)?;
        let Some(lease) = lease else {
            continue;
        };
        if owns_invoice_payment_lease(&lease) {
            continue;
        }
        match claim_state(store, &organization_id, &idempotency_key)? {
            Some(OperationClaimState::Succeeded) => {
                release_terminal(store, &organization_id, &lease)?;
                released += 1;
            }
            Some(OperationClaimState::Failed) => {
                release_no_effect(store, &organization_id, &lease)?;
                released += 1;
            }
            Some(_) => {}
            None => {
                release_orphan(store, &organization_id, &lease)?;
                released += 1;
            }
        }
    }
    Ok(released)
}

fn release_orphan(
    store: &SqlitePaymentStore,
    organization_id: &str,
    lease: &FinancialMutationLease,
) -> Result<(), PaymentError> {
    store.tx(|tx| {
        let changed = tx
            .execute(
                "UPDATE financial_mutation_lease SET state = 'released', released_ms = ?3, \
                 version = version + 1 \
                 WHERE organization_id = ?1 AND id = ?2 AND state = 'held'",
                params![organization_id, lease.id, now_ms()],
            )
            .map_err(|error| {
                PaymentError::Store(format!("release orphan financial lease: {error}"))
            })?;
        if changed != 1 {
            return Err(PaymentError::Conflict(format!(
                "financial lease {} changed during orphan recovery",
                lease.id
            )));
        }
        Ok(())
    })
}

/// The durable period identity of a subscription: period start and end as
/// read from the durable row (never a caller-provided period).
pub(crate) fn period_key(subscription: &PaymentSubscription) -> String {
    format!(
        "{}-{}",
        subscription.current_period_start_ms, subscription.current_period_end_ms
    )
}

/// True when a subscription webhook would race an in-flight local mutation.
/// The webhook is refused before its event is recorded, so delivery can be
/// retried and applied in order after the fence is released.
pub(crate) fn webhook_blocked_by_fence(
    store: &SqlitePaymentStore,
    organization_id: &str,
    provider_subscription_id: &str,
) -> Result<bool, PaymentError> {
    if store
        .active_for(
            organization_id,
            &ConflictDomain::Subscription {
                organization_id: organization_id.to_string(),
            },
            organization_id,
        )?
        .is_some()
    {
        return Ok(true);
    }
    let Some(subscription) = store.subscription(organization_id)? else {
        return Ok(false);
    };
    if subscription.provider_subscription_id != provider_subscription_id {
        return Ok(false);
    }
    Ok(store
        .active_for(
            organization_id,
            &ConflictDomain::SubscriptionMutation {
                subscription_id: subscription.id.clone(),
            },
            &subscription.id,
        )?
        .is_some())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Release the fence bound to one claim after reconciliation recorded its
/// terminal outcome. Used by the shared unknown-claim reconciliation loop for
/// subscription/plan/seat/cancel/invoice claims.
pub(crate) fn release_for_claim(
    store: &SqlitePaymentStore,
    organization_id: &str,
    idempotency_key: &str,
) -> Result<(), PaymentError> {
    let Some(lease) = store
        .leases_needing_reconciliation(organization_id)?
        .into_iter()
        .find(|lease| lease.idempotency_key == idempotency_key)
    else {
        return Ok(());
    };
    settle(store, organization_id, &lease, idempotency_key)
}
