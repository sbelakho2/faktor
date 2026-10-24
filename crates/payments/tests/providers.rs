//! Adversarial adapter tests against local mocked HTTP servers and JSON
//! fixtures. No test reaches a real provider; Stripe test-mode credentials
//! are optional evidence, never a gate.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::adyen::{sign_adyen_webhook_hmac, AdyenPaymentProvider};
use faktor_payments::provider::{
    PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest, ProviderChargeRequest,
    ProviderCheckoutRequest, ProviderCustomerRequest, ProviderRefundRequest,
    ProviderSubscribeRequest,
};
use faktor_payments::stripe::StripePaymentProvider;
use faktor_payments::{
    AdyenConfig, BillingInterval, CurrencySpec, DunningConfig, PayInvoiceRequest, PaymentError,
    PaymentService, PaymentsConfig, PlanPrice, PriceCatalogue, ProviderKind, SecretString,
    SqlitePaymentStore, StaticSecrets, StripeConfig, SubscribeRequest, TaxConfig, WebhookHeaders,
};
use faktor_provider::egress::{HttpTransport, PolicyCheckedHttpTransport};
use faktor_provider::testing::{MockAction, MockServer};
use serde_json::Value;
use tempfile::TempDir;

const START: i64 = 1_700_000_000_000;
const ADYEN_KEY: &str = "AQE_adyen_test_api_key";
const ADYEN_HMAC: &str = "abababababababababababababababababababababababababababababababab";
const STRIPE_KEY: &str = "sk_test_fixture_key";

fn transport() -> Arc<dyn HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::permissive())
}

fn adyen_config(base_url: String) -> AdyenConfig {
    AdyenConfig {
        base_url,
        api_key_env: "FAKTOR_ADYEN_API_KEY".into(),
        webhook_hmac_key_env: "FAKTOR_ADYEN_HMAC_KEY".into(),
        merchant_account: "FaktorTestMerchant".into(),
        user_agent: "faktor-payments-test/0.1".into(),
    }
}

fn stripe_config(base_url: String) -> StripeConfig {
    StripeConfig {
        base_url,
        api_key_env: "FAKTOR_STRIPE_SECRET_KEY".into(),
        webhook_secret_env: "FAKTOR_STRIPE_WEBHOOK_SECRET".into(),
        user_agent: "faktor-payments-test/0.1".into(),
    }
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
    plans.insert(
        "team".into(),
        PlanPrice {
            plan_id: "team".into(),
            currency: "USD".into(),
            monthly_minor: 20_000,
            annual_minor: 200_000,
            per_seat_monthly_minor: 3_000,
            per_seat_annual_minor: 30_000,
        },
    );
    PaymentsConfig {
        provider: ProviderKind::Stripe,
        catalogue: PriceCatalogue {
            plans,
            currencies: BTreeMap::from([("USD".into(), CurrencySpec { exponent: 2 })]),
            fx: BTreeMap::new(),
        },
        tax: TaxConfig::default(),
        dunning: DunningConfig::default(),
        webhook_secret_env: "FAKTOR_WEBHOOK_SECRET".into(),
        vault_key_env: faktor_payments::catalog::default_vault_key_env(),
        stripe: StripeConfig::default(),
        adyen: AdyenConfig::default(),
    }
}

fn service(
    dir: &TempDir,
    provider: Arc<dyn PaymentProvider>,
    clock: Arc<ManualClock>,
) -> PaymentService {
    let store = Arc::new(SqlitePaymentStore::open(&dir.path().join("payments.db")).expect("store"));
    PaymentService::with_provider(
        store,
        provider,
        payments_config(),
        clock,
        Arc::new(StaticSecrets::default()),
    )
    .expect("service")
}

