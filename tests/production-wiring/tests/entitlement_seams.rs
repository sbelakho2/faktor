//! Production-wiring certification for the VALUE-ENTITLEMENT enforcement
//! seams: the layered value entitlements (faktor-cloud plans) must gate the
//! ACTUAL capability boundaries of the production daemon — not a report or
//! UI layer — with typed refusals that name the entitlement and the plan.
//!
//! The seams exercised THROUGH the production constructors on this path:
//!
//! 1. **verification policy** — `[verification] advanced = true` builds the
//!    advanced policy through the gated constructor inside the executable's
//!    own `build_daemon_core`; an entitled plan produces the advanced policy
//!    on the graph, a denied plan refuses the DAEMON typed, and no billing
//!    authority refuses `cloud_absent` typed;
//! 2. **worker placement** — the LITERAL production `WorkerPlaneAdapter`
//!    (same construction `serve_impl` performs) refuses an unentitled
//!    `private_runners` placement typed BEFORE any job generation is minted,
//!    and places remotely when the gate is entitled;
//! 3. **analytics read** — the real server surface (`faktor_server::serve`
//!    over the daemon graph's own authorities) refuses an unentitled
//!    organization-scope `/native/outcomes?org=...` read typed, answers the
//!    entitled read, and answers the shared typed `billing_disabled` when no
//!    billing authority is wired. The org-less local read is never gated.
//!
//! Every authority here is the production constructor: the daemon graph, the
//! commercial `EntitlementService` over a real store, the real worker plane,
//! the real server router.

use std::path::Path;
use std::sync::Arc;

use faktor_cloud::{
    BillingConfig, ControlPlane, EntitlementService, ManualClock, MemoryBillingStore,
    MemoryControlPlaneStore, OrganizationId, PlanConfig, SecretToken, SqliteControlPlaneStore,
    Subscription, SubscriptionId, SubscriptionStatus, FEATURE_ADVANCED_VERIFICATION_POLICY,
    FEATURE_OUTCOME_ANALYTICS, FEATURE_PRIVATE_RUNNERS,
};
use faktor_tests_production_wiring::harness::{self, Config};

const NOW_MS: i64 = 1_700_000_000_000;

fn config_file(dir: &Path, body: serde_json::Value) -> Config {
    let path = dir.join("wiring-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

fn plan(plan_id: &str, features: &[&str]) -> PlanConfig {
    PlanConfig {
        plan_id: plan_id.to_string(),
        features: features.iter().map(|f| f.to_string()).collect(),
        limits: Default::default(),
    }
}

/// Subscribe the daemon tenant to `plan_id` durably in the SAME billing
/// database `build_daemon_core` opens for the boot-time gate decision.
fn seed_subscription(config: &Config, data_dir: &Path, plan_id: &str) {
    let path = config
        .billing
        .billing_path(data_dir)
        .unwrap()
        .expect("enabled billing resolves its path");
    let store = Arc::new(SqliteControlPlaneStore::open(&path).unwrap())
        as Arc<dyn faktor_cloud::BillingStore>;
    let service_config = config
        .billing
        .service_config()
        .unwrap()
        .expect("enabled billing resolves its service config");
    let organization = config.billing.organization().unwrap();
    let service = EntitlementService::with_system_clock_for_organization(
        store,
        service_config,
        organization.clone(),
    )
    .unwrap();
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new(format!("sub_{plan_id}")).unwrap(),
            organization,
            plan_id: plan_id.to_string(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: NOW_MS,
        })
        .unwrap();
}

// ------------------------------------------------------ 1. verification policy

/// The ADVANCED verification policy is the `advanced_verification_policy`
/// paid augmentation: with `[verification] advanced = true` the executable's
/// own daemon assembly must construct it through the gated constructor.
/// Entitled => the graph's verification service carries the advanced policy
/// (full checks no longer backgrounded, raised floors).
#[test]
fn advanced_verification_policy_is_built_through_the_gated_constructor_when_entitled() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(
        dir.path(),
        serde_json::json!({
            "model": "default",
            "billing": {
                "enabled": true,
                "organization": "org_local",
                "account": "acct_local",
                "default_plan": "team",
                "plans": {
                    "team": {
                        "plan_id": "team",
                        "features": [FEATURE_ADVANCED_VERIFICATION_POLICY],
                    }
                }
            },
            "verification": {"advanced": true}
        }),
    );
    seed_subscription(&config, dir.path(), "team");
    let graph = harness::build_production_graph(dir.path(), config)
        .expect("an entitled advanced policy builds the daemon");
    let policy = graph.verification.policy();
    assert!(
        policy.implies_advanced(),
        "the daemon graph's verification service carries the ADVANCED policy"
    );
    assert_eq!(
        policy.quick_max,
        faktor_verify::exec::VerificationPolicy::ADVANCED_QUICK_MAX
    );
    assert_eq!(
        policy.unit_max,
        faktor_verify::exec::VerificationPolicy::ADVANCED_UNIT_MAX
    );
    assert!(!policy.full_as_background);
}

