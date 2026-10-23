use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::{
    AddPaymentMethodRequest, BillingInterval, CancelRequest, ChangePlanRequest, ChangeSeatsRequest,
    CurrencySpec, DunningConfig, InvoiceStatus, IssueInvoiceRequest, PayInvoiceRequest,
    PaymentError, PaymentService, PaymentsConfig, PlanPrice, PriceCatalogue, ProviderKind,
    RefundRequest, SqlitePaymentStore, StaticSecrets, StripeConfig, SubscribeRequest, TaxConfig,
    TaxRate, UsageCharge, WebhookHeaders, DEFAULT_WEBHOOK_WINDOW_MS,
};
use tempfile::TempDir;

const SECRET_ENV: &str = "FAKTOR_TEST_WEBHOOK_SECRET";
const WEBHOOK_SECRET: &str = "whsec_test_local";
const START: i64 = 1_700_000_000_000;

fn config() -> PaymentsConfig {
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
    plans.insert(
        "euro".into(),
        PlanPrice {
            plan_id: "euro".into(),
            currency: "EUR".into(),
            monthly_minor: 9_000,
            annual_minor: 90_000,
            per_seat_monthly_minor: 2_000,
            per_seat_annual_minor: 20_000,
        },
    );
    let mut currencies = BTreeMap::new();
    currencies.insert("USD".into(), CurrencySpec { exponent: 2 });
    currencies.insert("EUR".into(), CurrencySpec { exponent: 2 });
    let mut rates = BTreeMap::new();
    rates.insert(
        "US-CA".into(),
        TaxRate {
            rate_bps: 825,
            inclusive: false,
        },
    );
    rates.insert(
        "DE".into(),
        TaxRate {
            rate_bps: 1_900,
            inclusive: true,
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
        webhook_secret_env: SECRET_ENV.into(),
        stripe: StripeConfig::default(),
    }
}

fn secrets() -> Arc<StaticSecrets> {
    Arc::new(StaticSecrets::new([(
        SECRET_ENV.to_string(),
        WEBHOOK_SECRET.to_string(),
    )]))
}

fn org(id: &str) -> OrganizationId {
    OrganizationId::try_new(id).unwrap()
}

fn store(path: &std::path::Path) -> Arc<SqlitePaymentStore> {
    Arc::new(SqlitePaymentStore::open(path).unwrap())
}

fn service(path: &std::path::Path, clock: Arc<ManualClock>) -> PaymentService {
    PaymentService::open_local(store(path), config(), clock, secrets()).unwrap()
}

fn subscribe(seats: u32, trial_days: Option<u32>) -> SubscribeRequest {
    SubscribeRequest {
        plan_id: "pro".into(),
        interval: BillingInterval::Monthly,
        seats,
        trial_days,
        customer_email: "owner@example.com".into(),
        customer_name: "Owner".into(),
        idempotency_key: "subscribe-1".into(),
    }
}

fn method_request() -> AddPaymentMethodRequest {
    AddPaymentMethodRequest {
        token: "tok_local_visa".into(),
        brand: "visa".into(),
        last4: "4242".into(),
        exp_month: 12,
        exp_year: 2030,
        set_default: true,
        idempotency_key: "method-1".into(),
    }
}

fn invoice_request(subscription_id: &str) -> IssueInvoiceRequest {
    IssueInvoiceRequest {
        subscription_id: subscription_id.into(),
        usage: vec![UsageCharge {
            description: "extra inference".into(),
            quantity: 3,
            unit_amount_minor: 500,
        }],
        credits_minor: 200,
        discount_minor: 0,
        tax_jurisdiction: Some("US-CA".into()),
        due_in_days: Some(7),
        idempotency_key: "invoice-1".into(),
    }
}

#[tokio::test]
async fn full_local_provider_lifecycle_is_durable_and_exact() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let service = service(&path, clock.clone());
    let organization = org("org-lifecycle");

    let started = service
        .subscribe(&organization, subscribe(2, Some(7)))
        .await
        .unwrap();
    assert!(started.subscription.state == faktor_payments::SubscriptionState::Trialing);
    assert_eq!(started.subscription.quantity, 2);
    let replayed = service
        .subscribe(&organization, subscribe(2, Some(7)))
        .await
        .unwrap();
    assert!(replayed.replayed);

    clock.advance(8 * 24 * 60 * 60 * 1000);
    let tick = service.tick(&organization).unwrap();
    assert!(tick.changed);
    assert_eq!(
        tick.subscription.state,
        faktor_payments::SubscriptionState::Active
    );

    let method = service
        .add_payment_method(&organization, method_request())
        .unwrap();
    assert_eq!(method.payment_method.last4, "4242");

    let issued = service
        .issue_invoice(&organization, invoice_request(&started.subscription.id))
        .await
        .unwrap();
    assert_eq!(issued.invoice.number, "INV-000001");
    // 100.00 plan + (2 x 25.00) seats + (3 x 5.00) usage - 2.00 credits.
    assert_eq!(issued.invoice.subtotal_minor, 10_000 + 5_000 + 1_500);
    assert_eq!(issued.invoice.credits_minor, 200);
    // 8.25% exclusive tax on 16,300 is 1,344.75 -> 1,345 (half away from zero).
    assert_eq!(issued.invoice.tax_minor, 1_345);
    assert_eq!(issued.invoice.total_minor, 17_645);
    assert_eq!(
        issued
            .invoice
            .lines
            .iter()
            .map(|line| line.amount_minor)
            .sum::<i64>(),
        issued.invoice.total_minor
    );

    let pay_request = PayInvoiceRequest {
        invoice_id: issued.invoice.id.clone(),
        payment_method_id: method.payment_method.id.clone(),
        idempotency_key: "pay-1".into(),
    };
    let paid = service
        .pay_invoice(&organization, pay_request.clone())
        .await
        .unwrap();
    assert_eq!(paid.invoice.status, InvoiceStatus::Paid);
    assert_eq!(paid.payment.amount_minor, 17_645);
    assert_eq!(paid.invoice.balance_minor(), 0);
    let pay_replay = service
        .pay_invoice(&organization, pay_request)
        .await
        .unwrap();
    assert!(pay_replay.replayed);

    let refunded = service
        .refund(
            &organization,
            RefundRequest {
                payment_id: paid.payment.id.clone(),
                amount_minor: 5_000,
                reason: "goodwill".into(),
                idempotency_key: "refund-1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(refunded.credit_note.number, "CN-000001");
    assert_eq!(
        refunded.payment.status,
        faktor_payments::PaymentStatus::PartiallyRefunded
    );
    assert_eq!(refunded.invoice.amount_refunded_minor, 5_000);

    let cancel = service
        .cancel(
            &organization,
            CancelRequest {
                subscription_id: started.subscription.id.clone(),
                at_period_end: true,
                idempotency_key: "cancel-1".into(),
            },
        )
        .await
        .unwrap();
    assert!(cancel.subscription.cancel_at_period_end);
    assert_eq!(
        cancel.subscription.state,
        faktor_payments::SubscriptionState::Active
    );
    clock.advance(31 * 24 * 60 * 60 * 1000);
    let canceled = service.tick(&organization).unwrap();
    assert_eq!(
        canceled.subscription.state,
        faktor_payments::SubscriptionState::Canceled
    );

    let overview = service.overview(&organization).unwrap();
    assert_eq!(overview.payment.state, "available");
    assert_eq!(overview.invoices.state, "available");
    assert_eq!(overview.seats_capability.state, "available");
    assert_eq!(overview.tax_capability.state, "available");
    assert_eq!(overview.currency_capability.state, "available");
    assert_eq!(overview.currency.as_deref(), Some("USD"));
    assert_eq!(overview.currency_exponent, Some(2));
    assert_eq!(overview.outstanding_minor, 0);
    assert_eq!(overview.paid_minor, 17_645);
}

#[tokio::test]
async fn plan_and_seat_proration_use_real_prices_and_exact_integer_math() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let service = service(&path, clock.clone());
    let organization = org("org-proration");
    let subscription = service
        .subscribe(&organization, subscribe(1, None))
        .await
        .unwrap()
        .subscription;
    clock.advance(10 * 24 * 60 * 60 * 1000);

    let seat_preview = service
        .preview_seat_change(
            &organization,
            faktor_payments::PreviewSeatsRequest {
                subscription_id: subscription.id.clone(),
                quantity: 3,
            },
        )
        .unwrap();
    // Two added seats at 25.00 for 20/30 days: 5000 * 20 / 30 = 3333.33.
    assert_eq!(seat_preview.charge_minor, 3_333);
    assert_eq!(seat_preview.credit_minor, 0);
    assert_eq!(seat_preview.net_minor, 3_333);

    let seat_change = service
        .change_seats(
            &organization,
            ChangeSeatsRequest {
                subscription_id: subscription.id.clone(),
                quantity: 3,
                idempotency_key: "seats-1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(seat_change.subscription.quantity, 3);
    assert_eq!(seat_change.proration.net_minor, 3_333);
    let seat_replay = service
        .change_seats(
            &organization,
            ChangeSeatsRequest {
                subscription_id: subscription.id.clone(),
                quantity: 3,
                idempotency_key: "seats-1".into(),
            },
        )
        .await
        .unwrap();
    assert!(seat_replay.replayed);

    let plan_preview = service
        .preview_plan_change(
            &organization,
            faktor_payments::PreviewPlanRequest {
                subscription_id: subscription.id.clone(),
                plan_id: "team".into(),
                interval: None,
            },
        )
        .unwrap();
    // Old 100.00 unused = -6666.67 -> -6667; new 200.00 remaining = 13333.33.
    assert_eq!(plan_preview.credit_minor, -6_667);
    assert_eq!(plan_preview.charge_minor, 13_333);
    assert_eq!(plan_preview.net_minor, 6_666);

    let changed = service
        .change_plan(
            &organization,
            ChangePlanRequest {
                subscription_id: subscription.id.clone(),
                plan_id: "team".into(),
                interval: None,
                idempotency_key: "plan-1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(changed.subscription.plan_id, "team");
    assert_eq!(changed.proration.net_minor, 6_666);

    let euro = service
        .preview_plan_change(
            &organization,
            faktor_payments::PreviewPlanRequest {
                subscription_id: subscription.id.clone(),
                plan_id: "euro".into(),
                interval: None,
            },
        )
        .unwrap_err();
    assert_eq!(
        euro,
        PaymentError::FxUnconfigured("EUR".into(), "USD".into())
    );
}

#[tokio::test]
async fn invoices_cover_inclusive_tax_credits_discounts_and_exports() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let service = service(&path, clock);
    let organization = org("org-invoice-tax");
    let subscription = service
        .subscribe(&organization, subscribe(1, None))
        .await
        .unwrap()
        .subscription;

    let mut request = invoice_request(&subscription.id);
    request.idempotency_key = "invoice-exclusive".into();
    request.usage[0].description = "=HYPERLINK(\"http://evil\")".into();
    let exclusive = service
        .issue_invoice(&organization, request.clone())
        .await
        .unwrap();
    assert!(!exclusive.invoice.tax_inclusive);
    assert_eq!(
        exclusive.invoice.total_minor,
        exclusive.invoice.subtotal_minor - 200 + exclusive.invoice.tax_minor
    );

    let mut inclusive = invoice_request(&subscription.id);
    inclusive.idempotency_key = "invoice-inclusive".into();
    inclusive.tax_jurisdiction = Some("DE".into());
    inclusive.credits_minor = 0;
    let inclusive = service
        .issue_invoice(&organization, inclusive)
        .await
        .unwrap();
    assert!(inclusive.invoice.tax_inclusive);
    assert_eq!(
        inclusive.invoice.total_minor,
        inclusive.invoice.subtotal_minor
    );
    assert!(inclusive.invoice.tax_minor > 0);

    let csv = service.export_invoices(&organization, "csv", 100).unwrap();
    assert!(
        csv.contains("'=HYPERLINK"),
        "formula must be neutralized: {csv}"
    );
    assert!(!csv.lines().any(|line| line.starts_with("=HYPERLINK")));
    let json = service.export_invoices(&organization, "json", 100).unwrap();
    assert!(json.contains("INV-000001"));
    assert!(json.contains("INV-000002"));
    let page = service.invoices(&organization, None, 1).unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.has_more);
    assert!(page.next_cursor.is_some());
    let second = service
        .invoices(&organization, page.next_cursor.as_deref(), 1)
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_ne!(page.items[0].id, second.items[0].id);
    assert!(matches!(
        service.export_invoices(&organization, "yaml", 1),
        Err(PaymentError::Malformed(_))
    ));
}

#[tokio::test]
async fn webhook_forgery_replay_and_out_of_order_are_typed() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let service = service(&path, clock.clone());
    let organization = org("org-webhook");
    let subscription = service
        .subscribe(&organization, subscribe(1, None))
        .await
        .unwrap()
        .subscription;
    let invoice = service
        .issue_invoice(&organization, invoice_request(&subscription.id))
        .await
        .unwrap()
        .invoice;
    let provider_invoice = invoice.provider_invoice_id.clone().unwrap();
    let expected_total = invoice.total_minor;

    let now_secs = START / 1000;
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_paid",
        "type": "invoice.paid",
        "created": now_secs,
        "data": {"object": {
            "id": provider_invoice,
            "amount_paid": expected_total,
            "metadata": {"organization_id": "org-webhook"}
        }}
    }))
    .unwrap();
    let forged = format!("t={now_secs},v1={}", "0".repeat(64));
    let forged_headers = WebhookHeaders::from_pairs([("Stripe-Signature", forged.as_str())]);
    assert!(matches!(
        service.handle_webhook(&forged_headers, &body, START),
        Err(PaymentError::WebhookUnauthorized(_))
    ));

    let header = faktor_payments::sign_local_webhook(WEBHOOK_SECRET, now_secs, &body);
    let headers = WebhookHeaders::from_pairs([("Stripe-Signature", header.as_str())]);
    let applied = service.handle_webhook(&headers, &body, START).unwrap();
    assert_eq!(
        applied.disposition,
        faktor_payments::WebhookDisposition::Applied
    );
    let paid = service.invoice(&organization, &invoice.id).unwrap();
    assert_eq!(paid.status, InvoiceStatus::Paid);
    assert_eq!(paid.amount_paid_minor, expected_total);

    // Replay of the exact same event id is idempotent.
    let duplicate = service.handle_webhook(&headers, &body, START).unwrap();
    assert_eq!(
        duplicate.disposition,
        faktor_payments::WebhookDisposition::Duplicate
    );
    assert_eq!(
        service
            .invoice(&organization, &invoice.id)
            .unwrap()
            .amount_paid_minor,
        expected_total
    );

    // An OLDER subscription event arriving after the paid event is stale.
    let stale_body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_old_sub",
        "type": "customer.subscription.updated",
        "created": now_secs - 10,
        "data": {"object": {
            "id": subscription.provider_subscription_id,
            "status": "past_due",
            "quantity": 9,
            "metadata": {"organization_id": "org-webhook"}
        }}
    }))
    .unwrap();
    let stale_header = faktor_payments::sign_local_webhook(WEBHOOK_SECRET, now_secs, &stale_body);
    let stale_headers = WebhookHeaders::from_pairs([("Stripe-Signature", stale_header.as_str())]);
    let stale = service
        .handle_webhook(&stale_headers, &stale_body, START)
        .unwrap();
    assert_eq!(
        stale.disposition,
        faktor_payments::WebhookDisposition::StaleIgnored
    );
    assert_eq!(
        service
            .store()
            .subscription("org-webhook")
            .unwrap()
            .unwrap()
            .quantity,
        1
    );

    // A NEWER event applies.
    let newer_body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_new_sub",
        "type": "customer.subscription.updated",
        "created": now_secs + 10,
        "data": {"object": {
            "id": subscription.provider_subscription_id,
            "status": "active",
            "quantity": 4,
            "metadata": {"organization_id": "org-webhook"}
        }}
    }))
    .unwrap();
    let newer_header = faktor_payments::sign_local_webhook(WEBHOOK_SECRET, now_secs, &newer_body);
    let newer_headers = WebhookHeaders::from_pairs([("Stripe-Signature", newer_header.as_str())]);
    let newer = service
        .handle_webhook(&newer_headers, &newer_body, START + 10_000)
        .unwrap();
    assert_eq!(
        newer.disposition,
        faktor_payments::WebhookDisposition::Applied
    );
    assert_eq!(
        service
            .store()
            .subscription("org-webhook")
            .unwrap()
            .unwrap()
            .quantity,
        4
    );
}

