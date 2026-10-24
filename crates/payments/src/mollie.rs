//! The Mollie Payments adapter.
//!
//! Every call executes through the daemon's checked egress transport. The API
//! key is resolved by environment-variable NAME (`FAKTOR_MOLLIE_API_KEY` by
//! default) and is never rendered by `Debug`, errors or logs. The base URL is
//! configurable so the same adapter runs against Mollie test mode
//! (`test_...` credential, `https://api.mollie.com/v2`) or a mocked server.
//!
//! Mollie does not accept Faktor's idempotency keys, so no `Idempotency-Key`
//! header is sent: duplicate protection lives in the service's durable
//! operation claim/fence and in an authoritative status re-fetch, never in a
//! provider-side header.
//!
//! Webhooks are unauthenticated and carry only an object id. The adapter
//! never trusts the posted payload: it fetches the resource from Mollie,
//! checks the exact id, the resource type and the Faktor metadata markers
//! written at creation (`organization_id`, `faktor_provider`,
//! `faktor_kind`), and only then returns an event. Refunds and subscriptions
//! need a parent id, supplied by the service's durable local binding.
//!
//! Subscriptions use Mollie's customer/subscription/mandate resources.
//! Cancellation at period end is Faktor-managed (Mollie has no scheduled
//! cancel); immediate cancellation deletes the subscription.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use faktor_cloud::OrganizationId;
use faktor_provider::egress::{
    execute_raw, EgressError, HttpTransport, RawRequest, RawResponse, ResponseBudget, RouteLabel,
    MAX_RAW_RESPONSE_BYTES,
};

use crate::catalog::{BillingInterval, MollieConfig};
use crate::error::PaymentError;
use crate::model::ProviderStatus;
use crate::money::{default_exponent, format_minor, valid_amount, valid_currency};
use crate::provider::{
    PaymentProvider, ProviderCancelRequest, ProviderChangePlanRequest, ProviderCharge,
    ProviderChargeRequest, ProviderCheckout, ProviderCheckoutRequest, ProviderCustomer,
    ProviderCustomerRequest, ProviderCustomerUpdate, ProviderInvoice, ProviderInvoiceRequest,
    ProviderMandate, ProviderMandateRequest, ProviderPaymentLink, ProviderPaymentLinkRequest,
    ProviderPaymentSnapshot, ProviderRefund, ProviderRefundRequest, ProviderSubscribeRequest,
    ProviderSubscription, VerifiedWebhookEvent, WebhookBinding, WebhookBindingLookup,
    WebhookHeaders,
};
use crate::secrets::SecretString;
use crate::webhook::MAX_WEBHOOK_BODY_BYTES;

const PROVIDER: &str = "mollie";
const HTTP_TIMEOUT_MS: u64 = 30_000;
const MAX_PROVIDER_REFERENCE: usize = 256;
const MAX_WEBHOOK_ID: usize = 128;

/// The parsed, still-untrusted reference of one Mollie webhook delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MollieWebhookKind {
    Payment,
    Refund,
    Subscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MollieWebhookRef {
    pub id: String,
    pub kind: MollieWebhookKind,
}

