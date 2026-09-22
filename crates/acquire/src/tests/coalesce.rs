//! Coalescing suite: one external request for N concurrent awaiters,
//! gate-controlled so the assertion is exact, and honest cancellation.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crate::coalesce::Coalescer;
use crate::error::{AcquisitionError, VerificationKind};
use crate::http::{fetch_direct_http, HttpAcquisition, HttpFetch};
use crate::request::{
    AcquisitionKey, AcquisitionRequest, RequestedField, RequestedFields, RequestedFreshness,
};
use crate::tests::{ctx_for, Gate, ScriptedResponse, ScriptedTransport, TEST_URL};

fn key(identity: &str, freshness: RequestedFreshness) -> AcquisitionKey {
    AcquisitionRequest::new(
        identity,
        RequestedFields::of([RequestedField::Identity]),
        freshness,
    )
    .unwrap()
    .coalescing_key()
    .unwrap()
}

async fn wait_for_waiters(coalescer: &Coalescer<HttpAcquisition>, expected: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while coalescer.stats().waiters < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every follower must attach before the gate opens");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n_concurrent_callers_issue_exactly_one_external_request() {
    let gate = Arc::new(Gate::new());
    let transport = Arc::new(ScriptedTransport::gated(
        [ScriptedResponse::json(r#"{"value":7}"#)],
        gate.clone(),
    ));
    let (ctx, _clock) = ctx_for(transport.clone());
    let coalescer = Arc::new(Coalescer::<HttpAcquisition>::new());
    let key = key(TEST_URL, RequestedFreshness::Live);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let coalescer = coalescer.clone();
        let ctx = ctx.clone();
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            coalescer
                .run(key, || async {
                    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL)).await
                })
                .await
        }));
    }

    gate.wait_entered(1).await;
    wait_for_waiters(&coalescer, 7).await;
    // One external request is in flight; nothing else may have entered.
    assert_eq!(transport.request_count(), 1);
    gate.release_one();

    let mut digests = BTreeSet::new();
    for handle in handles {
        let acquisition = handle.await.unwrap().unwrap();
        digests.insert(
            acquisition
                .provenance
                .as_ref()
                .unwrap()
                .content_digest
                .clone(),
        );
    }
    assert_eq!(digests.len(), 1, "every awaiter shares one observation");
    assert_eq!(transport.request_count(), 1);
    let stats = coalescer.stats();
    assert_eq!(stats.leaders, 1);
    assert_eq!(stats.waiters, 7);
    assert_eq!(coalescer.in_flight(), 0);
}

#[tokio::test]
async fn different_identities_freshness_or_scopes_never_coalesce() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::json(r#"{"n":1}"#),
        ScriptedResponse::json(r#"{"n":2}"#),
        ScriptedResponse::json(r#"{"n":3}"#),
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let coalescer = Coalescer::<HttpAcquisition>::new();
    let op = || async { fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL)).await };

    coalescer
        .run(key(TEST_URL, RequestedFreshness::Live), op)
        .await
        .unwrap();
    coalescer
        .run(key(TEST_URL, RequestedFreshness::CacheOnly), op)
        .await
        .unwrap();
    coalescer
        .run(
            key("https://example.invalid/other", RequestedFreshness::Live),
            op,
        )
        .await
        .unwrap();
    assert_eq!(transport.request_count(), 3);
    assert_eq!(coalescer.stats().leaders, 3);
    assert_eq!(coalescer.in_flight(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_typed_failure_is_shared_with_every_awaiter() {
    let gate = Arc::new(Gate::new());
    let transport = Arc::new(ScriptedTransport::gated(
        [
            ScriptedResponse::new(401, ""),
            // If coalescing breaks, a second request gets this success and
            // the assertion below exposes it.
            ScriptedResponse::json(r#"{"leaked":true}"#),
        ],
        gate.clone(),
    ));
    let (ctx, _clock) = ctx_for(transport.clone());
    let coalescer = Arc::new(Coalescer::<HttpAcquisition>::new());
    let key = key(TEST_URL, RequestedFreshness::Live);

    let mut handles = Vec::new();
    for _ in 0..5 {
        let coalescer = coalescer.clone();
        let ctx = ctx.clone();
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            coalescer
                .run(key, || async {
                    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL)).await
                })
                .await
        }));
    }
    gate.wait_entered(1).await;
    wait_for_waiters(&coalescer, 4).await;
    gate.release_all();

    for handle in handles {
        let err = handle.await.unwrap().unwrap_err();
        assert_eq!(err, AcquisitionError::AuthenticationRequired);
    }
    assert_eq!(transport.request_count(), 1);
    assert_eq!(coalescer.stats().waiters, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_of_the_leader_releases_awaiters_and_frees_the_slot() {
    let gate = Arc::new(Gate::new());
    let transport = Arc::new(ScriptedTransport::gated(
        [ScriptedResponse::json(r#"{"never":"released"}"#)],
        gate.clone(),
    ));
    let (ctx, _clock) = ctx_for(transport.clone());
    let coalescer = Arc::new(Coalescer::<HttpAcquisition>::new());
    let key = key(TEST_URL, RequestedFreshness::Live);
    let token = ctx.cancellation().clone();

    let spawn = |coalescer: Arc<Coalescer<HttpAcquisition>>, ctx: crate::AcquireCtx| {
        let key = key.clone();
        tokio::spawn(async move {
            coalescer
                .run(key, || async {
                    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL)).await
                })
                .await
        })
    };

    let leader = spawn(coalescer.clone(), ctx.clone());
    gate.wait_entered(1).await;
    let follower = spawn(coalescer.clone(), ctx.clone());
    wait_for_waiters(&coalescer, 1).await;
    token.cancel();

    let leader_result = tokio::time::timeout(Duration::from_secs(5), leader)
        .await
        .expect("the leader must abort")
        .unwrap();
    assert_eq!(leader_result.unwrap_err(), AcquisitionError::Cancelled);
    let follower_result = tokio::time::timeout(Duration::from_secs(5), follower)
        .await
        .expect("awaiters must never hang when the leader is cancelled")
        .unwrap();
    assert_eq!(follower_result.unwrap_err(), AcquisitionError::Cancelled);
    assert_eq!(transport.request_count(), 1);
    assert_eq!(
        coalescer.in_flight(),
        0,
        "the slot is freed for the next caller"
    );
    gate.release_all();

    // Verification: a follow-up caller is a fresh leader and executes.
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json("{}")]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let again = coalescer
        .run(key, || async {
            fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL)).await
        })
        .await;
    assert!(again.is_ok());
    assert_eq!(coalescer.stats().leaders, 2);
    let _ = VerificationKind::Unknown;
}
