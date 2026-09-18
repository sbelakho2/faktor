//! Signed-webhook verification and the durable webhook inbox.
//!
//! Verification (all-or-nothing, refuse before any durable write):
//!
//! - a delivery id and a `sha256=<64 hex>` signature header are REQUIRED —
//!   a missing signature is refused exactly like a bad one;
//! - the signature is HMAC-SHA256 over the raw request body compared in
//!   constant time (length differences are refused before comparison);
//! - when the sender supplies a SIGNED timestamp (our own senders sign
//!   `{timestamp_ms}.{body}`), the timestamp must be within the replay
//!   window of the receiving clock; GitHub's `X-Hub-Signature-256` covers
//!   the body only, so for that mode the replay authority is the durable
//!   delivery-id claim below plus idempotent application.
//!
//! Ingest claims the delivery id durably ([`ScmStore::claim_webhook_delivery`]):
//! the first claim is `Fresh` (apply), every later claim is `Duplicate`
//! (drop). The claim survives restarts, so a replayed or redelivered hook
//! can never be applied twice even across a crash.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::ScmError;
use crate::ids::ScmInstallationId;
use crate::store::{DeliveryClaim, ScmStore};

/// Hard bound on one webhook body: an oversized payload is refused before
/// any signature comparison or durable write.
pub const MAX_WEBHOOK_BODY_BYTES: usize = 1024 * 1024;
/// Hard bound on one delivery id / event header value.
pub const MAX_WEBHOOK_HEADER_BYTES: usize = 256;
/// Default replay window for signed-timestamp payloads (5 minutes).
pub const DEFAULT_REPLAY_WINDOW_MS: i64 = 5 * 60 * 1000;

/// The verification-relevant headers of one webhook request. Values are
/// exactly as received (bounded by [`MAX_WEBHOOK_HEADER_BYTES`] at parse).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebhookHeaders {
    /// `X-Hub-Signature-256` (GitHub) or `X-Faktor-Signature-256`: the
    /// `sha256=<hex>` HMAC of the body (GitHub mode) or of
    /// `{timestamp_ms}.{body}` (signed-timestamp mode).
    pub signature_256: Option<String>,
    /// `X-GitHub-Delivery` or `X-Faktor-Delivery`: the dedupe identity.
    pub delivery_id: Option<String>,
    /// `X-GitHub-Event` or `X-Faktor-Event`: the event name.
    pub event: Option<String>,
    /// A signed send timestamp (only our own senders); when present the
    /// signature is verified over `{timestamp_ms}.{body}` and the replay
    /// window is enforced.
    pub timestamp_ms: Option<i64>,
    /// A timestamp header arrived but could not be parsed as an i64. Such a
    /// delivery is refused fail-closed (it can never be proven inside the
    /// replay window) instead of silently degrading to body-only mode.
    pub timestamp_malformed: bool,
}

impl WebhookHeaders {
    /// Parse from lowercase-or-mixed header pairs (`(name, value)`), the
    /// same shape `HeaderMap` iteration produces. Unknown headers are
    /// ignored; oversized values are carried and refused at verification.
    pub fn from_pairs<'a, I: IntoIterator<Item = (&'a str, &'a str)>>(pairs: I) -> Self {
        let mut out = Self::default();
        for (name, value) in pairs {
            match name.to_ascii_lowercase().as_str() {
                "x-hub-signature-256" | "x-github-signature-256" | "x-faktor-signature-256" => {
                    out.signature_256 = Some(value.to_string());
                }
                "x-github-delivery" | "x-faktor-delivery" => {
                    out.delivery_id = Some(value.to_string());
                }
                "x-github-event" | "x-faktor-event" => {
                    out.event = Some(value.to_string());
                }
                "x-faktor-timestamp" => match value.trim().parse::<i64>() {
                    Ok(ts) => out.timestamp_ms = Some(ts),
                    Err(_) => out.timestamp_malformed = true,
                },
                _ => {}
            }
        }
        out
    }
}

