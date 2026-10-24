//! Adversarial financial-operation claims and webhook truth tests.
//!
//! Every case deliberately crosses a crash, a duplicate delivery, an
//! out-of-order provider event or a concurrent writer. A successful claim
//! must dispatch exactly once and a webhook must never regress settled state.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use faktor_cloud::{Clock, ManualClock, OrganizationId};
use faktor_payments::provider::{ProviderCheckout, ProviderCheckoutRequest};
use faktor_payments::secrets::SecretString;
use faktor_payments::{
    sign_local_webhook, AddPaymentMethodRequest, BillingInterval, ChangePlanRequest,
    ChangeSeatsRequest, ClaimAcquisition, CurrencySpec, DunningConfig, FinancialResourceType,
    IssueInvoiceRequest, OperationClaimDraft, OperationClaimState, PayInvoiceRequest, PaymentError,
    PaymentProvider, PaymentService, PaymentStatus, PaymentsConfig, PlanPrice, PriceCatalogue,
    ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge, ProviderChargeRequest,
    ProviderCustomer, ProviderCustomerRequest, ProviderInvoice, ProviderInvoiceRequest,
    ProviderKind, ProviderReconcileRequest, ProviderReconciled, ProviderRefund,
    ProviderRefundRequest, ProviderSubscribeRequest, ProviderSubscription, RefundRequest,
    RefundStatus, SqlitePaymentStore, StaticSecrets, SubscribeRequest, TaxConfig, TaxRate,
    UsageCharge, VerifiedWebhookEvent, WebhookDisposition, WebhookHeaders,
};
use tempfile::TempDir;

const NEW_WEBHOOK: &str = "FAKTOR_CLAIMS_WEBHOOK_SECRET";
const WEBHOOK_SECRET: &str = "whsec_claims";
const NOW: i64 = 1_800_000_000_000;

fn config() -> PaymentsConfig {
    let mut plans = BTreeMap::new();
    plans.insert(
        "pro".into(),
        PlanPrice {
            plan_id: "pro".into(),
            currency: "USD".into(),
            monthly_minor: 100_000,
            annual_minor: 1_000_000,
            per_seat_monthly_minor: 0,
            per_seat_annual_minor: 0,
        },
    );
    let mut currencies = BTreeMap::new();
    currencies.insert("USD".into(), CurrencySpec { exponent: 2 });
    let mut rates = BTreeMap::new();
    rates.insert(
        "US-CA".into(),
        TaxRate {
            rate_bps: 0,
            inclusive: false,
        },
    );
    PaymentsConfig {
        provider: ProviderKind::Local,
        catalogue: PriceCatalogue {
            plans,
            currencies,
            fx: BTreeMap::new(),
        },
        tax: TaxConfig {
            default_jurisdiction: None,
            rates,
        },
        dunning: DunningConfig {
            max_attempts: 2,
            grace_ms: 10_000,
        },
        webhook_secret_env: NEW_WEBHOOK.into(),
        vault_key_env: "FAKTOR_CLAIMS_VAULT_KEY".into(),
        stripe: Default::default(),
        adyen: Default::default(),
    }
}

fn secrets() -> Arc<StaticSecrets> {
    Arc::new(StaticSecrets::new([(
        NEW_WEBHOOK.to_string(),
        WEBHOOK_SECRET.to_string(),
    )]))
}

fn org() -> OrganizationId {
    OrganizationId::try_new("org-claims").unwrap()
}

struct ScriptedProvider {
    inner: faktor_payments::LocalPaymentProvider,
    charge_statuses: Mutex<VecDeque<String>>,
    refund_statuses: Mutex<VecDeque<String>>,
    subscribe_calls: AtomicUsize,
    crash_after_subscribe: AtomicBool,
    delay: Duration,
    sequence: AtomicUsize,
}

