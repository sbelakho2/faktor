//! Adversarial adapter tests against local mocked HTTP servers and JSON
//! fixtures. No test reaches a real provider; live Mollie/Stripe test-mode
//! credentials are optional evidence, never a gate.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::mollie::MolliePaymentProvider;
use faktor_payments::provider::{
    EmptyWebhookBindings, PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest,
    ProviderChargeRequest, ProviderCheckoutRequest, ProviderCustomerRequest,
    ProviderCustomerUpdate, ProviderMandateRequest, ProviderPaymentLinkRequest,
    ProviderRefundRequest, ProviderSubscribeRequest, WebhookBinding, WebhookBindingLookup,
};
use faktor_payments::stripe::StripePaymentProvider;
use faktor_payments::{
    BillingInterval, CurrencySpec, DunningConfig, MollieConfig, PayInvoiceRequest, PaymentError,
    PaymentService, PaymentsConfig, PlanPrice, PriceCatalogue, ProviderKind, SecretString,
    SqlitePaymentStore, StaticSecrets, StripeConfig, SubscribeRequest, TaxConfig, WebhookHeaders,
};
use faktor_provider::egress::{HttpTransport, PolicyCheckedHttpTransport};
use faktor_provider::testing::{MockAction, MockServer};
use serde_json::Value;
use tempfile::TempDir;

const START: i64 = 1_700_000_000_000;
const MOLLIE_KEY: &str = "test_mollie_fixture_key";
const STRIPE_KEY: &str = "sk_test_fixture_key";

fn transport() -> Arc<dyn HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::permissive())
}

fn mollie_config(base_url: String) -> MollieConfig {
    MollieConfig {
        base_url,
        api_key_env: "FAKTOR_MOLLIE_API_KEY".into(),
        webhook_url: Some("https://example.invalid/webhooks/payments".into()),
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
        mollie: MollieConfig::default(),
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
async fn mollie_checkout_stored_charge_and_refund_fixture_sequence() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v2/payments",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 201,
                    body: include_str!("fixtures/mollie/payment_open.json").into(),
                },
                MockAction::Respond {
                    status: 201,
                    body: include_str!("fixtures/mollie/payment_paid.json").into(),
                },
            ],
        },
    );
    mock.route(
        "POST",
        "/v2/payments/tr_payment_1/refunds",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 201,
                    body: include_str!("fixtures/mollie/refund_queued.json").into(),
                },
                MockAction::Respond {
                    status: 201,
                    body: include_str!("fixtures/mollie/refund_refunded.json").into(),
                },
                MockAction::Respond {
                    status: 201,
                    body: include_str!("fixtures/mollie/refund_failed.json").into(),
                },
            ],
        },
    );
    let base = mock.base_url().await;
    let provider = MolliePaymentProvider::new(
        &mollie_config(format!("{base}/v2")),
        SecretString::new(MOLLIE_KEY),
        transport(),
    )
    .unwrap();

    let checkout = provider
        .create_checkout(ProviderCheckoutRequest {
            organization_id: "org_1".into(),
            idempotency_key: "checkout-1".into(),
            customer_id: "cst_customer_1".into(),
            description: "Pro plan".into(),
            amount_minor: 10_000,
            currency: "USD".into(),
            success_url: "https://example.invalid/ok".into(),
            cancel_url: "https://example.invalid/cancel".into(),
            webhook_url: Some("https://example.invalid/webhooks/payments".into()),
            local_invoice_id: Some("inv_1".into()),
        })
        .await
        .unwrap();
    assert_eq!(checkout.id, "tr_payment_1");
    assert_eq!(checkout.status, "open");
    assert_eq!(
        checkout.url.as_deref(),
        Some("https://checkout.mollie.test/tr_payment_1")
    );
    let headers = mock.last_request_headers();
    assert!(headers
        .iter()
        .any(|(name, value)| name == "authorization" && value == &format!("Bearer {MOLLIE_KEY}")));
    assert!(
        !headers.iter().any(|(name, _)| name == "idempotency-key"),
        "Mollie does not receive Faktor idempotency keys"
    );
    let checkout_body = &mock.last_request().unwrap().2;
    assert!(checkout_body.contains("\"value\":\"100.00\""));
    assert!(checkout_body.contains("redirectUrl"));
    assert!(checkout_body.contains("webhookUrl"));
    assert!(checkout_body.contains("faktor_kind"));
    assert!(checkout_body.contains("local_invoice_id"));
    assert!(checkout_body.contains("\"sequenceType\":\"first\""));
    assert!(checkout_body.contains("\"customerId\":\"cst_customer_1\""));

    let charge = provider
        .charge(ProviderChargeRequest {
            organization_id: "org_1".into(),
            idempotency_key: "charge-1".into(),
            customer_id: "cst_customer_1".into(),
            payment_method_token: SecretString::new("mdt_mandate_1"),
            amount_minor: 10_000,
            currency: "USD".into(),
            invoice_ref: Some("inv_1".into()),
        })
        .await
        .unwrap();
    assert_eq!(charge.id, "tr_payment_1");
    assert_eq!(charge.status, "succeeded");
    let charge_body = mock.last_request().unwrap().2;
    assert!(charge_body.contains("\"mandateId\":\"mdt_mandate_1\""));
    assert!(charge_body.contains("\"customerId\":\"cst_customer_1\""));
    assert!(charge_body.contains("\"sequenceType\":\"recurring\""));

    let refund = |key: &str| ProviderRefundRequest {
        organization_id: "org_1".into(),
        idempotency_key: key.into(),
        provider_payment_id: "tr_payment_1".into(),
        amount_minor: 2_500,
        currency: "USD".into(),
        reason: "goodwill".into(),
    };
    assert_eq!(
        provider.refund(refund("refund-1")).await.unwrap().status,
        "pending"
    );
    assert_eq!(
        provider.refund(refund("refund-2")).await.unwrap().status,
        "succeeded"
    );
    assert_eq!(
        provider.refund(refund("refund-3")).await.unwrap().status,
        "failed"
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    for (_, _, body) in &requests {
        assert!(!body.contains(MOLLIE_KEY), "key must never enter a body");
        assert!(!body.contains("idempotency"), "no idempotency key body");
    }
    let headers = mock.last_request_headers();
    assert!(!headers.iter().any(|(name, _)| name == "idempotency-key"));
}

