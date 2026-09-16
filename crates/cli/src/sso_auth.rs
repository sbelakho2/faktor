//! Daemon-side construction of the SSO authority: the network OIDC adapter
//! over the ONE checked transport plus the operator-staged payload contract
//! (a confidential client's secret is loaded from the configured payload
//! directory; a missing, world-readable or corrupt payload is a typed
//! construction refusal, so the daemon never boots half-configured SSO).
//!
//! The adapter is exactly [`faktor_cloud::NetworkOidcAdapter`] (discovery
//! cache, JWKS rotation, RS256/HS256 verification). When a client secret is
//! staged, the authority wraps it in a confidential-client exchange
//! (`client_secret_post` on the token endpoint) — a public PKCE client
//! (no staged secret) keeps the adapter's own exchange path byte-identical.

use std::sync::Arc;

use faktor_cloud::{
    AsyncOidcAdapter, Clock, CodeExchangeRequest, IdTokenExpectations, NetworkOidcAdapter,
    NetworkOidcConfig, OidcClaims, OidcDiscovery, OidcError, OidcMembership, OidcTokenSet,
    SsoLogin, SystemClock,
};
use faktor_provider::egress::{execute_raw, HttpTransport, RawRequest};

use crate::config::CloudSsoCfg;
use crate::payload::PayloadDir;

/// The maximum accepted token-endpoint response body (mirrors the checked
/// transport's raw bound; kept explicit so the wrapper is self-contained).
const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;

/// Build the wired SSO authority. Fails closed: a malformed section, an
/// unreachable-shaped issuer/client id, or a referenced-but-unusable client
/// secret refuses construction (the caller turns it into a startup error).
pub fn build_sso_authority(
    cfg: &CloudSsoCfg,
    payload_root: &std::path::Path,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<SsoLogin>, String> {
    cfg.validate()?;
    if !cfg.enabled {
        return Err("sso: the section is disabled".into());
    }
    let issuer = cfg.issuer()?;
    let client_id = cfg.client_id()?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let network = NetworkOidcConfig {
        issuer: issuer.clone(),
        client_id: client_id.clone(),
        discovery_max_age_ms: cfg
            .discovery_max_age_ms
            .unwrap_or(faktor_cloud::DEFAULT_DISCOVERY_MAX_AGE_MS),
        jwks_max_age_ms: cfg
            .jwks_max_age_ms
            .unwrap_or(faktor_cloud::DEFAULT_JWKS_MAX_AGE_MS),
        max_jwks_refetches: cfg
            .max_jwks_refetches
            .unwrap_or(faktor_cloud::NetworkOidcConfig::default().max_jwks_refetches),
    };
    let adapter = NetworkOidcAdapter::new(transport.clone(), clock.clone(), network)
        .map_err(|e| format!("sso: {e}"))?;
    let adapter: Arc<dyn AsyncOidcAdapter> = match cfg.client_secret.as_deref() {
        None => Arc::new(adapter),
        Some(name) => {
            let secret = PayloadDir::new(payload_root)
                .load_secret(name)
                .map_err(|e| format!("sso: {e}"))?;
            Arc::new(ConfidentialOidcAdapter::new(
                Arc::new(adapter),
                transport,
                issuer,
                client_id,
                secret,
            ))
        }
    };
    Ok(Arc::new(SsoLogin::new(adapter, clock)))
}

/// The confidential-client wrapper: everything delegates to the network
/// adapter except the authorization-code exchange, which authenticates the
/// client with the operator-staged secret (`client_secret_post`). The
/// secret is never logged and never appears in an error.
pub struct ConfidentialOidcAdapter {
    inner: Arc<NetworkOidcAdapter>,
    transport: Arc<dyn HttpTransport>,
    issuer: String,
    client_id: String,
    client_secret: String,
}

impl std::fmt::Debug for ConfidentialOidcAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfidentialOidcAdapter")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl ConfidentialOidcAdapter {
    pub fn new(
        inner: Arc<NetworkOidcAdapter>,
        transport: Arc<dyn HttpTransport>,
        issuer: String,
        client_id: String,
        client_secret: String,
    ) -> Self {
        Self {
            inner,
            transport,
            issuer,
            client_id,
            client_secret,
        }
    }
}

