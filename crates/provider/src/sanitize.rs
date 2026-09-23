//! Provider error-body sanitization (audit P1: upstream bodies must never
//! reach `ProviderError` or model context unscrubbed).
//!
//! Adapters already bound how many bytes of a non-success response body they
//! read, but a bounded body is still hostile: providers echo request
//! snippets, and gateways can reflect credentials. Every adapter therefore
//! passes the error text through ONE registered scrubber before it is
//! stored:
//!
//! - [`ErrorScrubber`] registers every credential the request actually
//!   carried ([`ErrorScrubber::with_request_credentials`]: credential
//!   headers and credential-named query parameters) as an exact,
//!   fingerprinted secret, and redacts those values plus the frozen
//!   [`faktor_security::SecretPolicy`] patterns;
//! - [`ErrorScrubber::diagnostic`] is the one safe shape used by every
//!   adapter: a safe `HTTP <status>` code plus the scrubbed, bounded
//!   diagnostic. Authentication failures (401/403/407) withhold the
//!   upstream body entirely — arbitrary body text is never preserved for
//!   auth errors by default;
//! - raw support diagnostics are deliberately NOT retained here: the error
//!   path only carries the scrubbed diagnostic, and any caller that needs
//!   bounded raw bytes for support must put them into a protected evidence
//!   artifact instead of the error/model context.

use std::fmt;
use std::sync::{Arc, Mutex};

use reqwest::header::HeaderMap;

use faktor_security::registry::SecretRegistry;
use faktor_security::{redact, SecretPolicy};

/// Hard bound on the scrubbed diagnostic carried inside one
/// [`crate::ProviderError`] message (bytes).
pub const MAX_ERROR_DIAGNOSTIC_BYTES: usize = 4 * 1024;

/// Bounded output growth on hostile input (at most this many exact-value
/// redactions replace one span each).
const MAX_SCRUB_HITS: usize = 64;

/// Header names whose VALUE is a credential (case-insensitive). The value
/// is registered by fingerprint; names are never secrets and are never
/// logged.
const CREDENTIAL_HEADER_NAMES: [&str; 5] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
];

/// Query parameter names whose VALUE is a credential (folded to lowercase,
/// `-` normalized to `_`). Gemini-style `?key=`, OAuth-style
/// `access_token=`, and the common `api_key=` spellings.
fn is_credential_query_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().replace('-', "_").as_str(),
        "key" | "api_key" | "apikey" | "access_token" | "token"
    )
}

/// The registered secret scrubber every adapter error path shares. Cheap to
/// clone (one `Arc`); the registry stores fingerprints only.
#[derive(Clone)]
pub struct ErrorScrubber {
    inner: Arc<ScrubberInner>,
}

struct ScrubberInner {
    registry: Mutex<SecretRegistry>,
    policy: SecretPolicy,
}

impl Default for ErrorScrubber {
    fn default() -> Self {
        Self::new()
    }
}

impl ErrorScrubber {
    /// A scrubber with the frozen default pattern policy and no registered
    /// values.
    pub fn new() -> Self {
        Self::with_policy(SecretPolicy::default())
    }

    /// A scrubber with an explicit pattern policy.
    pub fn with_policy(policy: SecretPolicy) -> Self {
        Self {
            inner: Arc::new(ScrubberInner {
                registry: Mutex::new(SecretRegistry::new()),
                policy,
            }),
        }
    }

    /// Register one exact secret (fingerprinted; the plaintext is never
    /// retained). Empty values are ignored.
    pub fn register_secret(&self, value: &str) {
        if value.is_empty() {
            return;
        }
        self.lock_registry().register(value.as_bytes());
    }

    /// Register every credential the request actually carried: credential
    /// header values (`Bearer <token>` registers the token and the full
    /// value) and credential-named query parameter values. Values are
    /// fingerprinted only; nothing is rendered or logged.
    pub fn with_request_credentials(&self, headers: &HeaderMap, url: &str) -> Self {
        for (name, value) in headers {
            if !CREDENTIAL_HEADER_NAMES
                .iter()
                .any(|known| known.eq_ignore_ascii_case(name.as_str()))
            {
                continue;
            }
            let Ok(value) = value.to_str() else {
                continue;
            };
            let token = value
                .split_once(' ')
                .map(|(_, rest)| rest.trim())
                .filter(|rest| !rest.is_empty())
                .unwrap_or(value);
            self.register_secret(token);
            if token != value {
                self.register_secret(value);
            }
        }
        if let Ok(parsed) = reqwest::Url::parse(url) {
            for (name, value) in parsed.query_pairs() {
                if is_credential_query_name(&name) && !value.is_empty() {
                    self.register_secret(&value);
                }
            }
        }
        self.clone()
    }