/// The Mollie Payments adapter.
pub struct MolliePaymentProvider {
    base_url: String,
    api_key: SecretString,
    webhook_url: Option<String>,
    user_agent: String,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for MolliePaymentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MolliePaymentProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("webhook_url", &self.webhook_url)
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

impl MolliePaymentProvider {
    pub fn new(
        config: &MollieConfig,
        api_key: SecretString,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, PaymentError> {
        config.validate()?;
        if api_key.is_empty() {
            return Err(PaymentError::ProviderUnconfigured(
                "the Mollie API key is empty".into(),
            ));
        }
        Ok(Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key,
            webhook_url: config.webhook_url.clone(),
            user_agent: config.user_agent.clone(),
            transport,
        })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        route: RouteLabel,
    ) -> Result<Value, PaymentError> {
        let encoded = match (method, body) {
            ("GET", _) | ("DELETE", None) => None,
            (_, Some(body)) => Some(serde_json::to_vec(body).map_err(|e| {
                PaymentError::Malformed(format!("mollie request encoding failed: {e}"))
            })?),
            (_, None) => None,
        };
        let mut request = RawRequest::new(method, format!("{}{}", self.base_url, path));
        request.headers.push((
            "authorization".into(),
            format!("Bearer {}", self.api_key.expose()),
        ));
        request
            .headers
            .push(("accept".into(), "application/hal+json".into()));
        request
            .headers
            .push(("user-agent".into(), self.user_agent.clone()));
        if encoded.is_some() {
            request
                .headers
                .push(("content-type".into(), "application/json".into()));
        }
        // Mollie does not honour Faktor's `Idempotency-Key`; the durable
        // claim/fence in the service owns duplicate suppression.
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
                    "mollie egress was denied by the installed destination policy".into(),
                ))
            }
            Ok(Err(e)) => {
                return Err(PaymentError::Provider(format!(
                    "mollie transport failed: {e}"
                )))
            }
            Err(_) => {
                return Err(PaymentError::Provider(format!(
                    "mollie request exceeded the {HTTP_TIMEOUT_MS} ms bound"
                )))
            }
        };
        parse_response(response)
    }

    async fn payment_resource(&self, payment_id: &str) -> Result<Value, PaymentError> {
        let value = self
            .request(
                "GET",
                &format!("/payments/{}", encode_segment(payment_id)),
                None,
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "payment", "tr_")?;
        if text(&value, "id")? != payment_id {
            return Err(PaymentError::Provider(
                "mollie returned a different payment id than requested".into(),
            ));
        }
        Ok(value)
    }

    async fn refund_resource(
        &self,
        provider_payment_id: &str,
        refund_id: &str,
    ) -> Result<Value, PaymentError> {
        let value = self
            .request(
                "GET",
                &format!(
                    "/payments/{}/refunds/{}",
                    encode_segment(provider_payment_id),
                    encode_segment(refund_id)
                ),
                None,
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "refund", "re_")?;
        if text(&value, "id")? != refund_id {
            return Err(PaymentError::Provider(
                "mollie returned a different refund id than requested".into(),
            ));
        }
        if let Some(payment_id) = value.get("paymentId").and_then(Value::as_str) {
            if payment_id != provider_payment_id {
                return Err(PaymentError::Provider(
                    "mollie refund does not belong to the bound payment".into(),
                ));
            }
        }
        Ok(value)
    }

    async fn subscription_resource(
        &self,
        customer_id: &str,
        subscription_id: &str,
    ) -> Result<Value, PaymentError> {
        let value = self
            .request(
                "GET",
                &format!(
                    "/customers/{}/subscriptions/{}",
                    encode_segment(customer_id),
                    encode_segment(subscription_id)
                ),
                None,
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "subscription", "sub_")?;
        if text(&value, "id")? != subscription_id {
            return Err(PaymentError::Provider(
                "mollie returned a different subscription id than requested".into(),
            ));
        }
        if let Some(customer) = value.get("customerId").and_then(Value::as_str) {
            if customer != customer_id {
                return Err(PaymentError::Provider(
                    "mollie subscription does not belong to the bound customer".into(),
                ));
            }
        }
        Ok(value)
    }

    async fn mandate_resource(
        &self,
        customer_id: &str,
        mandate_id: &str,
    ) -> Result<Value, PaymentError> {
        let value = self
            .request(
                "GET",
                &format!(
                    "/customers/{}/mandates/{}",
                    encode_segment(customer_id),
                    encode_segment(mandate_id)
                ),
                None,
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "mandate", "mdt_")?;
        if text(&value, "id")? != mandate_id {
            return Err(PaymentError::Provider(
                "mollie returned a different mandate id than requested".into(),
            ));
        }
        Ok(value)
    }

    /// Read-only payment fetch. The returned metadata is provider data; the
    /// caller must bind it before use.
    pub async fn get_payment(
        &self,
        provider_payment_id: &str,
    ) -> Result<ProviderPaymentSnapshot, PaymentError> {
        let value = self.payment_resource(provider_payment_id).await?;
        let (amount_minor, currency) = parse_amount(
            value
                .get("amount")
                .ok_or_else(|| PaymentError::Provider("mollie payment has no amount".into()))?,
        )?;
        Ok(ProviderPaymentSnapshot {
            id: text(&value, "id")?,
            status: text(&value, "status")?,
            amount_minor,
            currency,
            customer_id: value
                .get("customerId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= MAX_PROVIDER_REFERENCE)
                .map(str::to_string),
            mandate_id: value
                .get("mandateId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= MAX_PROVIDER_REFERENCE)
                .map(str::to_string),
            metadata: value.get("metadata").cloned().unwrap_or(Value::Null),
        })
    }

    /// Read-only refund fetch.
    pub async fn get_refund(
        &self,
        provider_payment_id: &str,
        provider_refund_id: &str,
    ) -> Result<ProviderRefund, PaymentError> {
        let value = self
            .refund_resource(provider_payment_id, provider_refund_id)
            .await?;
        let (amount_minor, currency) = parse_amount(
            value
                .get("amount")
                .ok_or_else(|| PaymentError::Provider("mollie refund has no amount".into()))?,
        )?;
        Ok(ProviderRefund {
            id: text(&value, "id")?,
            status: normalize_refund_status(&text(&value, "status")?)?.to_string(),
            amount_minor,
            currency,
        })
    }

    fn require_webhook_url(&self, request: Option<&str>) -> Result<String, PaymentError> {
        request
            .map(str::to_string)
            .or_else(|| self.webhook_url.clone())
            .ok_or_else(|| {
                PaymentError::Malformed(
                    "mollie requires a webhook URL so payment outcomes are fetched authoritatively"
                        .into(),
                )
            })
    }

    fn payment_event(
        &self,
        value: &Value,
        expected_org: Option<&str>,
        now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        let id = text(value, "id")?;
        let status = text(value, "status")?;
        let organization = verify_binding(value.get("metadata"), "payment", expected_org)?;
        let (amount_minor, currency) = parse_amount(value.get("amount").ok_or_else(|| {
            PaymentError::WebhookMalformed("mollie payment has no amount".into())
        })?)?;
        let local_invoice = metadata_text(value.get("metadata"), "local_invoice_id");
        let event_type = match status.as_str() {
            "paid" => "payment.succeeded",
            "failed" | "canceled" | "expired" => {
                if local_invoice.is_some() {
                    "invoice.payment_failed"
                } else {
                    "payment.failed"
                }
            }
            "pending" | "open" => "payment.pending",
            "authorized" => "payment.processing",
            other => {
                return Err(PaymentError::WebhookMalformed(format!(
                    "mollie payment carries unknown status {other:?}"
                )))
            }
        };
        let event_id = format!("{PROVIDER}:payment:{id}:{status}");
        let metadata = bounded_metadata(value.get("metadata"));
        let mut object = json!({
            "id": id,
            "status": status,
            "amount_minor": amount_minor,
            "amount": amount_minor,
            "amount_paid": if status == "paid" { amount_minor } else { 0 },
            "amount_refunded": 0,
            "currency": currency,
            "metadata": metadata,
        });
        if let Some(customer) = value.get("customerId").and_then(Value::as_str) {
            if !customer.is_empty() && customer.len() <= MAX_PROVIDER_REFERENCE {
                object["customer"] = Value::String(customer.to_string());
            }
        }
        Ok(VerifiedWebhookEvent {
            id: event_id.clone(),
            event_type: event_type.to_string(),
            created_ms: now_ms,
            organization_id: Some(organization),
            payload: json!({
                "id": event_id.clone(),
                "type": event_type,
                "created": now_ms,
                "data": {"object": object},
                "mollie": {"resource": "payment", "id": id, "status": status},
            }),
        })
    }

    fn refund_event(
        &self,
        value: &Value,
        expected_org: &str,
        now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        let id = text(value, "id")?;
        let raw_status = text(value, "status")?;
        let status = normalize_refund_status(&raw_status)?;
        let organization = verify_binding(value.get("metadata"), "refund", Some(expected_org))?;
        let (amount_minor, currency) = parse_amount(value.get("amount").ok_or_else(|| {
            PaymentError::WebhookMalformed("mollie refund has no amount".into())
        })?)?;
        let event_type = match status {
            "succeeded" => "refund.succeeded",
            "failed" => "refund.failed",
            _ => "refund.updated",
        };
        let event_id = format!("{PROVIDER}:refund:{id}:{status}");
        let object = json!({
            "id": id,
            "status": status,
            "amount_minor": amount_minor,
            "amount": amount_minor,
            "currency": currency,
            "metadata": bounded_metadata(value.get("metadata")),
            "customer": value.get("customerId").and_then(Value::as_str),
        });
        Ok(VerifiedWebhookEvent {
            id: event_id.clone(),
            event_type: event_type.to_string(),
            created_ms: now_ms,
            organization_id: Some(organization),
            payload: json!({
                "id": event_id.clone(),
                "type": event_type,
                "created": now_ms,
                "data": {"object": object},
                "mollie": {"resource": "refund", "id": id, "status": raw_status},
            }),
        })
    }

    fn subscription_event(
        &self,
        value: &Value,
        expected_org: &str,
        now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        let id = text(value, "id")?;
        let raw_status = text(value, "status")?;
        let organization =
            verify_binding(value.get("metadata"), "subscription", Some(expected_org))?;
        let (status, event_type) = match raw_status.as_str() {
            "active" => ("active", "customer.subscription.updated"),
            "pending" => ("pending", "customer.subscription.updated"),
            "suspended" => ("past_due", "customer.subscription.updated"),
            "canceled" | "completed" | "expired" => ("canceled", "customer.subscription.deleted"),
            other => {
                return Err(PaymentError::WebhookMalformed(format!(
                    "mollie subscription carries unknown status {other:?}"
                )))
            }
        };
        let event_id = format!("{PROVIDER}:subscription:{id}:{status}");
        let object = json!({
            "id": id,
            "status": status,
            "metadata": bounded_metadata(value.get("metadata")),
            "customer": value.get("customerId").and_then(Value::as_str),
        });
        Ok(VerifiedWebhookEvent {
            id: event_id.clone(),
            event_type: event_type.to_string(),
            created_ms: now_ms,
            organization_id: Some(organization),
            payload: json!({
                "id": event_id.clone(),
                "type": event_type,
                "created": now_ms,
                "data": {"object": object},
                "mollie": {"resource": "subscription", "id": id, "status": raw_status},
            }),
        })
    }
}

#[async_trait]
impl PaymentProvider for MolliePaymentProvider {
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
                "/customers",
                Some(&json!({
                    "name": request.name,
                    "email": request.email,
                    "metadata": binding_metadata(&request.organization_id, "customer", None),
                })),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "customer", "cst_")?;
        Ok(ProviderCustomer {
            id: text(&value, "id")?,
            email: text_or(&value, "email", &request.email),
            name: text_or(&value, "name", &request.name),
        })
    }

    async fn update_customer(
        &self,
        request: ProviderCustomerUpdate,
    ) -> Result<ProviderCustomer, PaymentError> {
        let mut body = json!({
            "metadata": binding_metadata(&request.organization_id, "customer", None),
        });
        if let Some(email) = request.email {
            body["email"] = Value::String(email);
        }
        if let Some(name) = request.name {
            body["name"] = Value::String(name);
        }
        let value = self
            .request(
                "PATCH",
                &format!("/customers/{}", encode_segment(&request.customer_id)),
                Some(&body),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "customer", "cst_")?;
        if text(&value, "id")? != request.customer_id {
            return Err(PaymentError::Provider(
                "mollie returned a different customer id than requested".into(),
            ));
        }
        Ok(ProviderCustomer {
            id: text(&value, "id")?,
            email: text_or(&value, "email", ""),
            name: text_or(&value, "name", ""),
        })
    }

    async fn delete_customer(
        &self,
        _organization_id: &str,
        customer_id: &str,
    ) -> Result<(), PaymentError> {
        self.request(
            "DELETE",
            &format!("/customers/{}", encode_segment(customer_id)),
            None,
            RouteLabel::PaymentProvider,
        )
        .await?;
        Ok(())
    }

    async fn subscribe(
        &self,
        request: ProviderSubscribeRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        let amount = request
            .unit_amount_minor
            .checked_mul(i64::from(request.quantity))
            .ok_or_else(|| PaymentError::Malformed("mollie subscription amount overflow".into()))?;
        let mut body = json!({
            "amount": amount_body(&request.currency, amount)?,
            "interval": interval_name(request.interval),
            "description": format!("{} ({})", request.plan_id, request.interval.as_str()),
            "metadata": binding_metadata(&request.organization_id, "subscription", None),
        });
        if let Some(webhook) = &self.webhook_url {
            body["webhookUrl"] = Value::String(webhook.clone());
        }
        if let Some(trial_end_ms) = request.trial_end_ms {
            if trial_end_ms > chrono::Utc::now().timestamp_millis() {
                if let Some(date) =
                    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(trial_end_ms)
                {
                    body["startDate"] = Value::String(date.format("%Y-%m-%d").to_string());
                }
            }
        }
        let value = self
            .request(
                "POST",
                &format!(
                    "/customers/{}/subscriptions",
                    encode_segment(&request.customer_id)
                ),
                Some(&body),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "subscription", "sub_")?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        Ok(ProviderSubscription {
            id: text(&value, "id")?,
            status: subscription_status(&text(&value, "status")?, request.trial_end_ms, now_ms)?,
            quantity: request.quantity,
            current_period_start_ms: now_ms,
            current_period_end_ms: now_ms.saturating_add(request.interval.period_ms()),
            cancel_at_period_end: false,
            item_id: None,
            product_id: None,
            price_id: None,
        })
    }

    async fn change_plan(
        &self,
        request: ProviderChangePlanRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        if request.customer_id.is_empty() {
            return Err(PaymentError::Provider(
                "mollie subscription has no durable customer id".into(),
            ));
        }
        let amount = request
            .unit_amount_minor
            .checked_mul(i64::from(request.quantity))
            .ok_or_else(|| PaymentError::Malformed("mollie subscription amount overflow".into()))?;
        let mut body = json!({
            "amount": amount_body(&request.currency, amount)?,
            "interval": interval_name(request.interval),
            "description": format!("{} ({})", request.plan_id, request.interval.as_str()),
            "metadata": binding_metadata(&request.organization_id, "subscription", None),
        });
        if let Some(webhook) = &self.webhook_url {
            body["webhookUrl"] = Value::String(webhook.clone());
        }
        let value = self
            .request(
                "PATCH",
                &format!(
                    "/customers/{}/subscriptions/{}",
                    encode_segment(&request.customer_id),
                    encode_segment(&request.subscription_id)
                ),
                Some(&body),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "subscription", "sub_")?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        Ok(ProviderSubscription {
            id: text(&value, "id")?,
            status: subscription_status(&text(&value, "status")?, None, now_ms)?,
            quantity: request.quantity,
            current_period_start_ms: now_ms,
            current_period_end_ms: now_ms.saturating_add(request.interval.period_ms()),
            cancel_at_period_end: false,
            item_id: None,
            product_id: None,
            price_id: None,
        })
    }

    async fn cancel(
        &self,
        request: ProviderCancelRequest,
    ) -> Result<ProviderSubscription, PaymentError> {
        if request.customer_id.is_empty() {
            return Err(PaymentError::Provider(
                "mollie subscription has no durable customer id".into(),
            ));
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        if request.at_period_end {
            // Mollie has no scheduled-cancel flag. Faktor stops initiating
            // charges and keeps the provider subscription until period end.
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
                "DELETE",
                &format!(
                    "/customers/{}/subscriptions/{}",
                    encode_segment(&request.customer_id),
                    encode_segment(&request.subscription_id)
                ),
                None,
                RouteLabel::PaymentProvider,
            )
            .await?;
        let id = if value.is_object() {
            text_or(&value, "id", &request.subscription_id)
        } else {
            request.subscription_id.clone()
        };
        Ok(ProviderSubscription {
            id,
            status: "canceled".into(),
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
        let webhook_url = self.require_webhook_url(request.webhook_url.as_deref())?;
        let metadata = binding_metadata(
            &request.organization_id,
            "payment",
            request.local_invoice_id.as_deref(),
        );
        let description = bounded_description(&request.description);
        let mut body = json!({
            "amount": amount_body(&request.currency, request.amount_minor)?,
            "description": description,
            "redirectUrl": request.success_url,
            "cancelUrl": request.cancel_url,
            "webhookUrl": webhook_url,
            "metadata": metadata,
        });
        if !request.customer_id.is_empty() {
            // A subscription checkout must create a mandate for the
            // recurring subscription; Mollie does that only for a `first`
            // payment bound to the customer.
            body["customerId"] = Value::String(request.customer_id.clone());
            body["sequenceType"] = Value::String("first".into());
        }
        let value = self
            .request(
                "POST",
                "/payments",
                Some(&body),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "payment", "tr_")?;
        let (amount_minor, currency) = parse_amount(
            value
                .get("amount")
                .ok_or_else(|| PaymentError::Provider("mollie payment has no amount".into()))?,
        )?;
        let url = value
            .pointer("/_links/checkout/href")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty() && url.len() <= 2048)
            .map(str::to_string);
        Ok(ProviderCheckout {
            id: text(&value, "id")?,
            status: text_or(&value, "status", "open"),
            url,
            amount_minor,
            currency,
        })
    }

    async fn create_invoice(
        &self,
        request: ProviderInvoiceRequest,
    ) -> Result<ProviderInvoice, PaymentError> {
        // Faktor remains the invoice authority. Mollie receives payments, not
        // Faktor invoices; the local identity is deterministic and persisted
        // by the service.
        Ok(ProviderInvoice {
            id: format!(
                "mollie-invoice-{}",
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

    async fn fetch_invoice(
        &self,
        _provider_invoice_id: &str,
    ) -> Result<crate::provider::ProviderInvoiceSnapshot, PaymentError> {
        Err(PaymentError::NotFound(
            "mollie does not expose Faktor invoices; Faktor is the invoice authority".into(),
        ))
    }

    async fn fetch_payment(
        &self,
        provider_payment_id: &str,
    ) -> Result<ProviderPaymentSnapshot, PaymentError> {
        self.get_payment(provider_payment_id).await
    }

    async fn fetch_refund(
        &self,
        provider_payment_id: &str,
        provider_refund_id: &str,
    ) -> Result<ProviderRefund, PaymentError> {
        self.get_refund(provider_payment_id, provider_refund_id)
            .await
    }

    async fn charge(&self, request: ProviderChargeRequest) -> Result<ProviderCharge, PaymentError> {
        let webhook_url = self.require_webhook_url(None)?;
        let metadata = binding_metadata(
            &request.organization_id,
            "payment",
            request.invoice_ref.as_deref(),
        );
        let body = json!({
            "amount": amount_body(&request.currency, request.amount_minor)?,
            "description": request
                .invoice_ref
                .as_deref()
                .map(|invoice| format!("Faktor invoice {invoice}"))
                .unwrap_or_else(|| "Faktor charge".to_string()),
            "customerId": request.customer_id,
            "mandateId": request.payment_method_token.expose(),
            "sequenceType": "recurring",
            "webhookUrl": webhook_url,
            "metadata": metadata,
        });
        let value = self
            .request(
                "POST",
                "/payments",
                Some(&body),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "payment", "tr_")?;
        let raw_status = text(&value, "status")?;
        let status = match raw_status.as_str() {
            "paid" => "succeeded",
            // Authorised/pending are not collected money; never report them
            // as succeeded and wait for the authoritative webhook.
            "authorized" | "pending" | "open" => "pending",
            "failed" | "canceled" | "expired" => {
                return Err(PaymentError::Declined(format!(
                    "mollie payment status {raw_status:?}"
                )))
            }
            other => {
                return Err(PaymentError::Provider(format!(
                    "mollie payment returned an unsupported status {other:?}"
                )))
            }
        };
        let (amount_minor, currency) = parse_amount(
            value
                .get("amount")
                .ok_or_else(|| PaymentError::Provider("mollie payment has no amount".into()))?,
        )?;
        Ok(ProviderCharge {
            id: text(&value, "id")?,
            status: status.to_string(),
            amount_minor,
            currency,
        })
    }

    async fn refund(&self, request: ProviderRefundRequest) -> Result<ProviderRefund, PaymentError> {
        if request.reason.len() > 255 {
            return Err(PaymentError::Malformed(
                "mollie refund description exceeds 255 bytes".into(),
            ));
        }
        let body = json!({
            "amount": amount_body(&request.currency, request.amount_minor)?,
            "description": request.reason,
            "metadata": binding_metadata(&request.organization_id, "refund", None),
        });
        let value = self
            .request(
                "POST",
                &format!(
                    "/payments/{}/refunds",
                    encode_segment(&request.provider_payment_id)
                ),
                Some(&body),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "refund", "re_")?;
        let (amount_minor, currency) = parse_amount(
            value
                .get("amount")
                .ok_or_else(|| PaymentError::Provider("mollie refund has no amount".into()))?,
        )?;
        Ok(ProviderRefund {
            id: text(&value, "id")?,
            status: normalize_refund_status(&text(&value, "status")?)?.to_string(),
            amount_minor,
            currency,
        })
    }

    fn verify_webhook(
        &self,
        _headers: &WebhookHeaders,
        body: &[u8],
        _now_ms: i64,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        // The posted body is only an id. Returning an event from it would let
        // an unauthenticated payload claim a status, so the synchronous path
        // refuses; `resolve_webhook` performs the authoritative fetch.
        parse_mollie_webhook(body)?;
        Err(PaymentError::WebhookUnauthorized(
            "mollie webhook truth requires an authoritative API fetch".into(),
        ))
    }

    async fn resolve_webhook(
        &self,
        _headers: &WebhookHeaders,
        body: &[u8],
        now_ms: i64,
        bindings: &dyn WebhookBindingLookup,
    ) -> Result<VerifiedWebhookEvent, PaymentError> {
        // The posted body is only an id. Every event below is built from the
        // resource fetched now, so duplicate/out-of-order deliveries converge
        // on current provider truth. The event id embeds the fetched status
        // (idempotency), while the service watermark orders observations;
        // `created_ms` is therefore the observation time, not a payload time.
        let reference = parse_mollie_webhook(body)?;
        match reference.kind {
            MollieWebhookKind::Payment => {
                let value = self.payment_resource(&reference.id).await?;
                let event = self.payment_event(&value, None, now_ms)?;
                let organization = event.organization_id.as_deref().unwrap_or_default();
                if !bindings.is_known_organization(organization)? {
                    return Err(PaymentError::WebhookUnauthorized(
                        "mollie resource names an organization this deployment does not serve"
                            .into(),
                    ));
                }
                Ok(event)
            }
            MollieWebhookKind::Refund => {
                let Some(WebhookBinding::Refund {
                    organization_id,
                    provider_payment_id,
                }) = bindings.binding(&reference.id)?
                else {
                    return Err(PaymentError::WebhookUnauthorized(
                        "mollie refund webhook has no durable local binding".into(),
                    ));
                };
                let value = self
                    .refund_resource(&provider_payment_id, &reference.id)
                    .await?;
                let payment_id =
                    value
                        .get("paymentId")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            PaymentError::WebhookMalformed(
                                "mollie refund carries no paymentId".into(),
                            )
                        })?;
                if payment_id != provider_payment_id {
                    return Err(PaymentError::WebhookUnauthorized(
                        "mollie refund payment binding does not match".into(),
                    ));
                }
                self.refund_event(&value, &organization_id, now_ms)
            }
            MollieWebhookKind::Subscription => {
                let Some(WebhookBinding::Subscription {
                    organization_id,
                    provider_customer_id,
                }) = bindings.binding(&reference.id)?
                else {
                    return Err(PaymentError::WebhookUnauthorized(
                        "mollie subscription webhook has no durable local binding".into(),
                    ));
                };
                let value = self
                    .subscription_resource(&provider_customer_id, &reference.id)
                    .await?;
                self.subscription_event(&value, &organization_id, now_ms)
            }
        }
    }

    async fn create_mandate(
        &self,
        request: ProviderMandateRequest,
    ) -> Result<ProviderMandate, PaymentError> {
        if request.method != "directdebit" {
            return Err(PaymentError::Malformed(
                "mollie mandates support only the directdebit method".into(),
            ));
        }
        let mut body = json!({"method": request.method});
        if let Some(name) = &request.consumer_name {
            body["consumerName"] = Value::String(name.clone());
        }
        if let Some(account) = &request.consumer_account {
            body["consumerAccount"] = Value::String(account.clone());
        }
        if let Some(date) = &request.signature_date {
            body["signatureDate"] = Value::String(date.clone());
        }
        let value = self
            .request(
                "POST",
                &format!(
                    "/customers/{}/mandates",
                    encode_segment(&request.customer_id)
                ),
                Some(&body),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "mandate", "mdt_")?;
        Ok(mandate_from_value(&value, &request.customer_id))
    }

    async fn get_mandate(
        &self,
        _organization_id: &str,
        customer_id: &str,
        mandate_id: &str,
    ) -> Result<ProviderMandate, PaymentError> {
        let value = self.mandate_resource(customer_id, mandate_id).await?;
        if let Some(customer) = value.get("customerId").and_then(Value::as_str) {
            if customer != customer_id {
                return Err(PaymentError::Provider(
                    "mollie mandate does not belong to the requested customer".into(),
                ));
            }
        }
        Ok(mandate_from_value(&value, customer_id))
    }

    async fn revoke_mandate(
        &self,
        _organization_id: &str,
        customer_id: &str,
        mandate_id: &str,
    ) -> Result<ProviderMandate, PaymentError> {
        self.request(
            "DELETE",
            &format!(
                "/customers/{}/mandates/{}",
                encode_segment(customer_id),
                encode_segment(mandate_id)
            ),
            None,
            RouteLabel::PaymentProvider,
        )
        .await?;
        Ok(ProviderMandate {
            id: mandate_id.to_string(),
            customer_id: customer_id.to_string(),
            status: "revoked".into(),
            method: "directdebit".into(),
        })
    }

    async fn create_payment_link(
        &self,
        request: ProviderPaymentLinkRequest,
    ) -> Result<ProviderPaymentLink, PaymentError> {
        let description = bounded_description(&request.description);
        let mut body = json!({
            "description": description,
            "amount": amount_body(&request.currency, request.amount_minor)?,
            "redirectUrl": request.redirect_url,
            "reusable": request.reusable,
            "metadata": binding_metadata(&request.organization_id, "payment_link", None),
        });
        // Links are for operator-issued handoffs. Only an explicit link
        // webhook is sent; the site's payment webhook ingress binds payments,
        // not `pl_` objects, and the adapter rejects unsupported prefixes.
        if let Some(webhook) = request.webhook_url {
            body["webhookUrl"] = Value::String(webhook);
        }
        let value = self
            .request(
                "POST",
                "/payment-links",
                Some(&body),
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "payment-link", "pl_")?;
        payment_link_from_value(&value)
    }

    async fn fetch_payment_link(
        &self,
        provider_payment_link_id: &str,
    ) -> Result<ProviderPaymentLink, PaymentError> {
        let value = self
            .request(
                "GET",
                &format!(
                    "/payment-links/{}",
                    encode_segment(provider_payment_link_id)
                ),
                None,
                RouteLabel::PaymentProvider,
            )
            .await?;
        require_resource(&value, "payment-link", "pl_")?;
        if text(&value, "id")? != provider_payment_link_id {
            return Err(PaymentError::Provider(
                "mollie returned a different payment-link id than requested".into(),
            ));
        }
        payment_link_from_value(&value)
    }

    async fn reconcile(
        &self,
        _request: crate::provider::ProviderReconcileRequest,
    ) -> Result<crate::provider::ProviderReconciled, PaymentError> {
        // Mollie has no idempotency-key search. The operation claim stays
        // `unknown` (never silently re-dispatched) until an operator or a
        // webhook re-fetches the durable provider id.
        Ok(crate::provider::ProviderReconciled::Unresolved(
            "mollie has no idempotency-key lookup; re-fetch by durable provider id".into(),
        ))
    }
}

/// Parse the id-only webhook body. Mollie posts
/// `application/x-www-form-urlencoded` with `id=<resource id>`; a JSON
/// object carrying `id` is accepted for mocks/operators. No other field is
/// read, and no status is trusted.
pub fn parse_mollie_webhook(body: &[u8]) -> Result<MollieWebhookRef, PaymentError> {
    if body.is_empty() || body.len() > MAX_WEBHOOK_BODY_BYTES {
        return Err(PaymentError::WebhookMalformed(
            "mollie webhook body is empty or exceeds the bound".into(),
        ));
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| PaymentError::WebhookMalformed("mollie webhook body is not UTF-8".into()))?;
    let trimmed = text.trim();
    let id = if trimmed.starts_with('{') {
        let value: Value = serde_json::from_str(trimmed).map_err(|_| {
            PaymentError::WebhookMalformed("mollie webhook body is not JSON".into())
        })?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| PaymentError::WebhookMalformed("mollie webhook carries no id".into()))?
    } else {
        let mut found = None;
        for pair in trimmed.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if key != "id" {
                continue;
            }
            if found.is_some() {
                return Err(PaymentError::WebhookMalformed(
                    "mollie webhook carries duplicate ids".into(),
                ));
            }
            found = Some(form_decode(value)?);
        }
        found
            .ok_or_else(|| PaymentError::WebhookMalformed("mollie webhook carries no id".into()))?
    };
    let kind = webhook_kind(&id)?;
    Ok(MollieWebhookRef { id, kind })
}

fn webhook_kind(id: &str) -> Result<MollieWebhookKind, PaymentError> {
    if id.len() > MAX_WEBHOOK_ID
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(PaymentError::WebhookMalformed(
            "mollie webhook id has an invalid shape".into(),
        ));
    }
    let kind = if id.starts_with("tr_") {
        MollieWebhookKind::Payment
    } else if id.starts_with("re_") {
        MollieWebhookKind::Refund
    } else if id.starts_with("sub_") {
        MollieWebhookKind::Subscription
    } else {
        return Err(PaymentError::WebhookMalformed(
            "mollie webhook id has an unsupported resource prefix".into(),
        ));
    };
    if id.len() <= 3 {
        return Err(PaymentError::WebhookMalformed(
            "mollie webhook id is empty".into(),
        ));
    }
    Ok(kind)
}

