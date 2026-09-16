//! Adversarial tests of the NETWORK OIDC adapter and the SSO login flow over
//! a MOCK OIDC provider served by a scripted [`HttpTransport`]: discovery
//! max-age caching, authorization-code exchange, RS256 ID-token verification
//! with JWKS rotation and bounded unknown-kid refetch, wrong state/nonce,
//! expired tokens and the control-plane session mint.
//!
//! The mock provider signs REAL RS256 tokens with ring; the adapter verifies
//! them through the same checked transport seam production uses. Nothing
//! touches the network.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use futures_util::future::BoxFuture;
use reqwest::{Body, Request, Response};
use ring::rand::SystemRandom;
use ring::signature::{KeyPair, RsaKeyPair, RSA_PKCS1_SHA256};

use faktor_cloud::{
    AsyncOidcAdapter, ClaimMapping, CodeExchangeRequest, ControlPlane, FakeOidcAdapter,
    IdTokenExpectations, ManualClock, MemoryControlPlaneStore, NetworkOidcAdapter,
    NetworkOidcConfig, OidcClaims, OidcError, Role, SsoConfigRef, SsoLogin,
};
use faktor_provider::egress::{EgressError, HttpTransport};

const ISSUER: &str = "https://idp.example";
const CLIENT: &str = "faktor-test";
const NOW_MS: i64 = 1_700_000_000_000;

// ------------------------------------------------------------ mock provider

struct MockKey {
    kid: String,
    keypair: RsaKeyPair,
    n: Vec<u8>,
    e: Vec<u8>,
}

struct ProviderState {
    keys: Vec<MockKey>,
    active: usize,
    /// code -> (subject, email, groups, nonce)
    codes: BTreeMap<String, MockTokenSpec>,
    discovery_max_age_s: Option<i64>,
    jwks_max_age_s: Option<i64>,
}

#[derive(Clone)]
struct MockTokenSpec {
    subject: String,
    email: String,
    groups: Vec<String>,
    nonce: Option<String>,
    expires_at_ms: i64,
}

struct MockProvider {
    state: Mutex<ProviderState>,
    requests: Mutex<Vec<(String, String)>>,
    discovery_requests: AtomicUsize,
    jwks_requests: AtomicUsize,
    token_requests: AtomicUsize,
}