    /// How many exact values are registered.
    pub fn registered_len(&self) -> usize {
        self.lock_registry().len()
    }

    /// Redact every registered value and every pattern hit. Never panics,
    /// whatever the input.
    pub fn scrub(&self, text: &str) -> String {
        let after_patterns = redact(text, &self.inner.policy);
        let exact = self.lock_registry().scan_exact(after_patterns.as_bytes());
        if exact.is_empty() {
            return after_patterns;
        }
        let bytes = after_patterns.as_bytes();
        let mut out = String::with_capacity(after_patterns.len() + 32);
        let mut cursor = 0usize;
        for hit in exact.into_iter().take(MAX_SCRUB_HITS) {
            let start = hit.offset.min(bytes.len());
            let end = (hit.offset + hit.len).min(bytes.len());
            if start < cursor || start > end {
                continue;
            }
            // Offsets come from a byte scan over `after_patterns`; the
            // boundary check keeps this panic-free even if a pattern rewrite
            // shifted the text.
            if !after_patterns.is_char_boundary(start) || !after_patterns.is_char_boundary(end) {
                continue;
            }
            out.push_str(&after_patterns[cursor..start]);
            out.push_str(&hit.redacted);
            cursor = end;
        }
        out.push_str(&after_patterns[cursor..]);
        out
    }

    /// THE safe error-body shape: `HTTP <status>` plus the scrubbed, bounded
    /// diagnostic (or the status alone for an empty body). Authentication
    /// failures (401/403/407) withhold the upstream body entirely — a body
    /// on a rejected-credentials response is server-authored data and is
    /// never preserved by default.
    pub fn diagnostic(&self, status: u16, raw_body: &str) -> String {
        if is_auth_status(status) {
            return format!("HTTP {status} authentication failure (upstream body withheld)");
        }
        let scrubbed = self.scrub(raw_body);
        let (bounded, truncated) = bound_on_char_boundary(&scrubbed, MAX_ERROR_DIAGNOSTIC_BYTES);
        let trimmed = bounded.trim();
        let mut out = String::with_capacity(trimmed.len() + 64);
        out.push_str("HTTP ");
        out.push_str(&status.to_string());
        if !trimmed.is_empty() {
            out.push_str(": ");
            out.push_str(trimmed);
        }
        if truncated {
            out.push_str(" [diagnostic truncated]");
        }
        out
    }

    /// THE safe shape for an IN-STREAM error payload that arrived under an
    /// HTTP 2xx, where no status exists to classify it (an SSE
    /// `error`/`response.failed` event or a malformed data line): `label`
    /// names the wire shape, the payload is scrubbed with the registered
    /// request credentials plus the frozen patterns and bounded exactly
    /// like [`Self::diagnostic`]. An `auth_shaped` payload (detected from
    /// the structured event code, or [`auth_shaped_text`] when only raw
    /// text exists) withholds the upstream text entirely — the same rule
    /// [`Self::diagnostic`] applies to 401/403/407 bodies.
    pub fn event_diagnostic(&self, label: &str, raw_payload: &str, auth_shaped: bool) -> String {
        if auth_shaped {
            return format!("{label}: upstream body withheld (authentication failure)");
        }
        let scrubbed = self.scrub(raw_payload);
        let (bounded, truncated) = bound_on_char_boundary(&scrubbed, MAX_ERROR_DIAGNOSTIC_BYTES);
        let trimmed = bounded.trim();
        if trimmed.is_empty() {
            return label.to_string();
        }
        let mut out = String::with_capacity(label.len() + trimmed.len() + 32);
        out.push_str(label);
        out.push_str(": ");
        out.push_str(trimmed);
        if truncated {
            out.push_str(" [diagnostic truncated]");
        }
        out
    }

    /// Scrub `text` and bound it to `max_bytes` on a char boundary. For
    /// structured fields (e.g. a provider error `code`) that ride alongside
    /// [`Self::event_diagnostic`] and must obey the same redaction contract.
    pub fn scrub_bounded(&self, text: &str, max_bytes: usize) -> String {
        let scrubbed = self.scrub(text);
        let (bounded, _) = bound_on_char_boundary(&scrubbed, max_bytes);
        bounded.to_string()
    }

