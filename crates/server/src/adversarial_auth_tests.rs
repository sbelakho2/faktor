//! Adversarial auth/bearer tests (task category 1): header shape matrix,
//! constant-time edges, route-class 401 coverage and session-scoping.
//!
//! Production entry points: [`crate::auth`] plus the served native router.

use super::tests::{test_deps, wait_until_server_dead};
use super::*;
use crate::auth::{
    check_bearer, check_bearer_value, check_password, AuthToken, ServerPassword,
    MAX_AUTH_HEADER_BYTES,
};
use faktor_security::secret::ct_eq_str;
use std::sync::Arc;

const VALID_PW: &str = "cafebabecafebabecafebabecafebabecafebabecafebabecafebabecafebabe";

fn planted_password() -> ServerPassword {
    ServerPassword::try_new(VALID_PW).expect("fixture password")
}

fn planted_token() -> AuthToken {
    AuthToken::generate()
}

#[test]
fn authorization_header_shape_matrix() {
    let pw = planted_password();
    let good = format!("Bearer {VALID_PW}");
    let cases: Vec<(Option<String>, bool, &str)> = vec![
        (Some(good.clone()), true, "exact bearer"),
        (None, false, "absent header"),
        (Some(String::new()), false, "empty header"),
        (Some("Bearer ".into()), false, "empty bearer value"),
        (Some(VALID_PW.into()), false, "no scheme"),
        (Some("bearer CAFEBABE".into()), false, "lowercase scheme"),
        (Some("BEARER CAFEBABE".into()), false, "uppercase scheme"),
        (Some("Token cafebabe".into()), false, "unknown scheme"),
        (Some("Digest cafebabe".into()), false, "digest scheme"),
        (
            Some(format!("Basic {}", VALID_PW)),
            false,
            "bare Basic value",
        ),
        (
            Some(format!("Bearer  {VALID_PW}")),
            false,
            "double space after scheme",
        ),
        (Some(format!("Bearer\t{VALID_PW}")), false, "tab separator"),
        (
            Some(format!("Bearer{VALID_PW}")),
            false,
            "missing separator",
        ),
        (Some(format!(" Bearer {VALID_PW}")), false, "leading space"),
        (Some(format!("Bearer {VALID_PW} ")), false, "trailing space"),
        (
            Some(format!("Bearer {VALID_PW}\n")),
            false,
            "trailing newline",
        ),
        (Some(format!("Bearer {VALID_PW}\r")), false, "trailing CR"),
        (
            Some(format!("Bearer\u{00a0}{VALID_PW}")),
            false,
            "NBSP separator",
        ),
        (
            Some(format!("Bearer\u{200b}{VALID_PW}")),
            false,
            "zero-width separator",
        ),
        (
            Some(format!("Вearer {VALID_PW}")),
            false,
            "Cyrillic homoglyph scheme",
        ),
        (
            Some(format!("Bearer {}", VALID_PW.to_uppercase())),
            false,
            "case-changed secret",
        ),
        (
            Some(format!("Bearer {}", &VALID_PW[..63])),
            false,
            "one-byte-short secret",
        ),
        (
            Some(format!("Bearer {VALID_PW}a")),
            false,
            "one-byte-long secret",
        ),
        (
            Some(format!("Bearer {}x", &VALID_PW[..63])),
            false,
            "same-length wrong secret",
        ),
        (
            Some(format!("Bearer {VALID_PW} extra")),
            false,
            "trailing garbage",
        ),
        (
            Some(format!("Bearer {VALID_PW}\u{0}")),
            false,
            "embedded NUL suffix",
        ),
        (Some("Bearer café".into()), false, "unicode bearer value"),
        (
            Some(format!("Bearer é{}", &VALID_PW[1..])),
            false,
            "multibyte first byte",
        ),
    ];
    for (header, expected, label) in cases {
        let header = header.as_deref();
        assert_eq!(
            pw.check_authorization(header),
            expected,
            "check_authorization {label}: {header:?}"
        );
        assert_eq!(
            check_password(&pw, header, None),
            expected,
            "check_password {label}: {header:?}"
        );
        if expected {
            let token = planted_token();
            assert!(
                check_bearer(&token, Some(&format!("Bearer {}", token.as_str()))),
                "control: the bearer path works alongside {label}"
            );
        }
    }
}