/// Two fixed 2048-bit RSA PKCS#8 test keys (test fixtures only; generated
/// once with `openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048`).
const TEST_RSA_KEY_1_PKCS8_B64: &str = concat!(
    "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCQmhUYw0X1Vp3/ku8a34CggkswIS6urpYlHqur",
    "x5OnuIcJn5haYpRW9eTCXNCsgFaej8ed76cPhHfNdboxdwLaQIwFuhTQklbZgjEJ8IbJvLJOmA3amBqzLnj+LYcH",
    "qBSXDZYziXbRmnDUaIObRXgmAXm+5PHKNrh9X/wQB3nRmISh0W0OuAspKaaUe1qSEUZViMJWEvkfil1cLTGJWwu7",
    "3KvqtG+8Zv2nNY5XxF7voJOoCIusC2DlqLzKe/zyBpOeRPacEmqx/rDEJC/YF09MwLcRDlzd+6PEogPe3juvIdxe",
    "Q7fU6UODsvwGtBRoNJZjeHOUId5Ogtr/LcNaWleRAgMBAAECggEADZduBDXWjYjmsO8Oqmf3nx6gEpYL3T9rvXF0",
    "kWDg8BLYHn9xwo0hhEr4hYxT0OL/QUgiayXrffnliNpX9e5Jxlnqz17+NdyLbUZfdZ+2C6BA4kJIixNg7xafKMT+",
    "vCmARfJw+8cf8/OgR1bLh/yvnmLlHpxGXctm6+Hpv1f0i062fQyNnteOIFFNXjKvKz/DHhIi0dBT/s6B+DZwTDgx",
    "DmDlNlZc0pizjD4L2KoCXu4lLAiZxtccUjaK5jmmrWi06IesgAneJbxSV6E0Z+NnFaOJT6jztEYfki0OPITn4a5B",
    "WWOIxakqn0FIi4qDWiFn4b1YDq99QZq3PZc8/JvyQQKBgQDCNweO9CssfIy9U19b+JarP3KHaZSrHDQI1e6hYmO1",
    "pp4TN82SG7BAnWkVOhlGnYgGo1L5UC97JjGB6hs2YqE/r8ZhB5VD3qn5RuRwNNT0zlyLGXAJUh2Oa5ujK+DqUnAf",
    "V80DkeEex86EwOeDa6Bs4zDp/EU2UDUkmm0XQ1Z/DwKBgQC+momXJaOahhIJMSYT9p1AFInkUW7FEobWkodaLpey",
    "5p4yqcbRSWC02oWnYenOJDGl3OfdhQxOa5CeFgx1JQgkAUEzg5nRuvVLmwYQYcVjbVOXjcy7ZDpZfbyNhFBAdwli",
    "RsJqvKJljC/xwK9a7xUTbXGvqNYs71f73dWCtDu/XwKBgHJNhuJIrBw7lW1b4zSy4qIY7mPp7LikGa/VkONkj8B6",
    "NnCjGBbUuu/cdNssXXHlBwi5GP1ohvlYqiyGxstEUxizb/LtTpkqNClk4s9zGJ6X0XmAWCL0NDb8+BWZnn7qU9ju",
    "iNeABNljyRTyn48GSd2r/L7JXUaxAAXx6SCW3hJbAoGAfKg5gH7/Zxp0RUq9qqTJ55UHMioIFh+tzDv9BgAe+sRV",
    "hrD+9PXWp7GbZANnlIibZ+z4QCq6B7fV1254K01S66leaUCSo1ZxA0eaSbCIFiT0XNRCp/Q/LTRM0wlMKz0vB/Vb",
    "Rc+lLmDnImdwyDpBQHl9tvLnUHAgzPsint8djGUCgYAgXp26nYTcrAYdaYw800tFfRw8he5MssOFUGwOmyZ2Tcna",
    "cQHov+jik1xSUTvAHNIoiA7WtLjTcwVE7uxjiynvp6tL78lxfIXMNnKiKnRrfp8f2oAgbPnVAssmlroPO41bQfGv",
    "MvGRP7l5VPr3eT8X9NEBJyODlzU+LzZgOl+Wew==",
);
const TEST_RSA_KEY_2_PKCS8_B64: &str = concat!(
    "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC8DsVELKPUPL4ti3n1ixiGVs+FPU08PbIM7ruL",
    "Shxq+j8ngKz3yysS+j0j+kGm1dfmqFvcnPaynsRE9hwe7ZVOACMfNtn4wUPam95gwh43IhzTTruW3nj1vx0ScvWB",
    "TTxveC47cdsS6Pd8ZXNf3E/D1ZBIrHq9Wt3J2r46PPm7SiPSWqktUzRRMCYz7Ly9VRsQzppyNX5bmeky9iXCD8mc",
    "zmVXSBA4CmZUpm9lXSRzqJ1m28RrRRRXSIBCl1FRv33wkDnq1/80ma6ElPU2D7bpvoLk0NUqYKCpbgKWJbsiGf09",
    "Fk+A0lhxxj+ujuvcj7daKd3ValR/M5DCNao8LfrFAgMBAAECggEAVuDKAGtRClk/kKHpZ2bpnxJWz5qY5lYoPfJC",
    "YSCNTVyrtW+sONO65AsIGOlh0BXlprErsxkunSlcyfEa36zpt323vBFmlJWQZ9tvWisDs3vGblZmslW38uvmHeJP",
    "CfupCmQuk5bPWwaWYvkpWmVY0kOE4xYPpA/o+3pbPGN0CbHziV4EYDq1K/YVyQ4B34H/RFwBIPx4+5pBOTYB77ya",
    "aNib2dm+OYukAmSO4owHYAq3Dw78It/y+MGeIgqcv9vi9QqwgRNwLxw4EEW+8I4JlitBCJEOuBlsqbK3+uwlUuBb",
    "6zKi/kXycxax01ZUM2Cr4Cfo9L4w7C2849IlWEPSNwKBgQD7dZJAdRTqB33ARYm6EVV69evgut6LxVcTpsTsXbp2",
    "sKOoEIaq00WYYMxPgPCiIXjWe9COY2IfJ0Tlm5yQKv0Rrore6v8+V218+gSe6BGAjzDxbJtN7ki8xVRh/MSENir4",
    "74vSz8JS3Nu1jZ1psq/Nja+LelCY5f8LfZe33vovHwKBgQC/dBxgqwbdqO6fuQJyNPs8F2zSy4NsXGySTmtcvwI6",
    "KmCep85G+Q54w5reC4FmF5KxE1Va1p7UZDvgMT+moIZKJBVU4TJDqQDs/k6lMxEloaSlQs3TFjqTho4EyCMWHU2f",
    "LysvQxaAQJxAcIhlERjhIRJbe2/cxBBx7QGKoTUtmwKBgB9nsqlcNg14fAscZDQZ4BwoRJpfnFXGgraQmH2Qwy35",
    "p6bg0YDaPBHo3Pt89hC5r3bSJdzyqpmLdP5cLfSPeeXQb8WhgdlOX/1A2HzkLPNqbsloMAlOnkT9PCm0wPJmNX27",
    "pTHiAroInWQSWLuPtocsj+USlKhT6UONHvq23XYvAoGATDtAXWFb/4CXWzPAfJcJ/jhZlWmBb/ExLeRZrXlEusJK",
    "7IFmii37DCzeilFMeckjGKzZDK1uWqV6jd8uN/us3PKXJ8/vQq+Vdcggqni1+CTvuPnrmIQ+WKV4AQFrrw+F679N",
    "U6lD2Vdgn+vu80cmf+W6OIDi3qWW4rX7KibcVMcCgYBJ6GfRITCDHyDr+moh4DsE6H9AmeXL8MwwvuEbZ0NS1QCv",
    "hCKxEhwrP+zDh+83nKjpy0TPTJjtFqCYFlw9/I0+pb2leaSqqcZLWb67EexErKlbTy/xD4NKy5mZW1XZO2QPo8f1",
    "8//KQP+/HuOToerD2AUkEdQaxONyheEhGHwy+A==",
);

