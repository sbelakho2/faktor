//! Adversarial tests of the generic REST billing-vendor adapter over a
//! scripted mock vendor: idempotency keys per period/cursor, cursor paging
//! with its bound, the 429/5xx/4xx retry matrix, credential resolution from
//! the configured environment variable, and the report-only invariant (the
//! local ledger is byte-identical after reporting).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use faktor_cloud::{
    BillingAccountId, BillingConfig, BillingVendorAdapter, BillingVendorConfig, ControlPlaneError,
    DurableSpendRow, EntitlementService, MemoryBillingStore, OrganizationId, SpendCategory,
    VENDOR_HTTP_TIMEOUT_MS,
};
use faktor_provider::egress::{CheckedResponse, EgressError, HttpTransport};
use reqwest::Request;

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

struct Recorded {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

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
        Box<dyn std::future::Future<Output = Result<CheckedResponse, EgressError>> + Send + '_>,
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
            Ok(CheckedResponse::from_response(reqwest::Response::from(
                response,
            )))
        })
    }
}

fn service() -> (Arc<EntitlementService>, OrganizationId, BillingAccountId) {
    let config = BillingConfig {
        default_plan: None,
        plans: Default::default(),
        managed_providers: ["managed-provider".to_string()].into_iter().collect(),
    };
    let store = Arc::new(MemoryBillingStore::new());
    let service = EntitlementService::with_system_clock(store, config).expect("service");
    let organization = OrganizationId::try_new("org_vendor").unwrap();
    let account = BillingAccountId::try_new("acct_vendor").unwrap();
    service
        .ensure_account(&organization, &account, "vendor-test", true)
        .unwrap();
    (service, organization, account)
}

fn seed_usage(
    service: &EntitlementService,
    organization: &OrganizationId,
    account: &BillingAccountId,
) {
    let row =
        |reservation: i64, provider: &str, input: u64, output: u64, cost: u64| DurableSpendRow {
            organization_id: organization.as_str().to_string(),
            session_id: 9,
            task_id: 11,
            reservation_id: reservation,
            attempt_id: format!("attempt-{reservation}"),
            provider: provider.to_string(),
            model: "m".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            provider_cost_micro: cost,
            provider_reported_micro: cost,
            state: "settled".into(),
            occurred_at_ms: 1_789_600_000_000,
            source_operation: "provider_call".into(),
        };
    service
        .ingest_spend_row(
            organization,
            account,
            &row(1, "managed-provider", 100, 20, 500),
            SpendCategory::Managed,
        )
        .unwrap();
    service
        .ingest_spend_row(
            organization,
            account,
            &row(2, "byok-provider", 50, 5, 100),
            SpendCategory::Byok,
        )
        .unwrap();
}

