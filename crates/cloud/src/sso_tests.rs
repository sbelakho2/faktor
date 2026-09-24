//! Adversarial unit tests of the SSO secret handling (P2-B): the state and
//! the nonce are domain SECRET TYPES whose plaintext must never appear in a
//! rendering (Debug, errors, or any nested formatting), the pending map is
//! keyed by SHA-256(state) rather than plaintext, and the wire projection is
//! the only plaintext escape hatch the frontend consumes.

use std::sync::Arc;

use crate::enterprise::SsoConfigRef;
use crate::ids::OrganizationId;
use crate::oidc::{FakeOidcAdapter, OidcClaims, OidcError};
use crate::rbac::Role;
use crate::service::{sha256_hex, ControlPlane, ManualClock};
use crate::store::MemoryControlPlaneStore;

use super::SsoLogin;

const NOW_MS: i64 = 1_700_000_000_000;
const ISSUER: &str = "https://idp.example";
const CLIENT: &str = "faktor-cloud-sso-tests";
const REDIRECT: &str = "https://app.example/callback";
/// A recognisable planted credential: every leak assertion searches for it.
const PLANTED: &str = "PLANTED-secret-marker-0123456789abcdef";

fn sso_ref() -> SsoConfigRef {
    SsoConfigRef {
        issuer: ISSUER.into(),
        client_id: CLIENT.into(),
        membership_claim: "groups".into(),
        group_role_map: std::collections::BTreeMap::from([(
            "faktor-admins".to_string(),
            Role::Admin,
        )]),
        client_secret_ref: None,
        enabled: true,
    }
}

struct Fix {
    authority: SsoLogin,
    fake: Arc<FakeOidcAdapter>,
    control_plane: ControlPlane,
    org: OrganizationId,
}

fn fix() -> Fix {
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let fake = Arc::new(FakeOidcAdapter::new(ISSUER, CLIENT, b"secret-one"));
    let authority = SsoLogin::new(fake.clone(), clock.clone());
    let control_plane = ControlPlane::new(Arc::new(MemoryControlPlaneStore::new()), clock);
    // The SSO login resolves/links against an EXISTING organization.
    let org = control_plane
        .bootstrap_organization("org-sso", "owner@example.test", "Owner", "boot-sso")
        .expect("the organization bootstraps")
        .organization
        .id;
    Fix {
        authority,
        fake,
        control_plane,
        org,
    }
}

fn claims_with_nonce(nonce: &str) -> OidcClaims {
    OidcClaims {
        issuer: ISSUER.into(),
        subject: "sub-1".into(),
        audience: vec![CLIENT.into()],
        email: Some("user@example.test".into()),
        email_verified: true,
        issued_at_ms: NOW_MS - 1_000,
        expires_at_ms: NOW_MS + 60_000,
        nonce: Some(nonce.into()),
        groups: vec!["faktor-admins".into()],
    }
}

/// Every reachable rendering of the started login and its domain secret
/// wrappers is redacted; the wire projection is the ONE place the plaintext
/// state (and the URL embedding state + nonce) is readable.
#[tokio::test]
async fn started_secrets_never_render_and_the_wire_projection_is_the_only_escape() {
    let f = fix();
    let start = f
        .authority
        .start(&f.org, &sso_ref(), REDIRECT)
        .await
        .unwrap();
    let state = start.state.expose().to_string();
    let nonce = start.nonce.expose().to_string();
    assert_eq!(state.len(), 64, "a 32-byte hex state");
    assert_eq!(nonce.len(), 64);
    assert_ne!(state, nonce);

    // A derived Debug (or one missing the authorization-URL redaction) would
    // print the secrets here; the custom impls must not.
    for rendered in [
        format!("{start:?}"),
        format!("{:?}", Some(&start)),
        format!("{:?}", vec![start.clone()]),
        format!("{:?}", (start.state.clone(), start.nonce.clone())),
    ] {
        assert!(!rendered.contains(&state), "state leaked: {rendered}");
        assert!(!rendered.contains(&nonce), "nonce leaked: {rendered}");
        assert!(rendered.contains("[redacted]"), "{rendered}");
    }

    // The wire projection carries the values for the frontend — its Debug is
    // still redacted, so only the explicit accessors (the wire serializer)
    // read them.
    let wire = start.into_wire_response();
    assert_eq!(wire.state(), state);
    assert!(wire.authorization_url().contains(&format!("state={state}")));
    assert!(wire.authorization_url().contains(&format!("nonce={nonce}")));
    let wire_debug = format!("{wire:?}");
    assert!(!wire_debug.contains(&state) && !wire_debug.contains(&nonce));
    // The sanctioned wire encoding is the frontend JSON the route builds;
    // that is where the values MUST appear. (`SsoStart`, `OAuthState` and
    // `OidcNonce` deliberately implement no `Serialize`/`Display` at all —
    // those absences are compile-time, so `Debug`/errors are the complete
    // runtime rendering surface.)
    let body = serde_json::json!({
        "authorizationUrl": wire.authorization_url(),
        "state": wire.state(),
    });
    assert_eq!(body["state"], serde_json::Value::String(state));
}

