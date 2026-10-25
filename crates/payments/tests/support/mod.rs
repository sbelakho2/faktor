#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use faktor_cloud::{Clock, ManualClock, OrganizationId};
use faktor_payments::{
    sign_local_webhook, AddPaymentMethodRequest, BillingInterval, CancelRequest, ChangePlanRequest,
    ChangeSeatsRequest, CurrencySpec, DunningConfig, Invoice, IssueInvoiceRequest,
    PayInvoiceOutcome, PayInvoiceRequest, Payment, PaymentError, PaymentProvider, PaymentService,
    PaymentSubscription, PaymentsConfig, PlanPrice, PriceCatalogue, ProviderCancelRequest,
    ProviderChangePlanRequest, ProviderCharge, ProviderChargeRequest, ProviderCheckout,
    ProviderCheckoutRequest, ProviderCustomer, ProviderCustomerRequest, ProviderInvoice,
    ProviderInvoiceRequest, ProviderInvoiceSnapshot, ProviderKind, ProviderReconcileRequest,
    ProviderReconciled, ProviderRefund, ProviderRefundRequest, ProviderStatus,
    ProviderSubscribeRequest, ProviderSubscription, RefundOutcome, RefundRequest,
    SqlitePaymentStore, StaticSecrets, SubscribeRequest, TaxConfig, TaxRate, UsageCharge,
    VerifiedWebhookEvent, WebhookHeaders, WebhookOutcome,
};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio::task::JoinSet;

pub const WEBHOOK_SECRET: &str = "whsec_financial_races";
pub const WEBHOOK_ENV: &str = "FAKTOR_RACE_WEBHOOK_SECRET";
pub const VAULT_ENV: &str = "FAKTOR_RACE_VAULT_KEY";
pub const ORG: &str = "org-financial-races";
pub const START_MS: i64 = 1_800_000_000_000;

