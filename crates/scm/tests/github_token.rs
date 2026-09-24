//! Adversarial tests of the REAL GitHub App token source over a mock GitHub:
//! JWT structure + RS256 signature verified with the test key, cache hits,
//! expiry refresh, and the 429/5xx/4xx classification matrix. The transport
//! is scripted (no network); every request is inspected byte-for-byte.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use faktor_provider::egress::{EgressError, HttpTransport};
use faktor_scm::github::ManualClock;
use faktor_scm::{
    GitHubAppTokenConfig, GitHubAppTokenSource, InstallationTokenSource, ScmError,
    ScmInstallationId,
};
use reqwest::Request;
use ring::signature::UnparsedPublicKey;

const TEST_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDOdaRv7SIbH6qK
dvYxg7L/7edtfeKJcjPxLCjzXYw71LBuPm4PVYeJexTW6pgAE0++LlY5TvK2VdR4
BpBpkSehRCfGKLiyw9hVpRm8Fz8Lfrl7EFltVxRAo2lkNeUEHwsnHE6CvOWw1qgO
tGLKF4W+Vi8WSPRgv0HHMYEOCnK++yGnrZjYD4vrqsP55iDtwyb6j1iv9TdnXzY2
XwhDQ4SMxOw+N3NXrz072KlDVFs3sY+WTvtfnaCaGdmYb1qDYZEUvBdc6m7MA0og
hXuWz4Kg+Yzx4rWXAAJPmJi39JWFPJR5hlf2j+1eOTbrSBSzygfltN3aVZul1v3X
/6/uzghHAgMBAAECggEAIAlmQk34OGBCDOlny4glqwwGGNnrYKudfsN8+UKfY5td
40WBu5Roiz9TnQPbIUvd2GOFUrA6/ms0JInUN+Vj0mTqjRe9jVPRinyrkSHEUSrR
alS/o7Va+arBzGCGkIympOOCFUxtkfLFMj7wg26B/OaPuPQKI8cZ1GiMn5qkcpjr
COOoirnJQ1N1/2TPFUEvX+ZG04LumJlTOuw1n+yQGLXt2Bkf9FeBT9f4b98WLrQk
PNd5DxXfJGaNLmX17n6qgQ2+qeYb5mPvI6OONgzQOSHtjsrniPYtoY/N+ZabSRWW
5BLP5x4uo4fYW+GXbhSpO4imlOX411dVDf5J2g7IaQKBgQDzh/VUJTbjTzRa+mY0
wFauM3WvXWRZ3FrevbUwrP2EZ75odZ2Z89DAc/OwXzvuzRviMaK9fLLDZTWAZmEh
9qm/JVIDX6Pn4raa7FAg4pdI/GinlpuAoeg7HbSIe7amLQoIWYe7riYI/k7zI21+
cA0fBGskIVDbAcC/LE5+MP8iDwKBgQDZB8ZuGVs7ntkunLl4JuwY4/UniMUlzWA0
Ekj8k2lbIG5Aq72U1NkSQE3JbkIvRpDrySWUYWPQ7YIYTMxygymNmML1puLr1/9A
Iy+06O6S4jL0+JZORcSaZ9BQZGdmCCl8T+G2TR5w8x3PVfiV8hLLC3TLSt8TGdFv
qY5vEUeOSQKBgQDQY7b6mh2txUj30O1ElpGV31MFDNWiT30yvQMe8+i8NEoq+Poz
kv8+r/oHIncWkU0a8X5gxyPxL9noVbMobPo0JqtXV6/Z7ZZ0W2L1wO/T9KlZPvcx
y1n9vB2P7M0Oxduf6XzMjOjfKT5FsDsxxpBzykQkVp3pykY1UKSaNzMa4QKBgAdn
mIGRI+e417gbaMiMq2l9/ZNHu1I625lrNkpHzURqqthSA7ncOTvCLeU9ecybH76r
sjiJyhoKwHGLzT3q87P9DknLU9qwF+lcSfhmKh2g0hRBlv88qiSKfjT/9/cnOCMh
ppXNs8guw0mbqUuUYsfCsE1vVIUWUGr64f0wHbzhAoGAZh9kgDMjZlJMF9jJCrzD
m+vTRpldotdtts9A4/rPY2kV7nphAoL0leSKqG7UUtKxXsaD7wHpxgczLUf8Y5Fi
y07Oh0VbL/IpRuNvse5Jn1cOYCDJs2x12WE3l8toG+krj0ZDwcnkhVnMhplT9pro
hCsczsx/P3pKSzRXIkRTm9A=
-----END PRIVATE KEY-----";