fn generate_key(kid: &str) -> MockKey {
    let b64 = if kid == "kid-1" {
        TEST_RSA_KEY_1_PKCS8_B64
    } else {
        TEST_RSA_KEY_2_PKCS8_B64
    };
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("fixture key decodes");
    let keypair = RsaKeyPair::from_pkcs8(der.as_ref()).expect("rsa keypair");
    let (n, e) = parse_rsa_public_key_der(keypair.public_key().as_ref());
    MockKey {
        kid: kid.to_string(),
        keypair,
        n,
        e,
    }
}

/// Minimal DER reader for `RSAPublicKey ::= SEQUENCE { modulus INTEGER,
/// publicExponent INTEGER }` (ring's `public_key().as_ref()`).
fn parse_rsa_public_key_der(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
    fn read_len(bytes: &[u8]) -> (usize, usize) {
        if bytes[0] & 0x80 == 0 {
            (bytes[0] as usize, 1)
        } else {
            let count = (bytes[0] & 0x7f) as usize;
            let mut value = 0usize;
            for byte in &bytes[1..=count] {
                value = (value << 8) | *byte as usize;
            }
            (value, 1 + count)
        }
    }
    let mut at = 0;
    assert_eq!(der[at], 0x30, "SEQUENCE");
    at += 1;
    let (_total, used) = read_len(&der[at..]);
    at += used;
    let mut integers: Vec<Vec<u8>> = Vec::new();
    for _ in 0..2 {
        assert_eq!(der[at], 0x02, "INTEGER");
        at += 1;
        let (len, used) = read_len(&der[at..]);
        at += used;
        let mut value = der[at..at + len].to_vec();
        at += len;
        while value.first() == Some(&0) {
            value.remove(0);
        }
        integers.push(value);
    }
    (integers[0].clone(), integers[1].clone())
}

