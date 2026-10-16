//! The VALUE-centric paid entitlement vocabulary: the paid product
//! capabilities that are independent of inference volume.
//!
//! Wave 3 metering is inference-centric (active tasks, children, provider
//! attempts, token quotas, managed spend, credits). This module is the
//! orthogonal, value-centric half of the commercial surface: a customer who
//! runs 100% locally (Ollama, no managed providers, no token spend, no
//! provider attempts) is still a monetizable Faktor customer because these
//! entitlements gate product VALUE, not tokens:
//!
//! - **Plan-mapped, never metered.** Every entitlement is granted by the
//!   resolved plan's feature set (see [`PlanConfig::value_entitlements`])
//!   and nothing else. Derivation reads the durable subscription row plus
//!   the configured plan table only — it never folds usage, never reads
//!   credits and never counts tokens, so an organization with ZERO usage
//!   events resolves exactly the same entitlement picture as a heavy one;
//! - **Offline-capable default.** With no cloud/billing authority wired the
//!   gate is [`ValueGate::CloudAbsent`]: every value entitlement is denied
//!   typed (cause [`ValueRefusalCause::CloudAbsent`], naming the
//!   enforcement hook) while the local product itself (everything that is
//!   not a value entitlement) stays fully functional — this module adds no
//!   runtime requirement to local operation;
//! - **Typed refusals, exact causes.** [`ValueEntitlementSet::check`]
//!   refuses with [`ValueEntitlementRefused`] naming the entitlement, its
//!   enforcement hook and the exact cause (`cloud_absent`,
//!   `cloud_unavailable`, `plan_missing`, `subscription_inactive`,
//!   `not_in_plan`) — never a boolean nobody can diagnose;
//! - **Enforcement hooks.** [`ValueEntitlement::enforcement_hook`] names
//!   the ONE runtime seam each entitlement gates (verification policy,
//!   contract compiler, intelligence federation, cross-repo index, outcome
//!   ledger, proof attestation, audit history, outcome learning, worker
//!   placement, identity federation). The hooks are typed vocabulary: the
//!   cloud half owns the decision, the runtime half owns the feature;
//! - **Enforcement is a daemon/service-seam contract, not a library API
//!   guarantee.** The production daemon consults the gate at every hook
//!   BEFORE the paid side effect: the advanced verification-policy
//!   constructor, the organization-contract compiler consult, the federated
//!   index query (shared/cross-repo intelligence), the outcome-ledger read,
//!   the proof attestation entry, the extended audit-history read, the
//!   outcome-learning feed, worker placement (before any job generation is
//!   minted) and worker claim admission (the same `private_runners`
//!   entitlement, consumed by `WorkerPlane::claim_next`), and the SSO /
//!   identity-federation entry behind `enterprise_governance`. The ONE
//!   deliberate library-level exception: the `faktor-worker` library's
//!   `WorkerPlane::new` / `new_for_embedded_host` construction is UNGATED
//!   for embedded hosts and tests (`admit_claim` returns `Ok(())` while no
//!   gate is installed) — the daemon's `serve` wiring is what installs the
//!   gate (`set_claim_entitlement_gate`) on its ONE plane, and an embedded
//!   host that needs the commercial boundary must install the gate itself.
//!   Licensing therefore holds at the daemon/service seams (placement mint,
//!   claim admission and the other boundaries above); the library API
//!   itself promises no entitlement enforcement;
//! - **Governance is capability flags, not an implementation.** Enterprise
//!   governance exposes the `sso`/`scim` capability flags
//!   ([`GovernanceCapabilities`]) behind the ONE
//!   [`ValueEntitlement::EnterpriseGovernance`] entitlement. The flags are
//!   only effective when the base entitlement is granted; this crate
//!   implements neither protocol — it only says whether the plan is
//!   entitled to wire one.
//!
//! There is no durable row and no migration here: the picture is DERIVED
//! from the subscription + plan config, exactly like the metering snapshot.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::billing::PlanConfig;
use crate::entitlements::EntitlementService;
use crate::error::ControlPlaneError;
use crate::ids::OrganizationId;

// -------------------------------------------------------------- vocabulary