#[test]
fn x_faktor_header_and_precedence_matrix() {
    let pw = planted_password();
    let wrong = "Bearer wrong";
    let cases: Vec<(Option<String>, Option<String>, bool, &str)> = vec![
        (None, Some(VALID_PW.into()), true, "x-header only"),
        (None, Some("wrong".into()), false, "x-header wrong"),
        (
            Some(wrong.into()),
            Some(VALID_PW.into()),
            true,
            "wrong bearer + right x-header",
        ),
        (
            Some(wrong.into()),
            Some("wrong".into()),
            false,
            "both wrong",
        ),
        (
            None,
            Some(VALID_PW.to_uppercase()),
            false,
            "x-header case changed",
        ),
        (
            None,
            Some(format!(" {VALID_PW}")),
            false,
            "x-header leading space",
        ),
        (
            None,
            Some(format!("{VALID_PW} ")),
            false,
            "x-header trailing space",
        ),
        (None, Some(VALID_PW[..63].into()), false, "x-header short"),
        (None, Some(String::new()), false, "x-header empty"),
        (
            Some(String::new()),
            Some(String::new()),
            false,
            "both empty",
        ),
    ];
    for (auth, x, expected, label) in cases {
        let auth = auth.as_deref();
        let x = x.as_deref();
        assert_eq!(
            check_password(&pw, auth, x),
            expected,
            "check_password {label}: auth={auth:?} x={x:?}"
        );
    }
    // The Authorization entry point never accepts the x-header form.
    assert!(
        !pw.check_authorization(Some(VALID_PW)),
        "check_authorization must not accept a raw x-header value"
    );
}

#[test]
fn header_bound_is_exact_at_the_byte() {
    let pw = planted_password();
    let token = planted_token();
    // Over-bound values are refused without comparing.
    for size in [
        MAX_AUTH_HEADER_BYTES + 1,
        MAX_AUTH_HEADER_BYTES * 2,
        64 * 1024,
    ] {
        let huge = format!("Bearer {}", "A".repeat(size));
        assert!(
            !pw.check_authorization(Some(&huge)),
            "password path refuses a {}-byte header",
            huge.len()
        );
        assert!(
            !check_password(&pw, Some(&huge), None),
            "check_password refuses a {}-byte authorization header",
            huge.len()
        );
        assert!(
            !check_password(&pw, None, Some(&huge)),
            "check_password refuses a {}-byte x-header",
            huge.len()
        );
        assert!(
            !check_bearer(&token, Some(&huge)),
            "token path refuses a {}-byte header",
            huge.len()
        );
        assert!(
            !check_bearer_value(VALID_PW, Some(&huge)),
            "worker bearer refuses a {}-byte header",
            huge.len()
        );
    }
    // At the bound with a wrong value: still refused, still no panic.
    let at_bound = format!("Bearer {}", "A".repeat(MAX_AUTH_HEADER_BYTES - 7));
    assert_eq!(at_bound.len(), MAX_AUTH_HEADER_BYTES, "bound fixture");
    assert!(
        !pw.check_authorization(Some(&at_bound)),
        "wrong value exactly at the bound is refused"
    );
    // The real header is far below the bound and accepted.
    let real = format!("Bearer {VALID_PW}");
    assert!(real.len() < MAX_AUTH_HEADER_BYTES, "real header size");
    assert!(pw.check_authorization(Some(&real)), "real header accepted");
}

