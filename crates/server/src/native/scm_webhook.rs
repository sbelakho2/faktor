//! The native SCM webhook route: the ONE unauthenticated native surface,
//! because the sender (a source-control provider) cannot present the daemon
//! password — the request is authenticated by the wired sink's HMAC
//! signature verification and claimed exactly once through the durable
//! inbox before anything is applied.
//!
//! With no sink wired (`[cloud.github_app]` disabled, the default) the route
//! answers a typed 409 `scm_webhook_disabled` and the daemon is otherwise
//! byte-identical. A verified delivery is answered 200 with the ingest
//! outcome; a duplicate delivery id is answered 200 `duplicate` and never
//! re-applied. Nothing about the delivery is logged with its body.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_protocol::error::ApiError;
use faktor_scm::{IngestOutcome, WebhookError, WebhookHeaders};

use super::wire_status;
use crate::api::AppState;

/// The host-wired webhook sink: verifies, claims and (idempotently)
/// schedules the delivery's re-sync. Implementations must not block; a
/// saturation refusal is a retryable store error so the provider redelivers.
pub trait WebhookSink: Send + Sync {
    fn deliver(&self, headers: &WebhookHeaders, body: &[u8])
        -> Result<IngestOutcome, WebhookError>;
}

fn webhook_disabled() -> ApiError {
    ApiError {
        code: "scm_webhook_disabled",
        message: "no SCM webhook sink is wired into this daemon \
                  (enable [cloud.github_app] to wire the GitHub App inbox)"
            .into(),
        http_status: 409,
        retryable: false,
    }
}

/// Map one typed webhook refusal onto the frozen API error surface. A
/// verification refusal is an authentication failure (the signature IS the
/// credential); a malformed payload is a terminal client error; store
/// unavailability is retryable so the provider redelivers. Retryability is
/// taken from the typed error itself, never re-derived here.
fn webhook_err(e: WebhookError) -> ApiError {
    let (code, http_status) = match &e {
        WebhookError::BodyTooLarge => ("payload_too_large", 413),
        WebhookError::MissingSignature
        | WebhookError::MalformedSignature
        | WebhookError::SignatureMismatch
        | WebhookError::StaleTimestamp => ("scm_webhook_unauthorized", 401),
        WebhookError::MissingDeliveryId | WebhookError::MalformedPayload(_) => ("malformed", 400),
        WebhookError::Store(_) => ("scm_webhook_unavailable", 503),
    };
    ApiError {
        code,
        message: e.to_string(),
        http_status,
        retryable: e.retryable(),
    }
}

/// `POST /native/scm/webhook` — verify + claim one signed delivery and
/// schedule its idempotent re-sync through the wired sink.
pub(crate) async fn native_scm_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(sink) = state.deps.scm_webhook.as_ref() else {
        return wire_status(webhook_disabled());
    };
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str(), value)))
        .collect();
    let webhook_headers = WebhookHeaders::from_pairs(pairs);
    match sink.deliver(&webhook_headers, &body) {
        Ok(IngestOutcome::Accepted(delivery)) => Json(serde_json::json!({
            "ok": true,
            "status": "accepted",
            "deliveryId": delivery.delivery_id,
            "event": delivery.event,
        }))
        .into_response(),
        Ok(IngestOutcome::Duplicate {
            delivery_id,
            first_seen_ms,
        }) => Json(serde_json::json!({
            "ok": true,
            "status": "duplicate",
            "deliveryId": delivery_id,
            "firstSeenMs": first_seen_ms,
        }))
        .into_response(),
        Err(e) => {
            // Verification refusals are 401/400/413; a store refusal is a
            // retryable 503 so the provider redelivers (the idempotent inbox
            // claim keeps the delivery exactly-once).
            wire_status(webhook_err(e))
        }
    }
}

#[cfg(test)]
#[path = "scm_webhook_tests.rs"]
mod scm_webhook_tests;
