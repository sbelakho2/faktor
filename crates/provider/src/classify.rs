//! Single HTTP error classifier for every adapter transport (audit waves
//! 93-96 subset). One classification policy, one code table — adapters must
//! never re-derive status→kind chains locally, or the taxonomies drift.
//!
//! Classification contract:
//!
//! | HTTP status | hint (`error.type`/`error.code`/`error.status`) | kind |
//! |---|---|---|
//! | 401, 407 | ANY (even a `rate_limit`-looking body) | `Permission` |
//! | 403 | `permission_denied`-family (Google `PERMISSION_DENIED`) | `Permission` |
//! | 403 | `rate_limit`-family (`RESOURCE_EXHAUSTED`, quota) | `RateLimited` |
//! | 403 | auth-family or none | `Permission` |
//! | 400 | auth/permission-family (OpenAI `code: invalid_api_key`) | `Permission` |
//! | 400 | rate-limit-family (`RESOURCE_EXHAUSTED`, quota) | `RateLimited` |
//! | 400 | none | `Malformed` |
//! | 404 | — | `NotFound` |
//! | 409 | — | `Conflict` |
//! | 422 | — | `Malformed` |
//! | 501 | — | `Internal` |
//! | 429 | ANY | `RateLimited` |
//! | 408, 425, 504 | — | `Timeout` |
//! | 5xx (rest) | — | `Provider { retryable: true }` |
//! | other 4xx | — | `Provider { retryable: false }` |
//!
//! Why status beats hint on 401/407/429: a body token is server-authored
//! data on a channel that already denied credentials or throttled us. A 401
//! whose body says `rate_limit` is a lying or misconfigured proxy — retrying
//! it would re-send secrets over an unauthenticated channel and burn
//! backoff sleeps for nothing. A 429 whose body says `invalid_api_key`
//! still got a genuine rate-limit response from an endpoint we reached with
//! valid-enough credentials to be throttled; rate limits stay retryable
//! with backoff. Hints only override where providers genuinely differ:
//! 400 and 403 (OpenAI 400s bad keys with `code: invalid_api_key`; Google
//! 403s permission boundaries with `PERMISSION_DENIED` — permission is NOT
//! auth, and quota 403s with `RESOURCE_EXHAUSTED` are rate-limit class; a
//! 400 carrying `RESOURCE_EXHAUSTED`/quota is equally rate-limit class, since
//! gateways surface upstream quota exhaustion with status 400).
//!
//! Hint scanning is STRUCTURED ONLY: JSON fields `error.type`,
//! `error.code` and `error.status` (OpenAI/Anthropic/Google shapes), key
//! and value matching case-insensitive, numeric codes stringified. Message
//! text is NEVER scanned — a message saying "invalid api key" proves
//! nothing.

use faktor_core::error::{Error, ErrorKind};
use serde_json::Value;

use crate::sanitize::ErrorScrubber;
use crate::{ProviderError, ProviderErrorKind};

/// A hint token classified into its semantic family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorHint {
    Auth,
    Permission,
    RateLimited,
}