/// A plan that does not map the entitlement refuses the DAEMON typed at the
/// constructor (naming the entitlement, hook, cause and plan) — never a
/// silent downgrade to the local profile.
#[test]
fn advanced_verification_policy_denied_by_plan_refuses_daemon_build_typed() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(
        dir.path(),
        serde_json::json!({
            "model": "default",
            "billing": {
                "enabled": true,
                "organization": "org_local",
                "account": "acct_local",
                "default_plan": "free",
                "plans": {"free": {"plan_id": "free"}}
            },
            "verification": {"advanced": true}
        }),
    );
    seed_subscription(&config, dir.path(), "free");
    let error = match harness::build_production_graph(dir.path(), config) {
        Ok(_) => panic!("a denied advanced policy must refuse the daemon build"),
        Err(error) => error,
    };
    assert!(error.contains("advanced_verification_policy"), "{error}");
    assert!(error.contains("not_in_plan"), "{error}");
    assert!(
        error.contains("\"free\""),
        "the refusal names the plan: {error}"
    );
}

/// No billing authority wired: the advanced request refuses `cloud_absent`
/// typed (offline keeps the local product, the paid augmentation is refused
/// loudly — never silently granted).
#[test]
fn advanced_verification_policy_offline_refuses_cloud_absent_typed() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(
        dir.path(),
        serde_json::json!({
            "model": "default",
            "verification": {"advanced": true}
        }),
    );
    let error = match harness::build_production_graph(dir.path(), config) {
        Ok(_) => panic!("advanced without any cloud authority must refuse typed"),
        Err(error) => error,
    };
    assert!(error.contains("cloud_absent"), "{error}");
    assert!(error.contains("advanced_verification_policy"), "{error}");
}

/// The LOCAL default profile is never gated: the same daemon builds without
/// any billing section when `advanced` stays false.
#[test]
fn default_verification_policy_is_never_gated() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), serde_json::json!({"model": "default"}));
    let graph = harness::build_production_graph(dir.path(), config)
        .expect("the local daemon builds with no billing at all");
    assert_eq!(
        graph.verification.policy(),
        faktor_verify::exec::VerificationPolicy::default()
    );
}

/// Numeric combinations that reach the advanced profile with `advanced`
/// left FALSE are still the paid augmentation: offline the daemon refuses
/// typed `cloud_absent`; entitled, the same config builds the advanced
/// profile through the checked constructor.
#[test]
fn numeric_advanced_combination_is_gated_by_the_entitlement() {
    // Offline refusal: `full_as_background: false` alone reaches the paid
    // inline-full-check depth.
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(
        dir.path(),
        serde_json::json!({
            "model": "default",
            "verification": {"full_as_background": false}
        }),
    );
    let error = match harness::build_production_graph(dir.path(), config) {
        Ok(_) => panic!("a numeric advanced combination must refuse the ungated daemon"),
        Err(error) => error,
    };
    assert!(error.contains("cloud_absent"), "{error}");
    assert!(error.contains("advanced_verification_policy"), "{error}");

    // Entitled: the advanced floors imply the entitlement and the advanced
    // profile is built (never the raw local numbers).
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(
        dir.path(),
        serde_json::json!({
            "model": "default",
            "billing": {
                "enabled": true,
                "organization": "org_local",
                "account": "acct_local",
                "default_plan": "team",
                "plans": {
                    "team": {
                        "plan_id": "team",
                        "features": [FEATURE_ADVANCED_VERIFICATION_POLICY],
                    }
                }
            },
            "verification": {"quick_max_s": 120, "unit_max_s": 1800}
        }),
    );
    seed_subscription(&config, dir.path(), "team");
    let graph = harness::build_production_graph(dir.path(), config)
        .expect("the entitled numeric combination builds");
    assert!(
        graph.verification.policy().implies_advanced(),
        "the numeric combination must resolve to the advanced profile"
    );
    assert_eq!(
        graph.verification.policy().quick_max,
        faktor_verify::exec::VerificationPolicy::ADVANCED_QUICK_MAX
    );
}

// -------------------------------------------------------- 2. worker placement

