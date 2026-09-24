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
    AsyncOidcAdapter, ClaimMapping, ClientAuthMethod, ClientSecret, CodeExchangeRequest,
    ControlPlane, FakeOidcAdapter, IdTokenExpectations, ManualClock, MemoryControlPlaneStore,
    NetworkOidcAdapter, NetworkOidcConfig, OidcClaims, OidcError, Role, SsoConfigRef, SsoLogin,
    OIDC_NETWORK_TIMEOUT_MS,
};
use faktor_provider::egress::{CheckedResponse, EgressError, HttpTransport};

const ISSUER: &str = "https://idp.example";
const CLIENT: &str = "faktor-test";
const NOW_MS: i64 = 1_700_000_000_000;

// ------------------------------------------------------------ mock provider

struct MockKey {
    kid: String,
    /// The RSA keypair (`None` for `oct` symmetric keys).
    keypair: Option<RsaKeyPair>,
    n: Vec<u8>,
    e: Vec<u8>,
    /// The `oct` shared secret (`None` for RSA keys).
    secret: Option<Vec<u8>>,
    /// The JWKS `alg` this key advertises.
    alg: String,
    /// The JWKS `use` field (absent = `None`).
    use_: Option<String>,
    /// The JWKS `key_ops` field (absent = `None`).
    key_ops: Option<Vec<String>>,
}

struct ProviderState {
    keys: Vec<MockKey>,
    active: usize,
    /// code -> (subject, email, groups, nonce)
    codes: BTreeMap<String, MockTokenSpec>,
    discovery_max_age_s: Option<i64>,
    jwks_max_age_s: Option<i64>,
    /// The discovery document's `id_token_signing_alg_values_supported`.
    discovery_algs: Vec<String>,
}

#[derive(Clone)]
enum MockAudience {
    Single(String),
    Multi(Vec<String>),
}

#[derive(Clone)]
struct MockTokenSpec {
    subject: String,
    email: String,
    groups: Vec<String>,
    nonce: Option<String>,
    expires_at_ms: i64,
    audience: MockAudience,
    azp: Option<String>,
    /// Raw `exp` seconds override (extreme-value tests).
    exp_s: Option<i64>,
    /// Raw `iat` seconds override (extreme-value tests).
    iat_s: Option<i64>,
}

impl MockTokenSpec {
    /// The JSON payload signed for this spec.
    fn payload(&self) -> serde_json::Value {
        let aud = match &self.audience {
            MockAudience::Single(value) => serde_json::Value::String(value.clone()),
            MockAudience::Multi(values) => serde_json::Value::Array(
                values
                    .iter()
                    .map(|value| serde_json::Value::String(value.clone()))
                    .collect(),
            ),
        };
        let seconds = self.expires_at_ms / 1000;
        let exp = self.exp_s.unwrap_or(seconds);
        let iat = self.iat_s.unwrap_or(seconds - 60);
        let mut payload = serde_json::json!({
            "iss": ISSUER,
            "sub": self.subject,
            "aud": aud,
            "email": self.email,
            "email_verified": true,
            "iat": iat,
            "exp": exp,
            "nonce": self.nonce,
            "groups": self.groups,
        });
        if let Some(azp) = &self.azp {
            payload["azp"] = serde_json::Value::String(azp.clone());
        }
        payload
    }
}

struct MockProvider {
    state: Mutex<ProviderState>,
    requests: Mutex<Vec<(String, String)>>,
    token_request_detail: Mutex<Option<TokenRequestDetail>>,
    discovery_requests: AtomicUsize,
    jwks_requests: AtomicUsize,
    token_requests: AtomicUsize,
}

/// The headers + body of one token-endpoint request.
type TokenRequestDetail = (Vec<(String, String)>, Vec<u8>);

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
        keypair: Some(keypair),
        n,
        e,
        secret: None,
        alg: "RS256".into(),
        use_: Some("sig".into()),
        key_ops: None,
    }
}

/// One `oct` symmetric key with the given JWKS metadata.
fn oct_key(kid: &str, secret: &[u8], use_: Option<&str>, key_ops: Option<&[&str]>) -> MockKey {
    MockKey {
        kid: kid.to_string(),
        keypair: None,
        n: Vec::new(),
        e: Vec::new(),
        secret: Some(secret.to_vec()),
        alg: "HS256".into(),
        use_: use_.map(str::to_string),
        key_ops: key_ops.map(|ops| ops.iter().map(|op| op.to_string()).collect()),
    }
}

