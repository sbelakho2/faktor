// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir,separate-process
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::{
    BillingInterval, CurrencySpec, DunningConfig, InvoiceReconciliationState, InvoiceStatus,
    IssueInvoiceRequest, LocalPaymentProvider, PaymentError, PaymentProvider, PaymentService,
    PaymentsConfig, PlanPrice, PriceCatalogue, ProviderCancelRequest, ProviderChangePlanRequest,
    ProviderCharge, ProviderChargeRequest, ProviderCheckout, ProviderCheckoutRequest,
    ProviderCustomer, ProviderCustomerRequest, ProviderInvoice, ProviderInvoiceRequest,
    ProviderInvoiceSnapshot, ProviderKind, ProviderReconcileRequest, ProviderReconciled,
    ProviderRefund, ProviderRefundRequest, ProviderSubscribeRequest, ProviderSubscription,
    RefundRequest, SqlitePaymentStore, StaticSecrets, SubscribeRequest, TaxConfig, TaxRate,
    UsageCharge, VerifiedWebhookEvent, WebhookDisposition, WebhookHeaders,
};
use tempfile::TempDir;

const SECRET_ENV: &str = "FAKTOR_AUTHORITY_WEBHOOK_SECRET";
const SECRET: &str = "whsec_authority";
const START: i64 = 1_700_000_000_000;

fn config() -> PaymentsConfig {
    let mut plans = BTreeMap::new();
    plans.insert(
        "pro".into(),
        PlanPrice {
            plan_id: "pro".into(),
            currency: "USD".into(),
            monthly_minor: 10_000,
            annual_minor: 100_000,
            per_seat_monthly_minor: 2_500,
            per_seat_annual_minor: 25_000,
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
            default_jurisdiction: Some("US-CA".into()),
            rates,
        },
        dunning: DunningConfig {
            max_attempts: 2,
            grace_ms: 10_000,
        },
        webhook_secret_env: SECRET_ENV.into(),
        ..PaymentsConfig::local()
    }
}

fn secrets() -> Arc<StaticSecrets> {
    Arc::new(StaticSecrets::new([(
        SECRET_ENV.to_string(),
        SECRET.to_string(),
    )]))
}

fn org(id: &str) -> OrganizationId {
    OrganizationId::try_new(id).unwrap()
}

fn subscribe_request() -> SubscribeRequest {
    SubscribeRequest {
        plan_id: "pro".into(),
        interval: BillingInterval::Monthly,
        seats: 1,
        trial_days: None,
        customer_email: "owner@example.com".into(),
        customer_name: "Owner".into(),
        idempotency_key: "subscribe-authority".into(),
    }
}

fn invoice_request(subscription_id: &str) -> IssueInvoiceRequest {
    IssueInvoiceRequest {
        subscription_id: subscription_id.into(),
        usage: vec![UsageCharge {
            description: "usage".into(),
            quantity: 1,
            unit_amount_minor: 500,
        }],
        credits_minor: 0,
        discount_minor: 0,
        tax_jurisdiction: None,
        due_in_days: Some(7),
        idempotency_key: "invoice-authority".into(),
    }
}

fn paid_body(event_id: &str, provider_invoice: &str, amount: i64, created: i64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": event_id,
        "type": "invoice.paid",
        "created": created,
        "data": {"object": {
            "id": provider_invoice,
            "status": "paid",
            "amount": amount,
            "amount_paid": amount,
            "amount_refunded": 0,
            "currency": "usd",
            "metadata": {"organization_id": "org-authority"}
        }}
    }))
    .unwrap()
}

struct Fixture {
    _dir: TempDir,
    service: PaymentService,
    clock: Arc<ManualClock>,
    organization: OrganizationId,
    provider_invoice: String,
    invoice_id: String,
    total: i64,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let store = Arc::new(SqlitePaymentStore::open(&dir.path().join("payment.db")).unwrap());
    let service = PaymentService::open_local(store, config(), clock.clone(), secrets()).unwrap();
    let organization = org("org-authority");
    let subscription = service
        .subscribe(&organization, subscribe_request())
        .await
        .unwrap()
        .subscription;
    let invoice = service
        .issue_invoice(&organization, invoice_request(&subscription.id))
        .await
        .unwrap()
        .invoice;
    Fixture {
        _dir: dir,
        service,
        clock,
        organization,
        provider_invoice: invoice.provider_invoice_id.clone().unwrap(),
        invoice_id: invoice.id,
        total: invoice.total_minor,
    }
}

