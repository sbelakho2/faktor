//! The Adyen Checkout adapter.
//!
//! Every call executes through the daemon's checked egress transport. The
//! API key and webhook HMAC key are resolved by environment-variable NAME
//! and are never rendered by `Debug`, errors or logs.
//!
//! Adyen has no subscription resource: subscriptions are merchant-managed
//! recurring agreements. The adapter therefore models a subscription as a
//! Checkout **payment link** with `recurringProcessingModel=Subscription`:
//! create opens the agreement link, a plan/seat change expires it and opens
//! its replacement, and immediate cancellation expires it. Stored payment
//! tokens are provider tokens (`storedPaymentMethodId`) used for off-session
//! recurring payments; raw card data never reaches this adapter.
//!
//! Webhooks are authenticated with Adyen's in-body HMAC-SHA256 signature
//! over the documented field order. The signature and timestamp are checked
//! before any event is returned to the service.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use faktor_provider::egress::{
    execute_raw, EgressError, HttpTransport, RawRequest, RawResponse, ResponseBudget, RouteLabel,
    MAX_RAW_RESPONSE_BYTES,
};

use crate::catalog::AdyenConfig;
use crate::error::PaymentError;
use crate::model::ProviderStatus;
use crate::provider::{
    PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge,
    ProviderChargeRequest, ProviderCheckout, ProviderCheckoutRequest, ProviderCustomer,
    ProviderCustomerRequest, ProviderInvoice, ProviderInvoiceRequest, ProviderRefund,
    ProviderRefundRequest, ProviderSubscribeRequest, ProviderSubscription, VerifiedWebhookEvent,
    WebhookHeaders,
};
use crate::secrets::SecretString;
use crate::webhook::{constant_time_eq, MAX_WEBHOOK_BODY_BYTES};

const PROVIDER: &str = "adyen";
const HTTP_TIMEOUT_MS: u64 = 30_000;
// Adyen retries notifications with their original eventDate; a five-minute
// Stripe-style window would reject legitimate retries. HMAC already
// authenticates the event and the service dedupes by event id, so accept a
// day of retry skew and reject anything older.
const ADYEN_WEBHOOK_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_PROVIDER_REFERENCE: usize = 256;

/// The Adyen Checkout adapter.
pub struct AdyenPaymentProvider {
    base_url: String,
    api_key: SecretString,
    hmac_key: Option<SecretString>,
    merchant_account: String,
    user_agent: String,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for AdyenPaymentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdyenPaymentProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("hmac_key", &self.hmac_key.as_ref().map(|_| "<redacted>"))
            .field("merchant_account", &self.merchant_account)
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

impl AdyenPaymentProvider {
    pub fn new(
        config: &AdyenConfig,
        api_key: SecretString,
        hmac_key: Option<SecretString>,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, PaymentError> {
        config.validate()?;
        Ok(Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key,
            hmac_key,
            merchant_account: config.merchant_account.clone(),
            user_agent: config.user_agent.clone(),
            transport,
        })
    }