pub fn org() -> OrganizationId {
    OrganizationId::try_new(ORG).unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CallKind {
    Customer,
    Subscribe,
    ChangePlan,
    Cancel,
    Checkout,
    Invoice,
    FetchInvoice,
    Charge,
    Refund,
    Reconcile,
}

impl CallKind {
    const ALL: [CallKind; 10] = [
        CallKind::Customer,
        CallKind::Subscribe,
        CallKind::ChangePlan,
        CallKind::Cancel,
        CallKind::Checkout,
        CallKind::Invoice,
        CallKind::FetchInvoice,
        CallKind::Charge,
        CallKind::Refund,
        CallKind::Reconcile,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            CallKind::Customer => "customer",
            CallKind::Subscribe => "subscribe",
            CallKind::ChangePlan => "change_plan",
            CallKind::Cancel => "cancel",
            CallKind::Checkout => "checkout",
            CallKind::Invoice => "invoice",
            CallKind::FetchInvoice => "fetch_invoice",
            CallKind::Charge => "charge",
            CallKind::Refund => "refund",
            CallKind::Reconcile => "reconcile",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    Ok,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    pub seq: u64,
    pub kind: CallKind,
    pub idempotency_key: String,
    pub amount_minor: Option<i64>,
    pub quantity: Option<u32>,
    pub plan_id: Option<String>,
    pub outcome: Option<CallOutcome>,
    pub effect_committed: bool,
    pub object_id: Option<String>,
}

impl CallRecord {
    pub fn succeeded(&self) -> bool {
        self.effect_committed && self.outcome == Some(CallOutcome::Ok)
    }
}

#[derive(Default)]
struct LedgerInner {
    next_seq: u64,
    records: Vec<CallRecord>,
}

#[derive(Default)]
pub struct CallLedger {
    inner: Mutex<LedgerInner>,
}

impl CallLedger {
    fn begin(
        &self,
        kind: CallKind,
        idempotency_key: &str,
        amount_minor: Option<i64>,
        quantity: Option<u32>,
        plan_id: Option<&str>,
    ) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.next_seq += 1;
        let seq = inner.next_seq;
        inner.records.push(CallRecord {
            seq,
            kind,
            idempotency_key: idempotency_key.to_string(),
            amount_minor,
            quantity,
            plan_id: plan_id.map(str::to_string),
            outcome: None,
            effect_committed: false,
            object_id: None,
        });
        seq
    }

    fn finish<T>(
        &self,
        seq: u64,
        result: &Result<T, PaymentError>,
        effect_committed: bool,
        object_id: Option<String>,
    ) {
        let outcome = match result {
            Ok(_) => CallOutcome::Ok,
            Err(error) => CallOutcome::Failed(error.to_string()),
        };
        let mut inner = self.inner.lock().unwrap();
        let record = inner
            .records
            .iter_mut()
            .find(|record| record.seq == seq)
            .expect("call ledger record was removed");
        record.outcome = Some(outcome);
        record.effect_committed = effect_committed;
        record.object_id = object_id;
    }

    pub fn records(&self) -> Vec<CallRecord> {
        self.inner.lock().unwrap().records.clone()
    }

    pub fn of_kind(&self, kind: CallKind) -> Vec<CallRecord> {
        self.records()
            .into_iter()
            .filter(|record| record.kind == kind)
            .collect()
    }

    pub fn committed(&self, kind: CallKind) -> Vec<CallRecord> {
        self.of_kind(kind)
            .into_iter()
            .filter(|record| record.effect_committed)
            .collect()
    }

    pub fn committed_total_minor(&self, kind: CallKind) -> i64 {
        self.committed(kind)
            .into_iter()
            .map(|record| record.amount_minor.unwrap_or(0))
            .sum()
    }

    pub fn committed_keys(&self, kind: CallKind) -> Vec<String> {
        self.committed(kind)
            .into_iter()
            .map(|record| record.idempotency_key)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct GateState {
    blocking: bool,
    opened: bool,
    arrivals: usize,
}

#[derive(Debug)]
pub struct CallGate {
    state: watch::Sender<GateState>,
}

impl CallGate {
    fn new() -> Self {
        let (state, _receiver) = watch::channel(GateState::default());
        Self { state }
    }

    pub fn block(&self) {
        self.state.send_modify(|state| state.blocking = true);
    }

    pub fn open(&self) {
        self.state.send_modify(|state| state.opened = true);
    }

    pub fn arrivals(&self) -> usize {
        self.state.borrow().arrivals
    }

    pub async fn wait_for_arrivals(&self, expected: usize) {
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

#[derive(Clone)]
struct GatePair {
    before: Arc<CallGate>,
    after: Arc<CallGate>,
}

pub struct LedgerProvider {
    inner: faktor_payments::LocalPaymentProvider,
    ledger: Arc<CallLedger>,
    gates: BTreeMap<CallKind, GatePair>,
    charge_status: Mutex<Option<String>>,
    refund_status: Mutex<Option<String>>,
    provider_subscriptions: Mutex<BTreeMap<String, ProviderSubscription>>,
}

impl LedgerProvider {
    pub fn new(
        store: Arc<SqlitePaymentStore>,
        clock: Arc<ManualClock>,
        webhook_secret: &str,
        ledger: Arc<CallLedger>,
    ) -> Self {
        let gates = CallKind::ALL
            .into_iter()
            .map(|kind| {
                (
                    kind,
                    GatePair {
                        before: Arc::new(CallGate::new()),
                        after: Arc::new(CallGate::new()),
                    },
                )
            })
            .collect();
        Self {
            inner: faktor_payments::LocalPaymentProvider::new(
                store,
                Some(faktor_payments::secrets::SecretString::new(webhook_secret)),
                clock,
            ),
            ledger,
            gates,
            charge_status: Mutex::new(None),
            refund_status: Mutex::new(None),
            provider_subscriptions: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn provider_subscription(
        &self,
        provider_subscription_id: &str,
    ) -> Option<ProviderSubscription> {
        self.provider_subscriptions
            .lock()
            .unwrap()
            .get(provider_subscription_id)
            .cloned()
    }

    fn remember_subscription(&self, subscription: &ProviderSubscription) {
        self.provider_subscriptions
            .lock()
            .unwrap()
            .insert(subscription.id.clone(), subscription.clone());
    }

    fn gate(&self, kind: CallKind) -> &GatePair {
        self.gates
            .get(&kind)
            .expect("every provider call kind has a gate")
    }

    pub fn block_before(&self, kind: CallKind) -> Arc<CallGate> {
        let gate = self.gate(kind).before.clone();
        gate.block();
        gate
    }

    pub fn block_after_commit(&self, kind: CallKind) -> Arc<CallGate> {
        let gate = self.gate(kind).after.clone();
        gate.block();
        gate
    }

    pub fn wait_before(&self, kind: CallKind, expected: usize) -> impl Future<Output = ()> + '_ {
        self.gate(kind).before.wait_for_arrivals(expected)
    }

    pub fn wait_after_commit(
        &self,
        kind: CallKind,
        expected: usize,
    ) -> impl Future<Output = ()> + '_ {
        self.gate(kind).after.wait_for_arrivals(expected)
    }

    pub fn set_charge_status(&self, status: &str) {
        *self.charge_status.lock().unwrap() = Some(status.to_string());
    }

    pub fn set_refund_status(&self, status: &str) {
        *self.refund_status.lock().unwrap() = Some(status.to_string());
    }

    fn ledger_ok<T>(&self, seq: u64, result: &Result<T, PaymentError>, object_id: Option<String>) {
        self.ledger.finish(seq, result, result.is_ok(), object_id);
    }
}

#[async_trait]
impl PaymentProvider for LedgerProvider {
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
        let key = request.idempotency_key.clone();
        let seq = self
            .ledger
            .begin(CallKind::Customer, &key, None, None, None);
        self.gate(CallKind::Customer).before.arrive().await;
        let result = self.inner.create_customer(request).await;
        let object_id = result.as_ref().ok().map(|customer| customer.id.clone());
        self.ledger_ok(seq, &result, object_id);
        if result.is_ok() {
            self.gate(CallKind::Customer).after.arrive().await;
        }
        result
    }

    async fn subscribe(
        &self,
        request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let key = request.idempotency_key.clone();
        let amount = Some(request.unit_amount_minor);
        let quantity = Some(request.quantity);
        let plan = request.plan_id.clone();
        let seq = self
            .ledger
            .begin(CallKind::Subscribe, &key, amount, quantity, Some(&plan));
        self.gate(CallKind::Subscribe).before.arrive().await;
        let result = self.inner.subscribe(request).await;
        let object_id = result
            .as_ref()
            .ok()
            .map(|subscription| subscription.id.clone());
        self.ledger_ok(seq, &result, object_id);
        if let Ok(subscription) = &result {
            self.remember_subscription(subscription);
        }
        if result.is_ok() {
            self.gate(CallKind::Subscribe).after.arrive().await;
        }
        result
    }

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let key = request.idempotency_key.clone();
        let amount = Some(request.unit_amount_minor);
        let quantity = Some(request.quantity);
        let plan = request.plan_id.clone();
        let seq = self
            .ledger
            .begin(CallKind::ChangePlan, &key, amount, quantity, Some(&plan));
        self.gate(CallKind::ChangePlan).before.arrive().await;
        let result = self.inner.change_plan(request).await;
        let object_id = result
            .as_ref()
            .ok()
            .map(|subscription| subscription.id.clone());
        self.ledger_ok(seq, &result, object_id);
        if let Ok(subscription) = &result {
            self.remember_subscription(subscription);
        }
        if result.is_ok() {
            self.gate(CallKind::ChangePlan).after.arrive().await;
        }
        result
    }

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let key = request.idempotency_key.clone();
        let seq = self.ledger.begin(CallKind::Cancel, &key, None, None, None);
        self.gate(CallKind::Cancel).before.arrive().await;
        let result = self.inner.cancel(request).await;
        let object_id = result
            .as_ref()
            .ok()
            .map(|subscription| subscription.id.clone());
        self.ledger_ok(seq, &result, object_id);
        if let Ok(subscription) = &result {
            self.remember_subscription(subscription);
        }
        if result.is_ok() {
            self.gate(CallKind::Cancel).after.arrive().await;
        }
        result
    }

    async fn create_checkout(
        &self,
        request: ProviderCheckoutRequest,
    ) -> Result<ProviderCheckout, PaymentError> {
        let key = request.idempotency_key.clone();
        let amount = Some(request.amount_minor);
        let seq = self
            .ledger
            .begin(CallKind::Checkout, &key, amount, None, None);
        self.gate(CallKind::Checkout).before.arrive().await;
        let result = self.inner.create_checkout(request).await;
        let object_id = result.as_ref().ok().map(|checkout| checkout.id.clone());
        self.ledger_ok(seq, &result, object_id);
        if result.is_ok() {
            self.gate(CallKind::Checkout).after.arrive().await;
        }
        result
    }

    async fn create_invoice(
        &self,
        request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, PaymentError> {
        let key = request.idempotency_key.clone();
        let amount = Some(request.amount_minor);
        let seq = self
            .ledger
            .begin(CallKind::Invoice, &key, amount, None, None);
        self.gate(CallKind::Invoice).before.arrive().await;
        let result = self.inner.create_invoice(request).await;
        let object_id = result.as_ref().ok().map(|invoice| invoice.id.clone());
        self.ledger_ok(seq, &result, object_id);
        if result.is_ok() {
            self.gate(CallKind::Invoice).after.arrive().await;
        }
        result
    }

    async fn fetch_invoice(
        &self,
        provider_invoice_id: &str,
    ) -> Result<ProviderInvoiceSnapshot, PaymentError> {
        let seq = self.ledger.begin(
            CallKind::FetchInvoice,
            provider_invoice_id,
            None,
            None,
            None,
        );
        let result = self.inner.fetch_invoice(provider_invoice_id).await;
        self.ledger.finish(seq, &result, false, None);
        result
    }

    async fn charge(&self, request: ProviderChargeRequest) -> Result<ProviderCharge, PaymentError> {
        let key = request.idempotency_key.clone();
        let amount = Some(request.amount_minor);
        let seq = self
            .ledger
            .begin(CallKind::Charge, &key, amount, None, None);
        self.gate(CallKind::Charge).before.arrive().await;
        let mut result = self.inner.charge(request).await;
        if let Ok(charge) = result.as_mut() {
            if let Some(status) = self.charge_status.lock().unwrap().clone() {
                charge.status = status;
            }
        }
        let object_id = result.as_ref().ok().map(|charge| charge.id.clone());
        self.ledger_ok(seq, &result, object_id);
        if result.is_ok() {
            self.gate(CallKind::Charge).after.arrive().await;
        }
        result
    }

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError> {
        let key = request.idempotency_key.clone();
        let amount = Some(request.amount_minor);
        let seq = self
            .ledger
            .begin(CallKind::Refund, &key, amount, None, None);
        self.gate(CallKind::Refund).before.arrive().await;
        let mut result = self.inner.refund(request).await;
        if let Ok(refund) = result.as_mut() {
            if let Some(status) = self.refund_status.lock().unwrap().clone() {
                refund.status = status;
            }
        }
        let object_id = result.as_ref().ok().map(|refund| refund.id.clone());
        self.ledger_ok(seq, &result, object_id);
        if result.is_ok() {
            self.gate(CallKind::Refund).after.arrive().await;
        }
        result
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
        let seq = self.ledger.begin(
            CallKind::Reconcile,
            &request.idempotency_key,
            None,
            None,
            None,
        );
        let result = self.inner.reconcile(request).await;
        self.ledger.finish(seq, &result, false, None);
        result
    }
}

pub type RaceStep =
    Pin<Box<dyn Future<Output = (String, Result<(), PaymentError>)> + Send + 'static>>;

pub struct RaceFixture {
    pub _dir: TempDir,
    pub path: PathBuf,
    pub store: Arc<SqlitePaymentStore>,
    pub clock: Arc<ManualClock>,
    pub ledger: Arc<CallLedger>,
    pub provider: Arc<LedgerProvider>,
    pub service: Arc<PaymentService>,
}

impl RaceFixture {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("payment.db");
        let store = Arc::new(SqlitePaymentStore::open(&path).unwrap());
        let clock = Arc::new(ManualClock::new(START_MS));
        let ledger = Arc::new(CallLedger::default());
        let provider = Arc::new(LedgerProvider::new(
            store.clone(),
            clock.clone(),
            WEBHOOK_SECRET,
            ledger.clone(),
        ));
        let service = Arc::new(
            PaymentService::with_provider(
                store.clone(),
                provider.clone(),
                race_config(),
                clock.clone(),
                race_secrets(),
            )
            .unwrap(),
        );
        Self {
            _dir: dir,
            path,
            store,
            clock,
            ledger,
            provider,
            service,
        }
    }

    pub fn service_over(
        &self,
        store: Arc<SqlitePaymentStore>,
        provider: Arc<LedgerProvider>,
    ) -> Arc<PaymentService> {
        Arc::new(
            PaymentService::with_provider(
                store,
                provider,
                race_config(),
                self.clock.clone(),
                race_secrets(),
            )
            .unwrap(),
        )
    }

    pub async fn subscribe(&self) -> PaymentSubscription {
        self.service
            .subscribe(&org(), subscribe_request("subscriber"))
            .await
            .unwrap()
            .subscription
    }

    pub async fn add_method(&self) -> faktor_payments::PaymentMethodView {
        self.service
            .add_payment_method(
                &org(),
                AddPaymentMethodRequest {
                    token: "tok_financial_races".into(),
                    brand: "visa".into(),
                    last4: "4242".into(),
                    exp_month: 12,
                    exp_year: 2030,
                    set_default: true,
                    idempotency_key: "payment-method".into(),
                },
            )
            .unwrap()
            .payment_method
    }

    pub async fn issue_invoice(&self, subscription_id: &str) -> Invoice {
        self.service
            .issue_invoice(&org(), invoice_request(subscription_id, "invoice"))
            .await
            .unwrap()
            .invoice
    }

    pub async fn paid_payment(&self) -> (Invoice, Payment) {
        let subscription = self.subscribe().await;
        let method = self.add_method().await;
        let invoice = self.issue_invoice(&subscription.id).await;
        let payment = self
            .service
            .pay_invoice(&org(), pay_request(&invoice.id, &method.id, "pay-invoice"))
            .await
            .unwrap()
            .payment;
        (invoice, payment)
    }

    pub fn pay_future(
        &self,
        invoice_id: String,
        payment_method_id: String,
        key: &'static str,
        start: Arc<tokio::sync::Barrier>,
    ) -> impl Future<Output = (String, Result<PayInvoiceOutcome, PaymentError>)> + Send + 'static
    {
        let service = self.service.clone();
        let organization = org();
        async move {
            start.wait().await;
            let result = service
                .pay_invoice(
                    &organization,
                    pay_request(&invoice_id, &payment_method_id, key),
                )
                .await;
            (key.to_string(), result)
        }
    }

    pub fn refund_future(
        &self,
        payment_id: String,
        amount_minor: i64,
        key: String,
        start: Arc<tokio::sync::Barrier>,
    ) -> impl Future<Output = (String, Result<RefundOutcome, PaymentError>)> + Send + 'static {
        let service = self.service.clone();
        let organization = org();
        async move {
            start.wait().await;
            let result = service
                .refund(
                    &organization,
                    refund_request(&payment_id, amount_minor, &key),
                )
                .await;
            (key, result)
        }
    }

    pub fn subscribe_future(
        &self,
        key: String,
        start: Arc<tokio::sync::Barrier>,
    ) -> impl Future<
        Output = (
            String,
            Result<faktor_payments::SubscriptionOutcome, PaymentError>,
        ),
    > + Send
           + 'static {
        let service = self.service.clone();
        let organization = org();
        async move {
            start.wait().await;
            let result = service
                .subscribe(&organization, subscribe_request(&key))
                .await;
            (key, result)
        }
    }

    pub fn change_plan_future(
        &self,
        subscription_id: String,
        plan_id: &'static str,
        key: &'static str,
        start: Arc<tokio::sync::Barrier>,
    ) -> impl Future<
        Output = (
            String,
            Result<faktor_payments::PlanChangeOutcome, PaymentError>,
        ),
    > + Send
           + 'static {
        let service = self.service.clone();
        let organization = org();
        async move {
            start.wait().await;
            let result = service
                .change_plan(
                    &organization,
                    ChangePlanRequest {
                        subscription_id,
                        plan_id: plan_id.into(),
                        interval: None,
                        idempotency_key: key.into(),
                    },
                )
                .await;
            (key.to_string(), result)
        }
    }

    pub fn change_seats_future(
        &self,
        subscription_id: String,
        quantity: u32,
        key: &'static str,
        start: Arc<tokio::sync::Barrier>,
    ) -> impl Future<
        Output = (
            String,
            Result<faktor_payments::SeatChangeOutcome, PaymentError>,
        ),
    > + Send
           + 'static {
        let service = self.service.clone();
        let organization = org();
        async move {
            start.wait().await;
            let result = service
                .change_seats(
                    &organization,
                    ChangeSeatsRequest {
                        subscription_id,
                        quantity,
                        idempotency_key: key.into(),
                    },
                )
                .await;
            (key.to_string(), result)
        }
    }

    pub fn cancel_future(
        &self,
        subscription_id: String,
        key: &'static str,
        start: Arc<tokio::sync::Barrier>,
    ) -> impl Future<
        Output = (
            String,
            Result<faktor_payments::SubscriptionOutcome, PaymentError>,
        ),
    > + Send
           + 'static {
        let service = self.service.clone();
        let organization = org();
        async move {
            start.wait().await;
            let result = service
                .cancel(
                    &organization,
                    CancelRequest {
                        subscription_id,
                        at_period_end: false,
                        idempotency_key: key.into(),
                    },
                )
                .await;
            (key.to_string(), result)
        }
    }

    pub fn change_plan_step(
        &self,
        subscription_id: String,
        plan_id: &'static str,
        key: &'static str,
        start: Arc<tokio::sync::Barrier>,
    ) -> RaceStep {
        let service = self.service.clone();
        let organization = org();
        Box::pin(async move {
            start.wait().await;
            let result = service
                .change_plan(
                    &organization,
                    ChangePlanRequest {
                        subscription_id,
                        plan_id: plan_id.into(),
                        interval: None,
                        idempotency_key: key.into(),
                    },
                )
                .await
                .map(|_| ());
            (key.to_string(), result)
        })
    }

    pub fn change_seats_step(
        &self,
        subscription_id: String,
        quantity: u32,
        key: &'static str,
        start: Arc<tokio::sync::Barrier>,
    ) -> RaceStep {
        let service = self.service.clone();
        let organization = org();
        Box::pin(async move {
            start.wait().await;
            let result = service
                .change_seats(
                    &organization,
                    ChangeSeatsRequest {
                        subscription_id,
                        quantity,
                        idempotency_key: key.into(),
                    },
                )
                .await
                .map(|_| ());
            (key.to_string(), result)
        })
    }

    pub fn cancel_step(
        &self,
        subscription_id: String,
        key: &'static str,
        start: Arc<tokio::sync::Barrier>,
    ) -> RaceStep {
        let service = self.service.clone();
        let organization = org();
        Box::pin(async move {
            start.wait().await;
            let result = service
                .cancel(
                    &organization,
                    CancelRequest {
                        subscription_id,
                        at_period_end: false,
                        idempotency_key: key.into(),
                    },
                )
                .await
                .map(|_| ());
            (key.to_string(), result)
        })
    }

    pub fn provider_subscription(&self, provider_subscription_id: &str) -> ProviderSubscription {
        self.provider
            .provider_subscription(provider_subscription_id)
            .expect("the provider mock has no subscription for that id")
    }

    pub fn payment_succeeded_webhook(
        &self,
        provider_payment_id: &str,
        amount_minor: i64,
        event_id: &str,
    ) -> (WebhookHeaders, Vec<u8>) {
        let now_secs = self.clock.now_ms() / 1000;
        let body = serde_json::to_vec(&serde_json::json!({
            "id": event_id,
            "type": "payment_intent.succeeded",
            "created": now_secs,
            "data": {"object": {
                "id": provider_payment_id,
                "amount": amount_minor,
                "status": "succeeded",
                "metadata": {"organization_id": ORG},
            }},
        }))
        .unwrap();
        let signature = sign_local_webhook(WEBHOOK_SECRET, now_secs, &body);
        (
            WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]),
            body,
        )
    }

    pub fn payment_succeeded_webhook_for_invoice(
        &self,
        provider_payment_id: &str,
        amount_minor: i64,
        invoice_id: &str,
        event_id: &str,
    ) -> (WebhookHeaders, Vec<u8>) {
        let now_secs = self.clock.now_ms() / 1000;
        let body = serde_json::to_vec(&serde_json::json!({
            "id": event_id,
            "type": "payment_intent.succeeded",
            "created": now_secs,
            "data": {"object": {
                "id": provider_payment_id,
                "amount": amount_minor,
                "status": "succeeded",
                "metadata": {"organization_id": ORG, "local_invoice_id": invoice_id},
            }},
        }))
        .unwrap();
        let signature = sign_local_webhook(WEBHOOK_SECRET, now_secs, &body);
        (
            WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]),
            body,
        )
    }

    pub fn payment_failed_webhook(
        &self,
        provider_payment_id: &str,
        invoice_id: &str,
        event_id: &str,
    ) -> (WebhookHeaders, Vec<u8>) {
        let now_secs = self.clock.now_ms() / 1000;
        let body = serde_json::to_vec(&serde_json::json!({
            "id": event_id,
            "type": "payment_intent.payment_failed",
            "created": now_secs,
            "data": {"object": {
                "id": provider_payment_id,
                "status": "failed",
                "metadata": {"organization_id": ORG, "local_invoice_id": invoice_id},
            }},
        }))
        .unwrap();
        let signature = sign_local_webhook(WEBHOOK_SECRET, now_secs, &body);
        (
            WebhookHeaders::from_pairs([("Stripe-Signature", signature.as_str())]),
            body,
        )
    }

    pub async fn deliver_payment_succeeded(
        &self,
        provider_payment_id: &str,
        amount_minor: i64,
        event_id: &str,
    ) -> WebhookOutcome {
        let (headers, body) =
            self.payment_succeeded_webhook(provider_payment_id, amount_minor, event_id);
        self.service
            .handle_webhook(&headers, &body, self.clock.now_ms())
            .await
            .unwrap()
    }

    pub fn refunds(&self, payment_id: &str) -> Vec<faktor_payments::Refund> {
        self.store.refunds_for_payment(ORG, payment_id).unwrap()
    }

    pub fn payments(&self) -> Vec<Payment> {
        self.service.payments(&org(), 50).unwrap()
    }

    pub fn invoice(&self, invoice_id: &str) -> Invoice {
        self.service.invoice(&org(), invoice_id).unwrap()
    }

    pub fn subscription(&self) -> PaymentSubscription {
        self.store.subscription(ORG).unwrap().unwrap()
    }

    pub fn claim(&self, key: &str) -> Option<faktor_payments::OperationClaim> {
        self.store.operation_claim(ORG, key).unwrap()
    }
}

