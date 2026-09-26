//! Alibaba.com Open API request construction.
//!
//! One request is a `POST` of an `application/x-www-form-urlencoded` body:
//! the endpoint parameters plus the signed common parameters
//! (`access_token`, `_aop_timestamp`, `_aop_signature`) produced by
//! [`AlibabaAuth::sign_params`]. The app key follows the documented `param2`
//! URL shape (a public identifier in the path); the app secret is only ever
//! the HMAC key and never becomes a wire value; the access token never
//! enters the URL.
//!
//! Endpoint bodies and parameter names are **contract-mocked**: they follow
//! the declared endpoints and the documented AOP scheme, and are exercised
//! only against recorded-response fixtures.

use faktor_commerce::SourceError;

use crate::http::{self, HttpRequest};

use super::auth::AlibabaAuth;
use super::{OPEN_API_PRODUCT_URL, OPEN_API_SEARCH_URL};

/// The product-detail request (`productId`).
pub(crate) fn product_request(
    auth: &AlibabaAuth,
    product_id: &str,
    now_ms: u64,
) -> Result<HttpRequest, SourceError> {
    signed_form_request(auth, OPEN_API_PRODUCT_URL, "productId", product_id, now_ms)
}

/// The keyword-search request (`keywords`).
pub(crate) fn search_request(
    auth: &AlibabaAuth,
    query: &str,
    now_ms: u64,
) -> Result<HttpRequest, SourceError> {
    signed_form_request(auth, OPEN_API_SEARCH_URL, "keywords", query, now_ms)
}

/// Build the signed form request for one endpoint parameter.
pub(crate) fn signed_form_request(
    auth: &AlibabaAuth,
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
pub(crate) fn form_body(params: &[(String, String)]) -> String {
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
    use crate::alibaba::auth::AccessToken;
    use crate::secrets::{SecretGuard, SecretString};
    use std::sync::Arc;

    fn auth_with_token(expires_at_ms: u64) -> AlibabaAuth {
        let guard = Arc::new(SecretGuard::new());
        for value in [
            "alibaba-sanitized-key",
            "alibaba-sanitized-app-secret",
            "alibaba-sanitized-access-token",
        ] {
            guard.register(value);
        }
        AlibabaAuth::new(
            SecretString::new("alibaba-sanitized-key".to_string()),
            SecretString::new("alibaba-sanitized-app-secret".to_string()),
            Some(AccessToken::new(
                SecretString::new("alibaba-sanitized-access-token".to_string()),
                expires_at_ms,
            )),
            None,
            None,
            guard,
        )
    }

    #[test]
    fn the_app_secret_never_reaches_the_url_or_body() {
        let auth = auth_with_token(1_700_003_600_000);
        let request = product_request(&auth, "1600123456789", 1_700_000_000_000).expect("request");
        let url = request.url().as_str();
        let body = String::from_utf8_lossy(request.body().expect("body")).into_owned();
        assert!(!url.contains("alibaba-sanitized-app-secret"), "{url}");
        assert!(!body.contains("alibaba-sanitized-app-secret"), "{body}");
        // The access token is body-only and never URL-visible.
        assert!(!url.contains("alibaba-sanitized-access-token"), "{url}");
        assert!(
            body.contains("access_token=alibaba-sanitized-access-token"),
            "{body}"
        );
        // The app key follows the documented URL shape (public identifier).
        assert!(url.ends_with("/alibaba-sanitized-key"), "{url}");
    }

    #[test]
    fn an_account_scoped_request_without_a_token_is_typed() {
        let guard = Arc::new(SecretGuard::new());
        guard.register("alibaba-sanitized-key");
        guard.register("alibaba-sanitized-app-secret");
        let auth = AlibabaAuth::new(
            SecretString::new("alibaba-sanitized-key".to_string()),
            SecretString::new("alibaba-sanitized-app-secret".to_string()),
            None,
            None,
            None,
            guard,
        );
        assert_eq!(
            product_request(&auth, "1600123456789", 1_700_000_000_000),
            Err(SourceError::AuthenticationRequired)
        );
    }
}
