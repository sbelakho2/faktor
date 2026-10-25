//! Native API module split from foundation_ext.rs.

use super::foundation::foundation_error;
use super::foundation_ext::{conflict, foundation, resolve_handle};
use super::*;

/// Durable settings revision fact (F071): a monotonically bumped per-session
/// integer written only by a successful settings update. It is the fence a
/// second editor submits; the actual settings live in the foundation store.
const SESSION_SETTINGS_FACT_KIND: &str = "session_settings";
const SESSION_SETTINGS_REVISION_KEY: &str = "revision";
const SESSION_SETTINGS_CONTEXT_WINDOW_KEY: &str = "contextWindow";
/// Mirror of the accepted per-session compaction model (`provider/model`)
/// the agent runtime reads at its next compaction. Empty/absent = inherit.
const SESSION_SETTINGS_COMPACTION_MODEL_KEY: &str = "compactionModel";

fn session_settings_revision(handle: &faktor_session::SessionHandle) -> u64 {
    handle
        .memory_fact_get(SESSION_SETTINGS_FACT_KIND, SESSION_SETTINGS_REVISION_KEY)
        .ok()
        .flatten()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeSessionSettingsBody {
    #[serde(default)]
    compaction_model: Option<String>,
    /// Optimistic fence: when present it must equal the durable settings
    /// revision the editor read (F071). A stale value is a typed 409 and
    /// NOTHING is applied.
    #[serde(default)]
    expected_revision: Option<u64>,
    /// Context-window override in tokens (F046), clamped DOWN to the
    /// model/runtime limit at the next turn.
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_turns: Option<u64>,
    #[serde(default)]
    max_consecutive_failures: Option<u64>,
    #[serde(default)]
    unset: Vec<String>,
}

/// `GET /native/session/{id}/settings` — durable selectable settings
/// (compaction model, task turn bound, consecutive-failure guard).
pub(crate) async fn native_session_settings_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let session = handle.id().raw();
    let revision = session_settings_revision(&handle);
    let compaction_model = foundation.session_settings.get(
        session,
        faktor_foundation::session_settings::COMPACTION_MODEL_KEY,
    );
    let context_window = foundation.session_settings.get(
        session,
        faktor_foundation::session_settings::CONTEXT_WINDOW_KEY,
    );
    let max_turns = foundation
        .session_settings
        .get(session, faktor_foundation::session_settings::MAX_TURNS_KEY);
    let max_failures = foundation.session_settings.get(
        session,
        faktor_foundation::session_settings::MAX_CONSECUTIVE_FAILURES_KEY,
    );
    match (compaction_model, context_window, max_turns, max_failures) {
        (Ok(compaction_model), Ok(context_window), Ok(max_turns), Ok(max_consecutive_failures)) => {
            Json(serde_json::json!({
                "sessionId": id,
                "revision": revision,
                "compactionModel": compaction_model,
                "contextWindow": context_window,
                "maxTurns": max_turns,
                "maxConsecutiveFailures": max_consecutive_failures,
                "applyTiming": {
                    "compactionModel": "next_compaction",
                    "contextWindow": "next_turn",
                    "maxTurns": "next_task",
                    "maxConsecutiveFailures": "next_task",
                },
            }))
            .into_response()
        }
        (Err(e), _, _, _) | (_, Err(e), _, _) | (_, _, Err(e), _) | (_, _, _, Err(e)) => {
            foundation_error(e)
        }
    }
}