impl MockProvider {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProviderState {
                keys: vec![generate_key("kid-1")],
                active: 0,
                codes: BTreeMap::new(),
                discovery_max_age_s: Some(3600),
                jwks_max_age_s: Some(3600),
            }),
            requests: Mutex::new(Vec::new()),
            discovery_requests: AtomicUsize::new(0),
            jwks_requests: AtomicUsize::new(0),
            token_requests: AtomicUsize::new(0),
        }
    }

    fn rotate(&self, kid: &str) {
        let mut state = self.state.lock().unwrap();
        state.keys.push(generate_key(kid));
        state.active = state.keys.len() - 1;
    }

    fn issue_code(&self, code: &str, spec: MockTokenSpec) {
        self.state
            .lock()
            .unwrap()
            .codes
            .insert(code.to_string(), spec);
    }

    fn request_count(&self, needle: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, url)| url.contains(needle))
            .count()
    }

    /// Sign a claims set under the ACTIVE key.
    fn sign(&self, spec: &MockTokenSpec) -> String {
        let state = self.state.lock().unwrap();
        let key = &state.keys[state.active];
        sign_claims(key, &key.kid, spec)
    }

    /// Sign a claims set under an ARBITRARY kid label (forged kid tests).
    fn sign_with_kid_label(&self, kid_label: &str, spec: &MockTokenSpec) -> String {
        let state = self.state.lock().unwrap();
        let key = &state.keys[state.active];
        sign_claims(key, kid_label, spec)
    }

    fn handle(
        &self,
        method: &str,
        url: &str,
        body: &[u8],
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        self.requests
            .lock()
            .unwrap()
            .push((method.to_string(), url.to_string()));
        let path = url.split('?').next().unwrap_or(url);
        if path.ends_with("/.well-known/openid-configuration") {
            self.discovery_requests.fetch_add(1, Ordering::SeqCst);
            let max_age = self.state.lock().unwrap().discovery_max_age_s;
            let mut headers = vec![("content-type".into(), "application/json".into())];
            if let Some(seconds) = max_age {
                headers.push(("cache-control".into(), format!("public, max-age={seconds}")));
            }
            let doc = serde_json::json!({
                "issuer": ISSUER,
                "authorization_endpoint": format!("{ISSUER}/authorize"),
                "token_endpoint": format!("{ISSUER}/token"),
                "jwks_uri": format!("{ISSUER}/jwks"),
                "id_token_signing_alg_values_supported": ["RS256"],
            });
            return (200, headers, serde_json::to_vec(&doc).unwrap());
        }
        if path.ends_with("/jwks") {
            self.jwks_requests.fetch_add(1, Ordering::SeqCst);
            let state = self.state.lock().unwrap();
            let max_age = state.jwks_max_age_s;
            let keys: Vec<serde_json::Value> = state
                .keys
                .iter()
                .enumerate()
                .filter(|(index, _)| *index == state.active)
                .map(|(_, key)| {
                    serde_json::json!({
                        "kty": "RSA",
                        "kid": key.kid,
                        "alg": "RS256",
                        "use": "sig",
                        "n": b64url(&key.n),
                        "e": b64url(&key.e),
                    })
                })
                .collect();
            let mut headers = vec![("content-type".into(), "application/json".into())];
            if let Some(seconds) = max_age {
                headers.push(("cache-control".into(), format!("max-age={seconds}")));
            }
            return (
                200,
                headers,
                serde_json::to_vec(&serde_json::json!({ "keys": keys })).unwrap(),
            );
        }
        if path.ends_with("/token") {
            self.token_requests.fetch_add(1, Ordering::SeqCst);
            let code = form_field(body, "code").unwrap_or_default();
            let mut state = self.state.lock().unwrap();
            let Some(spec) = state.codes.remove(&code) else {
                return (
                    400,
                    vec![("content-type".into(), "application/json".into())],
                    serde_json::to_vec(&serde_json::json!({"error": "invalid_grant"})).unwrap(),
                );
            };
            let key_index = state.active;
            let key = &state.keys[key_index];
            let id_token = sign_claims(key, &key.kid, &spec);
            let response = serde_json::json!({
                "access_token": "access-token",
                "id_token": id_token,
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "openid email profile",
            });
            return (
                200,
                vec![("content-type".into(), "application/json".into())],
                serde_json::to_vec(&response).unwrap(),
            );
        }
        (404, Vec::new(), Vec::new())
    }
}

