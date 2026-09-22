//! Quota suite: documented windows, exact reset accounting, Retry-After
//! honoring and bounded adaptive throttling, all through the context.

use std::sync::Arc;

use crate::error::AcquisitionError;
use crate::http::{fetch_direct_http, HttpFetch};
use crate::quota::{QuotaState, QuotaWindow, MAX_THROTTLE_SCALE};
use crate::tests::{ctx_for, ScriptedResponse, ScriptedTransport, TEST_URL};

const NOW: u64 = 1_000_000;

fn ctx_with_quota(
    responses: impl IntoIterator<Item = ScriptedResponse>,
    quota: QuotaState,
) -> (
    crate::AcquireCtx,
    Arc<faktor_core::time::TestClock>,
    Arc<ScriptedTransport>,
) {
    let transport = Arc::new(ScriptedTransport::new(responses));
    let (ctx, clock) = ctx_for(transport.clone());
    (ctx.with_quota(quota), clock, transport)
}

#[tokio::test]
async fn window_reset_restores_budget_and_clears_the_exhaustion_record() {
    let (ctx, clock, transport) = ctx_with_quota(
        [
            ScriptedResponse::json(r#"{"n":1}"#),
            ScriptedResponse::json(r#"{"n":2}"#),
            ScriptedResponse::json(r#"{"n":3}"#),
        ],
        QuotaState::new(QuotaWindow::per_minute(2), NOW),
    );
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();

    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        AcquisitionError::QuotaExhausted {
            reset_ms: NOW + 60_000
        }
    );
    assert_eq!(
        transport.request_count(),
        2,
        "refused attempt makes no request"
    );
    assert!(ctx.with_quota_state(|quota| quota.is_exhausted_at(NOW + 59_999)));

    // The reset instant restores the budget exactly.
    clock.set(NOW as i64 + 60_000);
    assert!(!ctx.with_quota_state(|quota| quota.is_exhausted_at(NOW + 60_000)));
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();
    assert_eq!(transport.request_count(), 3);
    assert_eq!(ctx.with_quota_state(|quota| quota.used), 1);
    assert_eq!(
        ctx.with_quota_state(|quota| quota.window_started_ms),
        NOW + 60_000
    );
}

#[tokio::test]
async fn retry_after_is_honored_without_consuming_further_budget() {
    let (ctx, clock, transport) = ctx_with_quota(
        [
            ScriptedResponse::new(429, "").header("retry-after", "30"),
            ScriptedResponse::json(r#"{"ok":true}"#),
        ],
        QuotaState::new(QuotaWindow::per_minute(10), NOW),
    );
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        AcquisitionError::RateLimited {
            retry_after_ms: 30_000
        }
    );
    assert_eq!(ctx.with_quota_state(|quota| quota.used), 1);

    // While the gate is installed every attempt refuses before the transport
    // and never consumes budget.
    for _ in 0..3 {
        let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AcquisitionError::RateLimited { retry_after_ms } if retry_after_ms <= 30_000
        ));
    }
    assert_eq!(transport.request_count(), 1);
    assert_eq!(ctx.with_quota_state(|quota| quota.used), 1);

    clock.set(NOW as i64 + 30_000);
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();
    assert_eq!(transport.request_count(), 2);
}

#[tokio::test]
async fn the_retry_after_gate_is_clamped_to_the_window_reset() {
    let (ctx, _clock, transport) = ctx_with_quota(
        [ScriptedResponse::json(r#"{"ok":true}"#)],
        QuotaState::new(QuotaWindow::per_minute(5), NOW),
    );
    ctx.record_retry_after(600_000);
    assert_eq!(
        ctx.with_quota_state(|quota| quota.retry_at_ms),
        Some(NOW + 60_000),
        "a Retry-After can never extend beyond the window reset"
    );
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        AcquisitionError::RateLimited {
            retry_after_ms: 60_000
        }
    );
    assert_eq!(transport.request_count(), 0);
}

#[tokio::test]
async fn adaptive_throttling_shrinks_bounded_and_success_restores() {
    use faktor_provider::egress::EgressError;
    let (ctx, clock, transport) = ctx_with_quota(
        [
            ScriptedResponse::failure(EgressError::Transport("reset".into())),
            ScriptedResponse::failure(EgressError::Transport("reset".into())),
            ScriptedResponse::json(r#"{"ok":true}"#),
        ],
        QuotaState::new(QuotaWindow::per_minute(8), NOW),
    );
    for _ in 0..10 {
        ctx.report_transient_throttle();
    }
    assert_eq!(
        ctx.with_quota_state(|quota| quota.throttle_scale),
        MAX_THROTTLE_SCALE
    );
    assert_eq!(ctx.with_quota_state(|quota| quota.effective_limit()), 2);

    // Both throttled permits are consumed by failing attempts; the third
    // attempt is refused before the transport (the throttle is real).
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    let err = fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert!(matches!(err, AcquisitionError::QuotaExhausted { .. }));
    assert_eq!(transport.request_count(), 2);

    // Success restores throughput one step at a time.
    ctx.report_success();
    ctx.report_success();
    ctx.report_success();
    assert_eq!(ctx.with_quota_state(|quota| quota.throttle_scale), 1);
    assert_eq!(ctx.with_quota_state(|quota| quota.effective_limit()), 8);
    clock.set(NOW as i64 + 60_000);
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();
    assert_eq!(transport.request_count(), 3);
}

#[test]
fn hostile_retry_after_values_are_rejected_never_guessed() {
    use crate::quota::parse_retry_after_seconds;
    assert_eq!(parse_retry_after_seconds("7"), Some(7_000));
    assert_eq!(
        parse_retry_after_seconds("Wed, 21 Oct 2015 07:28:00 GMT"),
        None
    );
    assert_eq!(parse_retry_after_seconds("-1"), None);
    assert_eq!(parse_retry_after_seconds("999999999999999999999999"), None);

    let mut quota = QuotaState::new(QuotaWindow::per_day(1), NOW);
    quota.record_retry_after(0, NOW);
    assert_eq!(quota.retry_at_ms, Some(NOW));
    quota.try_acquire(NOW).unwrap();
    // A zero Retry-After is a no-op gate, not a permanent lock.
    assert_eq!(
        quota.try_acquire(NOW).unwrap_err(),
        AcquisitionError::QuotaExhausted {
            reset_ms: NOW + 86_400_000
        }
    );
}
