//! OAuth 2.0 for remote Streamable HTTP MCP servers.
//!
//! The MCP server is the OAuth **resource server** and may advertise an
//! authorization server through RFC 9728 protected-resource metadata (via a
//! `WWW-Authenticate: Bearer resource_metadata=...` challenge or the
//! well-known fallback). This module performs, over the ONE checked egress
//! authority:
//!
//! 1. protected-resource + authorization-server metadata discovery;
//! 2. authorization-code + PKCE (S256) with a loopback redirect URI;
//! 3. token exchange, expiry-aware refresh and RFC 7009 revocation;
//! 4. the client-credentials grant when the authorization server advertises
//!    it;
//! 5. scope negotiation against the resource/authorization-server metadata.
//!
//! # Secrets
//!
//! Every credential (access/refresh token, client secret, PKCE verifier and
//! authorization-flow state) is a [`SecretToken`]: redacted `Debug`,
//! zeroized on drop, and never rendered in an error. The vault bundle
//! serializer is explicit ([`OAuthTokenSet::to_vault_bytes`]); no `Serialize`
//! derive can accidentally print a token.
//!
//! # Typed failures
//!
//! [`OAUTH_UNCONFIGURED_CODE`] = no usable metadata/client registration,
//! [`OAUTH_DENIED_CODE`] = the authorization server refused (including a
//! PKCE/state/scope mismatch) and [`OAUTH_EXPIRED_CODE`] = the credential has
//! expired and cannot be refreshed. These are `ErrorKind::Provider` codes,
//! never a fabricated "connected" state.
//!
//! # Bounds
//!
//! Discovery/token bodies, form fields, metadata lists and endpoint counts
//! are all bounded. Every request rides a checked client pinned to the one
//! destination origin and to the loopback/external address-class policy; no
//! environment proxy or redirect bypass exists.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use faktor_core::error::{Error, ErrorKind};
use faktor_provider::egress::{
    execute_raw, CheckedHttpClient, EgressAddressPolicy, EgressClientOptions, EgressError,
    RawRequest, RawResponse, ResponseBudget,
};
use faktor_security::destination::DestinationPolicy;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::remote::SecretToken;

pub const OAUTH_UNCONFIGURED_CODE: &str = "oauth_unconfigured";
pub const OAUTH_DENIED_CODE: &str = "oauth_denied";
pub const OAUTH_EXPIRED_CODE: &str = "oauth_expired";
pub const OAUTH_DISCOVERY_CODE: &str = "oauth_discovery";

/// The durable auth phases surfaced by the native MCP surface. A token only
/// exists in `authorized`; `expired`/`denied` never read as usable.
pub const AUTH_AUTHORIZED: &str = "authorized";
pub const AUTH_SIGNED_OUT: &str = "signed_out";
pub const AUTH_EXPIRED: &str = "expired";
pub const AUTH_DENIED: &str = "denied";
pub const AUTH_UNCONFIGURED: &str = "unconfigured";

const MAX_METADATA_BYTES: u64 = 256 * 1024;
const MAX_TOKEN_BYTES: u64 = 64 * 1024;
const MAX_DISCOVERY_URLS: usize = 8;
const MAX_LIST_ENTRIES: usize = 64;
const MAX_SCOPE_CHARS: usize = 128;
const MAX_ENDPOINT_CHARS: usize = 2_048;
const MAX_CLIENT_ID_CHARS: usize = 512;
const MAX_ACCOUNT_CHARS: usize = 256;
const MAX_FLOW_TTL_MS: i64 = 10 * 60 * 1000;
const EXPIRY_SKEW_MS: i64 = 30_000;
const OAUTH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FORM_BYTES: usize = 16 * 1024;

/// A parsed `WWW-Authenticate` bearer challenge. Nothing here is a
/// credential: it is discovery metadata supplied by the resource server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BearerChallenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
}

/// Protected-resource metadata (RFC 9728), already validated and bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub scopes_supported: Vec<String>,
}

/// Authorization-server metadata (RFC 8414 / OIDC discovery), already
/// validated and bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: String,
    pub revocation_endpoint: Option<String>,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
}

/// The result of discovery: the resource, its authorization server and the
/// scopes both sides advertise. The negotiated request scope is intentionally
/// not chosen here (an operator may narrow it at login time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub resource: String,
    pub protected: ProtectedResourceMetadata,
    pub authorization_server: AuthorizationServerMetadata,
    /// The negotiated default scope set (protected-resource scopes plus any
    /// scope named by the bearer challenge, bounded and de-duplicated).
    pub scopes: Vec<String>,
}

/// A client registration used for token-endpoint authentication.
#[derive(Clone)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: Option<SecretToken>,
}

impl std::fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenEndpointAuthMethod {
    ClientSecretBasic,
    ClientSecretPost,
    None,
}

/// The token-endpoint authentication method selected from the authorization
/// server metadata. A secret-bearing method requires a secret; an
/// authorization server that offers no usable method is typed unconfigured.
fn choose_auth_method(
    metadata: &AuthorizationServerMetadata,
    credentials: &ClientCredentials,
) -> Result<TokenEndpointAuthMethod, Error> {
    let methods = &metadata.token_endpoint_auth_methods_supported;
    let offered = |name: &str| methods.iter().any(|method| method == name);
    match (&credentials.client_secret, methods.is_empty()) {
        (Some(_), true) => Ok(TokenEndpointAuthMethod::ClientSecretBasic),
        (Some(_), false) if offered("client_secret_basic") => {
            Ok(TokenEndpointAuthMethod::ClientSecretBasic)
        }
        (Some(_), false) if offered("client_secret_post") => {
            Ok(TokenEndpointAuthMethod::ClientSecretPost)
        }
        (Some(_), false) if offered("none") => Ok(TokenEndpointAuthMethod::None),
        (None, true) => Ok(TokenEndpointAuthMethod::None),
        (None, false) if offered("none") => Ok(TokenEndpointAuthMethod::None),
        _ => Err(unconfigured(
            "the authorization server does not advertise a token-endpoint authentication method usable with the configured client",
        )),
    }
}

/// One authorization-code + PKCE flow, held by the operator surface between
/// the browser redirect and the token exchange.
pub struct PendingAuthorization {
    client: OAuthClient,
    discovery: Discovery,
    credentials: ClientCredentials,
    redirect_uri: String,
    scopes: Vec<String>,
    server_id: String,
    account: Option<String>,
    state: SecretToken,
    verifier: SecretToken,
    authorization_url: String,
    expires_at_ms: i64,
    consumed: AtomicBool,
}

impl std::fmt::Debug for PendingAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingAuthorization")
            // The authorization URL carries the CSRF `state` query parameter;
            // only its non-secret origin/path may appear in a log line.
            .field(
                "authorization_url",
                &reqwest::Url::parse(&self.authorization_url)
                    .map(|mut url| {
                        url.set_query(None);
                        url.to_string()
                    })
                    .unwrap_or_else(|_| "<invalid>".into()),
            )
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("state", &"<redacted>")
            .field("verifier", &"<redacted>")
            .finish()
    }
}

impl PendingAuthorization {
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn issuer(&self) -> &str {
        &self.discovery.authorization_server.issuer
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms
    }

    /// Complete the flow from the callback URL (or bare query string). The
    /// flow is single-use: a second call is a typed denial even if the same
    /// authorization code is replayed.
    pub async fn complete(&self, callback: &str) -> Result<OAuthTokenSet, Error> {
        let callback = parse_callback(callback)?;
        self.finish(
            callback.code,
            callback.state,
            callback.error,
            callback.error_description,
        )
        .await
    }

    /// Complete from already-extracted callback parameters (the CLI/IDE
    /// owns the listener and must not re-encode a code/state through a URL).
    pub async fn complete_with_params(
        &self,
        code: &str,
        state: &str,
    ) -> Result<OAuthTokenSet, Error> {
        self.finish(Some(code.to_string()), Some(state.to_string()), None, None)
            .await
    }