/// One RSA key with overridden JWKS metadata (conflicting `alg`/`use`/
/// `key_ops` adversarial cases).
fn rsa_key_with(kid: &str, alg: &str, use_: Option<&str>, key_ops: Option<&[&str]>) -> MockKey {
    MockKey {
        alg: alg.to_string(),
        use_: use_.map(str::to_string),
        key_ops: key_ops.map(|ops| ops.iter().map(|op| op.to_string()).collect()),
        ..generate_key(kid)
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
                discovery_algs: vec!["RS256".into()],
            }),
            requests: Mutex::new(Vec::new()),
            token_request_detail: Mutex::new(None),
            discovery_requests: AtomicUsize::new(0),
            jwks_requests: AtomicUsize::new(0),
            token_requests: AtomicUsize::new(0),
        }
    }

    /// Install one pre-built key (with arbitrary metadata) as the ONLY
    /// active key.
    fn push_key(&self, key: MockKey) {
        let mut state = self.state.lock().unwrap();
        state.keys.push(key);
        state.active = state.keys.len() - 1;
    }

    /// The discovery document's advertised signing algorithms.
    fn set_discovery_algs(&self, algs: &[&str]) {
        let mut state = self.state.lock().unwrap();
        state.discovery_algs = algs.iter().map(|alg| alg.to_string()).collect();
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

    /// The headers + body of the LAST token-endpoint request (the
    /// confidential-client tests assert the configured method on the wire).
    fn token_request(&self) -> (Vec<(String, String)>, Vec<u8>) {
        self.token_request_detail
            .lock()
            .unwrap()
            .clone()
            .expect("a token request was recorded")
    }

    /// Sign a claims set under the ACTIVE key.
    fn sign(&self, spec: &MockTokenSpec) -> String {
        let state = self.state.lock().unwrap();
        let key = &state.keys[state.active];
        sign_claims(key, &key.kid, spec)
    }

    /// Sign an arbitrary raw payload under the ACTIVE key (malformed-claim
    /// adversarial cases).
    fn sign_payload(&self, payload: &serde_json::Value) -> String {
        let state = self.state.lock().unwrap();
        let key = &state.keys[state.active];
        sign_raw_payload(key, &key.kid, payload)
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
        request_headers: &[(String, String)],
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
                "id_token_signing_alg_values_supported": self.state.lock().unwrap().discovery_algs.clone(),
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
                    let mut value = serde_json::json!({
                        "kty": if key.secret.is_some() { "oct" } else { "RSA" },
                        "kid": key.kid,
                        "alg": key.alg,
                    });
                    if let Some(use_) = &key.use_ {
                        value["use"] = serde_json::Value::String(use_.clone());
                    }
                    if let Some(key_ops) = &key.key_ops {
                        value["key_ops"] = serde_json::Value::Array(
                            key_ops
                                .iter()
                                .map(|op| serde_json::Value::String(op.clone()))
                                .collect(),
                        );
                    }
                    match &key.secret {
                        Some(secret) => value["k"] = serde_json::Value::String(b64url(secret)),
                        None => {
                            value["n"] = serde_json::Value::String(b64url(&key.n));
                            value["e"] = serde_json::Value::String(b64url(&key.e));
                        }
                    }
                    value
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
            *self.token_request_detail.lock().unwrap() =
                Some((request_headers.to_vec(), body.to_vec()));
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
    sign_raw_payload(key, kid, &spec.payload())
}

fn sign_raw_payload(key: &MockKey, kid: &str, payload: &serde_json::Value) -> String {
    // The header `alg` is the algorithm the signature is ACTUALLY produced
    // with (RS256 for RSA material, HS256 for oct material); the JWKS may
    // advertise conflicting metadata, which the policy must refuse.
    let alg = if key.secret.is_some() {
        "HS256"
    } else {
        "RS256"
    };
    let header = serde_json::json!({"alg": alg, "kid": kid, "typ": "JWT"});
    let header = b64url(&serde_json::to_vec(&header).unwrap());
    let payload = b64url(&serde_json::to_vec(payload).unwrap());
    let input = format!("{header}.{payload}");
    match (&key.secret, &key.keypair) {
        (Some(secret), _) => {
            let signature = hmac_sha256(secret, input.as_bytes());
            format!("{input}.{}", b64url(&signature))
        }
        (None, Some(keypair)) => {
            let rng = SystemRandom::new();
            let mut signature = vec![0u8; keypair.public().modulus_len()];
            keypair
                .sign(&RSA_PKCS1_SHA256, &rng, input.as_bytes(), &mut signature)
                .expect("sign");
            format!("{input}.{}", b64url(&signature))
        }
        (None, None) => panic!("a mock key needs RSA or oct material"),
    }
}

/// A JWT with header `alg = "none"` and an empty signature (never acceptable).
fn alg_none_token(spec: &MockTokenSpec) -> String {
    let header = b64url(br#"{"alg":"none","kid":"kid-1","typ":"JWT"}"#);
    let payload = b64url(&serde_json::to_vec(&spec.payload()).unwrap());
    format!("{header}.{payload}.")
}

fn hmac_sha256(secret: &[u8], message: &[u8]) -> Vec<u8> {
    ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret),
        message,
    )
    .as_ref()
    .to_vec()
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
    fn execute(&self, req: Request) -> BoxFuture<'_, Result<CheckedResponse, EgressError>> {
        let method = req.method().to_string();
        let url = req.url().to_string();
        let headers: Vec<(String, String)> = req
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect();
        let body = req
            .body()
            .and_then(|b| b.as_bytes())
            .unwrap_or(&[])
            .to_vec();
        let (status, headers, body) = self.handle(&method, &url, &headers, &body);
        Box::pin(async move {
            let mut builder = http::Response::builder().status(status);
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            let response = builder
                .body(Body::from(body))
                .map_err(|e| EgressError::Build(e.to_string()))?;
            Ok(CheckedResponse::from_response(Response::from(response)))
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
        client_auth: ClientAuthMethod::None,
        client_secret: None,
        allowed_algorithms: vec!["RS256".into()],
    }
}

/// The same strict config with a confidential client method + secret.
fn confidential_config(method: ClientAuthMethod, secret: &str) -> NetworkOidcConfig {
    NetworkOidcConfig {
        client_auth: method,
        client_secret: Some(ClientSecret::new(secret).unwrap()),
        ..config()
    }
}

fn adapter(provider: Arc<MockProvider>, clock: Arc<ManualClock>) -> NetworkOidcAdapter {
    NetworkOidcAdapter::new(provider, clock, config()).unwrap()
}

/// The strict config with an explicit allowed-algorithm policy.
fn config_with_algs(algs: &[&str]) -> NetworkOidcConfig {
    NetworkOidcConfig {
        allowed_algorithms: algs.iter().map(|alg| alg.to_string()).collect(),
        ..config()
    }
}

fn adapter_with_algs(
    provider: Arc<MockProvider>,
    clock: Arc<ManualClock>,
    algs: &[&str],
) -> NetworkOidcAdapter {
    NetworkOidcAdapter::new(provider, clock, config_with_algs(algs)).unwrap()
}

fn expectations(nonce: Option<&str>, now_ms: i64) -> IdTokenExpectations {
    IdTokenExpectations {
        issuer: ISSUER.into(),
        audience: CLIENT.into(),
        nonce: nonce.map(|n| n.into()),
        now_ms,
        clock_skew_ms: 0,
    }
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
        audience: MockAudience::Single(CLIENT.into()),
        azp: None,
        exp_s: None,
        iat_s: None,
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

/// The wire projection of a started login (P2-B): the authorization URL plus
/// the plaintext state for the frontend — the domain type exposes neither.
fn wire_of(start: faktor_cloud::sso::SsoStart) -> faktor_cloud::sso::SsoStartWireResponse {
    start.into_wire_response()
}

/// The nonce of a started login, read from its authorization URL (the domain
/// type never exposes the wrapped nonce).
fn nonce_of(wire: &faktor_cloud::sso::SsoStartWireResponse) -> String {
    query_param(wire.authorization_url(), "nonce").expect("the URL carries the nonce")
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
    let started = wire_of(started);
    assert!(started.authorization_url().starts_with(ISSUER));
    assert_eq!(
        query_param(started.authorization_url(), "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(query_param(started.authorization_url(), "code_challenge").is_some());
    let nonce = nonce_of(&started);
    assert!(
        started
            .authorization_url()
            .contains(&format!("nonce={nonce}")),
        "the URL nonce is the login nonce"
    );

    provider.issue_code("code-1", token_spec(Some(&nonce), NOW_MS + 60_000));
    let outcome = login
        .callback(
            &cp,
            &org.id,
            &sso_ref(),
            "https://app.example/callback",
            "code-1",
            started.state(),
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
                started.state()
            )
            .await,
        Err(OidcError::StateInvalid)
    ));
}

/// The SSO flow is subject-first: an IdP email rename between two callbacks
/// still logs into the SAME linked user and adopts the new verified email
/// (the verified claim is what the service policy consumes).
#[tokio::test]
async fn sso_email_rename_follows_the_linked_subject() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock.clone());
    let cp = control_plane(clock.clone());
    let boot = cp
        .bootstrap_organization("acme", "owner@acme.test", "Owner", "boot-rename")
        .unwrap();
    let org = boot.organization;
    let login = SsoLogin::new(Arc::new(adapter), clock);

    let started = login
        .start(&org.id, &sso_ref(), "https://app.example/callback")
        .await
        .unwrap();
    let started = wire_of(started);
    provider.issue_code(
        "code-r1",
        token_spec(Some(&nonce_of(&started)), NOW_MS + 60_000),
    );
    let first = login
        .callback(
            &cp,
            &org.id,
            &sso_ref(),
            "https://app.example/callback",
            "code-r1",
            started.state(),
        )
        .await
        .unwrap();
    let user = first.login.user.id.clone();
    assert_eq!(first.login.user.email, "user@example.test");

    let started = login
        .start(&org.id, &sso_ref(), "https://app.example/callback")
        .await
        .unwrap();
    let started = wire_of(started);
    let mut renamed = token_spec(Some(&nonce_of(&started)), NOW_MS + 60_000);
    renamed.email = "renamed@example.test".into();
    provider.issue_code("code-r2", renamed);
    let second = login
        .callback(
            &cp,
            &org.id,
            &sso_ref(),
            "https://app.example/callback",
            "code-r2",
            started.state(),
        )
        .await
        .unwrap();
    assert_eq!(
        second.login.user.id, user,
        "the subject link decides the account"
    );
    assert_eq!(second.login.user.email, "renamed@example.test");
    assert_eq!(second.login.role, Role::Admin);
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
    let started = wire_of(started);
    provider.issue_code("code-2", token_spec(Some("nonce-other"), NOW_MS + 60_000));
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-2",
                started.state()
            )
            .await,
        Err(OidcError::NonceMismatch)
    ));

    // The redirect URI must equal the one the login started with.
    let started = login
        .start(&org.id, &sso_ref(), "https://app.example/callback")
        .await
        .unwrap();
    let started = wire_of(started);
    provider.issue_code(
        "code-3",
        token_spec(Some(&nonce_of(&started)), NOW_MS + 60_000),
    );
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://evil.example/callback",
                "code-3",
                started.state()
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
                started.state()
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
    let started = wire_of(started);
    provider.issue_code(
        "code-4",
        token_spec(Some(&nonce_of(&started)), NOW_MS - 1_000),
    );
    assert!(matches!(
        login
            .callback(
                &cp,
                &org.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-4",
                started.state()
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

/// ONE adapter serves both client kinds: the public-PKCE exchange form is
/// byte-identical to the pre-confidential shape and carries no credential.
#[tokio::test]
async fn public_exchange_form_is_byte_identical_and_unauthenticated() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock);
    provider.issue_code("code-public", token_spec(None, NOW_MS + 60_000));
    let tokens = adapter
        .exchange_code(&exchange("code-public"))
        .await
        .unwrap();
    assert_eq!(tokens.token_type, "Bearer");
    let (headers, body) = provider.token_request();
    assert_eq!(
        String::from_utf8(body).unwrap(),
        "grant_type=authorization_code&code=code-public&redirect_uri=https%3A%2F%2Fapp.example%2Fcallback&code_verifier=verifier&client_id=faktor-test",
        "the public path's form must not change"
    );
    assert!(headers.iter().all(|(name, _)| name != "authorization"));
}

/// `client_secret_post`: the configured secret rides the form; it is
/// redacted everywhere it could be rendered or logged.
#[tokio::test]
async fn confidential_post_sends_the_secret_in_the_form_and_never_renders_it() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let config = confidential_config(ClientAuthMethod::ClientSecretPost, "top-secret");
    let adapter = NetworkOidcAdapter::new(provider.clone(), clock, config.clone()).unwrap();
    provider.issue_code("code-post", token_spec(None, NOW_MS + 60_000));
    let tokens = adapter.exchange_code(&exchange("code-post")).await.unwrap();
    assert!(!tokens.id_token.is_empty());
    let (headers, body) = provider.token_request();
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("client_id=faktor-test"), "{body}");
    assert!(body.contains("client_secret=top-secret"), "{body}");
    assert!(body.contains("code_verifier=verifier"), "{body}");
    assert!(headers.iter().all(|(name, _)| name != "authorization"));

    // A refused exchange never echoes the secret...
    let error = adapter
        .exchange_code(&exchange("code-never-issued"))
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("top-secret"), "{error}");
    // ...and neither Debug nor Display ever renders it.
    let secret = config.client_secret.clone().unwrap();
    for rendered in [
        format!("{adapter:?}"),
        format!("{config:?}"),
        format!("{secret}"),
        format!("{secret:?}"),
    ] {
        assert!(!rendered.contains("top-secret"), "{rendered}");
    }
    assert!(format!("{secret:?}").contains("redacted"));
    assert_eq!(format!("{secret}"), "<redacted>");
}

