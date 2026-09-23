//! Provider configuration hardening shared by every adapter (P0
//! plaintext-secret fix).
//!
//! Adapter configs carry two credential shapes:
//!
//! - one API key ([`faktor_security::secret::SecretValue`]) — wrapped,
//!   redacted `Debug`, zeroized on drop, exposed only through `expose()`;
//! - arbitrary gateway headers ([`ExtraHeaders`]) — validated names and
//!   values at CONFIGURATION time (an invalid header is a typed
//!   [`ProviderConfigError`], never a silently dropped request header), and
//!   a masking `Debug` for auth/secret-shaped names.
//!
//! Errors in this module never carry the credential (or any part of it):
//! they name the shape of the problem only. [`bearer_auth_header`] builds
//! the `Authorization` header value through a zeroizing temporary, so an
//! invalid credential fails the request with a typed error instead of
//! turning the request anonymous.
//!
//! Everything is bounded: header count, name length and value length.

use std::fmt;

pub use faktor_security::secret::SecretValue;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use zeroize::Zeroizing;

/// Maximum number of extra headers accepted on one provider config.
pub const MAX_EXTRA_HEADERS: usize = 32;
/// Maximum length in bytes of one extra-header NAME.
pub const MAX_EXTRA_HEADER_NAME_BYTES: usize = 128;
/// Maximum length in bytes of one extra-header VALUE.
pub const MAX_EXTRA_HEADER_VALUE_BYTES: usize = 8 * 1024;

/// A provider configuration refusal. Contains no header names, no header
/// values and no credentials — only the shape of the problem, so it is
/// always safe to log/format/attach to a `ProviderError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConfigError {
    /// The configured credential cannot be encoded as an HTTP header value
    /// (e.g. it contains a control character or non-ASCII bytes).
    InvalidCredential,
    /// The extra-header name is empty, too long, or not a valid HTTP token.
    InvalidHeaderName,
    /// The extra-header value is not a valid HTTP header value.
    InvalidHeaderValue,
    /// More extra headers were configured than [`MAX_EXTRA_HEADERS`].
    TooManyHeaders { count: usize, max: usize },
    /// An extra-header value exceeds [`MAX_EXTRA_HEADER_VALUE_BYTES`].
    HeaderValueTooLarge { len: usize, max: usize },
}

impl fmt::Display for ProviderConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderConfigError::InvalidCredential => write!(
                f,
                "provider credential cannot be encoded as an HTTP header value \
                 (value withheld)"
            ),
            ProviderConfigError::InvalidHeaderName => {
                write!(f, "extra header name is not a valid HTTP header name")
            }
            ProviderConfigError::InvalidHeaderValue => {
                write!(
                    f,
                    "extra header value is not a valid HTTP header value \
                     (value withheld)"
                )
            }
            ProviderConfigError::TooManyHeaders { count, max } => {
                write!(
                    f,
                    "too many extra headers: {count} configured, {max} allowed"
                )
            }
            ProviderConfigError::HeaderValueTooLarge { len, max } => {
                write!(
                    f,
                    "extra header value too large: {len} bytes, {max} allowed \
                     (value withheld)"
                )
            }
        }
    }
}

impl std::error::Error for ProviderConfigError {}

/// `true` iff a header name is auth/secret-shaped: its alphanumeric tokens
/// contain `auth`, `token`, `secret`, `password`, `credential`, `cookie`,
/// `signature`, `key`, `bearer` or `session` (case-insensitive). Used by
/// [`ExtraHeaders`]' `Debug` to mask values of such headers.
pub fn is_secret_header_name(name: &str) -> bool {
    const NEEDLES: [&str; 10] = [
        "auth",
        "token",
        "secret",
        "password",
        "credential",
        "cookie",
        "signature",
        "key",
        "bearer",
        "session",
    ];
    name.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| NEEDLES.iter().any(|needle| token.contains(needle)))
}

/// Validated extra headers forwarded on a gateway-style request.
///
/// Construction validates every name (`HeaderName`) and value
/// (`HeaderValue`) and enforces the count/size bounds, so an invalid header
/// can never silently disappear later: [`ExtraHeaders::apply`] inserts
/// exactly the configured set. `Debug` masks the VALUE of every
/// auth/secret-shaped name (see [`is_secret_header_name`]).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ExtraHeaders {
    entries: Vec<(HeaderName, SecretValue)>,
}

