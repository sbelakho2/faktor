//! The Stripe-compatible adapter. Every call executes through the daemon's
//! checked egress transport (`faktor_provider::egress`); no raw reqwest type
//! is named here. Credentials are resolved from the configured environment
//! variable NAMES and are never rendered by `Debug`, errors or logs.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use faktor_provider::egress::{
    execute_raw, EgressError, HttpTransport, RawRequest, RawResponse, ResponseBudget, RouteLabel,
    MAX_RAW_RESPONSE_BYTES,
};

use crate::catalog::{BillingInterval, StripeConfig};
use crate::error::PaymentError;
use crate::model::ProviderStatus;
use crate::provider::{
    PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge,
    ProviderChargeRequest, ProviderCustomer, ProviderCustomerRequest, ProviderInvoice,
    ProviderInvoiceRequest, ProviderRefund, ProviderRefundRequest, ProviderSubscribeRequest,
    ProviderSubscription, VerifiedWebhookEvent, WebhookHeaders,
};
use crate::secrets::SecretString;
use crate::webhook::{verify_webhook, DEFAULT_WEBHOOK_WINDOW_MS};

const PROVIDER: &str = "stripe";
const HTTP_TIMEOUT_MS: u64 = 30_000;

/// The Stripe adapter.
pub struct StripePaymentProvider {
    base_url: String,
    api_key: SecretString,
    webhook_secret: Option<SecretString>,
    user_agent: String,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for StripePaymentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripePaymentProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

impl StripePaymentProvider {
    pub fn new(
        config: &StripeConfig,
        api_key: SecretString,
        webhook_secret: Option<SecretString>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key,
            webhook_secret,
            user_agent: config.user_agent.clone(),
            transport,
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        form: &[(String, String)],
        idempotency_key: Option<&str>,
        route: RouteLabel,
    ) -> Result<Value, PaymentError> {
        let body = encode_form(form);
        let mut request = RawRequest::new(method, format!("{}{}", self.base_url, path));
        request.headers.push((
            "authorization".into(),
            format!("Bearer {}", self.api_key.expose()),
        ));
        request.headers.push((
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
        request
            .headers
            .push(("accept".into(), "application/json".into()));
        request
            .headers
            .push(("user-agent".into(), self.user_agent.clone()));
        if let Some(key) = idempotency_key {
            request
                .headers
                .push(("idempotency-key".into(), key.to_string()));
        }
        if method != "GET" {
            request.body = Some(body.into_bytes());
        } else if let Some(query) = query_suffix(&body) {
            request.url.push('?');
            request.url.push_str(query);
        }
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
                    "stripe egress was denied by the installed destination policy".into(),
                ))
            }
            Ok(Err(e)) => {
                return Err(PaymentError::Provider(format!(
                    "stripe transport failed: {e}"
                )))
            }
            Err(_) => {
                return Err(PaymentError::Provider(format!(
                    "stripe request exceeded the {HTTP_TIMEOUT_MS} ms bound"
                )))
            }
        };
        parse_response(response)
    }
}

#[async_trait]
impl PaymentProvider for StripePaymentProvider {
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
        let value = self
            .request(
                "POST",
                "/v1/customers",
                &[
                    ("email".into(), request.email.clone()),
                    ("name".into(), request.name.clone()),
                    (
                        "metadata[organization_id]".into(),
                        request.organization_id.clone(),
                    ),
                ],
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?;
        Ok(ProviderCustomer {
            id: text(&value, "id")?,
            email: text_or(&value, "email", &request.email),
            name: text_or(&value, "name", &request.name),
        })
    }