fn form_decode(value: &str) -> Result<String, PaymentError> {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(PaymentError::WebhookMalformed(
                        "mollie webhook id has a truncated escape".into(),
                    ));
                }
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| {
                    PaymentError::WebhookMalformed("mollie webhook id has an invalid escape".into())
                })?;
                let byte = u8::from_str_radix(hex, 16).map_err(|_| {
                    PaymentError::WebhookMalformed("mollie webhook id has an invalid escape".into())
                })?;
                out.push(byte);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out)
        .map_err(|_| PaymentError::WebhookMalformed("mollie webhook id is not UTF-8".into()))
}

fn parse_response(response: RawResponse) -> Result<Value, PaymentError> {
    if response.status == 204 && response.body.is_empty() {
        return Ok(Value::Null);
    }
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| {
        PaymentError::Provider(format!(
            "mollie answered HTTP {} with a non-JSON body",
            response.status
        ))
    })?;
    if (200..300).contains(&response.status) {
        return Ok(value);
    }
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("no provider detail")
        .chars()
        .take(256)
        .collect::<String>();
    let field = value.get("field").and_then(Value::as_str).unwrap_or("");
    let message = format!(
        "mollie refused HTTP {} ({}): {detail}",
        response.status, title
    );
    if response.status == 402 {
        return Err(PaymentError::Declined(message));
    }
    if response.status == 404 {
        return Err(PaymentError::NotFound(message));
    }
    if field.is_empty() {
        Err(PaymentError::Provider(message))
    } else {
        Err(PaymentError::Provider(format!(
            "{message} [field {}]",
            field.chars().take(64).collect::<String>()
        )))
    }
}