#[tokio::test]
async fn mollie_processing_is_pending_and_refusal_is_typed() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v2/payments",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 201,
                    body: include_str!("fixtures/mollie/payment_pending.json").into(),
                },
                MockAction::Respond {
                    status: 201,
                    body: include_str!("fixtures/mollie/payment_failed.json").into(),
                },
                MockAction::Respond {
                    status: 500,
                    body: include_str!("fixtures/mollie/error_server.json").into(),
                },
                MockAction::Respond {
                    status: 422,
                    body: include_str!("fixtures/mollie/error_unprocessable.json").into(),
                },
            ],
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_forged",
        MockAction::Respond {
            status: 404,
            body: include_str!("fixtures/mollie/error_not_found.json").into(),
        },
    );
    let base = mock.base_url().await;
    let provider = MolliePaymentProvider::new(
        &mollie_config(format!("{base}/v2")),
        SecretString::new(MOLLIE_KEY),
        transport(),
    )
    .unwrap();
    let request = || ProviderChargeRequest {
        organization_id: "org_1".into(),
        idempotency_key: "charge-pending".into(),
        customer_id: "cst_customer_1".into(),
        payment_method_token: SecretString::new("mdt_mandate_1"),
        amount_minor: 10_000,
        currency: "USD".into(),
        invoice_ref: Some("inv_1".into()),
    };
    let pending = provider.charge(request()).await.unwrap();
    assert_eq!(pending.status, "pending", "pending must never be succeeded");
    match provider.charge(request()).await {
        Err(PaymentError::Declined(message)) => assert!(message.contains("failed")),
        other => panic!("expected typed refusal, got {other:?}"),
    }
    match provider.charge(request()).await {
        Err(PaymentError::Provider(message)) => assert!(message.contains("500")),
        other => panic!("expected typed provider error, got {other:?}"),
    }
    match provider.charge(request()).await {
        Err(PaymentError::Provider(message)) => assert!(message.contains("amount")),
        other => panic!("expected typed unprocessable error, got {other:?}"),
    }
    let forged = provider.get_payment("tr_forged").await;
    assert!(matches!(forged, Err(PaymentError::NotFound(_))));
}

