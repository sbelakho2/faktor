//! Durable payment/invoice/subscription vocabulary. Money is always an
//! integer minor-unit amount plus an ISO-shaped currency; no field derives a
//! total from a float.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::authority::{InvoiceReconciliationState, ReconciliationSummary};
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

    /// Provider events arrive at-least-once and possibly late. A canceled
    /// subscription is terminal and a live subscription never regresses to
    /// the pre-payment trial state.
    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            SubscriptionState::Canceled => matches!(next, SubscriptionState::Canceled),
            SubscriptionState::Active | SubscriptionState::PastDue => {
                !matches!(next, SubscriptionState::Trialing)
            }
            SubscriptionState::Trialing => true,
        }
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
    /// Durable provider item identity used for every later update (Stripe
    /// `subscription_item` id). Never recomputed from local plan data.
    #[serde(default)]
    pub provider_item_id: Option<String>,
    /// Durable provider product identity used by inline `price_data`.
    #[serde(default)]
    pub provider_product_id: Option<String>,
    /// Durable provider price identity returned with the subscription.
    #[serde(default)]
    pub provider_price_id: Option<String>,
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

/// A stored payment method. This type is deliberately token-free: the
/// provider token exists only as vault ciphertext in
/// [`StoredPaymentMethod`] and is never serialized to a route, export, log
/// or UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentMethod {
    pub id: String,
    pub organization_id: String,
    pub provider: String,
    pub brand: String,
    pub last4: String,
    pub exp_month: u8,
    pub exp_year: u16,
    pub is_default: bool,
    pub created_ms: i64,
}

/// The at-rest payment-method row. `token_ciphertext` is AEAD-sealed vault
/// material bound to `(organization_id, id)`; plaintext never enters the
/// store and the row is never rendered on a wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPaymentMethod {
    #[serde(flatten)]
    pub method: PaymentMethod,
    pub token_ciphertext: String,
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

impl From<&StoredPaymentMethod> for PaymentMethodView {
    fn from(stored: &StoredPaymentMethod) -> Self {
        Self::from(&stored.method)
    }
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

    /// `paid`, `void` and `uncollectible` are terminal. A late provider
    /// event can never reopen a settled invoice.
    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            InvoiceStatus::Draft => true,
            InvoiceStatus::Open => matches!(
                next,
                InvoiceStatus::Open
                    | InvoiceStatus::PastDue
                    | InvoiceStatus::Paid
                    | InvoiceStatus::Void
                    | InvoiceStatus::Uncollectible
            ),
            InvoiceStatus::PastDue => matches!(
                next,
                InvoiceStatus::PastDue
                    | InvoiceStatus::Paid
                    | InvoiceStatus::Void
                    | InvoiceStatus::Uncollectible
            ),
            InvoiceStatus::Paid => matches!(next, InvoiceStatus::Paid),
            InvoiceStatus::Void => matches!(next, InvoiceStatus::Void),
            InvoiceStatus::Uncollectible => matches!(next, InvoiceStatus::Uncollectible),
        }
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
    /// The exact provider idempotency key used to create the linked provider
    /// invoice. Reconciliation can look the operation up by this key when the
    /// provider cannot fetch by invoice id.
    #[serde(default)]
    pub provider_idempotency_key: Option<String>,
    /// The provider observation state for this invoice. `Unreconciled` on
    /// every pre-observation row (and every new row until an observation is
    /// recorded).
    #[serde(default)]
    pub reconciliation: InvoiceReconciliationState,
    pub currency: String,
    pub status: InvoiceStatus,
    /// Identity of the provider payment whose failure last advanced the
    /// dunning attempt counter. Provider retries usually fan one failed
    /// attempt out into both a payment-intent event and an invoice event;
    /// comparing this identity keeps the attempt counter exact while a new
    /// payment identity still advances it.
    #[serde(default)]
    pub last_dunning_payment_id: Option<String>,
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

/// Payment state. Provider `processing`, `requires_action` and `pending`
/// answers are durable transient states: they are never reported as
/// succeeded and never settle an invoice or a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Pending,
    Processing,
    RequiresAction,
    Succeeded,
    PartiallyRefunded,
    Refunded,
    Failed,
}