#[tokio::test]
async fn adyen_checkout_payment_and_refund_fixture_sequence() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v72/sessions",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/adyen/checkout_session.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v72/payments",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/adyen/payment_authorised.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v72/refunds",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/adyen/refund_received.json").into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/adyen/refund_refunded.json").into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/adyen/refund_refused.json").into(),
                },
            ],
        },
    );
    let base = mock.base_url().await;
    let provider = AdyenPaymentProvider::new(
        &adyen_config(format!("{base}/v72")),
        SecretString::new(ADYEN_KEY),
        Some(SecretString::new(ADYEN_HMAC)),
        transport(),
    )
    .unwrap();

    let checkout = provider
        .create_checkout(ProviderCheckoutRequest {
            organization_id: "org_1".into(),
            idempotency_key: "checkout-1".into(),
            customer_id: "shopper_1".into(),
            description: "Pro plan".into(),
            amount_minor: 10_000,
            currency: "USD".into(),
            success_url: "https://example.invalid/ok".into(),
            cancel_url: "https://example.invalid/cancel".into(),
            local_invoice_id: None,
        })
        .await
        .unwrap();
    assert_eq!(checkout.id, "CS_checkout_1");
    assert_eq!(checkout.status, "open");
    assert!(checkout.url.is_some());

    let charge = provider
        .charge(ProviderChargeRequest {
            organization_id: "org_1".into(),
            idempotency_key: "charge-1".into(),
            customer_id: "shopper_1".into(),
            payment_method_token: SecretString::new("pm_adyen_stored_1"),
            amount_minor: 10_000,
            currency: "USD".into(),
            invoice_ref: Some("inv_1".into()),
        })
        .await
        .unwrap();
    assert_eq!(charge.id, "PSP_payment_1");
    assert_eq!(charge.status, "succeeded");

    let pending = provider
        .refund(ProviderRefundRequest {
            organization_id: "org_1".into(),
            idempotency_key: "refund-1".into(),
            provider_payment_id: "PSP_payment_1".into(),
            amount_minor: 2_500,
            currency: "USD".into(),
            reason: "goodwill".into(),
        })
        .await
        .unwrap();
    assert_eq!(pending.status, "pending");
    let succeeded = provider
        .refund(ProviderRefundRequest {
            organization_id: "org_1".into(),
            idempotency_key: "refund-2".into(),
            provider_payment_id: "PSP_payment_1".into(),
            amount_minor: 2_500,
            currency: "USD".into(),
            reason: "goodwill".into(),
        })
        .await
        .unwrap();
    assert_eq!(succeeded.status, "succeeded");
    let failed = provider
        .refund(ProviderRefundRequest {
            organization_id: "org_1".into(),
            idempotency_key: "refund-3".into(),
            provider_payment_id: "PSP_payment_1".into(),
            amount_minor: 2_500,
            currency: "USD".into(),
            reason: "goodwill".into(),
        })
        .await
        .unwrap();
    assert_eq!(failed.status, "failed");

    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    for (method, path, body) in &requests {
        let _ = (method, path);
        assert!(!body.contains("x-api-key"), "key must never enter a body");
    }
    let payment = requests
        .iter()
        .find(|(method, path, _)| method == "POST" && path == "/v72/payments")
        .unwrap();
    assert!(payment.2.contains("storedPaymentMethodId"));
    assert!(payment.2.contains("pm_adyen_stored_1"));
    let headers = mock.last_request_headers();
    assert!(headers
        .iter()
        .any(|(name, value)| name == "idempotency-key" && value == "refund-3"));
    assert!(headers
        .iter()
        .any(|(name, value)| name == "x-api-key" && value == ADYEN_KEY));
}

#[tokio::test]
async fn adyen_processing_is_pending_and_refusal_is_typed() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v72/payments",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/adyen/payment_received.json").into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/adyen/payment_refused.json").into(),
                },
                MockAction::Respond {
                    status: 500,
                    body: r#"{"errorCode":"905","errorType":"validation","message":"boom","status":500}"#.into(),
                },
            ],
        },
    );
    let base = mock.base_url().await;
    let provider = AdyenPaymentProvider::new(
        &adyen_config(format!("{base}/v72")),
        SecretString::new(ADYEN_KEY),
        None,
        transport(),
    )
    .unwrap();
    let request = || ProviderChargeRequest {
        organization_id: "org_1".into(),
        idempotency_key: "charge-pending".into(),
        customer_id: "shopper_1".into(),
        payment_method_token: SecretString::new("pm_stored_1"),
        amount_minor: 10_000,
        currency: "USD".into(),
        invoice_ref: Some("inv_1".into()),
    };
    let pending = provider.charge(request()).await.unwrap();
    assert_eq!(
        pending.status, "pending",
        "Received must never be succeeded"
    );
    match provider.charge(request()).await {
        Err(PaymentError::Declined(message)) => assert!(message.contains("Refused")),
        other => panic!("expected typed refusal, got {other:?}"),
    }
    match provider.charge(request()).await {
        Err(PaymentError::Provider(message)) => assert!(message.contains("905")),
        other => panic!("expected typed provider error, got {other:?}"),
    }
}

