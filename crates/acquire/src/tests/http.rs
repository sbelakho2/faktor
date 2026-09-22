//! Bounded direct-HTTP suite: every bound is typed, 304 never parses,
//! cancellation and deadline stop in-flight work, and no refusal ever
//! reaches the transport.

use std::sync::Arc;
use std::time::Duration;

use faktor_provider::egress::EgressError;

use crate::cache::{CacheEntry, CacheKey, CacheState, FieldFreshnessClass};
use crate::error::{AcquisitionError, VerificationKind};
use crate::http::{
    check_json_nesting, fetch_direct_http, parse_bounded_json, resolve_redirect, store_acquisition,
    validate_fetch_url, DirectHttpPolicy, HttpFetch,
};
use crate::provenance::content_digest;
use crate::request::RequestedField;
use crate::tests::{ctx_for, Gate, ScriptedResponse, ScriptedTransport, TEST_URL};

fn policy() -> DirectHttpPolicy {
    DirectHttpPolicy::default()
}

#[tokio::test]
async fn response_byte_bound_is_typed_and_checked_before_parsing() {
    let mut small = policy();
    small.max_response_bytes = 16;
    // The body is not valid JSON and exceeds the byte bound: the byte bound
    // must fire first (typed), never the parser.
    let body = format!(r#"{{"data":"{}"}}"#, "x".repeat(64));
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json(body)]));
    let (ctx, _clock) = ctx_for(transport);
    let ctx = ctx.with_policy(small);
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(err, AcquisitionError::ResponseTooLarge { limit_bytes: 16 });
}

#[tokio::test]
async fn json_nesting_bound_is_typed() {
    let mut bounds = policy();
    bounds.max_json_nesting = 3;
    let deep = format!("{}1{}", "[".repeat(5), "]".repeat(5));
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json(deep)]));
    let (ctx, _clock) = ctx_for(transport);
    let ctx = ctx.with_policy(bounds);
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        AcquisitionError::NestingTooDeep {
            limit: 3,
            observed: 4
        }
    );
}

#[tokio::test]
async fn non_json_bodies_are_extraction_failures_never_partial_values() {
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json(
        b"<<<not json>>>".to_vec(),
    )]));
    let (ctx, _clock) = ctx_for(transport);
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(err, AcquisitionError::ExtractionIncomplete);
}

#[tokio::test]
async fn redirect_cap_is_typed_and_counted() {
    let mut bounds = policy();
    bounds.max_redirects = 2;
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::new(302, "").header("location", "/next-1"),
        ScriptedResponse::new(302, "").header("location", "/next-2"),
        ScriptedResponse::new(302, "").header("location", "/next-3"),
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let ctx = ctx.with_policy(bounds);
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(err, AcquisitionError::TooManyRedirects { limit: 2 });
    assert_eq!(transport.request_count(), 3);
}

#[tokio::test]
async fn redirects_follow_safely_and_never_smuggle_other_schemes() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::new(301, "").header("location", "https://other.invalid/final"),
        ScriptedResponse::json(r#"{"ok":true}"#),
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let acquisition = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();
    assert_eq!(acquisition.redirects, 1);
    assert_eq!(acquisition.final_url, "https://other.invalid/final");
    assert_eq!(
        transport.last_request().unwrap().url,
        "https://other.invalid/final"
    );
    let urls: Vec<String> = transport
        .requests()
        .into_iter()
        .map(|recorded| recorded.url)
        .collect();
    assert_eq!(
        urls,
        vec![
            TEST_URL.to_string(),
            "https://other.invalid/final".to_string()
        ]
    );

    // A Location without a header value is a typed refusal.
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::new(302, "")]));
    let (ctx, _clock) = ctx_for(transport);
    assert!(matches!(
        fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
            .await
            .unwrap_err(),
        AcquisitionError::InvalidRequest { .. }
    ));

    // A scheme-smuggling Location is refused before it can be fetched.
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::new(302, "").header("location", "file:///etc/passwd")
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    assert!(matches!(
        fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
            .await
            .unwrap_err(),
        AcquisitionError::InvalidRequest { .. }
    ));
    assert_eq!(transport.request_count(), 1);
}

