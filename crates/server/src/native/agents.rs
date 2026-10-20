//! Native agent control surface: durable child agents, orchestrator graph
//! projection and exactly-once child commands.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_core::id::SessionId;
use faktor_protocol::error::ApiError;

use super::*;
use crate::api::AppState;

/// Wave-12/13 durable row kinds of the orchestration runtime (owned by
/// `faktor-orchestrator`; the projection here reads them generically):
/// plan rows (key = run id), registry rows (key `<run>/<child_id>`),
/// merge envelopes (key `<run>/<child_id>/merge/<cs_id>/<seq>`) and merge
/// parts (key `<run>/<child_id>/merge/<cs_id>/<seq>/part/<name>`).
pub(crate) const ORCH_PLAN_KIND: &str = "orchestrator_plan";

pub(crate) const ORCH_REGISTRY_KIND: &str = "orchestrator_registry";

pub(crate) const ORCH_MERGE_KIND: &str = "orchestrator_merge";

pub(crate) const ORCH_MERGE_PART_KIND: &str = "orchestrator_merge_part";

/// Cap on graph child nodes (mirrors the orchestrator's own cap).
pub(crate) const MAX_GRAPH_CHILDREN: usize = 256;

/// Strict query DTO: `?session=<id>` only; an unknown field is a 400.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeOrchestratorGraphQuery {
    session: String,
}

/// Bound on the orchestrator fact set read by one graph projection (the
/// historical page walk could touch 51,200 rows). Beyond it the projection
/// REFUSES loudly: a hostile row space is never silently truncated into a
/// partial graph.
pub(crate) const MAX_ORCHESTRATOR_FACTS: u64 = 50_000;

/// The durable orchestrator rows of one session, read ONCE through the
/// `(kind, session_id)` index. The historical walk paged through every
/// ordinary memory fact (200 rows x up to 256 pages) and filtered in Rust,
/// so a session with large user memory paid tens of thousands of irrelevant
/// rows per graph/agents/task-run read.
pub(crate) fn orchestrator_graph_facts(
    handle: &faktor_session::SessionHandle,
) -> Result<Vec<(String, String, String)>, String> {
    let (facts, has_more) = handle
        .memory_facts_of_kinds(
            &[
                // In-session task runs are orchestration rows too: omitting
                // this kind made `/native/agents`, `/task-runs` and the
                // start receipt projection 404 a run that had just landed.
                TASK_RUN_ROW_KIND,
                ORCH_PLAN_KIND,
                ORCH_REGISTRY_KIND,
                ORCH_MERGE_KIND,
                ORCH_MERGE_PART_KIND,
            ],
            MAX_ORCHESTRATOR_FACTS,
        )
        .map_err(|e| format!("orchestrator fact read: {}", e.message))?;
    if has_more {
        return Err(format!(
            "orchestrator fact set exceeds the {MAX_ORCHESTRATOR_FACTS}-row graph bound; \
             refusing a partial graph"
        ));
    }
    Ok(facts)
}

/// Rejoin one chunked row set (header + `key/cNNN` chunks) into its chunk
/// strings. Header rows carry `{"chunks": n}`; a count mismatch is loud.
pub(crate) fn orchestrator_chunks_of(
    facts: &[(String, String, String)],
    kind: &str,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    let mut header: Option<serde_json::Value> = None;
    let mut chunks: Vec<(usize, String)> = Vec::new();
    for (k, kk, v) in facts {
        if k != kind {
            continue;
        }
        if kk == key {
            header = Some(
                serde_json::from_str(v)
                    .map_err(|e| format!("chunked row header decode {kind}/{key}: {e}"))?,
            );
        } else if let Some(rest) = kk.strip_prefix(key).and_then(|r| r.strip_prefix("/c")) {
            let idx: usize = rest
                .parse()
                .map_err(|_| format!("hostile chunk key {kind}/{kk}"))?;
            chunks.push((idx, v.clone()));
        }
    }
    let Some(header) = header else {
        return Ok(None);
    };
    chunks.sort_by_key(|(i, _)| *i);
    let n = header.get("chunks").and_then(|c| c.as_u64()).unwrap_or(0) as usize;
    if chunks.len() != n {
        return Err(format!(
            "chunked row {kind}/{key} is incomplete ({} of {n} chunks)",
            chunks.len()
        ));
    }
    Ok(Some(chunks.into_iter().map(|(_, v)| v).collect()))
}

pub(crate) fn orchestrator_graph_row_error(
    facts: &[(String, String, String)],
    kind: &str,
    key: &str,
) -> String {
    let raw = facts
        .iter()
        .find(|(k, kk, _)| k == kind && kk == key)
        .map(|(_, _, v)| v.clone())
        .unwrap_or_default();
    format!("stored row {kind}/{key} of {:?} is not valid JSON", raw)
}