#[tokio::test]
async fn refunds_are_bounded_by_paid_and_credit_notes_number() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let service = service(&path, clock);
    let organization = org("org-refund");
    let subscription = service
        .subscribe(&organization, subscribe(1, None))
        .await
        .unwrap()
        .subscription;
    let method = service
        .add_payment_method(&organization, method_request())
        .unwrap()
        .payment_method;
    let invoice = service
        .issue_invoice(&organization, invoice_request(&subscription.id))
        .await
        .unwrap()
        .invoice;
    let payment = service
        .pay_invoice(
            &organization,
            PayInvoiceRequest {
                invoice_id: invoice.id.clone(),
                payment_method_id: method.id,
                idempotency_key: "pay-refund".into(),
            },
        )
        .await
        .unwrap()
        .payment;

    let first = service
        .refund(
            &organization,
            RefundRequest {
                payment_id: payment.id.clone(),
                amount_minor: 400,
                reason: "first".into(),
                idempotency_key: "refund-a".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(first.credit_note.number, "CN-000001");
    let too_much = service
        .refund(
            &organization,
            RefundRequest {
                payment_id: payment.id.clone(),
                amount_minor: payment.amount_minor - 399 + 1,
                reason: "too much".into(),
                idempotency_key: "refund-b".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(too_much, PaymentError::RefundExceedsPaid { .. }));
    let second = service
        .refund(
            &organization,
            RefundRequest {
                payment_id: payment.id.clone(),
                amount_minor: payment.amount_minor - 400,
                reason: "rest".into(),
                idempotency_key: "refund-c".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(second.credit_note.number, "CN-000002");
    assert_eq!(
        second.payment.status,
        faktor_payments::PaymentStatus::Refunded
    );
    assert_eq!(second.payment.refunded_minor, payment.amount_minor);
    let exhausted = service
        .refund(
            &organization,
            RefundRequest {
                payment_id: payment.id,
                amount_minor: 1,
                reason: "one more".into(),
                idempotency_key: "refund-d".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(exhausted, PaymentError::RefundExceedsPaid { .. }));
}

#[tokio::test]
async fn tenant_isolation_and_auth_before_disclosure_shapes_are_real() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let service = service(&path, clock);
    let org_a = org("org-a");
    let org_b = org("org-b");
    let sub_a = service
        .subscribe(&org_a, subscribe(1, None))
        .await
        .unwrap()
        .subscription;
    let sub_b = service
        .subscribe(
            &org_b,
            SubscribeRequest {
                idempotency_key: "subscribe-b".into(),
                customer_email: "b@example.com".into(),
                customer_name: "B".into(),
                ..subscribe(1, None)
            },
        )
        .await
        .unwrap()
        .subscription;
    let invoice_a = service
        .issue_invoice(&org_a, invoice_request(&sub_a.id))
        .await
        .unwrap()
        .invoice;
    let invoice_b = service
        .issue_invoice(
            &org_b,
            IssueInvoiceRequest {
                idempotency_key: "invoice-b".into(),
                ..invoice_request(&sub_b.id)
            },
        )
        .await
        .unwrap()
        .invoice;
    assert!(matches!(
        service.invoice(&org_b, &invoice_a.id),
        Err(PaymentError::NotFound(_))
    ));
    assert!(matches!(
        service.invoice(&org_a, &invoice_b.id),
        Err(PaymentError::NotFound(_))
    ));
    let method = service
        .add_payment_method(&org_a, method_request())
        .unwrap()
        .payment_method;
    assert!(service
        .payment_methods(&org_b)
        .unwrap()
        .iter()
        .all(|other| other.id != method.id));
    let overview_b = service.overview(&org_b).unwrap();
    assert_eq!(overview_b.invoice_count, 1);
    assert_ne!(overview_b.organization_id, "org-a");
}

#[tokio::test]
async fn crash_restart_idempotency_replays_the_same_rows() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let organization = org("org-crash");
    let request = subscribe(1, None);
    let invoice_request_value;
    {
        let service = service(&path, clock.clone());
        let subscription = service
            .subscribe(&organization, request.clone())
            .await
            .unwrap()
            .subscription;
        invoice_request_value = invoice_request(&subscription.id);
        service
            .issue_invoice(&organization, invoice_request_value.clone())
            .await
            .unwrap();
    }
    // Reopen the exact durable file: the idempotent operations replay and
    // never append a second subscription/invoice.
    let service = service(&path, clock);
    let replay_sub = service.subscribe(&organization, request).await.unwrap();
    assert!(replay_sub.replayed);
    let replay_invoice = service
        .issue_invoice(&organization, invoice_request_value)
        .await
        .unwrap();
    assert!(replay_invoice.replayed);
    assert_eq!(
        service
            .invoices(&organization, None, 100)
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(service.overview(&organization).unwrap().invoice_count, 1);
}

#[tokio::test]
async fn provider_unconfigured_is_typed_and_never_fabricates_payment() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let mut cfg = config();
    cfg.provider = ProviderKind::Stripe;
    cfg.stripe = StripeConfig {
        base_url: "https://api.stripe.com".into(),
        api_key_env: "FAKTOR_TEST_ABSENT_STRIPE_KEY".into(),
        webhook_secret_env: "FAKTOR_TEST_ABSENT_WEBHOOK_KEY".into(),
        user_agent: "faktor-test/0.1".into(),
    };
    let service = PaymentService::open_stripe(
        store(&dir.path().join("payment.db")),
        cfg,
        clock,
        Arc::new(StaticSecrets::default()),
        None,
    )
    .unwrap();
    let organization = org("org-unconfigured");
    let error = service
        .subscribe(&organization, subscribe(1, None))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "payment_provider_unconfigured");
    assert_eq!(service.provider_status().state, "unconfigured");
    let overview = service.overview(&organization).unwrap();
    assert_eq!(overview.payment.state, "unavailable");
    assert_eq!(
        overview.payment.cause.as_deref(),
        Some("payment_provider_unconfigured")
    );
}

#[tokio::test]
async fn payment_methods_never_accept_a_raw_pan_and_never_serialize_the_token() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let service = service(&dir.path().join("payment.db"), clock);
    let organization = org("org-pan");
    let mut request = method_request();
    request.token = "4242 4242 4242 4242".into();
    assert!(matches!(
        service.add_payment_method(&organization, request),
        Err(PaymentError::Malformed(_))
    ));
    let method = service
        .add_payment_method(&organization, method_request())
        .unwrap();
    let json = serde_json::to_value(&method.payment_method).unwrap();
    assert!(!json.to_string().contains("tok_local_visa"));
    assert_eq!(json["last4"], "4242");
    assert_eq!(json["brand"], "visa");
}

#[tokio::test]
async fn webhook_timestamp_window_is_enforced() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let service = service(&dir.path().join("payment.db"), clock);
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_stale",
        "type": "invoice.paid",
        "created": START / 1000,
        "data": {"object": {"metadata": {"organization_id": "org-window"}}}
    }))
    .unwrap();
    let stale = faktor_payments::sign_local_webhook(
        WEBHOOK_SECRET,
        START / 1000 - (DEFAULT_WEBHOOK_WINDOW_MS / 1000) - 1,
        &body,
    );
    let headers = WebhookHeaders::from_pairs([("Stripe-Signature", stale.as_str())]);
    assert_eq!(
        service.handle_webhook(&headers, &body, START),
        Err(PaymentError::WebhookStaleTimestamp)
    );
}

