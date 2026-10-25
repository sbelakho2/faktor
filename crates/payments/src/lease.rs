//! Durable financial mutation leases.
//!
//! A lease is the resource fence around one externally visible money
//! mutation. Exactly one held lease may exist for
//! `(organization_id, conflict_domain, logical_resource_id)`; the partial
//! unique index enforces that even if two processes race. A lease stays held
//! through `claimed`, `dispatched` and `unknown` claim states and is only
//! released after a terminal provider outcome or a reconciliation that proved
//! no provider effect. Startup recovery advances the fencing generation of
//! every held lease and never silently frees one.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::PaymentError;
use crate::model::{OperationClaim, OperationClaimDraft, OperationClaimState};
use crate::money::valid_currency;
use crate::store::SqlitePaymentStore;

const LEASE_COLUMNS: &str = "id, organization_id, conflict_domain, logical_resource_id, \
     operation_claim_id, idempotency_key, state, acquired_ms, released_ms, \
     reserved_amount_micro, currency, request_digest, version";

/// The conflict scope of one financial mutation. Two mutations that carry
/// the same domain and logical resource may not run concurrently.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictDomain {
    Subscription {
        organization_id: String,
    },
    InvoicePayment {
        invoice_id: String,
    },
    PaymentRefund {
        payment_id: String,
    },
    SubscriptionMutation {
        subscription_id: String,
    },
    InvoiceIssue {
        subscription_id: String,
        period: String,
    },
}

impl ConflictDomain {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictDomain::Subscription { .. } => "subscription",
            ConflictDomain::InvoicePayment { .. } => "invoice_payment",
            ConflictDomain::PaymentRefund { .. } => "payment_refund",
            ConflictDomain::SubscriptionMutation { .. } => "subscription_mutation",
            ConflictDomain::InvoiceIssue { .. } => "invoice_issue",
        }
    }

    /// The canonical resource id fenced by this domain. A lease acquired for
    /// the domain and this id is the only active fence for the key.
    pub fn logical_resource_id(&self) -> String {
        match self {
            ConflictDomain::Subscription { organization_id } => organization_id.clone(),
            ConflictDomain::InvoicePayment { invoice_id } => invoice_id.clone(),
            ConflictDomain::PaymentRefund { payment_id } => payment_id.clone(),
            ConflictDomain::SubscriptionMutation { subscription_id } => subscription_id.clone(),
            ConflictDomain::InvoiceIssue {
                subscription_id,
                period,
            } => format!("{subscription_id}:{period}"),
        }
    }

    fn parse(kind: &str, logical_resource_id: &str) -> Option<Self> {
        match kind {
            "subscription" => Some(ConflictDomain::Subscription {
                organization_id: logical_resource_id.to_string(),
            }),
            "invoice_payment" => Some(ConflictDomain::InvoicePayment {
                invoice_id: logical_resource_id.to_string(),
            }),
            "payment_refund" => Some(ConflictDomain::PaymentRefund {
                payment_id: logical_resource_id.to_string(),
            }),
            "subscription_mutation" => Some(ConflictDomain::SubscriptionMutation {
                subscription_id: logical_resource_id.to_string(),
            }),
            "invoice_issue" => {
                let (subscription_id, period) = logical_resource_id.split_once(':')?;
                Some(ConflictDomain::InvoiceIssue {
                    subscription_id: subscription_id.to_string(),
                    period: period.to_string(),
                })
            }
            _ => None,
        }
    }

    fn validate(&self) -> Result<(), PaymentError> {
        let empty = match self {
            ConflictDomain::Subscription { organization_id } => organization_id.is_empty(),
            ConflictDomain::InvoicePayment { invoice_id } => invoice_id.is_empty(),
            ConflictDomain::PaymentRefund { payment_id } => payment_id.is_empty(),
            ConflictDomain::SubscriptionMutation { subscription_id } => subscription_id.is_empty(),
            ConflictDomain::InvoiceIssue {
                subscription_id,
                period,
            } => subscription_id.is_empty() || period.is_empty(),
        };
        if empty {
            return Err(PaymentError::Malformed(
                "a conflict domain carries an empty resource id".into(),
            ));
        }
        Ok(())
    }
}

/// The durable lease state. Only `held` blocks a new acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Held,
    Terminal,
    Released,
}