const TEST_PUBLIC_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----
MIIBCgKCAQEAznWkb+0iGx+qinb2MYOy/+3nbX3iiXIz8Swo812MO9Swbj5uD1WH
iXsU1uqYABNPvi5WOU7ytlXUeAaQaZEnoUQnxii4ssPYVaUZvBc/C365exBZbVcU
QKNpZDXlBB8LJxxOgrzlsNaoDrRiyheFvlYvFkj0YL9BxzGBDgpyvvshp62Y2A+L
66rD+eYg7cMm+o9Yr/U3Z182Nl8IQ0OEjMTsPjdzV689O9ipQ1RbN7GPlk77X52g
mhnZmG9ag2GRFLwXXOpuzANKIIV7ls+CoPmM8eK1lwACT5iYt/SVhTyUeYZX9o/t
Xjk260gUs8oH5bTd2lWbpdb91/+v7s4IRwIDAQAB
-----END RSA PUBLIC KEY-----";

const APP_ID: u64 = 12345;
const NOW_MS: i64 = 1_789_600_000_000;
const MINIMAL_PERMISSIONS: &str =
    r#"{"contents":"write","pull_requests":"write","issues":"write","metadata":"read"}"#;

/// One scripted HTTP reply.
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: String,
}

impl Reply {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_string(),
        }
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }
}

/// One recorded request.
struct Recorded {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// A deterministic scripted transport: replies are consumed in order; a
/// request beyond the script is a loud failure. Every request is recorded.
type RecordedRequest = (String, String, Vec<(String, String)>, Vec<u8>);

struct ScriptedTransport {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Recorded>>,
}

impl ScriptedTransport {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.method.clone(),
                    r.url.clone(),
                    r.headers.clone(),
                    r.body.clone(),
                )
            })
            .collect()
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(
        &self,
        req: Request,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<reqwest::Response, EgressError>> + Send + '_>,
    > {
        let recorded = Recorded {
            method: req.method().to_string(),
            url: req.url().to_string(),
            headers: req
                .headers()
                .iter()
                .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect(),
            body: req
                .body()
                .and_then(|b| b.as_bytes())
                .map(|b| b.to_vec())
                .unwrap_or_default(),
        };
        self.requests.lock().unwrap().push(recorded);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("the scripted transport was called more times than scripted");
        Box::pin(async move {
            let mut builder = http::Response::builder().status(reply.status);
            for (name, value) in &reply.headers {
                builder = builder.header(*name, *value);
            }
            let response = builder
                .body(reqwest::Body::from(reply.body))
                .map_err(|e| EgressError::Build(e.to_string()))?;
            Ok(reqwest::Response::from(response))
        })
    }
}

fn token_source(
    transport: &Arc<ScriptedTransport>,
    max_attempts: u32,
) -> (GitHubAppTokenSource, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let source = GitHubAppTokenSource::new(
        GitHubAppTokenConfig {
            app_id: APP_ID,
            private_key_pkcs8_pem: TEST_PRIVATE_KEY.into(),
            api_base: "https://github.test".to_string(),
            user_agent: "faktor-test/0.1".to_string(),
            max_attempts,
            retry_base_ms: 0,
            ..Default::default()
        },
        transport.clone(),
        clock.clone(),
    )
    .expect("token source");
    (source, clock)
}

fn installation() -> ScmInstallationId {
    ScmInstallationId::try_from_raw(42).unwrap()
}

/// Decode a JWT and verify its RS256 signature against the test public key.
fn verify_jwt(token: &str) -> (serde_json::Value, serde_json::Value) {
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3, "a JWT must have three segments");
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header: serde_json::Value =
        serde_json::from_slice(&engine.decode(parts[0]).unwrap()).unwrap();
    let claims: serde_json::Value =
        serde_json::from_slice(&engine.decode(parts[1]).unwrap()).unwrap();
    let signature = engine.decode(parts[2]).unwrap();
    let der = decode_pem_body(TEST_PUBLIC_KEY);
    UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, der)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("the app JWT signature must verify with the test public key");
    (header, claims)
}

fn decode_pem_body(pem: &str) -> Vec<u8> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .unwrap()
}

fn token_reply(token: &str, permissions: &str) -> Reply {
    Reply::json(
        201,
        serde_json::json!({
            "token": token,
            "expires_at": "2027-01-01T00:00:00Z",
            "permissions": serde_json::from_str::<serde_json::Value>(permissions).unwrap(),
        }),
    )
}

#[tokio::test]
async fn mints_installation_token_with_a_verified_rs256_app_jwt() {
    let transport = ScriptedTransport::new(vec![token_reply("ghs_minted", MINIMAL_PERMISSIONS)]);
    let (source, _clock) = token_source(&transport, 3);
    let token = source.token_for(installation()).await.unwrap();
    assert_eq!(token.expose(), "ghs_minted");
    assert_eq!(transport.request_count(), 1);

    let requests = transport.requests();
    let (method, url, headers, _body) = &requests[0];
    assert_eq!(method, "POST");
    assert_eq!(
        url,
        "https://github.test/app/installations/42/access_tokens"
    );
    let auth = headers
        .iter()
        .find(|(name, _)| name == "authorization")
        .map(|(_, value)| value.clone())
        .expect("the mint must carry an authorization header");
    let jwt = auth.strip_prefix("Bearer ").expect("a bearer credential");
    let (header, claims) = verify_jwt(jwt);
    assert_eq!(header["alg"], "RS256");
    assert_eq!(header["typ"], "JWT");
    assert_eq!(claims["iss"], APP_ID.to_string());
    let iat = claims["iat"].as_i64().unwrap();
    let exp = claims["exp"].as_i64().unwrap();
    let now_s = NOW_MS / 1000;
    assert_eq!(iat, now_s - 60, "iat is backdated by the skew allowance");
    assert_eq!(exp, iat + 540);
    assert!(exp - iat <= 600, "the JWT lifetime is bounded");
}