fn placement_plane() -> (Arc<faktor_worker::WorkerPlane>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(faktor_worker::SqliteWorkerStore::open(&dir.path().join("workers.db")).unwrap());
    let plane = faktor_worker::WorkerPlane::with_system_clock(store);
    let org = OrganizationId::try_new("org_local").unwrap();
    let issued = plane
        .mint_registration_token(&org, "org_local", "placement-test")
        .unwrap();
    let plain = SecretToken::try_new(issued.token.expose().to_string()).unwrap();
    let mut capabilities = faktor_worker::WorkerCapabilities {
        os: "linux".into(),
        arch: "x86_64".into(),
        toolchains: vec!["rust".into()],
        sandbox: Vec::new(),
        network: faktor_worker::NetworkProfile::None,
        cpu_cores: 8,
        memory_mb: 16_384,
        gpu: None,
        region: "us".into(),
        trust_domain: "org_local".into(),
        protocol_version: faktor_worker::WORKER_PROTOCOL_VERSION,
    };
    capabilities.normalize().unwrap();
    plane
        .register(
            &org,
            &faktor_worker::WorkerId::try_new("wrk_1").unwrap(),
            &plain,
            capabilities,
            "worker one",
        )
        .unwrap();
    (plane, dir)
}

fn placement_spec() -> faktor_orchestrator::placement::PlacementSpec {
    faktor_orchestrator::placement::PlacementSpec {
        job_key: "session-1/goal-abc".into(),
        organization: "org_local".into(),
        trust_domain: "org_local".into(),
        payload_digest: "a".repeat(64),
        kind: "in_session".into(),
        os: None,
        arch: None,
        toolchains: Vec::new(),
        sandbox: Vec::new(),
        network: None,
        min_cpu_cores: 0,
        min_memory_mb: 0,
        gpu: false,
        region: None,
    }
}

fn placement_gate(plan_id: &str, features: &[&str]) -> faktor_core::entitlement::ValueGate {
    let mut config = BillingConfig::default();
    config
        .plans
        .insert(plan_id.to_string(), plan(plan_id, features));
    let service = EntitlementService::new_for_organization(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
        config,
        None,
    )
    .unwrap();
    let organization = OrganizationId::try_new("org_local").unwrap();
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new("sub_placement").unwrap(),
            organization: organization.clone(),
            plan_id: plan_id.to_string(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: NOW_MS,
        })
        .unwrap();
    service.enforcement_gate(&organization)
}

/// A denied plan refuses the remote placement TYPED at the production adapter
/// BEFORE any job generation exists on the worker plane; an entitled plan
/// places remotely through the same constructor.
#[test]
fn private_runner_placement_is_gated_typed_at_the_production_adapter() {
    use faktor_orchestrator::placement::{PlacementDecision, PlacementRefusal};

    let (plane, _dir) = placement_plane();
    let organization = OrganizationId::try_new("org_local").unwrap();
    let requirements = faktor_worker::JobRequirements {
        os: None,
        arch: None,
        toolchains: Vec::new(),
        sandbox: Vec::new(),
        network: None,
        min_cpu_cores: 0,
        min_memory_mb: 0,
        gpu: false,
        region: None,
        trust_domain: "org_local".into(),
    };

    // Denied: typed refusal naming the entitlement, hook, cause and plan;
    // NOTHING is minted (no scheduled job exists afterwards).
    let denied = harness::production_worker_plane_adapter(
        plane.clone(),
        organization.clone(),
        "org_local".into(),
        requirements.clone(),
        placement_gate("free", &[]),
    );
    let refusal = denied.place(&placement_spec()).unwrap_err();
    match refusal {
        PlacementRefusal::Entitlement(refusal) => {
            assert_eq!(
                refusal.entitlement,
                faktor_core::entitlement::ValueEntitlement::PrivateRunners
            );
            assert_eq!(refusal.hook.as_str(), "worker_placement");
            assert_eq!(
                refusal.cause,
                faktor_core::entitlement::ValueRefusalCause::NotInPlan
            );
            assert_eq!(refusal.plan_id.as_deref(), Some("free"));
            assert!(refusal.to_string().contains("private_runners"));
        }
        other => panic!("expected the typed entitlement refusal, got {other:?}"),
    }

    // Entitled: the SAME adapter with the private_runners plan places the
    // run remotely through the real worker plane.
    let entitled = harness::production_worker_plane_adapter(
        plane.clone(),
        organization.clone(),
        "org_local".into(),
        requirements,
        placement_gate("team", &[FEATURE_PRIVATE_RUNNERS]),
    );
    match entitled.place(&placement_spec()).unwrap() {
        PlacementDecision::Remote {
            job_id, worker_id, ..
        } => {
            assert!(job_id.starts_with("job_"), "{job_id}");
            assert_eq!(worker_id, "wrk_1");
        }
        other => panic!("expected a remote placement, got {other:?}"),
    }
}

