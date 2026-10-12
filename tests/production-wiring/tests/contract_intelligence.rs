//! Production-wiring certification for the SYSTEM-INTELLIGENCE contract
//! enrichment seam (`docs/specs/autonomous-engineering.md`).
//!
//! The production daemon's drive-start contract compilation must accept an
//! OPTIONAL `SystemIntelligenceProvider` through the daemon's own
//! construction path (`build_daemon_core`), so a configured provider's
//! enrichment is additive with `external:<provider>` provenance and an
//! absent provider leaves the native contract byte-identical.
//!
//! These tests build the REAL daemon graph through the production builder
//! (`build_production_graph_with_system_intelligence` -> the executable's
//! `build_daemon_with_system_intelligence` -> `build_daemon_core`), create a
//! real workspace + session through the daemon's own session manager, and
//! run an ORDINARY agent turn (`AgentRuntime::run_turn`). The installed spy
//! is the ONLY seam substitution; it proves the compile path on that very
//! turn consults the provider installed by the core, and the durable task
//! contract attests the result.
//!
//! The `None` arm proves the production default: the same turn path with no
//! injected provider produces a durable contract that carries NO
//! external-origin statement at all.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use faktor_core::contract_compile::{
    EnrichmentKind, EnrichmentRequest, EnrichmentStatement, SystemIntelligenceProvider,
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

/// The certification spy: counts every consult and proposes ONE bounded,
/// well-formed disclosure risk whose text identifies the spy. It never
/// refuses, so the compile path must apply its statement additively.
#[derive(Default)]
struct SpyProvider {
    calls: AtomicUsize,
}

impl SystemIntelligenceProvider for SpyProvider {
    fn provider_id(&self) -> &str {
        "spy"
    }

    fn enrich(&self, request: &EnrichmentRequest<'_>) -> Result<Vec<EnrichmentStatement>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if request.max_statements == 0 {
            return Ok(Vec::new());
        }
        Ok(vec![EnrichmentStatement {
            kind: EnrichmentKind::Risk,
            text: "the contract compiler consulted the configured system-intelligence spy"
                .to_string(),
            confidence_bps: 9_000,
            basis: "production-wiring spy consult".to_string(),
            evidence: None,
            binding: None,
            severity: Some(faktor_core::contract::RiskSeverity::Low),
            advisory: false,
        }])
    }
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
    graph
        .agent
        .run_turn(handle.id(), "Fix the refresh-token race", &[])
        .await
        .expect("the ordinary turn runs through the production path");

    assert!(
        spy.calls.load(Ordering::SeqCst) >= 1,
        "the daemon's own drive-start contract compilation must consult the installed provider"
    );
    let task_id = handle.task_id().expect("the turn durably created a task");
    let task = handle
        .get_task(task_id)
        .expect("the task row reads")
        .expect("the task row exists");
    let consulted = task
        .contract
        .risks
        .iter()
        .find(|risk| {
            matches!(
                risk.origin,
                ContractOrigin::External { ref provider } if provider == "spy"
            )
        })
        .expect("the spy's statement must be durable in the task contract");
    assert!(consulted.description.contains("system-intelligence spy"));
    assert!(
        !task.contract.proof_obligations.is_empty(),
        "native statements must survive the additive enrichment: {:?}",
        task.contract
    );
    assert_eq!(
        external_statements(&task.contract),
        1,
        "the spy proposes exactly one additive statement"
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
