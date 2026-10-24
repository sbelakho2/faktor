//! faktor-payments — the durable payment/invoice/subscription subsystem.
//!
//! Surfaces:
//!
//! - [`provider::PaymentProvider`]: create customer, subscribe, change plan,
//!   cancel, create invoice, charge, refund, verify webhook;
//! - [`local::LocalPaymentProvider`]: a fully offline SQLite-backed provider
//!   used by the local product and tests;
//! - [`stripe::StripePaymentProvider`]: a Stripe-compatible adapter over the
//!   daemon's checked egress transport, with env-var-named credentials and
//!   durable provider subscription/item/product/price identity;
//! - [`adyen::AdyenPaymentProvider`]: the Adyen Checkout adapter (payment
//!   links, sessions, payments, refunds, stored tokens, in-body HMAC
//!   webhooks) over the same checked egress transport;
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
//! Security invariants: no raw PAN is accepted or stored, provider tokens are
//! AEAD-sealed by [`vault`] before persistence and are never serialized to a
//! route/export/UI, no secret value is logged, webhook signature/timestamp
//! verification runs before any state is touched, and every query is
//! organization-scoped.

pub mod adyen;
pub mod authority;
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
pub mod vault;
pub mod webhook;

pub use adyen::sign_adyen_webhook_hmac;
pub use authority::{
    InvoiceReconciliationState, ProviderInvoiceObservation, ProviderObservationSource,
    ProviderPaymentObservation, ProviderRefundObservation, ReconciliationItem,
    ReconciliationReport, ReconciliationSummary, MAX_RECONCILIATION_INVOICES,
};
pub use catalog::{
    AdyenConfig, BillingInterval, CurrencySpec, DunningConfig, FxRate, PaymentsConfig, PlanPrice,
    PriceCatalogue, ProviderKind, StripeConfig, TaxConfig, TaxRate,
};
pub use error::{PaymentError, PaymentErrorBody};
pub use local::{sign_local_webhook, LocalPaymentProvider};
pub use model::{
    AddPaymentMethodRequest, CancelRequest, CapabilityStatus, ChangePlanRequest,
    ChangeSeatsRequest, ClaimAcquisition, CreditNote, FinancialResourceType, Invoice, InvoiceLine,
    InvoiceLineKind, InvoiceOutcome, InvoicePage, InvoiceStatus, IssueInvoiceRequest,
    LifecycleOutcome, OperationClaim, OperationClaimDraft, OperationClaimState, PayInvoiceOutcome,
    PayInvoiceRequest, Payment, PaymentCustomer, PaymentMethod, PaymentMethodOutcome,
    PaymentMethodView, PaymentStatus, PaymentSubscription, PaymentsOverview, PendingProration,
    PlanChangeOutcome, PreviewPlanRequest, PreviewSeatsRequest, ProrationPreview, ProviderStatus,
    Refund, RefundOutcome, RefundRequest, RefundStatus, SeatChangeOutcome, StoredPaymentMethod,
    SubscribeRequest, SubscriptionOutcome, SubscriptionState, TaxSurface, UsageCharge,
    WebhookDisposition, WebhookOutcome,
};
pub use money::{
    convert, exclusive_tax, format_minor, inclusive_tax, prorate, valid_currency, MAX_MONEY_MINOR,
};
pub use provider::{
    PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge,
    ProviderChargeRequest, ProviderCheckout, ProviderCheckoutRequest, ProviderCustomer,
    ProviderCustomerRequest, ProviderInvoice, ProviderInvoiceRequest, ProviderInvoiceSnapshot,
    ProviderObjectKind, ProviderReconcileRequest, ProviderReconciled, ProviderRefund,
    ProviderRefundRequest, ProviderSubscribeRequest, ProviderSubscription,
    UnconfiguredPaymentProvider, VerifiedWebhookEvent, WebhookHeaders,
};
pub use secrets::{EnvSecrets, SecretSource, SecretString, StaticSecrets};
pub use service::{
    validate_payment_token, PaymentService, MAX_DUE_DAYS, MAX_EXPORT_BYTES, MAX_INVOICE_PAGE,
    MAX_SEATS, MAX_TEXT_BYTES, MAX_TRIAL_DAYS, MAX_USAGE_LINES,
};
pub use store::{
    ClaimRecovery, IdempotencyRecord, IdempotencyWrite, InvoiceStats, SqlitePaymentStore,
    WebhookDecision, WebhookEffects, WebhookPlan, WebhookRecord,
};
pub use vault::{PaymentTokenVault, MAX_TOKEN_BYTES, VAULT_KEY_LEN};
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
        ProviderKind::Adyen => PaymentService::open_adyen(store, config, clock, secrets, transport),
    }
}
