//! Production wiring for the Adyen/Stripe provider selection: the exact
//! `build_service` seam the daemon calls must install a configured adapter
//! with checked egress, or the typed unconfigured provider when the named
//! credential is absent. Secrets are supplied by name and never rendered.

use std::collections::BTreeMap;
use std::sync::Arc;

use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::{
    build_service, AdyenConfig, BillingInterval, CurrencySpec, DunningConfig, PaymentError,
    PaymentService, PaymentsConfig, PlanPrice, PriceCatalogue, ProviderKind, SqlitePaymentStore,
    StaticSecrets, StripeConfig, SubscribeRequest, TaxConfig,
};
use faktor_provider::egress::{HttpTransport, PolicyCheckedHttpTransport};

const NOW_MS: i64 = 1_700_000_000_000;
const API_ENV: &str = "FAKTOR_TEST_ADYEN_API_KEY";
const HMAC_ENV: &str = "FAKTOR_TEST_ADYEN_HMAC_KEY";

fn config(provider: ProviderKind) -> PaymentsConfig {
    PaymentsConfig {
        provider,
        catalogue: PriceCatalogue {
            plans: BTreeMap::from([(
                "pro".to_string(),
                PlanPrice {
                    plan_id: "pro".into(),
                    currency: "USD".into(),
                    monthly_minor: 10_000,
                    annual_minor: 100_000,
                    per_seat_monthly_minor: 2_500,
                    per_seat_annual_minor: 25_000,
                },
            )]),
            currencies: BTreeMap::from([("USD".to_string(), CurrencySpec { exponent: 2 })]),
            fx: BTreeMap::new(),
        },
        tax: TaxConfig::default(),
        dunning: DunningConfig::default(),
        webhook_secret_env: "FAKTOR_TEST_WEBHOOK_SECRET".into(),
        vault_key_env: faktor_payments::catalog::default_vault_key_env(),
        stripe: StripeConfig::default(),
        adyen: AdyenConfig {
            base_url: "https://checkout-test.adyen.com/v72".into(),
            api_key_env: API_ENV.into(),
            webhook_hmac_key_env: HMAC_ENV.into(),
            merchant_account: "FaktorWiringMerchant".into(),
            user_agent: "faktor-wiring-test/0.1".into(),
        },
    }
}

fn build(
    secrets: StaticSecrets,
    transport: Option<Arc<dyn HttpTransport>>,
    provider: ProviderKind,
) -> PaymentService {
    let store = Arc::new(SqlitePaymentStore::open_in_memory().unwrap());
    build_service(
        store,
        config(provider),
        Arc::new(ManualClock::new(NOW_MS)),
        transport,
        Arc::new(secrets),
    )
    .unwrap()
}

#[tokio::test]
async fn adyen_is_wired_and_missing_named_credentials_are_typed() {
    let secrets = || {
        StaticSecrets::new([
            (API_ENV.to_string(), "AQE_wiring_api_key".to_string()),
            (HMAC_ENV.to_string(), "ab".repeat(32)),
        ])
    };
    let service = build(
        secrets(),
        Some(Arc::new(PolicyCheckedHttpTransport::permissive())),
        ProviderKind::Adyen,
    );
    assert_eq!(service.provider_kind(), ProviderKind::Adyen);
    assert_eq!(service.provider_status().state, "available");
    assert!(!format!("{:?}", service.provider_status()).contains("AQE_wiring_api_key"));

    let unconfigured = build(
        StaticSecrets::default(),
        Some(Arc::new(PolicyCheckedHttpTransport::permissive())),
        ProviderKind::Adyen,
    );
    assert_eq!(unconfigured.provider_status().state, "unconfigured");
    let org = OrganizationId::try_new("org_wiring").unwrap();
    let error = unconfigured
        .subscribe(
            &org,
            SubscribeRequest {
                plan_id: "pro".into(),
                interval: BillingInterval::Monthly,
                seats: 1,
                trial_days: None,
                customer_email: "wiring@example.invalid".into(),
                customer_name: "Wiring".into(),
                idempotency_key: "wiring-sub".into(),
            },
        )
        .await
        .expect_err("an unconfigured provider must never fabricate a subscription");
    assert!(matches!(error, PaymentError::ProviderUnconfigured(_)));

    // The configured adapter without a transport is also honestly typed.
    let no_transport = build(secrets(), None, ProviderKind::Adyen);
    assert_eq!(no_transport.provider_status().state, "unconfigured");
    assert_eq!(
        build(StaticSecrets::default(), None, ProviderKind::Stripe).provider_kind(),
        ProviderKind::Stripe
    );
}