/// Percent-encode one form value (RFC 3986 unreserved set kept literal).
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte))
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Debug, serde::Deserialize)]
struct RawTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    id_token: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[async_trait::async_trait]
impl AsyncOidcAdapter for ConfidentialOidcAdapter {
    async fn discovery(&self, issuer: &str) -> Result<OidcDiscovery, OidcError> {
        self.inner.discovery(issuer).await
    }

    async fn exchange_code(
        &self,
        request: &CodeExchangeRequest,
    ) -> Result<OidcTokenSet, OidcError> {
        if request.code.is_empty()
            || request.redirect_uri.is_empty()
            || request.code_verifier.is_empty()
        {
            return Err(OidcError::CodeExchangeRefused(
                "code, redirect_uri and code_verifier are required".into(),
            ));
        }
        if request.code.len() > MAX_TOKEN_RESPONSE_BYTES
            || request.redirect_uri.len() > MAX_TOKEN_RESPONSE_BYTES
            || request.code_verifier.len() > MAX_TOKEN_RESPONSE_BYTES
        {
            return Err(OidcError::CodeExchangeRefused(
                "authorization-code exchange inputs are oversized".into(),
            ));
        }
        let discovery = self.inner.discovery(&self.issuer).await?;
        let form = [
            ("grant_type", "authorization_code"),
            ("code", request.code.as_str()),
            ("redirect_uri", request.redirect_uri.as_str()),
            ("code_verifier", request.code_verifier.as_str()),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
        ];
        let mut body = String::new();
        for (index, (name, value)) in form.iter().enumerate() {
            if index > 0 {
                body.push('&');
            }
            body.push_str(name);
            body.push('=');
            body.push_str(&form_encode(value));
        }
        let raw = execute_raw(
            &*self.transport,
            RawRequest::new("POST", discovery.token_endpoint)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("accept", "application/json")
                .bytes_body(body.into_bytes()),
        )
        .await
        .map_err(|e| OidcError::CodeExchangeRefused(e.to_string()))?;
        if !(200..300).contains(&raw.status) {
            return Err(OidcError::CodeExchangeRefused(format!(
                "token endpoint answered {}",
                raw.status
            )));
        }
        if raw.body.len() > MAX_TOKEN_RESPONSE_BYTES {
            return Err(OidcError::CodeExchangeRefused(
                "token response is oversized".into(),
            ));
        }
        let token: RawTokenResponse = serde_json::from_slice(&raw.body)
            .map_err(|e| OidcError::CodeExchangeRefused(format!("token json: {e}")))?;
        if token.id_token.is_empty() {
            return Err(OidcError::CodeExchangeRefused(
                "token response carries no id_token".into(),
            ));
        }
        if token.token_type.is_empty() {
            return Err(OidcError::CodeExchangeRefused(
                "token response carries no token_type".into(),
            ));
        }
        Ok(OidcTokenSet {
            access_token: token.access_token.unwrap_or_default(),
            id_token: token.id_token,
            token_type: token.token_type,
            expires_in_s: token.expires_in.unwrap_or(0),
        })
    }

    async fn verify_id_token(
        &self,
        id_token: &str,
        expected: &IdTokenExpectations,
    ) -> Result<OidcClaims, OidcError> {
        self.inner.verify_id_token(id_token, expected).await
    }

    fn map_membership(
        &self,
        claims: &OidcClaims,
        mapping: &faktor_cloud::ClaimMapping,
    ) -> Result<OidcMembership, OidcError> {
        self.inner.map_membership(claims, mapping)
    }
}

#[cfg(test)]
#[path = "sso_auth_tests.rs"]
mod sso_auth_tests;
