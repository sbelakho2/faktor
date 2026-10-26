//! Provider invoice-paid reconciliation (split from the webhook core so the
//! phase boundary stays reviewable and the core dispatcher stays bounded).

use super::*;

impl PaymentService {
    pub(super) fn webhook_invoice_paid_tx(
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
        // A provider invoice names the payment that settled it. Binding that
        // identity here makes invoice.paid and payment_intent.succeeded
        // converge on ONE local receipt instead of minting a second
        // refundable payment for the same money.
        let provider_payment_id = object
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
            .map(str::to_string);
        let provider_invoice_id = object_id().map(str::to_string);
        let local_id = local_invoice_id();
        let invoice = if let Some(local_id) = local_id {
            invoice_tx(tx, org, local_id)?
        } else if let Some(provider_invoice_id) = provider_invoice_id.as_deref() {
            invoice_by_provider_id_tx(tx, provider_invoice_id)?
        } else {
            None
        };
        let Some(mut invoice) = invoice else {
            return Ok(WebhookEffects::default());
        };
        if invoice.organization_id != org {
            return Ok(WebhookEffects::default());
        }
        if Self::settlement_fenced_by_subscription_mutation(tx, org, &invoice)? {
            return Err(PaymentError::Conflict(
                "a subscription mutation is in flight; retry the webhook".into(),
            ));
        }
        let provider_invoice_id =
            provider_invoice_id.or_else(|| invoice.provider_invoice_id.clone());
        let Some(provider_invoice_id) = provider_invoice_id else {
            return Ok(WebhookEffects::default());
        };
        let provider_status = object
            .and_then(|object| object.get("status"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("paid")
            .to_string();
        if matches!(
            provider_status.as_str(),
            "processing" | "pending" | "requires_action" | "open" | "draft"
        ) {
            return Ok(WebhookEffects::default());
        }
        let provider_total = object
            .and_then(|object| object.get("amount"))
            .and_then(serde_json::Value::as_i64)
            .or_else(|| {
                object
                    .and_then(|object| object.get("amount_paid"))
                    .and_then(serde_json::Value::as_i64)
            })
            .unwrap_or(invoice.total_minor);
        let provider_paid = object
            .and_then(|object| object.get("amount_paid"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(if provider_status == "paid" {
                provider_total
            } else {
                0
            });
        let provider_refunded = object
            .and_then(|object| object.get("amount_refunded"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let provider_currency = object
            .and_then(|object| object.get("currency"))
            .and_then(serde_json::Value::as_str)
            .map(|currency| currency.to_uppercase())
            .unwrap_or_else(|| invoice.currency.clone());
        let paid_at_ms = object
            .and_then(|object| object.pointer("/status_transitions/paid_at"))
            .and_then(serde_json::Value::as_i64)
            .map(|seconds| seconds.saturating_mul(1000))
            .or(Some(now));
        let reconciliation = if provider_currency != invoice.currency {
            InvoiceReconciliationState::CurrencyMismatch
        } else if provider_total != invoice.total_minor {
            InvoiceReconciliationState::AmountMismatch
        } else {
            InvoiceReconciliationState::Matched
        };
        let observation = ProviderInvoiceObservation {
            provider: self.provider.name().to_string(),
            provider_invoice_id,
            organization_id: org.to_string(),
            invoice_id: invoice.id.clone(),
            status: provider_status,
            amount_minor: provider_total,
            amount_paid_minor: provider_paid,
            amount_refunded_minor: provider_refunded,
            currency: provider_currency,
            paid_at_ms,
            observed_ms: now,
            source: ProviderObservationSource::Webhook,
            reconciliation,
        };
        invoice.reconciliation = reconciliation;
        if reconciliation.is_conflict() {
            return Ok(WebhookEffects {
                invoice: Some(invoice),
                provider_invoice: Some(observation),
                ..WebhookEffects::default()
            });
        }
        if !invoice.status.can_transition_to(InvoiceStatus::Paid) {
            return Ok(WebhookEffects {
                invoice: Some(invoice),
                provider_invoice: Some(observation),
                ..WebhookEffects::default()
            });
        }
        let already_paid = invoice.amount_paid_minor >= invoice.total_minor;
        let mut payment = None;
        if !already_paid {
            let paid = provider_paid.max(0).min(invoice.balance_minor());
            if paid > 0 {
                invoice.amount_paid_minor = checked_add(invoice.amount_paid_minor, paid)?;
                payment = match provider_payment_id.as_deref() {
                    Some(provider_payment_id) => {
                        match payment_by_provider_id_tx(tx, org, provider_payment_id)? {
                            Some(mut existing) => {
                                if existing.status.can_transition_to(PaymentStatus::Succeeded) {
                                    existing.status = PaymentStatus::Succeeded;
                                }
                                Some(existing)
                            }
                            None => Some(Payment {
                                id: format!("pay_{}", uuid::Uuid::new_v4()),
                                organization_id: org.to_string(),
                                invoice_id: invoice.id.clone(),
                                provider: self.provider.name().to_string(),
                                provider_payment_id: provider_payment_id.to_string(),
                                payment_method_id: None,
                                amount_minor: paid,
                                refunded_minor: 0,
                                currency: invoice.currency.clone(),
                                status: PaymentStatus::Succeeded,
                                created_ms: now,
                            }),
                        }
                    }
                    None => Some(Payment {
                        id: format!("pay_{}", uuid::Uuid::new_v4()),
                        organization_id: org.to_string(),
                        invoice_id: invoice.id.clone(),
                        provider: self.provider.name().to_string(),
                        provider_payment_id: format!("{}:payment", event.id),
                        payment_method_id: None,
                        amount_minor: paid,
                        refunded_minor: 0,
                        currency: invoice.currency.clone(),
                        status: PaymentStatus::Succeeded,
                        created_ms: now,
                    }),
                };
            }
            if invoice.amount_paid_minor >= invoice.total_minor {
                invoice.status = InvoiceStatus::Paid;
                invoice.paid_ms = Some(now);
            }
        }
        let provider_payment = payment.as_ref().map(|payment| ProviderPaymentObservation {
            provider: self.provider.name().to_string(),
            provider_payment_id: payment.provider_payment_id.clone(),
            organization_id: org.to_string(),
            invoice_id: invoice.id.clone(),
            payment_id: Some(payment.id.clone()),
            status: "succeeded".into(),
            amount_minor: payment.amount_minor,
            refunded_minor: 0,
            currency: payment.currency.clone(),
            paid_at_ms: Some(now),
            observed_ms: now,
            source: ProviderObservationSource::Webhook,
        });
        let subscription = match invoice.subscription_id.as_deref() {
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
            invoice: Some(invoice),
            payment,
            provider_invoice: Some(observation),
            provider_payment,
            ..WebhookEffects::default()
        })
    }
}