    async fn post(
        &self,
        path: &str,
        body: &Value,
        idempotency_key: &str,
        route: RouteLabel,
    ) -> Result<Value, PaymentError> {
        self.request("POST", path, body, Some(idempotency_key), route)
            .await
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &Value,
        idempotency_key: Option<&str>,
        route: RouteLabel,
    ) -> Result<Value, PaymentError> {
        let encoded = if method == "GET" {
            None
        } else {
            Some(serde_json::to_vec(body).map_err(|e| {
                PaymentError::Malformed(format!("adyen request encoding failed: {e}"))
            })?)
        };
        let mut request = RawRequest::new(method, format!("{}{}", self.base_url, path));
        request
            .headers
            .push(("x-api-key".into(), self.api_key.expose().to_string()));
        request
            .headers
            .push(("accept".into(), "application/json".into()));
        request
            .headers
            .push(("user-agent".into(), self.user_agent.clone()));
        if method != "GET" {
            request
                .headers
                .push(("content-type".into(), "application/json".into()));
        }
        if let Some(key) = idempotency_key {
            request
                .headers
                .push(("idempotency-key".into(), key.to_string()));
        }
        request.body = encoded;
        request.route = Some(route);
        let budget = ResponseBudget::for_timeout(
            Duration::from_millis(HTTP_TIMEOUT_MS),
            MAX_RAW_RESPONSE_BYTES as u64,
        );
        let response = match tokio::time::timeout(
            Duration::from_millis(HTTP_TIMEOUT_MS),
            execute_raw(self.transport.as_ref(), request, &budget),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(EgressError::Denied { .. })) => {
                return Err(PaymentError::Provider(
                    "adyen egress was denied by the installed destination policy".into(),
                ))
            }
            Ok(Err(e)) => {
                return Err(PaymentError::Provider(format!(
                    "adyen transport failed: {e}"
                )))
            }
            Err(_) => {
                return Err(PaymentError::Provider(format!(
                    "adyen request exceeded the {HTTP_TIMEOUT_MS} ms bound"
                )))
            }
        };
        parse_response(response)
    }

    fn shopper_reference(organization_id: &str) -> String {
        use sha2::Digest;
        let digest = Sha256::digest(organization_id.as_bytes());
        let mut hex = String::with_capacity(32);
        for byte in digest.iter().take(16) {
            hex.push_str(&format!("{byte:02x}"));
        }
        format!("faktor-shopper-{hex}")
    }

    fn reference(kind: &str, organization_id: &str, local_id: &str) -> String {
        format!("faktor:{organization_id}:{kind}:{local_id}")
    }

    async fn create_subscription_link(
        &self,
        request: &ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let amount = request
            .unit_amount_minor
            .checked_mul(i64::from(request.quantity))
            .ok_or_else(|| PaymentError::Malformed("adyen subscription amount overflow".into()))?;
        let body = json!({
            "merchantAccount": self.merchant_account,
            "reference": Self::reference("plan", &request.organization_id, &request.plan_id),
            "amount": {"currency": request.currency, "value": amount},
            "shopperReference": request.customer_id,
            "recurringProcessingModel": "Subscription",
            "storePaymentMethod": true,
            "description": format!("{} ({})", request.plan_id, request.interval.as_str()),
        });
        let value = self
            .post(
                "/paymentLinks",
                &body,
                &request.idempotency_key,
                RouteLabel::PaymentProvider,
            )
            .await?;
        let id = text(&value, "id")?;
        let status = text_or(&value, "status", "active");
        if status == "expired" || status == "paid" {
            return Err(PaymentError::Provider(format!(
                "adyen payment link returned unusable status {status:?}"
            )));
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let trialing = request.trial_end_ms.is_some_and(|trial| trial > now_ms);
        Ok(ProviderSubscription {
            id,
            status: if trialing {
                "trialing".into()
            } else {
                "active".into()
            },
            quantity: request.quantity,
            current_period_start_ms: now_ms,
            current_period_end_ms: now_ms.saturating_add(request.interval.period_ms()),
            cancel_at_period_end: false,
            item_id: None,
            product_id: None,
            price_id: None,
        })
    }
}

#[async_trait]
impl PaymentProvider for AdyenPaymentProvider {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus::available(self.name())
    }

    async fn create_customer(
        &self,
        request: ProviderCustomerRequest,
    ) -> Result<ProviderCustomer, PaymentError> {
        // Adyen has no customer object: the shopperReference is the durable
        // identity used on every token/payment call.
        Ok(ProviderCustomer {
            id: Self::shopper_reference(&request.organization_id),
            email: request.email,
            name: request.name,
        })
    }