fn require_resource(value: &Value, resource: &str, prefix: &str) -> Result<(), PaymentError> {
    if let Some(kind) = value.get("resource").and_then(Value::as_str) {
        if kind != resource {
            return Err(PaymentError::Provider(format!(
                "mollie returned resource {kind:?}, expected {resource:?}"
            )));
        }
    }
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    if !id.starts_with(prefix) || id.len() <= prefix.len() || id.len() > MAX_WEBHOOK_ID {
        return Err(PaymentError::Provider(format!(
            "mollie returned an invalid {resource} id"
        )));
    }
    Ok(())
}

fn text(value: &Value, field: &str) -> Result<String, PaymentError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= MAX_PROVIDER_REFERENCE)
        .map(str::to_string)
        .ok_or_else(|| PaymentError::Provider(format!("mollie response has no {field}")))
}

fn text_or(value: &Value, field: &str, default: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= MAX_PROVIDER_REFERENCE)
        .unwrap_or(default)
        .to_string()
}

fn metadata_text(metadata: Option<&Value>, key: &str) -> Option<String> {
    metadata
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_PROVIDER_REFERENCE)
        .map(str::to_string)
}

fn bounded_metadata(metadata: Option<&Value>) -> Value {
    let Some(object) = metadata.and_then(Value::as_object) else {
        return Value::Null;
    };
    let mut bounded = Map::new();
    for (key, value) in object.iter().take(16) {
        if key.len() > 64 {
            continue;
        }
        if !matches!(
            key.as_str(),
            "organization_id"
                | "faktor_provider"
                | "faktor_kind"
                | "local_invoice_id"
                | "local_refund_id"
                | "plan_id"
        ) {
            continue;
        }
        if let Some(value) = value
            .as_str()
            .filter(|value| value.len() <= MAX_PROVIDER_REFERENCE)
        {
            bounded.insert(key.clone(), Value::String(value.to_string()));
        }
    }
    Value::Object(bounded)
}