#[tokio::test]
async fn adyen_subscription_create_update_cancel_uses_payment_links() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v72/paymentLinks",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/adyen/payment_link_active.json").into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/adyen/payment_link_replacement.json").into(),
                },
            ],
        },
    );
    mock.route(
        "PATCH",
        "/v72/paymentLinks/PL_link_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/adyen/payment_link_expired.json").into(),
        },
    );
    mock.route(
        "PATCH",
        "/v72/paymentLinks/PL_link_2",
        MockAction::Respond {
            status: 200,
            body: r#"{"id":"PL_link_2","status":"expired"}"#.into(),
        },
    );
    let base = mock.base_url().await;
    let provider = AdyenPaymentProvider::new(
        &adyen_config(format!("{base}/v72")),
        SecretString::new(ADYEN_KEY),
        None,
        transport(),
    )
    .unwrap();
    let created = provider
        .subscribe(ProviderSubscribeRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-1".into(),
            customer_id: "shopper_1".into(),
            plan_id: "pro".into(),
            interval: BillingInterval::Monthly,
            quantity: 1,
            trial_end_ms: None,
            currency: "USD".into(),
            unit_amount_minor: 10_000,
        })
        .await
        .unwrap();
    assert_eq!(created.id, "PL_link_1");
    assert_eq!(created.status, "active");

    let changed = provider
        .change_plan(ProviderChangePlanRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-change".into(),
            subscription_id: created.id.clone(),
            provider_item_id: None,
            provider_product_id: None,
            provider_price_id: None,
            plan_changed: true,
            plan_id: "team".into(),
            interval: BillingInterval::Monthly,
            quantity: 1,
            currency: "USD".into(),
            unit_amount_minor: 20_000,
            prorate: true,
        })
        .await
        .unwrap();
    assert_eq!(changed.id, "PL_link_2");

    let canceled = provider
        .cancel(ProviderCancelRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-cancel".into(),
            subscription_id: changed.id,
            at_period_end: false,
        })
        .await
        .unwrap();
    assert_eq!(canceled.status, "canceled");

    let paths: Vec<String> = mock
        .requests()
        .into_iter()
        .map(|(method, path, _)| format!("{method} {path}"))
        .collect();
    assert_eq!(
        paths,
        vec![
            "POST /v72/paymentLinks",
            "PATCH /v72/paymentLinks/PL_link_1",
            "POST /v72/paymentLinks",
            "PATCH /v72/paymentLinks/PL_link_2",
        ]
    );
}