impl LeaseState {
    pub const fn as_str(self) -> &'static str {
        match self {
            LeaseState::Held => "held",
            LeaseState::Terminal => "terminal",
            LeaseState::Released => "released",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "held" => LeaseState::Held,
            "terminal" => LeaseState::Terminal,
            "released" => LeaseState::Released,
            _ => return None,
        })
    }
}

/// One durable financial mutation lease row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinancialMutationLease {
    pub id: String,
    pub organization_id: String,
    pub conflict_domain: ConflictDomain,
    pub logical_resource_id: String,
    pub operation_claim_id: String,
    pub idempotency_key: String,
    pub state: LeaseState,
    pub acquired_ms: i64,
    pub released_ms: Option<i64>,
    pub reserved_amount_micro: Option<i64>,
    pub currency: Option<String>,
    pub request_digest: String,
    pub version: i64,
}

impl FinancialMutationLease {
    /// A held lease has not proved a terminal outcome yet. After a restart it
    /// must be reconciled; it is never freed by recovery.
    pub const fn needs_reconciliation(&self) -> bool {
        matches!(self.state, LeaseState::Held)
    }

    pub const fn is_active(&self) -> bool {
        matches!(self.state, LeaseState::Held)
    }
}

/// The operation-claim identity a lease is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseClaim {
    /// The durable operation claim identity. In this store a claim is keyed
    /// by its idempotency key, so both fields address the same claim row.
    pub operation_claim_id: String,
    pub idempotency_key: String,
    pub request_digest: String,
}

impl LeaseClaim {
    pub fn new(
        operation_claim_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        request_digest: impl Into<String>,
    ) -> Self {
        Self {
            operation_claim_id: operation_claim_id.into(),
            idempotency_key: idempotency_key.into(),
            request_digest: request_digest.into(),
        }
    }

    pub fn from_draft(draft: &OperationClaimDraft) -> Self {
        Self {
            operation_claim_id: draft.idempotency_key.clone(),
            idempotency_key: draft.idempotency_key.clone(),
            request_digest: draft.request_digest.clone(),
        }
    }

    pub fn from_claim(claim: &OperationClaim) -> Self {
        Self {
            operation_claim_id: claim.idempotency_key.clone(),
            idempotency_key: claim.idempotency_key.clone(),
            request_digest: claim.request_digest.clone(),
        }
    }

    fn validate(&self) -> Result<(), PaymentError> {
        if self.operation_claim_id.is_empty()
            || self.idempotency_key.is_empty()
            || self.request_digest.is_empty()
        {
            return Err(PaymentError::Malformed(
                "a lease claim needs a non-empty id, idempotency key and request digest".into(),
            ));
        }
        Ok(())
    }
}

/// An optional amount reserved by the mutation that owns the lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseAmount {
    pub reserved_micro: i64,
    pub currency: String,
}

impl LeaseAmount {
    pub fn new(reserved_micro: i64, currency: impl Into<String>) -> Result<Self, PaymentError> {
        let amount = Self {
            reserved_micro,
            currency: currency.into(),
        };
        amount.validate()?;
        Ok(amount)
    }

    fn validate(&self) -> Result<(), PaymentError> {
        if self.reserved_micro <= 0 {
            return Err(PaymentError::Malformed(
                "a reserved lease amount must be positive".into(),
            ));
        }
        if !valid_currency(&self.currency) {
            return Err(PaymentError::CurrencyMismatch(format!(
                "lease amount currency {} is not an ISO-shaped code",
                self.currency
            )));
        }
        Ok(())
    }
}

/// The result of trying to acquire one resource fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAcquisition {
    Held(FinancialMutationLease),
    Busy { holder: FinancialMutationLease },
    AlreadyHeldBySameClaim(FinancialMutationLease),
}

/// The proved terminal outcome used to release a fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    Succeeded,
    Failed,
}

impl TerminalOutcome {
    const fn claim_state(self) -> OperationClaimState {
        match self {
            TerminalOutcome::Succeeded => OperationClaimState::Succeeded,
            TerminalOutcome::Failed => OperationClaimState::Failed,
        }
    }
}

/// The resolution of a lease whose operation claim was `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReconciliation {
    /// A read-only provider reconciliation proved that no effect exists.
    NoEffect,
    /// The provider produced a terminal outcome.
    Terminal(TerminalOutcome),
}

/// Counts from startup recovery of held leases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeaseRecovery {
    pub held: usize,
    pub needs_reconciliation: usize,
}