fn signed(body: &[u8]) -> WebhookHeaders {
    let header = faktor_payments::sign_local_webhook(SECRET, START / 1000, body);
    WebhookHeaders::from_pairs([("Stripe-Signature", header.as_str())])
}

#[tokio::test]
async fn matching_webhook_updates_faktor_invoice_and_observation_atomically() {
    let fixture = fixture().await;
    let body = paid_body(
        "evt_match",
        &fixture.provider_invoice,
        fixture.total,
        START / 1000,
    );
    let outcome = fixture
        .service
        .handle_webhook(&signed(&body), &body, START)
        .await
        .unwrap();
    assert_eq!(outcome.disposition, WebhookDisposition::Applied);

    let invoice = fixture
        .service
        .invoice(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(invoice.status, InvoiceStatus::Paid);
    assert_eq!(invoice.amount_paid_minor, fixture.total);
    assert_eq!(invoice.total_minor, fixture.total);
    assert_eq!(invoice.reconciliation, InvoiceReconciliationState::Matched);
    assert_eq!(
        fixture
            .service
            .payments(&fixture.organization, 10)
            .unwrap()
            .len(),
        1
    );
    let observations = fixture
        .service
        .provider_invoice_observations(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].amount_minor, fixture.total);
    assert_eq!(
        observations[0].reconciliation,
        InvoiceReconciliationState::Matched
    );
    let provider_payments = fixture
        .service
        .provider_payment_observations(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(provider_payments.len(), 1);
    assert_eq!(provider_payments[0].amount_minor, fixture.total);
    assert_eq!(provider_payments[0].currency, "USD");
}

#[tokio::test]
async fn provider_amount_mismatch_is_a_typed_conflict_and_never_rewrites_totals() {
    let fixture = fixture().await;
    let provider_amount = fixture.total + 777;
    let body = paid_body(
        "evt_mismatch",
        &fixture.provider_invoice,
        provider_amount,
        START / 1000,
    );
    let outcome = fixture
        .service
        .handle_webhook(&signed(&body), &body, START)
        .await
        .unwrap();
    assert_eq!(
        outcome.disposition,
        WebhookDisposition::ReconciliationConflict
    );

    let invoice = fixture
        .service
        .invoice(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(invoice.total_minor, fixture.total, "Faktor total unchanged");
    assert_eq!(invoice.subtotal_minor, fixture.total);
    assert_eq!(invoice.amount_paid_minor, 0);
    assert_eq!(invoice.status, InvoiceStatus::Open);
    assert_eq!(
        invoice.reconciliation,
        InvoiceReconciliationState::AmountMismatch
    );
    assert_eq!(
        fixture
            .service
            .payments(&fixture.organization, 10)
            .unwrap()
            .len(),
        0
    );
    let observations = fixture
        .service
        .provider_invoice_observations(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(observations[0].amount_minor, provider_amount);
    assert_eq!(
        observations[0].reconciliation,
        InvoiceReconciliationState::AmountMismatch
    );
}

#[tokio::test]
async fn provider_currency_mismatch_is_a_conflict_and_keeps_faktor_currency() {
    let fixture = fixture().await;
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_currency",
        "type": "invoice.paid",
        "created": START / 1000,
        "data": {"object": {
            "id": fixture.provider_invoice,
            "status": "paid",
            "amount": fixture.total,
            "amount_paid": fixture.total,
            "amount_refunded": 0,
            "currency": "eur",
            "metadata": {"organization_id": "org-authority"}
        }}
    }))
    .unwrap();
    let outcome = fixture
        .service
        .handle_webhook(&signed(&body), &body, START)
        .await
        .unwrap();
    assert_eq!(
        outcome.disposition,
        WebhookDisposition::ReconciliationConflict
    );
    let invoice = fixture
        .service
        .invoice(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(invoice.currency, "USD");
    assert_eq!(invoice.total_minor, fixture.total);
    assert_eq!(invoice.amount_paid_minor, 0);
    assert_eq!(
        invoice.reconciliation,
        InvoiceReconciliationState::CurrencyMismatch
    );
}

#[tokio::test]
async fn duplicate_and_replayed_paid_events_never_double_credit() {
    let fixture = fixture().await;
    let body = paid_body(
        "evt_paid_once",
        &fixture.provider_invoice,
        fixture.total,
        START / 1000,
    );
    fixture
        .service
        .handle_webhook(&signed(&body), &body, START)
        .await
        .unwrap();
    let duplicate = fixture
        .service
        .handle_webhook(&signed(&body), &body, START)
        .await
        .unwrap();
    assert_eq!(duplicate.disposition, WebhookDisposition::Duplicate);

    // A NEW event id for the same already-paid invoice must not add a second
    // payment or change the authored total.
    let replay = paid_body(
        "evt_paid_again",
        &fixture.provider_invoice,
        fixture.total,
        START / 1000 + 1,
    );
    fixture
        .service
        .handle_webhook(&signed(&replay), &replay, START + 5_000)
        .await
        .unwrap();
    let invoice = fixture
        .service
        .invoice(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(invoice.amount_paid_minor, fixture.total);
    assert_eq!(invoice.total_minor, fixture.total);
    assert_eq!(
        fixture
            .service
            .payments(&fixture.organization, 10)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn invoice_and_payment_events_describe_one_receipt() {
    let fixture = fixture().await;
    let provider_payment = "pi_authority_shared";
    let invoice_event = serde_json::to_vec(&serde_json::json!({
        "id": "evt_invoice_first",
        "type": "invoice.paid",
        "created": START / 1000,
        "data": {"object": {
            "id": fixture.provider_invoice.clone(),
            "status": "paid",
            "amount": fixture.total,
            "amount_paid": fixture.total,
            "amount_refunded": 0,
            "currency": "usd",
            "payment_intent": provider_payment,
            "metadata": {"organization_id": "org-authority"}
        }}
    }))
    .unwrap();
    let first = fixture
        .service
        .handle_webhook(&signed(&invoice_event), &invoice_event, START)
        .await
        .unwrap();
    assert_eq!(first.disposition, WebhookDisposition::Applied);

    let payment_event = serde_json::to_vec(&serde_json::json!({
        "id": "evt_payment_after",
        "type": "payment_intent.succeeded",
        "created": START / 1000 + 1,
        "data": {"object": {
            "id": provider_payment,
            "status": "succeeded",
            "amount": fixture.total,
            "currency": "USD",
            "metadata": {
                "organization_id": "org-authority",
                "local_invoice_id": fixture.invoice_id.clone(),
            }
        }}
    }))
    .unwrap();
    let second = fixture
        .service
        .handle_webhook(&signed(&payment_event), &payment_event, START + 1_000)
        .await
        .unwrap();
    // The payment event describes the same settled provider payment that the
    // invoice event already bound. It is authenticated and recorded, but it
    // produces no second ledger effect, so the truthful disposition is
    // `Ignored` rather than a second `Applied`.
    assert_eq!(second.disposition, WebhookDisposition::Ignored);

    let payments = fixture.service.payments(&fixture.organization, 10).unwrap();
    assert_eq!(
        payments.len(),
        1,
        "two provider descriptions of one payment must not mint two refundable receipts"
    );
    assert_eq!(payments[0].provider_payment_id, provider_payment);
    assert_eq!(payments[0].amount_minor, fixture.total);
    let invoice = fixture
        .service
        .invoice(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(invoice.status, InvoiceStatus::Paid);
    assert_eq!(invoice.amount_paid_minor, fixture.total);
    assert_eq!(invoice.total_minor, fixture.total);
}

#[tokio::test]
async fn manual_reconciliation_matches_present_and_surfaces_absent_provider_invoice() {
    let fixture = fixture().await;
    let report = fixture
        .service
        .reconcile(&fixture.organization, 10)
        .await
        .unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.matched, 1);
    assert_eq!(report.conflicts, 0);
    let invoice = fixture
        .service
        .invoice(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(invoice.reconciliation, InvoiceReconciliationState::Matched);

    // A scripted provider that cannot find the invoice produces the typed
    // `ProviderInvoiceMissing` state, never an invented total.
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let store = Arc::new(SqlitePaymentStore::open(&dir.path().join("payment.db")).unwrap());
    let local = Arc::new(LocalPaymentProvider::new(
        store.clone(),
        Some(faktor_payments::SecretString::new(SECRET)),
        clock.clone(),
    ));
    let provider = Arc::new(MissingInvoiceProvider {
        inner: local,
        calls: Mutex::new(0),
    });
    let service =
        PaymentService::with_provider(store, provider, config(), clock, secrets()).unwrap();
    let organization = org("org-missing");
    let subscription = service
        .subscribe(&organization, subscribe_request())
        .await
        .unwrap()
        .subscription;
    let invoice = service
        .issue_invoice(&organization, invoice_request(&subscription.id))
        .await
        .unwrap()
        .invoice;
    let report = service.reconcile(&organization, 10).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.missing, 1);
    assert_eq!(report.conflicts, 1);
    let stored = service.invoice(&organization, &invoice.id).unwrap();
    assert_eq!(
        stored.reconciliation,
        InvoiceReconciliationState::ProviderInvoiceMissing
    );
    assert_eq!(stored.total_minor, invoice.total_minor);
}

#[tokio::test]
async fn bounded_reconciliation_passes_rotate_across_the_whole_ledger() {
    let fixture = fixture().await;
    let organization = fixture.organization.clone();
    let subscription_id = fixture
        .service
        .invoice(&organization, &fixture.invoice_id)
        .unwrap()
        .subscription_id
        .expect("the fixture invoice is subscription-bound");
    let mut expected = vec![fixture.invoice_id.clone()];
    for index in 0..2 {
        let mut request = invoice_request(&subscription_id);
        request.usage[0].description = format!("rotating usage {index}");
        request.idempotency_key = format!("rotation-invoice-{index}");
        expected.push(
            fixture
                .service
                .issue_invoice(&organization, request)
                .await
                .unwrap()
                .invoice
                .id,
        );
    }

    // The per-pass bound is one invoice. Four passes must still reach every
    // invoice rather than re-checking the same first row forever.
    let mut seen = Vec::new();
    for _ in 0..expected.len() {
        fixture.clock.advance(1);
        let report = fixture.service.reconcile(&organization, 1).await.unwrap();
        assert_eq!(report.checked, 1);
        seen.push(report.items[0].invoice_id.clone());
    }
    seen.sort();
    expected.sort();
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn refund_and_credit_note_bind_to_a_faktor_invoice_line() {
    let fixture = fixture().await;
    let method = fixture
        .service
        .add_payment_method(
            &fixture.organization,
            faktor_payments::AddPaymentMethodRequest {
                token: "tok_authority".into(),
                brand: "visa".into(),
                last4: "4242".into(),
                exp_month: 12,
                exp_year: 2030,
                set_default: true,
                idempotency_key: "method-authority".into(),
            },
        )
        .unwrap()
        .payment_method;
    let paid = fixture
        .service
        .pay_invoice(
            &fixture.organization,
            faktor_payments::PayInvoiceRequest {
                invoice_id: fixture.invoice_id.clone(),
                payment_method_id: method.id,
                idempotency_key: "pay-authority".into(),
            },
        )
        .await
        .unwrap();
    let refunded = fixture
        .service
        .refund(
            &fixture.organization,
            RefundRequest {
                payment_id: paid.payment.id,
                amount_minor: 100,
                reason: "line binding".into(),
                idempotency_key: "refund-authority".into(),
            },
        )
        .await
        .unwrap();
    let invoice = fixture
        .service
        .invoice(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    let line = refunded
        .credit_note
        .as_ref()
        .and_then(|note| note.invoice_line_seq)
        .expect("credit note names a Faktor invoice line");
    assert!((line as usize) < invoice.lines.len());
    assert!(invoice.lines[line as usize].amount_minor > 0);
    assert_eq!(refunded.refund.invoice_line_seq, Some(line));
    let provider_refunds = fixture
        .service
        .provider_refund_observations(&fixture.organization, &fixture.invoice_id)
        .unwrap();
    assert_eq!(provider_refunds.len(), 1);
    assert_eq!(provider_refunds[0].amount_minor, 100);
}

/// Delegates every provider call to the real local provider except the
/// read-only invoice fetch, which always reports `NotFound`.
struct MissingInvoiceProvider {
    inner: Arc<LocalPaymentProvider>,
    calls: Mutex<usize>,
}

#[async_trait]
impl PaymentProvider for MissingInvoiceProvider {
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

    async fn fetch_invoice(
        &self,
        _provider_invoice_id: &str,
    ) -> Result<ProviderInvoiceSnapshot, PaymentError> {
        *self.calls.lock().unwrap() += 1;
        Err(PaymentError::NotFound("provider invoice".into()))
    }

    async fn charge(&self, request: ProviderChargeRequest) -> Result<ProviderCharge, PaymentError> {
        self.inner.charge(request).await
    }

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError> {
        self.inner.refund(request).await
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
