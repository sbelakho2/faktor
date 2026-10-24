//! Different-idempotency-key fences for subscription creation, plan/seat
//! changes, cancellation and period invoices.
//!
//! Every case races distinct keys through a barrier and asserts the external
//! call ledger, not just local state. Crash recovery uses a commit-then-fail
//! provider reopened over the same durable store.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::{
    sign_local_webhook, BillingInterval, ChangePlanRequest, ChangeSeatsRequest, ConflictDomain,
    CurrencySpec, DunningConfig, Invoice, LocalPaymentProvider, PaymentError, PaymentProvider,
    PaymentService, PaymentsConfig, PlanPrice, PriceCatalogue, ProviderCancelRequest,
    ProviderChangePlanRequest, ProviderCharge, ProviderChargeRequest, ProviderCheckout,
    ProviderCheckoutRequest, ProviderCustomer, ProviderCustomerRequest, ProviderInvoice,
    ProviderInvoiceRequest, ProviderKind, ProviderReconcileRequest, ProviderReconciled,
    ProviderRefund, ProviderRefundRequest, ProviderStatus, ProviderSubscribeRequest,
    ProviderSubscription, SqlitePaymentStore, StaticSecrets, SubscribeRequest, SubscriptionState,
    TaxConfig, VerifiedWebhookEvent, WebhookDisposition, WebhookHeaders,
};
use tempfile::TempDir;
use tokio::sync::{watch, Barrier};

const WEBHOOK_SECRET: &str = "whsec_subscription_fencing";
const WEBHOOK_ENV: &str = "FAKTOR_SUBSCRIPTION_FENCING_WEBHOOK";
const START_MS: i64 = 1_800_000_000_000;
const ORG: &str = "org-subscription-fencing";

fn org() -> OrganizationId {
    OrganizationId::try_new(ORG).unwrap()
}

fn secrets() -> Arc<StaticSecrets> {
    Arc::new(StaticSecrets::new([(
        WEBHOOK_ENV.to_string(),
        WEBHOOK_SECRET.to_string(),
    )]))
}

fn config() -> PaymentsConfig {
    let mut plans = BTreeMap::new();
    plans.insert(
        "pro".into(),
        PlanPrice {
            plan_id: "pro".into(),
            currency: "USD".into(),
            monthly_minor: 10_000,
            annual_minor: 100_000,
            per_seat_monthly_minor: 0,
            per_seat_annual_minor: 0,
        },
    );
    plans.insert(
        "team".into(),
        PlanPrice {
            plan_id: "team".into(),
            currency: "USD".into(),
            monthly_minor: 20_000,
            annual_minor: 200_000,
            per_seat_monthly_minor: 0,
            per_seat_annual_minor: 0,
        },
    );
    PaymentsConfig {
        provider: ProviderKind::Local,
        catalogue: PriceCatalogue {
            plans,
            currencies: [("USD".to_string(), CurrencySpec { exponent: 2 })]
                .into_iter()
                .collect(),
            fx: BTreeMap::new(),
        },
        tax: TaxConfig {
            default_jurisdiction: None,
            rates: BTreeMap::new(),
        },
        dunning: DunningConfig {
            max_attempts: 2,
            grace_ms: 10_000,
        },
        webhook_secret_env: WEBHOOK_ENV.into(),
        vault_key_env: "FAKTOR_SUBSCRIPTION_FENCING_VAULT".into(),
        stripe: Default::default(),
        mollie: Default::default(),
    }
}

fn subscribe_request(key: &str) -> SubscribeRequest {
    SubscribeRequest {
        plan_id: "pro".into(),
        interval: BillingInterval::Monthly,
        seats: 1,
        trial_days: None,
        customer_email: "fencing@example.com".into(),
        customer_name: "Fencing".into(),
        idempotency_key: key.into(),
    }
}