/// The pending map stores SHA-256(state), never the plaintext; both the mint
/// and the callback lookup go through the digest, and the single-use deletion
/// removes the digest key.
#[tokio::test]
async fn pending_map_is_digest_keyed_and_lookup_hashes_the_wire_state() {
    let f = fix();
    let org = f.org.clone();
    let start = f.authority.start(&org, &sso_ref(), REDIRECT).await.unwrap();
    let state = start.state.expose().to_string();
    let nonce = start.nonce.expose().to_string();
    let key = sha256_hex(state.as_bytes());
    {
        let pending = f.authority.lock_pending();
        assert_eq!(pending.len(), 1);
        let (stored_key, login) = pending.iter().next().unwrap();
        assert_eq!(stored_key, &key, "the map key is SHA-256(state)");
        assert_ne!(stored_key, &state, "the plaintext state is never a key");
        assert!(!stored_key.contains(&nonce));
        assert!(
            !stored_key.contains(&state) && !nonce.is_empty(),
            "credentials never ride a key"
        );
        assert_eq!(
            login.nonce.expose(),
            nonce,
            "the nonce is wrapped, not discarded"
        );
        assert_eq!(login.redirect_uri, REDIRECT);
    }

    // Presenting the DIGEST itself does not authenticate: the callback
    // hashes whatever the wire carries, so there is no digest-as-state
    // bypass.
    let err = f
        .authority
        .callback(&f.control_plane, &org, &sso_ref(), REDIRECT, "code-x", &key)
        .await
        .unwrap_err();
    assert!(matches!(err, OidcError::StateInvalid));

    // The real state resolves the digest-keyed row: the exchange then refuses
    // the unknown code, so any StateInvalid here would prove the digest
    // lookup failed.
    let err = f
        .authority
        .callback(
            &f.control_plane,
            &org,
            &sso_ref(),
            REDIRECT,
            "code-x",
            &state,
        )
        .await
        .unwrap_err();
    assert!(!matches!(err, OidcError::StateInvalid), "{err:?}");
    assert!(
        f.authority.lock_pending().is_empty(),
        "single use consumed the row"
    );
    // Replay: the digest key is gone.
    let err = f
        .authority
        .callback(
            &f.control_plane,
            &org,
            &sso_ref(),
            REDIRECT,
            "code-x",
            &state,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OidcError::StateInvalid));
    for rendered in [format!("{err}"), format!("{err:?}")] {
        assert!(!rendered.contains(&state));
        assert!(!rendered.contains(&nonce));
    }
}

/// Wrong-state and wrong-nonce refusals never carry the planted wire value
/// or the login's real state/nonce in any rendering.
#[tokio::test]
async fn wrong_state_and_nonce_errors_never_carry_planted_or_real_secrets() {
    let f = fix();
    let org = f.org.clone();
    let start = f.authority.start(&org, &sso_ref(), REDIRECT).await.unwrap();
    let state = start.state.expose().to_string();
    let nonce = start.nonce.expose().to_string();

    let err = f
        .authority
        .callback(
            &f.control_plane,
            &org,
            &sso_ref(),
            REDIRECT,
            "code-1",
            PLANTED,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OidcError::StateInvalid));
    for rendered in [format!("{err}"), format!("{err:?}")] {
        assert!(!rendered.contains(PLANTED));
        assert!(!rendered.contains(&state));
        assert!(!rendered.contains(&nonce));
    }

    // The state exists, but the ID token is bound to a different (planted)
    // nonce: the typed NonceMismatch refusal names neither nonce.
    f.fake
        .issue_code("code-wrong-nonce", claims_with_nonce(PLANTED));
    let err = f
        .authority
        .callback(
            &f.control_plane,
            &org,
            &sso_ref(),
            REDIRECT,
            "code-wrong-nonce",
            &state,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OidcError::NonceMismatch), "{err:?}");
    for rendered in [format!("{err}"), format!("{err:?}")] {
        assert!(!rendered.contains(PLANTED));
        assert!(!rendered.contains(&state));
        assert!(!rendered.contains(&nonce));
    }
}

/// The wire state completes a real login against the wrapped nonce (the
/// values survive the wrapping end-to-end) and the single-use contract still
/// holds for the digest-keyed row.
#[tokio::test]
async fn wire_state_completes_the_login_with_the_wrapped_nonce() {
    let f = fix();
    let org = f.org.clone();
    let start = f.authority.start(&org, &sso_ref(), REDIRECT).await.unwrap();
    let nonce = start.nonce.expose().to_string();
    let wire = start.into_wire_response();
    let state = wire.state().to_string();
    f.fake.issue_code("code-ok", claims_with_nonce(&nonce));
    let outcome = f
        .authority
        .callback(
            &f.control_plane,
            &org,
            &sso_ref(),
            REDIRECT,
            "code-ok",
            &state,
        )
        .await
        .unwrap();
    assert_eq!(outcome.membership.role, Role::Admin);
    let err = f
        .authority
        .callback(
            &f.control_plane,
            &org,
            &sso_ref(),
            REDIRECT,
            "code-ok",
            &state,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OidcError::StateInvalid));
}
