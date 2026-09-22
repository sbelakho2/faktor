//! The cloud-backed agent-side debit authority (Wave 5 residual): implements
//! the agent runtime's [`ProviderAttemptDebits`] seam over the durable
//! [`EntitlementService`] credit ledger.
//!
//! Semantics (all decided by configured data, never by a provider-name
//! comparison in the agent):
//!
//! - **Managed vs BYOK** comes from the configured managed-provider set
//!   ([`BillingConfig::category_of`]); a BYOK attempt is classified and
//!   records nothing — usage flows through the ordinary settlement path;
//! - **Record-before-call**: [`ProviderAttemptDebits::begin`] opens a durable
//!   credit hold (a pending consume) keyed by the attempt's own op id, so a
//!   replayed begin can never double-debit and a crash between hold and call
//!   leaves the hold in place — never a silently free managed call;
//! - **Settlement follows the reservation ledger's actual**
//!   ([`ProviderAttemptDebits::settle`] is called with the micro amount the
//!   session budget ledger folded, or not at all when the attempt is not
//!   settled); a pre-dispatch refusal refunds the full hold
//!   ([`ProviderAttemptDebits::refund`]).
//!
//! The hold token exposed to the agent is the attempt id (the idempotency
//! key); settle/refund resolve it back to the durable credit entry id.

use std::sync::Arc;

use faktor_agent::{
    DebitDecision, DebitError, DebitHold, ProviderAttemptDebit, ProviderAttemptDebits,
};
use faktor_cloud::{
    BillingAccountId, ControlPlaneError, CreditEntryId, EntitlementService, OrganizationId,
    SpendCategory,
};

/// The daemon's debit authority over one configured tenant organization and
/// billing account.
pub struct CloudAttemptDebits {
    service: Arc<EntitlementService>,
    organization: OrganizationId,
    account: BillingAccountId,
}

impl std::fmt::Debug for CloudAttemptDebits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudAttemptDebits")
            .field("organization", &self.organization)
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

impl CloudAttemptDebits {
    pub fn new(
        service: Arc<EntitlementService>,
        organization: OrganizationId,
        account: BillingAccountId,
    ) -> Self {
        Self {
            service,
            organization,
            account,
        }
    }

    /// Resolve the durable credit entry the hold's idempotency key recorded.
    fn entry_id(&self, hold: &DebitHold) -> Result<CreditEntryId, DebitError> {
        self.service
            .credit_entry_id_by_idempotency_key(&self.organization, &hold.id)
            .map_err(|e| DebitError::Unavailable {
                reason: e.to_string(),
            })?
            .ok_or_else(|| DebitError::Unavailable {
                reason: format!(
                    "no credit entry is recorded for hold {} (the record-before-call row is missing)",
                    hold.id
                ),
            })
    }

    /// Map one control-plane failure of a POST-hold credit transition: an
    /// already-settled/over-refunded/idempotency conflict is a local machine
    /// violation; a store failure is an infrastructure unavailability.
    fn transition_error(e: ControlPlaneError) -> DebitError {
        match e {
            ControlPlaneError::Conflict(detail) | ControlPlaneError::Malformed(detail) => {
                DebitError::InvalidState { detail }
            }
            other => DebitError::Unavailable {
                reason: other.to_string(),
            },
        }
    }
}

impl ProviderAttemptDebits for CloudAttemptDebits {
    fn is_managed(&self, provider: &str) -> bool {
        matches!(
            self.service.config().category_of(provider),
            SpendCategory::Managed
        )
    }

