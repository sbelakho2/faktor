//! The durable LOCAL payment provider: a fully offline, SQLite-backed
//! implementation of [`PaymentProvider`]. It never touches the network, and
//! its objects are idempotent by `(provider, kind, organization,
//! idempotency_key)`, so a retried operation after a crash returns the SAME
//! provider object instead of minting a second one.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};

use faktor_cloud::{Clock, SystemClock};

use crate::catalog::BillingInterval;
use crate::error::PaymentError;
use crate::model::ProviderStatus;
use crate::provider::{
    PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge,
    ProviderChargeRequest, ProviderCustomer, ProviderCustomerRequest, ProviderInvoice,
    ProviderInvoiceRequest, ProviderRefund, ProviderRefundRequest, ProviderSubscribeRequest,
    ProviderSubscription, VerifiedWebhookEvent, WebhookHeaders,
};
use crate::secrets::SecretString;
use crate::store::SqlitePaymentStore;
use crate::webhook::{verify_webhook, DEFAULT_WEBHOOK_WINDOW_MS};

const PROVIDER: &str = "local";

/// The local provider.
pub struct LocalPaymentProvider {
    store: Arc<SqlitePaymentStore>,
    webhook_secret: Option<SecretString>,
    clock: Arc<dyn Clock>,
    name: &'static str,
}

impl LocalPaymentProvider {
    pub fn new(
        store: Arc<SqlitePaymentStore>,
        webhook_secret: Option<SecretString>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            webhook_secret,
            clock,
            name: PROVIDER,
        }
    }

    /// A local provider over the wall clock (hosts that do not inject one).
    pub fn with_system_clock(
        store: Arc<SqlitePaymentStore>,
        webhook_secret: Option<SecretString>,
    ) -> Self {
        Self::new(store, webhook_secret, Arc::new(SystemClock))
    }

    pub fn store(&self) -> &Arc<SqlitePaymentStore> {
        &self.store
    }

    fn deterministic_id(prefix: &str, organization_id: &str, kind: &str, key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(organization_id.as_bytes());
        hasher.update([0]);
        hasher.update(kind.as_bytes());
        hasher.update([0]);
        hasher.update(key.as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("{prefix}_{hex}")
    }

    fn object<T: serde::de::DeserializeOwned + serde::Serialize>(
        &self,
        kind: &str,
        organization_id: &str,
        idempotency_key: &str,
        id_prefix: &str,
        build: impl FnOnce(String) -> T,
    ) -> Result<T, PaymentError> {
        if let Some((_id, payload)) =
            self.store
                .provider_object(PROVIDER, kind, organization_id, idempotency_key)?
        {
            let mut value = payload;
            if let Some(object) = value.get_mut("object") {
                return serde_json::from_value(object.take()).map_err(|e| {
                    PaymentError::Store(format!("local provider object decoding: {e}"))
                });
            }
            return serde_json::from_value(value)
                .map_err(|e| PaymentError::Store(format!("local provider object decoding: {e}")));
        }
        let id = if idempotency_key.is_empty() {
            format!("{id_prefix}_{}", uuid::Uuid::new_v4())
        } else {
            Self::deterministic_id(id_prefix, organization_id, kind, idempotency_key)
        };
        let object = build(id);
        let encoded = serde_json::to_value(&object)
            .map_err(|e| PaymentError::Store(format!("local provider encoding: {e}")))?;
        self.store.put_provider_object(
            PROVIDER,
            kind,
            organization_id,
            idempotency_key,
            &object_id(&encoded),
            &json!({"object": encoded}),
        )?;
        Ok(object)
    }
}

fn object_id(value: &serde_json::Value) -> String {
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("local_object")
        .to_string()
}

