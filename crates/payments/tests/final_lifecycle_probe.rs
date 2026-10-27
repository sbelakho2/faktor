//! Independent final-wave payment probe (verifier-authored): local provider
//! full lifecycle with exact invoice/proration/seat/tax/currency arithmetic
//! and webhook forgery/replay refusal.
//!
//! All arithmetic is asserted as exact integers: no float tolerance, no
//! saturating fallbacks. The local provider is the in-tree offline provider,
//! so this probe needs no third-party service.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{Clock, ManualClock, OrganizationId};
use faktor_payments::{
    convert, exclusive_tax, inclusive_tax, prorate, sign_local_webhook, BillingInterval,
    ChangePlanRequest, ChangeSeatsRequest, CurrencySpec, InvoiceLineKind, IssueInvoiceRequest,
    PaymentError, PaymentService, PaymentsConfig, PlanPrice, PreviewPlanRequest,
    PreviewSeatsRequest, PriceCatalogue, ProviderKind, SqlitePaymentStore, StaticSecrets,
    SubscribeRequest, TaxConfig, TaxRate, WebhookDisposition, WebhookHeaders,
};

const T0: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;
const WEBHOOK_SECRET: &str = "whsec_final_probe";
const WEBHOOK_ENV: &str = "FINAL_PROBE_WEBHOOK_SECRET";

fn plan(
    plan_id: &str,
    monthly: i64,
    annual: i64,
    seat_monthly: i64,
    seat_annual: i64,
) -> PlanPrice {
    PlanPrice {
        plan_id: plan_id.into(),
        currency: "USD".into(),
        monthly_minor: monthly,
        annual_minor: annual,
        per_seat_monthly_minor: seat_monthly,
        per_seat_annual_minor: seat_annual,
    }
}

fn config() -> PaymentsConfig {
    let mut plans = BTreeMap::new();
    plans.insert("pro".into(), plan("pro", 1_000, 10_000, 250, 2_500));
    plans.insert("team".into(), plan("team", 2_000, 20_000, 0, 0));
    let catalogue = PriceCatalogue {
        plans,
        currencies: [("USD".to_string(), CurrencySpec { exponent: 2 })]
            .into_iter()
            .collect(),
        fx: [("USD/EUR".to_string(), 9_000)].into_iter().collect(),
    };
    let mut rates = BTreeMap::new();
    rates.insert(
        "DE".to_string(),
        TaxRate {
            rate_bps: 1_900,
            inclusive: false,
        },
    );
    rates.insert(
        "JP".to_string(),
        TaxRate {
            rate_bps: 1_000,
            inclusive: true,
        },
    );
    PaymentsConfig {
        provider: ProviderKind::Local,
        catalogue,
        tax: TaxConfig {
            default_jurisdiction: None,
            rates,
        },
        dunning: Default::default(),
        webhook_secret_env: WEBHOOK_ENV.into(),
        vault_key_env: faktor_payments::catalog::default_vault_key_env(),
        stripe: Default::default(),
        mollie: Default::default(),
    }
}

struct Env {
    service: PaymentService,
    clock: Arc<ManualClock>,
    org: OrganizationId,
}

fn open() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqlitePaymentStore::open(&dir.path().join("payment.db")).unwrap());
    let clock = Arc::new(ManualClock::new(T0));
    let secrets = Arc::new(StaticSecrets::new([(
        WEBHOOK_ENV.to_string(),
        WEBHOOK_SECRET.to_string(),
    )]));
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let service = PaymentService::open_local(store, config(), clock_dyn, secrets).unwrap();
    Env {
        service,
        clock,
        org: OrganizationId::try_new("org_final_probe").unwrap(),
    }
}

fn subscribe_request(key: &str) -> SubscribeRequest {
    SubscribeRequest {
        plan_id: "pro".into(),
        interval: BillingInterval::Monthly,
        seats: 3,
        trial_days: None,
        customer_email: "billing@example.com".into(),
        customer_name: "Final Probe".into(),
        idempotency_key: key.into(),
    }
}

