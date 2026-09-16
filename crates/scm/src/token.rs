//! The REAL GitHub App installation-token source: app JWT (RS256 over the
//! app's PKCS#8 private key) exchanged for a short-lived installation token
//! through the injected checked transport.
//!
//! Flow, exactly as GitHub documents it:
//!
//! 1. [`GitHubAppTokenSource::app_jwt`] signs `{"alg":"RS256","typ":"JWT"}`
//!    with claims `iss` = app id, `iat` = now − skew, `exp` = iat + TTL —
//!    the JWT is the APP credential and is cached until near expiry;
//! 2. [`GitHubAppTokenSource::token_for`] POSTs
//!    `{api_base}/app/installations/{id}/access_tokens` with that bearer
//!    through the checked [`HttpTransport`] (the daemon's ONE policy-gated
//!    egress seam), with bounded retries and backoff for 429/5xx/transport
//!    failures and NO retry for 4xx refusals;
//! 3. the granted token (token + `expires_at` + permissions) is cached per
//!    installation with an expiry skew, and a token missing the adapter's
//!    minimal repository permissions is refused BEFORE it is ever cached or
//!    used.
//!
//! Every failure is typed ([`ScmError`]); logs never carry the key or a
//! token (both `Debug` representations are redacted).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use faktor_provider::egress::{execute_raw, EgressError, HttpTransport, RawRequest, RawResponse};
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};

use crate::error::ScmError;
use crate::github::{
    chrono_like_parse_ms, require_permissions, Clock, InstallationToken, InstallationTokenSource,
    TOKEN_EXPIRY_SKEW_MS,
};
use crate::ids::ScmInstallationId;

/// Bound on the private-key PEM document.
pub const MAX_APP_KEY_PEM_BYTES: usize = 64 * 1024;
/// Bound on the mint attempts of ONE installation token (1 = no retry).
pub const MAX_APP_TOKEN_ATTEMPTS: u32 = 5;
/// Default app-JWT lifetime (GitHub's maximum is 10 minutes).
pub const DEFAULT_JWT_TTL_SECS: i64 = 540;
/// Default backdating of `iat` (clock skew allowance).
pub const DEFAULT_JWT_SKEW_SECS: i64 = 60;
/// Hard ceiling on the app-JWT lifetime.
pub const MAX_JWT_TTL_SECS: i64 = 600;
/// Hard ceiling on one retry delay.
pub const MAX_RETRY_DELAY_MS: i64 = 30_000;

/// The strict configuration of the real token source.
#[derive(Clone, PartialEq, Eq)]
pub struct GitHubAppTokenConfig {
    /// The GitHub App id (`iss` of every app JWT).
    pub app_id: u64,
    /// The app's unencrypted PKCS#8 private key PEM (`BEGIN PRIVATE KEY`).
    /// A secret: never logged (`Debug` is redacted).
    pub private_key_pkcs8_pem: String,
    /// REST API base (no trailing slash): `https://api.github.com` in
    /// production, a loopback mock in tests.
    pub api_base: String,
    pub user_agent: String,
    /// Total mint attempts of one token (bounded).
    pub max_attempts: u32,
    /// Base of the deterministic exponential retry backoff.
    pub retry_base_ms: i64,
    /// App-JWT lifetime in seconds.
    pub jwt_ttl_secs: i64,
    /// `iat` backdating in seconds.
    pub jwt_skew_secs: i64,
}

impl Default for GitHubAppTokenConfig {
    fn default() -> Self {
        Self {
            app_id: 0,
            private_key_pkcs8_pem: String::new(),
            api_base: "https://api.github.com".to_string(),
            user_agent: "faktor-scm/0.1".to_string(),
            max_attempts: MAX_APP_TOKEN_ATTEMPTS,
            retry_base_ms: 250,
            jwt_ttl_secs: DEFAULT_JWT_TTL_SECS,
            jwt_skew_secs: DEFAULT_JWT_SKEW_SECS,
        }
    }
}

impl fmt::Debug for GitHubAppTokenConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubAppTokenConfig")
            .field("app_id", &self.app_id)
            .field("private_key_pkcs8_pem", &"<redacted>")
            .field("api_base", &self.api_base)
            .field("user_agent", &self.user_agent)
            .field("max_attempts", &self.max_attempts)
            .field("retry_base_ms", &self.retry_base_ms)
            .field("jwt_ttl_secs", &self.jwt_ttl_secs)
            .field("jwt_skew_secs", &self.jwt_skew_secs)
            .finish()
    }
}

