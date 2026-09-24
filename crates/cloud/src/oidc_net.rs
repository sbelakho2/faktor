//! The NETWORK OIDC adapter: the [`OidcAdapter`] contract over the checked
//! egress transport.
//!
//! [`NetworkOidcAdapter`] implements [`AsyncOidcAdapter`] — the asynchronous
//! sibling of the in-process [`crate::oidc::OidcAdapter`] seam (every sync
//! adapter, including [`crate::oidc::FakeOidcAdapter`], is usable through it
//! via a blanket forwarding impl) — over the ONE checked transport
//! ([`faktor_provider::egress::HttpTransport`]): discovery, authorization-code
//! exchange, ID-token verification with JWKS rotation and claim -> membership
//! mapping. No `reqwest` type is named here; every request executes through
//! [`faktor_provider::egress::execute_raw`], so the transport's request-time
//! destination gate applies to every call.
//!
//! ONE implementation serves both client kinds. The [`ClientAuthMethod`] of
//! [`NetworkOidcConfig`] selects the token-endpoint authentication:
//!
//! - `None` (the default): the public PKCE client — the exchange sends
//!   exactly `grant_type, code, redirect_uri, code_verifier, client_id` and
//!   nothing else, byte-identical to the pre-confidential shape;
//! - `client_secret_post`: the configured [`ClientSecret`] rides the form;
//! - `client_secret_basic`: the secret rides an RFC 6749 §2.3.1 HTTP Basic
//!   header (`base64(urlencode(client_id):urlencode(secret))`) and neither
//!   the secret nor the client id appears in the form.
//!
//! The secret is validated at construction, redacted in `Debug`/`Display`
//! and never interpolated into an error: a missing secret for a confidential
//! method is a typed configuration refusal (fail closed), never a silently
//! unauthenticated exchange.
//!
//! Caching and bounds (all documented):
//!
//! - **discovery** is cached for the response's `Cache-Control: max-age`
//!   (seconds), clamped to the configured ceiling; a missing header uses the
//!   configured default. A hit never refetches;
//! - **JWKS** is cached the same way. A token whose `kid` is not in the fresh
//!   cache triggers at most [`NetworkOidcConfig::max_jwks_refetches`] FORCED
//!   refetches per verification (bounded rotation response); a kid that stays
//!   unknown after the bound is the typed [`OidcError::UnknownKey`];
//! - response bodies are bounded by the transport's own
//!   [`faktor_provider::egress::MAX_RAW_RESPONSE_BYTES`];
//! - the accepted signing algorithm is the strict INTERSECTION of three
//!   independently authoritative sets: the discovery document's
//!   `id_token_signing_alg_values_supported`, the selected JWK's own
//!   `alg`/`kty`/`use`/`key_ops` constraints, and the deployment-configured
//!   [`NetworkOidcConfig::allowed_algorithms`]. The header `alg` must equal
//!   the algorithm actually used to verify; a mismatch is the typed
//!   [`OidcError::AlgorithmRefused`] naming each set. `none` is never
//!   accepted, and symmetric `HS*` is accepted only when the operator lists
//!   it explicitly AND the JWK is an `oct` key with `use = "sig"` (the
//!   verifiable implementations are `HS256` and `RS256`);
//! - `aud`/`azp` follow OpenID Connect: a multi-valued `aud` requires
//!   `azp == client_id`, and an `azp` that is present must always match —
//!   a client id merely occurring somewhere in a multi-valued `aud` is
//!   never sufficient ([`OidcError::WrongAzp`]);
//! - `exp`/`iat` arithmetic is `i128`-exact over the skew window and every
//!   millisecond conversion is checked: overflow is a typed
//!   [`OidcError::TimestampOutOfRange`], never a wrap or a panic.

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde::Deserialize;

use faktor_provider::egress::{
    execute_raw, HttpTransport, RawRequest, RawResponse, ResponseBudget, RouteLabel,
    MAX_RAW_RESPONSE_BYTES,
};
use faktor_security::secret::SecretValue;

use crate::oidc::{
    constant_time_eq, hmac_sha256, map_membership_claims, ClaimMapping, CodeExchangeRequest,
    IdTokenExpectations, OidcAdapter, OidcClaims, OidcDiscovery, OidcError, OidcMembership,
    OidcNonce, OidcTokenSet,
};

/// Default discovery cache TTL when the provider sends no `Cache-Control`.
pub const DEFAULT_DISCOVERY_MAX_AGE_MS: i64 = 300_000;
/// Default JWKS cache TTL when the provider sends no `Cache-Control`.
pub const DEFAULT_JWKS_MAX_AGE_MS: i64 = 300_000;
/// Hard cap on the configured per-verification JWKS refetches.
pub const MAX_JWKS_REFETCHES: u32 = 3;
/// Hard cap on the configured discovery/JWKS max-age ceiling.
pub const MAX_CACHE_MAX_AGE_MS: i64 = 3_600_000;
/// Hard cap on one configured client secret (mirrors the staged-payload
/// bound: a secret is not a document).
pub const MAX_CLIENT_SECRET_BYTES: usize = 4096;
/// Hard cap on one authorization-code exchange input field.
pub const MAX_CODE_EXCHANGE_INPUT_BYTES: usize = 1024 * 1024;
/// The signing algorithms this adapter can verify at all. `none` is not an
/// algorithm and is never verifiable; symmetric `HS*` is verifiable here
/// only under the explicit conditions of
/// [`NetworkOidcConfig::allowed_algorithms`].
pub const SUPPORTED_ALGORITHMS: &[&str] = &["HS256", "RS256"];
/// The default deployment policy: asymmetric signatures only.
pub const DEFAULT_ALLOWED_ALGORITHMS: &[&str] = &["RS256"];
/// Hard cap on the configured allowed-algorithm list.
pub const MAX_ALLOWED_ALGORITHMS: usize = 8;