// The value-entitlement vocabulary is the SHARED runtime enforcement seam
// vocabulary (faktor-core): the plan-mapping half here and the runtime
// capability boundaries consult the same types, so a tag can never drift.
// The commercial decision stays HERE; faktor-core owns the typed refusal
// shape and the gate handle the seams hold.
pub use faktor_core::entitlement::{
    EnforcementHook, ValueEntitlement, ValueRefusalCause, FEATURE_ADVANCED_VERIFICATION_POLICY,
    FEATURE_AUDIT_RETENTION_HISTORY, FEATURE_CROSS_REPO_INTELLIGENCE,
    FEATURE_ENTERPRISE_GOVERNANCE, FEATURE_GOVERNANCE_SCIM, FEATURE_GOVERNANCE_SSO,
    FEATURE_MODEL_OPTIMIZATION_LEARNING, FEATURE_ORGANIZATION_CONTRACTS, FEATURE_OUTCOME_ANALYTICS,
    FEATURE_PRIVATE_RUNNERS, FEATURE_SHARED_INTELLIGENCE, FEATURE_SIGNED_PROOF_PROVENANCE,
    VALUE_FEATURE_TAGS,
};

/// One enterprise-governance capability flag. These are HOOKS as capability
/// flags, not implementations: the crate never speaks SAML/OIDC/SCIM; it
/// only reports whether the granted plan is entitled to wire one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernanceCapability {
    Sso,
    Scim,
}

impl GovernanceCapability {
    pub const ALL: [GovernanceCapability; 2] =
        [GovernanceCapability::Sso, GovernanceCapability::Scim];

    pub const fn as_str(self) -> &'static str {
        match self {
            GovernanceCapability::Sso => "sso",
            GovernanceCapability::Scim => "scim",
        }
    }

    /// The plan feature tag that enables the capability (only effective
    /// behind [`ValueEntitlement::EnterpriseGovernance`]).
    pub const fn feature_tag(self) -> &'static str {
        match self {
            GovernanceCapability::Sso => FEATURE_GOVERNANCE_SSO,
            GovernanceCapability::Scim => FEATURE_GOVERNANCE_SCIM,
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == raw)
    }
}

impl std::fmt::Display for GovernanceCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The enterprise-governance capability picture of a resolved plan. A flag
/// is only effective when the base entitlement is granted (documents on
/// [`PlanConfig::governance_capabilities`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GovernanceCapabilities {
    pub sso: bool,
    pub scim: bool,
}

impl GovernanceCapabilities {
    pub fn has(&self, capability: GovernanceCapability) -> bool {
        match capability {
            GovernanceCapability::Sso => self.sso,
            GovernanceCapability::Scim => self.scim,
        }
    }
}

/// The typed refusal of one value-entitlement check: ALWAYS names the
/// entitlement, its enforcement hook and the exact cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueEntitlementRefused {
    pub organization_id: OrganizationId,
    pub entitlement: ValueEntitlement,
    pub hook: EnforcementHook,
    pub cause: ValueRefusalCause,
    /// The resolved plan (`None` when none resolved).
    pub plan_id: Option<String>,
    /// Set when the refusal is about one governance capability.
    pub governance_capability: Option<GovernanceCapability>,
    pub now_ms: i64,
}

impl std::fmt::Display for ValueEntitlementRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value entitlement {} (hook {}) not granted for organization {}: {}",
            self.entitlement, self.hook, self.organization_id, self.cause
        )?;
        if let Some(capability) = self.governance_capability {
            write!(f, " (capability {capability})")?;
        }
        write!(f, " (plan {:?})", self.plan_id)
    }
}

impl std::error::Error for ValueEntitlementRefused {}

/// One per-entitlement decision, for reporting surfaces: the entitlement,
/// its hook, whether it is enforceable NOW and (when not) the exact cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueEntitlementDecision {
    pub entitlement: ValueEntitlement,
    pub hook: EnforcementHook,
    pub granted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<ValueRefusalCause>,
}

/// The derived value-entitlement picture of one organization: plan-mapped
/// entitlements plus their effective governance capability flags. Every
/// field is a pure function of the durable subscription row + the
/// configured plan table — usage, credits, tokens and provider attempts are
/// never read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueEntitlementSet {
    pub organization_id: OrganizationId,
    pub plan_id: Option<String>,
    /// False when no plan resolved (unentitled tenant / undefined plan).
    pub plan_found: bool,
    pub subscription_active: bool,
    /// Entitlements the resolved plan MAPS (the plan's value features).
    /// Enforceability additionally requires an active subscription; use
    /// [`Self::check`] for the gate decision.
    pub plan_entitlements: BTreeSet<ValueEntitlement>,
    pub governance: GovernanceCapabilities,
    pub now_ms: i64,
}

