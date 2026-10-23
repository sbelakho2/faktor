//! Authoritative usage reads: per-session and cross-session aggregates.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_core::id::SessionId;

use super::*;
use crate::api::AppState;
use faktor_cloud::money;

/// The strict query of `/native/usage`. Without `org` the route keeps its
/// frozen pre-billing shape (the durable cross-session aggregate below);
/// with `org` it answers the Wave 3 per-organization usage fold — see
/// [`billing_usage`]. Unknown query keys are a 400 (a typo never silently
/// selects the legacy branch).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeUsageQuery {
    org: Option<String>,
    since: Option<String>,
    limit: Option<u64>,
}

/// `GET /native/usage` — cross-session usage aggregate (legacy shape) or,
/// with `?org=`, the organization's commercial usage fold.
///
/// Two documented layers:
///
/// - `totals.budget|spent` + `perSession` keep the FROZEN legacy view over
///   the memory facts of kind `usage` (keys `budget`/`spent`). No runtime
///   path writes those facts — they exist for simulators/tooling only — so
///   the totals are honest zeros when nothing recorded them.
/// - `durable` is the AUTHORITATIVE aggregate over the persisted rows:
///   every `provider_call` row of every session (tokens = the persisted
///   `tokens_in`+`tokens_out` totals — cache reads/writes and reasoning are
///   folded into these two counters by the usage settlement BEFORE
///   persistence, so the rows carry exactly what is summed here), the
///   per-row prefix observations, and every `cost_reservation` row of every
///   typed task (in-flight reserved/dispatched + settled/refunded/uncertain
///   with the folded spends and the provider-reported micro totals). Hostile
///   rows can never break the
///   aggregate: unparseable reservation JSON is skipped per row, sessions
///   are read defensively, and the reservation scan is bounded — when the
///   per-task scan cap is hit the response says `truncated: true` instead of
///   pretending to be exact.
///
/// The `?org=` branch (Wave 3, additive) is org-isolated: the caller's
/// control-plane principal fixes the organization.
pub(crate) async fn native_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeUsageQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    if let Some(org) = query.org {
        return billing_usage(state, headers, org, query.since, query.limit);
    }
    // Once the commercial billing plane is wired, the org-less legacy alias
    // would serve whole-daemon usage without the tenant/plan gate; require
    // the explicit organization scope instead (the pre-billing daemon keeps
    // its frozen shape byte-identically).
    if state.deps.billing.is_some() {
        return wire_status(org_required());
    }
    const MAX_SESSIONS: usize = 10_000;
    let mut budget_total: i64 = 0;
    let mut spent_total: i64 = 0;
    let mut per_session: Vec<serde_json::Value> = Vec::new();
    // A session that vanishes mid-list is skipped, never fatal (the wire
    // list has the same convention).
    let mut sessions = match state.deps.session.list_sessions(None) {
        Ok(s) => s,
        Err(e) => return api_err(&e),
    };
    sessions.truncate(MAX_SESSIONS);
    let store = state.deps.session.store();
    let session_ids: Vec<SessionId> = sessions.iter().map(|h| h.id()).collect();
    // ---- durable layer (authoritative; P0-63), BATCH reads (perf wave):
    // one statement per bounded session/task chunk replaces the per-session
    // and per-task round-trips. Byte-identical DTO: every number comes from
    // the same SQL expression as the single-session read, and anything the
    // batch cannot serve EXACTLY (a shape the per-row validated read would
    // reject, task rows beyond the scan scope, or a chunk-level SQL
    // failure) falls back to the original per-session/per-task reads below.
    let usage_batch = store
        .session_usage_aggregates(&session_ids)
        .unwrap_or_else(|_| faktor_store::SessionUsageBatch {
            unreadable: session_ids.clone(),
            ..Default::default()
        });
    let usage_by_session: std::collections::HashMap<u64, &faktor_store::SessionUsageAggregate> =
        usage_batch
            .rows
            .iter()
            .map(|a| (a.session_id.raw(), a))
            .collect();
    let usage_unreadable: std::collections::HashSet<u64> =
        usage_batch.unreadable.iter().map(|s| s.raw()).collect();
    let tasks_batch = store
        .task_usage_scope(&session_ids, MAX_NATIVE_LIST as u32)
        .unwrap_or_else(|_| faktor_store::TaskUsageBatch {
            unreadable: session_ids.clone(),
            ..Default::default()
        });
    let mut tasks_by_session: std::collections::HashMap<u64, Vec<faktor_store::TaskUsageRow>> =
        std::collections::HashMap::new();
    for row in tasks_batch.rows {
        tasks_by_session
            .entry(row.session_id.raw())
            .or_default()
            .push(row);
    }
    let tasks_unreadable: std::collections::HashSet<u64> =
        tasks_batch.unreadable.iter().map(|s| s.raw()).collect();
    // The reservation batch covers exactly the `(session, task)` pairs the
    // route reads — the servable scope rows of the servable sessions.
    let pairs: Vec<(SessionId, faktor_core::id::TaskId)> = tasks_by_session
        .iter()
        .filter(|(sid, _)| !tasks_unreadable.contains(sid))
        .flat_map(|(_, rows)| rows.iter().map(|r| (r.session_id, r.task_id)))
        .collect();
    let reservations_batch = store
        .cost_reservations_for_tasks(&pairs, MAX_NATIVE_RESERVATIONS_SCAN)
        .unwrap_or_else(|_| faktor_store::TaskReservationBatch {
            unreadable: pairs.clone(),
            ..Default::default()
        });
    let mut reservations_by_pair: std::collections::HashMap<
        (u64, u64),
        Vec<faktor_store::CostReservationRow>,
    > = std::collections::HashMap::new();
    for row in reservations_batch.rows {
        reservations_by_pair
            .entry((row.session_id.raw(), row.task_id.raw()))
            .or_default()
            .push(row);
    }
    let reservations_unreadable: std::collections::HashSet<(u64, u64)> = reservations_batch
        .unreadable
        .iter()
        .map(|(s, t)| (s.raw(), t.raw()))
        .collect();
    let mut sessions_with_calls: u64 = 0;
    let mut prefix_rows: u64 = 0;
    let mut prefix_tokens: u64 = 0;
    let mut prefix_stability_observations: u64 = 0;
    let mut durable_tokens: u64 = 0;
    let mut res = ReservationFold::default();
    let mut task_budget_spent_micro: u64 = 0;
    for handle in &sessions {
        let sid = handle.id();
        let servable = usage_by_session
            .get(&sid.raw())
            .filter(|_| !usage_unreadable.contains(&sid.raw()));
        let session_tokens = match servable {
            Some(agg) => agg.tokens,
            None => match store.session_usage_tokens(sid) {
                Ok(t) => t,
                Err(_) => continue, // vanished/corrupt session row mid-scan
            },
        };
        if session_tokens > 0 {
            sessions_with_calls += 1;
        }
        durable_tokens = durable_tokens.saturating_add(session_tokens);
        match servable.filter(|a| a.prefix_servable) {
            Some(agg) => {
                prefix_rows = prefix_rows.saturating_add(agg.prefix_rows);
                prefix_tokens = prefix_tokens.saturating_add(agg.prefix_tokens);
                prefix_stability_observations += agg.prefix_stability_observations;
            }
            None => match store.provider_call_prefix_rows(sid) {
                Ok(rows) => {
                    prefix_rows = prefix_rows.saturating_add(rows.len() as u64);
                    for r in rows {
                        prefix_tokens = prefix_tokens.saturating_add(r.prompt_tokens as u64);
                        if r.prefix_stability.is_some() {
                            prefix_stability_observations += 1;
                        }
                    }
                }
                Err(_) => continue, // a hostile prefix row fails this session's read loudly
            },
        }
        if let Some(scope) = tasks_by_session
            .get(&sid.raw())
            .filter(|_| !tasks_unreadable.contains(&sid.raw()))
        {
            for task in scope {
                task_budget_spent_micro = task_budget_spent_micro
                    .saturating_add(u64::try_from(task.spent_cost_micro).unwrap_or(u64::MAX));
                // Batch rows for the pair; a pair the batch could not serve
                // falls back to the single-task read (which reproduces the
                // legacy skip), an absent pair has no rows.
                let fallback_rows;
                let rows: &[faktor_store::CostReservationRow] = if let Some(rows) =
                    reservations_by_pair.get(&(task.session_id.raw(), task.task_id.raw()))
                {
                    rows
                } else if reservations_unreadable
                    .contains(&(task.session_id.raw(), task.task_id.raw()))
                {
                    match store.cost_reservations_of(
                        task.session_id,
                        task.task_id,
                        MAX_NATIVE_RESERVATIONS_SCAN,
                    ) {
                        Ok(rows) => {
                            fallback_rows = rows;
                            &fallback_rows
                        }
                        Err(_) => continue,
                    }
                } else {
                    &[]
                };
                if rows.len() as i64 >= MAX_NATIVE_RESERVATIONS_SCAN {
                    res.truncated = true;
                }
                res.fold(rows);
            }
        } else {
            let tasks = match handle.list_tasks() {
                Ok(t) => t,
                Err(_) => continue,
            };
            for task in tasks.iter().take(MAX_NATIVE_LIST) {
                if let Ok(Some(cost)) = store.cost_task_row(sid, task.task_id) {
                    task_budget_spent_micro =
                        task_budget_spent_micro.saturating_add(cost.spent_cost_micro);
                }
                let rows = match store.cost_reservations_of(
                    sid,
                    task.task_id,
                    MAX_NATIVE_RESERVATIONS_SCAN,
                ) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if rows.len() as i64 >= MAX_NATIVE_RESERVATIONS_SCAN {
                    res.truncated = true;
                }
                res.fold(&rows);
            }
        }
    }
    let mut durable = serde_json::json!({
        "sessionsWithCalls": sessions_with_calls,
        "providerCalls": {
            "tokens": durable_tokens,
            "prefixObservations": prefix_rows,
            "prefixTokens": prefix_tokens,
            "prefixStabilityObservations": prefix_stability_observations,
        },
        "taskSpend": { "settledCostMicro": money::json(task_budget_spent_micro) },
        "reservations": {
            "open": { "count": res.open_count, "predictedMicro": money::json(res.open_predicted) },
            "settled": {
                "count": res.settled_count,
                "predictedMicro": money::json(res.settled_predicted),
                "spentMicro": money::json(res.settled_spent),
                "providerReportedMicro": money::json(res.settled_reported),
            },
            "refunded": { "count": res.refunded_count, "predictedMicro": money::json(res.refunded_predicted) },
            "uncertain": {
                "count": res.uncertain_count,
                "predictedMicro": money::json(res.uncertain_predicted),
            },
        },
    });
    if res.truncated {
        durable["truncated"] = serde_json::json!(true);
    }
    // ---- legacy per-session facts (kind `usage`): batch read with the same
    // per-session last-wins fold; chunks the SQL layer could not serve fall
    // back to the per-session fact read.
    let facts_batch = store
        .usage_facts_by_session(&session_ids)
        .unwrap_or_else(|_| faktor_store::UsageFactsBatch {
            unreadable: session_ids.clone(),
            ..Default::default()
        });
    let mut facts_by_session: std::collections::HashMap<u64, (Option<i64>, Option<i64>)> =
        std::collections::HashMap::new();
    for (sid, key, value) in &facts_batch.rows {
        let entry = facts_by_session.entry(sid.raw()).or_default();
        match key.as_str() {
            "budget" => entry.0 = value.parse::<i64>().ok(),
            "spent" => entry.1 = value.parse::<i64>().ok(),
            _ => {}
        }
    }
    let facts_unreadable: std::collections::HashSet<u64> =
        facts_batch.unreadable.iter().map(|s| s.raw()).collect();
    for handle in &sessions {
        let sid = handle.id();
        let (budget, spent) = if facts_unreadable.contains(&sid.raw()) {
            let facts = match handle.memory_facts() {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut budget: Option<i64> = None;
            let mut spent: Option<i64> = None;
            for (kind, key, value) in facts {
                if kind != "usage" {
                    continue;
                }
                let parsed = value.parse::<i64>().ok();
                match key.as_str() {
                    "budget" => budget = parsed,
                    "spent" => spent = parsed,
                    _ => {}
                }
            }
            (budget, spent)
        } else {
            facts_by_session
                .get(&sid.raw())
                .copied()
                .unwrap_or((None, None))
        };
        if budget.is_none() && spent.is_none() {
            continue;
        }
        budget_total = budget_total.saturating_add(budget.unwrap_or(0));
        spent_total = spent_total.saturating_add(spent.unwrap_or(0));
        per_session.push(serde_json::json!({
            "sessionId": handle.id().to_string(),
            "budget": budget,
            "spent": spent,
        }));
    }
    money_json(serde_json::json!({
        "sessions": sessions.len(),
        "totals": { "budget": budget_total, "spent": spent_total },
        "perSession": per_session,
        "durable": durable,
    }))
}

