//! Cache-policy suite: field-level freshness classes, account-scope
//! isolation, conditional revalidation and the no-duplicate-snapshot rule.

use std::sync::Arc;

use crate::cache::{
    freshness_class, CacheEntry, CacheKey, CacheState, ConditionalValidators, FieldFreshnessClass,
    FreshnessClassTtl,
};
use crate::http::{fetch_direct_http, store_acquisition, HttpFetch};
use crate::request::{AcquisitionRequest, RequestedField, RequestedFields, RequestedFreshness};
use crate::tests::{ctx_for, ScriptedResponse, ScriptedTransport, TEST_URL};

fn entry(observed_at_ms: u64, class: FieldFreshnessClass, etag: Option<&str>) -> CacheEntry {
    CacheEntry {
        observed_at_ms,
        class,
        etag: etag.map(str::to_string),
        last_modified: None,
        content_digest: Some("digest".into()),
        content: Some(serde_json::json!({"kept": true})),
    }
}

#[test]
fn freshness_classes_are_documented_per_field_with_documented_ttls() {
    let ttl = FreshnessClassTtl::default();
    assert_eq!(ttl.slow_ms, 86_400_000);
    assert_eq!(ttl.moderate_ms, 1_800_000);
    assert_eq!(ttl.fast_ms, 900_000);
    for field in [
        RequestedField::Identity,
        RequestedField::Descriptive,
        RequestedField::Packaging,
    ] {
        assert_eq!(freshness_class(field), FieldFreshnessClass::Slow);
    }
    for field in [RequestedField::AccountTerms, RequestedField::Provenance] {
        assert_eq!(freshness_class(field), FieldFreshnessClass::Moderate);
    }
    for field in [
        RequestedField::Availability,
        RequestedField::QuantityTiers,
        RequestedField::BulkSet,
    ] {
        assert_eq!(freshness_class(field), FieldFreshnessClass::Fast);
    }

    // A fast field goes stale while a slow field of the same age is fresh.
    let now = 10_000_000;
    let fast = entry(now - ttl.fast_ms - 1, FieldFreshnessClass::Fast, None);
    let slow = entry(now - ttl.fast_ms - 1, FieldFreshnessClass::Slow, None);
    assert!(!fast.is_fresh(&ttl, now));
    assert!(slow.is_fresh(&ttl, now));
}

#[test]
fn cache_identity_includes_the_account_scope() {
    let mut cache = CacheState::new();
    let ttl = FreshnessClassTtl::default();
    cache.insert(
        CacheKey::new(
            "https://h/p",
            Some("acct-1".into()),
            RequestedField::Identity,
        ),
        entry(0, FieldFreshnessClass::Slow, Some("\"a\"")),
    );
    assert!(cache
        .entry("https://h/p", Some("acct-1"), RequestedField::Identity)
        .is_some());
    let unscoped = cache.coverage("https://h/p", None, [RequestedField::Identity], &ttl, 0);
    assert_eq!(unscoped.missing, vec![RequestedField::Identity]);
    let other_scope = cache.coverage(
        "https://h/p",
        Some("acct-2"),
        [RequestedField::Identity],
        &ttl,
        0,
    );
    assert_eq!(other_scope.missing, vec![RequestedField::Identity]);
    // Validators never leak across scopes either.
    assert!(cache
        .conditional_validators("https://h/p", None, [RequestedField::Identity])
        .is_none());
}

#[tokio::test]
async fn a_304_keeps_the_stored_snapshot_and_its_digest() {
    let mut cache = CacheState::new();
    let key = CacheKey::new(TEST_URL, None, RequestedField::Descriptive);
    cache.insert(
        key.clone(),
        entry(123, FieldFreshnessClass::Slow, Some("\"stable\"")),
    );
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::new(
        304,
        Vec::new(),
    )
    .header("etag", "\"stable\"")]));
    let (ctx, _clock) = ctx_for(transport);
    let fetch = HttpFetch::get(TEST_URL).with_validators(cache.get(&key).unwrap().validators());
    let acquisition = fetch_direct_http(&ctx, &fetch).await.unwrap();
    let len_before = cache.len();
    assert!(!store_acquisition(
        &mut cache,
        key.clone(),
        FieldFreshnessClass::Slow,
        &acquisition
    ));
    assert_eq!(cache.len(), len_before);
    let stored = cache.get(&key).unwrap();
    assert_eq!(stored.content_digest.as_deref(), Some("digest"));
    assert_eq!(stored.observed_at_ms, 123);
    assert_eq!(stored.content, Some(serde_json::json!({"kept": true})));
}

