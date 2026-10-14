//! The runtime-side value-entitlement enforcement seam.
//!
//! The commercial half (faktor-cloud) resolves an organization's plan into a
//! typed entitlement picture. This module is the OTHER half: the typed
//! vocabulary and the ONE gate handle the runtime seams hold at their
//! capability boundaries, so a paid augmentation can no longer be reached by
//! calling the underlying capability directly — the seam consults
//! [`ValueGate`] first and a denial is a typed [`ValueGateRefusal`] naming
//! the entitlement, its enforcement hook and (when resolved) the plan.
//!
//! Laws:
//!
//! - **Absent cloud = typed refusal, never a silent grant.** A seam with no
//!   wired gate is [`ValueGate::absent`]: every paid augmentation is refused
//!   typed (`cloud_absent`) while the local product itself keeps working
//!   exactly as before (gates are consulted only on augmentation paths).
//! - **The decision is cloud-owned.** The trait [`ValueEntitlementGate`] is
//!   implemented by the commercial layer (faktor-cloud's entitlement
//!   service); this crate owns only the vocabulary and the refusal shape.
//! - **Typed refusals name the entitlement and plan.** [`ValueGateRefusal`]
//!   is a struct, never a boolean or an empty result.
//!
//! The vocabulary here mirrors the cloud plan tags one-to-one (`as_str` is
//! the stable wire tag); faktor-cloud re-exports these types so the plan
//! table, the report surface and the runtime seams can never drift.

use std::sync::Arc;

// ------------------------------------------------------------- feature tags

/// Plan feature tags: one per value entitlement. They are the ONLY
/// plan-mapping keys; the plan table is the only place a grant comes from.
pub const FEATURE_ADVANCED_VERIFICATION_POLICY: &str = "advanced_verification_policy";
pub const FEATURE_ORGANIZATION_CONTRACTS: &str = "organization_contracts";
pub const FEATURE_SHARED_INTELLIGENCE: &str = "shared_intelligence";
pub const FEATURE_CROSS_REPO_INTELLIGENCE: &str = "cross_repo_intelligence";
pub const FEATURE_OUTCOME_ANALYTICS: &str = "outcome_analytics";
pub const FEATURE_SIGNED_PROOF_PROVENANCE: &str = "signed_proof_provenance";
pub const FEATURE_AUDIT_RETENTION_HISTORY: &str = "audit_retention_history";
pub const FEATURE_MODEL_OPTIMIZATION_LEARNING: &str = "model_optimization_learning";
pub const FEATURE_PRIVATE_RUNNERS: &str = "private_runners";
pub const FEATURE_ENTERPRISE_GOVERNANCE: &str = "enterprise_governance";
pub const FEATURE_GOVERNANCE_SSO: &str = "governance_sso";
pub const FEATURE_GOVERNANCE_SCIM: &str = "governance_scim";

/// Every legal value-centric plan feature tag (the 10 entitlements plus the
/// 2 governance capability flags). The commercial plan validation folds
/// these in, so an unknown tag stays a strict config error.
pub const VALUE_FEATURE_TAGS: &[&str] = &[
    FEATURE_ADVANCED_VERIFICATION_POLICY,
    FEATURE_ORGANIZATION_CONTRACTS,
    FEATURE_SHARED_INTELLIGENCE,
    FEATURE_CROSS_REPO_INTELLIGENCE,
    FEATURE_OUTCOME_ANALYTICS,
    FEATURE_SIGNED_PROOF_PROVENANCE,
    FEATURE_AUDIT_RETENTION_HISTORY,
    FEATURE_MODEL_OPTIMIZATION_LEARNING,
    FEATURE_PRIVATE_RUNNERS,
    FEATURE_ENTERPRISE_GOVERNANCE,
    FEATURE_GOVERNANCE_SSO,
    FEATURE_GOVERNANCE_SCIM,
];

// -------------------------------------------------------------- vocabulary

/// The typed value-centric paid entitlements. Each one is plan-mapped and
/// inference-independent: it gates product value, never token volume.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ValueEntitlement {
    /// Advanced verification policy (verification depth floors, claim-bound
    /// proof obligations and quality gates beyond the local default).
    AdvancedVerificationPolicy,
    /// Organization contracts: org-authored executable contracts compiled
    /// into the verification spine.
    OrganizationContracts,
    /// Shared intelligence: organization-scoped federated context/insights.
    SharedIntelligence,
    /// Cross-repo intelligence: indexing/search knowledge spanning
    /// repositories of the organization.
    CrossRepoIntelligence,
    /// Outcome analytics: the engineering outcome ledger reads/aggregates.
    OutcomeAnalytics,
    /// Signed proof/provenance: externally verifiable attestations of
    /// finished work.
    SignedProofProvenance,
    /// Audit retention history: durable long-horizon audit trail reads.
    AuditRetentionHistory,
    /// Model-optimization learning: shared outcome learning that tunes
    /// routing/model choice from organization outcomes.
    ModelOptimizationLearning,
    /// Private/self-hosted runners: organization-owned execution workers.
    PrivateRunners,
    /// Enterprise governance: the ONE entitlement behind the SSO/SCIM
    /// capability flags (flags only — no protocol implementation here).
    EnterpriseGovernance,
}

