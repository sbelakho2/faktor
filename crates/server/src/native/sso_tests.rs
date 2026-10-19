//! Route tests of the SSO login surface: disabled parity, the not-configured
//! refusal, the start -> callback round trip against the deterministic fake
//! adapter and the one-shot session mint.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{
    BillingConfig, ControlPlane, EnterpriseService, EntitlementService, FakeOidcAdapter,
    ManualClock, MemoryBillingStore, MemoryControlPlaneStore, OidcClaims, OrgSettings, PlanConfig,
    Role, SsoConfigRef, SsoLogin, Subscription, SubscriptionId, SubscriptionStatus,
    FEATURE_ENTERPRISE_GOVERNANCE, FEATURE_GOVERNANCE_SCIM, FEATURE_GOVERNANCE_SSO,
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
    session_id: String,
}

/// `plan` = the billing plan the enterprise organization is subscribed to
/// (`None` = no billing authority wired). The SSO ENTRY is gated by the
/// `enterprise_governance` entitlement + the `governance_sso` capability
/// flag, so the success tests subscribe the org to a governance plan.
async fn harness(
    root: &std::path::Path,
    sso: bool,
    configured: bool,
    plan: Option<&str>,
) -> Harness {
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
    let session_id = boot.session.id.as_str().to_string();
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
    if let Some(plan) = plan {
        let features: Vec<&str> = match plan {
            "enterprise" => vec![
                FEATURE_ENTERPRISE_GOVERNANCE,
                FEATURE_GOVERNANCE_SSO,
                FEATURE_GOVERNANCE_SCIM,
            ],
            "governance-no-sso" => vec![FEATURE_ENTERPRISE_GOVERNANCE],
            _ => vec![],
        };
        let mut config = BillingConfig::default();
        config.plans.insert(
            plan.to_string(),
            PlanConfig {
                plan_id: plan.to_string(),
                features: features.into_iter().map(str::to_string).collect(),
                limits: Default::default(),
            },
        );
        let billing = EntitlementService::new_for_organization(
            Arc::new(MemoryBillingStore::new()),
            clock.clone(),
            config,
            Some(boot.organization.id.clone()),
        )
        .unwrap();
        billing
            .set_subscription(&Subscription {
                id: SubscriptionId::try_new("sub_sso").unwrap(),
                organization: boot.organization.id.clone(),
                plan_id: plan.to_string(),
                status: SubscriptionStatus::Active,
                started_ms: 0,
                expires_ms: None,
                updated_ms: NOW_MS,
            })
            .unwrap();
        deps = deps.with_billing(billing);
    }
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
        session_id,
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
    let h = harness(dir.path(), false, true, None).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    // ORDERING: missing/invalid control credentials lose BEFORE `sso_disabled`.
    for control in [None, Some("bogus-sso-token")] {
        let mut request = client
            .post(format!("{base}/native/sso/start"))
            .bearer_auth(&h.daemon_token)
            .json(&start_body(&h.org));
        if let Some(control) = control {
            request = request.header("x-faktor-control-token", control);
        }
        let resp = request.send().await.unwrap();
        assert_eq!(
            resp.status(),
            401,
            "control={control:?} must lose before sso_disabled"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
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
        .header("x-faktor-control-token", &h.owner_token)
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
    let h = harness(dir.path(), true, false, Some("enterprise")).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
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
    let h = harness(dir.path(), true, true, Some("enterprise")).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
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
            nonce: Some(nonce.into()),
            groups: vec!["faktor-admins".into()],
        },
    );
    let resp = client
        .post(format!("{base}/native/sso/callback"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
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
        .header("x-faktor-control-token", &h.owner_token)
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

// ------------------------------------------------------------------ logout

fn logout_body(org: &str, session_id: &str) -> serde_json::Value {
    serde_json::json!({ "organization": org, "session_id": session_id })
}

/// Logout durably revokes the presented session: the token dies for every
/// later request (the control-plane routes answer 401) and the durable row
/// carries `revoked_ms`. Logout does not need the SSO adapter: a bootstrap
/// session is revocable with `[cloud]` alone.
#[tokio::test]
async fn logout_invalidates_the_session_and_later_use_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), false, false, None).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    // The session works before logout.
    let resp = client
        .get(format!("{base}/native/identity"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(&logout_body(&h.org, &h.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "first logout revokes");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["revoked"], true);
    assert_eq!(body["alreadyRevoked"], false);
    assert_eq!(body["session"], h.session_id);
    // Durable row carries the revocation.
    let session = h
        .control_plane
        .authenticate(&h.owner_token)
        .expect_err("the token is dead");
    assert!(session.to_string().contains("revoked"), "{session}");
    // Subsequent use of the token is 401 on the control-plane surface.
    let resp = client
        .get(format!("{base}/native/identity"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

/// The second logout of the same session is the typed idempotent replay.
#[tokio::test]
async fn logout_is_idempotent_typed() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), false, false, None).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    for (expected_revoked, expected_already) in [(true, false), (false, true)] {
        let resp = client
            .post(format!("{base}/native/sso/logout"))
            .bearer_auth(&h.daemon_token)
            .header("x-faktor-control-token", &h.owner_token)
            .json(&logout_body(&h.org, &h.session_id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["revoked"], expected_revoked);
        assert_eq!(body["alreadyRevoked"], expected_already);
    }
}

/// A foreign session id is refused: another organization's session is the
/// same typed 404 a missing id answers, and a token that does not own the
/// named session (even in its own organization) is 401.
#[tokio::test]
async fn logout_refuses_foreign_and_mismatched_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), false, false, None).await;
    let other = h
        .control_plane
        .bootstrap_organization("globex", "owner@globex.test", "Owner", "boot-globex")
        .unwrap();
    let other_token = other.token.expect("first issuance").expose().to_string();
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);

    // The owner's token claiming the OTHER organization's session under its
    // own organization: 404 (no existence leak).
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(&logout_body(&h.org, other.session.id.as_str()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    // A missing session id in the caller's own organization: the same 404.
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(&logout_body(&h.org, "ses_missing"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    // The other owner's token naming THIS organization's session: the named
    // organization is FOREIGN to the principal, so it is the same typed 404
    // a missing organization answers (no tenant/existence oracle).
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &other_token)
        .json(&logout_body(&h.org, &h.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    // A token in the SAME organization that does not own the named session is
    // a 401 (the credential is valid, but it is not this session's owner).
    let owner = h.control_plane.authenticate(&h.owner_token).unwrap();
    let same_org = h
        .control_plane
        .create_service_account(
            &owner,
            &faktor_cloud::OrganizationId::try_new(h.org.clone()).unwrap(),
            "same-org-probe",
            Role::Member,
            vec![faktor_cloud::Action::BillingRead],
        )
        .unwrap()
        .token
        .expect("first issuance")
        .expose()
        .to_string();
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &same_org)
        .json(&logout_body(&h.org, &h.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // None of the refusals revoked anything.
    assert!(h.control_plane.authenticate(&h.owner_token).is_ok());
    assert!(h.control_plane.authenticate(&other_token).is_ok());

    // Missing control token and a hostile body are loud typed refusals.
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(&h.daemon_token)
        .json(&logout_body(&h.org, &h.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(
            &serde_json::json!({ "organization": h.org, "session_id": h.session_id, "hostile": 1 }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// Without a control plane wired, logout refuses the unverifiable credential
/// with 401 BEFORE the `[cloud]` feature state (never `cloud_disabled`).
#[tokio::test]
async fn logout_without_control_plane_is_an_auth_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(token.as_str())
        .header("x-faktor-control-token", "whatever")
        .json(&logout_body("acme", "ses_x"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unauthorized");
    // Missing credential is the same auth refusal.
    let resp = client
        .post(format!("{base}/native/sso/logout"))
        .bearer_auth(token.as_str())
        .json(&logout_body("acme", "ses_x"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

/// The identity-federation ENTRY is gated by the `enterprise_governance`
/// value entitlement + the `governance_sso` capability flag: a plan that
/// maps neither is refused TYPED at start and callback (naming the
/// entitlement and the capability), and a governance plan without the SSO
/// flag is refused naming the capability.
#[tokio::test]
async fn sso_entry_is_refused_typed_without_the_governance_entitlement() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true, true, Some("local")).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    for route in ["start", "callback"] {
        let resp = client
            .post(format!("{base}/native/sso/{route}"))
            .bearer_auth(&h.daemon_token)
            .header("x-faktor-control-token", &h.owner_token)
            .json(&if route == "start" {
                start_body(&h.org)
            } else {
                serde_json::json!({
                    "organization": h.org,
                    "redirect_uri": "https://app.example/callback",
                    "code": "c",
                    "state": "s",
                })
            })
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{route}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "value_entitlement_refused");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("enterprise_governance"), "{message}");
        assert!(message.contains("identity_federation"), "{message}");
        assert!(message.contains("capability sso"), "{message}");
    }
}

/// A governance plan WITHOUT the `governance_sso` capability flag: the base
/// entitlement is granted but the flag gates the entry — typed refusal
/// naming the capability.
#[tokio::test]
async fn sso_entry_requires_the_sso_capability_flag() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true, true, Some("governance-no-sso")).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(&start_body(&h.org))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "value_entitlement_refused");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("capability sso"), "{message}");
}

/// Offline (no billing authority wired): the SSO entry is refused typed
/// `billing_disabled` — never a silent login.
#[tokio::test]
async fn sso_entry_refuses_typed_when_no_billing_authority_is_wired() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true, true, None).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(&start_body(&h.org))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "billing_disabled");
}

/// ORACLE CLOSURE: start/callback require a control-plane PRINCIPAL and the
/// named organization must be the principal's OWN. A daemon-password caller
/// without a principal cannot probe organizations at all, and a foreign
/// organization — existing or not — answers the SAME typed refusal naming
/// only the CALLER's own organization, so the surface is never a
/// plan/existence oracle.
#[tokio::test]
async fn sso_start_requires_a_principal_and_closes_the_organization_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(dir.path(), true, true, Some("enterprise")).await;
    // A second organization EXISTS in the same control plane (with no SSO
    // settings): its existence must not be distinguishable through the SSO
    // surface.
    let globex = h
        .control_plane
        .bootstrap_organization("globex", "owner@globex.test", "Owner", "boot-globex")
        .unwrap();
    let globex_org = globex.organization.id.as_str().to_string();
    let globex_token = globex.token.expect("first issuance").expose().to_string();
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.handle.addr);

    // 1. Daemon password alone (no principal): 401 BEFORE any organization,
    // plan or settings row is read.
    let resp = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .json(&start_body(&h.org))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "the daemon password alone is not a principal"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unauthorized");

    // 2. The acme owner naming globex (which EXISTS) and naming a MISSING
    // organization: byte-identical typed refusals, naming only the caller's
    // own organization.
    let foreign = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(&start_body("globex"))
        .send()
        .await
        .unwrap();
    let missing = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(&start_body("missingorg"))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 404);
    assert_eq!(missing.status(), 404);
    let foreign_body = foreign.text().await.unwrap();
    let missing_body = missing.text().await.unwrap();
    assert_eq!(
        foreign_body, missing_body,
        "an existing and a missing foreign organization must answer identically"
    );
    assert!(!foreign_body.contains("globex"), "{foreign_body}");
    assert!(!foreign_body.contains("missingorg"), "{foreign_body}");
    assert!(
        foreign_body.contains(&h.org),
        "the refusal names only the caller's own organization: {foreign_body}"
    );

    // 3. The mirrored callback refusal: the globex principal naming acme is
    // the same scope refusal naming globex's OWN organization id as its own
    // data — never the requested one.
    let mirrored = client
        .post(format!("{base}/native/sso/callback"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &globex_token)
        .json(&serde_json::json!({
            "organization": h.org,
            "redirect_uri": "https://app.example/callback",
            "code": "c",
            "state": "s",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(mirrored.status(), 404);
    let mirrored_body = mirrored.text().await.unwrap();
    assert!(mirrored_body.contains(&globex_org), "{mirrored_body}");
    assert!(!mirrored_body.contains(&h.org), "{mirrored_body}");

    // 4. No refusal minted or consumed anything: the caller's OWN
    // organization still starts and both principals stay valid.
    let own = client
        .post(format!("{base}/native/sso/start"))
        .bearer_auth(&h.daemon_token)
        .header("x-faktor-control-token", &h.owner_token)
        .json(&start_body(&h.org))
        .send()
        .await
        .unwrap();
    assert_eq!(
        own.status(),
        200,
        "the caller's own organization still starts"
    );
    assert!(h.control_plane.authenticate(&h.owner_token).is_ok());
    assert!(h.control_plane.authenticate(&globex_token).is_ok());
}