impl ValueEntitlementSet {
    /// Whether the entitlement is enforceable NOW (plan-mapped AND the
    /// subscription is active; plan resolution must hold).
    pub fn has(&self, entitlement: ValueEntitlement) -> bool {
        self.check(entitlement).is_ok()
    }

    /// Whether the resolved plan MAPS the entitlement (regardless of the
    /// subscription state).
    pub fn plan_grants(&self, entitlement: ValueEntitlement) -> bool {
        self.plan_entitlements.contains(&entitlement)
    }

    /// The typed refusal for this entitlement, or `None` when it is
    /// granted and currently enforceable.
    pub fn refusal(&self, entitlement: ValueEntitlement) -> Option<ValueEntitlementRefused> {
        if !self.plan_found {
            return Some(self.refused(entitlement, ValueRefusalCause::PlanMissing));
        }
        if !self.subscription_active {
            return Some(self.refused(entitlement, ValueRefusalCause::SubscriptionInactive));
        }
        if !self.plan_grants(entitlement) {
            return Some(self.refused(entitlement, ValueRefusalCause::NotInPlan));
        }
        None
    }

    /// The ONE value-entitlement check (the enforcement hook's decision).
    pub fn check(&self, entitlement: ValueEntitlement) -> Result<(), ValueEntitlementRefused> {
        match self.refusal(entitlement) {
            None => Ok(()),
            Some(refusal) => Err(refusal),
        }
    }

    /// The effective governance capability flag: the base
    /// [`ValueEntitlement::EnterpriseGovernance`] must be enforceable and
    /// the plan must carry the capability tag. The flag is vocabulary, not
    /// an implementation.
    pub fn governance_capability(&self, capability: GovernanceCapability) -> bool {
        self.has(ValueEntitlement::EnterpriseGovernance) && self.governance.has(capability)
    }

    /// Check one governance capability flag, typed: a missing flag refuses
    /// naming both the capability and the governance entitlement that gates
    /// it.
    pub fn check_governance_capability(
        &self,
        capability: GovernanceCapability,
    ) -> Result<(), ValueEntitlementRefused> {
        self.check(ValueEntitlement::EnterpriseGovernance)
            .and_then(|()| {
                if self.governance.has(capability) {
                    Ok(())
                } else {
                    Err(self.refused(
                        ValueEntitlement::EnterpriseGovernance,
                        ValueRefusalCause::NotInPlan,
                    ))
                }
            })
            .map_err(|mut refusal| {
                refusal.governance_capability = Some(capability);
                refusal
            })
    }

    /// One decision per value entitlement (stable tag order), for the
    /// reporting surfaces.
    pub fn decisions(&self) -> Vec<ValueEntitlementDecision> {
        ValueEntitlement::ALL
            .into_iter()
            .map(|entitlement| {
                let refusal = self.refusal(entitlement);
                ValueEntitlementDecision {
                    entitlement,
                    hook: entitlement.enforcement_hook(),
                    granted: refusal.is_none(),
                    cause: refusal.map(|r| r.cause),
                }
            })
            .collect()
    }

    fn refused(
        &self,
        entitlement: ValueEntitlement,
        cause: ValueRefusalCause,
    ) -> ValueEntitlementRefused {
        ValueEntitlementRefused {
            organization_id: self.organization_id.clone(),
            entitlement,
            hook: entitlement.enforcement_hook(),
            cause,
            plan_id: self.plan_id.clone(),
            governance_capability: None,
            now_ms: self.now_ms,
        }
    }
}

/// The offline-capable default gate: no cloud/billing authority is wired.
///
/// [`ValueGate::CloudAbsent`] denies every value entitlement typed (cause
/// [`ValueRefusalCause::CloudAbsent`]) WITHOUT making the local product
/// depend on a cloud: the local runtime, the provider adapters and every
/// non-value capability keep working exactly as before. The server surface
/// mirrors this as the typed 409 `billing_disabled`; embedders get the
/// vocabulary directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueGate {
    /// No cloud/billing authority is wired (the offline default).
    CloudAbsent { organization_id: OrganizationId },
    /// The authority resolved plan/subscription for the organization.
    Resolved(ValueEntitlementSet),
}