/// Documented wall-clock bound for ONE OIDC network round trip (discovery,
/// JWKS, or token exchange). The shared egress client only bounds connect,
/// so an issuer that accepts and then stalls would otherwise pin the
/// adapter — and every verification queued behind it — forever.
pub const OIDC_NETWORK_TIMEOUT_MS: u64 = 30_000;

/// Execute one raw OIDC request under [`OIDC_NETWORK_TIMEOUT_MS`]. The
/// returned `String` is the call-attributable failure message; callers wrap
/// it in their typed error variant. A breach names the bound explicitly.
async fn execute_raw_bounded(
    transport: &dyn HttpTransport,
    request: RawRequest,
    what: &str,
) -> Result<RawResponse, String> {
    // Every OIDC read passes an explicit response budget: head/idle/total
    // all equal the documented network bound, and the body is capped by the
    // seam's materialization bound.
    let budget = ResponseBudget::for_timeout(
        std::time::Duration::from_millis(OIDC_NETWORK_TIMEOUT_MS),
        MAX_RAW_RESPONSE_BYTES as u64,
    );
    match tokio::time::timeout(
        std::time::Duration::from_millis(OIDC_NETWORK_TIMEOUT_MS),
        execute_raw(transport, request, &budget),
    )
    .await
    {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(e)) => Err(format!("{what}: {e}")),
        Err(_) => Err(format!(
            "{what} exceeded the {OIDC_NETWORK_TIMEOUT_MS} ms network bound"
        )),
    }
}

/// How the adapter authenticates itself at the token endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClientAuthMethod {
    /// Public client: PKCE only (the pre-existing exchange shape).
    #[default]
    None,
    /// `client_secret_post`: the secret rides the token request form.
    ClientSecretPost,
    /// `client_secret_basic`: the secret rides an HTTP Basic header.
    ClientSecretBasic,
}

impl ClientAuthMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            ClientAuthMethod::None => "none",
            ClientAuthMethod::ClientSecretPost => "client_secret_post",
            ClientAuthMethod::ClientSecretBasic => "client_secret_basic",
        }
    }

    /// Strict parse of the configured method name (exact match only).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "none" => Some(ClientAuthMethod::None),
            "client_secret_post" => Some(ClientAuthMethod::ClientSecretPost),
            "client_secret_basic" => Some(ClientAuthMethod::ClientSecretBasic),
            _ => None,
        }
    }

    /// Whether the method requires a configured secret.
    pub const fn requires_secret(self) -> bool {
        !matches!(self, ClientAuthMethod::None)
    }
}

/// A confidential client's secret. The value is bounded printable ASCII, and
/// its `Debug`/`Display` are REDACTED — the only reach for the bytes is the
/// token-endpoint request the configured method builds, and no error message
/// ever carries it.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientSecret(String);

impl ClientSecret {
    /// Validate + wrap one secret. A malformed secret is a typed refusal
    /// that never echoes the value.
    pub fn new(secret: impl Into<String>) -> Result<Self, OidcError> {
        let secret = secret.into();
        if secret.is_empty() || secret.len() > MAX_CLIENT_SECRET_BYTES {
            return Err(OidcError::DiscoveryUnavailable(format!(
                "client secret must be 1..={MAX_CLIENT_SECRET_BYTES} bytes"
            )));
        }
        if !secret.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(OidcError::DiscoveryUnavailable(
                "client secret must be printable ASCII without whitespace".into(),
            ));
        }
        Ok(Self(secret))
    }

    /// The secret bytes (the exchange is the ONLY caller).
    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ClientSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientSecret(<redacted>)")
    }
}

impl std::fmt::Display for ClientSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The strict network-adapter configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkOidcConfig {
    /// The ONE issuer this adapter serves; discovery of any other issuer is
    /// refused (a document can never be swapped in).
    pub issuer: String,
    pub client_id: String,
    /// Ceiling for the honored `Cache-Control: max-age` of discovery (and the
    /// default when the header is absent).
    pub discovery_max_age_ms: i64,
    /// Ceiling for the honored `Cache-Control: max-age` of the JWKS.
    pub jwks_max_age_ms: i64,
    /// Forced JWKS refetches allowed per verification when the `kid` is
    /// unknown (bounded rotation response).
    pub max_jwks_refetches: u32,
    /// How the adapter authenticates at the token endpoint (`none` = public
    /// PKCE client, the pre-existing shape).
    pub client_auth: ClientAuthMethod,
    /// The confidential client's secret; required iff `client_auth` is not
    /// `none`, refused when configured without a method.
    pub client_secret: Option<ClientSecret>,
    /// The algorithms this deployment explicitly allows. The JWT header
    /// `alg` must be in the intersection of this list, the discovery
    /// document's `id_token_signing_alg_values_supported` and the signing
    /// JWK's own `alg`/`kty`/`use`/`key_ops` constraints, and must equal the
    /// algorithm actually used for verification. Defaults to `["RS256"]`:
    /// symmetric `HS*` is refused unless listed here explicitly AND the JWK
    /// is an `oct` key with `use = "sig"`. `none` is never accepted.
    pub allowed_algorithms: Vec<String>,
}

impl Default for NetworkOidcConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            client_id: String::new(),
            discovery_max_age_ms: DEFAULT_DISCOVERY_MAX_AGE_MS,
            jwks_max_age_ms: DEFAULT_JWKS_MAX_AGE_MS,
            max_jwks_refetches: 2,
            client_auth: ClientAuthMethod::None,
            client_secret: None,
            allowed_algorithms: DEFAULT_ALLOWED_ALGORITHMS
                .iter()
                .map(|alg| (*alg).to_string())
                .collect(),
        }
    }
}

