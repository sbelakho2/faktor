use faktor_cloud::OrganizationId;
use faktor_payments::{
    BillingInterval, ChangePlanRequest, Invoice, InvoiceStatus, PaymentService, ProviderCheckout,
    ProviderCheckoutRequest, SubscribeRequest,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
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

/// How long one hosted checkout may be reused for the same invoice balance.
pub const CHECKOUT_FENCE_TTL_MS: i64 = 30 * 60 * 1000;

/// The durable invoice-scoped checkout authority. Exactly one fence is
/// `pending`/`open` per organization and invoice; it binds the provider
/// request to the invoice balance generation captured when it was acquired.
#[derive(Debug, Clone)]
pub struct CheckoutFence {
    pub id: String,
    pub organization_id: String,
    pub invoice_id: String,
    pub balance_generation: String,
    pub status: String,
    pub request_id: String,
    pub provider: String,
    pub provider_idempotency_key: String,
    pub provider_request: String,
    pub plan_id: String,
    pub interval: String,
    pub amount_minor: i64,
    pub currency: String,
    pub provider_checkout_id: Option<String>,
    pub checkout_url: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub expires_ms: i64,
}

pub struct CheckoutFenceDraft<'a> {
    pub id: &'a str,
    pub organization_id: &'a str,
    pub invoice_id: &'a str,
    pub balance_generation: &'a str,
    pub request_id: &'a str,
    pub provider: &'a str,
    pub provider_idempotency_key: &'a str,
    pub provider_request: &'a str,
    pub plan_id: &'a str,
    pub interval: &'a str,
    pub amount_minor: i64,
    pub currency: &'a str,
    pub now_ms: i64,
}

#[derive(Debug, Clone)]
pub enum CheckoutFenceAcquisition {
    /// The same open balance already has an activated provider checkout.
    Existing(CheckoutFence),
    /// Another request holds the fence and is at (or heading to) the provider.
    Pending(CheckoutFence),
    /// This caller owns the fence and must create the provider checkout.
    Acquired(CheckoutFence),
}

const FENCE_COLUMNS: &str = "id,organization_id,invoice_id,balance_generation,status,request_id,\
     provider,provider_idempotency_key,provider_request,plan_id,interval,amount_minor,currency,\
     provider_checkout_id,checkout_url,created_ms,updated_ms,expires_ms";