/// `client_secret_basic`: the secret rides the RFC 6749 §2.3.1 Basic header
/// (urlencoded credentials, standard base64) and NEVER the form.
#[tokio::test]
async fn confidential_basic_sends_the_basic_header_and_keeps_the_form_clean() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let config = confidential_config(ClientAuthMethod::ClientSecretBasic, "top/secret?");
    let adapter = NetworkOidcAdapter::new(provider.clone(), clock, config).unwrap();
    provider.issue_code("code-basic", token_spec(None, NOW_MS + 60_000));
    adapter
        .exchange_code(&exchange("code-basic"))
        .await
        .unwrap();
    let (headers, body) = provider.token_request();
    let expected = base64::engine::general_purpose::STANDARD.encode(b"faktor-test:top%2Fsecret%3F");
    let authorization = headers
        .iter()
        .find(|(name, _)| name == "authorization")
        .map(|(_, value)| value.clone())
        .expect("the basic method carries an authorization header");
    assert_eq!(authorization, format!("Basic {expected}"));
    let body = String::from_utf8(body).unwrap();
    assert!(!body.contains("client_secret"), "{body}");
    assert!(!body.contains("client_id"), "{body}");
    assert!(!body.contains("top"), "{body}");
}

/// A confidential method without a secret (and a secret without a method)
/// fails closed typed at construction, BEFORE any request could be built.
#[tokio::test]
async fn confidential_config_contradictions_fail_closed_typed() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let err = NetworkOidcAdapter::new(
        provider.clone(),
        clock.clone(),
        NetworkOidcConfig {
            client_auth: ClientAuthMethod::ClientSecretPost,
            ..config()
        },
    )
    .unwrap_err();
    assert!(matches!(err, OidcError::DiscoveryUnavailable(_)));
    assert!(err.to_string().contains("client_secret_post"), "{err}");
    let err = NetworkOidcAdapter::new(
        provider.clone(),
        clock,
        NetworkOidcConfig {
            client_auth: ClientAuthMethod::None,
            client_secret: Some(ClientSecret::new("s3cret").unwrap()),
            ..config()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("client_auth"), "{err}");
    assert!(!err.to_string().contains("s3cret"), "{err}");
    assert_eq!(provider.request_count(""), 0, "no request was ever sent");
}

