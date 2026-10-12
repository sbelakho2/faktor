//! Adversarial route tests of the value-centric paid entitlement report
//! (`GET /native/value-entitlements`): billing-disabled parity, plan-tier
//! mapping over the wire, governance capability flags, typed per-entitlement
//! decisions for unentitled/expired tenants, inference-independence (a
//! zero-usage local organization's report is byte-identical before and after
//! usage rows exist) and canonical-schema wire parity (the generated IDE
//! clients parse the pinned schema entries).

use std::sync::Arc;

use crate::api::tests::test_deps;
use crate::{serve, ServerHandle};
use faktor_cloud::{
    BillingAccountId, BillingConfig, EntitlementService, ManualClock, MemoryBillingStore,
    OrganizationId, Subscription, SubscriptionId, SubscriptionStatus, UsageEvent, UsageEventId,
    UsageUnit, ValueEntitlement,
};
use faktor_session::SessionManager;

const NOW_MS: i64 = 1_700_000_000_000;

/// The shared value-only plan-tier fixture (empty `managed_providers`, no
/// limits): the route test consumes the SAME fixture the cloud tests lock.
fn tier_config() -> BillingConfig {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/entitlements/value-plan-tiers.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("value tier fixture"))
        .expect("valid BillingConfig fixture")
}

fn billing_service(configured_org: Option<&OrganizationId>) -> Arc<EntitlementService> {
    EntitlementService::new_for_organization(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
        tier_config(),
        configured_org.cloned(),
    )
    .unwrap()
}

async fn value_deps(
    root: &std::path::Path,
    configured_org: Option<&OrganizationId>,
) -> (
    ServerHandle,
    Arc<faktor_cloud::ControlPlane>,
    Arc<EntitlementService>,
    String,
    Arc<SessionManager>,
) {
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let billing = billing_service(configured_org);
    let deps = test_deps(root)
        .with_control_plane(control_plane.clone())
        .with_billing(billing.clone());
    let token = deps.auth_token.clone();
    let session = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    (
        handle,
        control_plane,
        billing,
        token.as_str().to_string(),
        session,
    )
}

fn bootstrap(
    control_plane: &faktor_cloud::ControlPlane,
    name: &str,
    email: &str,
) -> (OrganizationId, String) {
    let result = control_plane
        .bootstrap_organization(name, email, "Owner", &format!("boot-{email}"))
        .unwrap();
    let org = result.organization.id.clone();
    let token = result
        .token
        .map(|t| t.expose().to_string())
        .expect("first issuance presents the token");
    (org, token)
}

fn subscribe(
    service: &EntitlementService,
    organization: &OrganizationId,
    plan: &str,
    expires: Option<i64>,
) {
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_route").unwrap(),
            organization: organization.clone(),
            plan_id: plan.into(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: expires,
            updated_ms: NOW_MS,
        })
        .unwrap();
}

/// All ten value tags in the stable enum order the decisions use.
fn all_value_tags() -> Vec<&'static str> {
    ValueEntitlement::ALL
        .into_iter()
        .map(|e| e.as_str())
        .collect()
}

