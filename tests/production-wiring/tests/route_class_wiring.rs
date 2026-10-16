//! Production-wiring certification for the ROUTE-TIME EXECUTION CLASS
//! (economic audit item 7 P1 classification): the daemon's own routing
//! authority (the production `Arc<dyn RoutingPolicy>` built by
//! `build_daemon_core` from the config's policy mode) must stamp the
//! economy/balanced/frontier policy tier — beside locality — at DECISION
//! time, and the reserve path must persist that class durably on the
//! `cost_reservation` row. Before this wiring, production could only ever
//! write `Local` (an authoritative local-zero route) or `Unknown`, so every
//! hosted tier metric read as unmeasured.
//!
//! Nothing is hand-built: the real daemon graph runs an ORDINARY turn over a
//! loopback OpenAI-compatible endpoint carrying an EXPLICIT operator price
//! (so the candidate is genuinely hosted-priced), and the assertion reads
//! the daemon's own durable reservation rows.

use std::path::Path;

use faktor_core::model::{ExecutionClass, PriceAuthority};
use faktor_provider::testing::{sse_body, MockAction, MockServer};
use faktor_tests_production_wiring::harness::{self, Config};

fn config_file(dir: &Path, body: serde_json::Value) -> Config {
    let path = dir.join("wiring-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

/// One loopback OpenAI-compatible chat endpoint with an explicit quality
/// declaration AND an explicit operator price: the model is a REAL hosted
/// candidate (Exact pricing authority), and the config leaves `routing_mode`
/// at the production default (Economy). The served id is DECLARED through
/// the endpoint's `models` list — under the capability-authority semantics
/// an undeclared id resolves `Unknown` (feature flags forced off) and the
/// daemon would refuse the turn pre-wire before any reservation exists.
fn economy_hosted_config(dir: &Path, base: &str) -> Config {
    config_file(
        dir,
        serde_json::json!({
            "model": "default",
            "compact_at_usage": 1.0,
            "sandbox": {"network": [base]},
            "providers": [{
                "kind": "open_ai",
                "id": "fake-openai",
                "base_url": base,
                "api_key_env": null,
                "api": "chat",
                "models": ["default"],
                "allow_loopback": true,
                "quality": {"coding_reliability": 80, "context_reliability": 80},
                "pricing": {
                    "input_micro_usd_per_million_tokens": 1_000_000,
                    "output_micro_usd_per_million_tokens": 2_000_000
                },
            }],
        }),
    )
}

fn write_workspace(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("workspace");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"wiring\"\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn stamp() -> u64 { 1 }\n").unwrap();
    root
}

/// An ordinary daemon turn under the Economy policy over a genuinely priced
/// hosted endpoint persists `hosted_economy` on the reservation — never
/// `Unknown` (no tier authority) and never `Local` (a price number is never
/// locality). The frozen `Exact` pricing snapshot rides the same row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn economy_route_persists_the_hosted_tier_at_decision_time() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/chat/completions",
        MockAction::Respond {
            status: 200,
            body: sse_body(&[serde_json::json!({
                "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]
            })]),
        },
    );
    let base = server.base_url().await;

    let dir = tempfile::tempdir().unwrap();
    let root = write_workspace(dir.path());
    let graph =
        harness::build_production_graph(dir.path(), economy_hosted_config(dir.path(), &base))
            .expect("production daemon graph");
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, "classify the route", "fake-openai", "default")
        .unwrap();
    graph
        .agent
        .run_turn(handle.id(), "classify the route", &[])
        .await
        .expect("the ordinary turn runs through the production path");
    let task_id = handle.task_id().expect("the turn durably created a task");
    let rows = graph
        .session
        .store()
        .cost_reservations_of(handle.id(), task_id, 16)
        .expect("the durable reservations read");
    assert!(
        !rows.is_empty(),
        "the routed model call must reserve durably before dispatch"
    );
    let classes: Vec<ExecutionClass> = rows.iter().map(|row| row.execution_class).collect();
    assert!(
        classes.contains(&ExecutionClass::HostedEconomy),
        "the Economy policy tier must be stamped at decision time: {classes:?}"
    );
    assert!(
        !classes.iter().any(|class| class.is_local()),
        "a genuinely priced hosted call is never Local: {classes:?}"
    );
    assert!(
        !classes.contains(&ExecutionClass::Unknown),
        "a served route carries its durable class, never Unknown: {classes:?}"
    );
    assert!(
        rows.iter().any(|row| {
            row.pricing_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.authority == PriceAuthority::Exact)
        }),
        "the decision-time class rides the same frozen pricing snapshot"
    );
}
