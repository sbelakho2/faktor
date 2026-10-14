//! Production-wiring certification for the SYSTEM-INTELLIGENCE contract
//! findings seam (`docs/specs/autonomous-engineering.md`).
//!
//! The production daemon's drive-start contract compilation must accept an
//! OPTIONAL `SystemIntelligenceProvider` through the daemon's own
//! construction path (`build_daemon_core`), ASK it the fixed bounded
//! engineering questions before the pure compile, and carry its findings
//! additively with `external:<provider>` provenance; an absent provider
//! leaves the native contract byte-identical.
//!
//! These tests build the REAL daemon graph through the production builder
//! (`build_production_graph_with_system_intelligence` -> the executable's
//! `build_daemon_with_system_intelligence` -> `build_daemon_core`), create a
//! real workspace + session through the daemon's own session manager, and
//! run an ORDINARY agent turn (`AgentRuntime::run_turn`). The installed spy
//! is the ONLY seam substitution; it proves the compile path on that very
//! turn asked the provider installed by the core EVERY bounded question, and
//! the durable task contract attests the typed findings.
//!
//! The `None` arm proves the production default: the same turn path with no
//! injected provider produces a durable contract that carries NO
//! external-origin statement at all.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use std::sync::Arc;

use faktor_core::contract_compile::{
    FindingsRequest, FindingsResponse, IntelligenceFuture, IntelligenceQuestion, ProviderFinding,
    SystemIntelligenceProvider,
};
use faktor_core::ContractOrigin;
use faktor_provider::testing::{MockAction, MockServer};

use faktor_tests_production_wiring::harness::{self, Config};

/// The one native Ollama chat response (the adapter accepts a single JSON
/// body on a streaming request).
fn ollama_chat_ok() -> String {
    r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#.into()
}

fn config_file(dir: &Path, body: serde_json::Value) -> Config {
    let path = dir.join("wiring-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

/// One loopback Ollama-family provider with an explicit quality declaration
/// (same shape as the retrieval certification), so the pinned turn clears the
/// Implement-phase floor.
fn ollama_config(dir: &Path, base: &str) -> Config {
    config_file(
        dir,
        serde_json::json!({
            "model": "default",
            "compact_at_usage": 1.0,
            "sandbox": {"network": [base]},
            "routing_mode": {"pinned": {"provider": "fake-ollama", "model": "default"}},
            "providers": [{
                "kind": "ollama",
                "id": "fake-ollama",
                "base_url": base,
                "allow_loopback": true,
                "quality": {"coding_reliability": 80, "context_reliability": 80},
            }],
        }),
    )
}

fn write_workspace(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("workspace");
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"auth\"\n").unwrap();
    std::fs::write(
        root.join("src/auth/refresh.rs"),
        "pub fn refresh() -> u64 { 0 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/refresh_tokens.rs"),
        "#[test]\nfn replayed_refresh_token_fails() {}\n",
    )
    .unwrap();
    root
}

/// The certification spy: records which bounded questions it was asked,
/// counts consults, and ANSWERS every question with one finding bound to the
/// request's own repository identity (the honest provider shape). It never
/// refuses, so the compile path must apply its findings additively.
#[derive(Default)]
struct SpyProvider {
    calls: AtomicUsize,
    questions: Mutex<Vec<&'static str>>,
}

impl SystemIntelligenceProvider for SpyProvider {
    fn provider_id(&self) -> &str {
        "spy"
    }

    fn findings<'a>(
        &'a self,
        request: &'a FindingsRequest<'a>,
    ) -> IntelligenceFuture<'a, Result<FindingsResponse, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            {
                let mut seen = self
                    .questions
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                for question in request.questions {
                    seen.push(question.as_str());
                }
            }
            if request.max_findings == 0 {
                return Ok(FindingsResponse::default());
            }
            let findings = request
                .questions
                .iter()
                .map(|question| ProviderFinding {
                    question: *question,
                    kind: question.expected_kind(),
                    text: format!("the production spy answered {}", question.as_str()),
                    basis: "production-wiring spy consult".to_string(),
                    confidence_bps: 9_000,
                    repository_digest: request.repository_digest.to_string(),
                    provider: "spy".to_string(),
                    revision: request.source_revision.map(str::to_string),
                    severity: None,
                })
                .collect();
            Ok(FindingsResponse {
                findings,
                diagnostics: Vec::new(),
            })
        })
    }
}

/// A wired value gate that grants every entitlement: the certification host
/// represents an ENTITLED organization, so the paid organization-contracts
/// consult is not refused by the commercial gate while the production
/// question path is certified.
#[derive(Debug)]
struct GrantingGate;

impl faktor_core::entitlement::ValueEntitlementGate for GrantingGate {
    fn check_value_entitlement(
        &self,
        _entitlement: faktor_core::entitlement::ValueEntitlement,
    ) -> Result<(), faktor_core::entitlement::ValueGateRefusal> {
        Ok(())
    }
}

fn granting_gate() -> faktor_core::entitlement::ValueGate {
    faktor_core::entitlement::ValueGate::wired(Arc::new(GrantingGate))
}