    async fn finish(
        &self,
        code: Option<String>,
        state: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    ) -> Result<OAuthTokenSet, Error> {
        let now = now_ms();
        if self.is_expired(now) {
            return Err(expired(
                "the authorization request expired before the callback",
            ));
        }
        if self.consumed.swap(true, Ordering::SeqCst) {
            return Err(denied(
                "authorization flow was already used (state/code replay refused)",
            ));
        }
        if let Some(error) = error {
            let description = error_description
                .as_deref()
                .map(|text| sanitize_text(text, 256))
                .filter(|text| !text.is_empty());
            return Err(denied(match description {
                Some(description) => {
                    format!("authorization server returned {error}: {description}")
                }
                None => format!("authorization server returned {error}"),
            }));
        }
        let state = state
            .as_deref()
            .ok_or_else(|| denied("the authorization callback carried no state"))?;
        if !constant_time_eq(state.as_bytes(), self.state.expose().as_bytes()) {
            return Err(denied(
                "the authorization callback state does not match this flow (CSRF refused)",
            ));
        }
        let code = code.ok_or_else(|| denied("the authorization callback carried no code"))?;
        if code.is_empty() || code.len() > 8_192 || code.chars().any(char::is_control) {
            return Err(denied("the authorization code is malformed"));
        }
        let code = SecretToken::from_bytes(code.into_bytes())
            .map_err(|_| denied("the authorization code is not a valid UTF-8 credential"))?;
        let auth_method =
            choose_auth_method(&self.discovery.authorization_server, &self.credentials)?;
        let mut form = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.expose().to_string()),
            ("redirect_uri".to_string(), self.redirect_uri.clone()),
            ("client_id".to_string(), self.credentials.client_id.clone()),
            (
                "code_verifier".to_string(),
                self.verifier.expose().to_string(),
            ),
            ("resource".to_string(), self.discovery.resource.clone()),
        ];
        apply_client_auth(&mut form, auth_method, &self.credentials);
        let value = self
            .client
            .token_request(
                &self.discovery.authorization_server.token_endpoint,
                form,
                auth_method,
                &self.credentials,
                false,
            )
            .await
            .map_err(|error| scrub_error(error, [Some(&code), Some(&self.verifier)]))?;
        token_set_from_response(
            &value,
            &self.discovery,
            &self.credentials,
            &self.scopes,
            &self.server_id,
            self.account.clone(),
            now_ms(),
            "authorization_code",
        )
    }
}

/// The OAuth engine. Cloning is cheap (configuration only); the checked
/// client is built per destination origin, so discovery from one origin can
/// never silently authenticate against an unrelated origin.
#[derive(Clone)]
pub struct OAuthClient {
    address_policy: EgressAddressPolicy,
    timeout: Duration,
    max_metadata_bytes: u64,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("address_policy", &self.address_policy)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Default for OAuthClient {
    fn default() -> Self {
        Self::production()
    }
}

impl OAuthClient {
    /// Production: externally routable destinations only, https endpoints.
    pub fn production() -> Self {
        Self {
            address_policy: EgressAddressPolicy::EXTERNAL,
            timeout: OAUTH_TIMEOUT,
            max_metadata_bytes: MAX_METADATA_BYTES,
        }
    }

