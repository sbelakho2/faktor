//! Vault-backed OAuth credential provider and the operator login surface
//! for remote Streamable HTTP MCP servers.
//!
//! Tokens are minted by [`faktor_mcp::oauth`] over the checked egress
//! authority, stored as an opaque vault bundle, and refreshed exactly once
//! after the resource server rejects the current bearer (401). The durable
//! MCP row carries only the vault handle, account and granted scopes; no
//! token ever enters an HTTP response, log line or model context.

use std::sync::Arc;

use faktor_core::error::{Error, ErrorKind};
use faktor_foundation::customization::McpServerRecord;
use faktor_foundation::Foundation;
use faktor_mcp::oauth::{
    ClientCredentials, OAuthClient, OAuthTokenSet, PendingAuthorization, AUTH_DENIED, AUTH_EXPIRED,
    AUTH_UNCONFIGURED, OAUTH_DENIED_CODE, OAUTH_EXPIRED_CODE, OAUTH_UNCONFIGURED_CODE,
};
use faktor_mcp::remote::SecretToken;
use futures_util::future::BoxFuture;
use rand::RngCore;
use serde_json::{json, Value};
use zeroize::Zeroizing;

use super::McpRuntime;

const MAX_PENDING_FLOWS: usize = 32;
const MAX_CLIENT_ID_CHARS: usize = 512;
const MAX_SECRET_CHARS: usize = 8_192;

/// A credential loaded for one connection. Exactly one of `token`/`auth` is
/// set (or neither when the server is unauthenticated).
pub(crate) struct LoadedCredential {
    pub token: Option<SecretToken>,
    pub auth: Option<Arc<dyn faktor_mcp::remote::RemoteAuth>>,
}

impl std::fmt::Debug for LoadedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedCredential")
            .field("token", &self.token)
            .field("oauth", &self.auth.as_ref().map(|_| "<configured>"))
            .finish()
    }
}

/// One vault-backed OAuth credential. `token()` proactively refreshes when
/// the recorded expiry is inside the skew; `refresh()` is the forced path
/// after a 401. A successful refresh rotates the SAME vault handle, so the
/// durable row never points at stale material.
pub(crate) struct RemoteVaultAuth {
    foundation: Arc<Foundation>,
    handle: String,
    server_id: String,
    bundle: tokio::sync::Mutex<OAuthTokenSet>,
    /// The last credentials handed to the transport, used only to scrub a
    /// server-reflected value out of an error message. Never serialized.
    redaction: std::sync::Mutex<Vec<SecretToken>>,
    client: OAuthClient,
}

impl RemoteVaultAuth {
    fn new(
        foundation: Arc<Foundation>,
        handle: String,
        bundle: OAuthTokenSet,
        client: OAuthClient,
    ) -> Self {
        Self {
            server_id: bundle.server_id.clone(),
            foundation,
            handle,
            redaction: std::sync::Mutex::new(bundle.redaction_tokens()),
            bundle: tokio::sync::Mutex::new(bundle),
            client,
        }
    }

    async fn access(&self, force: bool) -> Result<SecretToken, Error> {
        let mut bundle = self.bundle.lock().await;
        if !force && !bundle.is_expired(now_ms()) {
            return Ok(bundle.access_token());
        }
        match self.client.refresh(&bundle).await {
            Ok(next) => {
                let bytes = next.to_vault_bytes()?;
                self.foundation
                    .vault
                    .rotate(&self.handle, bytes.as_slice())
                    .map_err(|error| {
                        Error::internal(format!(
                            "the refreshed MCP OAuth credential could not be stored: {error}"
                        ))
                    })?;
                let token = next.access_token();
                let mut redaction = self
                    .redaction
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *redaction = next.redaction_tokens();
                *bundle = next;
                Ok(token)
            }
            Err(error) => {
                match error_code(&error) {
                    Some(OAUTH_EXPIRED_CODE) => self.mark_state(AUTH_EXPIRED),
                    Some(OAUTH_DENIED_CODE) => self.mark_state(AUTH_DENIED),
                    _ => {}
                }
                Err(error)
            }
        }
    }