    async fn subscribe(
        &self,
        request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        self.create_subscription_link(&request).await
    }

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        // Expire the current agreement link, then open the replacement. The
        // returned id is the durable provider subscription identity the
        // service persists.
        self.request(
            "PATCH",
            &format!("/paymentLinks/{}", encode_segment(&request.subscription_id)),
            &json!({"status": "expired"}),
            Some(&format!("{}:expire", request.idempotency_key)),
            RouteLabel::PaymentProvider,
        )
        .await?;
        let subscribe = ProviderSubscribeRequest {
            organization_id: request.organization_id.clone(),
            idempotency_key: format!("{}:relink", request.idempotency_key),
            customer_id: Self::shopper_reference(&request.organization_id),
            plan_id: request.plan_id.clone(),
            interval: request.interval,
            quantity: request.quantity,
            trial_end_ms: None,
            currency: request.currency.clone(),
            unit_amount_minor: request.unit_amount_minor,
        };
        let mut subscription = self.create_subscription_link(&subscribe).await?;
        subscription.quantity = request.quantity;
        Ok(subscription)
    }

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        if request.at_period_end {
            // Merchant-managed agreement: Faktor stops initiating charges at
            // period end; the provider link itself stays valid until then.
            return Ok(ProviderSubscription {
                id: request.subscription_id,
                status: "active".into(),
                quantity: 1,
                current_period_start_ms: now_ms,
                current_period_end_ms: now_ms,
                cancel_at_period_end: true,
                item_id: None,
                product_id: None,
                price_id: None,
            });
        }
        let value = self
            .request(
                "PATCH",
                &format!("/paymentLinks/{}", encode_segment(&request.subscription_id)),
                &json!({"status": "expired"}),
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?;
        Ok(ProviderSubscription {
            id: text_or(&value, "id", &request.subscription_id),
            status: if text_or(&value, "status", "expired") == "expired" {
                "canceled".into()
            } else {
                text_or(&value, "status", "canceled")
            },
            quantity: 1,
            current_period_start_ms: now_ms,
            current_period_end_ms: now_ms,
            cancel_at_period_end: false,
            item_id: None,
            product_id: None,
            price_id: None,
        })
    }

    async fn create_checkout(
        &self,
        request: ProviderCheckoutRequest,
    ) -> Result<ProviderCheckout, PaymentError> {
        let reference = match request.local_invoice_id.as_deref() {
            Some(invoice_id) => Self::reference("invoice", &request.organization_id, invoice_id),
            None => Self::reference(
                "checkout",
                &request.organization_id,
                &request.idempotency_key,
            ),
        };
        let body = json!({
            "merchantAccount": self.merchant_account,
            "reference": reference,
            "amount": {"currency": request.currency, "value": request.amount_minor},
            "shopperReference": request.customer_id,
            "returnUrl": request.success_url,
            "description": request.description,
            "recurringProcessingModel": "Subscription",
            "storePaymentMethod": true,
        });
        let value = self
            .post(
                "/sessions",
                &body,
                &request.idempotency_key,
                RouteLabel::PaymentProvider,
            )
            .await?;
        Ok(ProviderCheckout {
            id: text(&value, "id")?,
            status: "open".into(),
            url: value
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty() && url.len() <= 2048)
                .map(str::to_string),
            amount_minor: request.amount_minor,
            currency: request.currency,
        })
    }

    async fn create_invoice(
        &self,
        request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, PaymentError> {
        // Faktor remains the invoice authority; Adyen only receives the
        // checkout/payment reference. The local provider invoice identity is
        // deterministic and persisted by the service.
        Ok(ProviderInvoice {
            id: format!(
                "adyen-invoice-{}",
                crate::webhook::hmac_sha256_hex(
                    request.organization_id.as_bytes(),
                    request.idempotency_key.as_bytes()
                )
            ),
            status: "open".into(),
            amount_minor: request.amount_minor,
            currency: request.currency.to_uppercase(),
        })
    }

    async fn charge(&self, request: ProviderChargeRequest) -> Result<ProviderCharge, PaymentError> {
        let reference = request
            .invoice_ref
            .as_deref()
            .map(|invoice| Self::reference("invoice", &request.organization_id, invoice))
            .unwrap_or_else(|| Self::reference("charge", &request.organization_id, "adhoc"));
        let body = json!({
            "merchantAccount": self.merchant_account,
            "reference": reference,
            "amount": {"currency": request.currency, "value": request.amount_minor},
            "shopperReference": request.customer_id,
            "paymentMethod": {
                "type": "scheme",
                "storedPaymentMethodId": request.payment_method_token.expose(),
            },
            "shopperInteraction": "Ecommerce",
            "recurringProcessingModel": "Subscription",
        });
        let value = self
            .post(
                "/payments",
                &body,
                &request.idempotency_key,
                RouteLabel::PaymentProvider,
            )
            .await?;
        let result = text_or(&value, "resultCode", "Error");
        let status = match result.as_str() {
            "Authorised" => "succeeded",
            "Received" | "Pending" => "pending",
            "Refused" | "Cancelled" | "Error" => {
                return Err(PaymentError::Declined(format!(
                    "adyen payment result {result:?}"
                )))
            }
            "RedirectShopper" | "ChallengeShopper" | "IdentifyShopper" => "requires_action",
            other => {
                return Err(PaymentError::Provider(format!(
                    "adyen payment returned an unsupported result {other:?}"
                )))
            }
        };
        Ok(ProviderCharge {
            id: text(&value, "pspReference")?,
            status: status.to_string(),
            amount_minor: number_or(&value, "amount/value", request.amount_minor),
            currency: text_or(&value, "amount/currency", &request.currency).to_uppercase(),
        })
    }

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError> {
        let body = json!({
            "merchantAccount": self.merchant_account,
            "paymentPspReference": request.provider_payment_id,
            "reference": Self::reference("refund", &request.organization_id, &request.idempotency_key),
            "amount": {"currency": request.currency, "value": request.amount_minor},
        });
        let value = self
            .post(
                "/refunds",
                &body,
                &request.idempotency_key,
                RouteLabel::PaymentProvider,
            )
            .await?;
        let status = match text_or(&value, "status", "received").as_str() {
            "received" | "pending" => "pending",
            "refunded" | "refundedExternally" => "succeeded",
            "refused" | "failed" => "failed",
            other => {
                return Err(PaymentError::Provider(format!(
                    "adyen refund returned an unsupported status {other:?}"
                )))
            }
        };
        Ok(ProviderRefund {
            id: text(&value, "pspReference")?,
            status: status.to_string(),
            amount_minor: number_or(&value, "amount/value", request.amount_minor),
            currency: text_or(&value, "amount/currency", &request.currency).to_uppercase(),
        })
    }

    fn verify_webhook(
        &self,
        _headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        let Some(secret) = &self.hmac_key else {
            return Err(PaymentError::ProviderUnconfigured(
                "the Adyen webhook HMAC key is not resolved".into(),
            ));
        };
        verify_adyen_webhook(secret.expose(), body, now_ms)
    }
}