#[test]
fn worker_bearer_value_contract() {
    let expected = "worker-plane-opaque-credential";
    let cases: Vec<(Option<&str>, bool, &str)> = vec![
        (Some("Bearer worker-plane-opaque-credential"), true, "exact"),
        (None, false, "absent"),
        (Some(""), false, "empty"),
        (Some(expected), false, "no scheme"),
        (
            Some("bearer worker-plane-opaque-credential"),
            false,
            "lowercase scheme",
        ),
        (
            Some("Bearer WORKER-PLANE-OPAQUE-CREDENTIAL"),
            false,
            "case change",
        ),
        (
            Some("Bearer worker-plane-opaque-credentia"),
            false,
            "truncated",
        ),
        (
            Some("Bearer worker-plane-opaque-credentialx"),
            false,
            "suffixed",
        ),
        (
            Some("Bearer worker-plane-opaque-credential extra"),
            false,
            "garbage",
        ),
        (Some("Bearer wrong"), false, "wrong value"),
        (Some("Basic d29ya2Vy"), false, "Basic scheme"),
    ];
    for (header, expected_ok, label) in cases {
        assert_eq!(
            check_bearer_value(expected, header),
            expected_ok,
            "check_bearer_value {label}: {header:?}"
        );
    }
}

#[test]
fn constant_time_equality_edges() {
    let cases: Vec<(String, String, bool, &str)> = vec![
        (VALID_PW.into(), VALID_PW.into(), true, "equal 64-hex"),
        (String::new(), String::new(), true, "both empty"),
        ("a".into(), "a".into(), true, "single char"),
        ("a".into(), "b".into(), false, "single char mismatch"),
        ("a".into(), String::new(), false, "one empty"),
        (String::new(), "a".into(), false, "other empty"),
        ("abc".into(), "abd".into(), false, "same length mismatch"),
        ("abc".into(), "abcd".into(), false, "shorter first"),
        ("abcd".into(), "abc".into(), false, "longer first"),
        ("é".into(), "é".into(), true, "multibyte equal"),
        ("é".into(), "e".into(), false, "multibyte vs ascii"),
        ("日本".into(), "日本".into(), true, "CJK equal"),
        ("日本".into(), "日".into(), false, "CJK prefix"),
        ("\u{0}".into(), "\u{0}".into(), true, "NUL equal"),
        (
            VALID_PW.into(),
            VALID_PW.to_uppercase(),
            false,
            "case mismatch",
        ),
        (
            VALID_PW.into(),
            format!("{VALID_PW}\u{0}"),
            false,
            "NUL suffix",
        ),
    ];
    for (a, b, expected, label) in cases {
        assert_eq!(
            ct_eq_str(&a, &b),
            expected,
            "ct_eq_str {label}: {a:?} vs {b:?}"
        );
    }
    assert!(
        planted_password().as_str() == VALID_PW,
        "fixture password round-trips"
    );
}