impl NetworkOidcConfig {
    /// Strict validation; every bound is enforced. A confidential method
    /// without a secret (or a secret without a method) is refused BEFORE any
    /// request could be built: the adapter never falls back to an
    /// unauthenticated exchange.
    pub fn validate(&self) -> Result<(), OidcError> {
        if !(self.issuer.starts_with("https://") || self.issuer.starts_with("http://")) {
            return Err(OidcError::DiscoveryUnavailable(
                "issuer must be an http(s) URL".into(),
            ));
        }
        if self.issuer.ends_with('/') {
            return Err(OidcError::DiscoveryUnavailable(
                "issuer must not carry a trailing slash".into(),
            ));
        }
        if self.client_id.trim().is_empty() || self.client_id.len() > 256 {
            return Err(OidcError::DiscoveryUnavailable(
                "client_id must be 1..=256 bytes".into(),
            ));
        }
        for (field, value) in [
            ("discovery_max_age_ms", self.discovery_max_age_ms),
            ("jwks_max_age_ms", self.jwks_max_age_ms),
        ] {
            if value <= 0 || value > MAX_CACHE_MAX_AGE_MS {
                return Err(OidcError::DiscoveryUnavailable(format!(
                    "{field} must be 1..={MAX_CACHE_MAX_AGE_MS}"
                )));
            }
        }
        if self.max_jwks_refetches > MAX_JWKS_REFETCHES {
            return Err(OidcError::DiscoveryUnavailable(format!(
                "max_jwks_refetches must be <= {MAX_JWKS_REFETCHES}"
            )));
        }
        match (self.client_auth, &self.client_secret) {
            (ClientAuthMethod::None, Some(_)) => {
                return Err(OidcError::DiscoveryUnavailable(
                    "a client secret is configured but client_auth is \"none\"; select \
                     client_secret_post or client_secret_basic"
                        .into(),
                ));
            }
            (method, None) if method.requires_secret() => {
                return Err(OidcError::DiscoveryUnavailable(format!(
                    "client auth method {:?} requires a client secret (none configured)",
                    method.as_str()
                )));
            }
            _ => {}
        }
        Self::validate_allowed_algorithms(&self.allowed_algorithms)
    }

    /// The strict algorithm policy validation: a bounded, non-empty list of
    /// algorithms this adapter can actually verify; `none` is refused with
    /// its own name (it can never be permitted).
    pub fn validate_allowed_algorithms(algorithms: &[String]) -> Result<(), OidcError> {
        if algorithms.is_empty() || algorithms.len() > MAX_ALLOWED_ALGORITHMS {
            return Err(OidcError::DiscoveryUnavailable(format!(
                "allowed_algorithms must carry 1..={MAX_ALLOWED_ALGORITHMS} entries"
            )));
        }
        for algorithm in algorithms {
            if algorithm == "none" {
                return Err(OidcError::DiscoveryUnavailable(
                    "\"none\" is never an accepted id-token signing algorithm".into(),
                ));
            }
            if !SUPPORTED_ALGORITHMS.contains(&algorithm.as_str()) {
                return Err(OidcError::DiscoveryUnavailable(format!(
                    "allowed_algorithms entry {algorithm:?} is not supported by this adapter \
                     (supported: {})",
                    SUPPORTED_ALGORITHMS.join(", ")
                )));
            }
        }
        Ok(())
    }
}

/// The asynchronous OIDC adapter contract the control plane consumes: the
/// exact [`OidcAdapter`] operations, awaited over the checked transport. A
/// blanket impl forwards every in-process (sync) adapter.
#[async_trait::async_trait]
pub trait AsyncOidcAdapter: Send + Sync {
    async fn discovery(&self, issuer: &str) -> Result<OidcDiscovery, OidcError>;
    async fn exchange_code(&self, request: &CodeExchangeRequest)
        -> Result<OidcTokenSet, OidcError>;
    async fn verify_id_token(
        &self,
        id_token: &str,
        expected: &IdTokenExpectations,
    ) -> Result<OidcClaims, OidcError>;
    fn map_membership(
        &self,
        claims: &OidcClaims,
        mapping: &ClaimMapping,
    ) -> Result<OidcMembership, OidcError>;
}

#[async_trait::async_trait]
impl<T: OidcAdapter + Send + Sync> AsyncOidcAdapter for T {
    async fn discovery(&self, issuer: &str) -> Result<OidcDiscovery, OidcError> {
        OidcAdapter::discovery(self, issuer)
    }

    async fn exchange_code(
        &self,
        request: &CodeExchangeRequest,
    ) -> Result<OidcTokenSet, OidcError> {
        OidcAdapter::exchange_code(self, request)
    }

    async fn verify_id_token(
        &self,
        id_token: &str,
        expected: &IdTokenExpectations,
    ) -> Result<OidcClaims, OidcError> {
        OidcAdapter::verify_id_token(self, id_token, expected)
    }

    fn map_membership(
        &self,
        claims: &OidcClaims,
        mapping: &ClaimMapping,
    ) -> Result<OidcMembership, OidcError> {
        OidcAdapter::map_membership(self, claims, mapping)
    }
}

/// One JWKS key as parsed from the provider document (metadata + the public
/// or shared-secret material the verification needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedJwksView {
    pub kid: String,
    pub kty: String,
    pub alg: Option<String>,
}