pub fn race_config() -> PaymentsConfig {
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
    let mut currencies = BTreeMap::new();
    currencies.insert("USD".into(), CurrencySpec { exponent: 2 });
    PaymentsConfig {
        provider: ProviderKind::Local,
        catalogue: PriceCatalogue {
            plans,
            currencies,
            fx: BTreeMap::new(),
        },
        tax: TaxConfig {
            default_jurisdiction: None,
            rates: BTreeMap::<String, TaxRate>::new(),
        },
        dunning: DunningConfig {
            max_attempts: 2,
            grace_ms: 10_000,
        },
        webhook_secret_env: WEBHOOK_ENV.into(),
        vault_key_env: VAULT_ENV.into(),
        stripe: Default::default(),
        mollie: Default::default(),
    }
}

pub fn race_secrets() -> Arc<StaticSecrets> {
    Arc::new(StaticSecrets::new([(
        WEBHOOK_ENV.to_string(),
        WEBHOOK_SECRET.to_string(),
    )]))
}

pub fn subscribe_request(key: &str) -> SubscribeRequest {
    SubscribeRequest {
        plan_id: "pro".into(),
        interval: BillingInterval::Monthly,
        seats: 1,
        trial_days: None,
        customer_email: "races@example.com".into(),
        customer_name: "Financial Races".into(),
        idempotency_key: key.into(),
    }
}

