//! Adversarial tests of the durable billing-report schedule: two periods
//! reported exactly once each across a restart, failure-then-success
//! reporting once, a final 4xx marking the period failed typed with no
//! retry, cursor continuation persisted before the next page send, and the
//! jittered bounded backoff.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{
    BillingAccountId, BillingConfig, BillingStore, BillingVendorAdapter, BillingVendorConfig,
    Clock, DurableSpendRow, EntitlementService, ManualClock, OrganizationId, ReportPeriodStatus,
    SpendCategory, SqliteControlPlaneStore,
};
use faktor_provider::egress::PolicyCheckedHttpTransport;

use super::{backoff_ms, period_key, BillingReportRunner, ReportPolicy, TickOutcome};
use crate::test_http::{MockServer, Reply};

const VENDOR_PATH: &str = "/v1/usage-reports";

fn transport() -> Arc<dyn faktor_provider::egress::HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::with_policy(None))
}

fn policy() -> ReportPolicy {
    ReportPolicy {
        interval_ms: 60_000,
        period_ms: 60_000,
        max_attempts: 3,
        retry_base_ms: 250,
        max_backoff_ms: 5_000,
        max_catch_up: 24,
    }
}

struct Harness {
    runner: BillingReportRunner,
    store: Arc<dyn BillingStore>,
    service: Arc<EntitlementService>,
    clock: Arc<ManualClock>,
    organization: OrganizationId,
}

fn harness(
    db: &std::path::Path,
    mock: &MockServer,
    clock: Arc<ManualClock>,
    policy: ReportPolicy,
) -> Harness {
    let store: Arc<dyn BillingStore> = Arc::new(SqliteControlPlaneStore::open(db).unwrap());
    let service = EntitlementService::new(
        store.clone(),
        clock.clone(),
        BillingConfig {
            default_plan: None,
            plans: BTreeMap::new(),
            managed_providers: Default::default(),
        },
    )
    .unwrap();
    let organization = OrganizationId::try_new("org_report").unwrap();
    let account = BillingAccountId::try_new("acct_report").unwrap();
    service
        .ensure_account(&organization, &account, "report-test", true)
        .unwrap();
    // One durable spend row so the reported fold is non-trivial.
    service
        .ingest_spend_row(
            &organization,
            &account,
            &DurableSpendRow {
                organization_id: organization.as_str().to_string(),
                session_id: 1,
                task_id: 2,
                reservation_id: 3,
                attempt_id: "attempt-3".into(),
                provider: "managed".into(),
                model: "m".into(),
                input_tokens: 100,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                provider_cost_micro: 500,
                provider_reported_micro: 500,
                state: "settled".into(),
                occurred_at_ms: 1_500,
                source_operation: "provider_call".into(),
            },
            SpendCategory::Managed,
        )
        .unwrap();
    let adapter = BillingVendorAdapter::new(
        service.clone(),
        BillingVendorConfig {
            base_url: mock.base(),
            // One page attempt per tick: the SCHEDULE owns the retry policy
            // (the vendor adapter's own retry matrix is exercised in the
            // cloud crate's tests).
            max_attempts: 1,
            retry_base_ms: 0,
            ..Default::default()
        },
        transport(),
        None,
    )
    .unwrap();
    let runner = BillingReportRunner::new(
        store.clone(),
        adapter,
        organization.clone(),
        policy,
        clock.clone(),
    );
    Harness {
        runner,
        store,
        service,
        clock,
        organization,
    }
}

