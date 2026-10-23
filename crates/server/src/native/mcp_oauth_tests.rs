//! Adversarial vault/provider tests for remote MCP OAuth credentials. No
//! network is used: a synthetic bundle exercises the exact durable binding,
//! expiry and redaction paths the daemon uses at connect time.

use faktor_foundation::customization::{McpServerDraft, McpServerRecord};
use faktor_foundation::Foundation;
use faktor_mcp::oauth::{OAuthClient, OAuthTokenSet, OAUTH_DENIED_CODE, OAUTH_EXPIRED_CODE};
use faktor_provider::egress::EgressAddressPolicy;

use super::{auth_status, daemon_callback_uri, load_credential, now_ms, store_tokens};

fn foundation() -> (tempfile::TempDir, Foundation) {
    let dir = tempfile::tempdir().unwrap();
    let foundation = Foundation::open(dir.path()).unwrap();
    (dir, foundation)
}

fn remote(foundation: &Foundation) -> McpServerRecord {
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
        .unwrap()
}

fn bundle(expires_at_ms: Option<i64>, refresh: Option<&str>) -> OAuthTokenSet {
    OAuthTokenSet::for_test(
        "remote",
        Some("operator"),
        "access-secret-value",
        refresh,
        expires_at_ms,
        vec!["mcp.read".into()],
        "https://auth.example.com/token",
        Some("https://auth.example.com/revoke"),
    )
}

fn local_client() -> OAuthClient {
    OAuthClient::with_address_policy(EgressAddressPolicy::LOCAL)
}

fn code(error: &faktor_core::error::Error) -> &str {
    match &error.kind {
        faktor_core::error::ErrorKind::Provider { code, .. } => code.as_str(),
        other => panic!("expected typed provider error, got {other:?}"),
    }
}

#[test]
fn oauth_bundle_is_vault_keyed_to_server_and_account_and_never_rendered() {
    let (_dir, foundation) = foundation();
    let record = remote(&foundation);
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 3_600_000;
    let record = store_tokens(
        &foundation,
        &record,
        bundle(Some(expires), Some("refresh-secret-value")),
    )
    .expect("store OAuth bundle");
    assert_eq!(record.auth_state, "authorized");
    assert_eq!(record.auth_account.as_deref(), Some("operator"));
    assert_eq!(record.auth_scopes, vec!["mcp.read".to_string()]);
    let handle = record.auth_token_ref.clone().expect("vault handle");
    assert!(handle.starts_with("vault://mcp_oauth_bundle/"));
    let material = foundation.vault.reveal(&handle).unwrap();
    let text = String::from_utf8_lossy(&material);
    assert!(text.contains("access-secret-value"));
    assert!(!foundation.vault.list().unwrap().iter().any(|secret| {
        secret.label.contains("access-secret-value")
            || secret.label.contains("refresh-secret-value")
    }));

    let loaded = load_credential(&foundation, &record, local_client()).expect("load bundle");
    assert!(loaded.token.is_none());
    assert!(loaded.auth.is_some());

    let status = auth_status(&foundation, &record);
    let rendered = status.to_string();
    assert_eq!(status["method"], "oauth");
    assert_eq!(status["account"], "operator");
    assert_eq!(status["scopes"][0], "mcp.read");
    assert_eq!(status["hasRefresh"], true);
    assert!(!rendered.contains("access-secret-value"));
    assert!(!rendered.contains("refresh-secret-value"));
    assert!(!rendered.contains("client-secret"));
}

#[tokio::test]
async fn vault_provider_returns_the_current_token_and_forced_refresh_without_one_is_expired() {
    let (_dir, foundation) = foundation();
    let record = remote(&foundation);
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 3_600_000;
    let record = store_tokens(&foundation, &record, bundle(Some(expires), None)).unwrap();
    let loaded = load_credential(&foundation, &record, local_client()).unwrap();
    let auth = loaded.auth.expect("OAuth provider");
    let token = auth.token().await.expect("unexpired token");
    assert_eq!(format!("{token:?}"), "SecretToken(<redacted>)");
    let scrubbed = auth.scrub("server echoed access-secret-value");
    assert_eq!(scrubbed, "server echoed [redacted]");
    let error = auth
        .refresh()
        .await
        .expect_err("no refresh token means the forced refresh is expired");
    assert_eq!(code(&error), OAUTH_EXPIRED_CODE, "{error:?}");
    let persisted = foundation
        .customization
        .mcp_server(&record.id)
        .unwrap()
        .expect("persisted record");
    assert_eq!(persisted.auth_state, "expired");
    let status = auth_status(&foundation, &persisted);
    assert_eq!(status["state"], "expired");
}

