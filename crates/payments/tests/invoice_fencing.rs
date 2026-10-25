// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir,separate-process
//! Invoice/payment/refund resource-fence adversarial tests.
//!
//! Every case uses different idempotency keys and a provider gate/barrier so
//! the competing request reaches the fence while the winner is at the
//! provider boundary.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::{
    sign_local_webhook, AddPaymentMethodRequest, BillingInterval, ClaimAcquisition, ConflictDomain,
    CurrencySpec, DunningConfig, FinancialResourceType, Invoice, IssueInvoiceRequest,
    LeaseAcquisition, LeaseClaim, OperationClaimDraft, OperationClaimState, PayInvoiceRequest,
    Payment, PaymentError, PaymentProvider, PaymentService, PaymentStatus, PaymentsConfig,
    PlanPrice, PriceCatalogue, ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge,
    ProviderChargeRequest, ProviderCheckout, ProviderCheckoutRequest, ProviderCustomer,
    ProviderCustomerRequest, ProviderInvoice, ProviderInvoiceRequest, ProviderInvoiceSnapshot,
    ProviderKind, ProviderReconcileRequest, ProviderReconciled, ProviderRefund,
    ProviderRefundRequest, ProviderSubscribeRequest, ProviderSubscription, RefundRequest,
    RefundStatus, SqlitePaymentStore, StaticSecrets, SubscribeRequest, SubscriptionState,
    TaxConfig, TerminalOutcome, UsageCharge, VerifiedWebhookEvent, WebhookDisposition,
    WebhookHeaders,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{Barrier, Semaphore};

const ORG: &str = "org-invoice-fence";
const SECRET_ENV: &str = "FAKTOR_FENCE_WEBHOOK_SECRET";
const SECRET: &str = "whsec_fence";
const VAULT_ENV: &str = "FAKTOR_FENCE_VAULT_KEY";
const NOW: i64 = 1_800_000_000_000;
const FULL: i64 = 10_000;

fn org() -> OrganizationId {
    OrganizationId::try_new(ORG).unwrap()
}

fn config() -> PaymentsConfig {
    let mut plans = BTreeMap::new();
    plans.insert(
        "pro".into(),
        PlanPrice {
            plan_id: "pro".into(),
            currency: "USD".into(),
            monthly_minor: FULL,
            annual_minor: 100_000,
            per_seat_monthly_minor: 0,
            per_seat_annual_minor: 0,
        },
    );
    let mut currencies = BTreeMap::new();
    currencies.insert("USD".into(), CurrencySpec { exponent: 2 });
    PaymentsConfig {
        provider: ProviderKind::Local,
        catalogue: PriceCatalogue {
            plans,
            currencies,
            fx: BTreeMap::new(),
        },
        tax: TaxConfig::default(),
        dunning: DunningConfig {
            max_attempts: 2,
            grace_ms: 10_000,
        },
        webhook_secret_env: SECRET_ENV.into(),
        vault_key_env: VAULT_ENV.into(),
        stripe: Default::default(),
        mollie: Default::default(),
    }
}

fn secrets() -> Arc<StaticSecrets> {
    Arc::new(StaticSecrets::new([(
        SECRET_ENV.to_string(),
        SECRET.to_string(),
    )]))
}

async fn subscribe(service: &PaymentService, key: &str) -> faktor_payments::PaymentSubscription {
    service
        .subscribe(
            &org(),
            SubscribeRequest {
                plan_id: "pro".into(),
                interval: BillingInterval::Monthly,
                seats: 1,
                trial_days: None,
                customer_email: "fence@example.com".into(),
                customer_name: "Fence".into(),
                idempotency_key: key.into(),
            },
        )
        .await
        .unwrap()
        .subscription
}

async fn add_method(service: &PaymentService) -> faktor_payments::PaymentMethodView {
    service
        .add_payment_method(
            &org(),
            AddPaymentMethodRequest {
                token: "tok_fence".into(),
                brand: "visa".into(),
                last4: "4242".into(),
                exp_month: 12,
                exp_year: 2030,
                set_default: true,
                idempotency_key: "fence-method".into(),
            },
        )
        .unwrap()
        .payment_method
}

async fn issue(service: &PaymentService, subscription_id: &str) -> Invoice {
    service
        .issue_invoice(
            &org(),
            IssueInvoiceRequest {
                subscription_id: subscription_id.into(),
                usage: Vec::<UsageCharge>::new(),
                credits_minor: 0,
                discount_minor: 0,
                tax_jurisdiction: None,
                due_in_days: Some(7),
                idempotency_key: "fence-invoice".into(),
            },
        )
        .await
        .unwrap()
        .invoice
}

fn pay_request(invoice_id: &str, method_id: &str, key: &str) -> PayInvoiceRequest {
    PayInvoiceRequest {
        invoice_id: invoice_id.into(),
        payment_method_id: method_id.into(),
        idempotency_key: key.into(),
    }
}

fn refund_request(payment_id: &str, amount_minor: i64, key: &str) -> RefundRequest {
    RefundRequest {
        payment_id: payment_id.into(),
        amount_minor,
        reason: "fence test".into(),
        idempotency_key: key.into(),
    }
}

struct GateProvider {
    inner: faktor_payments::LocalPaymentProvider,
    charge_calls: AtomicUsize,
    refund_calls: AtomicUsize,
    charge_entered: Semaphore,
    charge_release: Semaphore,
    refund_entered: Semaphore,
    refund_release: Semaphore,
    block_charge: AtomicBool,
    block_refund: AtomicBool,
    crash_after_charge: AtomicBool,
    charge_error: Mutex<Option<PaymentError>>,
    refund_error: Mutex<Option<PaymentError>>,
    charge_status: Mutex<Option<String>>,
    refund_status: Mutex<Option<String>>,
    charge_amount: Mutex<Option<i64>>,
    charge_currency: Mutex<Option<String>>,
    refund_amount: Mutex<Option<i64>>,
    refund_currency: Mutex<Option<String>>,
    last_charge: Mutex<Option<ProviderCharge>>,
    last_refund: Mutex<Option<ProviderRefund>>,
}