impl SqlitePaymentStore {
    /// Acquire the one active fence for a domain/resource key. The held row is
    /// committed in one immediate transaction before any provider call.
    pub fn try_acquire(
        &self,
        organization_id: &str,
        domain: &ConflictDomain,
        resource: &str,
        claim: &LeaseClaim,
        amount: Option<LeaseAmount>,
    ) -> Result<LeaseAcquisition, PaymentError> {
        validate_scope(organization_id, domain, resource)?;
        claim.validate()?;
        let (reserved_amount_micro, currency) = match amount {
            Some(amount) => {
                amount.validate()?;
                (Some(amount.reserved_micro), Some(amount.currency))
            }
            None => (None, None),
        };
        let now = lease_now_ms();
        self.tx(|tx| {
            if let Some(existing) =
                read_active_lease_conn(tx, organization_id, domain.as_str(), resource)?
            {
                if existing.operation_claim_id == claim.operation_claim_id
                    && existing.idempotency_key == claim.idempotency_key
                {
                    if existing.request_digest == claim.request_digest {
                        return Ok(LeaseAcquisition::AlreadyHeldBySameClaim(existing));
                    }
                    return Err(PaymentError::IdempotencyConflict(format!(
                        "lease claim {} reused with a different request digest",
                        claim.operation_claim_id
                    )));
                }
                return Ok(LeaseAcquisition::Busy { holder: existing });
            }
            if let Some(other) = read_active_lease_for_identities_conn(
                tx,
                organization_id,
                &claim.operation_claim_id,
                &claim.idempotency_key,
            )? {
                return Err(PaymentError::IdempotencyConflict(format!(
                    "lease claim {} is already bound to {}/{}",
                    claim.operation_claim_id,
                    other.conflict_domain.as_str(),
                    other.logical_resource_id
                )));
            }
            let lease = FinancialMutationLease {
                id: format!("lease_{}", uuid::Uuid::new_v4()),
                organization_id: organization_id.to_string(),
                conflict_domain: domain.clone(),
                logical_resource_id: resource.to_string(),
                operation_claim_id: claim.operation_claim_id.clone(),
                idempotency_key: claim.idempotency_key.clone(),
                state: LeaseState::Held,
                acquired_ms: now,
                released_ms: None,
                reserved_amount_micro,
                currency,
                request_digest: claim.request_digest.clone(),
                version: 1,
            };
            match insert_lease_conn(tx, &lease) {
                Ok(()) => Ok(LeaseAcquisition::Held(lease)),
                Err(PaymentError::Conflict(_)) => {
                    let holder =
                        read_active_lease_conn(tx, organization_id, domain.as_str(), resource)?
                            .ok_or_else(|| {
                                PaymentError::Store(
                                    "financial lease uniqueness lost without a holder".into(),
                                )
                            })?;
                    if holder.operation_claim_id == claim.operation_claim_id
                        && holder.idempotency_key == claim.idempotency_key
                        && holder.request_digest == claim.request_digest
                    {
                        Ok(LeaseAcquisition::AlreadyHeldBySameClaim(holder))
                    } else {
                        Ok(LeaseAcquisition::Busy { holder })
                    }
                }
                Err(error) => Err(error),
            }
        })
    }

    /// Advance the fencing generation of a held lease. An `unknown` claim is
    /// not renewable: it must be reconciled first.
    pub fn renew(
        &self,
        organization_id: &str,
        lease_id: &str,
    ) -> Result<FinancialMutationLease, PaymentError> {
        self.tx(|tx| {
            let lease = require_held_lease(tx, organization_id, lease_id)?;
            let (claim_state, request_digest) =
                claim_identity_conn(tx, organization_id, &lease.idempotency_key)?.ok_or_else(
                    || {
                        PaymentError::Conflict(format!(
                            "lease {lease_id} has no operation claim; it needs reconciliation"
                        ))
                    },
                )?;
            if request_digest != lease.request_digest {
                return Err(PaymentError::Conflict(format!(
                    "lease {lease_id} and its operation claim disagree on the request digest"
                )));
            }
            if !claim_state.is_in_flight() {
                return Err(PaymentError::Conflict(format!(
                    "lease {lease_id} is {} and needs reconciliation",
                    claim_state.as_str()
                )));
            }
            tx.execute(
                "UPDATE financial_mutation_lease SET version = version + 1 \
                 WHERE organization_id = ?1 AND id = ?2 AND state = 'held'",
                params![organization_id, lease_id],
            )
            .map_err(store_err)?;
            read_lease_by_id_conn(tx, organization_id, lease_id)?
                .ok_or_else(|| PaymentError::Store("financial lease vanished after renewal".into()))
        })
    }