fn invoice_request(subscription_id: &str, key: &str) -> IssueInvoiceRequest {
    IssueInvoiceRequest {
        subscription_id: subscription_id.into(),
        usage: vec![faktor_payments::UsageCharge {
            description: "api calls".into(),
            quantity: 7,
            unit_amount_minor: 13,
        }],
        credits_minor: 100,
        discount_minor: 41,
        tax_jurisdiction: Some("DE".into()),
        due_in_days: Some(14),
        idempotency_key: key.into(),
    }
}

#[tokio::test]
async fn exact_invoice_proration_seat_tax_and_currency_math() {
    let Env {
        service,
        clock,
        org,
    } = open();
    let sub = service
        .subscribe(&org, subscribe_request("sub-1"))
        .await
        .unwrap();
    assert!(!sub.replayed);
    assert_eq!(sub.subscription.quantity, 3);
    assert_eq!(sub.subscription.plan_id, "pro");
    assert_eq!(
        sub.subscription.current_period_end_ms - sub.subscription.current_period_start_ms,
        30 * DAY
    );
    let subscription_id = sub.subscription.id.clone();

    // Invoice 1: plan 1000 + seats 3*250=750 + usage 7*13=91 => 1841;
    // discount 41 + credits 100 => taxable 1700; DE 19% exclusive => 323;
    // total 2023. Every amount is exact integer minor units.
    let first = service
        .issue_invoice(&org, invoice_request(&subscription_id, "inv-1"))
        .await
        .unwrap();
    let invoice = &first.invoice;
    assert_eq!(invoice.subtotal_minor, 1_841);
    assert_eq!(invoice.discount_minor, 41);
    assert_eq!(invoice.credits_minor, 100);
    assert_eq!(invoice.tax_minor, 323);
    assert_eq!(invoice.total_minor, 2_023);
    assert!(!invoice.tax_inclusive);
    assert_eq!(invoice.number, "INV-000001");
    let amounts: Vec<(InvoiceLineKind, i64)> = invoice
        .lines
        .iter()
        .map(|line| (line.kind, line.amount_minor))
        .collect();
    assert!(amounts.contains(&(InvoiceLineKind::Plan, 1_000)));
    assert!(amounts.contains(&(InvoiceLineKind::Seat, 750)));
    assert!(amounts.contains(&(InvoiceLineKind::Usage, 91)));
    assert!(amounts.contains(&(InvoiceLineKind::Discount, -41)));
    assert!(amounts.contains(&(InvoiceLineKind::Credits, -100)));
    assert!(amounts.contains(&(InvoiceLineKind::Tax, 323)));
    let sum: i64 = invoice.lines.iter().map(|line| line.amount_minor).sum();
    assert_eq!(sum, invoice.total_minor);

    // Replay returns the SAME invoice and never a second charge.
    let replay = service
        .issue_invoice(&org, invoice_request(&subscription_id, "inv-1"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.invoice.id, invoice.id);
    assert_eq!(service.invoices(&org, None, 10).unwrap().items.len(), 1);

    // Same key, different request => typed idempotency conflict.
    let mut altered = invoice_request(&subscription_id, "inv-1");
    altered.usage[0].quantity = 8;
    assert!(matches!(
        service.issue_invoice(&org, altered).await,
        Err(PaymentError::IdempotencyConflict(_))
    ));

    // Ten days in: remaining 20/30. Seats 3 -> 5 = +500 full, 500*20/30.
    clock.advance(10 * DAY);
    let seat_preview = service
        .preview_seat_change(
            &org,
            PreviewSeatsRequest {
                subscription_id: subscription_id.clone(),
                quantity: 5,
            },
        )
        .unwrap();
    assert_eq!(seat_preview.remaining_ms, 20 * DAY);
    assert_eq!(seat_preview.period_ms, 30 * DAY);
    assert_eq!(seat_preview.old_amount_minor, 750);
    assert_eq!(seat_preview.new_amount_minor, 1_250);
    assert_eq!(seat_preview.charge_minor, 333);
    assert_eq!(seat_preview.credit_minor, 0);
    assert_eq!(seat_preview.net_minor, 333);
    let seats = service
        .change_seats(
            &org,
            ChangeSeatsRequest {
                subscription_id: subscription_id.clone(),
                quantity: 5,
                idempotency_key: "seats-1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(seats.proration, seat_preview);
    assert_eq!(seats.subscription.quantity, 5);

    // Invoice 2 carries the pending seat proration exactly once:
    // plan 1000 + seats 5*250=1250 + proration 333 = 2583, no tax (None).
    let mut second_req = invoice_request(&subscription_id, "inv-2");
    second_req.usage.clear();
    second_req.credits_minor = 0;
    second_req.discount_minor = 0;
    second_req.tax_jurisdiction = None;
    let second = service.issue_invoice(&org, second_req).await.unwrap();
    assert_eq!(second.invoice.subtotal_minor, 2_583);
    assert_eq!(second.invoice.tax_minor, 0);
    assert_eq!(second.invoice.total_minor, 2_583);
    // Invoice 3: the proration was consumed, so only 1000 + 1250 remains.
    let mut third_req = invoice_request(&subscription_id, "inv-3");
    third_req.usage.clear();
    third_req.credits_minor = 0;
    third_req.discount_minor = 0;
    third_req.tax_jurisdiction = None;
    let third = service.issue_invoice(&org, third_req).await.unwrap();
    assert_eq!(third.invoice.subtotal_minor, 2_250);

    // Plan change pro -> team with 5 seats: old recurring 1000 + 5 x 250 =
    // 2250, new recurring 2000 + 5 x 0 = 2000, 20/30 remaining. Both the
    // plan and the seat price difference are prorated.
    let plan_preview = service
        .preview_plan_change(
            &org,
            PreviewPlanRequest {
                subscription_id: subscription_id.clone(),
                plan_id: "team".into(),
                interval: Some(BillingInterval::Monthly),
            },
        )
        .unwrap();
    assert_eq!(plan_preview.credit_minor, -1_500);
    assert_eq!(plan_preview.charge_minor, 1_333);
    assert_eq!(plan_preview.net_minor, -167);
    let changed = service
        .change_plan(
            &org,
            ChangePlanRequest {
                subscription_id: subscription_id.clone(),
                plan_id: "team".into(),
                interval: Some(BillingInterval::Monthly),
                idempotency_key: "plan-1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(changed.proration, plan_preview);
    assert_eq!(changed.subscription.plan_id, "team");
    let mut fourth_req = invoice_request(&subscription_id, "inv-4");
    fourth_req.usage.clear();
    fourth_req.credits_minor = 0;
    fourth_req.discount_minor = 0;
    fourth_req.tax_jurisdiction = None;
    let fourth = service.issue_invoice(&org, fourth_req).await.unwrap();
    // New recurring 2000 + proration net -167 = 1833.
    assert_eq!(fourth.invoice.subtotal_minor, 1_833);

    // Pure money helpers: the exact same arithmetic the service embeds.
    assert_eq!(exclusive_tax(1_700, 1_900).unwrap(), 323);
    assert_eq!(inclusive_tax(1_100, 1_000).unwrap(), 100);
    assert_eq!(inclusive_tax(1_100, 0).unwrap(), 0);
    assert_eq!(convert(1_234, 15_000).unwrap(), 1_851);
    assert_eq!(convert(1_000, 9_000).unwrap(), 900);
    assert_eq!(prorate(1_000, 20 * DAY, 30 * DAY).unwrap(), 667);
    assert_eq!(prorate(500, 20 * DAY, 30 * DAY).unwrap(), 333);
    assert_eq!(prorate(2_000, 20 * DAY, 30 * DAY).unwrap(), 1_333);
    assert_eq!(prorate(0, 20 * DAY, 30 * DAY).unwrap(), 0);
}

#[tokio::test]
async fn webhook_forgery_staleness_and_replay_are_refused() {
    let Env {
        service,
        clock,
        org,
    } = open();
    service
        .subscribe(&org, subscribe_request("sub-wh"))
        .await
        .unwrap();
    let now = clock.now_ms();
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_final_probe_1",
        "type": "probe.noop",
        "created": now / 1000,
        "data": {"object": {"metadata": {"organization_id": org.as_str()}}},
    }))
    .unwrap();
    let header = |secret: &str, ts: i64, payload: &[u8]| {
        WebhookHeaders::from_pairs([(
            "Stripe-Signature",
            &*sign_local_webhook(secret, ts, payload),
        )])
    };
    let ts = now / 1000;
    let good = header(WEBHOOK_SECRET, ts, &body);

    // Valid signature: accepted exactly once; the no-op type is ignored.
    let first = service.handle_webhook(&good, &body, now).await.unwrap();
    assert_eq!(first.disposition, WebhookDisposition::Ignored);
    // Byte-identical replay: recognized as a duplicate, no second effect.
    let replay = service.handle_webhook(&good, &body, now).await.unwrap();
    assert_eq!(replay.disposition, WebhookDisposition::Duplicate);
    assert_eq!(replay.event_id, first.event_id);

    // Tampered body under a valid signature header: refused, no durable row.
    let mut forged_body = body.clone();
    forged_body.push(b' ');
    assert!(matches!(
        service.handle_webhook(&good, &forged_body, now).await,
        Err(PaymentError::WebhookUnauthorized(_))
    ));
    // Wrong secret.
    assert!(matches!(
        service
            .handle_webhook(&header("whsec_wrong", ts, &body), &body, now)
            .await,
        Err(PaymentError::WebhookUnauthorized(_))
    ));
    // Missing signature header.
    assert!(matches!(
        service
            .handle_webhook(
                &WebhookHeaders::from_pairs([("content-type", "application/json")]),
                &body,
                now
            )
            .await,
        Err(PaymentError::WebhookUnauthorized(_))
    ));
    // Signature valid but outside the freshness window.
    let stale = header(WEBHOOK_SECRET, ts - 3_600, &body);
    assert_eq!(
        service.handle_webhook(&stale, &body, now).await,
        Err(PaymentError::WebhookStaleTimestamp)
    );
    // The subscription is untouched by every refused delivery.
    assert!(
        service.overview(&org).unwrap().subscription.is_some(),
        "the original subscription must remain"
    );
    let invoices = service.invoices(&org, None, 10).unwrap();
    assert!(invoices.items.is_empty());
}

#[tokio::test]
async fn provider_unconfigured_is_typed_not_fabricated() {
    // A Stripe config with NO API key resolves the honest unconfigured
    // provider: reads still work, paid operations refuse typed.
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqlitePaymentStore::open(&dir.path().join("payment.db")).unwrap());
    let clock = Arc::new(ManualClock::new(T0));
    let clock_dyn: Arc<dyn Clock> = clock;
    let mut cfg = config();
    cfg.provider = ProviderKind::Stripe;
    cfg.webhook_secret_env = WEBHOOK_ENV.into();
    let secrets = Arc::new(StaticSecrets::default());
    let service = PaymentService::open_stripe(store, cfg, clock_dyn, secrets, None).unwrap();
    assert_eq!(service.provider_kind(), ProviderKind::Stripe);
    let status = service.provider_status();
    assert_eq!(status.state, "unconfigured", "no key => not configured");
    assert!(status.cause.is_some(), "the refusal cause must be explicit");
    let org = OrganizationId::try_new("org_unconfigured").unwrap();
    let err = service
        .subscribe(&org, subscribe_request("sub-unconfigured"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PaymentError::ProviderUnconfigured(_)),
        "a missing provider must refuse typed, never fake a subscription: {err:?}"
    );
    assert!(
        service.invoices(&org, None, 10).unwrap().items.is_empty(),
        "no invoice may be fabricated"
    );
}
