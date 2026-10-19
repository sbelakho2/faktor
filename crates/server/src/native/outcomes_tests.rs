//! Adversarial route tests of the Engineering Outcome Ledger native read
//! surface (economic audit item 7): auth refusal, empty-ledger parity, exact
//! aggregate fold over durable records, money-key encoding, strict DTOs and
//! bounded cursor paging.

use std::sync::Arc;

use crate::api::tests::test_deps;
use crate::{serve, ServerHandle};
use faktor_agent::{
    ChangedCodeScale, EconomicSettlement, FinalVerificationBasis, MeasuredMoney, MoneyState,
    OutcomeLedger, ProofCoverage, RunModelStep, RunVerifiedSample, SettlementStep,
    TerminalEngineeringOutcome, OUTCOME_RECORD_SCHEMA_V,
};
use faktor_core::model::{ExecutionClass, RiskBucket, RouterPhase, TaskClass};
use faktor_session::SessionManager;

fn step(provider: &str, model: &str, execution_class: ExecutionClass) -> RunModelStep {
    RunModelStep {
        provider: provider.into(),
        model: model.into(),
        local: execution_class.is_local(),
        execution_class,
    }
}

/// One fixture run: a local-zero run (class `local`) or a durably-hosted run
/// (class `hosted_economy`); every step carries a durable execution class,
/// so the frontier metric is measured over the whole fixture.
fn record(
    run_id: &str,
    completed_at_ms: i64,
    local: bool,
) -> (TerminalEngineeringOutcome, EconomicSettlement) {
    let (provider, model, model_spend, class) = if local {
        ("ollama", "local-7b", 0u64, ExecutionClass::Local)
    } else {
        (
            "hosted",
            "m-standard",
            1_000_000u64,
            ExecutionClass::HostedEconomy,
        )
    };
    let outcome = TerminalEngineeringOutcome {
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
        verified_first_verification_pass: true,
        repair_attempts: 0,
        model_sequence: vec![step(provider, model, class)],
        first_execution_class: class,
        final_execution_class: class,
        attribution_state: faktor_agent::AttributionState::Complete,
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
    };
    let settlement = EconomicSettlement {
        schema_version: faktor_agent::SETTLEMENT_SCHEMA_V,
        run_id: run_id.into(),
        seq: 0,
        money_state: MoneyState::Complete,
        reason: "fixture".into(),
        model_spend_micro: Some(model_spend),
        verification_spend_micro: Some(100_000),
        rework_spend_micro: Some(0),
        avoided_hosted: if local {
            MeasuredMoney::Measured(250_000)
        } else {
            MeasuredMoney::Unavailable
        },
        steps: vec![SettlementStep {
            provider: provider.into(),
            model: model.into(),
            local,
            execution_class: class,
            spend_micro: model_spend,
        }],
        settled_at_ms: completed_at_ms,
    };
    (outcome, settlement)
}

