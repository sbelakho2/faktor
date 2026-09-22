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

/// Redact credential-shaped query parameters from a URL (logging only; the
/// live URL is never rewritten).
pub fn redact_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if param_is_sensitive(key) => format!("{key}=[redacted]"),
            _ => pair.to_string(),
        })
        .collect();
    format!("{base}?{}", redacted.join("&"))
}

fn param_is_sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "token",
        "key",
        "secret",
        "password",
        "passwd",
        "auth",
        "sign",
        "signature",
        "session",
        "cookie",
        "credential",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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