fn decode_fence(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckoutFence> {
    Ok(CheckoutFence {
        id: row.get(0)?,
        organization_id: row.get(1)?,
        invoice_id: row.get(2)?,
        balance_generation: row.get(3)?,
        status: row.get(4)?,
        request_id: row.get(5)?,
        provider: row.get(6)?,
        provider_idempotency_key: row.get(7)?,
        provider_request: row.get(8)?,
        plan_id: row.get(9)?,
        interval: row.get(10)?,
        amount_minor: row.get(11)?,
        currency: row.get(12)?,
        provider_checkout_id: row.get(13)?,
        checkout_url: row.get(14)?,
        created_ms: row.get(15)?,
        updated_ms: row.get(16)?,
        expires_ms: row.get(17)?,
    })
}

fn active_checkout_fence(
    conn: &rusqlite::Connection,
    org_id: &str,
    invoice_id: &str,
) -> SiteResult<Option<CheckoutFence>> {
    conn.query_row(
        &format!(
            "SELECT {FENCE_COLUMNS} FROM billing_checkout_fences \
             WHERE organization_id=?1 AND invoice_id=?2 AND status IN ('pending','open') \
             ORDER BY created_ms DESC LIMIT 1"
        ),
        params![org_id, invoice_id],
        decode_fence,
    )
    .optional()
    .map_err(SiteError::from)
}

pub fn checkout_fence_by_id(
    conn: &rusqlite::Connection,
    org_id: &str,
    fence_id: &str,
) -> SiteResult<Option<CheckoutFence>> {
    conn.query_row(
        &format!(
            "SELECT {FENCE_COLUMNS} FROM billing_checkout_fences \
             WHERE organization_id=?1 AND id=?2"
        ),
        params![org_id, fence_id],
        decode_fence,
    )
    .optional()
    .map_err(SiteError::from)
}

fn set_fence_status(
    conn: &rusqlite::Connection,
    org_id: &str,
    fence_id: &str,
    from: &str,
    to: &str,
    now_ms: i64,
) -> SiteResult<()> {
    conn.execute(
        "UPDATE billing_checkout_fences SET status=?3,updated_ms=?4 \
         WHERE organization_id=?1 AND id=?2 AND status=?5",
        params![org_id, fence_id, to, now_ms, from],
    )?;
    Ok(())
}

/// The canonical balance generation of one invoice. It moves only when the
/// authoritative invoice balance/status moves, never with a caller identity.
pub fn checkout_balance_generation(invoice: &Invoice) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"faktor.checkout.balance.v1\0");
    hasher.update(invoice.id.as_bytes());
    hasher.update([0]);
    hasher.update(invoice.organization_id.as_bytes());
    hasher.update([0]);
    hasher.update(invoice.currency.as_bytes());
    hasher.update([0]);
    hasher.update(invoice.status.as_str().as_bytes());
    for value in [
        invoice.total_minor,
        invoice.amount_paid_minor,
        invoice.amount_refunded_minor,
    ] {
        hasher.update(value.to_be_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn new_checkout_fence_id() -> String {
    format!("chk_{}", Uuid::new_v4())
}

pub fn checkout_provider_key(fence_id: &str) -> String {
    format!("site-checkout-{fence_id}")
}

fn insert_checkout_fence(
    conn: &rusqlite::Connection,
    draft: &CheckoutFenceDraft<'_>,
) -> SiteResult<CheckoutFence> {
    let expires_ms = draft.now_ms.saturating_add(CHECKOUT_FENCE_TTL_MS);
    conn.execute(
        "INSERT INTO billing_checkout_fences \
         (id,organization_id,invoice_id,balance_generation,status,request_id,provider,\
          provider_idempotency_key,provider_request,plan_id,interval,amount_minor,currency,\
          created_ms,updated_ms,expires_ms) \
         VALUES (?1,?2,?3,?4,'pending',?5,?6,?7,?8,?9,?10,?11,?12,?13,?13,?14)",
        params![
            draft.id,
            draft.organization_id,
            draft.invoice_id,
            draft.balance_generation,
            draft.request_id,
            draft.provider,
            draft.provider_idempotency_key,
            draft.provider_request,
            draft.plan_id,
            draft.interval,
            draft.amount_minor,
            draft.currency,
            draft.now_ms,
            expires_ms
        ],
    )?;
    Ok(CheckoutFence {
        id: draft.id.to_string(),
        organization_id: draft.organization_id.to_string(),
        invoice_id: draft.invoice_id.to_string(),
        balance_generation: draft.balance_generation.to_string(),
        status: "pending".into(),
        request_id: draft.request_id.to_string(),
        provider: draft.provider.to_string(),
        provider_idempotency_key: draft.provider_idempotency_key.to_string(),
        provider_request: draft.provider_request.to_string(),
        plan_id: draft.plan_id.to_string(),
        interval: draft.interval.to_string(),
        amount_minor: draft.amount_minor,
        currency: draft.currency.to_string(),
        provider_checkout_id: None,
        checkout_url: None,
        created_ms: draft.now_ms,
        updated_ms: draft.now_ms,
        expires_ms,
    })
}

/// Acquire the one invoice-scoped checkout authority before any provider
/// call. A different request for the same open balance either receives the
/// existing activated checkout, waits on the pending owner, or adopts a
/// crashed pending owner with its EXACT persisted provider request.
pub fn acquire_checkout_fence(
    conn: &mut rusqlite::Connection,
    draft: &CheckoutFenceDraft<'_>,
) -> SiteResult<CheckoutFenceAcquisition> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(fence) = active_checkout_fence(&tx, draft.organization_id, draft.invoice_id)? {
        match fence.status.as_str() {
            "open" => {
                if fence.balance_generation == draft.balance_generation
                    && fence.expires_ms > draft.now_ms
                {
                    tx.commit()?;
                    return Ok(CheckoutFenceAcquisition::Existing(fence));
                }
                let next = if fence.balance_generation == draft.balance_generation {
                    "expired"
                } else {
                    "superseded"
                };
                set_fence_status(
                    &tx,
                    draft.organization_id,
                    &fence.id,
                    "open",
                    next,
                    draft.now_ms,
                )?;
                tx.execute(
                    "UPDATE billing_checkouts SET status=?3,updated_ms=?4 \
                     WHERE organization_id=?1 AND id=?2 AND status='open'",
                    params![draft.organization_id, fence.id, next, draft.now_ms],
                )?;
            }
            // A pending fence is owned by a live request (the caller checks
            // process ownership) or survived a crash; acquisition never
            // silently replaces it, so the persisted provider key is reused.
            "pending" => {
                tx.commit()?;
                return Ok(CheckoutFenceAcquisition::Pending(fence));
            }
            _ => {}
        }
    }
    let fence = insert_checkout_fence(&tx, draft)?;
    tx.commit()?;
    Ok(CheckoutFenceAcquisition::Acquired(fence))
}

/// Reclaim a pending fence that no live request in this process owns (it
/// survived a crash). The persisted provider key and request are kept, so a
/// retry reaches the same provider idempotency boundary instead of creating a
/// second checkout object.
pub fn reclaim_pending_checkout_fence(
    conn: &mut rusqlite::Connection,
    fence: &CheckoutFence,
    now_ms: i64,
) -> SiteResult<CheckoutFenceAcquisition> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut current =
        checkout_fence_by_id(&tx, &fence.organization_id, &fence.id)?.ok_or(SiteError::NotFound)?;
    match current.status.as_str() {
        "open" => {
            tx.commit()?;
            Ok(CheckoutFenceAcquisition::Existing(current))
        }
        "pending" => {
            tx.execute(
                "UPDATE billing_checkout_fences SET updated_ms=?3 \
                 WHERE organization_id=?1 AND id=?2 AND status='pending'",
                params![fence.organization_id, fence.id, now_ms],
            )?;
            current.updated_ms = now_ms;
            tx.commit()?;
            Ok(CheckoutFenceAcquisition::Acquired(current))
        }
        _ => Err(SiteError::Conflict(
            "The checkout authority is no longer pending.",
        )),
    }
}