impl ValueEntitlement {
    /// Every value entitlement, in stable tag order.
    pub const ALL: [ValueEntitlement; 10] = [
        ValueEntitlement::AdvancedVerificationPolicy,
        ValueEntitlement::OrganizationContracts,
        ValueEntitlement::SharedIntelligence,
        ValueEntitlement::CrossRepoIntelligence,
        ValueEntitlement::OutcomeAnalytics,
        ValueEntitlement::SignedProofProvenance,
        ValueEntitlement::AuditRetentionHistory,
        ValueEntitlement::ModelOptimizationLearning,
        ValueEntitlement::PrivateRunners,
        ValueEntitlement::EnterpriseGovernance,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ValueEntitlement::AdvancedVerificationPolicy => FEATURE_ADVANCED_VERIFICATION_POLICY,
            ValueEntitlement::OrganizationContracts => FEATURE_ORGANIZATION_CONTRACTS,
            ValueEntitlement::SharedIntelligence => FEATURE_SHARED_INTELLIGENCE,
            ValueEntitlement::CrossRepoIntelligence => FEATURE_CROSS_REPO_INTELLIGENCE,
            ValueEntitlement::OutcomeAnalytics => FEATURE_OUTCOME_ANALYTICS,
            ValueEntitlement::SignedProofProvenance => FEATURE_SIGNED_PROOF_PROVENANCE,
            ValueEntitlement::AuditRetentionHistory => FEATURE_AUDIT_RETENTION_HISTORY,
            ValueEntitlement::ModelOptimizationLearning => FEATURE_MODEL_OPTIMIZATION_LEARNING,
            ValueEntitlement::PrivateRunners => FEATURE_PRIVATE_RUNNERS,
            ValueEntitlement::EnterpriseGovernance => FEATURE_ENTERPRISE_GOVERNANCE,
        }
    }

    /// The plan feature tag that grants this entitlement (identical to
    /// [`Self::as_str`] by construction).
    pub const fn feature_tag(self) -> &'static str {
        self.as_str()
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == raw)
    }

    /// The ONE runtime enforcement hook this entitlement gates. The cloud
    /// half decides; the named runtime seam enforces.
    pub const fn enforcement_hook(self) -> EnforcementHook {
        match self {
            ValueEntitlement::AdvancedVerificationPolicy => EnforcementHook::VerificationPolicy,
            ValueEntitlement::OrganizationContracts => EnforcementHook::ContractCompiler,
            ValueEntitlement::SharedIntelligence => EnforcementHook::IntelligenceFederation,
            ValueEntitlement::CrossRepoIntelligence => EnforcementHook::CrossRepoIndex,
            ValueEntitlement::OutcomeAnalytics => EnforcementHook::OutcomeLedger,
            ValueEntitlement::SignedProofProvenance => EnforcementHook::ProofAttestation,
            ValueEntitlement::AuditRetentionHistory => EnforcementHook::AuditHistory,
            ValueEntitlement::ModelOptimizationLearning => EnforcementHook::OutcomeLearning,
            ValueEntitlement::PrivateRunners => EnforcementHook::WorkerPlacement,
            ValueEntitlement::EnterpriseGovernance => EnforcementHook::IdentityFederation,
        }
    }

    /// The stable tag of [`Self::enforcement_hook`].
    pub const fn hook_tag(self) -> &'static str {
        self.enforcement_hook().as_str()
    }
}

impl std::fmt::Display for ValueEntitlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The typed runtime seam one value entitlement gates (see
/// [`ValueEntitlement::enforcement_hook`]).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementHook {
    /// The verification-policy seam (faktor-verify).
    VerificationPolicy,
    /// The contract-compiler seam (faktor-core contract compile).
    ContractCompiler,
    /// The shared-intelligence seam.
    IntelligenceFederation,
    /// The cross-repository index seam.
    CrossRepoIndex,
    /// The engineering outcome ledger seam.
    OutcomeLedger,
    /// The signed proof/provenance attestation seam.
    ProofAttestation,
    /// The audit retention history seam.
    AuditHistory,
    /// The model-optimization / shared outcome learning seam.
    OutcomeLearning,
    /// The worker placement seam (private/self-hosted runners).
    WorkerPlacement,
    /// The identity federation seam (enterprise governance SSO/SCIM).
    IdentityFederation,
}