    fn lock_registry(&self) -> std::sync::MutexGuard<'_, SecretRegistry> {
        // Poison-tolerant: a panic elsewhere must never turn secret
        // scrubbing into a panic or a silent pass-through.
        self.inner
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// True for the statuses that reject credentials (401/403/407).
pub const fn is_auth_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 407)
}

/// True when `text` carries an authentication-failure marker. In-stream
/// payloads arrive under an HTTP 2xx, so there is no status to classify
/// them; the check folds to lowercase alphanumerics, so `invalid_api_key`,
/// `InvalidApiKey`, `authentication_error`, `unauthorized` and
/// `permission_denied` all match. Prose that merely mentions an API key is
/// withheld too — the safe direction: a withheld body can never leak a
/// secret.
pub fn auth_shaped_text(text: &str) -> bool {
    let folded: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    folded.contains("invalidapikey")
        || folded.contains("apikey")
        || folded.contains("invalidkey")
        || folded.contains("authentication")
        || folded.contains("unauthorized")
        || folded.contains("forbidden")
        || folded.contains("invalidcredential")
        || folded.contains("permissiondenied")
}

/// Cut `text` to at most `max` bytes on a char boundary. Returns the slice
/// and whether it was cut.
fn bound_on_char_boundary(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

impl fmt::Debug for ErrorScrubber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Counts only: the registry's plaintext is never retained and the
        // policy holds patterns, not secrets.
        f.debug_struct("ErrorScrubber")
            .field("registered", &self.registered_len())
            .field("scan_enabled", &self.inner.policy.scan_enabled)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLANTED_PATTERN: &str = "sk-abcdefghijklmnopqrstuvwx";
    const PLANTED_EXACT: &str = "exact-credential-value-9f2a";

    #[test]
    fn pattern_and_registered_secrets_are_redacted() {
        let scrubber = ErrorScrubber::new();
        scrubber.register_secret(PLANTED_EXACT);
        let body = format!(r#"{{"error":"key {PLANTED_PATTERN} / {PLANTED_EXACT} leaked"}}"#);
        let scrubbed = scrubber.scrub(&body);
        assert!(!scrubbed.contains(PLANTED_PATTERN), "{scrubbed}");
        assert!(!scrubbed.contains(PLANTED_EXACT), "{scrubbed}");
        assert!(scrubbed.contains("leaked"), "non-secret text survives");
        assert!(scrubbed.contains("<redacted:"));
    }

    #[test]
    fn diagnostic_withholds_auth_bodies_and_bounds_others() {
        let scrubber = ErrorScrubber::new();
        scrubber.register_secret(PLANTED_EXACT);
        // Auth: no raw body by default.
        let auth = scrubber.diagnostic(401, &format!("token {PLANTED_EXACT} rejected"));
        assert!(!auth.contains(PLANTED_EXACT));
        assert!(!auth.contains("rejected"));
        assert!(auth.contains("HTTP 401"));
        assert!(super::is_auth_status(403) && super::is_auth_status(407));
        assert!(!super::is_auth_status(429));
        // Non-auth: scrubbed and bounded.
        let long = format!(
            "{}{}",
            "x".repeat(MAX_ERROR_DIAGNOSTIC_BYTES * 2),
            PLANTED_EXACT
        );
        let bounded = scrubber.diagnostic(429, &long);
        assert!(
            bounded.len() <= MAX_ERROR_DIAGNOSTIC_BYTES + 64,
            "{}",
            bounded.len()
        );
        assert!(!bounded.contains(PLANTED_EXACT));
        assert!(bounded.contains("truncated"));
        assert!(bounded.contains("HTTP 429"));
        let empty = scrubber.diagnostic(500, "");
        assert_eq!(empty, "HTTP 500");
    }

    #[test]
    fn request_credentials_are_registered_from_headers_and_query() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {PLANTED_EXACT}").parse().unwrap(),
        );
        headers.insert("x-api-key", "header-key-1234567890".parse().unwrap());
        let scrubber = ErrorScrubber::new().with_request_credentials(
            &headers,
            "https://x.test/v1?alt=sse&key=query-key-1234567890",
        );
        // Bearer token + full authorization value + x-api-key + query key.
        assert_eq!(scrubber.registered_len(), 4);
        let body = format!("a {PLANTED_EXACT} b header-key-1234567890 c query-key-1234567890 d");
        let scrubbed = scrubber.scrub(&body);
        assert!(!scrubbed.contains(PLANTED_EXACT));
        assert!(!scrubbed.contains("header-key-1234567890"));
        assert!(!scrubbed.contains("query-key-1234567890"));
        // The Debug face never renders values.
        let debug = format!("{scrubber:?}");
        assert!(!debug.contains(PLANTED_EXACT), "{debug}");
    }

    #[test]
    fn in_stream_event_diagnostic_scrubs_bounds_or_withholds() {
        let scrubber = ErrorScrubber::new();
        scrubber.register_secret(PLANTED_EXACT);
        let payload = format!(
            "{{\"type\":\"error\",\"message\":\"SENTINEL {PLANTED_PATTERN} {PLANTED_EXACT}\"}}"
        );
        let non_auth = scrubber.event_diagnostic("bad anthropic SSE", &payload, false);
        assert!(non_auth.starts_with("bad anthropic SSE: "), "{non_auth}");
        assert!(non_auth.contains("SENTINEL"), "{non_auth}");
        assert!(!non_auth.contains(PLANTED_PATTERN), "{non_auth}");
        assert!(!non_auth.contains(PLANTED_EXACT), "{non_auth}");
        // Auth-shaped: the raw payload is withheld entirely.
        let withheld = scrubber.event_diagnostic("bad gemini SSE", &payload, true);
        assert_eq!(
            withheld,
            "bad gemini SSE: upstream body withheld (authentication failure)"
        );
        assert!(!withheld.contains("SENTINEL"), "{withheld}");
        // Oversized payload: bounded with the truncation note.
        let long = format!(
            "{}{}",
            "y".repeat(MAX_ERROR_DIAGNOSTIC_BYTES * 2),
            PLANTED_EXACT
        );
        let bounded = scrubber.event_diagnostic("bad SSE line", &long, false);
        assert!(
            bounded.len() <= MAX_ERROR_DIAGNOSTIC_BYTES + 64,
            "{}",
            bounded.len()
        );
        assert!(bounded.contains("truncated"), "{bounded}");
        assert!(!bounded.contains(PLANTED_EXACT), "{bounded}");
        // An empty payload degenerates to the label alone.
        assert_eq!(
            scrubber.event_diagnostic("bad SSE line", "", false),
            "bad SSE line"
        );
        // scrub_bounded applies the same redaction to structured fields.
        let code = scrubber.scrub_bounded(&format!("invalid_api_key {PLANTED_EXACT}"), 64);
        assert!(!code.contains(PLANTED_EXACT), "{code}");
        assert!(code.len() <= 64, "{code}");
    }

    #[test]
    fn auth_shaped_text_detects_markers_without_false_negatives() {
        for shaped in [
            "invalid_api_key",
            "InvalidApiKey",
            "invalid-key",
            "authentication_error",
            "permission_denied",
            "Unauthorized",
            "forbidden",
            "api key not valid",
        ] {
            assert!(auth_shaped_text(shaped), "{shaped} must be auth-shaped");
        }
        for plain in [
            "rate_limit_exceeded",
            "overloaded_error",
            "server_error",
            "malformed request",
            "quota exceeded",
        ] {
            assert!(!auth_shaped_text(plain), "{plain} must not be withheld");
        }
    }

    #[test]
    fn hostile_input_never_panics_and_poisoned_registry_still_scrubs() {
        let scrubber = ErrorScrubber::new();
        scrubber.register_secret(PLANTED_EXACT);
        // Multi-byte chars crossing the bound must not panic.
        let hostile = format!("{}\u{1f600}", "a".repeat(MAX_ERROR_DIAGNOSTIC_BYTES - 1));
        let out = scrubber.diagnostic(500, &hostile);
        assert!(out.len() <= MAX_ERROR_DIAGNOSTIC_BYTES + 64);
        // Poison injection: scrubbing still works, never panics.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = scrubber.inner.registry.lock().unwrap();
            panic!("poison injection");
        }));
        assert!(scrubber.inner.registry.lock().is_err());
        let scrubbed = scrubber.scrub(&format!("prefix {PLANTED_EXACT} suffix"));
        assert!(!scrubbed.contains(PLANTED_EXACT));
        assert_eq!(
            scrubber.diagnostic(401, "body").as_str(),
            "HTTP 401 authentication failure (upstream body withheld)"
        );
    }
}