/// Record the provider checkout and activate the fence in one transaction.
pub fn activate_checkout_fence(
    conn: &mut rusqlite::Connection,
    fence: &CheckoutFence,
    provider_checkout: &ProviderCheckout,
    now_ms: i64,
) -> SiteResult<CheckoutRecord> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = checkout_fence_by_id(&tx, &fence.organization_id, &fence.id)?
        .ok_or_else(|| SiteError::internal_message("checkout fence vanished"))?;
    if current.status == "open" {
        if let Some(record) = checkout_by_id(&tx, &fence.organization_id, &fence.id)? {
            tx.commit()?;
            return Ok(record);
        }
    }
    if current.status != "pending" {
        return Err(SiteError::Conflict(
            "The checkout authority is no longer open.",
        ));
    }
    let checkout_url = provider_checkout_url(provider_checkout);
    let draft = CheckoutDraft {
        organization_id: &fence.organization_id,
        invoice_id: &fence.invoice_id,
        plan_id: &fence.plan_id,
        interval: &fence.interval,
        amount_minor: fence.amount_minor,
        currency: &fence.currency,
        provider: &fence.provider,
        idempotency_key: &fence.provider_idempotency_key,
        now_ms,
    };
    let record = insert_checkout_with_id(
        &tx,
        &fence.id,
        &draft,
        Some(provider_checkout.id.as_str()),
        checkout_url.as_deref(),
    )?;
    let expires_ms = now_ms.saturating_add(CHECKOUT_FENCE_TTL_MS);
    let changed = tx.execute(
        "UPDATE billing_checkout_fences \
         SET status='open',provider_checkout_id=?3,checkout_url=?4,updated_ms=?5,expires_ms=?6 \
         WHERE organization_id=?1 AND id=?2 AND status='pending'",
        params![
            fence.organization_id,
            fence.id,
            provider_checkout.id,
            checkout_url,
            now_ms,
            expires_ms
        ],
    )?;
    if changed != 1 {
        return Err(SiteError::Conflict(
            "The checkout authority changed concurrently.",
        ));
    }
    tx.commit()?;
    Ok(record)
}

pub fn fail_checkout_fence(
    conn: &rusqlite::Connection,
    org_id: &str,
    fence_id: &str,
    now_ms: i64,
) -> SiteResult<()> {
    set_fence_status(conn, org_id, fence_id, "pending", "failed", now_ms)
}

pub fn supersede_checkout_fence(
    conn: &rusqlite::Connection,
    org_id: &str,
    fence_id: &str,
    now_ms: i64,
) -> SiteResult<()> {
    set_fence_status(conn, org_id, fence_id, "pending", "superseded", now_ms)
}

/// Cancel the active checkout for one invoice; the next request may open a
/// new provider checkout.
pub fn cancel_checkout_fence(
    conn: &rusqlite::Connection,
    org_id: &str,
    invoice_id: &str,
    now_ms: i64,
) -> SiteResult<()> {
    conn.execute(
        "UPDATE billing_checkout_fences SET status='canceled',updated_ms=?3 \
         WHERE organization_id=?1 AND invoice_id=?2 AND status IN ('pending','open')",
        params![org_id, invoice_id, now_ms],
    )?;
    conn.execute(
        "UPDATE billing_checkouts SET status='canceled',updated_ms=?3 \
         WHERE organization_id=?1 AND invoice_id=?2 AND status='open'",
        params![org_id, invoice_id, now_ms],
    )?;
    Ok(())
}

pub fn insert_checkout(
    conn: &rusqlite::Connection,
    draft: &CheckoutDraft<'_>,
    provider_checkout_id: Option<&str>,
    checkout_url: Option<&str>,
) -> SiteResult<CheckoutRecord> {
    let id = format!("chk_{}", Uuid::new_v4());
    insert_checkout_with_id(conn, &id, draft, provider_checkout_id, checkout_url)
}

pub fn insert_checkout_with_id(
    conn: &rusqlite::Connection,
    id: &str,
    draft: &CheckoutDraft<'_>,
    provider_checkout_id: Option<&str>,
    checkout_url: Option<&str>,
) -> SiteResult<CheckoutRecord> {
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
        id: id.to_string(),
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
    conn.execute(
        "UPDATE billing_checkout_fences SET status='paid',updated_ms=?3 \
         WHERE organization_id=?1 AND invoice_id=?2 AND status IN ('pending','open')",
        params![org_id, invoice_id, now_ms],
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
