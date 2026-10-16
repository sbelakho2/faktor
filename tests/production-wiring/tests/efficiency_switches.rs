//! Production-wiring certification of the `[efficiency]` switches the daemon
//! graph consumes (audit: every documented off-switch must change observable
//! production behavior).
//!
//! The graph under test is the executable's own `build_daemon`
//! ([`harness::build_production_graph`]) over the daemon's real store. The
//! `[efficiency].rework_routing` switch gates the INSTALLATION of the
//! store-backed verified-outcome registry at graph-build (builder step 7):
//! ON (the production default, including an absent `[efficiency]` section)
//! installs the durable registry the runtime's deterministic gate sites
//! record into and every route consult reads; OFF installs the documented
//! no-op registry, so a verified sample recorded through the SAME policy
//! entry the runtime uses never reaches durable state.

use std::path::Path;

use faktor_core::model::{RiskBucket, RouterPhase, TaskClass};
use faktor_tests_production_wiring::harness::{self, Config, DaemonGraph};

fn config_file(dir: &Path, body: serde_json::Value) -> Config {
    let path = dir.join("efficiency-config.json");
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    Config::load(&path).expect("the production config loader must accept the test config")
}

/// The durable row of the sample recorded below, read through the store
/// projection the routing consults (v18 `model_outcome_stats`).
fn recorded(graph: &DaemonGraph) -> Option<faktor_store::ModelOutcomeStatsRow> {
    graph
        .session
        .store()
        .model_outcome_stats_get(
            "alpha",
            "fast",
            RouterPhase::Implement,
            TaskClass::Medium,
            RiskBucket::Low,
        )
        .expect("the daemon store must be readable")
}

/// One settled Implement call whose deterministic gate verified — exactly
/// the shape the runtime's genuine-end attribution records through the
/// installed routing policy.
fn verified_sample() -> faktor_agent::SettledCallOutcome {
    faktor_agent::SettledCallOutcome {
        provider: "alpha".into(),
        model: "fast".into(),
        phase: RouterPhase::Implement,
        success: true,
        retried: false,
        rate_limited: false,
        latency_ms: 120,
        verified: Some(faktor_agent::VerifiedCallAttribution {
            task_class: TaskClass::Medium,
            risk_bucket: RiskBucket::Low,
            verified_success: true,
            rework_cost_micro: 0,
            rework_turns: 0,
        }),
    }
}

#[test]
fn rework_routing_switch_gates_the_store_backed_outcome_registry() {
    let dir = tempfile::tempdir().unwrap();

    // ON: an absent `[efficiency]` section keeps the production default
    // (`EfficiencyCfg::production_defaults`, every flag on). The recorded
    // verified sample must land in the daemon's durable store.
    let on = harness::build_production_graph(
        &dir.path().join("on"),
        config_file(dir.path(), serde_json::json!({})),
    )
    .expect("default daemon graph builds");
    on.routing.record_call_outcome(&verified_sample());
    let row = recorded(&on).expect("rework_routing on: the durable registry is installed");
    assert_eq!(row.successes_first_pass, 1);
    assert_eq!(row.failures_first_pass, 0);
    assert_eq!(row.sample_count, 1);
    drop(on);

    // OFF: the explicit switch must change production behavior — the same
    // record call through the same policy entry reaches the no-op registry:
    // no durable sample exists, and every later route consult reads "no
    // verified history" (the documented legacy estimate).
    let off = harness::build_production_graph(
        &dir.path().join("off"),
        config_file(
            dir.path(),
            serde_json::json!({ "efficiency": { "rework_routing": false } }),
        ),
    )
    .expect("daemon graph builds with rework_routing off");
    off.routing.record_call_outcome(&verified_sample());
    assert!(
        recorded(&off).is_none(),
        "rework_routing off: no verified-outcome sample may reach durable state"
    );
}