#[derive(Debug, Clone)]
struct JwkKey {
    kid: String,
    kty: String,
    alg: Option<String>,
    /// The optional JWK `use` (`sig`/`enc`); only `sig` may verify.
    use_: Option<String>,
    /// The optional JWK `key_ops`; when present it must contain `verify`.
    key_ops: Option<Vec<String>>,
    /// `oct` shared secret (base64url).
    k: Option<Vec<u8>>,
    /// `RSA` modulus/exponent (base64url).
    n: Option<Vec<u8>>,
    e: Option<Vec<u8>>,
}

#[derive(Clone)]
struct CachedDiscovery {
    doc: OidcDiscovery,
    fetched_ms: i64,
    max_age_ms: i64,
}

struct CachedJwks {
    keys: Vec<JwkKey>,
    fetched_ms: i64,
    max_age_ms: i64,
}

/// The network adapter over one checked transport + clock.
pub struct NetworkOidcAdapter {
    transport: Arc<dyn HttpTransport>,
    clock: Arc<dyn crate::service::Clock>,
    config: NetworkOidcConfig,
    discovery_cache: Mutex<Option<CachedDiscovery>>,
    jwks_cache: Mutex<Option<CachedJwks>>,
}

impl std::fmt::Debug for NetworkOidcAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkOidcAdapter")
            .field("issuer", &self.config.issuer)
            .field("client_id", &self.config.client_id)
            .field("client_auth", &self.config.client_auth.as_str())
            .field("authenticated", &self.config.client_secret.is_some())
            .field(
                "jwks_cached",
                &self.jwks_cache.lock().map(|c| c.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl NetworkOidcAdapter {
    /// Build + validate the adapter (no network call happens here).
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        clock: Arc<dyn crate::service::Clock>,
        config: NetworkOidcConfig,
    ) -> Result<Self, OidcError> {
        config.validate()?;
        Ok(Self {
            transport,
            clock,
            config,
            discovery_cache: Mutex::new(None),
            jwks_cache: Mutex::new(None),
        })
    }

    /// The current JWKS metadata (never key material); fetched when the cache
    /// is empty or stale.
    pub async fn jwks_view(&self) -> Result<Vec<CachedJwksView>, OidcError> {
        let keys = self.jwks_keys(false).await?;
        Ok(keys
            .iter()
            .map(|key| CachedJwksView {
                kid: key.kid.clone(),
                kty: key.kty.clone(),
                alg: key.alg.clone(),
            })
            .collect())
    }

    fn lock_discovery(&self) -> std::sync::MutexGuard<'_, Option<CachedDiscovery>> {
        self.discovery_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_jwks(&self) -> std::sync::MutexGuard<'_, Option<CachedJwks>> {
        self.jwks_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn fetch_discovery(&self) -> Result<CachedDiscovery, OidcError> {
        let url = format!("{}/.well-known/openid-configuration", self.config.issuer);
        let response = execute_raw_bounded(
            &*self.transport,
            RawRequest::new("GET", url).route(RouteLabel::OidcDiscovery),
            "discovery",
        )
        .await
        .map_err(OidcError::DiscoveryUnavailable)?;
        if !(200..300).contains(&response.status) {
            return Err(OidcError::DiscoveryUnavailable(format!(
                "discovery endpoint answered {}",
                response.status
            )));
        }
        let raw: RawDiscovery = serde_json::from_slice(&response.body)
            .map_err(|e| OidcError::DiscoveryUnavailable(format!("discovery json: {e}")))?;
        if raw.issuer != self.config.issuer {
            return Err(OidcError::DiscoveryUnavailable(format!(
                "discovery document names issuer {:?}, expected {:?}",
                raw.issuer, self.config.issuer
            )));
        }
        for endpoint in [
            &raw.authorization_endpoint,
            &raw.token_endpoint,
            &raw.jwks_uri,
        ] {
            if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
                return Err(OidcError::DiscoveryUnavailable(format!(
                    "discovery endpoint {endpoint:?} is not an http(s) URL"
                )));
            }
        }
        let now = self.clock.now_ms();
        let max_age = response
            .header("cache-control")
            .and_then(parse_cache_control_max_age)
            .map(|secs| secs.saturating_mul(1000).max(1))
            .unwrap_or(self.config.discovery_max_age_ms)
            .min(self.config.discovery_max_age_ms);
        let cached = CachedDiscovery {
            doc: OidcDiscovery {
                issuer: raw.issuer,
                authorization_endpoint: raw.authorization_endpoint,
                token_endpoint: raw.token_endpoint,
                jwks_uri: raw.jwks_uri,
                supported_algorithms: raw.id_token_signing_alg_values_supported,
            },
            fetched_ms: now,
            max_age_ms: max_age,
        };
        *self.lock_discovery() = Some(cached.clone());
        Ok(cached)
    }

    async fn fetch_jwks(&self) -> Result<Vec<JwkKey>, OidcError> {
        let doc = self.discovery(&self.config.issuer).await?;
        let response = execute_raw_bounded(
            &*self.transport,
            RawRequest::new("GET", doc.jwks_uri).route(RouteLabel::OidcJwks),
            "jwks",
        )
        .await
        .map_err(OidcError::DiscoveryUnavailable)?;
        if !(200..300).contains(&response.status) {
            return Err(OidcError::DiscoveryUnavailable(format!(
                "jwks endpoint answered {}",
                response.status
            )));
        }
        let document: RawJwks = serde_json::from_slice(&response.body)
            .map_err(|e| OidcError::DiscoveryUnavailable(format!("jwks json: {e}")))?;
        if document.keys.len() > 64 {
            return Err(OidcError::DiscoveryUnavailable(
                "jwks document carries more than 64 keys".into(),
            ));
        }
        let now = self.clock.now_ms();
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut keys = Vec::with_capacity(document.keys.len());
        for raw in document.keys {
            let decode =
                |value: Option<String>, what: &str| -> Result<Option<Vec<u8>>, OidcError> {
                    match value {
                        None => Ok(None),
                        Some(value) => engine.decode(value.as_bytes()).map(Some).map_err(|e| {
                            OidcError::DiscoveryUnavailable(format!(
                                "jwks {what} is not base64url: {e}"
                            ))
                        }),
                    }
                };
            keys.push(JwkKey {
                kid: raw.kid,
                kty: raw.kty,
                alg: raw.alg,
                use_: raw.use_,
                key_ops: raw.key_ops,
                k: decode(raw.k, "k")?,
                n: decode(raw.n, "n")?,
                e: decode(raw.e, "e")?,
            });
        }
        let max_age = response
            .header("cache-control")
            .and_then(parse_cache_control_max_age)
            .map(|secs| secs.saturating_mul(1000).max(1))
            .unwrap_or(self.config.jwks_max_age_ms)
            .min(self.config.jwks_max_age_ms);
        *self.lock_jwks() = Some(CachedJwks {
            keys: keys.clone(),
            fetched_ms: now,
            max_age_ms: max_age,
        });
        Ok(keys)
    }

    /// Resolve one signing key by `kid`: a fresh cache hit wins, otherwise
    /// the JWKS is (re)fetched. A `kid` missing from the fresh cache is
    /// answered with at most `max_jwks_refetches` forced refetches.
    async fn jwks_keys(&self, force: bool) -> Result<Vec<JwkKey>, OidcError> {
        let now = self.clock.now_ms();
        if !force {
            let cache = self.lock_jwks();
            if let Some(cached) = &*cache {
                if now.saturating_sub(cached.fetched_ms) < cached.max_age_ms {
                    return Ok(cached.keys.clone());
                }
            }
        }
        self.fetch_jwks().await
    }

    async fn signing_key(&self, kid: &str) -> Result<JwkKey, OidcError> {
        let now = self.clock.now_ms();
        {
            let cache = self.lock_jwks();
            if let Some(cached) = &*cache {
                if now.saturating_sub(cached.fetched_ms) < cached.max_age_ms {
                    if let Some(key) = cached.keys.iter().find(|key| key.kid == kid) {
                        return Ok(key.clone());
                    }
                }
            }
        }
        let mut refetches: u32 = 0;
        loop {
            let keys = self.fetch_jwks().await?;
            if let Some(key) = keys.iter().find(|key| key.kid == kid) {
                return Ok(key.clone());
            }
            refetches += 1;
            if refetches > self.config.max_jwks_refetches {
                return Err(OidcError::UnknownKey(kid.to_string()));
            }
        }
    }
}