/// `GET /native/orchestrator/graph?session=<id>` (audit 93) — the single
/// durable operation graph of one orchestration run, assembled as a READ
/// over the wave-12/13 rows of the parent session: the plan row (root),
/// the registry rows (children), each child session's control rows
/// (steering history with exactly-once applied timestamps) and the merge
/// envelopes/parts (merge outcome). Auth-gated like every `/native`
/// handler.
///
/// Wire contract (documented — mirrors the typed `OpGraph` of
/// `faktor-orchestrator`):
///
/// ```json
/// { "plan_id": "<run id>",
///   "goal": "...",
///   "state": "Running|Done|Failed|Cancelled|Blocked|Pending",
///   "work_items": [ { "item_id": "...", "kind": "...", "state": "..." } ],
///   "children": [ { "child_id": "...", "session_id": 2,
///       "operation_id": 0, "worktree_id": 1, "ownership": "...",
///       "state": "...", "budget": null, "capabilities": [],
///       "plan_step_index": 0,
///       "steer_events": [ { "kind": { "kind": "pause" }, "seq": 1,
///                            "applied_ms": 123 } ],
///       "merge": null | { "change_set_id": "...", "merged": ["a.rs"],
///                         "rejected": [], "conflicts": [] } } ] }
/// ```
///
/// Ordering is deterministic (plan step order, then spawn order); states
/// are derived from the durable child rows with the executor re-attach
/// semantics. Errors: hostile/missing session ids (non-numeric, 0,
/// unknown, or a session without any orchestration run) are 404; a parent
/// session holding several runs is a 409 naming the runs; corrupted rows
/// are 500 — never a silently partial graph.
pub(crate) async fn native_orchestrator_graph(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeOrchestratorGraphQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let graph_404 = |m: String| {
        let e = ApiError {
            code: "not_found",
            message: m,
            http_status: 404,
            retryable: false,
        };
        (StatusCode::NOT_FOUND, Json(e.to_json())).into_response()
    };
    // Hostile ids are 404 for this endpoint (a graph is identified by a
    // session; anything that cannot be one has no graph).
    let Ok(raw) = q.session.parse::<u64>() else {
        return graph_404(format!("invalid session id {:?}", q.session));
    };
    if raw == 0 {
        return graph_404("session id cannot be 0".to_string());
    }
    let sid = SessionId::new(raw);
    let handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => return graph_404(format!("session {sid} has no orchestration graph")),
        Err(e) => return api_err(&e),
    };
    let facts = match orchestrator_graph_facts(&handle) {
        Ok(f) => f,
        Err(m) => {
            let e = ApiError {
                code: "internal",
                message: m,
                http_status: 500,
                retryable: false,
            };
            return wire_status(e);
        }
    };
    // Every run with durable plan or registry rows, sorted.
    let mut runs: Vec<String> = Vec::new();
    let push_run = |runs: &mut Vec<String>, run: &str| {
        if !runs.iter().any(|r| r == run) {
            runs.push(run.to_string());
        }
    };
    for (kind, key, _) in &facts {
        match kind.as_str() {
            ORCH_PLAN_KIND => push_run(&mut runs, key),
            ORCH_REGISTRY_KIND => {
                if let Some(run) = key.rsplit_once('/').map(|(r, _)| r) {
                    push_run(&mut runs, run);
                }
            }
            _ => {}
        }
    }
    runs.sort();
    if runs.is_empty() {
        return graph_404(format!("session {sid} has no orchestration graph"));
    }
    if runs.len() > 1 {
        let e = ApiError {
            code: "conflict",
            message: format!(
                "session {sid} holds {} orchestration runs ({}); the graph of one run needs one plan",
                runs.len(),
                runs.join(", ")
            ),
            http_status: 409,
            retryable: false,
        };
        return wire_status(e);
    }
    let run = runs.into_iter().next().expect("len checked");
    // ---- plan row: root node.
    let plan_value: serde_json::Value = {
        let chunks = match orchestrator_chunks_of(&facts, ORCH_PLAN_KIND, &run) {
            Ok(c) => c,
            Err(m) => return wire_status(internal_graph_err(m)),
        };
        let raw = chunks.map(|c| c.join("")).unwrap_or_default();
        let raw = if raw.is_empty() {
            facts
                .iter()
                .find(|(k, kk, _)| k == ORCH_PLAN_KIND && kk == &run)
                .map(|(_, _, v)| v.clone())
                .unwrap_or_default()
        } else {
            raw
        };
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => v,
            Err(_) => {
                return wire_status(internal_graph_err(orchestrator_graph_row_error(
                    &facts,
                    ORCH_PLAN_KIND,
                    &run,
                )));
            }
        }
    };
    let plan = plan_value.get("plan").cloned().unwrap_or_default();
    let goal = plan
        .get("goal")
        .and_then(|g| g.as_str())
        .unwrap_or("")
        .to_string();
    let plan = match faktor_orchestrator::TaskPlan::from_legacy_value(
        plan_value.get("plan").cloned().unwrap_or_default(),
    ) {
        Ok(p) => p,
        Err(m) => {
            return wire_status(internal_graph_err(format!(
                "stored plan row of run {run} is invalid: {m}"
            )));
        }
    };
    // ---- registry rows: children (parse errors are loud).
    let prefix = format!("{run}/");
    let mut child_rows: Vec<(String, faktor_orchestrator::runtime::ChildRuntime)> = Vec::new();
    for (kind, key, value) in &facts {
        if kind != ORCH_REGISTRY_KIND {
            continue;
        }
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() || rest.contains('/') {
            return wire_status(internal_graph_err(format!(
                "hostile registry row key {key:?} under run {run}"
            )));
        }
        match serde_json::from_str::<faktor_orchestrator::runtime::ChildRuntime>(value) {
            Ok(row) => child_rows.push((rest.to_string(), row)),
            Err(e) => {
                return wire_status(internal_graph_err(format!(
                    "stored row {ORCH_REGISTRY_KIND}/{key} is not a valid child runtime row: {e}"
                )));
            }
        }
    }
    if child_rows.len() > MAX_GRAPH_CHILDREN {
        return wire_status(internal_graph_err(format!(
            "run {run} holds {} durable child rows (cap {MAX_GRAPH_CHILDREN}); refusing an unbounded graph",
            child_rows.len()
        )));
    }
    // plan_step_index: position of the child's item id in the plan.
    let step_index = |item: &str| plan.work_items.iter().position(|w| w.id == item);
    // The SHARED aggregation: this JSON surface and the typed operation
    // graph call the SAME functions over the SAME rows, so their derived
    // states are byte-identical.
    let rows: Vec<faktor_orchestrator::runtime::ChildRuntime> =
        child_rows.iter().map(|(_, row)| row.clone()).collect();
    let derived = faktor_orchestrator::derived_item_states(&plan, &rows);
    let root_state = faktor_orchestrator::derived_root_state(&derived);
    // ---- children projection.
    let mut children: Vec<(
        Option<usize>,
        i64,
        String,
        faktor_orchestrator::runtime::ChildRuntime,
    )> = Vec::new();
    for (child_id, row) in child_rows {
        let idx = step_index(&row.item_id);
        children.push((idx, row.created_ms, child_id, row));
    }
    children.sort_by(|a, b| {
        (a.0.unwrap_or(usize::MAX), a.2.clone()).cmp(&(b.0.unwrap_or(usize::MAX), b.2.clone()))
    });
    let mut out_children = Vec::with_capacity(children.len());
    for (_idx, _created, child_id, mut row) in children {
        // The registry JSON is never rewritten by controls: derive the
        // mutable truth (task-row budget, drive-state phase) from the
        // child's OWN durable rows before projecting anything.
        faktor_orchestrator::runtime::OrchestratorRuntime::derive_child_projection(
            &state.deps.session,
            &mut row,
        );
        // Steering history from the child session's control rows.
        let sid_raw = row.session_id;
        let mut presentation: Option<faktor_session::child::PresentationState> = None;
        let steer_events: Vec<serde_json::Value> = if sid_raw == 0 {
            Vec::new()
        } else {
            match state.deps.session.get_session(SessionId::new(sid_raw)) {
                Ok(Some(ch)) => {
                    let events = match ch.orchestrator_ctl_all() {
                        Ok(ctl) => ctl
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "kind": c.control,
                                    "seq": c.seq,
                                    "applied_ms": c.applied_ms,
                                })
                            })
                            .collect(),
                        Err(e) => {
                            return wire_status(internal_graph_err(format!(
                                "steering rows of child {child_id}: {}",
                                e.message
                            )));
                        }
                    };
                    presentation = Some(match ch.child_presentation(&row.child_id) {
                        Ok(p) => p,
                        Err(e) => {
                            return wire_status(internal_graph_err(format!(
                                "presentation of child {child_id}: {}",
                                e.message
                            )));
                        }
                    });
                    events
                }
                Ok(None) => {
                    return wire_status(internal_graph_err(format!(
                        "graph assembly: child {child_id} names missing session {sid_raw}"
                    )));
                }
                Err(e) => return api_err(&e),
            }
        };
        let presentation = presentation.unwrap_or_default();
        // Latest durable merge envelope + its parts.
        let merge_prefix = format!("{run}/{child_id}/merge/");
        let mut envelopes: Vec<(u64, serde_json::Value)> = Vec::new();
        for (kind, key, value) in &facts {
            if kind != ORCH_MERGE_KIND {
                continue;
            }
            let Some(rest) = key.strip_prefix(&merge_prefix) else {
                continue;
            };
            let Some((_cs, seq_s)) = rest.rsplit_once('/') else {
                return wire_status(internal_graph_err(format!("hostile merge row key {key:?}")));
            };
            let Ok(seq) = seq_s.parse::<u64>() else {
                return wire_status(internal_graph_err(format!("hostile merge row key {key:?}")));
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(value) else {
                return wire_status(internal_graph_err(format!(
                    "merge envelope {key:?} is not valid JSON"
                )));
            };
            envelopes.push((seq, v));
        }
        envelopes.sort_by_key(|(seq, _)| *seq);
        let merge: Option<serde_json::Value> = envelopes.last().map(|(seq, env)| {
            let cs_id = env
                .get("cs_id")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let part_prefix = format!("{run}/{child_id}/merge/{cs_id}/{seq}/part/");
            let mut merged: Vec<serde_json::Value> = Vec::new();
            let mut rejected: Vec<serde_json::Value> = Vec::new();
            let mut conflicts: Vec<serde_json::Value> = Vec::new();
            for part in ["merged", "rejected", "conflicts"] {
                let key = format!("{part_prefix}{part}");
                let Ok(Some(chunks)) = orchestrator_chunks_of(&facts, ORCH_MERGE_PART_KIND, &key)
                else {
                    continue;
                };
                for c in &chunks {
                    let Ok(items) = serde_json::from_str::<serde_json::Value>(c) else {
                        continue;
                    };
                    let Some(items) = items.as_array() else {
                        continue;
                    };
                    for item in items {
                        match part {
                            "merged" | "rejected" => {
                                if let Some(p) = item.as_str() {
                                    (if part == "merged" {
                                        &mut merged
                                    } else {
                                        &mut rejected
                                    })
                                    .push(serde_json::json!(p));
                                }
                            }
                            _ => {
                                if let Some(p) = item.as_array() {
                                    if p.len() == 2 {
                                        conflicts.push(serde_json::json!([
                                            p[0].as_str().unwrap_or(""),
                                            p[1].as_str().unwrap_or("")
                                        ]));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            merged.sort_by(|a, b| a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or("")));
            rejected.sort_by(|a, b| a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or("")));
            conflicts.sort_by(|a, b| {
                let ka = a[0].as_str().unwrap_or("");
                let kb = b[0].as_str().unwrap_or("");
                ka.cmp(kb)
                    .then_with(|| a[1].as_str().unwrap_or("").cmp(b[1].as_str().unwrap_or("")))
            });
            serde_json::json!({
                "change_set_id": cs_id,
                "merged": merged,
                "rejected": rejected,
                "conflicts": conflicts,
            })
        });
        let child_state = work_state_tag(faktor_orchestrator::project_child_state(&row));
        let blocker = row.blocker().map(|b| {
            serde_json::json!({
                "kind": b.kind,
                "reason": b.reason,
                "dependency": b.dependency,
                "resolution": b.resolution,
                "last_progress_ms": b.last_progress_ms,
            })
        });
        out_children.push(serde_json::json!({
            "child_id": child_id,
            "session_id": row.session_id,
            "operation_id": row.operation_id,
            "worktree_id": row.worktree_id,
            "ownership": row.ownership,
            "state": child_state,
            "execution_phase": row.execution_phase.as_str(),
            "presentation": presentation,
            "blocker": blocker,
            "budget": row.budget_max_tokens,
            "capabilities": row.permissions,
            "plan_step_index": step_index(&row.item_id),
            "steer_events": steer_events,
            "merge": merge,
        }));
    }
    Json(serde_json::json!({
        "plan_id": run,
        "goal": goal,
        "state": work_state_tag(root_state),
        "work_items": plan
            .work_items
            .iter()
            .zip(&derived)
            .map(|(w, st)| serde_json::json!({
                "item_id": w.id.clone(),
                "kind": w.kind,
                "state": work_state_tag(*st),
            }))
            .collect::<Vec<_>>(),
        "children": out_children,
    }))
    .into_response()
}

/// Strict native request bodies (deny_unknown_fields — a typo is a 400).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSteerBody {
    text: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeModelBody {
    model: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeBudgetBody {
    max_tokens: Option<u64>,
    /// Money input: decimal string or legacy JSON integer (see
    /// `faktor_cloud::money`).
    #[serde(default, with = "faktor_cloud::money::option")]
    max_cost_micro: Option<u64>,
}

/// Strict presentation-transition body: `{"state": "foreground"|"background"}`
/// only (deny_unknown_fields; an unknown state tag, a missing field or a
/// hostile extra member is a plain 400).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePresentationBody {
    state: faktor_session::child::PresentationState,
}

/// Strict query DTO: `?session=<id>` only (mirrors the graph endpoint).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeAgentsQuery {
    session: String,
}

/// Chunk-aware value reader for one durable row: the PLAIN value when the
/// row is a single JSON document, the header + `key/cNNN` chunks joined
/// when the row was written chunked. Missing rows are `None`.
pub(crate) fn orchestrator_row_value(
    facts: &[(String, String, String)],
    kind: &str,
    key: &str,
) -> Result<Option<String>, String> {
    let header: Option<&(String, String, String)> =
        facts.iter().find(|(k, kk, _)| k == kind && kk == key);
    let is_header = header.is_some_and(|(_, _, v)| {
        serde_json::from_str::<serde_json::Value>(v)
            .ok()
            .and_then(|j| j.get("chunks").and_then(|c| c.as_u64()))
            .unwrap_or(0)
            > 0
    });
    if let Some(row) = header {
        if !is_header {
            return Ok(Some(row.2.clone()));
        }
        if let Some(chunks) = orchestrator_chunks_of(facts, kind, key)? {
            return Ok(Some(chunks.concat()));
        }
    }
    Ok(None)
}

/// One-pass read index over a session's durable fact rows, shared by the
/// native agent listing. The historical readers ran a linear
/// `facts.iter().find(...)` per value lookup AND a full facts scan per run /
/// per child, so a listing of R runs × C children over F rows cost
/// O((R + C) · F). The index answers every lookup in O(1) (non-chunked rows)
/// while producing EXACTLY the historical results:
///
/// - [`Self::row_value`] mirrors [`orchestrator_row_value`]: FIRST fact wins,
///   and a chunked first value routes through the chunk joiner unchanged;
/// - [`Self::registry_rows_for_run`] mirrors the
///   `key.strip_prefix(run + "/")` scan: each registry row is bucketed under
///   EVERY slash-terminated key prefix, so a run sees exactly the rows whose
///   key starts with `run + "/"`, in facts order (the order the scan
///   produced);
/// - [`Self::merge_best`] mirrors the per-child `.../merge/` prefix scan:
///   each envelope is bucketed under every full `.../merge/` prefix with the
///   historical strictly-greater sequence rule (first maximum wins).
pub(crate) struct FactIndex<'a> {
    first: std::collections::HashMap<(&'a str, &'a str), (&'a str, bool)>,
    registry_by_run: std::collections::HashMap<&'a str, Vec<(&'a str, &'a str)>>,
    merge_by_prefix: std::collections::HashMap<&'a str, (u64, &'a str)>,
}

impl<'a> FactIndex<'a> {
    pub(crate) fn new(facts: &'a [(String, String, String)]) -> Self {
        let mut first: std::collections::HashMap<(&'a str, &'a str), (&'a str, bool)> =
            std::collections::HashMap::new();
        let mut registry_by_run: std::collections::HashMap<&'a str, Vec<(&'a str, &'a str)>> =
            std::collections::HashMap::new();
        let mut merge_by_prefix: std::collections::HashMap<&'a str, (u64, &'a str)> =
            std::collections::HashMap::new();
        for (kind, key, value) in facts {
            let chunked = serde_json::from_str::<serde_json::Value>(value)
                .ok()
                .and_then(|j| j.get("chunks").and_then(|c| c.as_u64()))
                .unwrap_or(0)
                > 0;
            first
                .entry((kind.as_str(), key.as_str()))
                .or_insert((value.as_str(), chunked));
            if kind == ORCH_REGISTRY_KIND {
                for (i, _) in key.match_indices('/') {
                    registry_by_run
                        .entry(&key[..i])
                        .or_default()
                        .push((key.as_str(), value.as_str()));
                }
            }
            if kind == ORCH_MERGE_KIND {
                for (i, _) in key.match_indices("/merge/") {
                    let end = i + "/merge/".len();
                    let rest = &key[end..];
                    let Some((_cs, seq_s)) = rest.rsplit_once('/') else {
                        continue;
                    };
                    let Ok(seq) = seq_s.parse::<u64>() else {
                        continue;
                    };
                    merge_by_prefix
                        .entry(&key[..end])
                        .and_modify(|(best, v)| {
                            if seq > *best {
                                *best = seq;
                                *v = value.as_str();
                            }
                        })
                        .or_insert((seq, value.as_str()));
                }
            }
        }
        Self {
            first,
            registry_by_run,
            merge_by_prefix,
        }
    }

    /// [`orchestrator_row_value`] over the index (see the struct docs).
    pub(crate) fn row_value(
        &self,
        facts: &[(String, String, String)],
        kind: &str,
        key: &str,
    ) -> Result<Option<String>, String> {
        match self.first.get(&(kind, key)) {
            Some((value, false)) => Ok(Some((*value).to_string())),
            Some((_, true)) => orchestrator_row_value(facts, kind, key),
            None => Ok(None),
        }
    }

    /// Every registry row of `run` in facts order, exactly the historical
    /// `strip_prefix(run + "/")` scan would have yielded.
    pub(crate) fn registry_rows_for_run(&self, run: &str) -> Option<&Vec<(&'a str, &'a str)>> {
        self.registry_by_run.get(run)
    }

    /// The latest merge envelope of one child under one run, exactly the
    /// historical per-child prefix scan's winner.
    pub(crate) fn merge_best(&self, run: &str, child: &str) -> Option<(u64, &'a str)> {
        let prefix = format!("{run}/{child}/merge/");
        self.merge_by_prefix.get(prefix.as_str()).copied()
    }
}

/// The stable wire tag of one work state. This is pure presentation: the
/// state itself ALWAYS comes from the shared canonical projection
/// ([`faktor_orchestrator::project_child_state`]).
pub(crate) fn work_state_tag(state: faktor_orchestrator::WorkState) -> &'static str {
    use faktor_orchestrator::WorkState::*;
    match state {
        Pending => "Pending",
        Running => "Running",
        Paused => "Paused",
        Waiting => "Waiting",
        Blocked => "Blocked",
        Done => "Done",
        Failed => "Failed",
        Cancelled => "Cancelled",
    }
}

/// The live state of one child: the shared canonical projection over the
/// durable ChildState, overlaid with the child session's durable drive
/// phase — a drive parked at a pause boundary reads Waiting even when the
/// registry row has not flipped yet.
pub(crate) fn child_live_state(
    row: &faktor_orchestrator::runtime::ChildRuntime,
    drive: &faktor_session::child::DriveState,
) -> &'static str {
    if !row.state.is_terminal() && drive.phase == faktor_session::child::ChildPhase::Waiting {
        return work_state_tag(faktor_orchestrator::WorkState::Waiting);
    }
    work_state_tag(faktor_orchestrator::project_child_state(row))
}