impl GateProvider {
    fn new(store: Arc<SqlitePaymentStore>, clock: Arc<ManualClock>) -> Self {
        Self {
            inner: faktor_payments::LocalPaymentProvider::new(
                store,
                Some(faktor_payments::secrets::SecretString::new(SECRET)),
                clock,
            ),
            charge_calls: AtomicUsize::new(0),
            refund_calls: AtomicUsize::new(0),
            charge_entered: Semaphore::new(0),
            charge_release: Semaphore::new(0),
            refund_entered: Semaphore::new(0),
            refund_release: Semaphore::new(0),
            block_charge: AtomicBool::new(false),
            block_refund: AtomicBool::new(false),
            crash_after_charge: AtomicBool::new(false),
            charge_error: Mutex::new(None),
            refund_error: Mutex::new(None),
            charge_status: Mutex::new(None),
            refund_status: Mutex::new(None),
            charge_amount: Mutex::new(None),
            charge_currency: Mutex::new(None),
            refund_amount: Mutex::new(None),
            refund_currency: Mutex::new(None),
            last_charge: Mutex::new(None),
            last_refund: Mutex::new(None),
        }
    }

    fn block_charge(&self) {
        self.block_charge.store(true, Ordering::SeqCst);
    }

    fn block_refund(&self) {
        self.block_refund.store(true, Ordering::SeqCst);
    }

    fn crash_after_charge(&self) {
        self.crash_after_charge.store(true, Ordering::SeqCst);
    }

    fn fail_charge(&self, error: PaymentError) {
        *self.charge_error.lock().unwrap() = Some(error);
    }

    fn fail_refund(&self, error: PaymentError) {
        *self.refund_error.lock().unwrap() = Some(error);
    }

    fn set_charge_status(&self, status: &str) {
        *self.charge_status.lock().unwrap() = Some(status.to_string());
    }

    fn set_refund_status(&self, status: &str) {
        *self.refund_status.lock().unwrap() = Some(status.to_string());
    }

    fn set_charge_amount(&self, amount_minor: i64) {
        *self.charge_amount.lock().unwrap() = Some(amount_minor);
    }

    fn set_charge_currency(&self, currency: &str) {
        *self.charge_currency.lock().unwrap() = Some(currency.to_string());
    }

    fn set_refund_amount(&self, amount_minor: i64) {
        *self.refund_amount.lock().unwrap() = Some(amount_minor);
    }

    fn set_refund_currency(&self, currency: &str) {
        *self.refund_currency.lock().unwrap() = Some(currency.to_string());
    }

    async fn wait_charge(&self) {
        self.charge_entered.acquire().await.unwrap().forget();
    }

    async fn wait_refund(&self) {
        self.refund_entered.acquire().await.unwrap().forget();
    }

    fn release_charge(&self) {
        self.charge_release.add_permits(1);
    }

    fn release_refund(&self) {
        self.refund_release.add_permits(1);
    }

    fn last_charge_id(&self) -> String {
        self.last_charge
            .lock()
            .unwrap()
            .as_ref()
            .expect("a committed charge exists")
            .id
            .clone()
    }

    fn charge_calls(&self) -> usize {
        self.charge_calls.load(Ordering::SeqCst)
    }

    fn refund_calls(&self) -> usize {
        self.refund_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PaymentProvider for GateProvider {
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
        self.inner.subscribe(request).await
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
        self.charge_calls.fetch_add(1, Ordering::SeqCst);
        let injected = self.charge_error.lock().unwrap().take();
        let mut result = match injected {
            Some(error) => Err(error),
            None => self.inner.charge(request).await,
        };
        if let Ok(charge) = result.as_mut() {
            if let Some(status) = self.charge_status.lock().unwrap().clone() {
                charge.status = status;
            }
            if let Some(amount) = *self.charge_amount.lock().unwrap() {
                charge.amount_minor = amount;
            }
            if let Some(currency) = self.charge_currency.lock().unwrap().clone() {
                charge.currency = currency;
            }
            *self.last_charge.lock().unwrap() = Some(charge.clone());
        }
        if self.block_charge.load(Ordering::SeqCst) {
            self.charge_entered.add_permits(1);
            self.charge_release.acquire().await.unwrap().forget();
            if self.crash_after_charge.load(Ordering::SeqCst) {
                panic!("simulated crash after the provider effect");
            }
        }
        result
    }

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError> {
        self.refund_calls.fetch_add(1, Ordering::SeqCst);
        let injected = self.refund_error.lock().unwrap().take();
        let mut result = match injected {
            Some(error) => Err(error),
            None => self.inner.refund(request).await,
        };
        if let Ok(refund) = result.as_mut() {
            if let Some(status) = self.refund_status.lock().unwrap().clone() {
                refund.status = status;
            }
            if let Some(amount) = *self.refund_amount.lock().unwrap() {
                refund.amount_minor = amount;
            }
            if let Some(currency) = self.refund_currency.lock().unwrap().clone() {
                refund.currency = currency;
            }
            *self.last_refund.lock().unwrap() = Some(refund.clone());
        }
        if self.block_refund.load(Ordering::SeqCst) {
            self.refund_entered.add_permits(1);
            self.refund_release.acquire().await.unwrap().forget();
        }
        result
    }

    async fn fetch_invoice(
        &self,
        provider_invoice_id: &str,
    ) -> Result<ProviderInvoiceSnapshot, PaymentError> {
        self.inner.fetch_invoice(provider_invoice_id).await
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

struct Fx {
    _dir: TempDir,
    path: std::path::PathBuf,
    store: Arc<SqlitePaymentStore>,
    provider: Arc<GateProvider>,
    clock: Arc<ManualClock>,
    service: Arc<PaymentService>,
}

fn fixture() -> Fx {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("payment.db");
    let store = Arc::new(SqlitePaymentStore::open(&path).unwrap());
    let clock = Arc::new(ManualClock::new(NOW));
    let provider = Arc::new(GateProvider::new(store.clone(), clock.clone()));
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
    Fx {
        _dir: dir,
        path,
        store,
        provider,
        clock,
        service,
    }
}

async fn paid_payment(fx: &Fx) -> (Invoice, Payment) {
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    let payment = fx
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "fence-pay"))
        .await
        .unwrap()
        .payment;
    (invoice, payment)
}

fn webhook(provider_payment_id: &str, amount: i64, invoice_id: &str) -> (WebhookHeaders, Vec<u8>) {
    let now_secs = NOW / 1000;
    let body = serde_json::to_vec(&json!({
        "id": format!("evt_{provider_payment_id}"),
        "type": "payment_intent.succeeded",
        "created": now_secs,
        "data": {"object": {
            "id": provider_payment_id,
            "amount": amount,
            "status": "succeeded",
            "metadata": {
                "organization_id": ORG,
                "local_invoice_id": invoice_id,
            },
        }},
    }))
    .unwrap();
    let signature = sign_local_webhook(SECRET, now_secs, &body);
    (
        WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]),
        body,
    )
}