fn invoice_request(subscription_id: &str, key: &str) -> faktor_payments::IssueInvoiceRequest {
    faktor_payments::IssueInvoiceRequest {
        subscription_id: subscription_id.into(),
        usage: Vec::new(),
        credits_minor: 0,
        discount_minor: 0,
        tax_jurisdiction: None,
        due_in_days: Some(7),
        idempotency_key: key.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CallKind {
    Subscribe,
    ChangePlan,
    Cancel,
    Invoice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallRecord {
    kind: CallKind,
    idempotency_key: String,
    quantity: Option<u32>,
    plan_id: Option<String>,
    committed: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct GateState {
    blocking: bool,
    opened: bool,
    arrivals: usize,
}

#[derive(Debug)]
struct CallGate {
    state: watch::Sender<GateState>,
}

impl CallGate {
    fn new() -> Self {
        let (state, _receiver) = watch::channel(GateState::default());
        Self { state }
    }

    fn block(&self) {
        self.state.send_modify(|state| state.blocking = true);
    }

    fn open(&self) {
        self.state.send_modify(|state| state.opened = true);
    }

    async fn wait_for_arrivals(&self, expected: usize) {
        let mut receiver = self.state.subscribe();
        while receiver.borrow_and_update().arrivals < expected {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    async fn arrive(&self) {
        let mut receiver = self.state.subscribe();
        self.state
            .send_modify(|state| state.arrivals = state.arrivals.saturating_add(1));
        let gate = *receiver.borrow_and_update();
        if !gate.blocking || gate.opened {
            return;
        }
        while !receiver.borrow_and_update().opened {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

struct TestProvider {
    inner: LocalPaymentProvider,
    records: Mutex<Vec<CallRecord>>,
    gates: BTreeMap<CallKind, Arc<CallGate>>,
}

impl TestProvider {
    fn new(store: Arc<SqlitePaymentStore>, clock: Arc<ManualClock>) -> Self {
        let gates = [
            CallKind::Subscribe,
            CallKind::ChangePlan,
            CallKind::Cancel,
            CallKind::Invoice,
        ]
        .into_iter()
        .map(|kind| (kind, Arc::new(CallGate::new())))
        .collect();
        Self {
            inner: LocalPaymentProvider::new(
                store,
                Some(faktor_payments::secrets::SecretString::new(WEBHOOK_SECRET)),
                clock,
            ),
            records: Mutex::new(Vec::new()),
            gates,
        }
    }

    fn gate(&self, kind: CallKind) -> Arc<CallGate> {
        self.gates.get(&kind).expect("gate").clone()
    }

    fn block_before(&self, kind: CallKind) -> Arc<CallGate> {
        let gate = self.gate(kind);
        gate.block();
        gate
    }

    fn begin(
        &self,
        kind: CallKind,
        key: &str,
        quantity: Option<u32>,
        plan_id: Option<String>,
    ) -> usize {
        let mut records = self.records.lock().unwrap();
        records.push(CallRecord {
            kind,
            idempotency_key: key.to_string(),
            quantity,
            plan_id,
            committed: false,
        });
        records.len() - 1
    }

    fn commit(&self, index: usize) {
        self.records.lock().unwrap()[index].committed = true;
    }

    fn committed(&self, kind: CallKind) -> Vec<CallRecord> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.kind == kind && record.committed)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl PaymentProvider for TestProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn status(&self) -> ProviderStatus {
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
        let index = self.begin(
            CallKind::Subscribe,
            &request.idempotency_key,
            Some(request.quantity),
            Some(request.plan_id.clone()),
        );
        self.gate(CallKind::Subscribe).arrive().await;
        let result = self.inner.subscribe(request).await;
        if result.is_ok() {
            self.commit(index);
        }
        result
    }

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let index = self.begin(
            CallKind::ChangePlan,
            &request.idempotency_key,
            Some(request.quantity),
            Some(request.plan_id.clone()),
        );
        self.gate(CallKind::ChangePlan).arrive().await;
        let result = self.inner.change_plan(request).await;
        if result.is_ok() {
            self.commit(index);
        }
        result
    }

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let index = self.begin(CallKind::Cancel, &request.idempotency_key, None, None);
        self.gate(CallKind::Cancel).arrive().await;
        let result = self.inner.cancel(request).await;
        if result.is_ok() {
            self.commit(index);
        }
        result
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
        let index = self.begin(CallKind::Invoice, &request.idempotency_key, None, None);
        self.gate(CallKind::Invoice).arrive().await;
        let result = self.inner.create_invoice(request).await;
        if result.is_ok() {
            self.commit(index);
        }
        result
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

struct Fixture {
    _dir: TempDir,
    store: Arc<SqlitePaymentStore>,
    provider: Arc<TestProvider>,
    service: Arc<PaymentService>,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("payment.db");
        let store = Arc::new(SqlitePaymentStore::open(&path).unwrap());
        let clock = Arc::new(ManualClock::new(START_MS));
        let provider = Arc::new(TestProvider::new(store.clone(), clock.clone()));
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
        Self {
            _dir: dir,
            store,
            provider,
            service,
        }
    }

    async fn subscribe(&self) -> faktor_payments::PaymentSubscription {
        self.service
            .subscribe(&org(), subscribe_request("initial-subscribe"))
            .await
            .expect("initial subscribe")
            .subscription
    }

    fn subscription(&self) -> faktor_payments::PaymentSubscription {
        self.store.subscription(ORG).unwrap().unwrap()
    }

    fn provider_change(&self, idempotency_key: &str) -> ProviderSubscription {
        let payload = self
            .store
            .provider_object(self.provider.name(), "change", ORG, idempotency_key)
            .unwrap()
            .expect("provider change object is missing")
            .1;
        let object = payload.get("object").cloned().unwrap_or(payload);
        serde_json::from_value(object).unwrap()
    }

    fn provider_subscription(&self, provider_subscription_id: &str) -> ProviderSubscription {
        let payload = self
            .store
            .provider_object(
                self.provider.name(),
                "subscription_by_id",
                ORG,
                provider_subscription_id,
            )
            .unwrap()
            .expect("provider subscription object is missing")
            .1;
        let object = payload.get("object").cloned().unwrap_or(payload);
        serde_json::from_value(object).unwrap()
    }
}

/// Wait until the losing different-key caller has been refused while the
/// winner is still parked at the provider gate. Opening the gate immediately
/// after the first arrival lets a slow loser acquire the fence AFTER the
/// winner settles, which turns a real fencing test into a scheduling test.
async fn wait_until_one_finished<A, B>(
    first: &tokio::task::JoinHandle<A>,
    second: &tokio::task::JoinHandle<B>,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !(first.is_finished() || second.is_finished()) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the losing mutation is refused while the winner holds the fence");
}

fn assert_typed_refusal(error: &PaymentError, context: &str) {
    assert!(
        matches!(
            error,
            PaymentError::Conflict(_)
                | PaymentError::IllegalState(_)
                | PaymentError::IdempotencyConflict(_)
        ),
        "{context}: expected a typed refusal, got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_provider_subscription_object_upserts_reflect_plan_seat_and_cancel() {
    let fx = Fixture::new();
    let subscription = fx.subscribe().await;
    let provider_id = subscription.provider_subscription_id.clone();
    let initial = fx.provider_subscription(&provider_id);
    assert_eq!(initial.quantity, 1);
    assert_eq!(initial.status, "active");
    let initial_version = fx
        .store
        .provider_object_version(fx.provider.name(), "subscription_by_id", ORG, &provider_id)
        .unwrap()
        .expect("subscription_by_id row exists")
        .0;

    let seats = fx
        .service
        .change_seats(
            &org(),
            ChangeSeatsRequest {
                subscription_id: subscription.id.clone(),
                quantity: 5,
                idempotency_key: "object-upsert-seats".into(),
            },
        )
        .await
        .expect("seat change applies");
    assert_eq!(seats.subscription.quantity, 5);
    let after_seats = fx.provider_subscription(&provider_id);
    assert_eq!(
        after_seats.quantity, 5,
        "change_seats must update subscription_by_id, not keep the original quantity"
    );
    assert_eq!(after_seats.status, "active");
    let seats_version = fx
        .store
        .provider_object_version(fx.provider.name(), "subscription_by_id", ORG, &provider_id)
        .unwrap()
        .expect("subscription_by_id row exists")
        .0;
    assert!(
        seats_version > initial_version,
        "the provider object version advances on a newer observation"
    );

    fx.service
        .change_plan(
            &org(),
            ChangePlanRequest {
                subscription_id: subscription.id.clone(),
                plan_id: "team".into(),
                interval: None,
                idempotency_key: "object-upsert-plan".into(),
            },
        )
        .await
        .expect("plan change applies");
    let after_plan = fx.provider_subscription(&provider_id);
    assert_eq!(after_plan.quantity, 5);
    assert_eq!(after_plan.status, "active");

    fx.service
        .cancel(
            &org(),
            faktor_payments::CancelRequest {
                subscription_id: subscription.id.clone(),
                at_period_end: false,
                idempotency_key: "object-upsert-cancel".into(),
            },
        )
        .await
        .expect("cancel applies");
    let canceled = fx.provider_subscription(&provider_id);
    assert_eq!(
        canceled.status, "canceled",
        "cancel must update subscription_by_id, not keep the original status"
    );
    assert_eq!(canceled.quantity, 5, "cancel never loses the seat count");
    let canceled_version = fx
        .store
        .provider_object_version(fx.provider.name(), "subscription_by_id", ORG, &provider_id)
        .unwrap()
        .expect("subscription_by_id row exists")
        .0;
    assert!(canceled_version > seats_version);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replayed_subscription_webhook_never_regresses_a_newer_seat_state() {
    let fx = Fixture::new();
    let subscription = fx.subscribe().await;
    let created_secs = START_MS / 1000 + 1;
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_replayed_subscription_update",
        "type": "customer.subscription.updated",
        "created": created_secs,
        "data": {"object": {
            "id": subscription.provider_subscription_id,
            "status": "active",
            "quantity": 7,
            "cancel_at_period_end": false,
            "metadata": {"organization_id": ORG},
        }},
    }))
    .unwrap();
    let signature = sign_local_webhook(WEBHOOK_SECRET, created_secs, &body);
    let headers = WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]);
    let now = created_secs * 1000;
    let first = fx
        .service
        .handle_webhook(&headers, &body, now)
        .await
        .expect("first delivery applies");
    assert_eq!(first.disposition, WebhookDisposition::Applied);
    assert_eq!(fx.subscription().quantity, 7);

    fx.service
        .change_seats(
            &org(),
            ChangeSeatsRequest {
                subscription_id: subscription.id.clone(),
                quantity: 3,
                idempotency_key: "webhook-replay-seats".into(),
            },
        )
        .await
        .expect("newer seat mutation applies");
    assert_eq!(fx.subscription().quantity, 3);
    let provider_after_seats = fx.provider_subscription(&subscription.provider_subscription_id);
    assert_eq!(provider_after_seats.quantity, 3);

    let replay = fx
        .service
        .handle_webhook(&headers, &body, now)
        .await
        .expect("replay is recognized");
    assert_eq!(replay.disposition, WebhookDisposition::Duplicate);
    assert_eq!(
        fx.subscription().quantity,
        3,
        "a replayed webhook never restores the older quantity"
    );
    assert_eq!(
        fx.provider_subscription(&subscription.provider_subscription_id)
            .quantity,
        3,
        "the durable provider object still carries the newer seat state"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_object_observations_are_versioned_for_every_financial_resource() {
    let store = SqlitePaymentStore::open_in_memory().unwrap();
    for kind in [
        "customer",
        "subscription",
        "subscription_by_id",
        "payment",
        "charge",
        "refund",
        "mandate",
    ] {
        let key = format!("{kind}:object");
        let first = store
            .put_provider_object_observed(
                "local",
                kind,
                ORG,
                &key,
                &key,
                &serde_json::json!({"object": {"status": "open", "quantity": 1}}),
                10,
            )
            .unwrap();
        assert!(first.applied, "{kind}: the first observation applies");
        assert_eq!(first.version, 1, "{kind}: first version");

        let newer = store
            .put_provider_object_observed(
                "local",
                kind,
                ORG,
                &key,
                &key,
                &serde_json::json!({"object": {"status": "paid", "quantity": 5}}),
                20,
            )
            .unwrap();
        assert!(newer.applied, "{kind}: a newer observation applies");
        assert_eq!(newer.version, 2, "{kind}: the version advances");

        let stale = store
            .put_provider_object_observed(
                "local",
                kind,
                ORG,
                &key,
                &key,
                &serde_json::json!({"object": {"status": "open", "quantity": 1}}),
                15,
            )
            .unwrap();
        assert!(!stale.applied, "{kind}: a stale observation is refused");
        assert_eq!(stale.version, 2, "{kind}: the stale write never regresses");
        let (_, payload) = store
            .provider_object("local", kind, ORG, &key)
            .unwrap()
            .expect("object exists");
        assert_eq!(
            payload.pointer("/object/status").and_then(|v| v.as_str()),
            Some("paid"),
            "{kind}: the durable payload is the newer observation"
        );
        assert_eq!(
            payload.pointer("/object/quantity").and_then(|v| v.as_u64()),
            Some(5)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn older_provider_observations_never_replace_newer_webhook_truth() {
    use faktor_payments::{
        InvoiceReconciliationState, ProviderInvoiceObservation, ProviderObservationSource,
        ProviderPaymentObservation, ProviderRefundObservation,
    };
    let store = SqlitePaymentStore::open_in_memory().unwrap();
    let newer = ProviderPaymentObservation {
        provider: "local".into(),
        provider_payment_id: "pi_observed".into(),
        organization_id: ORG.into(),
        invoice_id: "inv_observed".into(),
        payment_id: Some("pay_observed".into()),
        status: "succeeded".into(),
        amount_minor: 500,
        refunded_minor: 0,
        currency: "USD".into(),
        paid_at_ms: Some(20),
        observed_ms: 20,
        source: ProviderObservationSource::Webhook,
    };
    store.put_provider_payment_observation(&newer).unwrap();
    let older = ProviderPaymentObservation {
        status: "failed".into(),
        amount_minor: 100,
        observed_ms: 10,
        source: ProviderObservationSource::Reconciliation,
        ..newer.clone()
    };
    store.put_provider_payment_observation(&older).unwrap();
    let payments = store
        .provider_payment_observations(ORG, "inv_observed")
        .unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].status, "succeeded");
    assert_eq!(payments[0].amount_minor, 500);
    assert_eq!(payments[0].observed_ms, 20);

    let refund_new = ProviderRefundObservation {
        provider: "local".into(),
        provider_refund_id: "re_observed".into(),
        organization_id: ORG.into(),
        invoice_id: "inv_observed".into(),
        payment_id: "pay_observed".into(),
        status: "succeeded".into(),
        amount_minor: 200,
        currency: "USD".into(),
        observed_ms: 20,
        source: ProviderObservationSource::Webhook,
    };
    store.put_provider_refund_observation(&refund_new).unwrap();
    store
        .put_provider_refund_observation(&ProviderRefundObservation {
            status: "failed".into(),
            amount_minor: 50,
            observed_ms: 10,
            ..refund_new
        })
        .unwrap();
    let refunds = store
        .provider_refund_observations(ORG, "inv_observed")
        .unwrap();
    assert_eq!(refunds.len(), 1);
    assert_eq!(refunds[0].status, "succeeded");
    assert_eq!(refunds[0].amount_minor, 200);

    let invoice_new = ProviderInvoiceObservation {
        provider: "local".into(),
        provider_invoice_id: "in_observed".into(),
        organization_id: ORG.into(),
        invoice_id: "inv_observed".into(),
        status: "paid".into(),
        amount_minor: 500,
        amount_paid_minor: 500,
        amount_refunded_minor: 0,
        currency: "USD".into(),
        paid_at_ms: Some(20),
        observed_ms: 20,
        source: ProviderObservationSource::Webhook,
        reconciliation: InvoiceReconciliationState::Matched,
    };
    store
        .put_provider_invoice_observation(&invoice_new)
        .unwrap();
    store
        .put_provider_invoice_observation(&ProviderInvoiceObservation {
            status: "open".into(),
            amount_minor: 500,
            amount_paid_minor: 0,
            paid_at_ms: None,
            observed_ms: 10,
            source: ProviderObservationSource::Reconciliation,
            reconciliation: InvoiceReconciliationState::Unreconciled,
            ..invoice_new
        })
        .unwrap();
    let invoices = store
        .provider_invoice_observations(ORG, "inv_observed")
        .unwrap();
    assert_eq!(invoices.len(), 1);
    assert_eq!(invoices[0].status, "paid");
    assert_eq!(invoices[0].observed_ms, 20);
    assert_eq!(
        invoices[0].reconciliation,
        InvoiceReconciliationState::Matched
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_key_subscribes_create_exactly_one_provider_subscription() {
    let fx = Fixture::new();
    let gate = fx.provider.block_before(CallKind::Subscribe);
    let start = Arc::new(Barrier::new(2));
    let task = |key: &'static str, start: Arc<Barrier>| {
        let service = fx.service.clone();
        let organization = org();
        async move {
            start.wait().await;
            let result = service
                .subscribe(&organization, subscribe_request(key))
                .await;
            (key, result)
        }
    };
    let first = tokio::spawn(task("subscribe-key-a", start.clone()));
    let second = tokio::spawn(task("subscribe-key-b", start.clone()));
    tokio::time::timeout(Duration::from_secs(5), gate.wait_for_arrivals(1))
        .await
        .expect("one subscribe reaches the provider");
    wait_until_one_finished(&first, &second).await;
    gate.open();
    let results = [first.await.unwrap(), second.await.unwrap()];

    let successes = results.iter().filter(|(_, result)| result.is_ok()).count();
    assert_eq!(successes, 1, "exactly one different-key subscribe applies");
    for (key, result) in &results {
        if let Err(error) = result {
            assert_typed_refusal(error, key);
        }
    }
    assert_eq!(
        fx.provider.committed(CallKind::Subscribe).len(),
        1,
        "the losing key never crosses the provider boundary"
    );
    let local = fx.subscription();
    assert_eq!(local.state, SubscriptionState::Active);
    let remote = fx.provider_subscription(&local.provider_subscription_id);
    assert_eq!(remote.quantity, local.quantity);
    assert_eq!(remote.item_id, local.provider_item_id);
    assert_eq!(remote.price_id, local.provider_price_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_key_plan_and_seat_changes_are_fenced_without_lost_update() {
    let fx = Fixture::new();
    let subscription = fx.subscribe().await;
    let gate = fx.provider.block_before(CallKind::ChangePlan);
    let start = Arc::new(Barrier::new(2));
    let plan = {
        let service = fx.service.clone();
        let organization = org();
        let subscription_id = subscription.id.clone();
        let start = start.clone();
        tokio::spawn(async move {
            start.wait().await;
            service
                .change_plan(
                    &organization,
                    ChangePlanRequest {
                        subscription_id,
                        plan_id: "team".into(),
                        interval: None,
                        idempotency_key: "plan-race-key".into(),
                    },
                )
                .await
        })
    };
    let seats = {
        let service = fx.service.clone();
        let organization = org();
        let subscription_id = subscription.id.clone();
        let start = start.clone();
        tokio::spawn(async move {
            start.wait().await;
            service
                .change_seats(
                    &organization,
                    ChangeSeatsRequest {
                        subscription_id,
                        quantity: 5,
                        idempotency_key: "seat-race-key".into(),
                    },
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_for_arrivals(1))
        .await
        .expect("one different-key mutation reaches the provider");
    wait_until_one_finished(&plan, &seats).await;
    gate.open();
    let plan = plan.await.unwrap();
    let seats = seats.await.unwrap();
    if let Err(error) = &plan {
        assert_typed_refusal(error, "plan");
    }
    if let Err(error) = &seats {
        assert_typed_refusal(error, "seats");
    }
    let successes = usize::from(plan.is_ok()) + usize::from(seats.is_ok());
    assert_eq!(
        successes, 1,
        "one mutation applies and the other is a typed conflict"
    );
    assert_eq!(
        fx.provider.committed(CallKind::ChangePlan).len(),
        1,
        "the losing mutation never reaches the provider"
    );
    let local = fx.subscription();
    let provider_calls = fx.provider.committed(CallKind::ChangePlan);
    assert_eq!(provider_calls.len(), 1);
    assert_eq!(
        provider_calls[0].quantity,
        Some(local.quantity),
        "no lost seat count in the provider request"
    );
    let provider_change = fx.provider_change(&provider_calls[0].idempotency_key);
    assert_eq!(provider_change.quantity, local.quantity);
    assert_eq!(provider_change.item_id, local.provider_item_id);
    assert_eq!(provider_change.price_id, local.provider_price_id);
    if plan.is_ok() {
        assert_eq!(local.plan_id, "team");
        assert_eq!(local.quantity, subscription.quantity);
    } else {
        assert_eq!(local.plan_id, subscription.plan_id);
        assert_eq!(local.quantity, 5);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_wins_deterministically_and_upgrade_cannot_resurrect() {
    let fx = Fixture::new();
    let subscription = fx.subscribe().await;
    let gate = fx.provider.block_before(CallKind::Cancel);
    let cancel = {
        let service = fx.service.clone();
        let organization = org();
        let subscription_id = subscription.id.clone();
        tokio::spawn(async move {
            service
                .cancel(
                    &organization,
                    faktor_payments::CancelRequest {
                        subscription_id,
                        at_period_end: false,
                        idempotency_key: "cancel-race-key".into(),
                    },
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_for_arrivals(1))
        .await
        .expect("cancel reaches the provider while holding the fence");

    let upgrade = fx
        .service
        .change_plan(
            &org(),
            ChangePlanRequest {
                subscription_id: subscription.id.clone(),
                plan_id: "team".into(),
                interval: None,
                idempotency_key: "upgrade-during-cancel".into(),
            },
        )
        .await;
    assert_typed_refusal(
        upgrade
            .as_ref()
            .expect_err("cancel fence refuses the upgrade"),
        "upgrade during cancel",
    );
    assert_eq!(fx.provider.committed(CallKind::ChangePlan).len(), 0);

    gate.open();
    cancel.await.unwrap().expect("cancel applies");
    assert_eq!(fx.subscription().state, SubscriptionState::Canceled);

    let resurrection = fx
        .service
        .change_plan(
            &org(),
            ChangePlanRequest {
                subscription_id: subscription.id.clone(),
                plan_id: "team".into(),
                interval: None,
                idempotency_key: "upgrade-after-cancel".into(),
            },
        )
        .await;
    assert!(
        resurrection.is_err(),
        "a canceled subscription can never be resurrected by an upgrade"
    );
    assert_eq!(fx.subscription().state, SubscriptionState::Canceled);
    assert_eq!(fx.provider.committed(CallKind::ChangePlan).len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_period_invoice_with_different_keys_issues_one() {
    let fx = Fixture::new();
    let subscription = fx.subscribe().await;
    let gate = fx.provider.block_before(CallKind::Invoice);
    let start = Arc::new(Barrier::new(2));
    let issue = |key: &'static str, start: Arc<Barrier>| {
        let service = fx.service.clone();
        let organization = org();
        let subscription_id = subscription.id.clone();
        async move {
            start.wait().await;
            let result = service
                .issue_invoice(&organization, invoice_request(&subscription_id, key))
                .await;
            (key, result)
        }
    };
    let first = tokio::spawn(issue("period-invoice-a", start.clone()));
    let second = tokio::spawn(issue("period-invoice-b", start.clone()));
    tokio::time::timeout(Duration::from_secs(5), gate.wait_for_arrivals(1))
        .await
        .expect("one period invoice reaches the provider");
    wait_until_one_finished(&first, &second).await;
    gate.open();
    let results = [first.await.unwrap(), second.await.unwrap()];
    let successes = results.iter().filter(|(_, result)| result.is_ok()).count();
    assert!(successes >= 1, "the period invoice is issued exactly once");
    for (key, result) in &results {
        if let Err(error) = result {
            assert_typed_refusal(error, key);
        }
    }
    assert_eq!(
        fx.provider.committed(CallKind::Invoice).len(),
        1,
        "only one key crosses the provider boundary"
    );
    assert_eq!(
        fx.service.invoices(&org(), None, 10).unwrap().items.len(),
        1
    );

    let issued: Invoice = results
        .iter()
        .find_map(|(_, result)| result.as_ref().ok().map(|outcome| outcome.invoice.clone()))
        .expect("one invoice outcome");
    for (_, result) in &results {
        if let Ok(outcome) = result {
            assert_eq!(outcome.invoice.id, issued.id);
        }
    }
    let replay = fx
        .service
        .issue_invoice(
            &org(),
            invoice_request(&subscription.id, "period-invoice-c"),
        )
        .await
        .expect("the durable period key replays the invoice");
    assert!(replay.replayed);
    assert_eq!(replay.invoice.id, issued.id);
    assert_eq!(fx.provider.committed(CallKind::Invoice).len(), 1);
    assert_eq!(
        fx.service.invoices(&org(), None, 10).unwrap().items.len(),
        1
    );
}

struct CrashAfterCommitProvider {
    inner: LocalPaymentProvider,
    subscribe_calls: Arc<AtomicUsize>,
    crash_once: AtomicUsize,
}

impl CrashAfterCommitProvider {
    fn new(
        store: Arc<SqlitePaymentStore>,
        clock: Arc<ManualClock>,
        subscribe_calls: Arc<AtomicUsize>,
        crash: bool,
    ) -> Self {
        Self {
            inner: LocalPaymentProvider::new(
                store,
                Some(faktor_payments::secrets::SecretString::new(WEBHOOK_SECRET)),
                clock,
            ),
            subscribe_calls,
            crash_once: AtomicUsize::new(usize::from(crash)),
        }
    }
}

#[async_trait]
impl PaymentProvider for CrashAfterCommitProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn status(&self) -> ProviderStatus {
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
        let remote = self.inner.subscribe(request).await?;
        if self.crash_once.swap(0, Ordering::SeqCst) == 1 {
            return Err(PaymentError::Provider(
                "simulated crash after the provider committed".into(),
            ));
        }
        Ok(remote)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_fence_keeps_the_fence_and_reconciles_without_a_second_effect() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("payment.db");
    let clock = Arc::new(ManualClock::new(START_MS));
    let calls = Arc::new(AtomicUsize::new(0));
    let organization = org();
    {
        let store = Arc::new(SqlitePaymentStore::open(&path).unwrap());
        let provider = Arc::new(CrashAfterCommitProvider::new(
            store.clone(),
            clock.clone(),
            calls.clone(),
            true,
        ));
        let service =
            PaymentService::with_provider(store, provider, config(), clock.clone(), secrets())
                .unwrap();
        let error = service
            .subscribe(&organization, subscribe_request("crash-fence-key"))
            .await
            .expect_err("the simulated crash hides the provider outcome");
        assert!(matches!(error, PaymentError::Provider(_)));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let store = Arc::new(SqlitePaymentStore::open(&path).unwrap());
    let held = store
        .active_for(
            organization.as_str(),
            &ConflictDomain::Subscription {
                organization_id: organization.as_str().to_string(),
            },
            organization.as_str(),
        )
        .unwrap();
    assert!(held.is_some(), "restart never silently frees the fence");

    let provider = Arc::new(CrashAfterCommitProvider::new(
        store.clone(),
        clock.clone(),
        calls.clone(),
        false,
    ));
    let restarted =
        PaymentService::with_provider(store.clone(), provider, config(), clock, secrets()).unwrap();

    let duplicate = restarted
        .subscribe(&organization, subscribe_request("different-after-crash"))
        .await;
    assert_typed_refusal(
        duplicate
            .as_ref()
            .expect_err("the held fence refuses the retry"),
        "retry after crash",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no second external subscription before reconciliation"
    );

    let resolved = restarted
        .reconcile_unknown_claims(&organization)
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let local = store.subscription(organization.as_str()).unwrap().unwrap();
    assert_eq!(local.state, SubscriptionState::Active);
    assert!(
        store
            .active_for(
                organization.as_str(),
                &ConflictDomain::Subscription {
                    organization_id: organization.as_str().to_string(),
                },
                organization.as_str(),
            )
            .unwrap()
            .is_none(),
        "reconciliation releases the fence after the terminal outcome"
    );

    let after = restarted
        .subscribe(&organization, subscribe_request("yet-another-key"))
        .await;
    assert_typed_refusal(
        after
            .as_ref()
            .expect_err("the reconciled subscription blocks creation"),
        "subscribe after reconciliation",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscription_webhook_during_in_flight_mutation_is_ordered() {
    let fx = Fixture::new();
    let subscription = fx.subscribe().await;
    let gate = fx.provider.block_before(CallKind::ChangePlan);
    let plan = {
        let service = fx.service.clone();
        let organization = org();
        let subscription_id = subscription.id.clone();
        tokio::spawn(async move {
            service
                .change_plan(
                    &organization,
                    ChangePlanRequest {
                        subscription_id,
                        plan_id: "team".into(),
                        interval: None,
                        idempotency_key: "webhook-plan-key".into(),
                    },
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_for_arrivals(1))
        .await
        .expect("plan change is in flight");

    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_subscription_updated_1",
        "type": "customer.subscription.updated",
        "created": START_MS / 1000,
        "data": {"object": {
            "id": subscription.provider_subscription_id,
            "status": "active",
            "quantity": 7,
            "cancel_at_period_end": false,
            "metadata": {"organization_id": ORG},
        }},
    }))
    .unwrap();
    let signature = sign_local_webhook(WEBHOOK_SECRET, START_MS / 1000, &body);
    let headers = WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]);
    let blocked = fx
        .service
        .handle_webhook(&headers, &body, START_MS)
        .await
        .expect_err("the in-flight mutation fence refuses the webhook");
    assert_typed_refusal(&blocked, "webhook during mutation");

    gate.open();
    plan.await
        .unwrap()
        .expect("plan applies after the webhook was refused");
    assert_eq!(fx.subscription().plan_id, "team");

    let applied = fx
        .service
        .handle_webhook(&headers, &body, START_MS)
        .await
        .expect("the retried webhook applies after the fence releases");
    assert_eq!(applied.disposition, WebhookDisposition::Applied);
    let local = fx.subscription();
    assert_eq!(local.plan_id, "team");
    assert_eq!(local.quantity, 7, "the webhook update is not lost");
}