fn sign_claims(key: &MockKey, kid: &str, spec: &MockTokenSpec) -> String {
    let header = serde_json::json!({"alg": "RS256", "kid": kid, "typ": "JWT"});
    let payload = serde_json::json!({
        "iss": ISSUER,
        "sub": spec.subject,
        "aud": CLIENT,
        "email": spec.email,
        "email_verified": true,
        "iat": spec.expires_at_ms / 1000 - 60,
        "exp": spec.expires_at_ms / 1000,
        "nonce": spec.nonce,
        "groups": spec.groups,
    });
    let header = b64url(&serde_json::to_vec(&header).unwrap());
    let payload = b64url(&serde_json::to_vec(&payload).unwrap());
    let input = format!("{header}.{payload}");
    let rng = SystemRandom::new();
    let mut signature = vec![0u8; key.keypair.public().modulus_len()];
    key.keypair
        .sign(&RSA_PKCS1_SHA256, &rng, input.as_bytes(), &mut signature)
        .expect("sign");
    format!("{input}.{}", b64url(&signature))
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn form_field(body: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    for pair in text.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            let mut decoded = String::new();
            let bytes = value.as_bytes();
            let mut at = 0;
            while at < bytes.len() {
                if bytes[at] == b'%' && at + 2 < bytes.len() {
                    let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).ok()?;
                    decoded.push(char::from(u8::from_str_radix(hex, 16).ok()?));
                    at += 3;
                } else {
                    decoded.push(char::from(bytes[at]));
                    at += 1;
                }
            }
            return Some(decoded);
        }
    }
    None
}

impl HttpTransport for MockProvider {
    fn execute(&self, req: Request) -> BoxFuture<'_, Result<Response, EgressError>> {
        let method = req.method().to_string();
        let url = req.url().to_string();
        let body = req
            .body()
            .and_then(|b| b.as_bytes())
            .unwrap_or(&[])
            .to_vec();
        let (status, headers, body) = self.handle(&method, &url, &body);
        Box::pin(async move {
            let mut builder = http::Response::builder().status(status);
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            let response = builder
                .body(Body::from(body))
                .map_err(|e| EgressError::Build(e.to_string()))?;
            Ok(Response::from(response))
        })
    }
}

// ------------------------------------------------------------------ fixtures

fn config() -> NetworkOidcConfig {
    NetworkOidcConfig {
        issuer: ISSUER.into(),
        client_id: CLIENT.into(),
        discovery_max_age_ms: 300_000,
        jwks_max_age_ms: 300_000,
        max_jwks_refetches: 2,
    }
}

fn adapter(provider: Arc<MockProvider>, clock: Arc<ManualClock>) -> NetworkOidcAdapter {
    NetworkOidcAdapter::new(provider, clock, config()).unwrap()
}