async fn outcomes_deps(
    root: &std::path::Path,
    records: &[(TerminalEngineeringOutcome, EconomicSettlement)],
) -> (ServerHandle, String, Arc<SessionManager>) {
    let deps = test_deps(root);
    let token = deps.auth_token.clone();
    let session = deps.session.clone();
    let ledger = OutcomeLedger::new(session.store());
    for (outcome, settlement) in records {
        ledger
            .record(outcome, settlement)
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
    assert_eq!(metrics["verifiedFirstVerificationPassPpm"], 0);
    assert_eq!(metrics["reworkCostRatioPpm"], 0);
    assert_eq!(metrics["successfulRunCostMicro"], "0");
    assert_eq!(metrics["verifiedCostPerSuccessfulChangeMicro"], "0");
    assert_eq!(metrics["localOnlyVerifiedPpm"], 0);
    assert_eq!(metrics["localFinalVerifiedPpm"], 0);
    assert_eq!(metrics["localFirstSuccessPpm"], 0);
    assert_eq!(metrics["localToHostedFallbackPpm"], 0);
    assert_eq!(metrics["hostedOnlyPpm"], 0);
    assert_eq!(metrics["meanTimeToVerifiedChangeMs"], 0);
    assert_eq!(metrics["humanInterventionPpm"], 0);
    assert_eq!(metrics["regressionRollbackPpm"], 0);
    assert_eq!(metrics["proofCoveragePpm"], 0);
    // No records carry execution-class authority: the frontier rate is
    // UNMEASURED (null), never a fabricated 0%.
    assert!(
        metrics["frontierEscalationPpm"].is_null(),
        "an empty ledger has no frontier authority"
    );
    assert_eq!(metrics["frontierEscalationMeasurement"], "unmeasured");
    // Local classification coverage mirrors the frontier rule: an empty
    // ledger has no boundary authority — unmeasured, with a counted cohort.
    assert_eq!(metrics["localClassedRuns"], 0);
    assert_eq!(metrics["localMeasurement"], "unmeasured");
    // Attribution: nothing to attest on an empty ledger.
    assert_eq!(metrics["attributionCompleteRuns"], 0);
    assert_eq!(metrics["attributionPartialRuns"], 0);
    assert_eq!(metrics["attributionUnavailableRuns"], 0);
    assert_eq!(metrics["attributionIncompleteExcluded"], 0);
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
    assert_eq!(metrics["verifiedFirstVerificationPassChanges"], 3);
    assert_eq!(metrics["verifiedFirstVerificationPassPpm"], 1_000_000);
    assert_eq!(metrics["totalVerifiedSpendMicro"], "2300000");
    assert_eq!(metrics["successfulRunCostMicro"], "766667"); // ceil
    assert_eq!(metrics["totalSpendMicro"], "2300000");
    // No failed attempts in this fixture: the all-in VCSC equals the
    // successful-run cost.
    assert_eq!(metrics["verifiedCostPerSuccessfulChangeMicro"], "766667");
    assert_eq!(metrics["totalReworkSpendMicro"], "0");
    assert_eq!(metrics["reworkCostRatioPpm"], 0);
    assert_eq!(metrics["localRuns"], 1);
    assert_eq!(metrics["localOnlyRuns"], 1);
    assert_eq!(metrics["localOnlyVerifiedChanges"], 1);
    assert_eq!(metrics["localOnlyVerifiedPpm"], 1_000_000);
    assert_eq!(metrics["localFinalRuns"], 1);
    assert_eq!(metrics["localFinalVerifiedChanges"], 1);
    assert_eq!(metrics["localFinalVerifiedPpm"], 1_000_000);
    assert_eq!(metrics["localFirstRuns"], 1);
    assert_eq!(metrics["localFirstSuccessChanges"], 1);
    assert_eq!(metrics["localFirstSuccessPpm"], 1_000_000);
    assert_eq!(metrics["localToHostedFallbackRuns"], 0);
    assert_eq!(metrics["localToHostedFallbackPpm"], 0);
    assert_eq!(metrics["hostedOnlyRuns"], 2);
    assert_eq!(metrics["hostedOnlyPpm"], 666_666);
    assert_eq!(metrics["avoidedHostedInferenceMicro"], "250000");
    assert_eq!(metrics["totalWallTimeMs"], 3_000);
    assert_eq!(metrics["meanTimeToVerifiedChangeMs"], 1_000);
    assert_eq!(metrics["humanInterventionPpm"], 0);
    assert_eq!(metrics["regressionRollbackPpm"], 0);
    assert_eq!(metrics["proofCoveragePpm"], 1_000_000);
    // Every fixture step carries a durable class: the frontier rate is a
    // REAL 0% (measured), not an unmeasured null.
    assert_eq!(metrics["frontierEscalationRuns"], 0);
    assert_eq!(metrics["frontierEscalationPpm"], 0);
    assert_eq!(metrics["frontierEscalationMeasurement"], "measured");
    // Local coverage is measured too (both boundary classes on every run),
    // and the attribution is complete for every fixture run.
    assert_eq!(metrics["localClassedRuns"], 3);
    assert_eq!(metrics["localMeasurement"], "measured");
    assert_eq!(metrics["attributionCompleteRuns"], 3);
    assert_eq!(metrics["attributionPartialRuns"], 0);
    assert_eq!(metrics["attributionUnavailableRuns"], 0);
    assert_eq!(metrics["attributionIncompleteExcluded"], 0);
    assert_eq!(metrics["scanned"], 3);
    assert_eq!(metrics["truncated"], false);
    let runs = body["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "bounded page");
    assert_eq!(runs[0]["runId"], "run-c");
    assert_eq!(runs[0]["verified"], true);
    assert_eq!(runs[0]["verifiedFirstVerificationPass"], true);
    assert_eq!(runs[0]["modelSpendMicro"], "1000000");
    assert_eq!(runs[0]["modelSequence"][0]["local"], false);
    assert_eq!(
        runs[0]["modelSequence"][0]["executionClass"],
        "hosted_economy"
    );
    // The exactly-preserved boundary classes and the attribution state ride
    // the run projection (the wire's classification/learning-honesty view).
    assert_eq!(runs[0]["firstExecutionClass"], "hosted_economy");
    assert_eq!(runs[0]["finalExecutionClass"], "hosted_economy");
    assert_eq!(runs[0]["attributionState"], "complete");
    assert_eq!(runs[1]["runId"], "run-b");
    assert_eq!(runs[1]["modelSequence"][0]["local"], true);
    assert_eq!(runs[1]["modelSequence"][0]["executionClass"], "local");
    assert_eq!(runs[1]["firstExecutionClass"], "local");
    assert_eq!(runs[1]["finalExecutionClass"], "local");
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

/// Legacy wire semantics (item 2): the schema says `local` is CLASS-DERIVED
/// (true only for `executionClass == "local"`), so a pre-class row that
/// recorded a local execution (class `unknown`, `local: true`) must NOT
/// promote the recorded bit to `local` on the wire — it rides the explicit
/// `legacyLocal` marker instead, and the metrics count it as neither local
/// nor hosted.
#[tokio::test]
async fn classless_legacy_local_step_is_never_promoted_on_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let (mut outcome, mut settlement) = record("run-legacy", 1_000, true);
    // The stored pre-class Row: class unknown (decoded as the serde default
    // on a payload that predates the field) with the recorded local bit.
    outcome.model_sequence = vec![RunModelStep {
        provider: "ollama".into(),
        model: "local-legacy".into(),
        local: true,
        execution_class: ExecutionClass::Unknown,
    }];
    // A pre-class payload predates the boundary-class fields too: both
    // boundaries decode `unknown` — the recorded local bit is never
    // promoted to a class anywhere on the wire.
    outcome.first_execution_class = ExecutionClass::Unknown;
    outcome.final_execution_class = ExecutionClass::Unknown;
    settlement.steps = vec![SettlementStep {
        provider: "ollama".into(),
        model: "local-legacy".into(),
        local: true,
        execution_class: ExecutionClass::Unknown,
        spend_micro: 0,
    }];
    settlement.model_spend_micro = Some(0);
    let (handle, token, _session) = outcomes_deps(dir.path(), &[(outcome, settlement)]).await;
    let base = format!("http://{}", handle.addr);
    let resp = reqwest::Client::new()
        .get(format!("{base}/native/outcomes"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let step = &body["runs"][0]["modelSequence"][0];
    assert_eq!(step["executionClass"], "unknown");
    assert_eq!(
        step["local"], false,
        "an unknown-class step is never wire-local: {step}"
    );
    assert_eq!(
        step["legacyLocal"], true,
        "the recorded pre-class local bit rides the legacy marker: {step}"
    );
    // A pre-class payload carries no boundary-class authority either.
    assert_eq!(body["runs"][0]["firstExecutionClass"], "unknown");
    assert_eq!(body["runs"][0]["finalExecutionClass"], "unknown");
    // The metrics classify by the durable class only: the run is neither
    // local nor hosted, and it still counts toward the runs-with-steps
    // denominator (it has a step).
    let metrics = &body["metrics"];
    assert_eq!(metrics["localRuns"], 0);
    assert_eq!(metrics["localOnlyRuns"], 0);
    assert_eq!(metrics["hostedOnlyRuns"], 0);
    assert_eq!(metrics["runsWithSteps"], 1);
    handle.request_shutdown();
}

/// Economic audit item 7 migration parity: the live wire keys AND JSON kinds
/// of `GET /native/outcomes` mirror the canonical schema types
/// field-for-field (the schema entries are what the generated IDE clients
/// parse; a drift is a generated-client bug). Adversarial directions: a
/// schema-only field, a wire-only field, a wrong kind and a wrong
/// nullability all fail here.
#[tokio::test]
async fn outcomes_wire_shape_matches_the_canonical_schema() {
    use std::collections::BTreeSet;

    use faktor_protocol::schema::{canonical, Shape, Ty, TypeDef};

    fn schema_def<'a>(types: &'a [TypeDef], name: &str) -> &'a TypeDef {
        types
            .iter()
            .find(|def| def.name == name)
            .unwrap_or_else(|| panic!("{name} must be in the canonical schema"))
    }

    fn wire_keys(value: &serde_json::Value, path: &str) -> BTreeSet<String> {
        value
            .as_object()
            .unwrap_or_else(|| panic!("{path}: expected a JSON object"))
            .keys()
            .cloned()
            .collect()
    }

    /// Structural parity of one schema struct against one live wire value.
    fn assert_shape(types: &[TypeDef], name: &str, value: &serde_json::Value, path: &str) {
        let def = schema_def(types, name);
        let Shape::Struct { fields } = &def.shape else {
            panic!("{name} must be a struct");
        };
        let wire = wire_keys(value, path);
        let schema: BTreeSet<String> = fields.iter().map(|f| f.name.to_string()).collect();
        assert_eq!(
            wire, schema,
            "{path}: wire keys must match schema type {name}"
        );
        for field in fields {
            let entry = &value[field.name];
            let field_path = format!("{path}.{}", field.name);
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
                    if let Ty::Named { name } = of.as_ref() {
                        for (index, item) in items.iter().enumerate() {
                            assert_shape(types, name, item, &format!("{field_path}[{index}]"));
                        }
                    }
                }
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let records = vec![record("run-a", 1_000, false), record("run-b", 2_000, true)];
    let (handle, token, _session) = outcomes_deps(dir.path(), &records).await;
    let base = format!("http://{}", handle.addr);
    let resp = reqwest::Client::new()
        .get(format!("{base}/native/outcomes?limit=1"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["nextCursor"].is_object(),
        "the fixture needs a full page with a non-null cursor"
    );
    let schema = canonical();
    assert_shape(&schema.types, "OutcomesPage", &body, "GET /native/outcomes");
    handle.request_shutdown();
}

/// The metrics fold cache is keyed by the ledger's monotonic revision: a
/// poll between writes is free, a new terminal run/correction invalidates
/// it, and the returned report reflects the post-write ledger.
#[tokio::test]
async fn outcomes_metrics_cache_invalidates_on_ledger_writes() {
    use super::OutcomesMetricsCache;
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let store = deps.session.store();
    let ledger = OutcomeLedger::new(store.clone());
    let cache = OutcomesMetricsCache::default();

    let empty = cache.report(&store, &ledger).await.unwrap();
    assert_eq!(empty.metrics.runs, 0);
    assert_eq!(empty.scanned, 0);

    let (outcome, settlement) = record("run-cache", 1_000, false);
    ledger.record(&outcome, &settlement).unwrap();

    let after = cache.report(&store, &ledger).await.unwrap();
    assert_eq!(after.metrics.runs, 1);
    assert_eq!(after.scanned, 1);
    let again = cache.report(&store, &ledger).await.unwrap();
    assert_eq!(again, after, "a cache hit returns the same fold");

    // Polling ceiling: 200 reads between writes do no ledger work.
    let started = std::time::Instant::now();
    for _ in 0..200 {
        let hit = cache.report(&store, &ledger).await.unwrap();
        assert_eq!(hit, after);
    }
    let hits = started.elapsed();
    println!("[perf] 200 cached outcomes reads: {hits:?}");
    assert!(
        hits < std::time::Duration::from_millis(100),
        "cached outcomes reads took {hits:?} for 200 polls"
    );
}