/// The durable reservation counters of the cross-session aggregate, folded
/// from the SAME status vocabulary and saturating arithmetic as before (the
/// batch and single-task reads share this exact fold).
#[derive(Default)]
struct ReservationFold {
    open_count: u64,
    open_predicted: u64,
    settled_count: u64,
    settled_predicted: u64,
    settled_spent: u64,
    settled_reported: u64,
    refunded_count: u64,
    refunded_predicted: u64,
    uncertain_count: u64,
    uncertain_predicted: u64,
    truncated: bool,
}

impl ReservationFold {
    fn fold(&mut self, rows: &[faktor_store::CostReservationRow]) {
        for row in rows {
            // Group vocabulary is the schema v17+ state set: in-flight
            // `reserved`/`dispatched`, terminal `settled`/`refunded`
            // and crash-recovery `uncertain` — the v15 `open`/
            // `abandoned` names were renamed at v16/v17 and never
            // occur in a migrated store.
            match row.status.as_str() {
                "reserved" | "dispatched" => {
                    self.open_count += 1;
                    self.open_predicted = self.open_predicted.saturating_add(row.predicted_micro);
                }
                "settled" => {
                    self.settled_count += 1;
                    self.settled_predicted =
                        self.settled_predicted.saturating_add(row.predicted_micro);
                    self.settled_spent = self
                        .settled_spent
                        .saturating_add(reservation_settled_spent_micro(row));
                    self.settled_reported = self
                        .settled_reported
                        .saturating_add(reservation_provider_reported_micro(row));
                }
                "refunded" => {
                    self.refunded_count += 1;
                    self.refunded_predicted =
                        self.refunded_predicted.saturating_add(row.predicted_micro);
                }
                "uncertain" => {
                    self.uncertain_count += 1;
                    self.uncertain_predicted =
                        self.uncertain_predicted.saturating_add(row.predicted_micro);
                }
                // Unknown status strings (hostile rows) contribute
                // nothing: the aggregate never guesses.
                _ => {}
            }
        }
    }
}