fn refund_webhook(provider_refund_id: &str, status: &str) -> (WebhookHeaders, Vec<u8>) {
    let now_secs = NOW / 1000;
    let body = serde_json::to_vec(&json!({
        "id": format!("evt_refund_{provider_refund_id}"),
        "type": "refund.updated",
        "created": now_secs,
        "data": {"object": {
            "id": provider_refund_id,
            "status": status,
            "metadata": {"organization_id": ORG},
        }},
    }))
    .unwrap();
    let signature = sign_local_webhook(SECRET, now_secs, &body);
    (
        WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]),
        body,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_pay_keys_charge_once() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider.block_charge();
    let start = Arc::new(Barrier::new(2));
    let mut tasks = Vec::new();
    for key in ["pay-a", "pay-b"] {
        let service = fx.service.clone();
        let start = start.clone();
        let invoice_id = invoice.id.clone();
        let method_id = method.id.clone();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            service
                .pay_invoice(&org(), pay_request(&invoice_id, &method_id, key))
                .await
        }));
    }
    fx.provider.wait_charge().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fx.provider.charge_calls(), 1);
    fx.provider.release_charge();
    let mut success = 0;
    let mut conflict = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => success += 1,
            Err(error) => {
                assert!(matches!(error, PaymentError::Conflict(_)), "{error:?}");
                conflict += 1;
            }
        }
    }
    assert_eq!((success, conflict), (1, 1));
    assert_eq!(fx.provider.charge_calls(), 1);
    assert_eq!(fx.service.payments(&org(), 10).unwrap().len(), 1);
    let stored = fx.service.invoice(&org(), &invoice.id).unwrap();
    assert_eq!(stored.amount_paid_minor, FULL);
    assert_eq!(stored.status, faktor_payments::InvoiceStatus::Paid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_full_refund_keys_refund_once() {
    let fx = fixture();
    let (_invoice, payment) = paid_payment(&fx).await;
    fx.provider.block_refund();
    let start = Arc::new(Barrier::new(2));
    let mut tasks = Vec::new();
    for key in ["refund-a", "refund-b"] {
        let service = fx.service.clone();
        let start = start.clone();
        let payment_id = payment.id.clone();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            service
                .refund(&org(), refund_request(&payment_id, FULL, key))
                .await
        }));
    }
    fx.provider.wait_refund().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fx.provider.refund_calls(), 1);
    fx.provider.release_refund();
    let mut success = 0;
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => success += 1,
            Err(error) => {
                assert!(
                    matches!(
                        error,
                        PaymentError::Conflict(_) | PaymentError::RefundExceedsPaid { .. }
                    ),
                    "{error:?}"
                );
                refused += 1;
            }
        }
    }
    assert_eq!((success, refused), (1, 1));
    assert_eq!(fx.provider.refund_calls(), 1);
    assert_eq!(
        fx.store
            .refunds_for_payment(ORG, &payment.id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_partial_refunds_never_exceed_collected() {
    let fx = fixture();
    let (_invoice, payment) = paid_payment(&fx).await;
    let start = Arc::new(Barrier::new(8));
    let mut tasks = Vec::new();
    for index in 0..8 {
        let service = fx.service.clone();
        let start = start.clone();
        let payment_id = payment.id.clone();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            service
                .refund(
                    &org(),
                    refund_request(&payment_id, 3_000, &format!("refund-{index}")),
                )
                .await
        }));
    }
    let mut accepted = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => accepted += 3_000,
            Err(error) => assert!(
                matches!(
                    error,
                    PaymentError::Conflict(_) | PaymentError::RefundExceedsPaid { .. }
                ),
                "{error:?}"
            ),
        }
    }
    assert!(accepted <= FULL);
    assert_eq!(accepted, 9_000);
    let refunds = fx.store.refunds_for_payment(ORG, &payment.id).unwrap();
    assert_eq!(refunds.len(), 3);
    assert_eq!(
        refunds
            .iter()
            .map(|refund| refund.amount_minor)
            .sum::<i64>(),
        accepted
    );
    let stored_payment = fx.service.payments(&org(), 10).unwrap().remove(0);
    assert_eq!(stored_payment.refunded_minor, accepted);
    assert_eq!(stored_payment.status, PaymentStatus::PartiallyRefunded);
}

#[tokio::test]
async fn definitive_provider_error_releases_only_after_no_effect_proof() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider
        .fail_charge(PaymentError::Declined("definitive refusal".into()));
    let first = fx
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-error"))
        .await;
    assert!(matches!(first, Err(PaymentError::Declined(_))));
    let second = fx
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-retry"))
        .await
        .unwrap();
    assert_eq!(second.payment.status, PaymentStatus::Succeeded);
    assert_eq!(fx.provider.charge_calls(), 2);
    assert_eq!(fx.service.payments(&org(), 10).unwrap().len(), 1);

    let ambiguous = fixture();
    let subscription = subscribe(&ambiguous.service, "fence-sub").await;
    let method = add_method(&ambiguous.service).await;
    let invoice = issue(&ambiguous.service, &subscription.id).await;
    ambiguous
        .provider
        .fail_charge(PaymentError::Provider("transport timeout".into()));
    let first = ambiguous
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-unknown"))
        .await;
    assert!(matches!(first, Err(PaymentError::Provider(_))));
    let blocked = ambiguous
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-blocked"))
        .await;
    assert!(matches!(blocked, Err(PaymentError::Conflict(_))));
    assert_eq!(ambiguous.provider.charge_calls(), 1);
    ambiguous
        .service
        .reconcile_unknown_claims(&org())
        .await
        .unwrap();
    let recovered = ambiguous
        .service
        .pay_invoice(
            &org(),
            pay_request(&invoice.id, &method.id, "pay-after-reconcile"),
        )
        .await
        .unwrap();
    assert_eq!(recovered.payment.status, PaymentStatus::Succeeded);
    assert_eq!(ambiguous.provider.charge_calls(), 2);
}

