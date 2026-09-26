//! 1688 Open Platform authentication and request signing.
//!
//! This module owns the 1688 credential state for
//! [`China1688Connector`](super::China1688Connector): the app key, the app
//! secret, the access token with its absolute expiry, the optional refresh
//! token, the millisecond timestamp and the canonical-parameter signature.
//! It also builds the signed `param2` requests; both use the shared AOP
//! common-parameter scheme documented in [`crate::aop`].
//!
//! # Contract status
//!
//! The signing scheme is the documented 1688 Open Platform `param2`
//! signature (sorted raw `name + value` parameters, HMAC-SHA1 under the app
//! secret, uppercase hex, `_aop_signature`, `_aop_timestamp`). The live
//! endpoint surface cannot be certified from this tree, so the family is
//! **contract-mocked**: requests are exercised against recorded-response
//! fixtures only, and no claim is made beyond the documented scheme.
//!
//! The app key follows the documented `param2` URL shape
//! (`.../{namespace}/{api}/{appKey}`) and is a public identifier; the app
//! secret is only the HMAC key; the access token is body-only and never
//! URL-visible.

use faktor_commerce::SourceError;

use crate::aop::AuthMaterial;
use crate::http::{self, HttpRequest};
use crate::secrets::{SecretGuard, SecretString};

pub use crate::aop::{AccessToken, RefreshedToken, TokenRefresher};

use super::{OPEN_PLATFORM_PRODUCT_URL, OPEN_PLATFORM_SEARCH_URL};

/// The signing scheme implemented by this family (documented `param2`).
pub const SIGNING_SCHEME: &str = "aop-param2-hmac-sha1-uppercase";
/// The certification status of this family's endpoint contract.
pub const CONTRACT_STATUS: &str = "contract-mocked";
/// This family does not require a nonce; `_aop_timestamp` bounds freshness.
pub const NONCE_REQUIRED: bool = false;

/// The 1688 Open Platform auth state.
pub(crate) struct China1688Auth {
    material: AuthMaterial,
}

impl China1688Auth {
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

/// The product-detail request (`offerId`).
pub(crate) fn product_request(
    auth: &China1688Auth,
    offer_id: &str,
    now_ms: u64,
) -> Result<HttpRequest, SourceError> {
    signed_form_request(auth, OPEN_PLATFORM_PRODUCT_URL, "offerId", offer_id, now_ms)
}

/// The keyword-search request (`keywords`).
pub(crate) fn search_request(
    auth: &China1688Auth,
    query: &str,
    now_ms: u64,
) -> Result<HttpRequest, SourceError> {
    signed_form_request(auth, OPEN_PLATFORM_SEARCH_URL, "keywords", query, now_ms)
}

/// Build the signed `param2` form request for one endpoint parameter.
fn signed_form_request(
    auth: &China1688Auth,
    endpoint: &str,
    param_name: &str,
    param_value: &str,
    now_ms: u64,
) -> Result<HttpRequest, SourceError> {
    let url = format!("{endpoint}/{}", http::query_escape(auth.app_key().expose()));
    let params = auth.sign_params(
        vec![(param_name.to_string(), param_value.to_string())],
        now_ms,
    )?;
    let request = HttpRequest::post_form(&url, form_body(&params).as_bytes())?
        .with_url_credential(auth.app_key());
    Ok(request)
}

/// Percent-encode one form body deterministically.
fn form_body(params: &[(String, String)]) -> String {
    let mut out = String::new();
    for (index, (name, value)) in params.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(&http::query_escape(name));
        out.push('=');
        out.push_str(&http::query_escape(value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture(relative: &str) -> String {
        crate::testing::fixture(relative).expect("fixture")
    }

    fn material(app_key: &str, app_secret: &str, token: Option<(&str, u64)>) -> China1688Auth {
        let guard = Arc::new(SecretGuard::new());
        guard.register(app_key);
        guard.register(app_secret);
        let access_token = token.map(|(value, expires_at_ms)| {
            guard.register(value);
            AccessToken::new(SecretString::new(value.to_string()), expires_at_ms)
        });
        China1688Auth::new(
            SecretString::new(app_key.to_string()),
            SecretString::new(app_secret.to_string()),
            access_token,
            None,
            None,
            guard,
        )
    }

    fn signature_of(signed: &[(String, String)]) -> String {
        signed
            .iter()
            .find(|(name, _)| name == crate::aop::SIGNATURE_PARAM)
            .map(|(_, value)| value.clone())
            .expect("signature")
    }

    /// Golden vectors generated independently with Python `hmac`/`hashlib`
    /// (see the fixture note): the canonical string and the uppercase-hex
    /// signature are pinned against a foreign implementation.
    #[test]
    fn golden_signature_vectors_match() {
        let golden: serde_json::Value =
            serde_json::from_str(&fixture("china1688/signing_golden.json")).expect("golden json");
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
            let without_signature: Vec<(String, String)> = signed
                .iter()
                .filter(|(name, _)| name != crate::aop::SIGNATURE_PARAM)
                .cloned()
                .collect();
            assert_eq!(
                crate::aop::canonical_parameter_string(&without_signature),
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
        }
    }

    #[test]
    fn the_request_is_signed_and_keeps_secrets_out_of_the_url() {
        let auth = material(
            "1688-sanitized-key",
            "1688-sanitized-app-secret",
            Some(("1688-sanitized-access-token", 1_700_003_600_000)),
        );
        let request = product_request(&auth, "678901234567", 1_700_000_000_000).expect("request");
        let url = request.url().as_str();
        let body = String::from_utf8_lossy(request.body().expect("body")).into_owned();
        assert!(url.ends_with("/1688-sanitized-key"), "{url}");
        assert!(!url.contains("access_token"), "{url}");
        assert!(!url.contains("1688-sanitized-app-secret"), "{url}");
        assert!(!body.contains("1688-sanitized-app-secret"), "{body}");
        assert!(body.contains("offerId=678901234567"), "{body}");
        assert!(
            body.contains("access_token=1688-sanitized-access-token"),
            "{body}"
        );
        assert!(body.contains("_aop_timestamp=1700000000000"), "{body}");
        assert!(body.contains("_aop_signature="), "{body}");
    }

    #[test]
    fn no_valid_token_means_typed_authentication_required() {
        let auth = material("1688-sanitized-key", "1688-sanitized-app-secret", None);
        assert_eq!(
            product_request(&auth, "678901234567", 1_700_000_000_000),
            Err(SourceError::AuthenticationRequired)
        );
        assert_eq!(
            search_request(&auth, "usb", 1_700_000_000_000),
            Err(SourceError::AuthenticationRequired)
        );
    }
}