pub fn invoice_request(subscription_id: &str, key: &str) -> IssueInvoiceRequest {
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

pub fn pay_request(invoice_id: &str, payment_method_id: &str, key: &str) -> PayInvoiceRequest {
    PayInvoiceRequest {
        invoice_id: invoice_id.into(),
        payment_method_id: payment_method_id.into(),
        idempotency_key: key.into(),
    }
}

pub fn refund_request(payment_id: &str, amount_minor: i64, key: &str) -> RefundRequest {
    RefundRequest {
        payment_id: payment_id.into(),
        amount_minor,
        reason: "adversarial financial race".into(),
        idempotency_key: key.into(),
    }
}

pub fn is_typed_refusal(error: &PaymentError) -> bool {
    matches!(
        error,
        PaymentError::Conflict(_)
            | PaymentError::IllegalState(_)
            | PaymentError::NothingDue
            | PaymentError::RefundExceedsPaid { .. }
            | PaymentError::IdempotencyConflict(_)
            | PaymentError::Declined(_)
    )
}

pub fn assert_typed_refusal(error: &PaymentError, context: &str) {
    assert!(
        is_typed_refusal(error),
        "{context}: expected a typed financial refusal, got {error:?}"
    );
}

async fn wait_all_arrived(gates: &[Arc<CallGate>]) {
    for gate in gates {
        gate.wait_for_arrivals(1).await;
    }
}

pub async fn run_race<T, F>(gate: Arc<CallGate>, expected: usize, futures: Vec<F>) -> Vec<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    run_race_multi(vec![gate], expected, futures).await
}