#[test]
fn client_secret_shape_is_strict_and_never_parsed_loosely() {
    assert!(ClientSecret::new("").is_err());
    assert!(ClientSecret::new("two words").is_err());
    assert!(ClientSecret::new("line\nbreak").is_err());
    assert!(ClientSecret::new("x".repeat(4097)).is_err());
    assert!(ClientSecret::new("x".repeat(4096)).is_ok());
    // The refusal never echoes the candidate value.
    let err = ClientSecret::new("hunter2 hunter2").unwrap_err();
    assert!(!err.to_string().contains("hunter2"), "{err}");
    assert_eq!(
        faktor_cloud::ClientAuthMethod::parse("client_secret_post"),
        Some(ClientAuthMethod::ClientSecretPost)
    );
    assert_eq!(
        faktor_cloud::ClientAuthMethod::parse("client_secret_basic"),
        Some(ClientAuthMethod::ClientSecretBasic)
    );
    assert_eq!(
        faktor_cloud::ClientAuthMethod::parse("none"),
        Some(ClientAuthMethod::None)
    );
    assert_eq!(
        faktor_cloud::ClientAuthMethod::parse("CLIENT_SECRET_POST"),
        None
    );
    assert_eq!(
        faktor_cloud::ClientAuthMethod::parse("client_secret_jwt"),
        None
    );
}