// ------------------------------------------------------ native v1: audits 62-64
// P0-62 (session-owned terminal projection), P0-63 (authoritative usage from
// the durable rows) and P0-64 (cursor messages, journal event pages,
// providers, task budget/progress, verification evidence). Every handler is
// auth-gated like every native route; query/body DTOs are strict
// (deny_unknown_fields — a typo is a 400); hostile ids are 400
// (unparseable/0) or typed 404 (unknown).

/// Reservation rows scanned per task for a usage aggregate. Every paid
/// model call settles exactly one reservation row; a hostile row flood past
/// this cap truncates LOUDLY (`truncated: true`), never silently.
pub(crate) const MAX_NATIVE_RESERVATIONS_SCAN: i64 = 10_000;

/// Cap on parsed route-decision summaries of one task.
pub(crate) const MAX_NATIVE_ROUTE_DECISIONS: usize = 200;

/// The amount one SETTLED reservation actually folded into the task's spent
/// total: the v18 canonical `settled_cost_micro`, falling back for pre-v18
/// rows (which never recorded which amount was folded) to the legacy
/// winner — the provider-reported amount when the frame carried one, else
/// the locally calculated amount. Hostile all-NULL rows fold zero.
pub(crate) fn reservation_settled_spent_micro(row: &faktor_store::CostReservationRow) -> u64 {
    row.settled_cost_micro
        .or(row.provider_reported_micro)
        .or(row.provider_cost_micro)
        .unwrap_or(0)
}

