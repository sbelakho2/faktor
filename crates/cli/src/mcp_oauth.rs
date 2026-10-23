//! Operator-facing remote MCP OAuth login/status/logout.
//!
//! `login` runs authorization-code + PKCE with a loopback listener owned by
//! THIS process, posts the callback to the OAuth engine, then writes only a
//! vault bundle to the foundation database. `status` renders the non-secret
//! lifecycle view. `logout` attempts RFC 7009 revocation and always deletes
//! the local vault generation. No command ever prints a token, refresh token
//! or client secret.

use std::io::Read;
use std::time::Duration;

use clap::Subcommand;
use faktor_core::error::Error;
use faktor_foundation::customization::McpServerRecord;
use faktor_foundation::Foundation;
use faktor_mcp::oauth::{ClientCredentials, OAuthClient, OAuthTokenSet, AUTH_DENIED, AUTH_EXPIRED};
use faktor_mcp::remote::SecretToken;
use faktor_provider::egress::EgressAddressPolicy;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Subcommand)]
pub(crate) enum McpAction {
    /// Authorization-code + PKCE (default) or client-credentials login.
    #[command(display_order = 10)]
    Login {
        /// The saved remote MCP server id.
        #[arg(long)]
        server: String,
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
        /// Pre-registered OAuth client id. Omitted: RFC 7591 dynamic
        /// registration is attempted when the AS advertises it.
        #[arg(long)]
        client_id: Option<String>,
        /// Read the client secret from stdin (never an argv value).
        #[arg(long)]
        client_secret_stdin: bool,
        #[arg(long, default_value = "authorization_code")]
        mode: String,
        /// Repeatable negotiated capability scope.
        #[arg(long = "scope")]
        scopes: Vec<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
        /// Test seam: allow a loopback authorization/MCP fixture.
        #[arg(long, hide = true)]
        allow_loopback: bool,
    },
    /// Print the durable auth state, account, scopes and expiry (no secrets).
    #[command(display_order = 20)]
    Status {
        #[arg(long)]
        server: String,
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Revoke remotely where possible, delete the local vault material and
    /// return the server to signed_out.
    #[command(display_order = 30)]
    Logout {
        #[arg(long)]
        server: String,
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
        /// Test seam: allow a loopback authorization-server fixture.
        #[arg(long, hide = true)]
        allow_loopback: bool,
    },
}

pub(crate) async fn command(action: McpAction) -> Result<(), String> {
    match action {
        McpAction::Login {
            server,
            data_dir,
            client_id,
            client_secret_stdin,
            mode,
            scopes,
            account,
            timeout_secs,
            allow_loopback,
        } => {
            login(
                &expand(&data_dir),
                &server,
                client_id,
                client_secret_stdin,
                &mode,
                scopes,
                account,
                timeout_secs,
                allow_loopback,
            )
            .await
        }
        McpAction::Status { server, data_dir } => {
            status(&expand(&data_dir), &server).map(|value| print_json(&value))
        }
        McpAction::Logout {
            server,
            data_dir,
            allow_loopback,
        } => logout(&expand(&data_dir), &server, allow_loopback)
            .await
            .map(|value| print_json(&value)),
    }
}

fn expand(path: &str) -> std::path::PathBuf {
    if path == "~" {
        return std::env::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return std::env::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| std::path::PathBuf::from(path));
    }
    std::path::PathBuf::from(path)
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

fn open(data_dir: &std::path::Path) -> Result<Foundation, String> {
    Foundation::open(data_dir).map_err(|error| format!("open foundation: {error}"))
}

fn record(foundation: &Foundation, server: &str) -> Result<McpServerRecord, String> {
    let record = foundation
        .customization
        .mcp_server(server)
        .map_err(|error| format!("read MCP server: {error}"))?
        .ok_or_else(|| format!("MCP server {server:?} is not saved"))?;
    if record.transport != "streamable_http" {
        return Err(format!(
            "MCP server {server:?} is a stdio server; OAuth applies to streamable_http only"
        ));
    }
    Ok(record)
}

fn oauth_client(allow_loopback: bool) -> OAuthClient {
    OAuthClient::with_address_policy(if allow_loopback {
        EgressAddressPolicy::LOCAL
    } else {
        EgressAddressPolicy::EXTERNAL
    })
}

fn read_secret_stdin() -> Result<String, String> {
    let mut value = String::new();
    std::io::stdin()
        .take(8_193)
        .read_to_string(&mut value)
        .map_err(|error| format!("read client secret from stdin: {error}"))?;
    let value = value.trim_end_matches(['\r', '\n']).to_string();
    if value.is_empty() || value.len() > 8_192 {
        return Err("the client secret from stdin must be 1..=8192 bytes".into());
    }
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
async fn login(
    data_dir: &std::path::Path,
    server: &str,
    client_id: Option<String>,
    client_secret_stdin: bool,
    mode: &str,
    scopes: Vec<String>,
    account: Option<String>,
    timeout_secs: u64,
    allow_loopback: bool,
) -> Result<(), String> {
    let foundation = open(data_dir)?;
    let record = record(&foundation, server)?;
    let endpoint = record
        .endpoint
        .as_deref()
        .ok_or_else(|| "the MCP server has no endpoint".to_string())?;
    if mode != "authorization_code" && mode != "auth_code" && mode != "client_credentials" {
        return Err(format!(
            "unsupported --mode {mode:?}; use authorization_code or client_credentials"
        ));
    }
    if mode == "client_credentials" && client_id.is_none() {
        return Err(
            "client_credentials requires --client-id (and usually --client-secret-stdin)"
                .to_string(),
        );
    }
    let oauth = oauth_client(allow_loopback);
    let challenge = oauth.probe_challenge(endpoint).await.ok().flatten();
    let discovery = oauth
        .discover(endpoint, challenge.as_ref())
        .await
        .map_err(display_error)?;
    let client_secret = if client_secret_stdin {
        Some(read_secret_stdin()?)
    } else {
        None
    };
    if mode == "client_credentials" {
        let client_id = client_id.ok_or_else(|| {
            "client_credentials requires --client-id (and usually --client-secret-stdin)"
                .to_string()
        })?;
        let credentials = credentials(&client_id, client_secret)?;
        let tokens = oauth
            .client_credentials(
                &discovery,
                &credentials,
                non_empty(scopes),
                &record.id,
                account,
            )
            .await
            .map_err(display_error)?;
        let record = store(&foundation, &record, tokens)?;
        print_json(&status_view(&foundation, &record));
        return Ok(());
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("bind the OAuth loopback listener: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configure the OAuth loopback listener: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read the OAuth loopback address: {error}"))?;
    let redirect_uri = format!("http://127.0.0.1:{}/callback", address.port());
    let credentials = match client_id {
        Some(client_id) => credentials(&client_id, client_secret)?,
        None => {
            let requested = if scopes.is_empty() {
                discovery.scopes.clone()
            } else {
                scopes.clone()
            };
            oauth
                .register_client(&discovery, &redirect_uri, &requested)
                .await
                .map_err(display_error)?
        }
    };
    let pending = oauth
        .begin_authorization(
            &discovery,
            &credentials,
            &redirect_uri,
            non_empty(scopes),
            &record.id,
            account,
        )
        .map_err(display_error)?;
    println!(
        "Open this URL to authorize the MCP server:\n{}\nWaiting for the loopback callback on {redirect_uri} ...",
        pending.authorization_url()
    );
    let listener = TcpListener::from_std(listener)
        .map_err(|error| format!("start the OAuth loopback listener: {error}"))?;
    let (code, state) = wait_for_callback(listener, timeout_secs).await?;
    let tokens = pending
        .complete_with_params(&code, &state)
        .await
        .map_err(display_error)?;
    let record = store(&foundation, &record, tokens)?;
    print_json(&status_view(&foundation, &record));
    Ok(())
}

fn credentials(
    client_id: &str,
    client_secret: Option<String>,
) -> Result<ClientCredentials, String> {
    if client_id.is_empty() || client_id.len() > 512 || client_id.chars().any(char::is_control) {
        return Err("the client id must be 1..=512 non-control characters".into());
    }
    let client_secret = client_secret
        .map(|secret| {
            SecretToken::from_bytes(secret.into_bytes())
                .map_err(|_| "the client secret is not a valid credential".to_string())
        })
        .transpose()?;
    Ok(ClientCredentials {
        client_id: client_id.to_string(),
        client_secret,
    })
}

fn non_empty(scopes: Vec<String>) -> Option<Vec<String>> {
    if scopes.is_empty() {
        None
    } else {
        Some(scopes)
    }
}

fn store(
    foundation: &Foundation,
    record: &McpServerRecord,
    tokens: OAuthTokenSet,
) -> Result<McpServerRecord, String> {
    let bytes = tokens.to_vault_bytes().map_err(display_error)?;
    let secret = foundation
        .vault
        .store("mcp_oauth_bundle", "MCP OAuth credential", bytes.as_slice())
        .map_err(|error| format!("store the OAuth credential: {error}"))?;
    let previous = record.auth_token_ref.clone();
    let updated = match foundation.customization.set_mcp_auth(
        &record.id,
        Some(secret.handle.clone()),
        tokens.account.clone(),
        tokens.scopes.clone(),
        Some(record.revision),
    ) {
        Ok(updated) => updated,
        Err(error) => {
            let _ = foundation.vault.delete(&secret.handle);
            return Err(format!("persist the OAuth binding: {error}"));
        }
    };
    if let Some(previous) = previous {
        if previous != secret.handle {
            let _ = foundation.vault.delete(&previous);
        }
    }
    Ok(updated)
}

fn status(data_dir: &std::path::Path, server: &str) -> Result<Value, String> {
    let foundation = open(data_dir)?;
    let record = record(&foundation, server)?;
    Ok(status_view(&foundation, &record))
}

fn status_view(foundation: &Foundation, record: &McpServerRecord) -> Value {
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    view["state"] = json!(if bundle.is_expired(now) {
        AUTH_EXPIRED
    } else {
        record.auth_state.as_str()
    });
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

async fn logout(
    data_dir: &std::path::Path,
    server: &str,
    allow_loopback: bool,
) -> Result<Value, String> {
    let foundation = open(data_dir)?;
    let record = record(&foundation, server)?;
    let mut remote_revoked = false;
    let mut remote_detail: Option<String> = None;
    if let Some(handle) = record.auth_token_ref.as_deref() {
        if let Ok(bytes) = foundation.vault.reveal(handle) {
            if let Ok(bundle) = OAuthTokenSet::from_vault_bytes(bytes) {
                match oauth_client(allow_loopback).revoke(&bundle).await {
                    Ok(outcome) => {
                        remote_revoked = outcome.remote;
                        remote_detail = outcome.detail;
                    }
                    Err(error) => remote_detail = Some(display_error(error)),
                }
            } else {
                remote_detail = Some(
                    "the stored credential is a raw bearer; it cannot be revoked remotely".into(),
                );
            }
        }
    }
    if let Some(handle) = record.auth_token_ref.clone() {
        foundation
            .vault
            .delete(&handle)
            .map_err(|error| format!("delete the local OAuth credential: {error}"))?;
    }
    let record = foundation
        .customization
        .set_mcp_auth(&record.id, None, None, Vec::new(), None)
        .map_err(|error| format!("clear the OAuth binding: {error}"))?;
    Ok(json!({
        "serverId": record.id,
        "state": record.auth_state,
        "revoked": true,
        "remoteRevoked": remote_revoked,
        "remoteDetail": remote_detail,
    }))
}

async fn wait_for_callback(
    listener: TcpListener,
    timeout_secs: u64,
) -> Result<(String, String), String> {
    let timeout = Duration::from_secs(timeout_secs.clamp(10, 900));
    let accept = async {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return Err("the OAuth loopback listener failed".to_string());
            };
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 2048];
            let header_end = loop {
                let read = stream
                    .read(&mut chunk)
                    .await
                    .map_err(|error| format!("read the OAuth callback: {error}"))?;
                if read == 0 {
                    break None;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    break Some(index + 4);
                }
                if buffer.len() > 64 * 1024 {
                    break None;
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..header_end]);
            let Some(target) = head.lines().next().and_then(|line| line.split(' ').nth(1)) else {
                continue;
            };
            if !target.starts_with("/callback") {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                continue;
            }
            let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
            let params = query_map(query);
            let status = match (params.get("code"), params.get("state")) {
                (Some(code), Some(state)) => {
                    // The state is verified by the pending flow before the
                    // code is exchanged; an incorrect state is a typed denial.
                    let body = b"authorization received; you can close this window";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.flush().await;
                    return Ok((code.clone(), state.clone()));
                }
                _ => {
                    if let Some(error) = params.get("error") {
                        return Err(format!(
                            "the authorization server refused the login: {}",
                            sanitize(error)
                        ));
                    }
                    // A stray loopback request (a browser probe, favicon,
                    // ...) must not abort the operator's pending login.
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                }
            };
            let _ = stream.write_all(status.as_bytes()).await;
            let _ = stream.flush().await;
        }
    };
    tokio::time::timeout(timeout, accept)
        .await
        .map_err(|_| "timed out waiting for the OAuth loopback callback".to_string())?
}