/// The effective model of one child: the drive state's applied model wins,
/// then the durable identity row, then the registry row's policy, then the
/// run's default model.
pub(crate) fn child_model(
    drive: &faktor_session::child::DriveState,
    identity: Option<&faktor_session::child::ChildIdentity>,
    row: &faktor_orchestrator::runtime::ChildRuntime,
    default_model: Option<&str>,
) -> Option<String> {
    if !drive.current_model.is_empty() {
        return Some(drive.current_model.clone());
    }
    if let Some(id) = identity {
        if !id.model.is_empty() {
            return Some(id.model.clone());
        }
    }
    row.model_policy
        .model
        .clone()
        .or_else(|| default_model.map(str::to_string))
}

/// The durable "latest result" summary of one child (wave-13/14 records):
/// its task goal plus the latest durable merge envelope's counts. Both are
/// served from the caller's ONE-PASS [`FactIndex`] (`merge_index`) and the
/// already-read identity goal — the historical body re-opened the child
/// session and re-scanned every fact row per child.
pub(crate) fn child_result_entry(
    identity_goal: &str,
    merge_index: Option<(u64, &str)>,
) -> serde_json::Value {
    let mut merge: Option<serde_json::Value> = None;
    if let Some((_seq, value)) = merge_index {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(value) {
            merge = Some(serde_json::json!({
                "change_set_id": v.get("cs_id").cloned().unwrap_or(serde_json::Value::Null),
                "merged": v.get("merged_count").cloned().unwrap_or(serde_json::Value::Null),
                "rejected": v.get("rejected_count").cloned().unwrap_or(serde_json::Value::Null),
                "conflicts": v.get("conflict_count").cloned().unwrap_or(serde_json::Value::Null),
            }));
        }
    }
    serde_json::json!({
        "summary": identity_goal,
        "merge": merge,
    })
}