/// The provider-reported amount of one settled reservation (the v18
/// canonical `provider_reported_cost_micro`; its pre-v18 twin fallback).
pub(crate) fn reservation_provider_reported_micro(row: &faktor_store::CostReservationRow) -> u64 {
    row.provider_reported_cost_micro
        .or(row.provider_reported_micro)
        .unwrap_or(0)
}

/// The durable reservation aggregate of ONE task: counts and micro sums per
/// status over the task's `cost_reservation` rows plus the parsed
/// route-decision summaries of its settled rows (audit P0-63). The scan is
/// bounded ([`MAX_NATIVE_RESERVATIONS_SCAN`]); a flood past the cap
/// truncates loudly (`truncated: true`).
pub(crate) fn native_task_reservation_view(
    store: &faktor_store::Store,
    session_id: SessionId,
    task_id: faktor_core::id::TaskId,
) -> Result<serde_json::Value, faktor_core::Error> {
    let rows = store
        .cost_reservations_of(session_id, task_id, MAX_NATIVE_RESERVATIONS_SCAN)
        .map_err(store_err_to_core)?;
    let mut open_count: u64 = 0;
    let mut open_predicted: u64 = 0;
    let mut settled_count: u64 = 0;
    let mut settled_predicted: u64 = 0;
    let mut settled_spent: u64 = 0;
    let mut settled_reported: u64 = 0;
    let mut refunded_count: u64 = 0;
    let mut refunded_predicted: u64 = 0;
    let mut uncertain_count: u64 = 0;
    let mut uncertain_predicted: u64 = 0;
    let mut route_decisions: Vec<serde_json::Value> = Vec::new();
    // The store lists newest first, so the summaries naturally keep the
    // newest decisions within the cap. Group vocabulary is the schema v17+
    // state set: in-flight = `reserved`/`dispatched` (the v15 `open` was
    // renamed at v17), crash recovery closes pre-dispatch rows REFUNDED and
    // dispatched-marker rows UNCERTAIN — the v15 `abandoned` state no longer
    // exists and never occurs in a migrated store.
    for row in &rows {
        match row.status.as_str() {
            "reserved" | "dispatched" => {
                open_count += 1;
                open_predicted = open_predicted.saturating_add(row.predicted_micro);
            }
            "settled" => {
                settled_count += 1;
                settled_predicted = settled_predicted.saturating_add(row.predicted_micro);
                settled_spent = settled_spent.saturating_add(reservation_settled_spent_micro(row));
                settled_reported =
                    settled_reported.saturating_add(reservation_provider_reported_micro(row));
                if route_decisions.len() < MAX_NATIVE_ROUTE_DECISIONS {
                    let route = row
                        .route_decision_json
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                        .unwrap_or_else(|| {
                            serde_json::json!({ "raw": row.route_decision_json.as_deref().unwrap_or("") })
                        });
                    route_decisions.push(serde_json::json!({
                        "reservationId": row.reservation_id,
                        "predictedMicro": money::json(row.predicted_micro),
                        "spentMicro": money::json(reservation_settled_spent_micro(row)),
                        "providerReportedMicro": money::json(reservation_provider_reported_micro(row)),
                        "decision": route,
                    }));
                }
            }
            "refunded" => {
                refunded_count += 1;
                refunded_predicted = refunded_predicted.saturating_add(row.predicted_micro);
            }
            "uncertain" => {
                uncertain_count += 1;
                uncertain_predicted = uncertain_predicted.saturating_add(row.predicted_micro);
            }
            // Unknown status strings (hostile rows) contribute nothing.
            _ => {}
        }
    }
    let truncated = rows.len() as i64 >= MAX_NATIVE_RESERVATIONS_SCAN;
    let mut v = serde_json::json!({
        "open": { "count": open_count, "predictedMicro": money::json(open_predicted) },
        "settled": {
            "count": settled_count,
            "predictedMicro": money::json(settled_predicted),
            "spentMicro": money::json(settled_spent),
            "providerReportedMicro": money::json(settled_reported),
        },
        "refunded": { "count": refunded_count, "predictedMicro": money::json(refunded_predicted) },
        "uncertain": { "count": uncertain_count, "predictedMicro": money::json(uncertain_predicted) },
        "routeDecisions": route_decisions,
    });
    if truncated {
        v["truncated"] = serde_json::json!(true);
    }
    Ok(v)
}