    fn begin(&self, attempt: &ProviderAttemptDebit) -> Result<DebitDecision, DebitError> {
        // BYOK (or a provider outside the configured managed set): usage is
        // recorded by the ordinary settlement path; nothing is debited.
        if !self.is_managed(&attempt.provider) {
            return Ok(DebitDecision::Byok);
        }
        if attempt.estimate_micro == 0 {
            return Err(DebitError::InvalidState {
                detail: "a managed attempt must carry a positive pre-call estimate".into(),
            });
        }
        match self.service.consume_before_call(
            &self.organization,
            &self.account,
            attempt.estimate_micro,
            None,
            &attempt.reason,
            Some(&attempt.attempt_id),
        ) {
            // Appended and Duplicate are both a durable hold for this
            // attempt id: the second one replayed the recorded entry.
            Ok(_) => DebitHold::new(attempt.attempt_id.clone(), attempt.estimate_micro)
                .map(DebitDecision::Hold),
            // Insufficient credits (and every other credit refusal) is an
            // authoritative pre-dispatch refusal: the provider must NOT be
            // called.
            Err(ControlPlaneError::Conflict(reason)) => Err(DebitError::Refused { reason }),
            Err(e) => Err(DebitError::Unavailable {
                reason: e.to_string(),
            }),
        }
    }

    fn settle(
        &self,
        attempt: &ProviderAttemptDebit,
        hold: &DebitHold,
        actual_micro: u64,
    ) -> Result<(), DebitError> {
        let entry = self.entry_id(hold)?;
        self.service
            .settle_consume(
                &self.organization,
                &self.account,
                &entry,
                actual_micro,
                &attempt.reason,
            )
            .map(|_| ())
            .map_err(Self::transition_error)
    }

