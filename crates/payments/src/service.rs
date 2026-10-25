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

use crate::authority::{
    InvoiceReconciliationState, ProviderInvoiceObservation, ProviderObservationSource,
    ProviderPaymentObservation, ProviderRefundObservation, ReconciliationItem,
    ReconciliationReport, MAX_RECONCILIATION_INVOICES,
};
use crate::catalog::{
    BillingInterval, DunningConfig, PaymentsConfig, PlanPrice, ProviderKind, TaxConfig, TaxRate,
};
use crate::error::{PaymentError, PaymentErrorBody};
use crate::lease::ConflictDomain;
use crate::model::{
    AddPaymentMethodRequest, CancelRequest, CapabilityStatus, ChangePlanRequest,
    ChangeSeatsRequest, ClaimAcquisition, CreditNote, FinancialResourceType, Invoice, InvoiceLine,
    InvoiceLineKind, InvoiceOutcome, InvoicePage, InvoiceStatus, IssueInvoiceRequest,
    LifecycleOutcome, OperationClaim, OperationClaimDraft, PayInvoiceOutcome, PayInvoiceRequest,
    Payment, PaymentCustomer, PaymentMethod, PaymentMethodOutcome, PaymentMethodView,
    PaymentStatus, PaymentSubscription, PaymentsOverview, PendingProration, PlanChangeOutcome,
    PreviewPlanRequest, PreviewSeatsRequest, ProrationPreview, ProviderStatus, Refund,
    RefundOutcome, RefundRequest, RefundStatus, SeatChangeOutcome, StoredPaymentMethod,
    SubscribeRequest, SubscriptionOutcome, SubscriptionState, TaxSurface, WebhookDisposition,
    WebhookOutcome,
};
use crate::money::{checked_add, checked_mul, exclusive_tax, format_minor, inclusive_tax, prorate};
use crate::provider::{
    PaymentProvider, ProviderInvoiceSnapshot, ProviderObjectKind, ProviderReconcileRequest,
    ProviderReconciled, VerifiedWebhookEvent, WebhookHeaders,
};
use crate::secrets::SecretSource;
use crate::store::{
    consume_proration_tx, customer_tx, insert_credit_note_tx, insert_invoice_tx, insert_payment_tx,
    insert_pending_proration_tx, insert_refund_tx, invoice_by_provider_id_tx, invoice_tx,
    next_sequence_tx, payment_by_provider_id_tx, payment_tx, put_customer_tx,
    put_payment_method_tx, put_subscription_tx, refund_by_id_tx, refund_by_provider_id_conn,
    subscription_by_id_tx, subscription_by_provider_id_tx, update_invoice_tx,
    upsert_provider_invoice_conn, watermark_tx, IdempotencyWrite, InvoiceStats, SqlitePaymentStore,
    WebhookDecision, WebhookEffects, WebhookPlan, WebhookRecord,
};
use crate::subscription_ops;
use crate::vault::PaymentTokenVault;

mod export;
mod invoice_ops;
mod tokens;
mod webhook_effects;
mod webhooks;

use export::{invoice_csv, invoices_csv};
pub use tokens::validate_payment_token;

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
    vault: PaymentTokenVault,
}

impl PaymentService {
    fn new(
        store: Arc<SqlitePaymentStore>,
        provider: Arc<dyn PaymentProvider>,
        config: PaymentsConfig,
        clock: Arc<dyn Clock>,
        vault: PaymentTokenVault,
    ) -> Result<Self, PaymentError> {
        config.validate()?;
        let service = Self {
            store,
            provider,
            config,
            clock,
            vault,
        };
        service.migrate_plaintext_payment_tokens()?;
        service.recover_dispatch_claims()?;
        Ok(service)
    }

    /// Startup recovery: claims that crossed the provider boundary in a
    /// previous process become `unknown`; claims that never dispatched are
    /// released (no provider effect can exist). Financial mutation fences
    /// whose claim is terminal or gone are released; every other held fence
    /// stays occupied for reconciliation.
    fn recover_dispatch_claims(&self) -> Result<(), PaymentError> {
        self.store.recover_claims(self.now_ms())?;
        subscription_ops::recover_leases_after_claim_recovery(self.store.as_ref())?;
        self.recover_invoice_and_refund_leases()?;
        self.store.recover_leases()?;
        Ok(())
    }

    /// Build a service around a caller-supplied provider (used by adapters
    /// and adversarial reconciliation tests). The provider still owns every
    /// external side effect; the service owns claims and durable state.
    pub fn with_provider(
        store: Arc<SqlitePaymentStore>,
        provider: Arc<dyn PaymentProvider>,
        config: PaymentsConfig,
        clock: Arc<dyn Clock>,
        secrets: Arc<dyn SecretSource>,
    ) -> Result<Self, PaymentError> {
        let vault =
            PaymentTokenVault::resolve(secrets.as_ref(), &config.vault_key_env, store.db_path())?;
        Self::new(store, provider, config, clock, vault)
    }

    fn open_vault(
        store: &SqlitePaymentStore,
        config: &PaymentsConfig,
        secrets: &dyn SecretSource,
    ) -> Result<PaymentTokenVault, PaymentError> {
        PaymentTokenVault::resolve(secrets, &config.vault_key_env, store.db_path())
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
        let vault = Self::open_vault(&store, &config, secrets.as_ref())?;
        let provider: Arc<dyn PaymentProvider> = Arc::new(crate::local::LocalPaymentProvider::new(
            store.clone(),
            secret,
            clock.clone(),
        ));
        Self::new(store, provider, config, clock, vault)
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
        let vault = Self::open_vault(&store, &config, secrets.as_ref())?;
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
        Self::new(store, provider, config, clock, vault)
    }