#[tokio::test]
async fn not_modified_short_circuits_before_parsing_and_never_writes_a_snapshot() {
    let mut cache = CacheState::new();
    let key = CacheKey::new(TEST_URL, None, RequestedField::Identity);
    cache.insert(
        key.clone(),
        CacheEntry {
            observed_at_ms: 500,
            class: FieldFreshnessClass::Slow,
            etag: Some("\"v1\"".into()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
            content_digest: Some("kept-digest".into()),
            content: Some(serde_json::json!({"a": 1})),
        },
    );
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::new(
        304,
        b"<<<deliberately not json>>>".to_vec(),
    )
    .header("etag", "\"v1\"")]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let fetch = HttpFetch::get(TEST_URL).with_validators(cache.get(&key).unwrap().validators());
    let acquisition = fetch_direct_http(&ctx, &fetch).await.unwrap();
    assert!(acquisition.not_modified);
    assert!(acquisition.json.is_none(), "a 304 is never parsed");
    assert!(acquisition.provenance.is_none());
    assert_eq!(acquisition.body_bytes, 0);
    assert_eq!(acquisition.status, 304);

    // The conditional headers were actually sent.
    let recorded = transport.last_request().unwrap();
    assert_eq!(recorded.header("if-none-match"), Some("\"v1\""));
    assert_eq!(
        recorded.header("if-modified-since"),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );

    // Storing a 304 is a no-op: no duplicate snapshot, the stored digest and
    // observation time stay authoritative.
    let len_before = cache.len();
    assert!(!store_acquisition(
        &mut cache,
        key.clone(),
        FieldFreshnessClass::Slow,
        &acquisition
    ));
    assert_eq!(cache.len(), len_before);
    let stored = cache.get(&key).unwrap();
    assert_eq!(stored.content_digest.as_deref(), Some("kept-digest"));
    assert_eq!(stored.observed_at_ms, 500);
    assert_eq!(stored.content, Some(serde_json::json!({"a": 1})));
}

#[tokio::test]
async fn a_successful_fetch_records_validators_digest_and_provenance() {
    let body = br#"{"value":42}"#.to_vec();
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json(
        body.clone(),
    )
    .header("etag", "\"v2\"")
    .header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT")]));
    let (ctx, _clock) = ctx_for(transport);
    let acquisition = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();
    assert_eq!(acquisition.status, 200);
    assert!(!acquisition.not_modified);
    assert_eq!(acquisition.json, Some(serde_json::json!({"value": 42})));
    assert_eq!(acquisition.body_bytes, body.len() as u64);
    assert_eq!(acquisition.validators.etag.as_deref(), Some("\"v2\""));
    let provenance = acquisition.provenance.as_ref().unwrap();
    assert_eq!(provenance.observed_at_ms, 1_000_000);
    assert_eq!(provenance.content_digest, content_digest(&body));
    assert!(!provenance.conditional);
    assert_eq!(provenance.final_url.as_deref(), Some(TEST_URL));

    let mut cache = CacheState::new();
    let key = CacheKey::new(TEST_URL, None, RequestedField::Identity);
    assert!(store_acquisition(
        &mut cache,
        key.clone(),
        FieldFreshnessClass::Slow,
        &acquisition
    ));
    let stored = cache.get(&key).unwrap();
    assert_eq!(
        stored.content_digest.as_deref(),
        Some(&*provenance.content_digest)
    );
    assert_eq!(stored.observed_at_ms, 1_000_000);
}

#[tokio::test]
async fn hostile_statuses_map_to_distinct_typed_errors() {
    let cases = [
        (401, AcquisitionError::AuthenticationRequired),
        (
            403,
            AcquisitionError::VerificationRequired {
                kind: VerificationKind::Challenge,
            },
        ),
        (404, AcquisitionError::NotFound),
        (410, AcquisitionError::NotFound),
        (408, AcquisitionError::NetworkTimeout),
        (504, AcquisitionError::NetworkTimeout),
        (500, AcquisitionError::ApiUnavailable),
        (503, AcquisitionError::ApiUnavailable),
        (
            418,
            AcquisitionError::InvalidRequest {
                detail: "unexpected response status 418".into(),
            },
        ),
    ];
    for (status, expected) in cases {
        let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::new(status, "")]));
        let (ctx, _clock) = ctx_for(transport);
        let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
            .await
            .unwrap_err();
        assert_eq!(err, expected, "status {status}");
    }
}

