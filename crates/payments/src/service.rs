//! The payment/invoice service: one durable authority over the local SQLite
//! store and exactly one [`PaymentProvider`]. It implements the subscription
//! lifecycle, exact invoice arithmetic, seats/proration, tax, payment
//! methods, bounded refunds and verified webhook ingestion. Every mutating
//! operation is request-bound-idempotent and commits its effect and its
//! idempotency record in the same transaction.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{Clock, OrganizationId};
use sha2::{Digest, Sha256};

use crate::catalog::{
    BillingInterval, DunningConfig, PaymentsConfig, PlanPrice, ProviderKind, TaxConfig, TaxRate,
};
use crate::error::{PaymentError, PaymentErrorBody};
use crate::model::{
    AddPaymentMethodRequest, CancelRequest, CapabilityStatus, ChangePlanRequest,
    ChangeSeatsRequest, CreditNote, Invoice, InvoiceLine, InvoiceLineKind, InvoiceOutcome,
    InvoicePage, InvoiceStatus, IssueInvoiceRequest, LifecycleOutcome, PayInvoiceOutcome,
    PayInvoiceRequest, Payment, PaymentCustomer, PaymentMethod, PaymentMethodOutcome,
    PaymentMethodView, PaymentStatus, PaymentSubscription, PaymentsOverview, PendingProration,
    PlanChangeOutcome, PreviewPlanRequest, PreviewSeatsRequest, ProrationPreview, ProviderStatus,
    Refund, RefundOutcome, RefundRequest, SeatChangeOutcome, SubscribeRequest, SubscriptionOutcome,
    SubscriptionState, TaxSurface, WebhookDisposition, WebhookOutcome,
};
use crate::money::{checked_add, checked_mul, exclusive_tax, format_minor, inclusive_tax, prorate};
use crate::provider::{PaymentProvider, VerifiedWebhookEvent, WebhookHeaders};
use crate::secrets::SecretSource;
use crate::store::{
    consume_proration_tx, insert_credit_note_tx, insert_invoice_tx, insert_payment_tx,
    insert_pending_proration_tx, insert_refund_tx, next_sequence_tx, put_customer_tx,
    put_payment_method_tx, put_subscription_tx, update_invoice_tx, IdempotencyWrite, InvoiceStats,
    SqlitePaymentStore, WebhookEffects, WebhookRecord,
};

/// Bound on one invoice page / export.
pub const MAX_INVOICE_PAGE: usize = 5_000;
/// Bound on usage lines one invoice accepts.
pub const MAX_USAGE_LINES: usize = 128;
/// Bound on one text field of a request.
pub const MAX_TEXT_BYTES: usize = 512;
/// Bound on the exported CSV/JSON body.
pub const MAX_EXPORT_BYTES: usize = 8 * 1024 * 1024;
/// Bound on seats a subscription may carry.
pub const MAX_SEATS: u32 = 1_000_000;
/// Bound on a trial length.
pub const MAX_TRIAL_DAYS: u32 = 365;
/// Bound on an invoice due window.
pub const MAX_DUE_DAYS: i64 = 365;

/// The payment service.
pub struct PaymentService {
    store: Arc<SqlitePaymentStore>,
    provider: Arc<dyn PaymentProvider>,
    config: PaymentsConfig,
    clock: Arc<dyn Clock>,
}

impl PaymentService {
    pub fn new(
        store: Arc<SqlitePaymentStore>,
        provider: Arc<dyn PaymentProvider>,
        config: PaymentsConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, PaymentError> {
        config.validate()?;
        Ok(Self {
            store,
            provider,
            config,
            clock,
        })
    }

    /// Construct the local provider. The webhook secret is optional (billing
    /// works offline without inbound webhooks, but verification then answers
    /// the typed unconfigured refusal).
    pub fn open_local(
        store: Arc<SqlitePaymentStore>,
        config: PaymentsConfig,
        clock: Arc<dyn Clock>,
        secrets: Arc<dyn SecretSource>,
    ) -> Result<Self, PaymentError> {
        let secret = secrets.get(&config.webhook_secret_env);
        let provider: Arc<dyn PaymentProvider> = Arc::new(crate::local::LocalPaymentProvider::new(
            store.clone(),
            secret,
            clock.clone(),
        ));
        Self::new(store, provider, config, clock)
    }

    /// Construct the Stripe adapter. A missing API key is NOT a construction
    /// failure: the service runs with [`crate::provider::UnconfiguredPaymentProvider`]
    /// so the overview can report the honest typed state and every paid
    /// operation refuses `payment_provider_unconfigured`.
    pub fn open_stripe(
        store: Arc<SqlitePaymentStore>,
        config: PaymentsConfig,
        clock: Arc<dyn Clock>,
        secrets: Arc<dyn SecretSource>,
        transport: Option<Arc<dyn faktor_provider::egress::HttpTransport>>,
    ) -> Result<Self, PaymentError> {
        let api_key = secrets.get(&config.stripe.api_key_env);
        let webhook_secret = secrets.get(&config.stripe.webhook_secret_env);
        let provider: Arc<dyn PaymentProvider> = match (api_key, transport) {
            (Some(api_key), Some(transport)) if !api_key.is_empty() => {
                Arc::new(crate::stripe::StripePaymentProvider::new(
                    &config.stripe,
                    api_key,
                    webhook_secret,
                    transport,
                ))
            }
            (Some(api_key), None) if !api_key.is_empty() => {
                Arc::new(crate::provider::UnconfiguredPaymentProvider::new(
                    "stripe",
                    "no checked egress transport is wired",
                ))
            }
            _ => Arc::new(crate::provider::UnconfiguredPaymentProvider::new(
                "stripe",
                format!(
                    "the environment variable {} does not resolve to an API key",
                    config.stripe.api_key_env
                ),
            )),
        };
        Self::new(store, provider, config, clock)
    }

    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    pub fn config(&self) -> &PaymentsConfig {
        &self.config
    }

    pub fn store(&self) -> &Arc<SqlitePaymentStore> {
        &self.store
    }

    pub fn provider_status(&self) -> ProviderStatus {
        self.provider.status()
    }

    pub fn provider_kind(&self) -> ProviderKind {
        self.config.provider
    }

    pub fn currency_exponent(&self, currency: &str) -> u32 {
        self.config.catalogue.exponent(currency)
    }

    pub fn format_money(&self, amount_minor: i64, currency: &str) -> String {
        format_minor(
            amount_minor,
            currency,
            self.config.catalogue.exponent(currency),
        )
    }

    // ------------------------------------------------------------- overview

    pub fn overview(
        &self,
        organization: &OrganizationId,
    ) -> Result<PaymentsOverview, PaymentError> {
        let org = organization.as_str();
        let customer = self.store.customer(org)?;
        let subscription = self.store.subscription(org)?;
        let stats = self.store.invoice_stats(org)?;
        let methods = self.store.payment_methods(org)?;
        let plan_price = subscription
            .as_ref()
            .and_then(|subscription| self.config.catalogue.plan(&subscription.plan_id).cloned());
        let currency = subscription
            .as_ref()
            .map(|sub| sub.currency.clone())
            .or_else(|| plan_price.as_ref().map(|price| price.currency.clone()));
        let exponent = currency
            .as_ref()
            .map(|currency| self.config.catalogue.exponent(currency));
        let status = self.provider.status();
        let payment = if status.state == "available" {
            CapabilityStatus::available()
        } else {
            CapabilityStatus::unavailable("payment_provider_unconfigured")
        };
        let tax = self.tax_surface();
        let tax_capability = if self.config.tax.is_configured() {
            CapabilityStatus::available()
        } else {
            CapabilityStatus::unavailable("tax_not_configured")
        };
        let currency_capability = if self.config.catalogue.plans.is_empty() {
            CapabilityStatus::unavailable("currency_not_configured")
        } else {
            CapabilityStatus::available()
        };
        Ok(PaymentsOverview {
            organization_id: org.to_string(),
            provider: status,
            customer,
            seats: subscription.as_ref().map(|sub| sub.quantity),
            subscription,
            plan_price,
            currency,
            currency_exponent: exponent,
            invoice_count: stats.invoice_count,
            open_invoice_count: stats.open_invoice_count,
            outstanding_minor: stats.outstanding_minor,
            paid_minor: stats.paid_minor,
            payment_method_count: methods.len() as u64,
            tax,
            payment,
            invoices: CapabilityStatus::available(),
            seats_capability: CapabilityStatus::available(),
            tax_capability,
            currency_capability,
            now_ms: self.now_ms(),
        })
    }

    pub fn tax_surface(&self) -> TaxSurface {
        if self.config.tax.is_configured() {
            TaxSurface {
                state: "available".into(),
                cause: None,
                default_jurisdiction: self.config.tax.default_jurisdiction.clone(),
                rates: self.config.tax.rates.clone(),
            }
        } else {
            TaxSurface {
                state: "unavailable".into(),
                cause: Some("tax_not_configured".into()),
                default_jurisdiction: self.config.tax.default_jurisdiction.clone(),
                rates: BTreeMap::new(),
            }
        }
    }

    // --------------------------------------------------------- subscriptions

    pub async fn subscribe(
        &self,
        organization: &OrganizationId,
        request: SubscribeRequest,
    ) -> Result<SubscriptionOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        if request.plan_id.is_empty() || request.plan_id.len() > 128 {
            return Err(PaymentError::Malformed(
                "plan_id must be 1..=128 bytes".into(),
            ));
        }
        if request.seats == 0 || request.seats > MAX_SEATS {
            return Err(PaymentError::Malformed(format!(
                "seats must be 1..={MAX_SEATS}"
            )));
        }
        if request.trial_days.is_some_and(|days| days > MAX_TRIAL_DAYS) {
            return Err(PaymentError::Malformed(format!(
                "trial_days must be 0..={MAX_TRIAL_DAYS}"
            )));
        }
        bounded_text("customer_email", &request.customer_email, MAX_TEXT_BYTES)?;
        bounded_text("customer_name", &request.customer_name, MAX_TEXT_BYTES)?;
        let plan = self
            .config
            .catalogue
            .plan(&request.plan_id)
            .ok_or_else(|| PaymentError::PlanPriceUnconfigured(request.plan_id.clone()))?;
        let price = plan.price(request.interval);
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) = self.replay("subscribe", org, &request.idempotency_key, &hash)? {
            let subscription = self.subscription_from_replay(org, &response)?;
            return Ok(SubscriptionOutcome {
                subscription,
                replayed: true,
            });
        }
        if let Some(existing) = self.store.subscription(org)? {
            if existing.is_live() {
                return Err(PaymentError::Conflict(
                    "the organization already has a live payment subscription".into(),
                ));
            }
        }
        let now = self.now_ms();
        let existing_customer = self.store.customer(org)?;
        let customer = match existing_customer {
            Some(customer) => customer,
            None => {
                let remote = self
                    .provider
                    .create_customer(crate::provider::ProviderCustomerRequest {
                        organization_id: org.to_string(),
                        idempotency_key: format!("{}:customer", request.idempotency_key),
                        email: request.customer_email.clone(),
                        name: request.customer_name.clone(),
                    })
                    .await?;
                PaymentCustomer {
                    id: format!("cus_{}", uuid::Uuid::new_v4()),
                    organization_id: org.to_string(),
                    provider: self.provider.name().to_string(),
                    provider_customer_id: remote.id,
                    email: remote.email,
                    name: remote.name,
                    created_ms: now,
                }
            }
        };
        let trial_end_ms = request
            .trial_days
            .filter(|days| *days > 0)
            .map(|days| now.saturating_add(i64::from(days) * 86_400_000));
        let remote = self
            .provider
            .subscribe(crate::provider::ProviderSubscribeRequest {
                organization_id: org.to_string(),
                idempotency_key: format!("{}:subscribe", request.idempotency_key),
                customer_id: customer.provider_customer_id.clone(),
                plan_id: request.plan_id.clone(),
                interval: request.interval,
                quantity: request.seats,
                trial_end_ms,
                currency: plan.currency.clone(),
                unit_amount_minor: price,
            })
            .await?;
        let period_start = if remote.current_period_start_ms > 0 {
            remote.current_period_start_ms
        } else {
            now
        };
        let period_end = if remote.current_period_end_ms > period_start {
            remote.current_period_end_ms
        } else {
            period_start.saturating_add(request.interval.period_ms())
        };
        let state = if trial_end_ms.is_some_and(|trial| trial > now) || remote.status == "trialing"
        {
            SubscriptionState::Trialing
        } else {
            SubscriptionState::Active
        };
        let subscription = PaymentSubscription {
            id: format!("sub_{}", uuid::Uuid::new_v4()),
            organization_id: org.to_string(),
            customer_id: customer.id.clone(),
            provider_subscription_id: remote.id,
            plan_id: request.plan_id.clone(),
            interval: request.interval,
            quantity: request.seats,
            state,
            currency: plan.currency.clone(),
            current_period_start_ms: period_start,
            current_period_end_ms: period_end,
            trial_end_ms,
            cancel_at_period_end: false,
            canceled_ms: None,
            dunning_attempts: 0,
            grace_end_ms: None,
            created_ms: now,
            updated_ms: now,
        };
        let write = self.write(
            org,
            &request.idempotency_key,
            "subscribe",
            hash.clone(),
            &serde_json::json!({"subscription_id": subscription.id}),
            now,
        )?;
        let cloned_subscription = subscription.clone();
        let cloned_customer = customer.clone();
        let is_new_customer = self.store.customer(org)?.is_none();
        match self.store.record_atomic(&write, |tx| {
            if is_new_customer {
                put_customer_tx(tx, &cloned_customer)?;
            }
            put_subscription_tx(tx, &cloned_subscription)
        }) {
            Ok(()) => Ok(SubscriptionOutcome {
                subscription,
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                if let Some(response) =
                    self.replay("subscribe", org, &request.idempotency_key, &hash)?
                {
                    return Ok(SubscriptionOutcome {
                        subscription: self.subscription_from_replay(org, &response)?,
                        replayed: true,
                    });
                }
                Err(PaymentError::Conflict(
                    "subscription was recorded concurrently".into(),
                ))
            }
            Err(e) => Err(e),
        }
    }

