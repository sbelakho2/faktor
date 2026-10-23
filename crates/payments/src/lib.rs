//! faktor-payments — the durable payment/invoice/subscription subsystem.
//!
//! Surfaces:
//!
//! - [`provider::PaymentProvider`]: create customer, subscribe, change plan,
//!   cancel, create invoice, charge, refund, verify webhook;
//! - [`local::LocalPaymentProvider`]: a fully offline SQLite-backed provider
//!   used by the local product and tests;
//! - [`stripe::StripePaymentProvider`]: a Stripe-compatible adapter over the
//!   daemon's checked egress transport, with env-var-named credentials;
//! - [`catalog`]: exact integer plan prices (monthly/annual, per plan
//!   currency), explicit FX, per-jurisdiction inclusive/exclusive tax and the
//!   dunning policy;
//! - [`service::PaymentService`]: subscription lifecycle, invoices, seats,
//!   proration, payment methods, bounded refunds/credit notes, exports and
//!   verified webhook ingestion;
//! - [`store::SqlitePaymentStore`]: the durable store (`payment.db`), with
//!   request-bound idempotency committed in the same transaction as its
//!   effect.
//!
//! Security invariants: no raw PAN is accepted or stored, no secret value is
//! logged or serialized, webhook signature/timestamp verification runs
//! before any state is touched, and every query is organization-scoped.

pub mod catalog;
pub mod error;
pub mod local;
pub mod model;
pub mod money;
pub mod provider;
pub mod secrets;
pub mod service;
pub mod store;
pub mod stripe;
pub mod webhook;

pub use catalog::{
    BillingInterval, CurrencySpec, DunningConfig, FxRate, PaymentsConfig, PlanPrice,
    PriceCatalogue, ProviderKind, StripeConfig, TaxConfig, TaxRate,
};
pub use error::{PaymentError, PaymentErrorBody};
pub use local::{sign_local_webhook, LocalPaymentProvider};
pub use model::{
    AddPaymentMethodRequest, CancelRequest, CapabilityStatus, ChangePlanRequest,
    ChangeSeatsRequest, CreditNote, Invoice, InvoiceLine, InvoiceLineKind, InvoiceOutcome,
    InvoicePage, InvoiceStatus, IssueInvoiceRequest, LifecycleOutcome, PayInvoiceOutcome,
    PayInvoiceRequest, Payment, PaymentCustomer, PaymentMethod, PaymentMethodOutcome,
    PaymentMethodView, PaymentStatus, PaymentSubscription, PaymentsOverview, PendingProration,
    PlanChangeOutcome, PreviewPlanRequest, PreviewSeatsRequest, ProrationPreview, ProviderStatus,
    Refund, RefundOutcome, RefundRequest, SeatChangeOutcome, SubscribeRequest, SubscriptionOutcome,
    SubscriptionState, TaxSurface, UsageCharge, WebhookDisposition, WebhookOutcome,
};
pub use money::{
    convert, exclusive_tax, format_minor, inclusive_tax, prorate, valid_currency, MAX_MONEY_MINOR,
};
pub use provider::{
    PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge,
    ProviderChargeRequest, ProviderCustomer, ProviderCustomerRequest, ProviderInvoice,
    ProviderInvoiceRequest, ProviderRefund, ProviderRefundRequest, ProviderSubscribeRequest,
    ProviderSubscription, UnconfiguredPaymentProvider, VerifiedWebhookEvent, WebhookHeaders,
};
pub use secrets::{EnvSecrets, SecretSource, SecretString, StaticSecrets};
pub use service::{
    validate_payment_token, PaymentService, MAX_DUE_DAYS, MAX_EXPORT_BYTES, MAX_INVOICE_PAGE,
    MAX_SEATS, MAX_TEXT_BYTES, MAX_TRIAL_DAYS, MAX_USAGE_LINES,
};
pub use store::{IdempotencyRecord, IdempotencyWrite, InvoiceStats, SqlitePaymentStore};
pub use webhook::{
    constant_time_eq, hmac_sha256_hex, sign_webhook_payload, verify_webhook,
    DEFAULT_WEBHOOK_WINDOW_MS, MAX_WEBHOOK_BODY_BYTES,
};

use std::sync::Arc;

use faktor_cloud::Clock;
use faktor_provider::egress::HttpTransport;

/// Build the configured provider/service pair. A local deployment needs no
/// transport; the Stripe adapter is constructed only with the checked
/// transport the caller supplies.
pub fn build_service(
    store: Arc<SqlitePaymentStore>,
    config: PaymentsConfig,
    clock: Arc<dyn Clock>,
    transport: Option<Arc<dyn HttpTransport>>,
    secrets: Arc<dyn SecretSource>,
) -> Result<PaymentService, PaymentError> {
    match config.provider {
        ProviderKind::Local => PaymentService::open_local(store, config, clock, secrets),
        ProviderKind::Stripe => {
            PaymentService::open_stripe(store, config, clock, secrets, transport)
        }
    }
}