/// The authoritative durable usage view of ONE session (audit P0-63):
/// provider-call token aggregates over the persisted columns (the
/// settlement folds cache reads/writes and reasoning into the recorded
/// input/output totals BEFORE persistence, so the rows — and therefore this
/// view — carry exactly those totals), the per-row prefix observations,
/// and per typed task: budget envelope + reservation aggregates + route
/// decision summaries. Store errors are loud; hostile rows can only
/// truncate, never fabricate.
pub(crate) fn native_session_usage_view(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
) -> Result<serde_json::Value, faktor_core::Error> {
    let store = state.deps.session.store();
    let sid = handle.id();
    let tokens = store.session_usage_tokens(sid).map_err(store_err_to_core)?;
    let prefix_rows = store
        .provider_call_prefix_rows(sid)
        .map_err(store_err_to_core)?;
    let prefix_view: Vec<serde_json::Value> = prefix_rows
        .iter()
        .take(MAX_NATIVE_LIST)
        .map(|r| {
            serde_json::json!({
                "rowId": r.row_id,
                "promptTokens": r.prompt_tokens,
                "stability": r.prefix_stability,
            })
        })
        .collect();
    let stability = handle.stored_prefix_stability()?.map(|a| {
        serde_json::json!({
            "observations": a.observations,
            "mean": a.mean,
            "stdDev": a.std_dev,
        })
    });
    let warn_at = warn_at_percent(state);
    let mut tasks: Vec<serde_json::Value> = Vec::new();
    let typed = handle.list_tasks()?;
    for task in typed.iter().take(MAX_NATIVE_LIST) {
        let cost = store
            .cost_task_row(sid, task.task_id)
            .map_err(store_err_to_core)?;
        let reservations = native_task_reservation_view(&store, sid, task.task_id)?;
        let open_micro = money::from_json(&reservations["open"]["predictedMicro"]).unwrap_or(0);
        let max_cost_micro = cost.as_ref().and_then(|c| c.max_cost_micro);
        let spent_micro = cost.as_ref().map(|c| c.spent_cost_micro).unwrap_or(0);
        let uncertain_micro =
            money::from_json(&reservations["uncertain"]["predictedMicro"]).unwrap_or(0);
        let committed = spent_micro
            .saturating_add(open_micro)
            .saturating_add(uncertain_micro);
        let (percent, warning, exhausted) = cap_state(max_cost_micro, committed, warn_at);
        tasks.push(serde_json::json!({
            "taskId": task.task_id.to_string(),
            "budget": {
                "maxTokens": task.budget.max_tokens,
                "maxTurns": task.budget.max_turns,
                "spentTokens": task.budget.spent_tokens,
                "spentTurns": task.budget.spent_turns,
                "maxCostMicro": money::json_opt(max_cost_micro),
                "spentCostMicro": money::json(spent_micro),
                "openReservedMicro": money::json(open_micro),
                "uncertainReservedMicro": money::json(uncertain_micro),
                "capState": {
                    "committedMicro": money::json(committed),
                    "percent": percent,
                    "warning": warning,
                    "exhausted": exhausted,
                },
            },
            "reservations": reservations,
        }));
    }
    Ok(serde_json::json!({
        "sessionId": sid.to_string(),
        "providerCalls": {
            "tokens": tokens,
            "prefixObservations": prefix_view,
        },
        "prefixStability": stability,
        "tasks": tasks,
    }))
}