fn vendor_adapter(transport: &Arc<ScriptedTransport>, max_attempts: u32) -> BillingVendorAdapter {
    let (service, _organization, _account) = service();
    BillingVendorAdapter::new(
        service,
        BillingVendorConfig {
            base_url: "https://vendor.test".into(),
            report_path: "/v1/usage-reports".into(),
            auth_env: "FAKTOR_TEST_VENDOR_TOKEN".into(),
            user_agent: "faktor-test/0.1".into(),
            max_attempts,
            retry_base_ms: 0,
        },
        transport.clone(),
        Some("vendor-secret-token".into()),
    )
    .expect("adapter")
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn report_is_idempotent_per_period_and_cursor_and_never_writes_the_ledger() {
    let (service, organization, account) = service();
    seed_usage(&service, &organization, &account);
    let before = service.fold(&organization).unwrap();
    let before_usage = service
        .usage_page(&organization, None, 50)
        .unwrap()
        .items
        .len();

    let transport = ScriptedTransport::new(vec![
        Reply::json(200, serde_json::json!({"next_cursor": null})),
        Reply::json(
            200,
            serde_json::json!({"next_cursor": null, "idempotent_replay": true}),
        ),
    ]);
    let adapter = BillingVendorAdapter::new(
        service.clone(),
        BillingVendorConfig {
            base_url: "https://vendor.test".into(),
            report_path: "/v1/usage-reports".into(),
            auth_env: "FAKTOR_TEST_VENDOR_TOKEN".into(),
            user_agent: "faktor-test/0.1".into(),
            max_attempts: 3,
            retry_base_ms: 0,
        },
        transport.clone(),
        Some("vendor-secret-token".into()),
    )
    .unwrap();

    let first = adapter
        .report_period(&organization, "2026-09", None)
        .await
        .unwrap();
    let second = adapter
        .report_period(&organization, "2026-09", None)
        .await
        .unwrap();
    assert_eq!(first.totals, before.totals);
    assert_eq!(first.next_cursor, None);
    assert!(!first.idempotent_replay);
    assert!(
        second.idempotent_replay,
        "the mock vendor de-duplicated the key"
    );

    // Both reports carried the SAME idempotency key and body: a replay is
    // byte-identical, so the vendor can de-duplicate it.
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    let key = |i: usize| {
        header(&requests[i].2, "idempotency-key")
            .unwrap()
            .to_string()
    };
    assert_eq!(key(0), key(1));
    assert_eq!(
        key(0),
        BillingVendorAdapter::idempotency_key(&organization, "2026-09", None)
    );
    assert_eq!(requests[0].0, "POST");
    assert_eq!(requests[0].1, "https://vendor.test/v1/usage-reports");
    assert_eq!(
        header(&requests[0].2, "authorization"),
        Some("Bearer vendor-secret-token")
    );
    assert_eq!(
        requests[0].3, requests[1].3,
        "the report body is deterministic"
    );
    let body: serde_json::Value = serde_json::from_slice(&requests[0].3).unwrap();
    assert_eq!(body["organization"], "org_vendor");
    assert_eq!(body["period"], "2026-09");
    assert_eq!(body["aggregate"]["input_tokens"], 150);
    assert_eq!(body["aggregate"]["output_tokens"], 25);
    assert_eq!(body["aggregate"]["managed_cost_micro"], 500);
    assert_eq!(body["aggregate"]["byok_cost_micro"], 100);

    // REPORT-ONLY: the local ledger is byte-identical (no new usage/credit
    // rows, same fold).
    assert_eq!(
        service
            .usage_page(&organization, None, 50)
            .unwrap()
            .items
            .len(),
        before_usage
    );
    assert_eq!(service.fold(&organization).unwrap(), before);
}

#[tokio::test]
async fn cursor_pages_each_get_their_own_key_and_the_page_bound_is_enforced() {
    let transport = ScriptedTransport::new(vec![
        Reply::json(200, serde_json::json!({"next_cursor": "cursor-1"})),
        Reply::json(200, serde_json::json!({"next_cursor": null})),
    ]);
    let adapter = vendor_adapter(&transport, 3);
    let (service, organization, _account) = service();
    seed_usage(&service, &organization, &_account);
    let (pages, next) = adapter
        .report_period_to_completion(&organization, "2026-09")
        .await
        .unwrap();
    assert_eq!(next, None);
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].next_cursor.as_deref(), Some("cursor-1"));
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    let key0 = header(&requests[0].2, "idempotency-key").unwrap();
    let key1 = header(&requests[1].2, "idempotency-key").unwrap();
    assert_ne!(key0, key1, "every page has its own report key");
    assert_eq!(
        key1,
        BillingVendorAdapter::idempotency_key(&organization, "2026-09", Some("cursor-1"))
    );

    // A vendor that never finishes paging is refused at the hard page bound.
    let endless: Vec<Reply> = (0..faktor_cloud::MAX_REPORT_PAGES)
        .map(|_| Reply::json(200, serde_json::json!({"next_cursor": "again"})))
        .collect();
    let transport = ScriptedTransport::new(endless);
    let adapter = vendor_adapter(&transport, 1);
    let err = adapter
        .report_period_to_completion(&organization, "2026-09")
        .await
        .unwrap_err();
    assert!(matches!(err, ControlPlaneError::Conflict(_)), "{err:?}");
    assert_eq!(transport.request_count(), faktor_cloud::MAX_REPORT_PAGES);
}

