//! Invoice payment and refund mutation fencing.
//!
//! A payment is fenced on its invoice id and a refund on its payment id. The
//! fence is acquired before the balance/refundable amount is read and before
//! any provider call, so retries with different idempotency keys cannot mint a
//! second provider effect. Refund operations also reserve their requested
//! amount in the lease; the reservation stays held while the provider outcome
//! is pending and is only released after a terminal or proved no-effect
//! outcome.

use std::time::Duration;

use faktor_cloud::OrganizationId;

use crate::authority::{ProviderPaymentObservation, ProviderRefundObservation};
use crate::error::PaymentError;
use crate::lease::{
    ConflictDomain, FinancialMutationLease, LeaseAcquisition, LeaseAmount, LeaseClaim,
    TerminalOutcome,
};
use crate::model::{
    ClaimAcquisition, FinancialResourceType, OperationClaim, OperationClaimDraft,
    OperationClaimState, PayInvoiceOutcome, PayInvoiceRequest, Payment, PaymentStatus, Refund,
    RefundOutcome, RefundRequest, RefundStatus,
};
use crate::money::checked_add;
use crate::service::{
    bind_refund_invoice_line, operation_resource_id, payment_status_from_provider, request_hash,
    settle_refund_ledger, PaymentService,
};
use crate::store::{
    insert_credit_note_tx, insert_payment_tx, insert_refund_tx, invoice_tx, next_sequence_tx,
    payment_by_provider_id_tx, payment_tx, refund_by_provider_id_conn, subscription_by_id_tx,
    update_invoice_tx, upsert_provider_payment_conn, upsert_provider_refund_conn, WebhookEffects,
};

const REFUND_FENCE_WAIT: Duration = Duration::from_secs(2);
const REFUND_FENCE_POLL: Duration = Duration::from_millis(5);
const MICRO_PER_MINOR: i64 = 1_000_000;