    /// Release a fence after the operation claim recorded a terminal
    /// provider outcome.
    pub fn release_terminal(
        &self,
        organization_id: &str,
        lease_id: &str,
        outcome: TerminalOutcome,
    ) -> Result<FinancialMutationLease, PaymentError> {
        self.tx(|tx| {
            let lease = require_held_lease(tx, organization_id, lease_id)?;
            let (claim_state, request_digest) = claim_identity_conn(
                tx,
                organization_id,
                &lease.idempotency_key,
            )?
            .ok_or_else(|| {
                PaymentError::Conflict(format!("lease {lease_id} has no terminal operation claim"))
            })?;
            if request_digest != lease.request_digest {
                return Err(PaymentError::Conflict(format!(
                    "lease {lease_id} and its operation claim disagree on the request digest"
                )));
            }
            let expected = outcome.claim_state();
            if claim_state != expected {
                return Err(PaymentError::Conflict(format!(
                    "lease {lease_id} claim is {} but the terminal outcome is {}",
                    claim_state.as_str(),
                    expected.as_str()
                )));
            }
            finish_held_lease_tx(tx, &lease, LeaseState::Terminal, lease_now_ms())
        })
    }

    /// Release a fence after a read-only reconciliation proved the operation
    /// created no provider effect. Only a `failed` claim (definitive dispatch
    /// refusal or provider reconciliation `not_applied`) qualifies.
    pub fn release_no_effect(
        &self,
        organization_id: &str,
        lease_id: &str,
    ) -> Result<FinancialMutationLease, PaymentError> {
        self.tx(|tx| {
            let lease = require_held_lease(tx, organization_id, lease_id)?;
            let (claim_state, request_digest) =
                claim_identity_conn(tx, organization_id, &lease.idempotency_key)?.ok_or_else(
                    || {
                        PaymentError::Conflict(format!(
                            "lease {lease_id} has no reconciled operation claim"
                        ))
                    },
                )?;
            if request_digest != lease.request_digest {
                return Err(PaymentError::Conflict(format!(
                    "lease {lease_id} and its operation claim disagree on the request digest"
                )));
            }
            if claim_state != OperationClaimState::Failed {
                return Err(PaymentError::Conflict(format!(
                    "lease {lease_id} claim is {}; a no-effect release requires a reconciled \
                     failed claim",
                    claim_state.as_str()
                )));
            }
            finish_held_lease_tx(tx, &lease, LeaseState::Released, lease_now_ms())
        })
    }

    /// Resolve a lease whose claim was `unknown`. The caller first records the
    /// provider reconciliation on the operation claim; this method refuses to
    /// release while the claim is still in flight or unknown.
    pub fn reconcile_unknown(
        &self,
        organization_id: &str,
        lease_id: &str,
        resolution: UnknownReconciliation,
    ) -> Result<FinancialMutationLease, PaymentError> {
        self.tx(|tx| {
            let lease = require_held_lease(tx, organization_id, lease_id)?;
            let (claim_state, request_digest) =
                claim_identity_conn(tx, organization_id, &lease.idempotency_key)?.ok_or_else(
                    || {
                        PaymentError::Conflict(format!(
                            "lease {lease_id} has no operation claim to reconcile"
                        ))
                    },
                )?;
            if request_digest != lease.request_digest {
                return Err(PaymentError::Conflict(format!(
                    "lease {lease_id} and its operation claim disagree on the request digest"
                )));
            }
            if !claim_state.is_terminal() {
                return Err(PaymentError::Conflict(format!(
                    "lease {lease_id} claim is {}; provider reconciliation has not recorded a \
                     terminal outcome",
                    claim_state.as_str()
                )));
            }
            match resolution {
                UnknownReconciliation::NoEffect => {
                    if claim_state != OperationClaimState::Failed {
                        return Err(PaymentError::Conflict(format!(
                            "lease {lease_id} claim is {}; no-effect reconciliation requires a \
                             failed claim",
                            claim_state.as_str()
                        )));
                    }
                    finish_held_lease_tx(tx, &lease, LeaseState::Released, lease_now_ms())
                }
                UnknownReconciliation::Terminal(outcome) => {
                    let expected = outcome.claim_state();
                    if claim_state != expected {
                        return Err(PaymentError::Conflict(format!(
                            "lease {lease_id} claim is {} but the reconciled outcome is {}",
                            claim_state.as_str(),
                            expected.as_str()
                        )));
                    }
                    finish_held_lease_tx(tx, &lease, LeaseState::Terminal, lease_now_ms())
                }
            }
        })
    }