    /// The local-fixture seam (loopback OAuth/MCP servers in tests). It is
    /// the same checked client builder with the explicit loopback address
    /// rule; it never disables the allowlist, resolver or secret handling.
    pub fn with_address_policy(address_policy: EgressAddressPolicy) -> Self {
        Self {
            address_policy,
            timeout: OAUTH_TIMEOUT,
            max_metadata_bytes: MAX_METADATA_BYTES,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.clamp(Duration::from_millis(100), Duration::from_secs(120));
        self
    }

    fn allow_http(&self) -> bool {
        self.address_policy.allow_loopback()
    }

    fn client_for(&self, url: &reqwest::Url) -> Result<CheckedHttpClient, Error> {
        let origin = url.origin().ascii_serialization();
        let policy = DestinationPolicy::parse_lines([origin.as_str()]).map_err(|error| {
            discovery_error(format!("OAuth destination policy is invalid: {error}"))
        })?;
        let options = EgressClientOptions {
            connect_timeout: Some(
                self.timeout
                    .min(Duration::from_secs(10))
                    .max(Duration::from_millis(100)),
            ),
            ..Default::default()
        };
        CheckedHttpClient::try_with_policy_scan_addresses_and_options(
            policy,
            None,
            self.address_policy,
            options,
        )
        .map_err(|error| discovery_error(format!("OAuth checked client: {error}")))
    }

    /// Probe the MCP resource for a bearer challenge. A public/no-auth
    /// server is not an error here; the caller falls back to well-known
    /// discovery. The probe body is a minimal JSON-RPC initialize so a
    /// Streamable HTTP server's auth middleware sees its normal request
    /// shape before answering 401.
    pub async fn probe_challenge(&self, resource: &str) -> Result<Option<BearerChallenge>, Error> {
        let resource = parse_resource_url(resource, self.allow_http())?;
        let client = self.client_for(&resource)?;
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": crate::remote::CLIENT_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "faktor-plus", "version": "0.1.0" }
            }
        });
        let request = RawRequest::new("POST", resource.as_str())
            .header("accept", "application/json, text/event-stream")
            .json_body(&body)
            .map_err(map_egress)?;
        let budget = ResponseBudget::for_timeout(self.timeout.min(Duration::from_secs(10)), 16_384);
        let response = execute_raw(&client, request, &budget)
            .await
            .map_err(map_egress)?;
        if !matches!(response.status, 401 | 403) {
            return Ok(None);
        }
        Ok(response
            .header("www-authenticate")
            .and_then(parse_bearer_challenge))
    }

    /// Discover the protected resource and its authorization server. The
    /// challenge hint (when present) is preferred; the RFC 9728 well-known
    /// fallback is always tried for a Streamable HTTP endpoint.
    pub async fn discover(
        &self,
        resource: &str,
        challenge: Option<&BearerChallenge>,
    ) -> Result<Discovery, Error> {
        let resource_url = parse_resource_url(resource, self.allow_http())?;
        let hint = challenge.and_then(|challenge| challenge.resource_metadata.as_deref());
        let protected = self.fetch_protected_resource(&resource_url, hint).await?;
        let advertised_resource =
            validate_endpoint_url(&protected.resource, self.allow_http(), "protected resource")?;
        if advertised_resource.query().is_some() {
            return Err(discovery_error(
                "protected-resource metadata describes a resource with a query component",
            ));
        }
        if !same_resource(&advertised_resource, &resource_url) {
            return Err(discovery_error(
                "protected-resource metadata describes a different resource than the MCP endpoint",
            ));
        }
        if protected.authorization_servers.is_empty() {
            return Err(unconfigured(
                "protected-resource metadata names no authorization server",
            ));
        }
        let authorization_server = self.fetch_authorization_server(&protected).await?;
        let mut scopes = if protected.scopes_supported.is_empty() {
            authorization_server.scopes_supported.clone()
        } else {
            protected.scopes_supported.clone()
        };
        if let Some(scope) = challenge.and_then(|challenge| challenge.scope.as_deref()) {
            for scope in scope.split_whitespace() {
                if scopes.len() >= MAX_LIST_ENTRIES {
                    break;
                }
                if !scopes.iter().any(|known| known == scope) {
                    scopes.push(scope.to_string());
                }
            }
        }
        Ok(Discovery {
            resource: protected.resource.clone(),
            protected,
            authorization_server,
            scopes,
        })
    }

    async fn fetch_protected_resource(
        &self,
        resource: &reqwest::Url,
        hint: Option<&str>,
    ) -> Result<ProtectedResourceMetadata, Error> {
        let mut urls = Vec::new();
        if let Some(hint) = hint {
            // A hostile/malformed challenge hint cannot suppress the
            // well-known fallback; only a valid checked URL is preferred.
            if let Ok(url) =
                validate_endpoint_url(hint, self.allow_http(), "protected resource metadata")
            {
                urls.push(url);
            }
        }
        urls.extend(protected_resource_urls(resource));
        let mut failures = 0_usize;
        let mut last_error = None;
        for url in urls.into_iter().take(MAX_DISCOVERY_URLS) {
            match self.fetch_json_optional(&url).await {
                Ok(Some(value)) => match ProtectedResourceMetadata::parse(&value) {
                    Ok(metadata) => return Ok(metadata),
                    Err(error) => last_error = Some(error),
                },
                Ok(None) => failures += 1,
                Err(error) => {
                    failures += 1;
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            unconfigured(format!(
                "the MCP resource advertises no protected-resource metadata (tried {failures} well-known location(s)); configure an OAuth-capable MCP server or a token binding"
            ))
        }))
    }

    async fn fetch_authorization_server(
        &self,
        protected: &ProtectedResourceMetadata,
    ) -> Result<AuthorizationServerMetadata, Error> {
        let mut last_error = None;
        for issuer in protected
            .authorization_servers
            .iter()
            .take(MAX_DISCOVERY_URLS)
        {
            let issuer_url = match validate_endpoint_url(
                issuer,
                self.allow_http(),
                "authorization server issuer",
            ) {
                Ok(url) => url,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
            for url in authorization_server_urls(&issuer_url) {
                match self.fetch_json_optional(&url).await {
                    Ok(Some(value)) => {
                        let metadata = match AuthorizationServerMetadata::parse(&value) {
                            Ok(metadata) => metadata,
                            Err(error) => {
                                last_error = Some(error);
                                continue;
                            }
                        };
                        if !same_issuer(&metadata.issuer, &issuer_url) {
                            last_error = Some(discovery_error(
                                "authorization-server metadata issuer does not match the advertised issuer",
                            ));
                            continue;
                        }
                        return Ok(metadata);
                    }
                    Ok(None) => {}
                    Err(error) => last_error = Some(error),
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            unconfigured("the advertised authorization server publishes no usable metadata")
        }))
    }

    /// Dynamic client registration (RFC 7591), used only when the operator
    /// did not supply a client id and the authorization server advertises a
    /// registration endpoint. The issued secret is wrapped immediately.
    pub async fn register_client(
        &self,
        discovery: &Discovery,
        redirect_uri: &str,
        scopes: &[String],
    ) -> Result<ClientCredentials, Error> {
        let Some(endpoint) = discovery
            .authorization_server
            .registration_endpoint
            .as_deref()
        else {
            return Err(unconfigured(
                "the authorization server advertises no dynamic client registration and no client id was supplied",
            ));
        };
        // Validate the redirect and scopes BEFORE the registration POST: an
        // invalid flow must not create a client registration side effect.
        validate_loopback_redirect(redirect_uri)?;
        if scopes.len() > MAX_LIST_ENTRIES {
            return Err(Error::new(
                ErrorKind::Oversized,
                "at most 64 OAuth scopes may be registered",
            ));
        }
        for scope in scopes {
            validate_scope(scope)?;
        }
        let endpoint = validate_endpoint_url(endpoint, self.allow_http(), "registration endpoint")?;
        let body = json!({
            "client_name": "Faktor",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "scope": scopes.join(" "),
        });
        let client = self.client_for(&endpoint)?;
        let request = RawRequest::new("POST", endpoint.as_str())
            .header("accept", "application/json")
            .json_body(&body)
            .map_err(map_egress)?;
        let response = execute_raw(
            &client,
            request,
            &ResponseBudget::for_timeout(self.timeout, self.max_metadata_bytes),
        )
        .await
        .map_err(map_egress)?;
        if !(200..300).contains(&response.status) {
            return Err(denied(format!(
                "dynamic client registration was refused (HTTP {})",
                response.status
            )));
        }
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|_| discovery_error("dynamic client registration returned malformed JSON"))?;
        let client_id = text_field(&value, "client_id", MAX_CLIENT_ID_CHARS)
            .filter(|client_id| !client_id.is_empty())
            .ok_or_else(|| discovery_error("dynamic client registration returned no client_id"))?;
        let client_secret = text_field(&value, "client_secret", 8_192)
            .map(|secret| {
                SecretToken::from_bytes(secret.into_bytes())
                    .map_err(|_| denied("dynamic client registration returned a malformed secret"))
            })
            .transpose()?;
        Ok(ClientCredentials {
            client_id,
            client_secret,
        })
    }

    /// Begin authorization-code + PKCE. The redirect URI is restricted to a
    /// loopback address (RFC 8252); the operator's CLI/IDE owns that
    /// listener and passes the callback back to [`PendingAuthorization`].
    pub fn begin_authorization(
        &self,
        discovery: &Discovery,
        credentials: &ClientCredentials,
        redirect_uri: &str,
        requested_scopes: Option<Vec<String>>,
        server_id: &str,
        account: Option<String>,
    ) -> Result<PendingAuthorization, Error> {
        let Some(authorization_endpoint) = discovery
            .authorization_server
            .authorization_endpoint
            .as_deref()
        else {
            return Err(unconfigured(
                "the authorization server advertises no authorization endpoint",
            ));
        };
        let methods = &discovery
            .authorization_server
            .code_challenge_methods_supported;
        if !methods.iter().any(|method| method == "S256") {
            return Err(unconfigured(
                "the authorization server does not advertise PKCE S256 support",
            ));
        }
        validate_loopback_redirect(redirect_uri)?;
        validate_identifier(&credentials.client_id, MAX_CLIENT_ID_CHARS, "client id")?;
        validate_identifier(server_id, 256, "server id")?;
        if let Some(account) = &account {
            validate_identifier(account, MAX_ACCOUNT_CHARS, "account")?;
        }
        let scopes = negotiate_scopes(discovery, requested_scopes)?;
        let auth_endpoint = validate_endpoint_url(
            authorization_endpoint,
            self.allow_http(),
            "authorization endpoint",
        )?;
        let state = random_secret();
        let verifier = random_secret();
        let challenge = pkce_challenge(&verifier);
        let mut url = auth_endpoint;
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("response_type", "code")
                .append_pair("client_id", &credentials.client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("state", state.expose())
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", &discovery.resource);
            if !scopes.is_empty() {
                pairs.append_pair("scope", &scopes.join(" "));
            }
        }
        Ok(PendingAuthorization {
            client: self.clone(),
            discovery: discovery.clone(),
            credentials: credentials.clone(),
            redirect_uri: redirect_uri.to_string(),
            scopes,
            server_id: server_id.to_string(),
            account,
            state,
            verifier,
            authorization_url: url.to_string(),
            expires_at_ms: now_ms().saturating_add(MAX_FLOW_TTL_MS),
            consumed: AtomicBool::new(false),
        })
    }

    /// The client-credentials grant, when the authorization server
    /// advertises it. No browser/redirect is involved.
    pub async fn client_credentials(
        &self,
        discovery: &Discovery,
        credentials: &ClientCredentials,
        requested_scopes: Option<Vec<String>>,
        server_id: &str,
        account: Option<String>,
    ) -> Result<OAuthTokenSet, Error> {
        let grants = &discovery.authorization_server.grant_types_supported;
        if !grants.iter().any(|grant| grant == "client_credentials") {
            return Err(unconfigured(
                "the authorization server does not advertise the client_credentials grant",
            ));
        }
        validate_identifier(&credentials.client_id, MAX_CLIENT_ID_CHARS, "client id")?;
        validate_identifier(server_id, 256, "server id")?;
        if let Some(account) = &account {
            validate_identifier(account, MAX_ACCOUNT_CHARS, "account")?;
        }
        let scopes = negotiate_scopes(discovery, requested_scopes)?;
        let auth_method = choose_auth_method(&discovery.authorization_server, credentials)?;
        let mut form = vec![
            ("grant_type".to_string(), "client_credentials".to_string()),
            ("client_id".to_string(), credentials.client_id.clone()),
            ("resource".to_string(), discovery.resource.clone()),
        ];
        if !scopes.is_empty() {
            form.push(("scope".to_string(), scopes.join(" ")));
        }
        apply_client_auth(&mut form, auth_method, credentials);
        let value = self
            .token_request(
                &discovery.authorization_server.token_endpoint,
                form,
                auth_method,
                credentials,
                false,
            )
            .await
            .map_err(|error| scrub_error(error, [credentials.client_secret.as_ref()]))?;
        token_set_from_response(
            &value,
            discovery,
            credentials,
            &scopes,
            server_id,
            account,
            now_ms(),
            "client_credentials",
        )
    }

    /// Exchange a refresh token. An `invalid_grant` response is the typed
    /// expired state; client/scope refusals are the typed denied state.
    pub async fn refresh(&self, tokens: &OAuthTokenSet) -> Result<OAuthTokenSet, Error> {
        let credentials = ClientCredentials {
            client_id: tokens.client_id.clone(),
            client_secret: tokens.client_secret.clone(),
        };
        let auth_method = tokens.token_endpoint_auth_method;
        // A client-credentials bundle has no refresh token: re-mint the
        // grant from the stored registration instead of inventing one.
        let client_credentials = tokens.grant_type == "client_credentials";
        if !client_credentials && tokens.refresh.is_none() {
            return Err(expired(
                "the access token expired and no refresh token is stored",
            ));
        }
        let mut form = if client_credentials {
            vec![
                ("grant_type".to_string(), "client_credentials".to_string()),
                ("client_id".to_string(), credentials.client_id.clone()),
                ("resource".to_string(), tokens.resource.clone()),
            ]
        } else {
            let refresh = tokens.refresh.as_ref().ok_or_else(|| {
                expired("the access token expired and no refresh token is stored")
            })?;
            vec![
                ("grant_type".to_string(), "refresh_token".to_string()),
                ("refresh_token".to_string(), refresh.expose().to_string()),
                ("client_id".to_string(), credentials.client_id.clone()),
                ("resource".to_string(), tokens.resource.clone()),
            ]
        };
        if !tokens.scopes.is_empty() {
            form.push(("scope".to_string(), tokens.scopes.join(" ")));
        }
        apply_client_auth(&mut form, auth_method, &credentials);
        let value = self
            .token_request(
                &tokens.token_endpoint,
                form,
                auth_method,
                &credentials,
                !client_credentials,
            )
            .await
            .map_err(|error| {
                scrub_error(
                    error,
                    [
                        Some(&tokens.access),
                        tokens.refresh.as_ref(),
                        tokens.client_secret.as_ref(),
                    ],
                )
            })?;
        apply_minted_tokens(tokens, &value, &tokens.scopes, now_ms())
    }

    /// RFC 7009 revocation, best-effort over the advertised endpoint. The
    /// caller still deletes the local vault material when this returns an
    /// error; `remote` only reports what the authorization server confirmed.
    pub async fn revoke(&self, tokens: &OAuthTokenSet) -> Result<RevocationOutcome, Error> {
        let Some(endpoint) = tokens.revocation_endpoint.as_deref() else {
            return Ok(RevocationOutcome {
                remote: false,
                detail: Some("the authorization server advertises no revocation endpoint".into()),
            });
        };
        let endpoint = validate_endpoint_url(endpoint, self.allow_http(), "revocation endpoint")?;
        let credentials = ClientCredentials {
            client_id: tokens.client_id.clone(),
            client_secret: tokens.client_secret.clone(),
        };
        let mut attempts = Vec::new();
        if let Some(refresh) = &tokens.refresh {
            attempts.push(("refresh_token", refresh.clone()));
        }
        attempts.push(("access_token", tokens.access.clone()));
        let mut last_status = None;
        let mut revoked = false;
        for (hint, token) in attempts {
            let mut form = vec![
                ("token".to_string(), token.expose().to_string()),
                ("token_type_hint".to_string(), hint.to_string()),
                ("client_id".to_string(), credentials.client_id.clone()),
            ];
            apply_client_auth(&mut form, tokens.token_endpoint_auth_method, &credentials);
            let response = self
                .form_request(
                    &endpoint,
                    form,
                    tokens.token_endpoint_auth_method,
                    &credentials,
                )
                .await
                .map_err(|error| {
                    scrub_error(
                        error,
                        [
                            Some(&tokens.access),
                            tokens.refresh.as_ref(),
                            tokens.client_secret.as_ref(),
                        ],
                    )
                })?;
            last_status = Some(response.status);
            if (200..300).contains(&response.status) {
                // Revoke every stored token type: an authorization server is
                // not required to cascade access-token invalidation from a
                // refresh-token revocation.
                revoked = true;
                continue;
            }
            if response.status == 400 || response.status == 404 {
                continue;
            }
            if revoked {
                break;
            }
            return Err(denied(format!(
                "the authorization server refused revocation (HTTP {})",
                response.status
            )));
        }
        Ok(RevocationOutcome {
            remote: revoked,
            detail: if revoked {
                None
            } else {
                Some(match last_status {
                    Some(status) => format!("the revocation endpoint answered HTTP {status}"),
                    None => "no revocable token was stored".into(),
                })
            },
        })
    }

    async fn fetch_json_optional(&self, url: &reqwest::Url) -> Result<Option<Value>, Error> {
        let client = self.client_for(url)?;
        let request = RawRequest::new("GET", url.as_str()).header("accept", "application/json");
        let response = execute_raw(
            &client,
            request,
            &ResponseBudget::for_timeout(self.timeout, self.max_metadata_bytes),
        )
        .await
        .map_err(map_egress)?;
        if matches!(response.status, 404 | 410) {
            return Ok(None);
        }
        if !(200..300).contains(&response.status) {
            return Err(discovery_error(format!(
                "OAuth metadata request answered HTTP {}",
                response.status
            )));
        }
        let value = serde_json::from_slice(&response.body)
            .map_err(|_| discovery_error("OAuth metadata is not valid JSON"))?;
        Ok(Some(value))
    }

    async fn token_request(
        &self,
        endpoint: &str,
        form: Vec<(String, String)>,
        auth_method: TokenEndpointAuthMethod,
        credentials: &ClientCredentials,
        expired_on_invalid_grant: bool,
    ) -> Result<Value, Error> {
        let endpoint = validate_endpoint_url(endpoint, self.allow_http(), "token endpoint")?;
        let response = self
            .form_request(&endpoint, form, auth_method, credentials)
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(token_response_error(&response, expired_on_invalid_grant));
        }
        serde_json::from_slice(&response.body)
            .map_err(|_| discovery_error("the token endpoint returned malformed JSON"))
    }

    async fn form_request(
        &self,
        endpoint: &reqwest::Url,
        form: Vec<(String, String)>,
        auth_method: TokenEndpointAuthMethod,
        credentials: &ClientCredentials,
    ) -> Result<RawResponse, Error> {
        let mut body = String::new();
        for (name, value) in &form {
            if !body.is_empty() {
                body.push('&');
            }
            body.push_str(&form_encode(name));
            body.push('=');
            body.push_str(&form_encode(value));
        }
        if body.len() > MAX_FORM_BYTES {
            return Err(Error::new(
                ErrorKind::Oversized,
                "the OAuth token request exceeds its bound",
            ));
        }
        let client = self.client_for(endpoint)?;
        let mut request = RawRequest::new("POST", endpoint.as_str())
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .bytes_body(body.into_bytes());
        if matches!(auth_method, TokenEndpointAuthMethod::ClientSecretBasic) {
            if let Some(secret) = &credentials.client_secret {
                // RFC 6749 2.3.1: both halves are form-urlencoded before
                // being joined for the Basic credential.
                let encoded = Zeroizing::new(format!(
                    "{}:{}",
                    form_encode(&credentials.client_id),
                    form_encode(secret.expose())
                ));
                let basic = Zeroizing::new(
                    base64::engine::general_purpose::STANDARD.encode(encoded.as_bytes()),
                );
                request = request.header("authorization", format!("Basic {}", basic.as_str()));
            }
        }
        execute_raw(
            &client,
            request,
            &ResponseBudget::for_timeout(self.timeout, MAX_TOKEN_BYTES),
        )
        .await
        .map_err(map_egress)
    }
}