#[tokio::test]
async fn value_entitlements_disabled_answers_typed_409() {
    let dir = tempfile::tempdir().unwrap();
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let deps = test_deps(dir.path()).with_control_plane(control_plane.clone());
    assert!(deps.billing.is_none(), "billing is disabled by default");
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (_org, control_token) = bootstrap(&control_plane, "A", "a@a.test");
    let resp = client
        .get(format!("{base}/native/value-entitlements"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "billing_disabled");
    // Without a control-plane principal: 401, not a guess.
    let resp = client
        .get(format!("{base}/native/value-entitlements"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn value_entitlements_report_maps_the_enterprise_tier_over_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = value_deps(dir.path(), None).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, control_token) = bootstrap(&control_plane, "A", "a@a.test");
    subscribe(&billing, &org, "enterprise", None);

    let resp = client
        .get(format!("{base}/native/value-entitlements"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let report = &body["value_entitlements"];
    assert_eq!(report["organization_id"], org.as_str());
    assert_eq!(report["plan_id"], "enterprise");
    assert_eq!(report["plan_found"], true);
    assert_eq!(report["subscription_active"], true);
    let tags: Vec<&str> = report["plan_entitlements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(tags, all_value_tags());
    assert_eq!(report["governance"]["sso"], true);
    assert_eq!(report["governance"]["scim"], true);
    // One typed decision per entitlement, hook named, nothing denied.
    let decisions = report["decisions"].as_array().unwrap();
    assert_eq!(decisions.len(), ValueEntitlement::ALL.len());
    for (decision, entitlement) in decisions.iter().zip(ValueEntitlement::ALL) {
        assert_eq!(decision["entitlement"], entitlement.as_str());
        assert_eq!(decision["hook"], entitlement.hook_tag());
        assert_eq!(decision["granted"], true);
        assert!(decision["cause"].is_null(), "{decision}");
    }
    // Inference-independence: the report carries no usage/token/credit data.
    assert!(report.get("total_tokens").is_none());
    assert!(report.get("credits").is_none());
    assert!(report.get("managed_spend_micro").is_none());
}

#[tokio::test]
async fn value_entitlements_refuse_typed_for_expired_unentitled_and_restricted_tenants() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = value_deps(dir.path(), None).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org_a, token_a) = bootstrap(&control_plane, "A", "a@a.test");
    let (org_c, token_c) = bootstrap(&control_plane, "C", "c@c.test");

    // Org A: an EXPIRED subscription denies every value entitlement typed.
    subscribe(&billing, &org_a, "enterprise", Some(NOW_MS - 1));
    let resp = client
        .get(format!("{base}/native/value-entitlements"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let report = &body["value_entitlements"];
    assert_eq!(report["plan_found"], true);
    assert_eq!(report["subscription_active"], false);
    for decision in report["decisions"].as_array().unwrap() {
        assert_eq!(decision["granted"], false);
        assert_eq!(decision["cause"], "subscription_inactive");
    }

    // Org A renewed at the LOCAL tier: exactly the local value set grants;
    // a higher-tier entitlement refuses naming the cause over the wire.
    subscribe(&billing, &org_a, "local", None);
    let resp = client
        .get(format!("{base}/native/value-entitlements"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &token_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let report = &body["value_entitlements"];
    let tags: Vec<&str> = report["plan_entitlements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        tags,
        vec![
            "advanced_verification_policy",
            "organization_contracts",
            "signed_proof_provenance"
        ]
    );
    let decisions = report["decisions"].as_array().unwrap();
    let private_runners = &decisions[8];
    assert_eq!(private_runners["entitlement"], "private_runners");
    assert_eq!(private_runners["hook"], "worker_placement");
    assert_eq!(private_runners["granted"], false);
    assert_eq!(private_runners["cause"], "not_in_plan");
    assert_eq!(report["governance"]["sso"], false);
    assert_eq!(report["governance"]["scim"], false);

    // Org C: unprovisioned, no subscription — nothing resolves, nothing is
    // handed down from the configured tenant.
    let resp = client
        .get(format!("{base}/native/value-entitlements"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &token_c)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let report = &body["value_entitlements"];
    assert_eq!(report["organization_id"], org_c.as_str());
    assert_eq!(report["plan_id"], serde_json::Value::Null);
    assert_eq!(report["plan_found"], false);
    assert!(report["plan_entitlements"].as_array().unwrap().is_empty());
    for decision in report["decisions"].as_array().unwrap() {
        assert_eq!(decision["granted"], false);
        assert_eq!(decision["cause"], "plan_missing");
    }
}

/// The inference-independence invariant over the real route: a local-only
/// deployment (BYOK-only account, no provider attempts) reports its value
/// entitlements, and recording usage rows does NOT change the value report
/// byte-for-byte — the value path never folds the ledger.
#[tokio::test]
async fn value_report_is_byte_identical_before_and_after_local_usage_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = value_deps(dir.path(), None).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, control_token) = bootstrap(&control_plane, "Local", "local@a.test");
    subscribe(&billing, &org, "pro", None);
    let account = BillingAccountId::try_new("acct_local").unwrap();
    billing
        .ensure_account(&org, &account, "local", false)
        .unwrap();

    let fetch = || {
        let client = client.clone();
        let base = base.clone();
        let token = token.clone();
        let control_token = control_token.clone();
        async move {
            let resp = client
                .get(format!("{base}/native/value-entitlements"))
                .bearer_auth(&token)
                .header("x-faktor-control-token", &control_token)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let body: serde_json::Value = resp.json().await.unwrap();
            body["value_entitlements"].clone()
        }
    };
    let before = fetch().await;

    // A zero-inference local run: BYOK events only (recorded, never
    // debited), no managed provider attempts.
    for index in 0..2u64 {
        let event = UsageEvent {
            id: UsageEventId::try_new(format!("uev_local_{index}")).unwrap(),
            organization_id: org.clone(),
            billing_account_id: account.clone(),
            task_id: 7,
            run_id: "1".into(),
            attempt_id: "a".into(),
            provider: "ollama".into(),
            model: "llama-local".into(),
            unit: UsageUnit::InputTokens,
            quantity: 10,
            provider_cost_micro: 0,
            source_operation: "session.usage.recorded".into(),
            occurred_at_ms: NOW_MS,
            reconciliation_state: faktor_cloud::ReconciliationState::Reconciled,
            correction_of: None,
            category: faktor_cloud::SpendCategory::Byok,
            source_key: format!("manual:local-usage:{index}"),
        };
        billing.record_usage(&event).unwrap();
    }
    // The metering half SAW the usage...
    let fold = billing.fold(&org).unwrap();
    assert_eq!(fold.totals.input_tokens, 20);
    assert_eq!(fold.totals.managed_cost_micro, 0);
    // ...and the value report is byte-identical: no ledger dependency.
    let after = fetch().await;
    assert_eq!(before, after, "value entitlements never fold usage");
    assert_eq!(before["plan_id"], "pro");
    assert_eq!(before["subscription_active"], true);
}

/// Value-entitlement migration parity: the live wire keys AND JSON kinds of
/// `GET /native/value-entitlements` mirror the canonical schema types
/// field-for-field (the schema entries are what the generated IDE clients
/// parse; a drift is a generated-client bug). Adversarial directions: a
/// missing non-optional schema key, a wire-only key, a wrong kind and a
/// wrong nullability all fail here. A granted decision OMITS `cause`
/// (`skip_serializing_if`), and an unresolved tenant carries a null
/// `plan_id`: both wire directions are pinned.
#[tokio::test]
async fn value_entitlements_wire_shape_matches_the_canonical_schema() {
    use std::collections::BTreeSet;

    use faktor_protocol::schema::{canonical, Shape, Ty, TypeDef};

    fn schema_def<'a>(types: &'a [TypeDef], name: &str) -> &'a TypeDef {
        types
            .iter()
            .find(|def| def.name == name)
            .unwrap_or_else(|| panic!("{name} must be in the canonical schema"))
    }

    /// Structural parity of one schema struct against one live wire value:
    /// every wire key is a schema field, every non-optional schema field is
    /// present, and every present value matches its kind and nullability.
    fn assert_shape(types: &[TypeDef], name: &str, value: &serde_json::Value, path: &str) {
        let def = schema_def(types, name);
        let Shape::Struct { fields } = &def.shape else {
            panic!("{name} must be a struct");
        };
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("{path}: expected a JSON object"));
        let schema: BTreeSet<&str> = fields.iter().map(|f| f.name).collect();
        for key in object.keys() {
            assert!(
                schema.contains(key.as_str()),
                "{path}: wire key {key} is not a field of schema type {name}"
            );
        }
        for field in fields {
            let field_path = format!("{path}.{}", field.name);
            let Some(entry) = object.get(field.name) else {
                assert!(
                    field.optional,
                    "{field_path}: missing non-optional schema field"
                );
                continue;
            };
            if entry.is_null() {
                assert!(
                    field.nullable,
                    "{field_path}: wire null but the schema says non-nullable"
                );
                continue;
            }
            match &field.ty {
                Ty::String => assert!(entry.is_string(), "{field_path}: expected a string"),
                Ty::I64 | Ty::I32 => assert!(
                    entry.is_i64() || entry.is_u64(),
                    "{field_path}: expected an integer, got {entry}"
                ),
                Ty::Bool => assert!(entry.is_boolean(), "{field_path}: expected a boolean"),
                Ty::Json => {}
                Ty::Named { name } => assert_shape(types, name, entry, &field_path),
                Ty::List { of } => {
                    let items = entry
                        .as_array()
                        .unwrap_or_else(|| panic!("{field_path}: expected an array"));
                    match of.as_ref() {
                        Ty::Named { name } => {
                            for (index, item) in items.iter().enumerate() {
                                assert_shape(types, name, item, &format!("{field_path}[{index}]"));
                            }
                        }
                        Ty::String => {
                            for (index, item) in items.iter().enumerate() {
                                assert!(
                                    item.is_string(),
                                    "{field_path}[{index}]: expected a string tag"
                                );
                            }
                        }
                        other => panic!("{field_path}: unsupported list element {other:?}"),
                    }
                }
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, billing, token, _session) = value_deps(dir.path(), None).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (org, control_token) = bootstrap(&control_plane, "A", "a@a.test");
    subscribe(&billing, &org, "local", None);
    let resp = client
        .get(format!("{base}/native/value-entitlements"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["ok"], true,
        "the native response keeps its ok envelope"
    );
    let schema = canonical();
    let report = &body["value_entitlements"];
    assert_shape(
        &schema.types,
        "ValueEntitlementsReport",
        report,
        "GET /native/value-entitlements",
    );
    // Both decision shapes ride the same page: the local tier grants some
    // entitlements (no `cause` key at all) and denies others (a string
    // `cause`), so the optional schema field is exercised in both wire
    // directions.
    let decisions = report["decisions"].as_array().unwrap();
    assert!(
        decisions.iter().any(|d| d.get("cause").is_none()),
        "the local tier must grant at least one entitlement"
    );
    assert!(
        decisions.iter().any(|d| d.get("cause").is_some()),
        "the local tier must deny at least one entitlement"
    );

    // Unprovisioned tenant: nothing resolves, so `plan_id` is present-null
    // and every decision carries the typed `plan_missing` cause.
    let (_org_c, token_c) = bootstrap(&control_plane, "C", "c@c.test");
    let resp = client
        .get(format!("{base}/native/value-entitlements"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &token_c)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["value_entitlements"]["plan_id"].is_null(),
        "an unresolved tenant reports a null plan_id"
    );
    assert_shape(
        &schema.types,
        "ValueEntitlementsReport",
        &body["value_entitlements"],
        "GET /native/value-entitlements (unprovisioned)",
    );
    handle.request_shutdown();
}