/// Typed webhook refusals. Every variant refused BEFORE any durable write.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("webhook body exceeds {MAX_WEBHOOK_BODY_BYTES} bytes")]
    BodyTooLarge,
    #[error("webhook signature header is missing")]
    MissingSignature,
    #[error("webhook delivery id header is missing")]
    MissingDeliveryId,
    /// The delivery authenticated but its payload names an invalid value
    /// (`installation.id` that is zero, negative, fractional, overflowing or
    /// non-numeric). Terminal for those bytes: a provider redelivery cannot
    /// repair them, so this is never retryable.
    #[error("webhook payload is malformed: {0}")]
    MalformedPayload(String),
    #[error("webhook signature header is malformed")]
    MalformedSignature,
    #[error("webhook signature does not match")]
    SignatureMismatch,
    #[error("webhook timestamp is outside the replay window")]
    StaleTimestamp,
    #[error("webhook store unavailable: {0}")]
    Store(String),
}

impl From<crate::store::ScmStoreError> for WebhookError {
    fn from(e: crate::store::ScmStoreError) -> Self {
        WebhookError::Store(e.to_string())
    }
}

impl WebhookError {
    /// Whether the provider may retry the same delivery unchanged. Only store
    /// unavailability is transient; every authentication, bounds and payload
    /// refusal is terminal for those bytes.
    pub const fn retryable(&self) -> bool {
        matches!(self, WebhookError::Store(_))
    }
}

/// The installation id a webhook payload names (`installation.id`), if any.
/// A present-but-invalid id (zero, negative, fractional, overflowing or
/// non-numeric) is a typed [`WebhookError::MalformedPayload`]: it is never
/// silently dropped and never remapped onto the all-installations sync. A
/// validly signed non-JSON body or a payload without `installation.id` names
/// no installation scope (`Ok(None)`).
pub fn installation_of(body: &[u8]) -> Result<Option<ScmInstallationId>, WebhookError> {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        // A validly signed non-JSON body names no installation scope (the
        // pre-existing all-installations behavior; never a panic).
        return Ok(None);
    };
    let Some(raw) = json
        .get("installation")
        .and_then(|installation| installation.get("id"))
    else {
        return Ok(None);
    };
    let raw = raw.as_u64().ok_or_else(|| {
        WebhookError::MalformedPayload(
            "installation.id must be an unsigned integer within u64 \
             (zero, negative, fractional, oversized and non-numeric values are refused)"
                .to_string(),
        )
    })?;
    let id = ScmInstallationId::try_from_raw(raw)
        .map_err(|e| WebhookError::MalformedPayload(format!("installation.id refused: {e}")))?;
    Ok(Some(id))
}

/// One verified delivery (nothing is applied before this exists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWebhook {
    pub delivery_id: String,
    pub event: String,
    /// Whether the signature covered a signed timestamp.
    pub timestamp_ms: Option<i64>,
}

/// The outcome of one ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// First sighting: the caller applies the event.
    Accepted(VerifiedWebhook),
    /// Already claimed durably: the caller drops the event.
    Duplicate {
        delivery_id: String,
        first_seen_ms: i64,
    },
}