impl ScriptedProvider {
    fn new(store: Arc<SqlitePaymentStore>, clock: Arc<ManualClock>) -> Self {
        Self {
            inner: faktor_payments::LocalPaymentProvider::new(
                store,
                Some(SecretString::new(WEBHOOK_SECRET)),
                clock,
            ),
            charge_statuses: Mutex::new(VecDeque::new()),
            refund_statuses: Mutex::new(VecDeque::new()),
            subscribe_calls: AtomicUsize::new(0),
            crash_after_subscribe: AtomicBool::new(false),
            delay: Duration::ZERO,
            sequence: AtomicUsize::new(0),
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn charge_status(&self, status: &str) {
        self.charge_statuses
            .lock()
            .unwrap()
            .push_back(status.into());
    }

    fn refund_status(&self, status: &str) {
        self.refund_statuses
            .lock()
            .unwrap()
            .push_back(status.into());
    }

    fn next_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}_{}",
            self.sequence.fetch_add(1, Ordering::SeqCst) + 1
        )
    }
}

#[async_trait]
impl PaymentProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn status(&self) -> faktor_payments::ProviderStatus {
        self.inner.status()
    }

    async fn create_customer(
        &self,
        request: ProviderCustomerRequest,
    ) -> Result<ProviderCustomer, PaymentError> {
        self.inner.create_customer(request).await
    }

    async fn subscribe(
        &self,
        request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        self.subscribe_calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let result = self.inner.subscribe(request).await;
        if self.crash_after_subscribe.swap(false, Ordering::SeqCst) {
            return Err(PaymentError::Provider(
                "simulated transport failure after provider commit".into(),
            ));
        }
        result
    }

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        self.inner.change_plan(request).await
    }

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        self.inner.cancel(request).await
    }

    async fn create_checkout(
        &self,
        request: ProviderCheckoutRequest,
    ) -> Result<ProviderCheckout, PaymentError> {
        self.inner.create_checkout(request).await
    }

    async fn create_invoice(
        &self,
        request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, PaymentError> {
        self.inner.create_invoice(request).await
    }

    async fn charge(&self, request: ProviderChargeRequest) -> Result<ProviderCharge, PaymentError> {
        let status = self
            .charge_statuses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "succeeded".into());
        Ok(ProviderCharge {
            id: self.next_id("pi"),
            status,
            amount_minor: request.amount_minor,
            currency: request.currency,
        })
    }

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError> {
        let status = self.refund_statuses.lock().unwrap().pop_front();
        match status {
            Some(status) => Ok(ProviderRefund {
                id: self.next_id("re"),
                status,
                amount_minor: request.amount_minor,
                currency: request.currency,
            }),
            None => self.inner.refund(request).await,
        }
    }

    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        self.inner.verify_webhook(headers, body, now_ms)
    }

    async fn reconcile(
        &self,
        request: ProviderReconcileRequest,
    ) -> Result<ProviderReconciled, PaymentError> {
        self.inner.reconcile(request).await
    }
}

struct Fixture {
    _dir: TempDir,
    service: Arc<PaymentService>,
    store: Arc<SqlitePaymentStore>,
    provider: Arc<ScriptedProvider>,
    clock: Arc<ManualClock>,
}

fn fixture(delay: Duration) -> Fixture {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("payment.db");
    let store = Arc::new(SqlitePaymentStore::open(&path).unwrap());
    let clock = Arc::new(ManualClock::new(NOW));
    let provider = Arc::new(ScriptedProvider::new(store.clone(), clock.clone()).with_delay(delay));
    let service = Arc::new(
        PaymentService::with_provider(
            store.clone(),
            provider.clone(),
            config(),
            clock.clone(),
            secrets(),
        )
        .unwrap(),
    );
    Fixture {
        _dir: dir,
        service,
        store,
        provider,
        clock,
    }
}

fn subscribe_request(key: &str) -> SubscribeRequest {
    SubscribeRequest {
        plan_id: "pro".into(),
        interval: BillingInterval::Monthly,
        seats: 1,
        trial_days: None,
        customer_email: "claims@example.com".into(),
        customer_name: "Claims".into(),
        idempotency_key: key.into(),
    }
}

fn invoice_request(subscription_id: &str, key: &str) -> IssueInvoiceRequest {
    IssueInvoiceRequest {
        subscription_id: subscription_id.into(),
        usage: Vec::<UsageCharge>::new(),
        credits_minor: 0,
        discount_minor: 0,
        tax_jurisdiction: None,
        due_in_days: Some(7),
        idempotency_key: key.into(),
    }
}