#[tokio::test]
async fn refund_provider_error_releases_only_after_no_effect_proof() {
    let fx = fixture();
    let (_invoice, payment) = paid_payment(&fx).await;
    fx.provider
        .fail_refund(PaymentError::Malformed("definitive refusal".into()));
    let first = fx
        .service
        .refund(&org(), refund_request(&payment.id, 2_500, "refund-error"))
        .await;
    assert!(matches!(first, Err(PaymentError::Malformed(_))));
    let second = fx
        .service
        .refund(&org(), refund_request(&payment.id, 2_500, "refund-retry"))
        .await
        .unwrap();
    assert_eq!(second.refund.status, RefundStatus::Succeeded);
    assert_eq!(fx.provider.refund_calls(), 2);
    assert_eq!(
        fx.store
            .refunds_for_payment(ORG, &payment.id)
            .unwrap()
            .len(),
        1
    );

    let ambiguous = fixture();
    let (_invoice, payment) = paid_payment(&ambiguous).await;
    ambiguous
        .provider
        .fail_refund(PaymentError::Provider("transport timeout".into()));
    let first = ambiguous
        .service
        .refund(&org(), refund_request(&payment.id, 2_500, "refund-unknown"))
        .await;
    assert!(matches!(first, Err(PaymentError::Provider(_))));
    let blocked = ambiguous
        .service
        .refund(&org(), refund_request(&payment.id, 2_500, "refund-blocked"))
        .await;
    assert!(matches!(blocked, Err(PaymentError::Conflict(_))));
    assert_eq!(ambiguous.provider.refund_calls(), 1);
    ambiguous
        .service
        .reconcile_unknown_claims(&org())
        .await
        .unwrap();
    let recovered = ambiguous
        .service
        .refund(
            &org(),
            refund_request(&payment.id, 2_500, "refund-after-reconcile"),
        )
        .await
        .unwrap();
    assert_eq!(recovered.refund.status, RefundStatus::Succeeded);
    assert_eq!(ambiguous.provider.refund_calls(), 2);
}

#[tokio::test]
async fn restart_keeps_unsettled_reservations_and_webhooks_release_them() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
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
    let provider_payment_id = charge.payment.provider_payment_id.clone();
    drop(fx.service);
    drop(fx.provider);
    drop(fx.store);

    let store = Arc::new(SqlitePaymentStore::open(&fx.path).unwrap());
    let clock = Arc::new(ManualClock::new(NOW));
    let provider = Arc::new(GateProvider::new(store.clone(), clock.clone()));
    let restarted = PaymentService::with_provider(
        store.clone(),
        provider.clone(),
        config(),
        clock.clone(),
        secrets(),
    )
    .unwrap();
    let blocked = restarted
        .pay_invoice(
            &org(),
            pay_request(&invoice.id, &method.id, "pay-other-processing"),
        )
        .await;
    assert!(matches!(blocked, Err(PaymentError::Conflict(_))));
    assert_eq!(provider.charge_calls(), 0);

    let (headers, body) = webhook(&provider_payment_id, FULL, &invoice.id);
    let settled = restarted
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap();
    assert_eq!(settled.disposition, WebhookDisposition::Applied);
    assert_eq!(
        restarted
            .invoice(&org(), &invoice.id)
            .unwrap()
            .amount_paid_minor,
        FULL
    );

    provider.set_refund_status("pending");
    let pending = restarted
        .refund(
            &org(),
            refund_request(&charge.payment.id, 4_000, "refund-pending"),
        )
        .await
        .unwrap();
    assert_eq!(pending.refund.status, RefundStatus::Pending);
    let provider_refund_id = pending.refund.provider_refund_id.clone();
    drop(restarted);
    drop(provider);
    drop(store);

    let store = Arc::new(SqlitePaymentStore::open(&fx.path).unwrap());
    let clock = Arc::new(ManualClock::new(NOW));
    let provider = Arc::new(GateProvider::new(store.clone(), clock.clone()));
    let restarted =
        PaymentService::with_provider(store.clone(), provider.clone(), config(), clock, secrets())
            .unwrap();
    let blocked = restarted
        .refund(
            &org(),
            refund_request(&charge.payment.id, 1_000, "refund-blocked-restart"),
        )
        .await;
    assert!(matches!(blocked, Err(PaymentError::Conflict(_))));
    assert_eq!(provider.refund_calls(), 0);
    assert_eq!(
        store
            .refunds_for_payment(ORG, &charge.payment.id)
            .unwrap()
            .len(),
        1
    );

    let (headers, body) = refund_webhook(&provider_refund_id, "succeeded");
    let settled = restarted
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap();
    assert_eq!(settled.disposition, WebhookDisposition::Applied);
    let next = restarted
        .refund(
            &org(),
            refund_request(&charge.payment.id, 1_000, "refund-after-settle"),
        )
        .await
        .unwrap();
    assert_eq!(next.refund.status, RefundStatus::Succeeded);
    assert_eq!(provider.refund_calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_fence_restart_blocks_duplicate_and_reconciles() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider.block_charge();
    fx.provider.crash_after_charge();
    let service = fx.service.clone();
    let invoice_id = invoice.id.clone();
    let method_id = method.id.clone();
    let task = tokio::spawn(async move {
        service
            .pay_invoice(&org(), pay_request(&invoice_id, &method_id, "pay-crash"))
            .await
    });
    fx.provider.wait_charge().await;
    assert_eq!(
        fx.store
            .operation_claim(ORG, "pay-crash")
            .unwrap()
            .unwrap()
            .state,
        OperationClaimState::Dispatched
    );
    task.abort();
    let _ = task.await;
    drop(fx.service);
    drop(fx.provider);
    drop(fx.store);

    let store = Arc::new(SqlitePaymentStore::open(&fx.path).unwrap());
    let clock = Arc::new(ManualClock::new(NOW));
    let provider = Arc::new(GateProvider::new(store.clone(), clock.clone()));
    let restarted =
        PaymentService::with_provider(store.clone(), provider.clone(), config(), clock, secrets())
            .unwrap();
    assert_eq!(
        store
            .operation_claim(ORG, "pay-crash")
            .unwrap()
            .unwrap()
            .state,
        OperationClaimState::Unknown
    );
    let duplicate = restarted
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-other"))
        .await;
    assert!(matches!(duplicate, Err(PaymentError::Conflict(_))));
    assert_eq!(provider.charge_calls(), 0);
    let resolved = restarted.reconcile_unknown_claims(&org()).await.unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(provider.charge_calls(), 0);
    let paid = restarted.invoice(&org(), &invoice.id).unwrap();
    assert_eq!(paid.amount_paid_minor, FULL);
    assert_eq!(paid.status, faktor_payments::InvoiceStatus::Paid);
    let payments = restarted.payments(&org(), 10).unwrap();
    assert_eq!(payments.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_settlement_during_in_flight_payment_does_not_double_apply() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider.block_charge();
    let service = fx.service.clone();
    let invoice_id = invoice.id.clone();
    let method_id = method.id.clone();
    let task = tokio::spawn(async move {
        service
            .pay_invoice(&org(), pay_request(&invoice_id, &method_id, "pay-webhook"))
            .await
    });
    fx.provider.wait_charge().await;
    let provider_payment_id = fx.provider.last_charge_id();
    let (headers, body) = webhook(&provider_payment_id, FULL, &invoice.id);
    let settled = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap();
    assert_eq!(settled.disposition, WebhookDisposition::Applied);
    fx.provider.release_charge();
    let paid = task.await.unwrap().unwrap();
    assert_eq!(paid.payment.provider_payment_id, provider_payment_id);
    let payments = fx.service.payments(&org(), 10).unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].amount_minor, FULL);
    assert_eq!(payments_paid(&fx, &invoice.id), FULL);
}

