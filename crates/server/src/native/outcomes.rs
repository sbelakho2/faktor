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
    /// The paid ORGANIZATION-scope read (`outcome_analytics`): with `org`
    /// given, the caller's control-plane principal must be a member of that
    /// organization AND the organization's plan must map
    /// `outcome_analytics`. A denial is the typed 403
    /// `value_entitlement_refused` naming the entitlement, its hook, the
    /// cause and the resolved plan; no billing authority wired = the shared
    /// typed 409 `billing_disabled`. Without `org` the route keeps its
    /// frozen local shape and never consults the gate.
    #[serde(default)]
    org: Option<String>,
}

/// The organization-scope authorization of one outcome read (the
/// `outcome_analytics` paid boundary): the principal must reach `org`
/// (Billing read, viewer+), a billing authority must be wired, and the
/// plan must map the entitlement. `Ok(())` continues into the same bounded
/// fold + page read; a denial is typed. The denial IS an HTTP response, so
/// the error stays inline here (the response is the point, not a payload).
#[allow(clippy::result_large_err)]
fn authorize_outcomes_organization(
    state: &AppState,
    headers: &HeaderMap,
    raw_org: &str,
) -> Result<(), Response> {
    let principal = match require_principal(state, headers) {
        Ok(principal) => principal,
        Err(e) => return Err(wire_status(e)),
    };
    let organization = match faktor_cloud::OrganizationId::try_new(raw_org.to_string()) {
        Ok(organization) => organization,
        Err(_) => {
            return Err(wire_status(malformed_body(
                "organization must be a bounded printable id",
            )))
        }
    };
    let Some(service) = state.deps.billing.as_ref() else {
        return Err(wire_status(billing_disabled()));
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return Err(wire_status(billing_err(denied.into())));
    }
    match service.check_value_entitlement(
        &organization,
        faktor_cloud::ValueEntitlement::OutcomeAnalytics,
    ) {
        Ok(()) => Ok(()),
        Err(refusal) => Err(wire_status(value_entitlement_refused(refusal))),
    }
}

