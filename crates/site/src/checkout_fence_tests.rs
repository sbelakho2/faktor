// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Hosted-checkout invoice fencing: exactly one provider checkout per open
//! invoice balance, durable across crashes, replaceable only after
//! expiry/cancel/payment or a genuine balance-generation change.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::{
    BillingInterval, IssueInvoiceRequest, LocalPaymentProvider, PaymentProvider,
    ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge, ProviderChargeRequest,
    ProviderCheckout, ProviderCheckoutRequest, ProviderCustomer, ProviderCustomerRequest,
    ProviderInvoice, ProviderInvoiceRequest, ProviderRefund, ProviderRefundRequest, ProviderStatus,
    ProviderSubscribeRequest, ProviderSubscription, SecretString, SqlitePaymentStore,
    SubscribeRequest, UsageCharge, WebhookHeaders,
};
use tokio::sync::Semaphore;

use crate::billing::{self, CHECKOUT_FENCE_TTL_MS};
use crate::security::random_secret;
use crate::test_support::{extract_secret, Client, TestApp, TEST_NOW_MS, TEST_SECRET};

struct GateCheckoutProvider {
    inner: OnceLock<Arc<LocalPaymentProvider>>,
    create_calls: AtomicUsize,
    gated: AtomicBool,
    entered: Semaphore,
    release: Semaphore,
}