#[tokio::test]
async fn retry_after_headers_are_honored_and_hostile_values_default() {
    for (header, expected) in [(Some("2"), 2_000u64), (Some("later"), 1_000), (None, 1_000)] {
        let mut response = ScriptedResponse::new(429, "");
        if let Some(value) = header {
            response = response.header("retry-after", value);
        }
        let transport = Arc::new(ScriptedTransport::new([response]));
        let (ctx, _clock) = ctx_for(transport);
        let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            AcquisitionError::RateLimited {
                retry_after_ms: expected
            }
        );
        assert_eq!(
            ctx.with_quota_state(|quota| quota.retry_at_ms),
            Some(1_000_000 + expected)
        );
    }
}

#[tokio::test]
async fn an_exhausted_quota_refuses_before_any_request_leaves() {
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json("{}")]));
    let (ctx, _clock) = ctx_for(transport.clone());
    ctx.with_quota_state(|quota| {
        quota.window.limit = 1;
        quota.try_acquire(1_000_000).unwrap();
    });
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        AcquisitionError::QuotaExhausted {
            reset_ms: 1_060_000
        }
    );
    assert_eq!(transport.request_count(), 0);
}

#[tokio::test]
async fn cancellation_stops_an_in_flight_request() {
    let gate = Arc::new(Gate::new());
    let transport = Arc::new(ScriptedTransport::gated(
        [ScriptedResponse::json(r#"{"ok":true}"#)],
        gate.clone(),
    ));
    let (ctx, _clock) = ctx_for(transport.clone());
    let token = ctx.cancellation().clone();
    let fetcher = tokio::spawn({
        let ctx = ctx.clone();
        async move { fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL)).await }
    });
    gate.wait_entered(1).await;
    token.cancel();
    let err = tokio::time::timeout(Duration::from_secs(5), fetcher)
        .await
        .expect("cancellation must interrupt the in-flight request")
        .unwrap()
        .unwrap_err();
    assert_eq!(err, AcquisitionError::Cancelled);
    assert_eq!(transport.request_count(), 1);
    gate.release_all();
}

#[tokio::test]
async fn a_past_deadline_refuses_before_the_transport() {
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json("{}")]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let ctx = ctx.with_deadline_ms(999_999);
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(err, AcquisitionError::Deadline);
    assert_eq!(transport.request_count(), 0);
}

#[tokio::test]
async fn the_request_timeout_is_typed_and_degrades_health() {
    let gate = Arc::new(Gate::new());
    let transport = Arc::new(ScriptedTransport::gated(
        [ScriptedResponse::json(r#"{"ok":true}"#)],
        gate.clone(),
    ));
    let (ctx, _clock) = ctx_for(transport.clone());
    let mut bounds = policy();
    bounds.request_timeout_ms = 100;
    let ctx = ctx.with_policy(bounds);
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(err, AcquisitionError::NetworkTimeout);
    assert_eq!(ctx.health().as_str(), "degraded");
    gate.release_all();
}

#[test]
fn url_and_nesting_helpers_refuse_hostile_input() {
    for bad in [
        "",
        "ftp://h/p",
        "file:///etc/passwd",
        "https://user:pass@h/p",
        "https://h/p\nx",
        &format!("https://h/{}", "x".repeat(5000)),
    ] {
        assert!(
            validate_fetch_url(bad, 4096).is_err(),
            "{bad:?} must be refused"
        );
    }
    assert!(validate_fetch_url("https://h/p", 4096).is_ok());

    let deep = format!("{}1{}", "[".repeat(40), "]".repeat(40));
    assert_eq!(
        parse_bounded_json(deep.as_bytes(), 32).unwrap_err(),
        AcquisitionError::NestingTooDeep {
            limit: 32,
            observed: 33
        }
    );
    assert_eq!(check_json_nesting(br#"{"a":"}}}}"}"#, 1).unwrap(), 1);

    for bad in ["file:///x", "javascript:alert(1)", "//u:p@h/x"] {
        assert!(resolve_redirect("https://h/a", bad, 4096).is_err());
    }
    assert_eq!(
        resolve_redirect("https://h/a/b", "../c", 4096).unwrap(),
        "https://h/c"
    );

    // An injected egress denial maps to the typed egress class.
    assert_eq!(
        AcquisitionError::from(EgressError::Transport("reset".into())),
        AcquisitionError::NetworkTimeout
    );
}