impl GitHubAppTokenConfig {
    /// Strict validation: a missing app id, an oversized/absent key, a
    /// non-http(s) base or out-of-bounds timings are refused at
    /// construction, never at first mint.
    pub fn validate(&self) -> Result<(), ScmError> {
        if self.app_id == 0 {
            return Err(ScmError::Config("app_id must be non-zero".into()));
        }
        if self.private_key_pkcs8_pem.is_empty()
            || self.private_key_pkcs8_pem.len() > MAX_APP_KEY_PEM_BYTES
        {
            return Err(ScmError::Config(format!(
                "private key PEM must be 1..={MAX_APP_KEY_PEM_BYTES} bytes"
            )));
        }
        let base = self.api_base.trim_end_matches('/');
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(ScmError::Config(
                "api_base must be an absolute http(s) URL".into(),
            ));
        }
        if base.contains(char::is_whitespace) || base.contains('@') {
            return Err(ScmError::Config(
                "api_base must not contain whitespace or userinfo".into(),
            ));
        }
        if self.user_agent.is_empty() || self.user_agent.len() > 256 {
            return Err(ScmError::Config("user_agent must be 1..=256 bytes".into()));
        }
        if self.max_attempts == 0 || self.max_attempts > MAX_APP_TOKEN_ATTEMPTS {
            return Err(ScmError::Config(format!(
                "max_attempts must be 1..={MAX_APP_TOKEN_ATTEMPTS}"
            )));
        }
        if self.retry_base_ms < 0 || self.retry_base_ms > MAX_RETRY_DELAY_MS {
            return Err(ScmError::Config(format!(
                "retry_base_ms must be 0..={MAX_RETRY_DELAY_MS}"
            )));
        }
        if self.jwt_ttl_secs <= 0 || self.jwt_ttl_secs > MAX_JWT_TTL_SECS {
            return Err(ScmError::Config(format!(
                "jwt_ttl_secs must be 1..={MAX_JWT_TTL_SECS}"
            )));
        }
        if self.jwt_skew_secs < 0 || self.jwt_skew_secs > 120 {
            return Err(ScmError::Config("jwt_skew_secs must be 0..=120".into()));
        }
        Ok(())
    }
}

/// One cached app JWT with its absolute expiry.
#[derive(Clone)]
struct AppJwt {
    token: String,
    expires_at_ms: i64,
}

impl fmt::Debug for AppJwt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppJwt")
            .field("token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// The production [`InstallationTokenSource`]: RS256 app JWTs exchanged for
/// cached installation tokens through the checked transport.
pub struct GitHubAppTokenSource {
    config: GitHubAppTokenConfig,
    key: Arc<RsaKeyPair>,
    transport: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
    jwt: Mutex<Option<AppJwt>>,
    tokens: Mutex<HashMap<u64, InstallationToken>>,
}

impl fmt::Debug for GitHubAppTokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubAppTokenSource")
            .field("app_id", &self.config.app_id)
            .field("api_base", &self.config.api_base)
            .finish_non_exhaustive()
    }
}

impl GitHubAppTokenSource {
    /// Build the source. The private key is parsed EAGERLY: a malformed key
    /// is a construction-time [`ScmError::Config`], never a first-call
    /// surprise.
    pub fn new(
        config: GitHubAppTokenConfig,
        transport: Arc<dyn HttpTransport>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ScmError> {
        config.validate()?;
        let der = decode_pkcs8_pem(&config.private_key_pkcs8_pem)?;
        let key = RsaKeyPair::from_pkcs8(&der).map_err(|e| {
            ScmError::Config(format!(
                "GitHub App private key is not a valid PKCS#8 RSA key: {e}"
            ))
        })?;
        Ok(Self {
            config: GitHubAppTokenConfig {
                api_base: config.api_base.trim_end_matches('/').to_string(),
                ..config
            },
            key: Arc::new(key),
            transport,
            clock,
            jwt: Mutex::new(None),
            tokens: Mutex::new(HashMap::new()),
        })
    }

    fn jwt_lock(&self) -> Result<std::sync::MutexGuard<'_, Option<AppJwt>>, ScmError> {
        self.jwt
            .lock()
            .map_err(|_| ScmError::Store("app JWT cache lock is poisoned".into()))
    }