fn sso_ref() -> SsoConfigRef {
    SsoConfigRef {
        issuer: ISSUER.into(),
        client_id: CLIENT.into(),
        membership_claim: "groups".into(),
        group_role_map: BTreeMap::from([("faktor-admins".to_string(), Role::Admin)]),
        client_secret_ref: None,
        enabled: true,
    }
}

fn control_plane(clock: Arc<ManualClock>) -> Arc<ControlPlane> {
    Arc::new(ControlPlane::new(
        Arc::new(MemoryControlPlaneStore::new()),
        clock,
    ))
}

fn token_spec(nonce: Option<&str>, expires_at_ms: i64) -> MockTokenSpec {
    MockTokenSpec {
        subject: "sub-1".into(),
        email: "user@example.test".into(),
        groups: vec!["faktor-admins".into(), "faktor-viewers".into()],
        nonce: nonce.map(str::to_string),
        expires_at_ms,
    }
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            return Some(value.to_string());
        }
    }
    None
}

// -------------------------------------------------------------------- tests

/// The full network flow: discovery (cached by max-age), code exchange,
/// RS256 verification against fetched JWKS, claim mapping and the minted
/// control-plane session.
#[tokio::test]
async fn network_happy_path_caches_discovery_and_mints_a_session() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock.clone());
    let cp = control_plane(clock.clone());
    let boot = cp
        .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-1")
        .unwrap();
    let org = boot.organization;
    let login = SsoLogin::new(Arc::new(adapter), clock.clone());

    let started = login
        .start(&org.id, &sso_ref(), "https://app.example/callback")
        .await
        .unwrap();
    assert!(started.authorization_url.starts_with(ISSUER));
    assert_eq!(
        query_param(&started.authorization_url, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(query_param(&started.authorization_url, "code_challenge").is_some());
    let nonce = query_param(&started.authorization_url, "nonce").unwrap();
    assert_eq!(nonce, started.nonce, "the URL nonce is the login nonce");

    provider.issue_code("code-1", token_spec(Some(&started.nonce), NOW_MS + 60_000));
    let outcome = login
        .callback(
            &cp,
            &org.id,
            &sso_ref(),
            "https://app.example/callback",
            "code-1",
            &started.state,
        )
        .await
        .unwrap();
    assert_eq!(outcome.membership.role, Role::Admin, "highest group wins");
    let principal = cp.authenticate(outcome.login.token.expose()).unwrap();
    assert_eq!(principal.role, Role::Admin);
    assert_eq!(principal.organization, org.id);

    // Discovery was fetched once and served from cache for the callback.
    assert_eq!(
        provider.discovery_requests.load(Ordering::SeqCst),
        1,
        "max-age caching serves the second discovery from cache"
    );
    assert_eq!(provider.token_requests.load(Ordering::SeqCst), 1);

    // The state is single use: replaying the callback is refused.
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-1",
                &started.state
            )
            .await,
        Err(OidcError::StateInvalid)
    ));
}

/// Wrong state, wrong nonce and a swapped redirect URI are refused before any
/// session exists; the state is consumed exactly once.
#[tokio::test]
async fn wrong_state_nonce_and_redirect_are_refused() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock.clone());
    let cp = control_plane(clock.clone());
    let boot = cp
        .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-2")
        .unwrap();
    let org = boot.organization;
    let login = SsoLogin::new(Arc::new(adapter), clock);

    // Unknown state.
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-x",
                "state-does-not-exist"
            )
            .await,
        Err(OidcError::StateInvalid)
    ));

    // Wrong nonce: the provider issues the token for a different nonce.
    let started = login
        .start(&org.id, &sso_ref(), "https://app.example/callback")
        .await
        .unwrap();
    provider.issue_code("code-2", token_spec(Some("nonce-other"), NOW_MS + 60_000));
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-2",
                &started.state
            )
            .await,
        Err(OidcError::NonceMismatch)
    ));

    // The redirect URI must equal the one the login started with.
    let started = login
        .start(&org.id, &sso_ref(), "https://app.example/callback")
        .await
        .unwrap();
    provider.issue_code("code-3", token_spec(Some(&started.nonce), NOW_MS + 60_000));
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://evil.example/callback",
                "code-3",
                &started.state
            )
            .await,
        Err(OidcError::RedirectMismatch)
    ));
    // That state was consumed by the mismatching attempt (single use).
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-3",
                &started.state
            )
            .await,
        Err(OidcError::StateInvalid)
    ));
}