fn failed_payment_webhook(
    provider_payment_id: &str,
    invoice_id: &str,
) -> (WebhookHeaders, Vec<u8>) {
    let now_secs = NOW / 1000;
    let body = serde_json::to_vec(&json!({
        "id": format!("evt_failed_{provider_payment_id}"),
        "type": "invoice.payment_failed",
        "created": now_secs,
        "data": {"object": {
            "id": provider_payment_id,
            "status": "failed",
            "amount": FULL,
            "currency": "USD",
            "metadata": {
                "organization_id": ORG,
                "local_invoice_id": invoice_id,
            },
        }},
    }))
    .unwrap();
    let signature = sign_local_webhook(SECRET, now_secs, &body);
    (
        WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]),
        body,
    )
}

fn payment_failed_webhook(
    provider_payment_id: &str,
    invoice_id: &str,
) -> (WebhookHeaders, Vec<u8>) {
    let now_secs = NOW / 1000;
    let body = serde_json::to_vec(&json!({
        "id": format!("evt_payment_failed_{provider_payment_id}"),
        "type": "payment_intent.payment_failed",
        "created": now_secs,
        "data": {"object": {
            "id": provider_payment_id,
            "status": "failed",
            "amount": FULL,
            "currency": "USD",
            "metadata": {
                "organization_id": ORG,
                "local_invoice_id": invoice_id,
            },
        }},
    }))
    .unwrap();
    let signature = sign_local_webhook(SECRET, now_secs, &body);
    (
        WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]),
        body,
    )
}

fn refund_truth_webhook(
    event_id: &str,
    provider_refund_id: &str,
    local_refund_id: Option<&str>,
    amount_minor: i64,
    currency: &str,
) -> (WebhookHeaders, Vec<u8>) {
    let now_secs = NOW / 1000;
    let mut metadata = serde_json::json!({"organization_id": ORG});
    if let Some(local_refund_id) = local_refund_id {
        metadata["local_refund_id"] = serde_json::Value::String(local_refund_id.to_string());
    }
    let body = serde_json::to_vec(&json!({
        "id": event_id,
        "type": "refund.succeeded",
        "created": now_secs,
        "data": {"object": {
            "id": provider_refund_id,
            "status": "succeeded",
            "amount": amount_minor,
            "amount_minor": amount_minor,
            "currency": currency,
            "metadata": metadata,
        }},
    }))
    .unwrap();
    let signature = sign_local_webhook(SECRET, now_secs, &body);
    (
        WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]),
        body,
    )
}