/// The result of a remote revocation attempt. Local deletion is a separate,
/// always-performed operation; this only says what the AS confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationOutcome {
    pub remote: bool,
    pub detail: Option<String>,
}

impl ProtectedResourceMetadata {
    fn parse(value: &Value) -> Result<Self, Error> {
        let resource = text_field(value, "resource", MAX_ENDPOINT_CHARS)
            .ok_or_else(|| unconfigured("protected-resource metadata has no resource"))?;
        let authorization_servers = string_list_field(
            value,
            "authorization_servers",
            MAX_DISCOVERY_URLS,
            MAX_ENDPOINT_CHARS,
        );
        let scopes_supported =
            string_list_field(value, "scopes_supported", MAX_LIST_ENTRIES, MAX_SCOPE_CHARS);
        Ok(Self {
            resource,
            authorization_servers,
            scopes_supported,
        })
    }
}

impl AuthorizationServerMetadata {
    fn parse(value: &Value) -> Result<Self, Error> {
        let issuer = text_field(value, "issuer", MAX_ENDPOINT_CHARS)
            .ok_or_else(|| unconfigured("authorization-server metadata has no issuer"))?;
        let token_endpoint = text_field(value, "token_endpoint", MAX_ENDPOINT_CHARS)
            .ok_or_else(|| unconfigured("authorization-server metadata has no token_endpoint"))?;
        Ok(Self {
            issuer,
            authorization_endpoint: text_field(value, "authorization_endpoint", MAX_ENDPOINT_CHARS),
            token_endpoint,
            revocation_endpoint: text_field(value, "revocation_endpoint", MAX_ENDPOINT_CHARS),
            registration_endpoint: text_field(value, "registration_endpoint", MAX_ENDPOINT_CHARS),
            scopes_supported: string_list_field(
                value,
                "scopes_supported",
                MAX_LIST_ENTRIES,
                MAX_SCOPE_CHARS,
            ),
            code_challenge_methods_supported: string_list_field(
                value,
                "code_challenge_methods_supported",
                MAX_LIST_ENTRIES,
                32,
            ),
            grant_types_supported: string_list_field(
                value,
                "grant_types_supported",
                MAX_LIST_ENTRIES,
                64,
            ),
            token_endpoint_auth_methods_supported: string_list_field(
                value,
                "token_endpoint_auth_methods_supported",
                MAX_LIST_ENTRIES,
                64,
            ),
        })
    }
}