#[async_trait]
impl PaymentProvider for LocalPaymentProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus::available(self.name)
    }

    async fn create_customer(
        &self,
        request: ProviderCustomerRequest,
    ) -> Result<ProviderCustomer, PaymentError> {
        self.object(
            "customer",
            &request.organization_id,
            &request.idempotency_key,
            "cus",
            |id| ProviderCustomer {
                id,
                email: request.email,
                name: request.name,
            },
        )
    }

    async fn subscribe(
        &self,
        request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let period = request.interval.period_ms();
        let now = self.clock.now_ms();
        let start = now;
        let end = now.saturating_add(period);
        let subscription = self.object(
            "subscription",
            &request.organization_id,
            &request.idempotency_key,
            "sub",
            |id| ProviderSubscription {
                id,
                status: if request.trial_end_ms.is_some_and(|trial| trial > now) {
                    "trialing".into()
                } else {
                    "active".into()
                },
                quantity: request.quantity,
                current_period_start_ms: start,
                current_period_end_ms: end,
                cancel_at_period_end: false,
            },
        )?;
        let encoded = serde_json::to_value(&subscription)
            .map_err(|e| PaymentError::Store(format!("local subscription encode: {e}")))?;
        self.store.put_provider_object(
            PROVIDER,
            "subscription_by_id",
            &request.organization_id,
            &subscription.id,
            &subscription.id,
            &json!({"object": encoded}),
        )?;
        Ok(subscription)
    }

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let existing = self
            .store
            .provider_object(
                PROVIDER,
                "subscription_by_id",
                &request.organization_id,
                &request.subscription_id,
            )?
            .map(|(_, payload)| payload);
        let mut subscription = if let Some(payload) = existing {
            let object = payload.get("object").cloned().unwrap_or(payload);
            serde_json::from_value::<ProviderSubscription>(object)
                .map_err(|e| PaymentError::Store(format!("local subscription decode: {e}")))?
        } else {
            ProviderSubscription {
                id: request.subscription_id.clone(),
                status: "active".into(),
                quantity: request.quantity,
                current_period_start_ms: self.clock.now_ms(),
                current_period_end_ms: self
                    .clock
                    .now_ms()
                    .saturating_add(request.interval.period_ms()),
                cancel_at_period_end: false,
            }
        };
        subscription.quantity = request.quantity;
        subscription.status = "active".into();
        let encoded = serde_json::to_value(&subscription)
            .map_err(|e| PaymentError::Store(format!("local subscription encode: {e}")))?;
        self.store.put_provider_object(
            PROVIDER,
            "change",
            &request.organization_id,
            &request.idempotency_key,
            &subscription.id,
            &json!({"object": encoded.clone()}),
        )?;
        self.store.put_provider_object(
            PROVIDER,
            "subscription_by_id",
            &request.organization_id,
            &subscription.id,
            &subscription.id,
            &json!({"object": encoded}),
        )?;
        Ok(subscription)
    }

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let existing = self
            .store
            .provider_object(
                PROVIDER,
                "subscription_by_id",
                &request.organization_id,
                &request.subscription_id,
            )?
            .map(|(_, payload)| payload);
        let mut subscription = existing
            .and_then(|payload| payload.get("object").cloned().or(Some(payload)))
            .and_then(|object| serde_json::from_value::<ProviderSubscription>(object).ok())
            .unwrap_or(ProviderSubscription {
                id: request.subscription_id.clone(),
                status: "active".into(),
                quantity: 1,
                current_period_start_ms: self.clock.now_ms(),
                current_period_end_ms: self.clock.now_ms(),
                cancel_at_period_end: false,
            });
        if request.at_period_end {
            subscription.cancel_at_period_end = true;
        } else {
            subscription.status = "canceled".into();
        }
        let encoded = serde_json::to_value(&subscription)
            .map_err(|e| PaymentError::Store(format!("local subscription encode: {e}")))?;
        self.store.put_provider_object(
            PROVIDER,
            "cancel",
            &request.organization_id,
            &request.idempotency_key,
            &subscription.id,
            &json!({"object": encoded.clone()}),
        )?;
        self.store.put_provider_object(
            PROVIDER,
            "subscription_by_id",
            &request.organization_id,
            &subscription.id,
            &subscription.id,
            &json!({"object": encoded}),
        )?;
        Ok(subscription)
    }

    async fn create_invoice(
        &self,
        request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, PaymentError> {
        self.object(
            "invoice",
            &request.organization_id,
            &request.idempotency_key,
            "in",
            |id| ProviderInvoice {
                id,
                status: "open".into(),
                amount_minor: request.amount_minor,
                currency: request.currency,
            },
        )
    }

    async fn charge(&self, request: ProviderChargeRequest) -> Result<ProviderCharge, PaymentError> {
        if request.payment_method_token.expose().contains("decline") {
            return Err(PaymentError::Declined(
                "the local test token requests a decline".into(),
            ));
        }
        self.object(
            "charge",
            &request.organization_id,
            &request.idempotency_key,
            "pi",
            |id| ProviderCharge {
                id,
                status: "succeeded".into(),
                amount_minor: request.amount_minor,
                currency: request.currency,
            },
        )
    }

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError> {
        self.object(
            "refund",
            &request.organization_id,
            &request.idempotency_key,
            "re",
            |id| ProviderRefund {
                id,
                status: "succeeded".into(),
                amount_minor: request.amount_minor,
                currency: request.currency,
            },
        )
    }

    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        let Some(secret) = &self.webhook_secret else {
            return Err(PaymentError::ProviderUnconfigured(
                "the local webhook secret is not resolved".into(),
            ));
        };
        verify_webhook(
            secret.expose(),
            headers,
            body,
            now_ms,
            DEFAULT_WEBHOOK_WINDOW_MS,
        )
    }
}

/// Signature helper for local operator/test deliveries.
pub fn sign_local_webhook(secret: &str, timestamp_secs: i64, body: &[u8]) -> String {
    crate::webhook::sign_webhook_payload(secret.as_bytes(), timestamp_secs, body)
}

/// The interval period used by the local provider.
pub fn local_period_ms(interval: BillingInterval) -> i64 {
    interval.period_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_provider_is_idempotent_and_offline() {
        let store = Arc::new(SqlitePaymentStore::open_in_memory().unwrap());
        let provider = LocalPaymentProvider::new(store, None, Arc::new(SystemClock));
        let customer = provider
            .create_customer(ProviderCustomerRequest {
                organization_id: "org_1".into(),
                idempotency_key: "k1".into(),
                email: "a@example.com".into(),
                name: "A".into(),
            })
            .await
            .unwrap();
        let again = provider
            .create_customer(ProviderCustomerRequest {
                organization_id: "org_1".into(),
                idempotency_key: "k1".into(),
                email: "a@example.com".into(),
                name: "A".into(),
            })
            .await
            .unwrap();
        assert_eq!(customer.id, again.id);
        assert!(!provider
            .create_customer(ProviderCustomerRequest {
                organization_id: "org_2".into(),
                idempotency_key: "k1".into(),
                email: "b@example.com".into(),
                name: "B".into(),
            })
            .await
            .unwrap()
            .id
            .is_empty());
        let declined = provider
            .charge(ProviderChargeRequest {
                organization_id: "org_1".into(),
                idempotency_key: "charge".into(),
                customer_id: customer.id,
                payment_method_token: crate::secrets::SecretString::new("tok_decline"),
                amount_minor: 100,
                currency: "USD".into(),
                invoice_ref: None,
            })
            .await;
        assert!(matches!(declined, Err(PaymentError::Declined(_))));
        assert_eq!(
            provider.verify_webhook(&WebhookHeaders::default(), b"{}", 0),
            Err(PaymentError::ProviderUnconfigured(
                "the local webhook secret is not resolved".into()
            ))
        );
    }
}
