//! Adversarial tests of the SSO authority construction: the payload
//! contract (missing/world-readable/corrupt client secret), the happy path
//! that mints a real authorization URL through the network adapter, and the
//! confidential-client exchange that posts the staged secret.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use faktor_cloud::{
    AsyncOidcAdapter, Clock, CodeExchangeRequest, NetworkOidcAdapter, NetworkOidcConfig,
    OrganizationId, SsoConfigRef, SystemClock,
};
use faktor_provider::egress::{HttpTransport, PolicyCheckedHttpTransport};

use super::{build_sso_authority, ConfidentialOidcAdapter};
use crate::config::CloudSsoCfg;
use crate::test_http::{MockServer, Reply};

fn transport() -> Arc<dyn HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::with_policy(None))
}

fn discovery_reply(base: &str) -> Reply {
    Reply::json(
        200,
        serde_json::json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "jwks_uri": format!("{base}/jwks"),
            "id_token_signing_alg_values_supported": ["RS256"],
        }),
    )
}

fn staged_secret(root: &Path, name: &str, body: &[u8]) {
    std::fs::create_dir_all(root).unwrap();
    let path = root.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn sso_cfg(issuer: String, secret: Option<&str>) -> CloudSsoCfg {
    CloudSsoCfg {
        enabled: true,
        issuer: Some(issuer),
        client_id: Some("client-1".into()),
        client_secret: secret.map(str::to_string),
        discovery_max_age_ms: None,
        jwks_max_age_ms: None,
        max_jwks_refetches: None,
    }
}

#[tokio::test]
async fn missing_client_secret_refuses_with_the_exact_expected_path() {
    let dir = tempfile::tempdir().unwrap();
    let payload_root = dir.path().join("payloads");
    let cfg = sso_cfg("https://idp.example".into(), Some("idp.secret"));
    let err = build_sso_authority(&cfg, &payload_root, transport()).unwrap_err();
    let expected = payload_root.join("idp.secret");
    assert!(
        err.contains(&expected.display().to_string()),
        "the refusal must name the exact expected path {expected:?}: {err}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn world_readable_client_secret_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let payload_root = dir.path().join("payloads");
    staged_secret(&payload_root, "idp.secret", b"top-secret\n");
    std::fs::set_permissions(
        payload_root.join("idp.secret"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let cfg = sso_cfg("https://idp.example".into(), Some("idp.secret"));
    let err = build_sso_authority(&cfg, &payload_root, transport()).unwrap_err();
    assert!(err.contains("0600"), "{err}");
    assert!(!err.contains("top-secret"), "{err}");
}

#[tokio::test]
async fn corrupt_client_secret_is_refused_typed() {
    let dir = tempfile::tempdir().unwrap();
    let payload_root = dir.path().join("payloads");
    staged_secret(&payload_root, "idp.secret", b"two words\n");
    let cfg = sso_cfg("https://idp.example".into(), Some("idp.secret"));
    let err = build_sso_authority(&cfg, &payload_root, transport()).unwrap_err();
    assert!(
        err.contains("whitespace") || err.contains("corrupt"),
        "{err}"
    );
}

#[tokio::test]
async fn public_client_happy_path_mints_an_authorization_url() {
    let dir = tempfile::tempdir().unwrap();
    let payload_root = dir.path().join("payloads");
    let mock = MockServer::start().await;
    let base = mock.base();
    mock.push(
        "GET",
        "/.well-known/openid-configuration",
        discovery_reply(&base),
    );
    let cfg = sso_cfg(base.clone(), None);
    let authority = build_sso_authority(&cfg, &payload_root, transport()).unwrap();
    let organization = OrganizationId::try_new("org_sso").unwrap();
    let sso_ref = SsoConfigRef {
        issuer: base.clone(),
        client_id: "client-1".into(),
        membership_claim: "groups".into(),
        group_role_map: BTreeMap::new(),
        client_secret_ref: None,
        enabled: true,
    };
    let start = authority
        .start(&organization, &sso_ref, "https://app.example/sso/callback")
        .await
        .unwrap();
    assert!(
        start
            .authorization_url
            .starts_with(&format!("{base}/authorize?")),
        "{}",
        start.authorization_url
    );
    assert!(start
        .authorization_url
        .contains("code_challenge_method=S256"));
    assert!(start
        .authorization_url
        .contains(&format!("state={}", start.state)));
    assert_eq!(start.state.len(), 64, "32 random bytes hex-encoded");
    assert_eq!(start.nonce.len(), 64);
    assert_eq!(
        mock.requests("GET", "/.well-known/openid-configuration")
            .len(),
        1,
        "discovery resolved exactly once"
    );
    // No payload directory was ever required for the public-client path.
    assert!(!payload_root.exists());
}

#[tokio::test]
async fn confidential_client_posts_the_staged_secret_and_never_logs_it() {
    let dir = tempfile::tempdir().unwrap();
    let payload_root = dir.path().join("payloads");
    staged_secret(&payload_root, "idp.secret", b"top-secret\n");
    let mock = MockServer::start().await;
    let base = mock.base();
    mock.push(
        "GET",
        "/.well-known/openid-configuration",
        discovery_reply(&base),
    );
    mock.push(
        "POST",
        "/token",
        Reply::json(
            200,
            serde_json::json!({
                "access_token": "at-1",
                "id_token": "id-1",
                "token_type": "Bearer",
                "expires_in": 60,
            }),
        ),
    );
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let inner = Arc::new(
        NetworkOidcAdapter::new(
            transport(),
            clock,
            NetworkOidcConfig {
                issuer: base.clone(),
                client_id: "client-1".into(),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let wrapper = ConfidentialOidcAdapter::new(
        inner,
        transport(),
        base.clone(),
        "client-1".into(),
        "top-secret".into(),
    );
    let outcome = AsyncOidcAdapter::exchange_code(
        &wrapper,
        &CodeExchangeRequest {
            code: "auth-code".into(),
            redirect_uri: "https://app.example/sso/callback".into(),
            code_verifier: "verifier".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.id_token, "id-1");
    assert_eq!(outcome.token_type, "Bearer");
    let posts = mock.requests("POST", "/token");
    assert_eq!(posts.len(), 1);
    let body = &posts[0].body;
    assert!(body.contains("grant_type=authorization_code"), "{body}");
    assert!(body.contains("client_id=client-1"), "{body}");
    assert!(body.contains("client_secret=top-secret"), "{body}");
    assert!(body.contains("code_verifier=verifier"), "{body}");
    let rendered = format!("{wrapper:?}");
    assert!(!rendered.contains("top-secret"), "{rendered}");
    assert!(!rendered.contains("secret"), "{rendered}");
}