#[tokio::test]
async fn refund_webhook_truth_mismatch_is_refused_before_settlement() {
    let fx = fixture();
    let (invoice, payment) = paid_payment(&fx).await;
    fx.provider.set_refund_status("pending");
    let pending = fx
        .service
        .refund(
            &org(),
            refund_request(&payment.id, 4_000, "refund-webhook-pending"),
        )
        .await
        .unwrap();
    assert_eq!(pending.refund.status, RefundStatus::Pending);
    let provider_refund_id = pending.refund.provider_refund_id.clone();

    let (headers, body) = refund_truth_webhook(
        "evt_refund_truth_amount",
        &provider_refund_id,
        None,
        4_001,
        "USD",
    );
    let error = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::Conflict(_)));

    let (headers, body) = refund_truth_webhook(
        "evt_refund_truth_currency",
        &provider_refund_id,
        None,
        4_000,
        "EUR",
    );
    let error = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::Conflict(_)));

    let (headers, body) = refund_truth_webhook(
        "evt_refund_truth_id",
        "re_foreign_refund",
        Some(&pending.refund.id),
        4_000,
        "USD",
    );
    let error = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::Conflict(_)));

    assert_eq!(
        fx.store
            .refund_by_id(ORG, &pending.refund.id)
            .unwrap()
            .unwrap()
            .status,
        RefundStatus::Pending
    );
    assert_eq!(fx.provider.refund_calls(), 1);

    let (headers, body) = refund_truth_webhook(
        "evt_refund_truth_ok",
        &provider_refund_id,
        None,
        4_000,
        "USD",
    );
    let applied = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap();
    assert_eq!(applied.disposition, WebhookDisposition::Applied);
    let settled = fx
        .store
        .refund_by_id(ORG, &pending.refund.id)
        .unwrap()
        .unwrap();
    assert_eq!(settled.status, RefundStatus::Succeeded);
    let paid = fx.service.payments(&org(), 10).unwrap();
    assert_eq!(paid[0].refunded_minor, 4_000);
    assert_eq!(
        fx.service
            .invoice(&org(), &invoice.id)
            .unwrap()
            .amount_refunded_minor,
        4_000
    );
    assert_eq!(fx.provider.refund_calls(), 1);
    let domain = ConflictDomain::PaymentRefund {
        payment_id: payment.id.clone(),
    };
    assert!(fx
        .store
        .active_for(ORG, &domain, &payment.id)
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_for_a_second_payment_during_in_flight_charge_keeps_both_receipts() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider.block_charge();
    let service = fx.service.clone();
    let invoice_id = invoice.id.clone();
    let method_id = method.id.clone();
    let task = tokio::spawn(async move {
        service
            .pay_invoice(&org(), pay_request(&invoice_id, &method_id, "pay-inflight"))
            .await
    });
    fx.provider.wait_charge().await;
    let in_flight_payment_id = fx.provider.last_charge_id();

    let second_provider_payment = "tr_checkout_second";
    let second_amount = 4_000;
    let (headers, body) = webhook(second_provider_payment, second_amount, &invoice.id);
    let settled = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap();
    assert_eq!(settled.disposition, WebhookDisposition::Applied);
    let after_webhook = fx.service.payments(&org(), 10).unwrap();
    assert_eq!(after_webhook.len(), 1);
    assert_eq!(
        after_webhook[0].provider_payment_id, second_provider_payment,
        "the webhook must not reuse the in-flight charge's local payment id"
    );
    assert_eq!(after_webhook[0].amount_minor, second_amount);
    assert_eq!(payments_paid(&fx, &invoice.id), second_amount);

    fx.provider.release_charge();
    let first = task.await.unwrap().unwrap();
    assert_eq!(first.payment.provider_payment_id, in_flight_payment_id);
    let payments = fx.service.payments(&org(), 10).unwrap();
    assert_eq!(payments.len(), 2, "both provider receipts stay durable");
    assert!(payments.iter().any(|payment| {
        payment.provider_payment_id == second_provider_payment
            && payment.amount_minor == second_amount
    }));
    assert!(payments.iter().any(|payment| {
        payment.provider_payment_id == in_flight_payment_id && payment.amount_minor == FULL
    }));
    let paid = fx.service.invoice(&org(), &invoice.id).unwrap();
    assert_eq!(paid.amount_paid_minor, FULL);
    assert_eq!(paid.status, faktor_payments::InvoiceStatus::Paid);
    assert_eq!(fx.provider.charge_calls(), 1);
    assert_eq!(fx.provider.refund_calls(), 0);
}

#[tokio::test]
async fn webhook_payment_truth_mismatch_for_a_recorded_payment_is_refused() {
    let fx = fixture();
    let (invoice, payment) = paid_payment(&fx).await;
    assert_eq!(payment.status, PaymentStatus::Succeeded);
    let (headers, body) = webhook(&payment.provider_payment_id, FULL - 1, &invoice.id);
    let refused = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .expect_err("a provider amount that contradicts the recorded payment is refused");
    assert!(matches!(refused, PaymentError::Conflict(_)));
    let payments = fx.service.payments(&org(), 10).unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0], payment);
    assert_eq!(payments_paid(&fx, &invoice.id), FULL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_payment_webhook_fails_the_pending_payment_and_releases_the_fence() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider.set_charge_status("pending");
    let pending = fx
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-pending"))
        .await
        .unwrap();
    assert_eq!(pending.payment.status, PaymentStatus::Pending);
    assert_eq!(pending.payment.amount_minor, FULL);
    assert_eq!(fx.provider.charge_calls(), 1);
    let domain = ConflictDomain::InvoicePayment {
        invoice_id: invoice.id.clone(),
    };
    assert!(fx
        .store
        .active_for(ORG, &domain, &invoice.id)
        .unwrap()
        .is_some());

    let (headers, body) = failed_payment_webhook(&pending.payment.provider_payment_id, &invoice.id);
    let failed = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap();
    assert_eq!(failed.disposition, WebhookDisposition::Applied);
    let payments = fx.service.payments(&org(), 10).unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].status, PaymentStatus::Failed);
    let after = fx.service.invoice(&org(), &invoice.id).unwrap();
    assert_eq!(after.amount_paid_minor, 0);
    assert_eq!(after.status, faktor_payments::InvoiceStatus::PastDue);
    assert!(
        fx.store
            .active_for(ORG, &domain, &invoice.id)
            .unwrap()
            .is_none(),
        "a failed payment must release the invoice-payment fence"
    );
    assert_eq!(
        fx.store.leases_needing_reconciliation(ORG).unwrap().len(),
        0
    );

    fx.provider.set_charge_status("succeeded");
    let retried = fx
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-retry"))
        .await
        .unwrap();
    assert_eq!(retried.payment.status, PaymentStatus::Succeeded);
    assert_eq!(fx.provider.charge_calls(), 2);
    let final_payments = fx.service.payments(&org(), 10).unwrap();
    assert_eq!(final_payments.len(), 2);
    let paid = fx.service.invoice(&org(), &invoice.id).unwrap();
    assert_eq!(paid.amount_paid_minor, FULL);
    assert_eq!(paid.status, faktor_payments::InvoiceStatus::Paid);
}