    fn mark_state(&self, state: &str) {
        // The state update is best-effort: the typed error is the
        // authority; a failed projection must not mask it.
        let _ = self
            .foundation
            .customization
            .set_mcp_auth_state(&self.server_id, state);
    }
}

impl faktor_mcp::remote::RemoteAuth for RemoteVaultAuth {
    fn token<'a>(&'a self) -> BoxFuture<'a, Result<SecretToken, Error>> {
        Box::pin(self.access(false))
    }

    fn refresh<'a>(&'a self) -> BoxFuture<'a, Result<SecretToken, Error>> {
        Box::pin(self.access(true))
    }

    fn scrub(&self, text: &str) -> String {
        self.redaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .fold(text.to_string(), |text, token| token.scrub(&text))
    }
}

/// Project a typed login failure onto a signed-out row so the operator
/// surface distinguishes `unconfigured` from `denied`/`expired`. A row that
/// still holds a usable credential is never clobbered by a failed retry.
pub(crate) fn project_login_error(
    foundation: &Foundation,
    record: &McpServerRecord,
    error: &Error,
) {
    if record.auth_token_ref.is_some() {
        return;
    }
    let state = match error_code(error) {
        Some(OAUTH_UNCONFIGURED_CODE) => AUTH_UNCONFIGURED,
        Some(OAUTH_DENIED_CODE) => AUTH_DENIED,
        Some(OAUTH_EXPIRED_CODE) => AUTH_EXPIRED,
        _ => return,
    };
    let _ = foundation
        .customization
        .set_mcp_auth_state(&record.id, state);
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn error_code(error: &Error) -> Option<&str> {
    match &error.kind {
        ErrorKind::Provider { code, .. } => Some(code.as_str()),
        _ => None,
    }
}

/// Load the record's credential. A raw legacy bearer is accepted unchanged
/// (the pre-OAuth token binding); a vault bundle is decoded and bound to the
/// exact server+account so a swapped handle cannot authenticate another
/// server. An expired bundle without a refresh token fails typed BEFORE any
/// connect.
pub(crate) fn load_credential(
    foundation: &Foundation,
    record: &McpServerRecord,
    client: OAuthClient,
) -> Result<LoadedCredential, Error> {
    let Some(handle) = record.auth_token_ref.clone() else {
        return Ok(LoadedCredential {
            token: None,
            auth: None,
        });
    };
    let bytes = Zeroizing::new(foundation.vault.reveal(&handle).map_err(|error| {
        Error::new(
            ErrorKind::Provider {
                code: faktor_mcp::remote::AUTH_REQUIRED_CODE.into(),
                retryable: false,
            },
            format!("the stored MCP credential is unavailable; reconnect OAuth: {error}"),
        )
    })?);
    match OAuthTokenSet::from_vault_bytes(bytes.to_vec()) {
        Ok(bundle) => {
            if bundle.server_id != record.id
                || bundle.account.as_deref() != record.auth_account.as_deref()
            {
                return Err(denied(
                    "the stored MCP OAuth credential is bound to a different server/account",
                ));
            }
            if bundle.is_expired(now_ms()) && !bundle.is_renewable() {
                return Err(Error::new(
                    ErrorKind::Provider {
                        code: OAUTH_EXPIRED_CODE.into(),
                        retryable: false,
                    },
                    "the stored MCP OAuth access token expired and cannot be refreshed; log in again",
                ));
            }
            let auth = RemoteVaultAuth::new(Arc::new(foundation.clone()), handle, bundle, client);
            Ok(LoadedCredential {
                token: None,
                auth: Some(Arc::new(auth)),
            })
        }
        Err(_) => {
            if bytes
                .iter()
                .copied()
                .find(|byte| !byte.is_ascii_whitespace())
                == Some(b'{')
            {
                return Err(denied(
                    "the stored MCP OAuth bundle is malformed; reconnect OAuth",
                ));
            }
            let token = SecretToken::from_bytes(bytes.to_vec()).map_err(|error| {
                Error::new(
                    ErrorKind::Provider {
                        code: faktor_mcp::remote::AUTH_REQUIRED_CODE.into(),
                        retryable: false,
                    },
                    format!("the stored MCP credential is unusable: {}", error.message),
                )
            })?;
            Ok(LoadedCredential {
                token: Some(token),
                auth: None,
            })
        }
    }
}

/// Store a freshly minted token bundle. The new material is durably written
/// before the record points at it; only then is the previous generation
/// deleted. A failure leaves the previous binding untouched.
pub(crate) fn store_tokens(
    foundation: &Foundation,
    record: &McpServerRecord,
    tokens: OAuthTokenSet,
) -> Result<McpServerRecord, Error> {
    let bytes = tokens.to_vault_bytes()?;
    let secret = foundation
        .vault
        .store("mcp_oauth_bundle", "MCP OAuth credential", bytes.as_slice())
        .map_err(|error| {
            Error::internal(format!(
                "the MCP OAuth credential could not be stored: {error}"
            ))
        })?;
    let previous = record.auth_token_ref.clone();
    match foundation.customization.set_mcp_auth(
        &record.id,
        Some(secret.handle.clone()),
        tokens.account.clone(),
        tokens.scopes.clone(),
        Some(record.revision),
    ) {
        Ok(record) => {
            if let Some(previous) = previous {
                if previous != secret.handle {
                    let _ = foundation.vault.delete(&previous);
                }
            }
            Ok(record)
        }
        Err(error) => {
            let _ = foundation.vault.delete(&secret.handle);
            let detail =
                format!("the MCP OAuth credential binding could not be persisted: {error}");
            Err(match error {
                faktor_foundation::FoundationError::Conflict(_) => Error::conflict(detail),
                _ => Error::internal(detail),
            })
        }
    }
}

/// The non-secret auth view: table state plus decoded OAuth facts. Never
/// renders a token, refresh token or client secret.
pub(crate) fn auth_status(foundation: &Foundation, record: &McpServerRecord) -> Value {
    let mut view = json!({
        "serverId": record.id,
        "transport": record.transport,
        "state": record.auth_state,
        "account": record.auth_account,
        "scopes": record.auth_scopes,
        "hasToken": record.auth_token_ref.is_some(),
        "method": if record.auth_token_ref.is_some() { "credential" } else { "none" },
        "revision": record.revision,
    });
    let Some(handle) = record.auth_token_ref.as_deref() else {
        return view;
    };
    let Ok(bytes) = foundation.vault.reveal(handle) else {
        return view;
    };
    let Ok(bundle) = OAuthTokenSet::from_vault_bytes(bytes) else {
        return view;
    };
    if bundle.server_id != record.id || bundle.account.as_deref() != record.auth_account.as_deref()
    {
        view["state"] = json!(AUTH_DENIED);
        view["binding"] = json!("mismatch");
        return view;
    }
    let expired = bundle.is_expired(now_ms());
    let state = if expired {
        AUTH_EXPIRED
    } else {
        record.auth_state.as_str()
    };
    view["state"] = json!(state);
    view["method"] = json!("oauth");
    view["issuer"] = json!(bundle.issuer);
    view["account"] = json!(bundle.account);
    view["scopes"] = json!(bundle.scopes);
    view["expiresAtMs"] = json!(bundle.expires_at_ms);
    view["hasRefresh"] = json!(bundle.has_refresh());
    view["renewable"] = json!(bundle.is_renewable());
    view["resource"] = json!(bundle.resource);
    view
}

/// One pending authorization-code flow. The verifier/state live inside
/// [`PendingAuthorization`] (secret-wrapped); this wrapper only adds the
/// server binding and the single-use lookup identity.
pub(crate) struct PendingLogin {
    pub pending: PendingAuthorization,
    pub server_id: String,
    pub revision: u64,
    pub expires_at_ms: i64,
}

/// Validate the operator-supplied client id/secret shapes before any
/// discovery request.
fn client_credentials(
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<ClientCredentials, Error> {
    let Some(client_id) = client_id else {
        return Err(Error::new(
            ErrorKind::Provider {
                code: OAUTH_UNCONFIGURED_CODE.into(),
                retryable: false,
            },
            "no OAuth client id was supplied and dynamic registration is unavailable",
        ));
    };
    if client_id.is_empty()
        || client_id.len() > MAX_CLIENT_ID_CHARS
        || client_id.chars().any(char::is_control)
    {
        return Err(Error::malformed(
            "the OAuth client id must be 1..=512 non-control characters",
        ));
    }
    let secret = match client_secret {
        Some(secret) => {
            if secret.is_empty()
                || secret.len() > MAX_SECRET_CHARS
                || secret.chars().any(char::is_control)
            {
                return Err(Error::malformed(
                    "the OAuth client secret must be 1..=8192 non-control characters",
                ));
            }
            Some(
                SecretToken::from_bytes(secret.as_bytes().to_vec()).map_err(|_| {
                    Error::malformed("the OAuth client secret is not a valid credential")
                })?,
            )
        }
        None => None,
    };
    Ok(ClientCredentials {
        client_id: client_id.to_string(),
        client_secret: secret,
    })
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

fn random_flow_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let mut id = String::with_capacity(32);
    for byte in bytes {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// Start an operator login. `authorization_code` returns the authorization
/// URL plus a single-use flow id; the operator's loopback listener (CLI/IDE)
/// captures the callback and posts it to [`complete_login`].
/// `client_credentials` performs the exchange immediately (no browser).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn begin_login(
    runtime: &McpRuntime,
    foundation: &Foundation,
    record: &McpServerRecord,
    mode: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    redirect_uri: Option<&str>,
    daemon_host: Option<&str>,
    scopes: Vec<String>,
    account: Option<String>,
) -> Result<Value, Error> {
    if record.transport != "streamable_http" {
        return Err(Error::new(
            ErrorKind::Conflict,
            "OAuth binding applies to streamable_http servers only",
        ));
    }
    let endpoint = record
        .endpoint
        .as_deref()
        .ok_or_else(|| Error::malformed("this MCP server has no endpoint"))?;
    if !matches!(
        mode,
        "client_credentials" | "auth_code" | "authorization_code" | "connect"
    ) {
        return Err(Error::malformed(format!(
            "unsupported OAuth login mode {mode:?}; use authorization_code or client_credentials"
        )));
    }
    if mode == "client_credentials" && client_id.is_none() {
        return Err(Error::new(
            ErrorKind::Provider {
                code: OAUTH_UNCONFIGURED_CODE.into(),
                retryable: false,
            },
            "no OAuth client id was supplied for the client_credentials grant",
        ));
    }
    let oauth = runtime.oauth_client();
    let challenge = oauth.probe_challenge(endpoint).await.ok().flatten();
    let discovery = oauth.discover(endpoint, challenge.as_ref()).await?;
    if scopes.iter().any(|scope| scope.is_empty()) {
        return Err(Error::malformed("an OAuth scope must not be empty"));
    }
    let flow_id = random_flow_id();
    match mode {
        "client_credentials" => {
            if redirect_uri.is_some() {
                return Err(Error::malformed(
                    "client_credentials does not use a redirect URI",
                ));
            }
            let credentials = client_credentials(client_id, client_secret)?;
            let tokens = oauth
                .client_credentials(
                    &discovery,
                    &credentials,
                    non_empty(scopes),
                    &record.id,
                    account,
                )
                .await?;
            let record = store_tokens(foundation, record, tokens)?;
            let _ = runtime.stop(&record.id).await;
            Ok(json!({
                "state": "authorized",
                "mode": "client_credentials",
                "auth": auth_status(foundation, &record),
                "connected": false,
                "note": "credential authorized; connect opens the remote session",
            }))
        }
        "auth_code" | "authorization_code" | "connect" => {
            let redirect_uri = match redirect_uri {
                Some(redirect_uri) => redirect_uri.to_string(),
                None => {
                    daemon_callback_uri(daemon_host, &record.id, &flow_id)?.ok_or_else(|| {
                        Error::malformed(
                        "the authorization-code flow requires the operator's loopback redirect URI",
                    )
                    })?
                }
            };
            let credentials = match client_id {
                Some(_) => client_credentials(client_id, client_secret)?,
                None => {
                    let scopes = if scopes.is_empty() {
                        discovery.scopes.clone()
                    } else {
                        scopes.clone()
                    };
                    oauth
                        .register_client(&discovery, &redirect_uri, &scopes)
                        .await?
                }
            };
            let pending = oauth.begin_authorization(
                &discovery,
                &credentials,
                &redirect_uri,
                non_empty(scopes),
                &record.id,
                account,
            )?;
            let expires_at_ms = pending.expires_at_ms();
            let authorization_url = pending.authorization_url().to_string();
            let scopes = pending.scopes().to_vec();
            let issuer = pending.issuer().to_string();
            {
                let mut flows = runtime.oauth_flows.lock().await;
                flows.retain(|_, flow| flow.expires_at_ms > now_ms());
                if flows.len() >= MAX_PENDING_FLOWS {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "too many pending MCP OAuth flows; finish or retry the existing login",
                    ));
                }
                flows.insert(
                    flow_id.clone(),
                    PendingLogin {
                        pending,
                        server_id: record.id.clone(),
                        revision: record.revision,
                        expires_at_ms,
                    },
                );
            }
            let note = if redirect_uri.contains("/oauth/callback/") {
                "open the authorization URL; the daemon loopback callback stores the credential, then poll /oauth/status"
            } else {
                "open the authorization URL and post the loopback callback to /oauth/complete"
            };
            Ok(json!({
                "state": "pending",
                "mode": "authorization_code",
                "flowId": flow_id,
                "authorizationUrl": authorization_url,
                "redirectUri": redirect_uri,
                "expiresAtMs": expires_at_ms,
                "scopes": scopes,
                "issuer": issuer,
                "serverId": record.id,
                "note": note,
            }))
        }
        other => Err(Error::malformed(format!(
            "unsupported OAuth login mode {other:?}; use authorization_code or client_credentials"
        ))),
    }
}

/// Build the daemon-owned loopback callback for a UI-started login. The
/// native listener's `Host` header is the only source of the port; a
/// non-loopback host, a missing explicit port or a malformed authority is a
/// typed refusal (never an attacker-chosen redirect target).
fn daemon_callback_uri(
    host: Option<&str>,
    server_id: &str,
    flow_id: &str,
) -> Result<Option<String>, Error> {
    let Some(host) = host else {
        return Ok(None);
    };
    let authority = reqwest::Url::parse(&format!("http://{host}/")).map_err(|_| {
        Error::malformed("the daemon Host header is not a valid loopback authority")
    })?;
    if authority.port().is_none() {
        return Err(Error::malformed(
            "the daemon Host header must carry an explicit loopback port",
        ));
    }
    let host_name = authority
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host_name != "127.0.0.1" && host_name != "localhost" && host_name != "::1" {
        return Err(Error::malformed(
            "the daemon OAuth callback must use a loopback host",
        ));
    }
    let origin = authority.origin().ascii_serialization();
    Ok(Some(format!(
        "{origin}/native/mcp/servers/{server_id}/oauth/callback/{flow_id}"
    )))
}

fn non_empty(scopes: Vec<String>) -> Option<Vec<String>> {
    if scopes.is_empty() {
        None
    } else {
        Some(scopes)
    }
}

/// Complete a pending authorization-code flow exactly once.
pub(crate) async fn complete_login(
    runtime: &McpRuntime,
    foundation: &Foundation,
    record: &McpServerRecord,
    flow_id: &str,
    code: &str,
    state: &str,
) -> Result<Value, Error> {
    if flow_id.is_empty() || flow_id.len() > 128 {
        return Err(denied("the OAuth flow id is malformed"));
    }
    if code.is_empty() || code.len() > 8_192 {
        return Err(denied("the authorization callback carried no usable code"));
    }
    if state.is_empty() || state.len() > 8_192 {
        return Err(denied("the authorization callback carried no usable state"));
    }
    let flow = {
        let mut flows = runtime.oauth_flows.lock().await;
        flows.remove(flow_id)
    };
    let Some(flow) = flow else {
        return Err(denied(
            "unknown, expired or already-used OAuth flow (replay refused)",
        ));
    };
    if flow.server_id != record.id {
        return Err(denied("the OAuth flow belongs to a different MCP server"));
    }
    if flow.revision != record.revision {
        return Err(denied(
            "the MCP server changed while the OAuth flow was pending (logout/update); start the login again",
        ));
    }
    if flow.expires_at_ms <= now_ms() {
        return Err(Error::new(
            ErrorKind::Provider {
                code: OAUTH_EXPIRED_CODE.into(),
                retryable: false,
            },
            "the OAuth authorization request expired; start the login again",
        ));
    }
    let tokens = flow.pending.complete_with_params(code, state).await?;
    let record = store_tokens(foundation, record, tokens)?;
    let _ = runtime.stop(&record.id).await;
    Ok(json!({
        "state": "authorized",
        "mode": "authorization_code",
        "auth": auth_status(foundation, &record),
        "connected": false,
        "note": "credential authorized; connect opens the remote session",
    }))
}

/// Disconnect: revoke remote material (best-effort, reported honestly),
/// delete the local vault generation, close any live session and return the
/// durable row to `signed_out`.
pub(crate) async fn logout(
    runtime: &McpRuntime,
    foundation: &Foundation,
    record: &McpServerRecord,
) -> Result<Value, Error> {
    let mut remote_revoked = false;
    let mut remote_detail: Option<String> = None;
    {
        // A disconnect cancels every browser flow for this server, including
        // one started from another surface; the revision guard protects
        // against a CLI logout that cannot reach this in-memory map.
        let mut flows = runtime.oauth_flows.lock().await;
        flows.retain(|_, flow| flow.server_id != record.id);
    }
    if let Some(handle) = record.auth_token_ref.as_deref() {
        if let Ok(bytes) = foundation.vault.reveal(handle) {
            if let Ok(bundle) = OAuthTokenSet::from_vault_bytes(bytes) {
                match runtime.oauth_client().revoke(&bundle).await {
                    Ok(outcome) => {
                        remote_revoked = outcome.remote;
                        remote_detail = outcome.detail;
                    }
                    Err(error) => {
                        remote_detail = Some(sanitize(&error.message));
                    }
                }
            } else {
                remote_detail = Some(
                    "the stored credential is a raw bearer token; this client cannot revoke it remotely"
                        .into(),
                );
            }
        }
    }
    if let Some(handle) = record.auth_token_ref.clone() {
        foundation.vault.delete(&handle).map_err(|error| {
            Error::internal(format!(
                "the local MCP OAuth credential could not be deleted: {error}"
            ))
        })?;
    }
    let _ = runtime.stop(&record.id).await;
    let record = foundation
        .customization
        .set_mcp_auth(&record.id, None, None, Vec::new(), None)
        .map_err(|error| Error::internal(format!("MCP auth clear failed: {error}")))?;
    Ok(json!({
        "state": record.auth_state,
        "revoked": true,
        "remoteRevoked": remote_revoked,
        "remoteDetail": remote_detail,
        "auth": auth_status(foundation, &record),
    }))
}

fn sanitize(message: &str) -> String {
    message
        .chars()
        .filter(|ch| !ch.is_control())
        .take(512)
        .collect()
}

#[cfg(test)]
#[path = "mcp_oauth_tests.rs"]
mod mcp_oauth_tests;