fn parse_response(response: RawResponse) -> Result<Value, PaymentError> {
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| {
        PaymentError::Provider(format!(
            "adyen answered HTTP {} with a non-JSON body",
            response.status
        ))
    })?;
    if (200..300).contains(&response.status) {
        return Ok(value);
    }
    let code = value
        .get("errorCode")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let kind = value
        .get("errorType")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no provider message")
        .chars()
        .take(256)
        .collect::<String>();
    if response.status == 402 {
        return Err(PaymentError::Declined(message));
    }
    Err(PaymentError::Provider(format!(
        "adyen refused HTTP {} ({kind}/{code}): {message}",
        response.status
    )))
}

fn text(value: &Value, field: &str) -> Result<String, PaymentError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= MAX_PROVIDER_REFERENCE)
        .map(str::to_string)
        .ok_or_else(|| PaymentError::Provider(format!("adyen response has no {field}")))
}

fn text_or(value: &Value, pointer: &str, default: &str) -> String {
    value
        .pointer(&format!("/{pointer}"))
        .or_else(|| value.pointer(pointer))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= MAX_PROVIDER_REFERENCE)
        .unwrap_or(default)
        .to_string()
}

fn number_or(value: &Value, pointer: &str, default: i64) -> i64 {
    value
        .pointer(&format!("/{}", pointer))
        .and_then(Value::as_i64)
        .unwrap_or(default)
}

fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Verify one Adyen notification batch. The HMAC key is either the raw
/// secret or the 64-hex-character Customer Area key (decoded first).
fn verify_adyen_webhook(
    secret: &str,
    body: &[u8],
    now_ms: i64,
) -> Result<VerifiedWebhookEvent, PaymentError> {
    if secret.is_empty() {
        return Err(PaymentError::ProviderUnconfigured(
            "no Adyen webhook HMAC key is resolved".into(),
        ));
    }
    if body.is_empty() || body.len() > MAX_WEBHOOK_BODY_BYTES {
        return Err(PaymentError::WebhookMalformed(
            "adyen webhook body is empty or exceeds the bound".into(),
        ));
    }
    let root: Value = serde_json::from_slice(body)
        .map_err(|_| PaymentError::WebhookMalformed("adyen webhook body is not JSON".into()))?;
    let items = root
        .get("notificationItems")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            PaymentError::WebhookMalformed("adyen webhook has no notificationItems array".into())
        })?;
    // The service applies exactly one event per delivery; a batch is refused
    // typed instead of silently dropping the tail.
    if items.len() != 1 {
        return Err(PaymentError::WebhookMalformed(
            "adyen webhook must carry exactly one notification item".into(),
        ));
    }
    let item = items[0].get("NotificationRequestItem").unwrap_or(&items[0]);
    let signature = item
        .pointer("/additionalData/hmacSignature")
        .or_else(|| item.get("hmacSignature"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| {
            PaymentError::WebhookUnauthorized("adyen webhook carries no HMAC signature".into())
        })?;
    let event_code = webhook_text(item, "eventCode")?;
    let psp_reference = webhook_text(item, "pspReference")?;
    let original_reference = webhook_text_or(item, "originalReference", "");
    let merchant_account = webhook_text_or(item, "merchantAccountCode", "");
    let merchant_reference = webhook_text_or(item, "merchantReference", "");
    let success_text = webhook_text(item, "success")?;
    let success = match success_text.as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return Err(PaymentError::WebhookMalformed(
                "adyen webhook success must be true or false".into(),
            ))
        }
    };
    let amount = item
        .get("amount")
        .ok_or_else(|| PaymentError::WebhookMalformed("adyen webhook carries no amount".into()))?;
    let amount_value = amount.get("value").and_then(Value::as_i64).ok_or_else(|| {
        PaymentError::WebhookMalformed("adyen webhook amount has no integer value".into())
    })?;
    let currency = amount
        .get("currency")
        .and_then(Value::as_str)
        .filter(|currency| currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()))
        .ok_or_else(|| {
            PaymentError::WebhookMalformed("adyen webhook amount has no currency".into())
        })?;
    let canonical = canonical_signature_string(
        &psp_reference,
        &original_reference,
        &merchant_account,
        &merchant_reference,
        amount_value,
        currency,
        &event_code,
        &success_text,
    );
    let expected = adyen_hmac(secret, canonical.as_bytes());
    if !constant_time_eq(signature.as_bytes(), expected.as_bytes()) {
        return Err(PaymentError::WebhookUnauthorized(
            "adyen webhook HMAC does not match the notification".into(),
        ));
    }
    let event_date = item
        .get("eventDate")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            PaymentError::WebhookMalformed("adyen webhook carries no eventDate".into())
        })?;
    let created_ms = chrono::DateTime::parse_from_rfc3339(event_date)
        .map_err(|_| {
            PaymentError::WebhookMalformed("adyen webhook eventDate is not RFC3339".into())
        })?
        .timestamp_millis();
    if now_ms < 0 || (now_ms - created_ms).abs() > ADYEN_WEBHOOK_WINDOW_MS {
        return Err(PaymentError::WebhookStaleTimestamp);
    }
    let (organization_id, kind, local_id) = parse_reference(&merchant_reference);
    let event_type = match (event_code.as_str(), success) {
        ("AUTHORISATION", true) => "payment.succeeded".to_string(),
        ("AUTHORISATION", false) => "payment.failed".to_string(),
        ("REFUND", true) | ("REFUND_WITH_DATA", true) => "refund.succeeded".to_string(),
        ("REFUND", false) | ("REFUND_FAILED", _) | ("REFUND_NOT_CLEARED", _) => {
            "refund.failed".to_string()
        }
        ("CANCELLATION", _) => "subscription.cancelled".to_string(),
        ("RECURRING_CONTRACT", _) => "subscription.updated".to_string(),
        (other, _) if other.starts_with("recurring.token.") => other.to_string(),
        (other, _) => format!("adyen.{}", other.to_ascii_lowercase()),
    };
    let mut metadata = serde_json::Map::new();
    if let Some(organization) = &organization_id {
        metadata.insert(
            "organization_id".into(),
            Value::String(organization.clone()),
        );
    }
    match kind.as_deref() {
        Some("invoice") => {
            geometry_insert(&mut metadata, "local_invoice_id", &local_id);
        }
        Some("refund") => {
            geometry_insert(&mut metadata, "local_refund_id", &local_id);
        }
        _ => {}
    }
    let normalized = json!({
        "id": format!("{psp_reference}:{event_code}"),
        "type": event_type,
        "created": created_ms,
        "data": {"object": {
            "id": psp_reference,
            "status": if success { "succeeded" } else { "failed" },
            "amount_minor": amount_value,
            "currency": currency,
            "payment_psp_reference": if original_reference.is_empty() {
                psp_reference.clone()
            } else {
                original_reference.clone()
            },
            "merchant_reference": merchant_reference,
            "metadata": metadata,
        }},
        "adyen": {
            "eventCode": event_code,
            "success": success,
            "eventDate": event_date,
        },
    });
    Ok(VerifiedWebhookEvent {
        id: format!("{psp_reference}:{event_code}"),
        event_type,
        created_ms,
        organization_id,
        payload: normalized,
    })
}