/// The committed warning threshold (`faktor.budget.warnAtPercent`): the SAME
/// foundation configuration snapshot the settings surface writes, clamped to
/// 1..=100. Absent/foundation-less hosts keep the documented 80% default.
fn warn_at_percent(state: &AppState) -> u64 {
    warn_at_percent_from(state.deps.foundation.as_deref())
}

fn warn_at_percent_from(foundation: Option<&faktor_foundation::Foundation>) -> u64 {
    foundation
        .and_then(|foundation| {
            foundation
                .config
                .u64_value("faktor.budget.warnAtPercent")
                .ok()
                .flatten()
        })
        .unwrap_or(80)
        .clamp(1, 100)
}

/// The durable cap projection (F106): a warning is induced at
/// `>= warn_at_percent` of the committed cap, exhaustion at >=100%. `None`
/// is unlimited (never a fake warning), and a zero cap is immediately
/// exhausted. The threshold is read from the committed configuration on every
/// read, so a settings change takes effect at the next usage projection.
fn cap_state(
    max_cost_micro: Option<u64>,
    committed: u64,
    warn_at_percent: u64,
) -> (u32, bool, bool) {
    match max_cost_micro {
        Some(cap) if cap > 0 => {
            let percent = u32::try_from(
                (u128::from(committed).saturating_mul(100) / u128::from(cap)).min(999),
            )
            .unwrap_or(999);
            let warning = u128::from(committed).saturating_mul(100)
                >= u128::from(cap).saturating_mul(u128::from(warn_at_percent.clamp(1, 100)));
            (percent, warning, committed >= cap)
        }
        Some(_) => (0, false, true),
        None => (0, false, false),
    }
}

