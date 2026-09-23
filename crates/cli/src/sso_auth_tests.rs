//! Adversarial tests of the SSO authority construction: the payload
//! contract (missing/world-readable/corrupt client secret), the strict
//! method selection, the public PKCE happy path that mints a real
//! authorization URL through the network adapter, and a confidential
//! authority built from a staged secret.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use faktor_cloud::{ClientAuthMethod, OrganizationId, SsoConfigRef};
use faktor_provider::egress::{HttpTransport, PolicyCheckedHttpTransport};

use super::build_sso_authority;
use crate::config::CloudSsoCfg;
use crate::test_http::{MockServer, Reply};

#[cfg(test)]
fn transport() -> Arc<dyn HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::permissive())
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
        client_secret_method: None,
        discovery_max_age_ms: None,
        jwks_max_age_ms: None,
        max_jwks_refetches: None,
        allowed_algorithms: None,
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

/// The confidential authority loads the staged secret through the payload
/// contract and serves the same start flow; a method that contradicts the
/// staged material is refused at load, never half-wired.
#[tokio::test]
async fn confidential_section_builds_from_the_staged_secret_and_refuses_bad_methods() {
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
    // Explicit client_secret_post builds and starts.
    let mut cfg = sso_cfg(base.clone(), Some("idp.secret"));
    cfg.client_secret_method = Some("client_secret_post".into());
    let authority = build_sso_authority(&cfg, &payload_root, transport()).unwrap();
    let organization = OrganizationId::try_new("org_sso").unwrap();
    let sso_ref = SsoConfigRef {
        issuer: base.clone(),
        client_id: "client-1".into(),
        membership_claim: "groups".into(),
        group_role_map: BTreeMap::new(),
        client_secret_ref: Some("idp.secret".into()),
        enabled: true,
    };
    let start = authority
        .start(&organization, &sso_ref, "https://app.example/sso/callback")
        .await
        .unwrap();
    assert!(start
        .authorization_url
        .starts_with(&format!("{base}/authorize?")));
    assert_eq!(
        cfg.client_auth_method().unwrap(),
        ClientAuthMethod::ClientSecretPost
    );

    // client_secret_basic is accepted and maps to the basic method.
    cfg.client_secret_method = Some("client_secret_basic".into());
    assert_eq!(
        cfg.client_auth_method().unwrap(),
        ClientAuthMethod::ClientSecretBasic
    );
    assert!(build_sso_authority(&cfg, &payload_root, transport()).is_ok());

    // An unknown method is a load refusal.
    cfg.client_secret_method = Some("client_secret_jwt".into());
    let err = cfg.validate().unwrap_err();
    assert!(err.contains("client_secret_post"), "{err}");
    // A confidential method without a staged secret payload is refused.
    let mut no_secret = sso_cfg(base, None);
    no_secret.client_secret_method = Some("client_secret_basic".into());
    let err = no_secret.validate().unwrap_err();
    assert!(err.contains("requires a `client_secret`"), "{err}");
    // A malformed serialized section is refused before it reaches the daemon.
    let err = serde_json::from_str::<CloudSsoCfg>(
        r#"{"enabled": true, "issuer": "https://idp.example", "client_id": "c", "client_secret": "s", "client_secret_method": "none"}"#,
    )
    .unwrap()
    .validate()
    .unwrap_err();
    assert!(err.contains("confidential"), "{err}");
}

/// The deployment algorithm policy defaults to RS256-only and refuses
/// `none`, empty and unsupported lists; an explicit HS256 opt-in
/// round-trips.
#[test]
fn allowed_algorithms_default_to_rs256_and_refuse_hostile_lists() {
    let mut cfg = sso_cfg("https://idp.example".into(), None);
    assert_eq!(cfg.allowed_algorithms().unwrap(), vec!["RS256".to_string()]);
    cfg.allowed_algorithms = Some(vec!["none".into()]);
    assert!(cfg.validate().unwrap_err().contains("none"));
    assert!(cfg.allowed_algorithms().unwrap_err().contains("none"));
    cfg.allowed_algorithms = Some(vec!["HS512".into()]);
    assert!(cfg.validate().unwrap_err().contains("not supported"));
    assert!(cfg
        .allowed_algorithms()
        .unwrap_err()
        .contains("not supported"));
    cfg.allowed_algorithms = Some(Vec::new());
    assert!(cfg.validate().unwrap_err().contains("1..="));
    cfg.allowed_algorithms = Some(vec!["RS256".into(), "HS256".into()]);
    assert_eq!(
        cfg.allowed_algorithms().unwrap(),
        vec!["RS256".to_string(), "HS256".to_string()]
    );
    // The refused list never reaches a wired authority.
    cfg.allowed_algorithms = Some(vec!["none".into()]);
    let err = build_sso_authority(&cfg, Path::new("/nonexistent"), transport()).unwrap_err();
    assert!(err.contains("none"), "{err}");
}