/// The live session-state tag of one TaskExecutor in-session run (single
/// task per session; the session's live row state is the run's state).
pub(crate) fn session_run_state_tag(agent_state: faktor_core::state::AgentState) -> &'static str {
    use faktor_core::state::AgentState::*;
    match agent_state {
        Completed | ReadyForNextTurn => "Done",
        Cancelled => "Cancelled",
        FailedRecoverable | FailedPermanent => "Failed",
        NeedsUserInput => "Blocked",
        Idle => "Pending",
        _ => "Running",
    }
}

/// One child entry of the listing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn native_child_entry(
    state: &AppState,
    index: &FactIndex<'_>,
    run: &str,
    mut row: faktor_orchestrator::runtime::ChildRuntime,
    default_model: Option<&str>,
) -> Result<serde_json::Value, ApiError> {
    // ONE open of the child session per entry: the historical projection
    // opened the same session up to three times (the derive, this
    // projection, the result summary) and every open runs the ledger +
    // journal open-time verification.
    let session = match state
        .deps
        .session
        .get_session(SessionId::new(row.session_id))
    {
        Ok(Some(h)) => h,
        Ok(None) => {
            return Err(internal_graph_err(format!(
                "agents listing: child {} names missing session {}",
                row.child_id, row.session_id
            )));
        }
        Err(e) => return Err(faktor_protocol::error::from_core(&e)),
    };
    // The ONE drive-state read of this entry. The registry JSON is never
    // rewritten by controls: derive the mutable truth (task-row budget,
    // drive-state phase) from the child's OWN durable rows before
    // projecting anything — the same assignment the manager derive applies,
    // from this (strict) read.
    let drive = session.orchestrator_drive_state_get().map_err(|e| {
        internal_graph_err(format!(
            "drive state of child {}: {}",
            row.child_id, e.message
        ))
    })?;
    faktor_orchestrator::runtime::OrchestratorRuntime::derive_child_projection_from(
        &session, &drive, &mut row,
    );
    // The child session's own durable row is the authority for its provider:
    // the same (provider, model) pair the catalog join at the UI needs, so
    // two providers exposing one model id can never be conflated.
    let child_row = session
        .row()
        .map_err(|e| faktor_protocol::error::from_core(&e))?;
    // Presentation/attention state: the durable ledger fold of the child
    // session (foreground when it never transitioned). It is surfaced to
    // UIs only; the drive, budgets and lineage are untouched by it.
    let presentation = session.child_presentation(&row.child_id).map_err(|e| {
        internal_graph_err(format!(
            "presentation of child {}: {}",
            row.child_id, e.message
        ))
    })?;
    let identity = session.orchestrator_child_identity_get().map_err(|e| {
        internal_graph_err(format!(
            "identity row of child {}: {}",
            row.child_id, e.message
        ))
    })?;
    let goal = identity
        .as_ref()
        .map(|i| i.task_goal.clone())
        .unwrap_or_default();
    let progress = state
        .deps
        .agent
        .progress_view(SessionId::new(row.session_id));
    let result = child_result_entry(&goal, index.merge_best(run, &row.child_id));
    let state_tag = child_live_state(&row, &drive);
    let blocker = row.blocker().map(|b| {
        serde_json::json!({
            "kind": b.kind,
            "reason": b.reason,
            "dependency": b.dependency,
            "resolution": b.resolution,
            "last_progress_ms": b.last_progress_ms,
        })
    });
    Ok(serde_json::json!({
        "agent_id": row.child_id,
        "kind": "child",
        "run_id": run,
        "session_id": row.session_id,
        "worktree_id": row.worktree_id,
        "item_id": row.item_id,
        "item_kind": row.kind,
        "state": state_tag,
        // The coarse durable drive phase (additive projection; independent
        // of lifecycle). Absent rows decode as planning.
        "execution_phase": drive.execution_phase.as_str(),
        "blocker": blocker,
        "provider": child_row.provider,
        "model": child_model(&drive, identity.as_ref(), &row, default_model),
        "budget": row.budget_max_tokens,
        "ownership": row.ownership,
        "capabilities": row.permissions,
        "presentation": presentation,
        "progress": progress,
        "result": result,
        "goal": goal,
    }))
}