/// Map one typed value-entitlement refusal onto the frozen API error: a
/// non-retryable 403 whose message names the entitlement, its hook, the
/// exact cause and (when resolved) the plan — never an empty result.
pub(crate) fn value_entitlement_refused(
    refusal: faktor_cloud::ValueEntitlementRefused,
) -> ApiError {
    ApiError {
        code: "value_entitlement_refused",
        message: refusal.to_string(),
        http_status: 403,
        retryable: false,
    }
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

/// The wire projection of one metrics fold. Zero is only for a MEASURED
/// zero: `avoidedHostedInferenceMicro` is null (never `"0"`) when no run
/// carried a measured counterfactual, and its `*Measurement` token says so.
/// The object is assembled from three bounded literals so the `json!`
/// macro's expansion stays under the crate recursion limit.
fn metrics_json(report: &faktor_agent::OutcomeLedgerMetricsReport) -> serde_json::Value {
    let m = &report.metrics;
    let avoided_measurement = if m.avoided_hosted_inference_micro.is_some() {
        "measured"
    } else {
        "unavailable"
    };
    let mut out = serde_json::json!({
        "runs": m.runs,
        "verifiedChanges": m.verified_changes,
        "verifiedFirstVerificationPassChanges": m.verified_first_verification_pass_changes,
        "verifiedFirstVerificationPassPpm": m.verified_first_verification_pass_ppm,
        "moneyCompleteRuns": m.money_complete_runs,
        "moneyPendingRuns": m.money_pending_runs,
        "moneyUnavailableRuns": m.money_unavailable_runs,
        "moneyIncompleteExcluded": m.money_incomplete_excluded,
        "verifiedChangesMoneyComplete": m.verified_changes_money_complete,
        "totalVerifiedSpendMicro": m.total_verified_spend_micro,
        "successfulRunCostMicro": m.successful_run_cost_micro,
        "totalSpendMicro": m.total_spend_micro,
        "verifiedCostPerSuccessfulChangeMicro": m.verified_cost_per_successful_change_micro,
        "totalReworkSpendMicro": m.total_rework_spend_micro,
        "reworkCostRatioPpm": m.rework_cost_ratio_ppm,
    });
    let tail = serde_json::json!({
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
        "localToHostedFallbackRuns": m.local_to_hosted_fallback_runs,
        "localToHostedFallbackPpm": m.local_to_hosted_fallback_ppm,
        "hostedOnlyRuns": m.hosted_only_runs,
        "runsWithSteps": m.runs_with_steps,
        "hostedOnlyPpm": m.hosted_only_ppm,
    });
    let frontier = serde_json::json!({
        "avoidedHostedInferenceMicro": m.avoided_hosted_inference_micro.map(|v| v.to_string()),
        "avoidedHostedInferenceMeasurement": avoided_measurement,
        "avoidedHostedMeasuredRuns": m.avoided_hosted_measured_runs,
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
        "frontierClassedRuns": m.frontier_classed_runs,
        "frontierEscalationClassedRuns": m.frontier_escalation_classed_runs,
        "frontierEscalationPpm": m.frontier_escalation_ppm,
        "frontierEscalationMeasurement": m.frontier_measurement.as_str(),
        "scanned": report.scanned,
        "truncated": report.truncated,
    });
    let object = out.as_object_mut().expect("json object literal");
    for (key, value) in tail.as_object().into_iter().flatten() {
        object.insert(key.clone(), value.clone());
    }
    for (key, value) in frontier.as_object().into_iter().flatten() {
        object.insert(key.clone(), value.clone());
    }
    out
}

/// The wire projection of one run entry: the immutable engineering outcome
/// plus the LATEST settled financial projection. Money keys ride `*Micro`
/// and are decimal strings — or NULL when the money is not settled (only a
/// `complete` projection carries amounts; zero is only a measured zero).
/// Every `modelSequence` step carries the CLASS-DERIVED `local` (true only
/// when `executionClass` is `local`) plus the documented `legacyLocal`
/// marker: a classless step (pre-class payload or unpriced path) exposes its
/// recorded legacy local bit there — the legacy bit is never promoted to a
/// class claim.
fn run_json(entry: &faktor_agent::OutcomeLedgerEntry) -> serde_json::Value {
    use faktor_agent::MoneyState;
    let record = &entry.outcome;
    let settlement = &entry.settlement;
    let complete = settlement.money_state == MoneyState::Complete;
    // Complete projections expose the settled per-step spend; a
    // not-yet-settled run exposes the engineering step identity with a null
    // spend (the money is not final, never a fabricated zero).
    let model_sequence: Vec<serde_json::Value> = if complete && !settlement.steps.is_empty() {
        settlement
            .steps
            .iter()
            .map(|step| {
                serde_json::json!({
                    "provider": step.provider,
                    "model": step.model,
                    // The wire `local` is CLASS-DERIVED (true only for a
                    // durably local step); a pre-class recorded local bit
                    // rides the documented `legacyLocal` marker instead of
                    // contradicting the schema's class-only claim.
                    "local": step.execution_class.is_local(),
                    "legacyLocal": !step.execution_class.is_known() && step.local,
                    "executionClass": step.execution_class.as_str(),
                    "spendMicro": step.spend_micro.to_string(),
                })
            })
            .collect()
    } else {
        record
            .model_sequence
            .iter()
            .map(|step| {
                serde_json::json!({
                    "provider": step.provider,
                    "model": step.model,
                    // Class-derived locality + the legacy marker (see the
                    // settled branch above).
                    "local": step.wire_local(),
                    "legacyLocal": step.legacy_local(),
                    "executionClass": step.execution_class.as_str(),
                    "spendMicro": serde_json::Value::Null,
                })
            })
            .collect()
    };
    let money = |value: Option<u64>| -> Option<String> {
        if complete {
            value.map(|v| v.to_string())
        } else {
            None
        }
    };
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
        "verifiedFirstVerificationPass": record.verified_first_verification_pass,
        "repairAttempts": record.repair_attempts,
        "modelSequence": model_sequence,
        "moneyState": settlement.money_state.as_str(),
        "modelSpendMicro": money(settlement.model_spend_micro),
        "verificationSpendMicro": money(settlement.verification_spend_micro),
        "reworkSpendMicro": money(settlement.rework_spend_micro),
        "avoidedHostedMicro": settlement.avoided_hosted.value().map(|v| v.to_string()),
        "avoidedHostedMeasurement": settlement.avoided_hosted.measurement_str(),
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
    // The org-scope read is the paid `outcome_analytics` boundary; the
    // org-less local read below is never gated.
    if let Some(raw_org) = query.org.as_deref() {
        if let Err(response) = authorize_outcomes_organization(&state, &headers, raw_org) {
            return response;
        }
    }
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
    let runs: Vec<serde_json::Value> = page.entries.iter().map(run_json).collect();
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