fn exchange(code: &str) -> CodeExchangeRequest {
    CodeExchangeRequest {
        code: code.into(),
        redirect_uri: "https://app.example/callback".into(),
        code_verifier: "verifier".into(),
    }
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
            tokens.id_token.expose(),
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
    let started = wire_of(started);
    provider.issue_code(
        "code-5",
        token_spec(Some(&nonce_of(&started)), NOW_MS + 60_000),
    );
    assert!(matches!(
        login
            .callback(
                &cp,
                &org_b.id,
                &sso_ref(),
                "https://app.example/callback",
                "code-5",
                started.state()
            )
            .await,
        Err(OidcError::StateInvalid)
    ));
}

// ------------------------------------------------- adversarial: alg policy

/// Build one single-purpose provider + adapter and verify one token.
async fn verify_with_meta(
    key: MockKey,
    discovery_algs: &[&str],
    configured_algs: &[&str],
    spec: &MockTokenSpec,
) -> Result<OidcClaims, OidcError> {
    let provider = Arc::new(MockProvider::new());
    provider.push_key(key);
    provider.set_discovery_algs(discovery_algs);
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter_with_algs(provider.clone(), clock, configured_algs);
    let token = provider.sign(spec);
    adapter
        .verify_id_token(&token, &expectations(None, NOW_MS))
        .await
}