    /// The one held lease for a domain/resource key, if any.
    pub fn active_for(
        &self,
        organization_id: &str,
        domain: &ConflictDomain,
        resource: &str,
    ) -> Result<Option<FinancialMutationLease>, PaymentError> {
        validate_scope(organization_id, domain, resource)?;
        let conn = self.lock()?;
        read_active_lease_conn(&conn, organization_id, domain.as_str(), resource)
    }

    pub fn lease(
        &self,
        organization_id: &str,
        lease_id: &str,
    ) -> Result<Option<FinancialMutationLease>, PaymentError> {
        let conn = self.lock()?;
        read_lease_by_id_conn(&conn, organization_id, lease_id)
    }

    /// The one held lease bound to an operation claim key. This is an
    /// indexed single-row lookup used on the reconciliation hot path; it must
    /// never scan every held lease per claim.
    pub fn held_lease_for_claim(
        &self,
        organization_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<FinancialMutationLease>, PaymentError> {
        let conn = self.lock()?;
        let raw = conn
            .query_row(
                &format!(
                    "SELECT {LEASE_COLUMNS} FROM financial_mutation_lease \
                     WHERE organization_id = ?1 AND idempotency_key = ?2 AND state = 'held' \
                     ORDER BY acquired_ms, id LIMIT 1"
                ),
                params![organization_id, idempotency_key],
                decode_raw_lease,
            )
            .optional()
            .map_err(store_err)?;
        raw.map(RawLease::into_lease).transpose()
    }

    /// Every held lease for an organization. Held means the terminal outcome
    /// is not proved yet, so each row is a reconciliation obligation.
    pub fn leases_needing_reconciliation(
        &self,
        organization_id: &str,
    ) -> Result<Vec<FinancialMutationLease>, PaymentError> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(&format!(
                "SELECT {LEASE_COLUMNS} FROM financial_mutation_lease \
                 WHERE organization_id = ?1 AND state = 'held' \
                 ORDER BY acquired_ms, id"
            ))
            .map_err(store_err)?;
        let rows = statement
            .query_map(params![organization_id], decode_raw_lease)
            .map_err(store_err)?;
        let mut leases = Vec::new();
        for row in rows {
            leases.push(row.map_err(store_err)?.into_lease()?);
        }
        Ok(leases)
    }

    /// Startup recovery: advance the fencing generation of every held lease
    /// and report them as reconciliation obligations. Nothing is released
    /// and no lease row is deleted.
    pub fn recover_leases(&self) -> Result<LeaseRecovery, PaymentError> {
        self.tx(|tx| {
            let held: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM financial_mutation_lease WHERE state = 'held'",
                    [],
                    |row| row.get(0),
                )
                .map_err(store_err)?;
            tx.execute(
                "UPDATE financial_mutation_lease SET version = version + 1 \
                 WHERE state = 'held'",
                [],
            )
            .map_err(store_err)?;
            let held = usize::try_from(held).unwrap_or(usize::MAX);
            Ok(LeaseRecovery {
                held,
                needs_reconciliation: held,
            })
        })
    }
}

fn validate_scope(
    organization_id: &str,
    domain: &ConflictDomain,
    resource: &str,
) -> Result<(), PaymentError> {
    domain.validate()?;
    if organization_id.is_empty() || resource.is_empty() {
        return Err(PaymentError::Malformed(
            "a financial lease needs a non-empty organization and resource".into(),
        ));
    }
    if let ConflictDomain::Subscription {
        organization_id: domain_organization,
    } = domain
    {
        if domain_organization != organization_id {
            return Err(PaymentError::Malformed(
                "the subscription conflict domain belongs to a different organization".into(),
            ));
        }
    }
    if domain.logical_resource_id() != resource {
        return Err(PaymentError::Malformed(format!(
            "lease resource {resource} does not match the {} domain",
            domain.as_str()
        )));
    }
    Ok(())
}

fn require_held_lease(
    conn: &Connection,
    organization_id: &str,
    lease_id: &str,
) -> Result<FinancialMutationLease, PaymentError> {
    let lease = read_lease_by_id_conn(conn, organization_id, lease_id)?
        .ok_or_else(|| PaymentError::NotFound("financial mutation lease".into()))?;
    if !lease.is_active() {
        return Err(PaymentError::IllegalState(format!(
            "financial mutation lease {lease_id} is {}",
            lease.state.as_str()
        )));
    }
    Ok(lease)
}

