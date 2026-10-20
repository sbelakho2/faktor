//! Checkpoint coverage native route (F200), split out of `session.rs`.

use super::*;

/// `GET /native/session/{id}/checkpoints/coverage` — the checkpoint
/// coverage report (F200): every path the journal covers, plus every
/// completed shell command whose external effect is `unknown` and therefore
/// NOT checkpoint-covered. The report never claims a shell effect was
/// captured; such runs force verification.
pub(crate) async fn native_checkpoint_coverage(
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
    const MAX_COVERAGE_RUNS: usize = 200;
    let checkpoints = match handle.checkpoints_of() {
        Ok(rows) => rows,
        Err(e) => return api_err(&e),
    };
    let mut covered: Vec<String> = checkpoints
        .iter()
        .map(|row| row.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    covered.sort();
    let runs = match handle.completed_tool_runs(MAX_COVERAGE_RUNS) {
        Ok(runs) => runs,
        Err(e) => return api_err(&e),
    };
    let uncovered: Vec<serde_json::Value> = runs
        .iter()
        .filter(|row| {
            row.tool == "run_command" || row.effect_status == "unknown"
        })
        .map(|row| {
            serde_json::json!({
                "kind": "shell_effect",
                "opId": row.op_id.raw(),
                "tool": row.tool,
                "status": row.status,
                "effectStatus": row.effect_status,
                "startedMs": row.started_ms,
                "endedMs": row.ended_ms,
                "covered": false,
                "reason": "a shell command's external effects are never known to the checkpoint journal; verification is required",
            })
        })
        .collect();
    let truncated = runs.len() >= MAX_COVERAGE_RUNS;
    Json(serde_json::json!({
        "sessionId": id,
        "covered": {
            "paths": covered,
            "checkpoints": checkpoints.len(),
            "throughSequence": checkpoints.iter().map(|c| c.sequence).max(),
        },
        "uncovered": uncovered,
        "complete": uncovered.is_empty(),
        "truncated": truncated,
        "policy": "file edits are checkpointed by path; shell commands are reported as uncovered shell effects and force verification",
    }))
    .into_response()
}
