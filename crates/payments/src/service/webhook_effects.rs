//! Webhook effect planning (split from the webhook core so the dispatcher,
//! each provider-event phase boundary and the source-size ceiling stay
//! reviewable). Every arm is a pure durable-row decision: the caller commits
//! the returned effects and the idempotency record in ONE transaction.

use super::*;

impl PaymentService {
    /// Map one verified provider event to the durable rows it owns. The
    /// dispatcher never writes; unbound/foreign/unrecognized events are an
    /// explicit no-op, while provider truth mismatches are typed conflicts.
    pub(super) fn webhook_effects_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        org: &str,
        event: &VerifiedWebhookEvent,
        now: i64,
    ) -> Result<WebhookEffects, PaymentError> {
        let object = event
            .payload
            .get("data")
            .and_then(|data| data.get("object"));
        match event.event_type.as_str() {
            "payment.succeeded" | "payment_intent.succeeded" | "charge.succeeded" => {
                self.webhook_payment_succeeded_tx(tx, org, object, now)
            }
            "payment.processing"
            | "payment_intent.processing"
            | "charge.processing"
            | "payment.requires_action"
            | "payment_intent.requires_action"
            | "charge.requires_action"
            | "payment.pending" => self.webhook_payment_processing_tx(tx, org, event, object),
            "payment.failed" | "payment_intent.payment_failed" | "charge.failed" => {
                self.webhook_payment_failed_tx(tx, org, object, now)
            }
            "refund.succeeded" | "refund.updated" | "refund.failed" | "charge.refund.updated" => {
                self.webhook_refund_tx(tx, org, object, now)
            }
            "invoice.paid" | "invoice.payment_succeeded" => {
                self.webhook_invoice_paid_tx(tx, org, event, object, now)
            }
            "invoice.payment_failed" | "invoice.payment_action_required" => {
                self.webhook_invoice_failed_tx(tx, org, event, object, now)
            }
            "customer.subscription.updated" => {
                self.webhook_subscription_updated_tx(tx, org, object, now)
            }
            "customer.subscription.deleted" | "subscription.cancelled" => {
                self.webhook_subscription_deleted_tx(tx, org, object, now)
            }
            _ => Ok(WebhookEffects::default()),
        }
    }

    fn webhook_payment_succeeded_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        org: &str,
        object: Option<&serde_json::Value>,
        now: i64,
    ) -> Result<WebhookEffects, PaymentError> {
        let object_id = || {
            object
                .and_then(|object| object.get("id"))
                .and_then(serde_json::Value::as_str)
        };
        let local_invoice_id = || {
            object
                .and_then(|object| object.pointer("/metadata/local_invoice_id"))
                .and_then(serde_json::Value::as_str)
        };
        let Some(provider_payment_id) = object_id() else {
            return Ok(WebhookEffects::default());
        };
        let mut payment = payment_by_provider_id_tx(tx, org, provider_payment_id)?;
        let mut invoice = match (payment.as_ref(), local_invoice_id()) {
            (Some(payment), _) => invoice_tx(tx, org, &payment.invoice_id)?,
            (None, Some(local_id)) => invoice_tx(tx, org, local_id)?,
            (None, None) => None,
        };
        let Some(invoice_ref) = invoice.as_mut() else {
            return Ok(WebhookEffects::default());
        };
        if invoice_ref.organization_id != org {
            return Ok(WebhookEffects::default());
        }
        if Self::settlement_fenced_by_subscription_mutation(tx, org, invoice_ref)? {
            return Err(PaymentError::Conflict(
                "a subscription mutation is in flight; retry the webhook".into(),
            ));
        }
        let amount = object.and_then(|object| {
            object
                .get("amount_minor")
                .and_then(serde_json::Value::as_i64)
                .or_else(|| object.get("amount").and_then(serde_json::Value::as_i64))
                .or_else(|| {
                    object
                        .get("amount_received")
                        .and_then(serde_json::Value::as_i64)
                })
        });
        let currency = object
            .and_then(|object| object.get("currency"))
            .and_then(serde_json::Value::as_str);
        if amount.is_some_and(|amount| amount < 0 || !crate::money::valid_amount(amount))
            || currency.is_some_and(|currency| !crate::money::valid_currency(currency))
        {
            return Err(PaymentError::Conflict(
                "webhook payment carries an invalid amount or currency".into(),
            ));
        }
        if let Some(payment) = payment.as_mut() {
            if amount.is_some_and(|amount| amount != payment.amount_minor)
                || currency.is_some_and(|currency| currency != payment.currency)
            {
                return Err(PaymentError::Conflict(
                    "webhook payment truth does not match the recorded payment".into(),
                ));
            }
            // A payment that is already settled has already credited its
            // invoice in the same transaction that recorded it. Applying a
            // second event for the same provider payment (for example
            // `payment.succeeded` after `invoice.paid` bound the same
            // payment_intent) must not add the amount again: the event id
            // differs, so event-level dedupe alone cannot protect the
            // ledger from order-dependent over-crediting.
            if payment.status == PaymentStatus::Succeeded {
                return Ok(WebhookEffects::default());
            }
            if !payment.status.can_transition_to(PaymentStatus::Succeeded) {
                return Ok(WebhookEffects::default());
            }
            payment.status = PaymentStatus::Succeeded;
        } else {
            if currency.is_some_and(|currency| currency != invoice_ref.currency) {
                return Err(PaymentError::Conflict(
                    "webhook payment currency does not match the invoice currency".into(),
                ));
            }
            let recorded = amount.unwrap_or_else(|| invoice_ref.balance_minor());
            payment = Some(Payment {
                id: format!("pay_{}", uuid::Uuid::new_v4()),
                organization_id: org.to_string(),
                invoice_id: invoice_ref.id.clone(),
                provider: self.provider.name().to_string(),
                provider_payment_id: provider_payment_id.to_string(),
                payment_method_id: None,
                amount_minor: recorded,
                refunded_minor: 0,
                currency: invoice_ref.currency.clone(),
                status: PaymentStatus::Succeeded,
                created_ms: now,
            });
        }
        let credited = amount
            .or_else(|| payment.as_ref().map(|payment| payment.amount_minor))
            .unwrap_or_else(|| invoice_ref.balance_minor())
            .clamp(0, invoice_ref.balance_minor());
        invoice_ref.amount_paid_minor = checked_add(invoice_ref.amount_paid_minor, credited)?;
        if invoice_ref.amount_paid_minor >= invoice_ref.total_minor
            && invoice_ref.status.can_transition_to(InvoiceStatus::Paid)
        {
            invoice_ref.status = InvoiceStatus::Paid;
            invoice_ref.paid_ms = Some(now);
        }
        let subscription = match invoice_ref.subscription_id.as_deref() {
            Some(id) if !Self::subscription_mutation_fence_active(tx, org, id)? => {
                subscription_by_id_tx(tx, org, id)
                    .ok()
                    .flatten()
                    .filter(|subscription| {
                        subscription
                            .state
                            .can_transition_to(SubscriptionState::Active)
                    })
                    .map(|mut subscription| {
                        subscription.state = SubscriptionState::Active;
                        subscription.dunning_attempts = 0;
                        subscription.grace_end_ms = None;
                        subscription.updated_ms = now;
                        subscription
                    })
            }
            _ => None,
        };
        Ok(WebhookEffects {
            subscription,
            invoice: Some(invoice_ref.clone()),
            payment,
            refund: None,
            credit_note: None,
            ..WebhookEffects::default()
        })
    }

    fn webhook_payment_processing_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        org: &str,
        event: &VerifiedWebhookEvent,
        object: Option<&serde_json::Value>,
    ) -> Result<WebhookEffects, PaymentError> {
        let object_id = || {
            object
                .and_then(|object| object.get("id"))
                .and_then(serde_json::Value::as_str)
        };
        let Some(provider_payment_id) = object_id() else {
            return Ok(WebhookEffects::default());
        };
        let Some(mut payment) = payment_by_provider_id_tx(tx, org, provider_payment_id)? else {
            return Ok(WebhookEffects::default());
        };
        let next = match event.event_type.as_str() {
            "payment.requires_action"
            | "payment_intent.requires_action"
            | "charge.requires_action" => PaymentStatus::RequiresAction,
            "payment.pending" => PaymentStatus::Pending,
            _ => PaymentStatus::Processing,
        };
        if !payment.status.can_transition_to(next) {
            return Ok(WebhookEffects::default());
        }
        payment.status = next;
        Ok(WebhookEffects {
            subscription: None,
            invoice: None,
            payment: Some(payment),
            refund: None,
            credit_note: None,
            ..WebhookEffects::default()
        })
    }

    fn webhook_payment_failed_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        org: &str,
        object: Option<&serde_json::Value>,
        now: i64,
    ) -> Result<WebhookEffects, PaymentError> {
        let object_id = || {
            object
                .and_then(|object| object.get("id"))
                .and_then(serde_json::Value::as_str)
        };
        let Some(provider_payment_id) = object_id() else {
            return Ok(WebhookEffects::default());
        };
        let Some(mut payment) = payment_by_provider_id_tx(tx, org, provider_payment_id)? else {
            return Ok(WebhookEffects::default());
        };
        if !payment.status.can_transition_to(PaymentStatus::Failed) {
            return Ok(WebhookEffects::default());
        }
        payment.status = PaymentStatus::Failed;
        let Some(mut invoice) = invoice_tx(tx, org, &payment.invoice_id)? else {
            return Ok(WebhookEffects {
                subscription: None,
                invoice: None,
                payment: Some(payment),
                refund: None,
                credit_note: None,
                ..WebhookEffects::default()
            });
        };
        if invoice.organization_id != org
            || !invoice.status.can_transition_to(InvoiceStatus::PastDue)
        {
            return Ok(WebhookEffects {
                subscription: None,
                invoice: None,
                payment: Some(payment),
                refund: None,
                credit_note: None,
                ..WebhookEffects::default()
            });
        }
        if let Some(subscription_id) = invoice.subscription_id.as_deref() {
            if Self::subscription_mutation_fence_active(tx, org, subscription_id)? {
                return Err(PaymentError::Conflict(
                    "a subscription mutation is in flight; retry the webhook".into(),
                ));
            }
        }
        // One failed provider attempt can be delivered as both a payment
        // event and an invoice event with different event ids. Advance the
        // attempt counter only when the payment identity is new.
        let advance_dunning =
            invoice.last_dunning_payment_id.as_deref() != Some(provider_payment_id);
        invoice.last_dunning_payment_id = Some(provider_payment_id.to_string());
        invoice.status = InvoiceStatus::PastDue;
        let mut subscription = invoice
            .subscription_id
            .as_deref()
            .and_then(|id| subscription_by_id_tx(tx, org, id).ok().flatten())
            .filter(|subscription| {
                subscription
                    .state
                    .can_transition_to(SubscriptionState::PastDue)
            });
        if let Some(sub) = subscription.as_mut() {
            sub.state = SubscriptionState::PastDue;
            if advance_dunning {
                sub.dunning_attempts = sub.dunning_attempts.saturating_add(1);
            }
            sub.updated_ms = now;
            if sub.grace_end_ms.is_none() {
                sub.grace_end_ms = Some(now.saturating_add(self.config.dunning.grace_ms.max(0)));
            }
            if sub.dunning_attempts >= self.config.dunning.max_attempts {
                sub.state = SubscriptionState::Canceled;
                sub.canceled_ms = Some(now);
                sub.cancel_at_period_end = false;
            }
        }
        Ok(WebhookEffects {
            subscription,
            invoice: Some(invoice),
            payment: Some(payment),
            refund: None,
            credit_note: None,
            ..WebhookEffects::default()
        })
    }

    fn webhook_refund_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        org: &str,
        object: Option<&serde_json::Value>,
        now: i64,
    ) -> Result<WebhookEffects, PaymentError> {
        let object_id = || {
            object
                .and_then(|object| object.get("id"))
                .and_then(serde_json::Value::as_str)
        };
        let provider_refund_id = object_id();
        let local_refund_id = object
            .and_then(|object| object.pointer("/metadata/local_refund_id"))
            .and_then(serde_json::Value::as_str);
        let mut refund = match local_refund_id {
            Some(local_id) => refund_by_id_tx(tx, org, local_id)?,
            None => {
                let Some(provider_refund_id) = provider_refund_id else {
                    return Ok(WebhookEffects::default());
                };
                refund_by_provider_id_conn(tx, org, provider_refund_id)?
            }
        };
        let Some(refund_ref) = refund.as_mut() else {
            // The provider already produced a refund object while our local
            // reservation is still between dispatch and its durable row.
            // Acknowledge nothing: recording this event would make the later
            // status transition a duplicate and strand the refund pending.
            if provider_refund_id.is_some() && Self::refund_mutation_in_flight(tx, org)? {
                return Err(PaymentError::Conflict(
                    "a refund mutation is in flight; retry the webhook".into(),
                ));
            }
            return Ok(WebhookEffects::default());
        };
        if refund_ref.organization_id != org || refund_ref.credit_note_id.is_some() {
            return Ok(WebhookEffects::default());
        }
        if provider_refund_id.is_some_and(|id| id != refund_ref.provider_refund_id.as_str()) {
            return Err(PaymentError::Conflict(
                "refund webhook id does not match the recorded provider refund".into(),
            ));
        }
        let webhook_amount = object.and_then(|object| {
            object
                .get("amount_minor")
                .and_then(serde_json::Value::as_i64)
                .or_else(|| object.get("amount").and_then(serde_json::Value::as_i64))
        });
        if webhook_amount.is_some_and(|amount| amount != refund_ref.amount_minor) {
            return Err(PaymentError::Conflict(
                "refund webhook amount does not match the recorded refund".into(),
            ));
        }
        let webhook_currency = object
            .and_then(|object| object.get("currency"))
            .and_then(serde_json::Value::as_str);
        if webhook_currency
            .is_some_and(|currency| !currency.eq_ignore_ascii_case(&refund_ref.currency))
        {
            return Err(PaymentError::Conflict(
                "refund webhook currency does not match the recorded refund".into(),
            ));
        }
        let provider_status = object
            .and_then(|object| object.get("status"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                PaymentError::WebhookMalformed(
                    "refund webhook carries no explicit provider status".into(),
                )
            })?;
        let next = RefundStatus::parse(provider_status).ok_or_else(|| {
            PaymentError::WebhookMalformed(format!(
                "refund webhook carries unknown status {provider_status:?}"
            ))
        })?;
        if !refund_ref.status.can_transition_to(next) {
            return Ok(WebhookEffects::default());
        }
        if next != RefundStatus::Succeeded {
            refund_ref.status = next;
            return Ok(WebhookEffects {
                subscription: None,
                invoice: None,
                payment: None,
                refund: Some(refund_ref.clone()),
                credit_note: None,
                ..WebhookEffects::default()
            });
        }
        let Some(mut payment) = payment_tx(tx, org, &refund_ref.payment_id)? else {
            return Ok(WebhookEffects::default());
        };
        let Some(mut invoice) = invoice_tx(tx, org, &payment.invoice_id)? else {
            return Ok(WebhookEffects::default());
        };
        refund_ref.status = RefundStatus::Succeeded;
        refund_ref.invoice_line_seq = bind_refund_invoice_line(&invoice);
        let provider_refund = ProviderRefundObservation {
            provider: self.provider.name().to_string(),
            provider_refund_id: refund_ref.provider_refund_id.clone(),
            organization_id: org.to_string(),
            invoice_id: invoice.id.clone(),
            payment_id: payment.id.clone(),
            status: RefundStatus::Succeeded.as_str().to_string(),
            amount_minor: refund_ref.amount_minor,
            currency: refund_ref.currency.clone(),
            observed_ms: now,
            source: ProviderObservationSource::Webhook,
        };
        let mut note = settle_refund_ledger(
            &mut payment,
            &mut invoice,
            refund_ref,
            org,
            format!("cn_{}", uuid::Uuid::new_v4()),
            now,
        )?;
        let sequence = next_sequence_tx(tx, org, "credit_note")?;
        note.number = format!("CN-{sequence:06}");
        Ok(WebhookEffects {
            subscription: None,
            invoice: Some(invoice),
            payment: Some(payment),
            refund: Some(refund_ref.clone()),
            credit_note: Some(note),
            provider_refund: Some(provider_refund),
            ..WebhookEffects::default()
        })
    }

    fn webhook_invoice_failed_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        org: &str,
        event: &VerifiedWebhookEvent,
        object: Option<&serde_json::Value>,
        now: i64,
    ) -> Result<WebhookEffects, PaymentError> {
        let object_id = || {
            object
                .and_then(|object| object.get("id"))
                .and_then(serde_json::Value::as_str)
        };
        let local_invoice_id = || {
            object
                .and_then(|object| object.pointer("/metadata/local_invoice_id"))
                .and_then(serde_json::Value::as_str)
        };
        let provider_invoice_id = object_id();
        let local_id = local_invoice_id();
        let mut failed_payment = if event.event_type == "invoice.payment_failed" {
            match provider_invoice_id {
                Some(provider_payment_id) => {
                    payment_by_provider_id_tx(tx, org, provider_payment_id)?
                        .filter(|payment| payment.status.can_transition_to(PaymentStatus::Failed))
                }
                None => None,
            }
        } else {
            None
        };
        if let Some(payment) = failed_payment.as_mut() {
            payment.status = PaymentStatus::Failed;
        }
        let invoice = if let Some(local_id) = local_id {
            invoice_tx(tx, org, local_id)?
        } else if let Some(provider_invoice_id) = provider_invoice_id {
            invoice_by_provider_id_tx(tx, provider_invoice_id)?
        } else {
            None
        };
        let Some(mut invoice) = invoice else {
            return Ok(WebhookEffects {
                payment: failed_payment,
                ..WebhookEffects::default()
            });
        };
        if invoice.organization_id != org
            || !invoice.status.can_transition_to(InvoiceStatus::PastDue)
        {
            return Ok(WebhookEffects {
                payment: failed_payment,
                ..WebhookEffects::default()
            });
        }
        if let Some(subscription_id) = invoice.subscription_id.as_deref() {
            if Self::subscription_mutation_fence_active(tx, org, subscription_id)? {
                return Err(PaymentError::Conflict(
                    "a subscription mutation is in flight; retry the webhook".into(),
                ));
            }
        }
        let failure_identity = failed_payment
            .as_ref()
            .map(|payment| payment.provider_payment_id.clone())
            .or_else(|| failure_payment_identity(object))
            .unwrap_or_else(|| event.id.clone());
        let advance_dunning =
            invoice.last_dunning_payment_id.as_deref() != Some(failure_identity.as_str());
        invoice.last_dunning_payment_id = Some(failure_identity);
        invoice.status = InvoiceStatus::PastDue;
        let subscription = invoice
            .subscription_id
            .as_deref()
            .and_then(|id| subscription_by_id_tx(tx, org, id).ok().flatten())
            .filter(|subscription| {
                subscription
                    .state
                    .can_transition_to(SubscriptionState::PastDue)
            })
            .map(|mut subscription| {
                if advance_dunning {
                    subscription.dunning_attempts = subscription.dunning_attempts.saturating_add(1);
                }
                subscription.state = SubscriptionState::PastDue;
                if subscription.grace_end_ms.is_none() {
                    subscription.grace_end_ms =
                        Some(now.saturating_add(self.config.dunning.grace_ms.max(0)));
                }
                subscription.updated_ms = now;
                if subscription.dunning_attempts >= self.config.dunning.max_attempts {
                    subscription.state = SubscriptionState::Canceled;
                    subscription.canceled_ms = Some(now);
                    subscription.cancel_at_period_end = false;
                }
                subscription
            });
        Ok(WebhookEffects {
            subscription,
            invoice: Some(invoice),
            payment: failed_payment,
            refund: None,
            credit_note: None,
            ..WebhookEffects::default()
        })
    }

    fn webhook_subscription_updated_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        org: &str,
        object: Option<&serde_json::Value>,
        now: i64,
    ) -> Result<WebhookEffects, PaymentError> {
        let object_id = || {
            object
                .and_then(|object| object.get("id"))
                .and_then(serde_json::Value::as_str)
        };
        let Some(provider_id) = object_id() else {
            return Ok(WebhookEffects::default());
        };
        let Some(mut subscription) = subscription_by_provider_id_tx(tx, provider_id)? else {
            return Ok(WebhookEffects::default());
        };
        if subscription.organization_id != org || subscription.state == SubscriptionState::Canceled
        {
            return Ok(WebhookEffects::default());
        }
        if Self::subscription_mutation_fence_active(tx, org, &subscription.id)?
            || crate::lease::active_lease_conn(
                tx,
                org,
                &ConflictDomain::Subscription {
                    organization_id: org.to_string(),
                },
            )?
            .is_some()
        {
            return Err(PaymentError::Conflict(
                "a subscription mutation is in flight; retry the webhook".into(),
            ));
        }
        if let Some(status) = object
            .and_then(|object| object.get("status"))
            .and_then(serde_json::Value::as_str)
        {
            if let Some(state) = SubscriptionState::parse(status) {
                if subscription.state.can_transition_to(state) {
                    subscription.state = state;
                    if state == SubscriptionState::Canceled {
                        subscription.canceled_ms = Some(now);
                    }
                }
            }
        }
        if let Some(quantity) = object
            .and_then(|object| object.get("quantity"))
            .and_then(serde_json::Value::as_u64)
        {
            let quantity = u32::try_from(quantity).map_err(|_| {
                PaymentError::WebhookMalformed("subscription quantity overflow".into())
            })?;
            if quantity == 0 || quantity > crate::service::MAX_SEATS {
                return Err(PaymentError::WebhookMalformed(format!(
                    "subscription quantity must be 1..={}",
                    crate::service::MAX_SEATS
                )));
            }
            subscription.quantity = quantity;
        }
        if let Some(cancel) = object
            .and_then(|object| object.get("cancel_at_period_end"))
            .and_then(serde_json::Value::as_bool)
        {
            subscription.cancel_at_period_end = cancel;
        }
        if let Some(end) = object
            .and_then(|object| object.get("current_period_end"))
            .and_then(serde_json::Value::as_i64)
        {
            let end = if end.abs() < 10_000_000_000 {
                end.saturating_mul(1000)
            } else {
                end
            };
            subscription.current_period_end_ms = subscription.current_period_end_ms.max(end);
        }
        subscription.updated_ms = now;
        Ok(WebhookEffects {
            subscription: Some(subscription),
            invoice: None,
            payment: None,
            refund: None,
            credit_note: None,
            ..WebhookEffects::default()
        })
    }

    fn webhook_subscription_deleted_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        org: &str,
        object: Option<&serde_json::Value>,
        now: i64,
    ) -> Result<WebhookEffects, PaymentError> {
        let object_id = || {
            object
                .and_then(|object| object.get("id"))
                .and_then(serde_json::Value::as_str)
        };
        let Some(provider_id) = object_id() else {
            return Ok(WebhookEffects::default());
        };
        let Some(mut subscription) = subscription_by_provider_id_tx(tx, provider_id)? else {
            return Ok(WebhookEffects::default());
        };
        if subscription.organization_id != org {
            return Ok(WebhookEffects::default());
        }
        if Self::subscription_mutation_fence_active(tx, org, &subscription.id)?
            || crate::lease::active_lease_conn(
                tx,
                org,
                &ConflictDomain::Subscription {
                    organization_id: org.to_string(),
                },
            )?
            .is_some()
        {
            return Err(PaymentError::Conflict(
                "a subscription mutation is in flight; retry the webhook".into(),
            ));
        }
        if subscription.state == SubscriptionState::Canceled && subscription.canceled_ms.is_some() {
            return Ok(WebhookEffects::default());
        }
        subscription.state = SubscriptionState::Canceled;
        subscription.canceled_ms = Some(now);
        subscription.cancel_at_period_end = false;
        subscription.updated_ms = now;
        Ok(WebhookEffects {
            subscription: Some(subscription),
            invoice: None,
            payment: None,
            refund: None,
            credit_note: None,
            ..WebhookEffects::default()
        })
    }
}

/// The provider payment identity carried by a failed payment/invoice event.
/// Stripe nests an id or object under `payment_intent`/`charge`; Mollie's
/// failed payment object id is already the payment id.
fn failure_payment_identity(object: Option<&serde_json::Value>) -> Option<String> {
    object
        .and_then(|object| {
            object
                .get("payment_intent")
                .or_else(|| object.get("charge"))
        })
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("id").and_then(serde_json::Value::as_str))
        })
        .filter(|id| !id.is_empty() && id.len() <= MAX_TEXT_BYTES)
        .map(str::to_string)
}
