//! Alibaba.com Open API authentication and request signing.
//!
//! This module owns the Alibaba-family credential state for
//! [`AlibabaConnector`](super::AlibabaConnector): the app key, the app
//! secret, the access token with its absolute expiry, the optional refresh
//! token, the timestamp and the canonical-parameter signature. Request
//! construction lives in [`super::api`]; both use the shared AOP
//! common-parameter scheme documented in [`crate::aop`].
//!
//! # Contract status
//!
//! The Alibaba.com buyer Open API endpoint contract could not be certified
//! live from this tree, so this family is **contract-mocked**: it implements
//! the documented Alibaba Open Platform common-parameter signing exactly
//! (sorted raw `name + value` parameters, HMAC-SHA1 under the app secret,
//! uppercase hex, `_aop_signature`, millisecond `_aop_timestamp`) against
//! the declared endpoints, and replays recorded-response fixtures offline.
//! It is not live-certified and makes no claim beyond the documented scheme.
//!
//! # Secret discipline
//!
//! * the app secret is never a parameter, header or URL component — it is
//!   only the HMAC key;
//! * the access token is sent in the request body (never the URL) and is
//!   registered with the secret scanner when resolved or refreshed;
//! * the app key is a public identifier in the URL path, declared as the
//!   request's URL credential so the outbound scanner can distinguish it
//!   from a leaked secret.

use faktor_commerce::SourceError;

use crate::aop::AuthMaterial;
use crate::secrets::{SecretGuard, SecretString};

pub use crate::aop::{AccessToken, RefreshedToken, TokenRefresher};

/// The signing scheme implemented by this family (documented AOP `param2`).
pub const SIGNING_SCHEME: &str = "aop-param2-hmac-sha1-uppercase";
/// The certification status of this family's endpoint contract.
pub const CONTRACT_STATUS: &str = "contract-mocked";
/// This family does not require a nonce; `_aop_timestamp` bounds freshness.
pub const NONCE_REQUIRED: bool = false;

/// The Alibaba.com Open API auth state.
pub(crate) struct AlibabaAuth {
    material: AuthMaterial,
}

impl std::fmt::Debug for AlibabaAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlibabaAuth")
            .field("material", &self.material)
            .finish()
    }
}

impl AlibabaAuth {
    pub(crate) fn new(
        app_key: SecretString,
        app_secret: SecretString,
        access_token: Option<AccessToken>,
        refresh_token: Option<SecretString>,
        refresher: Option<std::sync::Arc<dyn TokenRefresher>>,
        guard: std::sync::Arc<SecretGuard>,
    ) -> Self {
        Self {
            material: AuthMaterial::new(
                app_key,
                app_secret,
                access_token,
                refresh_token,
                refresher,
                guard,
            ),
        }
    }

    /// The public app key (still wrapped: it never prints).
    pub(crate) fn app_key(&self) -> &SecretString {
        self.material.app_key()
    }

    /// True when a valid, unexpired token is installed at `now_ms`.
    pub(crate) fn has_valid_token(&self, now_ms: u64) -> bool {
        self.material.has_valid_token(now_ms)
    }

    /// The installed token's expiry, when one exists.
    pub(crate) fn token_expires_at_ms(&self) -> Option<u64> {
        self.material.token_expires_at_ms()
    }