/// Fold a wire token for membership: lowercase, alphanumerics only, so
/// `PERMISSION_DENIED`, `permission_denied`, `Permission-Denied` and
/// `permission denied` all compare equal.
fn fold(w: &str) -> String {
    w.trim()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The auth-family token set: credential rejection, never message text.
fn is_auth_token(folded: &str) -> bool {
    matches!(
        folded,
        "authenticationerror"
            | "invalidapikey"
            | "invalidkey"
            | "unauthorized"
            | "authentication"
            | "auth"
            | "autherror"
            | "apikeyinvalid"
            | "apikeynotfound"
            | "apikeyrequired"
            | "missingapikey"
            | "unauthenticated"
            | "invalidcredentials"
            | "accesstokeninvalid"
            | "authenticationfailed"
    ) || folded.starts_with("apikey")
        // Conservative family match: the in-stream classifier this table
        // replaced used `contains("auth")`, so `authorization_error`,
        // `oauth_error` and `authentication_failure` must stay auth-shaped
        // (credential rejection), not fall through to the generic terminal
        // kind. The explicit list above keeps the exact-token cases cheap.
        || folded.contains("auth")
}

/// The permission-family token set (Google `PERMISSION_DENIED`, Anthropic
/// `permission_error`, ...). The caller was authenticated but not allowed.
fn is_permission_token(folded: &str) -> bool {
    folded == "forbidden"
        || folded == "accessdenied"
        || folded == "notallowed"
        || folded.starts_with("permission")
}

/// The rate-limit-family token set (incl. Google quota denials that ride
/// 403: `RESOURCE_EXHAUSTED`, `quota_exceeded`, ...).
fn is_rate_limit_token(folded: &str) -> bool {
    folded.starts_with("ratelimit")
        || folded == "toomanyrequests"
        || folded.starts_with("toomany")
        || folded.starts_with("quota")
        || folded.starts_with("resourceexhausted")
        || folded.starts_with("throttl")
        || folded == "requestslimitexceeded"
}

/// Classify one normalized wire token into its family.
pub fn hint_kind(hint: &str) -> Option<ErrorHint> {
    let folded = fold(hint);
    if folded.is_empty() {
        return None;
    }
    // Precedence when a body carries several structured fields: an auth
    // token beats a permission token beats a rate-limit token (a request
    // whose code says the credentials are invalid is an auth failure no
    // matter what the status field claims).
    if is_auth_token(&folded) {
        Some(ErrorHint::Auth)
    } else if is_permission_token(&folded) {
        Some(ErrorHint::Permission)
    } else if is_rate_limit_token(&folded) {
        Some(ErrorHint::RateLimited)
    } else {
        None
    }
}

/// Extract the strongest structured hint from a raw error body. Returns the
/// normalized token (`invalid_api_key`, `PERMISSION_DENIED`, ...) of the
/// highest-precedence classified field, or `None`.
///
/// Recognized shapes (never the message text):
/// - OpenAI/Anthropic: `{"error": {"type": "authentication_error" | "code": "invalid_api_key"}}`
/// - Google: `{"error": {"status": "PERMISSION_DENIED" | "code": 7}}`
/// - Google ErrorInfo detail rows (400-level auth/quota): `{"error": {"details": [{"reason": "API_KEY_INVALID"}]}}`
/// - SSE error events: `{"type": "error", "error": {"code": ...}}`
/// - Responses / SSE error events: `{"type": "error", "code": "invalid_api_key"}`
///
/// Non-JSON and non-object bodies yield `None`.
pub fn body_error_hint(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value_error_hint(&value)
}

/// The same structured scan for an already-parsed JSON value: the `error`
/// member's object when present, else the value itself (SSE and Responses
/// error events carry their structured fields at the top level). Never
/// inspects message text.
pub fn value_error_hint(value: &Value) -> Option<String> {
    if let Some(error) = find_ci(value, "error").and_then(|e| e.as_object()) {
        return error_object_hint(error);
    }
    value.as_object().and_then(error_object_hint)
}

/// Scan ONE structured error object for its strongest hint field.
fn error_object_hint(error: &serde_json::Map<String, Value>) -> Option<String> {
    let mut best: Option<(u8, String)> = None;
    let mut push = |rank: u8, token: &str| {
        if best.as_ref().is_none_or(|(r, _)| rank > *r) {
            best = Some((rank, fold(token)));
        }
    };
    // Direct structured fields: error.type / error.code / error.status.
    for field in ["code", "status", "type"] {
        let Some(raw) = error
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(field))
            .map(|(_, v)| v)
        else {
            continue;
        };
        let text = match raw {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if let Some(kind) = hint_kind(&text) {
            push(rank(kind), &text);
        }
    }
    // Google ErrorInfo rows: error.details[].reason (structured, never the
    // message text) — a 400 carrying API_KEY_INVALID there is an auth
    // failure, a RATE_LIMIT_EXCEEDED row is rate-limit class.
    if let Some(details) = error
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("details"))
        .map(|(_, v)| v)
    {
        if let Some(rows) = details.as_array() {
            for row in rows {
                let Some(inner) = row.as_object() else {
                    continue;
                };
                let Some(Value::String(reason)) = inner
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("reason"))
                    .map(|(_, v)| v)
                else {
                    continue;
                };
                if let Some(kind) = hint_kind(reason) {
                    push(rank(kind), reason);
                }
            }
        }
    }
    best.map(|(_, token)| token)
}

fn rank(kind: ErrorHint) -> u8 {
    match kind {
        ErrorHint::Auth => 3,
        ErrorHint::Permission => 2,
        ErrorHint::RateLimited => 1,
    }
}