#[tokio::test]
async fn dunning_attempts_and_grace_cancel_a_past_due_subscription() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let service = service(&path, clock.clone());
    let organization = org("org-dunning");
    let subscription = service
        .subscribe(&organization, subscribe(1, None))
        .await
        .unwrap()
        .subscription;
    let invoice = service
        .issue_invoice(&organization, invoice_request(&subscription.id))
        .await
        .unwrap()
        .invoice;
    let first = service
        .record_payment_failure(&organization, &invoice.id)
        .unwrap()
        .unwrap();
    assert_eq!(first.state, faktor_payments::SubscriptionState::PastDue);
    assert_eq!(first.dunning_attempts, 1);
    let second = service
        .record_payment_failure(&organization, &invoice.id)
        .unwrap()
        .unwrap();
    assert_eq!(second.state, faktor_payments::SubscriptionState::Canceled);
    assert_eq!(second.dunning_attempts, 2);
    assert!(second.grace_end_ms.is_some());
}

#[tokio::test]
async fn dunning_grace_expiry_cancels_without_exhausting_attempts() {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::new(START));
    let path = dir.path().join("payment.db");
    let mut cfg = config();
    cfg.dunning = faktor_payments::DunningConfig {
        max_attempts: 5,
        grace_ms: 1_000,
    };
    let service = PaymentService::open_local(store(&path), cfg, clock.clone(), secrets()).unwrap();
    let organization = org("org-grace");
    let subscription = service
        .subscribe(&organization, subscribe(1, None))
        .await
        .unwrap()
        .subscription;
    let invoice = service
        .issue_invoice(&organization, invoice_request(&subscription.id))
        .await
        .unwrap()
        .invoice;
    let failed = service
        .record_payment_failure(&organization, &invoice.id)
        .unwrap()
        .unwrap();
    assert_eq!(failed.state, faktor_payments::SubscriptionState::PastDue);
    assert_eq!(failed.dunning_attempts, 1);
    clock.advance(1_001);
    let canceled = service.tick(&organization).unwrap();
    assert!(canceled.changed);
    assert_eq!(
        canceled.subscription.state,
        faktor_payments::SubscriptionState::Canceled
    );
}