impl ValueGate {
    pub fn organization_id(&self) -> &OrganizationId {
        match self {
            ValueGate::CloudAbsent { organization_id } => organization_id,
            ValueGate::Resolved(set) => &set.organization_id,
        }
    }

    /// The resolved set (`None` for the offline default).
    pub fn resolved(&self) -> Option<&ValueEntitlementSet> {
        match self {
            ValueGate::CloudAbsent { .. } => None,
            ValueGate::Resolved(set) => Some(set),
        }
    }

    /// The offline default gate for one organization.
    pub fn cloud_absent(organization: &OrganizationId) -> Self {
        ValueGate::CloudAbsent {
            organization_id: organization.clone(),
        }
    }

    /// The ONE check of the gate: an absent cloud denies every entitlement
    /// typed; a resolved set applies the plan/subscription rules.
    pub fn check(&self, entitlement: ValueEntitlement) -> Result<(), ValueEntitlementRefused> {
        match self {
            ValueGate::CloudAbsent { organization_id } => Err(ValueEntitlementRefused {
                organization_id: organization_id.clone(),
                entitlement,
                hook: entitlement.enforcement_hook(),
                cause: ValueRefusalCause::CloudAbsent,
                plan_id: None,
                governance_capability: None,
                now_ms: 0,
            }),
            ValueGate::Resolved(set) => set.check(entitlement),
        }
    }

    /// One decision per value entitlement (all denied `cloud_absent` for the
    /// offline default).
    pub fn decisions(&self) -> Vec<ValueEntitlementDecision> {
        match self {
            ValueGate::CloudAbsent { .. } => ValueEntitlement::ALL
                .into_iter()
                .map(|entitlement| ValueEntitlementDecision {
                    entitlement,
                    hook: entitlement.enforcement_hook(),
                    granted: false,
                    cause: Some(ValueRefusalCause::CloudAbsent),
                })
                .collect(),
            ValueGate::Resolved(set) => set.decisions(),
        }
    }
}

// ------------------------------------------------------------ plan mapping

impl PlanConfig {
    /// The value entitlements this plan maps, in stable tag order. Purely
    /// feature-driven: an empty feature set maps nothing (a plan may be
    /// inference-billing-only).
    pub fn value_entitlements(&self) -> BTreeSet<ValueEntitlement> {
        ValueEntitlement::ALL
            .into_iter()
            .filter(|entitlement| self.features.contains(entitlement.feature_tag()))
            .collect()
    }

    /// The governance capability flags this plan maps. Each flag is only
    /// effective behind [`ValueEntitlement::EnterpriseGovernance`]: a
    /// capability tag without the base entitlement resolves `false`
    /// (documented, never a half-granted governance surface).
    pub fn governance_capabilities(&self) -> GovernanceCapabilities {
        let base = self.features.contains(FEATURE_ENTERPRISE_GOVERNANCE);
        GovernanceCapabilities {
            sso: base && self.features.contains(FEATURE_GOVERNANCE_SSO),
            scim: base && self.features.contains(FEATURE_GOVERNANCE_SCIM),
        }
    }
}

// ------------------------------------------------------------ service gate

impl EntitlementService {
    /// Derive one organization's value-entitlement set. Plan resolution is
    /// EXACTLY the metering snapshot's rule (the subscription's plan wins;
    /// else the configured default plan, but ONLY for the configured billing
    /// organization), yet this path deliberately reads NO usage, NO credits
    /// and NO provider attempts: a zero-inference local deployment resolves
    /// the same value picture as a metered one.
    pub fn value_entitlements(
        &self,
        organization: &OrganizationId,
    ) -> Result<ValueEntitlementSet, ControlPlaneError> {
        let subscription = self.store().subscription(organization)?;
        let default_plan = if self.billing_organization() == Some(organization) {
            self.config().default_plan.clone()
        } else {
            None
        };
        let plan_id = subscription
            .as_ref()
            .map(|s| s.plan_id.clone())
            .or(default_plan);
        let plan = plan_id.as_deref().and_then(|id| self.config().plan(id));
        let now = self.now_ms();
        Ok(ValueEntitlementSet {
            organization_id: organization.clone(),
            plan_found: plan.is_some(),
            subscription_active: subscription
                .as_ref()
                .map(|s| s.is_active_at(now))
                .unwrap_or(false),
            plan_entitlements: plan.map(PlanConfig::value_entitlements).unwrap_or_default(),
            governance: plan
                .map(PlanConfig::governance_capabilities)
                .unwrap_or_default(),
            plan_id,
            now_ms: now,
        })
    }