#[tokio::test]
async fn mollie_customer_subscription_mandate_and_payment_link_lifecycle() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v2/customers",
        MockAction::Respond {
            status: 201,
            body: include_str!("fixtures/mollie/customer.json").into(),
        },
    );
    mock.route(
        "PATCH",
        "/v2/customers/cst_customer_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/customer.json").into(),
        },
    );
    mock.route(
        "DELETE",
        "/v2/customers/cst_customer_1",
        MockAction::Respond {
            status: 204,
            body: String::new(),
        },
    );
    mock.route(
        "POST",
        "/v2/customers/cst_customer_1/subscriptions",
        MockAction::Respond {
            status: 201,
            body: include_str!("fixtures/mollie/subscription_active.json").into(),
        },
    );
    mock.route(
        "PATCH",
        "/v2/customers/cst_customer_1/subscriptions/sub_subscription_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/subscription_updated.json").into(),
        },
    );
    mock.route(
        "DELETE",
        "/v2/customers/cst_customer_1/subscriptions/sub_subscription_1",
        MockAction::Respond {
            status: 204,
            body: String::new(),
        },
    );
    mock.route(
        "POST",
        "/v2/customers/cst_customer_1/mandates",
        MockAction::Respond {
            status: 201,
            body: include_str!("fixtures/mollie/mandate.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/customers/cst_customer_1/mandates/mdt_mandate_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/mandate.json").into(),
        },
    );
    mock.route(
        "DELETE",
        "/v2/customers/cst_customer_1/mandates/mdt_mandate_1",
        MockAction::Respond {
            status: 204,
            body: String::new(),
        },
    );
    mock.route(
        "POST",
        "/v2/payment-links",
        MockAction::Respond {
            status: 201,
            body: include_str!("fixtures/mollie/payment_link.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/payment-links/pl_link_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/payment_link.json").into(),
        },
    );
    let base = mock.base_url().await;
    let provider = MolliePaymentProvider::new(
        &mollie_config(format!("{base}/v2")),
        SecretString::new(MOLLIE_KEY),
        transport(),
    )
    .unwrap();

    let customer = provider
        .create_customer(ProviderCustomerRequest {
            organization_id: "org_1".into(),
            idempotency_key: "customer-1".into(),
            email: "billing@example.invalid".into(),
            name: "Org One".into(),
        })
        .await
        .unwrap();
    assert_eq!(customer.id, "cst_customer_1");
    let updated_customer = provider
        .update_customer(ProviderCustomerUpdate {
            organization_id: "org_1".into(),
            customer_id: customer.id.clone(),
            email: Some("billing@example.invalid".into()),
            name: Some("Org One".into()),
        })
        .await
        .unwrap();
    assert_eq!(updated_customer.id, "cst_customer_1");

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
    assert_eq!(created.id, "sub_subscription_1");
    assert_eq!(created.status, "active");
    let subscribe_body = mock.last_request().unwrap().2;
    assert!(subscribe_body.contains("\"value\":\"200.00\""));
    assert!(subscribe_body.contains("\"interval\":\"1 month\""));
    assert!(subscribe_body.contains("faktor_kind"));

    let changed = provider
        .change_plan(ProviderChangePlanRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-change".into(),
            subscription_id: created.id.clone(),
            customer_id: customer.id.clone(),
            provider_item_id: None,
            provider_product_id: None,
            provider_price_id: None,
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
    assert_eq!(changed.id, "sub_subscription_1");
    let change_body = mock.last_request().unwrap().2;
    assert!(change_body.contains("\"value\":\"400.00\""));

    let scheduled = provider
        .cancel(ProviderCancelRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-cancel-soon".into(),
            subscription_id: changed.id.clone(),
            customer_id: customer.id.clone(),
            at_period_end: true,
        })
        .await
        .unwrap();
    assert!(scheduled.cancel_at_period_end);
    assert_eq!(
        mock.request_count(),
        4,
        "period-end cancel is Faktor-managed"
    );

    let canceled = provider
        .cancel(ProviderCancelRequest {
            organization_id: "org_1".into(),
            idempotency_key: "sub-cancel".into(),
            subscription_id: changed.id,
            customer_id: customer.id.clone(),
            at_period_end: false,
        })
        .await
        .unwrap();
    assert_eq!(canceled.status, "canceled");

    let mandate = provider
        .create_mandate(ProviderMandateRequest {
            organization_id: "org_1".into(),
            idempotency_key: "mandate-1".into(),
            customer_id: customer.id.clone(),
            method: "directdebit".into(),
            consumer_name: Some("Org One".into()),
            consumer_account: Some("NL55INGB0000000000".into()),
            signature_date: Some("2026-10-24".into()),
        })
        .await
        .unwrap();
    assert_eq!(mandate.id, "mdt_mandate_1");
    assert_eq!(mandate.status, "valid");
    let fetched = provider
        .get_mandate("org_1", &customer.id, "mdt_mandate_1")
        .await
        .unwrap();
    assert_eq!(fetched.customer_id, "cst_customer_1");
    let revoked = provider
        .revoke_mandate("org_1", &customer.id, "mdt_mandate_1")
        .await
        .unwrap();
    assert_eq!(revoked.status, "revoked");

    let link = provider
        .create_payment_link(ProviderPaymentLinkRequest {
            organization_id: "org_1".into(),
            idempotency_key: "link-1".into(),
            description: "Faktor invoice inv_1".into(),
            amount_minor: 10_000,
            currency: "USD".into(),
            redirect_url: "https://example.invalid/ok".into(),
            webhook_url: None,
            reusable: true,
        })
        .await
        .unwrap();
    assert_eq!(link.id, "pl_link_1");
    assert_eq!(link.status, "active");
    assert!(link
        .url
        .as_deref()
        .unwrap()
        .starts_with("https://payment-links."));
    let fetched_link = provider.fetch_payment_link("pl_link_1").await.unwrap();
    assert_eq!(fetched_link.amount_minor, 10_000);

    provider
        .delete_customer("org_1", &customer.id)
        .await
        .unwrap();

    let paths: Vec<String> = mock
        .requests()
        .into_iter()
        .map(|(method, path, _)| format!("{method} {path}"))
        .collect();
    assert_eq!(
        paths,
        vec![
            "POST /v2/customers",
            "PATCH /v2/customers/cst_customer_1",
            "POST /v2/customers/cst_customer_1/subscriptions",
            "PATCH /v2/customers/cst_customer_1/subscriptions/sub_subscription_1",
            "DELETE /v2/customers/cst_customer_1/subscriptions/sub_subscription_1",
            "POST /v2/customers/cst_customer_1/mandates",
            "GET /v2/customers/cst_customer_1/mandates/mdt_mandate_1",
            "DELETE /v2/customers/cst_customer_1/mandates/mdt_mandate_1",
            "POST /v2/payment-links",
            "GET /v2/payment-links/pl_link_1",
            "DELETE /v2/customers/cst_customer_1",
        ]
    );
}