fn finish_held_lease_tx(
    tx: &Connection,
    lease: &FinancialMutationLease,
    state: LeaseState,
    now_ms: i64,
) -> Result<FinancialMutationLease, PaymentError> {
    let changed = tx
        .execute(
            "UPDATE financial_mutation_lease SET state = ?3, released_ms = ?4, \
             version = version + 1 \
             WHERE organization_id = ?1 AND id = ?2 AND state = 'held'",
            params![lease.organization_id, lease.id, state.as_str(), now_ms],
        )
        .map_err(store_err)?;
    if changed != 1 {
        return Err(PaymentError::Conflict(format!(
            "financial mutation lease {} was released concurrently",
            lease.id
        )));
    }
    read_lease_by_id_conn(tx, &lease.organization_id, &lease.id)?.ok_or_else(|| {
        PaymentError::Store("financial mutation lease vanished after release".into())
    })
}

fn claim_identity_conn(
    conn: &Connection,
    organization_id: &str,
    idempotency_key: &str,
) -> Result<Option<(OperationClaimState, String)>, PaymentError> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT state, request_digest FROM payment_operation_claim \
             WHERE organization_id = ?1 AND idempotency_key = ?2",
            params![organization_id, idempotency_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(store_err)?;
    row.map(|(state, digest)| {
        OperationClaimState::parse(&state)
            .map(|state| (state, digest))
            .ok_or_else(|| {
                PaymentError::Store(format!(
                    "operation claim {idempotency_key} has state {state}"
                ))
            })
    })
    .transpose()
}

fn read_lease_by_id_conn(
    conn: &Connection,
    organization_id: &str,
    lease_id: &str,
) -> Result<Option<FinancialMutationLease>, PaymentError> {
    let raw = conn
        .query_row(
            &format!(
                "SELECT {LEASE_COLUMNS} FROM financial_mutation_lease \
                 WHERE organization_id = ?1 AND id = ?2"
            ),
            params![organization_id, lease_id],
            decode_raw_lease,
        )
        .optional()
        .map_err(store_err)?;
    raw.map(RawLease::into_lease).transpose()
}

/// Read the active fence for one domain inside an already-open transaction.
/// State-only writers (lifecycle ticks, dunning) use this to prove the
/// resource is not owned by an in-flight provider mutation.
pub(crate) fn active_lease_conn(
    conn: &Connection,
    organization_id: &str,
    domain: &ConflictDomain,
) -> Result<Option<FinancialMutationLease>, PaymentError> {
    if domain.logical_resource_id().is_empty() {
        return Err(PaymentError::Malformed(
            "a conflict domain carries an empty resource id".into(),
        ));
    }
    read_active_lease_conn(
        conn,
        organization_id,
        domain.as_str(),
        &domain.logical_resource_id(),
    )
}

fn read_active_lease_conn(
    conn: &Connection,
    organization_id: &str,
    conflict_domain: &str,
    logical_resource_id: &str,
) -> Result<Option<FinancialMutationLease>, PaymentError> {
    let raw = conn
        .query_row(
            &format!(
                "SELECT {LEASE_COLUMNS} FROM financial_mutation_lease \
                 WHERE organization_id = ?1 AND conflict_domain = ?2 \
                 AND logical_resource_id = ?3 AND state = 'held' \
                 ORDER BY acquired_ms DESC, id DESC LIMIT 1"
            ),
            params![organization_id, conflict_domain, logical_resource_id],
            decode_raw_lease,
        )
        .optional()
        .map_err(store_err)?;
    raw.map(RawLease::into_lease).transpose()
}

fn read_active_lease_for_identities_conn(
    conn: &Connection,
    organization_id: &str,
    operation_claim_id: &str,
    idempotency_key: &str,
) -> Result<Option<FinancialMutationLease>, PaymentError> {
    let raw = conn
        .query_row(
            &format!(
                "SELECT {LEASE_COLUMNS} FROM financial_mutation_lease \
                 WHERE organization_id = ?1 AND state = 'held' \
                 AND (operation_claim_id = ?2 OR idempotency_key = ?3) \
                 ORDER BY acquired_ms, id LIMIT 1"
            ),
            params![organization_id, operation_claim_id, idempotency_key],
            decode_raw_lease,
        )
        .optional()
        .map_err(store_err)?;
    raw.map(RawLease::into_lease).transpose()
}

