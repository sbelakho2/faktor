use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::json;

use crate::AppState;

pub async fn health(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    let db_ok = state.db.conn().is_ok();
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "status": if db_ok { "ok" } else { "degraded" },
            "schema": 1,
            "billing_provider": state.payments.provider_kind().as_str(),
            "billing": state.payments.provider_status().state,
        })),
    )
}
