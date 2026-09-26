//! Alibaba Open Platform (AOP) common-parameter request signing, shared by
//! the two Alibaba-family marketplaces ([`crate::alibaba`],
//! [`crate::china1688`]).
//!
//! The scheme implemented here is the documented AOP `param2` signature:
//!
//! 1. every request parameter — the endpoint parameters plus the common
//!    `access_token` and `_aop_timestamp` parameters — is rendered as the
//!    raw `name + value` pair, without a separator and **without** form
//!    encoding (the signature covers the value as submitted, not its wire
//!    escape);
//! 2. the pairs are sorted by parameter name in byte order and concatenated
//!    into the canonical string;
//! 3. the app secret is the HMAC-SHA1 key over that string; the uppercase
//!    hex digest is written as `_aop_signature`.
//!
//! `_aop_signature` is excluded from the canonical string by construction:
//! it is appended after the canonical string is frozen.
//!
//! # Secret discipline
//!
//! * the **app secret** never becomes a parameter, a header or a wire value:
//!   it is only the HMAC key inside [`AuthMaterial`];
//! * the **access token** is carried in the request body, never the URL, and
//!   is registered with the outbound [`SecretGuard`] the moment it is
//!   resolved or refreshed;
//! * the app key is a public identifier and follows the documented `param2`
//!   URL shape; when it is placed in the URL the request declares it as the
//!   URL credential so the outbound scanner can tell it apart from a leak.
//!
//! No nonce parameter is required by this family (`NONCE_REQUIRED` in the
//! family auth modules): the millisecond `_aop_timestamp` is the documented
//! freshness bound. The local clock is the injected `AcquireCtx` clock.

use std::fmt;
use std::sync::{Arc, Mutex};

use faktor_commerce::SourceError;

use crate::secrets::{SecretGuard, SecretString};

/// The documented common parameter carrying the bearer token.
pub(crate) const ACCESS_TOKEN_PARAM: &str = "access_token";
/// The documented common parameter carrying the request timestamp (millis).
pub(crate) const TIMESTAMP_PARAM: &str = "_aop_timestamp";
/// The documented common parameter carrying the signature.
pub(crate) const SIGNATURE_PARAM: &str = "_aop_signature";
/// A token within this window of expiry is already considered stale, so a
/// request never leaves with a token that expires in flight.
pub(crate) const DEFAULT_TOKEN_SKEW_MS: u64 = 60_000;

/// An access token with its absolute expiry (unix milliseconds).
///
/// The value is a [`SecretString`]: unprintable, unserializable, and only
/// reachable through the crate-private accessor the request builders use.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    value: SecretString,
    expires_at_ms: u64,
}

impl AccessToken {
    /// Wrap a token value with its absolute expiry.
    pub fn new(value: SecretString, expires_at_ms: u64) -> Self {
        Self {
            value,
            expires_at_ms,
        }
    }

    /// The absolute expiry in unix milliseconds.
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    /// True when the token is usable at `now_ms` under `skew_ms`.
    pub fn is_valid(&self, now_ms: u64, skew_ms: u64) -> bool {
        now_ms.saturating_add(skew_ms) < self.expires_at_ms
    }

