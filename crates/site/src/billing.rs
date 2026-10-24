use faktor_cloud::OrganizationId;
use faktor_payments::{
    BillingInterval, ChangePlanRequest, Invoice, InvoiceStatus, PaymentService, ProviderCheckout,
    ProviderCheckoutRequest, SubscribeRequest,
};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::error::{SiteError, SiteResult};

#[derive(Debug, Clone)]
pub struct PlanView {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub monthly_minor: i64,
    pub annual_minor: i64,
    pub features: &'static [&'static str],
}

pub const PLANS: &[PlanView] = &[
    PlanView {
        id: "solo",
        name: "Solo",
        description: "One engineer, one shared evidence trail.",
        monthly_minor: 1_900,
        annual_minor: 19_000,
        features: &[
            "One workspace",
            "Native runtime and CLI",
            "VS Code and JetBrains surfaces",
            "Durable invoices and receipts",
        ],
    },
    PlanView {
        id: "team",
        name: "Team",
        description: "Shared organization, roles and a visible evidence record.",
        monthly_minor: 4_900,
        annual_minor: 49_000,
        features: &[
            "Organization roles and membership",
            "Usage and invoice history",
            "Shared billing record",
            "Provider-token payment methods",
        ],
    },
    PlanView {
        id: "enterprise",
        name: "Enterprise",
        description: "A larger organization with the same inspectable finish.",
        monthly_minor: 19_900,
        annual_minor: 199_000,
        features: &[
            "Platform administration",
            "Usage and receipt exports",
            "Full invoice history",
            "Provider-hosted checkout",
        ],
    },
];

pub fn plan(id: &str) -> Option<&'static PlanView> {
    PLANS.iter().find(|plan| plan.id == id)
}

#[derive(Debug, Clone)]
pub struct CheckoutRecord {
    pub id: String,
    pub organization_id: String,
    pub invoice_id: String,
    pub plan_id: String,
    pub interval: String,
    pub amount_minor: i64,
    pub currency: String,
    pub provider: String,
    pub provider_checkout_id: Option<String>,
    pub checkout_url: Option<String>,
    pub status: String,
    pub created_ms: i64,
}

pub struct CheckoutDraft<'a> {
    pub organization_id: &'a str,
    pub invoice_id: &'a str,
    pub plan_id: &'a str,
    pub interval: &'a str,
    pub amount_minor: i64,
    pub currency: &'a str,
    pub provider: &'a str,
    pub idempotency_key: &'a str,
    pub now_ms: i64,
}

pub fn insert_checkout(
    conn: &rusqlite::Connection,
    draft: &CheckoutDraft<'_>,
    provider_checkout_id: Option<&str>,
    checkout_url: Option<&str>,
) -> SiteResult<CheckoutRecord> {
    let id = format!("chk_{}", Uuid::new_v4());
    conn.execute(
        "INSERT INTO billing_checkouts (id,organization_id,invoice_id,plan_id,interval,amount_minor,currency,provider,provider_checkout_id,checkout_url,status,idempotency_key,created_ms,updated_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'open',?11,?12,?12)",
        params![
            id,
            draft.organization_id,
            draft.invoice_id,
            draft.plan_id,
            draft.interval,
            draft.amount_minor,
            draft.currency,
            draft.provider,
            provider_checkout_id,
            checkout_url,
            draft.idempotency_key,
            draft.now_ms
        ],
    )?;
    Ok(CheckoutRecord {
        id,
        organization_id: draft.organization_id.to_string(),
        invoice_id: draft.invoice_id.to_string(),
        plan_id: draft.plan_id.to_string(),
        interval: draft.interval.to_string(),
        amount_minor: draft.amount_minor,
        currency: draft.currency.to_string(),
        provider: draft.provider.to_string(),
        provider_checkout_id: provider_checkout_id.map(str::to_string),
        checkout_url: checkout_url.map(str::to_string),
        status: "open".into(),
        created_ms: draft.now_ms,
    })
}