#[tokio::test]
async fn two_periods_are_reported_exactly_once_each_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("billing.db");
    let mock = MockServer::start().await;
    mock.push("POST", VENDOR_PATH, Reply::json(200, serde_json::json!({})));
    mock.push("POST", VENDOR_PATH, Reply::json(200, serde_json::json!({})));
    let clock = Arc::new(ManualClock::new(1_000));
    {
        let h = harness(&db, &mock, clock.clone(), policy());
        match h.runner.tick().await.unwrap() {
            TickOutcome::Reported { period, .. } => assert_eq!(period, "p0"),
            other => panic!("expected Reported(p0), got {other:?}"),
        }
        // Same period again: nothing is re-sent.
        assert_eq!(h.runner.tick().await.unwrap(), TickOutcome::Idle);
        assert_eq!(mock.request_count(), 1);
        clock.advance(60_000);
        match h.runner.tick().await.unwrap() {
            TickOutcome::Reported { period, .. } => assert_eq!(period, "p1"),
            other => panic!("expected Reported(p1), got {other:?}"),
        }
        assert_eq!(mock.requests("POST", VENDOR_PATH).len(), 2);
        assert_eq!(h.clock.now_ms(), 61_000);
        let rows = h.store.report_periods(&h.organization, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|r| r.status == ReportPeriodStatus::Reported));
        drop(h);
    }
    // Restart: the same file, a new runner. The current period is already
    // reported, so nothing is ever re-sent.
    let h = harness(&db, &mock, clock, policy());
    assert_eq!(h.runner.tick().await.unwrap(), TickOutcome::Idle);
    assert_eq!(
        mock.requests("POST", VENDOR_PATH).len(),
        2,
        "each period was reported exactly once"
    );
    let bodies: Vec<String> = mock
        .requests("POST", VENDOR_PATH)
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert!(bodies[0].contains(r#""period":"p0""#), "{}", bodies[0]);
    assert!(bodies[0].contains(r#""organization":"org_report""#));
    assert!(bodies[0].contains(r#""input_tokens":100"#), "{}", bodies[0]);
    assert!(bodies[1].contains(r#""period":"p1""#), "{}", bodies[1]);
    let keys: Vec<String> = mock
        .requests("POST", VENDOR_PATH)
        .iter()
        .map(|r| r.header("idempotency-key").unwrap_or_default().to_string())
        .collect();
    assert!(keys[0].ends_with(":p0:head"), "{}", keys[0]);
    assert!(keys[1].ends_with(":p1:head"), "{}", keys[1]);
    let _ = h.service;
}

#[tokio::test]
async fn a_failed_report_retries_and_reports_the_period_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("billing.db");
    let mock = MockServer::start().await;
    mock.push("POST", VENDOR_PATH, Reply::text(500, "boom"));
    mock.push("POST", VENDOR_PATH, Reply::json(200, serde_json::json!({})));
    let clock = Arc::new(ManualClock::new(1_000));
    let mut p = policy();
    p.retry_base_ms = 0;
    let h = harness(&db, &mock, clock.clone(), p);
    match h.runner.tick().await.unwrap() {
        TickOutcome::RetryScheduled { attempts, .. } => assert_eq!(attempts, 1),
        other => panic!("expected RetryScheduled, got {other:?}"),
    }
    let row = h
        .store
        .report_period(&h.organization, "p0")
        .unwrap()
        .unwrap();
    assert_eq!(row.status, ReportPeriodStatus::Open);
    assert_eq!(row.attempts, 1);
    assert!(row.last_error.as_deref().unwrap().contains("500"));
    // The backoff of a zero base is zero: the retry is due immediately and
    // succeeds exactly once.
    match h.runner.tick().await.unwrap() {
        TickOutcome::Reported { period, .. } => assert_eq!(period, "p0"),
        other => panic!("expected Reported, got {other:?}"),
    }
    assert_eq!(h.runner.tick().await.unwrap(), TickOutcome::Idle);
    assert_eq!(mock.request_count(), 2);
    // Record-before-send idempotency: the retry replays the EXACT same
    // period/cursor key, so a vendor can never observe two distinct reports
    // for one period.
    let posts = mock.requests("POST", VENDOR_PATH);
    assert_eq!(
        posts[0].header("idempotency-key"),
        posts[1].header("idempotency-key")
    );
    let row = h
        .store
        .report_period(&h.organization, "p0")
        .unwrap()
        .unwrap();
    assert_eq!(row.status, ReportPeriodStatus::Reported);
    assert_eq!(row.last_error, None);
    assert!(row.reported_at_ms.is_some());
}

#[tokio::test]
async fn final_4xx_marks_the_period_failed_typed_without_retry() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("billing.db");
    let mock = MockServer::start().await;
    mock.push("POST", VENDOR_PATH, Reply::text(403, "forbidden"));
    let clock = Arc::new(ManualClock::new(1_000));
    let h = harness(&db, &mock, clock.clone(), policy());
    match h.runner.tick().await.unwrap() {
        TickOutcome::Failed { period, error } => {
            assert_eq!(period, "p0");
            assert!(error.contains("403"), "{error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(h.runner.tick().await.unwrap(), TickOutcome::Idle);
    // The failed period is terminal: further ticks in the SAME period never
    // retry it (a later period is a new, independently reported row).
    clock.advance(30_000);
    assert_eq!(h.runner.tick().await.unwrap(), TickOutcome::Idle);
    assert_eq!(mock.request_count(), 1, "a final 4xx is never retried");
    let row = h
        .store
        .report_period(&h.organization, "p0")
        .unwrap()
        .unwrap();
    assert_eq!(row.status, ReportPeriodStatus::Failed);
    assert!(row.last_error.as_deref().unwrap().contains("403"));
}

#[tokio::test]
async fn cursor_continuation_is_recorded_before_the_next_send_and_resumes_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("billing.db");
    let mock = MockServer::start().await;
    mock.push(
        "POST",
        VENDOR_PATH,
        Reply::json(200, serde_json::json!({"next_cursor": "c1"})),
    );
    mock.push("POST", VENDOR_PATH, Reply::json(200, serde_json::json!({})));
    let clock = Arc::new(ManualClock::new(1_000));
    {
        let h = harness(&db, &mock, clock.clone(), policy());
        match h.runner.tick().await.unwrap() {
            TickOutcome::Continued { next_cursor, .. } => assert_eq!(next_cursor, "c1"),
            other => panic!("expected Continued, got {other:?}"),
        }
        let row = h
            .store
            .report_period(&h.organization, "p0")
            .unwrap()
            .unwrap();
        assert_eq!(row.next_cursor.as_deref(), Some("c1"));
        assert_eq!(row.status, ReportPeriodStatus::Open);
        drop(h);
    }
    // Restart mid-period: the recorded cursor is resumed, and the page key
    // is the same one a crash would have replayed.
    let h = harness(&db, &mock, clock, policy());
    match h.runner.tick().await.unwrap() {
        TickOutcome::Reported { period, .. } => assert_eq!(period, "p0"),
        other => panic!("expected Reported, got {other:?}"),
    }
    let posts = mock.requests("POST", VENDOR_PATH);
    assert_eq!(posts.len(), 2);
    assert!(
        posts[1].body.contains(r#""cursor":"c1""#),
        "{}",
        posts[1].body
    );
    assert!(
        posts[1]
            .header("idempotency-key")
            .unwrap_or_default()
            .ends_with(":p0:c1"),
        "{:?}",
        posts[1].header("idempotency-key")
    );
}

#[test]
fn period_keys_are_stable_and_backoff_is_jittered_and_bounded() {
    assert_eq!(period_key(0, 60_000), "p0");
    assert_eq!(period_key(59_999, 60_000), "p0");
    assert_eq!(period_key(60_000, 60_000), "p1");
    assert_eq!(period_key(-1, 60_000), "p-1");

    let p = ReportPolicy {
        interval_ms: 60_000,
        period_ms: 60_000,
        max_attempts: 5,
        retry_base_ms: 1_000,
        max_backoff_ms: 10_000,
        max_catch_up: 24,
    };
    for attempts in 0..12u32 {
        let delay = backoff_ms(&p, "p7", attempts);
        assert!((0..=10_000).contains(&delay), "{attempts}: {delay}");
        if attempts <= 3 {
            let base = 1_000i64 << attempts;
            assert!(
                delay >= base - base / 4 && delay <= base,
                "attempts {attempts}: {delay} outside [{}, {base}]",
                base - base / 4
            );
        }
        // Deterministic for the same (period, attempt).
        assert_eq!(delay, backoff_ms(&p, "p7", attempts));
    }
    // Different periods jitter differently at least once (not lockstep).
    let any_diff = (0..8u32).any(|a| backoff_ms(&p, "p7", a) != backoff_ms(&p, "p8", a));
    assert!(any_diff);
    // A zero retry base yields zero delay (immediate retry policy).
    let zero = ReportPolicy {
        retry_base_ms: 0,
        ..p
    };
    assert_eq!(backoff_ms(&zero, "p7", 3), 0);
}

/// Downtime crossing three whole periods: each missed period is backfilled
/// oldest-first, exactly once, before the regular cadence resumes.
#[tokio::test]
async fn three_missed_periods_are_backfilled_oldest_first_exactly_once_each() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("billing.db");
    let mock = MockServer::start().await;
    for _ in 0..8 {
        mock.push("POST", VENDOR_PATH, Reply::json(200, serde_json::json!({})));
    }
    let clock = Arc::new(ManualClock::new(1_000));
    let h = harness(&db, &mock, clock.clone(), policy());
    match h.runner.tick().await.unwrap() {
        TickOutcome::Reported { period, .. } => assert_eq!(period, "p0"),
        other => panic!("expected Reported(p0), got {other:?}"),
    }
    // The schedule is down for four period boundaries: p1..p3 were crossed.
    clock.advance(4 * 60_000);
    for expected in ["p1", "p2", "p3", "p4"] {
        match h.runner.tick().await.unwrap() {
            TickOutcome::Reported { period, .. } => assert_eq!(period, expected),
            other => panic!("expected Reported({expected}), got {other:?}"),
        }
    }
    assert_eq!(h.runner.tick().await.unwrap(), TickOutcome::Idle);
    let posts = mock.requests("POST", VENDOR_PATH);
    assert_eq!(posts.len(), 5, "one page per period, oldest first");
    for (index, post) in posts.iter().enumerate() {
        assert!(
            post.body.contains(&format!(r#""period":"p{index}""#)),
            "{}",
            post.body
        );
        assert!(
            post.header("idempotency-key")
                .unwrap_or_default()
                .ends_with(&format!(":p{index}:head")),
            "period p{index} must keep its own record-before-send key"
        );
    }
    for index in 0..=4i64 {
        let row = h
            .store
            .report_period(&h.organization, &format!("p{index}"))
            .unwrap()
            .unwrap();
        assert_eq!(row.status, ReportPeriodStatus::Reported);
        assert!(row.skipped_at_ms.is_none());
    }
}

/// A downtime gap larger than the configured cap: the most recent missed
/// periods are backfilled and every older one is marked skipped-permanently
/// with a durable audit row — never silently dropped.
#[tokio::test]
async fn catch_up_cap_marks_older_periods_skipped_with_a_durable_audit_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("billing.db");
    let mock = MockServer::start().await;
    for _ in 0..8 {
        mock.push("POST", VENDOR_PATH, Reply::json(200, serde_json::json!({})));
    }
    let clock = Arc::new(ManualClock::new(1_000));
    let mut p = policy();
    p.max_catch_up = 2;
    let h = harness(&db, &mock, clock.clone(), p);
    match h.runner.tick().await.unwrap() {
        TickOutcome::Reported { period, .. } => assert_eq!(period, "p0"),
        other => panic!("expected Reported(p0), got {other:?}"),
    }
    // Three missed periods (p1..p3), cap 2: p1 is skipped, p2/p3 backfilled.
    clock.advance(4 * 60_000);
    for expected in ["p2", "p3", "p4"] {
        match h.runner.tick().await.unwrap() {
            TickOutcome::Reported { period, .. } => assert_eq!(period, expected),
            other => panic!("expected Reported({expected}), got {other:?}"),
        }
    }
    assert_eq!(h.runner.tick().await.unwrap(), TickOutcome::Idle);
    // The skipped period was NEVER sent...
    let posts = mock.requests("POST", VENDOR_PATH);
    assert_eq!(posts.len(), 4);
    assert!(posts
        .iter()
        .all(|post| !post.body.contains(r#""period":"p1""#)));
    // ...and it is durably audited, naming the range and the cap.
    let audit = h
        .store
        .report_period(&h.organization, "p1")
        .unwrap()
        .expect("the skipped period carries an audit row");
    assert_eq!(audit.status, ReportPeriodStatus::Skipped);
    assert_eq!(audit.skipped_at_ms, Some(clock.now_ms()));
    let message = audit.last_error.as_deref().unwrap();
    assert!(
        message.contains("skipped 1 missed periods p1..p1"),
        "{message}"
    );
    assert!(message.contains("cap"), "{message}");
    // The backfilled periods are terminal too (exactly once).
    for period in ["p2", "p3", "p4"] {
        assert_eq!(
            h.store
                .report_period(&h.organization, period)
                .unwrap()
                .unwrap()
                .status,
            ReportPeriodStatus::Reported
        );
    }
}

/// A restart in the MIDDLE of a backfill replays nothing: the already
/// reported missed period is terminal, the remaining ones continue oldest
/// first, and no period is double-reported.
#[tokio::test]
async fn restart_mid_backfill_never_double_reports() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("billing.db");
    let mock = MockServer::start().await;
    for _ in 0..8 {
        mock.push("POST", VENDOR_PATH, Reply::json(200, serde_json::json!({})));
    }
    let clock = Arc::new(ManualClock::new(1_000));
    {
        let h = harness(&db, &mock, clock.clone(), policy());
        assert!(matches!(
            h.runner.tick().await.unwrap(),
            TickOutcome::Reported { .. }
        ));
        clock.advance(4 * 60_000);
        // The first catch-up tick plans p1..p3 and reports the oldest.
        match h.runner.tick().await.unwrap() {
            TickOutcome::Reported { period, .. } => assert_eq!(period, "p1"),
            other => panic!("expected Reported(p1), got {other:?}"),
        }
        drop(h);
    }
    // Crash + restart mid-backfill: p1 is already terminal; p2, p3 and the
    // current period continue exactly once each.
    let h = harness(&db, &mock, clock, policy());
    for expected in ["p2", "p3", "p4"] {
        match h.runner.tick().await.unwrap() {
            TickOutcome::Reported { period, .. } => assert_eq!(period, expected),
            other => panic!("expected Reported({expected}), got {other:?}"),
        }
    }
    assert_eq!(h.runner.tick().await.unwrap(), TickOutcome::Idle);
    let posts = mock.requests("POST", VENDOR_PATH);
    assert_eq!(posts.len(), 5, "each of p0..p4 exactly once");
    let mut keys: Vec<String> = posts
        .iter()
        .map(|post| {
            post.header("idempotency-key")
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 5, "no idempotency key was replayed");
    assert!(
        !h.store
            .report_periods(&h.organization, 10)
            .unwrap()
            .iter()
            .any(|row| row.status == ReportPeriodStatus::Skipped),
        "a restart never re-audits or skips a period that was backfilled"
    );
}