    /// The signed form parameters for one request: the endpoint parameters
    /// plus `access_token`, `_aop_timestamp` and `_aop_signature`.
    ///
    /// Fails [`SourceError::AuthenticationRequired`] when no valid unexpired
    /// token exists and no refresh yields one.
    pub(crate) fn sign_params(
        &self,
        api_params: Vec<(String, String)>,
        now_ms: u64,
    ) -> Result<Vec<(String, String)>, SourceError> {
        self.material.sign_params(api_params, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture(relative: &str) -> String {
        crate::testing::fixture(relative).expect("fixture")
    }

    fn material(app_key: &str, app_secret: &str, token: Option<(&str, u64)>) -> AlibabaAuth {
        let guard = Arc::new(SecretGuard::new());
        guard.register(app_key);
        guard.register(app_secret);
        let access_token = token.map(|(value, expires_at_ms)| {
            guard.register(value);
            AccessToken::new(SecretString::new(value.to_string()), expires_at_ms)
        });
        AlibabaAuth::new(
            SecretString::new(app_key.to_string()),
            SecretString::new(app_secret.to_string()),
            access_token,
            None,
            None,
            guard,
        )
    }

    /// The golden vectors were generated independently with Python
    /// `hmac`/`hashlib` from the documented AOP scheme (see the fixture's
    /// `note`), so this pins the Rust implementation against a foreign
    /// implementation rather than itself.
    #[test]
    fn golden_signature_vectors_match() {
        let golden: serde_json::Value =
            serde_json::from_str(&fixture("alibaba/signing_golden.json")).expect("golden json");
        assert_eq!(golden["scheme"], SIGNING_SCHEME);
        assert_eq!(golden["contract"], CONTRACT_STATUS);
        let timestamp_ms = golden["timestamp_ms"].as_u64().expect("timestamp");
        let token = golden["access_token"].as_str().expect("token").to_string();
        let auth = material(
            golden["app_key"].as_str().expect("key"),
            golden["app_secret"].as_str().expect("secret"),
            Some((&token, timestamp_ms + 3_600_000)),
        );
        for vector in golden["vectors"].as_array().expect("vectors") {
            let api_params: Vec<(String, String)> = vector["api_params"]
                .as_array()
                .expect("params")
                .iter()
                .map(|pair| {
                    (
                        pair[0].as_str().expect("name").to_string(),
                        pair[1].as_str().expect("value").to_string(),
                    )
                })
                .collect();
            let signed = auth.sign_params(api_params, timestamp_ms).expect("signed");
            let canonical = canonical_from(&signed);
            assert_eq!(
                canonical,
                vector["canonical"].as_str().expect("canonical"),
                "golden canonical for {}",
                vector["name"]
            );
            assert_eq!(
                signature_of(&signed),
                vector["signature"].as_str().expect("signature"),
                "golden signature for {}",
                vector["name"]
            );
            assert_eq!(
                signed
                    .iter()
                    .find(|(name, _)| name == crate::aop::ACCESS_TOKEN_PARAM)
                    .map(|(_, value)| value.as_str()),
                Some(token.as_str())
            );
        }
    }

    fn canonical_from(signed: &[(String, String)]) -> String {
        let without_signature: Vec<(String, String)> = signed
            .iter()
            .filter(|(name, _)| name != crate::aop::SIGNATURE_PARAM)
            .cloned()
            .collect();
        crate::aop::canonical_parameter_string(&without_signature)
    }

    #[test]
    fn a_missing_token_is_typed_authentication_required() {
        let auth = material(
            "alibaba-sanitized-key",
            "alibaba-sanitized-app-secret",
            None,
        );
        assert_eq!(
            auth.sign_params(vec![], 1_700_000_000_000),
            Err(SourceError::AuthenticationRequired)
        );
        assert!(!auth.has_valid_token(1_700_000_000_000));
        assert_eq!(auth.token_expires_at_ms(), None);
    }

    #[test]
    fn an_expired_or_nearly_expired_token_is_refused_without_a_refresher() {
        let auth = material(
            "alibaba-sanitized-key",
            "alibaba-sanitized-app-secret",
            Some(("stale-token", 1_700_000_000_000)),
        );
        // Past expiry.
        assert_eq!(
            auth.sign_params(vec![], 1_700_000_000_001),
            Err(SourceError::AuthenticationRequired)
        );
        // Inside the skew window: already stale.
        assert_eq!(
            auth.sign_params(vec![], 1_699_999_950_000),
            Err(SourceError::AuthenticationRequired)
        );
        // Comfortably before expiry: usable.
        assert!(auth
            .sign_params(vec![], 1_699_999_000_000)
            .expect("valid token")
            .iter()
            .any(|(name, _)| name == crate::aop::SIGNATURE_PARAM));
    }

    #[test]
    fn tampering_with_any_bound_parameter_changes_the_signature() {
        let auth = material(
            "alibaba-sanitized-key",
            "alibaba-sanitized-app-secret",
            Some(("token", 1_700_003_600_000)),
        );
        let base = auth
            .sign_params(
                vec![("productId".to_string(), "1600123456789".to_string())],
                1_700_000_000_000,
            )
            .expect("signed");
        let base_signature = signature_of(&base);
        for tampered in [
            vec![("productId".to_string(), "1600123456790".to_string())],
            vec![("productId".to_string(), "1600123456789".to_string()); 2],
        ] {
            let other = auth
                .sign_params(tampered, 1_700_000_000_000)
                .expect("signed");
            assert_ne!(signature_of(&other), base_signature);
        }
        let later = auth
            .sign_params(
                vec![("productId".to_string(), "1600123456789".to_string())],
                1_700_000_000_001,
            )
            .expect("signed");
        assert_ne!(signature_of(&later), base_signature);
    }

    fn signature_of(signed: &[(String, String)]) -> String {
        signed
            .iter()
            .find(|(name, _)| name == crate::aop::SIGNATURE_PARAM)
            .map(|(_, value)| value.clone())
            .expect("signature")
    }

    #[test]
    fn auth_debug_never_prints_secret_material() {
        let auth = material(
            "alibaba-sanitized-key",
            "alibaba-sanitized-app-secret",
            Some(("alibaba-sanitized-token", 1_700_003_600_000)),
        );
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("alibaba-sanitized-secret"), "{rendered}");
        assert!(!rendered.contains("alibaba-sanitized-token"), "{rendered}");
    }
}
