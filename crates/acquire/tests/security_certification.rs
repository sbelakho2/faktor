//! Security certification for the acquisition engine's direct-HTTP path
//! (`docs/acquire.md` §2/§16): malicious redirects (non-HTTP schemes, loops,
//! credential-bearing targets) are refused typed, and cross-host redirect
//! targets are refused by the destination-policy authority the runtime
//! installs.
//!
//! Offline: the injected transport replays canned responses; no socket is
//! opened, and the destination-policy checks are pure.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use faktor_acquire::http::{fetch_direct_http, resolve_redirect, DirectHttpPolicy, HttpFetch};
use faktor_acquire::{AcquireCtx, AcquisitionError};
use faktor_core::cancellation::CancellationToken;
use faktor_core::time::SystemClock;
use faktor_provider::egress::{EgressError, HttpTransport};
use faktor_security::destination::{Decision, DestinationPolicy, RequestTarget};
use futures::future::BoxFuture;
use reqwest::{Body, Request, Response};

#[derive(Clone)]
struct Canned {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

struct ScriptedTransport {
    queue: Mutex<VecDeque<Canned>>,
    requests: Mutex<Vec<String>>,
}

impl ScriptedTransport {
    fn new(seed: Vec<Canned>) -> Self {
        Self {
            queue: Mutex::new(seed.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().expect("requests lock").len()
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(&self, req: Request) -> BoxFuture<'_, Result<Response, EgressError>> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(req.url().to_string());
        let canned = self.queue.lock().expect("queue lock").pop_front();
        Box::pin(async move {
            let Some(canned) = canned else {
                return Err(EgressError::Transport("no scripted response".into()));
            };
            let mut builder = http::Response::builder().status(canned.status);
            for (name, value) in canned.headers {
                builder = builder.header(name, value);
            }
            let response = builder
                .body(Body::from(canned.body))
                .map_err(|e| EgressError::Build(e.to_string()))?;
            Ok(Response::from(response))
        })
    }
}

fn ctx_for(transport: Arc<ScriptedTransport>, policy: DirectHttpPolicy) -> AcquireCtx {
    AcquireCtx::new(transport, Arc::new(SystemClock), CancellationToken::new()).with_policy(policy)
}

/// Non-HTTP schemes and credential-bearing targets can never be smuggled
/// through a redirect: the resolver refuses them typed.
#[test]
fn scheme_smuggling_redirects_are_refused_typed() {
    let base = "https://api.mouser.com/api/v1/search/partnumber";
    for hostile in [
        "javascript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "file:///etc/passwd",
        "ftp://evil.example/x",
    ] {
        let error =
            resolve_redirect(base, hostile, 4096).expect_err(&format!("{hostile} must be refused"));
        assert!(
            matches!(error, AcquisitionError::InvalidRequest { .. }),
            "{hostile}: {error:?}"
        );
    }
    // Credential-bearing absolute targets are refused too.
    assert!(resolve_redirect(base, "https://user:pass@evil.example/x", 4096).is_err());
    // Control characters and empty/oversized targets are typed refusals.
    assert!(resolve_redirect(base, "https://evil.example/\u{7}", 4096).is_err());
    assert!(resolve_redirect(base, "", 4096).is_err());
    assert!(resolve_redirect(
        base,
        &format!("https://evil.example/{}", "x".repeat(5000)),
        4096
    )
    .is_err());
    // Same-host and safe relative targets still resolve.
    assert_eq!(
        resolve_redirect(base, "/api/v1/other", 4096).expect("root-relative"),
        "https://api.mouser.com/api/v1/other"
    );
}

/// A redirect loop is bounded and typed; the transport is called exactly
/// `max_redirects + 1` times, never unbounded.
#[tokio::test]
async fn redirect_loops_are_typed_and_bounded() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        Canned {
            status: 302,
            headers: vec![("location", "/loop")],
            body: Vec::new(),
        };
        3
    ]));
    let policy = DirectHttpPolicy {
        max_redirects: 2,
        ..DirectHttpPolicy::default()
    };
    let ctx = ctx_for(transport.clone(), policy);
    let error = fetch_direct_http(&ctx, &HttpFetch::get("https://api.mouser.com/api/v1/loop"))
        .await
        .expect_err("the loop must be refused");
    assert_eq!(
        error,
        AcquisitionError::TooManyRedirects { limit: 2 },
        "a redirect loop is a typed refusal"
    );
    assert_eq!(
        transport.request_count(),
        3,
        "redirects are bounded by max_redirects + 1 attempts, never a spin"
    );
}

/// Redirect responses without a Location or with a 3xx-but-nonstandard body
/// fail typed instead of being treated as success.
#[tokio::test]
async fn redirect_without_location_is_typed() {
    let transport = Arc::new(ScriptedTransport::new(vec![Canned {
        status: 301,
        headers: Vec::new(),
        body: b"moved".to_vec(),
    }]));
    let ctx = ctx_for(transport, DirectHttpPolicy::default());
    let error = fetch_direct_http(&ctx, &HttpFetch::get("https://api.mouser.com/api/v1/x"))
        .await
        .expect_err("a Location-less redirect is malformed");
    assert!(matches!(error, AcquisitionError::InvalidRequest { .. }));
}

/// Cross-host redirect target: the resolver keeps only safe http(s)
/// targets, and the destination-policy authority the runtime installs
/// refuses a target outside the first-party allowlist, typed.
#[test]
fn cross_host_redirect_target_is_refused_by_the_destination_policy() {
    let base = "https://api.mouser.com/api/v1/search/partnumber";
    let cross_host = resolve_redirect(base, "https://evil.example/exfil", 4096)
        .expect("a safe-scheme cross-host target resolves");
    let policy = DestinationPolicy::parse_lines(["https://api.mouser.com"])
        .expect("first-party destination policy");
    let target = RequestTarget::parse(&cross_host).expect("target parses");
    assert!(
        matches!(target.check_against(&policy), Decision::Denied(_)),
        "cross-host redirect target must be denied by the policy"
    );
    // Load-bearing: the same-host target is allowed by the same policy.
    let same_host = resolve_redirect(base, "/api/v1/search/keyword", 4096).expect("same host");
    let target = RequestTarget::parse(&same_host).expect("target parses");
    assert!(
        target.check_against(&policy).is_allowed(),
        "the first-party target must pass; the policy is not blanket-deny"
    );
    // A cross-host target under a scheme the policy does not allow is
    // refused earlier and typed.
    let target = RequestTarget::parse("http://api.mouser.com/x").expect("target parses");
    assert!(target.check_against(&policy).denied_reason().is_some());
}

/// The checked egress transport refuses an unexpected destination typed
/// before any connect.
#[test]
fn checked_transport_refuses_an_unexpected_destination() {
    let policy = DestinationPolicy::parse_lines(["https://api.mouser.com"]).expect("policy");
    let transport = faktor_provider::egress::CheckedHttpClient::with_policy(Some(policy));
    let url = reqwest::Url::parse("https://evil.example/collect").expect("url");
    let error = transport.check(&url).expect_err("unexpected destination");
    assert!(
        matches!(error, EgressError::Denied { .. }),
        "typed deny: {error:?}"
    );
    let allowed = reqwest::Url::parse("https://api.mouser.com/api/v1/x").expect("url");
    assert!(transport.check(&allowed).is_ok());
}