impl ExtraHeaders {
    /// No extra headers.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Validate and wrap configured `(name, value)` pairs. Any invalid name
    /// or value, or any bound breach, is a typed [`ProviderConfigError`].
    pub fn try_new<I, N, V>(pairs: I) -> Result<Self, ProviderConfigError>
    where
        I: IntoIterator<Item = (N, V)>,
        N: AsRef<str>,
        V: AsRef<str>,
    {
        let mut entries = Vec::new();
        for (name, value) in pairs {
            if entries.len() >= MAX_EXTRA_HEADERS {
                return Err(ProviderConfigError::TooManyHeaders {
                    count: entries.len() + 1,
                    max: MAX_EXTRA_HEADERS,
                });
            }
            let name = name.as_ref();
            let value = value.as_ref();
            if name.is_empty() || name.len() > MAX_EXTRA_HEADER_NAME_BYTES {
                return Err(ProviderConfigError::InvalidHeaderName);
            }
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ProviderConfigError::InvalidHeaderName)?;
            if value.len() > MAX_EXTRA_HEADER_VALUE_BYTES {
                return Err(ProviderConfigError::HeaderValueTooLarge {
                    len: value.len(),
                    max: MAX_EXTRA_HEADER_VALUE_BYTES,
                });
            }
            // Validate now; `apply` re-encodes the stored secret.
            HeaderValue::from_str(value).map_err(|_| ProviderConfigError::InvalidHeaderValue)?;
            entries.push((name, SecretValue::new(value)));
        }
        Ok(Self { entries })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The configured `(name, value)` pairs. The value is plaintext, so
    /// callers must not log it.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_str(), value.expose()))
    }

    /// Insert every configured header into `target` (per-name replace, the
    /// same semantics the gateway extra-headers path always had). Values
    /// were validated at construction; an impossible re-encode failure is
    /// an invariant breach, never silent data loss.
    pub fn apply(&self, target: &mut HeaderMap) {
        for (name, value) in &self.entries {
            match HeaderValue::from_str(value.expose()) {
                Ok(value) => {
                    target.insert(name.clone(), value);
                }
                Err(_) => debug_assert!(false, "ExtraHeaders values are validated on construction"),
            }
        }
    }
}

impl fmt::Debug for ExtraHeaders {
    /// Redacted by name shape: auth/secret-shaped headers print
    /// `[redacted]` instead of their value; non-auth headers (referer,
    /// title, ...) print verbatim because they are configuration metadata.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries: Vec<(&str, &str)> = self
            .entries
            .iter()
            .map(|(name, value)| {
                let name = name.as_str();
                let rendered = if is_secret_header_name(name) {
                    "[redacted]"
                } else {
                    value.expose()
                };
                (name, rendered)
            })
            .collect();
        f.debug_tuple("ExtraHeaders").field(&entries).finish()
    }
}