    async fn subscribe(
        &self,
        request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let mut form = vec![
            ("customer".into(), request.customer_id.clone()),
            (
                "items[0][price_data][currency]".into(),
                request.currency.to_lowercase(),
            ),
            (
                "items[0][price_data][product_data][name]".into(),
                request.plan_id.clone(),
            ),
            (
                "items[0][price_data][unit_amount]".into(),
                request.unit_amount_minor.to_string(),
            ),
            (
                "items[0][price_data][recurring][interval]".into(),
                match request.interval {
                    BillingInterval::Monthly => "month".to_string(),
                    BillingInterval::Annual => "year".to_string(),
                },
            ),
            ("items[0][quantity]".into(), request.quantity.to_string()),
            (
                "metadata[organization_id]".into(),
                request.organization_id.clone(),
            ),
            ("metadata[plan_id]".into(), request.plan_id.clone()),
            ("proration_behavior".into(), "create_prorations".into()),
        ];
        if let Some(trial_end_ms) = request.trial_end_ms {
            form.push(("trial_end".into(), (trial_end_ms / 1000).to_string()));
        }
        let value = self
            .request(
                "POST",
                "/v1/subscriptions",
                &form,
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?;
        parse_subscription(&value)
    }

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let path = format!(
            "/v1/subscriptions/{}",
            encode_segment(&request.subscription_id)
        );
        let form = vec![
            (
                "items[0][price_data][currency]".into(),
                request.currency.to_lowercase(),
            ),
            (
                "items[0][price_data][product_data][name]".into(),
                request.plan_id.clone(),
            ),
            (
                "items[0][price_data][unit_amount]".into(),
                request.unit_amount_minor.to_string(),
            ),
            (
                "items[0][price_data][recurring][interval]".into(),
                match request.interval {
                    BillingInterval::Monthly => "month".to_string(),
                    BillingInterval::Annual => "year".to_string(),
                },
            ),
            ("items[0][quantity]".into(), request.quantity.to_string()),
            (
                "proration_behavior".into(),
                if request.prorate {
                    "create_prorations".to_string()
                } else {
                    "none".to_string()
                },
            ),
        ];
        let value = self
            .request(
                "POST",
                &path,
                &form,
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?;
        parse_subscription(&value)
    }

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let path = format!(
            "/v1/subscriptions/{}",
            encode_segment(&request.subscription_id)
        );
        let value = if request.at_period_end {
            self.request(
                "POST",
                &path,
                &[("cancel_at_period_end".into(), "true".into())],
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?
        } else {
            self.request(
                "DELETE",
                &path,
                &[],
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?
        };
        parse_subscription(&value)
    }

    async fn create_invoice(
        &self,
        request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, PaymentError> {
        let value = self
            .request(
                "POST",
                "/v1/invoices",
                &[
                    ("customer".into(), request.customer_id.clone()),
                    ("currency".into(), request.currency.to_lowercase()),
                    ("auto_advance".into(), "false".into()),
                    ("collection_method".into(), "send_invoice".into()),
                    ("description".into(), request.description.clone()),
                    (
                        "metadata[organization_id]".into(),
                        request.organization_id.clone(),
                    ),
                    (
                        "metadata[amount_minor]".into(),
                        request.amount_minor.to_string(),
                    ),
                ],
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?;
        Ok(ProviderInvoice {
            id: text(&value, "id")?,
            status: text_or(&value, "status", "open"),
            amount_minor: number_or(&value, "total", request.amount_minor),
            currency: text_or(&value, "currency", &request.currency.to_lowercase()).to_uppercase(),
        })
    }

    async fn charge(&self, request: ProviderChargeRequest) -> Result<ProviderCharge, PaymentError> {
        let value = self
            .request(
                "POST",
                "/v1/payment_intents",
                &[
                    ("amount".into(), request.amount_minor.to_string()),
                    ("currency".into(), request.currency.to_lowercase()),
                    ("customer".into(), request.customer_id.clone()),
                    (
                        "payment_method".into(),
                        request.payment_method_token.expose().to_string(),
                    ),
                    ("confirm".into(), "true".into()),
                    ("off_session".into(), "true".into()),
                    (
                        "metadata[organization_id]".into(),
                        request.organization_id.clone(),
                    ),
                ],
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?;
        let status = text_or(&value, "status", "failed");
        let charge = ProviderCharge {
            id: text(&value, "id")?,
            status: status.clone(),
            amount_minor: number_or(&value, "amount", request.amount_minor),
            currency: text_or(&value, "currency", &request.currency.to_lowercase()).to_uppercase(),
        };
        match status.as_str() {
            "succeeded" | "processing" => Ok(charge),
            "requires_action" => Err(PaymentError::Declined(
                "the payment intent requires additional customer action".into(),
            )),
            other => Err(PaymentError::Declined(format!(
                "stripe payment intent status {other:?}"
            ))),
        }
    }

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError> {
        let value = self
            .request(
                "POST",
                "/v1/refunds",
                &[
                    ("payment_intent".into(), request.provider_payment_id.clone()),
                    ("amount".into(), request.amount_minor.to_string()),
                    ("reason".into(), "requested_by_customer".into()),
                    (
                        "metadata[organization_id]".into(),
                        request.organization_id.clone(),
                    ),
                    ("metadata[reason]".into(), request.reason.clone()),
                ],
                Some(&request.idempotency_key),
                RouteLabel::PaymentProvider,
            )
            .await?;
        Ok(ProviderRefund {
            id: text(&value, "id")?,
            status: text_or(&value, "status", "succeeded"),
            amount_minor: number_or(&value, "amount", request.amount_minor),
            currency: text_or(&value, "currency", &request.currency.to_lowercase()).to_uppercase(),
        })
    }

    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        let Some(secret) = &self.webhook_secret else {
            return Err(PaymentError::ProviderUnconfigured(
                "the Stripe webhook secret is not resolved".into(),
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

fn parse_response(response: RawResponse) -> Result<Value, PaymentError> {
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| {
        PaymentError::Provider(format!(
            "stripe answered HTTP {} with a non-JSON body",
            response.status
        ))
    })?;
    if (200..300).contains(&response.status) {
        return Ok(value);
    }
    let code = value
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let kind = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("no provider message")
        .chars()
        .take(256)
        .collect::<String>();
    if response.status == 402 || code == "card_declined" {
        return Err(PaymentError::Declined(message));
    }
    Err(PaymentError::Provider(format!(
        "stripe refused HTTP {} ({kind}/{code}): {message}",
        response.status
    )))
}

fn parse_subscription(value: &Value) -> Result<ProviderSubscription, PaymentError> {
    let start = number_or(value, "current_period_start", 0);
    let end = number_or(value, "current_period_end", 0);
    let quantity = value
        .pointer("/items/data/0/quantity")
        .and_then(Value::as_u64)
        .or_else(|| value.get("quantity").and_then(Value::as_u64))
        .unwrap_or(1);
    Ok(ProviderSubscription {
        id: text(value, "id")?,
        status: text_or(value, "status", "active"),
        quantity: u32::try_from(quantity).unwrap_or(u32::MAX),
        current_period_start_ms: seconds_to_ms(start),
        current_period_end_ms: seconds_to_ms(end),
        cancel_at_period_end: value
            .get("cancel_at_period_end")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn seconds_to_ms(seconds: i64) -> i64 {
    if seconds.abs() < 10_000_000_000 {
        seconds.saturating_mul(1000)
    } else {
        seconds
    }
}

fn text(value: &Value, field: &str) -> Result<String, PaymentError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= 256)
        .map(str::to_string)
        .ok_or_else(|| PaymentError::Provider(format!("stripe response has no {field}")))
}

fn text_or(value: &Value, field: &str, default: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| default.to_string())
}

fn number_or(value: &Value, field: &str, default: i64) -> i64 {
    value.get(field).and_then(Value::as_i64).unwrap_or(default)
}

fn encode_form(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", encode_segment(name), encode_segment(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn query_suffix(body: &str) -> Option<&str> {
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'[' | b']' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_provider::egress::MockHttpTransport;

    #[test]
    fn form_encoding_is_exact() {
        assert_eq!(encode_segment("a b"), "a+b");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("items[0]"), "items[0]");
    }

    #[tokio::test]
    async fn adapter_uses_the_checked_transport_and_redacts_debug() {
        let transport = Arc::new(MockHttpTransport::new(
            200,
            r#"{"id":"cus_1","email":"a@example.com","name":"A"}"#,
        ));
        let provider = StripePaymentProvider::new(
            &StripeConfig::default(),
            SecretString::new("sk_live_secret"),
            Some(SecretString::new("whsec_secret")),
            transport.clone(),
        );
        assert!(!format!("{provider:?}").contains("sk_live_secret"));
        let customer = provider
            .create_customer(ProviderCustomerRequest {
                organization_id: "org_1".into(),
                idempotency_key: "k".into(),
                email: "a@example.com".into(),
                name: "A".into(),
            })
            .await
            .unwrap();
        assert_eq!(customer.id, "cus_1");
        let seen = transport.requests();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].1.contains("/v1/customers"));
        assert!(seen[0].0.eq_ignore_ascii_case("post"));
    }

    #[tokio::test]
    async fn provider_errors_are_typed_and_never_echo_the_request_body() {
        let transport = Arc::new(MockHttpTransport::new(
            402,
            r#"{"error":{"type":"card_error","code":"card_declined","message":"declined"}}"#,
        ));
        let provider = StripePaymentProvider::new(
            &StripeConfig::default(),
            SecretString::new("sk"),
            None,
            transport,
        );
        let error = provider
            .charge(ProviderChargeRequest {
                organization_id: "org_1".into(),
                idempotency_key: "k".into(),
                customer_id: "cus_1".into(),
                payment_method_token: SecretString::new("pm_secret_token"),
                amount_minor: 100,
                currency: "USD".into(),
                invoice_ref: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, PaymentError::Declined(_)));
        assert!(!error.to_string().contains("pm_secret_token"));
    }
}
