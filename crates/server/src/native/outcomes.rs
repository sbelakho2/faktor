//! The Engineering Outcome Ledger native read surface (economic audit item
//! 7, additive): `GET /native/outcomes` — the durable completed-run ledger's
//! first-class metrics aggregates plus one bounded newest-first page of run
//! records.
//!
//! Pure durable read: the metrics fold parses every stored record loudly
//! (payload schema + projection cross-checks); a corrupt or drifted row is a
//! typed 500 `internal`, never a fabricated number. Money fields are
//! decimal strings on the wire (`*Micro`); rates are integer ppm.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::*;

/// Strict query of `/native/outcomes`: an unknown key is a 400. The
/// `before_ms`/`before_run` pair is the composite cursor of the previous
/// page (both or neither); `limit` is validated by the shared native page
/// bound (oversized limits are rejected, never silently clamped).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeOutcomesQuery {
    #[serde(default)]
    before_ms: Option<i64>,
    #[serde(default)]
    before_run: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

/// Map one typed ledger failure onto the wire: durable store failures keep
/// their store taxonomy; corrupt/invalid rows are loud 500s.
fn ledger_api_err(e: faktor_agent::OutcomeLedgerError) -> Response {
    match e {
        faktor_agent::OutcomeLedgerError::Store(inner) => api_err(&store_err_to_core(inner)),
        other => wire_status(internal_graph_err(format!(
            "engineering outcome ledger: {other}"
        ))),
    }
}

/// The wire projection of one metrics fold.
fn metrics_json(report: &faktor_agent::OutcomeLedgerMetricsReport) -> serde_json::Value {
    let m = &report.metrics;
    serde_json::json!({
        "runs": m.runs,
        "verifiedChanges": m.verified_changes,
        "verifiedFirstPassChanges": m.verified_first_pass_changes,
        "verifiedFirstPassPpm": m.verified_first_pass_ppm,
        "totalVerifiedSpendMicro": m.total_verified_spend_micro,
        "successfulRunCostMicro": m.successful_run_cost_micro,
        "totalSpendMicro": m.total_spend_micro,
        "verifiedCostPerSuccessfulChangeMicro": m.verified_cost_per_successful_change_micro,
        "totalReworkSpendMicro": m.total_rework_spend_micro,
        "reworkCostRatioPpm": m.rework_cost_ratio_ppm,
        "localRuns": m.local_runs,
        "localOnlyRuns": m.local_only_runs,
        "localOnlyVerifiedChanges": m.local_only_verified_changes,
        "localOnlyVerifiedPpm": m.local_only_verified_ppm,
        "localFinalRuns": m.local_final_runs,
        "localFinalVerifiedChanges": m.local_final_verified_changes,
        "localFinalVerifiedPpm": m.local_final_verified_ppm,
        "localFirstRuns": m.local_first_runs,
        "localFirstSuccessChanges": m.local_first_success_changes,
        "localFirstSuccessPpm": m.local_first_success_ppm,
        "localToFrontierFallbackRuns": m.local_to_frontier_fallback_runs,
        "localToFrontierFallbackPpm": m.local_to_frontier_fallback_ppm,
        "frontierOnlyRuns": m.frontier_only_runs,
        "frontierOnlyPpm": m.frontier_only_ppm,
        "avoidedHostedInferenceMicro": m.avoided_hosted_inference_micro,
        "totalWallTimeMs": m.total_wall_time_ms,
        "meanTimeToVerifiedChangeMs": m.mean_time_to_verified_change_ms,
        "humanInterventionRuns": m.human_intervention_runs,
        "humanInterventionPpm": m.human_intervention_ppm,
        "rollbackRuns": m.rollback_runs,
        "regressionRollbackPpm": m.regression_rollback_ppm,
        "checksPassed": m.checks_passed,
        "checksRequired": m.checks_required,
        "proofCoveragePpm": m.proof_coverage_ppm,
        "frontierEscalationRuns": m.frontier_escalation_runs,
        "frontierEscalationPpm": m.frontier_escalation_ppm,
        "scanned": report.scanned,
        "truncated": report.truncated,
    })
}