impl EnforcementHook {
    pub const fn as_str(self) -> &'static str {
        match self {
            EnforcementHook::VerificationPolicy => "verification_policy",
            EnforcementHook::ContractCompiler => "contract_compiler",
            EnforcementHook::IntelligenceFederation => "intelligence_federation",
            EnforcementHook::CrossRepoIndex => "cross_repo_index",
            EnforcementHook::OutcomeLedger => "outcome_ledger",
            EnforcementHook::ProofAttestation => "proof_attestation",
            EnforcementHook::AuditHistory => "audit_history",
            EnforcementHook::OutcomeLearning => "outcome_learning",
            EnforcementHook::WorkerPlacement => "worker_placement",
            EnforcementHook::IdentityFederation => "identity_federation",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        ValueEntitlement::ALL
            .into_iter()
            .map(|e| e.enforcement_hook())
            .find(|hook| hook.as_str() == raw)
    }
}

impl std::fmt::Display for EnforcementHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The typed cause of a value-entitlement refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueRefusalCause {
    /// No cloud/billing authority is wired at all (the offline default):
    /// every value entitlement is denied while the local product stays
    /// fully functional.
    CloudAbsent,
    /// A cloud authority exists but its plan/subscription read failed; the
    /// gate fails closed (never grants on an unreadable authority).
    CloudUnavailable,
    /// No plan resolved for the organization (unprovisioned tenant, or a
    /// named-but-undefined plan): value grants are never guessed.
    PlanMissing,
    /// The plan is resolved but its subscription is expired/canceled.
    SubscriptionInactive,
    /// The plan is resolved and active but does not map this entitlement.
    NotInPlan,
}

impl ValueRefusalCause {
    pub const ALL: [ValueRefusalCause; 5] = [
        ValueRefusalCause::CloudAbsent,
        ValueRefusalCause::CloudUnavailable,
        ValueRefusalCause::PlanMissing,
        ValueRefusalCause::SubscriptionInactive,
        ValueRefusalCause::NotInPlan,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ValueRefusalCause::CloudAbsent => "cloud_absent",
            ValueRefusalCause::CloudUnavailable => "cloud_unavailable",
            ValueRefusalCause::PlanMissing => "plan_missing",
            ValueRefusalCause::SubscriptionInactive => "subscription_inactive",
            ValueRefusalCause::NotInPlan => "not_in_plan",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == raw)
    }
}

impl std::fmt::Display for ValueRefusalCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ------------------------------------------------------------------- gate

/// The typed refusal of ONE runtime enforcement check: ALWAYS names the
/// entitlement, its hook and the exact cause, and carries the resolved plan
/// id when one resolved. The `message` is the diagnosable rendering the
/// commercial layer produced (it may name the governance capability).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueGateRefusal {
    pub entitlement: ValueEntitlement,
    pub hook: EnforcementHook,
    pub cause: ValueRefusalCause,
    /// The resolved plan (`None` when none resolved / cloud absent).
    pub plan_id: Option<String>,
    /// The organization the decision was made for, when known.
    pub organization_id: Option<String>,
    pub message: String,
}

impl std::fmt::Display for ValueGateRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValueGateRefusal {}

impl ValueGateRefusal {
    /// The refusal of an absent cloud authority for one entitlement.
    pub fn cloud_absent(entitlement: ValueEntitlement) -> Self {
        Self {
            entitlement,
            hook: entitlement.enforcement_hook(),
            cause: ValueRefusalCause::CloudAbsent,
            plan_id: None,
            organization_id: None,
            message: format!(
                "value entitlement {} (hook {}) not granted: no cloud/billing authority is wired (cloud_absent)",
                entitlement.as_str(),
                entitlement.hook_tag()
            ),
        }
    }
}

/// The ONE runtime enforcement authority the seams consult. Implementations
/// are thread-safe; a read failure must refuse typed, never grant.
pub trait ValueEntitlementGate: Send + Sync + std::fmt::Debug {
    fn check_value_entitlement(
        &self,
        entitlement: ValueEntitlement,
    ) -> Result<(), ValueGateRefusal>;
}

/// A cloneable gate handle held by a runtime seam. `absent` is the offline
/// default: every check refuses typed (`cloud_absent`) while the local
/// product path (which never consults the gate) keeps working.
#[derive(Clone, Default)]
pub struct ValueGate {
    inner: Option<Arc<dyn ValueEntitlementGate>>,
}

impl std::fmt::Debug for ValueGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValueGate")
            .field("wired", &self.is_wired())
            .finish()
    }
}

