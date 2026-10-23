//! Adversarial tests of the value-centric paid entitlement vocabulary: plan
//! tier fixtures, inference-independence (a zero-inference local/Ollama
//! deployment stays entitled), typed refusals naming the enforcement hook,
//! governance capability flags, and the offline (cloud-absent) default.

use std::sync::Arc;

use faktor_cloud::{
    BillingAccountId, BillingConfig, EntitlementService, GovernanceCapabilities,
    GovernanceCapability, ManualClock, MemoryBillingStore, OrganizationId, Subscription,
    SubscriptionId, SubscriptionStatus, UsageEvent, UsageEventId, UsageUnit, ValueEntitlement,
    ValueEntitlementSet, ValueGate, ValueRefusalCause, FEATURE_ENTERPRISE_GOVERNANCE,
    FEATURE_GOVERNANCE_SCIM, FEATURE_GOVERNANCE_SSO, FEATURE_SHARED_INTELLIGENCE,
    FEATURE_SIGNED_PROOF_PROVENANCE,
};

const NOW_MS: i64 = 1_700_000_000_000;

/// The shared value-only plan-tier fixture: `managed_providers` is empty and
/// no plan carries a limit, so the fixture itself proves that value
/// entitlements are inference-independent.
fn tier_fixture() -> BillingConfig {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/entitlements/value-plan-tiers.json"
    );
    let raw = std::fs::read_to_string(path).expect("the value plan tier fixture exists");
    let config: BillingConfig =
        serde_json::from_str(&raw).expect("the fixture is a strict BillingConfig");
    config.validate().expect("the fixture plan table is valid");
    config
}

fn org(id: &str) -> OrganizationId {
    OrganizationId::try_new(id).unwrap()
}

fn service(
    config: BillingConfig,
    configured_org: Option<&OrganizationId>,
) -> Arc<EntitlementService> {
    EntitlementService::new_for_organization(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
        config,
        configured_org.cloned(),
    )
    .unwrap()
}

fn subscribe(
    service: &EntitlementService,
    organization: &OrganizationId,
    plan: &str,
    expires_ms: Option<i64>,
) {
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_tier").unwrap(),
            organization: organization.clone(),
            plan_id: plan.into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms,
            updated_ms: NOW_MS,
        })
        .unwrap();
}

fn tags(set: &ValueEntitlementSet) -> Vec<&'static str> {
    set.plan_entitlements.iter().map(|e| e.as_str()).collect()
}

/// The fixture maps the tiers exactly and monotonically: every higher tier
/// is a strict superset of the lower one, and governance capability flags
/// appear only on the enterprise tier.
#[test]
fn value_plan_tier_fixture_maps_tiers_monotonically() {
    let config = tier_fixture();
    assert_eq!(config.default_plan.as_deref(), Some("local"));
    assert!(config.managed_providers.is_empty(), "inference-independent");
    for (id, plan) in &config.plans {
        assert!(
            plan.limits.is_empty(),
            "plan {id} carries no inference limit"
        );
    }

    let local = config.plan("local").unwrap().value_entitlements();
    let pro = config.plan("pro").unwrap().value_entitlements();
    let team = config.plan("team").unwrap().value_entitlements();
    let enterprise = config.plan("enterprise").unwrap().value_entitlements();

    assert_eq!(
        local.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
        vec![
            "advanced_verification_policy",
            "organization_contracts",
            "signed_proof_provenance"
        ]
    );
    for (lower, higher) in [(&local, &pro), (&pro, &team), (&team, &enterprise)] {
        assert!(
            lower.is_subset(higher),
            "tiers are monotone: {lower:?} must be a subset of {higher:?}"
        );
        assert!(lower.len() < higher.len(), "each tier adds value");
    }
    assert_eq!(
        enterprise.len(),
        ValueEntitlement::ALL.len(),
        "the enterprise tier maps every value entitlement"
    );
    assert_eq!(
        config.plan("local").unwrap().governance_capabilities(),
        GovernanceCapabilities::default()
    );
    let enterprise_governance = config.plan("enterprise").unwrap().governance_capabilities();
    assert!(enterprise_governance.sso && enterprise_governance.scim);
}