fn webhook_text(item: &Value, field: &str) -> Result<String, PaymentError> {
    item.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_PROVIDER_REFERENCE)
        .map(str::to_string)
        .ok_or_else(|| PaymentError::WebhookMalformed(format!("adyen webhook has no {field}")))
}

fn webhook_text_or(item: &Value, field: &str, default: &str) -> String {
    item.get(field)
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_PROVIDER_REFERENCE)
        .unwrap_or(default)
        .to_string()
}

fn parse_reference(reference: &str) -> (Option<String>, Option<String>, String) {
    let mut parts = reference.splitn(4, ':');
    if parts.next() != Some("faktor") {
        return (None, None, String::new());
    }
    let organization = parts.next().map(str::to_string);
    let kind = parts.next().map(str::to_string);
    let local_id = parts.next().unwrap_or_default().to_string();
    (organization, kind, local_id)
}

fn geometry_insert(metadata: &mut serde_json::Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() && value.len() <= MAX_PROVIDER_REFERENCE {
        metadata.insert(key.to_string(), Value::String(value.to_string()));
    }
}

#[allow(clippy::too_many_arguments)]
fn canonical_signature_string(
    psp_reference: &str,
    original_reference: &str,
    merchant_account: &str,
    merchant_reference: &str,
    value: i64,
    currency: &str,
    event_code: &str,
    success: &str,
) -> String {
    [
        psp_reference.to_string(),
        original_reference.to_string(),
        merchant_account.to_string(),
        merchant_reference.to_string(),
        value.to_string(),
        currency.to_string(),
        event_code.to_string(),
        success.to_string(),
    ]
    .iter()
    .map(|field| escape_hmac_field(field))
    .collect::<Vec<_>>()
    .join(":")
}