fn binding_metadata(organization_id: &str, kind: &str, local_id: Option<&str>) -> Value {
    let mut metadata = Map::new();
    metadata.insert(
        "organization_id".into(),
        Value::String(organization_id.to_string()),
    );
    metadata.insert("faktor_provider".into(), Value::String(PROVIDER.into()));
    metadata.insert("faktor_kind".into(), Value::String(kind.into()));
    if let Some(local_id) =
        local_id.filter(|id| !id.is_empty() && id.len() <= MAX_PROVIDER_REFERENCE)
    {
        let key = match kind {
            "payment" => "local_invoice_id",
            "refund" => "local_refund_id",
            _ => "local_id",
        };
        metadata.insert(key.into(), Value::String(local_id.to_string()));
    }
    Value::Object(metadata)
}

fn verify_binding(
    metadata: Option<&Value>,
    expected_kind: &str,
    expected_org: Option<&str>,
) -> Result<String, PaymentError> {
    let object = metadata.and_then(Value::as_object).ok_or_else(|| {
        PaymentError::WebhookUnauthorized(
            "mollie resource carries no Faktor metadata binding".into(),
        )
    })?;
    let provider = object
        .get("faktor_provider")
        .and_then(Value::as_str)
        .unwrap_or("");
    if provider != PROVIDER {
        return Err(PaymentError::WebhookUnauthorized(
            "mollie resource is not marked as a Faktor-created resource".into(),
        ));
    }
    let kind = object
        .get("faktor_kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    if kind != expected_kind {
        return Err(PaymentError::WebhookUnauthorized(
            "mollie resource kind does not match its webhook binding".into(),
        ));
    }
    let organization = object
        .get("organization_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if organization.is_empty()
        || organization.len() > 128
        || OrganizationId::try_new(organization.to_string()).is_err()
    {
        return Err(PaymentError::WebhookUnauthorized(
            "mollie resource carries no valid organization binding".into(),
        ));
    }
    if let Some(expected) = expected_org {
        if expected != organization {
            return Err(PaymentError::WebhookUnauthorized(
                "mollie resource organization does not match the local binding".into(),
            ));
        }
    }
    Ok(organization.to_string())
}

fn normalize_refund_status(status: &str) -> Result<&'static str, PaymentError> {
    Ok(match status {
        "queued" | "pending" | "processing" => "pending",
        "refunded" => "succeeded",
        "failed" | "canceled" => "failed",
        other => {
            return Err(PaymentError::Provider(format!(
                "mollie refund returned an unsupported status {other:?}"
            )))
        }
    })
}