fn query_map(raw: &str) -> std::collections::HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(name), decode(value))
        })
        .collect()
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sanitize(message: &str) -> String {
    message
        .chars()
        .filter(|ch| !ch.is_control())
        .take(512)
        .collect()
}

fn display_error(error: Error) -> String {
    match &error.kind {
        faktor_core::error::ErrorKind::Provider { code, .. } => {
            format!("{code}: {}", error.message)
        }
        _ => error.message,
    }
}

#[cfg(test)]
mod tests {
    use faktor_foundation::customization::McpServerDraft;

    use super::*;

    fn basis() -> (tempfile::TempDir, Foundation) {
        let dir = tempfile::tempdir().unwrap();
        let foundation = Foundation::open(dir.path()).unwrap();
        foundation
            .customization
            .save_mcp_server(
                &McpServerDraft {
                    id: Some("remote".into()),
                    name: "Remote".into(),
                    command: String::new(),
                    args: vec![],
                    env: vec![],
                    trust_origin: None,
                    trusted: Some(true),
                    enabled: Some(true),
                    transport: Some("streamable_http".into()),
                    endpoint: Some("https://mcp.example.com/rpc".into()),
                    timeout_ms: Some(5_000),
                    output_cap_bytes: Some(1_048_576),
                    stderr_cap_bytes: None,
                },
                None,
            )
            .unwrap();
        (dir, foundation)
    }

