//! Durable payment/invoice/subscription vocabulary. Money is always an
//! integer minor-unit amount plus an ISO-shaped currency; no field derives a
//! total from a float.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::catalog::{BillingInterval, PlanPrice, TaxRate};

/// The lifecycle state of a paid subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionState {
    Trialing,
    Active,
    PastDue,
    Canceled,
}

impl SubscriptionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            SubscriptionState::Trialing => "trialing",
            SubscriptionState::Active => "active",
            SubscriptionState::PastDue => "past_due",
            SubscriptionState::Canceled => "canceled",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "trialing" => SubscriptionState::Trialing,
            "active" => SubscriptionState::Active,
            "past_due" => SubscriptionState::PastDue,
            "canceled" => SubscriptionState::Canceled,
            _ => return None,
        })
    }

    pub const fn is_live(self) -> bool {
        matches!(
            self,
            SubscriptionState::Trialing | SubscriptionState::Active | SubscriptionState::PastDue
        )
    }
}

/// One durable paid subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentSubscription {
    pub id: String,
    pub organization_id: String,
    pub customer_id: String,
    pub provider_subscription_id: String,
    pub plan_id: String,
    pub interval: BillingInterval,
    pub quantity: u32,
    pub state: SubscriptionState,
    pub currency: String,
    pub current_period_start_ms: i64,
    pub current_period_end_ms: i64,
    pub trial_end_ms: Option<i64>,
    pub cancel_at_period_end: bool,
    pub canceled_ms: Option<i64>,
    pub dunning_attempts: u32,
    pub grace_end_ms: Option<i64>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl PaymentSubscription {
    pub fn is_live(&self) -> bool {
        self.state.is_live() && self.canceled_ms.is_none()
    }

    pub fn period_ms(&self) -> i64 {
        (self.current_period_end_ms - self.current_period_start_ms).max(1)
    }

    pub fn remaining_ms(&self, at_ms: i64) -> i64 {
        (self.current_period_end_ms - at_ms).clamp(0, self.period_ms())
    }
}

/// A provider-side customer mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentCustomer {
    pub id: String,
    pub organization_id: String,
    pub provider: String,
    pub provider_customer_id: String,
    pub email: String,
    pub name: String,
    pub created_ms: i64,
}

/// A stored payment method. The token is a provider token (or a local opaque
/// placeholder); the raw PAN is never accepted, never stored and never
/// serialized to a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentMethod {
    pub id: String,
    pub organization_id: String,
    pub provider: String,
    pub token: String,
    pub brand: String,
    pub last4: String,
    pub exp_month: u8,
    pub exp_year: u16,
    pub is_default: bool,
    pub created_ms: i64,
}

/// The safe wire projection of a payment method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaymentMethodView {
    pub id: String,
    pub provider: String,
    pub brand: String,
    pub last4: String,
    pub exp_month: u8,
    pub exp_year: u16,
    pub is_default: bool,
    pub created_ms: i64,
}

impl From<&PaymentMethod> for PaymentMethodView {
    fn from(method: &PaymentMethod) -> Self {
        Self {
            id: method.id.clone(),
            provider: method.provider.clone(),
            brand: method.brand.clone(),
            last4: method.last4.clone(),
            exp_month: method.exp_month,
            exp_year: method.exp_year,
            is_default: method.is_default,
            created_ms: method.created_ms,
        }
    }
}

/// Invoice state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    Draft,
    Open,
    Paid,
    PastDue,
    Void,
    Uncollectible,
}

impl InvoiceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            InvoiceStatus::Draft => "draft",
            InvoiceStatus::Open => "open",
            InvoiceStatus::Paid => "paid",
            InvoiceStatus::PastDue => "past_due",
            InvoiceStatus::Void => "void",
            InvoiceStatus::Uncollectible => "uncollectible",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "draft" => InvoiceStatus::Draft,
            "open" => InvoiceStatus::Open,
            "paid" => InvoiceStatus::Paid,
            "past_due" => InvoiceStatus::PastDue,
            "void" => InvoiceStatus::Void,
            "uncollectible" => InvoiceStatus::Uncollectible,
            _ => return None,
        })
    }

    pub const fn is_open(self) -> bool {
        matches!(self, InvoiceStatus::Open | InvoiceStatus::PastDue)
    }
}

/// The exact line-item kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceLineKind {
    Plan,
    Seat,
    Proration,
    Usage,
    Credits,
    Tax,
    Discount,
}

