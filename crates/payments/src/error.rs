//! Typed payment-domain failures. Codes and statuses are stable; no variant
//! carries a raw credential, PAN or provider request body.

use serde::Serialize;

/// One billing-payment refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PaymentError {
    #[error("payment provider is not configured: {0}")]
    ProviderUnconfigured(String),
    #[error("payment provider call failed: {0}")]
    Provider(String),
    #[error("payment webhook unauthorized: {0}")]
    WebhookUnauthorized(String),
    #[error("payment webhook timestamp is outside the replay window")]
    WebhookStaleTimestamp,
    #[error("payment webhook payload is malformed: {0}")]
    WebhookMalformed(String),
    #[error("payment request is malformed: {0}")]
    Malformed(String),
    #[error("payment object not found: {0}")]
    NotFound(String),
    #[error("payment conflict: {0}")]
    Conflict(String),
    #[error("payment configuration error: {0}")]
    Config(String),
    #[error("payment store unavailable: {0}")]
    Store(String),
    #[error("plan price is not configured for {0}")]
    PlanPriceUnconfigured(String),
    #[error("currency mismatch: {0}")]
    CurrencyMismatch(String),
    #[error("no explicit FX rate is configured for {0}->{1}")]
    FxUnconfigured(String, String),
    #[error("tax jurisdiction is not configured: {0}")]
    TaxUnconfigured(String),
    #[error("invoice has no open balance")]
    NothingDue,
    #[error(
        "refund exceeds the paid amount (paid {paid}, already refunded {refunded}, requested \
         {requested})"
    )]
    RefundExceedsPaid {
        paid: i64,
        refunded: i64,
        requested: i64,
    },
    #[error("payment declined: {0}")]
    Declined(String),
    #[error("idempotency key conflict: {0}")]
    IdempotencyConflict(String),
    #[error("operation is not permitted in state {0}")]
    IllegalState(String),
}

impl PaymentError {
    /// The stable machine code.
    pub const fn code(&self) -> &'static str {
        match self {
            PaymentError::ProviderUnconfigured(_) => "payment_provider_unconfigured",
            PaymentError::Provider(_) => "payment_provider_error",
            PaymentError::WebhookUnauthorized(_) => "payment_webhook_unauthorized",
            PaymentError::WebhookStaleTimestamp => "payment_webhook_stale",
            PaymentError::WebhookMalformed(_) => "malformed",
            PaymentError::Malformed(_) => "malformed",
            PaymentError::NotFound(_) => "not_found",
            PaymentError::Conflict(_) => "conflict",
            PaymentError::Config(_) => "payment_config",
            PaymentError::Store(_) => "payment_store_unavailable",
            PaymentError::PlanPriceUnconfigured(_) => "plan_price_unconfigured",
            PaymentError::CurrencyMismatch(_) => "currency_mismatch",
            PaymentError::FxUnconfigured(_, _) => "fx_rate_unconfigured",
            PaymentError::TaxUnconfigured(_) => "tax_unconfigured",
            PaymentError::NothingDue => "nothing_due",
            PaymentError::RefundExceedsPaid { .. } => "refund_exceeds_paid",
            PaymentError::Declined(_) => "payment_declined",
            PaymentError::IdempotencyConflict(_) => "idempotency_conflict",
            PaymentError::IllegalState(_) => "illegal_state",
        }
    }

    /// The HTTP status the native route answers.
    pub const fn http_status(&self) -> u16 {
        match self {
            PaymentError::WebhookUnauthorized(_)
            | PaymentError::WebhookStaleTimestamp
            | PaymentError::WebhookMalformed(_) => 401,
            PaymentError::Malformed(_) => 400,
            PaymentError::NotFound(_) => 404,
            PaymentError::Declined(_) => 402,
            PaymentError::ProviderUnconfigured(_)
            | PaymentError::Conflict(_)
            | PaymentError::IdempotencyConflict(_)
            | PaymentError::PlanPriceUnconfigured(_)
            | PaymentError::CurrencyMismatch(_)
            | PaymentError::FxUnconfigured(_, _)
            | PaymentError::TaxUnconfigured(_)
            | PaymentError::NothingDue
            | PaymentError::RefundExceedsPaid { .. }
            | PaymentError::IllegalState(_) => 409,
            PaymentError::Provider(_) | PaymentError::Config(_) => 502,
            PaymentError::Store(_) => 503,
        }
    }

    pub const fn retryable(&self) -> bool {
        matches!(self, PaymentError::Store(_) | PaymentError::Provider(_))
    }

    /// True when a provider refusal PROVES no remote effect was created.
    /// Anything else (transport, provider 5xx, store) is ambiguous and must
    /// become an `unknown` operation claim.
    pub const fn is_definitive_dispatch_failure(&self) -> bool {
        matches!(
            self,
            PaymentError::ProviderUnconfigured(_)
                | PaymentError::WebhookUnauthorized(_)
                | PaymentError::WebhookStaleTimestamp
                | PaymentError::WebhookMalformed(_)
                | PaymentError::Malformed(_)
                | PaymentError::NotFound(_)
                | PaymentError::Config(_)
                | PaymentError::PlanPriceUnconfigured(_)
                | PaymentError::CurrencyMismatch(_)
                | PaymentError::FxUnconfigured(_, _)
                | PaymentError::TaxUnconfigured(_)
                | PaymentError::NothingDue
                | PaymentError::RefundExceedsPaid { .. }
                | PaymentError::Declined(_)
                | PaymentError::IllegalState(_)
        )
    }

    /// Rebuild the typed refusal recorded on a failed operation claim.
    pub fn from_recorded(code: &str, message: &str) -> Self {
        match code {
            "payment_provider_unconfigured" => {
                PaymentError::ProviderUnconfigured(message.to_string())
            }
            "payment_provider_error" => PaymentError::Provider(message.to_string()),
            "malformed" => PaymentError::Malformed(message.to_string()),
            "not_found" => PaymentError::NotFound(message.to_string()),
            "conflict" => PaymentError::Conflict(message.to_string()),
            "payment_config" => PaymentError::Config(message.to_string()),
            "payment_store_unavailable" => PaymentError::Store(message.to_string()),
            "plan_price_unconfigured" => PaymentError::PlanPriceUnconfigured(message.to_string()),
            "currency_mismatch" => PaymentError::CurrencyMismatch(message.to_string()),
            "fx_rate_unconfigured" => PaymentError::FxUnconfigured(
                "recorded".into(),
                "provider reconciliation required".into(),
            ),
            "tax_unconfigured" => PaymentError::TaxUnconfigured(message.to_string()),
            "nothing_due" => PaymentError::NothingDue,
            "refund_exceeds_paid" => PaymentError::Conflict(message.to_string()),
            "payment_declined" => PaymentError::Declined(message.to_string()),
            "idempotency_conflict" => PaymentError::IdempotencyConflict(message.to_string()),
            "illegal_state" => PaymentError::IllegalState(message.to_string()),
            _ => PaymentError::Conflict(message.to_string()),
        }
    }
}

/// The wire shape of one refusal.
#[derive(Debug, Clone, Serialize)]
pub struct PaymentErrorBody {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl PaymentError {
    pub fn body(&self) -> PaymentErrorBody {
        PaymentErrorBody {
            code: self.code(),
            message: self.to_string(),
            retryable: self.retryable(),
        }
    }
}
