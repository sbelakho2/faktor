//! Faktor-owned invoice authority and linked provider observations.
//!
//! The durable [`crate::model::Invoice`] row is the ONE invoice truth: its
//! number, line items, totals, tax, credits, status, period and organization
//! are authored and persisted by this crate. Provider invoices are upstream
//! records: [`ProviderInvoiceObservation`] stores what the provider reported
//! (id, status, amount, currency, paid_at, refunds) linked to the Faktor
//! invoice. An observation never supplies an invoice total and is never read
//! as an invoice source by a route, the CLI or the overview; it can only
//! produce a typed [`InvoiceReconciliationState`].
//!
//! A mismatch is a first-class durable state, not an overwrite: the Faktor
//! totals stay exactly as authored, while the observation records the
//! provider's divergent amount/currency for operator review.

use serde::{Deserialize, Serialize};

/// Hard bound on one reconciliation pass.
pub const MAX_RECONCILIATION_INVOICES: usize = 5_000;

/// The reconciliation state of one Faktor invoice against its linked
/// provider invoice observation.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceReconciliationState {
    /// No provider observation has been recorded yet.
    #[default]
    Unreconciled,
    /// The provider invoice exists and agrees with the Faktor total/currency.
    Matched,
    /// The provider reported a different amount; Faktor totals are unchanged.
    AmountMismatch,
    /// The provider reported a different currency; Faktor currency is
    /// unchanged.
    CurrencyMismatch,
    /// The provider has no invoice for the linked provider id.
    ProviderInvoiceMissing,
}

impl InvoiceReconciliationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            InvoiceReconciliationState::Unreconciled => "unreconciled",
            InvoiceReconciliationState::Matched => "matched",
            InvoiceReconciliationState::AmountMismatch => "amount_mismatch",
            InvoiceReconciliationState::CurrencyMismatch => "currency_mismatch",
            InvoiceReconciliationState::ProviderInvoiceMissing => "provider_invoice_missing",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "unreconciled" => InvoiceReconciliationState::Unreconciled,
            "matched" => InvoiceReconciliationState::Matched,
            "amount_mismatch" => InvoiceReconciliationState::AmountMismatch,
            "currency_mismatch" => InvoiceReconciliationState::CurrencyMismatch,
            "provider_invoice_missing" => InvoiceReconciliationState::ProviderInvoiceMissing,
            _ => return None,
        })
    }

    /// A conflict state is surfaced, never silently "fixed" by rewriting the
    /// Faktor invoice.
    pub const fn is_conflict(self) -> bool {
        matches!(
            self,
            InvoiceReconciliationState::AmountMismatch
                | InvoiceReconciliationState::CurrencyMismatch
                | InvoiceReconciliationState::ProviderInvoiceMissing
        )
    }
}

impl std::fmt::Display for InvoiceReconciliationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where one provider observation came from. The source is part of the
/// durable record so a webhook observation can never masquerade as a
/// reconciliation fetch (or vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderObservationSource {
    /// Backfilled from a pre-observation (schema v1/v2) row.
    Migration,
    /// Recorded while creating the provider invoice for a Faktor invoice.
    Issue,
    /// Recorded from a verified webhook delivery.
    Webhook,
    /// Recorded from a read-only provider fetch.
    Reconciliation,
    /// Recorded while dispatching a charge for a Faktor invoice.
    Payment,
    /// Recorded while dispatching a refund for a Faktor invoice payment.
    Refund,
}

impl ProviderObservationSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderObservationSource::Migration => "migration",
            ProviderObservationSource::Issue => "issue",
            ProviderObservationSource::Webhook => "webhook",
            ProviderObservationSource::Reconciliation => "reconciliation",
            ProviderObservationSource::Payment => "payment",
            ProviderObservationSource::Refund => "refund",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "migration" => ProviderObservationSource::Migration,
            "issue" => ProviderObservationSource::Issue,
            "webhook" => ProviderObservationSource::Webhook,
            "reconciliation" => ProviderObservationSource::Reconciliation,
            "payment" => ProviderObservationSource::Payment,
            "refund" => ProviderObservationSource::Refund,
            _ => return None,
        })
    }
}