#[test]
fn adyen_webhook_hmac_binds_fields_and_rejects_replay_window() {
    let provider = AdyenPaymentProvider::new(
        &adyen_config("https://checkout-test.adyen.com/v72".into()),
        SecretString::new(ADYEN_KEY),
        Some(SecretString::new(ADYEN_HMAC)),
        Arc::new(faktor_provider::egress::MockHttpTransport::new(200, "{}")),
    )
    .unwrap();
    let now = START;
    let event_date = "2023-11-14T22:13:20Z";
    let value = 10_000;
    let signature = sign_adyen_webhook_hmac(
        ADYEN_HMAC,
        "PSP_payment_1",
        "",
        "FaktorTestMerchant",
        "faktor:org_1:invoice:inv_1",
        value,
        "USD",
        "AUTHORISATION",
        "true",
    );
    let body = |amount: i64, signature: &str, event_date: &str| {
        serde_json::json!({
            "live": false,
            "notificationItems": [{
                "NotificationRequestItem": {
                    "eventCode": "AUTHORISATION",
                    "eventDate": event_date,
                    "merchantAccountCode": "FaktorTestMerchant",
                    "merchantReference": "faktor:org_1:invoice:inv_1",
                    "amount": {"currency": "USD", "value": amount},
                    "pspReference": "PSP_payment_1",
                    "originalReference": "",
                    "success": "true",
                    "additionalData": {"hmacSignature": signature}
                }
            }]
        })
        .to_string()
    };
    let verified = provider
        .verify_webhook(
            &WebhookHeaders::default(),
            body(value, &signature, event_date).as_bytes(),
            now,
        )
        .unwrap();
    assert_eq!(verified.event_type, "payment.succeeded");
    assert_eq!(verified.organization_id.as_deref(), Some("org_1"));
    assert_eq!(
        verified
            .payload
            .pointer("/data/object/metadata/local_invoice_id"),
        Some(&Value::String("inv_1".into()))
    );
    assert_eq!(verified.id, "PSP_payment_1:AUTHORISATION");

    for (payload, expected) in [
        (
            body(value + 1, &signature, event_date),
            "hmac does not match",
        ),
        (body(value, "AAAA", event_date), "hmac does not match"),
        (body(value, &signature, "2020-01-01T00:00:00Z"), "timestamp"),
    ] {
        match provider.verify_webhook(&WebhookHeaders::default(), payload.as_bytes(), now) {
            Err(error) => assert!(
                error.to_string().to_lowercase().contains(expected),
                "unexpected error {error}"
            ),
            Ok(_) => panic!("forged/stale webhook verified"),
        }
    }
    let duplicate = provider
        .verify_webhook(
            &WebhookHeaders::default(),
            body(value, &signature, event_date).as_bytes(),
            now,
        )
        .unwrap();
    assert_eq!(duplicate.id, verified.id, "duplicate id must be stable");
}

