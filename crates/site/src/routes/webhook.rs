use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use faktor_payments::{PaymentError, WebhookDisposition, WebhookHeaders};

use crate::web;
use crate::AppState;

/// The provider-neutral payment webhook ingress. Local (signed) and Mollie
/// (id-only, resolved by an authoritative API fetch) events both arrive here;
/// the configured adapter owns verification.
pub async fn payments(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(error) = state.limit(&state.ip_key(&headers, "webhook:payments"), 240, 60 * 1000) {
        return web::text_response(
            error.status(),
            "text/plain; charset=utf-8",
            "rate_limited".into(),
        );
    }
    let webhook_headers = WebhookHeaders::from_pairs(
        headers
            .iter()
            .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str(), value))),
    );
    match state
        .payments
        .handle_webhook(&webhook_headers, &body, state.now_ms())
        .await
    {
        Ok(outcome) => {
            let status = match outcome.disposition {
                WebhookDisposition::Applied
                | WebhookDisposition::Duplicate
                | WebhookDisposition::Ignored
                | WebhookDisposition::StaleIgnored => StatusCode::OK,
                WebhookDisposition::ReconciliationConflict => StatusCode::BAD_REQUEST,
            };
            let body = match outcome.disposition {
                WebhookDisposition::Duplicate => "[duplicate]",
                WebhookDisposition::StaleIgnored => "[stale]",
                WebhookDisposition::ReconciliationConflict => "[conflict]",
                WebhookDisposition::Ignored => "[accepted]",
                WebhookDisposition::Applied => "[accepted]",
            };
            tracing::info!(
                event = %outcome.event_id,
                event_type = %outcome.event_type,
                disposition = ?outcome.disposition,
                "payment webhook processed"
            );
            web::text_response(status, "text/plain; charset=utf-8", body.into())
        }
        Err(error) => {
            let status = match &error {
                PaymentError::WebhookUnauthorized(_) | PaymentError::WebhookStaleTimestamp => {
                    StatusCode::UNAUTHORIZED
                }
                PaymentError::WebhookMalformed(_) | PaymentError::Declined(_) => {
                    StatusCode::BAD_REQUEST
                }
                PaymentError::NotFound(_) => StatusCode::NOT_FOUND,
                PaymentError::ProviderUnconfigured(_) => StatusCode::SERVICE_UNAVAILABLE,
                PaymentError::Provider(_) | PaymentError::Config(_) => StatusCode::BAD_GATEWAY,
                PaymentError::Store(_) => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            tracing::warn!(status = status.as_u16(), error = %error, "payment webhook refused");
            web::text_response(status, "text/plain; charset=utf-8", error.code().into())
        }
    }
}