    fn token_lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<u64, InstallationToken>>, ScmError> {
        self.tokens
            .lock()
            .map_err(|_| ScmError::Store("installation token cache lock is poisoned".into()))
    }

    /// The app JWT, cached until within the skew window of its expiry.
    fn app_jwt(&self, now_ms: i64) -> Result<AppJwt, ScmError> {
        {
            let cache = self.jwt_lock()?;
            if let Some(jwt) = cache.as_ref() {
                let skew_ms = self.config.jwt_skew_secs.saturating_mul(1000);
                if jwt.expires_at_ms.saturating_sub(skew_ms) > now_ms {
                    return Ok(jwt.clone());
                }
            }
        }
        let iat = (now_ms / 1000).saturating_sub(self.config.jwt_skew_secs);
        let exp = iat.saturating_add(self.config.jwt_ttl_secs);
        let header = r#"{"alg":"RS256","typ":"JWT"}"#;
        let claims = serde_json::json!({
            "iat": iat,
            "exp": exp,
            "iss": self.config.app_id.to_string(),
        });
        let claims = serde_json::to_vec(&claims)
            .map_err(|e| ScmError::Config(format!("app JWT claims: {e}")))?;
        let signing_input = format!("{}.{}", b64url(header.as_bytes()), b64url(&claims));
        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| ScmError::Config("app JWT signing failed".into()))?;
        let jwt = AppJwt {
            token: format!("{signing_input}.{}", b64url(&signature)),
            expires_at_ms: exp.saturating_mul(1000),
        };
        *self.jwt_lock()? = Some(jwt.clone());
        Ok(jwt)
    }

    fn installation_url(&self, installation: ScmInstallationId) -> String {
        format!(
            "{}/app/installations/{}/access_tokens",
            self.config.api_base,
            installation.raw()
        )
    }

    /// Deterministic exponential backoff, capped; a provider `retry-after`
    /// wins (bounded).
    fn retry_delay_ms(&self, attempt: u32, retry_after_ms: Option<i64>) -> i64 {
        if let Some(retry_after) = retry_after_ms {
            return retry_after.clamp(0, MAX_RETRY_DELAY_MS);
        }
        let shift = attempt.min(6);
        self.config
            .retry_base_ms
            .saturating_mul(1i64 << shift)
            .clamp(0, MAX_RETRY_DELAY_MS)
    }

    /// Mint one installation token through the checked transport with
    /// bounded retries. 4xx (except 429) is final; 429/5xx/transport are
    /// retried within the attempt bound, then surfaced typed.
    async fn mint(
        &self,
        installation: ScmInstallationId,
        jwt: &str,
    ) -> Result<InstallationToken, ScmError> {
        let url = self.installation_url(installation);
        let mut attempt = 0u32;
        loop {
            let request = RawRequest::new("POST", url.clone())
                .header("accept", "application/vnd.github+json")
                .header("x-github-api-version", "2022-11-28")
                .header("user-agent", self.config.user_agent.clone())
                .header("authorization", format!("Bearer {jwt}"));
            let last_attempt = attempt + 1 >= self.config.max_attempts;
            let (error, retry_after_ms): (ScmError, Option<i64>) =
                match execute_raw(self.transport.as_ref(), request).await {
                    Ok(response) => match self.classify(response) {
                        Ok(token) => return Ok(token),
                        Err(retry) => (retry.error, retry.retry_after_ms),
                    },
                    // A policy denial is final: retrying cannot change the
                    // destination decision.
                    Err(EgressError::Denied { url, .. }) => {
                        return Err(ScmError::Forbidden(format!(
                            "egress to {url} denied by policy"
                        )))
                    }
                    Err(e) => (ScmError::Transport(e.to_string()), None),
                };
            if !mint_retryable(&error) || last_attempt {
                return Err(error);
            }
            let delay = self.retry_delay_ms(attempt, retry_after_ms);
            attempt += 1;
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
            }
        }
    }

    /// One mint response → a usable token, or the typed refusal with its
    /// retry metadata.
    fn classify(&self, response: RawResponse) -> Result<InstallationToken, MintRetry> {
        let status = response.status;
        let excerpt = bounded_excerpt(&response.body_text());
        match status {
            200..=299 => self.parse_token(&response).map_err(MintRetry::final_),
            401 => Err(MintRetry::final_(ScmError::Unauthorized(
                "installation token mint rejected the app JWT (401)".into(),
            ))),
            403 => Err(MintRetry::final_(ScmError::Forbidden(format!(
                "installation token mint refused (403: {excerpt})"
            )))),
            404 => Err(MintRetry::final_(ScmError::NotFound(
                "installation not found (404)".into(),
            ))),
            429 => {
                let retry_after_ms = response
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .map(|secs| secs.max(0).saturating_mul(1000));
                Err(MintRetry {
                    error: ScmError::RateLimited {
                        retry_after_ms: retry_after_ms.unwrap_or(0),
                    },
                    retry_after_ms,
                })
            }
            500..=599 => Err(MintRetry {
                error: ScmError::Api {
                    status,
                    detail: format!("installation token mint failed ({status}): {excerpt}"),
                },
                retry_after_ms: None,
            }),
            other => Err(MintRetry::final_(ScmError::Api {
                status: other,
                detail: format!("installation token mint refused ({other}): {excerpt}"),
            })),
        }
    }

    fn parse_token(&self, response: &RawResponse) -> Result<InstallationToken, ScmError> {
        let json: serde_json::Value =
            serde_json::from_slice(&response.body).map_err(|e| ScmError::Api {
                status: response.status,
                detail: format!("installation token mint response is unparseable: {e}"),
            })?;
        let token = json
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ScmError::Api {
                status: response.status,
                detail: "installation token mint response has no token".into(),
            })?;
        let expires_at_ms = json
            .get("expires_at")
            .and_then(|v| v.as_str())
            .and_then(chrono_like_parse_ms)
            .ok_or_else(|| ScmError::Api {
                status: response.status,
                detail: "installation token mint response has no parseable expires_at".into(),
            })?;
        if expires_at_ms <= self.clock.now_ms() {
            return Err(ScmError::Unauthorized(
                "installation token mint returned an already-expired token".into(),
            ));
        }
        let permissions: Vec<(String, String)> = json
            .get("permissions")
            .and_then(|v| v.as_object())
            .map(|permissions| {
                permissions
                    .iter()
                    .filter_map(|(name, level)| {
                        level
                            .as_str()
                            .map(|level| (name.clone(), level.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let token = InstallationToken::new(token, expires_at_ms, permissions)?;
        // Fail closed: a token missing the adapter's minimal repository
        // permissions is refused before it is cached or used.
        require_permissions(&token)?;
        Ok(token)
    }
}

#[async_trait]
impl InstallationTokenSource for GitHubAppTokenSource {
    /// Resolve (and cache) one installation token. A cached token is reused
    /// while it is more than [`TOKEN_EXPIRY_SKEW_MS`] from expiry; an
    /// expired/near-expiry token is refreshed by a fresh mint.
    async fn token_for(
        &self,
        installation: ScmInstallationId,
    ) -> Result<InstallationToken, ScmError> {
        let now = self.clock.now_ms();
        {
            let cache = self.token_lock()?;
            if let Some(token) = cache.get(&installation.raw()) {
                if token.expires_at_ms() - TOKEN_EXPIRY_SKEW_MS > now {
                    return Ok(token.clone());
                }
            }
        }
        let jwt = self.app_jwt(now)?;
        let token = self.mint(installation, &jwt.token).await?;
        self.token_lock()?.insert(installation.raw(), token.clone());
        Ok(token)
    }

    /// The app credential: the signed app JWT (no repository permissions —
    /// it authenticates the APP, not an installation).
    async fn app_token(&self) -> Result<InstallationToken, ScmError> {
        let jwt = self.app_jwt(self.clock.now_ms())?;
        InstallationToken::new(jwt.token, jwt.expires_at_ms, Vec::new())
    }
}

/// The one retryable mint failure plus its provider-directed delay.
struct MintRetry {
    error: ScmError,
    retry_after_ms: Option<i64>,
}

impl MintRetry {
    fn final_(error: ScmError) -> Self {
        Self {
            error,
            retry_after_ms: None,
        }
    }
}

/// Whether one mint failure may be retried within the attempt bound: a
/// provider 5xx and an infrastructure transport failure are transient; a
/// rate limit is retried with its provider-directed delay; every other
/// refusal (4xx and local configuration/store failures) is final.
fn mint_retryable(error: &ScmError) -> bool {
    match error {
        ScmError::RateLimited { .. } | ScmError::Transport(_) => true,
        ScmError::Api { status, .. } => (500..=599).contains(status),
        _ => false,
    }
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode an unencrypted PKCS#8 (`BEGIN PRIVATE KEY`) PEM document. A
/// `BEGIN RSA PRIVATE KEY` document (PKCS#1) is refused with a typed
/// configuration error: converting it is the operator's job
/// (`openssl pkcs8 -topk8`), never a silent in-process guess.
fn decode_pkcs8_pem(pem: &str) -> Result<Vec<u8>, ScmError> {
    let mut started = false;
    let mut body = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            if line.contains("RSA PRIVATE KEY") {
                return Err(ScmError::Config(
                    "private key is PKCS#1 (`BEGIN RSA PRIVATE KEY`); convert it to PKCS#8 (`BEGIN PRIVATE KEY`) first".into(),
                ));
            }
            if !line.contains("BEGIN PRIVATE KEY") {
                return Err(ScmError::Config(
                    "private key PEM must be an unencrypted PKCS#8 document (`BEGIN PRIVATE KEY`)"
                        .into(),
                ));
            }
            started = true;
            continue;
        }
        if line.starts_with("-----END") {
            break;
        }
        if started {
            body.push_str(line);
        }
    }
    if body.is_empty() {
        return Err(ScmError::Config(
            "private key PEM carries no PKCS#8 body".into(),
        ));
    }
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| ScmError::Config(format!("private key PEM base64: {e}")))
}

fn bounded_excerpt(text: &str) -> String {
    const MAX: usize = 200;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::ManualClock;
    use faktor_provider::egress::MockHttpTransport;

    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDOdaRv7SIbH6qK
-----END PRIVATE KEY-----";

    fn source_with_key(pem: &str) -> Result<GitHubAppTokenSource, ScmError> {
        GitHubAppTokenSource::new(
            GitHubAppTokenConfig {
                app_id: 7,
                private_key_pkcs8_pem: pem.to_string(),
                ..Default::default()
            },
            Arc::new(MockHttpTransport::new(200, "{}")),
            Arc::new(ManualClock::new(1_000_000)),
        )
    }

    #[test]
    fn config_and_key_material_are_validated_at_construction() {
        // The generated test key above is truncated: base64 decodes, but the
        // PKCS#8 structure does not — a construction-time typed refusal.
        assert!(matches!(
            source_with_key(TEST_KEY),
            Err(ScmError::Config(_))
        ));
        for bad in [
            GitHubAppTokenConfig {
                app_id: 0,
                private_key_pkcs8_pem: "x".into(),
                ..Default::default()
            },
            GitHubAppTokenConfig {
                app_id: 7,
                private_key_pkcs8_pem: String::new(),
                ..Default::default()
            },
            GitHubAppTokenConfig {
                app_id: 7,
                private_key_pkcs8_pem: "x".into(),
                api_base: "ftp://host".into(),
                ..Default::default()
            },
            GitHubAppTokenConfig {
                app_id: 7,
                private_key_pkcs8_pem: "x".into(),
                max_attempts: 0,
                ..Default::default()
            },
            GitHubAppTokenConfig {
                app_id: 7,
                private_key_pkcs8_pem: "x".into(),
                jwt_ttl_secs: 600_000,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn pkcs1_and_encrypted_keys_are_typed_refusals() {
        for pem in [
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----",
            "no pem at all",
        ] {
            let err = decode_pkcs8_pem(pem).unwrap_err();
            assert!(matches!(err, ScmError::Config(_)), "{err:?}");
        }
    }

    #[test]
    fn debug_never_renders_key_material() {
        let config = GitHubAppTokenConfig {
            app_id: 7,
            private_key_pkcs8_pem: "super-secret-key-material".into(),
            ..Default::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-key-material"));
        assert!(rendered.contains("<redacted>"));
    }
}
