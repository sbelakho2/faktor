//! Retry suite: transient-only retries with bounded attempts and backoff,
//! driven through the scripted transport (injected failures, never the
//! network). Auth, verification and quota failures must surface immediately.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use faktor_provider::egress::EgressError;

use crate::error::{AcquisitionError, VerificationKind};
use crate::http::{fetch_direct_http, HttpFetch};
use crate::retry::{run_with_retry, AcquisitionRetryPolicy};
use crate::tests::{ctx_for, ScriptedResponse, ScriptedTransport, TEST_URL};

fn policy(max_attempts: u32) -> AcquisitionRetryPolicy {
    AcquisitionRetryPolicy {
        max_attempts,
        base_backoff_ms: 1,
        max_backoff_ms: 2,
        jitter_percent: 0,
        seed: 1,
    }
}

async fn fetch_once(ctx: &crate::AcquireCtx) -> Result<crate::HttpAcquisition, AcquisitionError> {
    fetch_direct_http(ctx, &HttpFetch::get(TEST_URL)).await
}

#[tokio::test]
async fn transient_transport_failures_are_retried_until_success() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::failure(EgressError::Transport("connect reset".into())),
        ScriptedResponse::failure(EgressError::Transport("connection closed".into())),
        ScriptedResponse::json(r#"{"ok":true}"#),
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let acquisition = run_with_retry(&ctx, &policy(5), |_| fetch_once(&ctx))
        .await
        .expect("the third attempt succeeds");
    assert_eq!(acquisition.status, 200);
    assert_eq!(transport.request_count(), 3);
}

#[tokio::test]
async fn attempts_are_bounded_even_for_transient_failures() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let err = run_with_retry(&ctx, &policy(3), |_| fetch_once(&ctx))
        .await
        .unwrap_err();
    assert_eq!(err, AcquisitionError::NetworkTimeout);
    assert_eq!(transport.request_count(), 3, "max_attempts is a hard bound");
}

#[tokio::test]
async fn authentication_and_verification_surface_immediately() {
    for (status, expected) in [
        (401, AcquisitionError::AuthenticationRequired),
        (
            403,
            AcquisitionError::VerificationRequired {
                kind: VerificationKind::Challenge,
            },
        ),
    ] {
        let transport = Arc::new(ScriptedTransport::new([
            ScriptedResponse::new(status, ""),
            ScriptedResponse::json(r#"{"ok":true}"#),
        ]));
        let (ctx, _clock) = ctx_for(transport.clone());
        let err = run_with_retry(&ctx, &policy(5), |_| fetch_once(&ctx))
            .await
            .unwrap_err();
        assert_eq!(err, expected);
        assert_eq!(
            transport.request_count(),
            1,
            "a human-required failure is never retried"
        );
    }
}

#[tokio::test]
async fn rate_limit_surfaces_with_retry_after_and_is_not_retried() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::new(429, "").header("retry-after", "30")
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let started = tokio::time::Instant::now();
    let err = run_with_retry(
        &ctx,
        &AcquisitionRetryPolicy {
            base_backoff_ms: 60_000,
            max_backoff_ms: 60_000,
            ..policy(5)
        },
        |_| fetch_once(&ctx),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        AcquisitionError::RateLimited {
            retry_after_ms: 30_000
        }
    );
    assert_eq!(transport.request_count(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the runtime must not sleep through a rate limit"
    );
    // The honored Retry-After gate refuses the next attempt before any
    // request leaves the process.
    let err = fetch_once(&ctx).await.unwrap_err();
    assert_eq!(
        err,
        AcquisitionError::RateLimited {
            retry_after_ms: 30_000
        }
    );
    assert_eq!(transport.request_count(), 1);
}

#[tokio::test]
async fn upstream_5xx_is_transient_and_bounded() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::new(502, ""),
        ScriptedResponse::new(503, ""),
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let err = run_with_retry(&ctx, &policy(2), |_| fetch_once(&ctx))
        .await
        .unwrap_err();
    assert_eq!(err, AcquisitionError::ApiUnavailable);
    assert_eq!(transport.request_count(), 2);
}

#[tokio::test]
async fn cancellation_during_backoff_aborts_without_further_attempts() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::json(r#"{"ok":true}"#),
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    let token = ctx.cancellation().clone();
    let entered_backoff = Arc::new(tokio::sync::Notify::new());
    let signal = entered_backoff.clone();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let runner = tokio::spawn({
        let ctx = ctx.clone();
        let op_ctx = ctx.clone();
        async move {
            run_with_retry(
                &ctx,
                &AcquisitionRetryPolicy {
                    max_attempts: 5,
                    base_backoff_ms: 60_000,
                    max_backoff_ms: 60_000,
                    jitter_percent: 0,
                    seed: 0,
                },
                move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    signal.notify_one();
                    let ctx = op_ctx.clone();
                    async move { fetch_once(&ctx).await }
                },
            )
            .await
        }
    });
    entered_backoff.notified().await;
    token.cancel();
    let err = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("cancellation must abort the backoff")
        .unwrap()
        .unwrap_err();
    assert_eq!(err, AcquisitionError::Cancelled);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(transport.request_count(), 1);
}

#[tokio::test]
async fn a_success_restores_the_adaptive_throttle() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::json(r#"{"ok":true}"#),
    ]));
    let (ctx, _clock) = ctx_for(transport.clone());
    run_with_retry(&ctx, &policy(5), |_| fetch_once(&ctx))
        .await
        .unwrap();
    // Two transient failures scaled the throttle down, one success restored
    // it one step (throttle_scale 1 -> 3 -> 2).
    let scale = ctx.with_quota_state(|quota| quota.throttle_scale);
    assert_eq!(scale, 2);
    assert_eq!(transport.request_count(), 3);
}