impl GateCheckoutProvider {
    fn new() -> Self {
        Self {
            inner: OnceLock::new(),
            create_calls: AtomicUsize::new(0),
            gated: AtomicBool::new(false),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    fn configure(&self, store: Arc<SqlitePaymentStore>, clock: Arc<ManualClock>) {
        let _ = self.inner.set(Arc::new(LocalPaymentProvider::new(
            store,
            Some(SecretString::new(TEST_SECRET)),
            clock,
        )));
    }

    fn inner(&self) -> &LocalPaymentProvider {
        self.inner.get().expect("provider is configured by TestApp")
    }

    fn create_calls(&self) -> usize {
        self.create_calls.load(Ordering::SeqCst)
    }

    fn gate(&self) {
        self.gated.store(true, Ordering::SeqCst);
    }

    async fn wait_entered(&self) {
        self.entered.acquire().await.unwrap().forget();
    }

    fn release_all(&self) {
        self.release.add_permits(64);
    }
}

#[async_trait]
impl PaymentProvider for GateCheckoutProvider {
    fn name(&self) -> &'static str {
        self.inner().name()
    }

    fn status(&self) -> ProviderStatus {
        self.inner().status()
    }

    async fn create_customer(
        &self,
        request: ProviderCustomerRequest,
    ) -> Result<ProviderCustomer, faktor_payments::PaymentError> {
        self.inner().create_customer(request).await
    }

    async fn subscribe(
        &self,
        request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, faktor_payments::PaymentError> {
        self.inner().subscribe(request).await
    }

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, faktor_payments::PaymentError> {
        self.inner().change_plan(request).await
    }

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, faktor_payments::PaymentError> {
        self.inner().cancel(request).await
    }

    async fn create_checkout(
        &self,
        request: ProviderCheckoutRequest,
    ) -> Result<ProviderCheckout, faktor_payments::PaymentError> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        if self.gated.load(Ordering::SeqCst) {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
        self.inner().create_checkout(request).await
    }

    async fn create_invoice(
        &self,
        request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, faktor_payments::PaymentError> {
        self.inner().create_invoice(request).await
    }

    async fn charge(
        &self,
        request: ProviderChargeRequest,
    ) -> Result<ProviderCharge, faktor_payments::PaymentError> {
        self.inner().charge(request).await
    }

    async fn refund(
        &self,
        request: ProviderRefundRequest,
    ) -> Result<ProviderRefund, faktor_payments::PaymentError> {
        self.inner().refund(request).await
    }

    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<faktor_payments::VerifiedWebhookEvent, faktor_payments::PaymentError> {
        self.inner().verify_webhook(headers, body, now_ms)
    }
}

fn app_with_gate() -> (TestApp, Arc<GateCheckoutProvider>) {
    let provider = Arc::new(GateCheckoutProvider::new());
    let factory = provider.clone();
    let app = TestApp::build_with_custom_provider(move |store, clock, _secrets| {
        factory.configure(store, clock);
        factory.clone() as Arc<dyn PaymentProvider>
    });
    (app, provider)
}

async fn signup_verified(app: &TestApp, email: &str) -> Client {
    let mut client = app.client();
    let page = client.get("/signup/").await;
    let csrf = client.form_csrf(&page.body);
    let response = client
        .post(
            "/signup/",
            &[
                ("csrf", csrf.as_str()),
                ("name", "Checkout Fence"),
                ("email", email),
                ("password", "correct horse battery staple"),
                ("confirm", "correct horse battery staple"),
                ("accept_terms", "1"),
            ],
        )
        .await;
    assert_eq!(response.status, 303, "{}", response.body);
    let mail = app
        .all_mails()
        .into_iter()
        .rev()
        .find(|mail| mail.to == email && mail.subject.contains("Verify"))
        .expect("verification mail");
    let secret = extract_secret(&mail.body);
    let page = client.get(&format!("/verify-email/?secret={secret}")).await;
    let csrf = client.form_csrf(&page.body);
    let response = client
        .post(
            "/verify-email/",
            &[("csrf", csrf.as_str()), ("secret", secret.as_str())],
        )
        .await;
    assert_eq!(response.status, 303);
    client
}

async fn seed_open_invoice(
    app: &TestApp,
    email: &str,
) -> (OrganizationId, faktor_payments::Invoice) {
    let org = OrganizationId::try_new(app.org_for_email(email)).unwrap();
    app.payments()
        .subscribe(
            &org,
            SubscribeRequest {
                plan_id: "solo".into(),
                interval: BillingInterval::Monthly,
                seats: 1,
                trial_days: None,
                customer_email: email.into(),
                customer_name: "Checkout Fence".into(),
                idempotency_key: "checkout-fence-subscribe".into(),
            },
        )
        .await
        .unwrap();
    let invoice = app
        .payments()
        .issue_invoice(
            &org,
            IssueInvoiceRequest {
                subscription_id: app
                    .payments()
                    .overview(&org)
                    .unwrap()
                    .subscription
                    .unwrap()
                    .id,
                usage: Vec::<UsageCharge>::new(),
                credits_minor: 0,
                discount_minor: 0,
                tax_jurisdiction: Some("NL".into()),
                due_in_days: Some(7),
                idempotency_key: "checkout-fence-invoice".into(),
            },
        )
        .await
        .unwrap()
        .invoice;
    (org, invoice)
}

async fn checkout(client: &mut Client, request_id: &str) -> String {
    let page = client.get("/account/billing/").await;
    assert_eq!(page.status, 200, "{}", page.body);
    let csrf = client.form_csrf(&page.body);
    let response = client
        .post(
            "/account/billing/checkout/",
            &[
                ("csrf", csrf.as_str()),
                ("plan_id", "solo"),
                ("interval", "monthly"),
                ("request_id", request_id),
            ],
        )
        .await;
    assert_eq!(response.status, 303, "{}", response.body);
    response
        .headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

fn checkout_rows(app: &TestApp, org: &OrganizationId, invoice_id: &str) -> i64 {
    app.state
        .db
        .conn()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM billing_checkouts WHERE organization_id=?1 AND invoice_id=?2",
            rusqlite::params![org.as_str(), invoice_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn active_fences(app: &TestApp, org: &OrganizationId, invoice_id: &str) -> i64 {
    app.state
        .db
        .conn()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM billing_checkout_fences \
             WHERE organization_id=?1 AND invoice_id=?2 AND status IN ('pending','open')",
            rusqlite::params![org.as_str(), invoice_id],
            |row| row.get(0),
        )
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_checkout_duplicate_requests_share_one_provider_object() {
    let (app, provider) = app_with_gate();
    let client = signup_verified(&app, "checkout-race@example.com").await;
    let (org, invoice) = seed_open_invoice(&app, "checkout-race@example.com").await;
    provider.gate();

    let first = random_secret();
    let second = random_secret();
    let mut left = client.clone();
    let mut right = client.clone();
    let left_key = first.clone();
    let right_key = second.clone();
    let left_task = tokio::spawn(async move { checkout(&mut left, &left_key).await });
    let right_task = tokio::spawn(async move { checkout(&mut right, &right_key).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), provider.wait_entered())
        .await
        .expect("the first checkout never reached the provider boundary");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    provider.release_all();

    let left_location = left_task.await.unwrap();
    let right_location = right_task.await.unwrap();
    assert_eq!(
        left_location, right_location,
        "two different idempotency keys for one open balance must converge on one checkout"
    );
    assert_eq!(
        provider.create_calls(),
        1,
        "the provider must create exactly one checkout object"
    );
    assert_eq!(checkout_rows(&app, &org, &invoice.id), 1);
    assert_eq!(active_fences(&app, &org, &invoice.id), 1);
}

#[tokio::test]
async fn hosted_checkout_fence_replaces_only_after_expiry_cancel_or_balance_change() {
    let (app, provider) = app_with_gate();
    let mut client = signup_verified(&app, "checkout-replace@example.com").await;
    let (org, invoice) = seed_open_invoice(&app, "checkout-replace@example.com").await;

    let first = checkout(&mut client, &random_secret()).await;
    assert_eq!(provider.create_calls(), 1);
    let same_balance = checkout(&mut client, &random_secret()).await;
    assert_eq!(
        first, same_balance,
        "the same open balance must reuse the active provider checkout"
    );
    assert_eq!(provider.create_calls(), 1);

    {
        let conn = app.state.db.conn().unwrap();
        billing::cancel_checkout_fence(&conn, org.as_str(), &invoice.id, TEST_NOW_MS).unwrap();
    }
    let after_cancel = checkout(&mut client, &random_secret()).await;
    assert_ne!(first, after_cancel);
    assert_eq!(provider.create_calls(), 2);

    app.clock.advance(CHECKOUT_FENCE_TTL_MS + 1);
    let after_expiry = checkout(&mut client, &random_secret()).await;
    assert_ne!(after_cancel, after_expiry);
    assert_eq!(provider.create_calls(), 3);

    let provider_payment_id = app
        .state
        .db
        .conn()
        .unwrap()
        .query_row(
            "SELECT provider_checkout_id FROM billing_checkouts \
             WHERE organization_id=?1 AND invoice_id=?2 ORDER BY created_ms DESC LIMIT 1",
            rusqlite::params![org.as_str(), invoice.id],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    let amount = invoice.balance_minor() - 500;
    let now_secs = app.state.now_ms() / 1000;
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_checkout_fence_partial",
        "type": "payment.succeeded",
        "created": now_secs,
        "data": {"object": {
            "id": provider_payment_id,
            "status": "succeeded",
            "amount": 500,
            "currency": invoice.currency,
            "metadata": {
                "organization_id": org.as_str(),
                "local_invoice_id": invoice.id,
            }
        }}
    }))
    .unwrap();
    let signature = faktor_payments::sign_local_webhook(TEST_SECRET, now_secs, &body);
    let outcome = app
        .payments()
        .handle_webhook(
            &WebhookHeaders::from_pairs([("x-faktor-signature", signature.as_str())]),
            &body,
            app.state.now_ms(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.disposition,
        faktor_payments::WebhookDisposition::Applied
    );
    let partially_paid = app.payments().invoice(&org, &invoice.id).unwrap();
    assert_eq!(partially_paid.amount_paid_minor, 500);
    assert_eq!(partially_paid.balance_minor(), amount);

    let after_balance = checkout(&mut client, &random_secret()).await;
    assert_ne!(after_expiry, after_balance);
    assert_eq!(provider.create_calls(), 4);
    let latest_amount: i64 = app
        .state
        .db
        .conn()
        .unwrap()
        .query_row(
            "SELECT amount_minor FROM billing_checkouts \
             WHERE organization_id=?1 AND invoice_id=?2 ORDER BY created_ms DESC LIMIT 1",
            rusqlite::params![org.as_str(), invoice.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(latest_amount, amount);
}

#[tokio::test]
async fn hosted_checkout_crash_after_fence_reconciles_without_duplicate() {
    let (app, provider) = app_with_gate();
    let mut client = signup_verified(&app, "checkout-crash@example.com").await;
    let (org, invoice) = seed_open_invoice(&app, "checkout-crash@example.com").await;
    let overview = app.payments().overview(&org).unwrap();
    let customer = overview.customer.as_ref().unwrap();
    let fence_id = billing::new_checkout_fence_id();
    let provider_key = billing::checkout_provider_key(&fence_id);
    let request = billing::checkout_request(
        &org,
        &customer.provider_customer_id,
        &invoice,
        "solo",
        &app.state.config.base_url,
        provider_key.clone(),
    )
    .unwrap();
    let provider_request = serde_json::to_string(&request).unwrap();
    let balance_generation = billing::checkout_balance_generation(&invoice);
    let request_id = random_secret();
    let draft = billing::CheckoutFenceDraft {
        id: &fence_id,
        organization_id: org.as_str(),
        invoice_id: &invoice.id,
        balance_generation: &balance_generation,
        request_id: &request_id,
        provider: app.payments().provider_kind().as_str(),
        provider_idempotency_key: &provider_key,
        provider_request: &provider_request,
        plan_id: "solo",
        interval: "monthly",
        amount_minor: invoice.balance_minor(),
        currency: &invoice.currency,
        now_ms: app.state.now_ms(),
    };
    let acquisition = {
        let mut conn = app.state.db.conn().unwrap();
        billing::acquire_checkout_fence(&mut conn, &draft).unwrap()
    };
    let fence = match acquisition {
        billing::CheckoutFenceAcquisition::Acquired(fence) => fence,
        other => panic!("expected the fence to be acquired, got {other:?}"),
    };

    // The provider effect committed, then the process died before activation.
    // The next request must reclaim the orphaned fence immediately (no live
    // owner in this process) and replay the persisted provider key.
    let provider_checkout = app.payments().create_checkout(&org, request).await.unwrap();
    assert_eq!(provider.create_calls(), 1);

    let location = checkout(&mut client, &random_secret()).await;
    assert_eq!(
        provider.create_calls(),
        1,
        "reconciliation must reuse the persisted provider request, never create a second object"
    );
    assert!(location.ends_with(&format!("/account/billing/local/{}/", fence.id)));
    assert_eq!(checkout_rows(&app, &org, &invoice.id), 1);
    assert_eq!(active_fences(&app, &org, &invoice.id), 1);
    let (status, stored_provider_id): (String, String) = app
        .state
        .db
        .conn()
        .unwrap()
        .query_row(
            "SELECT status,provider_checkout_id FROM billing_checkout_fences WHERE id=?1",
            rusqlite::params![fence.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "open");
    assert_eq!(stored_provider_id, provider_checkout.id);
}