/// `GET /native/agents?session=<id>` (and `/native/session/{id}/agents`):
/// the real agent listing of one session — every orchestrated run's
/// children plus the parent's own task runs. Empty ONLY when the session
/// genuinely has no task run. Durable rows only; tampered rows are loud
/// 500s, never a silently partial listing.
pub(crate) fn native_agents_body(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let parent = handle.id();
    let facts = orchestrator_graph_facts(handle).map_err(internal_graph_err)?;
    // ONE pass over the fact rows for every lookup below: the historical
    // body scanned facts per run and per child (O(runs · facts) for the run
    // grouping, plus a scan per child row lookup and per child result).
    let index = FactIndex::new(&facts);

    // Dedupe in ONE hash pass. The historical `runs.iter().any(...)` made
    // this O(runs^2) for a long-lived session: 2,000 durable runs meant ~2M
    // string comparisons per /native/agents or /task-runs read. Insertion
    // order is still the fact order; the vectors are sorted below anyway.
    let mut seen_in_session: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_orchestrated: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut in_session: Vec<String> = Vec::new();
    let mut orchestrated: Vec<String> = Vec::new();
    let push_run =
        |runs: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, run: &str| {
            if seen.insert(run.to_string()) {
                runs.push(run.to_string());
            }
        };
    for (kind, key, _) in &facts {
        match kind.as_str() {
            TASK_RUN_ROW_KIND => push_run(&mut in_session, &mut seen_in_session, key),
            ORCH_PLAN_KIND => push_run(&mut orchestrated, &mut seen_orchestrated, key),
            ORCH_REGISTRY_KIND => {
                if let Some(run) = key.rsplit_once('/').map(|(r, _c)| r) {
                    if !run.is_empty() {
                        push_run(&mut orchestrated, &mut seen_orchestrated, run);
                    }
                }
            }
            _ => {}
        }
    }
    in_session.sort();
    orchestrated.sort();

    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut push = |e: serde_json::Value| {
        if entries.len() < MAX_AGENT_ENTRIES {
            entries.push(e);
        }
    };

    // (a) In-session task runs: the parent's own single-item runs. Every
    // run of the parent shares ONE session row, ONE task row and ONE
    // progress view; the historical loop re-read all three per run (so a
    // 2,000-run session issued ~6,000 point queries per listing).
    let session_row = handle
        .row()
        .map_err(|e| faktor_protocol::error::from_core(&e))?;
    let task = handle
        .get_task(session_row.task_id)
        .map_err(|e| faktor_protocol::error::from_core(&e))?;
    let budget = task.as_ref().and_then(|t| t.budget.max_tokens);
    let progress = state.deps.agent.progress_view(parent);
    // State: the durable typed task row wins when terminal (wave-24) —
    // a cancelled/verified/failed run reads as such even though the
    // session itself is parked. Constant for every run of this session.
    let run_state = run_state_tag_for(task.as_ref().map(|t| t.state), session_row.state);
    for key in &in_session {
        let value = match index.row_value(&facts, TASK_RUN_ROW_KIND, key) {
            Ok(Some(v)) => v,
            Ok(None) => continue,
            Err(m) => return Err(internal_graph_err(m)),
        };
        let row = match faktor_orchestrator::runtime::task_executor::TaskRunRow::decode(&value) {
            Ok(r) => r,
            Err(m) => {
                return Err(internal_graph_err(format!(
                    "stored taskexec run row {TASK_RUN_ROW_KIND}/{key}: {m}"
                )));
            }
        };
        push(serde_json::json!({
            "agent_id": key,
            "kind": "self",
            "run_id": key,
            "session_id": parent.raw(),
            "worktree_id": session_row.worktree_id.raw(),
            "goal": row.goal,
            "item_ids": row.item_ids,
            "state": run_state,
            "model": session_row.model.clone(),
            "budget": budget,
            "ownership": "self",
            "capabilities": [],
            "progress": progress.clone(),
            "result": serde_json::Value::Null,
        }));
    }

    // (b) Orchestrated runs: the parent run + its real children.
    for run in &orchestrated {
        // The durable plan row (goal, steps, default model, owner).
        let plan_json: Option<serde_json::Value> =
            match index.row_value(&facts, ORCH_PLAN_KIND, run) {
                Ok(Some(v)) => match serde_json::from_str::<serde_json::Value>(&v) {
                    Ok(j) => Some(j),
                    Err(_) => {
                        return Err(internal_graph_err(format!(
                        "stored row {ORCH_PLAN_KIND}/{run} of session {parent} is not valid JSON"
                    )));
                    }
                },
                Ok(None) => None,
                Err(m) => return Err(internal_graph_err(m)),
            };
        let default_model = plan_json
            .as_ref()
            .and_then(|v| v.get("default_model"))
            .and_then(|m| m.as_str())
            .map(str::to_string);
        let plan_goal = plan_json
            .as_ref()
            .and_then(|v| v.get("plan"))
            .and_then(|p| p.get("goal"))
            .and_then(|g| g.as_str())
            .unwrap_or("")
            .to_string();
        // The typed plan for the SHARED aggregation. A missing plan row is
        // the empty plan (child rows without plan items own no step state).
        let plan = match plan_json.as_ref().and_then(|v| v.get("plan").cloned()) {
            Some(p) => match faktor_orchestrator::TaskPlan::from_legacy_value(p) {
                Ok(p) => p,
                Err(m) => {
                    return Err(internal_graph_err(format!(
                        "stored plan row {ORCH_PLAN_KIND}/{run} of session {parent} is invalid: {m}"
                    )));
                }
            },
            None => faktor_orchestrator::TaskPlan {
                goal: String::new(),
                non_goals: Vec::new(),
                constraints: Vec::new(),
                work_items: Vec::new(),
            },
        };

        // Every durable child row of this run (typed; tampered rows loud).
        // The bucket holds exactly the rows the historical
        // `strip_prefix("<run>/")` facts scan yielded, in facts order.
        let mut children: Vec<(i64, String, faktor_orchestrator::runtime::ChildRuntime)> =
            Vec::new();
        for (key, _value) in index.registry_rows_for_run(run).into_iter().flatten() {
            let rest = &key[run.len() + 1..];
            if rest.is_empty() || rest.contains('/') {
                return Err(internal_graph_err(format!(
                    "hostile registry row key {key:?} under run {run}"
                )));
            }
            let value = match index.row_value(&facts, ORCH_REGISTRY_KIND, key) {
                Ok(Some(v)) => v,
                Ok(None) => continue,
                Err(m) => return Err(internal_graph_err(m)),
            };
            let row: faktor_orchestrator::runtime::ChildRuntime =
                serde_json::from_str(&value).map_err(|_| {
                    internal_graph_err(format!(
                        "stored row {ORCH_REGISTRY_KIND}/{key} of session {parent} is not valid JSON"
                    ))
                })?;
            children.push((row.created_ms, rest.to_string(), row));
        }
        children.sort_by_key(|a| (a.0, a.1.clone()));

        // The parent's own task run: the SHARED aggregation over the same
        // durable rows the JSON graph endpoint serves (one projection, no
        // second derivation can drift).
        let rows: Vec<faktor_orchestrator::runtime::ChildRuntime> =
            children.iter().map(|(_, _, row)| row.clone()).collect();
        let derived = faktor_orchestrator::derived_item_states(&plan, &rows);
        let root_state = faktor_orchestrator::derived_root_state(&derived);
        let owner_wt = handle
            .row()
            .map_err(|e| faktor_protocol::error::from_core(&e))?
            .worktree_id
            .raw();
        let progress = state.deps.agent.progress_view(parent);
        push(serde_json::json!({
            "agent_id": run,
            "kind": "self",
            "run_id": run,
            "session_id": parent.raw(),
            "worktree_id": owner_wt,
            "goal": plan_goal,
            "item_ids": plan.work_items.iter().map(|w| w.id.clone()).collect::<Vec<_>>(),
            "state": work_state_tag(root_state),
            "model": serde_json::Value::Null,
            "budget": serde_json::Value::Null,
            "ownership": "self",
            "capabilities": [],
            "progress": progress,
            "result": serde_json::Value::Null,
        }));
        for (_created, _child_id, row) in children {
            push(native_child_entry(
                state,
                &index,
                run,
                row,
                default_model.as_deref(),
            )?);
        }
    }
    Ok(entries)
}