/// The algorithms ONE JWK's own metadata permits (among the algorithms this
/// adapter supports): the key type + material decide the family, an explicit
/// `alg` must equal that candidate, `use` must be absent or `sig` (an `oct`
/// key must declare `use = "sig"` explicitly — a bare shared secret is never
/// implicitly trusted) and `key_ops`, when present, must contain `verify`.
fn jwk_permitted_algorithms(key: &JwkKey) -> Vec<String> {
    let candidate = match key.kty.as_str() {
        "RSA" if key.n.is_some() && key.e.is_some() => "RS256",
        "oct" if key.k.is_some() => "HS256",
        _ => return Vec::new(),
    };
    if key.alg.as_deref().is_some_and(|alg| alg != candidate) {
        return Vec::new();
    }
    let use_ok = match key.use_.as_deref() {
        Some("sig") => true,
        Some(_) => false,
        None => candidate != "HS256",
    };
    let ops_ok = key
        .key_ops
        .as_ref()
        .is_none_or(|ops| ops.iter().any(|op| op == "verify"));
    if use_ok && ops_ok {
        vec![candidate.to_string()]
    } else {
        Vec::new()
    }
}

/// The strict algorithm policy: the header `alg` is accepted iff it is
/// contained in ALL THREE authoritative sets — the discovery document's
/// advertised algorithms, the selected JWK's own metadata permits, and the
/// deployment's configured `allowed_algorithms` — and is one of the
/// algorithms this adapter actually implements. Any mismatch is the typed
/// [`OidcError::AlgorithmRefused`] naming every set.
fn check_algorithm_policy(
    alg: &str,
    discovery: &OidcDiscovery,
    key: &JwkKey,
    configured: &[String],
) -> Result<(), OidcError> {
    let jwk = jwk_permitted_algorithms(key);
    let accepted = SUPPORTED_ALGORITHMS.contains(&alg)
        && discovery
            .supported_algorithms
            .iter()
            .any(|advertised| advertised == alg)
        && configured.iter().any(|allowed| allowed == alg)
        && jwk.iter().any(|allowed| allowed == alg);
    if accepted {
        return Ok(());
    }
    Err(OidcError::AlgorithmRefused {
        alg: alg.to_string(),
        discovery: discovery.supported_algorithms.clone(),
        jwk,
        configured: configured.to_vec(),
    })
}