fn subscription_status(
    status: &str,
    trial_end_ms: Option<i64>,
    now_ms: i64,
) -> Result<String, PaymentError> {
    if trial_end_ms.is_some_and(|trial| trial > now_ms)
        && !matches!(status, "canceled" | "completed")
    {
        return Ok("trialing".into());
    }
    Ok(match status {
        "active" => "active",
        "pending" => "pending",
        "suspended" => "past_due",
        "canceled" | "completed" | "expired" => "canceled",
        other => {
            return Err(PaymentError::Provider(format!(
                "mollie subscription returned an unsupported status {other:?}"
            )))
        }
    }
    .to_string())
}

fn interval_name(interval: BillingInterval) -> String {
    match interval {
        BillingInterval::Monthly => "1 month".into(),
        BillingInterval::Annual => "12 months".into(),
    }
}

fn amount_body(currency: &str, amount_minor: i64) -> Result<Value, PaymentError> {
    if !valid_currency(currency) {
        return Err(PaymentError::Malformed(
            "mollie amount currency must be three uppercase letters".into(),
        ));
    }
    if amount_minor <= 0 || !valid_amount(amount_minor) {
        return Err(PaymentError::Malformed(
            "mollie amount must be a positive bounded minor amount".into(),
        ));
    }
    let formatted = format_minor(amount_minor, currency, default_exponent(currency));
    let value = formatted
        .split_once(' ')
        .map(|(value, _)| value.to_string())
        .ok_or_else(|| PaymentError::Malformed("mollie amount formatting failed".into()))?;
    Ok(json!({"currency": currency, "value": value}))
}