    /// The plaintext, for wire construction only.
    pub(crate) fn value(&self) -> &SecretString {
        &self.value
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken")
            .field("value", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// The result of one token refresh exchange.
#[derive(Debug, Clone)]
pub struct RefreshedToken {
    access_token: SecretString,
    expires_at_ms: u64,
}

impl RefreshedToken {
    /// Wrap the refreshed token and its absolute expiry.
    pub fn new(access_token: SecretString, expires_at_ms: u64) -> Self {
        Self {
            access_token,
            expires_at_ms,
        }
    }

    /// The absolute expiry in unix milliseconds.
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    pub(crate) fn access_token(&self) -> &SecretString {
        &self.access_token
    }
}

/// Exchanges a refresh token for a fresh access token.
///
/// The production wiring resolves tokens from the environment (operator
/// rotation) and does not need a refresher; this seam exists so the
/// expiry/refresh path is exercised deterministically and so a daemon can
/// plug the documented OAuth exchange in later. Implementations receive the
/// configured refresh token when present and must return
/// [`SourceError::AuthenticationRequired`] (or another typed error) on
/// failure — never partial secret material in an error.
pub trait TokenRefresher: Send + Sync {
    /// Perform one exchange at `now_ms`.
    fn refresh(
        &self,
        refresh_token: Option<&SecretString>,
        now_ms: u64,
    ) -> Result<RefreshedToken, SourceError>;
}

/// Parse an absolute expiry in unix milliseconds from a configured value.
/// Digit-only and bounded; anything else is rejected (fail closed: the
/// caller treats it as "no valid token").
pub(crate) fn parse_expiry_ms(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 20 || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

/// The canonical string: raw `name + value` pairs sorted by name (byte
/// order), concatenated with no separator.
pub(crate) fn canonical_parameter_string(params: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut out = String::with_capacity(
        params
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum(),
    );
    for (name, value) in sorted {
        out.push_str(name);
        out.push_str(value);
    }
    out
}

/// Uppercase-hex HMAC-SHA1 of `canonical` under the app secret.
pub(crate) fn aop_signature_hex(app_secret: &SecretString, canonical: &str) -> String {
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha1::Sha1> as Mac>::new_from_slice(app_secret.expose().as_bytes())
        .expect("HMAC-SHA1 accepts a key of any length");
    mac.update(canonical.as_bytes());
    hex_upper(mac.finalize().into_bytes().as_slice())
}

fn hex_upper(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// The family-neutral auth state: app key, app secret, the current access
/// token (with expiry), the optional refresh token and refresher, and the
/// secret guard every resolved value is registered with.
pub(crate) struct AuthMaterial {
    app_key: SecretString,
    app_secret: SecretString,
    token: Mutex<Option<AccessToken>>,
    refresh_token: Option<SecretString>,
    refresher: Option<Arc<dyn TokenRefresher>>,
    guard: Arc<SecretGuard>,
}

impl AuthMaterial {
    pub(crate) fn new(
        app_key: SecretString,
        app_secret: SecretString,
        token: Option<AccessToken>,
        refresh_token: Option<SecretString>,
        refresher: Option<Arc<dyn TokenRefresher>>,
        guard: Arc<SecretGuard>,
    ) -> Self {
        Self {
            app_key,
            app_secret,
            token: Mutex::new(token),
            refresh_token,
            refresher,
            guard,
        }
    }

    /// The app key (a public identifier; still wrapped so it never prints).
    pub(crate) fn app_key(&self) -> &SecretString {
        &self.app_key
    }

    /// The current token's expiry, when one was installed.
    pub(crate) fn token_expires_at_ms(&self) -> Option<u64> {
        self.lock().as_ref().map(AccessToken::expires_at_ms)
    }

    /// True when a token is present and valid at `now_ms` (with skew).
    pub(crate) fn has_valid_token(&self, now_ms: u64) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(|token| token.is_valid(now_ms, DEFAULT_TOKEN_SKEW_MS))
    }

    /// The access token for one request at `now_ms`.
    ///
    /// A present, unexpired token is used. Otherwise the injected refresher
    /// is consulted (when one and a refresh token exist); a refresh failure
    /// or an absent/unexpired token is the typed
    /// [`SourceError::AuthenticationRequired`] — never an anonymous request.
    pub(crate) fn ensure_access_token(&self, now_ms: u64) -> Result<SecretString, SourceError> {
        let mut token = self.lock();
        if let Some(current) = token.as_ref() {
            if current.is_valid(now_ms, DEFAULT_TOKEN_SKEW_MS) {
                return Ok(current.value().clone());
            }
        }
        let Some(refresher) = self.refresher.as_deref() else {
            return Err(SourceError::AuthenticationRequired);
        };
        let refreshed = refresher
            .refresh(self.refresh_token.as_ref(), now_ms)
            .map_err(|_| SourceError::AuthenticationRequired)?;
        self.guard.register(refreshed.access_token.expose());
        let installed =
            AccessToken::new(refreshed.access_token().clone(), refreshed.expires_at_ms());
        let value = installed.value().clone();
        *token = Some(installed);
        Ok(value)
    }

    /// Build the full signed parameter list for one request: the endpoint
    /// parameters, the common `access_token`/`_aop_timestamp` parameters,
    /// and `_aop_signature` last.
    pub(crate) fn sign_params(
        &self,
        api_params: Vec<(String, String)>,
        now_ms: u64,
    ) -> Result<Vec<(String, String)>, SourceError> {
        let mut params = api_params;
        let access_token = self.ensure_access_token(now_ms)?;
        params.push((
            ACCESS_TOKEN_PARAM.to_string(),
            access_token.expose().to_string(),
        ));
        params.push((TIMESTAMP_PARAM.to_string(), now_ms.to_string()));
        let canonical = canonical_parameter_string(&params);
        let signature = aop_signature_hex(&self.app_secret, &canonical);
        params.push((SIGNATURE_PARAM.to_string(), signature));
        Ok(params)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<AccessToken>> {
        // Poison-tolerant: a panic elsewhere must not turn auth state into a
        // panic or a silent pass.
        self.token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl fmt::Debug for AuthMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthMaterial")
            .field("app_key", &"<redacted>")
            .field("app_secret", &"<redacted>")
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("has_refresher", &self.refresher.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic refresher: one scripted outcome, recorded calls.
    struct ScriptedRefresher {
        outcome: Result<RefreshedToken, SourceError>,
        calls: Mutex<Vec<(u64, bool)>>,
    }

    impl ScriptedRefresher {
        fn returning(token: &str, expires_at_ms: u64) -> Self {
            Self {
                outcome: Ok(RefreshedToken::new(
                    SecretString::new(token.to_string()),
                    expires_at_ms,
                )),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                outcome: Err(SourceError::AuthenticationRequired),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(u64, bool)> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl TokenRefresher for ScriptedRefresher {
        fn refresh(
            &self,
            refresh_token: Option<&SecretString>,
            now_ms: u64,
        ) -> Result<RefreshedToken, SourceError> {
            self.calls
                .lock()
                .expect("calls lock")
                .push((now_ms, refresh_token.is_some()));
            self.outcome.clone()
        }
    }

    fn material(
        token: Option<AccessToken>,
        refresh_token: Option<&str>,
        refresher: Option<Arc<ScriptedRefresher>>,
        guard: Arc<SecretGuard>,
    ) -> AuthMaterial {
        guard.register("aop-test-app-secret");
        AuthMaterial::new(
            SecretString::new("aop-test-app-key".to_string()),
            SecretString::new("aop-test-app-secret".to_string()),
            token,
            refresh_token.map(|value| {
                guard.register(value);
                SecretString::new(value.to_string())
            }),
            refresher.map(|scripted| scripted as Arc<dyn TokenRefresher>),
            guard,
        )
    }

    #[test]
    fn an_expired_token_refreshes_once_and_the_new_token_is_signed_and_registered() {
        let guard = Arc::new(SecretGuard::new());
        guard.register("aop-expired-token");
        let refresher = Arc::new(ScriptedRefresher::returning(
            "aop-refreshed-token",
            1_800_000_000_000,
        ));
        let auth = material(
            Some(AccessToken::new(
                SecretString::new("aop-expired-token".to_string()),
                1_700_000_000_000,
            )),
            Some("aop-refresh-token"),
            Some(refresher.clone()),
            guard.clone(),
        );
        assert!(!auth.has_valid_token(1_700_000_000_001));
        let signed = auth
            .sign_params(vec![], 1_700_000_000_001)
            .expect("refreshed");
        assert!(signed
            .iter()
            .any(|(name, value)| { name == ACCESS_TOKEN_PARAM && value == "aop-refreshed-token" }));
        assert!(auth.has_valid_token(1_700_000_000_001));
        assert_eq!(auth.token_expires_at_ms(), Some(1_800_000_000_000));
        // The refresher saw the configured refresh token exactly once.
        assert_eq!(refresher.calls(), vec![(1_700_000_000_001, true)]);
        // The refreshed secret was registered with the scanner, so it can
        // never surface unredacted.
        assert!(
            guard.scrub("aop-refreshed-token") != "aop-refreshed-token",
            "the refreshed token must be scanner-registered"
        );
        // A second request inside validity must not refresh again.
        let again = auth.sign_params(vec![], 1_700_000_000_002).expect("valid");
        assert!(again
            .iter()
            .any(|(name, value)| name == ACCESS_TOKEN_PARAM && value == "aop-refreshed-token"));
        assert_eq!(refresher.calls().len(), 1);
    }

    #[test]
    fn a_failing_refresh_is_typed_authentication_required() {
        let guard = Arc::new(SecretGuard::new());
        let refresher = Arc::new(ScriptedRefresher::failing());
        let auth = material(
            None,
            Some("aop-refresh-token"),
            Some(refresher.clone()),
            guard,
        );
        assert_eq!(
            auth.sign_params(vec![], 1_700_000_000_000),
            Err(SourceError::AuthenticationRequired)
        );
        assert_eq!(refresher.calls(), vec![(1_700_000_000_000, true)]);
        assert!(!auth.has_valid_token(1_700_000_000_000));
    }

    #[test]
    fn a_token_expiring_inside_the_skew_window_is_refreshed() {
        let guard = Arc::new(SecretGuard::new());
        guard.register("aop-borderline-token");
        let refresher = Arc::new(ScriptedRefresher::returning(
            "aop-refreshed-token",
            1_800_000_000_000,
        ));
        let auth = material(
            Some(AccessToken::new(
                SecretString::new("aop-borderline-token".to_string()),
                1_700_000_060_000,
            )),
            Some("aop-refresh-token"),
            Some(refresher.clone()),
            guard,
        );
        // Exactly at the edge: not valid under the documented skew.
        assert!(!auth.has_valid_token(1_700_000_000_000));
        let signed = auth
            .sign_params(vec![], 1_700_000_000_000)
            .expect("refreshed");
        assert!(signed
            .iter()
            .any(|(name, value)| name == ACCESS_TOKEN_PARAM && value == "aop-refreshed-token"));
    }

    #[test]
    fn signature_is_hmac_sha1_uppercase_over_sorted_raw_parameters() {
        let guard = Arc::new(SecretGuard::new());
        guard.register("aop-test-app-secret");
        guard.register("aop-test-token");
        let auth = material(
            Some(AccessToken::new(
                SecretString::new("aop-test-token".to_string()),
                1_800_000_000_000,
            )),
            None,
            None,
            guard,
        );
        let signed = auth
            .sign_params(
                vec![
                    ("b".to_string(), "2".to_string()),
                    ("a".to_string(), "1".to_string()),
                ],
                1_700_000_000_000,
            )
            .expect("signed");
        let canonical: Vec<(String, String)> = signed
            .iter()
            .filter(|(name, _)| name != SIGNATURE_PARAM)
            .cloned()
            .collect();
        assert_eq!(
            canonical_parameter_string(&canonical),
            "_aop_timestamp1700000000000a1access_tokenaop-test-tokenb2"
        );
        let signature = signed
            .iter()
            .find(|(name, _)| name == SIGNATURE_PARAM)
            .map(|(_, value)| value.clone())
            .expect("signature");
        assert_eq!(signature.len(), 40);
        assert!(signature
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)));
    }

    #[test]
    fn access_token_debug_never_prints_the_value() {
        let token = AccessToken::new(SecretString::new("aop-very-secret".to_string()), 42);
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("aop-very-secret"), "{rendered}");
        assert!(rendered.contains("42"), "{rendered}");
    }
}