/// One linked provider-invoice observation. `invoice_id` always names the
/// Faktor invoice; no read path treats `amount_minor` as an invoice total.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInvoiceObservation {
    pub provider: String,
    pub provider_invoice_id: String,
    pub organization_id: String,
    /// The authoritative Faktor invoice this observation is linked to.
    pub invoice_id: String,
    /// The provider's own status string, preserved verbatim for audit.
    pub status: String,
    pub amount_minor: i64,
    pub amount_paid_minor: i64,
    pub amount_refunded_minor: i64,
    pub currency: String,
    pub paid_at_ms: Option<i64>,
    pub observed_ms: i64,
    pub source: ProviderObservationSource,
    pub reconciliation: InvoiceReconciliationState,
}

/// One linked provider-payment observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPaymentObservation {
    pub provider: String,
    pub provider_payment_id: String,
    pub organization_id: String,
    pub invoice_id: String,
    /// The Faktor payment row, when this observation was reconciled into one.
    pub payment_id: Option<String>,
    pub status: String,
    pub amount_minor: i64,
    pub refunded_minor: i64,
    pub currency: String,
    pub paid_at_ms: Option<i64>,
    pub observed_ms: i64,
    pub source: ProviderObservationSource,
}

/// One linked provider-refund observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRefundObservation {
    pub provider: String,
    pub provider_refund_id: String,
    pub organization_id: String,
    pub invoice_id: String,
    pub payment_id: String,
    pub status: String,
    pub amount_minor: i64,
    pub currency: String,
    pub observed_ms: i64,
    pub source: ProviderObservationSource,
}

/// Counts of one organization's reconciliation states, derived from Faktor
/// invoice rows (never from provider totals).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationSummary {
    pub unreconciled: u64,
    pub matched: u64,
    pub amount_mismatch: u64,
    pub currency_mismatch: u64,
    pub provider_invoice_missing: u64,
}

impl ReconciliationSummary {
    pub const fn conflicts(&self) -> u64 {
        self.amount_mismatch + self.currency_mismatch + self.provider_invoice_missing
    }
}

/// One invoice's reconciliation result in a manual/periodic pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReconciliationItem {
    pub invoice_id: String,
    pub number: String,
    pub provider_invoice_id: String,
    pub state: InvoiceReconciliationState,
    pub observed_amount_minor: Option<i64>,
    pub observed_currency: Option<String>,
    pub observed_status: Option<String>,
}

/// The result of one bounded reconciliation pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReconciliationReport {
    pub organization_id: String,
    pub checked: u64,
    pub matched: u64,
    pub conflicts: u64,
    pub missing: u64,
    pub unreconciled: u64,
    pub observed_ms: i64,
    pub items: Vec<ReconciliationItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_state_roundtrips_and_conflicts_are_typed() {
        for state in [
            InvoiceReconciliationState::Unreconciled,
            InvoiceReconciliationState::Matched,
            InvoiceReconciliationState::AmountMismatch,
            InvoiceReconciliationState::CurrencyMismatch,
            InvoiceReconciliationState::ProviderInvoiceMissing,
        ] {
            assert_eq!(
                InvoiceReconciliationState::parse(state.as_str()),
                Some(state)
            );
            assert_eq!(state.to_string(), state.as_str());
        }
        assert!(!InvoiceReconciliationState::Unreconciled.is_conflict());
        assert!(!InvoiceReconciliationState::Matched.is_conflict());
        assert!(InvoiceReconciliationState::AmountMismatch.is_conflict());
        assert!(InvoiceReconciliationState::CurrencyMismatch.is_conflict());
        assert!(InvoiceReconciliationState::ProviderInvoiceMissing.is_conflict());
        assert_eq!(
            InvoiceReconciliationState::default(),
            InvoiceReconciliationState::Unreconciled
        );
    }

    #[test]
    fn observation_sources_roundtrip() {
        for source in [
            ProviderObservationSource::Migration,
            ProviderObservationSource::Issue,
            ProviderObservationSource::Webhook,
            ProviderObservationSource::Reconciliation,
            ProviderObservationSource::Payment,
            ProviderObservationSource::Refund,
        ] {
            assert_eq!(
                ProviderObservationSource::parse(source.as_str()),
                Some(source)
            );
        }
        assert_eq!(ProviderObservationSource::parse("forged"), None);
    }

    #[test]
    fn summary_conflict_count_never_underflows() {
        let summary = ReconciliationSummary {
            unreconciled: 1,
            matched: 2,
            amount_mismatch: 3,
            currency_mismatch: 4,
            provider_invoice_missing: 5,
        };
        assert_eq!(summary.conflicts(), 12);
    }
}