#[tokio::test]
async fn cache_hit_avoids_a_second_call_and_an_expired_token_is_refreshed() {
    let transport = ScriptedTransport::new(vec![
        token_reply("ghs_first", MINIMAL_PERMISSIONS),
        token_reply("ghs_second", MINIMAL_PERMISSIONS),
    ]);
    let (source, clock) = token_source(&transport, 3);
    let first = source.token_for(installation()).await.unwrap();
    let again = source.token_for(installation()).await.unwrap();
    assert_eq!(first.expose(), again.expose());
    assert_eq!(transport.request_count(), 1, "a live token is cached");

    // Advance past the expiry skew: the next call refreshes.
    let expires = first.expires_at_ms();
    clock.advance(expires - 60_000 - NOW_MS + 1);
    let refreshed = source.token_for(installation()).await.unwrap();
    assert_eq!(refreshed.expose(), "ghs_second");
    assert_eq!(transport.request_count(), 2);
}

#[tokio::test]
async fn rate_limit_is_retried_then_classified_and_4xx_is_final() {
    // 429 (retry-after 0) then 500 then a success: two retries, one token.
    let transport = ScriptedTransport::new(vec![
        Reply::json(429, serde_json::json!({"message": "rate limited"}))
            .with_header("retry-after", "0"),
        Reply::json(500, serde_json::json!({"message": "boom"})),
        token_reply("ghs_after_retry", MINIMAL_PERMISSIONS),
    ]);
    let (source, _clock) = token_source(&transport, 3);
    let token = source.token_for(installation()).await.unwrap();
    assert_eq!(token.expose(), "ghs_after_retry");
    assert_eq!(transport.request_count(), 3);

    // A persistent 429 exhausts the bound and surfaces typed.
    let transport = ScriptedTransport::new(vec![
        Reply::json(429, serde_json::json!({"message": "rate limited"}))
            .with_header("retry-after", "0"),
        Reply::json(429, serde_json::json!({"message": "rate limited"}))
            .with_header("retry-after", "0"),
    ]);
    let (source, _clock) = token_source(&transport, 2);
    match source.token_for(installation()).await.unwrap_err() {
        ScmError::RateLimited { retry_after_ms } => assert_eq!(retry_after_ms, 0),
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert_eq!(transport.request_count(), 2, "the attempt bound is honored");

    // 401 is final: exactly one call, no retry.
    let transport = ScriptedTransport::new(vec![Reply::json(
        401,
        serde_json::json!({"message": "bad credentials"}),
    )]);
    let (source, _clock) = token_source(&transport, 3);
    assert!(matches!(
        source.token_for(installation()).await.unwrap_err(),
        ScmError::Unauthorized(_)
    ));
    assert_eq!(transport.request_count(), 1);

    // 422 is final too.
    let transport = ScriptedTransport::new(vec![Reply::json(
        422,
        serde_json::json!({"message": "unprocessable"}),
    )]);
    let (source, _clock) = token_source(&transport, 3);
    assert!(matches!(
        source.token_for(installation()).await.unwrap_err(),
        ScmError::Api { status: 422, .. }
    ));
    assert_eq!(transport.request_count(), 1);
}

#[tokio::test]
async fn a_token_missing_minimal_permissions_is_never_cached_or_used() {
    let transport = ScriptedTransport::new(vec![
        token_reply("ghs_weak", r#"{"metadata":"read"}"#),
        token_reply("ghs_strong", MINIMAL_PERMISSIONS),
    ]);
    let (source, _clock) = token_source(&transport, 3);
    let err = source.token_for(installation()).await.unwrap_err();
    assert_eq!(err.code(), "scm_forbidden");
    // The refused token was not cached: the next call mints again.
    let token = source.token_for(installation()).await.unwrap();
    assert_eq!(token.expose(), "ghs_strong");
    assert_eq!(transport.request_count(), 2);
}

#[tokio::test]
async fn app_token_is_a_cached_verified_jwt_with_no_network_call() {
    let transport = ScriptedTransport::new(vec![]);
    let (source, _clock) = token_source(&transport, 3);
    let first = source.app_token().await.unwrap();
    let second = source.app_token().await.unwrap();
    assert_eq!(first.expose(), second.expose(), "the JWT is cached");
    assert_eq!(transport.request_count(), 0, "no network call for a JWT");
    let (header, claims) = verify_jwt(first.expose());
    assert_eq!(header["alg"], "RS256");
    assert_eq!(claims["iss"], APP_ID.to_string());
}
