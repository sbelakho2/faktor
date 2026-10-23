//! NEW-GUARD adversarial verification (entitlement + dual-producer layers,
//! owned by the mutation campaign). Three production paths are pinned here,
//! each against a planted mutation of the exact guard:
//!
//! 1. `INV-ENTITLEMENT-OUTCOME-ANALYTICS-REFUSAL`: an organization-scope
//!    `/native/outcomes?org=...` read is the paid `outcome_analytics`
//!    boundary; a plan that does not map the entitlement must refuse TYPED
//!    (403 `value_entitlement_refused` naming entitlement + plan) — never
//!    short-circuit into the fold, never answer an empty/200 page.
//! 2. `INV-PRIVATE-RUNNERS-PLACEMENT-REFUSAL`: the LITERAL production
//!    `WorkerPlaneAdapter` consults the `private_runners` gate BEFORE
//!    minting a job generation; a denial is the typed refusal and leaves NO
//!    durable job behind, while the same plane still places when entitled.
//! 3. `INV-DUAL-PRODUCER-FACTS-DURABLE`: the terminal-run facts the
//!    completion gate persists BEFORE the task row turns terminal are the
//!    ONE durable truth both producers (agent-path record + executor
//!    settlement) derive from; deleting the persistence makes them diverge
//!    from the persisted facts.

use std::path::Path;
use std::sync::Arc;

use faktor_agent::{
    session_terminal_run_id, FinalVerificationBasis, MeasuredMoney, MoneyState, OutcomeLedger,
    RunVerifiedSample, TerminalRunFacts, TerminalRunFactsOutbox, TERMINAL_FACTS_SCHEMA_V,
};
use faktor_cloud::{
    BillingConfig, ControlPlane, EntitlementService, ManualClock, MemoryBillingStore,
    MemoryControlPlaneStore, OrganizationId, PlanConfig, SecretToken, Subscription, SubscriptionId,
    SubscriptionStatus, FEATURE_OUTCOME_ANALYTICS, FEATURE_PRIVATE_RUNNERS,
};
use faktor_core::id::TaskId;
use faktor_core::model::RouterPhase;
use faktor_core::state::{
    CheckExecution, FileStateEvidence, TaskState, TaskTransition, VerificationStatus,
};
use faktor_core::SessionId;
use faktor_orchestrator::runtime::task_executor::{RunSettlement, SettlementOutcome};
use faktor_session::{Task, TaskBudget};
use faktor_tests_production_wiring::harness::{self, Config};

const NOW_MS: i64 = 1_700_000_000_000;

fn config_file(dir: &Path, body: serde_json::Value) -> Config {
    let path = dir.join("new-guard-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

fn plan(plan_id: &str, features: &[&str]) -> PlanConfig {
    PlanConfig {
        period_ms: None,
        plan_id: plan_id.to_string(),
        features: features.iter().map(|f| f.to_string()).collect(),
        limits: Default::default(),
    }
}

fn billing_gate(
    plan_id: &str,
    features: &[&str],
    organization: &OrganizationId,
    clock: Arc<ManualClock>,
) -> Arc<EntitlementService> {
    let mut config = BillingConfig::default();
    config
        .plans
        .insert(plan_id.to_string(), plan(plan_id, features));
    let service = EntitlementService::new_for_organization(
        Arc::new(MemoryBillingStore::new()),
        clock,
        config,
        None,
    )
    .unwrap();
    service
        .set_subscription(&Subscription {
            id: SubscriptionId::try_new(format!("sub_{plan_id}")).unwrap(),
            organization: organization.clone(),
            plan_id: plan_id.to_string(),
            status: SubscriptionStatus::Active,
            started_ms: 0,
            expires_ms: None,
            updated_ms: NOW_MS,
        })
        .unwrap();
    service
}

// ------------------------------------------- 1. outcome-analytics entitlement

/// The real server surface over the daemon graph's own authorities: an
/// unentitled plan gets the typed 403 refusal naming the entitlement, its
/// hook and the resolved plan — never a 200, never a silently empty fold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn organization_outcomes_read_refuses_unentitled_plan_typed() {
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
        .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-new-guard")
        .unwrap();
    let organization = boot.organization.id.clone();
    let control_token = boot.token.expect("first issuance").expose().to_string();
    let billing = billing_gate("free", &[], &organization, clock.clone());

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
        403,
        "an unentitled organization read must refuse, not short-circuit"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "value_entitlement_refused");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("outcome_analytics"), "{message}");
    assert!(message.contains("outcome_ledger"), "{message}");
    assert!(message.contains("\"free\""), "{message}");
    drop(handle);
}