/// A 100% local deployment (no managed providers, no usage events, no
/// credits, no provider attempts) still exposes and enforces every value
/// entitlement its plan maps — and the metering snapshot stays honestly
/// empty.
#[test]
fn zero_inference_local_deployment_exposes_and_enforces_value_entitlements() {
    let organization = org("org_local");
    let service = service(tier_fixture(), Some(&organization));
    subscribe(&service, &organization, "pro", None);

    let set = service.value_entitlements(&organization).unwrap();
    assert_eq!(set.plan_id.as_deref(), Some("pro"));
    assert!(set.plan_found);
    assert!(set.subscription_active);
    assert_eq!(
        tags(&set),
        vec![
            "advanced_verification_policy",
            "organization_contracts",
            "shared_intelligence",
            "outcome_analytics",
            "signed_proof_provenance",
            "audit_retention_history",
            "model_optimization_learning"
        ]
    );
    // Every mapped entitlement passes the ONE gate; it names the hook.
    for entitlement in &set.plan_entitlements {
        assert!(set.check(*entitlement).is_ok(), "{entitlement}");
    }
    // A not-in-plan entitlement refuses TYPED, naming entitlement+hook+cause.
    let refusal = set
        .check(ValueEntitlement::PrivateRunners)
        .expect_err("the pro tier does not map private runners");
    assert_eq!(refusal.cause, ValueRefusalCause::NotInPlan);
    assert_eq!(refusal.hook.as_str(), "worker_placement");
    assert_eq!(refusal.plan_id.as_deref(), Some("pro"));
    let message = refusal.to_string();
    assert!(message.contains("private_runners"), "{message}");
    assert!(message.contains("worker_placement"), "{message}");
    assert!(message.contains("not_in_plan"), "{message}");

    // The metering half is untouched and honestly zero: no usage events, no
    // managed spend, no tokens — value entitlements never depend on it.
    let snapshot = service.entitlement_snapshot(&organization).unwrap();
    assert_eq!(snapshot.total_tokens, 0);
    assert_eq!(snapshot.managed_spend_micro, 0);
    assert_eq!(snapshot.byok_spend_micro, 0);
    assert!(snapshot.credits.pending_consumes == 0);
    // The value gate itself reads NOTHING from usage: even with an
    // unroutable plan config (no limits at all) it admits a new task.
    assert!(service
        .check_admission(
            &organization,
            &faktor_cloud::AdmissionRequest::boundary(faktor_cloud::AdmissionBoundary::NewTask),
        )
        .is_ok());
}

/// Adversarial: the value derivation NEVER folds the usage ledger. A planted
/// overflow (three `i64::MAX` managed cost events) makes the metering
/// snapshot refuse typed (`ledger_overflow`, fail closed) while the value
/// picture still resolves and grants — the two halves are independent.
#[test]
fn value_grants_are_independent_of_an_overflowing_usage_ledger() {
    let organization = org("org_local");
    let service = service(tier_fixture(), Some(&organization));
    subscribe(&service, &organization, "enterprise", None);
    let account = BillingAccountId::try_new("acct_local").unwrap();
    service
        .ensure_account(&organization, &account, "local", false)
        .unwrap();
    let max = i64::MAX as u64;
    for index in 0..3u64 {
        let event = UsageEvent {
            id: UsageEventId::try_new(format!("uev_overflow_{index}")).unwrap(),
            organization_id: organization.clone(),
            billing_account_id: account.clone(),
            task_id: 7,
            run_id: "1".into(),
            attempt_id: "a".into(),
            provider: "ollama".into(),
            model: "m".into(),
            unit: UsageUnit::ProviderCostMicro,
            quantity: 1,
            provider_cost_micro: max,
            source_operation: "session.cost_reservation.settled".into(),
            occurred_at_ms: NOW_MS,
            reconciliation_state: faktor_cloud::ReconciliationState::Reconciled,
            correction_of: None,
            category: faktor_cloud::SpendCategory::Byok,
            source_key: format!("manual:value-overflow:{index}"),
        };
        service.record_usage(&event).unwrap();
    }
    // The metering snapshot fails closed on the overflow (unchanged rule).
    match service.entitlement_snapshot(&organization) {
        Err(faktor_cloud::ControlPlaneError::Ledger(
            faktor_cloud::CreditLedgerError::Overflow { field, .. },
        )) => assert_eq!(field, "provider_cost_micro"),
        other => panic!("the overflowed fold must refuse typed, got {other:?}"),
    }
    // The value picture is unaffected: no fold runs on this path.
    let set = service.value_entitlements(&organization).unwrap();
    assert_eq!(set.plan_entitlements.len(), ValueEntitlement::ALL.len());
    assert!(service
        .check_value_entitlement(&organization, ValueEntitlement::SharedIntelligence)
        .is_ok());
}