fn find_ci<'v>(value: &'v Value, key: &str) -> Option<&'v Value> {
    let obj = value.as_object()?;
    obj.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// THE classifier: HTTP status → typed core kind, with the documented
/// 400/403 hint overrides. Every adapter transport error path must call
/// this — no locally duplicated status chains anywhere.
pub fn classify_http(status: u16, body_hint: Option<&str>) -> ErrorKind {
    classify_http_hint(status, body_hint.and_then(hint_kind))
}

/// The classifier over the ALREADY-PARSED structured hint family. Hints have
/// no wire shape here — this is the exhaustive status × hint authority the
/// truth table and every adapter expectation derive from.
pub fn classify_http_hint(status: u16, hint: Option<ErrorHint>) -> ErrorKind {
    match status {
        // Credentials were rejected: NEVER retryable. Permission wins over
        // any body hint — see the module docs.
        401 | 407 => ErrorKind::Permission,
        // 403: the server authenticated the request but refused it. The
        // hint decides the family — permission is NOT auth (Google
        // PERMISSION_DENIED), quota exhaustion is rate-limit class; a bare
        // 403 (or an auth token) is a terminal credential/permission denial.
        403 => match hint {
            Some(ErrorHint::Permission) => ErrorKind::Permission,
            Some(ErrorHint::RateLimited) => ErrorKind::RateLimited,
            _ => ErrorKind::Permission,
        },
        // 400 with an auth/permission token: some providers 400 instead of
        // 401 on bad keys (OpenAI-style `code: invalid_api_key`); a
        // rate-limit token on a 400 (proxies and gateways surface provider
        // quota exhaustion with status 400) is rate-limit class and stays
        // retryable with backoff. No hint: malformed request.
        400 => match hint {
            Some(ErrorHint::Auth) | Some(ErrorHint::Permission) => ErrorKind::Permission,
            Some(ErrorHint::RateLimited) => ErrorKind::RateLimited,
            None => ErrorKind::Malformed,
        },
        404 => ErrorKind::NotFound,
        409 => ErrorKind::Conflict,
        422 => ErrorKind::Malformed,
        501 => ErrorKind::Internal,
        // Rate limits stay retryable with backoff; the status wins over any
        // hint (a throttled channel proved it reached a live endpoint).
        429 => ErrorKind::RateLimited,
        408 | 425 | 504 => ErrorKind::Timeout,
        // 5xx are infrastructure failures: retryable (legacy semantics).
        500..=599 => ErrorKind::Provider {
            code: status.to_string(),
            retryable: true,
        },
        // Every other non-success status is a client-class failure: typed
        // kinds above where the contract has one, otherwise a non-retryable
        // provider error.
        401..=499 => ErrorKind::Provider {
            code: status.to_string(),
            retryable: false,
        },
        _ => ErrorKind::Provider {
            code: status.to_string(),
            retryable: false,
        },
    }
}

/// The exhaustive status × hint truth table this module implements, one row
/// per representative status class × every [`ErrorHint`] family (and the
/// no-hint cell). Tests and generated adapter expectations iterate THIS
/// table, so a classifier cell can never silently drift from its documented
/// contract.
pub fn classification_table() -> Vec<(u16, Option<ErrorHint>, ErrorKind)> {
    const STATUSES: [u16; 18] = [
        400, 401, 403, 404, 405, 408, 409, 418, 422, 425, 429, 500, 501, 502, 503, 504, 529, 599,
    ];
    const HINTS: [Option<ErrorHint>; 4] = [
        None,
        Some(ErrorHint::Auth),
        Some(ErrorHint::Permission),
        Some(ErrorHint::RateLimited),
    ];
    let mut rows = Vec::with_capacity(STATUSES.len() * HINTS.len());
    for status in STATUSES {
        for hint in HINTS {
            rows.push((status, hint, classify_http_hint(status, hint)));
        }
    }
    rows
}

/// The wire token that reaches each hint family, used by table-driven tests
/// to prove the token scanner and the hint-family table agree.
pub fn classification_table_token(hint: Option<ErrorHint>) -> Option<&'static str> {
    match hint {
        None => None,
        Some(ErrorHint::Auth) => Some("invalid_api_key"),
        Some(ErrorHint::Permission) => Some("PERMISSION_DENIED"),
        Some(ErrorHint::RateLimited) => Some("RESOURCE_EXHAUSTED"),
    }
}

