//! Production-wiring certification for the HUMAN approval authority.
//!
//! The daemon graph's own `build_daemon_core` path must register the
//! `approval_request`/`approval_status` tools over the durable authority in
//! its data dir, expose no decide tool, and observe a decision made by a
//! human operator on that same durable ledger. The test acts as the human
//! through the public authority/key API (never through a tool) — exactly the
//! split production enforces.

use std::path::Path;
use std::sync::Arc;

use faktor_agent::{Tool, ToolCallMode, ToolOutcome, ToolRunCtx};
use faktor_approval::{ApprovalAuthority, DecisionOutcome, OperatorKey, Principal, PrincipalKind};
use faktor_core::cancellation::CancellationToken;
use faktor_core::id::{OpId, SessionId, TaskId, WorkspaceId, WorktreeId};
use faktor_core::WorkspaceIdentity;
use faktor_tests_production_wiring::harness::{self, Config};
use serde_json::{json, Value};

fn graph(dir: &Path) -> harness::DaemonGraph {
    harness::build_production_graph(&dir.join("data"), Config::default())
        .expect("production daemon graph")
}

fn tool_ctx() -> ToolRunCtx {
    ToolRunCtx {
        session_id: SessionId::new(1),
        op_id: OpId::new(1),
        identity: WorkspaceIdentity::new(WorkspaceId::new(1), WorktreeId::new(1), TaskId::new(1)),
        cancellation: CancellationToken::new(),
        artifacts: Arc::new(faktor_agent::ToolArtifactSink::Null),
        tool_call_mode: ToolCallMode::Native,
        workspace: None,
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        deadline_ms: 30_000,
        permission_granted: true,
    }
}

async fn call(tool: &Tool, args: Value) -> ToolOutcome {
    (tool.execute)(tool_ctx(), args).await.expect("tool result")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_registers_durable_approval_tools_and_no_decide_tool() {
    let directory = tempfile::tempdir().unwrap();
    let graph = graph(directory.path());
    let names = graph.agent.deps().tools.names();
    assert!(names.iter().any(|name| name == "approval_request"));
    assert!(names.iter().any(|name| name == "approval_status"));
    assert!(
        !names.iter().any(|name| name == "approval_decide"),
        "no agent tool may decide an approval"
    );

    let digest = "a".repeat(64);
    let request = call(
        &harness::daemon_tool(&graph, "approval_request"),
        json!({
            "kind": "engineering_constraint_relaxation",
            "subject": "req-set-1",
            "evidence": ["cpk-study"],
            "proposed_change": "hole tolerance +/-0.010 -> +/-0.025 mm",
            "risk": "high",
            "requester_id": "supply-agent",
            "bound_revision": "7",
            "bound_digest": digest,
            "expires_in_ms": 60000,
        }),
    )
    .await;
    let request: Value = serde_json::from_str(&request.text).unwrap();
    assert_eq!(request["status"], "approval_pending");
    let request_id = request["request"]["id"].as_str().unwrap().to_string();

    // A human operator decides on the daemon's data dir through the
    // authoritative key/ledger API, never through the graph's tools.
    let data_dir = directory.path().join("data");
    let key = OperatorKey::generate();
    faktor_approval::save_operator_key(&data_dir, "ops", &key, false).unwrap();
    faktor_approval::register_operator_key(&data_dir, "ops", &key, faktor_approval::unix_now_ms())
        .unwrap();
    let authority = ApprovalAuthority::open(&data_dir).unwrap();
    authority
        .decide(
            &request_id,
            Principal::new("ops", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "reviewed".to_string(),
            faktor_approval::unix_now_ms(),
        )
        .unwrap();

    let status = call(
        &harness::daemon_tool(&graph, "approval_status"),
        json!({"request_id": request_id}),
    )
    .await;
    let status: Value = serde_json::from_str(&status.text).unwrap();
    assert_eq!(status["status"], "approved");
    assert_eq!(status["bound_digest"], "a".repeat(64));

    // The daemon's own supply_decision tool refuses a relaxation until the
    // human approval binds to the live engineering revision/digest.
    let mut loop_request: faktor_supply_engineering::LoopRequest = serde_json::from_str(
        include_str!("../../../crates/cli/tests/fixtures/supply/loop_request.json"),
    )
    .unwrap();
    let new_limit: engineering_core::NumericLimit = serde_json::from_value(json!({
        "kind": "max",
        "limit": {"value": 140.0, "unit": {"symbol": "mohm", "dimension": [2,1,-3,-2,0,0,0], "scale": 0.001, "offset": 0.0}}
    }))
    .unwrap();
    let relaxation: faktor_supply_engineering::Relaxation = serde_json::from_value(json!({
        "requirement_id": "esr",
        "new_limit": new_limit,
        "rationale": "measured ESR distribution supports 140 mohm",
        "provenance": loop_request.engineering.requirements[2].provenance,
        "falsifier": "a lot exceeding 140 mohm",
        "evidence": loop_request.engineering.evidence[0],
    }))
    .unwrap();
    let live_digest = loop_request
        .engineering
        .relaxation_digest("esr", &new_limit)
        .unwrap()
        .to_hex();
    let live_revision = loop_request.engineering.revision.get().to_string();
    loop_request.relaxations.push(relaxation);
    let loop_args = serde_json::to_value(&loop_request).unwrap();

    let refused = call(
        &harness::daemon_tool(&graph, "supply_decision"),
        loop_args.clone(),
    )
    .await;
    let refused: Value = serde_json::from_str(&refused.text).unwrap();
    assert_eq!(refused["status"], "approval_required", "{refused}");
    assert_eq!(refused["kind"], "engineering_constraint_relaxation");

    let human = ApprovalAuthority::open(&data_dir).unwrap();
    let now = faktor_approval::unix_now_ms();
    let loop_approval = human
        .propose(
            faktor_approval::ApprovalDraft {
                kind: faktor_approval::ApprovalKind::EngineeringConstraintRelaxation,
                subject: loop_request.engineering.id.clone(),
                evidence: vec!["cpk-study".to_string()],
                proposed_change: "ESR max 100 -> 140 mohm".to_string(),
                risk: faktor_approval::RiskLevel::High,
                requester: Principal::new("supply-agent", PrincipalKind::Agent).unwrap(),
                bound_revision: live_revision,
                bound_digest: live_digest,
                expires_at_ms: now + 60_000,
            },
            now,
        )
        .unwrap();
    human
        .decide(
            &loop_approval.id,
            Principal::new("ops", PrincipalKind::HumanOperator).unwrap(),
            &key,
            DecisionOutcome::Approved,
            "reviewed".to_string(),
            now + 1,
        )
        .unwrap();
    let executed = call(&harness::daemon_tool(&graph, "supply_decision"), loop_args).await;
    let executed: Value = serde_json::from_str(&executed.text).unwrap();
    assert_eq!(
        executed["applied_relaxations"].as_array().unwrap().len(),
        1,
        "{executed}"
    );
}