struct Bindings;

impl WebhookBindingLookup for Bindings {
    fn binding(&self, id: &str) -> Result<Option<WebhookBinding>, PaymentError> {
        Ok(match id {
            "re_refund_1" => Some(WebhookBinding::Refund {
                organization_id: "org_1".into(),
                provider_payment_id: "tr_payment_1".into(),
            }),
            "sub_subscription_1" => Some(WebhookBinding::Subscription {
                organization_id: "org_1".into(),
                provider_customer_id: "cst_customer_1".into(),
            }),
            _ => None,
        })
    }

    fn is_known_organization(&self, organization_id: &str) -> Result<bool, PaymentError> {
        Ok(organization_id == "org_1")
    }
}

#[tokio::test]
async fn mollie_webhook_is_id_only_and_fetches_authoritative_truth() {
    let mock = MockServer::new();
    mock.route(
        "GET",
        "/v2/payments/tr_payment_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/payment_paid.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_expired",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/payment_expired.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_foreign",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/payment_foreign.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_unmarked",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/payment_unmarked.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_forged",
        MockAction::Respond {
            status: 404,
            body: include_str!("fixtures/mollie/error_not_found.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_payment_1/refunds/re_refund_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/refund_refunded.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/customers/cst_customer_1/subscriptions/sub_subscription_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/subscription_active.json").into(),
        },
    );
    let base = mock.base_url().await;
    let provider = MolliePaymentProvider::new(
        &mollie_config(format!("{base}/v2")),
        SecretString::new(MOLLIE_KEY),
        transport(),
    )
    .unwrap();

    // The posted status is a lie; only the fetched resource is truth.
    let body = b"id=tr_payment_1&status=failed";
    let event = provider
        .resolve_webhook(
            &WebhookHeaders::default(),
            body,
            START,
            &EmptyWebhookBindings,
        )
        .await
        .unwrap();
    assert_eq!(event.event_type, "payment.succeeded");
    assert_eq!(event.organization_id.as_deref(), Some("org_1"));
    assert_eq!(event.id, "mollie:payment:tr_payment_1:paid");
    assert_eq!(
        event.payload.pointer("/mollie/status"),
        Some(&Value::String("paid".into()))
    );
    assert_eq!(
        event
            .payload
            .pointer("/data/object/metadata/local_invoice_id"),
        Some(&Value::String("inv_1".into()))
    );
    assert!(!event.payload.to_string().contains("\"failed\""));
    let replayed = provider
        .resolve_webhook(
            &WebhookHeaders::default(),
            body,
            START + 1_000,
            &EmptyWebhookBindings,
        )
        .await
        .unwrap();
    assert_eq!(
        replayed.id, event.id,
        "duplicate webhook ids must be stable"
    );

    let fetched = provider.get_payment("tr_payment_1").await.unwrap();
    assert_eq!(fetched.status, "paid");
    assert_eq!(fetched.amount_minor, 10_000);
    assert_eq!(fetched.currency, "USD");
    let expired = provider
        .resolve_webhook(
            &WebhookHeaders::default(),
            b"id=tr_expired",
            START,
            &Bindings,
        )
        .await
        .unwrap();
    assert_eq!(expired.event_type, "invoice.payment_failed");
    assert_eq!(expired.id, "mollie:payment:tr_expired:expired");

    let synchronous = provider.verify_webhook(&WebhookHeaders::default(), body, START);
    assert!(matches!(
        synchronous,
        Err(PaymentError::WebhookUnauthorized(_))
    ));
    let forged = provider
        .resolve_webhook(
            &WebhookHeaders::default(),
            b"id=tr_forged",
            START,
            &Bindings,
        )
        .await;
    assert!(matches!(forged, Err(PaymentError::NotFound(_))));
    for foreign in [&b"id=tr_foreign"[..], &b"id=tr_unmarked"[..]] {
        let refused = provider
            .resolve_webhook(&WebhookHeaders::default(), foreign, START, &Bindings)
            .await;
        assert!(
            matches!(refused, Err(PaymentError::WebhookUnauthorized(_))),
            "{refused:?}"
        );
    }
    let missing_binding = provider
        .resolve_webhook(
            &WebhookHeaders::default(),
            b"id=re_unknown",
            START,
            &EmptyWebhookBindings,
        )
        .await;
    assert!(matches!(
        missing_binding,
        Err(PaymentError::WebhookUnauthorized(_))
    ));

    let refund = provider
        .resolve_webhook(
            &WebhookHeaders::default(),
            b"id=re_refund_1",
            START,
            &Bindings,
        )
        .await
        .unwrap();
    assert_eq!(refund.event_type, "refund.succeeded");
    assert_eq!(refund.id, "mollie:refund:re_refund_1:succeeded");

    let subscription = provider
        .resolve_webhook(
            &WebhookHeaders::default(),
            b"id=sub_subscription_1",
            START,
            &Bindings,
        )
        .await
        .unwrap();
    assert_eq!(subscription.event_type, "customer.subscription.updated");
    assert_eq!(
        subscription.id,
        "mollie:subscription:sub_subscription_1:active"
    );
}