impl ValueGate {
    /// No cloud/billing authority: every check refuses `cloud_absent`.
    pub const fn absent() -> Self {
        Self { inner: None }
    }

    /// The gate over the commercial enforcement authority.
    pub fn wired(gate: Arc<dyn ValueEntitlementGate>) -> Self {
        Self { inner: Some(gate) }
    }

    pub fn is_wired(&self) -> bool {
        self.inner.is_some()
    }

    /// The ONE enforcement check: wired gates get the commercial decision;
    /// an absent gate refuses typed (never a silent grant).
    pub fn check(&self, entitlement: ValueEntitlement) -> Result<(), ValueGateRefusal> {
        match &self.inner {
            Some(gate) => gate.check_value_entitlement(entitlement),
            None => Err(ValueGateRefusal::cloud_absent(entitlement)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn vocabulary_roundtrips_and_unknown_tags_are_refused() {
        for entitlement in ValueEntitlement::ALL {
            assert_eq!(entitlement.to_string(), entitlement.as_str());
            assert_eq!(
                ValueEntitlement::parse(entitlement.as_str()),
                Some(entitlement)
            );
            assert_eq!(entitlement.feature_tag(), entitlement.as_str());
            assert_eq!(
                entitlement.hook_tag(),
                entitlement.enforcement_hook().as_str()
            );
            assert_eq!(
                EnforcementHook::parse(entitlement.hook_tag()),
                Some(entitlement.enforcement_hook())
            );
            assert!(VALUE_FEATURE_TAGS.contains(&entitlement.feature_tag()));
        }
        for cause in ValueRefusalCause::ALL {
            assert_eq!(ValueRefusalCause::parse(cause.as_str()), Some(cause));
        }
        assert_eq!(ValueEntitlement::parse("tokens"), None);
        assert_eq!(ValueEntitlement::parse(""), None);
        assert_eq!(EnforcementHook::parse("nope"), None);
        assert_eq!(ValueRefusalCause::parse("cheap"), None);
        // The hooks are ONE-to-ONE with the entitlements (no shared seam).
        let hooks: BTreeSet<&str> = ValueEntitlement::ALL
            .into_iter()
            .map(|e| e.hook_tag())
            .collect();
        assert_eq!(hooks.len(), ValueEntitlement::ALL.len());
    }

    #[test]
    fn absent_gate_refuses_every_entitlement_typed_with_the_hook() {
        let gate = ValueGate::absent();
        assert!(!gate.is_wired());
        for entitlement in ValueEntitlement::ALL {
            let refusal = gate.check(entitlement).unwrap_err();
            assert_eq!(refusal.cause, ValueRefusalCause::CloudAbsent);
            assert_eq!(refusal.entitlement, entitlement);
            assert_eq!(refusal.hook, entitlement.enforcement_hook());
            assert_eq!(refusal.plan_id, None);
            let message = refusal.to_string();
            assert!(message.contains(entitlement.as_str()), "{message}");
            assert!(message.contains(entitlement.hook_tag()), "{message}");
            assert!(message.contains("cloud_absent"), "{message}");
        }
        assert!(!ValueGate::default().is_wired());
    }

    #[test]
    fn wired_gate_answers_the_commercial_decision_and_carries_the_plan() {
        #[derive(Debug)]
        struct DenyOutcomeAnalytics;

        impl ValueEntitlementGate for DenyOutcomeAnalytics {
            fn check_value_entitlement(
                &self,
                entitlement: ValueEntitlement,
            ) -> Result<(), ValueGateRefusal> {
                if entitlement == ValueEntitlement::OutcomeAnalytics {
                    return Err(ValueGateRefusal {
                        entitlement,
                        hook: entitlement.enforcement_hook(),
                        cause: ValueRefusalCause::NotInPlan,
                        plan_id: Some("local".into()),
                        organization_id: Some("org_acme".into()),
                        message: "value entitlement outcome_analytics (hook outcome_ledger) not granted for organization org_acme: not_in_plan (plan Some(\"local\"))".into(),
                    });
                }
                Ok(())
            }
        }

        let gate = ValueGate::wired(Arc::new(DenyOutcomeAnalytics));
        assert!(gate.is_wired());
        assert!(gate
            .check(ValueEntitlement::AdvancedVerificationPolicy)
            .is_ok());
        let refusal = gate.check(ValueEntitlement::OutcomeAnalytics).unwrap_err();
        assert_eq!(refusal.cause, ValueRefusalCause::NotInPlan);
        assert_eq!(refusal.plan_id.as_deref(), Some("local"));
        assert_eq!(refusal.organization_id.as_deref(), Some("org_acme"));
        assert!(refusal.to_string().contains("not_in_plan"));
        assert!(refusal.to_string().contains("local"));
    }
}
