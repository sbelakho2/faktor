//! Adversarial route tests of the Engineering Outcome Ledger native read
//! surface (economic audit item 7): auth refusal, empty-ledger parity, exact
//! aggregate fold over durable records, money-key encoding, strict DTOs and
//! bounded cursor paging.

use std::sync::Arc;

use crate::api::tests::test_deps;
use crate::{serve, ServerHandle};
use faktor_agent::{
    ChangedCodeScale, FinalVerificationBasis, OutcomeLedger, ProofCoverage, RunModelStep,
    RunOutcomeRecord, RunVerifiedSample, OUTCOME_RECORD_SCHEMA_V,
};
use faktor_core::model::{RiskBucket, RouterPhase, TaskClass};
use faktor_session::SessionManager;

fn step(provider: &str, model: &str, local: bool, spend_micro: u64) -> RunModelStep {
    RunModelStep {
        provider: provider.into(),
        model: model.into(),
        local,
        spend_micro,
    }
}

fn record(run_id: &str, completed_at_ms: i64, local: bool) -> RunOutcomeRecord {
    let (provider, model, spend) = if local {
        ("ollama", "local-7b", 0u64)
    } else {
        ("hosted", "m-standard", 1_000_000)
    };
    RunOutcomeRecord {
        schema_version: OUTCOME_RECORD_SCHEMA_V,
        run_id: run_id.into(),
        session_id: 3,
        task_id: 5,
        task_class: TaskClass::Medium,
        risk_class: RiskBucket::Low,
        candidate_snapshot: Some("tm1:candidate".into()),
        final_snapshot: "tm1:final".into(),
        accepted: true,
        integrated: true,
        verified_first_attempt: true,
        repair_attempts: 0,
        model_sequence: vec![step(provider, model, local, spend)],
        model_spend_micro: spend,
        verification_spend_micro: 100_000,
        rework_spend_micro: 0,
        avoided_hosted_micro: if local { 250_000 } else { 0 },
        wall_time_ms: 1_000,
        human_interventions: 0,
        proof: ProofCoverage {
            checks_passed: 2,
            checks_required: 2,
        },
        change_scale: ChangedCodeScale {
            files: 1,
            added_lines: 10,
            removed_lines: 2,
        },
        rollback_events: 0,
        recovery_events: 0,
        final_basis: FinalVerificationBasis::DeterministicChecks,
        frontier_escalations: 0,
        completed_at_ms,
        verified_samples: vec![RunVerifiedSample {
            provider: provider.into(),
            model: model.into(),
            phase: RouterPhase::Implement,
            verified_success: true,
            rework_cost_micro: 0,
            rework_turns: 0,
        }],
    }
}

async fn outcomes_deps(
    root: &std::path::Path,
    records: &[RunOutcomeRecord],
) -> (ServerHandle, String, Arc<SessionManager>) {
    let deps = test_deps(root);
    let token = deps.auth_token.clone();
    let session = deps.session.clone();
    let ledger = OutcomeLedger::new(session.store());
    for record in records {
        ledger
            .record(record)
            .expect("fixture record is valid and records exactly once");
    }
    let handle = serve(deps, 0).await.unwrap();
    (handle, token.as_str().to_string(), session)
}