/// The same route with the entitlement mapped answers the organization read
/// (the gate is a boundary, never a blanket refusal). Once the commercial
/// plane is wired, the org-less alias is refused typed (`org_required`)
/// instead of serving whole-daemon metrics outside the tenant/plan gate;
/// the truly local (no billing authority) alias stays ungated in
/// `entitlement_seams.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entitled_organization_read_answers_and_orgless_alias_requires_scope() {
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
        .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-new-guard-2")
        .unwrap();
    let organization = boot.organization.id.clone();
    let control_token = boot.token.expect("first issuance").expose().to_string();
    let billing = billing_gate(
        "telemetry",
        &[FEATURE_OUTCOME_ANALYTICS],
        &organization,
        clock.clone(),
    );

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
    assert_eq!(response.status().as_u16(), 200);
    let response = client
        .get(format!("http://{}/native/outcomes", handle.addr))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        400,
        "a wired billing plane requires an explicit organization scope"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "org_required");
    drop(handle);
}

// ------------------------------------------------- 2. worker placement gate

fn placement_plane() -> (Arc<faktor_worker::WorkerPlane>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(faktor_worker::SqliteWorkerStore::open(&dir.path().join("workers.db")).unwrap());
    let plane = faktor_worker::WorkerPlane::with_system_clock(store);
    let org = OrganizationId::try_new("org_local").unwrap();
    let issued = plane
        .mint_registration_token(&org, "org_local", "new-guard")
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
        job_key: "session-1/new-guard".into(),
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
    let organization = OrganizationId::try_new("org_local").unwrap();
    let service = billing_gate(
        plan_id,
        features,
        &organization,
        Arc::new(ManualClock::new(NOW_MS)),
    );
    service.enforcement_gate(&organization)
}

/// A denied `private_runners` placement is refused TYPED at the production
/// adapter and leaves NO durable job; the SAME plane places remotely once the
/// plan maps the entitlement.
#[test]
fn remote_placement_denied_by_plan_refuses_typed_and_mints_no_job() {
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
    let spec = placement_spec();

    let denied = harness::production_worker_plane_adapter(
        plane.clone(),
        organization.clone(),
        "org_local".into(),
        requirements.clone(),
        placement_gate("free", &[]),
    );
    let refusal = denied
        .place(&spec)
        .expect_err("expected the typed entitlement refusal");
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
        }
        other => panic!("expected the typed entitlement refusal, got {other:?}"),
    }
    let key = faktor_worker::JobKey::try_new(spec.job_key.clone()).unwrap();
    assert!(
        plane
            .store()
            .job_by_key(organization.as_str(), &key)
            .unwrap()
            .is_none(),
        "a denied placement must never mint a durable job generation"
    );

    let entitled = harness::production_worker_plane_adapter(
        plane.clone(),
        organization.clone(),
        "org_local".into(),
        requirements,
        placement_gate("team", &[FEATURE_PRIVATE_RUNNERS]),
    );
    match entitled.place(&spec).unwrap() {
        PlacementDecision::Remote {
            job_id, worker_id, ..
        } => {
            assert!(job_id.starts_with("job_"), "{job_id}");
            assert_eq!(worker_id, "wrk_1");
        }
        other => panic!("expected a remote placement once entitled, got {other:?}"),
    }
    assert!(
        plane
            .store()
            .job_by_key(organization.as_str(), &key)
            .unwrap()
            .is_some(),
        "the entitled placement must mint the durable job"
    );
}

// ------------------------------------------------- 3. dual-producer facts

fn seed_graph(dir: &Path) -> harness::DaemonGraph {
    harness::build_production_graph(&dir.join("data"), Config::default())
        .expect("production daemon graph")
}