fn insert_lease_conn(
    conn: &Connection,
    lease: &FinancialMutationLease,
) -> Result<(), PaymentError> {
    conn.execute(
        "INSERT INTO financial_mutation_lease \
         (id, organization_id, conflict_domain, logical_resource_id, operation_claim_id, \
          idempotency_key, state, acquired_ms, released_ms, reserved_amount_micro, currency, \
          request_digest, version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            lease.id,
            lease.organization_id,
            lease.conflict_domain.as_str(),
            lease.logical_resource_id,
            lease.operation_claim_id,
            lease.idempotency_key,
            lease.state.as_str(),
            lease.acquired_ms,
            lease.released_ms,
            lease.reserved_amount_micro,
            lease.currency,
            lease.request_digest,
            lease.version,
        ],
    )
    .map_err(|error| {
        if is_constraint_violation(&error) {
            PaymentError::Conflict("financial lease is already held".into())
        } else {
            store_err(error)
        }
    })?;
    Ok(())
}

fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

struct RawLease {
    id: String,
    organization_id: String,
    conflict_domain: String,
    logical_resource_id: String,
    operation_claim_id: String,
    idempotency_key: String,
    state: String,
    acquired_ms: i64,
    released_ms: Option<i64>,
    reserved_amount_micro: Option<i64>,
    currency: Option<String>,
    request_digest: String,
    version: i64,
}

impl RawLease {
    fn into_lease(self) -> Result<FinancialMutationLease, PaymentError> {
        let conflict_domain =
            ConflictDomain::parse(&self.conflict_domain, &self.logical_resource_id).ok_or_else(
                || {
                    PaymentError::Store(format!(
                        "financial lease {} has unknown domain {}",
                        self.id, self.conflict_domain
                    ))
                },
            )?;
        let state = LeaseState::parse(&self.state).ok_or_else(|| {
            PaymentError::Store(format!(
                "financial lease {} has unknown state {}",
                self.id, self.state
            ))
        })?;
        Ok(FinancialMutationLease {
            id: self.id,
            organization_id: self.organization_id,
            conflict_domain,
            logical_resource_id: self.logical_resource_id,
            operation_claim_id: self.operation_claim_id,
            idempotency_key: self.idempotency_key,
            state,
            acquired_ms: self.acquired_ms,
            released_ms: self.released_ms,
            reserved_amount_micro: self.reserved_amount_micro,
            currency: self.currency,
            request_digest: self.request_digest,
            version: self.version,
        })
    }
}

fn decode_raw_lease(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawLease> {
    Ok(RawLease {
        id: row.get(0)?,
        organization_id: row.get(1)?,
        conflict_domain: row.get(2)?,
        logical_resource_id: row.get(3)?,
        operation_claim_id: row.get(4)?,
        idempotency_key: row.get(5)?,
        state: row.get(6)?,
        acquired_ms: row.get(7)?,
        released_ms: row.get(8)?,
        reserved_amount_micro: row.get(9)?,
        currency: row.get(10)?,
        request_digest: row.get(11)?,
        version: row.get(12)?,
    })
}

pub(crate) fn backfill_financial_leases(conn: &Connection) -> Result<(), PaymentError> {
    let claim_table: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name = 'payment_operation_claim'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_err)?;
    if claim_table.is_none() {
        return Ok(());
    }
    let claims = {
        let mut statement = conn
            .prepare(
                "SELECT organization_id, idempotency_key, operation, resource_id, \
                 request_digest, context, created_ms FROM payment_operation_claim \
                 WHERE state IN ('claimed','dispatched','unknown') \
                 ORDER BY created_ms, idempotency_key",
            )
            .map_err(store_err)?;
        let rows = statement
            .query_map([], |row| {
                Ok(BackfillClaim {
                    organization_id: row.get(0)?,
                    idempotency_key: row.get(1)?,
                    operation: row.get(2)?,
                    resource_id: row.get(3)?,
                    request_digest: row.get(4)?,
                    context: row.get(5)?,
                    created_ms: row.get(6)?,
                })
            })
            .map_err(store_err)?;
        let mut claims = Vec::new();
        for row in rows {
            claims.push(row.map_err(store_err)?);
        }
        claims
    };

    for claim in claims {
        let Some((domain, resource, currency)) = backfill_domain(&claim)? else {
            continue;
        };
        let lease = FinancialMutationLease {
            id: format!("lease_{}", uuid::Uuid::new_v4()),
            organization_id: claim.organization_id.clone(),
            conflict_domain: domain,
            logical_resource_id: resource,
            operation_claim_id: claim.idempotency_key.clone(),
            idempotency_key: claim.idempotency_key.clone(),
            state: LeaseState::Held,
            acquired_ms: claim.created_ms,
            released_ms: None,
            reserved_amount_micro: None,
            currency,
            request_digest: claim.request_digest.clone(),
            version: 1,
        };
        conn.execute(
            "INSERT INTO financial_mutation_lease \
             (id, organization_id, conflict_domain, logical_resource_id, operation_claim_id, \
              idempotency_key, state, acquired_ms, released_ms, reserved_amount_micro, currency, \
              request_digest, version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
             ON CONFLICT DO NOTHING",
            params![
                lease.id,
                lease.organization_id,
                lease.conflict_domain.as_str(),
                lease.logical_resource_id,
                lease.operation_claim_id,
                lease.idempotency_key,
                lease.state.as_str(),
                lease.acquired_ms,
                lease.released_ms,
                lease.reserved_amount_micro,
                lease.currency,
                lease.request_digest,
                lease.version,
            ],
        )
        .map_err(store_err)?;
    }
    Ok(())
}