#[tokio::test]
async fn payment_failed_dunning_defers_under_subscription_mutation_fence() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider.set_charge_status("pending");
    let pending = fx
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-failed"))
        .await
        .unwrap();
    assert_eq!(pending.payment.status, PaymentStatus::Pending);

    let key = "payment-failed-fence-holder";
    let draft = OperationClaimDraft {
        organization_id: ORG.into(),
        idempotency_key: key.into(),
        operation: "change_plan".into(),
        resource_type: FinancialResourceType::Subscription,
        resource_id: subscription.id.clone(),
        request_digest: "payment-failed-digest".into(),
        provider_idempotency_key: format!("{key}:change-plan"),
        context: "{}".into(),
        created_ms: NOW,
    };
    assert!(matches!(
        fx.store.claim_operation(&draft).unwrap(),
        ClaimAcquisition::Acquired
    ));
    let domain = ConflictDomain::SubscriptionMutation {
        subscription_id: subscription.id.clone(),
    };
    let lease = match fx
        .store
        .try_acquire(
            ORG,
            &domain,
            &subscription.id,
            &LeaseClaim::new(key, key, "payment-failed-digest"),
            None,
        )
        .unwrap()
    {
        LeaseAcquisition::Held(lease) => lease,
        other => panic!("expected a held subscription mutation fence, got {other:?}"),
    };

    let (headers, body) = payment_failed_webhook(&pending.payment.provider_payment_id, &invoice.id);
    let refused = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .expect_err("a held plan/seat/cancel mutation must defer the failed-payment webhook");
    assert!(matches!(refused, PaymentError::Conflict(_)));
    assert_eq!(
        fx.service.invoice(&org(), &invoice.id).unwrap().status,
        faktor_payments::InvoiceStatus::Open,
        "a deferred webhook records no invoice effect"
    );
    assert_eq!(
        fx.service.payments(&org(), 10).unwrap()[0].status,
        PaymentStatus::Pending,
        "a deferred webhook records no payment effect"
    );
    let fenced = fx.store.subscription(ORG).unwrap().unwrap();
    assert_eq!(
        fenced.state,
        SubscriptionState::Active,
        "a deferred webhook never writes dunning over the fenced mutation"
    );
    assert_eq!(fenced.dunning_attempts, 0);
    assert!(fenced.grace_end_ms.is_none());
    assert!(fenced.canceled_ms.is_none());

    fx.store
        .fail_claim(ORG, key, "conflict", "test fence release", NOW)
        .unwrap();
    fx.store.release_no_effect(ORG, &lease.id).unwrap();

    let applied = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap();
    assert_eq!(
        applied.disposition,
        WebhookDisposition::Applied,
        "the unrecorded webhook must apply once on retry, never as a duplicate"
    );
    let failed_subscription = fx.store.subscription(ORG).unwrap().unwrap();
    assert_eq!(failed_subscription.state, SubscriptionState::PastDue);
    assert_eq!(failed_subscription.dunning_attempts, 1);
    assert!(failed_subscription.grace_end_ms.is_some());
    assert_eq!(
        fx.service.invoice(&org(), &invoice.id).unwrap().status,
        faktor_payments::InvoiceStatus::PastDue
    );
    assert_eq!(
        fx.service.payments(&org(), 10).unwrap()[0].status,
        PaymentStatus::Failed
    );
    assert!(fx
        .store
        .active_for(
            ORG,
            &ConflictDomain::InvoicePayment {
                invoice_id: invoice.id.clone()
            },
            &invoice.id
        )
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn provider_charge_amount_mismatch_is_refused_without_crediting() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider.set_charge_amount(FULL + 1);
    let error = fx
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-amount"))
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::Provider(_)));
    assert_eq!(fx.provider.charge_calls(), 1);
    assert_eq!(
        fx.store
            .operation_claim(ORG, "pay-amount")
            .unwrap()
            .unwrap()
            .state,
        OperationClaimState::Unknown
    );
    assert_eq!(fx.service.payments(&org(), 10).unwrap().len(), 0);
    assert_eq!(payments_paid(&fx, &invoice.id), 0);
    let domain = ConflictDomain::InvoicePayment {
        invoice_id: invoice.id.clone(),
    };
    assert!(fx
        .store
        .active_for(ORG, &domain, &invoice.id)
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn provider_charge_currency_mismatch_is_refused_without_crediting() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let method = add_method(&fx.service).await;
    let invoice = issue(&fx.service, &subscription.id).await;
    fx.provider.set_charge_currency("EUR");
    let error = fx
        .service
        .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-currency"))
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::Provider(_)));
    assert_eq!(fx.provider.charge_calls(), 1);
    assert_eq!(fx.service.payments(&org(), 10).unwrap().len(), 0);
    assert_eq!(payments_paid(&fx, &invoice.id), 0);
}

#[tokio::test]
async fn provider_refund_amount_mismatch_is_refused_and_keeps_the_reservation() {
    let fx = fixture();
    let (invoice, payment) = paid_payment(&fx).await;
    fx.provider.set_refund_amount(3_999);
    let error = fx
        .service
        .refund(&org(), refund_request(&payment.id, 4_000, "refund-amount"))
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::Provider(_)));
    assert_eq!(fx.provider.refund_calls(), 1);
    assert_eq!(
        fx.store
            .refunds_for_payment(ORG, &payment.id)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        fx.service
            .invoice(&org(), &invoice.id)
            .unwrap()
            .amount_refunded_minor,
        0
    );
    let domain = ConflictDomain::PaymentRefund {
        payment_id: payment.id.clone(),
    };
    assert!(fx
        .store
        .active_for(ORG, &domain, &payment.id)
        .unwrap()
        .is_some());
    assert_eq!(
        fx.store
            .operation_claim(ORG, "refund-amount")
            .unwrap()
            .unwrap()
            .state,
        OperationClaimState::Unknown
    );
}

#[tokio::test]
async fn provider_refund_currency_mismatch_is_refused_and_keeps_the_reservation() {
    let fx = fixture();
    let (invoice, payment) = paid_payment(&fx).await;
    fx.provider.set_refund_currency("EUR");
    let error = fx
        .service
        .refund(
            &org(),
            refund_request(&payment.id, 4_000, "refund-currency"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::Provider(_)));
    assert_eq!(fx.provider.refund_calls(), 1);
    assert_eq!(
        fx.store
            .refunds_for_payment(ORG, &payment.id)
            .unwrap()
            .len(),
        0
    );
    let current = fx.service.payments(&org(), 10).unwrap();
    assert_eq!(current[0].refunded_minor, 0);
    assert_eq!(
        fx.service
            .invoice(&org(), &invoice.id)
            .unwrap()
            .amount_refunded_minor,
        0
    );
    let domain = ConflictDomain::PaymentRefund {
        payment_id: payment.id.clone(),
    };
    assert!(fx
        .store
        .active_for(ORG, &domain, &payment.id)
        .unwrap()
        .is_some());
    assert_eq!(fx.provider.refund_calls(), 1);
}

