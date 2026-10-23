//! HMAC-SHA256 webhook authentication. The signature is the credential for
//! the unauthenticated provider ingress: verify the timestamp window and the
//! constant-time signature BEFORE parsing or touching durable state.

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

use crate::error::PaymentError;
use crate::provider::{VerifiedWebhookEvent, WebhookHeaders};

/// Bound on one webhook body.
pub const MAX_WEBHOOK_BODY_BYTES: usize = 1024 * 1024;
/// Default replay window (five minutes).
pub const DEFAULT_WEBHOOK_WINDOW_MS: i64 = 5 * 60 * 1000;
/// Bound on the event id/type text kept in memory.
const MAX_EVENT_TEXT: usize = 256;

/// Lowercase hex HMAC-SHA256 over `payload`.
pub fn hmac_sha256_hex(secret: &[u8], payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(payload);
    let bytes = mac.finalize().into_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Constant-time equality for two same-purpose digests.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Sign a payload in the Stripe-compatible `t=<secs>,v1=<hex>` shape. This
/// is used by the local provider's test/operator seam and by tests; the
/// production Stripe adapter verifies the provider's own header.
pub fn sign_webhook_payload(secret: &[u8], timestamp_secs: i64, body: &[u8]) -> String {
    let mut payload = timestamp_secs.to_string().into_bytes();
    payload.push(b'.');
    payload.extend_from_slice(body);
    format!(
        "t={timestamp_secs},v1={}",
        hmac_sha256_hex(secret, &payload)
    )
}

/// Extract `(timestamp_seconds, v1_signatures, v0_signatures)` from the
/// header. Unknown scheme entries are ignored; a header with no timestamp or
/// no signature is a typed malformed refusal.
fn parse_signature_header(header: &str) -> Result<(i64, Vec<String>, Vec<String>), PaymentError> {
    let mut timestamp = None;
    let mut v1 = Vec::new();
    let mut v0 = Vec::new();
    for part in header.split(',') {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        match name {
            "t" => {
                timestamp = Some(value.parse::<i64>().map_err(|_| {
                    PaymentError::WebhookMalformed("signature timestamp is not an integer".into())
                })?);
            }
            "v1" => {
                if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    v1.push(value.to_ascii_lowercase());
                } else {
                    return Err(PaymentError::WebhookMalformed(
                        "signature v1 is not a 64-hex digest".into(),
                    ));
                }
            }
            "v0" => v0.push(value.to_string()),
            _ => {}
        }
    }
    let timestamp = timestamp.ok_or_else(|| {
        PaymentError::WebhookMalformed("signature header carries no timestamp".into())
    })?;
    if v1.is_empty() {
        return Err(PaymentError::WebhookUnauthorized(
            "signature header carries no v1 digest".into(),
        ));
    }
    Ok((timestamp, v1, v0))
}