    fn refund(
        &self,
        _attempt: &ProviderAttemptDebit,
        hold: &DebitHold,
        reason: &str,
    ) -> Result<(), DebitError> {
        let entry = self.entry_id(hold)?;
        self.service
            .refund_consume(
                &self.organization,
                &self.account,
                &entry,
                hold.amount_micro,
                reason,
            )
            .map(|_| ())
            .map_err(Self::transition_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_cloud::{BillingConfig, MemoryBillingStore, PlanConfig};

    const ATTEST: &str = "attest";

    fn test_service(managed: &[&str]) -> Arc<EntitlementService> {
        let config = BillingConfig {
            default_plan: Some("test".into()),
            plans: [(
                "test".to_string(),
                PlanConfig {
                    plan_id: "test".into(),
                    features: Default::default(),
                    limits: Default::default(),
                },
            )]
            .into_iter()
            .collect(),
            managed_providers: managed.iter().map(|p| p.to_string()).collect(),
        };
        let store = Arc::new(MemoryBillingStore::new());
        EntitlementService::with_system_clock(store, config).expect("service")
    }

    /// A ready adapter + its (org, account) over a funded account.
    fn funded(managed: &[&str], grant_micro: u64) -> (Arc<EntitlementService>, CloudAttemptDebits) {
        let service = test_service(managed);
        let organization = OrganizationId::try_new("org_test").unwrap();
        let account = BillingAccountId::try_new("acct_test").unwrap();
        service
            .ensure_account(&organization, &account, ATTEST, !managed.is_empty())
            .unwrap();
        if grant_micro > 0 {
            service
                .grant_credits(&organization, &account, grant_micro, "seed", None)
                .unwrap();
        }
        (
            service.clone(),
            CloudAttemptDebits::new(service, organization, account),
        )
    }

    /// A ready adapter + its tenant over an account with NO credits.
    fn unfunded() -> (Arc<EntitlementService>, CloudAttemptDebits, OrganizationId) {
        let service = test_service(&["managed-provider"]);
        let organization = OrganizationId::try_new("org_empty").unwrap();
        let account = BillingAccountId::try_new("acct_empty").unwrap();
        service
            .ensure_account(&organization, &account, ATTEST, true)
            .unwrap();
        (
            service.clone(),
            CloudAttemptDebits::new(service, organization.clone(), account),
            organization,
        )
    }

    fn attempt(id: &str) -> ProviderAttemptDebit {
        ProviderAttemptDebit {
            attempt_id: id.into(),
            provider: "managed-provider".into(),
            model: "m".into(),
            session_id: 1,
            task_id: Some(2),
            estimate_micro: 400,
            reason: "agent_provider_attempt".into(),
        }
    }

    #[test]
    fn managed_begin_opens_exactly_one_durable_hold_and_replays_idempotently() {
        let (service, debits) = funded(&["managed-provider"], 1_000);
        let organization = OrganizationId::try_new("org_test").unwrap();
        assert!(debits.is_managed("managed-provider"));
        assert_eq!(
            debits.begin(&attempt("41")).unwrap(),
            DebitDecision::Hold(DebitHold::new("41", 400).unwrap())
        );
        // A replay of the SAME attempt id (crash/retry of the local step)
        // returns the same hold and does not double-debit.
        assert_eq!(
            debits.begin(&attempt("41")).unwrap(),
            DebitDecision::Hold(DebitHold::new("41", 400).unwrap())
        );
        let balance = service.credit_balance(&organization).unwrap();
        assert_eq!(balance.pending_consumes, 1);
        assert_eq!(balance.held_micro, 400);
        assert_eq!(balance.balance_micro().unwrap(), 600);
    }

    #[test]
    fn settle_consumes_the_hold_at_the_actual_cost() {
        let (service, debits) = funded(&["managed-provider"], 1_000);
        let organization = OrganizationId::try_new("org_test").unwrap();
        let debit = attempt("42");
        let DebitDecision::Hold(hold) = debits.begin(&debit).unwrap() else {
            panic!("a managed attempt must hold");
        };
        debits.settle(&debit, &hold, 260).unwrap();
        let balance = service.credit_balance(&organization).unwrap();
        assert_eq!(balance.pending_consumes, 0, "the hold is consumed");
        assert_eq!(
            balance.consumed_micro, 260,
            "the debt is the ACTUAL, not the estimate"
        );
        assert_eq!(balance.balance_micro().unwrap(), 740);
    }

    #[test]
    fn pre_dispatch_refund_releases_the_hold_in_full() {
        let (service, debits) = funded(&["managed-provider"], 1_000);
        let organization = OrganizationId::try_new("org_test").unwrap();
        let debit = attempt("43");
        let DebitDecision::Hold(hold) = debits.begin(&debit).unwrap() else {
            panic!("a managed attempt must hold");
        };
        debits
            .refund(&debit, &hold, "dispatch_marker_failed")
            .unwrap();
        let balance = service.credit_balance(&organization).unwrap();
        // The refund is the offsetting append-only event: the hold's estimate
        // stays recorded (audit trail) while the free balance is fully
        // restored.
        assert_eq!(balance.refunded_micro, 400);
        assert_eq!(balance.balance_micro().unwrap(), 1_000);
    }

    #[test]
    fn byok_never_debits_and_insufficient_credits_refuses_pre_dispatch() {
        // BYOK provider: classified, nothing durable, no hold token.
        let (service, debits) = funded(&["managed-provider"], 1_000);
        let organization = OrganizationId::try_new("org_test").unwrap();
        let mut byok = attempt("44");
        byok.provider = "byok-provider".into();
        assert!(!debits.is_managed("byok-provider"));
        assert_eq!(debits.begin(&byok).unwrap(), DebitDecision::Byok);
        let balance = service.credit_balance(&organization).unwrap();
        assert_eq!(balance.pending_consumes, 0);
        assert_eq!(balance.consumed_micro, 0);

        // Managed attempt on an empty balance: typed Refused, nothing held.
        let (empty, empty_debits, empty_org) = unfunded();
        match empty_debits.begin(&attempt("45")) {
            Err(DebitError::Refused { reason }) => {
                assert!(reason.contains("insufficient credits"), "{reason}");
            }
            other => panic!("expected a typed Refused, got {other:?}"),
        }
        let balance = empty.credit_balance(&empty_org).unwrap();
        assert_eq!(balance.pending_consumes, 0);
    }
}