#[tokio::test]
async fn pending_refund_commitment_blocks_a_second_refund_after_a_released_fence() {
    let fx = fixture();
    let (invoice, payment) = paid_payment(&fx).await;
    fx.provider.set_refund_status("pending");
    let pending = fx
        .service
        .refund(&org(), refund_request(&payment.id, 4_000, "refund-pending"))
        .await
        .unwrap();
    assert_eq!(pending.refund.status, RefundStatus::Pending);
    assert_eq!(fx.provider.refund_calls(), 1);
    let domain = ConflictDomain::PaymentRefund {
        payment_id: payment.id.clone(),
    };
    let lease = fx
        .store
        .active_for(ORG, &domain, &payment.id)
        .unwrap()
        .expect("the pending refund holds its reservation");
    fx.store
        .release_terminal(ORG, &lease.id, TerminalOutcome::Succeeded)
        .unwrap();

    let error = fx
        .service
        .refund(&org(), refund_request(&payment.id, 7_000, "refund-over"))
        .await
        .unwrap_err();
    assert!(matches!(error, PaymentError::RefundExceedsPaid { .. }));
    assert_eq!(
        fx.provider.refund_calls(),
        1,
        "the pending refund stays committed and the provider is never called"
    );
    assert_eq!(
        fx.store
            .refunds_for_payment(ORG, &payment.id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fx.service
            .invoice(&org(), &invoice.id)
            .unwrap()
            .amount_refunded_minor,
        0
    );
}

#[tokio::test]
async fn payment_webhook_cannot_activate_a_subscription_under_its_mutation_fence() {
    let fx = fixture();
    let subscription = subscribe(&fx.service, "fence-sub").await;
    let invoice = issue(&fx.service, &subscription.id).await;
    let mut past_due = fx.store.subscription(ORG).unwrap().unwrap();
    past_due.state = SubscriptionState::PastDue;
    past_due.grace_end_ms = Some(NOW + 60_000);
    fx.store.put_subscription(&past_due).unwrap();
    let domain = ConflictDomain::SubscriptionMutation {
        subscription_id: subscription.id.clone(),
    };
    let holder = LeaseClaim::new("webhook-holder", "webhook-holder", "digest");
    assert!(matches!(
        fx.store
            .try_acquire(ORG, &domain, &subscription.id, &holder, None)
            .unwrap(),
        LeaseAcquisition::Held(_)
    ));

    let (headers, body) = webhook("tr_settle_fenced", FULL, &invoice.id);
    let applied = fx
        .service
        .handle_webhook(&headers, &body, NOW)
        .await
        .unwrap();
    assert_eq!(applied.disposition, WebhookDisposition::Applied);
    assert_eq!(
        fx.service.invoice(&org(), &invoice.id).unwrap().status,
        faktor_payments::InvoiceStatus::Paid
    );
    assert_eq!(fx.service.payments(&org(), 10).unwrap().len(), 1);
    assert_eq!(
        fx.store.subscription(ORG).unwrap().unwrap().state,
        SubscriptionState::PastDue,
        "the webhook must not write over the fenced subscription mutation"
    );
    assert_eq!(fx.provider.charge_calls(), 0);
}

#[tokio::test]
async fn lifecycle_tick_cannot_write_under_the_subscription_fence() {
    let fx = fixture();
    let started = fx
        .service
        .subscribe(
            &org(),
            SubscribeRequest {
                plan_id: "pro".into(),
                interval: BillingInterval::Monthly,
                seats: 1,
                trial_days: Some(7),
                customer_email: "tick@example.com".into(),
                customer_name: "Tick".into(),
                idempotency_key: "tick-sub".into(),
            },
        )
        .await
        .unwrap()
        .subscription;
    assert_eq!(started.state, SubscriptionState::Trialing);
    let domain = ConflictDomain::SubscriptionMutation {
        subscription_id: started.id.clone(),
    };
    let holder = LeaseClaim::new("tick-holder", "tick-holder", "digest");
    assert!(matches!(
        fx.store
            .try_acquire(ORG, &domain, &started.id, &holder, None)
            .unwrap(),
        LeaseAcquisition::Held(_)
    ));
    fx.clock.advance(8 * 24 * 60 * 60 * 1000);
    let outcome = fx.service.tick(&org()).unwrap();
    assert!(!outcome.changed);
    assert_eq!(outcome.subscription.state, SubscriptionState::Trialing);
    let stored = fx.store.subscription(ORG).unwrap().unwrap();
    assert_eq!(stored.state, SubscriptionState::Trialing);
    assert_eq!(
        stored.current_period_start_ms,
        started.current_period_start_ms
    );
}

#[tokio::test]
async fn payment_failure_cannot_regress_a_paid_invoice_or_canceled_subscription() {
    let fx = fixture();
    let (invoice, _payment) = paid_payment(&fx).await;
    let mut subscription = fx.store.subscription(ORG).unwrap().unwrap();
    subscription.state = SubscriptionState::Canceled;
    subscription.canceled_ms = Some(NOW);
    subscription.cancel_at_period_end = false;
    fx.store.put_subscription(&subscription).unwrap();
    let returned = fx
        .service
        .record_payment_failure(&org(), &invoice.id)
        .unwrap()
        .expect("the canceled subscription is still returned");
    assert_eq!(returned.state, SubscriptionState::Canceled);
    assert_eq!(returned.dunning_attempts, subscription.dunning_attempts);
    let after_invoice = fx.service.invoice(&org(), &invoice.id).unwrap();
    assert_eq!(after_invoice.status, faktor_payments::InvoiceStatus::Paid);
    assert_eq!(after_invoice.amount_paid_minor, FULL);
    assert_eq!(
        fx.store.subscription(ORG).unwrap().unwrap().state,
        SubscriptionState::Canceled
    );
    assert_eq!(fx.provider.charge_calls(), 1);
    assert_eq!(fx.provider.refund_calls(), 0);
}

fn payments_paid(fx: &Fx, invoice_id: &str) -> i64 {
    fx.service
        .invoice(&org(), invoice_id)
        .unwrap()
        .amount_paid_minor
}