#[async_trait::async_trait]
impl AsyncOidcAdapter for NetworkOidcAdapter {
    async fn discovery(&self, issuer: &str) -> Result<OidcDiscovery, OidcError> {
        if issuer != self.config.issuer {
            return Err(OidcError::DiscoveryUnavailable(format!(
                "issuer {issuer:?} is not served by this adapter"
            )));
        }
        let now = self.clock.now_ms();
        {
            let cache = self.lock_discovery();
            if let Some(cached) = &*cache {
                if now.saturating_sub(cached.fetched_ms) < cached.max_age_ms {
                    return Ok(cached.doc.clone());
                }
            }
        }
        Ok(self.fetch_discovery().await?.doc)
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
        if request.code.len() > MAX_CODE_EXCHANGE_INPUT_BYTES
            || request.redirect_uri.len() > MAX_CODE_EXCHANGE_INPUT_BYTES
            || request.code_verifier.len() > MAX_CODE_EXCHANGE_INPUT_BYTES
        {
            return Err(OidcError::CodeExchangeRefused(
                "authorization-code exchange inputs are oversized".into(),
            ));
        }
        let doc = self.discovery(&self.config.issuer).await?;
        // The secret is resolved ONCE per exchange; a confidential method
        // without one can only exist if validate() was bypassed, and refuses
        // here typed instead of ever sending an unauthenticated request.
        let secret = match self.config.client_auth {
            ClientAuthMethod::None => None,
            _ => Some(
                self.config
                    .client_secret
                    .as_ref()
                    .ok_or_else(|| {
                        OidcError::CodeExchangeRefused(format!(
                            "client auth method {:?} requires a client secret (none configured)",
                            self.config.client_auth.as_str()
                        ))
                    })?
                    .expose(),
            ),
        };
        // Public PKCE shape first (unchanged byte-for-byte); the confidential
        // methods APPEND their credential per the configured method.
        let mut body = String::new();
        let mut field = |name: &str, value: &str| {
            if !body.is_empty() {
                body.push('&');
            }
            body.push_str(name);
            body.push('=');
            body.push_str(&urlencode(value));
        };
        field("grant_type", "authorization_code");
        field("code", request.code.expose());
        field("redirect_uri", request.redirect_uri.as_str());
        field("code_verifier", request.code_verifier.expose());
        match self.config.client_auth {
            // RFC 6749 §2.3.1: the client_id rides the Basic header, never
            // the form, when the client authenticates with HTTP Basic.
            ClientAuthMethod::ClientSecretBasic => {}
            ClientAuthMethod::None | ClientAuthMethod::ClientSecretPost => {
                field("client_id", self.config.client_id.as_str());
            }
        }
        if self.config.client_auth == ClientAuthMethod::ClientSecretPost {
            field("client_secret", secret.unwrap_or_default());
        }
        let mut raw_request = RawRequest::new("POST", doc.token_endpoint)
            .route(RouteLabel::OidcTokenExchange)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .bytes_body(body.into_bytes());
        if self.config.client_auth == ClientAuthMethod::ClientSecretBasic {
            let credentials = format!(
                "{}:{}",
                urlencode(self.config.client_id.as_str()),
                urlencode(secret.unwrap_or_default())
            );
            let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
            raw_request = raw_request.header("authorization", format!("Basic {encoded}"));
        }
        let raw = execute_raw_bounded(&*self.transport, raw_request, "token exchange")
            .await
            .map_err(OidcError::CodeExchangeRefused)?;
        if !(200..300).contains(&raw.status) {
            return Err(OidcError::CodeExchangeRefused(format!(
                "token endpoint answered {}",
                raw.status
            )));
        }
        let token: RawTokenResponse = serde_json::from_slice(&raw.body)
            .map_err(|e| OidcError::CodeExchangeRefused(format!("token json: {e}")))?;
        // The wire DTO's secret-bearing fields are consumed EXACTLY here and
        // wrapped immediately; no `String` token ever enters the domain.
        token_set_from_raw(token)
    }