fn external_statements(contract: &faktor_core::EngineeringContract) -> usize {
    let mut count = 0usize;
    for constraint in &contract.constraints {
        count += usize::from(matches!(constraint.origin, ContractOrigin::External { .. }));
    }
    for invariant in &contract.invariants {
        count += usize::from(matches!(invariant.origin, ContractOrigin::External { .. }));
    }
    for risk in &contract.risks {
        count += usize::from(matches!(risk.origin, ContractOrigin::External { .. }));
    }
    for obligation in &contract.proof_obligations {
        count += usize::from(matches!(obligation.origin, ContractOrigin::External { .. }));
    }
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn build_daemon_core_installs_the_seam_and_the_turn_path_consults_it() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/api/chat",
        MockAction::Respond {
            status: 200,
            body: ollama_chat_ok(),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = write_workspace(dir.path());
    let spy = Arc::new(SpyProvider::default());
    let graph = harness::build_production_graph_with_system_intelligence(
        dir.path(),
        ollama_config(dir.path(), &base),
        Some(spy.clone()),
    )
    .expect("production daemon graph builds with the system-intelligence seam");

    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    // The initial task's goal is the session name (the runtime restores the
    // task row from the session title), so the production compile really
    // sees the refresh objective.
    let handle = graph
        .session
        .create_session(
            workspace,
            "Fix the refresh-token race",
            "fake-ollama",
            "default",
        )
        .unwrap();
    // The certification host is an ENTITLED organization: the commercial
    // value gate grants the organization-contracts consult, so the question
    // path through the core is actually exercised.
    graph.agent.set_value_gate(granting_gate());
    graph
        .agent
        .run_turn(handle.id(), "Fix the refresh-token race", &[])
        .await
        .expect("the ordinary turn runs through the production path");

    assert!(
        spy.calls.load(Ordering::SeqCst) >= 1,
        "the daemon's own drive-start contract compilation must ASK the installed provider"
    );
    let asked: Vec<&str> = spy
        .questions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    for question in IntelligenceQuestion::ALL {
        assert!(
            asked.contains(&question.as_str()),
            "the daemon's compile path must ask question {}: {asked:?}",
            question.as_str()
        );
    }
    let task_id = handle.task_id().expect("the turn durably created a task");
    let task = handle
        .get_task(task_id)
        .expect("the task row reads")
        .expect("the task row exists");
    // The answers are durable typed statements with external:spy provenance.
    for (label, question) in [
        (
            "affected component: ",
            IntelligenceQuestion::AffectedComponents,
        ),
        (
            "hidden dependency: ",
            IntelligenceQuestion::HiddenDependencies,
        ),
        ("blast radius: ", IntelligenceQuestion::BlastRadius),
        (
            "historical failure: ",
            IntelligenceQuestion::HistoricalFailures,
        ),
    ] {
        let expected = format!("{label}the production spy answered {}", question.as_str());
        let risk = task
            .contract
            .risks
            .iter()
            .find(|risk| risk.description == expected)
            .unwrap_or_else(|| panic!("the spy's typed finding {expected:?} must be durable"));
        assert_eq!(
            risk.origin,
            ContractOrigin::External {
                provider: "spy".into()
            }
        );
    }
    let invariant = format!(
        "invariant that must survive: the production spy answered {}",
        IntelligenceQuestion::Invariants.as_str()
    );
    assert!(
        task.contract
            .invariants
            .iter()
            .any(|entry| entry.statement == invariant
                && matches!(entry.origin, ContractOrigin::External { .. })),
        "the spy's invariant answer must be durable: {:?}",
        task.contract.invariants
    );
    assert!(
        !task
            .contract
            .risks
            .iter()
            .any(|risk| risk.description.contains("declares")),
        "a provider finding is never a capability description"
    );
    assert!(
        !task.contract.proof_obligations.is_empty(),
        "native statements must survive the additive findings: {:?}",
        task.contract
    );
    assert_eq!(
        external_statements(&task.contract),
        5,
        "the spy answers the five bounded questions with exactly five statements"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_provider_the_durable_contract_has_no_external_statements() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/api/chat",
        MockAction::Respond {
            status: 200,
            body: ollama_chat_ok(),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = write_workspace(dir.path());
    let graph = harness::build_production_graph_with_system_intelligence(
        dir.path(),
        ollama_config(dir.path(), &base),
        None,
    )
    .expect("production daemon graph builds without the seam");

    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(
            workspace,
            "Fix the refresh-token race",
            "fake-ollama",
            "default",
        )
        .unwrap();
    graph
        .agent
        .run_turn(handle.id(), "Fix the refresh-token race", &[])
        .await
        .expect("the ordinary turn runs through the production path");

    let task_id = handle.task_id().expect("the turn durably created a task");
    let task = handle
        .get_task(task_id)
        .expect("the task row reads")
        .expect("the task row exists");
    assert_eq!(
        external_statements(&task.contract),
        0,
        "no provider configured/installed => native-only contract: {:?}",
        task.contract
    );
    assert!(
        !task.contract.proof_obligations.is_empty(),
        "the native contract still compiles: {:?}",
        task.contract
    );
}