struct BackfillClaim {
    organization_id: String,
    idempotency_key: String,
    operation: String,
    resource_id: String,
    request_digest: String,
    context: String,
    created_ms: i64,
}

fn backfill_domain(
    claim: &BackfillClaim,
) -> Result<Option<(ConflictDomain, String, Option<String>)>, PaymentError> {
    let (domain, currency) = match claim.operation.as_str() {
        "subscribe" => (
            ConflictDomain::Subscription {
                organization_id: claim.organization_id.clone(),
            },
            None,
        ),
        "change_plan" | "cancel" | "change_seats" => (
            ConflictDomain::SubscriptionMutation {
                subscription_id: claim.resource_id.clone(),
            },
            None,
        ),
        "issue_invoice" => {
            let context = parse_claim_context(claim)?;
            let subscription_id = context
                .pointer("/request/subscription_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    PaymentError::Store(format!(
                        "in-flight invoice claim {} has no subscription id",
                        claim.idempotency_key
                    ))
                })?;
            let period = match (
                context
                    .pointer("/invoice/period_start_ms")
                    .and_then(serde_json::Value::as_i64),
                context
                    .pointer("/invoice/period_end_ms")
                    .and_then(serde_json::Value::as_i64),
            ) {
                (Some(start), Some(end)) => format!("{start}-{end}"),
                _ => claim.resource_id.clone(),
            };
            let currency = context
                .pointer("/invoice/currency")
                .and_then(serde_json::Value::as_str)
                .filter(|currency| valid_currency(currency))
                .map(str::to_string);
            (
                ConflictDomain::InvoiceIssue {
                    subscription_id: subscription_id.to_string(),
                    period,
                },
                currency,
            )
        }
        "pay_invoice" => {
            let context = parse_claim_context(claim)?;
            let invoice_id = context
                .pointer("/request/invoice_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    PaymentError::Store(format!(
                        "in-flight payment claim {} has no invoice id",
                        claim.idempotency_key
                    ))
                })?;
            let currency = context
                .get("currency")
                .and_then(serde_json::Value::as_str)
                .filter(|currency| valid_currency(currency))
                .map(str::to_string);
            (
                ConflictDomain::InvoicePayment {
                    invoice_id: invoice_id.to_string(),
                },
                currency,
            )
        }
        "refund" => {
            let context = parse_claim_context(claim)?;
            let payment_id = context
                .pointer("/request/payment_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    PaymentError::Store(format!(
                        "in-flight refund claim {} has no payment id",
                        claim.idempotency_key
                    ))
                })?;
            (
                ConflictDomain::PaymentRefund {
                    payment_id: payment_id.to_string(),
                },
                None,
            )
        }
        _ => return Ok(None),
    };
    let resource = domain.logical_resource_id();
    Ok(Some((domain, resource, currency)))
}

fn parse_claim_context(claim: &BackfillClaim) -> Result<serde_json::Value, PaymentError> {
    serde_json::from_str(&claim.context).map_err(|error| {
        PaymentError::Store(format!(
            "in-flight claim {} context is malformed: {error}",
            claim.idempotency_key
        ))
    })
}

fn store_err(error: rusqlite::Error) -> PaymentError {
    PaymentError::Store(format!("payment lease store: {error}"))
}

fn lease_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