    async fn verify_id_token(
        &self,
        id_token: &str,
        expected: &IdTokenExpectations,
    ) -> Result<OidcClaims, OidcError> {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut segments = id_token.split('.');
        let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(OidcError::Malformed(
                "a JWT must have exactly three dot-separated segments".into(),
            ));
        };
        let header: serde_json::Value = serde_json::from_slice(
            &engine
                .decode(header_b64)
                .map_err(|e| OidcError::Malformed(format!("header base64: {e}")))?,
        )
        .map_err(|e| OidcError::Malformed(format!("header json: {e}")))?;
        let kid = header
            .get("kid")
            .and_then(|value| value.as_str())
            .ok_or_else(|| OidcError::Malformed("header carries no kid".into()))?;
        let alg = header
            .get("alg")
            .and_then(|value| value.as_str())
            .ok_or_else(|| OidcError::Malformed("header carries no alg".into()))?;
        let signature = engine
            .decode(signature_b64)
            .map_err(|e| OidcError::Malformed(format!("signature base64: {e}")))?;
        let signing_input = format!("{header_b64}.{payload_b64}");
        // The algorithm policy is decided BEFORE any key material is used to
        // verify: the header `alg` must be in the intersection of the
        // discovery set, the JWK's own constraints and the configured
        // allowed algorithms, and the dispatch below is an exact match (no
        // case folding, no alias). `none`/unknown algorithms never reach a
        // verifier.
        let discovery = self.discovery(&self.config.issuer).await?;
        let key = self.signing_key(kid).await?;
        check_algorithm_policy(alg, &discovery, &key, &self.config.allowed_algorithms)?;
        match alg {
            "HS256" => {
                let secret = key.k.as_deref().ok_or_else(|| {
                    OidcError::Malformed(format!("kid {kid:?} carries no oct secret for HS256"))
                })?;
                let expected_signature = hmac_sha256(secret, signing_input.as_bytes());
                if !constant_time_eq(&signature, &expected_signature) {
                    return Err(OidcError::BadSignature);
                }
            }
            "RS256" => {
                let (n, e) = match (&key.n, &key.e) {
                    (Some(n), Some(e)) => (n.clone(), e.clone()),
                    _ => {
                        return Err(OidcError::Malformed(format!(
                            "kid {kid:?} carries no RSA n/e components for RS256"
                        )));
                    }
                };
                let components = ring::signature::RsaPublicKeyComponents {
                    n: n.as_slice(),
                    e: e.as_slice(),
                };
                components
                    .verify(
                        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                        signing_input.as_bytes(),
                        &signature,
                    )
                    .map_err(|_| OidcError::BadSignature)?;
            }
            other => {
                // Unreachable by construction (the policy above only lets
                // SUPPORTED_ALGORITHMS through); kept as a defensive typed
                // refusal, never a silent acceptance.
                return Err(OidcError::AlgorithmRefused {
                    alg: other.to_string(),
                    discovery: discovery.supported_algorithms,
                    jwk: jwk_permitted_algorithms(&key),
                    configured: self.config.allowed_algorithms.clone(),
                });
            }
        }
        let payload: serde_json::Value = serde_json::from_slice(
            &engine
                .decode(payload_b64)
                .map_err(|e| OidcError::Malformed(format!("payload base64: {e}")))?,
        )
        .map_err(|e| OidcError::Malformed(format!("payload json: {e}")))?;
        let text = |field: &str| -> Result<String, OidcError> {
            payload
                .get(field)
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    OidcError::Malformed(format!("claim {field:?} is missing or not text"))
                })
        };
        let number = |field: &str| -> Result<i64, OidcError> {
            payload
                .get(field)
                .and_then(|value| value.as_i64())
                .ok_or_else(|| {
                    OidcError::Malformed(format!("claim {field:?} is missing or not a number"))
                })
        };
        let issuer = text("iss")?;
        if issuer != expected.issuer {
            return Err(OidcError::WrongIssuer {
                expected: expected.issuer.clone(),
                actual: issuer,
            });
        }
        let audience = match payload.get("aud") {
            Some(serde_json::Value::String(single)) => vec![single.clone()],
            Some(serde_json::Value::Array(list)) => {
                if list.is_empty() {
                    return Err(OidcError::Malformed("claim \"aud\" is empty".into()));
                }
                let mut values = Vec::with_capacity(list.len());
                for value in list {
                    let Some(text) = value.as_str() else {
                        return Err(OidcError::Malformed(
                            "claim \"aud\" carries a non-text entry".into(),
                        ));
                    };
                    values.push(text.to_string());
                }
                values
            }
            _ => {
                return Err(OidcError::Malformed(
                    "claim \"aud\" is missing or malformed".into(),
                ));
            }
        };
        if !audience.iter().any(|aud| aud == &expected.audience) {
            return Err(OidcError::WrongAudience {
                expected: expected.audience.clone(),
            });
        }
        // OIDC azp rules: a multi-valued `aud` REQUIRES `azp` to equal the
        // client id (client_id merely occurring somewhere in `aud` is never
        // sufficient), and an `azp` that is present must match regardless of
        // the `aud` shape.
        let azp = match payload.get("azp") {
            None => None,
            Some(serde_json::Value::String(value)) => Some(value.clone()),
            Some(_) => return Err(OidcError::Malformed("claim \"azp\" is not text".into())),
        };
        match &azp {
            Some(value) if value != &expected.audience => {
                return Err(OidcError::WrongAzp {
                    expected: expected.audience.clone(),
                    actual: azp.clone(),
                });
            }
            None if audience.len() > 1 => {
                return Err(OidcError::WrongAzp {
                    expected: expected.audience.clone(),
                    actual: None,
                });
            }
            _ => {}
        }
        // Time arithmetic is i128-exact over the skew window; the second ->
        // millisecond conversion is checked, so i64::MAX/MIN claims are
        // typed refusals rather than wraps, saturations or panics.
        let expires_at_s = number("exp")?;
        let expires_at_ms =
            expires_at_s
                .checked_mul(1000)
                .ok_or_else(|| OidcError::TimestampOutOfRange {
                    claim: "exp".into(),
                    value: expires_at_s,
                })?;
        let issued_at_s = number("iat")?;
        let issued_at_ms =
            issued_at_s
                .checked_mul(1000)
                .ok_or_else(|| OidcError::TimestampOutOfRange {
                    claim: "iat".into(),
                    value: issued_at_s,
                })?;
        let skew = i128::from(expected.clock_skew_ms.max(0));
        let now_ms = i128::from(expected.now_ms);
        if i128::from(expires_at_ms) + skew < now_ms {
            return Err(OidcError::Expired);
        }
        if i128::from(issued_at_ms) - skew > now_ms {
            return Err(OidcError::NotYetValid);
        }
        let nonce = payload
            .get("nonce")
            .and_then(|value| value.as_str())
            .map(OidcNonce::new);
        if let Some(expected_nonce) = &expected.nonce {
            if nonce.as_ref() != Some(expected_nonce) {
                return Err(OidcError::NonceMismatch);
            }
        }
        Ok(OidcClaims {
            issuer,
            subject: text("sub")?,
            audience,
            email: payload
                .get("email")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            email_verified: payload
                .get("email_verified")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            issued_at_ms,
            expires_at_ms,
            nonce,
            groups: payload
                .get("groups")
                .and_then(|value| value.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    fn map_membership(
        &self,
        claims: &OidcClaims,
        mapping: &ClaimMapping,
    ) -> Result<OidcMembership, OidcError> {
        map_membership_claims(claims, mapping)
    }
}

/// Lenient provider payloads (extra provider fields are ignored; the strict
/// shape is enforced on the fields the adapter consumes).
#[derive(Debug, Deserialize)]
struct RawDiscovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    id_token_signing_alg_values_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawJwks {
    keys: Vec<RawJwk>,
}

#[derive(Debug, Deserialize)]
struct RawJwk {
    kid: String,
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default, rename = "use")]
    use_: Option<String>,
    #[serde(default)]
    key_ops: Option<Vec<String>>,
    #[serde(default)]
    k: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

/// The OIDC token-endpoint WIRE response (a provider payload). Its
/// token-bearing fields exist only for the immediate conversion into
/// [`OidcTokenSet`] (wrapped in [`SecretValue`] before any domain use); the
/// strict shape of the consumed fields is enforced at conversion. `Debug` is
/// manual and redacts both token fields so even a stray `{:?}` of the
/// short-lived capture cannot print a bearer token.
// SECRET-FIELD-GATE-WIRE-DTO: provider token-endpoint response parsed and converted to OidcTokenSet in the same expression block, never stored as plaintext.
#[derive(Deserialize)]
struct RawTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    id_token: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    expires_in: Option<i64>,
}