fn headers(body: &[u8], now_secs: i64) -> WebhookHeaders {
    let signature = sign_local_webhook(WEBHOOK_SECRET, now_secs, body);
    WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())])
}

fn event_body(id: &str, kind: &str, created_secs: i64, object: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id,
        "type": kind,
        "created": created_secs,
        "data": {"object": object},
    }))
    .unwrap()
}

async fn ready_paid_payment(
    fixture: &Fixture,
) -> (faktor_payments::Invoice, faktor_payments::Payment) {
    let organization = org();
    let subscription = fixture
        .service
        .subscribe(&organization, subscribe_request("sub-pay"))
        .await
        .unwrap()
        .subscription;
    let method = fixture
        .service
        .add_payment_method(
            &organization,
            AddPaymentMethodRequest {
                token: "tok_claims".into(),
                brand: "visa".into(),
                last4: "4242".into(),
                exp_month: 12,
                exp_year: 2030,
                set_default: true,
                idempotency_key: "method-pay".into(),
            },
        )
        .unwrap()
        .payment_method;
    let invoice = fixture
        .service
        .issue_invoice(
            &organization,
            invoice_request(&subscription.id, "invoice-pay"),
        )
        .await
        .unwrap()
        .invoice;
    let payment = fixture
        .service
        .pay_invoice(
            &organization,
            PayInvoiceRequest {
                invoice_id: invoice.id.clone(),
                payment_method_id: method.id,
                idempotency_key: "pay-1".into(),
            },
        )
        .await
        .unwrap()
        .payment;
    (invoice, payment)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_duplicate_claims_dispatch_exactly_once() {
    let fixture = fixture(Duration::from_millis(150));
    let organization = org();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let service = fixture.service.clone();
        let organization = organization.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            service
                .subscribe(&organization, subscribe_request("same-key"))
                .await
        }));
    }
    let mut successes = 0;
    let mut dispatched_conflicts = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_outcome) => {
                successes += 1;
            }
            Err(PaymentError::Conflict(message)) => {
                assert!(
                    message.contains("already") || message.contains("in flight"),
                    "unexpected conflict: {message}"
                );
                dispatched_conflicts += 1;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(fixture.provider.subscribe_calls.load(Ordering::SeqCst), 1);
    assert_eq!(successes + dispatched_conflicts, 8);
    assert!(successes >= 1);
    let claims = fixture
        .store
        .operation_claims(organization.as_str())
        .unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].state, OperationClaimState::Succeeded);
    assert_eq!(claims[0].resource_type, FinancialResourceType::Subscription);
    assert!(claims[0].dispatched_ms.is_some());
    assert!(claims[0].response.is_some());
    assert!(fixture
        .store
        .subscription(organization.as_str())
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn hosted_checkout_side_effect_is_claimed_and_replayed() {
    let fixture = fixture(Duration::ZERO);
    let organization = org();
    let subscription = fixture
        .service
        .subscribe(&organization, subscribe_request("sub-checkout"))
        .await
        .unwrap()
        .subscription;
    let invoice = fixture
        .service
        .issue_invoice(
            &organization,
            invoice_request(&subscription.id, "invoice-checkout"),
        )
        .await
        .unwrap()
        .invoice;
    let request = ProviderCheckoutRequest {
        organization_id: organization.as_str().into(),
        idempotency_key: "checkout-1".into(),
        customer_id: subscription.customer_id,
        description: "Checkout".into(),
        amount_minor: invoice.total_minor,
        currency: invoice.currency.clone(),
        success_url: "https://example.test/success".into(),
        cancel_url: "https://example.test/cancel".into(),
        local_invoice_id: Some(invoice.id.clone()),
    };
    let first = fixture
        .service
        .create_checkout(&organization, request.clone())
        .await
        .unwrap();
    let replay = fixture
        .service
        .create_checkout(&organization, request)
        .await
        .unwrap();
    assert_eq!(first.id, replay.id);
    let claim = fixture
        .store
        .operation_claim(organization.as_str(), "checkout-1")
        .unwrap()
        .unwrap();
    assert_eq!(claim.state, OperationClaimState::Succeeded);
    assert_eq!(claim.resource_type, FinancialResourceType::Checkout);
    assert_eq!(claim.provider_idempotency_key, "checkout-1:checkout");
}

#[tokio::test]
async fn crash_after_dispatch_is_reconciled_without_redispatch() {
    let fixture = fixture(Duration::ZERO);
    let organization = org();
    fixture
        .provider
        .crash_after_subscribe
        .store(true, Ordering::SeqCst);
    let error = fixture
        .service
        .subscribe(&organization, subscribe_request("sub-crash"))
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::Provider(_)));
    let claim = fixture
        .store
        .operation_claim(organization.as_str(), "sub-crash")
        .unwrap()
        .unwrap();
    assert_eq!(claim.state, OperationClaimState::Unknown);
    assert!(claim.dispatched_ms.is_some());
    assert!(fixture
        .store
        .subscription(organization.as_str())
        .unwrap()
        .is_none());
    assert_eq!(fixture.provider.subscribe_calls.load(Ordering::SeqCst), 1);
    let retry = fixture
        .service
        .subscribe(&organization, subscribe_request("sub-crash"))
        .await
        .unwrap_err();
    assert!(matches!(retry, PaymentError::Conflict(_)));
    assert_eq!(fixture.provider.subscribe_calls.load(Ordering::SeqCst), 1);

    let reopened = PaymentService::with_provider(
        fixture.store.clone(),
        fixture.provider.clone(),
        config(),
        fixture.clock.clone(),
        secrets(),
    )
    .unwrap();
    assert_eq!(
        reopened
            .store()
            .operation_claim(organization.as_str(), "sub-crash")
            .unwrap()
            .unwrap()
            .state,
        OperationClaimState::Unknown
    );
    let resolved = reopened
        .reconcile_unknown_claims(&organization)
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].state, OperationClaimState::Succeeded);
    assert_eq!(fixture.provider.subscribe_calls.load(Ordering::SeqCst), 1);
    let subscription = fixture
        .store
        .subscription(organization.as_str())
        .unwrap()
        .expect("reconciliation must materialize the provider-confirmed subscription");
    assert_eq!(subscription.id, claim.resource_id);
    assert!(subscription.provider_subscription_id.starts_with("sub_"));
}