/// One stored OAuth credential bundle. All credentials are [`SecretToken`]
/// values; the explicit vault serializer is the only way bytes leave this
/// type, and `Debug` never renders them.
#[derive(Clone)]
pub struct OAuthTokenSet {
    access: SecretToken,
    refresh: Option<SecretToken>,
    pub token_type: String,
    pub expires_at_ms: Option<i64>,
    pub obtained_ms: i64,
    pub scopes: Vec<String>,
    pub issuer: String,
    pub token_endpoint: String,
    pub revocation_endpoint: Option<String>,
    pub client_id: String,
    client_secret: Option<SecretToken>,
    pub token_endpoint_auth_method: TokenEndpointAuthMethod,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub resource: String,
    pub server_id: String,
    pub account: Option<String>,
    /// `authorization_code` or `client_credentials`. A client-credentials
    /// bundle has no refresh token and is re-minted from the stored client
    /// registration when it expires.
    grant_type: String,
}

impl std::fmt::Debug for OAuthTokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthTokenSet")
            .field("access", &"<redacted>")
            .field("refresh", &self.refresh.as_ref().map(|_| "<redacted>"))
            .field("token_type", &self.token_type)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scopes", &self.scopes)
            .field("issuer", &self.issuer)
            .field("resource", &self.resource)
            .field("server_id", &self.server_id)
            .field("account", &self.account)
            .field("grant_type", &self.grant_type)
            .finish_non_exhaustive()
    }
}

impl OAuthTokenSet {
    pub fn access_token(&self) -> SecretToken {
        self.access.clone()
    }

    pub fn refresh_token(&self) -> Option<SecretToken> {
        self.refresh.clone()
    }

    /// Every credential that may be reflected back by a resource or
    /// authorization server. The tokens are opaque wrappers with redacted
    /// `Debug`; this never renders plaintext.
    pub fn redaction_tokens(&self) -> Vec<SecretToken> {
        let mut tokens = vec![self.access.clone()];
        if let Some(refresh) = &self.refresh {
            tokens.push(refresh.clone());
        }
        if let Some(secret) = &self.client_secret {
            tokens.push(secret.clone());
        }
        tokens
    }

    /// Replace every stored credential in server-supplied text.
    pub fn scrub(&self, text: &str) -> String {
        self.redaction_tokens()
            .iter()
            .fold(text.to_string(), |text, token| token.scrub(&text))
    }

    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.expires_at_ms
            .is_some_and(|expires| now_ms.saturating_add(EXPIRY_SKEW_MS) >= expires)
    }

    pub fn has_refresh(&self) -> bool {
        self.refresh.is_some()
    }

    /// True when an expired bundle can be renewed without operator input:
    /// either a refresh token or a stored client-credentials registration.
    pub fn is_renewable(&self) -> bool {
        self.refresh.is_some() || self.grant_type == "client_credentials"
    }

    pub fn scopes_cover(&self, required: &[String]) -> bool {
        required.iter().all(|scope| self.scopes.contains(scope))
    }

    /// The only serialization path for a token bundle. The bytes are
    /// zeroized after the vault has copied them (the vault owns the
    /// durable material).
    pub fn to_vault_bytes(&self) -> Result<Zeroizing<Vec<u8>>, Error> {
        let value = json!({
            "version": 1,
            "server_id": self.server_id,
            "account": self.account,
            "access": self.access.expose(),
            "refresh": self.refresh.as_ref().map(SecretToken::expose),
            "token_type": self.token_type,
            "expires_at_ms": self.expires_at_ms,
            "obtained_ms": self.obtained_ms,
            "scopes": self.scopes,
            "issuer": self.issuer,
            "token_endpoint": self.token_endpoint,
            "revocation_endpoint": self.revocation_endpoint,
            "client_id": self.client_id,
            "client_secret": self.client_secret.as_ref().map(SecretToken::expose),
            "auth_method": auth_method_name(self.token_endpoint_auth_method),
            "auth_methods": self.token_endpoint_auth_methods_supported,
            "resource": self.resource,
            "grant_type": self.grant_type,
        });
        let bytes = Zeroizing::new(
            serde_json::to_vec(&value)
                .map_err(|_| Error::internal("MCP OAuth token bundle serialization failed"))?,
        );
        if bytes.len() > 64 * 1024 {
            return Err(Error::new(
                ErrorKind::Oversized,
                "the MCP OAuth token bundle exceeds its storage bound",
            ));
        }
        Ok(bytes)
    }

    /// Parse vault material. A malformed bundle is typed denied and never
    /// falls back to treating the bytes as a bearer token.
    pub fn from_vault_bytes(bytes: Vec<u8>) -> Result<Self, Error> {
        if bytes.len() > 64 * 1024 {
            return Err(Error::new(
                ErrorKind::Oversized,
                "the stored MCP OAuth bundle exceeds its bound",
            ));
        }
        let bytes = Zeroizing::new(bytes);
        let value: Value = serde_json::from_slice(bytes.as_slice())
            .map_err(|_| denied("the stored MCP OAuth bundle is malformed"))?;
        if value.get("version").and_then(Value::as_u64) != Some(1) {
            return Err(denied(
                "the stored MCP OAuth bundle has an unsupported version",
            ));
        }
        let access = text_field(&value, "access", 8_192)
            .ok_or_else(|| denied("the stored MCP OAuth bundle has no access token"))?;
        let access = SecretToken::from_bytes(access.into_bytes())
            .map_err(|_| denied("the stored MCP OAuth access token is malformed"))?;
        let refresh = text_field(&value, "refresh", 8_192)
            .map(|refresh| {
                SecretToken::from_bytes(refresh.into_bytes())
                    .map_err(|_| denied("the stored MCP OAuth refresh token is malformed"))
            })
            .transpose()?;
        let client_secret = text_field(&value, "client_secret", 8_192)
            .map(|secret| {
                SecretToken::from_bytes(secret.into_bytes())
                    .map_err(|_| denied("the stored MCP OAuth client secret is malformed"))
            })
            .transpose()?;
        let auth_method = match value
            .get("auth_method")
            .and_then(Value::as_str)
            .unwrap_or("none")
        {
            "client_secret_basic" => TokenEndpointAuthMethod::ClientSecretBasic,
            "client_secret_post" => TokenEndpointAuthMethod::ClientSecretPost,
            _ => TokenEndpointAuthMethod::None,
        };
        let token_type = text_field(&value, "token_type", 64).unwrap_or_else(|| "Bearer".into());
        if !token_type.eq_ignore_ascii_case("bearer") {
            return Err(denied(
                "the stored MCP OAuth bundle carries an unsupported token type",
            ));
        }
        Ok(Self {
            access,
            refresh,
            token_type,
            expires_at_ms: value.get("expires_at_ms").and_then(Value::as_i64),
            obtained_ms: value
                .get("obtained_ms")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            scopes: string_list_field(&value, "scopes", MAX_LIST_ENTRIES, MAX_SCOPE_CHARS),
            issuer: text_field(&value, "issuer", MAX_ENDPOINT_CHARS)
                .ok_or_else(|| denied("the stored MCP OAuth bundle has no issuer"))?,
            token_endpoint: text_field(&value, "token_endpoint", MAX_ENDPOINT_CHARS)
                .ok_or_else(|| denied("the stored MCP OAuth bundle has no token endpoint"))?,
            revocation_endpoint: text_field(&value, "revocation_endpoint", MAX_ENDPOINT_CHARS),
            client_id: text_field(&value, "client_id", MAX_CLIENT_ID_CHARS).unwrap_or_default(),
            client_secret,
            token_endpoint_auth_method: auth_method,
            token_endpoint_auth_methods_supported: string_list_field(
                &value,
                "auth_methods",
                MAX_LIST_ENTRIES,
                64,
            ),
            resource: text_field(&value, "resource", MAX_ENDPOINT_CHARS).unwrap_or_default(),
            server_id: text_field(&value, "server_id", 256).unwrap_or_default(),
            account: text_field(&value, "account", MAX_ACCOUNT_CHARS),
            grant_type: text_field(&value, "grant_type", 64)
                .unwrap_or_else(|| "authorization_code".into()),
        })
    }

    /// Test/utility constructor for a vault-bound credential. Only compiled
    /// for fixture tests; production tokens are always minted by a real
    /// token exchange.
    #[cfg(any(test, feature = "test-utils"))]
    #[allow(clippy::too_many_arguments)]
    pub fn for_test(
        server_id: &str,
        account: Option<&str>,
        access: &str,
        refresh: Option<&str>,
        expires_at_ms: Option<i64>,
        scopes: Vec<String>,
        token_endpoint: &str,
        revocation_endpoint: Option<&str>,
    ) -> Self {
        Self {
            access: SecretToken::new(access),
            refresh: refresh.map(SecretToken::new),
            token_type: "Bearer".into(),
            expires_at_ms,
            obtained_ms: now_ms(),
            scopes,
            issuer: "https://authorization.example.com".into(),
            token_endpoint: token_endpoint.to_string(),
            revocation_endpoint: revocation_endpoint.map(str::to_string),
            client_id: "test-client".into(),
            client_secret: None,
            token_endpoint_auth_method: TokenEndpointAuthMethod::None,
            token_endpoint_auth_methods_supported: vec!["none".into()],
            resource: "https://mcp.example.com/rpc".into(),
            server_id: server_id.to_string(),
            account: account.map(str::to_string),
            grant_type: "authorization_code".into(),
        }
    }
}