/// Tenant isolation and subscription states: an unprovisioned tenant never
/// inherits a plan, an unknown plan name resolves `plan_missing`, and an
/// expired subscription refuses `subscription_inactive` — every refusal
/// typed, nothing guessed.
#[test]
fn unprovisioned_unknown_plan_and_expired_subscription_refuse_typed() {
    let configured = org("org_configured");
    let foreign = org("org_foreign");
    let service = service(tier_fixture(), Some(&configured));

    // The configured organization resolves the default plan by mapping...
    let set = service.value_entitlements(&configured).unwrap();
    assert!(set.plan_found);
    assert_eq!(set.plan_id.as_deref(), Some("local"));
    assert!(!set.subscription_active, "no subscription row yet");
    let refusal = set
        .check(ValueEntitlement::AdvancedVerificationPolicy)
        .expect_err("an inactive subscription refuses");
    assert_eq!(refusal.cause, ValueRefusalCause::SubscriptionInactive);

    // ...the foreign tenant resolves NOTHING: no handed-down default plan.
    let set = service.value_entitlements(&foreign).unwrap();
    assert!(!set.plan_found);
    assert!(set.plan_id.is_none());
    assert!(set.plan_entitlements.is_empty());
    for entitlement in ValueEntitlement::ALL {
        let refusal = set.check(entitlement).unwrap_err();
        assert_eq!(refusal.cause, ValueRefusalCause::PlanMissing);
        assert_eq!(refusal.organization_id, foreign);
    }
    // A subscription naming an undefined plan is the same typed gap.
    subscribe(&service, &foreign, "platinum", None);
    let set = service.value_entitlements(&foreign).unwrap();
    assert!(!set.plan_found);
    assert_eq!(set.plan_id.as_deref(), Some("platinum"));
    assert_eq!(
        set.check(ValueEntitlement::SharedIntelligence)
            .unwrap_err()
            .cause,
        ValueRefusalCause::PlanMissing
    );
    // An expired active row is inactive by DERIVED expiry, never a sweeper.
    subscribe(&service, &configured, "local", Some(NOW_MS - 1));
    let set = service.value_entitlements(&configured).unwrap();
    assert!(set.plan_found);
    assert!(!set.subscription_active);
    assert!(set.plan_grants(ValueEntitlement::SignedProofProvenance));
    assert_eq!(
        set.check(ValueEntitlement::SignedProofProvenance)
            .unwrap_err()
            .cause,
        ValueRefusalCause::SubscriptionInactive
    );
    subscribe(&service, &configured, "local", None);
    assert!(service
        .check_value_entitlement(&configured, ValueEntitlement::SignedProofProvenance)
        .is_ok());
}

/// The offline default: with no cloud authority wired the gate denies all
/// ten typed with cause `cloud_absent` and the enforcement hook named, while
/// nothing about the local product is gated by this crate.
#[test]
fn cloud_absent_gate_denies_all_value_entitlements_typed() {
    let organization = org("org_offline");
    let gate = ValueGate::cloud_absent(&organization);
    assert!(gate.resolved().is_none());
    for entitlement in ValueEntitlement::ALL {
        let refusal = gate.check(entitlement).unwrap_err();
        assert_eq!(refusal.cause, ValueRefusalCause::CloudAbsent);
        assert_eq!(refusal.hook, entitlement.enforcement_hook());
        assert!(refusal.to_string().contains(entitlement.hook_tag()));
    }
    let decisions = gate.decisions();
    assert_eq!(decisions.len(), ValueEntitlement::ALL.len());
    assert!(decisions.iter().all(|decision| !decision.granted));
    assert!(decisions
        .iter()
        .all(|decision| decision.cause == Some(ValueRefusalCause::CloudAbsent)));
    // The typed decision vocabulary round-trips (reporting surface).
    let wire = serde_json::to_value(&decisions).unwrap();
    assert_eq!(wire[0]["entitlement"], "advanced_verification_policy");
    assert_eq!(wire[0]["hook"], "verification_policy");
    assert_eq!(wire[0]["granted"], false);
    assert_eq!(wire[0]["cause"], "cloud_absent");
}

