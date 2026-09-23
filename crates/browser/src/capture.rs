//! Bounded capture and redaction (spec §9/§11/§15).
//!
//! Everything the browser authority returns is bounded before it leaves this
//! module, and every header/URL that could carry a credential is redacted.
//! Cookies never enter a capture artifact and are never model-visible: the
//! header redaction drops `Cookie`/`Set-Cookie` values, and this crate has no
//! API that reads cookie values at all.

/// Hard capture bounds. All values are byte caps; zero is refused by
/// validation so a bound can never be silently disabled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureLimits {
    /// Per-response-body capture cap (decoded bytes).
    pub max_body_bytes: usize,
    /// Hard cap above which a body is refused outright instead of
    /// truncated.
    pub hard_max_body_bytes: usize,
    /// DOM/outer-HTML capture cap.
    pub max_dom_bytes: usize,
    /// Evaluated-text capture cap.
    pub max_text_bytes: usize,
    /// Screenshot capture cap.
    pub max_screenshot_bytes: usize,
    /// Bounded network record ring per page.
    pub max_network_records: usize,
    /// Per-command CDP message cap.
    pub max_cdp_message_bytes: usize,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 256 * 1024,
            hard_max_body_bytes: 4 * 1024 * 1024,
            max_dom_bytes: 1024 * 1024,
            max_text_bytes: 512 * 1024,
            max_screenshot_bytes: 4 * 1024 * 1024,
            max_network_records: 1024,
            max_cdp_message_bytes: 8 * 1024 * 1024,
        }
    }
}

impl CaptureLimits {
    /// Bounds are explicit and finite: a zero or inverted cap is a typed
    /// refusal, never a silently unbounded capture.
    pub fn validate(&self) -> Result<(), crate::error::BrowserError> {
        let checks: [(&str, usize); 7] = [
            ("max_body_bytes", self.max_body_bytes),
            ("hard_max_body_bytes", self.hard_max_body_bytes),
            ("max_dom_bytes", self.max_dom_bytes),
            ("max_text_bytes", self.max_text_bytes),
            ("max_screenshot_bytes", self.max_screenshot_bytes),
            ("max_network_records", self.max_network_records),
            ("max_cdp_message_bytes", self.max_cdp_message_bytes),
        ];
        for (name, value) in checks {
            if value == 0 {
                return Err(crate::error::BrowserError::invalid_config(format!(
                    "capture bound {name} must be > 0"
                )));
            }
        }
        if self.max_body_bytes > self.hard_max_body_bytes {
            return Err(crate::error::BrowserError::invalid_config(
                "capture max_body_bytes must not exceed hard_max_body_bytes",
            ));
        }
        if self.hard_max_body_bytes > 64 * 1024 * 1024 {
            return Err(crate::error::BrowserError::invalid_config(
                "capture hard_max_body_bytes exceeds the 64MiB absolute ceiling",
            ));
        }
        Ok(())
    }
}

/// Header names whose VALUES are always redacted before any capture/log.
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "proxy-authorization",
    "x-api-key",
    "x-auth-token",
    "x-csrf-token",
];

/// Is this header name credential-bearing?
pub fn header_is_sensitive(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    SENSITIVE_HEADERS.contains(&lower.as_str())
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
}

/// Redact credential-bearing header values. `[redacted]` replaces the value;
/// the name is kept so diagnostics can still say the header existed.
pub fn redact_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            if header_is_sensitive(name) {
                (name.clone(), "[redacted]".to_string())
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect()
}

/// Query parameter names whose values are credentials and are never
/// rendered. Matched as canonical identities, not substrings: `keychain`
/// and `signed` are not credentials, `api_key`/`api-key`/`api key` and
/// `api%5Fkey` are one identity.
const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "token",
    "key",
    "apikey",
    "api_key",
    "access_token",
    "client_secret",
    "secret",
    "password",
    "passwd",
    "auth",
    "sign",
    "signature",
    "sig",
    "session",
    "cookie",
    "credential",
];

