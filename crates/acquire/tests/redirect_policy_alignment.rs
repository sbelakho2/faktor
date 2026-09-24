//! Cross-crate proof (P2): acquire's URL validation/redirect resolution and
//! the provider's parsed-URL destination gate cannot diverge.
//!
//! `resolve_redirect` delegates every URL-semantic decision (backslash and
//! authority normalization, IDN, port canonicalization, dot segments) to the
//! shared parser that `faktor_provider::egress::check_url` gates on, and it
//! returns that parser's canonical form. This test pins the property from
//! outside both crates:
//!
//! - every accepted resolved URL is byte-identical to its own parse (so the
//!   hand-built `RawRequest` can only ever be parsed to the URL that was
//!   validated),
//! - the transport observes exactly that parsed URL (the gate runs on the
//!   request's own URL),
//! - the gate's decision on the observed URL equals its decision on the
//!   resolver's output, and
//! - hostile backslash/authority/userinfo shapes are either refused typed or
//!   land on the origin the shared parser canonicalized (never a smuggled
//!   host).
//!
//! Offline: `MockHttpTransport` never opens a socket; the policy checks are
//! pure.

use faktor_acquire::http::{resolve_redirect, validate_fetch_url};
use faktor_provider::egress::{
    check_url, execute_raw, MockHttpTransport, RawRequest, ResponseBudget,
};

/// The response budget every test read passes: one 30 s wall bound and the
/// seam's materialization byte cap.
fn test_budget() -> ResponseBudget {
    ResponseBudget::for_timeout(
        std::time::Duration::from_secs(30),
        faktor_provider::egress::MAX_RAW_RESPONSE_BYTES as u64,
    )
}
use faktor_security::destination::DestinationPolicy;

const BASE: &str = "https://api.allowed.example/v1/base";
const LIMIT: usize = 4096;

fn allowed_policy() -> DestinationPolicy {
    DestinationPolicy::parse_lines(["https://api.allowed.example"]).unwrap()
}

/// The adversarial redirect corpus: relative/absolute shapes plus authority
/// normalization, credentials, scheme smuggling, port tricks and controls.
fn corpus() -> Vec<&'static str> {
    vec![
        "child",
        "/root",
        "../up",
        "../../../past-root",
        "./same?z=2",
        "?q=1",
        "#frag",
        "//other.example/x",
        "https://api.allowed.example/next",
        // Backslash/authority normalization attempts.
        "https://api.allowed.example\\@evil.example/x",
        "https://api.allowed.example\\.evil.example/x",
        "https://api.allowed.example%5c@evil.example/x",
        "\\\\evil.example/x",
        // Credentials and scheme smuggling.
        "https://user:pass@api.allowed.example/x",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,x",
        // Port tricks and controls.
        "https://api.allowed.example:0/x",
        "https://api.allowed.example:notaport/x",
        "https://api.allowed.example/x\nsmuggled",
    ]
}

#[tokio::test]
async fn resolved_redirects_cannot_diverge_from_the_parsed_gate() {
    let policy = allowed_policy();
    for location in corpus() {
        let outcome = resolve_redirect(BASE, location, LIMIT);
        let Ok(resolved) = outcome else {
            // Refused before a RawRequest could ever exist: nothing to send.
            continue;
        };
        // 1. Canonical round-trip: the string IS the shared parser's output,
        // so the gate can never parse it to a different origin.
        let parsed = reqwest::Url::parse(&resolved)
            .unwrap_or_else(|e| panic!("accepted {location:?} -> {resolved:?} must parse: {e}"));
        assert_eq!(
            parsed.as_str(),
            resolved,
            "resolved URL must be the parser's canonical form (no textual divergence)"
        );
        assert!(
            validate_fetch_url(&resolved, LIMIT).is_ok(),
            "the canonical form re-validates"
        );
        // The textual authority the old string-based validator would have
        // inspected is gone: the origin is exactly the parsed one.
        let before = check_url(&policy, &parsed);
        // 2. A hand-built RawRequest executes through the mock; the
        // transport observes the request's OWN parsed URL.
        let transport = MockHttpTransport::new(200, "{}");
        let response = execute_raw(
            &transport,
            RawRequest::new("GET", resolved.clone()),
            &test_budget(),
        )
        .await
        .unwrap_or_else(|e| panic!("canonical {resolved:?} must build/execute: {e}"));
        assert_eq!(response.status, 200);
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        let observed = &requests[0].1;
        assert_eq!(
            observed, &resolved,
            "the transport observes exactly the validated canonical URL"
        );
        // 3. The gate decision on the observed URL is identical to its
        // decision on the resolver's output (same parser, same bytes).
        let observed_url = reqwest::Url::parse(observed).unwrap();
        assert_eq!(
            check_url(&policy, &observed_url).is_ok(),
            before.is_ok(),
            "gate decision must not change between resolution and send"
        );
    }
}

#[tokio::test]
async fn backslash_authority_shapes_land_on_the_canonical_origin_not_a_smuggled_host() {
    let policy = allowed_policy();
    // Every shape that textually names the allowed host must canonicalize to
    // the ALLOWED origin (a backslash can never introduce a second authority
    // the string-based validator missed), and every shape that tries to
    // embed credentials is refused outright.
    let mut accepted = 0usize;
    for location in [
        "https://api.allowed.example\\@evil.example/x",
        "https://api.allowed.example\\.evil.example/x",
        "https://api.allowed.example%5c@evil.example/x",
    ] {
        match resolve_redirect(BASE, location, LIMIT) {
            Ok(resolved) => {
                accepted += 1;
                let parsed = reqwest::Url::parse(&resolved).unwrap();
                assert_eq!(
                    parsed.host_str(),
                    Some("api.allowed.example"),
                    "{location:?} must canonicalize to the allowed host, got {resolved:?}"
                );
                assert!(
                    check_url(&policy, &parsed).is_ok(),
                    "{location:?} -> {resolved:?} must be allowed by the parsed gate"
                );
                let transport = MockHttpTransport::new(200, "{}");
                execute_raw(
                    &transport,
                    RawRequest::new("GET", resolved.clone()),
                    &test_budget(),
                )
                .await
                .expect("canonical allowed URL executes");
                assert_eq!(transport.requests()[0].1, resolved);
            }
            Err(_) => {
                // Refused by the shared parser (e.g. decoded userinfo from
                // `%5c@`): no RawRequest exists, nothing can diverge.
            }
        }
    }
    assert!(
        accepted >= 1,
        "the corpus must exercise at least one canonicalizing backslash shape"
    );
    // And the genuinely foreign shapes are denied by the parsed gate (and
    // still cannot be smuggled back in by a backslash).
    for location in [
        "//evil.example/x",
        "https://evil.example/x",
        "https://api.allowed.example.evil.example/x",
    ] {
        let resolved = resolve_redirect(BASE, location, LIMIT)
            .unwrap_or_else(|e| panic!("{location:?} must resolve to a URL: {e}"));
        let parsed = reqwest::Url::parse(&resolved).unwrap();
        assert!(
            check_url(&policy, &parsed).is_err(),
            "{location:?} -> {resolved:?} must be denied"
        );
        let transport = MockHttpTransport::new(200, "{}");
        // The RawRequest itself builds (execute_raw does not gate); the gate
        // decision is what a policy transport would enforce on this exact
        // parsed URL:
        let _ = execute_raw(
            &transport,
            RawRequest::new("GET", resolved.clone()),
            &test_budget(),
        )
        .await;
        assert_eq!(transport.requests()[0].1, resolved);
        assert!(check_url(&policy, &parsed).is_err());
    }
}