impl InvoiceLineKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            InvoiceLineKind::Plan => "plan",
            InvoiceLineKind::Seat => "seat",
            InvoiceLineKind::Proration => "proration",
            InvoiceLineKind::Usage => "usage",
            InvoiceLineKind::Credits => "credits",
            InvoiceLineKind::Tax => "tax",
            InvoiceLineKind::Discount => "discount",
        }
    }
}

/// One invoice line. `amount_minor` is the exact extended amount; credits,
/// discounts and a downgrade proration are negative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceLine {
    pub kind: InvoiceLineKind,
    pub description: String,
    pub quantity: i64,
    pub unit_amount_minor: i64,
    pub amount_minor: i64,
}

/// One durable invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invoice {
    pub id: String,
    pub organization_id: String,
    pub number: String,
    pub subscription_id: Option<String>,
    pub provider_invoice_id: Option<String>,
    pub currency: String,
    pub status: InvoiceStatus,
    pub lines: Vec<InvoiceLine>,
    /// plan + seat + proration + usage (before credits/discounts).
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub credits_minor: i64,
    /// Tax is informational for an inclusive jurisdiction and additive for
    /// an exclusive one.
    pub tax_minor: i64,
    pub tax_jurisdiction: Option<String>,
    pub tax_inclusive: bool,
    pub total_minor: i64,
    pub amount_paid_minor: i64,
    pub amount_refunded_minor: i64,
    pub period_start_ms: Option<i64>,
    pub period_end_ms: Option<i64>,
    pub issued_ms: i64,
    pub due_ms: i64,
    pub paid_ms: Option<i64>,
    pub voided_ms: Option<i64>,
    pub created_ms: i64,
}

impl Invoice {
    pub fn balance_minor(&self) -> i64 {
        (self.total_minor - self.amount_paid_minor).max(0)
    }
}

/// Payment state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Succeeded,
    PartiallyRefunded,
    Refunded,
    Failed,
}

impl PaymentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            PaymentStatus::Succeeded => "succeeded",
            PaymentStatus::PartiallyRefunded => "partially_refunded",
            PaymentStatus::Refunded => "refunded",
            PaymentStatus::Failed => "failed",
        }
    }
}

/// One successful charge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payment {
    pub id: String,
    pub organization_id: String,
    pub invoice_id: String,
    pub provider: String,
    pub provider_payment_id: String,
    pub payment_method_id: Option<String>,
    pub amount_minor: i64,
    pub refunded_minor: i64,
    pub currency: String,
    pub status: PaymentStatus,
    pub created_ms: i64,
}

/// One refund, always bounded by what the referenced payment collected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refund {
    pub id: String,
    pub organization_id: String,
    pub payment_id: String,
    pub provider_refund_id: String,
    pub amount_minor: i64,
    pub currency: String,
    pub reason: String,
    pub created_ms: i64,
}

/// One credit note generated by a refund.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreditNote {
    pub id: String,
    pub organization_id: String,
    pub number: String,
    pub invoice_id: String,
    pub payment_id: String,
    pub amount_minor: i64,
    pub currency: String,
    pub reason: String,
    pub created_ms: i64,
}

/// A pending proration line, consumed exactly once by the next invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingProration {
    pub id: String,
    pub organization_id: String,
    pub subscription_id: String,
    pub description: String,
    pub amount_minor: i64,
    pub currency: String,
    pub created_ms: i64,
    pub consumed_by: Option<String>,
}

/// The computed money of one plan or seat change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProrationPreview {
    pub kind: String,
    pub subscription_id: String,
    pub from_plan: String,
    pub to_plan: String,
    pub previous_quantity: Option<u32>,
    pub new_quantity: Option<u32>,
    pub old_amount_minor: i64,
    pub new_amount_minor: i64,
    /// Unused portion of the old amount (negative: a credit).
    pub credit_minor: i64,
    /// Remaining portion of the new amount (positive: a charge).
    pub charge_minor: i64,
    pub net_minor: i64,
    pub currency: String,
    pub remaining_ms: i64,
    pub period_ms: i64,
    pub effective_ms: i64,
}

/// The result of a plan change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanChangeOutcome {
    pub subscription: PaymentSubscription,
    pub proration: ProrationPreview,
    pub replayed: bool,
}

/// The result of a seat change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeatChangeOutcome {
    pub subscription: PaymentSubscription,
    pub proration: ProrationPreview,
    pub replayed: bool,
}

/// The result of creating or cancelling a subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubscriptionOutcome {
    pub subscription: PaymentSubscription,
    pub replayed: bool,
}

