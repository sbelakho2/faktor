//! `faktor billing`: local operator parity with the commercial account
//! routes (`GET /native/billing/overview`, `GET /native/usage/export`,
//! `POST /native/billing/subscription`).
//!
//! This is a LOCAL admin surface: it opens the same durable billing database
//! the daemon uses and calls the same `EntitlementService` methods, so the
//! effect is the same row the HTTP routes mutate (no second implementation).
//! It does NOT perform read-through session ingestion: `usage` reports the
//! durable ledger as last written, while the daemon's `/native/usage` folds
//! new session spend before reading. The output is strict JSON, money fields
//! are decimal strings, and a disabled `[billing]` section refuses typed.

use clap::Subcommand;
use faktor_cloud::{
    csv_field, BillingAccountId, BillingProration, CreditAppend, EntitlementService,
    OrganizationId, SubscriptionStatus, SystemClock,
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
) -> Result<(std::sync::Arc<EntitlementService>, OrganizationId), String> {
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
    let store = std::sync::Arc::new(
        faktor_cloud::SqliteControlPlaneStore::open(&path)
            .map_err(|e| format!("billing store {}: {e}", path.display()))?,
    );
    let service = EntitlementService::new_for_organization(
        store,
        std::sync::Arc::new(SystemClock),
        billing_config,
        Some(organization.clone()),
    )
    .map_err(|e| format!("billing service: {e}"))?;
    Ok((service, organization))
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

/// Render one billing subcommand as JSON (or the exact CSV text for
/// `usage --format csv`). No stdout/exit here so the parity is testable.
pub(crate) fn render(
    action: BillingAction,
    data_dir: &std::path::Path,
    config_path: Option<std::path::PathBuf>,
) -> Result<Value, String> {
    let (config, _semantic) = load_entry_config(data_dir, config_path)?;
    let (service, organization) = service(&config, data_dir)?;
    match action {
        BillingAction::Overview => service
            .overview(&organization)
            .map(|overview| json!({ "billing": overview }))
            .map_err(|e| format!("billing overview: {e}")),
        BillingAction::Usage {
            since,
            limit,
            format,
        } => usage_page(&service, &organization, since, limit, &format),
        BillingAction::Credits { action } => {
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
    }
}

/// The CLI entry: render, then print (JSON pretty) or refuse loudly.
pub(crate) fn command(
    action: BillingAction,
    data_dir: std::path::PathBuf,
    config_path: Option<std::path::PathBuf>,
) {
    match render(action, &data_dir, config_path) {
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