/// `PUT /native/session/{id}/settings` — atomic validated update. Unknown
/// keys, out-of-range caps and unserved compaction models are typed 400/409.
pub(crate) async fn native_session_settings_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeSessionSettingsBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid session settings body")),
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let _settings_guard = state.deps.settings_lock.lock().await;
    let current_revision = session_settings_revision(&handle);
    if let Some(expected) = body.expected_revision {
        if expected != current_revision {
            return conflict(
                "settings_revision_stale",
                format!(
                    "settings revision {expected} is stale (current {current_revision}); nothing was applied"
                ),
            );
        }
    }
    let mut applied = Vec::new();
    let session = handle.id().raw();
    if let Some(model) = body.compaction_model.as_deref() {
        if model == "inherit" {
            let _ = foundation.session_settings.clear(
                session,
                faktor_foundation::session_settings::COMPACTION_MODEL_KEY,
            );
            if let Err(e) = handle.upsert_memory_fact(
                SESSION_SETTINGS_FACT_KIND,
                SESSION_SETTINGS_COMPACTION_MODEL_KEY,
                "",
            ) {
                return api_err(&e);
            }
            applied.push("compactionModel=inherit");
        } else {
            let Some((provider, model_id)) = model.split_once('/') else {
                return wire_status(malformed_body(
                    "compactionModel must be `inherit` or `provider/model`",
                ));
            };
            if state
                .deps
                .agent
                .deps()
                .providers
                .capabilities(provider, model_id)
                .is_none()
            {
                return conflict(
                    "unserved_compaction_model",
                    format!("compaction model {model:?} is not served by this daemon"),
                );
            }
            if let Err(e) = foundation.session_settings.set(
                session,
                faktor_foundation::session_settings::COMPACTION_MODEL_KEY,
                &serde_json::Value::String(model.to_string()),
            ) {
                return foundation_error(e);
            }
            // The AGENT reads its effective compaction model from the
            // session's durable fact space (the foundation store is not
            // reachable from the agent runtime). Mirror the accepted value;
            // an absent/empty fact inherits the daemon config, exactly like
            // `inherit`. Without this the setting was accepted, reported as
            // `next_compaction`, and silently ignored by every compaction.
            if let Err(e) = handle.upsert_memory_fact(
                SESSION_SETTINGS_FACT_KIND,
                SESSION_SETTINGS_COMPACTION_MODEL_KEY,
                model,
            ) {
                return api_err(&e);
            }
            applied.push("compactionModel");
        }
    }
    if let Some(tokens) = body.context_window {
        if !(faktor_foundation::session_settings::MIN_CONTEXT_WINDOW_TOKENS
            ..=faktor_foundation::session_settings::MAX_CONTEXT_WINDOW_TOKENS)
            .contains(&tokens)
        {
            return wire_status(malformed_body(
                "contextWindow must be within the documented token bounds",
            ));
        }
        if let Err(e) = foundation.session_settings.set(
            session,
            faktor_foundation::session_settings::CONTEXT_WINDOW_KEY,
            &serde_json::json!(tokens),
        ) {
            return foundation_error(e);
        }
        if let Err(e) = handle.upsert_memory_fact(
            SESSION_SETTINGS_FACT_KIND,
            SESSION_SETTINGS_CONTEXT_WINDOW_KEY,
            &tokens.to_string(),
        ) {
            return api_err(&e);
        }
        applied.push("contextWindow");
    }
    if let Some(turns) = body.max_turns {
        if let Err(e) = foundation.session_settings.set(
            session,
            faktor_foundation::session_settings::MAX_TURNS_KEY,
            &serde_json::json!(turns),
        ) {
            return foundation_error(e);
        }
        applied.push("maxTurns");
    }
    if let Some(failures) = body.max_consecutive_failures {
        if let Err(e) = foundation.session_settings.set(
            session,
            faktor_foundation::session_settings::MAX_CONSECUTIVE_FAILURES_KEY,
            &serde_json::json!(failures),
        ) {
            return foundation_error(e);
        }
        applied.push("maxConsecutiveFailures");
    }
    for key in &body.unset {
        let storage_key = match key.as_str() {
            "compactionModel" => faktor_foundation::session_settings::COMPACTION_MODEL_KEY,
            "contextWindow" => faktor_foundation::session_settings::CONTEXT_WINDOW_KEY,
            "maxTurns" => faktor_foundation::session_settings::MAX_TURNS_KEY,
            "maxConsecutiveFailures" => {
                faktor_foundation::session_settings::MAX_CONSECUTIVE_FAILURES_KEY
            }
            _ => {
                return wire_status(ApiError {
                    code: "malformed",
                    message: format!("unknown setting {key:?}"),
                    http_status: 400,
                    retryable: false,
                })
            }
        };
        if let Err(e) = foundation.session_settings.clear(session, storage_key) {
            return foundation_error(e);
        }
        if key == "contextWindow" {
            if let Err(e) = handle.upsert_memory_fact(
                SESSION_SETTINGS_FACT_KIND,
                SESSION_SETTINGS_CONTEXT_WINDOW_KEY,
                "",
            ) {
                return api_err(&e);
            }
        }
        if key == "compactionModel" {
            if let Err(e) = handle.upsert_memory_fact(
                SESSION_SETTINGS_FACT_KIND,
                SESSION_SETTINGS_COMPACTION_MODEL_KEY,
                "",
            ) {
                return api_err(&e);
            }
        }
        applied.push(key.as_str());
    }
    let revision = if applied.is_empty() {
        current_revision
    } else {
        let next = current_revision.saturating_add(1);
        if let Err(e) = handle.upsert_memory_fact(
            SESSION_SETTINGS_FACT_KIND,
            SESSION_SETTINGS_REVISION_KEY,
            &next.to_string(),
        ) {
            return api_err(&e);
        }
        next
    };
    Json(serde_json::json!({
        "sessionId": id,
        "revision": revision,
        "applied": applied,
        "applyTiming": {
            "compactionModel": "next_compaction",
            "maxTurns": "next_task",
            "maxConsecutiveFailures": "next_task",
        },
    }))
    .into_response()
}