/// Build the `Authorization: Bearer <key>` header value for a wrapped key.
/// The transient plaintext buffer is zeroized; an unencodable credential is
/// a typed error that MUST fail the request — the historical
/// `if let Ok(v)` silently dropped the header and turned an authenticated
/// request anonymous.
pub fn bearer_auth_header(key: &SecretValue) -> Result<HeaderValue, ProviderConfigError> {
    let mut raw = Zeroizing::new(String::with_capacity("Bearer ".len() + key.len()));
    raw.push_str("Bearer ");
    raw.push_str(key.expose());
    HeaderValue::from_str(&raw).map_err(|_| ProviderConfigError::InvalidCredential)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recognisable planted credential; no rendering below may contain it.
    const PLANTED: &str = "PLANTED-API-KEY-abcdef0123456789";

    #[test]
    fn invalid_credential_is_typed_and_redacted() {
        let injected = SecretValue::new("key-with-\r\n-injection");
        let err = bearer_auth_header(&injected).unwrap_err();
        let rendered = format!("{err} {err:?}");
        assert_eq!(err, ProviderConfigError::InvalidCredential);
        assert!(
            !rendered.contains("injection"),
            "credential leaked: {rendered}"
        );
        assert!(
            !rendered.contains("key-with"),
            "credential leaked: {rendered}"
        );
    }

    #[test]
    fn valid_credential_yields_bearer_value() {
        let key = SecretValue::new(PLANTED);
        let value = bearer_auth_header(&key).unwrap();
        assert_eq!(value.to_str().unwrap(), format!("Bearer {PLANTED}"));
    }

    #[test]
    fn extra_headers_validate_names_values_and_bounds() {
        assert!(ExtraHeaders::try_new(Vec::<(String, String)>::new())
            .unwrap()
            .is_empty());
        assert_eq!(
            ExtraHeaders::try_new([("x-bad header", "v")]).unwrap_err(),
            ProviderConfigError::InvalidHeaderName
        );
        assert_eq!(
            ExtraHeaders::try_new([("", "v")]).unwrap_err(),
            ProviderConfigError::InvalidHeaderName
        );
        assert_eq!(
            ExtraHeaders::try_new([("x-ok", "bad\nvalue")]).unwrap_err(),
            ProviderConfigError::InvalidHeaderValue
        );
        let long_name = "x".repeat(MAX_EXTRA_HEADER_NAME_BYTES + 1);
        assert_eq!(
            ExtraHeaders::try_new([(long_name.as_str(), "v")]).unwrap_err(),
            ProviderConfigError::InvalidHeaderName
        );
        let long_value = "v".repeat(MAX_EXTRA_HEADER_VALUE_BYTES + 1);
        assert_eq!(
            ExtraHeaders::try_new([("x-ok", long_value.as_str())]).unwrap_err(),
            ProviderConfigError::HeaderValueTooLarge {
                len: MAX_EXTRA_HEADER_VALUE_BYTES + 1,
                max: MAX_EXTRA_HEADER_VALUE_BYTES,
            }
        );
        let too_many: Vec<(String, String)> = (0..=MAX_EXTRA_HEADERS)
            .map(|i| (format!("x-h-{i}"), "v".to_string()))
            .collect();
        assert_eq!(
            ExtraHeaders::try_new(too_many).unwrap_err(),
            ProviderConfigError::TooManyHeaders {
                count: MAX_EXTRA_HEADERS + 1,
                max: MAX_EXTRA_HEADERS,
            }
        );
    }

    #[test]
    fn extra_headers_debug_masks_auth_shaped_values_only() {
        let headers = ExtraHeaders::try_new([
            ("X-Title", "Faktor"),
            ("Authorization", PLANTED),
            ("X-Api-Key", PLANTED),
            ("Cookie", PLANTED),
            ("X-Custom-Token", PLANTED),
        ])
        .unwrap();
        let rendered = format!("{headers:?}");
        assert!(
            !rendered.contains(PLANTED),
            "extra header leaked: {rendered}"
        );
        assert!(rendered.matches("[redacted]").count() >= 4);
        assert!(rendered.contains("Faktor"), "non-auth values stay visible");
    }

    #[test]
    fn extra_headers_apply_inserts_validated_pairs_verbatim() {
        let headers =
            ExtraHeaders::try_new([("X-Title", "Faktor"), ("Authorization", "Bearer override")])
                .unwrap();
        let mut target = HeaderMap::new();
        headers.apply(&mut target);
        assert_eq!(target.get("x-title").unwrap(), "Faktor");
        assert_eq!(target.get("authorization").unwrap(), "Bearer override");
        assert_eq!(headers.iter().count(), 2);
    }

    #[test]
    fn secret_header_name_matcher() {
        for name in [
            "authorization",
            "Proxy-Authorization",
            "x-api-key",
            "x-goog-api-key",
            "Cookie",
            "x-auth-token",
            "X-Credential",
            "x-session",
        ] {
            assert!(is_secret_header_name(name), "{name} must be masked");
        }
        for name in [
            "x-title",
            "http-referer",
            "user-agent",
            "accept",
            "x-request-id",
        ] {
            assert!(!is_secret_header_name(name), "{name} is metadata");
        }
    }

    #[test]
    fn config_errors_never_render_planted_values() {
        // Errors carry shape only, never the value.
        for err in [
            ProviderConfigError::InvalidCredential,
            ProviderConfigError::InvalidHeaderName,
            ProviderConfigError::InvalidHeaderValue,
            ProviderConfigError::TooManyHeaders { count: 99, max: 32 },
            ProviderConfigError::HeaderValueTooLarge { len: 99, max: 32 },
        ] {
            let rendered = format!("{err} {err:?}");
            assert!(!rendered.contains(PLANTED), "error leaked: {rendered}");
        }
    }
}
