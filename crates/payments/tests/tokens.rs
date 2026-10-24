//! Token-ingestion and at-rest secrecy probes. These tests deliberately try
//! card-like payloads, legacy plaintext rows, tenant swaps and rotation to
//! prove the provider-token-only boundary.

use std::sync::Arc;

use faktor_cloud::{ManualClock, OrganizationId};
use faktor_payments::{
    validate_payment_token, AddPaymentMethodRequest, CurrencySpec, DunningConfig, MollieConfig,
    PaymentService, PaymentTokenVault, PaymentsConfig, PlanPrice, PriceCatalogue, ProviderKind,
    SqlitePaymentStore, StaticSecrets, StripeConfig, TaxConfig,
};
use tempfile::TempDir;

const START: i64 = 1_700_000_000_000;

fn payments_config() -> PaymentsConfig {
    PaymentsConfig {
        provider: ProviderKind::Local,
        catalogue: PriceCatalogue {
            plans: std::collections::BTreeMap::from([(
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
            currencies: std::collections::BTreeMap::from([(
                "USD".to_string(),
                CurrencySpec { exponent: 2 },
            )]),
            fx: Default::default(),
        },
        tax: TaxConfig::default(),
        dunning: DunningConfig::default(),
        webhook_secret_env: "FAKTOR_TEST_WEBHOOK_SECRET".into(),
        vault_key_env: faktor_payments::catalog::default_vault_key_env(),
        stripe: StripeConfig::default(),
        mollie: MollieConfig::default(),
    }
}

fn service(dir: &TempDir) -> (PaymentService, Arc<SqlitePaymentStore>, std::path::PathBuf) {
    let db = dir.path().join("payments.db");
    let store = Arc::new(SqlitePaymentStore::open(&db).expect("store"));
    let service = PaymentService::open_local(
        store.clone(),
        payments_config(),
        Arc::new(ManualClock::new(START)),
        Arc::new(StaticSecrets::default()),
    )
    .expect("service");
    (service, store, db)
}

fn method(token: &str, key: &str) -> AddPaymentMethodRequest {
    AddPaymentMethodRequest {
        token: token.into(),
        brand: "visa".into(),
        last4: "4242".into(),
        exp_month: 12,
        exp_year: 2030,
        set_default: false,
        idempotency_key: key.into(),
    }
}

#[test]
fn strict_token_shape_rejects_pan_at_any_offset_and_oversize() {
    for rejected in [
        "4242424242424242",
        "pm_4242424242424242",
        "pm_4242424242424241",
        "tok_4111111111111111",
        "tok_4242-4242-4242-4242",
        "tok_4242 4242 4242 4242",
        "shop_5555555555554444_v2",
        "4242_4242_4242_4242",
        "tok_card\nnumber",
    ] {
        assert!(
            validate_payment_token(rejected).is_err(),
            "card-like token {rejected:?} must be refused"
        );
    }
    assert!(validate_payment_token("pm_1PxYzAbCdEfGhIjKlMn").is_ok());
    assert!(validate_payment_token("mollie_mandate_7fc0d3a2").is_ok());
    assert!(validate_payment_token("tok_with/slash_and:colon.ok").is_ok());
    let too_long = "a".repeat(faktor_payments::MAX_TOKEN_BYTES + 1);
    assert!(validate_payment_token(&too_long).is_err());
    let exactly_max = "a".repeat(faktor_payments::MAX_TOKEN_BYTES);
    assert!(validate_payment_token(&exactly_max).is_ok());
}

#[test]
fn token_is_vault_sealed_and_never_rendered() {
    let dir = tempfile::tempdir().unwrap();
    let (service, store, db) = service(&dir);
    let org = OrganizationId::try_new("org_1").unwrap();
    let outcome = service
        .add_payment_method(&org, method("pm_opaque_valid_token", "add-1"))
        .unwrap();
    assert_eq!(outcome.payment_method.last4, "4242");
    let view = serde_json::to_string(&outcome.payment_method).unwrap();
    assert!(!view.contains("pm_opaque_valid_token"));
    assert!(!view.contains("token"));

    let raw: String = {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.query_row(
            "SELECT payload FROM payment_method WHERE id = ?1",
            [&outcome.payment_method.id],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(raw.contains("token_ciphertext"));
    assert!(raw.contains("v1:"));
    assert!(!raw.contains("pm_opaque_valid_token"));
    assert!(!raw.contains("\"token\""));

    let vault = PaymentTokenVault::resolve(&StaticSecrets::default(), "ABSENT", Some(db.as_path()))
        .unwrap();
    let stored = store
        .payment_method("org_1", &outcome.payment_method.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        vault
            .open(
                "org_1",
                &outcome.payment_method.id,
                &stored.token_ciphertext
            )
            .unwrap()
            .expose(),
        "pm_opaque_valid_token"
    );
    assert!(vault
        .open(
            "org_2",
            &outcome.payment_method.id,
            &stored.token_ciphertext
        )
        .is_err());
    let views = service.payment_methods(&org).unwrap();
    assert_eq!(views.len(), 1);
    let serialized = serde_json::to_string(&views).unwrap();
    assert!(!serialized.contains("token"));
}

#[test]
fn rotation_replaces_ciphertext_and_removal_deletes_it() {
    let dir = tempfile::tempdir().unwrap();
    let (service, store, db) = service(&dir);
    let org = OrganizationId::try_new("org_1").unwrap();
    let first = service
        .add_payment_method(&org, method("tok_old_valid_value", "add-old"))
        .unwrap();
    let second = service
        .add_payment_method(&org, method("tok_second_valid_value", "add-second"))
        .unwrap();
    assert!(first.payment_method.is_default);

    service
        .rotate_payment_method_token(
            &org,
            &first.payment_method.id,
            "tok_rotated_valid_value",
            "rotate-1",
        )
        .unwrap();
    let vault = PaymentTokenVault::resolve(&StaticSecrets::default(), "ABSENT", Some(db.as_path()))
        .unwrap();
    let stored = store
        .payment_method("org_1", &first.payment_method.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        vault
            .open("org_1", &first.payment_method.id, &stored.token_ciphertext)
            .unwrap()
            .expose(),
        "tok_rotated_valid_value"
    );
    let raw: String = {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.query_row(
            "SELECT payload FROM payment_method WHERE id = ?1",
            [&first.payment_method.id],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(!raw.contains("tok_old_valid_value"));
    assert!(!raw.contains("tok_rotated_valid_value"));

    assert!(service
        .remove_payment_method(&org, &first.payment_method.id, "remove-1")
        .unwrap());
    assert!(store
        .payment_method("org_1", &first.payment_method.id)
        .unwrap()
        .is_none());
    let remaining = service.payment_methods(&org).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, second.payment_method.id);
    assert!(
        remaining[0].is_default,
        "removing the default promotes the oldest survivor"
    );
}

#[test]
fn legacy_plaintext_rows_are_migrated_or_deleted_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("payments.db");
    let store = Arc::new(SqlitePaymentStore::open(&db).unwrap());
    let legacy = serde_json::json!({
        "id": "pm_legacy",
        "organization_id": "org_1",
        "provider": "local",
        "token": "tok_legacy_valid_value",
        "brand": "visa",
        "last4": "4242",
        "exp_month": 12,
        "exp_year": 2030,
        "is_default": true,
        "created_ms": START,
    });
    let pan = serde_json::json!({
        "id": "pm_pan",
        "organization_id": "org_1",
        "provider": "local",
        "token": "4242424242424242",
        "brand": "visa",
        "last4": "4242",
        "exp_month": 12,
        "exp_year": 2030,
        "is_default": false,
        "created_ms": START,
    });
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO payment_method (id, organization_id, is_default, payload) VALUES \
             (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "pm_legacy",
                "org_1",
                1,
                serde_json::to_string(&legacy).unwrap()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO payment_method (id, organization_id, is_default, payload) VALUES \
             (?1, ?2, ?3, ?4)",
            rusqlite::params!["pm_pan", "org_1", 0, serde_json::to_string(&pan).unwrap()],
        )
        .unwrap();
    }
    // The PAN row makes startup fail closed, but only after deleting it.
    let error = PaymentService::open_local(
        store.clone(),
        payments_config(),
        Arc::new(ManualClock::new(START)),
        Arc::new(StaticSecrets::default()),
    )
    .err()
    .expect("a legacy raw PAN must abort startup");
    assert!(error.to_string().contains("raw card numbers"));
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM payment_method WHERE id = 'pm_pan'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "the unmigratable PAN row is deleted");
        let payload: String = conn
            .query_row(
                "SELECT payload FROM payment_method WHERE id = 'pm_legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(payload.contains("token_ciphertext"));
        assert!(!payload.contains("tok_legacy_valid_value"));
        assert!(!payload.contains("\"token\""));
    }
    let vault = PaymentTokenVault::resolve(&StaticSecrets::default(), "ABSENT", Some(db.as_path()))
        .unwrap();
    let stored = store.payment_method("org_1", "pm_legacy").unwrap().unwrap();
    assert_eq!(
        vault
            .open("org_1", "pm_legacy", &stored.token_ciphertext)
            .unwrap()
            .expose(),
        "tok_legacy_valid_value"
    );
}
