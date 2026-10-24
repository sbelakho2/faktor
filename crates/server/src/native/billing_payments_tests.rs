//! Adversarial HTTP tests for the additive payment/invoice plane: real
//! overview states, strict DTOs, tenant isolation, idempotency, signature-
//! first webhook ingress and disabled parity.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::api::tests::test_deps;
use crate::{serve, ServerHandle};
use faktor_cloud::{
    BillingConfig, EntitlementService, ManualClock, MemoryBillingStore, OrganizationId, PlanConfig,
};
use faktor_payments::{
    CurrencySpec, DunningConfig, PaymentService, PaymentsConfig, PlanPrice, PriceCatalogue,
    ProviderKind, SqlitePaymentStore, StaticSecrets, TaxConfig, TaxRate,
};

const NOW_MS: i64 = 1_700_000_000_000;
const WEBHOOK_SECRET: &str = "whsec_route_test";

fn billing_config() -> BillingConfig {
    let mut config = BillingConfig::default();
    config.plans.insert(
        "pro".into(),
        PlanConfig {
            period_ms: None,
            plan_id: "pro".into(),
            features: Default::default(),
            limits: Default::default(),
        },
    );
    config.default_plan = Some("pro".into());
    config
}

fn billing_service() -> Arc<EntitlementService> {
    EntitlementService::new(
        Arc::new(MemoryBillingStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
        billing_config(),
    )
    .unwrap()
}

fn payments_config() -> PaymentsConfig {
    let mut plans = BTreeMap::new();
    plans.insert(
        "pro".into(),
        PlanPrice {
            plan_id: "pro".into(),
            currency: "USD".into(),
            monthly_minor: 10_000,
            annual_minor: 100_000,
            per_seat_monthly_minor: 2_500,
            per_seat_annual_minor: 25_000,
        },
    );
    let mut currencies = BTreeMap::new();
    currencies.insert("USD".into(), CurrencySpec { exponent: 2 });
    let mut rates = BTreeMap::new();
    rates.insert(
        "US-CA".into(),
        TaxRate {
            rate_bps: 825,
            inclusive: false,
        },
    );
    PaymentsConfig {
        provider: ProviderKind::Local,
        catalogue: PriceCatalogue {
            plans,
            currencies,
            fx: BTreeMap::new(),
        },
        tax: TaxConfig {
            default_jurisdiction: Some("US-CA".into()),
            rates,
        },
        dunning: DunningConfig {
            max_attempts: 2,
            grace_ms: 10_000,
        },
        webhook_secret_env: "FAKTOR_ROUTE_WEBHOOK_SECRET".into(),
        vault_key_env: faktor_payments::catalog::default_vault_key_env(),
        stripe: Default::default(),
        mollie: Default::default(),
    }
}

fn payments_service(clock: Arc<ManualClock>) -> Arc<PaymentService> {
    let store = Arc::new(SqlitePaymentStore::open_in_memory().unwrap());
    let secrets = Arc::new(StaticSecrets::new([(
        "FAKTOR_ROUTE_WEBHOOK_SECRET".to_string(),
        WEBHOOK_SECRET.to_string(),
    )]));
    Arc::new(PaymentService::open_local(store, payments_config(), clock, secrets).unwrap())
}

fn bootstrap(
    control_plane: &faktor_cloud::ControlPlane,
    name: &str,
    email: &str,
) -> (String, String) {
    let result = control_plane
        .bootstrap_organization(name, email, "Owner", &format!("boot-{email}"))
        .unwrap();
    let org = result.organization.id.as_str().to_string();
    let token = result
        .token
        .map(|t| t.expose().to_string())
        .expect("first issuance presents the token");
    (org, token)
}

async fn payments_deps(
    root: &std::path::Path,
) -> (ServerHandle, Arc<faktor_cloud::ControlPlane>, String) {
    let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
        Arc::new(faktor_cloud::MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let payments = payments_service(Arc::new(ManualClock::new(NOW_MS)));
    let deps = test_deps(root)
        .with_control_plane(control_plane.clone())
        .with_billing(billing_service())
        .with_payments(payments);
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    (handle, control_plane, token.as_str().to_string())
}

fn subscribe_body() -> serde_json::Value {
    serde_json::json!({
        "plan_id": "pro",
        "interval": "monthly",
        "seats": 2,
        "customer_email": "owner@example.com",
        "customer_name": "Owner",
    })
}

async fn subscribe(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    control: &str,
) -> (String, serde_json::Value) {
    let response = client
        .post(format!("{base}/native/billing/subscribe"))
        .bearer_auth(token)
        .header("x-faktor-control-token", control)
        .header("Idempotency-Key", "route-subscribe")
        .json(&subscribe_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    (
        body["subscription"]["id"].as_str().unwrap().to_string(),
        body,
    )
}

#[tokio::test]
async fn payments_routes_are_real_and_overview_states_are_wired() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, token) = payments_deps(dir.path()).await;
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();
    let (org, owner) = bootstrap(&control_plane, "A", "a@a.test");
    let (subscription_id, _) = subscribe(&client, &base, &token, &owner).await;

    let response = client
        .post(format!("{base}/native/billing/payment-methods"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .header("Idempotency-Key", "route-method")
        .json(&serde_json::json!({
            "token": "tok_local_visa",
            "brand": "visa",
            "last4": "4242",
            "exp_month": 12,
            "exp_year": 2030,
            "set_default": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let method: serde_json::Value = response.json().await.unwrap();
    let method_id = method["paymentMethod"]["id"].as_str().unwrap().to_string();
    assert!(!method.to_string().contains("tok_local_visa"));

    let response = client
        .post(format!("{base}/native/billing/invoices"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .header("Idempotency-Key", "route-invoice")
        .json(&serde_json::json!({
            "subscription_id": subscription_id,
            "usage": [{"description": "extra", "quantity": 3, "unit_amount_minor": 500}],
            "credits_minor": 200,
            "discount_minor": 0,
            "tax_jurisdiction": "US-CA",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let invoice: serde_json::Value = response.json().await.unwrap();
    assert_eq!(invoice["invoice"]["number"], "INV-000001");
    let invoice_id = invoice["invoice"]["id"].as_str().unwrap().to_string();
    assert_eq!(invoice["invoice"]["total_minor"], 17_645);

    let response = client
        .get(format!("{base}/native/billing/invoices?org={org}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let list: serde_json::Value = response.json().await.unwrap();
    assert_eq!(list["items"].as_array().unwrap().len(), 1);

    let response = client
        .get(format!("{base}/native/billing/invoices/{invoice_id}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let detail: serde_json::Value = response.json().await.unwrap();
    assert_eq!(detail["invoice"]["reconciliation"], "matched");
    assert_eq!(detail["providerObservations"].as_array().unwrap().len(), 1);
    assert_eq!(
        detail["providerObservations"][0]["invoice_id"],
        invoice_id.as_str()
    );

    let response = client
        .get(format!("{base}/native/billing/invoices?format=csv"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/csv; charset=utf-8"
    );
    assert!(response.text().await.unwrap().contains("INV-000001"));

    let response = client
        .post(format!("{base}/native/billing/invoices/{invoice_id}/pay"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .header("Idempotency-Key", "route-pay")
        .json(&serde_json::json!({"payment_method_id": method_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let paid: serde_json::Value = response.json().await.unwrap();
    assert_eq!(paid["invoice"]["status"], "paid");
    assert_eq!(paid["payment"]["amount_minor"], 17_645);

    let response = client
        .get(format!("{base}/native/billing/payments"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let payments: serde_json::Value = response.json().await.unwrap();
    assert_eq!(payments["payments"].as_array().unwrap().len(), 1);

    let response = client
        .get(format!("{base}/native/billing/tax"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let tax: serde_json::Value = response.json().await.unwrap();
    assert_eq!(tax["tax"]["state"], "available");
    assert_eq!(tax["prices"]["pro"]["formattedMonthly"], "100.00 USD");

    let response = client
        .get(format!("{base}/native/billing/overview"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let overview: serde_json::Value = response.json().await.unwrap();
    assert_eq!(overview["billing"]["payment"]["state"], "available");
    assert_eq!(overview["billing"]["invoices"]["state"], "available");
    assert_eq!(overview["billing"]["seats"]["state"], "available");
    assert_eq!(overview["billing"]["tax"]["state"], "available");
    assert_eq!(overview["billing"]["currency"]["state"], "available");
    assert_eq!(overview["billing"]["payments"]["currency"], "USD");
    assert_eq!(
        overview["billing"]["payments"]["provider"]["provider"],
        "local"
    );
    assert_eq!(overview["billing"]["payments"]["paid_minor"], 17_645);

    // Manual provider reconciliation is a read-only fetch pass over Faktor
    // truth: it records the observation and never rewrites the total.
    let response = client
        .post(format!("{base}/native/billing/reconcile?org={org}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let report: serde_json::Value = response.json().await.unwrap();
    assert_eq!(report["reconciliation"]["checked"], 1);
    assert_eq!(report["reconciliation"]["matched"], 1);
    assert_eq!(report["reconciliation"]["conflicts"], 0);
    let response = client
        .get(format!("{base}/native/billing/invoices/{invoice_id}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    let detail: serde_json::Value = response.json().await.unwrap();
    assert_eq!(detail["invoice"]["total_minor"], 17_645);
    assert_eq!(detail["invoice"]["status"], "paid");
    assert_eq!(detail["invoice"]["reconciliation"], "matched");
}

#[tokio::test]
async fn payment_method_routes_reject_raw_pan_and_never_return_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, token) = payments_deps(dir.path()).await;
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();
    let (_, owner) = bootstrap(&control_plane, "PAN", "pan@a.test");
    let _ = subscribe(&client, &base, &token, &owner).await;

    for (index, pan) in [
        "4242424242424242",
        "pm_4242424242424242",
        "tok_4111111111111111",
        "tok_4242 4242 4242 4242",
    ]
    .into_iter()
    .enumerate()
    {
        let response = client
            .post(format!("{base}/native/billing/payment-methods"))
            .bearer_auth(&token)
            .header("x-faktor-control-token", &owner)
            .header("Idempotency-Key", format!("route-pan-{index}"))
            .json(&serde_json::json!({
                "token": pan,
                "brand": "visa",
                "last4": "4242",
                "exp_month": 12,
                "exp_year": 2030,
                "set_default": true,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "raw PAN {pan:?} must be refused");
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "malformed");
        assert!(!body.to_string().contains(pan));
    }

    let response = client
        .post(format!("{base}/native/billing/payment-methods"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .header("Idempotency-Key", "route-valid-method")
        .json(&serde_json::json!({
            "token": "pm_route_valid_token",
            "brand": "visa",
            "last4": "4242",
            "exp_month": 12,
            "exp_year": 2030,
            "set_default": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let method: serde_json::Value = response.json().await.unwrap();
    let id = method["paymentMethod"]["id"].as_str().unwrap().to_string();
    assert!(!method.to_string().contains("pm_route_valid_token"));

    let response = client
        .post(format!("{base}/native/billing/payment-methods/{id}/rotate"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .header("Idempotency-Key", "route-rotate-pan")
        .json(&serde_json::json!({"token": "tok_4111111111111111"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "malformed");

    let response = client
        .post(format!("{base}/native/billing/payment-methods/{id}/rotate"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .header("Idempotency-Key", "route-rotate-valid")
        .json(&serde_json::json!({"token": "pm_route_rotated_token"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let response = client
        .get(format!("{base}/native/billing/payment-methods"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let listed: serde_json::Value = response.json().await.unwrap();
    let text = listed.to_string();
    assert!(!text.contains("pm_route_valid_token"));
    assert!(!text.contains("pm_route_rotated_token"));
    assert!(!text.contains("token_ciphertext"));

    let response = client
        .request(
            reqwest::Method::DELETE,
            format!("{base}/native/billing/payment-methods/{id}"),
        )
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .header("Idempotency-Key", "route-remove")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn payments_routes_authorize_before_disclosure_and_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, token) = payments_deps(dir.path()).await;
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();
    let (org_a, owner_a) = bootstrap(&control_plane, "A", "a@a.test");
    let (_org_b, owner_b) = bootstrap(&control_plane, "B", "b@b.test");
    let (subscription_id, _) = subscribe(&client, &base, &token, &owner_a).await;
    let response = client
        .post(format!("{base}/native/billing/invoices"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_a)
        .header("Idempotency-Key", "route-invoice-a")
        .json(&serde_json::json!({"subscription_id": subscription_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let invoice: serde_json::Value = response.json().await.unwrap();
    let invoice_id = invoice["invoice"]["id"].as_str().unwrap();

    // No daemon password: 401 before the route reveals anything.
    let response = client
        .get(format!("{base}/native/billing/invoices"))
        .header("x-faktor-control-token", &owner_a)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // Password but no control principal: 401.
    let response = client
        .get(format!("{base}/native/billing/invoices"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // A foreign organization id is the byte-identical 404.
    let response = client
        .get(format!("{base}/native/billing/invoices?org={org_a}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_b)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("organization not found"));

    // A foreign invoice id is 404 for the other tenant.
    let response = client
        .get(format!("{base}/native/billing/invoices/{invoice_id}"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_b)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    // Strict DTO: an unknown field is refused before any effect.
    let response = client
        .post(format!("{base}/native/billing/subscribe"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_a)
        .header("Idempotency-Key", "route-bad")
        .json(&serde_json::json!({
            "plan_id": "pro",
            "interval": "monthly",
            "seats": 1,
            "customer_email": "a@a.test",
            "customer_name": "A",
            "price": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);

    // The mutation requires its request-bound idempotency key.
    let response = client
        .post(format!("{base}/native/billing/seats"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner_a)
        .json(&serde_json::json!({"subscription_id": subscription_id, "quantity": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.text().await.unwrap().contains("idempotency"));
}

#[tokio::test]
async fn webhook_route_is_signature_first_and_disabled_parity_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, control_plane, token) = payments_deps(dir.path()).await;
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();
    let (org, owner) = bootstrap(&control_plane, "A", "a@a.test");
    let (subscription_id, _) = subscribe(&client, &base, &token, &owner).await;
    let response = client
        .post(format!("{base}/native/billing/invoices"))
        .bearer_auth(&token)
        .header("x-faktor-control-token", &owner)
        .header("Idempotency-Key", "webhook-invoice")
        .json(&serde_json::json!({"subscription_id": subscription_id}))
        .send()
        .await
        .unwrap();
    let invoice: serde_json::Value = response.json().await.unwrap();
    let provider_invoice = invoice["invoice"]["provider_invoice_id"]
        .as_str()
        .unwrap()
        .to_string();
    let amount = invoice["invoice"]["total_minor"].as_i64().unwrap();
    let now_secs = NOW_MS / 1000;
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_route_paid",
        "type": "invoice.paid",
        "created": now_secs,
        "data": {"object": {
            "id": provider_invoice,
            "amount_paid": amount,
            "metadata": {"organization_id": org}
        }}
    }))
    .unwrap();

    // Forged/garbage signature: 401 with no state touched.
    let response = client
        .post(format!("{base}/native/billing/webhook"))
        .header(
            "stripe-signature",
            format!("t={now_secs},v1={}", "0".repeat(64)),
        )
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("payment_webhook_unauthorized"));

    // A valid signature (no daemon password) is accepted and idempotent.
    let header = faktor_payments::sign_local_webhook(WEBHOOK_SECRET, now_secs, &body);
    let response = client
        .post(format!("{base}/native/billing/webhook"))
        .header("stripe-signature", header.clone())
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let applied: serde_json::Value = response.json().await.unwrap();
    assert_eq!(applied["status"], "applied");
    let response = client
        .post(format!("{base}/native/billing/webhook"))
        .header("stripe-signature", header)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["status"],
        "duplicate"
    );

    // Disabled parity: a daemon with no payment service answers typed 409.
    let disabled_dir = tempfile::tempdir().unwrap();
    let disabled_deps = test_deps(disabled_dir.path())
        .with_control_plane(control_plane)
        .with_billing(billing_service());
    let disabled_token = disabled_deps.auth_token.clone();
    let disabled_handle = serve(disabled_deps, 0).await.unwrap();
    let disabled_base = format!("http://{}", disabled_handle.addr);
    let response = client
        .get(format!("{disabled_base}/native/billing/invoices"))
        .bearer_auth(disabled_token.as_str())
        .header("x-faktor-control-token", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert!(response.text().await.unwrap().contains("payments_disabled"));
}

#[test]
fn route_config_is_strict_and_currency_is_exact() {
    let org = OrganizationId::try_new("org-x").unwrap();
    assert_eq!(org.as_str(), "org-x");
    let service = PaymentService::open_local(
        Arc::new(SqlitePaymentStore::open_in_memory().unwrap()),
        payments_config(),
        Arc::new(ManualClock::new(NOW_MS)),
        Arc::new(StaticSecrets::default()),
    )
    .unwrap();
    assert_eq!(service.format_money(12_345, "USD"), "123.45 USD");
    assert_eq!(service.provider_status().provider, "local");
}