#[tokio::test]
async fn stripe_identity_is_persisted_and_updates_use_the_item_id() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v1/customers",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/stripe/customer.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v1/subscriptions",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/stripe/subscription_created.json").into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/stripe/subscription_updated.json").into(),
                },
            ],
        },
    );
    mock.route(
        "POST",
        "/v1/subscriptions/sub_test_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/stripe/subscription_updated.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v1/subscriptions/sub_wrong",
        MockAction::Respond {
            status: 400,
            body: include_str!("fixtures/stripe/error_resource_missing.json").into(),
        },
    );
    let base = mock.base_url().await;
    let provider = StripePaymentProvider::new(
        &stripe_config(base),
        SecretString::new(STRIPE_KEY),
        None,
        transport(),
    );

    let customer = provider
        .create_customer(ProviderCustomerRequest {
            organization_id: "org_1".into(),
            idempotency_key: "customer-1".into(),
            email: "billing@example.invalid".into(),
            name: "Org One".into(),
        })
        .await
        .unwrap();
    assert_eq!(customer.id, "cus_test_1");

    let created = provider
        .subscribe(ProviderSubscribeRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-1".into(),
            customer_id: customer.id.clone(),
            plan_id: "pro".into(),
            interval: BillingInterval::Monthly,
            quantity: 2,
            trial_end_ms: None,
            currency: "USD".into(),
            unit_amount_minor: 10_000,
        })
        .await
        .unwrap();
    assert_eq!(created.item_id.as_deref(), Some("si_test_1"));
    assert_eq!(created.product_id.as_deref(), Some("prod_test_1"));
    assert_eq!(created.price_id.as_deref(), Some("price_test_1"));

    // No durable item id: typed refusal, and no update request leaves.
    let before = mock.request_count();
    match provider
        .change_plan(ProviderChangePlanRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-plan-missing-item".into(),
            subscription_id: created.id.clone(),
            provider_item_id: None,
            provider_product_id: created.product_id.clone(),
            provider_price_id: created.price_id.clone(),
            plan_changed: true,
            plan_id: "team".into(),
            interval: BillingInterval::Monthly,
            quantity: 2,
            currency: "USD".into(),
            unit_amount_minor: 20_000,
            prorate: true,
        })
        .await
    {
        Err(PaymentError::Provider(message)) => assert!(message.contains("item id")),
        other => panic!("expected typed item-id refusal, got {other:?}"),
    }
    assert_eq!(mock.request_count(), before);

    let changed = provider
        .change_plan(ProviderChangePlanRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-plan-1".into(),
            subscription_id: created.id.clone(),
            provider_item_id: created.item_id.clone(),
            provider_product_id: created.product_id.clone(),
            provider_price_id: created.price_id.clone(),
            plan_changed: true,
            plan_id: "team".into(),
            interval: BillingInterval::Monthly,
            quantity: 2,
            currency: "USD".into(),
            unit_amount_minor: 20_000,
            prorate: true,
        })
        .await
        .unwrap();
    assert_eq!(changed.price_id.as_deref(), Some("price_test_2"));
    let update = mock
        .requests()
        .into_iter()
        .find(|(method, path, _)| method == "POST" && path == "/v1/subscriptions/sub_test_1")
        .unwrap();
    assert!(
        update.2.contains("items[0][id]=si_test_1")
            || update.2.contains("items%5B0%5D%5Bid%5D=si_test_1"),
        "body={}",
        update.2
    );
    assert!(
        update
            .2
            .contains("items[0][price_data][product]=prod_test_1")
            || update
                .2
                .contains("items%5B0%5D%5Bprice_data%5D%5Bproduct%5D=prod_test_1"),
        "body={}",
        update.2
    );
    assert!(
        !update.2.contains("product_data"),
        "inline update must reuse the product id"
    );

    provider
        .change_plan(ProviderChangePlanRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-seats-1".into(),
            subscription_id: created.id,
            provider_item_id: created.item_id,
            provider_product_id: created.product_id,
            provider_price_id: changed.price_id,
            plan_changed: false,
            plan_id: "team".into(),
            interval: BillingInterval::Monthly,
            quantity: 5,
            currency: "USD".into(),
            unit_amount_minor: 20_000,
            prorate: true,
        })
        .await
        .unwrap();
    let requests = mock.requests();
    let seat_update = requests
        .iter()
        .filter(|(method, path, _)| method == "POST" && path == "/v1/subscriptions/sub_test_1")
        .nth(1)
        .unwrap();
    assert!(
        seat_update.2.contains("items[0][id]=si_test_1")
            || seat_update.2.contains("items%5B0%5D%5Bid%5D=si_test_1"),
        "body={}",
        seat_update.2
    );
    assert!(
        !seat_update.2.contains("price_data"),
        "a seat update must reuse the stored price/item"
    );

    // A wrong durable item id is rejected by the provider, typed; the mock
    // returns Stripe's resource_missing error and no state is fabricated.
    match provider
        .change_plan(ProviderChangePlanRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-wrong-item".into(),
            subscription_id: "sub_wrong".into(),
            provider_item_id: Some("si_wrong".into()),
            provider_product_id: Some("prod_test_1".into()),
            provider_price_id: Some("price_test_1".into()),
            plan_changed: true,
            plan_id: "pro".into(),
            interval: BillingInterval::Monthly,
            quantity: 1,
            currency: "USD".into(),
            unit_amount_minor: 10_000,
            prorate: true,
        })
        .await
    {
        Err(PaymentError::Provider(message)) => {
            assert!(message.contains("resource_missing"), "{message}");
        }
        other => panic!("expected typed provider rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn stripe_processing_and_refund_statuses_are_truthful() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v1/payment_intents",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/stripe/payment_intent_processing.json").into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/stripe/payment_intent_succeeded.json").into(),
                },
            ],
        },
    );
    mock.route(
        "POST",
        "/v1/refunds",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/stripe/refund_pending.json").into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/stripe/refund_failed.json").into(),
                },
            ],
        },
    );
    let base = mock.base_url().await;
    let provider = StripePaymentProvider::new(
        &stripe_config(base),
        SecretString::new(STRIPE_KEY),
        None,
        transport(),
    );
    let charge = |key: &str| ProviderChargeRequest {
        organization_id: "org_1".into(),
        idempotency_key: key.into(),
        customer_id: "cus_test_1".into(),
        payment_method_token: SecretString::new("pm_test_1"),
        amount_minor: 10_000,
        currency: "USD".into(),
        invoice_ref: Some("inv_1".into()),
    };
    let processing = provider.charge(charge("charge-processing")).await.unwrap();
    assert_eq!(processing.status, "pending");
    let succeeded = provider.charge(charge("charge-succeeded")).await.unwrap();
    assert_eq!(succeeded.status, "succeeded");

    let refund = |key: &str| ProviderRefundRequest {
        organization_id: "org_1".into(),
        idempotency_key: key.into(),
        provider_payment_id: "pi_test_1".into(),
        amount_minor: 2_500,
        currency: "USD".into(),
        reason: "goodwill".into(),
    };
    assert_eq!(
        provider
            .refund(refund("refund-pending"))
            .await
            .unwrap()
            .status,
        "pending"
    );
    assert_eq!(
        provider
            .refund(refund("refund-failed"))
            .await
            .unwrap()
            .status,
        "failed"
    );
}