    /// Construct the Mollie adapter. A missing API key is not a construction
    /// failure: the honest unconfigured provider is installed so reads work
    /// and every paid operation refuses typed.
    pub fn open_mollie(
        store: Arc<SqlitePaymentStore>,
        config: PaymentsConfig,
        clock: Arc<dyn Clock>,
        secrets: Arc<dyn SecretSource>,
        transport: Option<Arc<dyn faktor_provider::egress::HttpTransport>>,
    ) -> Result<Self, PaymentError> {
        let api_key = secrets.get(&config.mollie.api_key_env);
        let vault = Self::open_vault(&store, &config, secrets.as_ref())?;
        let provider: Arc<dyn PaymentProvider> = match (api_key, transport) {
            (Some(api_key), Some(transport)) if !api_key.is_empty() => Arc::new(
                crate::mollie::MolliePaymentProvider::new(&config.mollie, api_key, transport)?,
            ),
            (Some(api_key), None) if !api_key.is_empty() => {
                Arc::new(crate::provider::UnconfiguredPaymentProvider::new(
                    "mollie",
                    "no checked egress transport is wired",
                ))
            }
            _ => Arc::new(crate::provider::UnconfiguredPaymentProvider::new(
                "mollie",
                format!(
                    "the environment variable {} does not resolve to an API key",
                    config.mollie.api_key_env
                ),
            )),
        };
        Self::new(store, provider, config, clock, vault)
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
        let reconciliation = self.store.reconciliation_summary(org)?;
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
            refunded_minor: stats.refunded_minor,
            reconciliation,
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
        subscription_ops::release_resolved(self.store.as_ref(), org)?;
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
        let subscription_id =
            operation_resource_id("sub", org, "subscribe", &request.idempotency_key);
        let provider_key = format!("{}:subscribe", request.idempotency_key);
        match self.acquire_claim(
            org,
            &request.idempotency_key,
            "subscribe",
            FinancialResourceType::Subscription,
            &subscription_id,
            &hash,
            &provider_key,
            &serde_json::json!({"request": &request}),
        )? {
            ClaimAcquisition::Acquired => {}
            ClaimAcquisition::Recorded(claim) => {
                let (response, _) = self.claim_replay(&claim, true)?;
                return Ok(SubscriptionOutcome {
                    subscription: self.subscription_from_replay(org, &response)?,
                    replayed: true,
                });
            }
            ClaimAcquisition::InFlight(claim) => return Err(Self::claim_in_flight_error(&claim)),
            ClaimAcquisition::Unresolved(claim) => {
                return Err(Self::claim_unresolved_error(&claim))
            }
        }
        let lease = match subscription_ops::acquire(
            self.store.as_ref(),
            org,
            &ConflictDomain::Subscription {
                organization_id: org.to_string(),
            },
            org,
            &request.idempotency_key,
            &hash,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                subscription_ops::abandon_claim(
                    self.store.as_ref(),
                    org,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        if let Some(existing) = self.store.subscription(org)? {
            if existing.is_live() {
                let error = PaymentError::Conflict(
                    "the organization already has a live payment subscription".into(),
                );
                self.store.fail_claim(
                    org,
                    &request.idempotency_key,
                    error.code(),
                    &error.to_string(),
                    now,
                )?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        }
        self.store
            .mark_claim_dispatched(org, &request.idempotency_key, now)?;
        let existing_customer = self.store.customer(org)?;
        let customer = match existing_customer {
            Some(customer) => customer,
            None => {
                let remote = match self
                    .provider
                    .create_customer(crate::provider::ProviderCustomerRequest {
                        organization_id: org.to_string(),
                        idempotency_key: format!("{}:customer", request.idempotency_key),
                        email: request.customer_email.clone(),
                        name: request.customer_name.clone(),
                    })
                    .await
                {
                    Ok(remote) => remote,
                    Err(error) => {
                        self.fail_dispatch(org, &request.idempotency_key, &error)?;
                        subscription_ops::settle(
                            self.store.as_ref(),
                            org,
                            &lease,
                            &request.idempotency_key,
                        )?;
                        return Err(error);
                    }
                };
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
        let remote = match self
            .provider
            .subscribe(crate::provider::ProviderSubscribeRequest {
                organization_id: org.to_string(),
                idempotency_key: provider_key,
                customer_id: customer.provider_customer_id.clone(),
                plan_id: request.plan_id.clone(),
                interval: request.interval,
                quantity: request.seats,
                trial_end_ms,
                currency: plan.currency.clone(),
                unit_amount_minor: price,
            })
            .await
        {
            Ok(remote) => remote,
            Err(error) => {
                self.fail_dispatch(org, &request.idempotency_key, &error)?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
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
        let state = match provider_subscription_state(&remote.status, trial_end_ms, now) {
            Ok(state) => state,
            Err(error) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, now)?;
                return Err(error);
            }
        };
        if state == SubscriptionState::Canceled {
            self.store
                .mark_claim_unknown(org, &request.idempotency_key, now)?;
            return Err(PaymentError::Provider(
                "provider created an already-canceled subscription".into(),
            ));
        }
        let subscription = PaymentSubscription {
            id: subscription_id,
            organization_id: org.to_string(),
            customer_id: customer.id.clone(),
            provider_subscription_id: remote.id,
            provider_item_id: remote.item_id,
            provider_product_id: remote.product_id,
            provider_price_id: remote.price_id,
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
            Ok(()) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                Ok(SubscriptionOutcome {
                    subscription,
                    replayed: false,
                })
            }
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
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
            Err(e) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, self.now_ms())?;
                Err(e)
            }
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
        subscription_ops::release_resolved(self.store.as_ref(), org)?;
        if let Some(response) = self.replay("change_plan", org, &request.idempotency_key, &hash)? {
            return self.plan_change_from_replay(org, &response, true);
        }
        let subscription = self.require_subscription(org, &request.subscription_id)?;
        if !subscription.is_live() {
            return Err(PaymentError::IllegalState(
                subscription.state.as_str().into(),
            ));
        }
        let provider_customer_id = self
            .store
            .customer(org)?
            .map(|customer| customer.provider_customer_id)
            .ok_or_else(|| PaymentError::IllegalState("provider customer is missing".into()))?;
        let interval = request.interval.unwrap_or(subscription.interval);
        let proration =
            self.compute_plan_proration(&subscription, &request.plan_id, interval, self.now_ms())?;
        let unit_amount =
            self.plan_amount_in(&request.plan_id, interval, &subscription.currency)?;
        let provider_key = format!("{}:change-plan", request.idempotency_key);
        match self.acquire_claim(
            org,
            &request.idempotency_key,
            "change_plan",
            FinancialResourceType::Subscription,
            &subscription.id,
            &hash,
            &provider_key,
            &serde_json::json!({"request": &request, "proration": &proration}),
        )? {
            ClaimAcquisition::Acquired => {}
            ClaimAcquisition::Recorded(claim) => {
                let (response, _) = self.claim_replay(&claim, true)?;
                return self.plan_change_from_replay(org, &response, true);
            }
            ClaimAcquisition::InFlight(claim) => return Err(Self::claim_in_flight_error(&claim)),
            ClaimAcquisition::Unresolved(claim) => {
                return Err(Self::claim_unresolved_error(&claim))
            }
        }
        let lease = match subscription_ops::acquire(
            self.store.as_ref(),
            org,
            &ConflictDomain::SubscriptionMutation {
                subscription_id: subscription.id.clone(),
            },
            &subscription.id,
            &request.idempotency_key,
            &hash,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                subscription_ops::abandon_claim(
                    self.store.as_ref(),
                    org,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        let current = match self.require_subscription(org, &request.subscription_id) {
            Ok(current) if current == subscription => current,
            Ok(_) | Err(_) => {
                let error = PaymentError::Conflict(
                    "the subscription changed while the plan change was being fenced".into(),
                );
                self.store.fail_claim(
                    org,
                    &request.idempotency_key,
                    error.code(),
                    &error.to_string(),
                    self.now_ms(),
                )?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        let subscription = current;
        self.store
            .mark_claim_dispatched(org, &request.idempotency_key, self.now_ms())?;
        let remote = match self
            .provider
            .change_plan(crate::provider::ProviderChangePlanRequest {
                organization_id: org.to_string(),
                idempotency_key: provider_key,
                subscription_id: subscription.provider_subscription_id.clone(),
                customer_id: provider_customer_id.clone(),
                provider_item_id: subscription.provider_item_id.clone(),
                provider_product_id: subscription.provider_product_id.clone(),
                provider_price_id: subscription.provider_price_id.clone(),
                plan_changed: true,
                plan_id: request.plan_id.clone(),
                interval,
                quantity: subscription.quantity,
                currency: subscription.currency.clone(),
                unit_amount_minor: unit_amount,
                prorate: true,
            })
            .await
        {
            Ok(remote) => remote,
            Err(error) => {
                self.fail_dispatch(org, &request.idempotency_key, &error)?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        let mut updated = subscription.clone();
        updated.provider_subscription_id = remote.id;
        updated.provider_item_id = remote.item_id;
        updated.provider_product_id = remote.product_id;
        updated.provider_price_id = remote.price_id;
        updated.quantity = remote.quantity;
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
            Ok(()) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                Ok(PlanChangeOutcome {
                    subscription: updated,
                    proration,
                    replayed: false,
                })
            }
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                let response = self
                    .replay("change_plan", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("plan change was recorded concurrently".into())
                    })?;
                self.plan_change_from_replay(org, &response, true)
            }
            Err(e) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, self.now_ms())?;
                Err(e)
            }
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
        subscription_ops::release_resolved(self.store.as_ref(), org)?;
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
        let provider_customer_id = self
            .store
            .customer(org)?
            .map(|customer| customer.provider_customer_id)
            .ok_or_else(|| PaymentError::IllegalState("provider customer is missing".into()))?;
        let provider_key = format!("{}:cancel", request.idempotency_key);
        match self.acquire_claim(
            org,
            &request.idempotency_key,
            "cancel",
            FinancialResourceType::Subscription,
            &subscription.id,
            &hash,
            &provider_key,
            &serde_json::json!({"request": &request}),
        )? {
            ClaimAcquisition::Acquired => {}
            ClaimAcquisition::Recorded(claim) => {
                let (response, _) = self.claim_replay(&claim, true)?;
                return Ok(SubscriptionOutcome {
                    subscription: self.subscription_from_replay(org, &response)?,
                    replayed: true,
                });
            }
            ClaimAcquisition::InFlight(claim) => return Err(Self::claim_in_flight_error(&claim)),
            ClaimAcquisition::Unresolved(claim) => {
                return Err(Self::claim_unresolved_error(&claim))
            }
        }
        let lease = match subscription_ops::acquire(
            self.store.as_ref(),
            org,
            &ConflictDomain::SubscriptionMutation {
                subscription_id: subscription.id.clone(),
            },
            &subscription.id,
            &request.idempotency_key,
            &hash,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                subscription_ops::abandon_claim(
                    self.store.as_ref(),
                    org,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        let subscription = match self.require_subscription(org, &request.subscription_id) {
            Ok(current) if current == subscription => current,
            Ok(_) | Err(_) => {
                let error = PaymentError::Conflict(
                    "the subscription changed while the cancel was being fenced".into(),
                );
                self.store.fail_claim(
                    org,
                    &request.idempotency_key,
                    error.code(),
                    &error.to_string(),
                    self.now_ms(),
                )?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        self.store
            .mark_claim_dispatched(org, &request.idempotency_key, self.now_ms())?;
        if let Err(error) = self
            .provider
            .cancel(crate::provider::ProviderCancelRequest {
                organization_id: org.to_string(),
                idempotency_key: provider_key,
                subscription_id: subscription.provider_subscription_id.clone(),
                customer_id: provider_customer_id,
                at_period_end: request.at_period_end,
            })
            .await
        {
            self.fail_dispatch(org, &request.idempotency_key, &error)?;
            subscription_ops::settle(self.store.as_ref(), org, &lease, &request.idempotency_key)?;
            return Err(error);
        }
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
            Ok(()) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                Ok(SubscriptionOutcome {
                    subscription: updated,
                    replayed: false,
                })
            }
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
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
            Err(e) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, self.now_ms())?;
                Err(e)
            }
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
        subscription_ops::release_resolved(self.store.as_ref(), org)?;
        if let Some(response) = self.replay("change_seats", org, &request.idempotency_key, &hash)? {
            return self.seat_change_from_replay(org, &response, true);
        }
        let subscription = self.require_subscription(org, &request.subscription_id)?;
        if !subscription.is_live() {
            return Err(PaymentError::IllegalState(
                subscription.state.as_str().into(),
            ));
        }
        let provider_customer_id = self
            .store
            .customer(org)?
            .map(|customer| customer.provider_customer_id)
            .ok_or_else(|| PaymentError::IllegalState("provider customer is missing".into()))?;
        let proration =
            self.compute_seat_proration(&subscription, request.quantity, self.now_ms())?;
        let provider_key = format!("{}:seats", request.idempotency_key);
        match self.acquire_claim(
            org,
            &request.idempotency_key,
            "change_seats",
            FinancialResourceType::Subscription,
            &subscription.id,
            &hash,
            &provider_key,
            &serde_json::json!({"request": &request, "proration": &proration}),
        )? {
            ClaimAcquisition::Acquired => {}
            ClaimAcquisition::Recorded(claim) => {
                let (response, _) = self.claim_replay(&claim, true)?;
                return self.seat_change_from_replay(org, &response, true);
            }
            ClaimAcquisition::InFlight(claim) => return Err(Self::claim_in_flight_error(&claim)),
            ClaimAcquisition::Unresolved(claim) => {
                return Err(Self::claim_unresolved_error(&claim))
            }
        }
        let lease = match subscription_ops::acquire(
            self.store.as_ref(),
            org,
            &ConflictDomain::SubscriptionMutation {
                subscription_id: subscription.id.clone(),
            },
            &subscription.id,
            &request.idempotency_key,
            &hash,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                subscription_ops::abandon_claim(
                    self.store.as_ref(),
                    org,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        let subscription = match self.require_subscription(org, &request.subscription_id) {
            Ok(current) if current == subscription => current,
            Ok(_) | Err(_) => {
                let error = PaymentError::Conflict(
                    "the subscription changed while the seat change was being fenced".into(),
                );
                self.store.fail_claim(
                    org,
                    &request.idempotency_key,
                    error.code(),
                    &error.to_string(),
                    self.now_ms(),
                )?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        self.store
            .mark_claim_dispatched(org, &request.idempotency_key, self.now_ms())?;
        let remote = match self
            .provider
            .change_plan(crate::provider::ProviderChangePlanRequest {
                organization_id: org.to_string(),
                idempotency_key: provider_key,
                subscription_id: subscription.provider_subscription_id.clone(),
                customer_id: provider_customer_id,
                provider_item_id: subscription.provider_item_id.clone(),
                provider_product_id: subscription.provider_product_id.clone(),
                provider_price_id: subscription.provider_price_id.clone(),
                plan_changed: false,
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
            .await
        {
            Ok(remote) => remote,
            Err(error) => {
                self.fail_dispatch(org, &request.idempotency_key, &error)?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        let now = self.now_ms();
        let mut updated = subscription;
        updated.provider_subscription_id = remote.id;
        updated.provider_item_id = remote.item_id;
        updated.provider_product_id = remote.product_id;
        updated.provider_price_id = remote.price_id;
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
            Ok(()) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                Ok(SeatChangeOutcome {
                    subscription: updated,
                    proration,
                    replayed: false,
                })
            }
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                let response = self
                    .replay("change_seats", org, &request.idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("seat change was recorded concurrently".into())
                    })?;
                self.seat_change_from_replay(org, &response, true)
            }
            Err(e) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, self.now_ms())?;
                Err(e)
            }
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
        subscription_ops::release_resolved(self.store.as_ref(), org)?;
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
        let period = subscription_ops::period_key(&subscription);
        let pending_ids: Vec<String> = pending.iter().map(|line| line.id.clone()).collect();
        let mut content = request.clone();
        content.idempotency_key.clear();
        let content_digest = request_hash(&serde_json::json!({
            "request": content,
            "period": period,
            "plan_id": subscription.plan_id,
            "interval": subscription.interval,
            "quantity": subscription.quantity,
            "pending_prorations": pending_ids,
        }))?;
        let invoice_id = operation_resource_id(
            "inv",
            org,
            "period_issue",
            &format!("{}:{period}:{content_digest}", subscription.id),
        );
        let invoice = Invoice {
            id: invoice_id.clone(),
            organization_id: org.to_string(),
            number: String::new(),
            subscription_id: Some(subscription.id.clone()),
            provider_invoice_id: None,
            provider_idempotency_key: Some(format!("{}:invoice", request.idempotency_key)),
            reconciliation: Default::default(),
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
        let provider_key = format!("{}:invoice", request.idempotency_key);
        match self.acquire_claim(
            org,
            &request.idempotency_key,
            "issue_invoice",
            FinancialResourceType::Invoice,
            &invoice_id,
            &hash,
            &provider_key,
            &serde_json::json!({
                "request": &request,
                "invoice": &invoice,
                "pending_ids": &pending_ids,
            }),
        )? {
            ClaimAcquisition::Acquired => {}
            ClaimAcquisition::Recorded(claim) => {
                let (response, _) = self.claim_replay(&claim, true)?;
                return Ok(InvoiceOutcome {
                    invoice: self.invoice_from_replay(org, &response)?,
                    replayed: true,
                });
            }
            ClaimAcquisition::InFlight(claim) => return Err(Self::claim_in_flight_error(&claim)),
            ClaimAcquisition::Unresolved(claim) => {
                return Err(Self::claim_unresolved_error(&claim))
            }
        }
        let lease = match subscription_ops::acquire(
            self.store.as_ref(),
            org,
            &ConflictDomain::InvoiceIssue {
                subscription_id: subscription.id.clone(),
                period: period.clone(),
            },
            &format!("{}:{period}", subscription.id),
            &request.idempotency_key,
            &hash,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                subscription_ops::abandon_claim(
                    self.store.as_ref(),
                    org,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        let subscription = match self.require_subscription(org, &request.subscription_id) {
            Ok(current)
                if current == subscription && subscription_ops::period_key(&current) == period =>
            {
                current
            }
            Ok(_) | Err(_) => {
                let error = PaymentError::Conflict(
                    "the subscription or its billing period changed while the invoice was being \
                     fenced"
                        .into(),
                );
                self.store.fail_claim(
                    org,
                    &request.idempotency_key,
                    error.code(),
                    &error.to_string(),
                    now,
                )?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        if let Some(existing) = self.store.invoice(org, &invoice_id)? {
            let write = self.write(
                org,
                &request.idempotency_key,
                "issue_invoice",
                hash.clone(),
                &serde_json::json!({"invoice_id": existing.id}),
                now,
            )?;
            self.store.record_atomic(&write, |_| Ok(()))?;
            subscription_ops::settle(self.store.as_ref(), org, &lease, &request.idempotency_key)?;
            return Ok(InvoiceOutcome {
                invoice: existing,
                replayed: true,
            });
        }
        self.store
            .mark_claim_dispatched(org, &request.idempotency_key, now)?;
        let provider_invoice = match self
            .provider
            .create_invoice(crate::provider::ProviderInvoiceRequest {
                organization_id: org.to_string(),
                idempotency_key: provider_key,
                customer_id: customer.provider_customer_id.clone(),
                subscription_id: Some(subscription.provider_subscription_id.clone()),
                description: format!("{} invoice", subscription.plan_id),
                amount_minor: total_minor,
                currency: subscription.currency.clone(),
            })
            .await
        {
            Ok(invoice) => invoice,
            Err(error) => {
                self.fail_dispatch(org, &request.idempotency_key, &error)?;
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                return Err(error);
            }
        };
        let mut invoice = invoice;
        invoice.provider_invoice_id = Some(provider_invoice.id.clone());
        let provider_snapshot = ProviderInvoiceSnapshot {
            id: provider_invoice.id.clone(),
            status: provider_invoice.status.clone(),
            amount_minor: provider_invoice.amount_minor,
            amount_paid_minor: if provider_invoice.status == "paid" {
                provider_invoice.amount_minor
            } else {
                0
            },
            amount_refunded_minor: 0,
            currency: provider_invoice.currency.clone(),
            paid_at_ms: (provider_invoice.status == "paid").then_some(now),
        };
        let write = self.write(
            org,
            &request.idempotency_key,
            "issue_invoice",
            hash.clone(),
            &serde_json::json!({"invoice_id": invoice_id}),
            now,
        )?;
        let cloned_lines = lines.clone();
        let effect_value = self.store.record_atomic(&write, |tx| {
            let sequence = next_sequence_tx(tx, org, "invoice")?;
            invoice.number = format!("INV-{sequence:06}");
            let observation = self.invoice_observation(
                &invoice,
                &provider_snapshot,
                ProviderObservationSource::Issue,
                now,
            );
            invoice.reconciliation = observation.reconciliation;
            insert_invoice_tx(tx, &invoice, &cloned_lines)?;
            upsert_provider_invoice_conn(tx, &observation)?;
            for pending_id in &pending_ids {
                consume_proration_tx(tx, pending_id, &invoice_id)?;
            }
            Ok(invoice.clone())
        });
        match effect_value {
            Ok(recorded) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
                Ok(InvoiceOutcome {
                    invoice: recorded,
                    replayed: false,
                })
            }
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                subscription_ops::settle(
                    self.store.as_ref(),
                    org,
                    &lease,
                    &request.idempotency_key,
                )?;
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
            Err(e) => {
                self.store
                    .mark_claim_unknown(org, &request.idempotency_key, self.now_ms())?;
                Err(e)
            }
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
        self.pay_invoice_fenced(organization, request).await
    }

    pub fn record_payment_failure(
        &self,
        organization: &OrganizationId,
        invoice_id: &str,
    ) -> Result<Option<PaymentSubscription>, PaymentError> {
        let org = organization.as_str();
        let now = self.now_ms();
        let max_attempts = self.config.dunning.max_attempts;
        let grace = self.config.dunning.grace_ms.max(0);
        let subscription = self.store.record_atomic(
            &IdempotencyWrite {
                organization_id: org.to_string(),
                key: format!("dunning:{}", uuid::Uuid::new_v4()),
                operation: "payment_failure".into(),
                request_hash: invoice_id.to_string(),
                response: "{}".into(),
                created_ms: now,
            },
            |tx| {
                // Re-read inside the transaction: a webhook may have settled
                // this invoice between the provider decline and this write.
                let mut invoice = invoice_tx(tx, org, invoice_id)?
                    .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
                if invoice.status.can_transition_to(InvoiceStatus::PastDue) {
                    invoice.status = InvoiceStatus::PastDue;
                }
                update_invoice_tx(tx, &invoice)?;
                let Some(subscription_id) = invoice.subscription_id.as_deref() else {
                    return Ok(None);
                };
                let Some(mut subscription) = subscription_by_id_tx(tx, org, subscription_id)?
                else {
                    return Ok(None);
                };
                // The subscription row belongs to a provider mutation while
                // its fence is held; never overwrite that operation's result.
                if crate::lease::active_lease_conn(
                    tx,
                    org,
                    &ConflictDomain::SubscriptionMutation {
                        subscription_id: subscription.id.clone(),
                    },
                )?
                .is_some()
                {
                    return Ok(Some(subscription));
                }
                if !subscription
                    .state
                    .can_transition_to(SubscriptionState::PastDue)
                {
                    return Ok(Some(subscription));
                }
                subscription.dunning_attempts = subscription.dunning_attempts.saturating_add(1);
                subscription.updated_ms = now;
                subscription.state = SubscriptionState::PastDue;
                if subscription.grace_end_ms.is_none() {
                    subscription.grace_end_ms = Some(now.saturating_add(grace));
                }
                if subscription.dunning_attempts >= max_attempts {
                    subscription.state = SubscriptionState::Canceled;
                    subscription.canceled_ms = Some(now);
                    subscription.cancel_at_period_end = false;
                }
                put_subscription_tx(tx, &subscription)?;
                Ok(Some(subscription))
            },
        )?;
        Ok(subscription)
    }

    pub async fn refund(
        &self,
        organization: &OrganizationId,
        request: RefundRequest,
    ) -> Result<RefundOutcome, PaymentError> {
        self.refund_fenced(organization, request).await
    }

    // ------------------------------------------------------------ lifecycle

    pub fn tick(&self, organization: &OrganizationId) -> Result<LifecycleOutcome, PaymentError> {
        let org = organization.as_str();
        let now = self.now_ms();
        let max_attempts = self.config.dunning.max_attempts;
        let (subscription, changed) = self.store.tx(|tx| {
            let Some(mut subscription) = crate::store::subscription_tx(tx, org)? else {
                return Err(PaymentError::NotFound("payment subscription".into()));
            };
            // A lifecycle tick mutates the same durable row as plan, seat and
            // cancel operations. Those hold the subscription fenced across
            // their provider call, so a tick that observes the fence must not
            // write a stale period/cancelation over the in-flight result.
            if crate::lease::active_lease_conn(
                tx,
                org,
                &ConflictDomain::SubscriptionMutation {
                    subscription_id: subscription.id.clone(),
                },
            )?
            .is_some()
                || crate::lease::active_lease_conn(
                    tx,
                    org,
                    &ConflictDomain::Subscription {
                        organization_id: org.to_string(),
                    },
                )?
                .is_some()
            {
                return Ok((subscription, false));
            }
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
                let exhausted = subscription.dunning_attempts >= max_attempts;
                if expired || exhausted {
                    subscription.state = SubscriptionState::Canceled;
                    subscription.canceled_ms = Some(now);
                    subscription.cancel_at_period_end = false;
                    changed = true;
                }
            }
            if changed {
                put_subscription_tx(tx, &subscription)?;
            }
            Ok((subscription, changed))
        })?;
        Ok(LifecycleOutcome {
            subscription,
            changed,
        })
    }

    /// Open one provider-hosted checkout for an already-recorded invoice.
    /// Provider creation is idempotent by `idempotency_key`; the service does
    /// not mutate local billing state here. Only a verified provider webhook
    /// may settle the invoice.
    pub async fn create_checkout(
        &self,
        organization: &OrganizationId,
        request: crate::provider::ProviderCheckoutRequest,
    ) -> Result<crate::provider::ProviderCheckout, PaymentError> {
        if request.organization_id != organization.as_str() {
            return Err(PaymentError::Malformed(
                "checkout organization does not match the authorized organization".into(),
            ));
        }
        self.validate_idempotency(&request.idempotency_key)?;
        bounded_text("checkout description", &request.description, MAX_TEXT_BYTES)?;
        if request.amount_minor <= 0 || !crate::money::valid_amount(request.amount_minor) {
            return Err(PaymentError::Malformed(
                "checkout amount must be a positive bounded minor amount".into(),
            ));
        }
        if !crate::money::valid_currency(&request.currency) {
            return Err(PaymentError::Malformed(
                "checkout currency must be three uppercase letters".into(),
            ));
        }
        if let Some(invoice_id) = &request.local_invoice_id {
            if invoice_id.is_empty() || invoice_id.len() > 128 {
                return Err(PaymentError::Malformed(
                    "checkout local_invoice_id must be 1..=128 bytes".into(),
                ));
            }
            let invoice = self.invoice(organization, invoice_id)?;
            if invoice.status != crate::model::InvoiceStatus::Open {
                return Err(PaymentError::IllegalState(invoice.status.as_str().into()));
            }
            if invoice.balance_minor() != request.amount_minor
                || invoice.currency != request.currency
            {
                return Err(PaymentError::Malformed(
                    "checkout amount or currency does not match the invoice balance".into(),
                ));
            }
        }
        let org = organization.as_str();
        let hash = request_hash(&request)?;
        if let Some(response) =
            self.replay("create_checkout", org, &request.idempotency_key, &hash)?
        {
            return serde_json::from_str(&response)
                .map_err(|e| PaymentError::Store(format!("checkout replay: {e}")));
        }
        let checkout_id =
            operation_resource_id("cs", org, "create_checkout", &request.idempotency_key);
        let provider_key = format!("{}:checkout", request.idempotency_key);
        match self.acquire_claim(
            org,
            &request.idempotency_key,
            "create_checkout",
            FinancialResourceType::Checkout,
            &checkout_id,
            &hash,
            &provider_key,
            &serde_json::json!({}),
        )? {
            ClaimAcquisition::Acquired => {}
            ClaimAcquisition::Recorded(claim) => {
                let (response, _) = self.claim_replay(&claim, true)?;
                return serde_json::from_str(&response)
                    .map_err(|e| PaymentError::Store(format!("checkout claim replay: {e}")));
            }
            ClaimAcquisition::InFlight(claim) => return Err(Self::claim_in_flight_error(&claim)),
            ClaimAcquisition::Unresolved(claim) => {
                return Err(Self::claim_unresolved_error(&claim))
            }
        }
        let now = self.now_ms();
        let idempotency_key = request.idempotency_key.clone();
        self.store
            .mark_claim_dispatched(org, &idempotency_key, now)?;
        let checkout = match self.provider.create_checkout(request).await {
            Ok(checkout) => checkout,
            Err(error) => {
                self.fail_dispatch(org, &idempotency_key, &error)?;
                return Err(error);
            }
        };
        let response = serde_json::to_value(&checkout)
            .map_err(|e| PaymentError::Store(format!("checkout encoding: {e}")))?;
        let write = self.write(
            org,
            &idempotency_key,
            "create_checkout",
            hash.clone(),
            &response,
            now,
        )?;
        match self.store.record_atomic(&write, |_| Ok(())) {
            Ok(()) => Ok(checkout),
            Err(PaymentError::IdempotencyConflict(_)) | Err(PaymentError::Conflict(_)) => {
                let response = self
                    .replay("create_checkout", org, &idempotency_key, &hash)?
                    .ok_or_else(|| {
                        PaymentError::Conflict("checkout was recorded concurrently".into())
                    })?;
                serde_json::from_str(&response)
                    .map_err(|e| PaymentError::Store(format!("checkout replay: {e}")))
            }
            Err(e) => {
                self.store
                    .mark_claim_unknown(org, &idempotency_key, self.now_ms())?;
                Err(e)
            }
        }
    }

    // -------------------------------------------------------------- webhook

    pub async fn handle_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<WebhookOutcome, PaymentError> {
        let event = self
            .provider
            .resolve_webhook(headers, body, now_ms, self.store.as_ref())
            .await?;
        let provider = self.provider.name();
        let organization = self.webhook_organization(&event)?;
        let org = organization.as_str();
        if let Some(provider_subscription_id) = event
            .payload
            .pointer("/data/object/id")
            .and_then(serde_json::Value::as_str)
        {
            let subscription_event = matches!(
                event.event_type.as_str(),
                "customer.subscription.updated"
                    | "customer.subscription.deleted"
                    | "subscription.cancelled"
            );
            if subscription_event
                && subscription_ops::webhook_blocked_by_fence(
                    self.store.as_ref(),
                    org,
                    provider_subscription_id,
                )?
            {
                return Err(PaymentError::Conflict(
                    "a subscription mutation is in flight; retry the webhook".into(),
                ));
            }
        }
        let digest = sha256_hex(body);
        let record = |disposition: &str| WebhookRecord {
            provider: provider.to_string(),
            event_id: event.id.clone(),
            organization_id: org.to_string(),
            created_ms: event.created_ms,
            event_type: event.event_type.clone(),
            disposition: disposition.to_string(),
            payload_digest: digest.clone(),
        };
        let decision = self.store.apply_webhook(provider, &event.id, |tx| {
            if let Some((last_created, last_id)) = watermark_tx(tx, org)? {
                if event.created_ms < last_created
                    || (event.created_ms == last_created && event.id.as_str() <= last_id.as_str())
                {
                    return Ok(WebhookPlan {
                        record: record("stale_ignored"),
                        effects: WebhookEffects::default(),
                        advance_watermark: false,
                        value: (WebhookDisposition::StaleIgnored, WebhookEffects::default()),
                    });
                }
            }
            let effects = self.webhook_effects_tx(tx, org, &event, now_ms)?;
            let conflict = effects
                .provider_invoice
                .as_ref()
                .is_some_and(|observation| observation.reconciliation.is_conflict());
            let applied = effects.subscription.is_some()
                || effects.invoice.is_some()
                || effects.payment.is_some()
                || effects.refund.is_some()
                || effects.credit_note.is_some()
                || effects.provider_invoice.is_some()
                || effects.provider_payment.is_some()
                || effects.provider_refund.is_some();
            let disposition = if conflict {
                WebhookDisposition::ReconciliationConflict
            } else if applied {
                WebhookDisposition::Applied
            } else {
                WebhookDisposition::Ignored
            };
            Ok(WebhookPlan {
                record: record(disposition_string(disposition)),
                effects: effects.clone(),
                advance_watermark: true,
                value: (disposition, effects),
            })
        })?;
        let (disposition, effects) = match decision {
            WebhookDecision::Duplicate(_) => {
                (WebhookDisposition::Duplicate, WebhookEffects::default())
            }
            WebhookDecision::Applied((disposition, effects)) => (disposition, effects),
        };
        self.release_leases_after_webhook(org, &effects);
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

    // ------------------------------------------------- provider observations

    /// The linked provider observations for one Faktor invoice. These are
    /// upstream records; callers must not treat them as invoice totals.
    pub fn provider_invoice_observations(
        &self,
        organization: &OrganizationId,
        invoice_id: &str,
    ) -> Result<Vec<ProviderInvoiceObservation>, PaymentError> {
        self.store
            .provider_invoice_observations(organization.as_str(), invoice_id)
    }

    pub fn provider_payment_observations(
        &self,
        organization: &OrganizationId,
        invoice_id: &str,
    ) -> Result<Vec<ProviderPaymentObservation>, PaymentError> {
        self.store
            .provider_payment_observations(organization.as_str(), invoice_id)
    }

    pub fn provider_refund_observations(
        &self,
        organization: &OrganizationId,
        invoice_id: &str,
    ) -> Result<Vec<ProviderRefundObservation>, PaymentError> {
        self.store
            .provider_refund_observations(organization.as_str(), invoice_id)
    }

    /// Run one bounded, read-only provider reconciliation pass. The Faktor
    /// invoice totals/status are never modified here: only its typed
    /// reconciliation state and the linked observation are recorded.
    pub async fn reconcile(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<ReconciliationReport, PaymentError> {
        if limit == 0 || limit > MAX_RECONCILIATION_INVOICES {
            return Err(PaymentError::Malformed(format!(
                "reconciliation limit must be 1..={MAX_RECONCILIATION_INVOICES}"
            )));
        }
        let org = organization.as_str();
        let invoices = self.store.invoices_for_reconciliation(org, limit)?;
        let observed_ms = self.now_ms();
        let mut report = ReconciliationReport {
            organization_id: org.to_string(),
            checked: 0,
            matched: 0,
            conflicts: 0,
            missing: 0,
            unreconciled: 0,
            observed_ms,
            items: Vec::new(),
        };
        for invoice in invoices {
            let Some(provider_invoice_id) = invoice.provider_invoice_id.clone() else {
                continue;
            };
            report.checked = report.checked.saturating_add(1);
            let fetched = self.provider.fetch_invoice(&provider_invoice_id).await;
            let snapshot = match fetched {
                Ok(snapshot) => Some(snapshot),
                Err(PaymentError::NotFound(_)) => None,
                Err(fetch_error) => {
                    // Fall back to the idempotency-key lookup when the
                    // provider cannot fetch by invoice id. This remains
                    // read-only: it never dispatches a new operation.
                    match self.reconcile_by_key(&invoice).await {
                        Ok(snapshot) => snapshot,
                        Err(_) => return Err(fetch_error),
                    }
                }
            };
            let observation = match snapshot {
                Some(snapshot) => self.invoice_observation(
                    &invoice,
                    &snapshot,
                    ProviderObservationSource::Reconciliation,
                    observed_ms,
                ),
                None => ProviderInvoiceObservation {
                    provider: self.provider.name().to_string(),
                    provider_invoice_id: provider_invoice_id.clone(),
                    organization_id: org.to_string(),
                    invoice_id: invoice.id.clone(),
                    status: "absent".into(),
                    amount_minor: 0,
                    amount_paid_minor: 0,
                    amount_refunded_minor: 0,
                    currency: invoice.currency.clone(),
                    paid_at_ms: None,
                    observed_ms,
                    source: ProviderObservationSource::Reconciliation,
                    reconciliation: InvoiceReconciliationState::ProviderInvoiceMissing,
                },
            };
            let mut updated = invoice.clone();
            updated.reconciliation = observation.reconciliation;
            self.store.record_reconciliation(&updated, &observation)?;
            report.items.push(ReconciliationItem {
                invoice_id: invoice.id,
                number: invoice.number,
                provider_invoice_id,
                state: observation.reconciliation,
                observed_amount_minor: Some(observation.amount_minor),
                observed_currency: Some(observation.currency),
                observed_status: Some(observation.status),
            });
            match observation.reconciliation {
                InvoiceReconciliationState::Matched => {
                    report.matched = report.matched.saturating_add(1);
                }
                InvoiceReconciliationState::Unreconciled => {
                    report.unreconciled = report.unreconciled.saturating_add(1);
                }
                InvoiceReconciliationState::AmountMismatch
                | InvoiceReconciliationState::CurrencyMismatch => {
                    report.conflicts = report.conflicts.saturating_add(1);
                }
                InvoiceReconciliationState::ProviderInvoiceMissing => {
                    report.missing = report.missing.saturating_add(1);
                    report.conflicts = report.conflicts.saturating_add(1);
                }
            }
        }
        Ok(report)
    }

    /// Read-only fallback lookup by the provider idempotency key recorded on
    /// the Faktor invoice. It never creates or resends an operation.
    async fn reconcile_by_key(
        &self,
        invoice: &Invoice,
    ) -> Result<Option<ProviderInvoiceSnapshot>, PaymentError> {
        let Some(key) = invoice.provider_idempotency_key.as_deref() else {
            return Err(PaymentError::Provider(
                "no provider idempotency key is recorded for this invoice".into(),
            ));
        };
        match self
            .provider
            .reconcile(ProviderReconcileRequest {
                organization_id: invoice.organization_id.clone(),
                kind: ProviderObjectKind::Invoice,
                idempotency_key: key.to_string(),
            })
            .await?
        {
            ProviderReconciled::Invoice(provider_invoice) => {
                let paid = provider_invoice.status == "paid";
                Ok(Some(ProviderInvoiceSnapshot {
                    id: provider_invoice.id,
                    status: provider_invoice.status,
                    amount_minor: provider_invoice.amount_minor,
                    amount_paid_minor: if paid {
                        provider_invoice.amount_minor
                    } else {
                        0
                    },
                    amount_refunded_minor: 0,
                    currency: provider_invoice.currency,
                    paid_at_ms: paid.then(|| self.now_ms()),
                }))
            }
            ProviderReconciled::NotApplied => Ok(None),
            ProviderReconciled::Unresolved(reason) => Err(PaymentError::Provider(reason)),
            _ => Err(PaymentError::Provider(
                "the provider reconciled a different object kind".into(),
            )),
        }
    }

    // --------------------------------------------------------------- helpers

    /// Build the linked observation and derive its typed reconciliation
    /// state. The observation's amount is provider truth; the invoice's
    /// authored total is never read from it.
    fn invoice_observation(
        &self,
        invoice: &Invoice,
        snapshot: &ProviderInvoiceSnapshot,
        source: ProviderObservationSource,
        observed_ms: i64,
    ) -> ProviderInvoiceObservation {
        let reconciliation = if snapshot.currency != invoice.currency {
            InvoiceReconciliationState::CurrencyMismatch
        } else if snapshot.amount_minor != invoice.total_minor {
            InvoiceReconciliationState::AmountMismatch
        } else {
            InvoiceReconciliationState::Matched
        };
        ProviderInvoiceObservation {
            provider: self.provider.name().to_string(),
            provider_invoice_id: snapshot.id.clone(),
            organization_id: invoice.organization_id.clone(),
            invoice_id: invoice.id.clone(),
            status: snapshot.status.clone(),
            amount_minor: snapshot.amount_minor,
            amount_paid_minor: snapshot.amount_paid_minor,
            amount_refunded_minor: snapshot.amount_refunded_minor,
            currency: snapshot.currency.clone(),
            paid_at_ms: snapshot.paid_at_ms,
            observed_ms,
            source,
            reconciliation,
        }
    }

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

    #[allow(clippy::too_many_arguments)]
    fn acquire_claim(
        &self,
        organization_id: &str,
        key: &str,
        operation: &str,
        resource_type: FinancialResourceType,
        resource_id: &str,
        request_digest: &str,
        provider_idempotency_key: &str,
        context: &serde_json::Value,
    ) -> Result<ClaimAcquisition, PaymentError> {
        let draft = OperationClaimDraft {
            organization_id: organization_id.to_string(),
            idempotency_key: key.to_string(),
            operation: operation.to_string(),
            resource_type,
            resource_id: resource_id.to_string(),
            request_digest: request_digest.to_string(),
            provider_idempotency_key: provider_idempotency_key.to_string(),
            context: serde_json::to_string(context)
                .map_err(|e| PaymentError::Store(format!("claim context encoding: {e}")))?,
            created_ms: self.now_ms(),
        };
        self.store.claim_operation(&draft)
    }

    fn claim_replay(
        &self,
        claim: &OperationClaim,
        replayed: bool,
    ) -> Result<(String, bool), PaymentError> {
        match claim.state {
            crate::model::OperationClaimState::Succeeded => {
                let response = claim.response.clone().ok_or_else(|| {
                    PaymentError::Conflict("a succeeded claim has no recorded response".into())
                })?;
                Ok((response, true))
            }
            crate::model::OperationClaimState::Failed => Err(PaymentError::from_recorded(
                claim.failure_code.as_deref().unwrap_or("conflict"),
                claim
                    .failure_message
                    .as_deref()
                    .unwrap_or("the recorded operation failed"),
            )),
            _ => Err(PaymentError::Conflict(format!(
                "operation claim {} is {} and cannot be replayed",
                claim.idempotency_key,
                claim.state.as_str()
            ))),
        }
        .map(|(response, _)| (response, replayed))
    }

    fn claim_in_flight_error(claim: &OperationClaim) -> PaymentError {
        PaymentError::Conflict(format!(
            "operation {} is already {}; outcomes are recorded, never re-dispatched",
            claim.idempotency_key,
            claim.state.as_str()
        ))
    }

    fn claim_unresolved_error(claim: &OperationClaim) -> PaymentError {
        PaymentError::Conflict(format!(
            "operation {} is unknown after dispatch; provider reconciliation is required",
            claim.idempotency_key
        ))
    }

    fn fail_dispatch(
        &self,
        organization_id: &str,
        key: &str,
        error: &PaymentError,
    ) -> Result<(), PaymentError> {
        let now = self.now_ms();
        if error.is_definitive_dispatch_failure() {
            self.store
                .fail_claim(organization_id, key, error.code(), &error.to_string(), now)?;
        } else {
            self.store.mark_claim_unknown(organization_id, key, now)?;
        }
        Ok(())
    }

    /// Resolve every `unknown` claim for one organization by a read-only
    /// provider lookup. A missing provider object proves the attempt did not
    /// apply; an applied object is rebuilt into the local ledger exactly
    /// once. Anything the adapter cannot look up stays unknown.
    pub async fn reconcile_unknown_claims(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<OperationClaim>, PaymentError> {
        let org = organization.as_str();
        let mut resolved = Vec::new();
        for claim in self.store.operation_claims(org)? {
            if claim.state != crate::model::OperationClaimState::Unknown {
                continue;
            }
            let kind = match claim.operation.as_str() {
                "subscribe" => ProviderObjectKind::Subscription,
                "change_plan" | "change_seats" => ProviderObjectKind::Change,
                "cancel" => ProviderObjectKind::Cancel,
                "issue_invoice" => ProviderObjectKind::Invoice,
                "pay_invoice" => ProviderObjectKind::Charge,
                "refund" => ProviderObjectKind::Refund,
                "create_checkout" => ProviderObjectKind::Checkout,
                other => {
                    return Err(PaymentError::Conflict(format!(
                        "operation {other} has no reconciliation rule"
                    )))
                }
            };
            let reconciled = self
                .provider
                .reconcile(ProviderReconcileRequest {
                    organization_id: org.to_string(),
                    kind,
                    idempotency_key: claim.provider_idempotency_key.clone(),
                })
                .await?;
            match reconciled {
                ProviderReconciled::NotApplied => {
                    self.store.fail_unknown_claim(
                        org,
                        &claim.idempotency_key,
                        "payment_provider_error",
                        "provider reconciliation found no effect for the dispatched key",
                        self.now_ms(),
                    )?;
                    self.release_lease_after_claim_reconciliation(org, &claim, false);
                    subscription_ops::release_for_claim(
                        self.store.as_ref(),
                        org,
                        &claim.idempotency_key,
                    )?;
                }
                ProviderReconciled::Unresolved(_) => {}
                other => {
                    if let Some(updated) = self
                        .apply_reconciled_claim(&claim, other, self.now_ms())
                        .await?
                    {
                        self.release_lease_after_claim_reconciliation(
                            org,
                            &updated,
                            self.claim_business_outcome_is_terminal(org, &updated),
                        );
                        subscription_ops::release_for_claim(
                            self.store.as_ref(),
                            org,
                            &updated.idempotency_key,
                        )?;
                        resolved.push(updated);
                    }
                }
            }
        }
        Ok(resolved)
    }

    async fn apply_reconciled_claim(
        &self,
        claim: &OperationClaim,
        reconciled: ProviderReconciled,
        now: i64,
    ) -> Result<Option<OperationClaim>, PaymentError> {
        let context: serde_json::Value = serde_json::from_str(&claim.context)
            .map_err(|e| PaymentError::Store(format!("claim context decoding: {e}")))?;
        let org = claim.organization_id.as_str();
        match (claim.operation.as_str(), reconciled) {
            ("subscribe", ProviderReconciled::Subscription(remote)) => {
                let request: SubscribeRequest = decode_field(&context, "request")?;
                let plan = self
                    .config
                    .catalogue
                    .plan(&request.plan_id)
                    .ok_or_else(|| PaymentError::PlanPriceUnconfigured(request.plan_id.clone()))?;
                let customer = match self.store.customer(org)? {
                    Some(customer) => customer,
                    None => {
                        let reconciled_customer = self
                            .provider
                            .reconcile(ProviderReconcileRequest {
                                organization_id: org.to_string(),
                                kind: ProviderObjectKind::Customer,
                                idempotency_key: claim
                                    .provider_idempotency_key
                                    .replace(":subscribe", ":customer"),
                            })
                            .await?;
                        let ProviderReconciled::Customer(customer) = reconciled_customer else {
                            return Ok(None);
                        };
                        PaymentCustomer {
                            id: format!("cus_{}", uuid::Uuid::new_v4()),
                            organization_id: org.to_string(),
                            provider: self.provider.name().to_string(),
                            provider_customer_id: customer.id,
                            email: customer.email,
                            name: customer.name,
                            created_ms: claim.created_ms,
                        }
                    }
                };
                let period_start = if remote.current_period_start_ms > 0 {
                    remote.current_period_start_ms
                } else {
                    claim.created_ms
                };
                let period_end = if remote.current_period_end_ms > period_start {
                    remote.current_period_end_ms
                } else {
                    period_start.saturating_add(request.interval.period_ms())
                };
                let trial_end_ms = request.trial_days.filter(|days| *days > 0).map(|days| {
                    claim
                        .created_ms
                        .saturating_add(i64::from(days) * 86_400_000)
                });
                let state = if trial_end_ms.is_some_and(|trial| trial > claim.created_ms)
                    || remote.status == "trialing"
                {
                    SubscriptionState::Trialing
                } else {
                    SubscriptionState::Active
                };
                let subscription = PaymentSubscription {
                    id: claim.resource_id.clone(),
                    organization_id: org.to_string(),
                    customer_id: customer.id.clone(),
                    provider_subscription_id: remote.id,
                    provider_item_id: remote.item_id,
                    provider_product_id: remote.product_id,
                    provider_price_id: remote.price_id,
                    plan_id: request.plan_id.clone(),
                    interval: request.interval,
                    quantity: remote.quantity,
                    state,
                    currency: plan.currency.clone(),
                    current_period_start_ms: period_start,
                    current_period_end_ms: period_end,
                    trial_end_ms,
                    cancel_at_period_end: false,
                    canceled_ms: None,
                    dunning_attempts: 0,
                    grace_end_ms: None,
                    created_ms: claim.created_ms,
                    updated_ms: now,
                };
                let response = serde_json::json!({"subscription_id": subscription.id});
                self.complete_reconciled(
                    claim,
                    &response,
                    |tx| {
                        if customer_tx(tx, org)?.is_none() {
                            put_customer_tx(tx, &customer)?;
                        }
                        put_subscription_tx(tx, &subscription)
                    },
                    now,
                )?;
            }
            ("change_plan", ProviderReconciled::Subscription(remote)) => {
                let request: ChangePlanRequest = decode_field(&context, "request")?;
                let proration: ProrationPreview = decode_field(&context, "proration")?;
                let mut subscription = self.require_subscription(org, &request.subscription_id)?;
                subscription.plan_id = request.plan_id;
                subscription.interval = request.interval.unwrap_or(subscription.interval);
                subscription.quantity = remote.quantity;
                subscription.updated_ms = now;
                let pending = self.pending_from_preview(org, &subscription.id, &proration, now);
                let response = serde_json::json!({
                    "subscription_id": subscription.id,
                    "proration": proration,
                });
                self.complete_reconciled(
                    claim,
                    &response,
                    |tx| {
                        put_subscription_tx(tx, &subscription)?;
                        for line in &pending {
                            insert_pending_proration_tx(tx, line)?;
                        }
                        Ok(())
                    },
                    now,
                )?;
            }
            ("change_seats", ProviderReconciled::Subscription(remote)) => {
                let request: ChangeSeatsRequest = decode_field(&context, "request")?;
                let proration: ProrationPreview = decode_field(&context, "proration")?;
                let mut subscription = self.require_subscription(org, &request.subscription_id)?;
                subscription.quantity = remote.quantity;
                subscription.updated_ms = now;
                let pending = self.pending_from_preview(org, &subscription.id, &proration, now);
                let response = serde_json::json!({
                    "subscription_id": subscription.id,
                    "proration": proration,
                });
                self.complete_reconciled(
                    claim,
                    &response,
                    |tx| {
                        put_subscription_tx(tx, &subscription)?;
                        for line in &pending {
                            insert_pending_proration_tx(tx, line)?;
                        }
                        Ok(())
                    },
                    now,
                )?;
            }
            ("cancel", ProviderReconciled::Subscription(_)) => {
                let request: CancelRequest = decode_field(&context, "request")?;
                let mut subscription = self.require_subscription(org, &request.subscription_id)?;
                subscription.updated_ms = now;
                if request.at_period_end {
                    subscription.cancel_at_period_end = true;
                } else {
                    subscription.state = SubscriptionState::Canceled;
                    subscription.canceled_ms = Some(now);
                    subscription.cancel_at_period_end = false;
                }
                let response = serde_json::json!({"subscription_id": subscription.id});
                self.complete_reconciled(
                    claim,
                    &response,
                    |tx| put_subscription_tx(tx, &subscription),
                    now,
                )?;
            }
            ("issue_invoice", ProviderReconciled::Invoice(remote)) => {
                let request: IssueInvoiceRequest = decode_field(&context, "request")?;
                let draft: Invoice = decode_field(&context, "invoice")?;
                let pending_ids: Vec<String> = decode_field(&context, "pending_ids")?;
                let mut invoice = draft;
                invoice.provider_invoice_id = Some(remote.id);
                let response = serde_json::json!({"invoice_id": invoice.id});
                self.complete_reconciled(
                    claim,
                    &response,
                    |tx| {
                        let sequence = next_sequence_tx(tx, org, "invoice")?;
                        invoice.number = format!("INV-{sequence:06}");
                        insert_invoice_tx(tx, &invoice, &invoice.lines)?;
                        for pending_id in &pending_ids {
                            consume_proration_tx(tx, pending_id, &invoice.id)?;
                        }
                        Ok(())
                    },
                    now,
                )?;
                let _ = request;
            }
            ("pay_invoice", ProviderReconciled::Charge(remote)) => {
                let request: PayInvoiceRequest = decode_field(&context, "request")?;
                let payment_id: String = decode_field(&context, "payment_id")?;
                let amount_minor: i64 = decode_field(&context, "amount_minor")?;
                let payment_method_id: String = decode_field(&context, "payment_method_id")?;
                let currency: String = decode_field(&context, "currency")?;
                let status = payment_status_from_provider(&remote.status)?;
                if let Some(existing) = self
                    .store
                    .payment(org, &payment_id)?
                    .or(self.store.payment_by_provider_id(org, &remote.id)?)
                {
                    let invoice = self
                        .store
                        .invoice(org, &existing.invoice_id)?
                        .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
                    let response = serde_json::json!({
                        "payment_id": existing.id,
                        "invoice_id": invoice.id,
                    });
                    self.complete_reconciled(claim, &response, |_| Ok(()), now)?;
                } else {
                    let mut invoice = self
                        .store
                        .invoice(org, &request.invoice_id)?
                        .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
                    let payment = Payment {
                        id: payment_id.clone(),
                        organization_id: org.to_string(),
                        invoice_id: invoice.id.clone(),
                        provider: self.provider.name().to_string(),
                        provider_payment_id: remote.id,
                        payment_method_id: Some(payment_method_id),
                        amount_minor,
                        refunded_minor: 0,
                        currency,
                        status,
                        created_ms: claim.created_ms,
                    };
                    let mut subscription = self
                        .store
                        .subscription(org)?
                        .filter(|sub| invoice.subscription_id.as_deref() == Some(sub.id.as_str()));
                    if status == PaymentStatus::Succeeded {
                        let credited = amount_minor.clamp(0, invoice.balance_minor());
                        invoice.amount_paid_minor =
                            checked_add(invoice.amount_paid_minor, credited)?;
                        if invoice.amount_paid_minor >= invoice.total_minor
                            && invoice.status.can_transition_to(InvoiceStatus::Paid)
                        {
                            invoice.status = InvoiceStatus::Paid;
                            invoice.paid_ms = Some(now);
                        }
                        if let Some(sub) = subscription.as_mut() {
                            if sub.state.can_transition_to(SubscriptionState::Active) {
                                sub.state = SubscriptionState::Active;
                            }
                            sub.dunning_attempts = 0;
                            sub.grace_end_ms = None;
                            sub.updated_ms = now;
                        }
                    }
                    let response = serde_json::json!({
                        "payment_id": payment.id,
                        "invoice_id": invoice.id,
                    });
                    self.complete_reconciled(
                        claim,
                        &response,
                        |tx| {
                            insert_payment_tx(tx, &payment)?;
                            update_invoice_tx(tx, &invoice)?;
                            if let Some(subscription) = &subscription {
                                put_subscription_tx(tx, subscription)?;
                            }
                            Ok(())
                        },
                        now,
                    )?;
                }
            }
            ("create_checkout", ProviderReconciled::Checkout(checkout)) => {
                let response = serde_json::to_value(&checkout)
                    .map_err(|e| PaymentError::Store(format!("checkout encoding: {e}")))?;
                self.complete_reconciled(claim, &response, |_| Ok(()), now)?;
            }
            ("refund", ProviderReconciled::Refund(remote)) => {
                let request: RefundRequest = decode_field(&context, "request")?;
                let refund_id: String = decode_field(&context, "refund_id")?;
                let refund_status = RefundStatus::parse(&remote.status).ok_or_else(|| {
                    PaymentError::Provider(format!(
                        "provider refund returned an unsupported status {:?}",
                        remote.status
                    ))
                })?;
                if let Some(existing) = self.store.refund_by_provider_id(org, &remote.id)? {
                    let response = serde_json::json!({
                        "refund_id": existing.id,
                        "credit_note_id": existing.credit_note_id,
                    });
                    self.complete_reconciled(claim, &response, |_| Ok(()), now)?;
                    return self.store.operation_claim(org, &claim.idempotency_key);
                }
                let mut payment = self
                    .store
                    .payment(org, &request.payment_id)?
                    .ok_or_else(|| PaymentError::NotFound("payment".into()))?;
                let mut invoice = self
                    .store
                    .invoice(org, &payment.invoice_id)?
                    .ok_or_else(|| PaymentError::NotFound("invoice".into()))?;
                let mut refund = Refund {
                    id: refund_id.clone(),
                    organization_id: org.to_string(),
                    payment_id: payment.id.clone(),
                    provider_refund_id: remote.id,
                    amount_minor: request.amount_minor,
                    currency: payment.currency.clone(),
                    reason: request.reason.clone(),
                    status: refund_status,
                    credit_note_id: None,
                    invoice_line_seq: None,
                    created_ms: claim.created_ms,
                };
                if refund_status == RefundStatus::Succeeded {
                    refund.invoice_line_seq = bind_refund_invoice_line(&invoice);
                }
                let credit_note = if refund_status == RefundStatus::Succeeded {
                    Some(settle_refund_ledger(
                        &mut payment,
                        &mut invoice,
                        &mut refund,
                        org,
                        format!("cn_{}", uuid::Uuid::new_v4()),
                        now,
                    )?)
                } else {
                    None
                };
                let response = serde_json::json!({
                    "refund_id": refund.id,
                    "credit_note_id": credit_note.as_ref().map(|note| note.id.clone()),
                });
                self.complete_reconciled(
                    claim,
                    &response,
                    |tx| {
                        let mut note = credit_note.clone();
                        if let Some(note) = note.as_mut() {
                            let sequence = next_sequence_tx(tx, org, "credit_note")?;
                            note.number = format!("CN-{sequence:06}");
                            insert_credit_note_tx(tx, note)?;
                        }
                        insert_refund_tx(tx, &refund)?;
                        insert_payment_tx(tx, &payment)?;
                        update_invoice_tx(tx, &invoice)
                    },
                    now,
                )?;
            }
            _ => return Ok(None),
        }
        self.store.operation_claim(org, &claim.idempotency_key)
    }

    fn complete_reconciled(
        &self,
        claim: &OperationClaim,
        response: &serde_json::Value,
        effect: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<(), PaymentError>,
        now: i64,
    ) -> Result<(), PaymentError> {
        let response = serde_json::to_string(response)
            .map_err(|e| PaymentError::Store(format!("claim response encoding: {e}")))?;
        self.store.record_atomic(
            &IdempotencyWrite {
                organization_id: claim.organization_id.clone(),
                key: claim.idempotency_key.clone(),
                operation: claim.operation.clone(),
                request_hash: claim.request_digest.clone(),
                response: response.clone(),
                created_ms: now,
            },
            effect,
        )
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
        let refund = self
            .store
            .refund_by_id(org, refund_id)?
            .ok_or_else(|| PaymentError::NotFound("refund".into()))?;
        let note_id = value
            .get("credit_note_id")
            .and_then(serde_json::Value::as_str)
            .or(refund.credit_note_id.as_deref());
        let note = match note_id {
            Some(note_id) => Some(
                self.store
                    .credit_note(org, note_id)?
                    .ok_or_else(|| PaymentError::NotFound("credit note".into()))?,
            ),
            None => None,
        };
        let payment = self
            .store
            .payment(org, &refund.payment_id)?
            .ok_or_else(|| PaymentError::NotFound("payment".into()))?;
        let invoice_id = note
            .as_ref()
            .map(|note| note.invoice_id.clone())
            .unwrap_or_else(|| payment.invoice_id.clone());
        let invoice = self
            .store
            .invoice(org, &invoice_id)?
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

    /// True while a plan/seat/cancel operation owns the subscription row
    /// across its provider call. Webhook and lifecycle writers must skip or
    /// refuse subscription effects instead of writing over that result.
    fn subscription_mutation_fence_active(
        tx: &rusqlite::Connection,
        org: &str,
        subscription_id: &str,
    ) -> Result<bool, PaymentError> {
        Ok(crate::lease::active_lease_conn(
            tx,
            org,
            &ConflictDomain::SubscriptionMutation {
                subscription_id: subscription_id.to_string(),
            },
        )?
        .is_some())
    }
}

fn provider_subscription_state(
    status: &str,
    trial_end_ms: Option<i64>,
    now: i64,
) -> Result<SubscriptionState, PaymentError> {
    if status == "trialing" {
        return Ok(SubscriptionState::Trialing);
    }
    match status {
        "active" => Ok(if trial_end_ms.is_some_and(|trial| trial > now) {
            SubscriptionState::Trialing
        } else {
            SubscriptionState::Active
        }),
        "past_due" | "unpaid" | "incomplete" | "paused" | "pending" => {
            Ok(SubscriptionState::PastDue)
        }
        "canceled" | "cancelled" | "incomplete_expired" | "expired" => {
            Ok(SubscriptionState::Canceled)
        }
        other => Err(PaymentError::Provider(format!(
            "provider subscription returned an unsupported status {other:?}"
        ))),
    }
}

fn operation_resource_id(
    prefix: &str,
    organization_id: &str,
    operation: &str,
    key: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(organization_id.as_bytes());
    hasher.update([0]);
    hasher.update(operation.as_bytes());
    hasher.update([0]);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}_{hex}")
}

fn decode_field<T: serde::de::DeserializeOwned>(
    context: &serde_json::Value,
    field: &str,
) -> Result<T, PaymentError> {
    serde_json::from_value(
        context.get(field).cloned().ok_or_else(|| {
            PaymentError::Store(format!("operation claim context has no {field}"))
        })?,
    )
    .map_err(|e| PaymentError::Store(format!("operation claim context {field}: {e}")))
}

fn payment_status_from_provider(raw: &str) -> Result<PaymentStatus, PaymentError> {
    let status = match raw {
        "succeeded" | "captured" | "authorised" | "authorized" => PaymentStatus::Succeeded,
        "processing" | "requires_capture" => PaymentStatus::Processing,
        "pending" | "received" => PaymentStatus::Pending,
        "requires_action" | "requires_confirmation" => PaymentStatus::RequiresAction,
        "failed" | "canceled" | "cancelled" => PaymentStatus::Failed,
        other => {
            return Err(PaymentError::Provider(format!(
                "provider charge returned an unsupported status {other:?}"
            )))
        }
    };
    Ok(status)
}

/// Apply the settled part of one refund to the payment and invoice and
/// build the credit note skeleton. The caller allocates its sequence in the
/// SAME transaction that inserts both rows.
fn settle_refund_ledger(
    payment: &mut Payment,
    invoice: &mut Invoice,
    refund: &mut Refund,
    organization_id: &str,
    credit_note_id: String,
    now: i64,
) -> Result<CreditNote, PaymentError> {
    if refund.status != RefundStatus::Succeeded {
        return Err(PaymentError::IllegalState(format!(
            "refund {} is not settled",
            refund.id
        )));
    }
    if refund.credit_note_id.is_some() {
        return Err(PaymentError::Conflict(format!(
            "refund {} already has a credit note",
            refund.id
        )));
    }
    payment.refunded_minor = checked_add(payment.refunded_minor, refund.amount_minor)?;
    if payment.refunded_minor > payment.amount_minor {
        return Err(PaymentError::RefundExceedsPaid {
            paid: payment.amount_minor,
            refunded: payment.refunded_minor - refund.amount_minor,
            requested: refund.amount_minor,
        });
    }
    payment.status = if payment.refunded_minor >= payment.amount_minor {
        PaymentStatus::Refunded
    } else {
        PaymentStatus::PartiallyRefunded
    };
    invoice.amount_refunded_minor =
        checked_add(invoice.amount_refunded_minor, refund.amount_minor)?;
    refund.credit_note_id = Some(credit_note_id.clone());
    Ok(CreditNote {
        id: credit_note_id,
        organization_id: organization_id.to_string(),
        number: String::new(),
        invoice_id: invoice.id.clone(),
        payment_id: payment.id.clone(),
        amount_minor: refund.amount_minor,
        currency: refund.currency.clone(),
        reason: refund.reason.clone(),
        invoice_line_seq: refund.invoice_line_seq,
        created_ms: now,
    })
}

/// Bind a refund/credit note to one authored charge line of the Faktor
/// invoice. The sequence is the Faktor invoice-line index, never a provider
/// line identity.
fn bind_refund_invoice_line(invoice: &Invoice) -> Option<u32> {
    invoice
        .lines
        .iter()
        .enumerate()
        .find(|(_, line)| {
            line.amount_minor > 0
                && matches!(
                    line.kind,
                    InvoiceLineKind::Plan
                        | InvoiceLineKind::Seat
                        | InvoiceLineKind::Proration
                        | InvoiceLineKind::Usage
                )
        })
        .map(|(seq, _)| seq as u32)
        .or_else(|| (!invoice.lines.is_empty()).then_some(0))
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
        WebhookDisposition::ReconciliationConflict => "reconciliation_conflict",
    }
}

/// Reject raw PAN/card-like input at every token boundary. A provider token
/// is opaque text (letters/digits and a small punctuation set); any 12..=19
/// digit run that is Luhn-valid is refused even when wrapped in a
/// `tok_`/`pm_` prefix, and card-like separators/whitespace are refused
/// outright. This validator is the ONE implementation used by the service,
/// CLI and native routes.
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