    #[test]
    fn status_and_logout_are_non_secret_and_durable() {
        let (dir, foundation) = basis();
        let status = status(dir.path(), "remote").expect("status");
        assert_eq!(status["state"], "signed_out");
        assert_eq!(status["hasToken"], false);

        let secret = foundation
            .vault
            .store("mcp_oauth", "MCP OAuth token for remote", b"sk-cli-secret")
            .unwrap();
        foundation
            .customization
            .set_mcp_auth(
                "remote",
                Some(secret.handle),
                Some("operator".into()),
                vec!["mcp.read".into()],
                None,
            )
            .unwrap();
        let credential_status = super::status(dir.path(), "remote").expect("credential status");
        assert_eq!(credential_status["method"], "credential");
        assert_eq!(credential_status["hasToken"], true);
        assert!(!credential_status.to_string().contains("sk-cli-secret"));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = runtime
            .block_on(logout(dir.path(), "remote", false))
            .expect("logout");
        assert_eq!(out["state"], "signed_out");
        assert_eq!(out["revoked"], true);
        assert_eq!(out["remoteRevoked"], false);
        let signed_out = super::status(dir.path(), "remote").expect("signed-out status");
        assert_eq!(signed_out["hasToken"], false);
        assert!(foundation
            .vault
            .list()
            .unwrap()
            .iter()
            .all(|secret| secret.verification == faktor_foundation::VerificationState::Revoked));
    }

    #[test]
    fn login_refuses_a_stdio_record_before_any_network() {
        let dir = tempfile::tempdir().unwrap();
        let foundation = Foundation::open(dir.path()).unwrap();
        foundation
            .customization
            .save_mcp_server(
                &McpServerDraft {
                    id: Some("local".into()),
                    name: "Local".into(),
                    command: "npx".into(),
                    args: vec![],
                    env: vec![],
                    trust_origin: None,
                    trusted: Some(true),
                    enabled: Some(true),
                    transport: Some("stdio".into()),
                    endpoint: None,
                    timeout_ms: None,
                    output_cap_bytes: None,
                    stderr_cap_bytes: None,
                },
                None,
            )
            .unwrap();
        let error = record(&foundation, "local").unwrap_err();
        assert!(error.contains("stdio"), "{error}");
    }
}