#[tokio::test]
async fn stripe_service_persists_identity_and_uses_it_for_updates() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v1/customers",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/stripe/customer.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v1/subscriptions",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/stripe/subscription_created.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v1/subscriptions/sub_test_1",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/stripe/subscription_updated.json").into(),
                },
                MockAction::Respond {
                    status: 200,
                    body: include_str!("fixtures/stripe/subscription_updated.json").into(),
                },
            ],
        },
    );
    let base = mock.base_url().await;
    let provider: Arc<dyn PaymentProvider> = Arc::new(StripePaymentProvider::new(
        &stripe_config(base),
        SecretString::new(STRIPE_KEY),
        None,
        transport(),
    ));
    let dir = tempfile::tempdir().unwrap();
    let provider_for_restart = provider.clone();
    let svc = service(&dir, provider, Arc::new(ManualClock::new(START)));
    let org = OrganizationId::try_new("org_1").unwrap();
    let subscribed = svc
        .subscribe(
            &org,
            SubscribeRequest {
                plan_id: "pro".into(),
                interval: BillingInterval::Monthly,
                seats: 2,
                trial_days: None,
                customer_email: "billing@example.invalid".into(),
                customer_name: "Org One".into(),
                idempotency_key: "stripe-identity-sub".into(),
            },
        )
        .await
        .unwrap();
    let subscription = &subscribed.subscription;
    assert_eq!(subscription.provider_item_id.as_deref(), Some("si_test_1"));
    assert_eq!(
        subscription.provider_product_id.as_deref(),
        Some("prod_test_1")
    );
    assert_eq!(
        subscription.provider_price_id.as_deref(),
        Some("price_test_1")
    );

    svc.change_plan(
        &org,
        faktor_payments::ChangePlanRequest {
            subscription_id: subscription.id.clone(),
            plan_id: "team".into(),
            interval: Some(BillingInterval::Monthly),
            idempotency_key: "stripe-identity-plan".into(),
        },
    )
    .await
    .unwrap();
    svc.change_seats(
        &org,
        faktor_payments::ChangeSeatsRequest {
            subscription_id: subscription.id.clone(),
            quantity: 4,
            idempotency_key: "stripe-identity-seats".into(),
        },
    )
    .await
    .unwrap();

    let updates: Vec<String> = mock
        .requests()
        .into_iter()
        .filter(|(method, path, _)| method == "POST" && path == "/v1/subscriptions/sub_test_1")
        .map(|(_, _, body)| body)
        .collect();
    assert_eq!(updates.len(), 2);
    for update in &updates {
        assert!(
            update.contains("items[0][id]=si_test_1")
                || update.contains("items%5B0%5D%5Bid%5D=si_test_1"),
            "update must reuse the durable item id: {update}"
        );
    }
    assert!(
        updates[0].contains("items[0][price_data][product]=prod_test_1")
            || updates[0].contains("items%5B0%5D%5Bprice_data%5D%5Bproduct%5D=prod_test_1"),
        "plan update must use the durable product id: {}",
        updates[0]
    );
    assert!(
        !updates[1].contains("price_data"),
        "seat-only update must reuse the stored price identity: {}",
        updates[1]
    );

    // The provider identity is durable, not an in-memory recomputation.
    drop(svc);
    let reopened = service(
        &dir,
        provider_for_restart,
        Arc::new(ManualClock::new(START)),
    );
    let stored = reopened.overview(&org).unwrap().subscription.unwrap();
    assert_eq!(stored.provider_item_id.as_deref(), Some("si_test_1"));
    assert_eq!(stored.provider_product_id.as_deref(), Some("prod_test_1"));
    assert_eq!(stored.provider_price_id.as_deref(), Some("price_test_2"));
}

