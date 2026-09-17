//! Daemon-side construction of the SSO authority: the network OIDC adapter
//! over the ONE checked transport plus the operator-staged payload contract
//! (a confidential client's secret is loaded from the configured payload
//! directory; a missing, world-readable or corrupt payload is a typed
//! construction refusal, so the daemon never boots half-configured SSO).
//!
//! The adapter is exactly [`faktor_cloud::NetworkOidcAdapter`] (discovery
//! cache, JWKS rotation, RS256/HS256 verification, and BOTH client kinds —
//! public PKCE and confidential `client_secret_post`/`client_secret_basic`).
//! When a client secret is staged, this module loads it per the payload
//! contract and hands it to the adapter's strict configuration: ONE
//! implementation serves both flows, and a confidential method without a
//! usable secret fails closed at construction.

use std::sync::Arc;

use faktor_cloud::{
    AsyncOidcAdapter, ClientSecret, Clock, NetworkOidcAdapter, NetworkOidcConfig, SsoLogin,
    SystemClock,
};

use crate::config::CloudSsoCfg;
use crate::payload::PayloadDir;

/// Build the wired SSO authority. Fails closed: a malformed section, an
/// unreachable-shaped issuer/client id, a missing or unusable client-secret
/// payload, or an inconsistent method/secret pair refuses construction (the
/// caller turns it into a startup error).
pub fn build_sso_authority(
    cfg: &CloudSsoCfg,
    payload_root: &std::path::Path,
    transport: Arc<dyn faktor_provider::egress::HttpTransport>,
) -> Result<Arc<SsoLogin>, String> {
    cfg.validate()?;
    if !cfg.enabled {
        return Err("sso: the section is disabled".into());
    }
    let issuer = cfg.issuer()?;
    let client_id = cfg.client_id()?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut network = NetworkOidcConfig {
        issuer,
        client_id,
        discovery_max_age_ms: cfg
            .discovery_max_age_ms
            .unwrap_or(faktor_cloud::DEFAULT_DISCOVERY_MAX_AGE_MS),
        jwks_max_age_ms: cfg
            .jwks_max_age_ms
            .unwrap_or(faktor_cloud::DEFAULT_JWKS_MAX_AGE_MS),
        max_jwks_refetches: cfg
            .max_jwks_refetches
            .unwrap_or(faktor_cloud::NetworkOidcConfig::default().max_jwks_refetches),
        allowed_algorithms: cfg.allowed_algorithms()?,
        ..Default::default()
    };
    if let Some(name) = cfg.client_secret.as_deref() {
        // The secret is loaded per the operator-staged payload contract and
        // handed to the adapter: it is never logged, never rendered and
        // never carried in an error.
        let secret = PayloadDir::new(payload_root)
            .load_secret(name)
            .map_err(|e| format!("sso: {e}"))?;
        let method = cfg.client_auth_method()?;
        network.client_auth = method;
        network.client_secret =
            Some(ClientSecret::new(secret).map_err(|e| format!("sso: client secret: {e}"))?);
    }
    let adapter = NetworkOidcAdapter::new(transport, clock.clone(), network)
        .map_err(|e| format!("sso: {e}"))?;
    let adapter: Arc<dyn AsyncOidcAdapter> = Arc::new(adapter);
    Ok(Arc::new(SsoLogin::new(adapter, clock)))
}

#[cfg(test)]
#[path = "sso_auth_tests.rs"]
mod sso_auth_tests;