/// An expired ID token is refused typed; the state is still single use.
#[tokio::test]
async fn expired_id_token_is_refused() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock.clone());
    let cp = control_plane(clock.clone());
    let boot = cp
        .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-3")
        .unwrap();
    let org = boot.organization;
    let login = SsoLogin::new(Arc::new(adapter), clock);
    let started = login
        .start(&org.id, &sso_ref(), "https://app.example/callback")
        .await
        .unwrap();
    provider.issue_code("code-4", token_spec(Some(&started.nonce), NOW_MS - 1_000));
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-4",
                &started.state
            )
            .await,
        Err(OidcError::Expired)
    ));
}

/// JWKS rotation: a token signed by the NEW kid verifies after the adapter
/// refetches; a token under a rotated-out kid is refused.
#[tokio::test]
async fn jwks_rotation_refetches_and_refuses_rotated_out_kids() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock);
    let expectations = IdTokenExpectations {
        issuer: ISSUER.into(),
        audience: CLIENT.into(),
        nonce: None,
        now_ms: NOW_MS,
        clock_skew_ms: 0,
    };
    let old_token = provider.sign(&token_spec(None, NOW_MS + 60_000));
    adapter
        .verify_id_token(&old_token, &expectations)
        .await
        .unwrap();

    provider.rotate("kid-2");
    let new_token = provider.sign(&token_spec(None, NOW_MS + 60_000));
    adapter
        .verify_id_token(&new_token, &expectations)
        .await
        .expect("the rotated-in kid verifies after a refetch");

    // The rotated-out kid no longer exists in the JWKS: typed UnknownKey.
    let forged_old = provider.sign_with_kid_label("kid-1", &token_spec(None, NOW_MS + 60_000));
    assert!(matches!(
        adapter
            .verify_id_token(&forged_old, &expectations)
            .await
            .unwrap_err(),
        OidcError::UnknownKey(kid) if kid == "kid-1"
    ));
    let view = adapter.jwks_view().await.unwrap();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].kid, "kid-2");
}

/// An unknown `kid` triggers at most `max_jwks_refetches` forced refetches
/// PER verification and then the typed refusal — never an unbounded refetch
/// loop.
#[tokio::test]
async fn unknown_kid_refetch_is_bounded() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock);
    let expectations = IdTokenExpectations {
        issuer: ISSUER.into(),
        audience: CLIENT.into(),
        nonce: None,
        now_ms: NOW_MS,
        clock_skew_ms: 0,
    };
    let ghost = provider.sign_with_kid_label("kid-ghost", &token_spec(None, NOW_MS + 60_000));
    // First verification: initial JWKS load + 2 forced refetches = 3.
    assert!(matches!(
        adapter
            .verify_id_token(&ghost, &expectations)
            .await
            .unwrap_err(),
        OidcError::UnknownKey(kid) if kid == "kid-ghost"
    ));
    assert_eq!(provider.jwks_requests.load(Ordering::SeqCst), 3);
    // Second verification: the cache is fresh but the kid is still unknown:
    // again exactly the bounded refetches, no compounding.
    assert!(adapter
        .verify_id_token(&ghost, &expectations)
        .await
        .is_err());
    assert_eq!(provider.jwks_requests.load(Ordering::SeqCst), 6);
    assert_eq!(provider.request_count("/jwks"), 6);
}