/// Authenticate and parse one webhook delivery. The header may be
/// `Stripe-Signature` or the local `x-faktor-signature`; both use the same
/// `t=<seconds>,v1=<HMAC-SHA256 over "t.body">` contract.
pub fn verify_webhook(
    secret: &str,
    headers: &WebhookHeaders,
    body: &[u8],
    now_ms: i64,
    window_ms: i64,
) -> Result<VerifiedWebhookEvent, PaymentError> {
    if secret.is_empty() {
        return Err(PaymentError::ProviderUnconfigured(
            "no webhook signing secret is resolved".into(),
        ));
    }
    if body.is_empty() || body.len() > MAX_WEBHOOK_BODY_BYTES {
        return Err(PaymentError::WebhookMalformed(
            "webhook body is empty or exceeds the bound".into(),
        ));
    }
    let header = headers
        .get("stripe-signature")
        .or_else(|| headers.get("x-faktor-signature"))
        .ok_or_else(|| {
            PaymentError::WebhookUnauthorized("webhook signature header is missing".into())
        })?;
    let (timestamp_secs, v1, _) = parse_signature_header(header)?;
    let timestamp_ms = timestamp_secs.saturating_mul(1000);
    if now_ms < 0 || (now_ms - timestamp_ms).abs() > window_ms {
        return Err(PaymentError::WebhookStaleTimestamp);
    }
    let mut signed = timestamp_secs.to_string().into_bytes();
    signed.push(b'.');
    signed.extend_from_slice(body);
    let expected = hmac_sha256_hex(secret.as_bytes(), &signed);
    let matched = v1
        .iter()
        .any(|candidate| constant_time_eq(candidate.as_bytes(), expected.as_bytes()));
    if !matched {
        return Err(PaymentError::WebhookUnauthorized(
            "webhook signature does not match the body".into(),
        ));
    }
    let payload: Value = serde_json::from_slice(body)
        .map_err(|_| PaymentError::WebhookMalformed("webhook body is not JSON".into()))?;
    let object = payload
        .as_object()
        .ok_or_else(|| PaymentError::WebhookMalformed("webhook body is not an object".into()))?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_EVENT_TEXT)
        .ok_or_else(|| PaymentError::WebhookMalformed("webhook event id is missing".into()))?;
    let event_type = object
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty() && kind.len() <= MAX_EVENT_TEXT)
        .ok_or_else(|| PaymentError::WebhookMalformed("webhook event type is missing".into()))?;
    let raw_created = object
        .get("created")
        .and_then(Value::as_i64)
        .ok_or_else(|| PaymentError::WebhookMalformed("webhook event created is missing".into()))?;
    let created_ms = if raw_created.abs() < 10_000_000_000 {
        raw_created.saturating_mul(1000)
    } else {
        raw_created
    };
    let organization_id = payload
        .pointer("/data/object/metadata/organization_id")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .pointer("/metadata/organization_id")
                .and_then(Value::as_str)
        })
        .or_else(|| payload.get("organization_id").and_then(Value::as_str))
        .map(str::to_string);
    Ok(VerifiedWebhookEvent {
        id: id.to_string(),
        event_type: event_type.to_string(),
        created_ms,
        organization_id,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(header: &str) -> WebhookHeaders {
        WebhookHeaders::from_pairs([("Stripe-Signature", header)])
    }

    const NOW: i64 = 1_800_000_000_000;
    const SECRET: &str = "whsec_test";

    fn body(id: &str, kind: &str, created: i64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": id,
            "type": kind,
            "created": created,
            "data": {"object": {"metadata": {"organization_id": "org-1"}}},
        }))
        .unwrap()
    }

    #[test]
    fn signed_payload_verifies_and_forgery_is_refused() {
        let body = body("evt_1", "invoice.paid", 1_800_000_000);
        let ts = NOW / 1000;
        let header = sign_webhook_payload(SECRET.as_bytes(), ts, &body);
        let event = verify_webhook(SECRET, &headers(&header), &body, NOW, 300_000).unwrap();
        assert_eq!(event.id, "evt_1");
        assert_eq!(event.organization_id.as_deref(), Some("org-1"));

        let mut forged = body.clone();
        forged.push(b' ');
        assert!(matches!(
            verify_webhook(SECRET, &headers(&header), &forged, NOW, 300_000),
            Err(PaymentError::WebhookUnauthorized(_))
        ));
        assert!(matches!(
            verify_webhook("other", &headers(&header), &body, NOW, 300_000),
            Err(PaymentError::WebhookUnauthorized(_))
        ));
    }

    #[test]
    fn stale_and_replayed_timestamps_are_refused_but_the_body_is_unchanged() {
        let body = body("evt_1", "invoice.paid", 1_800_000_000);
        let stale_ts = NOW / 1000 - 301;
        let stale = sign_webhook_payload(SECRET.as_bytes(), stale_ts, &body);
        assert_eq!(
            verify_webhook(SECRET, &headers(&stale), &body, NOW, 300_000),
            Err(PaymentError::WebhookStaleTimestamp)
        );
        let missing = WebhookHeaders::from_pairs([("content-type", "application/json")]);
        assert!(matches!(
            verify_webhook(SECRET, &missing, &body, NOW, 300_000),
            Err(PaymentError::WebhookUnauthorized(_))
        ));
    }
}