/// The wire projection of one run record (money keys ride `*Micro`).
fn run_json(record: &faktor_agent::RunOutcomeRecord) -> serde_json::Value {
    let model_sequence: Vec<serde_json::Value> = record
        .model_sequence
        .iter()
        .map(|step| {
            serde_json::json!({
                "provider": step.provider,
                "model": step.model,
                "local": step.local,
                "spendMicro": step.spend_micro,
            })
        })
        .collect();
    serde_json::json!({
        "runId": record.run_id,
        "sessionId": record.session_id,
        "taskId": record.task_id,
        "taskClass": serde_json::to_value(record.task_class).unwrap_or_default(),
        "riskClass": serde_json::to_value(record.risk_class).unwrap_or_default(),
        "candidateSnapshot": record.candidate_snapshot,
        "finalSnapshot": record.final_snapshot,
        "accepted": record.accepted,
        "integrated": record.integrated,
        "verified": record.verified(),
        "verifiedFirstAttempt": record.verified_first_attempt,
        "repairAttempts": record.repair_attempts,
        "modelSequence": model_sequence,
        "modelSpendMicro": record.model_spend_micro,
        "verificationSpendMicro": record.verification_spend_micro,
        "reworkSpendMicro": record.rework_spend_micro,
        "avoidedHostedMicro": record.avoided_hosted_micro,
        "wallTimeMs": record.wall_time_ms,
        "humanInterventions": record.human_interventions,
        "proof": {
            "checksPassed": record.proof.checks_passed,
            "checksRequired": record.proof.checks_required,
            "coveragePpm": record.proof.coverage_ppm(),
        },
        "changeScale": {
            "files": record.change_scale.files,
            "addedLines": record.change_scale.added_lines,
            "removedLines": record.change_scale.removed_lines,
        },
        "rollbackEvents": record.rollback_events,
        "recoveryEvents": record.recovery_events,
        "finalBasis": record.final_basis.as_str(),
        "frontierEscalations": record.frontier_escalations,
        "completedAtMs": record.completed_at_ms,
    })
}

/// `GET /native/outcomes[?limit=<n>&before_ms=<ms>&before_run=<id>]` — the
/// Engineering Outcome Ledger's durable aggregates (the first-class metrics,
/// including Successful-Run Cost and the all-in VCSC, plus the fold's scan
/// honesty) and one bounded newest-first page
/// of runs. Pure read; auth-gated; strict DTO; oversized/zero limits and
/// half-given cursors are 400s.
pub(crate) async fn native_outcomes(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeOutcomesQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let limit = match page_limit(query.limit, MAX_NATIVE_CURSOR_PAGE) {
        Ok(limit) => limit,
        Err(e) => return wire_status(e),
    };
    let cursor = match (query.before_ms, query.before_run) {
        (None, None) => None,
        (Some(at_ms), Some(run_id)) if !run_id.is_empty() => Some(faktor_agent::OutcomeCursor {
            completed_at_ms: at_ms,
            run_id,
        }),
        _ => {
            return wire_status(malformed_body(
                "before_ms and a non-empty before_run must be given together",
            ));
        }
    };
    let page_limit_rows = match u64::try_from(limit) {
        Ok(rows) => rows,
        Err(_) => return wire_status(malformed_body("invalid outcomes page bound")),
    };
    let ledger = faktor_agent::OutcomeLedger::new(state.deps.session.store());
    let metrics = match ledger.metrics() {
        Ok(report) => report,
        Err(e) => return ledger_api_err(e),
    };
    let page = match ledger.page(cursor.as_ref(), page_limit_rows) {
        Ok(page) => page,
        Err(e) => return ledger_api_err(e),
    };
    let runs: Vec<serde_json::Value> = page.records.iter().map(run_json).collect();
    let next_cursor = page.next.as_ref().map(|next| {
        serde_json::json!({
            "completedAtMs": next.completed_at_ms,
            "runId": next.run_id,
        })
    });
    money_json(serde_json::json!({
        "metrics": metrics_json(&metrics),
        "runs": runs,
        "nextCursor": next_cursor,
    }))
}

#[cfg(test)]
#[path = "outcomes_tests.rs"]
mod outcomes_tests;