/// The discovery cache honors the RESPONSE max-age (clamped by config):
/// within the window a hit is served with no request; past it the document is
/// refetched.
#[tokio::test]
async fn discovery_cache_honors_max_age() {
    let provider = Arc::new(MockProvider::new());
    provider.state.lock().unwrap().discovery_max_age_s = Some(1);
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock.clone());
    adapter.discovery(ISSUER).await.unwrap();
    adapter.discovery(ISSUER).await.unwrap();
    assert_eq!(provider.discovery_requests.load(Ordering::SeqCst), 1);
    clock.advance(2_000);
    adapter.discovery(ISSUER).await.unwrap();
    assert_eq!(
        provider.discovery_requests.load(Ordering::SeqCst),
        2,
        "the max-age window expired: the document is refetched"
    );
    // A foreign issuer is refused before any request.
    assert!(adapter.discovery("https://other.example").await.is_err());
    assert_eq!(provider.discovery_requests.load(Ordering::SeqCst), 2);
}

/// The sync fake implements the same async seam through the blanket
/// forwarding (the server route surface drives the fake in tests), and the
/// network adapter's mode is parseable end-to-end.
#[tokio::test]
async fn fake_adapter_is_usable_through_the_async_seam() {
    let fake = Arc::new(FakeOidcAdapter::new(ISSUER, CLIENT, b"secret-one"));
    fake.issue_code(
        "code-fake",
        OidcClaims {
            issuer: ISSUER.into(),
            subject: "sub-1".into(),
            audience: vec![CLIENT.into()],
            email: Some("user@example.test".into()),
            email_verified: true,
            issued_at_ms: NOW_MS - 1_000,
            expires_at_ms: NOW_MS + 60_000,
            nonce: Some("nonce-1".into()),
            groups: vec!["faktor-admins".into()],
        },
    );
    let tokens = fake
        .exchange_code(&CodeExchangeRequest {
            code: "code-fake".into(),
            redirect_uri: "https://app.example/callback".into(),
            code_verifier: "verifier".into(),
        })
        .await
        .unwrap();
    let claims = fake
        .verify_id_token(
            &tokens.id_token,
            &IdTokenExpectations {
                issuer: ISSUER.into(),
                audience: CLIENT.into(),
                nonce: Some("nonce-1".into()),
                now_ms: NOW_MS,
                clock_skew_ms: 0,
            },
        )
        .await
        .unwrap();
    let membership = fake
        .map_membership(
            &claims,
            &ClaimMapping {
                email_claim: "email".into(),
                groups_claim: "groups".into(),
                group_roles: BTreeMap::from([("faktor-admins".to_string(), Role::Admin)]),
                default_role: Role::Viewer,
                require_verified_email: true,
            },
        )
        .unwrap();
    assert_eq!(membership.role, Role::Admin);
}

/// The organization minted by the login is the one the pending state was
/// bound to; a different organization can never consume another's state.
#[tokio::test]
async fn callback_is_bound_to_the_starting_organization() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock.clone());
    let cp = control_plane(clock.clone());
    let org_a = cp
        .bootstrap_organization("a", "a@acme.test", "A", "boot-a")
        .unwrap()
        .organization;
    let org_b = cp
        .bootstrap_organization("b", "b@acme.test", "B", "boot-b")
        .unwrap()
        .organization;
    let login = SsoLogin::new(Arc::new(adapter), clock);
    let started = login
        .start(&org_a.id, &sso_ref(), "https://app.example/callback")
        .await
        .unwrap();
    provider.issue_code("code-5", token_spec(Some(&started.nonce), NOW_MS + 60_000));
    assert!(matches!(
        login
            .callback(
                &cp,
                &org_b.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-5",
                &started.state
            )
            .await,
        Err(OidcError::StateInvalid)
    ));
}