/// Parse a `WWW-Authenticate` value. Only the bearer scheme is recognized;
/// the parser is tolerant of whitespace and quoted-string escapes but never
/// derives a URL from an unquoted hostile fragment.
pub fn parse_bearer_challenge(header: &str) -> Option<BearerChallenge> {
    let header = header.trim();
    let rest = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))?;
    let mut challenge = BearerChallenge::default();
    for part in split_header_params(rest) {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = unquote(value.trim());
        match name.as_str() {
            "resource_metadata" if !value.is_empty() => {
                challenge.resource_metadata = Some(value);
            }
            "scope" if !value.is_empty() => challenge.scope = Some(value),
            _ => {}
        }
    }
    if challenge.resource_metadata.is_none() && challenge.scope.is_none() {
        None
    } else {
        Some(challenge)
    }
}

fn split_header_params(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0_usize;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    let mut out = String::with_capacity(value.len());
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    out
}

struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

fn parse_callback(raw: &str) -> Result<Callback, Error> {
    let raw = raw.trim();
    let url = if raw.starts_with('?') {
        reqwest::Url::parse(&format!("http://127.0.0.1/callback{raw}"))
    } else if raw.contains("://") {
        reqwest::Url::parse(raw)
    } else {
        // A bare `code=...&state=...` query is accepted without inventing a
        // non-loopback origin.
        reqwest::Url::parse(&format!("http://127.0.0.1/callback?{raw}"))
    }
    .map_err(|_| denied("the authorization callback is not a valid URL"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(denied("the authorization callback must be http(s)"));
    }
    let mut callback = Callback {
        code: None,
        state: None,
        error: None,
        error_description: None,
    };
    for (name, value) in url.query_pairs() {
        if value.len() > 8_192 {
            return Err(denied(
                "the authorization callback contains an oversized value",
            ));
        }
        match name.as_ref() {
            "code" => callback.code = Some(value.into_owned()),
            "state" => callback.state = Some(value.into_owned()),
            "error" => callback.error = Some(value.into_owned()),
            "error_description" => callback.error_description = Some(value.into_owned()),
            _ => {}
        }
    }
    Ok(callback)
}

#[allow(clippy::too_many_arguments)]
fn token_set_from_response(
    value: &Value,
    discovery: &Discovery,
    credentials: &ClientCredentials,
    requested_scopes: &[String],
    server_id: &str,
    account: Option<String>,
    obtained_ms: i64,
    grant_type: &str,
) -> Result<OAuthTokenSet, Error> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(token_error(error, value, 400, false));
    }
    let access = access_from_response(value)?;
    let granted = scope_field(value).unwrap_or_else(|| requested_scopes.to_vec());
    ensure_scopes_cover(requested_scopes, &granted)?;
    let refresh = text_field(value, "refresh_token", 8_192)
        .map(|refresh| {
            SecretToken::from_bytes(refresh.into_bytes())
                .map_err(|_| denied("the token response contained a malformed refresh token"))
        })
        .transpose()?;
    Ok(OAuthTokenSet {
        access,
        refresh,
        token_type: token_type(value)?,
        expires_at_ms: expires_at_from_response(value, obtained_ms),
        obtained_ms,
        scopes: granted,
        issuer: discovery.authorization_server.issuer.clone(),
        token_endpoint: discovery.authorization_server.token_endpoint.clone(),
        revocation_endpoint: discovery.authorization_server.revocation_endpoint.clone(),
        client_id: credentials.client_id.clone(),
        client_secret: credentials.client_secret.clone(),
        token_endpoint_auth_method: choose_auth_method(
            &discovery.authorization_server,
            credentials,
        )?,
        token_endpoint_auth_methods_supported: discovery
            .authorization_server
            .token_endpoint_auth_methods_supported
            .clone(),
        resource: discovery.resource.clone(),
        server_id: server_id.to_string(),
        account,
        grant_type: grant_type.to_string(),
    })
}

/// Apply a successful token response to an existing bundle. A refresh keeps
/// the old refresh token when the authorization server does not rotate it.
fn apply_minted_tokens(
    current: &OAuthTokenSet,
    value: &Value,
    requested_scopes: &[String],
    obtained_ms: i64,
) -> Result<OAuthTokenSet, Error> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(token_error(error, value, 400, false));
    }
    let granted = scope_field(value).unwrap_or_else(|| requested_scopes.to_vec());
    ensure_scopes_cover(requested_scopes, &granted)?;
    let mut next = current.clone();
    next.access = access_from_response(value)?;
    next.token_type = token_type(value)?;
    next.expires_at_ms = expires_at_from_response(value, obtained_ms);
    next.obtained_ms = obtained_ms;
    if let Some(refresh) = text_field(value, "refresh_token", 8_192) {
        next.refresh = Some(
            SecretToken::from_bytes(refresh.into_bytes())
                .map_err(|_| denied("the refresh response contained a malformed refresh token"))?,
        );
    }
    next.scopes = granted;
    Ok(next)
}

fn access_from_response(value: &Value) -> Result<SecretToken, Error> {
    let access = text_field(value, "access_token", 8_192)
        .ok_or_else(|| denied("the token endpoint returned no access token"))?;
    SecretToken::from_bytes(access.into_bytes())
        .map_err(|_| denied("the token endpoint returned a malformed access token"))
}