fn escape_hmac_field(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for ch in field.chars() {
        if ch == '\\' || ch == ':' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn adyen_hmac(secret: &str, payload: &[u8]) -> String {
    let key = decode_hmac_key(secret);
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(payload);
    B64.encode(mac.finalize().into_bytes())
}

fn decode_hmac_key(secret: &str) -> Vec<u8> {
    let trimmed = secret.trim();
    if trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut out = Vec::with_capacity(32);
        for pair in trimmed.as_bytes().as_chunks::<2>().0 {
            if let Ok(text) = std::str::from_utf8(pair) {
                if let Ok(byte) = u8::from_str_radix(text, 16) {
                    out.push(byte);
                }
            }
        }
        if out.len() == 32 {
            return out;
        }
    }
    secret.as_bytes().to_vec()
}

/// Test/operator helper: sign one notification item exactly as Adyen does.
#[allow(clippy::too_many_arguments)]
pub fn sign_adyen_webhook_hmac(
    secret: &str,
    psp_reference: &str,
    original_reference: &str,
    merchant_account: &str,
    merchant_reference: &str,
    value: i64,
    currency: &str,
    event_code: &str,
    success: &str,
) -> String {
    adyen_hmac(
        secret,
        canonical_signature_string(
            psp_reference,
            original_reference,
            merchant_account,
            merchant_reference,
            value,
            currency,
            event_code,
            success,
        )
        .as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_canonical_escaping_is_exact() {
        assert_eq!(escape_hmac_field("a:b\\c"), "a\\:b\\\\c");
        let key = "00".repeat(32);
        let unsigned = sign_adyen_webhook_hmac(
            &key,
            "psp",
            "",
            "Merchant",
            "faktor:org:invoice:inv_1",
            100,
            "USD",
            "AUTHORISATION",
            "true",
        );
        assert_eq!(unsigned.len(), 44);
    }

    #[test]
    fn sign_verify_roundtrip_uses_the_same_canonical_string() {
        let secret = "ab".repeat(32);
        let sig = sign_adyen_webhook_hmac(
            &secret,
            "PSP_payment_1",
            "",
            "FaktorTestMerchant",
            "faktor:org_1:invoice:inv_1",
            10_000,
            "USD",
            "AUTHORISATION",
            "true",
        );
        let _canonical = canonical_signature_string(
            "PSP_payment_1",
            "",
            "FaktorTestMerchant",
            "faktor:org_1:invoice:inv_1",
            10_000,
            "USD",
            "AUTHORISATION",
            "true",
        );
        let body = serde_json::json!({
            "live": false,
            "notificationItems": [{"NotificationRequestItem": {
                "eventCode": "AUTHORISATION",
                "eventDate": "2023-11-14T22:13:20Z",
                "merchantAccountCode": "FaktorTestMerchant",
                "merchantReference": "faktor:org_1:invoice:inv_1",
                "amount": {"currency": "USD", "value": 10_000},
                "pspReference": "PSP_payment_1",
                "originalReference": "",
                "success": "true",
                "additionalData": {"hmacSignature": sig}
            }}]
        })
        .to_string();
        let provider = AdyenPaymentProvider::new(
            &AdyenConfig::default(),
            SecretString::new("key"),
            Some(SecretString::new(secret)),
            Arc::new(faktor_provider::egress::MockHttpTransport::new(200, "{}")),
        )
        .unwrap();
        let event = provider
            .verify_webhook(
                &WebhookHeaders::default(),
                body.as_bytes(),
                1_700_000_000_000,
            )
            .unwrap();
        assert_eq!(event.event_type, "payment.succeeded");
    }

    #[test]
    fn debug_never_renders_credentials() {
        let provider = AdyenPaymentProvider::new(
            &AdyenConfig::default(),
            SecretString::new("AQE_adyen_secret"),
            Some(SecretString::new("hmac_secret_value")),
            Arc::new(faktor_provider::egress::MockHttpTransport::new(200, "{}")),
        )
        .unwrap();
        let debug = format!("{provider:?}");
        assert!(!debug.contains("AQE_adyen_secret"));
        assert!(!debug.contains("hmac_secret_value"));
    }
}