impl PaymentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            PaymentStatus::Pending => "pending",
            PaymentStatus::Processing => "processing",
            PaymentStatus::RequiresAction => "requires_action",
            PaymentStatus::Succeeded => "succeeded",
            PaymentStatus::PartiallyRefunded => "partially_refunded",
            PaymentStatus::Refunded => "refunded",
            PaymentStatus::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" => PaymentStatus::Pending,
            "processing" => PaymentStatus::Processing,
            "requires_action" => PaymentStatus::RequiresAction,
            "succeeded" => PaymentStatus::Succeeded,
            "partially_refunded" => PaymentStatus::PartiallyRefunded,
            "refunded" => PaymentStatus::Refunded,
            "failed" => PaymentStatus::Failed,
            _ => return None,
        })
    }

    /// True only when the provider has confirmed that money moved.
    pub const fn is_settled(self) -> bool {
        matches!(
            self,
            PaymentStatus::Succeeded | PaymentStatus::PartiallyRefunded | PaymentStatus::Refunded
        )
    }

    /// True for a provider answer that has not settled yet.
    pub const fn is_transient(self) -> bool {
        matches!(
            self,
            PaymentStatus::Pending | PaymentStatus::Processing | PaymentStatus::RequiresAction
        )
    }

    /// Monotonic payment-state order: a settled or failed payment never
    /// regresses to a provider-transient state.
    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            PaymentStatus::Pending => matches!(
                next,
                PaymentStatus::Processing
                    | PaymentStatus::RequiresAction
                    | PaymentStatus::Succeeded
                    | PaymentStatus::Failed
            ),
            PaymentStatus::Processing | PaymentStatus::RequiresAction => matches!(
                next,
                PaymentStatus::Pending
                    | PaymentStatus::Processing
                    | PaymentStatus::RequiresAction
                    | PaymentStatus::Succeeded
                    | PaymentStatus::Failed
            ),
            PaymentStatus::Succeeded => {
                matches!(
                    next,
                    PaymentStatus::PartiallyRefunded | PaymentStatus::Refunded
                )
            }
            PaymentStatus::PartiallyRefunded => matches!(next, PaymentStatus::Refunded),
            PaymentStatus::Refunded | PaymentStatus::Failed => false,
        }
    }
}

/// Refund truth. A provider `pending`/`processing` refund is durable but
/// unsettled; a failed refund never touches the payment or invoice ledger.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RefundStatus {
    #[default]
    Pending,
    Succeeded,
    Failed,
}

impl RefundStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            RefundStatus::Pending => "pending",
            RefundStatus::Succeeded => "succeeded",
            RefundStatus::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" | "processing" | "requires_action" => RefundStatus::Pending,
            "succeeded" => RefundStatus::Succeeded,
            "failed" | "canceled" => RefundStatus::Failed,
            _ => return None,
        })
    }

    /// Pending refunds may only resolve to a terminal provider truth.
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(self, RefundStatus::Pending) && !matches!(next, RefundStatus::Pending)
    }
}

fn legacy_refund_status() -> RefundStatus {
    RefundStatus::Succeeded
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
    /// Rows written before the status field existed only existed after a
    /// successful provider refund, so their legacy default is succeeded.
    #[serde(default = "legacy_refund_status")]
    pub status: RefundStatus,
    /// Set exactly once, in the same transaction that settles the refund.
    #[serde(default)]
    pub credit_note_id: Option<String>,
    /// The Faktor invoice line this refund/credit note is bound to.
    #[serde(default)]
    pub invoice_line_seq: Option<u32>,
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
    /// The Faktor invoice line this credit note is bound to.
    #[serde(default)]
    pub invoice_line_seq: Option<u32>,
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
    /// Present only once the provider confirmed the refund (and therefore
    /// the credit note was allocated and settled in the same transaction).
    pub credit_note: Option<CreditNote>,
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
    pub refunded_minor: i64,
    /// Counts of the Faktor invoices' provider-reconciliation states. Never
    /// derived from provider totals.
    pub reconciliation: ReconciliationSummary,
    pub payment_method_count: u64,
    pub tax: TaxSurface,
    pub payment: CapabilityStatus,
    pub invoices: CapabilityStatus,
    pub seats_capability: CapabilityStatus,
    pub tax_capability: CapabilityStatus,
    pub currency_capability: CapabilityStatus,
    pub now_ms: i64,
}