fn parse_amount(amount: &Value) -> Result<(i64, String), PaymentError> {
    let currency = amount
        .get("currency")
        .and_then(Value::as_str)
        .filter(|currency| valid_currency(currency))
        .ok_or_else(|| PaymentError::Provider("mollie amount has no currency".into()))?;
    let value = amount
        .get("value")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 64)
        .ok_or_else(|| PaymentError::Provider("mollie amount has no value".into()))?;
    let minor = parse_decimal_minor(value, currency)?;
    Ok((minor, currency.to_string()))
}

fn parse_decimal_minor(value: &str, currency: &str) -> Result<i64, PaymentError> {
    let exponent = default_exponent(currency) as usize;
    let mut parts = value.split('.');
    let major = parts.next().unwrap_or("");
    if major.is_empty() || !major.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PaymentError::Provider(
            "mollie amount value is not an exact decimal".into(),
        ));
    }
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PaymentError::Provider(
            "mollie amount value is not an exact decimal".into(),
        ));
    }
    if fraction.len() > exponent && !fraction[exponent..].bytes().all(|byte| byte == b'0') {
        return Err(PaymentError::Provider(
            "mollie amount has more decimals than the currency carries".into(),
        ));
    }
    let taken = &fraction[..fraction.len().min(exponent)];
    let mut digits = String::with_capacity(major.len() + exponent + 1);
    digits.push_str(major);
    for index in 0..exponent {
        digits.push(taken.as_bytes().get(index).copied().unwrap_or(b'0') as char);
    }
    let value = digits
        .parse::<i128>()
        .map_err(|_| PaymentError::Provider("mollie amount overflows".into()))?;
    let value = i64::try_from(value)
        .map_err(|_| PaymentError::Provider("mollie amount overflows".into()))?;
    if !valid_amount(value) {
        return Err(PaymentError::Provider(
            "mollie amount exceeds the bounded money domain".into(),
        ));
    }
    Ok(value)
}