/// Shared error mapping of one orchestrator control into the wire.
pub(crate) fn exec_error_response(e: &faktor_orchestrator::runtime::ExecError) -> Response {
    let (code, status, retryable) = match e {
        faktor_orchestrator::runtime::ExecError::NotFound(_) => ("not_found", 404, false),
        faktor_orchestrator::runtime::ExecError::Conflict(_) => ("conflict", 409, false),
        faktor_orchestrator::runtime::ExecError::InvalidState(_)
        | faktor_orchestrator::runtime::ExecError::WorkspaceDrift(_) => ("conflict", 409, false),
        faktor_orchestrator::runtime::ExecError::CeilingExceeded { .. } => {
            ("ceiling_exceeded", 429, false)
        }
        faktor_orchestrator::runtime::ExecError::Oversized(_)
        | faktor_orchestrator::runtime::ExecError::Malformed(_)
        | faktor_orchestrator::runtime::ExecError::InvalidPlan(_)
        | faktor_orchestrator::runtime::ExecError::InvalidApproval(_)
        | faktor_orchestrator::runtime::ExecError::UndecidedPaths(_) => ("malformed", 400, false),
        // An authoritative durable write / control ack failed. The control
        // was NOT claimed applied and the same call may be safely retried:
        // surface that as a typed RETRIABLE service failure, never a silent
        // success and never a permanent internal error.
        faktor_orchestrator::runtime::ExecError::RetriablePersistence { .. } => {
            ("persistence_failed", 503, true)
        }
        _ => ("internal", 500, false),
    };
    let e = ApiError {
        code,
        message: e.to_string(),
        http_status: status,
        retryable,
    };
    wire_status(e)
}

/// One control enqueue on a child of the active execution. Hostile/unknown
/// child ids are typed 404; terminal-state refusals are 409; the durable
/// row's exactly-once state answers as `{queuedSeq, applied}`.
pub(crate) fn agent_control(
    state: &AppState,
    child_id: &str,
    control: faktor_session::child::ChildControl,
) -> Response {
    match state.deps.orchestrator.control_child(child_id, control) {
        Ok(ack) => Json(serde_json::json!({
            "queuedSeq": ack.queued_seq,
            "applied": ack.applied,
        }))
        .into_response(),
        Err(e) => exec_error_response(&e),
    }
}

/// Whether the model selector is served by a registered provider (the
/// catalog the `/models` endpoint lists).
pub(crate) fn model_known(state: &AppState, model: &str) -> bool {
    state
        .deps
        .agent
        .deps()
        .providers
        .all()
        .iter()
        .any(|p| p.known_models().iter().any(|m| m == model))
}

/// `GET /native/agents?session=<id>` — see [`native_agents_body`].
/// Hostile/missing session ids are typed 404 (a listing without a session
/// is never an empty phantom).
pub(crate) async fn native_agents(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeAgentsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let Ok(raw) = q.session.parse::<u64>() else {
        return wire_status(not_found(&format!("invalid session id {:?}", q.session)));
    };
    if raw == 0 {
        return wire_status(not_found("session id cannot be 0"));
    }
    let sid = SessionId::new(raw);
    let handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => return wire_status(not_found(&format!("session {sid}"))),
        Err(e) => return api_err(&e),
    };
    match native_agents_body(&state, &handle) {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => wire_status(e),
    }
}

// --------------------------------------------------- native task runs (wave-24)
// The ONE task-start surface of the daemon's HTTP layer: POST starts a task
// through `state.deps.tasks.start_task` (the TaskExecutor — the same single
// authority the daemon graph wires; there is no second start architecture
// reachable from the server), GET lists the session's durable task runs
// with per-run state, and the per-run GET/cancel read and drive one run.
// Request bodies parse with the strict native DTOs (deny_unknown_fields —
// a typo or hostile value is a 400). Unknown sessions and unknown runs are
// typed 404s; run-level conflicts (terminal runs, live crash residue) are
// typed 409s.

/// `GET /native/agents/{child_id}/pause` — durable Pause control; applied
/// at the child's next safe reasoning boundary (`applied: null` until the
/// drive acks it exactly once).
pub(crate) async fn native_agent_pause(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    agent_control(
        &state,
        &child_id,
        faktor_session::child::ChildControl::Pause,
    )
}

pub(crate) async fn native_agent_resume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    agent_control(
        &state,
        &child_id,
        faktor_session::child::ChildControl::Resume,
    )
}

pub(crate) async fn native_agent_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    agent_control(
        &state,
        &child_id,
        faktor_session::child::ChildControl::Cancel,
    )
}

pub(crate) async fn native_agent_retry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    agent_control(
        &state,
        &child_id,
        faktor_session::child::ChildControl::Retry,
    )
}

/// `POST /native/agents/{child_id}/steer` — body `{"text": ...}` (bounded
/// note, durable row, applied at the next boundary).
pub(crate) async fn native_agent_steer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
    body: Result<Json<NativeSteerBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native steer body")),
    };
    agent_control(
        &state,
        &child_id,
        faktor_session::child::ChildControl::Steer { note: body.text },
    )
}

/// `POST /native/agents/{child_id}/model` — body `{"model": "..."}`. The
/// selector must be served by a registered provider (catalog check), else
/// a typed 404. Takes effect at the next provider selection.
pub(crate) async fn native_agent_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
    body: Result<Json<NativeModelBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native model body")),
    };
    if !model_known(&state, &body.model) {
        let e = ApiError {
            code: "not_found",
            message: format!(
                "model {:?} is not served by any registered provider",
                body.model
            ),
            http_status: 404,
            retryable: false,
        };
        return wire_status(e);
    }
    agent_control(
        &state,
        &child_id,
        faktor_session::child::ChildControl::ChangeModel { model: body.model },
    )
}

