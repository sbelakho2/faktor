//! The [`PaymentProvider`] seam. The local SQLite provider and the
//! Stripe-compatible adapter implement exactly this trait; the service never
//! branches on `provider == "..."`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::BillingInterval;
use crate::error::PaymentError;
use crate::model::ProviderStatus;

/// Bounded provider text.
pub const MAX_PROVIDER_TEXT: usize = 512;

/// One provider customer creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCustomerRequest {
    pub organization_id: String,
    pub idempotency_key: String,
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCustomer {
    pub id: String,
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSubscribeRequest {
    pub organization_id: String,
    pub idempotency_key: String,
    pub customer_id: String,
    pub plan_id: String,
    pub interval: BillingInterval,
    pub quantity: u32,
    pub trial_end_ms: Option<i64>,
    pub currency: String,
    pub unit_amount_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderChangePlanRequest {
    pub organization_id: String,
    pub idempotency_key: String,
    pub subscription_id: String,
    pub plan_id: String,
    pub interval: BillingInterval,
    pub quantity: u32,
    pub currency: String,
    pub unit_amount_minor: i64,
    pub prorate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCancelRequest {
    pub organization_id: String,
    pub idempotency_key: String,
    pub subscription_id: String,
    pub at_period_end: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSubscription {
    pub id: String,
    pub status: String,
    pub quantity: u32,
    pub current_period_start_ms: i64,
    pub current_period_end_ms: i64,
    pub cancel_at_period_end: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInvoiceRequest {
    pub organization_id: String,
    pub idempotency_key: String,
    pub customer_id: String,
    pub subscription_id: Option<String>,
    pub description: String,
    pub amount_minor: i64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInvoice {
    pub id: String,
    pub status: String,
    pub amount_minor: i64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderChargeRequest {
    pub organization_id: String,
    pub idempotency_key: String,
    pub customer_id: String,
    /// The provider-side payment-method token is a bearer credential: it is
    /// wrapped so `Debug`/artifacts never render it, and it is exposed only
    /// at the provider transport boundary.
    pub payment_method_token: crate::secrets::SecretString,
    pub amount_minor: i64,
    pub currency: String,
    pub invoice_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCharge {
    pub id: String,
    pub status: String,
    pub amount_minor: i64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRefundRequest {
    pub organization_id: String,
    pub idempotency_key: String,
    pub provider_payment_id: String,
    pub amount_minor: i64,
    pub currency: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRefund {
    pub id: String,
    pub status: String,
    pub amount_minor: i64,
    pub currency: String,
}

/// The headers of one inbound webhook delivery (lower-case names, bounded
/// values). The body is passed separately as bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebhookHeaders {
    pairs: Vec<(String, String)>,
}

impl WebhookHeaders {
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            pairs: pairs
                .into_iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
                .collect(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One verified provider event. `payload` is untrusted provider data that
/// already passed the signature/replay checks; the service applies only the
/// typed fields it understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWebhookEvent {
    pub id: String,
    pub event_type: String,
    pub created_ms: i64,
    pub organization_id: Option<String>,
    pub payload: Value,
}

/// One neutral provider invoice projection used by the service parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderEventObject {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub amount_minor: Option<i64>,
    #[serde(default)]
    pub amount_paid_minor: Option<i64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub quantity: Option<u32>,
    #[serde(default)]
    pub current_period_end_ms: Option<i64>,
    #[serde(default)]
    pub cancel_at_period_end: Option<bool>,
    #[serde(default)]
    pub organization_id: Option<String>,
    #[serde(default)]
    pub local_invoice_id: Option<String>,
    #[serde(default)]
    pub subscription_id: Option<String>,
}

/// The provider abstraction. Every method is fallible and never logs the
/// request: credentials and payment tokens exist only inside the adapter.
#[async_trait]
pub trait PaymentProvider: Send + Sync {
    fn name(&self) -> &'static str;

    fn status(&self) -> ProviderStatus;

    async fn create_customer(
        &self,
        request: ProviderCustomerRequest,
    ) -> Result<ProviderCustomer, PaymentError>;

    async fn subscribe(
        &self,
        request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError>;

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError>;

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError>;

    async fn create_invoice(
        &self,
        request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, PaymentError>;

    async fn charge(&self, request: ProviderChargeRequest) -> Result<ProviderCharge, PaymentError>;

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError>;

    /// Verify a webhook's signature, timestamp window and event shape. This
    /// is synchronous: it must not perform I/O before authentication.
    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError>;
}

/// The typed provider used when configuration selects a provider whose
/// credential is absent. Every operation returns
/// [`PaymentError::ProviderUnconfigured`]; construction succeeds so the
/// daemon can still report the honest capability state.
#[derive(Debug, Clone)]
pub struct UnconfiguredPaymentProvider {
    provider: &'static str,
    cause: String,
}

impl UnconfiguredPaymentProvider {
    pub fn new(provider: &'static str, cause: impl Into<String>) -> Self {
        Self {
            provider,
            cause: cause.into(),
        }
    }
}

#[async_trait]
impl PaymentProvider for UnconfiguredPaymentProvider {
    fn name(&self) -> &'static str {
        self.provider
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus::unconfigured(self.provider, self.cause.clone())
    }

    async fn create_customer(
        &self,
        _request: ProviderCustomerRequest,
    ) -> Result<ProviderCustomer, PaymentError> {
        Err(PaymentError::ProviderUnconfigured(self.cause.clone()))
    }

    async fn subscribe(
        &self,
        _request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        Err(PaymentError::ProviderUnconfigured(self.cause.clone()))
    }

    async fn change_plan(
        &self,
        _request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        Err(PaymentError::ProviderUnconfigured(self.cause.clone()))
    }

    async fn cancel(
        &self,
        _request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        Err(PaymentError::ProviderUnconfigured(self.cause.clone()))
    }

    async fn create_invoice(
        &self,
        _request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, PaymentError> {
        Err(PaymentError::ProviderUnconfigured(self.cause.clone()))
    }

    async fn charge(
        &self,
        _request: ProviderChargeRequest,
    ) -> Result<ProviderCharge, PaymentError> {
        Err(PaymentError::ProviderUnconfigured(self.cause.clone()))
    }

    async fn refund(
        &self,
        _request: ProviderRefundRequest,
    ) -> Result<ProviderRefund, PaymentError> {
        Err(PaymentError::ProviderUnconfigured(self.cause.clone()))
    }

    fn verify_webhook(
        &self,
        _headers: &WebhookHeaders,
        _body: &[u8],
        _now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        Err(PaymentError::ProviderUnconfigured(self.cause.clone()))
    }
}