// ----------------------------------------------------------- 3. analytics read

/// The real server surface over the daemon graph's own authorities: an
/// organization-scope `/native/outcomes?org=...` read is the
/// `outcome_analytics` paid boundary; the entitled org receives the fold, a
/// denied plan is refused typed (403, naming entitlement/hook/cause/plan),
/// no billing authority is the shared typed 409, and the org-less local read
/// is never gated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn organization_outcome_read_is_gated_typed_through_the_server_surface() {
    for (plan_id, features, expected) in [
        ("telemetry", &[FEATURE_OUTCOME_ANALYTICS][..], 200u16),
        ("free", &[][..], 403),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = config_file(dir.path(), serde_json::json!({"model": "default"}));
        let graph = harness::build_production_graph(dir.path(), config).unwrap();
        let deps = faktor_server::ServerDeps::new_with(
            graph.session.clone(),
            graph.agent.clone(),
            graph.permissions.clone(),
            graph.orchestrator.clone(),
            graph.tasks.clone(),
            graph.budgets.clone(),
        );
        let token = deps.auth_token.clone();
        let clock = Arc::new(ManualClock::new(NOW_MS));
        let control_plane = Arc::new(ControlPlane::new(
            Arc::new(MemoryControlPlaneStore::new()),
            clock.clone(),
        ));
        let boot = control_plane
            .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-outcomes")
            .unwrap();
        let organization = boot.organization.id.clone();
        let control_token = boot.token.expect("first issuance").expose().to_string();

        let mut billing_config = BillingConfig::default();
        billing_config
            .plans
            .insert(plan_id.to_string(), plan(plan_id, features));
        let billing = EntitlementService::new_for_organization(
            Arc::new(MemoryBillingStore::new()),
            clock.clone(),
            billing_config,
            None,
        )
        .unwrap();
        billing
            .set_subscription(&Subscription {
                id: SubscriptionId::try_new("sub_outcomes").unwrap(),
                organization: organization.clone(),
                plan_id: plan_id.to_string(),
                status: SubscriptionStatus::Active,
                started_ms: 0,
                expires_ms: None,
                updated_ms: NOW_MS,
            })
            .unwrap();

        let deps = deps
            .with_control_plane(control_plane.clone())
            .with_billing(billing);
        let handle = faktor_server::serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let response = client
            .get(format!(
                "http://{}/native/outcomes?org={}",
                handle.addr, organization
            ))
            .bearer_auth(token.as_str())
            .header("x-faktor-control-token", &control_token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            expected,
            "plan {plan_id}: {response:?}"
        );
        if expected == 403 {
            let body: serde_json::Value = response.json().await.unwrap();
            assert_eq!(body["error"]["code"], "value_entitlement_refused");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.contains("outcome_analytics"), "{message}");
            assert!(message.contains("outcome_ledger"), "{message}");
            assert!(message.contains("\"free\""), "{message}");
        }
        drop(handle);
    }

    // Offline: no billing authority wired => the shared typed 409 for the
    // org-scope read; the org-less LOCAL read still answers.
    let dir = tempfile::tempdir().unwrap();
    let config = config_file(dir.path(), serde_json::json!({"model": "default"}));
    let graph = harness::build_production_graph(dir.path(), config).unwrap();
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let control_plane = Arc::new(ControlPlane::new(
        Arc::new(MemoryControlPlaneStore::new()),
        clock.clone(),
    ));
    let boot = control_plane
        .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-offline")
        .unwrap();
    let organization = boot.organization.id.clone();
    let control_token = boot.token.expect("first issuance").expose().to_string();
    let deps = faktor_server::ServerDeps::new_with(
        graph.session.clone(),
        graph.agent.clone(),
        graph.permissions.clone(),
        graph.orchestrator.clone(),
        graph.tasks.clone(),
        graph.budgets.clone(),
    )
    .with_control_plane(control_plane.clone());
    let token = deps.auth_token.clone();
    let handle = faktor_server::serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let response = client
        .get(format!(
            "http://{}/native/outcomes?org={}",
            handle.addr, organization
        ))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", &control_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 409);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "billing_disabled");
    let response = client
        .get(format!("http://{}/native/outcomes", handle.addr))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        200,
        "the org-less local read is never gated"
    );
}