pub async fn run_race_multi<T, F>(
    gates: Vec<Arc<CallGate>>,
    expected: usize,
    futures: Vec<F>,
) -> Vec<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let mut tasks = JoinSet::new();
    for future in futures {
        tasks.spawn(future);
    }
    let mut results = Vec::with_capacity(expected);
    let mut opened = false;
    let soft_deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    let hard_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !tasks.is_empty() {
        tokio::select! {
            _ = wait_all_arrived(&gates), if !opened => {
                for gate in &gates {
                    gate.open();
                }
                opened = true;
            }
            joined = tasks.join_next() => {
                match joined {
                    Some(Ok(result)) => results.push(result),
                    Some(Err(error)) => panic!("financial race task failed: {error}"),
                    None => break,
                }
                if !opened {
                    for gate in &gates {
                        gate.open();
                    }
                    opened = true;
                }
            }
            _ = tokio::time::sleep_until(soft_deadline), if !opened => {
                for gate in &gates {
                    gate.open();
                }
                opened = true;
            }
            _ = tokio::time::sleep_until(hard_deadline) => {
                panic!("financial race exceeded its bounded time");
            }
        }
    }
    results
}

pub fn event_id(payment_id: &str) -> String {
    format!("evt_{payment_id}")
}

pub fn parse_object<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}