#[tokio::test]
async fn stripe_subscription_replay_is_idempotent_end_to_end() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v1/customers",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/stripe/customer.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v1/subscriptions",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/stripe/subscription_created.json").into(),
        },
    );
    mock.route(
        "POST",
        "/v1/invoices",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/stripe/invoice.json").into(),
        },
    );
    let base = mock.base_url().await;
    let provider: Arc<dyn PaymentProvider> = Arc::new(StripePaymentProvider::new(
        &stripe_config(base),
        SecretString::new(STRIPE_KEY),
        None,
        transport(),
    ));
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    // The service config's provider is swapped to the mock-backed adapter.
    let service = service(&dir, provider, clock);
    let org = OrganizationId::try_new("org_1").unwrap();
    let request = SubscribeRequest {
        plan_id: "pro".into(),
        interval: BillingInterval::Monthly,
        seats: 2,
        trial_days: None,
        customer_email: "billing@example.invalid".into(),
        customer_name: "Org One".into(),
        idempotency_key: "subscribe-replay".into(),
    };
    let first = service.subscribe(&org, request.clone()).await.unwrap();
    let second = service.subscribe(&org, request).await.unwrap();
    assert!(!first.replayed);
    assert!(second.replayed);
    assert_eq!(
        first.subscription.provider_item_id,
        second.subscription.provider_item_id
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|(method, path, _)| method == "POST" && path == "/v1/subscriptions")
            .count(),
        1,
        "replay must not dispatch a second provider subscription"
    );
    let subscribe_headers = mock.last_request_headers();
    assert!(subscribe_headers
        .iter()
        .any(|(name, value)| name == "idempotency-key" && !value.is_empty()));

    // Pending payment must not settle the invoice or subscription locally.
    let invoice = service
        .issue_invoice(
            &org,
            faktor_payments::IssueInvoiceRequest {
                subscription_id: first.subscription.id.clone(),
                usage: Vec::new(),
                credits_minor: 0,
                discount_minor: 0,
                tax_jurisdiction: None,
                due_in_days: Some(7),
                idempotency_key: "invoice-1".into(),
            },
        )
        .await
        .unwrap();
    let method = service
        .add_payment_method(
            &org,
            faktor_payments::AddPaymentMethodRequest {
                token: "pm_test_1".into(),
                brand: "visa".into(),
                last4: "4242".into(),
                exp_month: 12,
                exp_year: 2030,
                set_default: true,
                idempotency_key: "method-1".into(),
            },
        )
        .unwrap();
    let paid = service
        .pay_invoice(
            &org,
            PayInvoiceRequest {
                invoice_id: invoice.invoice.id.clone(),
                payment_method_id: method.payment_method.id,
                idempotency_key: "pay-1".into(),
            },
        )
        .await;
    // No payment_intents route is scripted in this test; the adapter's 404
    // is typed, and crucially no local payment was fabricated.
    assert!(paid.is_err());
    assert!(service
        .payments(&org, 10)
        .unwrap()
        .iter()
        .all(|payment| payment.status != faktor_payments::PaymentStatus::Succeeded));
}