/// HMAC-SHA256 as lowercase hex.
pub fn hmac_sha256_hex(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .expect("HMAC accepts keys of every length (including the empty key)");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Constant-time equality (length is compared first; unequal lengths never
/// compare equal and never short-circuit into a partial comparison).
pub fn constant_time_eq(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(actual.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn decode_signature(header: &str) -> Result<[u8; 32], WebhookError> {
    let hex = header
        .strip_prefix("sha256=")
        .ok_or(WebhookError::MalformedSignature)?;
    if hex.len() != 64 {
        return Err(WebhookError::MalformedSignature);
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(chunk).map_err(|_| WebhookError::MalformedSignature)?;
        out[i] = u8::from_str_radix(text, 16).map_err(|_| WebhookError::MalformedSignature)?;
    }
    Ok(out)
}

/// One webhook verifier: the shared secret plus the replay window. The
/// verifier is pure (no I/O); [`WebhookInbox`] adds the durable dedupe.
#[derive(Clone)]
pub struct WebhookVerifier {
    secret: Vec<u8>,
    replay_window_ms: i64,
}

impl WebhookVerifier {
    /// A verifier with the default replay window. The secret is bounded and
    /// never printed (the struct has no `Debug`).
    pub fn new(secret: impl Into<Vec<u8>>) -> Result<Self, ScmError> {
        let secret = secret.into();
        if secret.is_empty() || secret.len() > 4096 {
            return Err(ScmError::InvalidInput(
                "webhook secret must be 1..=4096 bytes".into(),
            ));
        }
        Ok(Self {
            secret,
            replay_window_ms: DEFAULT_REPLAY_WINDOW_MS,
        })
    }

    /// Override the replay window (bounded: 0..=1 hour).
    pub fn with_replay_window(mut self, window_ms: i64) -> Result<Self, ScmError> {
        if !(0..=3_600_000).contains(&window_ms) {
            return Err(ScmError::InvalidInput(
                "replay window must be 0..=3600000 ms".into(),
            ));
        }
        self.replay_window_ms = window_ms;
        Ok(self)
    }

    /// Verify one delivery at `now_ms`: required headers, bounded body,
    /// HMAC, and (when a signed timestamp is present) the replay window.
    pub fn verify(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<VerifiedWebhook, WebhookError> {
        if body.len() > MAX_WEBHOOK_BODY_BYTES {
            return Err(WebhookError::BodyTooLarge);
        }
        let delivery_id = headers
            .delivery_id
            .as_deref()
            .filter(|v| !v.is_empty() && v.len() <= MAX_WEBHOOK_HEADER_BYTES)
            .ok_or(WebhookError::MissingDeliveryId)?;
        let signature = headers
            .signature_256
            .as_deref()
            .filter(|v| v.len() <= MAX_WEBHOOK_HEADER_BYTES + 8)
            .ok_or(WebhookError::MissingSignature)?;
        let claimed = decode_signature(signature)?;
        let mac_input: Vec<u8> = match headers.timestamp_ms {
            Some(ts) => {
                // Attacker-controlled i64s: widen to i128 BEFORE the
                // subtraction so `i64::MIN`/`i64::MAX` can never overflow
                // (debug panic / release wrap) and every delta is exact.
                let delta = (i128::from(now_ms) - i128::from(ts)).unsigned_abs();
                if delta > self.replay_window_ms as u128 {
                    return Err(WebhookError::StaleTimestamp);
                }
                let mut input = format!("{ts}.").into_bytes();
                input.extend_from_slice(body);
                input
            }
            None => {
                // Present-but-unparseable timestamp: fail closed. There is no
                // freshness proof, and silently treating it as body-only mode
                // would let a malformed header select a weaker mode.
                if headers.timestamp_malformed {
                    return Err(WebhookError::StaleTimestamp);
                }
                body.to_vec()
            }
        };
        if !constant_time_eq(
            &claimed,
            &decode_hex(&hmac_sha256_hex(&self.secret, &mac_input)),
        ) {
            return Err(WebhookError::SignatureMismatch);
        }
        Ok(VerifiedWebhook {
            delivery_id: delivery_id.to_string(),
            event: headers
                .event
                .as_deref()
                .filter(|v| !v.is_empty() && v.len() <= MAX_WEBHOOK_HEADER_BYTES)
                .unwrap_or("unknown")
                .to_string(),
            timestamp_ms: headers.timestamp_ms,
        })
    }
}

fn decode_hex(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        if let Ok(text) = std::str::from_utf8(chunk) {
            if let Ok(byte) = u8::from_str_radix(text, 16) {
                out[i] = byte;
            }
        }
    }
    out
}

/// The durable webhook inbox: verify, then claim the delivery id exactly
/// once. Nothing reaches the store before the signature verifies.
pub struct WebhookInbox {
    verifier: WebhookVerifier,
    store: std::sync::Arc<dyn ScmStore>,
}

impl WebhookInbox {
    pub fn new(verifier: WebhookVerifier, store: std::sync::Arc<dyn ScmStore>) -> Self {
        Self { verifier, store }
    }

    /// Verify one delivery at `now_ms`.
    pub fn verify(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<VerifiedWebhook, WebhookError> {
        self.verifier.verify(headers, body, now_ms)
    }

    /// Verify and claim one delivery. A verification failure leaves the
    /// store untouched; a duplicate claim is dropped (never re-applied).
    pub fn ingest(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<IngestOutcome, WebhookError> {
        let verified = self.verifier.verify(headers, body, now_ms)?;
        match self
            .store
            .claim_webhook_delivery(&verified.delivery_id, &verified.event, now_ms)?
        {
            DeliveryClaim::Fresh => Ok(IngestOutcome::Accepted(verified)),
            DeliveryClaim::Duplicate { first_seen_ms } => Ok(IngestOutcome::Duplicate {
                delivery_id: verified.delivery_id,
                first_seen_ms,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryScmStore;

    const SECRET: &[u8] = b"webhook-secret";

    fn signed_headers(body: &[u8], delivery: &str) -> WebhookHeaders {
        WebhookHeaders {
            signature_256: Some(format!("sha256={}", hmac_sha256_hex(SECRET, body))),
            delivery_id: Some(delivery.to_string()),
            event: Some("push".into()),
            timestamp_ms: None,
            timestamp_malformed: false,
        }
    }

    fn inbox() -> (WebhookInbox, std::sync::Arc<dyn ScmStore>) {
        let store: std::sync::Arc<dyn ScmStore> = std::sync::Arc::new(MemoryScmStore::new());
        (
            WebhookInbox::new(
                WebhookVerifier::new(SECRET.to_vec()).unwrap(),
                store.clone(),
            ),
            store,
        )
    }

    #[test]
    fn hmac_matches_rfc4231_vector() {
        // RFC 4231 test case 1 (key = 20 x 0x0b, data = "Hi There").
        let key = vec![0x0b; 20];
        assert_eq!(
            hmac_sha256_hex(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn bad_and_missing_signatures_are_refused_without_touching_the_store() {
        let (inbox, store) = inbox();
        let body = br#"{"action":"opened"}"#;

        let mut missing = signed_headers(body, "d-1");
        missing.signature_256 = None;
        assert_eq!(
            inbox.ingest(&missing, body, 1000).unwrap_err(),
            WebhookError::MissingSignature
        );

        let mut malformed = signed_headers(body, "d-2");
        malformed.signature_256 = Some("sha1=deadbeef".into());
        assert_eq!(
            inbox.ingest(&malformed, body, 1000).unwrap_err(),
            WebhookError::MalformedSignature
        );

        let mut wrong = signed_headers(body, "d-3");
        wrong.signature_256 = Some(format!("sha256={}", hmac_sha256_hex(b"other-secret", body)));
        assert_eq!(
            inbox.ingest(&wrong, body, 1000).unwrap_err(),
            WebhookError::SignatureMismatch
        );

        let mut tampered_body = signed_headers(body, "d-4");
        tampered_body.signature_256 = Some(format!("sha256={}", hmac_sha256_hex(SECRET, b"other")));
        assert_eq!(
            inbox.ingest(&tampered_body, body, 1000).unwrap_err(),
            WebhookError::SignatureMismatch
        );

        let mut no_delivery = signed_headers(body, "d-5");
        no_delivery.delivery_id = None;
        assert_eq!(
            inbox.ingest(&no_delivery, body, 1000).unwrap_err(),
            WebhookError::MissingDeliveryId
        );

        assert!(
            store.webhook_deliveries(10).unwrap().is_empty(),
            "a refused delivery must never reach the durable inbox"
        );
    }

    #[test]
    fn oversized_body_and_oversized_headers_are_refused() {
        let (inbox, store) = inbox();
        let oversized = vec![b'x'; MAX_WEBHOOK_BODY_BYTES + 1];
        let mut headers = signed_headers(&oversized, "d-big");
        headers.signature_256 = Some(format!("sha256={}", hmac_sha256_hex(SECRET, &oversized)));
        assert_eq!(
            inbox.ingest(&headers, &oversized, 1000).unwrap_err(),
            WebhookError::BodyTooLarge
        );
        let body = b"{}";
        let mut long_id = signed_headers(body, "d");
        long_id.delivery_id = Some("x".repeat(MAX_WEBHOOK_HEADER_BYTES + 1));
        assert_eq!(
            inbox.ingest(&long_id, body, 1000).unwrap_err(),
            WebhookError::MissingDeliveryId
        );
        assert!(store.webhook_deliveries(10).unwrap().is_empty());
    }

    #[test]
    fn replayed_delivery_is_deduped_and_fresh_ones_apply() {
        let (inbox, store) = inbox();
        let body = b"{}";
        let headers = signed_headers(body, "d-1");
        assert!(matches!(
            inbox.ingest(&headers, body, 1000).unwrap(),
            IngestOutcome::Accepted(_)
        ));
        // The same body+signature replayed under the SAME delivery id.
        assert_eq!(
            inbox.ingest(&headers, body, 5000).unwrap(),
            IngestOutcome::Duplicate {
                delivery_id: "d-1".into(),
                first_seen_ms: 1000
            }
        );
        // A NEW delivery id with the same body is a new delivery (fresh).
        assert!(matches!(
            inbox
                .ingest(&signed_headers(body, "d-2"), body, 6000)
                .unwrap(),
            IngestOutcome::Accepted(_)
        ));
        assert_eq!(store.webhook_deliveries(10).unwrap().len(), 2);
    }

    #[test]
    fn signed_timestamp_window_rejects_stale_and_future_deliveries() {
        let (inbox, _store) = inbox();
        let body = b"{}";
        let now = 1_000_000i64;
        let make = |ts: i64| {
            let mut input = format!("{ts}.").into_bytes();
            input.extend_from_slice(body);
            WebhookHeaders {
                signature_256: Some(format!("sha256={}", hmac_sha256_hex(SECRET, &input))),
                delivery_id: Some(format!("d-{ts}")),
                event: Some("push".into()),
                timestamp_ms: Some(ts),
                timestamp_malformed: false,
            }
        };
        assert!(inbox.ingest(&make(now), body, now).is_ok());
        assert_eq!(
            inbox
                .ingest(&make(now - DEFAULT_REPLAY_WINDOW_MS - 1), body, now)
                .unwrap_err(),
            WebhookError::StaleTimestamp
        );
        assert_eq!(
            inbox
                .ingest(&make(now + DEFAULT_REPLAY_WINDOW_MS + 1), body, now)
                .unwrap_err(),
            WebhookError::StaleTimestamp
        );
        // A timestamp inside the window signed over the WRONG input is a
        // signature mismatch, never accepted on freshness alone.
        let mut forged = make(now);
        forged.signature_256 = Some(format!("sha256={}", hmac_sha256_hex(SECRET, body)));
        assert_eq!(
            inbox.ingest(&forged, body, now).unwrap_err(),
            WebhookError::SignatureMismatch
        );
    }

    #[test]
    fn header_parsing_is_alias_exact_and_bounded() {
        let headers = WebhookHeaders::from_pairs([
            ("X-GitHub-Signature-256", "sha256=aa"),
            ("X-GitHub-Delivery", "d1"),
            ("X-GitHub-Event", "pull_request"),
            ("X-Faktor-Timestamp", "1234"),
            ("X-Unrelated", "ignored"),
        ]);
        assert_eq!(headers.signature_256.as_deref(), Some("sha256=aa"));
        assert_eq!(headers.delivery_id.as_deref(), Some("d1"));
        assert_eq!(headers.event.as_deref(), Some("pull_request"));
        assert_eq!(headers.timestamp_ms, Some(1234));
        assert!(!headers.timestamp_malformed);
        let headers = WebhookHeaders::from_pairs([("X-Faktor-Timestamp", "not-a-number")]);
        assert_eq!(headers.timestamp_ms, None);
        assert!(
            headers.timestamp_malformed,
            "a present-but-unparseable timestamp must be marked malformed"
        );
    }

    #[test]
    fn timestamp_arithmetic_extremes_are_refused_without_panicking() {
        let (inbox, store) = inbox();
        let body = b"{}";
        let now = 1_000_000i64;
        let make = |ts: i64| {
            let mut input = format!("{ts}.").into_bytes();
            input.extend_from_slice(body);
            WebhookHeaders {
                signature_256: Some(format!("sha256={}", hmac_sha256_hex(SECRET, &input))),
                delivery_id: Some(format!("d-extreme-{ts}")),
                event: Some("push".into()),
                timestamp_ms: Some(ts),
                timestamp_malformed: false,
            }
        };
        // Every extreme is a typed stale refusal: no i64 overflow, no panic,
        // and nothing durable is claimed. The signatures are VALID for the
        // claimed timestamp, so the refusal is purely the window arithmetic.
        for ts in [
            i64::MIN,
            i64::MAX,
            now - DEFAULT_REPLAY_WINDOW_MS - 1,
            now + DEFAULT_REPLAY_WINDOW_MS + 1,
            0,
            -1,
            now - 1,
        ] {
            let headers = make(ts);
            let verdict = inbox.ingest(&headers, body, now);
            if ts == now - 1 {
                assert!(verdict.is_ok(), "a recent timestamp must be accepted");
            } else {
                assert_eq!(
                    verdict.unwrap_err(),
                    WebhookError::StaleTimestamp,
                    "timestamp {ts} must be a typed stale refusal"
                );
            }
        }
        assert_eq!(
            store.webhook_deliveries(10).unwrap().len(),
            1,
            "only the one in-window delivery may reach the durable inbox"
        );
    }

    #[test]
    fn malformed_timestamp_is_refused_even_with_a_valid_body_signature() {
        let (inbox, store) = inbox();
        let body = b"{}";
        let mut headers = signed_headers(body, "d-malformed");
        // A valid GitHub-style body signature plus an unparseable timestamp
        // header must NOT be accepted in body-only mode.
        headers.timestamp_malformed = true;
        assert_eq!(
            inbox.ingest(&headers, body, 1_000).unwrap_err(),
            WebhookError::StaleTimestamp
        );
        // The same refusal when the malformed header arrives over the pair
        // parser (the HTTP path).
        let signature = format!("sha256={}", hmac_sha256_hex(SECRET, body));
        let parsed = WebhookHeaders::from_pairs([
            ("X-Faktor-Timestamp", "9999999999999999999999"),
            ("X-Faktor-Signature-256", signature.as_str()),
            ("X-Faktor-Delivery", "d-malformed-2"),
        ]);
        assert!(parsed.timestamp_malformed);
        assert_eq!(
            inbox.ingest(&parsed, body, 1_000).unwrap_err(),
            WebhookError::StaleTimestamp
        );
        assert!(store.webhook_deliveries(10).unwrap().is_empty());
    }

    #[test]
    fn malformed_installation_id_is_a_typed_terminal_refusal() {
        assert_eq!(
            installation_of(br#"{"installation":{"id":7}}"#).unwrap(),
            Some(ScmInstallationId::new(7))
        );
        assert_eq!(
            installation_of(br#"{"installation":{"id":18446744073709551615}}"#).unwrap(),
            Some(ScmInstallationId::new(u64::MAX)),
            "the u64 boundary is a valid installation id"
        );
        assert_eq!(installation_of(br#"{"action":"created"}"#).unwrap(), None);
        assert_eq!(installation_of(b"not-json").unwrap(), None);
        for body in [
            br#"{"installation":{"id":0}}"#.as_slice(),
            br#"{"installation":{"id":-1}}"#,
            br#"{"installation":{"id":1.5}}"#,
            br#"{"installation":{"id":18446744073709551616}}"#,
            br#"{"installation":{"id":"7"}}"#,
        ] {
            let err = installation_of(body).unwrap_err();
            assert!(
                matches!(err, WebhookError::MalformedPayload(_)),
                "{body:?} must be a typed malformed payload, got {err:?}"
            );
            assert!(!err.retryable(), "malformed {body:?} must never be retried");
            assert!(err.to_string().contains("installation.id"), "{err}");
        }
    }

    #[test]
    fn webhook_retryability_is_typed_by_variant() {
        let terminal = [
            WebhookError::BodyTooLarge,
            WebhookError::MissingSignature,
            WebhookError::MissingDeliveryId,
            WebhookError::MalformedSignature,
            WebhookError::SignatureMismatch,
            WebhookError::StaleTimestamp,
            WebhookError::MalformedPayload("installation.id refused".into()),
        ];
        for err in &terminal {
            assert!(!err.retryable(), "{err:?} must be terminal");
        }
        assert!(WebhookError::Store("db down".into()).retryable());
        assert!(
            WebhookError::from(crate::store::ScmStoreError::Backend("db down".into())).retryable()
        );
    }

    #[test]
    fn secret_bounds_are_enforced() {
        assert!(WebhookVerifier::new(Vec::new()).is_err());
        assert!(WebhookVerifier::new(vec![7u8; 4097]).is_err());
        assert!(WebhookVerifier::new(vec![7u8; 8])
            .unwrap()
            .with_replay_window(3_600_001)
            .is_err());
    }

    #[test]
    fn constant_time_eq_is_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