/// Governance is vocabulary: the base entitlement gates both capability
/// flags; a capability tag without the base never surfaces; a missing flag
/// refuses naming the capability; and the refusal names the hook.
#[test]
fn governance_capability_flags_are_gated_behind_the_base_entitlement() {
    let organization = org("org_ent");
    let ent_service = service(tier_fixture(), Some(&organization));
    subscribe(&ent_service, &organization, "enterprise", None);
    let set = ent_service.value_entitlements(&organization).unwrap();
    assert!(set.governance_capability(GovernanceCapability::Sso));
    assert!(set.governance_capability(GovernanceCapability::Scim));
    assert!(set
        .check_governance_capability(GovernanceCapability::Sso)
        .is_ok());
    let base_refusal = set.refusal(ValueEntitlement::EnterpriseGovernance);
    assert!(base_refusal.is_none());

    // A plan with the capability tag but WITHOUT the base entitlement: the
    // flag never surfaces and the base check refuses NotInPlan, naming the
    // capability.
    let mut config = BillingConfig::default();
    config.plans.insert(
        "half".into(),
        faktor_cloud::PlanConfig {
            period_ms: None,
            plan_id: "half".into(),
            features: [FEATURE_GOVERNANCE_SSO.to_string()].into_iter().collect(),
            limits: Default::default(),
        },
    );
    let half_service = service(config, Some(&organization));
    subscribe(&half_service, &organization, "half", None);
    let set = half_service.value_entitlements(&organization).unwrap();
    assert!(!set.governance.sso, "no base entitlement, no flag");
    assert!(!set.governance_capability(GovernanceCapability::Sso));
    let refusal = set
        .check_governance_capability(GovernanceCapability::Sso)
        .unwrap_err();
    assert_eq!(refusal.entitlement, ValueEntitlement::EnterpriseGovernance);
    assert_eq!(
        refusal.governance_capability,
        Some(GovernanceCapability::Sso)
    );
    assert_eq!(refusal.cause, ValueRefusalCause::NotInPlan);
    assert_eq!(refusal.hook.as_str(), "identity_federation");
    assert!(refusal.to_string().contains("capability sso"));

    // The base granted but the capability tag absent: the flag refuses,
    // still naming the capability.
    let mut config = BillingConfig::default();
    config.plans.insert(
        "gov".into(),
        faktor_cloud::PlanConfig {
            period_ms: None,
            plan_id: "gov".into(),
            features: [FEATURE_ENTERPRISE_GOVERNANCE.to_string()]
                .into_iter()
                .collect(),
            limits: Default::default(),
        },
    );
    let gov_service = service(config, Some(&organization));
    subscribe(&gov_service, &organization, "gov", None);
    let set = gov_service.value_entitlements(&organization).unwrap();
    assert!(set
        .check_governance_capability(GovernanceCapability::Sso)
        .is_err());
    let refusal = set
        .check_governance_capability(GovernanceCapability::Scim)
        .unwrap_err();
    assert_eq!(refusal.cause, ValueRefusalCause::NotInPlan);
    assert_eq!(
        refusal.governance_capability,
        Some(GovernanceCapability::Scim)
    );
    assert!(refusal.to_string().contains("capability scim"), "{refusal}");
}