#[tokio::test]
async fn a_200_replaces_the_snapshot_with_the_new_digest() {
    let mut cache = CacheState::new();
    let key = CacheKey::new(TEST_URL, None, RequestedField::Descriptive);
    cache.insert(
        key.clone(),
        entry(123, FieldFreshnessClass::Slow, Some("\"old\"")),
    );
    let body = br#"{"fresh":true}"#.to_vec();
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json(
        body.clone(),
    )
    .header("etag", "\"new\"")]));
    let (ctx, _clock) = ctx_for(transport);
    let fetch = HttpFetch::get(TEST_URL).with_validators(cache.get(&key).unwrap().validators());
    let acquisition = fetch_direct_http(&ctx, &fetch).await.unwrap();
    assert!(store_acquisition(
        &mut cache,
        key.clone(),
        FieldFreshnessClass::Slow,
        &acquisition
    ));
    let stored = cache.get(&key).unwrap();
    assert_eq!(stored.etag.as_deref(), Some("\"new\""));
    assert_eq!(
        stored.content_digest.as_deref(),
        acquisition
            .provenance
            .as_ref()
            .map(|p| p.content_digest.as_str())
    );
    assert_eq!(stored.observed_at_ms, 1_000_000);
}

#[test]
fn conditional_validators_merge_deterministically_and_prefer_etag() {
    let mut cache = CacheState::new();
    let mut first = entry(0, FieldFreshnessClass::Slow, None);
    first.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".into());
    cache.insert(
        CacheKey::new("https://h/p", None, RequestedField::Identity),
        first,
    );
    cache.insert(
        CacheKey::new("https://h/p", None, RequestedField::Descriptive),
        entry(0, FieldFreshnessClass::Slow, Some("\"etag\"")),
    );
    let validators = cache
        .conditional_validators(
            "https://h/p",
            None,
            [RequestedField::Identity, RequestedField::Descriptive],
        )
        .unwrap();
    assert_eq!(validators.etag.as_deref(), Some("\"etag\""));
    assert_eq!(
        validators.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    assert_eq!(
        validators.headers(),
        vec![
            ("if-none-match".to_string(), "\"etag\"".to_string()),
            (
                "if-modified-since".to_string(),
                "Wed, 21 Oct 2015 07:28:00 GMT".to_string()
            ),
        ]
    );
    // An unwritten field contributes nothing and is never invented.
    assert!(cache
        .conditional_validators("https://h/p", None, [RequestedField::BulkSet])
        .is_none());
}

#[test]
fn coverage_reports_fresh_stale_and_missing_without_mutating() {
    let mut cache = CacheState::new();
    let ttl = FreshnessClassTtl::default();
    cache.insert(
        CacheKey::new("https://h/p", None, RequestedField::Identity),
        entry(0, FieldFreshnessClass::Slow, None),
    );
    cache.insert(
        CacheKey::new("https://h/p", None, RequestedField::Availability),
        entry(0, FieldFreshnessClass::Fast, None),
    );
    let now = ttl.fast_ms + 1;
    let coverage = cache.coverage(
        "https://h/p",
        None,
        [
            RequestedField::Identity,
            RequestedField::Availability,
            RequestedField::BulkSet,
        ],
        &ttl,
        now,
    );
    assert_eq!(coverage.fresh, vec![RequestedField::Identity]);
    assert_eq!(coverage.stale, vec![RequestedField::Availability]);
    assert_eq!(coverage.missing, vec![RequestedField::BulkSet]);
    assert!(!coverage.all_fresh());
    assert!(!coverage.all_present());
    assert_eq!(cache.len(), 2, "coverage never mutates the cache");
}

#[test]
fn requests_distinguish_freshness_policies_for_cache_identity() {
    let live = AcquisitionRequest::new(
        "https://h/p",
        RequestedFields::of([RequestedField::Identity]),
        RequestedFreshness::Live,
    )
    .unwrap();
    let cache_only = AcquisitionRequest::new(
        "https://h/p",
        RequestedFields::of([RequestedField::Identity]),
        RequestedFreshness::CacheOnly,
    )
    .unwrap();
    assert_ne!(
        live.coalescing_key().unwrap(),
        cache_only.coalescing_key().unwrap()
    );
    assert_eq!(
        ConditionalValidators::from_headers([("ETag", "\"v\""), ("etag", "\"w\"")])
            .etag
            .as_deref(),
        Some("\"v\"")
    );
}