/// Decode one hex digit.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode one `application/x-www-form-urlencoded` component
/// (`+` => space, `%XX` => byte). Returns `None` for a truncated or
/// non-hex escape, or when the decoded bytes are not valid UTF-8: the
/// caller treats that as sensitive.
fn decode_form_component(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                let high = hex_nibble(*bytes.get(index + 1)?)?;
                let low = hex_nibble(*bytes.get(index + 2)?)?;
                out.push((high << 4) | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Decode a name for classification only, repeatedly and bounded, so a
/// multiply-encoded spelling (`api%254Bey`) classifies as its plain form.
/// Every changing pass either shrinks the string or turns a `+` into a
/// space (stable on the next pass), so `raw.len() + 1` passes suffice; not
/// stabilizing within the bound is malformed and fails closed (`None`).
fn decode_form_name(raw: &str) -> Option<String> {
    let mut current = raw.to_string();
    for _ in 0..=raw.len() {
        let decoded = decode_form_component(&current)?;
        if decoded == current {
            return Some(decoded);
        }
        current = decoded;
    }
    None
}

/// Canonical form of a decoded name for sensitive-name classification:
/// ASCII-lowercased alphanumerics only, so `api_key`, `api-key`, `api key`
/// (from `+`), `apiKey` and `apikey` are one identity while any other name
/// keeps its full distinguishing content.
fn canonical_sensitive_name(name: &str) -> Vec<u8> {
    name.bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|byte| byte.to_ascii_lowercase())
        .collect()
}

/// True when a decoded name matches one entry of the sensitive table under
/// the canonical form.
fn is_sensitive_name(name: &str, table: &[&str]) -> bool {
    let canonical = canonical_sensitive_name(name);
    table
        .iter()
        .any(|sensitive| canonical_sensitive_name(sensitive) == canonical)
}

/// True when a raw query parameter name carries a credential. The stored
/// value must be masked whenever this returns true.
fn param_is_sensitive(raw_name: &str) -> bool {
    match decode_form_name(raw_name) {
        Some(decoded) => is_sensitive_name(&decoded, SENSITIVE_QUERY_PARAMS),
        None => true,
    }
}

/// Redact credential-shaped query parameters from a URL (logging only; the
/// live URL is never rewritten).
///
/// Names are classified structurally: each name is percent-decoded
/// (repeatedly, bounded) with `application/x-www-form-urlencoded`
/// semantics and canonicalized (ASCII lowercase, separators ignored) before
/// comparison, so `api%4Bey`, `access%5ftoken` and `client%5Fsecret`
/// classify as their plain forms. Only the value of a sensitive pair is
/// masked; the original percent-encoding of sensitive names and of every
/// non-sensitive pair is preserved byte for byte. A name whose escape
/// sequence cannot be decoded or never stabilizes is treated as sensitive
/// (fail closed: a false positive only redacts, a false negative would
/// leak).
///
/// Parity requirement: keep the decode/canonicalize pipeline in lockstep
/// with `crates/commerce-connectors/src/http.rs` (`decode_form_component`,
/// `decode_form_name`, `canonical_sensitive_name`, `is_sensitive_query_name`),
/// which fixes the same defect class for connector requests.
pub fn redact_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(base);
    out.push('?');
    for (index, pair) in query.split('&').enumerate() {
        if index > 0 {
            out.push('&');
        }
        match pair.split_once('=') {
            Some((name, _)) if param_is_sensitive(name) => {
                out.push_str(name);
                out.push_str("=[redacted]");
            }
            _ => out.push_str(pair),
        }
    }
    out
}

/// Mask **every** query value, for log lines only (never for stored or
/// model-visible records). Path, host, parameter names and pairs without `=`
/// are preserved byte for byte; each `name=value` pair becomes
/// `name=[redacted]` regardless of the name, so an unrecognized credential
/// parameter cannot leak into a log.
pub fn redact_url_query_values(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(base);
    out.push('?');
    for (index, pair) in query.split('&').enumerate() {
        if index > 0 {
            out.push('&');
        }
        match pair.split_once('=') {
            Some((name, _)) => {
                out.push_str(name);
                out.push_str("=[redacted]");
            }
            None => out.push_str(pair),
        }
    }
    out
}

/// A captured text artifact with an explicit truncation flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedText {
    pub text: String,
    pub truncated: bool,
    pub byte_len: usize,
}

/// A captured byte artifact with an explicit truncation flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedBytes {
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub byte_len: usize,
}

/// Truncate `raw` to `cap` bytes on a char boundary (never split UTF-8).
pub fn bound_text(raw: &str, cap: usize) -> CapturedText {
    let byte_len = raw.len();
    if byte_len <= cap {
        return CapturedText {
            text: raw.to_string(),
            truncated: false,
            byte_len,
        };
    }
    let mut end = cap;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    CapturedText {
        text: raw[..end].to_string(),
        truncated: true,
        byte_len,
    }
}