/// The result of issuing an invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvoiceOutcome {
    pub invoice: Invoice,
    pub replayed: bool,
}

/// The result of paying an invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PayInvoiceOutcome {
    pub payment: Payment,
    pub invoice: Invoice,
    pub subscription: Option<PaymentSubscription>,
    pub replayed: bool,
}

/// The result of a refund.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefundOutcome {
    pub refund: Refund,
    pub credit_note: CreditNote,
    pub payment: Payment,
    pub invoice: Invoice,
    pub replayed: bool,
}

/// The result of adding a payment method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaymentMethodOutcome {
    pub payment_method: PaymentMethodView,
    pub replayed: bool,
}

/// A bounded invoice page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvoicePage {
    pub items: Vec<Invoice>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

/// The tax surface of one organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaxSurface {
    pub state: String,
    pub cause: Option<String>,
    pub default_jurisdiction: Option<String>,
    pub rates: BTreeMap<String, TaxRate>,
}

/// One capability status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityStatus {
    pub state: &'static str,
    pub cause: Option<String>,
}

impl CapabilityStatus {
    pub fn available() -> Self {
        Self {
            state: "available",
            cause: None,
        }
    }

    pub fn unavailable(cause: impl Into<String>) -> Self {
        Self {
            state: "unavailable",
            cause: Some(cause.into()),
        }
    }
}

/// The provider's live status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    pub provider: String,
    pub state: &'static str,
    pub cause: Option<String>,
}

impl ProviderStatus {
    pub fn available(provider: &str) -> Self {
        Self {
            provider: provider.to_string(),
            state: "available",
            cause: None,
        }
    }

    pub fn unconfigured(provider: &str, cause: impl Into<String>) -> Self {
        Self {
            provider: provider.to_string(),
            state: "unconfigured",
            cause: Some(cause.into()),
        }
    }
}

/// The real commercial overview supplied by the payments plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaymentsOverview {
    pub organization_id: String,
    pub provider: ProviderStatus,
    pub customer: Option<PaymentCustomer>,
    pub subscription: Option<PaymentSubscription>,
    pub plan_price: Option<PlanPrice>,
    pub seats: Option<u32>,
    pub currency: Option<String>,
    pub currency_exponent: Option<u32>,
    pub invoice_count: u64,
    pub open_invoice_count: u64,
    pub outstanding_minor: i64,
    pub paid_minor: i64,
    pub payment_method_count: u64,
    pub tax: TaxSurface,
    pub payment: CapabilityStatus,
    pub invoices: CapabilityStatus,
    pub seats_capability: CapabilityStatus,
    pub tax_capability: CapabilityStatus,
    pub currency_capability: CapabilityStatus,
    pub now_ms: i64,
}

/// Webhook processing disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookDisposition {
    Applied,
    Duplicate,
    StaleIgnored,
    Ignored,
}

/// The result of one verified webhook delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebhookOutcome {
    pub disposition: WebhookDisposition,
    pub event_id: String,
    pub event_type: String,
}

/// The result of one lifecycle tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LifecycleOutcome {
    pub subscription: PaymentSubscription,
    pub changed: bool,
}

// ---------------------------------------------------------------- requests

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeRequest {
    pub plan_id: String,
    pub interval: BillingInterval,
    pub seats: u32,
    pub trial_days: Option<u32>,
    pub customer_email: String,
    pub customer_name: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePlanRequest {
    pub subscription_id: String,
    pub plan_id: String,
    pub interval: Option<BillingInterval>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewPlanRequest {
    pub subscription_id: String,
    pub plan_id: String,
    pub interval: Option<BillingInterval>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub subscription_id: String,
    pub at_period_end: bool,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeSeatsRequest {
    pub subscription_id: String,
    pub quantity: u32,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewSeatsRequest {
    pub subscription_id: String,
    pub quantity: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageCharge {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueInvoiceRequest {
    pub subscription_id: String,
    pub usage: Vec<UsageCharge>,
    pub credits_minor: i64,
    pub discount_minor: i64,
    pub tax_jurisdiction: Option<String>,
    pub due_in_days: Option<i64>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayInvoiceRequest {
    pub invoice_id: String,
    pub payment_method_id: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefundRequest {
    pub payment_id: String,
    pub amount_minor: i64,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddPaymentMethodRequest {
    pub token: String,
    pub brand: String,
    pub last4: String,
    pub exp_month: u8,
    pub exp_year: u16,
    pub set_default: bool,
    pub idempotency_key: String,
}