/// Classify a body hint with NO status context (SSE `error` events on an
/// otherwise-2xx stream, proxies that carry auth errors inside the stream).
/// `None` when the hint is not a typed family — callers keep their generic
/// non-retryable kind then.
pub fn classify_hint_only(body_hint: Option<&str>) -> Option<ErrorKind> {
    match body_hint.and_then(hint_kind) {
        Some(ErrorHint::Auth) => Some(ErrorKind::Permission),
        Some(ErrorHint::Permission) => Some(ErrorKind::Permission),
        Some(ErrorHint::RateLimited) => Some(ErrorKind::RateLimited),
        None => None,
    }
}

/// Classify one HTTP error response end-to-end: the structured hint is
/// extracted from `body` (structured fields only, never message text), the
/// status and hint go through [`classify_http`], and the resulting core kind
/// is rendered into its provider envelope with the numeric status as the
/// code. The message is rendered from the SAME classification — the
/// classified kind decides whether the upstream body is withheld (terminal
/// auth-classified responses) or scrubbed and bounded — so the kind,
/// retryability and message shape can never disagree. A 403 carrying
/// `RESOURCE_EXHAUSTED` is retryable rate-limit class and keeps its scrubbed
/// diagnostic; a 400 carrying `invalid_api_key` is auth-classified and
/// withholds the body exactly like a 401. The raw body never reaches the
/// error.
pub fn provider_error_for_http_with_scrubber(
    status: u16,
    body: &str,
    scrubber: &ErrorScrubber,
) -> ProviderError {
    let hint = hint_for_status(status, body);
    let kind = classify_http(status, hint.as_deref());
    let message = scrubber.diagnostic_with_auth_disposition(
        status,
        body,
        matches!(kind, ErrorKind::Permission),
    );
    provider_error_for(kind, status.to_string(), message)
}

/// Only 400/403 consume a body hint; every other status classifies from the
/// status alone, so the (up to 64 KiB) body is never JSON-parsed for them.
fn hint_for_status(status: u16, body: &str) -> Option<String> {
    if matches!(status, 400 | 403) {
        body_error_hint(body)
    } else {
        None
    }
}

/// Build a typed core error from a classified kind. The NON-streaming
/// adapter paths that return `faktor_core::Error` (ollama metadata:
/// `/api/ps`, `/api/tags`, `/api/show`) construct their refusals here, so
/// the provider taxonomy has exactly one constructor site outside the
/// streaming envelope.
pub fn core_error_for(kind: ErrorKind, message: impl Into<String>) -> Error {
    Error::new(kind, message)
}

/// Envelope for a structured hint with NO status context (SSE and Responses
/// `error` events on an otherwise-2xx stream). `None` when the hint is not a
/// typed family — callers keep their generic non-retryable kind then.
pub fn provider_error_for_hint(hint: Option<&str>) -> Option<ProviderErrorKind> {
    classify_hint_only(hint).map(|kind| provider_error_kind(&kind))
}

/// Build the transport error envelope for a classified kind. The envelope
/// mirrors the kind's retryability exactly, so a state-aware retry loop
/// that consults `ProviderError::retryable` inherits the taxonomy.
pub fn provider_error_for(
    kind: ErrorKind,
    code: impl Into<String>,
    message: impl Into<String>,
) -> ProviderError {
    ProviderError::with_code(provider_error_kind(&kind), code, message)
}