/// `POST /native/agents/{child_id}/budget` — body is EXACTLY ONE of
/// `{"max_tokens": N}` or `{"max_cost_micro": N}` (deny_unknown_fields;
/// hostile bodies are typed 400s, 0 on either axis is refused as ambiguous —
/// the store reads a NULL/0 cap as "unlimited", so an explicit zero cap can
/// never mean "remove the cap"). The body maps onto the typed budget-change
/// vocabulary ([`faktor_session::child::ChildBudgetChange`]); each axis is a
/// synchronous durable effect:
/// - `ChangeTokenBudget` → the historic token path: the wave-9 task-row
///   `max_tokens` patch, delivered exactly once through the `ChangeBudget`
///   control queue (acked at enqueue; `applied: true`);
/// - `ChangeCostBudget` → the child cost-cap effect (audit 9/H): the child's
///   durable task-row `max_cost_micro` plus its budget-scope enrollment under
///   the run's root row (root + child subscope admission). A direct durable
///   effect — no drive-boundary consumer exists for it, so no queue row is
///   written (`queuedSeq: null`).
pub(crate) async fn native_agent_budget(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
    body: Result<Json<NativeBudgetBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native budget body")),
    };
    let change = match (body.max_tokens, body.max_cost_micro) {
        (Some(max_tokens), None) => {
            faktor_session::child::ChildBudgetChange::ChangeTokenBudget { max_tokens }
        }
        (None, Some(max_cost_micro)) => {
            faktor_session::child::ChildBudgetChange::ChangeCostBudget { max_cost_micro }
        }
        _ => {
            return wire_status(malformed_body(
                "send exactly one of max_tokens or max_cost_micro",
            ));
        }
    };
    if let Err(e) = change.validate() {
        return wire_status(malformed_body(&e.to_string()));
    }
    match change {
        faktor_session::child::ChildBudgetChange::ChangeTokenBudget { max_tokens } => {
            agent_control(
                &state,
                &child_id,
                faktor_session::child::ChildControl::ChangeBudget { max_tokens },
            )
        }
        faktor_session::child::ChildBudgetChange::ChangeCostBudget { max_cost_micro } => {
            child_cost_cap_change(&state, &child_id, max_cost_micro)
        }
    }
}

/// The durable registry row of ONE child scoped to `handle`'s own row
/// space: the parent's `orchestrator_registry` rows keyed `<run>/<child>`.
/// The path session is the authority — a child id that only exists under
/// another session is a typed 404, never resolved globally. Corrupt rows
/// are loud 500s.
pub(crate) fn durable_child_row(
    handle: &faktor_session::SessionHandle,
    child_id: &str,
) -> Result<faktor_orchestrator::runtime::ChildRuntime, ApiError> {
    if child_id.is_empty()
        || child_id.len() > 64
        || !child_id.is_ascii()
        || child_id.contains('/')
        || child_id.contains('\\')
        || child_id.chars().any(|c| c.is_control())
    {
        return Err(not_found(&format!("unknown child {child_id:?}")));
    }
    let facts = orchestrator_graph_facts(handle).map_err(internal_graph_err)?;
    for (kind, key, _) in &facts {
        if kind != ORCH_REGISTRY_KIND {
            continue;
        }
        let Some((_run, rest)) = key.rsplit_once('/') else {
            continue;
        };
        if rest != child_id {
            continue;
        }
        let value = match orchestrator_row_value(&facts, ORCH_REGISTRY_KIND, key) {
            Ok(Some(v)) => v,
            Ok(None) => continue,
            Err(m) => return Err(internal_graph_err(m)),
        };
        let row: faktor_orchestrator::runtime::ChildRuntime = serde_json::from_str(&value)
            .map_err(|_| {
                internal_graph_err(format!(
                    "stored row {ORCH_REGISTRY_KIND}/{key} is not a valid child runtime row"
                ))
            })?;
        if row.child_id != child_id {
            return Err(internal_graph_err(format!(
                "stored row {ORCH_REGISTRY_KIND}/{key} names child {:?}",
                row.child_id
            )));
        }
        if row.parent_session_id != handle.id().raw() {
            return Err(internal_graph_err(format!(
                "stored row {ORCH_REGISTRY_KIND}/{key} names parent session {} under session {}",
                row.parent_session_id,
                handle.id()
            )));
        }
        return Ok(row);
    }
    Err(not_found(&format!(
        "unknown child {child_id} of session {}",
        handle.id()
    )))
}

/// `POST /native/session/{id}/agents/{child}/presentation` — record one
/// durable presentation transition of a child of THIS session
/// (`{"state":"foreground"|"background"}`, strict DTO). Presentation is an
/// attention concept only: it never changes scheduling ownership, budgets
/// or lineage, and the same ChildId/session/worktree continues. The
/// transition is idempotent (a same-state set answers `changed:false` with
/// no durable row); a TERMINAL child is Background-only, so a foreground
/// revival is a typed 409. Hostile/unknown child ids and unknown sessions
/// are 404s; hostile bodies are 400s.
pub(crate) async fn native_agent_presentation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, child)): Path<(String, String)>,
    body: Result<Json<NativePresentationBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native presentation body")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let row = match durable_child_row(&handle, &child) {
        Ok(r) => r,
        Err(e) => return wire_status(e),
    };
    let child_session = match state
        .deps
        .session
        .get_session(SessionId::new(row.session_id))
    {
        Ok(Some(h)) => h,
        Ok(None) => return wire_status(not_found(&format!("child session {}", row.session_id))),
        Err(e) => return api_err(&e),
    };
    // Idempotent no-op first: repeating the child's current state answers
    // `changed:false` and writes nothing, even for a terminal child.
    let current = match child_session.child_presentation(&row.child_id) {
        Ok(p) => p,
        Err(e) => return api_err(&e),
    };
    if current == body.state {
        return Json(serde_json::json!({
            "child_id": row.child_id,
            "presentation": current,
            "changed": false,
        }))
        .into_response();
    }
    // Terminal children are Background-only: refuse the foreground revival
    // BEFORE any write (the session-layer rule enforces the same invariant
    // against the child session's own state).
    if row.state.is_terminal() && body.state == faktor_session::child::PresentationState::Foreground
    {
        return wire_status(ApiError {
            code: "conflict",
            message: format!(
                "cannot foreground child {child}: state {:?} is terminal (terminal children may be Background only)",
                row.state
            ),
            http_status: 409,
            retryable: false,
        });
    }
    match child_session.set_child_presentation(&row.child_id, body.state) {
        Ok(changed) => {
            let presentation = child_session
                .child_presentation(&row.child_id)
                .unwrap_or(body.state);
            Json(serde_json::json!({
                "child_id": row.child_id,
                "presentation": presentation,
                "changed": changed.is_some(),
            }))
            .into_response()
        }
        Err(e) => api_err(&e),
    }
}

