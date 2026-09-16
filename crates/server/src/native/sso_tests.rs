//! Route tests of the SSO login surface: disabled parity, the not-configured
//! refusal, the start -> callback round trip against the deterministic fake
//! adapter and the one-shot session mint.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{
    ControlPlane, EnterpriseService, FakeOidcAdapter, ManualClock, MemoryControlPlaneStore,
    OidcClaims, OrgSettings, Role, SsoConfigRef, SsoLogin,
};

use crate::api::tests::test_deps;
use crate::{serve, ServerHandle};

const NOW_MS: i64 = 1_700_000_000_000;
const ISSUER: &str = "https://idp.example";
const CLIENT: &str = "faktor-server-test";

struct Harness {
    handle: ServerHandle,
    control_plane: Arc<ControlPlane>,
    fake: Arc<FakeOidcAdapter>,
    daemon_token: String,
    org: String,
    owner_token: String,
}

async fn harness(root: &std::path::Path, sso: bool, configured: bool) -> Harness {
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let store = Arc::new(MemoryControlPlaneStore::new());
    let control_plane = Arc::new(ControlPlane::new(store.clone(), clock.clone()));
    let enterprise =
        Arc::new(EnterpriseService::new(store.clone(), clock.clone()).expect("enterprise service"));
    let boot = control_plane
        .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-sso")
        .unwrap();
    let org = boot.organization.id.as_str().to_string();
    let owner_token = boot.token.expect("first issuance").expose().to_string();
    let principal = control_plane.authenticate(&owner_token).unwrap();
    if configured {
        let mut settings = OrgSettings::empty(boot.organization.id.clone(), NOW_MS);
        settings.sso = Some(SsoConfigRef {
            issuer: ISSUER.into(),
            client_id: CLIENT.into(),
            membership_claim: "groups".into(),
            group_role_map: BTreeMap::from([("faktor-admins".to_string(), Role::Admin)]),
            client_secret_ref: None,
            enabled: true,
        });
        enterprise.set_settings(&principal, settings).unwrap();
    }
    let fake = Arc::new(FakeOidcAdapter::new(ISSUER, CLIENT, b"secret-one"));
    let mut deps = test_deps(root)
        .with_control_plane(control_plane.clone())
        .with_enterprise(enterprise, None);
    if sso {
        deps = deps.with_sso(Arc::new(SsoLogin::new(fake.clone(), clock)));
    } else {
        assert!(deps.sso.is_none(), "sso is disabled by default");
    }
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    Harness {
        handle,
        control_plane,
        fake,
        daemon_token: token.as_str().to_string(),
        org,
        owner_token,
    }
}

fn start_body(org: &str) -> serde_json::Value {
    serde_json::json!({
        "organization": org,
        "redirect_uri": "https://app.example/callback",
    })
}

/// Disabled parity: without an SSO authority both routes answer the typed
/// 409 and nothing else changes.
#[tokio::test]
async fn sso_disabled_parity() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), false, true).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .json(&start_body(&h.org))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "sso_disabled");
    let resp = client
        .post(format!("{base}/native/sso/callback"))
        .bearer_auth(&h.daemon_token)
        .json(&serde_json::json!({
            "organization": h.org,
            "redirect_uri": "https://app.example/callback",
            "code": "c",
            "state": "s",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "sso_disabled");
}

/// With the authority wired but no organization SSO configuration, start is
/// the typed not-configured refusal.
#[tokio::test]
async fn sso_not_configured_is_a_typed_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true, false).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .json(&start_body(&h.org))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "sso_not_configured");
}

/// The full route round trip: start mints the authorization URL, the callback
/// verifies the fake token and mints a control-plane session that
/// authenticates with the mapped role.
#[tokio::test]
async fn sso_start_callback_mints_a_control_plane_session() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true, true).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .json(&start_body(&h.org))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let authorization_url = body["authorizationUrl"].as_str().unwrap().to_string();
    let state = body["state"].as_str().unwrap().to_string();
    let nonce = authorization_url
        .split('&')
        .find_map(|pair| pair.strip_prefix("nonce="))
        .expect("the authorization URL carries the nonce")
        .to_string();
    assert!(authorization_url.contains("code_challenge_method=S256"));

    h.fake.issue_code(
        "code-route-1",
        OidcClaims {
            issuer: ISSUER.into(),
            subject: "sub-route-1".into(),
            audience: vec![CLIENT.into()],
            email: Some("user@example.test".into()),
            email_verified: true,
            issued_at_ms: NOW_MS - 1_000,
            expires_at_ms: NOW_MS + 60_000,
            nonce: Some(nonce),
            groups: vec!["faktor-admins".into()],
        },
    );
    let resp = client
        .post(format!("{base}/native/sso/callback"))
        .bearer_auth(&h.daemon_token)
        .json(&serde_json::json!({
            "organization": h.org,
            "redirect_uri": "https://app.example/callback",
            "code": "code-route-1",
            "state": state,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "callback must mint the session");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["role"], "admin");
    let session_token = body["token"].as_str().unwrap().to_string();
    let principal = h.control_plane.authenticate(&session_token).unwrap();
    assert_eq!(principal.role, Role::Admin);
    assert_eq!(principal.organization.as_str(), h.org);

    // The state is single use: replaying the callback is refused.
    let resp = client
        .post(format!("{base}/native/sso/callback"))
        .bearer_auth(&h.daemon_token)
        .json(&serde_json::json!({
            "organization": h.org,
            "redirect_uri": "https://app.example/callback",
            "code": "code-route-1",
            "state": "state-does-not-exist",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "sso_state_invalid");
    // The owner token is untouched by the SSO surface.
    assert!(h.control_plane.authenticate(&h.owner_token).is_ok());
}