    pub fn preview_plan_change(
        &self,
        organization: &OrganizationId,
        request: PreviewPlanRequest,
    ) -> Result<ProrationPreview, PaymentError> {
        let subscription =
            self.require_subscription(organization.as_str(), &request.subscription_id)?;
        let target_interval = request.interval.unwrap_or(subscription.interval);
        self.compute_plan_proration(
            &subscription,
            &request.plan_id,
            target_interval,
            self.now_ms(),
        )
    }

    pub async fn change_plan(
        &self,
        organization: &OrganizationId,
        request: ChangePlanRequest,
    ) -> Result<PlanChangeOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) = self.replay("change_plan", org, &request.idempotency_key, &hash)? {
            return self.plan_change_from_replay(org, &response, true);
        }
        let subscription = self.require_subscription(org, &request.subscription_id)?;
        if !subscription.is_live() {
            return Err(PaymentError::IllegalState(
                subscription.state.as_str().into(),
            ));
        }
        let interval = request.interval.unwrap_or(subscription.interval);
        let proration =
            self.compute_plan_proration(&subscription, &request.plan_id, interval, self.now_ms())?;
        let unit_amount =
            self.plan_amount_in(&request.plan_id, interval, &subscription.currency)?;
        self.provider
            .change_plan(crate::provider::ProviderChangePlanRequest {
                organization_id: org.to_string(),
                idempotency_key: format!("{}:change-plan", request.idempotency_key),
                subscription_id: subscription.provider_subscription_id.clone(),
                plan_id: request.plan_id.clone(),
                interval,
                quantity: subscription.quantity,
                currency: subscription.currency.clone(),
                unit_amount_minor: unit_amount,
                prorate: true,
            })
            .await?;
        let mut updated = subscription.clone();
        updated.plan_id = request.plan_id.clone();
        updated.interval = interval;
        updated.updated_ms = self.now_ms();
        let now = self.now_ms();
        let pending = self.pending_from_preview(org, &updated.id, &proration, now);
        let write = self.write(
            org,
            &request.idempotency_key,
            "change_plan",
            hash.clone(),
            &serde_json::json!({
                "subscription_id": updated.id,
                "proration": proration,
            }),
            now,
        )?;
        let cloned = updated.clone();
        let pending_clone = pending.clone();
        match self.store.record_atomic(&write, |tx| {
            put_subscription_tx(tx, &cloned)?;
            for line in &pending_clone {
                insert_pending_proration_tx(tx, line)?;
            }
            Ok(())
        }) {
            Ok(()) => Ok(PlanChangeOutcome {
                subscription: updated,
                proration,
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("change_plan", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("plan change was recorded concurrently".into())
                    })?;
                self.plan_change_from_replay(org, &response, true)
            }
            Err(e) => Err(e),
        }
    }

    pub async fn cancel(
        &self,
        organization: &OrganizationId,
        request: CancelRequest,
    ) -> Result<SubscriptionOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) = self.replay("cancel", org, &request.idempotency_key, &hash)? {
            return Ok(SubscriptionOutcome {
                subscription: self.subscription_from_replay(org, &response)?,
                replayed: true,
            });
        }
        let subscription = self.require_subscription(org, &request.subscription_id)?;
        if subscription.state == SubscriptionState::Canceled {
            return Err(PaymentError::IllegalState("canceled".into()));
        }
        self.provider
            .cancel(crate::provider::ProviderCancelRequest {
                organization_id: org.to_string(),
                idempotency_key: format!("{}:cancel", request.idempotency_key),
                subscription_id: subscription.provider_subscription_id.clone(),
                at_period_end: request.at_period_end,
            })
            .await?;
        let now = self.now_ms();
        let mut updated = subscription;
        updated.updated_ms = now;
        if request.at_period_end {
            updated.cancel_at_period_end = true;
        } else {
            updated.state = SubscriptionState::Canceled;
            updated.canceled_ms = Some(now);
            updated.cancel_at_period_end = false;
        }
        let write = self.write(
            org,
            &request.idempotency_key,
            "cancel",
            hash.clone(),
            &serde_json::json!({"subscription_id": updated.id}),
            now,
        )?;
        let cloned = updated.clone();
        match self
            .store
            .record_atomic(&write, |tx| put_subscription_tx(tx, &cloned))
        {
            Ok(()) => Ok(SubscriptionOutcome {
                subscription: updated,
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("cancel", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("cancel was recorded concurrently".into())
                    })?;
                Ok(SubscriptionOutcome {
                    subscription: self.subscription_from_replay(org, &response)?,
                    replayed: true,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub fn preview_seat_change(
        &self,
        organization: &OrganizationId,
        request: PreviewSeatsRequest,
    ) -> Result<ProrationPreview, PaymentError> {
        let subscription =
            self.require_subscription(organization.as_str(), &request.subscription_id)?;
        self.compute_seat_proration(&subscription, request.quantity, self.now_ms())
    }

    pub async fn change_seats(
        &self,
        organization: &OrganizationId,
        request: ChangeSeatsRequest,
    ) -> Result<SeatChangeOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        if request.quantity == 0 || request.quantity > MAX_SEATS {
            return Err(PaymentError::Malformed(format!(
                "quantity must be 1..={MAX_SEATS}"
            )));
        }
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) = self.replay("change_seats", org, &request.idempotency_key, &hash)? {
            return self.seat_change_from_replay(org, &response, true);
        }
        let subscription = self.require_subscription(org, &request.subscription_id)?;
        if !subscription.is_live() {
            return Err(PaymentError::IllegalState(
                subscription.state.as_str().into(),
            ));
        }
        let proration =
            self.compute_seat_proration(&subscription, request.quantity, self.now_ms())?;
        self.provider
            .change_plan(crate::provider::ProviderChangePlanRequest {
                organization_id: org.to_string(),
                idempotency_key: format!("{}:seats", request.idempotency_key),
                subscription_id: subscription.provider_subscription_id.clone(),
                plan_id: subscription.plan_id.clone(),
                interval: subscription.interval,
                quantity: request.quantity,
                currency: subscription.currency.clone(),
                unit_amount_minor: self.plan_amount_in(
                    &subscription.plan_id,
                    subscription.interval,
                    &subscription.currency,
                )?,
                prorate: true,
            })
            .await?;
        let now = self.now_ms();
        let mut updated = subscription;
        updated.quantity = request.quantity;
        updated.updated_ms = now;
        let pending = self.pending_from_preview(org, &updated.id, &proration, now);
        let write = self.write(
            org,
            &request.idempotency_key,
            "change_seats",
            hash.clone(),
            &serde_json::json!({
                "subscription_id": updated.id,
                "proration": proration,
            }),
            now,
        )?;
        let cloned = updated.clone();
        let pending_clone = pending.clone();
        match self.store.record_atomic(&write, |tx| {
            put_subscription_tx(tx, &cloned)?;
            for line in &pending_clone {
                insert_pending_proration_tx(tx, line)?;
            }
            Ok(())
        }) {
            Ok(()) => Ok(SeatChangeOutcome {
                subscription: updated,
                proration,
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("change_seats", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("seat change was recorded concurrently".into())
                    })?;
                self.seat_change_from_replay(org, &response, true)
            }
            Err(e) => Err(e),
        }
    }

    // ------------------------------------------------------------- invoices

    pub async fn issue_invoice(
        &self,
        organization: &OrganizationId,
        request: IssueInvoiceRequest,
    ) -> Result<InvoiceOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        if request.usage.len() > MAX_USAGE_LINES {
            return Err(PaymentError::Malformed(format!(
                "an invoice accepts at most {MAX_USAGE_LINES} usage lines"
            )));
        }
        if request.credits_minor < 0 || request.discount_minor < 0 {
            return Err(PaymentError::Malformed(
                "credits_minor and discount_minor must be non-negative".into(),
            ));
        }
        for charge in &request.usage {
            bounded_text("usage description", &charge.description, MAX_TEXT_BYTES)?;
            if charge.quantity < 0 || charge.unit_amount_minor < 0 {
                return Err(PaymentError::Malformed(
                    "usage quantity and unit amount must be non-negative".into(),
                ));
            }
        }
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) =
            self.replay("issue_invoice", org, &request.idempotency_key, &hash)?
        {
            return Ok(InvoiceOutcome {
                invoice: self.invoice_from_replay(org, &response)?,
                replayed: true,
            });
        }
        let subscription = self.require_subscription(org, &request.subscription_id)?;
        if subscription.state == SubscriptionState::Canceled {
            return Err(PaymentError::IllegalState("canceled".into()));
        }
        let plan_amount = if subscription.state == SubscriptionState::Trialing {
            0
        } else {
            self.plan_amount_in(
                &subscription.plan_id,
                subscription.interval,
                &subscription.currency,
            )?
        };
        let seat_unit = if subscription.state == SubscriptionState::Trialing {
            0
        } else {
            self.seat_amount_in(
                &subscription.plan_id,
                subscription.interval,
                &subscription.currency,
            )?
        };
        let seat_amount = checked_mul(i64::from(subscription.quantity), seat_unit)?;
        let mut lines = Vec::new();
        if plan_amount != 0 {
            lines.push(InvoiceLine {
                kind: InvoiceLineKind::Plan,
                description: format!(
                    "{} plan ({})",
                    subscription.plan_id,
                    subscription.interval.as_str()
                ),
                quantity: 1,
                unit_amount_minor: plan_amount,
                amount_minor: plan_amount,
            });
        }
        if seat_amount != 0 {
            lines.push(InvoiceLine {
                kind: InvoiceLineKind::Seat,
                description: format!("{} seats", subscription.quantity),
                quantity: i64::from(subscription.quantity),
                unit_amount_minor: seat_unit,
                amount_minor: seat_amount,
            });
        }
        let pending = crate::store::pending_prorations(&self.store, org, &subscription.id)?;
        for proration in &pending {
            lines.push(InvoiceLine {
                kind: InvoiceLineKind::Proration,
                description: proration.description.clone(),
                quantity: 1,
                unit_amount_minor: proration.amount_minor,
                amount_minor: proration.amount_minor,
            });
        }
        for charge in &request.usage {
            let amount = checked_mul(charge.quantity, charge.unit_amount_minor)?;
            if amount != 0 {
                lines.push(InvoiceLine {
                    kind: InvoiceLineKind::Usage,
                    description: charge.description.clone(),
                    quantity: charge.quantity,
                    unit_amount_minor: charge.unit_amount_minor,
                    amount_minor: amount,
                });
            }
        }
        let mut subtotal = 0i64;
        for line in &lines {
            subtotal = checked_add(subtotal, line.amount_minor)?;
        }
        if subtotal < 0 {
            return Err(PaymentError::Malformed(
                "invoice subtotal (including proration credits) is negative".into(),
            ));
        }
        let reductions = checked_add(request.credits_minor, request.discount_minor)?;
        if reductions > subtotal {
            return Err(PaymentError::Malformed(
                "credits plus discounts exceed the invoice subtotal".into(),
            ));
        }
        if request.discount_minor > 0 {
            lines.push(InvoiceLine {
                kind: InvoiceLineKind::Discount,
                description: "discount".into(),
                quantity: 1,
                unit_amount_minor: -request.discount_minor,
                amount_minor: -request.discount_minor,
            });
        }
        if request.credits_minor > 0 {
            lines.push(InvoiceLine {
                kind: InvoiceLineKind::Credits,
                description: "account credits".into(),
                quantity: 1,
                unit_amount_minor: -request.credits_minor,
                amount_minor: -request.credits_minor,
            });
        }
        let taxable = subtotal - reductions;
        let tax_rate = self
            .config
            .tax
            .rate_for(request.tax_jurisdiction.as_deref())?;
        let (tax_minor, total_minor, tax_inclusive) = match tax_rate {
            Some(rate) if rate.inclusive => (inclusive_tax(taxable, rate.rate_bps)?, taxable, true),
            Some(rate) => (
                exclusive_tax(taxable, rate.rate_bps)?,
                checked_add(taxable, exclusive_tax(taxable, rate.rate_bps)?)?,
                false,
            ),
            None => (0, taxable, false),
        };
        if tax_minor != 0 {
            lines.push(InvoiceLine {
                kind: InvoiceLineKind::Tax,
                description: format!(
                    "tax {}",
                    request.tax_jurisdiction.as_deref().unwrap_or("default")
                ),
                quantity: 1,
                unit_amount_minor: tax_minor,
                amount_minor: tax_minor,
            });
        }
        if total_minor < 0 {
            return Err(PaymentError::Malformed("invoice total is negative".into()));
        }
        let customer = self
            .store
            .customer(org)?
            .ok_or_else(|| PaymentError::NotFound("payment customer".into()))?;
        let now = self.now_ms();
        let due_days = request.due_in_days.unwrap_or(7).clamp(1, MAX_DUE_DAYS);
        let provider_invoice = self
            .provider
            .create_invoice(crate::provider::ProviderInvoiceRequest {
                organization_id: org.to_string(),
                idempotency_key: format!("{}:invoice", request.idempotency_key),
                customer_id: customer.provider_customer_id.clone(),
                subscription_id: Some(subscription.provider_subscription_id.clone()),
                description: format!("{} invoice", subscription.plan_id),
                amount_minor: total_minor,
                currency: subscription.currency.clone(),
            })
            .await?;
        let invoice_id = format!("inv_{}", uuid::Uuid::new_v4());
        let write = self.write(
            org,
            &request.idempotency_key,
            "issue_invoice",
            hash.clone(),
            &serde_json::json!({"invoice_id": invoice_id}),
            now,
        )?;
        let mut invoice = Invoice {
            id: invoice_id.clone(),
            organization_id: org.to_string(),
            number: String::new(),
            subscription_id: Some(subscription.id.clone()),
            provider_invoice_id: Some(provider_invoice.id),
            currency: subscription.currency.clone(),
            status: InvoiceStatus::Open,
            lines: lines.clone(),
            subtotal_minor: subtotal,
            discount_minor: request.discount_minor,
            credits_minor: request.credits_minor,
            tax_minor,
            tax_jurisdiction: request
                .tax_jurisdiction
                .clone()
                .or_else(|| self.config.tax.default_jurisdiction.clone()),
            tax_inclusive,
            total_minor,
            amount_paid_minor: 0,
            amount_refunded_minor: 0,
            period_start_ms: Some(subscription.current_period_start_ms),
            period_end_ms: Some(subscription.current_period_end_ms),
            issued_ms: now,
            due_ms: now.saturating_add(due_days * 86_400_000),
            paid_ms: None,
            voided_ms: None,
            created_ms: now,
        };
        let pending_ids: Vec<String> = pending.iter().map(|line| line.id.clone()).collect();
        let cloned_lines = lines.clone();
        let effect_value = self.store.record_atomic(&write, |tx| {
            let sequence = next_sequence_tx(tx, org, "invoice")?;
            invoice.number = format!("INV-{sequence:06}");
            insert_invoice_tx(tx, &invoice, &cloned_lines)?;
            for pending_id in &pending_ids {
                consume_proration_tx(tx, pending_id, &invoice_id)?;
            }
            Ok(invoice.clone())
        });
        match effect_value {
            Ok(recorded) => Ok(InvoiceOutcome {
                invoice: recorded,
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("issue_invoice", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("invoice was recorded concurrently".into())
                    })?;
                Ok(InvoiceOutcome {
                    invoice: self.invoice_from_replay(org, &response)?,
                    replayed: true,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub fn invoices(
        &self,
        organization: &OrganizationId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<InvoicePage, PaymentError> {
        if limit == 0 || limit > MAX_INVOICE_PAGE {
            return Err(PaymentError::Malformed(format!(
                "limit must be 1..={MAX_INVOICE_PAGE}"
            )));
        }
        self.store.invoices(organization.as_str(), cursor, limit)
    }

    pub fn invoice(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Invoice, PaymentError> {
        self.store
            .invoice(organization.as_str(), id)?
            .ok_or_else(|| PaymentError::NotFound("invoice".into()))
    }

    pub fn payments(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<Vec<Payment>, PaymentError> {
        if limit == 0 || limit > MAX_INVOICE_PAGE {
            return Err(PaymentError::Malformed(format!(
                "limit must be 1..={MAX_INVOICE_PAGE}"
            )));
        }
        self.store.payments(organization.as_str(), limit)
    }

    pub async fn pay_invoice(
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
        let mut invoice = self.invoice(organization, &request.invoice_id)?;
        if !invoice.status.is_open() {
            return Err(PaymentError::IllegalState(invoice.status.as_str().into()));
        }
        let balance = invoice.balance_minor();
        if balance <= 0 {
            return Err(PaymentError::NothingDue);
        }
        let method = self
            .store
            .payment_method(org, &request.payment_method_id)?
            .ok_or_else(|| PaymentError::NotFound("payment method".into()))?;
        if method.organization_id != org {
            return Err(PaymentError::NotFound("payment method".into()));
        }
        if method.provider != self.provider.name() {
            return Err(PaymentError::Malformed(
                "the payment method belongs to a different payment provider".into(),
            ));
        }
        let charge = match self
            .provider
            .charge(crate::provider::ProviderChargeRequest {
                organization_id: org.to_string(),
                idempotency_key: format!("{}:charge", request.idempotency_key),
                customer_id: self
                    .store
                    .customer(org)?
                    .ok_or_else(|| PaymentError::NotFound("payment customer".into()))?
                    .provider_customer_id,
                payment_method_token: crate::secrets::SecretString::new(method.token.clone()),
                amount_minor: balance,
                currency: invoice.currency.clone(),
                invoice_ref: Some(invoice.id.clone()),
            })
            .await
        {
            Ok(charge) => charge,
            Err(PaymentError::Declined(reason)) => {
                let _ = self.record_payment_failure(organization, &invoice.id);
                return Err(PaymentError::Declined(reason));
            }
            Err(e) => return Err(e),
        };
        let now = self.now_ms();
        let payment = Payment {
            id: format!("pay_{}", uuid::Uuid::new_v4()),
            organization_id: org.to_string(),
            invoice_id: invoice.id.clone(),
            provider: self.provider.name().to_string(),
            provider_payment_id: charge.id,
            payment_method_id: Some(request.payment_method_id.clone()),
            amount_minor: balance,
            refunded_minor: 0,
            currency: invoice.currency.clone(),
            status: PaymentStatus::Succeeded,
            created_ms: now,
        };
        invoice.amount_paid_minor = checked_add(invoice.amount_paid_minor, balance)?;
        invoice.status = InvoiceStatus::Paid;
        invoice.paid_ms = Some(now);
        let updated_invoice = invoice.clone();
        let mut subscription = self
            .store
            .subscription(org)?
            .filter(|sub| updated_invoice.subscription_id.as_deref() == Some(sub.id.as_str()));
        if let Some(sub) = subscription.as_mut() {
            sub.state = SubscriptionState::Active;
            sub.dunning_attempts = 0;
            sub.grace_end_ms = None;
            sub.updated_ms = now;
        }
        let write = self.write(
            org,
            &request.idempotency_key,
            "pay_invoice",
            hash.clone(),
            &serde_json::json!({
                "payment_id": payment.id,
                "invoice_id": updated_invoice.id,
            }),
            now,
        )?;
        let cloned_payment = payment.clone();
        let cloned_invoice = updated_invoice.clone();
        let cloned_subscription = subscription.clone();
        match self.store.record_atomic(&write, |tx| {
            insert_payment_tx(tx, &cloned_payment)?;
            update_invoice_tx(tx, &cloned_invoice)?;
            if let Some(subscription) = &cloned_subscription {
                put_subscription_tx(tx, subscription)?;
            }
            Ok(())
        }) {
            Ok(()) => Ok(PayInvoiceOutcome {
                payment,
                invoice: updated_invoice,
                subscription,
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("pay_invoice", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("payment was recorded concurrently".into())
                    })?;
                self.pay_from_replay(org, &response, true)
            }
            Err(e) => Err(e),
        }
    }

    pub fn record_payment_failure(
        &self,
        organization: &OrganizationId,
        invoice_id: &str,
    ) -> Result<Option<PaymentSubscription>, PaymentError> {
        let org = organization.as_str();
        let mut invoice = self
            .store
            .invoice(org, invoice_id)?
            .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
        invoice.status = InvoiceStatus::PastDue;
        let now = self.now_ms();
        let mut subscription = invoice
            .subscription_id
            .as_deref()
            .and_then(|id| self.store.subscription_by_id(org, id).ok().flatten());
        if let Some(sub) = subscription.as_mut() {
            sub.dunning_attempts = sub.dunning_attempts.saturating_add(1);
            sub.updated_ms = now;
            sub.state = SubscriptionState::PastDue;
            let grace = self.config.dunning.grace_ms.max(0);
            if sub.grace_end_ms.is_none() {
                sub.grace_end_ms = Some(now.saturating_add(grace));
            }
            if sub.dunning_attempts >= self.config.dunning.max_attempts {
                sub.state = SubscriptionState::Canceled;
                sub.canceled_ms = Some(now);
                sub.cancel_at_period_end = false;
            }
        }
        self.store.record_atomic(
            &IdempotencyWrite {
                organization_id: org.to_string(),
                key: format!("dunning:{}", uuid::Uuid::new_v4()),
                operation: "payment_failure".into(),
                request_hash: invoice_id.to_string(),
                response: "{}".into(),
                created_ms: now,
            },
            |tx| {
                update_invoice_tx(tx, &invoice)?;
                if let Some(subscription) = &subscription {
                    put_subscription_tx(tx, subscription)?;
                }
                Ok(())
            },
        )?;
        Ok(subscription)
    }

    pub async fn refund(
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
        bounded_text("refund reason", &request.reason, MAX_TEXT_BYTES)?;
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) = self.replay("refund", org, &request.idempotency_key, &hash)? {
            return self.refund_from_replay(org, &response, true);
        }
        let mut payment = self
            .store
            .payment(org, &request.payment_id)?
            .ok_or_else(|| PaymentError::NotFound("payment".into()))?;
        let refunded = self
            .store
            .refunded_for_payment(org, &payment.id)?
            .max(payment.refunded_minor);
        let available = payment.amount_minor - refunded;
        if request.amount_minor > available {
            return Err(PaymentError::RefundExceedsPaid {
                paid: payment.amount_minor,
                refunded,
                requested: request.amount_minor,
            });
        }
        let remote = self
            .provider
            .refund(crate::provider::ProviderRefundRequest {
                organization_id: org.to_string(),
                idempotency_key: format!("{}:refund", request.idempotency_key),
                provider_payment_id: payment.provider_payment_id.clone(),
                amount_minor: request.amount_minor,
                currency: payment.currency.clone(),
                reason: request.reason.clone(),
            })
            .await?;
        let now = self.now_ms();
        let mut invoice = self
            .store
            .invoice(org, &payment.invoice_id)?
            .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
        let refund = Refund {
            id: format!("re_{}", uuid::Uuid::new_v4()),
            organization_id: org.to_string(),
            payment_id: payment.id.clone(),
            provider_refund_id: remote.id,
            amount_minor: request.amount_minor,
            currency: payment.currency.clone(),
            reason: request.reason.clone(),
            created_ms: now,
        };
        payment.refunded_minor = checked_add(payment.refunded_minor, request.amount_minor)?;
        payment.status = if payment.refunded_minor >= payment.amount_minor {
            PaymentStatus::Refunded
        } else {
            PaymentStatus::PartiallyRefunded
        };
        invoice.amount_refunded_minor =
            checked_add(invoice.amount_refunded_minor, request.amount_minor)?;
        let sequence = self.store.next_sequence(org, "credit_note")?;
        let credit_note = CreditNote {
            id: format!("cn_{}", uuid::Uuid::new_v4()),
            organization_id: org.to_string(),
            number: format!("CN-{sequence:06}"),
            invoice_id: invoice.id.clone(),
            payment_id: payment.id.clone(),
            amount_minor: request.amount_minor,
            currency: payment.currency.clone(),
            reason: request.reason.clone(),
            created_ms: now,
        };
        let write = self.write(
            org,
            &request.idempotency_key,
            "refund",
            hash.clone(),
            &serde_json::json!({"refund_id": refund.id, "credit_note_id": credit_note.id}),
            now,
        )?;
        let cloned_refund = refund.clone();
        let cloned_note = credit_note.clone();
        let cloned_payment = payment.clone();
        let cloned_invoice = invoice.clone();
        match self.store.record_atomic(&write, |tx| {
            insert_refund_tx(tx, &cloned_refund)?;
            insert_credit_note_tx(tx, &cloned_note)?;
            insert_payment_tx(tx, &cloned_payment)?;
            update_invoice_tx(tx, &cloned_invoice)
        }) {
            Ok(()) => Ok(RefundOutcome {
                refund,
                credit_note,
                payment,
                invoice,
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("refund", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("refund was recorded concurrently".into())
                    })?;
                self.refund_from_replay(org, &response, true)
            }
            Err(e) => Err(e),
        }
    }

    pub fn add_payment_method(
        &self,
        organization: &OrganizationId,
        request: AddPaymentMethodRequest,
    ) -> Result<PaymentMethodOutcome, PaymentError> {
        self.validate_idempotency(&request.idempotency_key)?;
        validate_payment_token(&request.token)?;
        bounded_text("payment brand", &request.brand, 64)?;
        if request.last4.len() != 4 || !request.last4.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PaymentError::Malformed(
                "last4 must be exactly four digits".into(),
            ));
        }
        if !(1..=12).contains(&request.exp_month) {
            return Err(PaymentError::Malformed("exp_month must be 1..=12".into()));
        }
        if !(2000..=2100).contains(&request.exp_year) {
            return Err(PaymentError::Malformed(
                "exp_year must be 2000..=2100".into(),
            ));
        }
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) =
            self.replay("add_payment_method", org, &request.idempotency_key, &hash)?
        {
            let method = self.method_from_replay(org, &response)?;
            return Ok(PaymentMethodOutcome {
                payment_method: method,
                replayed: true,
            });
        }
        let now = self.now_ms();
        let existing = self.store.payment_methods(org)?;
        let is_default = request.set_default || existing.is_empty();
        let method = PaymentMethod {
            id: format!("pm_{}", uuid::Uuid::new_v4()),
            organization_id: org.to_string(),
            provider: self.provider.name().to_string(),
            token: request.token,
            brand: request.brand,
            last4: request.last4,
            exp_month: request.exp_month,
            exp_year: request.exp_year,
            is_default,
            created_ms: now,
        };
        let write = self.write(
            org,
            &request.idempotency_key,
            "add_payment_method",
            hash.clone(),
            &serde_json::json!({"payment_method_id": method.id}),
            now,
        )?;
        let cloned = method.clone();
        match self
            .store
            .record_atomic(&write, |tx| put_payment_method_tx(tx, &cloned))
        {
            Ok(()) => Ok(PaymentMethodOutcome {
                payment_method: PaymentMethodView::from(&method),
                replayed: false,
            }),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response =
                    self.replay("add_payment_method", org, &request.idempotency_key, &hash)?;
                let method = response
                    .ok_or_else(|| {
                        PaymentError::Conflict("payment method was recorded concurrently".into())
                    })
                    .and_then(|response| self.method_from_replay(org, &response))?;
                Ok(PaymentMethodOutcome {
                    payment_method: method,
                    replayed: true,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub fn payment_methods(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<PaymentMethodView>, PaymentError> {
        Ok(self
            .store
            .payment_methods(organization.as_str())?
            .iter()
            .map(PaymentMethodView::from)
            .collect())
    }

    // ------------------------------------------------------------ lifecycle

    pub fn tick(&self, organization: &OrganizationId) -> Result<LifecycleOutcome, PaymentError> {
        let org = organization.as_str();
        let Some(mut subscription) = self.store.subscription(org)? else {
            return Err(PaymentError::NotFound("payment subscription".into()));
        };
        let now = self.now_ms();
        let mut changed = false;
        if subscription.state == SubscriptionState::Trialing
            && subscription.trial_end_ms.is_some_and(|trial| trial <= now)
        {
            if subscription.cancel_at_period_end {
                subscription.state = SubscriptionState::Canceled;
                subscription.canceled_ms = Some(now);
                subscription.cancel_at_period_end = false;
            } else {
                subscription.state = SubscriptionState::Active;
                let anchor = subscription.trial_end_ms.unwrap_or(now);
                subscription.current_period_start_ms = anchor;
                subscription.current_period_end_ms =
                    anchor.saturating_add(subscription.interval.period_ms());
            }
            subscription.updated_ms = now;
            changed = true;
        }
        if matches!(
            subscription.state,
            SubscriptionState::Active | SubscriptionState::PastDue
        ) && now >= subscription.current_period_end_ms
        {
            if subscription.cancel_at_period_end {
                subscription.state = SubscriptionState::Canceled;
                subscription.canceled_ms = Some(now);
                subscription.cancel_at_period_end = false;
                changed = true;
            } else {
                subscription.current_period_start_ms = subscription.current_period_end_ms;
                subscription.current_period_end_ms = subscription
                    .current_period_end_ms
                    .saturating_add(subscription.interval.period_ms());
                subscription.updated_ms = now;
                changed = true;
            }
        }
        if subscription.state == SubscriptionState::PastDue {
            let expired = subscription.grace_end_ms.is_some_and(|grace| grace <= now);
            let exhausted = subscription.dunning_attempts >= self.config.dunning.max_attempts;
            if expired || exhausted {
                subscription.state = SubscriptionState::Canceled;
                subscription.canceled_ms = Some(now);
                subscription.cancel_at_period_end = false;
                changed = true;
            }
        }
        if changed {
            self.store.put_subscription(&subscription)?;
        }
        Ok(LifecycleOutcome {
            subscription,
            changed,
        })
    }

    // -------------------------------------------------------------- webhook

    pub fn handle_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<WebhookOutcome, PaymentError> {
        let event = self.provider.verify_webhook(headers, body, now_ms)?;
        let provider = self.provider.name();
        if self.store.webhook_seen(provider, &event.id)?.is_some() {
            return Ok(WebhookOutcome {
                disposition: WebhookDisposition::Duplicate,
                event_id: event.id,
                event_type: event.event_type,
            });
        }
        let organization = self.webhook_organization(&event)?;
        let org = organization.as_str();
        let digest = sha256_hex(body);
        if let Some((last_created, last_id)) = self.store.webhook_watermark(org)? {
            if event.created_ms < last_created
                || (event.created_ms == last_created && event.id.as_str() <= last_id.as_str())
            {
                self.store.record_webhook_only(&WebhookRecord {
                    provider: provider.to_string(),
                    event_id: event.id.clone(),
                    organization_id: org.to_string(),
                    created_ms: event.created_ms,
                    event_type: event.event_type.clone(),
                    disposition: "stale_ignored".into(),
                    payload_digest: digest,
                })?;
                return Ok(WebhookOutcome {
                    disposition: WebhookDisposition::StaleIgnored,
                    event_id: event.id,
                    event_type: event.event_type,
                });
            }
        }
        let effects = self.webhook_effects(org, &event, now_ms)?;
        let applied = effects.subscription.is_some()
            || effects.invoice.is_some()
            || effects.payment.is_some();
        let disposition = if applied {
            WebhookDisposition::Applied
        } else {
            WebhookDisposition::Ignored
        };
        self.store.record_webhook(
            &WebhookRecord {
                provider: provider.to_string(),
                event_id: event.id.clone(),
                organization_id: org.to_string(),
                created_ms: event.created_ms,
                event_type: event.event_type.clone(),
                disposition: disposition_string(disposition).to_string(),
                payload_digest: digest,
            },
            &effects,
            true,
        )?;
        Ok(WebhookOutcome {
            disposition,
            event_id: event.id,
            event_type: event.event_type,
        })
    }

    // --------------------------------------------------------------- export

    pub fn export_invoices(
        &self,
        organization: &OrganizationId,
        format: &str,
        limit: usize,
    ) -> Result<String, PaymentError> {
        if format != "json" && format != "csv" {
            return Err(PaymentError::Malformed("format must be json or csv".into()));
        }
        let page = self.invoices(organization, None, limit)?;
        let text = if format == "json" {
            serde_json::to_string_pretty(&page.items)
                .map_err(|e| PaymentError::Store(format!("invoice export: {e}")))?
        } else {
            invoices_csv(&page.items)
        };
        if text.len() > MAX_EXPORT_BYTES {
            return Err(PaymentError::Malformed(format!(
                "invoice export exceeds the {MAX_EXPORT_BYTES} byte bound"
            )));
        }
        Ok(text)
    }

    pub fn export_invoice_csv(&self, invoice: &Invoice) -> String {
        invoice_csv(invoice)
    }

    // --------------------------------------------------------------- helpers

    fn require_subscription(
        &self,
        organization_id: &str,
        id: &str,
    ) -> Result<PaymentSubscription, PaymentError> {
        self.store
            .subscription_by_id(organization_id, id)?
            .ok_or_else(|| PaymentError::NotFound("subscription".into()))
    }

    fn compute_plan_proration(
        &self,
        subscription: &PaymentSubscription,
        new_plan: &str,
        new_interval: BillingInterval,
        now: i64,
    ) -> Result<ProrationPreview, PaymentError> {
        let old_amount = self.plan_amount_in(
            &subscription.plan_id,
            subscription.interval,
            &subscription.currency,
        )?;
        let new_amount = self.plan_amount_in(new_plan, new_interval, &subscription.currency)?;
        let remaining = subscription.remaining_ms(now);
        let period = subscription.period_ms();
        let credit = -prorate(old_amount, remaining, period)?;
        let charge = prorate(new_amount, remaining, period)?;
        Ok(ProrationPreview {
            kind: "plan_change".into(),
            subscription_id: subscription.id.clone(),
            from_plan: subscription.plan_id.clone(),
            to_plan: new_plan.to_string(),
            previous_quantity: None,
            new_quantity: None,
            old_amount_minor: old_amount,
            new_amount_minor: new_amount,
            credit_minor: credit,
            charge_minor: charge,
            net_minor: checked_add(credit, charge)?,
            currency: subscription.currency.clone(),
            remaining_ms: remaining,
            period_ms: period,
            effective_ms: now,
        })
    }

    fn compute_seat_proration(
        &self,
        subscription: &PaymentSubscription,
        new_quantity: u32,
        now: i64,
    ) -> Result<ProrationPreview, PaymentError> {
        if new_quantity == 0 || new_quantity > MAX_SEATS {
            return Err(PaymentError::Malformed(format!(
                "quantity must be 1..={MAX_SEATS}"
            )));
        }
        let seat_price = self.seat_amount_in(
            &subscription.plan_id,
            subscription.interval,
            &subscription.currency,
        )?;
        let old_amount = checked_mul(i64::from(subscription.quantity), seat_price)?;
        let new_amount = checked_mul(i64::from(new_quantity), seat_price)?;
        let delta = new_amount - old_amount;
        let remaining = subscription.remaining_ms(now);
        let period = subscription.period_ms();
        let amount = prorate(delta, remaining, period)?;
        let (credit, charge) = if amount < 0 { (amount, 0) } else { (0, amount) };
        Ok(ProrationPreview {
            kind: "seat_change".into(),
            subscription_id: subscription.id.clone(),
            from_plan: subscription.plan_id.clone(),
            to_plan: subscription.plan_id.clone(),
            previous_quantity: Some(subscription.quantity),
            new_quantity: Some(new_quantity),
            old_amount_minor: old_amount,
            new_amount_minor: new_amount,
            credit_minor: credit,
            charge_minor: charge,
            net_minor: amount,
            currency: subscription.currency.clone(),
            remaining_ms: remaining,
            period_ms: period,
            effective_ms: now,
        })
    }

    fn plan_amount_in(
        &self,
        plan_id: &str,
        interval: BillingInterval,
        target: &str,
    ) -> Result<i64, PaymentError> {
        let plan = self
            .config
            .catalogue
            .plan(plan_id)
            .ok_or_else(|| PaymentError::PlanPriceUnconfigured(plan_id.to_string()))?;
        self.config
            .catalogue
            .convert(plan.price(interval), &plan.currency, target)
    }

    fn seat_amount_in(
        &self,
        plan_id: &str,
        interval: BillingInterval,
        target: &str,
    ) -> Result<i64, PaymentError> {
        let plan = self
            .config
            .catalogue
            .plan(plan_id)
            .ok_or_else(|| PaymentError::PlanPriceUnconfigured(plan_id.to_string()))?;
        self.config
            .catalogue
            .convert(plan.seat_price(interval), &plan.currency, target)
    }

    fn pending_from_preview(
        &self,
        organization_id: &str,
        subscription_id: &str,
        preview: &ProrationPreview,
        now: i64,
    ) -> Vec<PendingProration> {
        let mut lines = Vec::new();
        if preview.credit_minor != 0 {
            lines.push(PendingProration {
                id: format!("pr_{}", uuid::Uuid::new_v4()),
                organization_id: organization_id.to_string(),
                subscription_id: subscription_id.to_string(),
                description: format!("unused {} time", preview.from_plan),
                amount_minor: preview.credit_minor,
                currency: preview.currency.clone(),
                created_ms: now,
                consumed_by: None,
            });
        }
        if preview.charge_minor != 0 {
            lines.push(PendingProration {
                id: format!("pr_{}", uuid::Uuid::new_v4()),
                organization_id: organization_id.to_string(),
                subscription_id: subscription_id.to_string(),
                description: format!("prorated {} time", preview.to_plan),
                amount_minor: preview.charge_minor,
                currency: preview.currency.clone(),
                created_ms: now,
                consumed_by: None,
            });
        }
        lines
    }

    fn validate_idempotency(&self, key: &str) -> Result<(), PaymentError> {
        if key.is_empty() || key.len() > 200 {
            return Err(PaymentError::Malformed(
                "an idempotency key of 1..=200 bytes is required".into(),
            ));
        }
        Ok(())
    }

    fn replay(
        &self,
        operation: &str,
        organization_id: &str,
        key: &str,
        request_hash: &str,
    ) -> Result<Option<String>, PaymentError> {
        let Some(record) = self.store.read_idempotency(organization_id, key)? else {
            return Ok(None);
        };
        if record.operation != operation || record.request_hash != request_hash {
            return Err(PaymentError::IdempotencyConflict(
                "the key was used for a different request".into(),
            ));
        }
        record.response.map(Some).ok_or_else(|| {
            PaymentError::Conflict("an unfinished idempotent operation exists".into())
        })
    }

    fn write(
        &self,
        organization_id: &str,
        key: &str,
        operation: &str,
        request_hash: String,
        response: &serde_json::Value,
        now: i64,
    ) -> Result<IdempotencyWrite, PaymentError> {
        Ok(IdempotencyWrite {
            organization_id: organization_id.to_string(),
            key: key.to_string(),
            operation: operation.to_string(),
            request_hash,
            response: serde_json::to_string(response)
                .map_err(|e| PaymentError::Store(format!("idempotency response: {e}")))?,
            created_ms: now,
        })
    }

    fn subscription_from_replay(
        &self,
        org: &str,
        response: &str,
    ) -> Result<PaymentSubscription, PaymentError> {
        let value: serde_json::Value = serde_json::from_str(response)
            .map_err(|e| PaymentError::Store(format!("subscription replay: {e}")))?;
        let id = value
            .get("subscription_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| PaymentError::Store("subscription replay has no id".into()))?;
        self.require_subscription(org, id)
    }

    fn invoice_from_replay(&self, org: &str, response: &str) -> Result<Invoice, PaymentError> {
        let value: serde_json::Value = serde_json::from_str(response)
            .map_err(|e| PaymentError::Store(format!("invoice replay: {e}")))?;
        let id = value
            .get("invoice_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| PaymentError::Store("invoice replay has no id".into()))?;
        self.store
            .invoice(org, id)?
            .ok_or_else(|| PaymentError::NotFound("invoice".into()))
    }

    fn plan_change_from_replay(
        &self,
        org: &str,
        response: &str,
        replayed: bool,
    ) -> Result<PlanChangeOutcome, PaymentError> {
        let value: serde_json::Value = serde_json::from_str(response)
            .map_err(|e| PaymentError::Store(format!("plan change replay: {e}")))?;
        let subscription = self.subscription_from_replay(org, response)?;
        let proration: ProrationPreview =
            serde_json::from_value(value.get("proration").cloned().ok_or_else(|| {
                PaymentError::Store("plan change replay has no proration".into())
            })?)
            .map_err(|e| PaymentError::Store(format!("plan change replay proration: {e}")))?;
        Ok(PlanChangeOutcome {
            subscription,
            proration,
            replayed,
        })
    }

    fn seat_change_from_replay(
        &self,
        org: &str,
        response: &str,
        replayed: bool,
    ) -> Result<SeatChangeOutcome, PaymentError> {
        let value: serde_json::Value = serde_json::from_str(response)
            .map_err(|e| PaymentError::Store(format!("seat change replay: {e}")))?;
        let subscription = self.subscription_from_replay(org, response)?;
        let proration: ProrationPreview =
            serde_json::from_value(value.get("proration").cloned().ok_or_else(|| {
                PaymentError::Store("seat change replay has no proration".into())
            })?)
            .map_err(|e| PaymentError::Store(format!("seat change replay proration: {e}")))?;
        Ok(SeatChangeOutcome {
            subscription,
            proration,
            replayed,
        })
    }

    fn method_from_replay(
        &self,
        org: &str,
        response: &str,
    ) -> Result<PaymentMethodView, PaymentError> {
        let value: serde_json::Value = serde_json::from_str(response)
            .map_err(|e| PaymentError::Store(format!("method replay: {e}")))?;
        let id = value
            .get("payment_method_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| PaymentError::Store("method replay has no id".into()))?;
        let method = self
            .store
            .payment_method(org, id)?
            .ok_or_else(|| PaymentError::NotFound("payment method".into()))?;
        Ok(PaymentMethodView::from(&method))
    }

    fn pay_from_replay(
        &self,
        org: &str,
        response: &str,
        replayed: bool,
    ) -> Result<PayInvoiceOutcome, PaymentError> {
        let value: serde_json::Value = serde_json::from_str(response)
            .map_err(|e| PaymentError::Store(format!("payment replay: {e}")))?;
        let payment_id = value
            .get("payment_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| PaymentError::Store("payment replay has no payment id".into()))?;
        let invoice_id = value
            .get("invoice_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| PaymentError::Store("payment replay has no invoice id".into()))?;
        let payment = self
            .store
            .payment(org, payment_id)?
            .ok_or_else(|| PaymentError::NotFound("payment".into()))?;
        let invoice = self
            .store
            .invoice(org, invoice_id)?
            .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
        let subscription = self.store.subscription(org)?;
        Ok(PayInvoiceOutcome {
            payment,
            invoice,
            subscription,
            replayed,
        })
    }

    fn refund_from_replay(
        &self,
        org: &str,
        response: &str,
        replayed: bool,
    ) -> Result<RefundOutcome, PaymentError> {
        let value: serde_json::Value = serde_json::from_str(response)
            .map_err(|e| PaymentError::Store(format!("refund replay: {e}")))?;
        let refund_id = value
            .get("refund_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| PaymentError::Store("refund replay has no id".into()))?;
        let note_id = value
            .get("credit_note_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| PaymentError::Store("refund replay has no credit note".into()))?;
        let refund = self
            .store
            .refund_by_id(org, refund_id)?
            .ok_or_else(|| PaymentError::NotFound("refund".into()))?;
        let note = self
            .store
            .credit_note(org, note_id)?
            .ok_or_else(|| PaymentError::NotFound("credit note".into()))?;
        let payment = self
            .store
            .payment(org, &refund.payment_id)?
            .ok_or_else(|| PaymentError::NotFound("payment".into()))?;
        let invoice = self
            .store
            .invoice(org, &note.invoice_id)?
            .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
        Ok(RefundOutcome {
            refund,
            credit_note: note,
            payment,
            invoice,
            replayed,
        })
    }

    fn webhook_organization(
        &self,
        event: &VerifiedWebhookEvent,
    ) -> Result<OrganizationId, PaymentError> {
        let raw = if let Some(org) = &event.organization_id {
            Some(org.clone())
        } else if let Some(customer) = event
            .payload
            .pointer("/data/object/customer")
            .and_then(serde_json::Value::as_str)
        {
            self.store
                .customer_organization(self.provider.name(), customer)?
        } else if let Some(subscription) = event
            .payload
            .pointer("/data/object/subscription")
            .and_then(serde_json::Value::as_str)
        {
            self.store.subscription_organization(subscription)?
        } else {
            None
        };
        let raw = raw.ok_or_else(|| {
            PaymentError::WebhookMalformed("webhook event is not bound to an organization".into())
        })?;
        OrganizationId::try_new(raw).map_err(|e| PaymentError::WebhookMalformed(e.to_string()))
    }

    fn webhook_effects(
        &self,
        org: &str,
        event: &VerifiedWebhookEvent,
        now: i64,
    ) -> Result<WebhookEffects, PaymentError> {
        let object = event
            .payload
            .get("data")
            .and_then(|data| data.get("object"));
        match event.event_type.as_str() {
            "invoice.paid" => {
                let Some(object) = object else {
                    return Ok(WebhookEffects::default());
                };
                let provider_invoice_id = object.get("id").and_then(serde_json::Value::as_str);
                let local_id = object
                    .get("metadata")
                    .and_then(|metadata| metadata.get("local_invoice_id"))
                    .and_then(serde_json::Value::as_str);
                let mut invoice = if let Some(local_id) = local_id {
                    self.store.invoice(org, local_id)?
                } else if let Some(provider_invoice_id) = provider_invoice_id {
                    self.store.invoice_by_provider_id(provider_invoice_id)?
                } else {
                    None
                };
                let Some(invoice_ref) = invoice.as_mut() else {
                    return Ok(WebhookEffects::default());
                };
                if invoice_ref.organization_id != org {
                    return Ok(WebhookEffects::default());
                }
                let paid = object
                    .get("amount_paid")
                    .and_then(serde_json::Value::as_i64)
                    .or_else(|| object.get("amount").and_then(serde_json::Value::as_i64))
                    .unwrap_or(invoice_ref.balance_minor());
                let paid = paid.clamp(0, invoice_ref.balance_minor());
                let mut payment = None;
                if paid > 0 {
                    invoice_ref.amount_paid_minor =
                        checked_add(invoice_ref.amount_paid_minor, paid)?;
                    payment = Some(Payment {
                        id: format!("pay_{}", uuid::Uuid::new_v4()),
                        organization_id: org.to_string(),
                        invoice_id: invoice_ref.id.clone(),
                        provider: self.provider.name().to_string(),
                        provider_payment_id: format!("{}:payment", event.id),
                        payment_method_id: None,
                        amount_minor: paid,
                        refunded_minor: 0,
                        currency: invoice_ref.currency.clone(),
                        status: PaymentStatus::Succeeded,
                        created_ms: now,
                    });
                }
                if invoice_ref.amount_paid_minor >= invoice_ref.total_minor {
                    invoice_ref.status = InvoiceStatus::Paid;
                    invoice_ref.paid_ms = Some(now);
                }
                let subscription = invoice_ref
                    .subscription_id
                    .as_deref()
                    .and_then(|id| self.store.subscription_by_id(org, id).ok().flatten())
                    .map(|mut subscription| {
                        subscription.state = SubscriptionState::Active;
                        subscription.dunning_attempts = 0;
                        subscription.grace_end_ms = None;
                        subscription.updated_ms = now;
                        subscription
                    });
                Ok(WebhookEffects {
                    subscription,
                    invoice: Some(invoice_ref.clone()),
                    payment,
                })
            }
            "invoice.payment_failed" => {
                let Some(object) = object else {
                    return Ok(WebhookEffects::default());
                };
                let provider_invoice_id = object.get("id").and_then(serde_json::Value::as_str);
                let local_id = object
                    .get("metadata")
                    .and_then(|metadata| metadata.get("local_invoice_id"))
                    .and_then(serde_json::Value::as_str);
                let invoice = if let Some(local_id) = local_id {
                    self.store.invoice(org, local_id)?
                } else if let Some(provider_invoice_id) = provider_invoice_id {
                    self.store.invoice_by_provider_id(provider_invoice_id)?
                } else {
                    None
                };
                let Some(mut invoice) = invoice else {
                    return Ok(WebhookEffects::default());
                };
                if invoice.organization_id != org {
                    return Ok(WebhookEffects::default());
                }
                invoice.status = InvoiceStatus::PastDue;
                let subscription = invoice
                    .subscription_id
                    .as_deref()
                    .and_then(|id| self.store.subscription_by_id(org, id).ok().flatten())
                    .map(|mut subscription| {
                        subscription.dunning_attempts =
                            subscription.dunning_attempts.saturating_add(1);
                        subscription.state = SubscriptionState::PastDue;
                        if subscription.grace_end_ms.is_none() {
                            subscription.grace_end_ms =
                                Some(now.saturating_add(self.config.dunning.grace_ms.max(0)));
                        }
                        subscription.updated_ms = now;
                        if subscription.dunning_attempts >= self.config.dunning.max_attempts {
                            subscription.state = SubscriptionState::Canceled;
                            subscription.canceled_ms = Some(now);
                        }
                        subscription
                    });
                Ok(WebhookEffects {
                    subscription,
                    invoice: Some(invoice),
                    payment: None,
                })
            }
            "customer.subscription.updated" => {
                let Some(object) = object else {
                    return Ok(WebhookEffects::default());
                };
                let Some(provider_id) = object.get("id").and_then(serde_json::Value::as_str) else {
                    return Ok(WebhookEffects::default());
                };
                let Some(mut subscription) = self
                    .store
                    .subscription_organization(provider_id)?
                    .and_then(|org| self.store.subscription(&org).ok().flatten())
                else {
                    return Ok(WebhookEffects::default());
                };
                if subscription.organization_id != org {
                    return Ok(WebhookEffects::default());
                }
                if let Some(status) = object.get("status").and_then(serde_json::Value::as_str) {
                    if let Some(state) = SubscriptionState::parse(status) {
                        subscription.state = state;
                        if state == SubscriptionState::Canceled {
                            subscription.canceled_ms = Some(now);
                        }
                    }
                }
                if let Some(quantity) = object.get("quantity").and_then(serde_json::Value::as_u64) {
                    subscription.quantity = u32::try_from(quantity).map_err(|_| {
                        PaymentError::WebhookMalformed("subscription quantity overflow".into())
                    })?;
                }
                if let Some(cancel) = object
                    .get("cancel_at_period_end")
                    .and_then(serde_json::Value::as_bool)
                {
                    subscription.cancel_at_period_end = cancel;
                }
                if let Some(end) = object
                    .get("current_period_end")
                    .and_then(serde_json::Value::as_i64)
                {
                    subscription.current_period_end_ms = if end.abs() < 10_000_000_000 {
                        end.saturating_mul(1000)
                    } else {
                        end
                    };
                }
                subscription.updated_ms = now;
                Ok(WebhookEffects {
                    subscription: Some(subscription),
                    invoice: None,
                    payment: None,
                })
            }
            "customer.subscription.deleted" => {
                let Some(object) = object else {
                    return Ok(WebhookEffects::default());
                };
                let Some(provider_id) = object.get("id").and_then(serde_json::Value::as_str) else {
                    return Ok(WebhookEffects::default());
                };
                let Some(mut subscription) = self
                    .store
                    .subscription_organization(provider_id)?
                    .and_then(|org| self.store.subscription(&org).ok().flatten())
                else {
                    return Ok(WebhookEffects::default());
                };
                if subscription.organization_id != org {
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
                })
            }
            _ => Ok(WebhookEffects::default()),
        }
    }
}

fn bounded_text(name: &str, value: &str, max: usize) -> Result<(), PaymentError> {
    if value.is_empty() || value.len() > max {
        return Err(PaymentError::Malformed(format!(
            "{name} must be 1..={max} bytes"
        )));
    }
    Ok(())
}

fn request_hash<T: serde::Serialize>(request: &T) -> Result<String, PaymentError> {
    let bytes = serde_json::to_vec(request)
        .map_err(|e| PaymentError::Malformed(format!("request encoding: {e}")))?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn disposition_string(disposition: WebhookDisposition) -> &'static str {
    match disposition {
        WebhookDisposition::Applied => "applied",
        WebhookDisposition::Duplicate => "duplicate",
        WebhookDisposition::StaleIgnored => "stale_ignored",
        WebhookDisposition::Ignored => "ignored",
    }
}

/// Reject raw PAN input: a token field must never be a 12..=19 digit card
/// number (with optional spaces/hyphens).
pub fn validate_payment_token(token: &str) -> Result<(), PaymentError> {
    if token.is_empty() || token.len() > 256 {
        return Err(PaymentError::Malformed(
            "payment token must be 1..=256 bytes".into(),
        ));
    }
    let digits = token
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>();
    let pan_shaped = (12..=19).contains(&digits.len())
        && token
            .chars()
            .all(|c| c.is_ascii_digit() || c == ' ' || c == '-');
    if pan_shaped {
        return Err(PaymentError::Malformed(
            "raw card numbers are never accepted; use a provider token".into(),
        ));
    }
    Ok(())
}

fn invoices_csv(invoices: &[Invoice]) -> String {
    let mut text = String::from(
        "number,invoice_id,status,currency,line_kind,description,quantity,unit_amount_minor,\
         amount_minor,subtotal_minor,discount_minor,credits_minor,tax_minor,total_minor,\
         amount_paid_minor,amount_refunded_minor,balance_minor,issued_ms,due_ms,\
         tax_jurisdiction,tax_inclusive\n",
    );
    for invoice in invoices {
        for line in &invoice.lines {
            let fields = vec![
                invoice.number.clone(),
                invoice.id.clone(),
                invoice.status.as_str().to_string(),
                invoice.currency.clone(),
                line.kind.as_str().to_string(),
                line.description.clone(),
                line.quantity.to_string(),
                line.unit_amount_minor.to_string(),
                line.amount_minor.to_string(),
                invoice.subtotal_minor.to_string(),
                invoice.discount_minor.to_string(),
                invoice.credits_minor.to_string(),
                invoice.tax_minor.to_string(),
                invoice.total_minor.to_string(),
                invoice.amount_paid_minor.to_string(),
                invoice.amount_refunded_minor.to_string(),
                invoice.balance_minor().to_string(),
                invoice.issued_ms.to_string(),
                invoice.due_ms.to_string(),
                invoice.tax_jurisdiction.clone().unwrap_or_default(),
                invoice.tax_inclusive.to_string(),
            ];
            text.push_str(
                &fields
                    .iter()
                    .map(|field| faktor_cloud::csv_field(field))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            text.push('\n');
        }
    }
    text
}

fn invoice_csv(invoice: &Invoice) -> String {
    invoices_csv(std::slice::from_ref(invoice))
}

// Re-export the config helpers the CLI/server need without leaking the
// service's private fields.
impl PaymentService {
    pub fn catalogue_plan(&self, plan_id: &str) -> Option<&PlanPrice> {
        self.config.catalogue.plan(plan_id)
    }

    pub fn tax_config(&self) -> &TaxConfig {
        &self.config.tax
    }

    pub fn dunning_config(&self) -> &DunningConfig {
        &self.config.dunning
    }

    pub fn preview_text(&self, preview: &ProrationPreview) -> String {
        format!(
            "{} -> {}: credit {} charge {} net {} ({} of {} ms)",
            preview.from_plan,
            preview.to_plan,
            self.format_money(preview.credit_minor, &preview.currency),
            self.format_money(preview.charge_minor, &preview.currency),
            self.format_money(preview.net_minor, &preview.currency),
            preview.remaining_ms,
            preview.period_ms,
        )
    }

    pub fn tax_rate(&self, jurisdiction: Option<&str>) -> Result<Option<&TaxRate>, PaymentError> {
        self.config.tax.rate_for(jurisdiction)
    }

    pub fn invoice_error_body(error: &PaymentError) -> PaymentErrorBody {
        error.body()
    }

    pub fn stats(&self, organization: &OrganizationId) -> Result<InvoiceStats, PaymentError> {
        self.store.invoice_stats(organization.as_str())
    }
}