#[tokio::test]
async fn retry_matrix_is_429_and_5xx_retried_and_every_other_4xx_final() {
    // 429 with retry-after 0 then success: retried.
    let transport = ScriptedTransport::new(vec![
        Reply::json(429, serde_json::json!({"error": "slow down"})).with_header("retry-after", "0"),
        Reply::json(200, serde_json::json!({"next_cursor": null})),
    ]);
    let adapter = vendor_adapter(&transport, 3);
    let (service, organization, account) = service();
    seed_usage(&service, &organization, &account);
    let outcome = adapter
        .report_period(&organization, "2026-09", None)
        .await
        .unwrap();
    assert_eq!(outcome.attempts, 2);
    assert_eq!(transport.request_count(), 2);

    // A persistent 5xx exhausts the bound and surfaces typed.
    let transport = ScriptedTransport::new(vec![
        Reply::json(500, serde_json::json!({"error": "boom"})),
        Reply::json(500, serde_json::json!({"error": "boom"})),
    ]);
    let adapter = vendor_adapter(&transport, 2);
    let err = adapter
        .report_period(&organization, "2026-09", None)
        .await
        .unwrap_err();
    assert!(matches!(err, ControlPlaneError::Backend(_)), "{err:?}");
    assert_eq!(transport.request_count(), 2);

    // 422 and 403 are final: exactly one call each, no retry.
    for (status, expected) in [(422u16, "malformed"), (403, "forbidden")] {
        let transport = ScriptedTransport::new(vec![Reply::json(
            status,
            serde_json::json!({"error": "refused"}),
        )]);
        let adapter = vendor_adapter(&transport, 3);
        let err = adapter
            .report_period(&organization, "2026-09", None)
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            if expected == "malformed" {
                "malformed"
            } else {
                "permission_denied"
            }
        );
        assert_eq!(
            transport.request_count(),
            1,
            "status {status} must not retry"
        );
    }
}

#[tokio::test]
async fn credentials_come_from_the_configured_env_and_missing_is_typed() {
    const VAR: &str = "FAKTOR_TEST_VENDOR_TOKEN_XYZ_UNSET";
    std::env::remove_var(VAR);
    let transport = ScriptedTransport::new(vec![]);
    let (service, _organization, _account) = service();
    let config = BillingVendorConfig {
        base_url: "https://vendor.test".into(),
        report_path: "/v1/usage-reports".into(),
        auth_env: VAR.into(),
        ..Default::default()
    };
    assert!(matches!(
        BillingVendorAdapter::from_env(service.clone(), config.clone(), transport.clone()),
        Err(ControlPlaneError::Config(_))
    ));
    std::env::set_var(VAR, "resolved-token");
    let resolved = BillingVendorAdapter::from_env(service, config, transport).unwrap();
    let rendered = format!("{resolved:?}");
    assert!(
        !rendered.contains("resolved-token"),
        "credentials never render"
    );
    std::env::remove_var(VAR);
}

#[test]
fn strict_config_validation_refuses_illegal_shapes() {
    for bad in [
        BillingVendorConfig {
            base_url: "ftp://vendor".into(),
            ..Default::default()
        },
        BillingVendorConfig {
            base_url: "https://vendor.test".into(),
            report_path: "reports".into(),
            ..Default::default()
        },
        BillingVendorConfig {
            base_url: "https://vendor.test".into(),
            auth_env: String::new(),
            ..Default::default()
        },
        BillingVendorConfig {
            base_url: "https://vendor.test".into(),
            max_attempts: 0,
            ..Default::default()
        },
        BillingVendorConfig {
            base_url: "https://vendor.test".into(),
            retry_base_ms: 600_000,
            ..Default::default()
        },
    ] {
        assert!(bad.validate().is_err(), "{bad:?} must be refused");
    }
}

/// A transport that never resolves: a vendor that accepts and stalls.
struct StallingTransport;

impl HttpTransport for StallingTransport {
    fn execute(
        &self,
        _req: Request,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CheckedResponse, EgressError>> + Send + '_>,
    > {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_vendor_is_bounded_by_the_attempt_and_network_bounds() {
    let (service, organization, _account) = service();
    let adapter = BillingVendorAdapter::new(
        service,
        BillingVendorConfig {
            base_url: "https://vendor.test".into(),
            report_path: "/v1/usage-reports".into(),
            auth_env: "FAKTOR_TEST_VENDOR_TOKEN".into(),
            user_agent: "faktor-test/0.1".into(),
            max_attempts: 3,
            retry_base_ms: 250,
        },
        Arc::new(StallingTransport),
        Some("vendor-secret-token".into()),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let call =
        tokio::spawn(async move { adapter.report_period(&organization, "2026-09", None).await });
    // Advance virtual time past every attempt bound + backoff, yielding so
    // the retry loop can schedule each next timer.
    for _ in 0..12 {
        tokio::task::yield_now().await;
        if call.is_finished() {
            break;
        }
        tokio::time::advance(std::time::Duration::from_millis(
            VENDOR_HTTP_TIMEOUT_MS + 1_000,
        ))
        .await;
    }
    let err = call.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, ControlPlaneError::Backend(m) if m.contains("network bound")),
        "{err:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "virtual time only: no real-time hang"
    );
}