fn token_type(value: &Value) -> Result<String, Error> {
    let token_type = text_field(value, "token_type", 64).unwrap_or_else(|| "Bearer".into());
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(denied(
            "the token endpoint returned an unsupported token type",
        ));
    }
    Ok(token_type)
}

fn expires_at_from_response(value: &Value, now: i64) -> Option<i64> {
    let expires_in = match value.get("expires_in") {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.parse::<i64>().ok(),
        _ => None,
    }?;
    let expires_in = expires_in.clamp(0, 60 * 60 * 24 * 30);
    Some(now.saturating_add(expires_in.saturating_mul(1_000)))
}

fn token_error(error: &str, value: &Value, _status: u16, expired_on_invalid_grant: bool) -> Error {
    let description = value
        .get("error_description")
        .and_then(Value::as_str)
        .map(|text| sanitize_text(text, 256))
        .filter(|text| !text.is_empty());
    match error {
        "invalid_grant" | "expired_token" if expired_on_invalid_grant => {
            expired(match description {
                Some(description) => format!("the authorization grant expired: {description}"),
                None => "the authorization grant expired".into(),
            })
        }
        _ => {
            denied(match description {
                Some(description) => {
                    format!("the authorization server refused the token request ({error}): {description}")
                }
                None => format!("the authorization server refused the token request ({error})"),
            })
        }
    }
}

fn token_response_error(response: &RawResponse, expired_on_invalid_grant: bool) -> Error {
    let value: Option<Value> = serde_json::from_slice(&response.body).ok();
    if let Some(value) = &value {
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            return token_error(error, value, response.status, expired_on_invalid_grant);
        }
    }
    if (500..600).contains(&response.status) {
        return Error::new(
            ErrorKind::Provider {
                code: OAUTH_DISCOVERY_CODE.into(),
                retryable: true,
            },
            format!("the OAuth token endpoint answered HTTP {}", response.status),
        );
    }
    denied(format!(
        "the OAuth token endpoint refused the request (HTTP {})",
        response.status
    ))
}

fn ensure_scopes_cover(requested: &[String], granted: &[String]) -> Result<(), Error> {
    let missing: Vec<&str> = requested
        .iter()
        .filter(|scope| !granted.contains(scope))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(denied(format!(
        "granted scopes do not cover the negotiated MCP capability scopes: missing {}",
        missing.join(" ")
    )))
}

fn negotiate_scopes(
    discovery: &Discovery,
    requested: Option<Vec<String>>,
) -> Result<Vec<String>, Error> {
    // Either side may publish the authoritative scope set: the protected
    // resource names what it accepts, the authorization server names what it
    // can mint. A scope advertised by either is negotiable; a scope neither
    // advertises is refused typed instead of sent optimistically.
    let mut supported = discovery.protected.scopes_supported.clone();
    for scope in &discovery.authorization_server.scopes_supported {
        if !supported.contains(scope) {
            supported.push(scope.clone());
        }
    }
    let explicit = requested.is_some();
    let requested = requested.unwrap_or_else(|| discovery.scopes.clone());
    if requested.len() > MAX_LIST_ENTRIES {
        return Err(Error::new(
            ErrorKind::Oversized,
            "at most 64 OAuth scopes may be requested",
        ));
    }
    let mut out = Vec::new();
    for scope in requested {
        validate_scope(&scope)?;
        // The discovery default already combines metadata and the resource
        // server's challenge scope, so only an operator-supplied scope is
        // checked against the advertised allowlist.
        if explicit && !supported.is_empty() && !supported.contains(&scope) {
            return Err(denied(format!(
                "neither the protected resource nor the authorization server advertises the requested scope {scope:?}"
            )));
        }
        if !out.contains(&scope) {
            out.push(scope);
        }
    }
    Ok(out)
}

fn validate_scope(scope: &str) -> Result<(), Error> {
    if scope.is_empty()
        || scope.len() > MAX_SCOPE_CHARS
        || scope
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(Error::new(
            ErrorKind::Malformed,
            "an OAuth scope must be 1..=128 non-whitespace printable characters",
        ));
    }
    Ok(())
}

fn protected_resource_urls(resource: &reqwest::Url) -> Vec<reqwest::Url> {
    let origin = resource.origin().ascii_serialization();
    let mut urls = Vec::new();
    let path = resource.path().trim_end_matches('/');
    if !path.is_empty() && path != "/" {
        if let Ok(url) = reqwest::Url::parse(&format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        )) {
            urls.push(url);
        }
    }
    if let Ok(url) = reqwest::Url::parse(&format!("{origin}/.well-known/oauth-protected-resource"))
    {
        urls.push(url);
    }
    urls
}

fn authorization_server_urls(issuer: &reqwest::Url) -> Vec<reqwest::Url> {
    let origin = issuer.origin().ascii_serialization();
    let path = issuer.path().trim_end_matches('/');
    let mut urls = Vec::new();
    for well_known in [
        ".well-known/oauth-authorization-server",
        ".well-known/openid-configuration",
    ] {
        let raw = if path.is_empty() || path == "/" {
            format!("{origin}/{well_known}")
        } else {
            format!("{origin}/{well_known}{path}")
        };
        if let Ok(url) = reqwest::Url::parse(&raw) {
            urls.push(url);
        }
        if !path.is_empty() && path != "/" {
            for raw in [
                format!("{origin}/{well_known}"),
                format!("{origin}{path}/{well_known}"),
            ] {
                if let Ok(url) = reqwest::Url::parse(&raw) {
                    urls.push(url);
                }
            }
        }
    }
    urls
}

fn same_resource(advertised: &reqwest::Url, expected: &reqwest::Url) -> bool {
    advertised.scheme() == expected.scheme()
        && advertised.host_str() == expected.host_str()
        && advertised.port_or_known_default() == expected.port_or_known_default()
        && advertised.path().trim_end_matches('/') == expected.path().trim_end_matches('/')
}

fn same_issuer(metadata_issuer: &str, expected: &reqwest::Url) -> bool {
    let Ok(metadata_issuer) = reqwest::Url::parse(metadata_issuer) else {
        return false;
    };
    if metadata_issuer.scheme() != expected.scheme()
        || metadata_issuer.host_str() != expected.host_str()
        || metadata_issuer.port_or_known_default() != expected.port_or_known_default()
    {
        return false;
    }
    metadata_issuer.path().trim_end_matches('/') == expected.path().trim_end_matches('/')
}

fn parse_resource_url(raw: &str, allow_http: bool) -> Result<reqwest::Url, Error> {
    let url = validate_endpoint_url(raw, allow_http, "MCP resource endpoint")?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::new(
            ErrorKind::Malformed,
            "the MCP OAuth resource endpoint must not carry a query or fragment",
        ));
    }
    Ok(url)
}