impl PaymentService {
    pub(crate) async fn pay_invoice_fenced(
        &self,
        organization: &OrganizationId,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) = self.replay("pay_invoice", org, &request.idempotency_key, &hash)? {
            return self.pay_from_replay(org, &response, true);
        }
        let payment_id = operation_resource_id("pay", org, "pay_invoice", &request.idempotency_key);
        let provider_key = format!("{}:charge", request.idempotency_key);
        let domain = ConflictDomain::InvoicePayment {
            invoice_id: request.invoice_id.clone(),
        };
        let resource = domain.logical_resource_id();
        let lease_claim =
            LeaseClaim::new(&request.idempotency_key, &request.idempotency_key, &hash);
        let lease = match self
            .store
            .try_acquire(org, &domain, &resource, &lease_claim, None)?
        {
            LeaseAcquisition::Held(lease) => lease,
            LeaseAcquisition::AlreadyHeldBySameClaim(lease) => {
                if let Some(claim) = self.store.operation_claim(org, &request.idempotency_key)? {
                    return self.resume_existing_pay_claim(org, claim, &lease);
                }
                lease
            }
            LeaseAcquisition::Busy { holder } => {
                return Err(PaymentError::Conflict(format!(
                    "invoice {} is fenced by operation {}; exactly one charge may be in flight",
                    request.invoice_id, holder.idempotency_key
                )))
            }
        };
        let invoice = match self.store.invoice(org, &request.invoice_id)? {
            Some(invoice) => invoice,
            None => {
                let error = PaymentError::NotFound("invoice".into());
                self.abort_leased_operation(
                    org,
                    "pay_invoice",
                    FinancialResourceType::Payment,
                    &payment_id,
                    &provider_key,
                    &request.idempotency_key,
                    &hash,
                    serde_json::json!({"request": &request, "payment_id": &payment_id}),
                    &lease,
                    &error,
                )?;
                return Err(error);
            }
        };
        if !invoice.status.is_open() {
            let error = PaymentError::IllegalState(invoice.status.as_str().into());
            self.abort_leased_operation(
                org,
                "pay_invoice",
                FinancialResourceType::Payment,
                &payment_id,
                &provider_key,
                &request.idempotency_key,
                &hash,
                serde_json::json!({"request": &request, "payment_id": &payment_id}),
                &lease,
                &error,
            )?;
            return Err(error);
        }
        let balance = invoice.balance_minor();
        if balance <= 0 {
            let error = PaymentError::NothingDue;
            self.abort_leased_operation(
                org,
                "pay_invoice",
                FinancialResourceType::Payment,
                &payment_id,
                &provider_key,
                &request.idempotency_key,
                &hash,
                serde_json::json!({"request": &request, "payment_id": &payment_id}),
                &lease,
                &error,
            )?;
            return Err(error);
        }
        let method = match self.store.payment_method(org, &request.payment_method_id)? {
            Some(method) if method.method.organization_id == org => method,
            _ => {
                let error = PaymentError::NotFound("payment method".into());
                self.abort_leased_operation(
                    org,
                    "pay_invoice",
                    FinancialResourceType::Payment,
                    &payment_id,
                    &provider_key,
                    &request.idempotency_key,
                    &hash,
                    serde_json::json!({"request": &request, "payment_id": &payment_id}),
                    &lease,
                    &error,
                )?;
                return Err(error);
            }
        };
        if method.method.provider != self.provider.name() {
            let error = PaymentError::Malformed(
                "the payment method belongs to a different payment provider".into(),
            );
            self.abort_leased_operation(
                org,
                "pay_invoice",
                FinancialResourceType::Payment,
                &payment_id,
                &provider_key,
                &request.idempotency_key,
                &hash,
                serde_json::json!({"request": &request, "payment_id": &payment_id}),
                &lease,
                &error,
            )?;
            return Err(error);
        }
        let token = match self.payment_method_token(org, &method.method.id) {
            Ok(token) => token,
            Err(error) => {
                self.abort_leased_operation(
                    org,
                    "pay_invoice",
                    FinancialResourceType::Payment,
                    &payment_id,
                    &provider_key,
                    &request.idempotency_key,
                    &hash,
                    serde_json::json!({"request": &request, "payment_id": &payment_id}),
                    &lease,
                    &error,
                )?;
                return Err(error);
            }
        };
        let claim_context = serde_json::json!({
            "request": &request,
            "payment_id": &payment_id,
            "amount_minor": balance,
            "payment_method_id": &request.payment_method_id,
            "currency": &invoice.currency,
        });
        match self.acquire_claim(
            org,
            &request.idempotency_key,
            "pay_invoice",
            FinancialResourceType::Payment,
            &payment_id,
            &hash,
            &provider_key,
            &claim_context,
        )? {
            ClaimAcquisition::Acquired => {}
            ClaimAcquisition::Recorded(claim) => {
                let (response, _) = self.claim_replay(&claim, true)?;
                self.release_lease_for_claim(org, &lease, &claim);
                return self.pay_from_replay(org, &response, true);
            }
            ClaimAcquisition::InFlight(claim) => return Err(Self::claim_in_flight_error(&claim)),
            ClaimAcquisition::Unresolved(claim) => {
                return Err(Self::claim_unresolved_error(&claim))
            }
        }
        self.store
            .mark_claim_dispatched(org, &request.idempotency_key, self.now_ms())?;
        let charge = match self
            .provider
            .charge(crate::provider::ProviderChargeRequest {
                organization_id: org.to_string(),
                idempotency_key: provider_key.clone(),
                customer_id: self
                    .store
                    .customer(org)?
                    .ok_or_else(|| PaymentError::NotFound("payment customer".into()))?
                    .provider_customer_id,
                payment_method_token: token,
                amount_minor: balance,
                currency: invoice.currency.clone(),
                invoice_ref: Some(invoice.id.clone()),
            })
            .await
        {
            Ok(charge) => charge,
            Err(PaymentError::Declined(reason)) => {
                self.store.fail_claim(
                    org,
                    &request.idempotency_key,
                    "payment_declined",
                    &reason,
                    self.now_ms(),
                )?;
                let _ = self.store.release_no_effect(org, &lease.id);
                let _ = self.record_payment_failure(organization, &invoice.id);
                return Err(PaymentError::Declined(reason));
            }
            Err(error) => {
                self.fail_dispatch(org, &request.idempotency_key, &error)?;
                self.release_lease_if_claim_failed(org, &lease);
                return Err(error);
            }
        };
        let now = self.now_ms();
        let status = match payment_status_from_provider(&charge.status) {
            Ok(status) => status,
            Err(error) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, now)?;
                return Err(error);
            }
        };
        if status != PaymentStatus::Failed
            && (charge.amount_minor != balance || charge.currency != invoice.currency)
        {
            self.store
                .mark_claim_unknown(org, &request.idempotency_key, now)?;
            return Err(PaymentError::Provider(format!(
                "provider charge {} {} does not match the requested {} {}",
                charge.amount_minor, charge.currency, balance, invoice.currency
            )));
        }
        let payment = Payment {
            id: payment_id.clone(),
            organization_id: org.to_string(),
            invoice_id: invoice.id.clone(),
            provider: self.provider.name().to_string(),
            provider_payment_id: charge.id.clone(),
            payment_method_id: Some(request.payment_method_id.clone()),
            amount_minor: balance,
            refunded_minor: 0,
            currency: invoice.currency.clone(),
            status,
            created_ms: now,
        };
        let provider_payment = ProviderPaymentObservation {
            provider: self.provider.name().to_string(),
            provider_payment_id: charge.id.clone(),
            organization_id: org.to_string(),
            invoice_id: invoice.id.clone(),
            payment_id: Some(payment.id.clone()),
            status: charge.status.clone(),
            amount_minor: charge.amount_minor,
            refunded_minor: 0,
            currency: charge.currency.clone(),
            paid_at_ms: status.is_settled().then_some(now),
            observed_ms: now,
            source: crate::authority::ProviderObservationSource::Payment,
        };
        let write = self.write(
            org,
            &request.idempotency_key,
            "pay_invoice",
            hash.clone(),
            &serde_json::json!({
                "payment_id": payment.id,
                "invoice_id": invoice.id,
            }),
            now,
        )?;
        let invoice_id = invoice.id.clone();
        let charge_id = charge.id.clone();
        let charge_status = charge.status.clone();
        let charge_amount = charge.amount_minor;
        let charge_currency = charge.currency.clone();
        let payment_template = payment.clone();
        let provider_payment_template = provider_payment.clone();
        let result = self.store.record_atomic(&write, |tx| {
            let mut current_invoice = invoice_tx(tx, org, &invoice_id)?
                .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
            let mut subscription = current_invoice
                .subscription_id
                .as_deref()
                .and_then(|id| subscription_by_id_tx(tx, org, id).ok().flatten());
            if let Some(existing) = payment_by_provider_id_tx(tx, org, &charge_id)? {
                return Ok((existing, current_invoice, subscription));
            }
            let mut payment = payment_template.clone();
            if status == PaymentStatus::Succeeded {
                let credited = charge_amount.clamp(0, current_invoice.balance_minor());
                current_invoice.amount_paid_minor =
                    checked_add(current_invoice.amount_paid_minor, credited)?;
                if current_invoice.amount_paid_minor >= current_invoice.total_minor
                    && current_invoice
                        .status
                        .can_transition_to(crate::model::InvoiceStatus::Paid)
                {
                    current_invoice.status = crate::model::InvoiceStatus::Paid;
                    current_invoice.paid_ms = Some(now);
                }
                if let Some(sub) = subscription.as_mut() {
                    if sub
                        .state
                        .can_transition_to(crate::model::SubscriptionState::Active)
                    {
                        sub.state = crate::model::SubscriptionState::Active;
                    }
                    sub.dunning_attempts = 0;
                    sub.grace_end_ms = None;
                    sub.updated_ms = now;
                }
            }
            payment.status = status;
            let mut observation = provider_payment_template.clone();
            observation.status = charge_status;
            observation.amount_minor = charge_amount;
            observation.currency = charge_currency;
            insert_payment_tx(tx, &payment)?;
            update_invoice_tx(tx, &current_invoice)?;
            upsert_provider_payment_conn(tx, &observation)?;
            if let Some(subscription) = &subscription {
                crate::store::put_subscription_tx(tx, subscription)?;
            }
            Ok((payment, current_invoice, subscription))
        });
        match result {
            Ok((payment, invoice, subscription)) => {
                if matches!(status, PaymentStatus::Succeeded | PaymentStatus::Failed) {
                    self.release_lease_terminal(org, &lease);
                }
                Ok(PayInvoiceOutcome {
                    payment,
                    invoice,
                    subscription,
                    replayed: false,
                })
            }
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("pay_invoice", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("payment was recorded concurrently".into())
                    })?;
                self.release_lease_if_claim_terminal(org, &lease);
                self.pay_from_replay(org, &response, true)
            }
            Err(e) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, self.now_ms())?;
                Err(e)
            }
        }
    }

    pub(crate) async fn refund_fenced(
        &self,
        organization: &OrganizationId,
        request: RefundRequest,
    ) -> Result<RefundOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        if request.amount_minor <= 0 {
            return Err(PaymentError::Malformed(
                "refund amount must be positive".into(),
            ));
        }
        crate::service::bounded_text(
            "refund reason",
            &request.reason,
            crate::service::MAX_TEXT_BYTES,
        )?;
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) = self.replay("refund", org, &request.idempotency_key, &hash)? {
            return self.refund_from_replay(org, &response, true);
        }
        let payment = self
            .store
            .payment(org, &request.payment_id)?
            .ok_or_else(|| PaymentError::NotFound("payment".into()))?;
        let refund_id = operation_resource_id("re", org, "refund", &request.idempotency_key);
        let provider_key = format!("{}:refund", request.idempotency_key);
        let domain = ConflictDomain::PaymentRefund {
            payment_id: payment.id.clone(),
        };
        let resource = domain.logical_resource_id();
        let lease_claim =
            LeaseClaim::new(&request.idempotency_key, &request.idempotency_key, &hash);
        let reserved_micro = request
            .amount_minor
            .checked_mul(MICRO_PER_MINOR)
            .ok_or_else(|| PaymentError::Malformed("refund amount is too large".into()))?;
        let amount = LeaseAmount::new(reserved_micro, payment.currency.clone())?;
        let lease = self
            .acquire_refund_lease(org, &domain, &resource, &lease_claim, amount, &payment.id)
            .await?;
        let current_payment = match self.store.payment(org, &payment.id)? {
            Some(payment) => payment,
            None => {
                let error = PaymentError::NotFound("payment".into());
                self.abort_leased_operation(
                    org,
                    "refund",
                    FinancialResourceType::Refund,
                    &refund_id,
                    &provider_key,
                    &request.idempotency_key,
                    &hash,
                    serde_json::json!({"request": &request, "refund_id": &refund_id}),
                    &lease,
                    &error,
                )?;
                return Err(error);
            }
        };
        let refunded = self
            .store
            .committed_refunds_for_payment(org, &current_payment.id)?
            .max(current_payment.refunded_minor);
        let available = current_payment.amount_minor - refunded;
        if request.amount_minor > available {
            let error = PaymentError::RefundExceedsPaid {
                paid: current_payment.amount_minor,
                refunded,
                requested: request.amount_minor,
            };
            self.abort_leased_operation(
                org,
                "refund",
                FinancialResourceType::Refund,
                &refund_id,
                &provider_key,
                &request.idempotency_key,
                &hash,
                serde_json::json!({"request": &request, "refund_id": &refund_id}),
                &lease,
                &error,
            )?;
            return Err(error);
        }
        match self.acquire_claim(
            org,
            &request.idempotency_key,
            "refund",
            FinancialResourceType::Refund,
            &refund_id,
            &hash,
            &provider_key,
            &serde_json::json!({"request": &request, "refund_id": &refund_id}),
        )? {
            ClaimAcquisition::Acquired => {}
            ClaimAcquisition::Recorded(claim) => {
                let (response, _) = self.claim_replay(&claim, true)?;
                self.release_lease_for_claim(org, &lease, &claim);
                return self.refund_from_replay(org, &response, true);
            }
            ClaimAcquisition::InFlight(claim) => return Err(Self::claim_in_flight_error(&claim)),
            ClaimAcquisition::Unresolved(claim) => {
                return Err(Self::claim_unresolved_error(&claim))
            }
        }
        self.store
            .mark_claim_dispatched(org, &request.idempotency_key, self.now_ms())?;
        let remote = match self
            .provider
            .refund(crate::provider::ProviderRefundRequest {
                organization_id: org.to_string(),
                idempotency_key: provider_key,
                provider_payment_id: current_payment.provider_payment_id.clone(),
                amount_minor: request.amount_minor,
                currency: current_payment.currency.clone(),
                reason: request.reason.clone(),
            })
            .await
        {
            Ok(remote) => remote,
            Err(error) => {
                self.fail_dispatch(org, &request.idempotency_key, &error)?;
                self.release_lease_if_claim_failed(org, &lease);
                return Err(error);
            }
        };
        let now = self.now_ms();
        if remote.amount_minor != request.amount_minor
            || remote.currency != current_payment.currency
        {
            self.store
                .mark_claim_unknown(org, &request.idempotency_key, now)?;
            return Err(PaymentError::Provider(format!(
                "provider refund {} {} does not match the requested {} {}",
                remote.amount_minor,
                remote.currency,
                request.amount_minor,
                current_payment.currency
            )));
        }
        let status = match RefundStatus::parse(&remote.status) {
            Some(status) => status,
            None => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, now)?;
                return Err(PaymentError::Provider(format!(
                    "provider refund returned an unsupported status {:?}",
                    remote.status
                )));
            }
        };
        let existing_refund = self.store.refund_by_provider_id(org, &remote.id)?;
        let response_refund_id = existing_refund
            .as_ref()
            .map(|refund| refund.id.clone())
            .unwrap_or_else(|| refund_id.clone());
        let credit_note_id = if status == RefundStatus::Succeeded {
            Some(format!("cn_{}", uuid::Uuid::new_v4()))
        } else {
            None
        };
        let refund = Refund {
            id: refund_id.clone(),
            organization_id: org.to_string(),
            payment_id: current_payment.id.clone(),
            provider_refund_id: remote.id.clone(),
            amount_minor: request.amount_minor,
            currency: current_payment.currency.clone(),
            reason: request.reason.clone(),
            status,
            credit_note_id: None,
            invoice_line_seq: None,
            created_ms: now,
        };
        let provider_refund = ProviderRefundObservation {
            provider: self.provider.name().to_string(),
            provider_refund_id: remote.id.clone(),
            organization_id: org.to_string(),
            invoice_id: current_payment.invoice_id.clone(),
            payment_id: current_payment.id.clone(),
            status: status.as_str().to_string(),
            amount_minor: request.amount_minor,
            currency: current_payment.currency.clone(),
            observed_ms: now,
            source: crate::authority::ProviderObservationSource::Refund,
        };
        let write = self.write(
            org,
            &request.idempotency_key,
            "refund",
            hash.clone(),
            &serde_json::json!({
                "refund_id": response_refund_id,
                "credit_note_id": credit_note_id,
            }),
            now,
        )?;
        let refund_template = refund.clone();
        let provider_refund_template = provider_refund.clone();
        let remote_id = remote.id.clone();
        let result = self.store.record_atomic(&write, |tx| {
            let mut current_payment = payment_tx(tx, org, &refund_template.payment_id)?
                .ok_or_else(|| PaymentError::NotFound("payment".into()))?;
            let mut current_invoice = invoice_tx(tx, org, &current_payment.invoice_id)?
                .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
            if let Some(existing) = refund_by_provider_id_conn(tx, org, &remote_id)? {
                return Ok((existing, None, current_payment, current_invoice));
            }
            let mut current_refund = refund_template.clone();
            current_refund.invoice_line_seq = bind_refund_invoice_line(&current_invoice);
            let mut note = None;
            if current_refund.status == RefundStatus::Succeeded {
                let note_id = credit_note_id.clone().ok_or_else(|| {
                    PaymentError::Store("settled refund has no credit-note id".into())
                })?;
                let mut settled = settle_refund_ledger(
                    &mut current_payment,
                    &mut current_invoice,
                    &mut current_refund,
                    org,
                    note_id,
                    now,
                )?;
                let sequence = next_sequence_tx(tx, org, "credit_note")?;
                settled.number = format!("CN-{sequence:06}");
                insert_credit_note_tx(tx, &settled)?;
                note = Some(settled);
            }
            insert_refund_tx(tx, &current_refund)?;
            upsert_provider_refund_conn(tx, &provider_refund_template)?;
            if current_refund.status == RefundStatus::Succeeded {
                insert_payment_tx(tx, &current_payment)?;
                update_invoice_tx(tx, &current_invoice)?;
            }
            Ok((current_refund, note, current_payment, current_invoice))
        });
        match result {
            Ok((refund, note, payment, invoice)) => {
                let note = match (note, refund.credit_note_id.clone()) {
                    (Some(note), _) => Some(note),
                    (None, Some(note_id)) => self.store.credit_note(org, &note_id)?,
                    (None, None) => None,
                };
                if matches!(status, RefundStatus::Succeeded | RefundStatus::Failed) {
                    self.release_lease_terminal(org, &lease);
                }
                Ok(RefundOutcome {
                    refund,
                    credit_note: note,
                    payment,
                    invoice,
                    replayed: false,
                })
            }
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("refund", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("refund was recorded concurrently".into())
                    })?;
                self.release_lease_if_claim_terminal(org, &lease);
                self.refund_from_replay(org, &response, true)
            }
            Err(e) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, self.now_ms())?;
                Err(e)
            }
        }
    }

    async fn acquire_refund_lease(
        &self,
        org: &str,
        domain: &ConflictDomain,
        resource: &str,
        claim: &LeaseClaim,
        amount: LeaseAmount,
        payment_id: &str,
    ) -> Result<FinancialMutationLease, PaymentError> {
        let deadline = tokio::time::Instant::now() + REFUND_FENCE_WAIT;
        loop {
            match self
                .store
                .try_acquire(org, domain, resource, claim, Some(amount.clone()))?
            {
                LeaseAcquisition::Held(lease) | LeaseAcquisition::AlreadyHeldBySameClaim(lease) => {
                    return Ok(lease)
                }
                LeaseAcquisition::Busy { holder } => {
                    if self.refund_fence_is_pinned(org, &holder, payment_id)? {
                        return Err(PaymentError::Conflict(format!(
                            "payment {} is fenced by operation {} with a pending refund reservation",
                            payment_id, holder.idempotency_key
                        )));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(PaymentError::Conflict(format!(
                            "payment {} is fenced by operation {}; retry after it reaches a \
                             terminal outcome",
                            payment_id, holder.idempotency_key
                        )));
                    }
                    tokio::time::sleep(REFUND_FENCE_POLL).await;
                }
            }
        }
    }

    fn refund_fence_is_pinned(
        &self,
        org: &str,
        holder: &FinancialMutationLease,
        payment_id: &str,
    ) -> Result<bool, PaymentError> {
        let Some(claim) = self.store.operation_claim(org, &holder.idempotency_key)? else {
            return Ok(false);
        };
        if claim.state == OperationClaimState::Unknown {
            return Ok(true);
        }
        if !claim.state.is_terminal() {
            return Ok(false);
        }
        Ok(self
            .store
            .refunds_for_payment(org, payment_id)?
            .iter()
            .any(|refund| refund.status == RefundStatus::Pending))
    }

    fn resume_existing_pay_claim(
        &self,
        org: &str,
        claim: OperationClaim,
        lease: &FinancialMutationLease,
    ) -> Result<PayInvoiceOutcome, PaymentError> {
        match claim.state {
            OperationClaimState::Succeeded => {
                let (response, _) = self.claim_replay(&claim, true)?;
                self.release_lease_for_claim(org, lease, &claim);
                self.pay_from_replay(org, &response, true)
            }
            OperationClaimState::Failed => {
                self.release_lease_for_claim(org, lease, &claim);
                Err(PaymentError::from_recorded(
                    claim.failure_code.as_deref().unwrap_or("conflict"),
                    claim
                        .failure_message
                        .as_deref()
                        .unwrap_or("the recorded operation failed"),
                ))
            }
            OperationClaimState::Unknown => Err(Self::claim_unresolved_error(&claim)),
            OperationClaimState::Claimed | OperationClaimState::Dispatched => {
                Err(Self::claim_in_flight_error(&claim))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn abort_leased_operation(
        &self,
        org: &str,
        operation: &str,
        resource_type: FinancialResourceType,
        resource_id: &str,
        provider_key: &str,
        key: &str,
        hash: &str,
        context: serde_json::Value,
        lease: &FinancialMutationLease,
        error: &PaymentError,
    ) -> Result<(), PaymentError> {
        let claim = self.store.operation_claim(org, key)?;
        let state = match claim {
            Some(claim) => Some(claim.state),
            None => match self.acquire_claim(
                org,
                key,
                operation,
                resource_type,
                resource_id,
                hash,
                provider_key,
                &context,
            )? {
                ClaimAcquisition::Acquired => {
                    self.store.fail_claim(
                        org,
                        key,
                        error.code(),
                        &error.to_string(),
                        self.now_ms(),
                    )?;
                    Some(OperationClaimState::Failed)
                }
                ClaimAcquisition::Recorded(claim) => Some(claim.state),
                ClaimAcquisition::InFlight(claim) => Some(claim.state),
                ClaimAcquisition::Unresolved(claim) => Some(claim.state),
            },
        };
        match state {
            Some(OperationClaimState::Succeeded) => {
                let _ = self
                    .store
                    .release_terminal(org, &lease.id, TerminalOutcome::Succeeded);
            }
            Some(OperationClaimState::Failed) => {
                let _ = self.store.release_no_effect(org, &lease.id);
            }
            _ => {}
        }
        Ok(())
    }

    fn release_lease_terminal(&self, org: &str, lease: &FinancialMutationLease) {
        let _ = self
            .store
            .release_terminal(org, &lease.id, TerminalOutcome::Succeeded);
    }

    fn release_lease_if_claim_failed(&self, org: &str, lease: &FinancialMutationLease) {
        match self.store.operation_claim(org, &lease.idempotency_key) {
            Ok(Some(claim)) if claim.state == OperationClaimState::Failed => {
                let _ = self.store.release_no_effect(org, &lease.id);
            }
            Ok(_) => {}
            Err(_) => {
                // A failed read leaves the fence held; startup/reconcile
                // recovery is the durable owner of that obligation.
            }
        }
    }

    fn release_lease_if_claim_terminal(&self, org: &str, lease: &FinancialMutationLease) {
        match self.store.operation_claim(org, &lease.idempotency_key) {
            Ok(Some(claim)) => self.release_lease_for_claim(org, lease, &claim),
            Ok(None) => {}
            Err(_) => {
                // Same fail-closed rule: never release a fence on an
                // unreadable claim.
            }
        }
    }

    fn release_lease_for_claim(
        &self,
        org: &str,
        lease: &FinancialMutationLease,
        claim: &OperationClaim,
    ) {
        match claim.state {
            OperationClaimState::Succeeded => {
                if self.claim_business_outcome_is_terminal(org, claim) {
                    self.release_lease_terminal(org, lease);
                }
            }
            OperationClaimState::Failed => {
                let _ = self.store.release_no_effect(org, &lease.id);
            }
            _ => {}
        }
    }

    pub(crate) fn recover_invoice_and_refund_leases(&self) -> Result<(), PaymentError> {
        let held: Vec<(String, String)> = self.store.tx(|tx| {
            let mut statement = tx
                .prepare(
                    "SELECT organization_id, id FROM financial_mutation_lease \
                     WHERE state = 'held' ORDER BY acquired_ms, id",
                )
                .map_err(|error| {
                    PaymentError::Store(format!("read held financial leases: {error}"))
                })?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| {
                    PaymentError::Store(format!("read held financial leases: {error}"))
                })?;
            let mut leases = Vec::new();
            for row in rows {
                leases.push(row.map_err(|error| {
                    PaymentError::Store(format!("read held financial leases: {error}"))
                })?);
            }
            Ok(leases)
        })?;
        for (org, lease_id) in held {
            let Some(lease) = self.store.lease(&org, &lease_id)? else {
                continue;
            };
            let operation = match &lease.conflict_domain {
                ConflictDomain::InvoicePayment { invoice_id } => (
                    "pay_invoice",
                    FinancialResourceType::Payment,
                    operation_resource_id("pay", &org, "pay_invoice", &lease.idempotency_key),
                    format!("{}:charge", lease.idempotency_key),
                    invoice_id.clone(),
                ),
                ConflictDomain::PaymentRefund { payment_id } => (
                    "refund",
                    FinancialResourceType::Refund,
                    operation_resource_id("re", &org, "refund", &lease.idempotency_key),
                    format!("{}:refund", lease.idempotency_key),
                    payment_id.clone(),
                ),
                _ => continue,
            };
            let (operation, resource_type, resource_id, provider_key, _resource) = operation;
            let claim = self.store.operation_claim(&org, &lease.idempotency_key)?;
            let Some(claim) = claim else {
                let draft = OperationClaimDraft {
                    organization_id: org.clone(),
                    idempotency_key: lease.idempotency_key.clone(),
                    operation: operation.to_string(),
                    resource_type,
                    resource_id,
                    request_digest: lease.request_digest.clone(),
                    provider_idempotency_key: provider_key,
                    context: "{}".into(),
                    created_ms: lease.acquired_ms,
                };
                if matches!(
                    self.store.claim_operation(&draft)?,
                    ClaimAcquisition::Acquired
                ) {
                    let _ = self.store.fail_claim(
                        &org,
                        &lease.idempotency_key,
                        "conflict",
                        "financial lease found without a claim; no provider effect exists",
                        self.now_ms(),
                    );
                    let _ = self.store.release_no_effect(&org, &lease.id);
                }
                continue;
            };
            match claim.state {
                OperationClaimState::Succeeded => {
                    if self.claim_business_outcome_is_terminal(&org, &claim) {
                        self.release_lease_terminal(&org, &lease);
                    }
                }
                OperationClaimState::Failed => {
                    let _ = self.store.release_no_effect(&org, &lease.id);
                }
                OperationClaimState::Claimed => {
                    let _ = self.store.fail_claim(
                        &org,
                        &lease.idempotency_key,
                        "conflict",
                        "financial lease never crossed the provider boundary",
                        self.now_ms(),
                    );
                    let _ = self.store.release_no_effect(&org, &lease.id);
                }
                OperationClaimState::Unknown | OperationClaimState::Dispatched => {}
            }
        }
        Ok(())
    }

    pub(crate) fn claim_business_outcome_is_terminal(
        &self,
        org: &str,
        claim: &OperationClaim,
    ) -> bool {
        let Ok(context) = serde_json::from_str::<serde_json::Value>(&claim.context) else {
            return false;
        };
        match claim.operation.as_str() {
            "pay_invoice" => {
                let Some(payment_id) = context
                    .get("payment_id")
                    .and_then(serde_json::Value::as_str)
                else {
                    return false;
                };
                self.store
                    .payment(org, payment_id)
                    .ok()
                    .flatten()
                    .is_some_and(|payment| !payment.status.is_transient())
            }
            "refund" => {
                let Some(refund_id) = context.get("refund_id").and_then(serde_json::Value::as_str)
                else {
                    return false;
                };
                self.store
                    .refund_by_id(org, refund_id)
                    .ok()
                    .flatten()
                    .is_some_and(|refund| refund.status != RefundStatus::Pending)
            }
            _ => true,
        }
    }

    pub(crate) fn release_lease_after_claim_reconciliation(
        &self,
        org: &str,
        claim: &OperationClaim,
        succeeded: bool,
    ) {
        let context: serde_json::Value = match serde_json::from_str(&claim.context) {
            Ok(context) => context,
            Err(_) => return,
        };
        let (domain, resource) = match claim.operation.as_str() {
            "pay_invoice" => {
                let Some(invoice_id) = context
                    .pointer("/request/invoice_id")
                    .and_then(serde_json::Value::as_str)
                else {
                    return;
                };
                (
                    ConflictDomain::InvoicePayment {
                        invoice_id: invoice_id.to_string(),
                    },
                    invoice_id.to_string(),
                )
            }
            "refund" => {
                let Some(payment_id) = context
                    .pointer("/request/payment_id")
                    .and_then(serde_json::Value::as_str)
                else {
                    return;
                };
                (
                    ConflictDomain::PaymentRefund {
                        payment_id: payment_id.to_string(),
                    },
                    payment_id.to_string(),
                )
            }
            _ => return,
        };
        let lease = match self.store.active_for(org, &domain, &resource) {
            Ok(Some(lease)) => lease,
            Ok(None) => return,
            Err(_) => return,
        };
        if lease.idempotency_key != claim.idempotency_key {
            return;
        }
        if succeeded {
            self.release_lease_terminal(org, &lease);
        } else {
            let _ = self.store.release_no_effect(org, &lease.id);
        }
    }

    #[allow(clippy::single_match)]
    pub(crate) fn release_leases_after_webhook(&self, org: &str, effects: &WebhookEffects) {
        if let Some(payment) = &effects.payment {
            if matches!(
                payment.status,
                PaymentStatus::Succeeded
                    | PaymentStatus::Failed
                    | PaymentStatus::PartiallyRefunded
                    | PaymentStatus::Refunded
            ) {
                let domain = ConflictDomain::InvoicePayment {
                    invoice_id: payment.invoice_id.clone(),
                };
                match self.store.active_for(org, &domain, &payment.invoice_id) {
                    Ok(Some(lease)) => self.release_lease_if_claim_terminal(org, &lease),
                    _ => {}
                }
            }
        }
        if let Some(refund) = &effects.refund {
            if matches!(
                refund.status,
                RefundStatus::Succeeded | RefundStatus::Failed
            ) {
                let domain = ConflictDomain::PaymentRefund {
                    payment_id: refund.payment_id.clone(),
                };
                match self.store.active_for(org, &domain, &refund.payment_id) {
                    Ok(Some(lease)) => self.release_lease_if_claim_terminal(org, &lease),
                    _ => {}
                }
            }
        }
    }
}