/// The synchronous durable effect of a ChangeCostBudget on one child (see
/// [`native_agent_budget`]): the child's task-row cost cap is patched and
/// its budget scope is enrolled under the run root through the durable
/// ledger. Terminal children refuse (409, mirroring the ChangeBudget
/// guard); unknown children are typed 404s.
pub(crate) fn child_cost_cap_change(
    state: &AppState,
    child_id: &str,
    max_cost_micro: u64,
) -> Response {
    let row = match state.deps.orchestrator.child(child_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            let e = ApiError {
                code: "not_found",
                message: format!("unknown child {child_id}"),
                http_status: 404,
                retryable: false,
            };
            return wire_status(e);
        }
        Err(e) => return exec_error_response(&e),
    };
    let session = match state
        .deps
        .session
        .get_session(SessionId::new(row.session_id))
    {
        Ok(Some(h)) => h,
        Ok(None) => {
            return wire_status(ApiError {
                code: "not_found",
                message: format!("child session {}", row.session_id),
                http_status: 404,
                retryable: false,
            });
        }
        Err(e) => return api_err(&e),
    };
    let terminal = match session.state() {
        Ok(s) => s.is_terminal(),
        Err(e) => return api_err(&e),
    };
    if row.state.is_terminal() || terminal {
        let e = ApiError {
            code: "conflict",
            message: format!("cannot change the budget of {child_id}: state is terminal"),
            http_status: 409,
            retryable: false,
        };
        return wire_status(e);
    }
    match state.deps.budgets.change_child_scope_cap(
        SessionId::new(row.session_id),
        child_id,
        Some(max_cost_micro),
    ) {
        Ok(()) => Json(serde_json::json!({
            "queuedSeq": serde_json::Value::Null,
            "applied": true,
        }))
        .into_response(),
        Err(e) => {
            let core: faktor_core::Error = e.into();
            api_err(&core)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Perf-wave evidence for [`FactIndex`]: every indexed lookup must be
    /// EQUAL to the historical per-item scans (row value, run child set,
    /// merge winner) over an adversarial corpus — duplicates, a chunked
    /// row, overlapping/hostile keys — and the indexed trajectory must beat
    /// the historical O(runs · facts + children · facts) one at the
    /// realistic 5k-row size.
    #[test]
    fn fact_index_matches_the_per_item_scans_and_is_cheaper() {
        fn old_child_rows<'a>(
            facts: &'a [(String, String, String)],
            run: &str,
        ) -> Vec<(&'a str, &'a str)> {
            let prefix = format!("{run}/");
            let mut out = Vec::new();
            for (kind, key, value) in facts {
                if kind != ORCH_REGISTRY_KIND {
                    continue;
                }
                if key.strip_prefix(&prefix).is_none() {
                    continue;
                }
                out.push((key.as_str(), value.as_str()));
            }
            out
        }
        fn old_merge_best<'a>(
            facts: &'a [(String, String, String)],
            run: &str,
            child: &str,
        ) -> Option<(u64, &'a str)> {
            let prefix = format!("{run}/{child}/merge/");
            let mut best: Option<(u64, &'a str)> = None;
            for (kind, key, value) in facts {
                if kind != ORCH_MERGE_KIND {
                    continue;
                }
                if let Some(rest) = key.strip_prefix(&prefix) {
                    if let Some((_cs, seq_s)) = rest.rsplit_once('/') {
                        if let Ok(seq) = seq_s.parse::<u64>() {
                            if best.map(|(b, _)| seq > b).unwrap_or(true) {
                                best = Some((seq, value));
                            }
                        }
                    }
                }
            }
            best
        }

        let runs = 250usize;
        let per_run = 4usize;
        let mut facts: Vec<(String, String, String)> = Vec::new();
        for r in 0..runs {
            let run = format!("run-{r:04}");
            facts.push((
                ORCH_PLAN_KIND.into(),
                run.clone(),
                format!("{{\"plan\":{{\"goal\":\"g{r}\"}}}}"),
            ));
            facts.push((
                TASK_RUN_ROW_KIND.into(),
                run.clone(),
                format!("{{\"goal\":\"t{r}\"}}"),
            ));
            for c in 0..per_run {
                let key = format!("{run}/child-{c}");
                facts.push((
                    ORCH_REGISTRY_KIND.into(),
                    key.clone(),
                    format!(
                        "{{\"child_id\":\"child-{c}\",\"session_id\":{}}}",
                        r * 10 + c + 1
                    ),
                ));
                for seq in 1..=3u64 {
                    facts.push((
                        ORCH_MERGE_KIND.into(),
                        format!("{key}/merge/cs-{c}/{seq}"),
                        format!("{{\"cs_id\":\"cs-{c}\",\"merged_count\":{seq}}}"),
                    ));
                    facts.push((
                        ORCH_MERGE_PART_KIND.into(),
                        format!("{key}/merge/cs-{c}/{seq}/part/merged"),
                        format!("[\"src/f{seq}.rs\"]"),
                    ));
                }
            }
        }
        // Adversarial rows: a duplicate registry key (FIRST value wins for
        // row_value; both rows are children for the run scan), a chunked
        // registry row (header + chunk), a hostile key with an extra slash
        // (refused by the caller's rest check, still bucketed exactly like
        // the prefix scan), and a double `merge` key.
        let hostile_run = "run-0000";
        facts.push((
            ORCH_REGISTRY_KIND.into(),
            format!("{hostile_run}/child-0"),
            "{\"duplicate\":true}".into(),
        ));
        facts.push((
            ORCH_REGISTRY_KIND.into(),
            format!("{hostile_run}/chunked"),
            "{\"chunks\":1}".into(),
        ));
        facts.push((
            ORCH_REGISTRY_KIND.into(),
            format!("{hostile_run}/chunked/c000"),
            "{\"chunk\":\"body\"}".into(),
        ));
        facts.push((
            ORCH_REGISTRY_KIND.into(),
            format!("{hostile_run}/bad/extra"),
            "{}".into(),
        ));
        facts.push((
            ORCH_MERGE_KIND.into(),
            format!("{hostile_run}/child-0/merge/cs-9/5/merge/cs-x/42"),
            "{\"cs_id\":\"cs-x\",\"merged_count\":42}".into(),
        ));

        let index = FactIndex::new(&facts);
        // Row values: every (kind, key) lookup agrees with the scan.
        for (kind, key, _) in &facts {
            assert_eq!(
                index.row_value(&facts, kind, key).unwrap(),
                orchestrator_row_value(&facts, kind, key).unwrap(),
                "row value drift at {kind}/{key}"
            );
        }
        assert_eq!(index.row_value(&facts, "missing", "row").unwrap(), None);
        // Child sets: exactly the historical prefix scan (order included).
        for r in 0..runs {
            let run = format!("run-{r:04}");
            let naive = old_child_rows(&facts, &run);
            let indexed: Vec<(&str, &str)> = index
                .registry_rows_for_run(&run)
                .into_iter()
                .flatten()
                .copied()
                .collect();
            assert_eq!(naive, indexed, "child set drift for {run}");
        }
        // Merge winners: exactly the historical per-child scan.
        let mut checked = 0usize;
        for r in 0..runs {
            let run = format!("run-{r:04}");
            for c in 0..(per_run + 2) {
                let child = format!("child-{c}");
                assert_eq!(
                    old_merge_best(&facts, &run, &child),
                    index.merge_best(&run, &child),
                    "merge winner drift for {run}/{child}"
                );
                checked += 1;
            }
        }
        assert!(checked > runs);
        assert_eq!(
            old_merge_best(&facts, hostile_run, "child-0"),
            index.merge_best(hostile_run, "child-0"),
            "the double-merge key attributes exactly like the scan"
        );

        // Before/after probe: the historical trajectory scans ALL facts per
        // run and per child; the indexed one builds once and probes.
        let t0 = std::time::Instant::now();
        let mut old_acc = 0usize;
        for r in 0..runs {
            let run = format!("run-{r:04}");
            for (key, _) in old_child_rows(&facts, &run) {
                old_acc += orchestrator_row_value(&facts, ORCH_REGISTRY_KIND, key)
                    .unwrap()
                    .map(|v| v.len())
                    .unwrap_or(0);
                if let Some((_seq, value)) = old_merge_best(&facts, &run, "child-0") {
                    old_acc += value.len();
                }
            }
        }
        let old_elapsed = t0.elapsed();
        let t1 = std::time::Instant::now();
        let index = FactIndex::new(&facts);
        let mut new_acc = 0usize;
        for r in 0..runs {
            let run = format!("run-{r:04}");
            for (key, _) in index.registry_rows_for_run(&run).into_iter().flatten() {
                new_acc += index
                    .row_value(&facts, ORCH_REGISTRY_KIND, key)
                    .unwrap()
                    .map(|v| v.len())
                    .unwrap_or(0);
                if let Some((_seq, value)) = index.merge_best(&run, "child-0") {
                    new_acc += value.len();
                }
            }
        }
        let new_elapsed = t1.elapsed();
        assert_eq!(old_acc, new_acc);
        eprintln!(
            "agents listing index ({} facts, {runs} runs x {per_run} children): per-item scans {old_elapsed:?} -> one-pass index {new_elapsed:?}",
            facts.len()
        );
        assert!(
            new_elapsed < old_elapsed,
            "the one-pass index must beat per-item scans: old {old_elapsed:?} new {new_elapsed:?}"
        );
    }

    #[test]
    fn workspace_drift_is_a_typed_conflict_not_an_internal_error() {
        let resp = exec_error_response(&faktor_orchestrator::runtime::ExecError::WorkspaceDrift(
            "symlink escape rejected".into(),
        ));
        assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
    }
}