fn seed_task(
    graph: &harness::DaemonGraph,
    parent: SessionId,
    transitions: &[TaskTransition],
) -> TaskId {
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(Task {
            task_id,
            session_id: parent,
            goal: "new-guard facts".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: TaskBudget::default(),
            contract: Default::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    for transition in transitions {
        let revision = handle.task_revision(task_id).unwrap();
        handle
            .transition_task(task_id, revision, *transition, None)
            .unwrap();
    }
    task_id
}

fn complete_verified(graph: &harness::DaemonGraph, parent: SessionId, task_id: TaskId) {
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let now = handle.now_ms();
    handle
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![CheckExecution {
                check: "cargo check".into(),
                program: "cargo".into(),
                args: vec!["check".into()],
                category: "build".into(),
                required: true,
                status: VerificationStatus::Passed,
                started_ms: now,
                finished_ms: Some(now + 1),
                exit: Some(0),
                summary: None,
            }],
            vec![FileStateEvidence {
                path: "README.md".into(),
                digest_hex: "0".repeat(64),
                size: 6,
            }],
            vec![],
            None,
            VerificationStatus::Passed,
            now,
        )
        .unwrap();
    let record_id = handle
        .list_verification_records(task_id)
        .unwrap()
        .last()
        .unwrap()
        .record_id;
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .complete_verified_task(task_id, revision, record_id)
        .unwrap();
}

fn workspace_session(graph: &harness::DaemonGraph, dir: &Path, name: &str) -> (SessionId, String) {
    let root = dir.join(format!("ws-{name}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("README.md"), "owner\n").unwrap();
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, name, "provider", "model")
        .unwrap();
    graph.session.ensure_owner_worktree(handle.id()).unwrap();
    (handle.id(), session_terminal_run_id(handle.id()))
}

fn terminal_facts() -> TerminalRunFacts {
    TerminalRunFacts {
        schema_version: TERMINAL_FACTS_SCHEMA_V,
        risk_class: Some(faktor_core::model::RiskBucket::Low),
        samples: vec![RunVerifiedSample {
            provider: "provider".into(),
            model: "model".into(),
            phase: RouterPhase::Implement,
            verified_success: true,
            rework_cost_micro: 0,
            rework_turns: 0,
        }],
    }
}

async fn settle(
    graph: &harness::DaemonGraph,
    parent: SessionId,
    run_id: &str,
) -> SettlementOutcome {
    // These harnesses seed the run's task row directly (they never admit a
    // run through the executor), so the bound identity is the session's
    // current task. Settlement must still be TOLD it explicitly.
    let task_id = graph
        .session
        .get_session(parent)
        .unwrap()
        .unwrap()
        .task_id()
        .unwrap();
    graph
        .tasks
        .settle_run(RunSettlement::InSession {
            parent,
            run_id: run_id.to_string(),
            task_id: Some(task_id),
        })
        .await
        .expect("the production settlement converges")
}

/// The persisted facts ARE the durable truth: `load` returns them, the
/// agent-path producer derives its record from them (verified samples + risk
/// class survive), and the executor's terminal settlement replays the SAME
/// authoritative record as a no-op.
#[tokio::test]
async fn persisted_terminal_facts_are_the_durable_truth_both_producers_derive() {
    let dir = tempfile::tempdir().unwrap();
    let graph = seed_graph(dir.path());
    let (parent, run_id) = workspace_session(&graph, dir.path(), "facts");
    let task_id = seed_task(
        &graph,
        parent,
        &[
            TaskTransition::StartRunning,
            TaskTransition::RequestVerification,
            TaskTransition::StartVerification,
        ],
    );
    complete_verified(&graph, parent, task_id);

    let facts = terminal_facts();
    let outbox = TerminalRunFactsOutbox::new(graph.session.store());
    outbox.persist(&run_id, &facts).expect("facts persist");
    let loaded = match outbox.load(&run_id) {
        Ok(loaded) => loaded,
        Err(error) => {
            panic!("removing the facts persistence left the risk class underivable: {error}")
        }
    };
    assert_eq!(
        loaded.risk_class, facts.risk_class,
        "removing the facts persistence left the risk class underivable"
    );
    assert_eq!(
        loaded.samples, facts.samples,
        "removing the facts persistence lost the verified samples"
    );

    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let ledger = OutcomeLedger::new(graph.session.store());
    assert_eq!(
        ledger
            .record_terminal_run(&handle, &run_id, handle.task_id().unwrap())
            .unwrap(),
        Some(faktor_agent::LedgerAppend::Recorded)
    );
    let record = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(record.verified_samples, facts.samples);
    assert_eq!(record.risk_class, faktor_core::model::RiskBucket::Low);
    assert_eq!(
        record.final_basis,
        FinalVerificationBasis::DeterministicChecks
    );

    // The executor producer derives the SAME record: a replay, never a fork.
    settle(&graph, parent, &run_id).await;
    assert_eq!(ledger.count().unwrap(), 1);
    assert_eq!(ledger.get(&run_id).unwrap().unwrap(), record);
    let settlement = ledger.settlement(&run_id).unwrap().unwrap();
    assert_eq!(settlement.money_state, MoneyState::Complete);
    assert_eq!(settlement.avoided_hosted, MeasuredMoney::Unavailable);
}