#[tokio::test]
async fn mollie_service_webhook_fetch_replay_forgery_and_out_of_order() {
    let mock = MockServer::new();
    mock.route(
        "POST",
        "/v2/customers",
        MockAction::Respond {
            status: 201,
            body: include_str!("fixtures/mollie/customer.json").into(),
        },
    );
    mock.route(
        "PATCH",
        "/v2/customers/cst_customer_1",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/customer.json").into(),
        },
    );
    mock.route(
        "DELETE",
        "/v2/customers/cst_customer_1",
        MockAction::Respond {
            status: 204,
            body: String::new(),
        },
    );
    mock.route(
        "POST",
        "/v2/customers/cst_customer_1/subscriptions",
        MockAction::Respond {
            status: 201,
            body: include_str!("fixtures/mollie/subscription_active.json").into(),
        },
    );
    let base = mock.base_url().await;
    let provider: Arc<dyn PaymentProvider> = Arc::new(
        MolliePaymentProvider::new(
            &mollie_config(format!("{base}/v2")),
            SecretString::new(MOLLIE_KEY),
            transport(),
        )
        .unwrap(),
    );
    let dir = tempfile::tempdir().unwrap();
    let service = service(&dir, provider, Arc::new(ManualClock::new(START)));
    let org = OrganizationId::try_new("org_1").unwrap();
    let subscribed = service
        .subscribe(
            &org,
            SubscribeRequest {
                plan_id: "pro".into(),
                interval: BillingInterval::Monthly,
                seats: 2,
                trial_days: None,
                customer_email: "billing@example.invalid".into(),
                customer_name: "Org One".into(),
                idempotency_key: "mollie-live-sub".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        subscribed.subscription.provider_subscription_id,
        "sub_subscription_1"
    );
    let invoice = service
        .issue_invoice(
            &org,
            faktor_payments::IssueInvoiceRequest {
                subscription_id: subscribed.subscription.id.clone(),
                usage: Vec::new(),
                credits_minor: 0,
                discount_minor: 0,
                tax_jurisdiction: None,
                due_in_days: Some(7),
                idempotency_key: "mollie-invoice-1".into(),
            },
        )
        .await
        .unwrap();
    let invoice_id = invoice.invoice.id;
    let total = invoice.invoice.total_minor;
    let amount_value = format!("{}.{:02}", total / 100, total % 100);
    let paid = include_str!("fixtures/mollie/payment_paid.json")
        .replace("\"inv_1\"", &format!("\"{invoice_id}\""))
        .replace("\"100.00\"", &format!("\"{amount_value}\""));
    let failed = include_str!("fixtures/mollie/payment_failed.json")
        .replace("\"inv_1\"", &format!("\"{invoice_id}\""))
        .replace("\"100.00\"", &format!("\"{amount_value}\""));
    mock.route(
        "GET",
        "/v2/payments/tr_payment_1",
        MockAction::Respond {
            status: 200,
            body: paid.clone(),
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_foreign",
        MockAction::Respond {
            status: 200,
            body: include_str!("fixtures/mollie/payment_foreign.json").into(),
        },
    );
    mock.route(
        "GET",
        "/v2/payments/tr_forged",
        MockAction::Respond {
            status: 404,
            body: include_str!("fixtures/mollie/error_not_found.json").into(),
        },
    );

    let body = b"id=tr_payment_1&status=failed";
    let applied = service
        .handle_webhook(&WebhookHeaders::default(), body, START)
        .await
        .unwrap();
    assert_eq!(
        applied.disposition,
        faktor_payments::WebhookDisposition::Applied
    );
    let paid_invoice = service.invoice(&org, &invoice_id).unwrap();
    assert_eq!(paid_invoice.status, faktor_payments::InvoiceStatus::Paid);
    assert_eq!(paid_invoice.amount_paid_minor, paid_invoice.total_minor);
    let replay = service
        .handle_webhook(&WebhookHeaders::default(), body, START + 1_000)
        .await
        .unwrap();
    assert_eq!(
        replay.disposition,
        faktor_payments::WebhookDisposition::Duplicate
    );

    // A later fetch that regresses the provider status must not unpay the
    // invoice: the local payment is already settled.
    mock.route(
        "GET",
        "/v2/payments/tr_payment_1",
        MockAction::Respond {
            status: 200,
            body: failed,
        },
    );
    let out_of_order = service
        .handle_webhook(&WebhookHeaders::default(), body, START + 2_000)
        .await
        .unwrap();
    assert_eq!(
        out_of_order.disposition,
        faktor_payments::WebhookDisposition::Ignored
    );
    assert_eq!(
        service.invoice(&org, &invoice_id).unwrap().status,
        faktor_payments::InvoiceStatus::Paid
    );

    let foreign = service
        .handle_webhook(&WebhookHeaders::default(), b"id=tr_foreign", START + 3_000)
        .await;
    assert!(matches!(foreign, Err(PaymentError::WebhookUnauthorized(_))));
    let forged = service
        .handle_webhook(&WebhookHeaders::default(), b"id=tr_forged", START + 4_000)
        .await;
    assert!(matches!(forged, Err(PaymentError::NotFound(_))));
}

#[tokio::test]
async fn mollie_live_test_mode_is_explicitly_opt_in() {
    if std::env::var("FAKTOR_MOLLIE_LIVE_OPT_IN").as_deref() != Ok("1") {
        eprintln!("SKIP: FAKTOR_MOLLIE_LIVE_OPT_IN is not 1 (no live Mollie evidence)");
        return;
    }
    let Ok(api_key) = std::env::var("FAKTOR_MOLLIE_API_KEY") else {
        eprintln!("SKIP: FAKTOR_MOLLIE_API_KEY is unset (typed live-credential absence)");
        return;
    };
    if api_key.trim().is_empty() {
        eprintln!("SKIP: FAKTOR_MOLLIE_API_KEY is empty (typed live-credential absence)");
        return;
    }
    let provider = MolliePaymentProvider::new(
        &MollieConfig {
            base_url: "https://api.mollie.com/v2".into(),
            api_key_env: "FAKTOR_MOLLIE_API_KEY".into(),
            webhook_url: Some("https://example.invalid/webhooks/payments".into()),
            user_agent: "faktor-payments-live-test/0.1".into(),
        },
        SecretString::new(api_key),
        transport(),
    )
    .unwrap();
    let checkout = provider
        .create_checkout(ProviderCheckoutRequest {
            organization_id: "org_live_probe".into(),
            idempotency_key: "mollie-live-probe".into(),
            customer_id: "cst_live_probe".into(),
            description: "Faktor live-mode probe".into(),
            amount_minor: 100,
            currency: "EUR".into(),
            success_url: "https://example.invalid/ok".into(),
            cancel_url: "https://example.invalid/cancel".into(),
            webhook_url: Some("https://example.invalid/webhooks/payments".into()),
            local_invoice_id: None,
        })
        .await
        .unwrap();
    assert!(checkout.id.starts_with("tr_"));
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
            customer_id: "cus_test_1".into(),
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
            customer_id: "cus_test_1".into(),
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
            customer_id: "cus_test_1".into(),
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
            customer_id: "cus_test_1".into(),
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
                    body: include_str!("fixtures/stripe/subscription_seats_updated.json").into(),
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