/// The kind of financial resource a provider operation mutates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinancialResourceType {
    Customer,
    Subscription,
    Invoice,
    Payment,
    Refund,
    Checkout,
}

impl FinancialResourceType {
    pub const fn as_str(self) -> &'static str {
        match self {
            FinancialResourceType::Customer => "customer",
            FinancialResourceType::Subscription => "subscription",
            FinancialResourceType::Invoice => "invoice",
            FinancialResourceType::Payment => "payment",
            FinancialResourceType::Refund => "refund",
            FinancialResourceType::Checkout => "checkout",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "customer" => FinancialResourceType::Customer,
            "subscription" => FinancialResourceType::Subscription,
            "invoice" => FinancialResourceType::Invoice,
            "payment" => FinancialResourceType::Payment,
            "refund" => FinancialResourceType::Refund,
            "checkout" => FinancialResourceType::Checkout,
            _ => return None,
        })
    }
}

/// The durable dispatch state of one external provider operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationClaimState {
    /// Durably claimed, no provider call has been authorized yet.
    Claimed,
    /// The process crossed the provider boundary. A crash here is unknown.
    Dispatched,
    /// Dispatch happened but the outcome was not committed: reconcile with
    /// the provider; never dispatch again with this idempotency key.
    Unknown,
    Succeeded,
    Failed,
}

impl OperationClaimState {
    pub const fn as_str(self) -> &'static str {
        match self {
            OperationClaimState::Claimed => "claimed",
            OperationClaimState::Dispatched => "dispatched",
            OperationClaimState::Unknown => "unknown",
            OperationClaimState::Succeeded => "succeeded",
            OperationClaimState::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "claimed" => OperationClaimState::Claimed,
            "dispatched" => OperationClaimState::Dispatched,
            "unknown" => OperationClaimState::Unknown,
            "succeeded" => OperationClaimState::Succeeded,
            "failed" => OperationClaimState::Failed,
            _ => return None,
        })
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            OperationClaimState::Succeeded | OperationClaimState::Failed
        )
    }

    pub const fn is_in_flight(self) -> bool {
        matches!(
            self,
            OperationClaimState::Claimed | OperationClaimState::Dispatched
        )
    }
}

/// One durable operation claim, keyed by
/// `(organization_id, idempotency_key)` and bound to one resource tuple
/// `(organization, resource_type, resource_id)` and request digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationClaim {
    pub organization_id: String,
    pub idempotency_key: String,
    pub operation: String,
    pub resource_type: FinancialResourceType,
    pub resource_id: String,
    pub request_digest: String,
    /// The deterministic key sent to the provider for this operation; the
    /// reconciliation path looks up exactly this object and never creates
    /// a new one.
    pub provider_idempotency_key: String,
    pub state: OperationClaimState,
    /// Sanitized, non-secret operation context used only to rebuild the
    /// local transaction after provider reconciliation.
    #[serde(default)]
    pub context: String,
    pub response: Option<String>,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    pub created_ms: i64,
    pub dispatched_ms: Option<i64>,
    pub resolved_ms: Option<i64>,
    pub updated_ms: i64,
}

impl OperationClaim {
    pub fn is_replayable(&self) -> bool {
        self.state.is_terminal() && self.response.is_some()
    }
}

/// A request to acquire one operation claim, committed before any dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationClaimDraft {
    pub organization_id: String,
    pub idempotency_key: String,
    pub operation: String,
    pub resource_type: FinancialResourceType,
    pub resource_id: String,
    pub request_digest: String,
    pub provider_idempotency_key: String,
    pub context: String,
    pub created_ms: i64,
}

/// The result of acquiring a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimAcquisition {
    /// This caller is the only one allowed to dispatch.
    Acquired,
    /// Another caller holds the claim and may be at the provider boundary.
    InFlight(OperationClaim),
    /// A crash left the provider outcome unknown; reconciliation is required.
    Unresolved(OperationClaim),
    /// The operation already reached a recorded terminal outcome.
    Recorded(OperationClaim),
}

/// Webhook processing disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookDisposition {
    Applied,
    Duplicate,
    StaleIgnored,
    Ignored,
    /// The provider reported an invoice amount/currency that disagrees with
    /// the Faktor invoice. The observation is durable and the Faktor totals
    /// are unchanged; operator reconciliation is required.
    ReconciliationConflict,
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