    /// The ONE value-entitlement gate: consult it at a value-feature
    /// boundary. Every refusal is typed; an unreadable cloud authority
    /// fails closed (`cloud_unavailable`), never an accidental grant.
    pub fn check_value_entitlement(
        &self,
        organization: &OrganizationId,
        entitlement: ValueEntitlement,
    ) -> Result<(), ValueEntitlementRefused> {
        match self.value_entitlements(organization) {
            Ok(set) => set.check(entitlement),
            Err(_) => Err(ValueEntitlementRefused {
                organization_id: organization.clone(),
                entitlement,
                hook: entitlement.enforcement_hook(),
                cause: ValueRefusalCause::CloudUnavailable,
                plan_id: None,
                governance_capability: None,
                now_ms: self.now_ms(),
            }),
        }
    }
}

// ------------------------------------------------------- enforcement gate

/// The refusal conversion into the runtime seam vocabulary: the seam keeps
/// the same entitlement/hook/cause/plan identity and the diagnosable
/// message (including the governance capability when one was named).
impl From<ValueEntitlementRefused> for faktor_core::entitlement::ValueGateRefusal {
    fn from(refused: ValueEntitlementRefused) -> Self {
        faktor_core::entitlement::ValueGateRefusal {
            entitlement: refused.entitlement,
            hook: refused.hook,
            cause: refused.cause,
            plan_id: refused.plan_id.clone(),
            organization_id: Some(refused.organization_id.to_string()),
            message: refused.to_string(),
        }
    }
}

/// The per-organization runtime enforcement gate over the entitlement
/// service: THE adapter the daemon and embedded hosts wire into runtime
/// seams (verification policy, contract enrichment, federated index,
/// worker placement, ...). Every decision reads the durable subscription +
/// the configured plan table only — never usage, credits or tokens.
#[derive(Debug)]
pub struct OrganizationValueGate {
    service: std::sync::Arc<EntitlementService>,
    organization: OrganizationId,
}

impl faktor_core::entitlement::ValueEntitlementGate for OrganizationValueGate {
    fn check_value_entitlement(
        &self,
        entitlement: ValueEntitlement,
    ) -> Result<(), faktor_core::entitlement::ValueGateRefusal> {
        self.service
            .check_value_entitlement(&self.organization, entitlement)
            .map_err(Into::into)
    }
}

impl EntitlementService {
    /// The runtime enforcement gate for one organization: a wired
    /// [`faktor_core::entitlement::ValueGate`] over this service. Wire it
    /// into a seam to enforce the org's plan at the capability boundary; a
    /// seam with no gate refuses every paid augmentation typed
    /// (`cloud_absent`) while the local product path keeps working.
    pub fn enforcement_gate(
        self: &std::sync::Arc<Self>,
        organization: &OrganizationId,
    ) -> faktor_core::entitlement::ValueGate {
        faktor_core::entitlement::ValueGate::wired(std::sync::Arc::new(OrganizationValueGate {
            service: self.clone(),
            organization: organization.clone(),
        }))
    }