#[tokio::test]
async fn webhooks_are_atomic_ordered_and_monotonic() {
    let fixture = fixture(Duration::ZERO);
    fixture.provider.charge_status("processing");
    let organization = org();
    let (invoice, payment) = ready_paid_payment(&fixture).await;
    assert_eq!(payment.status, PaymentStatus::Processing);
    assert_eq!(
        fixture
            .service
            .invoice(&organization, &invoice.id)
            .unwrap()
            .status,
        faktor_payments::InvoiceStatus::Open
    );
    assert_eq!(
        fixture
            .service
            .invoice(&organization, &invoice.id)
            .unwrap()
            .amount_paid_minor,
        0
    );

    let now_secs = NOW / 1000;
    let success_body = event_body(
        "evt_payment_succeeded",
        "payment_intent.succeeded",
        now_secs,
        serde_json::json!({
            "id": payment.provider_payment_id,
            "amount": invoice.total_minor,
            "status": "succeeded",
            "metadata": {"organization_id": organization.as_str()},
        }),
    );
    let first = fixture
        .service
        .handle_webhook(&headers(&success_body, now_secs), &success_body, NOW)
        .unwrap();
    assert_eq!(first.disposition, WebhookDisposition::Applied);
    let settled = fixture.service.payments(&organization, 10).unwrap();
    assert_eq!(settled[0].status, PaymentStatus::Succeeded);
    let paid = fixture.service.invoice(&organization, &invoice.id).unwrap();
    assert_eq!(paid.status, faktor_payments::InvoiceStatus::Paid);
    assert_eq!(paid.amount_paid_minor, invoice.total_minor);

    let duplicate = fixture
        .service
        .handle_webhook(&headers(&success_body, now_secs), &success_body, NOW)
        .unwrap();
    assert_eq!(duplicate.disposition, WebhookDisposition::Duplicate);
    assert_eq!(
        fixture
            .service
            .invoice(&organization, &invoice.id)
            .unwrap()
            .amount_paid_minor,
        invoice.total_minor
    );

    // A newer event that would regress a paid invoice is ignored by the
    // monotonic state rules, not just the watermark.
    let regression_body = event_body(
        "evt_payment_failed_late",
        "payment_intent.payment_failed",
        now_secs + 5,
        serde_json::json!({
            "id": settled[0].provider_payment_id,
            "status": "failed",
            "metadata": {"organization_id": organization.as_str()},
        }),
    );
    let regression = fixture
        .service
        .handle_webhook(&headers(&regression_body, now_secs), &regression_body, NOW)
        .unwrap();
    assert_eq!(regression.disposition, WebhookDisposition::Ignored);
    let still_paid = fixture.service.invoice(&organization, &invoice.id).unwrap();
    assert_eq!(still_paid.status, faktor_payments::InvoiceStatus::Paid);
    assert_eq!(still_paid.amount_paid_minor, invoice.total_minor);

    // An older event is stale and never rewrites quantity/state.
    let stale_body = event_body(
        "evt_old_processing",
        "payment_intent.processing",
        now_secs - 10,
        serde_json::json!({
            "id": settled[0].provider_payment_id,
            "metadata": {"organization_id": organization.as_str()},
        }),
    );
    let stale = fixture
        .service
        .handle_webhook(&headers(&stale_body, now_secs), &stale_body, NOW)
        .unwrap();
    assert_eq!(stale.disposition, WebhookDisposition::StaleIgnored);
    assert_eq!(
        fixture.service.payments(&organization, 10).unwrap()[0].status,
        PaymentStatus::Succeeded
    );

    let subscription = fixture
        .store
        .subscription(organization.as_str())
        .unwrap()
        .expect("subscription exists");
    assert_eq!(
        subscription.state,
        faktor_payments::SubscriptionState::Active
    );
    let trial_regression = event_body(
        "evt_subscription_trial",
        "customer.subscription.updated",
        now_secs + 6,
        serde_json::json!({
            "id": subscription.provider_subscription_id,
            "status": "trialing",
            "quantity": 1,
            "metadata": {"organization_id": organization.as_str()},
        }),
    );
    fixture
        .service
        .handle_webhook(
            &headers(&trial_regression, now_secs),
            &trial_regression,
            NOW,
        )
        .unwrap();
    assert_eq!(
        fixture
            .store
            .subscription(organization.as_str())
            .unwrap()
            .unwrap()
            .state,
        faktor_payments::SubscriptionState::Active,
        "a live subscription never regresses to trial on a late event"
    );
}