/// Truncate raw bytes to `cap`.
pub fn bound_bytes(raw: Vec<u8>, cap: usize) -> CapturedBytes {
    let byte_len = raw.len();
    if byte_len <= cap {
        return CapturedBytes {
            bytes: raw,
            truncated: false,
            byte_len,
        };
    }
    let mut bytes = raw;
    bytes.truncate(cap);
    CapturedBytes {
        bytes,
        truncated: true,
        byte_len,
    }
}

/// Decode a CDP `{body, base64Encoded}` pair under an explicit bound. The
/// encoded size is checked BEFORE decoding so an oversized hostile body is
/// refused without materializing it.
pub fn decode_cdp_body(
    body: &str,
    base64_encoded: bool,
    cap: usize,
    hard_cap: usize,
) -> Result<CapturedBytes, crate::error::BrowserError> {
    if !base64_encoded {
        let bytes = body.as_bytes().to_vec();
        if bytes.len() > hard_cap {
            return Err(crate::error::BrowserError::ResponseTooLarge {
                limit_bytes: hard_cap,
                observed_bytes: Some(bytes.len()),
            });
        }
        return Ok(bound_bytes(bytes, cap));
    }
    // base64 expands 4/3; check the encoded length first.
    let encoded_max = hard_cap.saturating_mul(4).div_ceil(3) + 4;
    if body.len() > encoded_max {
        return Err(crate::error::BrowserError::ResponseTooLarge {
            limit_bytes: hard_cap,
            observed_bytes: None,
        });
    }
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| crate::error::BrowserError::Cdp {
            detail: format!("invalid base64 CDP body: {e}"),
        })?;
    if decoded.len() > hard_cap {
        return Err(crate::error::BrowserError::ResponseTooLarge {
            limit_bytes: hard_cap,
            observed_bytes: Some(decoded.len()),
        });
    }
    Ok(bound_bytes(decoded, cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_bound_is_utf8_safe_and_flags_truncation() {
        let raw = "日本語テキスト";
        let bounded = bound_text(raw, 4);
        assert!(bounded.truncated);
        assert_eq!(bounded.text, "日");
        assert_eq!(bounded.byte_len, raw.len());
        let exact = bound_text(raw, raw.len());
        assert!(!exact.truncated);
        assert_eq!(exact.text, raw);
    }

    #[test]
    fn sensitive_headers_are_redacted_by_name_not_by_value() {
        let headers = vec![
            ("Cookie".to_string(), "session=supersecret".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Auth-Token".to_string(), "bearer-abcdef".to_string()),
        ];
        let redacted = redact_headers(&headers);
        assert_eq!(redacted[0].1, "[redacted]");
        assert_eq!(redacted[1].1, "application/json");
        assert_eq!(redacted[2].1, "[redacted]");
        assert!(!format!("{redacted:?}").contains("supersecret"));
        assert!(!format!("{redacted:?}").contains("bearer-abcdef"));
    }

    #[test]
    fn urls_redact_credential_shaped_params_only() {
        let url = "https://x.test/p?q=shoes&token=abc&page=2&api_key=xyz";
        let redacted = redact_url(url);
        assert!(redacted.contains("q=shoes"));
        assert!(redacted.contains("page=2"));
        assert!(!redacted.contains("abc"));
        assert!(!redacted.contains("xyz"));
        assert_eq!(redact_url("https://x.test/p"), "https://x.test/p");
    }

    /// Encoded spellings of sensitive names that raw-name classification
    /// missed. Every one must mask only the value, keep the name's original
    /// spelling and leave non-sensitive pairs byte for byte unchanged.
    const ENCODED_SENSITIVE_NAMES: &[&str] = &[
        "api%4Bey",
        "api%4bey",
        "API%4bEY",
        "api%5Fkey",
        "api%5fkey",
        "api+key",
        "api%2Bkey",
        "api.key",
        "api-key",
        "apikey",
        "access%5ftoken",
        "access%5Ftoken",
        "access-token",
        "ACCESS%5FTOKEN",
        "client%5Fsecret",
        "client%5fsecret",
        "client-secret",
        "CLIENT%5FSECRET",
        "sec%72et",
        "si%67",
        "to%6ben",
        // Multiply-encoded spellings must classify as their plain form.
        "api%254Bey",
        "access%25255ftoken",
        "client%2525255Fsecret",
    ];

    #[test]
    fn urls_redact_encoded_credential_names_and_preserve_everything_else() {
        const SECRET: &str = "SECRET-CREDENTIAL-0123456789";
        for spelling in ENCODED_SENSITIVE_NAMES {
            let url = format!("https://x.test/p?{spelling}={SECRET}&plain=keep%20this&limit=10");
            let rendered = redact_url(&url);
            assert!(
                !rendered.contains(SECRET),
                "{spelling} leaked the credential: {rendered}"
            );
            assert!(
                !format!("{rendered:?}").contains(SECRET),
                "{spelling} leaked through Debug: {rendered:?}"
            );
            assert!(
                rendered.contains(&format!("{spelling}=[redacted]")),
                "{spelling} must keep its original spelling and mask only the value: {rendered}"
            );
            assert!(
                rendered.ends_with("&plain=keep%20this&limit=10"),
                "{spelling} changed a non-sensitive pair: {rendered}"
            );
        }
    }

    #[test]
    fn urls_fail_closed_on_malformed_or_undecodable_names() {
        // Truncated/non-hex escapes, non-UTF-8 decoded names and names that
        // never stabilize can carry the credential too: masking is the only
        // safe verdict. A false positive only redacts; a false negative
        // would leak.
        for hostile in [
            "api%Key",
            "api%4",
            "api%",
            "%4Bey",
            "api%FFkey",
            "%c3%28token",
        ] {
            let url = format!("https://x.test/p?{hostile}=SECRET&plain=keep");
            let rendered = redact_url(&url);
            assert!(
                !rendered.contains("SECRET"),
                "{hostile} leaked through fail-closed classification: {rendered}"
            );
            assert!(
                rendered.contains(&format!("{hostile}=[redacted]")),
                "{hostile} must mask the value while keeping the raw name: {rendered}"
            );
            assert!(
                rendered.ends_with("&plain=keep"),
                "non-sensitive pairs are untouched: {rendered}"
            );
        }
    }

    #[test]
    fn urls_leave_clean_and_non_sensitive_urls_unchanged() {
        for clean in [
            "https://x.test/p",
            "https://x.test/p?a=1&b%20c=2&limit=10",
            "https://x.test/p?apricot=1&q=shoes&page=2",
            // Substring matches are not identities: these are not credentials.
            "https://x.test/p?keychain=2&monkey=1&signed=3",
            // The name is not a value: a pair without `=` carries no secret.
            "https://x.test/p?token",
        ] {
            assert_eq!(redact_url(clean), clean, "{clean} must be unchanged");
        }
    }

    #[test]
    fn oversized_bodies_are_refused_before_decoding() {
        let encoded = "A".repeat(200);
        let err = decode_cdp_body(&encoded, true, 8, 16).expect_err("must refuse");
        assert_eq!(err.code(), "response_too_large");
        assert_eq!(
            err,
            crate::error::BrowserError::ResponseTooLarge {
                limit_bytes: 16,
                observed_bytes: None
            }
        );
        // A body inside the hard cap but over the soft cap truncates.
        let bounded = decode_cdp_body("aGVsbG8=", true, 3, 64).unwrap();
        assert!(bounded.truncated);
        assert_eq!(bounded.bytes, b"hel");
        assert_eq!(bounded.byte_len, 5);
    }

    #[test]
    fn log_query_value_masking_hides_every_value() {
        let url = "https://x.test/p?q=shoes&token=abc&page=2&bare&limit=10";
        let rendered = redact_url_query_values(url);
        assert_eq!(
            rendered,
            "https://x.test/p?q=[redacted]&token=[redacted]&page=[redacted]&bare&limit=[redacted]"
        );
        assert!(!rendered.contains("shoes"));
        assert!(!rendered.contains("abc"));
        // No query: byte-identical.
        assert_eq!(
            redact_url_query_values("https://x.test/p"),
            "https://x.test/p"
        );
    }

    #[test]
    fn zero_bounds_are_refused() {
        let limits = CaptureLimits {
            max_body_bytes: 0,
            ..CaptureLimits::default()
        };
        assert!(limits.validate().is_err());
        let limits = CaptureLimits {
            max_body_bytes: CaptureLimits::default().hard_max_body_bytes + 1,
            ..CaptureLimits::default()
        };
        assert!(limits.validate().is_err());
        assert!(CaptureLimits::default().validate().is_ok());
    }
}