#[test]
fn password_shape_strength_is_enforced() {
    let accepted = [
        VALID_PW.to_string(),
        VALID_PW.to_uppercase(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
    ];
    for value in accepted {
        assert!(
            ServerPassword::try_new(value.clone()).is_ok(),
            "64 ASCII hex must be accepted: {value:?}"
        );
    }
    let rejected: Vec<(String, &str)> = vec![
        (String::new(), "empty"),
        ("x".into(), "one char"),
        ("a".repeat(63), "63 chars"),
        ("a".repeat(65), "65 chars"),
        ("z".repeat(64), "non-hex"),
        ("g".repeat(64), "non-hex g"),
        ("é".repeat(64), "64 multibyte chars are not 64 bytes"),
        (format!("{}!", "a".repeat(63)), "punctuation"),
        (format!("{}\u{0}", "a".repeat(63)), "NUL"),
        (format!("0x{}", "a".repeat(62)), "0x prefix"),
        (VALID_PW.to_string() + "\n", "trailing newline"),
        (format!(" {}", &VALID_PW[1..]), "leading space"),
    ];
    for (value, label) in rejected {
        let err = ServerPassword::try_new(value.clone())
            .expect_err(&format!("weak password ({label}) must be refused"));
        assert_eq!(
            err,
            crate::auth::AuthConfigError::MalformedServerPassword {
                expected_hex_chars: 64,
                actual_chars: value.chars().count(),
            },
            "typed shape refusal for {label}"
        );
        let rendered = format!("{err} {err:?}");
        assert!(
            !rendered.contains(VALID_PW),
            "shape refusal for {label} must not leak the planted secret"
        );
    }
}

// ------------------------------------------------------- HTTP route classes

/// Every route class, as (method, path-with-placeholder).
const ROUTE_CLASSES: &[(&str, &str)] = &[
    ("GET", "/session/1/projection"),
    ("GET", "/models"),
    ("GET", "/capabilities"),
    ("GET", "/native/health"),
    ("GET", "/native/ready"),
    ("GET", "/native/usage"),
    ("GET", "/native/entitlements"),
    ("GET", "/native/sessions"),
    ("GET", "/native/session/1/turns"),
    ("GET", "/native/session/1/tasks"),
    ("GET", "/native/session/1/agents"),
    ("GET", "/native/session/1/events"),
    ("GET", "/native/session/1/usage"),
    ("GET", "/native/session/1/terminal"),
    ("GET", "/native/session/1/terminal/events"),
    ("GET", "/native/orchestrator/graph?session=1"),
    ("GET", "/native/orchestrator/graph"),
    ("GET", "/native/agents"),
    ("GET", "/native/agents?session=1"),
    ("GET", "/native/messages"),
    ("GET", "/native/events"),
    ("GET", "/native/terminals"),
    ("GET", "/native/permissions"),
    ("GET", "/native/providers"),
    ("GET", "/native/semantic/status"),
    ("GET", "/native/index/coverage"),
    ("GET", "/native/identity"),
    ("GET", "/native/repositories"),
    ("GET", "/native/updater/status"),
    ("GET", "/native/workers"),
    ("GET", "/native/evidence/1"),
    ("GET", "/native/tasks/1/proof"),
    ("POST", "/native/session"),
    ("POST", "/native/session/1/prompt"),
    ("POST", "/native/session/1/abort"),
    ("POST", "/native/agents/1/pause"),
    ("POST", "/native/agents/1/retry"),
    ("POST", "/native/agents/1/steer"),
    ("POST", "/native/agents/1/model"),
    ("POST", "/native/session/1/terminal"),
    ("POST", "/native/permission/reply"),
    ("POST", "/native/updater/check"),
    ("POST", "/native/updater/stage"),
    ("POST", "/native/credits/grant"),
];

#[tokio::test]
async fn every_route_class_rejects_missing_and_malformed_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let token = deps.auth_token.clone();
    let handle = serve_arc(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();

    for (method, path) in ROUTE_CLASSES {
        let request = match *method {
            "GET" => client.get(format!("{base}{path}")),
            _ => {
                let body = if *path == "/native/scm/webhook" {
                    serde_json::json!({"digest": "sha256:x", "body": {}})
                } else {
                    serde_json::json!({})
                };
                client.post(format!("{base}{path}")).json(&body)
            }
        };
        let response = request.send().await.unwrap();
        assert_eq!(
            response.status(),
            401,
            "{method} {path} without credentials must be 401"
        );
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        assert!(
            body["error"]["code"].is_string() || body["error"].is_string(),
            "{method} {path} 401 body names a typed error: {body}"
        );
    }
    // Malformed credentials are 401 on the same route class too.
    for header in ["Bearer wrong", "bearer", "Basic Zm9v", VALID_PW, ""] {
        let response = client
            .get(format!("{base}/native/health"))
            .header("authorization", header)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            401,
            "malformed authorization {header:?} must be 401"
        );
    }
    // The SCM webhook route is signature-authenticated when wired (an SCM
    // cannot carry the daemon password): with no sink it is the typed 409
    // `webhook_disabled`, never a 401/200 oracle about the body.
    let response = client
        .post(format!("{base}/native/scm/webhook"))
        .json(&serde_json::json!({"digest": "sha256:x", "body": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        409,
        "an unwired webhook sink is a typed 409 configuration state"
    );
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    assert_eq!(
        body["error"]["code"], "scm_webhook_disabled",
        "the unwired webhook refusal names the configuration state"
    );
    // Worker registration is registration-token authenticated (a worker
    // does not hold the daemon password): a malformed body is the strict
    // DTO 400, and with a valid body an unwired plane is the typed 409
    // `workers_disabled`. Never a 200 without a token.
    let response = client
        .post(format!("{base}/native/workers/register"))
        .json(&serde_json::json!({"token": "w".repeat(64), "worker_id": "w1"}))
        .send()
        .await
        .unwrap();
    assert!(
        response.status() == 400 || response.status() == 409,
        "worker registration without a valid token and DTO is a typed refusal, got {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    assert!(
        body["error"]["code"].is_string(),
        "worker registration refusal carries a typed code: {body}"
    );
    // Oversized credentials are 401-or-431, never 200, and the daemon stays
    // alive afterwards.
    for size in [MAX_AUTH_HEADER_BYTES + 1, 64 * 1024] {
        let response = client
            .get(format!("{base}/native/health"))
            .header("authorization", format!("Bearer {}", "A".repeat(size)))
            .send()
            .await
            .unwrap();
        assert!(
            response.status() == 401 || response.status() == 431,
            "oversized authorization ({size} bytes) must be refused, got {}",
            response.status()
        );
    }
    let health = client
        .get(format!("{base}/native/health"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200, "daemon survives hostile headers");
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}

/// Every route the PRODUCTION sources register (`api/lifecycle.rs` +
/// `worker_plane.rs`), with EVERY method of a chain. Parsed from the same
/// files the runtime compiles, so a newly added route method cannot hide
/// behind a hand-maintained list.
fn registered_route_classes() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for source in [
        include_str!("api/lifecycle.rs"),
        include_str!("worker_plane.rs"),
    ] {
        for (path, methods) in parse_route_registrations(source) {
            for method in methods {
                out.push((method, path.clone()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Parse `.route("<path>", <method>(handler)[.<method>(handler)]*)` pairs.
/// Quoted strings are skipped while balancing parentheses; path parameters
/// are instantiated as `1` so the real served router matches the route.
fn parse_route_registrations(source: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(at) = source[search..].find(".route") {
        let start = search + at;
        search = start + ".route".len();
        let Some(open_rel) = source[search..].find('(') else {
            break;
        };
        let open = search + open_rel;
        let Some(body) = balanced_body(source, open) else {
            continue;
        };
        let Some(path) = first_quoted(body) else {
            continue;
        };
        let methods = ["get", "post", "put", "patch", "delete"]
            .into_iter()
            .filter(|name| {
                let needle = format!("{name}(");
                body.match_indices(&needle).any(|(idx, _)| {
                    idx == 0
                        || (!body.as_bytes()[idx - 1].is_ascii_alphanumeric()
                            && body.as_bytes()[idx - 1] != b'_')
                })
            })
            .map(str::to_string)
            .collect::<Vec<_>>();
        out.push((instantiate_route_path(&path), methods));
    }
    out
}

fn balanced_body(source: &str, open: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, byte) in bytes.iter().enumerate().skip(open) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[open + 1..idx]);
                }
            }
            _ => {}
        }
    }
    None
}

fn first_quoted(body: &str) -> Option<String> {
    let start = body.find('"')? + 1;
    let end = body[start..].find('"')? + start;
    Some(body[start..end].to_string())
}

fn instantiate_route_path(path: &str) -> String {
    let mut out = String::new();
    let mut in_param = false;
    for ch in path.chars() {
        match ch {
            '{' => in_param = true,
            '}' if in_param => {
                in_param = false;
                out.push('1');
            }
            _ if !in_param => out.push(ch),
            _ => {}
        }
    }
    out
}

/// P1-PROOF: the route contract is BEHAVIORAL, not a string comparison.
/// Every registered route method of the production router sources is driven
/// over a real served socket with no credentials and must be refused by the
/// auth layer (the webhook is signature-authenticated and refuses typed as
/// disabled). A route added without auth fails this test.
#[tokio::test]
async fn every_registered_route_rejects_missing_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let handle = serve_arc(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for (method, path) in registered_route_classes() {
        let url = format!("{base}{path}");
        let request = match method.as_str() {
            "get" => client.get(url.clone()),
            "delete" => client.delete(url.clone()),
            "post" => client.post(url.clone()).json(&serde_json::json!({})),
            "put" => client.put(url.clone()).json(&serde_json::json!({})),
            "patch" => client.patch(url.clone()).json(&serde_json::json!({})),
            _ => continue,
        };
        let status = request.send().await.unwrap().status().as_u16();
        checked += 1;
        // The worker plane is registration-token authenticated with a strict
        // DTO: an anonymous, shape-less call is refused typed (400/409)
        // before or at the token check — NEVER 2xx. The SCM webhook is
        // signature-authenticated and typed 409 while unwired.
        let expected: &[u16] = if path == "/native/scm/webhook" {
            &[409]
        } else if path.starts_with("/native/workers/") || path.starts_with("/native/jobs/") {
            &[400, 401, 403, 409]
        } else {
            &[401]
        };
        if !expected.contains(&status) {
            failures.push(format!("{method} {path}: {status} (want {expected:?})"));
        }
    }
    assert!(
        checked >= 80,
        "the router sources must yield the full route surface, saw {checked}"
    );
    assert!(
        failures.is_empty(),
        "unauthenticated route methods must be refused: {failures:?}"
    );
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}

#[tokio::test]
async fn all_three_credential_forms_authenticate_every_route_class() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let password = deps.server_password.clone();
    let token = deps.auth_token.clone();
    let handle = serve_arc(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();

    for (method, path) in ROUTE_CLASSES {
        let send = |builder: reqwest::RequestBuilder| async {
            let body = if *method == "GET" {
                None
            } else if *path == "/native/scm/webhook" {
                Some(serde_json::json!({"digest": "sha256:x", "body": {}}))
            } else {
                Some(serde_json::json!({}))
            };
            match body {
                Some(body) => builder.json(&body).send().await.unwrap(),
                None => builder.send().await.unwrap(),
            }
        };
        // Password bearer.
        let response = send(
            client
                .request(
                    method.parse::<reqwest::Method>().unwrap(),
                    format!("{base}{path}"),
                )
                .bearer_auth(password.as_str()),
        )
        .await;
        assert_ne!(
            response.status(),
            401,
            "{method} {path} must accept the password bearer"
        );
        // x-faktor-server-password.
        let response = send(
            client
                .request(
                    method.parse::<reqwest::Method>().unwrap(),
                    format!("{base}{path}"),
                )
                .header("x-faktor-server-password", password.as_str()),
        )
        .await;
        assert_ne!(
            response.status(),
            401,
            "{method} {path} must accept the x-header password"
        );
        // Legacy per-start token.
        let response = send(
            client
                .request(
                    method.parse::<reqwest::Method>().unwrap(),
                    format!("{base}{path}"),
                )
                .bearer_auth(token.as_str()),
        )
        .await;
        assert_ne!(
            response.status(),
            401,
            "{method} {path} must accept the legacy bearer token"
        );
    }
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}

#[tokio::test]
async fn session_scoped_routes_are_not_fooled_by_hostile_or_foreign_ids() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let token = deps.auth_token.clone();
    // Create one real session through the store so the daemon has a
    // legitimate id to contrast against.
    let ws = deps.session.create_workspace("/scoping").unwrap();
    let real = deps
        .session
        .create_session(ws, "scoping", "fake", "m")
        .unwrap();
    let real_id = real.id().raw();
    let handle = serve_arc(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();

    let scoped_get = [
        "/session/{id}/projection",
        "/native/session/{id}/turns",
        "/native/session/{id}/tasks",
        "/native/session/{id}/agents",
        "/native/session/{id}/usage",
        "/native/session/{id}/events",
    ];
    for template in scoped_get {
        for foreign in ["18446744073709551615", "0", "abc", "-1", "1.5"] {
            let path = template.replace("{id}", foreign);
            let response = client
                .get(format!("{base}{path}"))
                .bearer_auth(token.as_str())
                .send()
                .await
                .unwrap();
            assert!(
                response.status().is_client_error(),
                "GET {path} with foreign id {foreign:?} must be a client error, got {}",
                response.status()
            );
            assert_ne!(
                response.status(),
                200,
                "GET {path} with foreign id {foreign:?} must never be 200"
            );
        }
    }
    for template in [
        "/native/session/{id}/prompt",
        "/native/session/{id}/abort",
        "/native/session/{id}/terminal",
    ] {
        for foreign in ["18446744073709551615", "0", "abc"] {
            let path = template.replace("{id}", foreign);
            let response = client
                .post(format!("{base}{path}"))
                .bearer_auth(token.as_str())
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap();
            assert!(
                response.status().is_client_error(),
                "POST {path} with foreign id {foreign:?} must be a client error, got {}",
                response.status()
            );
        }
    }
    // The real session is reachable (200 or a typed non-auth failure), so
    // the foreign-id refusals above are about scoping, not a dead route.
    let response = client
        .get(format!("{base}/session/{real_id}/projection"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_ne!(
        response.status(),
        401,
        "the real session route must not be an auth failure"
    );
    assert_ne!(
        response.status(),
        404,
        "the real session route must resolve"
    );
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}

#[tokio::test]
async fn duplicate_and_split_authorization_headers_never_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let password = deps.server_password.clone();
    let token = deps.auth_token.clone();
    let handle = serve_arc(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();

    // Two wrong headers never authenticate.
    let response = client
        .get(format!("{base}/native/health"))
        .header("authorization", "Bearer wrong-1")
        .header("authorization", "Bearer wrong-2")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "two wrong authorization headers must be 401"
    );
    // Two x-header values, both wrong.
    let response = client
        .get(format!("{base}/native/health"))
        .header("x-faktor-server-password", "wrong-1")
        .header("x-faktor-server-password", "wrong-2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "two wrong x-headers must be 401");
    // A wrong Authorization plus the right x-header authenticates through
    // the x-header arm (independent arms).
    let response = client
        .get(format!("{base}/native/health"))
        .header("authorization", "Bearer wrong")
        .header("x-faktor-server-password", password.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "a valid x-header authenticates despite a wrong Authorization"
    );
    // A split bearer across two headers can never reassemble.
    let response = client
        .get(format!("{base}/native/health"))
        .header("authorization", "Bea")
        .header("authorization", &format!("rer {}", token.as_str()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "split bearer headers must not reassemble"
    );
    // The first header wins deterministically: [wrong, right] is 401.
    let response = client
        .get(format!("{base}/native/health"))
        .header("authorization", "Bearer wrong")
        .header("authorization", format!("Bearer {}", password.as_str()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "when duplicates are present the first header is authoritative (fail closed)"
    );
    // [right, wrong] is 200 under the same rule.
    let response = client
        .get(format!("{base}/native/health"))
        .header("authorization", format!("Bearer {}", password.as_str()))
        .header("authorization", "Bearer wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "the first-valid duplicate authenticates"
    );
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}