/// A header `alg` outside the discovery document's advertised set is refused
/// typed, and the refusal names every set in the intersection.
#[tokio::test]
async fn algorithm_not_advertised_by_discovery_is_refused_naming_each_set() {
    let err = verify_with_meta(
        oct_key("kid-oct", b"shared-secret", Some("sig"), None),
        &["RS256"],
        &["RS256", "HS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap_err();
    let rendered = err.to_string();
    match err {
        OidcError::AlgorithmRefused {
            alg,
            discovery,
            jwk,
            configured,
        } => {
            assert_eq!(alg, "HS256");
            assert_eq!(discovery, ["RS256"]);
            assert_eq!(jwk, ["HS256"]);
            assert_eq!(configured, ["RS256", "HS256"]);
        }
        other => panic!("expected AlgorithmRefused, got {other:?}"),
    }
    for needle in [
        "HS256",
        "RS256",
        "discovery",
        "signing key",
        "configuration",
    ] {
        assert!(rendered.contains(needle), "{rendered}");
    }
}

/// The header `alg` being advertised by discovery is not enough: the JWK's
/// own `alg`/`use`/`key_ops` constraints must also permit it, and a
/// consistent JWK passes.
#[tokio::test]
async fn alg_in_discovery_but_conflicting_with_jwk_metadata_is_refused() {
    // The JWK advertises RS384 while the token is actually RS256-signed.
    let err = verify_with_meta(
        rsa_key_with("kid-1", "RS384", Some("sig"), None),
        &["RS256"],
        &["RS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, OidcError::AlgorithmRefused { ref jwk, .. } if jwk.is_empty()),
        "{err}"
    );

    // use = enc never verifies signatures.
    let err = verify_with_meta(
        rsa_key_with("kid-1", "RS256", Some("enc"), None),
        &["RS256"],
        &["RS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OidcError::AlgorithmRefused { ref jwk, .. } if jwk.is_empty()));

    // key_ops without `verify` never verifies signatures.
    let err = verify_with_meta(
        rsa_key_with("kid-1", "RS256", Some("sig"), Some(&["encrypt"])),
        &["RS256"],
        &["RS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OidcError::AlgorithmRefused { ref jwk, .. } if jwk.is_empty()));

    // A fully consistent JWK (matching alg, use = sig, key_ops = verify)
    // passes the same policy.
    let claims = verify_with_meta(
        rsa_key_with("kid-1", "RS256", Some("sig"), Some(&["verify"])),
        &["RS256"],
        &["RS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap();
    assert_eq!(claims.subject, "sub-1");
}

/// Symmetric HS256 is accepted only when the operator explicitly allows it
/// AND the JWK is an `oct` key declaring `use = "sig"`; every weaker shape
/// is refused and `alg = none` is never acceptable.
#[tokio::test]
async fn symmetric_hs256_is_accepted_only_under_the_explicit_config_with_an_oct_sig_jwk() {
    // Positive: explicit config + discovery + oct/use=sig.
    let claims = verify_with_meta(
        oct_key("kid-oct", b"shared-secret", Some("sig"), None),
        &["HS256"],
        &["RS256", "HS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap();
    assert_eq!(claims.subject, "sub-1");

    // The SAME token under the default policy (RS256 only) is refused.
    let err = verify_with_meta(
        oct_key("kid-oct", b"shared-secret", Some("sig"), None),
        &["HS256"],
        &["RS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, OidcError::AlgorithmRefused { ref configured, .. } if configured == &["RS256"]),
        "{err}"
    );

    // use = enc is refused even with the explicit config...
    let err = verify_with_meta(
        oct_key("kid-oct", b"shared-secret", Some("enc"), None),
        &["HS256"],
        &["RS256", "HS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OidcError::AlgorithmRefused { ref jwk, .. } if jwk.is_empty()));

    // ...and so is an oct key that never declares `use = "sig"`.
    let err = verify_with_meta(
        oct_key("kid-oct", b"shared-secret", None, None),
        &["HS256"],
        &["RS256", "HS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OidcError::AlgorithmRefused { ref jwk, .. } if jwk.is_empty()));

    // A key_ops that omits `verify` is refused too.
    let err = verify_with_meta(
        oct_key("kid-oct", b"shared-secret", Some("sig"), Some(&["sign"])),
        &["HS256"],
        &["RS256", "HS256"],
        &token_spec(None, NOW_MS + 60_000),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OidcError::AlgorithmRefused { ref jwk, .. } if jwk.is_empty()));

    // `alg = "none"` with an empty signature is never accepted.
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider, clock);
    let token = alg_none_token(&token_spec(None, NOW_MS + 60_000));
    let err = adapter
        .verify_id_token(&token, &expectations(None, NOW_MS))
        .await
        .unwrap_err();
    assert!(
        matches!(err, OidcError::AlgorithmRefused { ref alg, .. } if alg == "none"),
        "{err}"
    );
}

/// The configured algorithm policy is strict at construction: empty lists,
/// `none` and unsupported names are refused before any request; the default
/// is RS256-only.
#[test]
fn configured_algorithm_policy_is_strict_at_construction() {
    assert!(NetworkOidcConfig::validate_allowed_algorithms(&[]).is_err());
    assert!(NetworkOidcConfig::validate_allowed_algorithms(&["none".to_string()]).is_err());
    assert!(NetworkOidcConfig::validate_allowed_algorithms(&["HS512".to_string()]).is_err());
    assert!(NetworkOidcConfig::validate_allowed_algorithms(&["hs256".to_string()]).is_err());
    assert!(NetworkOidcConfig::validate_allowed_algorithms(&[
        "RS256".to_string(),
        "HS256".to_string(),
    ])
    .is_ok());
    assert_eq!(NetworkOidcConfig::default().allowed_algorithms, ["RS256"]);
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let err = NetworkOidcAdapter::new(provider, clock, config_with_algs(&["none"])).unwrap_err();
    assert!(err.to_string().contains("none"), "{err}");
}

// --------------------------------------------------------- adversarial: azp

/// An `azp` claim that contradicts the expected client is refused, a
/// multi-valued `aud` REQUIRES `azp == client_id`, and a single-valued `aud`
/// with a present `azp` must still match.
#[tokio::test]
async fn multi_valued_audience_requires_matching_azp_and_azp_always_must_match() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock);
    let multi = MockAudience::Multi(vec![CLIENT.into(), "other-client".into()]);

    // Multi-valued aud without azp: refused (client_id occurring in aud is
    // never sufficient).
    let token = provider.sign(&MockTokenSpec {
        audience: multi.clone(),
        ..token_spec(None, NOW_MS + 60_000)
    });
    assert_eq!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::WrongAzp {
            expected: CLIENT.into(),
            actual: None,
        }
    );

    // Multi-valued aud with a wrong azp: refused.
    let token = provider.sign(&MockTokenSpec {
        audience: multi.clone(),
        azp: Some("other-client".into()),
        ..token_spec(None, NOW_MS + 60_000)
    });
    assert_eq!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::WrongAzp {
            expected: CLIENT.into(),
            actual: Some("other-client".into()),
        }
    );

    // Multi-valued aud with the correct azp: accepted.
    let token = provider.sign(&MockTokenSpec {
        audience: multi.clone(),
        azp: Some(CLIENT.into()),
        ..token_spec(None, NOW_MS + 60_000)
    });
    let claims = adapter
        .verify_id_token(&token, &expectations(None, NOW_MS))
        .await
        .unwrap();
    assert_eq!(claims.audience, [CLIENT, "other-client"]);

    // A multi-valued aud that does not even contain the client id is a
    // WrongAudience regardless of azp.
    let token = provider.sign(&MockTokenSpec {
        audience: MockAudience::Multi(vec!["other-client".into(), "third".into()]),
        azp: Some(CLIENT.into()),
        ..token_spec(None, NOW_MS + 60_000)
    });
    assert!(matches!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::WrongAudience { .. }
    ));

    // Single-valued aud with a wrong azp: refused.
    let token = provider.sign(&MockTokenSpec {
        azp: Some("other-client".into()),
        ..token_spec(None, NOW_MS + 60_000)
    });
    assert_eq!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::WrongAzp {
            expected: CLIENT.into(),
            actual: Some("other-client".into()),
        }
    );

    // Single-valued aud with the matching azp: accepted.
    let token = provider.sign(&MockTokenSpec {
        azp: Some(CLIENT.into()),
        ..token_spec(None, NOW_MS + 60_000)
    });
    assert!(adapter
        .verify_id_token(&token, &expectations(None, NOW_MS))
        .await
        .is_ok());

    // An `aud` array with a non-text entry is malformed, never partially
    // honored.
    let mut payload = token_spec(None, NOW_MS + 60_000).payload();
    payload["aud"] = serde_json::json!([CLIENT, 7]);
    let token = provider.sign_payload(&payload);
    assert!(matches!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::Malformed(_)
    ));

    // A non-text azp is malformed too.
    let mut payload = token_spec(None, NOW_MS + 60_000).payload();
    payload["azp"] = serde_json::json!(7);
    let token = provider.sign_payload(&payload);
    assert!(matches!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::Malformed(_)
    ));
}

// --------------------------------------------------------- adversarial: time

/// i64::MAX/MIN time claims and extreme skews cannot overflow: impossible
/// millisecond conversions are typed refusals and every comparison is
/// i128-exact.
#[tokio::test]
async fn extreme_time_claims_are_typed_outcomes_never_overflow() {
    let provider = Arc::new(MockProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let adapter = adapter(provider.clone(), clock);

    // exp = i64::MAX seconds cannot be represented in milliseconds.
    let spec = MockTokenSpec {
        exp_s: Some(i64::MAX),
        ..token_spec(None, NOW_MS + 60_000)
    };
    let token = provider.sign(&spec);
    assert_eq!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::TimestampOutOfRange {
            claim: "exp".into(),
            value: i64::MAX,
        }
    );

    // iat = i64::MIN seconds cannot be represented in milliseconds either.
    let spec = MockTokenSpec {
        iat_s: Some(i64::MIN),
        ..token_spec(None, NOW_MS + 60_000)
    };
    let token = provider.sign(&spec);
    assert_eq!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::TimestampOutOfRange {
            claim: "iat".into(),
            value: i64::MIN,
        }
    );

    // exp = i64::MIN seconds is refused the same way (multiplication
    // underflows).
    let spec = MockTokenSpec {
        exp_s: Some(i64::MIN),
        ..token_spec(None, NOW_MS + 60_000)
    };
    let token = provider.sign(&spec);
    assert!(matches!(
        adapter
            .verify_id_token(&token, &expectations(None, NOW_MS))
            .await
            .unwrap_err(),
        OidcError::TimestampOutOfRange { ref claim, .. } if claim == "exp"
    ));

    // A near-ceiling exp and a floor iat with i64::MAX skew stay in i128:
    // the token is accepted without any wrap or panic.
    let spec = MockTokenSpec {
        exp_s: Some(i64::MAX / 1000),
        iat_s: Some(i64::MIN / 1000),
        ..token_spec(None, NOW_MS + 60_000)
    };
    let token = provider.sign(&spec);
    let mut wide = expectations(None, 0);
    wide.clock_skew_ms = i64::MAX;
    adapter
        .verify_id_token(&token, &wide)
        .await
        .expect("an i128 skew window cannot overflow");

    // The same floor exp with now = i64::MAX and i64::MAX skew is a typed
    // Expired (not an overflow).
    let spec = MockTokenSpec {
        exp_s: Some(i64::MIN / 1000),
        ..token_spec(None, NOW_MS + 60_000)
    };
    let token = provider.sign(&spec);
    let mut wide = expectations(None, i64::MAX);
    wide.clock_skew_ms = i64::MAX;
    assert_eq!(
        adapter.verify_id_token(&token, &wide).await.unwrap_err(),
        OidcError::Expired
    );
}

/// A transport that never resolves: an issuer that accepts and stalls.
struct StallingTransport;

impl HttpTransport for StallingTransport {
    fn execute(&self, _req: Request) -> BoxFuture<'_, Result<CheckedResponse, EgressError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_issuer_is_a_typed_timeout_at_the_documented_bound() {
    let adapter = NetworkOidcAdapter::new(
        Arc::new(StallingTransport),
        Arc::new(ManualClock::new(NOW_MS)),
        config(),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let call = tokio::spawn(async move { adapter.discovery(ISSUER).await });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(
        OIDC_NETWORK_TIMEOUT_MS + 1_000,
    ))
    .await;
    let err = call.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, OidcError::DiscoveryUnavailable(m) if m.contains("network bound")),
        "{err:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "virtual time only: no real-time hang"
    );
}