/// The envelope for one classified core kind. Exhaustive by construction —
/// a new core kind forces a decision here. Kinds the provider envelope
/// cannot represent keep the envelope's retryability: retryable storage and
/// scheduler failures ride `Server`, every terminal kind folds to
/// `BadRequest`.
fn provider_error_kind(kind: &ErrorKind) -> ProviderErrorKind {
    match kind {
        ErrorKind::Permission => ProviderErrorKind::Auth,
        ErrorKind::RateLimited => ProviderErrorKind::RateLimited,
        ErrorKind::Timeout => ProviderErrorKind::Timeout,
        ErrorKind::Network => ProviderErrorKind::Network,
        ErrorKind::Malformed => ProviderErrorKind::Malformed,
        ErrorKind::Cancelled => ProviderErrorKind::Cancelled,
        ErrorKind::Provider {
            retryable: true, ..
        } => ProviderErrorKind::Server,
        ErrorKind::Store | ErrorKind::Deadlock => ProviderErrorKind::Server,
        ErrorKind::Provider {
            retryable: false, ..
        }
        | ErrorKind::NotFound
        | ErrorKind::Conflict
        | ErrorKind::Internal
        | ErrorKind::Oversized
        | ErrorKind::InvalidState { .. } => ProviderErrorKind::BadRequest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_for(status: u16, hint: Option<&str>) -> ErrorKind {
        classify_http(status, hint)
    }

    #[test]
    fn classifier_matrix_status_x_hint() {
        use ErrorKind as K;
        // Auth rows: 401/407 win over EVERY hint, even a rate-limit body.
        assert_eq!(kind_for(401, None), K::Permission);
        assert_eq!(kind_for(401, Some("rate_limit")), K::Permission);
        assert_eq!(kind_for(401, Some("invalid_api_key")), K::Permission);
        assert_eq!(kind_for(407, None), K::Permission);
        assert_eq!(kind_for(407, Some("rate_limit_exceeded")), K::Permission);
        // 403: hint decides; bare 403 is auth (legacy semantics).
        assert_eq!(kind_for(403, None), K::Permission);
        assert_eq!(kind_for(403, Some("invalid_api_key")), K::Permission);
        assert_eq!(kind_for(403, Some("authentication_error")), K::Permission);
        // Permission is NOT auth (Google PERMISSION_DENIED boundary).
        assert_eq!(kind_for(403, Some("PERMISSION_DENIED")), K::Permission);
        assert_eq!(kind_for(403, Some("permission_denied")), K::Permission);
        assert_eq!(kind_for(403, Some("permission_error")), K::Permission);
        assert_eq!(kind_for(403, Some("forbidden")), K::Permission);
        assert_eq!(kind_for(403, Some("access_denied")), K::Permission);
        // Google quota denials ride 403 with a rate-limit code.
        assert_eq!(kind_for(403, Some("RESOURCE_EXHAUSTED")), K::RateLimited);
        assert_eq!(kind_for(403, Some("quota_exceeded")), K::RateLimited);
        // 400: auth hint overrides the status (OpenAI code: invalid_api_key
        // on a 400 body); no hint is a malformed request.
        assert_eq!(kind_for(400, None), K::Malformed);
        assert_eq!(kind_for(400, Some("invalid_api_key")), K::Permission);
        assert_eq!(kind_for(400, Some("authentication_error")), K::Permission);
        assert_eq!(kind_for(400, Some("unauthorized")), K::Permission);
        assert_eq!(kind_for(400, Some("permission")), K::Permission);
        // A 400 carrying a structured rate-limit/quota code is rate-limit
        // class and STAYS RETRYABLE (proxies/gateways surface provider quota
        // exhaustion with status 400).
        assert_eq!(kind_for(400, Some("rate_limit")), K::RateLimited);
        assert_eq!(kind_for(400, Some("RESOURCE_EXHAUSTED")), K::RateLimited);
        assert_eq!(kind_for(400, Some("quota_exceeded")), K::RateLimited);
        // Typed 4xx.
        assert_eq!(kind_for(404, None), K::NotFound);
        assert_eq!(kind_for(409, None), K::Conflict);
        assert_eq!(kind_for(422, None), K::Malformed);
        assert_eq!(kind_for(501, None), K::Internal);
        // Rate limits: status wins over every hint.
        assert_eq!(kind_for(429, None), K::RateLimited);
        assert_eq!(kind_for(429, Some("invalid_api_key")), K::RateLimited);
        // Timeout-ish retryable rows (408/425 stay, 504 keeps legacy).
        assert_eq!(kind_for(408, None), K::Timeout);
        assert_eq!(kind_for(425, None), K::Timeout);
        assert_eq!(kind_for(504, None), K::Timeout);
        // 5xx retryable, other 4xx not.
        assert!(kind_for(500, None).is_retryable());
        assert!(kind_for(503, None).is_retryable());
        assert!(kind_for(529, None).is_retryable());
        assert_eq!(
            kind_for(500, Some("invalid_api_key")),
            K::Provider {
                code: "500".into(),
                retryable: true
            }
        );
        assert_eq!(
            kind_for(405, None),
            K::Provider {
                code: "405".into(),
                retryable: false
            }
        );
        assert_eq!(
            kind_for(418, None),
            K::Provider {
                code: "418".into(),
                retryable: false
            }
        );
        // Retryability follows the kind: permission/typed 4xx are terminal.
        assert!(!K::Permission.is_retryable());
        assert!(!K::Internal.is_retryable());
        assert!(!K::NotFound.is_retryable());
        assert!(!K::Conflict.is_retryable());
        assert!(!K::Malformed.is_retryable());
    }

    #[test]
    fn exhaustive_status_hint_table_is_the_single_authority() {
        use ErrorKind as K;
        let table = classification_table();
        assert_eq!(table.len(), 18 * 4, "every status class × hint family");
        for (status, hint, expected) in &table {
            assert_eq!(
                classify_http_hint(*status, *hint),
                *expected,
                "hint-family cell {status} {hint:?}"
            );
            assert_eq!(
                classify_http(*status, classification_table_token(*hint)),
                *expected,
                "wire-token path must agree with the hint-family cell for {status} {hint:?}"
            );
            // The transport envelope retryability mirrors the kind exactly
            // for every cell, so no adapter can diverge on retry behavior.
            let env = provider_error_for(expected.clone(), status.to_string(), "boom");
            assert_eq!(
                env.retryable,
                expected.is_retryable(),
                "envelope retryability for {status} {hint:?}"
            );
        }
        // The formerly-missing cell is pinned by name as well.
        assert_eq!(kind_for(400, Some("RESOURCE_EXHAUSTED")), K::RateLimited);
        assert!(K::RateLimited.is_retryable());
    }

    #[test]
    fn hint_scanning_is_structured_and_never_reads_message_text() {
        // OpenAI 401/400 shapes.
        assert_eq!(
            body_error_hint(r#"{"error":{"type":"authentication_error","message":"bad key"}}"#)
                .as_deref(),
            Some("authenticationerror")
        );
        assert_eq!(
            body_error_hint(
                r#"{"error":{"message":"Incorrect API key","type":"invalid_request_error","code":"invalid_api_key"}}"#
            )
            .as_deref(),
            Some("invalidapikey"),
            "error.code wins over an unclassified error.type"
        );
        // Anthropic 403.
        assert_eq!(
            body_error_hint(r#"{"type":"error","error":{"type":"permission_error"}}"#).as_deref(),
            Some("permissionerror")
        );
        // Google shapes: status field, and numeric code 7 stringified.
        assert_eq!(
            body_error_hint(
                r#"{"error":{"code":7,"message":"nope","status":"PERMISSION_DENIED"}}"#
            )
            .as_deref(),
            Some("permissiondenied")
        );
        // A message that LOOKS like an auth error must NOT be scanned.
        assert_eq!(
            body_error_hint(r#"{"error":{"message":"invalid api key supplied"}}"#),
            None,
            "message text is never a hint"
        );
        // Non-JSON bodies, garbage and plain strings yield no hint.
        assert_eq!(body_error_hint("401 Unauthorized"), None);
        assert_eq!(body_error_hint(""), None);
        assert_eq!(body_error_hint(r#"{"error":"model not found"}"#), None);
        assert_eq!(body_error_hint("{not json"), None);
        // Case-insensitive keys.
        assert_eq!(
            body_error_hint(r#"{"ERROR":{"CODE":"INVALID_API_KEY"}}"#).as_deref(),
            Some("invalidapikey")
        );
        // Hostile mixed keys with conflicting tokens: auth wins.
        assert_eq!(
            body_error_hint(
                r#"{"error":{"status":"RESOURCE_EXHAUSTED","type":"authentication_error"}}"#
            )
            .as_deref(),
            Some("authenticationerror")
        );
        // SSE / Responses error events carry the structured code at the top
        // level (there is no nested `error` object).
        assert_eq!(
            body_error_hint(r#"{"type":"error","code":"invalid_api_key","message":"bad"}"#)
                .as_deref(),
            Some("invalidapikey")
        );
    }

    #[test]
    fn parsed_event_hints_share_the_same_structured_scan() {
        let ev: Value =
            serde_json::from_str(r#"{"type":"error","code":"rate_limit_exceeded"}"#).unwrap();
        assert_eq!(value_error_hint(&ev).as_deref(), Some("ratelimitexceeded"));
        // A nested error object (Responses `response.failed`) is read the
        // same way the HTTP envelope is.
        let ev: Value = serde_json::from_str(
            r#"{"type":"response.failed","response":{"error":{"type":"authentication_error"}}}"#,
        )
        .unwrap();
        let nested = ev
            .get("response")
            .and_then(|r| r.get("error"))
            .expect("nested error");
        assert_eq!(
            value_error_hint(nested).as_deref(),
            Some("authenticationerror")
        );
        // Message text is never a hint, parsed or not.
        let ev: Value =
            serde_json::from_str(r#"{"type":"error","message":"invalid api key"}"#).unwrap();
        assert_eq!(value_error_hint(&ev), None);
    }

    #[test]
    fn http_envelope_uses_structured_body_hints() {
        let scrubber = ErrorScrubber::new();
        // OpenAI-style 400 on a bad key: the auth hint overrides the status.
        let err = provider_error_for_http_with_scrubber(
            400,
            r#"{"error":{"code":"invalid_api_key","message":"bad"}}"#,
            &scrubber,
        );
        assert_eq!(err.kind, ProviderErrorKind::Auth);
        assert!(!err.retryable);
        assert_eq!(err.code.as_deref(), Some("400"));
        // Google quota denial rides 403: rate-limit class stays retryable.
        let err = provider_error_for_http_with_scrubber(
            403,
            r#"{"error":{"status":"RESOURCE_EXHAUSTED","code":429}}"#,
            &scrubber,
        );
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
        assert!(err.retryable);
        // Google permission boundary stays terminal.
        let err = provider_error_for_http_with_scrubber(
            403,
            r#"{"error":{"status":"PERMISSION_DENIED"}}"#,
            &scrubber,
        );
        assert_eq!(err.kind, ProviderErrorKind::Auth);
        assert!(!err.retryable);
        // No structured hint: the status taxonomy decides.
        let err = provider_error_for_http_with_scrubber(
            400,
            r#"{"error":{"message":"nope"}}"#,
            &scrubber,
        );
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(!err.retryable);
        let err = provider_error_for_http_with_scrubber(
            503,
            r#"{"error":{"message":"down"}}"#,
            &scrubber,
        );
        assert_eq!(err.kind, ProviderErrorKind::Server);
        assert!(err.retryable);
    }

    #[test]
    fn hint_envelope_for_stream_events() {
        assert_eq!(
            provider_error_for_hint(Some("invalid_api_key")),
            Some(ProviderErrorKind::Auth)
        );
        assert_eq!(
            provider_error_for_hint(Some("RESOURCE_EXHAUSTED")),
            Some(ProviderErrorKind::RateLimited)
        );
        assert_eq!(
            provider_error_for_hint(Some("server_error")),
            None,
            "unknown codes keep the caller's generic terminal kind"
        );
        assert_eq!(provider_error_for_hint(None), None);
    }

    #[test]
    fn envelope_maps_kind_retryability_exactly() {
        for (status, hint) in [
            (401, None),
            (403, None),
            (403, Some("PERMISSION_DENIED")),
            (400, Some("invalid_api_key")),
            (404, None),
            (409, None),
            (422, None),
            (405, None),
        ] {
            let kind = classify_http(status, hint);
            let err = provider_error_for(kind.clone(), status.to_string(), "boom");
            assert_eq!(
                err.retryable,
                kind.is_retryable(),
                "{status} {hint:?}: envelope retryability must mirror the kind"
            );
            assert!(!err.retryable, "{status} {hint:?} must be terminal");
            let code = status.to_string();
            assert_eq!(err.code.as_deref(), Some(code.as_str()));
        }
        // Retryable rows keep the envelope retryable.
        for (status, hint) in [(429, None), (500, None), (503, None), (408, None)] {
            let kind = classify_http(status, hint);
            let err = provider_error_for(kind.clone(), status.to_string(), "boom");
            assert!(err.retryable, "{status} {hint:?} must stay retryable");
            let code = status.to_string();
            assert_eq!(err.code.as_deref(), Some(code.as_str()));
        }
        // Kinds surface typed through the envelope: core `Permission` is the
        // frozen `Auth` envelope; unrepresentable terminal kinds fold to the
        // non-retryable `BadRequest` envelope.
        let err = provider_error_for(ErrorKind::Permission, "401", "k");
        assert_eq!(err.kind, ProviderErrorKind::Auth);
        let err = provider_error_for(ErrorKind::Permission, "403", "k");
        assert_eq!(err.kind, ProviderErrorKind::Auth);
        let err = provider_error_for(ErrorKind::Internal, "422", "k");
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        let err = provider_error_for(ErrorKind::NotFound, "404", "k");
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        let err = provider_error_for(ErrorKind::Conflict, "409", "k");
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        let err = provider_error_for(ErrorKind::RateLimited, "429", "k");
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
    }

    #[test]
    fn core_error_for_carries_the_classified_kind_and_message() {
        let err = core_error_for(ErrorKind::Permission, "invalid api key");
        assert_eq!(err.kind, ErrorKind::Permission);
        assert!(err.message.contains("invalid api key"));
        assert!(!err.retryable);
        let err = core_error_for(classify_http(422, None), "malformed request");
        assert_eq!(err.kind, ErrorKind::Malformed);
    }

    #[test]
    fn hint_kind_token_families() {
        assert_eq!(hint_kind("authentication_error"), Some(ErrorHint::Auth));
        assert_eq!(hint_kind("Invalid_API_Key"), Some(ErrorHint::Auth));
        assert_eq!(hint_kind("unauthorized"), Some(ErrorHint::Auth));
        // The conservative family match keeps every auth-shaped spelling the
        // replaced in-stream classifier caught (it used `contains("auth")`).
        assert_eq!(hint_kind("authorization_error"), Some(ErrorHint::Auth));
        assert_eq!(hint_kind("authentication_failure"), Some(ErrorHint::Auth));
        assert_eq!(hint_kind("oauth_error"), Some(ErrorHint::Auth));
        assert_eq!(hint_kind("PERMISSION_DENIED"), Some(ErrorHint::Permission));
        assert_eq!(hint_kind("forbidden"), Some(ErrorHint::Permission));
        assert_eq!(hint_kind("rate_limit"), Some(ErrorHint::RateLimited));
        assert_eq!(
            hint_kind("RESOURCE_EXHAUSTED"),
            Some(ErrorHint::RateLimited)
        );
        assert_eq!(
            hint_kind("rate_limit_exceeded"),
            Some(ErrorHint::RateLimited)
        );
        assert_eq!(hint_kind("too_many_requests"), Some(ErrorHint::RateLimited));
        assert_eq!(hint_kind("internal_error"), None);
        assert_eq!(hint_kind("invalid_request_error"), None);
        assert_eq!(hint_kind(""), None);
        assert_eq!(hint_kind("  "), None);
    }

    #[test]
    fn http_message_shape_follows_the_classified_auth_disposition() {
        let scrubber = ErrorScrubber::new();
        // 403 quota is rate-limit class: retryable, and its scrubbed body is
        // kept (the status-only rule would have claimed an auth failure).
        let err = provider_error_for_http_with_scrubber(
            403,
            r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"quota exhausted"}}"#,
            &scrubber,
        );
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
        assert!(err.retryable);
        assert!(err.message.contains("HTTP 403"), "{}", err.message);
        assert!(
            !err.message.contains("authentication failure"),
            "{}",
            err.message
        );
        // 400 bad key is auth class: terminal and withheld.
        let err = provider_error_for_http_with_scrubber(
            400,
            r#"{"error":{"code":"invalid_api_key","message":"bad key"}}"#,
            &scrubber,
        );
        assert_eq!(err.kind, ProviderErrorKind::Auth);
        assert!(!err.retryable);
        assert!(err.message.contains("withheld"), "{}", err.message);
        assert!(!err.message.contains("bad key"), "{}", err.message);
        // 401/403 without a hint stay auth and withheld.
        for status in [401u16, 403] {
            let err = provider_error_for_http_with_scrubber(status, "denied", &scrubber);
            assert_eq!(err.kind, ProviderErrorKind::Auth, "status {status}");
            assert!(err.message.contains("withheld"), "status {status}");
        }
    }

    #[test]
    fn hint_only_classification_for_sse_error_events() {
        assert_eq!(
            classify_hint_only(Some("invalid_api_key")),
            Some(ErrorKind::Permission)
        );
        assert_eq!(
            classify_hint_only(Some("PERMISSION_DENIED")),
            Some(ErrorKind::Permission)
        );
        assert_eq!(
            classify_hint_only(Some("rate_limit_exceeded")),
            Some(ErrorKind::RateLimited)
        );
        assert_eq!(classify_hint_only(None), None);
        assert_eq!(classify_hint_only(Some("internal_error")), None);
        assert_eq!(classify_hint_only(Some("")), None);
    }
}