fn bounded_description(description: &str) -> String {
    let trimmed = description.trim();
    if trimmed.is_empty() {
        "Faktor payment".to_string()
    } else {
        trimmed.chars().take(255).collect()
    }
}

fn mandate_from_value(value: &Value, customer_id: &str) -> ProviderMandate {
    ProviderMandate {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        customer_id: value
            .get("customerId")
            .and_then(Value::as_str)
            .unwrap_or(customer_id)
            .to_string(),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .to_string(),
        method: value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("directdebit")
            .to_string(),
    }
}

fn payment_link_from_value(value: &Value) -> Result<ProviderPaymentLink, PaymentError> {
    let (amount_minor, currency) = parse_amount(
        value
            .get("amount")
            .ok_or_else(|| PaymentError::Provider("mollie payment link has no amount".into()))?,
    )?;
    let paid = value.get("paidAt").is_some_and(|paid| !paid.is_null());
    let archived = value
        .get("archived")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ProviderPaymentLink {
        id: text(value, "id")?,
        status: if paid {
            "paid".into()
        } else if archived {
            "archived".into()
        } else {
            "active".into()
        },
        url: value
            .pointer("/_links/paymentLink/href")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty() && url.len() <= 2048)
            .map(str::to_string),
        amount_minor,
        currency,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> MolliePaymentProvider {
        let config = MollieConfig {
            webhook_url: Some("https://example.invalid/webhooks/payments".into()),
            ..MollieConfig::default()
        };
        MolliePaymentProvider::new(
            &config,
            SecretString::new("test_mollie_secret_key"),
            Arc::new(faktor_provider::egress::MockHttpTransport::new(200, "{}")),
        )
        .unwrap()
    }

    #[test]
    fn debug_never_renders_the_api_key() {
        let debug = format!("{:?}", provider());
        assert!(!debug.contains("test_mollie_secret_key"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn id_only_webhook_parsing_never_reads_a_status() {
        let form = parse_mollie_webhook(b"id=tr_payment_1&status=failed").unwrap();
        assert_eq!(form.id, "tr_payment_1");
        assert_eq!(form.kind, MollieWebhookKind::Payment);
        let json = parse_mollie_webhook(br#"{"id":"re_refund_1","status":"refunded"}"#).unwrap();
        assert_eq!(json.kind, MollieWebhookKind::Refund);
        let subscription = parse_mollie_webhook(b"id=sub_subscription_1").unwrap();
        assert_eq!(subscription.kind, MollieWebhookKind::Subscription);
        for body in [
            &b""[..],
            b"id=tr_1&id=tr_2",
            b"id=pl_link_1",
            b"id=tr_1%2F",
            b"{\"status\":\"paid\"}",
        ] {
            assert!(parse_mollie_webhook(body).is_err(), "{body:?}");
        }
    }

    #[test]
    fn amounts_are_exact_and_currency_aware() {
        assert_eq!(
            amount_body("EUR", 1900).unwrap(),
            json!({"currency": "EUR", "value": "19.00"})
        );
        assert_eq!(
            amount_body("JPY", 1900).unwrap(),
            json!({"currency": "JPY", "value": "1900"})
        );
        assert_eq!(
            parse_amount(&json!({"currency": "EUR", "value": "19.00"})).unwrap(),
            (1900, "EUR".into())
        );
        assert_eq!(
            parse_amount(&json!({"currency": "JPY", "value": "1900"})).unwrap(),
            (1900, "JPY".into())
        );
        assert!(parse_amount(&json!({"currency": "EUR", "value": "19.001"})).is_err());
        assert!(parse_amount(&json!({"currency": "EUR", "value": "-19.00"})).is_err());
        assert!(amount_body("EUR", 0).is_err());
    }

    #[test]
    fn binding_rejects_foreign_and_unmarked_metadata() {
        let ours = binding_metadata("org_1", "payment", Some("inv_1"));
        assert_eq!(
            verify_binding(Some(&ours), "payment", None).unwrap(),
            "org_1"
        );
        assert!(matches!(
            verify_binding(Some(&ours), "refund", None),
            Err(PaymentError::WebhookUnauthorized(_))
        ));
        assert!(matches!(
            verify_binding(Some(&json!({"organization_id": "org_2"})), "payment", None),
            Err(PaymentError::WebhookUnauthorized(_))
        ));
        assert!(matches!(
            verify_binding(Some(&ours), "payment", Some("org_2")),
            Err(PaymentError::WebhookUnauthorized(_))
        ));
    }
}
