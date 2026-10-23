//! `faktor billing`: local operator parity with the commercial account
//! routes.
//!
//! The metering subcommands (`overview`, `usage`, `subscription`, `credits`)
//! open the same durable billing database the daemon uses. The payment
//! subcommands (`invoices`, `invoice`, `payments`, `subscribe`, `seats`,
//! `tax`) open the same `payment.db` and call the same `PaymentService` the
//! additive native routes use. No second implementation exists.
//!
//! Output is strict JSON; money fields remain exact integer minor units and
//! the tax/price surfaces also carry exact formatted strings. A disabled
//! section refuses typed, never silently no-ops.

use std::sync::Arc;

use clap::Subcommand;
use faktor_cloud::{
    csv_field, BillingAccountId, BillingProration, CreditAppend, EntitlementService,
    OrganizationId, SubscriptionStatus, SystemClock,
};
use faktor_payments::{
    BillingInterval, ChangeSeatsRequest, PaymentService, SqlitePaymentStore, SubscribeRequest,
};
use serde_json::{json, Value};

use crate::config::Config;
use crate::load_entry_config;

/// The `faktor billing` subcommands.
#[derive(Subcommand)]
pub(crate) enum BillingAction {
    /// The durable account/subscription overview and typed capability states.
    Overview,
    /// One bounded page of the durable usage ledger AS LAST WRITTEN (the
    /// daemon's `/native/usage` performs the read-through projection of new
    /// session spend first). JSON or CSV.
    Usage {
        /// Exclusive event cursor (the previous page's `nextCursor`).
        #[arg(long)]
        since: Option<String>,
        /// Page size (1..=5000, default 200).
        #[arg(long)]
        limit: Option<u64>,
        /// `json` (default) or `csv`.
        #[arg(long, default_value = "json")]
        format: String,
    },
    /// Change (upgrade/downgrade) or cancel the configured subscription.
    Subscription {
        #[command(subcommand)]
        action: BillingSubscriptionAction,
    },
    /// Admin-only durable credit grant (the local operator is the owner).
    Credits {
        #[command(subcommand)]
        action: BillingCreditsAction,
    },
    /// List/filter durable invoices (JSON or CSV export).
    Invoices {
        /// Exclusive invoice cursor (the previous page's `nextCursor`).
        #[arg(long)]
        since: Option<String>,
        /// Page/export bound (1..=5000, default 200).
        #[arg(long)]
        limit: Option<u64>,
        /// `json` (default) or `csv`.
        #[arg(long, default_value = "json")]
        format: String,
    },
    /// Read one invoice by id (JSON, or its line-item CSV).
    Invoice {
        /// The invoice id (`inv_...`).
        id: String,
        /// `json` (default) or `csv`.
        #[arg(long, default_value = "json")]
        format: String,
    },
    /// List durable payments and their refund state.
    Payments {
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Start the configured paid subscription.
    Subscribe {
        /// A priced plan id from `[billing.payments.prices]`.
        #[arg(long)]
        plan: String,
        /// `monthly` (default) or `annual`.
        #[arg(long, default_value = "monthly")]
        interval: String,
        #[arg(long, default_value_t = 1)]
        seats: u32,
        /// Optional trial length in days (bounded to 365).
        #[arg(long)]
        trial_days: Option<u32>,
        #[arg(long, default_value = "owner@example.com")]
        email: String,
        #[arg(long, default_value = "Local Owner")]
        name: String,
        /// Replay key (a retry with the same key performs no second effect).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Change the subscription's seat quantity (durable proration).
    Seats {
        #[arg(long)]
        quantity: u32,
        /// Subscription id; omitted = the organization's current subscription.
        #[arg(long)]
        subscription: Option<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// The configured tax jurisdictions and exact plan price catalogue.
    Tax,
}

/// The `faktor billing credits` subcommands.
#[derive(Subcommand)]
pub(crate) enum BillingCreditsAction {
    /// Append one grant to the account's credit ledger.
    Grant {
        /// Non-negative micro-credit amount (decimal).
        #[arg(long)]
        amount_micro: u64,
        /// Human-readable reason recorded in the ledger.
        #[arg(long, default_value = "local operator grant")]
        reason: String,
        /// Optional replay key; a duplicate key appends nothing.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

/// The `faktor billing subscription` subcommands.
#[derive(Subcommand)]
pub(crate) enum BillingSubscriptionAction {
    /// Change the plan (the effect is immediate at the next admission).
    Change {
        /// A plan id from the `[billing.plans]` table.
        plan: String,
        /// `active` (default), `trialing` or `expired`.
        #[arg(long, default_value = "active")]
        status: String,
        /// Trial/plan expiry in milliseconds since the Unix epoch.
        #[arg(long)]
        expires_ms: Option<i64>,
    },
    /// Cancel the subscription (new admissions are denied immediately).
    Cancel {
        /// Required: cancellation is consequential.
        #[arg(long)]
        confirm: bool,
    },
}

const MAX_CLI_EXPORT: usize = 5_000;

fn service(
    config: &Config,
    data_dir: &std::path::Path,
) -> Result<(Arc<EntitlementService>, OrganizationId), String> {
    let Some(billing_config) = config
        .billing
        .service_config()
        .map_err(|e| format!("billing: {e}"))?
    else {
        return Err(
            "the [billing] section is disabled (set `enabled = true`, `organization`, `plans`)"
                .into(),
        );
    };
    let organization = config
        .billing
        .organization()
        .map_err(|e| format!("billing: {e}"))?;
    let path = config
        .billing
        .billing_path(data_dir)
        .map_err(|e| format!("billing: {e}"))?
        .ok_or("billing: enabled without a database path")?;
    let store = Arc::new(
        faktor_cloud::SqliteControlPlaneStore::open(&path)
            .map_err(|e| format!("billing store {}: {e}", path.display()))?,
    );
    let service = EntitlementService::new_for_organization(
        store,
        Arc::new(SystemClock),
        billing_config,
        Some(organization.clone()),
    )
    .map_err(|e| format!("billing service: {e}"))?;
    Ok((service, organization))
}

fn payments_service(
    config: &Config,
    data_dir: &std::path::Path,
) -> Result<(Arc<PaymentService>, OrganizationId), String> {
    let Some(payments_cfg) = config
        .billing
        .payments
        .as_ref()
        .filter(|payments| payments.enabled)
    else {
        return Err(
            "the [billing.payments] section is disabled (enable it and configure `prices`)".into(),
        );
    };
    let runtime = payments_cfg
        .runtime_config(&config.billing.plans)
        .map_err(|e| format!("billing payments: {e}"))?;
    let path = config
        .billing
        .payments_path(data_dir)
        .map_err(|e| format!("billing payments: {e}"))?
        .ok_or("billing payments: enabled without a database path")?;
    let store = Arc::new(
        SqlitePaymentStore::open(&path)
            .map_err(|e| format!("payment store {}: {e}", path.display()))?,
    );
    let transport = if runtime.provider == faktor_payments::ProviderKind::Stripe {
        let policy = config
            .sandbox_policy()
            .map_err(|e| format!("billing payments: sandbox policy: {e}"))?
            .network
            .installed()
            .cloned()
            .unwrap_or_else(faktor_security::destination::DestinationPolicy::empty);
        let transport =
            faktor_provider::egress::PolicyCheckedHttpTransport::try_with_policy(policy)
                .map_err(|e| format!("billing payments: egress client: {e}"))?;
        Some(Arc::new(transport) as Arc<dyn faktor_provider::egress::HttpTransport>)
    } else {
        None
    };
    let clock: Arc<dyn faktor_cloud::Clock> = Arc::new(SystemClock);
    let service = faktor_payments::build_service(
        store,
        runtime,
        clock,
        transport,
        Arc::new(faktor_payments::EnvSecrets),
    )
    .map_err(|e| format!("billing payments: {e}"))?;
    Ok((
        Arc::new(service),
        config
            .billing
            .organization()
            .map_err(|e| format!("billing: {e}"))?,
    ))
}

fn account_exists(
    service: &EntitlementService,
    organization: &OrganizationId,
) -> Result<BillingAccountId, String> {
    service
        .default_account(organization)
        .map_err(|e| format!("billing account: {e}"))?
        .ok_or_else(|| {
            "the organization has no billing account (start the daemon with [billing] enabled once)"
                .into()
        })
}

fn usage_page(
    service: &EntitlementService,
    organization: &OrganizationId,
    since: Option<String>,
    limit: Option<u64>,
    format: &str,
) -> Result<Value, String> {
    if format != "json" && format != "csv" {
        return Err("usage: --format must be json or csv".into());
    }
    let limit = match limit {
        None => 200usize,
        Some(0) => return Err("usage: --limit must be >= 1".into()),
        Some(value) if value as usize > MAX_CLI_EXPORT => {
            return Err(format!(
                "usage: --limit exceeds the export bound {MAX_CLI_EXPORT}"
            ))
        }
        Some(value) => value as usize,
    };
    let page = service
        .usage_page(organization, since.as_deref(), limit)
        .map_err(|e| format!("usage: {e}"))?;
    if format == "csv" {
        let mut text = String::from(
            "cursor,event_id,task_id,run_id,attempt_id,provider,model,unit,quantity,provider_cost_micro,category,reconciliation_state,occurred_at_ms,source_operation,correction_of\n",
        );
        for row in &page.items {
            let event = &row.event;
            let fields = [
                row.event_seq.to_string(),
                event.id.as_str().to_string(),
                event.task_id.to_string(),
                event.run_id.clone(),
                event.attempt_id.clone(),
                event.provider.clone(),
                event.model.clone(),
                event.unit.as_str().to_string(),
                event.quantity.to_string(),
                event.provider_cost_micro.to_string(),
                event.category.as_str().to_string(),
                event.reconciliation_state.as_str().to_string(),
                event.occurred_at_ms.to_string(),
                event.source_operation.clone(),
                event
                    .correction_of
                    .as_ref()
                    .map(|id| id.as_str().to_string())
                    .unwrap_or_default(),
            ];
            text.push_str(&fields.map(|field| csv_field(&field)).join(","));
            text.push('\n');
        }
        return Ok(Value::String(text));
    }
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|row| json!({ "cursor": row.event_seq, "event": &row.event }))
        .collect();
    Ok(json!({
        "items": items,
        "nextCursor": page.next_cursor,
        "truncated": page.next_cursor.is_some(),
    }))
}

fn interval(raw: &str) -> Result<BillingInterval, String> {
    BillingInterval::parse(raw)
        .ok_or_else(|| format!("interval must be monthly or annual (got {raw:?})"))
}

fn new_idempotency_key(raw: Option<String>, operation: &str) -> String {
    raw.unwrap_or_else(|| format!("cli-{operation}-{}", uuid_like()))
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{now:x}-{}", COUNTER.fetch_add(1, Ordering::SeqCst))
}

fn payment_limit(limit: Option<u64>, operation: &str) -> Result<usize, String> {
    match limit {
        None => Ok(200),
        Some(0) => Err(format!("{operation}: --limit must be >= 1")),
        Some(value) if value > MAX_CLI_EXPORT as u64 => Err(format!(
            "{operation}: --limit exceeds the export bound {MAX_CLI_EXPORT}"
        )),
        Some(value) => Ok(value as usize),
    }
}

fn price_surface(service: &PaymentService) -> Value {
    let mut prices = serde_json::Map::new();
    for (plan_id, price) in &service.config().catalogue.plans {
        prices.insert(
            plan_id.clone(),
            json!({
                "currency": price.currency,
                "monthlyMinor": price.monthly_minor,
                "annualMinor": price.annual_minor,
                "perSeatMonthlyMinor": price.per_seat_monthly_minor,
                "perSeatAnnualMinor": price.per_seat_annual_minor,
                "formattedMonthly": service.format_money(price.monthly_minor, &price.currency),
                "formattedAnnual": service.format_money(price.annual_minor, &price.currency),
            }),
        );
    }
    Value::Object(prices)
}

async fn render_impl(
    action: BillingAction,
    data_dir: &std::path::Path,
    config_path: Option<std::path::PathBuf>,
) -> Result<Value, String> {
    let (config, _semantic) = load_entry_config(data_dir, config_path)?;
    match action {
        BillingAction::Overview => {
            let (service, organization) = service(&config, data_dir)?;
            let overview = service
                .overview(&organization)
                .map_err(|e| format!("billing overview: {e}"))?;
            let mut value = json!({ "billing": overview });
            if config
                .billing
                .payments
                .as_ref()
                .is_some_and(|payments| payments.enabled)
            {
                let (payments, _) = payments_service(&config, data_dir)?;
                let real = payments
                    .overview(&organization)
                    .map_err(|e| format!("billing payments overview: {e}"))?;
                if let Some(billing) = value["billing"].as_object_mut() {
                    billing.insert("payment".into(), json!(real.payment));
                    billing.insert("invoices".into(), json!(real.invoices));
                    billing.insert("seats".into(), json!(real.seats_capability));
                    billing.insert("tax".into(), json!(real.tax_capability));
                    billing.insert("currency".into(), json!(real.currency_capability));
                    billing.insert("payments".into(), json!(real));
                }
            }
            Ok(value)
        }
        BillingAction::Usage {
            since,
            limit,
            format,
        } => {
            let (service, organization) = service(&config, data_dir)?;
            usage_page(&service, &organization, since, limit, &format)
        }
        BillingAction::Credits { action } => {
            let (service, organization) = service(&config, data_dir)?;
            let account = account_exists(&service, &organization)?;
            match action {
                BillingCreditsAction::Grant {
                    amount_micro,
                    reason,
                    idempotency_key,
                } => {
                    let append = service
                        .grant_credits(
                            &organization,
                            &account,
                            amount_micro,
                            &reason,
                            idempotency_key.as_deref(),
                        )
                        .map_err(|e| format!("credits grant: {e}"))?;
                    let credits = service
                        .credit_balance(&organization)
                        .map_err(|e| format!("credits balance: {e}"))?;
                    let outcome = match append {
                        CreditAppend::Appended => "appended",
                        CreditAppend::Duplicate => "duplicate",
                    };
                    Ok(json!({ "append": outcome, "credits": credits }))
                }
            }
        }
        BillingAction::Subscription { action } => {
            let (service, organization) = service(&config, data_dir)?;
            let _ = account_exists(&service, &organization)?;
            match action {
                BillingSubscriptionAction::Change {
                    plan,
                    status,
                    expires_ms,
                } => {
                    let status = match status.as_str() {
                        "active" => SubscriptionStatus::Active,
                        "trialing" => SubscriptionStatus::Trialing,
                        "expired" => SubscriptionStatus::Expired,
                        "canceled" => {
                            return Err(
                                "subscription change: use `subscription cancel --confirm`".into()
                            )
                        }
                        other => {
                            return Err(format!(
                                "subscription change: unknown status {other:?} (active|trialing|expired)"
                            ))
                        }
                    };
                    let (subscription, replayed) = service
                        .change_subscription(&organization, &plan, status, expires_ms)
                        .map_err(|e| format!("subscription change: {e}"))?;
                    let billing = service
                        .overview(&organization)
                        .map_err(|e| format!("billing overview: {e}"))?;
                    Ok(json!({
                        "replayed": replayed,
                        "subscription": subscription,
                        "proration": BillingProration::unavailable_no_prices(),
                        "billing": billing,
                    }))
                }
                BillingSubscriptionAction::Cancel { confirm } => {
                    if !confirm {
                        return Err(
                            "subscription cancel: pass --confirm (cancellation denies new admissions)"
                                .into(),
                        );
                    }
                    let (subscription, replayed) = service
                        .cancel_subscription(&organization)
                        .map_err(|e| format!("subscription cancel: {e}"))?;
                    let billing = service
                        .overview(&organization)
                        .map_err(|e| format!("billing overview: {e}"))?;
                    Ok(json!({
                        "replayed": replayed,
                        "subscription": subscription,
                        "proration": BillingProration::unavailable_no_prices(),
                        "billing": billing,
                    }))
                }
            }
        }
        BillingAction::Invoices {
            since,
            limit,
            format,
        } => {
            let (service, organization) = payments_service(&config, data_dir)?;
            if format != "json" && format != "csv" {
                return Err("invoices: --format must be json or csv".into());
            }
            let limit = payment_limit(limit, "invoices")?;
            if format == "csv" {
                return service
                    .export_invoices(&organization, "csv", limit)
                    .map(Value::String)
                    .map_err(|e| format!("invoices: {e}"));
            }
            let page = service
                .invoices(&organization, since.as_deref(), limit)
                .map_err(|e| format!("invoices: {e}"))?;
            Ok(json!({
                "items": page.items,
                "nextCursor": page.next_cursor,
                "hasMore": page.has_more,
            }))
        }
        BillingAction::Invoice { id, format } => {
            let (service, organization) = payments_service(&config, data_dir)?;
            let invoice = service
                .invoice(&organization, &id)
                .map_err(|e| format!("invoice: {e}"))?;
            if format == "csv" {
                return Ok(Value::String(service.export_invoice_csv(&invoice)));
            }
            if format != "json" {
                return Err("invoice: --format must be json or csv".into());
            }
            Ok(json!({ "invoice": invoice }))
        }
        BillingAction::Payments { limit } => {
            let (service, organization) = payments_service(&config, data_dir)?;
            let limit = payment_limit(limit, "payments")?;
            let payments = service
                .payments(&organization, limit)
                .map_err(|e| format!("payments: {e}"))?;
            Ok(json!({ "payments": payments }))
        }
        BillingAction::Subscribe {
            plan,
            interval: interval_raw,
            seats,
            trial_days,
            email,
            name,
            idempotency_key,
        } => {
            let (service, organization) = payments_service(&config, data_dir)?;
            let outcome = service
                .subscribe(
                    &organization,
                    SubscribeRequest {
                        plan_id: plan,
                        interval: interval(&interval_raw)?,
                        seats,
                        trial_days,
                        customer_email: email,
                        customer_name: name,
                        idempotency_key: idempotency_key
                            .unwrap_or_else(|| new_idempotency_key(None, "subscribe")),
                    },
                )
                .await
                .map_err(|e| format!("subscribe: {e}"))?;
            Ok(json!({
                "replayed": outcome.replayed,
                "subscription": outcome.subscription,
                "overview": service.overview(&organization).map_err(|e| format!("overview: {e}"))?,
            }))
        }
        BillingAction::Seats {
            quantity,
            subscription,
            idempotency_key,
        } => {
            let (service, organization) = payments_service(&config, data_dir)?;
            let subscription = match subscription {
                Some(id) => id,
                None => service
                    .overview(&organization)
                    .map_err(|e| format!("seats: {e}"))?
                    .subscription
                    .map(|subscription| subscription.id)
                    .ok_or("seats: the organization has no payment subscription")?,
            };
            let outcome = service
                .change_seats(
                    &organization,
                    ChangeSeatsRequest {
                        subscription_id: subscription,
                        quantity,
                        idempotency_key: idempotency_key
                            .unwrap_or_else(|| new_idempotency_key(None, "seats")),
                    },
                )
                .await
                .map_err(|e| format!("seats: {e}"))?;
            Ok(json!({
                "replayed": outcome.replayed,
                "subscription": outcome.subscription,
                "proration": outcome.proration,
                "prorationText": service.preview_text(&outcome.proration),
            }))
        }
        BillingAction::Tax => {
            let (service, organization) = payments_service(&config, data_dir)?;
            let overview = service
                .overview(&organization)
                .map_err(|e| format!("tax: {e}"))?;
            Ok(json!({
                "tax": service.tax_surface(),
                "prices": price_surface(&service),
                "currency": overview.currency,
                "currencyExponent": overview.currency_exponent,
                "provider": overview.provider,
            }))
        }
    }
}

/// Render one billing subcommand synchronously (used by tests and local
/// callers that are not already inside a Tokio runtime). New payment
/// subcommands use the same async implementation.
#[allow(dead_code)]
pub(crate) fn render(
    action: BillingAction,
    data_dir: &std::path::Path,
    config_path: Option<std::path::PathBuf>,
) -> Result<Value, String> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(
            "billing: this entry point must not be called inside an async runtime; use \
             `render_async`"
                .into(),
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("billing runtime: {e}"))?;
    runtime.block_on(render_impl(action, data_dir, config_path))
}

/// The async renderer (the daemon/CLI entry point).
pub(crate) async fn render_async(
    action: BillingAction,
    data_dir: &std::path::Path,
    config_path: Option<std::path::PathBuf>,
) -> Result<Value, String> {
    render_impl(action, data_dir, config_path).await
}

/// The CLI entry: render, then print (JSON pretty) or refuse loudly.
pub(crate) async fn command(
    action: BillingAction,
    data_dir: std::path::PathBuf,
    config_path: Option<std::path::PathBuf>,
) {
    match render_async(action, &data_dir, config_path).await {
        Ok(Value::String(text)) => print!("{text}"),
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        ),
        Err(error) => {
            eprintln!("faktor billing: {error}");
            std::process::exit(1);
        }
    }
}
