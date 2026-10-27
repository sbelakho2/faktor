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
//! - [`mollie::MolliePaymentProvider`]: the Mollie Payments adapter
//!   (payments, payment links, refunds, customers, subscriptions, mandates,
//!   stored-mandate charges) over the same checked egress transport. Mollie
//!   webhooks carry only an id; the adapter fetches the resource before any
//!   state is applied and verifies the Faktor metadata binding;
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
//! route/export/UI, no secret value is logged, webhook authentication
//! (signature/timestamp where the provider signs, an authoritative id-bound
//! API fetch where it does not) runs before any state is touched, and every
//! query is organization-scoped.
// TEST-SEPARATION: oracle=independent-reference isolation=isolated-data-dir

pub mod authority;
pub mod catalog;
pub mod error;
pub mod lease;
pub mod local;
pub mod model;
pub mod mollie;
pub mod money;
pub mod provider;
#[cfg(test)]
mod scale_tests;
pub mod secrets;
pub mod service;
pub mod store;
pub mod stripe;
mod subscription_ops;
pub mod vault;
pub mod webhook;

pub use authority::{
    InvoiceReconciliationState, ProviderInvoiceObservation, ProviderObservationSource,
    ProviderPaymentObservation, ProviderRefundObservation, ReconciliationItem,
    ReconciliationReport, ReconciliationSummary, MAX_RECONCILIATION_INVOICES,
};
pub use catalog::{
    BillingInterval, CurrencySpec, DunningConfig, FxRate, MollieConfig, PaymentsConfig, PlanPrice,
    PriceCatalogue, ProviderKind, StripeConfig, TaxConfig, TaxRate,
};
pub use error::{PaymentError, PaymentErrorBody};
pub use lease::{
    ConflictDomain, FinancialMutationLease, LeaseAcquisition, LeaseAmount, LeaseClaim,
    LeaseRecovery, LeaseState, TerminalOutcome, UnknownReconciliation,
};
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
    EmptyWebhookBindings, PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest,
    ProviderCharge, ProviderChargeRequest, ProviderCheckout, ProviderCheckoutRequest,
    ProviderCustomer, ProviderCustomerRequest, ProviderCustomerUpdate, ProviderInvoice,
    ProviderInvoiceRequest, ProviderInvoiceSnapshot, ProviderMandate, ProviderMandateRequest,
    ProviderObjectKind, ProviderPaymentLink, ProviderPaymentLinkRequest, ProviderPaymentSnapshot,
    ProviderReconcileRequest, ProviderReconciled, ProviderRefund, ProviderRefundRequest,
    ProviderSubscribeRequest, ProviderSubscription, UnconfiguredPaymentProvider,
    VerifiedWebhookEvent, WebhookBinding, WebhookBindingLookup, WebhookHeaders,
};
pub use secrets::{EnvSecrets, SecretSource, SecretString, StaticSecrets};
pub use service::{
    validate_payment_token, PaymentService, MAX_DUE_DAYS, MAX_EXPORT_BYTES, MAX_INVOICE_PAGE,
    MAX_PAYMENT_METHODS, MAX_SEATS, MAX_TEXT_BYTES, MAX_TRIAL_DAYS, MAX_USAGE_LINES,
};
pub use store::{
    ClaimRecovery, IdempotencyRecord, IdempotencyWrite, InvoiceStats, ProviderObjectWrite,
    SqlitePaymentStore, WebhookDecision, WebhookEffects, WebhookPlan, WebhookRecord,
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
        ProviderKind::Mollie => {
            PaymentService::open_mollie(store, config, clock, secrets, transport)
        }
    }
}