/// Config strictness: the value tags are legal plan features (and refuse
/// unknown names exactly like the inference tags); a plan table with BOTH
/// halves validates.
#[test]
fn plan_validation_accepts_value_tags_and_refuses_unknown_names() {
    let mut config = BillingConfig::default();
    config.plans.insert(
        "value".into(),
        faktor_cloud::PlanConfig {
            period_ms: None,
            plan_id: "value".into(),
            features: [
                FEATURE_SHARED_INTELLIGENCE.to_string(),
                FEATURE_SIGNED_PROOF_PROVENANCE.to_string(),
                FEATURE_GOVERNANCE_SCIM.to_string(),
            ]
            .into_iter()
            .collect(),
            limits: Default::default(),
        },
    );
    config.validate().expect("value feature tags are legal");
    let mut bad = config.clone();
    bad.plans
        .get_mut("value")
        .unwrap()
        .features
        .insert("value_gold".into());
    let error = bad.validate().unwrap_err().to_string();
    assert!(error.contains("value_gold"), "{error}");
    let mut bad = config;
    bad.plans
        .get_mut("value")
        .unwrap()
        .features
        .insert(FEATURE_GOVERNANCE_SSO.to_string());
    bad.validate().expect(
        "a full governance plan validates (base missing is a mapping rule, not a config error)",
    );
}

/// The runtime enforcement adapter: the gate over the entitlement service
/// answers the SAME decisions the set does (entitled = Ok, denied = the
/// typed refusal naming entitlement, hook, cause and plan), an out-of-plan
/// org's refusal survives the conversion into the runtime seam vocabulary,
/// an absent gate refuses typed with `cloud_absent`, and the service-level
/// governance capability wrapper names the capability.
#[test]
fn enforcement_gate_mirrors_the_set_and_converts_typed() {
    use faktor_core::entitlement::{ValueGate as SeamGate, ValueRefusalCause as SeamCause};

    let configured = org("org_configured");
    let pro_service = service(tier_fixture(), Some(&configured));
    subscribe(&pro_service, &configured, "pro", None);
    let gate = pro_service.enforcement_gate(&configured);
    assert!(gate.is_wired());
    assert!(gate.check(ValueEntitlement::OutcomeAnalytics).is_ok());
    let refusal = gate.check(ValueEntitlement::PrivateRunners).unwrap_err();
    assert_eq!(refusal.entitlement, ValueEntitlement::PrivateRunners);
    assert_eq!(refusal.hook.as_str(), "worker_placement");
    assert_eq!(refusal.cause, SeamCause::NotInPlan);
    assert_eq!(refusal.plan_id.as_deref(), Some("pro"));
    assert_eq!(refusal.organization_id.as_deref(), Some("org_configured"));
    assert!(refusal.to_string().contains("private_runners"), "{refusal}");

    // A foreign organization never inherits the configured tenant's plan.
    let foreign = org("org_foreign");
    let foreign_gate = pro_service.enforcement_gate(&foreign);
    let refusal = foreign_gate
        .check(ValueEntitlement::OutcomeAnalytics)
        .unwrap_err();
    assert_eq!(refusal.cause, SeamCause::PlanMissing);
    assert_eq!(refusal.plan_id, None);

    // The offline default: no cloud authority => every check refuses typed.
    let absent = SeamGate::absent();
    let refusal = absent
        .check(ValueEntitlement::OutcomeAnalytics)
        .unwrap_err();
    assert_eq!(refusal.cause, SeamCause::CloudAbsent);
    assert_eq!(refusal.plan_id, None);
    assert!(refusal.to_string().contains("cloud_absent"), "{refusal}");

    // Service-level governance wrapper: the enterprise plan's SSO flag is
    // enforceable; the pro plan refuses typed naming the capability.
    let enterprise = org("org_enterprise");
    let enterprise_service = service(tier_fixture(), Some(&enterprise));
    subscribe(&enterprise_service, &enterprise, "enterprise", None);
    assert!(enterprise_service
        .check_governance_capability(&enterprise, GovernanceCapability::Sso)
        .is_ok());
    assert!(enterprise_service
        .check_governance_capability(&enterprise, GovernanceCapability::Scim)
        .is_ok());
    let refusal = pro_service
        .check_governance_capability(&configured, GovernanceCapability::Sso)
        .unwrap_err();
    assert_eq!(refusal.entitlement, ValueEntitlement::EnterpriseGovernance);
    assert_eq!(refusal.hook.as_str(), "identity_federation");
    assert!(refusal.to_string().contains("capability sso"), "{refusal}");
}