#[tokio::test]
async fn pending_and_failed_refunds_stay_unsettled_until_confirmed() {
    let fixture = fixture(Duration::ZERO);
    let organization = org();
    let (invoice, payment) = ready_paid_payment(&fixture).await;
    assert_eq!(payment.status, PaymentStatus::Succeeded);

    fixture.provider.refund_status("pending");
    let pending = fixture
        .service
        .refund(
            &organization,
            RefundRequest {
                payment_id: payment.id.clone(),
                amount_minor: 1_500,
                reason: "pending provider truth".into(),
                idempotency_key: "refund-pending".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(pending.refund.status, RefundStatus::Pending);
    assert!(pending.credit_note.is_none());
    assert_eq!(pending.payment.refunded_minor, 0);
    assert_eq!(pending.invoice.amount_refunded_minor, 0);
    let payment_id = payment.id.clone();
    let stored = fixture
        .store
        .refunds_for_payment(organization.as_str(), &payment_id)
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].status, RefundStatus::Pending);

    let now_secs = NOW / 1000;
    let missing_status = event_body(
        "evt_refund_missing_status",
        "refund.updated",
        now_secs + 1,
        serde_json::json!({
            "id": pending.refund.provider_refund_id,
            "metadata": {"organization_id": organization.as_str()},
        }),
    );
    let refused =
        fixture
            .service
            .handle_webhook(&headers(&missing_status, now_secs), &missing_status, NOW);
    assert!(matches!(refused, Err(PaymentError::WebhookMalformed(_))));
    let still_pending = fixture
        .store
        .refunds_for_payment(organization.as_str(), &payment_id)
        .unwrap();
    assert_eq!(still_pending[0].status, RefundStatus::Pending);
    assert!(still_pending[0].credit_note_id.is_none());
    assert_eq!(
        fixture.service.payments(&organization, 10).unwrap()[0].refunded_minor,
        0
    );

    let success_body = event_body(
        "evt_refund_succeeded",
        "refund.updated",
        now_secs + 1,
        serde_json::json!({
            "id": pending.refund.provider_refund_id,
            "status": "succeeded",
            "metadata": {"organization_id": organization.as_str()},
        }),
    );
    let applied = fixture
        .service
        .handle_webhook(&headers(&success_body, now_secs), &success_body, NOW)
        .unwrap();
    assert_eq!(applied.disposition, WebhookDisposition::Applied);
    let refunds = fixture
        .store
        .refunds_for_payment(organization.as_str(), &payment_id)
        .unwrap();
    assert_eq!(refunds[0].status, RefundStatus::Succeeded);
    assert!(refunds[0].credit_note_id.is_some());
    let settled_type = fixture.service.payments(&organization, 10).unwrap();
    assert_eq!(settled_type[0].status, PaymentStatus::PartiallyRefunded);
    assert_eq!(settled_type[0].refunded_minor, 1_500);
    let settled_invoice = fixture.service.invoice(&organization, &invoice.id).unwrap();
    assert_eq!(settled_invoice.amount_refunded_minor, 1_500);
    let note = fixture
        .store
        .credit_note(
            organization.as_str(),
            refunds[0].credit_note_id.as_deref().unwrap(),
        )
        .unwrap()
        .expect("confirmed refund allocates its credit note");
    assert_eq!(note.number, "CN-000001");
    let replay = fixture
        .service
        .refund(
            &organization,
            RefundRequest {
                payment_id: payment_id.clone(),
                amount_minor: 1_500,
                reason: "pending provider truth".into(),
                idempotency_key: "refund-pending".into(),
            },
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.refund.status, RefundStatus::Succeeded);
    assert_eq!(
        replay
            .credit_note
            .as_ref()
            .expect("confirmed replay exposes the credit note")
            .number,
        "CN-000001"
    );

    // Duplicate confirmation cannot allocate a second credit note.
    let duplicate = fixture
        .service
        .handle_webhook(&headers(&success_body, now_secs), &success_body, NOW)
        .unwrap();
    assert_eq!(duplicate.disposition, WebhookDisposition::Duplicate);

    fixture.provider.refund_status("failed");
    let failed = fixture
        .service
        .refund(
            &organization,
            RefundRequest {
                payment_id: payment_id.clone(),
                amount_minor: 500,
                reason: "provider failed".into(),
                idempotency_key: "refund-failed".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(failed.refund.status, RefundStatus::Failed);
    assert!(failed.credit_note.is_none());
    assert_eq!(failed.payment.refunded_minor, 1_500);
    assert_eq!(failed.invoice.amount_refunded_minor, 1_500);

    // A later "succeeded" delivery can NEVER resurrect a failed refund.
    let late_body = event_body(
        "evt_refund_late_success",
        "refund.updated",
        now_secs + 2,
        serde_json::json!({
            "id": failed.refund.provider_refund_id,
            "status": "succeeded",
            "metadata": {"organization_id": organization.as_str()},
        }),
    );
    let late = fixture
        .service
        .handle_webhook(&headers(&late_body, now_secs), &late_body, NOW)
        .unwrap();
    assert_eq!(late.disposition, WebhookDisposition::Ignored);
    let after = fixture
        .store
        .refunds_for_payment(organization.as_str(), &payment_id)
        .unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(
        after
            .iter()
            .find(|refund| refund.id == failed.refund.id)
            .unwrap()
            .status,
        RefundStatus::Failed
    );
    assert_eq!(
        fixture.service.payments(&organization, 10).unwrap()[0].refunded_minor,
        1_500
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_duplicate_webhook_allocates_exactly_one_credit_note() {
    let fixture = fixture(Duration::ZERO);
    let organization = org();
    let (invoice, payment) = ready_paid_payment(&fixture).await;
    fixture.provider.refund_status("pending");
    let pending = fixture
        .service
        .refund(
            &organization,
            RefundRequest {
                payment_id: payment.id.clone(),
                amount_minor: 2_000,
                reason: "concurrent webhook".into(),
                idempotency_key: "refund-webhook-race".into(),
            },
        )
        .await
        .unwrap();
    let now_secs = NOW / 1000;
    let body = event_body(
        "evt_refund_race",
        "refund.updated",
        now_secs + 1,
        serde_json::json!({
            "id": pending.refund.provider_refund_id,
            "status": "succeeded",
            "metadata": {"organization_id": organization.as_str()},
        }),
    );
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let service = fixture.service.clone();
        let body = body.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            service
                .handle_webhook(&headers(&body, now_secs), &body, NOW)
                .unwrap()
                .disposition
        }));
    }
    let mut applied = 0;
    let mut duplicates = 0;
    for thread in threads {
        match thread.join().unwrap() {
            WebhookDisposition::Applied => applied += 1,
            WebhookDisposition::Duplicate => duplicates += 1,
            other => panic!("unexpected disposition {other:?}"),
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(duplicates, 7);
    let refunds = fixture
        .store
        .refunds_for_payment(organization.as_str(), &payment.id)
        .unwrap();
    assert_eq!(refunds.len(), 1);
    assert_eq!(refunds[0].status, RefundStatus::Succeeded);
    assert!(refunds[0].credit_note_id.is_some());
    assert_eq!(
        fixture
            .store
            .credit_note(
                organization.as_str(),
                refunds[0].credit_note_id.as_deref().unwrap(),
            )
            .unwrap()
            .unwrap()
            .number,
        "CN-000001"
    );
    assert_eq!(
        fixture.service.payments(&organization, 10).unwrap()[0].refunded_minor,
        2_000
    );
    assert_eq!(
        fixture
            .service
            .invoice(&organization, &invoice.id)
            .unwrap()
            .amount_refunded_minor,
        2_000
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_credit_note_allocation_is_gap_free_and_unique() {
    let fixture = fixture(Duration::ZERO);
    let organization = org();
    let (invoice, payment) = ready_paid_payment(&fixture).await;
    let payment_id = payment.id.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(12));
    let mut tasks = Vec::new();
    for index in 0..12 {
        let service = fixture.service.clone();
        let organization = organization.clone();
        let barrier = barrier.clone();
        let payment_id = payment_id.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            service
                .refund(
                    &organization,
                    RefundRequest {
                        payment_id,
                        amount_minor: 1_000,
                        reason: format!("concurrent refund {index}"),
                        idempotency_key: format!("refund-concurrent-{index}"),
                    },
                )
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let refunds = fixture
        .store
        .refunds_for_payment(organization.as_str(), &payment_id)
        .unwrap();
    assert_eq!(refunds.len(), 12);
    let mut numbers: Vec<String> = refunds
        .iter()
        .filter_map(|refund| refund.credit_note_id.as_ref())
        .map(|id| {
            fixture
                .store
                .credit_note(organization.as_str(), id)
                .unwrap()
                .unwrap()
                .number
        })
        .collect();
    numbers.sort();
    let expected: Vec<String> = (1..=12).map(|n| format!("CN-{n:06}")).collect();
    assert_eq!(numbers, expected);
    let payment = fixture.service.payments(&organization, 10).unwrap();
    assert_eq!(payment[0].refunded_minor, 12_000);
    assert_eq!(payment[0].status, PaymentStatus::PartiallyRefunded);
    assert_eq!(
        fixture
            .service
            .invoice(&organization, &invoice.id)
            .unwrap()
            .amount_refunded_minor,
        12_000
    );
}

#[tokio::test]
async fn claim_is_bound_to_key_resource_digest_and_never_redispatched() {
    let fixture = fixture(Duration::ZERO);
    let organization = org();
    let first = fixture
        .service
        .subscribe(&organization, subscribe_request("claim-bound"))
        .await
        .unwrap();
    let claim = fixture
        .store
        .operation_claim(organization.as_str(), "claim-bound")
        .unwrap()
        .unwrap();
    assert_eq!(claim.state, OperationClaimState::Succeeded);
    assert_eq!(claim.resource_type, FinancialResourceType::Subscription);
    assert_eq!(claim.resource_id, first.subscription.id);
    assert_eq!(claim.operation, "subscribe");
    assert_eq!(claim.provider_idempotency_key, "claim-bound:subscribe");
    assert!(claim.created_ms <= claim.dispatched_ms.unwrap());
    assert!(claim.resolved_ms.is_some());
    assert_eq!(claim.request_digest.len(), 64);
    assert_eq!(fixture.provider.subscribe_calls.load(Ordering::SeqCst), 1);

    let replay = fixture
        .service
        .subscribe(&organization, subscribe_request("claim-bound"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(fixture.provider.subscribe_calls.load(Ordering::SeqCst), 1);

    let mut different = subscribe_request("claim-bound");
    different.seats = 2;
    assert!(matches!(
        fixture.service.subscribe(&organization, different).await,
        Err(PaymentError::IdempotencyConflict(_))
    ));

    // Crash recovery semantics at the store boundary: a dispatched claim is
    // unknown (never replayable), a merely claimed row had no effect and is
    // released so the same key may retry.
    let now = fixture.clock.now_ms();
    let dispatched = OperationClaimDraft {
        organization_id: organization.as_str().into(),
        idempotency_key: "manual-dispatched".into(),
        operation: "pay_invoice".into(),
        resource_type: FinancialResourceType::Payment,
        resource_id: "pay_manual".into(),
        request_digest: "d".repeat(64),
        provider_idempotency_key: "manual-dispatched:charge".into(),
        context: "{}".into(),
        created_ms: now,
    };
    assert_eq!(
        fixture.store.claim_operation(&dispatched).unwrap(),
        ClaimAcquisition::Acquired
    );
    fixture
        .store
        .mark_claim_dispatched(organization.as_str(), "manual-dispatched", now + 1)
        .unwrap();
    let abandoned = OperationClaimDraft {
        idempotency_key: "manual-abandoned".into(),
        resource_id: "pay_abandoned".into(),
        ..dispatched.clone()
    };
    assert_eq!(
        fixture.store.claim_operation(&abandoned).unwrap(),
        ClaimAcquisition::Acquired
    );
    let recovery = fixture.store.recover_claims(now + 2).unwrap();
    assert_eq!(recovery.unknown, 1);
    assert_eq!(recovery.released, 1);
    assert_eq!(
        fixture
            .store
            .operation_claim(organization.as_str(), "manual-dispatched")
            .unwrap()
            .unwrap()
            .state,
        OperationClaimState::Unknown
    );
    assert!(fixture
        .store
        .operation_claim(organization.as_str(), "manual-abandoned")
        .unwrap()
        .is_none());
    let reconciled = fixture
        .service
        .reconcile_unknown_claims(&organization)
        .await
        .unwrap();
    assert!(reconciled.is_empty());
    let not_applied = fixture
        .store
        .operation_claim(organization.as_str(), "manual-dispatched")
        .unwrap()
        .unwrap();
    assert_eq!(not_applied.state, OperationClaimState::Failed);
    assert_eq!(
        not_applied.failure_code.as_deref(),
        Some("payment_provider_error")
    );
    assert!(not_applied.response.is_none());
}

#[tokio::test]
async fn plan_seat_and_cancel_claims_cover_mutations() {
    let fixture = fixture(Duration::ZERO);
    let organization = org();
    let subscription = fixture
        .service
        .subscribe(&organization, subscribe_request("sub-mutations"))
        .await
        .unwrap()
        .subscription;
    let plan = fixture
        .service
        .change_plan(
            &organization,
            ChangePlanRequest {
                subscription_id: subscription.id.clone(),
                plan_id: "pro".into(),
                interval: Some(BillingInterval::Annual),
                idempotency_key: "plan-change".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(plan.subscription.interval, BillingInterval::Annual);
    let seats = fixture
        .service
        .change_seats(
            &organization,
            ChangeSeatsRequest {
                subscription_id: subscription.id.clone(),
                quantity: 3,
                idempotency_key: "seat-change".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(seats.subscription.quantity, 3);
    for key in ["plan-change", "seat-change"] {
        let claim = fixture
            .store
            .operation_claim(organization.as_str(), key)
            .unwrap()
            .unwrap();
        assert_eq!(claim.state, OperationClaimState::Succeeded);
        assert_eq!(claim.resource_type, FinancialResourceType::Subscription);
        assert_eq!(claim.resource_id, subscription.id);
    }
    assert_eq!(fixture.provider.subscribe_calls.load(Ordering::SeqCst), 1);
}