/// `GET /native/session/{id}/usage` — the authoritative usage of ONE
/// session over its durable rows (audit P0-63; see
/// [`native_session_usage_view`] for the exact row sources). Hostile ids
/// 400, unknown sessions 404.
pub(crate) async fn native_session_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match native_session_usage_view(&state, &handle) {
        Ok(v) => money_json(v),
        Err(e) => api_err(&e),
    }
}

#[cfg(test)]
mod cap_state_tests {
    use super::{cap_state, warn_at_percent_from};

    #[test]
    fn cap_warning_uses_the_committed_threshold_and_exhaustion_at_the_cap() {
        assert_eq!(cap_state(None, u64::MAX, 80), (0, false, false));
        assert_eq!(cap_state(Some(0), 0, 80), (0, false, true));
        assert_eq!(cap_state(Some(1000), 799, 80), (79, false, false));
        assert_eq!(cap_state(Some(1000), 800, 80), (80, true, false));
        assert_eq!(cap_state(Some(1000), 999, 80), (99, true, false));
        assert_eq!(cap_state(Some(1000), 1000, 80), (100, true, true));
        assert_eq!(cap_state(Some(1000), 5000, 80), (500, true, true));
        // The committed setting is honored (and clamped): 50 warns earlier,
        // 90 later; 0 cannot make every task warn.
        assert_eq!(cap_state(Some(1000), 500, 50), (50, true, false));
        assert_eq!(cap_state(Some(1000), 899, 90), (89, false, false));
        assert_eq!(
            cap_state(Some(1000), 10, 0),
            (1, true, false),
            "0 clamps to the 1% minimum"
        );
        // No overflow at the u64 ceiling.
        assert_eq!(
            cap_state(Some(u64::MAX), u64::MAX - 1, 80),
            (99, true, false)
        );
    }

    #[test]
    fn warn_threshold_is_read_from_the_committed_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let foundation = faktor_foundation::Foundation::open(dir.path()).unwrap();
        assert_eq!(warn_at_percent_from(Some(&foundation)), 80);
        let snapshot = foundation.config.snapshot().unwrap();
        let patch = faktor_foundation::config::Patch {
            expected_revision: snapshot.revision,
            changes: vec![faktor_foundation::config::Change {
                key: "faktor.budget.warnAtPercent".into(),
                scope: faktor_foundation::config::Scope::User,
                value: Some(serde_json::json!(50)),
            }],
        };
        foundation.config.commit(&patch).unwrap();
        assert_eq!(warn_at_percent_from(Some(&foundation)), 50);
        // A committed out-of-range value is refused by the descriptor, not
        // silently clamped in the store.
        let snapshot = foundation.config.snapshot().unwrap();
        let invalid = faktor_foundation::config::Patch {
            expected_revision: snapshot.revision,
            changes: vec![faktor_foundation::config::Change {
                key: "faktor.budget.warnAtPercent".into(),
                scope: faktor_foundation::config::Scope::User,
                value: Some(serde_json::json!(101)),
            }],
        };
        assert!(foundation.config.commit(&invalid).is_err());
        assert_eq!(warn_at_percent_from(Some(&foundation)), 50);
    }
}