fn validate_endpoint_url(raw: &str, allow_http: bool, what: &str) -> Result<reqwest::Url, Error> {
    if raw.len() > MAX_ENDPOINT_CHARS {
        return Err(unconfigured(format!("the {what} URL exceeds its bound")));
    }
    let url = reqwest::Url::parse(raw)
        .map_err(|_| unconfigured(format!("the {what} is not an absolute URL")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(unconfigured(format!(
            "the {what} must be an absolute http(s) URL"
        )));
    }
    if url.scheme() == "http" && !allow_http {
        return Err(unconfigured(format!(
            "the {what} must use https (plaintext is refused for external destinations)"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(unconfigured(format!(
            "the {what} must not embed userinfo credentials"
        )));
    }
    if url.fragment().is_some() {
        return Err(unconfigured(format!(
            "the {what} must not carry a fragment"
        )));
    }
    Ok(url)
}

fn validate_loopback_redirect(raw: &str) -> Result<(), Error> {
    let url = reqwest::Url::parse(raw)
        .map_err(|_| denied("the OAuth redirect URI is not an absolute URL"))?;
    if url.scheme() != "http" {
        return Err(denied(
            "the OAuth redirect URI must be a plain-http loopback URL",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| denied("the OAuth redirect URI must name a loopback host"))?
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";
    if !loopback {
        return Err(denied(
            "the OAuth redirect URI must target the operator's loopback listener (RFC 8252)",
        ));
    }
    if url.fragment().is_some() || url.query().is_some() {
        return Err(denied(
            "the OAuth redirect URI must not carry a query or fragment",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(denied(
            "the OAuth redirect URI must not embed userinfo credentials",
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, max: usize, what: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(Error::new(
            ErrorKind::Malformed,
            format!("the {what} must be 1..={max} non-control characters"),
        ));
    }
    Ok(())
}

fn text_field(value: &Value, name: &str, max: usize) -> Option<String> {
    let text = value.get(name)?.as_str()?;
    if text.len() > max || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.to_string())
}

fn string_list_field(value: &Value, name: &str, max_items: usize, max: usize) -> Vec<String> {
    value
        .get(name)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(max_items)
                .filter_map(Value::as_str)
                .filter(|text| text.len() <= max && !text.chars().any(char::is_control))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn scope_field(value: &Value) -> Option<Vec<String>> {
    let scope = value.get("scope").and_then(Value::as_str)?;
    let scopes: Vec<String> = scope
        .split_whitespace()
        .filter(|scope| validate_scope(scope).is_ok())
        .map(str::to_string)
        .take(MAX_LIST_ENTRIES)
        .collect();
    Some(scopes)
}

fn sanitize_text(text: &str, max: usize) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(max)
        .collect()
}

/// Remove every credential the client sent from an authorization-server
/// supplied error message. An AS may reflect a token/verifier back at us;
/// the typed error never carries the secret.
fn scrub_error<'a>(
    mut error: Error,
    secrets: impl IntoIterator<Item = Option<&'a SecretToken>>,
) -> Error {
    for secret in secrets.into_iter().flatten() {
        error.message = secret.scrub(&error.message);
    }
    error
}

fn apply_client_auth(
    form: &mut Vec<(String, String)>,
    method: TokenEndpointAuthMethod,
    credentials: &ClientCredentials,
) {
    match method {
        TokenEndpointAuthMethod::ClientSecretPost => {
            if let Some(secret) = &credentials.client_secret {
                form.push(("client_secret".to_string(), secret.expose().to_string()));
            }
        }
        TokenEndpointAuthMethod::ClientSecretBasic | TokenEndpointAuthMethod::None => {}
    }
}

fn auth_method_name(method: TokenEndpointAuthMethod) -> &'static str {
    match method {
        TokenEndpointAuthMethod::ClientSecretBasic => "client_secret_basic",
        TokenEndpointAuthMethod::ClientSecretPost => "client_secret_post",
        TokenEndpointAuthMethod::None => "none",
    }
}

fn random_secret() -> SecretToken {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    SecretToken::new(encoded)
}

fn pkce_challenge(verifier: &SecretToken) -> String {
    let digest = Sha256::digest(verifier.expose().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn unconfigured(message: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::Provider {
            code: OAUTH_UNCONFIGURED_CODE.into(),
            retryable: false,
        },
        message,
    )
}

fn denied(message: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::Provider {
            code: OAUTH_DENIED_CODE.into(),
            retryable: false,
        },
        message,
    )
}

fn expired(message: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::Provider {
            code: OAUTH_EXPIRED_CODE.into(),
            retryable: false,
        },
        message,
    )
}

fn discovery_error(message: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::Provider {
            code: OAUTH_DISCOVERY_CODE.into(),
            retryable: false,
        },
        message,
    )
}

fn map_egress(error: EgressError) -> Error {
    match error {
        EgressError::ResponseBudgetExceeded { component, .. } => {
            use faktor_provider::egress::BudgetComponent;
            match component {
                BudgetComponent::Bytes | BudgetComponent::Frames => Error::new(
                    ErrorKind::Oversized,
                    "the OAuth response exceeded its byte bound",
                ),
                _ => Error::timeout("the OAuth request exceeded its deadline"),
            }
        }
        EgressError::Denied { reason, .. } => Error::new(
            ErrorKind::Permission,
            format!("the OAuth destination was denied: {reason}"),
        ),
        EgressError::Transport(_) => {
            Error::new(ErrorKind::Network, "the OAuth endpoint was unreachable")
        }
        EgressError::ClientBuild { .. } => {
            Error::internal("the OAuth checked client could not be built")
        }
        EgressError::ResponseTooLarge { .. } => Error::new(
            ErrorKind::Oversized,
            "the OAuth response exceeded the materialization bound",
        ),
        other => discovery_error(format!("OAuth transport failed: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflected_credentials_are_scrubbed_from_typed_errors() {
        let tokens = OAuthTokenSet::for_test(
            "remote",
            Some("operator"),
            "access-secret",
            Some("refresh-secret"),
            Some(now_ms() + 60_000),
            vec!["mcp.read".into()],
            "https://auth.example.com/token",
            None,
        );
        assert_eq!(
            tokens.scrub("server echoed access-secret and refresh-secret"),
            "server echoed [redacted] and [redacted]"
        );
        let error = scrub_error(
            denied("the server echoed access-secret"),
            [Some(&tokens.access)],
        );
        assert_eq!(error.message, "the server echoed [redacted]");
        assert_eq!(
            match &error.kind {
                ErrorKind::Provider { code, .. } => code.as_str(),
                other => panic!("expected typed provider error, got {other:?}"),
            },
            OAUTH_DENIED_CODE
        );
    }

    #[test]
    fn bearer_challenge_parser_accepts_quoted_and_escaped_values_only() {
        let parsed = parse_bearer_challenge(
            "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource\", scope=\"mcp.read mcp.write\"",
        )
        .expect("challenge");
        assert_eq!(
            parsed.resource_metadata.as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(parsed.scope.as_deref(), Some("mcp.read mcp.write"));
        assert!(parse_bearer_challenge("Basic abc").is_none());
        assert!(parse_bearer_challenge("Bearer").is_none());
    }

    #[test]
    fn callback_parser_accepts_url_and_bare_query_forms() {
        for raw in [
            "http://127.0.0.1:9/callback?code=abc&state=xyz",
            "?code=abc&state=xyz",
            "code=abc&state=xyz",
        ] {
            let callback = parse_callback(raw).expect("callback");
            assert_eq!(callback.code.as_deref(), Some("abc"));
            assert_eq!(callback.state.as_deref(), Some("xyz"));
        }
    }

    #[test]
    fn metadata_without_required_fields_is_typed_unconfigured() {
        for error in [
            ProtectedResourceMetadata::parse(&json!({"authorization_servers": []}))
                .expect_err("no resource"),
            AuthorizationServerMetadata::parse(&json!({"issuer": "https://auth.example.com"}))
                .expect_err("no token endpoint"),
        ] {
            assert_eq!(
                match &error.kind {
                    ErrorKind::Provider { code, .. } => code.as_str(),
                    other => panic!("expected typed provider error, got {other:?}"),
                },
                OAUTH_UNCONFIGURED_CODE,
                "{error:?}"
            );
        }
    }

    #[test]
    fn pending_authorization_debug_redacts_the_csrf_state() {
        let discovery = Discovery {
            resource: "https://mcp.example.com/rpc".into(),
            protected: ProtectedResourceMetadata {
                resource: "https://mcp.example.com/rpc".into(),
                authorization_servers: vec!["https://auth.example.com".into()],
                scopes_supported: vec!["mcp.read".into()],
            },
            authorization_server: AuthorizationServerMetadata {
                issuer: "https://auth.example.com".into(),
                authorization_endpoint: Some("https://auth.example.com/authorize".into()),
                token_endpoint: "https://auth.example.com/token".into(),
                revocation_endpoint: Some("https://auth.example.com/revoke".into()),
                registration_endpoint: None,
                scopes_supported: vec!["mcp.read".into()],
                code_challenge_methods_supported: vec!["S256".into()],
                grant_types_supported: vec!["authorization_code".into()],
                token_endpoint_auth_methods_supported: vec!["none".into()],
            },
            scopes: vec!["mcp.read".into()],
        };
        let pending = OAuthClient::production()
            .begin_authorization(
                &discovery,
                &ClientCredentials {
                    client_id: "client".into(),
                    client_secret: None,
                },
                "http://127.0.0.1:43123/callback",
                None,
                "remote",
                Some("operator".into()),
            )
            .expect("pending authorization");
        let rendered = format!("{pending:?}");
        assert!(pending.authorization_url().contains("state="));
        assert!(!rendered.contains("state="), "{rendered}");
        assert!(!rendered.contains("code_challenge"), "{rendered}");
    }
}