#[test]
fn swapped_server_or_account_binding_is_refused() {
    let (_dir, foundation) = foundation();
    let record = remote(&foundation);
    let record = store_tokens(&foundation, &record, bundle(None, Some("refresh"))).unwrap();

    let mut swapped_account = record.clone();
    swapped_account.auth_account = Some("someone-else".into());
    let error = load_credential(&foundation, &swapped_account, local_client()).unwrap_err();
    assert_eq!(code(&error), OAUTH_DENIED_CODE, "{error:?}");

    let mut swapped_server = record.clone();
    swapped_server.id = "other".into();
    let error = load_credential(&foundation, &swapped_server, local_client()).unwrap_err();
    assert_eq!(code(&error), OAUTH_DENIED_CODE, "{error:?}");
}

#[test]
fn expired_bundle_without_refresh_is_typed_expired_at_connect() {
    let (_dir, foundation) = foundation();
    let record = remote(&foundation);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let record = store_tokens(&foundation, &record, bundle(Some(now - 60_000), None)).unwrap();
    let error = load_credential(&foundation, &record, local_client()).unwrap_err();
    assert_eq!(code(&error), OAUTH_EXPIRED_CODE, "{error:?}");
    let status = auth_status(&foundation, &record);
    assert_eq!(status["state"], "expired");
    assert_eq!(status["hasRefresh"], false);
}

#[test]
fn a_stale_login_cannot_overwrite_a_newer_binding() {
    let (_dir, foundation) = foundation();
    let record = remote(&foundation);
    let stale = record.clone();
    let first = store_tokens(
        &foundation,
        &record,
        bundle(Some(now_ms() + 3_600_000), Some("first-refresh")),
    )
    .expect("first binding");
    let error = store_tokens(
        &foundation,
        &stale,
        bundle(Some(now_ms() + 3_600_000), Some("second-refresh")),
    )
    .expect_err("a stale revision must not overwrite a newer binding");
    assert!(error.message.contains("revision changed"), "{error:?}");
    let current = foundation
        .customization
        .mcp_server(&record.id)
        .unwrap()
        .expect("persisted record");
    assert_eq!(current.auth_token_ref, first.auth_token_ref);
    assert_eq!(
        foundation
            .vault
            .list()
            .unwrap()
            .iter()
            .filter(|secret| secret.verification != faktor_foundation::VerificationState::Revoked)
            .count(),
        1,
        "the losing login's vault material is deleted, not orphaned"
    );
}

#[test]
fn malformed_bundle_is_never_treated_as_a_raw_bearer() {
    let (_dir, foundation) = foundation();
    let record = remote(&foundation);
    let secret = foundation
        .vault
        .store(
            "mcp_oauth_bundle",
            "MCP OAuth credential",
            b"{\"version\":1}",
        )
        .unwrap();
    let record = foundation
        .customization
        .set_mcp_auth(
            &record.id,
            Some(secret.handle),
            Some("operator".into()),
            vec!["mcp.read".into()],
            None,
        )
        .unwrap();
    let error = load_credential(&foundation, &record, local_client()).unwrap_err();
    assert_eq!(code(&error), OAUTH_DENIED_CODE, "{error:?}");
}

#[test]
fn legacy_raw_bearer_still_loads_and_is_redacted_in_status() {
    let (_dir, foundation) = foundation();
    let record = remote(&foundation);
    let secret = foundation
        .vault
        .store(
            "mcp_oauth",
            "MCP OAuth token for remote",
            b"sk-legacy-secret",
        )
        .unwrap();
    let record = foundation
        .customization
        .set_mcp_auth(
            &record.id,
            Some(secret.handle),
            Some("operator".into()),
            vec![],
            None,
        )
        .unwrap();
    let loaded = load_credential(&foundation, &record, local_client()).unwrap();
    assert!(loaded.auth.is_none());
    assert!(loaded.token.is_some());
    let rendered = auth_status(&foundation, &record).to_string();
    assert_eq!(auth_status(&foundation, &record)["method"], "credential");
    assert!(!rendered.contains("sk-legacy-secret"));
}

#[test]
fn daemon_callback_is_loopback_only_and_never_attacker_chosen() {
    let callback = daemon_callback_uri(Some("127.0.0.1:43123"), "remote", "flow123")
        .unwrap()
        .expect("loopback callback");
    assert_eq!(
        callback,
        "http://127.0.0.1:43123/native/mcp/servers/remote/oauth/callback/flow123"
    );
    assert!(
        daemon_callback_uri(Some("localhost:43123"), "remote", "flow123")
            .unwrap()
            .is_some()
    );
    assert!(daemon_callback_uri(Some("evil.example.com:43123"), "remote", "flow123").is_err());
    assert!(daemon_callback_uri(Some("127.0.0.1"), "remote", "flow123").is_err());
    assert!(daemon_callback_uri(
        Some("127.0.0.1:43123@evil.example.com"),
        "remote",
        "flow123"
    )
    .is_err());
}