    /// Check one governance capability flag (SSO/SCIM) for an organization,
    /// typed: the base [`ValueEntitlement::EnterpriseGovernance`] must be
    /// enforceable AND the plan must carry the capability tag. An unreadable
    /// authority fails closed (`cloud_unavailable`).
    pub fn check_governance_capability(
        &self,
        organization: &OrganizationId,
        capability: GovernanceCapability,
    ) -> Result<(), ValueEntitlementRefused> {
        match self.value_entitlements(organization) {
            Ok(set) => set.check_governance_capability(capability),
            Err(_) => Err(ValueEntitlementRefused {
                organization_id: organization.clone(),
                entitlement: ValueEntitlement::EnterpriseGovernance,
                hook: ValueEntitlement::EnterpriseGovernance.enforcement_hook(),
                cause: ValueRefusalCause::CloudUnavailable,
                plan_id: None,
                governance_capability: Some(capability),
                now_ms: self.now_ms(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn plan(features: &[&str]) -> PlanConfig {
        PlanConfig {
            plan_id: "p".into(),
            features: features.iter().map(|f| f.to_string()).collect(),
            limits: BTreeMap::new(),
        }
    }

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
            assert!(
                VALUE_FEATURE_TAGS.contains(&entitlement.feature_tag()),
                "{entitlement} must be a legal plan feature"
            );
            assert!(
                crate::billing::ALL_FEATURES.contains(&entitlement.feature_tag()),
                "{entitlement} must be accepted by plan validation"
            );
        }
        for capability in GovernanceCapability::ALL {
            assert_eq!(
                GovernanceCapability::parse(capability.as_str()),
                Some(capability)
            );
            assert!(VALUE_FEATURE_TAGS.contains(&capability.feature_tag()));
            assert!(crate::billing::ALL_FEATURES.contains(&capability.feature_tag()));
        }
        for cause in ValueRefusalCause::ALL {
            assert_eq!(ValueRefusalCause::parse(cause.as_str()), Some(cause));
        }
        assert_eq!(ValueEntitlement::parse("tokens"), None);
        assert_eq!(ValueEntitlement::parse(""), None);
        assert_eq!(EnforcementHook::parse("nope"), None);
        assert_eq!(GovernanceCapability::parse("saml"), None);
        assert_eq!(ValueRefusalCause::parse("cheap"), None);
        // The hooks are ONE-to-ONE with the entitlements (no shared seam).
        let hooks: BTreeSet<&str> = ValueEntitlement::ALL
            .into_iter()
            .map(|e| e.hook_tag())
            .collect();
        assert_eq!(hooks.len(), ValueEntitlement::ALL.len());
    }

    #[test]
    fn plan_mapping_is_feature_driven_and_governance_flags_need_the_base() {
        let pro = plan(&[FEATURE_SHARED_INTELLIGENCE, FEATURE_GOVERNANCE_SSO]);
        let mapped = pro.value_entitlements();
        assert!(mapped.contains(&ValueEntitlement::SharedIntelligence));
        assert_eq!(mapped.len(), 1, "only mapped entitlements");
        assert_eq!(
            pro.governance_capabilities(),
            GovernanceCapabilities::default(),
            "a capability tag without the base entitlement is never effective"
        );
        let enterprise = plan(&[
            FEATURE_ENTERPRISE_GOVERNANCE,
            FEATURE_GOVERNANCE_SSO,
            FEATURE_GOVERNANCE_SCIM,
        ]);
        let capabilities = enterprise.governance_capabilities();
        assert!(capabilities.sso && capabilities.scim);
        let sso_only = plan(&[FEATURE_ENTERPRISE_GOVERNANCE, FEATURE_GOVERNANCE_SSO]);
        assert!(sso_only.governance_capabilities().sso);
        assert!(!sso_only.governance_capabilities().scim);
        assert_eq!(plan(&[]).value_entitlements().len(), 0);
    }

    #[test]
    fn cloud_absent_gate_denies_everything_typed_with_hooks() {
        let org = OrganizationId::try_new("org_local").unwrap();
        let gate = ValueGate::cloud_absent(&org);
        assert_eq!(gate.organization_id(), &org);
        assert!(gate.resolved().is_none());
        for entitlement in ValueEntitlement::ALL {
            let refusal = gate.check(entitlement).unwrap_err();
            assert_eq!(refusal.cause, ValueRefusalCause::CloudAbsent);
            assert_eq!(refusal.entitlement, entitlement);
            assert_eq!(refusal.hook, entitlement.enforcement_hook());
            assert_eq!(refusal.organization_id, org);
            let message = refusal.to_string();
            assert!(message.contains(entitlement.as_str()), "{message}");
            assert!(message.contains(entitlement.hook_tag()), "{message}");
            assert!(message.contains("cloud_absent"), "{message}");
        }
        let decisions = gate.decisions();
        assert_eq!(decisions.len(), ValueEntitlement::ALL.len());
        assert!(decisions.iter().all(|d| !d.granted));
        assert!(decisions
            .iter()
            .all(|d| d.cause == Some(ValueRefusalCause::CloudAbsent)));
    }
}