impl std::fmt::Debug for RawTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawTokenResponse")
            .field("access_token", &"[redacted]")
            .field("id_token", &"[redacted]")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Convert one parsed OIDC token-endpoint WIRE response into the domain
/// [`OidcTokenSet`]: the plaintext tokens are wrapped in [`SecretValue`] in
/// this one expression block and the wire `String`s are dropped. This is the
/// ONLY place the wire DTO's secret-bearing fields are read.
fn token_set_from_raw(token: RawTokenResponse) -> Result<OidcTokenSet, OidcError> {
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
        access_token: SecretValue::new(token.access_token.unwrap_or_default()),
        id_token: SecretValue::new(token.id_token),
        token_type: token.token_type,
        expires_in_s: token.expires_in.unwrap_or(0),
    })
}

/// Parse one `Cache-Control` header's `max-age` (seconds). Pure; garbage or
/// a missing directive yields `None` (the configured default applies).
pub fn parse_cache_control_max_age(header: &str) -> Option<i64> {
    for directive in header.split(',') {
        let directive = directive.trim();
        let Some(value) = directive
            .strip_prefix("max-age=")
            .or_else(|| directive.strip_prefix("max-age ="))
        else {
            continue;
        };
        if let Ok(seconds) = value.trim().parse::<i64>() {
            return Some(seconds);
        }
    }
    None
}

pub(crate) fn urlencode(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_dto_accepts_a_real_provider_payload_and_converts_immediately() {
        // A real token-endpoint payload shape: extra provider fields are
        // ignored, the consumed fields convert into `OidcTokenSet` with both
        // bearer tokens wrapped.
        let body = serde_json::json!({
            "access_token": "PLANTED-ACCESS-TOKEN-0123456789",
            "id_token": "PLANTED-ID-TOKEN-0123456789",
            "token_type": "Bearer",
            "expires_in": 3599,
            "scope": "openid email profile",
            "refresh_token": "ignored-by-the-strict-shape",
        })
        .to_string();
        let raw: RawTokenResponse = serde_json::from_str(&body).expect("a real payload parses");
        let tokens = token_set_from_raw(raw).expect("the payload converts");
        assert_eq!(
            tokens.access_token.expose(),
            "PLANTED-ACCESS-TOKEN-0123456789"
        );
        assert_eq!(tokens.id_token.expose(), "PLANTED-ID-TOKEN-0123456789");
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_in_s, 3599);
        // The converted carrier never renders the planted token.
        let rendered = format!("{tokens:?}");
        assert!(!rendered.contains("PLANTED-ACCESS-TOKEN-0123456789"));
        assert!(!rendered.contains("PLANTED-ID-TOKEN-0123456789"));
        // The short-lived wire capture itself is redacted too: even a stray
        // `{:?}` of the DTO cannot print the bearer tokens.
        let capture: RawTokenResponse = serde_json::from_str(&body).unwrap();
        let rendered = format!("{capture:?}");
        assert!(!rendered.contains("PLANTED-ACCESS-TOKEN-0123456789"));
        assert!(!rendered.contains("PLANTED-ID-TOKEN-0123456789"));
        assert!(rendered.contains("[redacted]"));

        // The documented defaults: no access_token and no expires_in.
        let raw: RawTokenResponse =
            serde_json::from_str(r#"{"id_token":"jwt","token_type":"Bearer"}"#).unwrap();
        let tokens = token_set_from_raw(raw).unwrap();
        assert!(tokens.access_token.is_empty());
        assert_eq!(tokens.expires_in_s, 0);
    }

    #[test]
    fn wire_dto_conversion_refuses_incomplete_payloads() {
        // A payload without the required id_token never even parses into the
        // wire DTO (the only required member), so it can never convert.
        assert!(serde_json::from_str::<RawTokenResponse>(
            r#"{"access_token":"a","token_type":"Bearer"}"#
        )
        .is_err());
        // An empty id_token parses but is refused at conversion.
        let empty_id: RawTokenResponse =
            serde_json::from_str(r#"{"id_token":"","token_type":"Bearer"}"#).unwrap();
        assert!(matches!(
            token_set_from_raw(empty_id),
            Err(OidcError::CodeExchangeRefused(_))
        ));
        // A missing token_type stays a typed conversion refusal.
        let missing_type: RawTokenResponse =
            serde_json::from_str(r#"{"access_token":"a","id_token":"jwt"}"#).unwrap();
        let err = token_set_from_raw(missing_type).unwrap_err();
        let rendered = format!("{err} {err:?}");
        assert!(!rendered.contains("access_token"));
        // A payload that is not an object at all is a parse refusal, never a
        // silent default.
        assert!(serde_json::from_str::<RawTokenResponse>("[]").is_err());
    }
}