pub fn checkout_by_id(
    conn: &rusqlite::Connection,
    org_id: &str,
    checkout_id: &str,
) -> SiteResult<Option<CheckoutRecord>> {
    conn.query_row(
        "SELECT id,organization_id,invoice_id,plan_id,interval,amount_minor,currency,provider,provider_checkout_id,checkout_url,status,created_ms FROM billing_checkouts WHERE id=?1 AND organization_id=?2",
        params![checkout_id, org_id],
        |row| {
            Ok(CheckoutRecord {
                id: row.get(0)?,
                organization_id: row.get(1)?,
                invoice_id: row.get(2)?,
                plan_id: row.get(3)?,
                interval: row.get(4)?,
                amount_minor: row.get(5)?,
                currency: row.get(6)?,
                provider: row.get(7)?,
                provider_checkout_id: row.get(8)?,
                checkout_url: row.get(9)?,
                status: row.get(10)?,
                created_ms: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(SiteError::from)
}

pub fn latest_checkout_for_invoice(
    conn: &rusqlite::Connection,
    org_id: &str,
    invoice_id: &str,
) -> SiteResult<Option<CheckoutRecord>> {
    conn.query_row(
        "SELECT id,organization_id,invoice_id,plan_id,interval,amount_minor,currency,provider,provider_checkout_id,checkout_url,status,created_ms FROM billing_checkouts WHERE organization_id=?1 AND invoice_id=?2 ORDER BY created_ms DESC LIMIT 1",
        params![org_id, invoice_id],
        |row| {
            Ok(CheckoutRecord {
                id: row.get(0)?,
                organization_id: row.get(1)?,
                invoice_id: row.get(2)?,
                plan_id: row.get(3)?,
                interval: row.get(4)?,
                amount_minor: row.get(5)?,
                currency: row.get(6)?,
                provider: row.get(7)?,
                provider_checkout_id: row.get(8)?,
                checkout_url: row.get(9)?,
                status: row.get(10)?,
                created_ms: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(SiteError::from)
}

pub fn checkout_by_idempotency(
    conn: &rusqlite::Connection,
    org_id: &str,
    idempotency_key: &str,
) -> SiteResult<Option<CheckoutRecord>> {
    conn.query_row(
        "SELECT id,organization_id,invoice_id,plan_id,interval,amount_minor,currency,provider,provider_checkout_id,checkout_url,status,created_ms FROM billing_checkouts WHERE organization_id=?1 AND idempotency_key=?2",
        params![org_id, idempotency_key],
        |row| {
            Ok(CheckoutRecord {
                id: row.get(0)?,
                organization_id: row.get(1)?,
                invoice_id: row.get(2)?,
                plan_id: row.get(3)?,
                interval: row.get(4)?,
                amount_minor: row.get(5)?,
                currency: row.get(6)?,
                provider: row.get(7)?,
                provider_checkout_id: row.get(8)?,
                checkout_url: row.get(9)?,
                status: row.get(10)?,
                created_ms: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(SiteError::from)
}

pub fn mark_checkout_paid(
    conn: &rusqlite::Connection,
    org_id: &str,
    invoice_id: &str,
    now_ms: i64,
) -> SiteResult<()> {
    conn.execute(
        "UPDATE billing_checkouts SET status='paid',updated_ms=?1 WHERE organization_id=?2 AND invoice_id=?3 AND status='open'",
        params![now_ms, org_id, invoice_id],
    )?;
    Ok(())
}

pub fn org_id(auth: &crate::auth::AuthSession) -> SiteResult<OrganizationId> {
    OrganizationId::try_new(auth.organization_id.clone()).map_err(|_| {
        SiteError::internal_message("stored organization id is not a valid control-plane id")
    })
}

pub fn open_invoice_for_subscription(
    payments: &PaymentService,
    org: &OrganizationId,
    subscription_id: &str,
) -> SiteResult<Option<Invoice>> {
    let page = payments.invoices(org, None, 50)?;
    Ok(page.items.into_iter().find(|invoice| {
        invoice.subscription_id.as_deref() == Some(subscription_id)
            && invoice.status == InvoiceStatus::Open
    }))
}

pub fn invoice_is_open(invoice: &Invoice) -> bool {
    invoice.status == InvoiceStatus::Open && invoice.balance_minor() > 0
}

pub fn interval_label(interval: BillingInterval) -> &'static str {
    match interval {
        BillingInterval::Monthly => "monthly",
        BillingInterval::Annual => "annual",
    }
}

pub fn subscription_request(
    plan_id: &str,
    interval: BillingInterval,
    email: &str,
    name: &str,
    idempotency_key: String,
) -> SiteResult<SubscribeRequest> {
    if plan(plan_id).is_none() {
        return Err(SiteError::Unprocessable("Choose a current plan."));
    }
    Ok(SubscribeRequest {
        plan_id: plan_id.to_string(),
        interval,
        seats: 1,
        trial_days: None,
        customer_email: email.to_string(),
        customer_name: name.to_string(),
        idempotency_key,
    })
}

pub fn change_request(
    subscription_id: &str,
    plan_id: &str,
    interval: Option<BillingInterval>,
    idempotency_key: String,
) -> SiteResult<ChangePlanRequest> {
    if plan(plan_id).is_none() {
        return Err(SiteError::Unprocessable("Choose a current plan."));
    }
    Ok(ChangePlanRequest {
        subscription_id: subscription_id.to_string(),
        plan_id: plan_id.to_string(),
        interval,
        idempotency_key,
    })
}

pub fn checkout_request(
    org: &OrganizationId,
    customer_id: &str,
    invoice: &Invoice,
    plan_id: &str,
    base_url: &str,
    idempotency_key: String,
) -> SiteResult<ProviderCheckoutRequest> {
    Ok(ProviderCheckoutRequest {
        organization_id: org.as_str().to_string(),
        idempotency_key,
        customer_id: customer_id.to_string(),
        description: format!("Faktor {} plan ({})", plan_id, invoice.number),
        amount_minor: invoice.balance_minor(),
        currency: invoice.currency.clone(),
        success_url: format!(
            "{base_url}/account/billing/checkout/return/?invoice={}",
            invoice.id
        ),
        cancel_url: format!("{base_url}/account/billing/"),
        webhook_url: Some(format!("{base_url}/webhooks/payments")),
        local_invoice_id: Some(invoice.id.clone()),
    })
}

pub fn provider_checkout_url(checkout: &ProviderCheckout) -> Option<String> {
    checkout.url.as_ref().and_then(|url| {
        if url.len() <= 2048
            && (url.starts_with("https://") || url.starts_with("http://"))
            && !url.chars().any(char::is_whitespace)
        {
            Some(url.clone())
        } else {
            None
        }
    })
}