/// Every money-named key (`*_micro` / `*Micro`) at any depth is a decimal
/// string, never a JSON number.
fn assert_money_encoding(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, entry) in map {
                if faktor_cloud::money::is_money_field(key) && !entry.is_null() {
                    let raw = entry.as_str().unwrap_or_else(|| {
                        panic!("money field {key:?} is emitted as a JSON number: {entry}")
                    });
                    raw.parse::<u64>()
                        .unwrap_or_else(|_| panic!("money field {key:?}={raw:?} is not decimal"));
                }
                assert_money_encoding(entry);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_money_encoding(item);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn unauthenticated_outcomes_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let resp = reqwest::Client::new()
        .get(format!("{base}/native/outcomes"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    handle.request_shutdown();
}

#[tokio::test]
async fn empty_ledger_answers_zero_metrics_and_no_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, token, _session) = outcomes_deps(dir.path(), &[]).await;
    let base = format!("http://{}", handle.addr);
    let resp = reqwest::Client::new()
        .get(format!("{base}/native/outcomes"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_money_encoding(&body);
    let metrics = &body["metrics"];
    assert_eq!(metrics["runs"], 0);
    assert_eq!(metrics["verifiedChanges"], 0);
    assert_eq!(metrics["verifiedFirstPassPpm"], 0);
    assert_eq!(metrics["reworkCostRatioPpm"], 0);
    assert_eq!(metrics["localSubstitutionPpm"], 0);
    assert_eq!(metrics["meanTimeToVerifiedChangeMs"], 0);
    assert_eq!(metrics["humanInterventionPpm"], 0);
    assert_eq!(metrics["regressionRollbackPpm"], 0);
    assert_eq!(metrics["proofCoveragePpm"], 0);
    assert_eq!(metrics["frontierEscalationPpm"], 0);
    assert_eq!(metrics["truncated"], false);
    assert_eq!(body["runs"].as_array().unwrap().len(), 0);
    assert!(body["nextCursor"].is_null());
    handle.request_shutdown();
}

#[tokio::test]
async fn ledger_metrics_and_run_page_match_the_durable_records() {
    let dir = tempfile::tempdir().unwrap();
    let records = vec![
        record("run-a", 1_000, false),
        record("run-b", 2_000, true),
        record("run-c", 3_000, false),
    ];
    let (handle, token, _session) = outcomes_deps(dir.path(), &records).await;
    let base = format!("http://{}", handle.addr);
    // One page of two: newest first + a total cursor.
    let resp = reqwest::Client::new()
        .get(format!("{base}/native/outcomes?limit=2"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_money_encoding(&body);
    let metrics = &body["metrics"];
    // A (hosted 1_100_000) + B (local 100_000) + C (hosted 1_100_000).
    assert_eq!(metrics["runs"], 3);
    assert_eq!(metrics["verifiedChanges"], 3);
    assert_eq!(metrics["verifiedFirstPassChanges"], 3);
    assert_eq!(metrics["verifiedFirstPassPpm"], 1_000_000);
    assert_eq!(metrics["totalVerifiedSpendMicro"], "2300000");
    assert_eq!(metrics["verifiedCostPerChangeMicro"], "766667"); // ceil
    assert_eq!(metrics["totalSpendMicro"], "2300000");
    assert_eq!(metrics["totalReworkSpendMicro"], "0");
    assert_eq!(metrics["reworkCostRatioPpm"], 0);
    assert_eq!(metrics["localRuns"], 1);
    assert_eq!(metrics["localSubstitutionPpm"], 333_333);
    assert_eq!(metrics["avoidedHostedInferenceMicro"], "250000");
    assert_eq!(metrics["totalWallTimeMs"], 3_000);
    assert_eq!(metrics["meanTimeToVerifiedChangeMs"], 1_000);
    assert_eq!(metrics["humanInterventionPpm"], 0);
    assert_eq!(metrics["regressionRollbackPpm"], 0);
    assert_eq!(metrics["proofCoveragePpm"], 1_000_000);
    assert_eq!(metrics["frontierEscalationPpm"], 0);
    assert_eq!(metrics["scanned"], 3);
    assert_eq!(metrics["truncated"], false);
    let runs = body["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "bounded page");
    assert_eq!(runs[0]["runId"], "run-c");
    assert_eq!(runs[0]["verified"], true);
    assert_eq!(runs[0]["modelSpendMicro"], "1000000");
    assert_eq!(runs[0]["modelSequence"][0]["local"], false);
    assert_eq!(runs[1]["runId"], "run-b");
    assert_eq!(runs[1]["modelSequence"][0]["local"], true);
    assert_eq!(runs[1]["avoidedHostedMicro"], "250000");
    assert_eq!(runs[1]["finalBasis"], "deterministic_checks");
    let next = &body["nextCursor"];
    assert_eq!(next["completedAtMs"], 2_000);
    assert_eq!(next["runId"], "run-b");
    // The cursor continues strictly older, without repeating run-b.
    let resp = reqwest::Client::new()
        .get(format!(
            "{base}/native/outcomes?limit=2&before_ms=2000&before_run=run-b"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let runs = body["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["runId"], "run-a");
    assert!(body["nextCursor"].is_null());
    handle.request_shutdown();
}

#[tokio::test]
async fn strict_query_and_page_bounds_are_typed_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, token, _session) =
        outcomes_deps(dir.path(), &[record("run-a", 1_000, false)]).await;
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();
    for url in [
        "/native/outcomes?unknown=1",
        "/native/outcomes?limit=0",
        "/native/outcomes?limit=201",
        "/native/outcomes?before_ms=10",
        "/native/outcomes?before_run=run-a",
        "/native/outcomes?before_ms=10&before_run=",
    ] {
        let resp = client
            .get(format!("{base}{url}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{url} must be a strict-DTO refusal");
    }
    // A hostile cursor run id is a POSITION, not an identity: it still
    // answers the strictly-older page (here: nothing older than ms 0).
    let resp = client
        .get(format!(
            "{base}/native/outcomes?before_ms=0&before_run=does-not-exist"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["runs"].as_array().unwrap().len(), 0);
    handle.request_shutdown();
}
