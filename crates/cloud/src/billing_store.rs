//! The durable billing seam: the [`BillingStore`] trait plus its in-memory
//! implementation and the additive SQLite implementation over the SAME
//! [`crate::store::SqliteControlPlaneStore`] database and migration ladder
//! (its migration v2 — the crate-owned next `user_version`).
//!
//! Append-only enforcement lives here: usage events and credit entries are
//! INSERT-only (no UPDATE/DELETE surface exists), ingestion is idempotent
//! per `(organization, source_key)`, and every credit mutation runs in ONE
//! immediate transaction that re-derives the balance before the insert, so
//! two racing consumes can never jointly overdraw the account and a refund
//! can never exceed what its referenced consume/settle actually spent.

use std::collections::BTreeMap;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::billing::{
    fold_credits, BillingAccount, CreditBalance, CreditEntry, CreditKind, InFlightTxn,
    Subscription, UsageEvent,
};
use crate::error::ControlPlaneError;
use crate::ids::{BillingAccountId, InFlightTxnId, OrganizationId};
use crate::store::{CloudStoreError, SqliteControlPlaneStore};

/// The SQL schema of the billing domain (migration v2 of the control-plane
/// ladder). Append-only by construction: no UPDATE/DELETE statement names
/// `usage_event` or `credit_entry` outside the store's own read paths.
pub const BILLING_SCHEMA_V2: &str = "
     CREATE TABLE IF NOT EXISTS billing_account (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_billing_account_org
        ON billing_account(organization_id, id);
     CREATE TABLE IF NOT EXISTS billing_subscription (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL UNIQUE,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_billing_subscription_org
        ON billing_subscription(organization_id);
     CREATE TABLE IF NOT EXISTS usage_event (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        event_seq INTEGER NOT NULL,
        source_key TEXT NOT NULL,
        billing_account_id TEXT NOT NULL,
        task_id INTEGER NOT NULL,
        run_id TEXT NOT NULL,
        attempt_id TEXT NOT NULL,
        category TEXT NOT NULL,
        correction_of TEXT,
        occurred_at_ms INTEGER NOT NULL,
        reconciliation_state TEXT NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, source_key)
     );
     CREATE INDEX IF NOT EXISTS idx_usage_event_org_seq
        ON usage_event(organization_id, event_seq);
     CREATE INDEX IF NOT EXISTS idx_usage_event_org_task
        ON usage_event(organization_id, task_id, event_seq);
     CREATE INDEX IF NOT EXISTS idx_usage_event_correction
        ON usage_event(organization_id, correction_of);
     CREATE TABLE IF NOT EXISTS usage_event_seq (
        organization_id TEXT PRIMARY KEY,
        next_seq INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS credit_entry (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        entry_seq INTEGER NOT NULL,
        billing_account_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        reference TEXT,
        usage_event_id TEXT,
        idempotency_key TEXT,
        amount_micro INTEGER NOT NULL,
        occurred_at_ms INTEGER NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, idempotency_key)
     );
     CREATE INDEX IF NOT EXISTS idx_credit_entry_org_seq
        ON credit_entry(organization_id, entry_seq);
     CREATE INDEX IF NOT EXISTS idx_credit_entry_org_ref
        ON credit_entry(organization_id, reference);
     CREATE TABLE IF NOT EXISTS billing_credit_seq (
        organization_id TEXT PRIMARY KEY,
        next_seq INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS billing_in_flight (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        reference TEXT NOT NULL,
        started_ms INTEGER NOT NULL,
        ended_ms INTEGER,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_billing_in_flight_org
        ON billing_in_flight(organization_id, ended_ms);
";

/// One stored usage event with its immutable durable order (`event_seq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUsageEvent {
    pub event_seq: i64,
    pub event: UsageEvent,
}

/// The SQL schema of the durable billing-report schedule (migration v4 of
/// the control-plane ladder). Append-only in spirit: rows advance through
/// their open -> reported/failed states, and a REPORTED/FAILED period is
/// terminal (the runner never re-opens it), so a period can never be
/// double-reported even across a crash or restart.
pub const BILLING_REPORT_SCHEMA_V4: &str = "
     CREATE TABLE IF NOT EXISTS billing_report_period (
        organization_id TEXT NOT NULL,
        period TEXT NOT NULL,
        status TEXT NOT NULL,
        next_attempt_at_ms INTEGER NOT NULL,
        first_seen_ms INTEGER NOT NULL,
        payload TEXT NOT NULL,
        PRIMARY KEY (organization_id, period)
     );
     CREATE INDEX IF NOT EXISTS idx_billing_report_period_due
        ON billing_report_period(organization_id, status, next_attempt_at_ms);
";

/// The status of one reporting period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportPeriodStatus {
    /// Open: not reported yet (possibly mid-report with a continuation
    /// cursor, or waiting for its retry backoff).
    Open,
    /// Reported to completion; terminal (never double-reported).
    Reported,
    /// Terminally failed (a final vendor refusal or exhausted attempts);
    /// never retried.
    Failed,
}

impl ReportPeriodStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReportPeriodStatus::Open => "open",
            ReportPeriodStatus::Reported => "reported",
            ReportPeriodStatus::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "open" => Some(ReportPeriodStatus::Open),
            "reported" => Some(ReportPeriodStatus::Reported),
            "failed" => Some(ReportPeriodStatus::Failed),
            _ => None,
        }
    }
}

/// One durable report period row: the schedule's unit of exactly-once
/// reporting. `next_cursor` is the vendor continuation cursor INSIDE the
/// period (recorded before the next page is sent, so a crash replays the
/// same page key and the vendor de-duplicates).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportPeriodRow {
    pub organization_id: String,
    pub period: String,
    #[serde(default)]
    pub next_cursor: Option<String>,
    pub status: ReportPeriodStatus,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    pub first_seen_ms: i64,
    #[serde(default)]
    pub reported_at_ms: Option<i64>,
    pub next_attempt_at_ms: i64,
}

/// Bound on the retained report periods of ONE organization (oldest rows
/// are pruned on insert; the schedule is bounded over an unbounded lifetime).
pub const MAX_REPORT_PERIODS_PER_ORG: usize = 512;

/// One stored credit entry with its immutable durable order (`entry_seq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCreditEntry {
    pub entry_seq: i64,
    pub entry: CreditEntry,
}

/// The outcome of one idempotent usage append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageAppend {
    /// The event is durable (a new row).
    Appended,
    /// The `(organization, source_key)` fact was already recorded: nothing
    /// was written and nothing is double-counted.
    Duplicate,
}

/// The typed refusal of one credit append.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CreditAppendRefusal {
    #[error(
        "insufficient credits: {available} microUSD available, {requested} microUSD requested \
         (nothing was written)"
    )]
    InsufficientCredits { available: u64, requested: u64 },
    #[error("credit reference {0} does not exist")]
    UnknownReference(String),
    #[error("credit reference {reference} is a {kind}, not a consume")]
    NotAConsume { reference: String, kind: String },
    #[error("consume {reference} was already settled")]
    AlreadySettled { reference: String },
    #[error(
        "refund refused: {requested} microUSD exceeds the {refundable} microUSD left of consume \
         {reference} (nothing was written)"
    )]
    RefundExceedsConsumed {
        reference: String,
        refundable: u64,
        requested: u64,
    },
    #[error("the idempotency key {0:?} was already used for a different credit entry")]
    IdempotencyConflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BillingStoreError {
    #[error("billing store backend unavailable: {0}")]
    Backend(String),
    #[error("billing store refused a malformed row: {0}")]
    Malformed(String),
    #[error(transparent)]
    Credit(#[from] CreditAppendRefusal),
}

impl From<CloudStoreError> for BillingStoreError {
    fn from(e: CloudStoreError) -> Self {
        match e {
            CloudStoreError::Backend(m) => BillingStoreError::Backend(m),
            CloudStoreError::Malformed(m) => BillingStoreError::Malformed(m),
        }
    }
}

impl From<BillingStoreError> for ControlPlaneError {
    fn from(e: BillingStoreError) -> Self {
        match e {
            BillingStoreError::Backend(m) => ControlPlaneError::Backend(m),
            BillingStoreError::Malformed(m) => ControlPlaneError::Malformed(m),
            BillingStoreError::Credit(CreditAppendRefusal::InsufficientCredits {
                available,
                requested,
            }) => ControlPlaneError::Conflict(format!(
                "insufficient credits: {available} microUSD available, {requested} requested"
            )),
            BillingStoreError::Credit(other) => ControlPlaneError::Conflict(other.to_string()),
        }
    }
}

/// The durable billing seam. Object-safe: the entitlement service holds one
/// `Arc<dyn BillingStore>`.
pub trait BillingStore: Send + Sync {
    fn put_billing_account(&self, account: &BillingAccount) -> Result<(), BillingStoreError>;
    fn billing_account(
        &self,
        organization: &OrganizationId,
        id: &BillingAccountId,
    ) -> Result<Option<BillingAccount>, BillingStoreError>;
    fn billing_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BillingAccount>, BillingStoreError>;

    fn put_subscription(&self, subscription: &Subscription) -> Result<(), BillingStoreError>;
    fn subscription(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Subscription>, BillingStoreError>;

    /// Append one usage event (idempotent per `(organization, source_key)`).
    fn append_usage_event(&self, event: &UsageEvent) -> Result<UsageAppend, BillingStoreError>;
    fn usage_event(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Option<StoredUsageEvent>, BillingStoreError>;
    /// One ascending page of stored events (cursor = `event_seq`).
    fn usage_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError>;
    /// Every event of one organization, ascending (bounded by `limit`).
    fn usage_events_of_task(
        &self,
        organization: &OrganizationId,
        task_id: u64,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError>;
    /// Whether a correction already names `base` (used to refuse double
    /// corrections; the fold itself tolerates chains).
    fn correction_exists(
        &self,
        organization: &OrganizationId,
        base: &str,
    ) -> Result<bool, BillingStoreError>;

    /// Append one credit entry under the store's balance/refund guards.
    fn append_credit_entry(&self, entry: &CreditEntry) -> Result<CreditAppend, BillingStoreError>;
    fn credit_entries(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredCreditEntry>, BillingStoreError>;
    /// One recorded credit entry by its idempotency key (the DURABLE
    /// identity of a record-before-call consume), `None` when no entry of
    /// this organization claimed the key. Additive read used by the
    /// agent-side debit adapter to settle/refund the hold it opened (the
    /// service's append returns the key's outcome, not the entry id).
    fn credit_entry_by_idempotency_key(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<StoredCreditEntry>, BillingStoreError>;
    fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, BillingStoreError>;

    fn begin_in_flight(&self, txn: &InFlightTxn) -> Result<(), BillingStoreError>;
    fn end_in_flight(
        &self,
        organization: &OrganizationId,
        id: &InFlightTxnId,
        at_ms: i64,
    ) -> Result<bool, BillingStoreError>;
    fn in_flight(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<InFlightTxn>, BillingStoreError>;

    /// Upsert one report period row (the record-before-send step: it is
    /// durable before any vendor call). Insertion prunes the organization's
    /// oldest rows beyond [`MAX_REPORT_PERIODS_PER_ORG`].
    fn put_report_period(&self, row: &ReportPeriodRow) -> Result<(), BillingStoreError>;
    /// One report period row (idempotency/restart resolution).
    fn report_period(
        &self,
        organization: &OrganizationId,
        period: &str,
    ) -> Result<Option<ReportPeriodRow>, BillingStoreError>;
    /// One bounded page of the organization's report rows, newest first.
    fn report_periods(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<Vec<ReportPeriodRow>, BillingStoreError>;
}

/// The outcome of one credit append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditAppend {
    Appended,
    /// The event's idempotency key was already claimed: the recorded entry
    /// is replayed and nothing is double-debited.
    Duplicate,
}

// ------------------------------------------------------------- in-memory

#[derive(Default)]
struct MemBilling {
    accounts: BTreeMap<String, BillingAccount>,
    subscriptions: BTreeMap<String, Subscription>,
    usage: BTreeMap<String, StoredUsageEvent>,
    usage_order: BTreeMap<String, Vec<String>>,
    credits: BTreeMap<String, StoredCreditEntry>,
    credit_order: BTreeMap<String, Vec<String>>,
    in_flight: BTreeMap<String, InFlightTxn>,
    report_periods: BTreeMap<String, ReportPeriodRow>,
}

/// In-memory [`BillingStore`] (tests and embedded hosts). Enforces exactly
/// the same append-only/idempotency/balance rules as the SQLite store.
#[derive(Default)]
pub struct MemoryBillingStore {
    inner: Mutex<MemBilling>,
}

impl MemoryBillingStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemBilling>, BillingStoreError> {
        self.inner
            .lock()
            .map_err(|_| BillingStoreError::Backend("in-memory billing lock is poisoned".into()))
    }
}

fn claim_credit(
    state: &mut MemBilling,
    entry: &CreditEntry,
) -> Result<CreditAppend, CreditAppendRefusal> {
    let org = entry.organization.as_str().to_string();
    if let Some(key) = &entry.idempotency_key {
        if let Some(existing) = state.credits.values().find(|row| {
            row.entry.organization == entry.organization
                && row.entry.idempotency_key.as_deref() == Some(key.as_str())
        }) {
            if existing.entry.kind == entry.kind
                && existing.entry.amount_micro == entry.amount_micro
                && existing.entry.reference == entry.reference
            {
                return Ok(CreditAppend::Duplicate);
            }
            return Err(CreditAppendRefusal::IdempotencyConflict(key.clone()));
        }
    }
    let entries: Vec<CreditEntry> = match state.credit_order.get(&org) {
        Some(ids) => ids
            .iter()
            .filter_map(|id| state.credits.get(id).map(|row| row.entry.clone()))
            .collect(),
        None => Vec::new(),
    };
    let balance = fold_credits(&entries);
    match entry.kind {
        CreditKind::Grant => {}
        CreditKind::Consume => {
            if entry.amount_micro > balance.balance_micro() {
                return Err(CreditAppendRefusal::InsufficientCredits {
                    available: balance.balance_micro(),
                    requested: entry.amount_micro,
                });
            }
        }
        CreditKind::Settle | CreditKind::Refund => {
            let reference = entry
                .reference
                .as_ref()
                .expect("validated: settle/refund carry a reference");
            let target = entries.iter().find(|row| &row.id == reference);
            let Some(target) = target else {
                return Err(CreditAppendRefusal::UnknownReference(
                    reference.as_str().to_string(),
                ));
            };
            if target.kind != CreditKind::Consume {
                return Err(CreditAppendRefusal::NotAConsume {
                    reference: reference.as_str().to_string(),
                    kind: target.kind.as_str().to_string(),
                });
            }
            let settle = entries.iter().find(|row| {
                row.kind == CreditKind::Settle && row.reference.as_ref() == Some(reference)
            });
            if entry.kind == CreditKind::Settle {
                if settle.is_some() {
                    return Err(CreditAppendRefusal::AlreadySettled {
                        reference: reference.as_str().to_string(),
                    });
                }
                // Only the DELTA above the pending hold must be covered: the
                // hold already reserved the consume's amount at write time.
                let delta = entry.amount_micro.saturating_sub(target.amount_micro);
                let available = balance.balance_micro().saturating_add(target.amount_micro);
                if delta > available {
                    return Err(CreditAppendRefusal::InsufficientCredits {
                        available,
                        requested: delta,
                    });
                }
            } else {
                // Refund exactness: what the consume actually spent (its
                // settle amount when settled, else the pending hold) minus
                // everything already refunded for it.
                let spent = settle
                    .map(|s| s.amount_micro)
                    .unwrap_or(target.amount_micro);
                let refunded: u64 = entries
                    .iter()
                    .filter(|row| {
                        row.kind == CreditKind::Refund && row.reference.as_ref() == Some(reference)
                    })
                    .map(|row| row.amount_micro)
                    .sum();
                let refundable = spent.saturating_sub(refunded);
                if entry.amount_micro > refundable {
                    return Err(CreditAppendRefusal::RefundExceedsConsumed {
                        reference: reference.as_str().to_string(),
                        refundable,
                        requested: entry.amount_micro,
                    });
                }
            }
        }
    }
    let seq = state.credit_order.get(&org).map(|v| v.len()).unwrap_or(0) as i64 + 1;
    state.credits.insert(
        entry.id.as_str().to_string(),
        StoredCreditEntry {
            entry_seq: seq,
            entry: entry.clone(),
        },
    );
    state
        .credit_order
        .entry(org)
        .or_default()
        .push(entry.id.as_str().to_string());
    Ok(CreditAppend::Appended)
}

impl BillingStore for MemoryBillingStore {
    fn put_billing_account(&self, account: &BillingAccount) -> Result<(), BillingStoreError> {
        self.lock()?
            .accounts
            .insert(account.id.as_str().to_string(), account.clone());
        Ok(())
    }

    fn billing_account(
        &self,
        organization: &OrganizationId,
        id: &BillingAccountId,
    ) -> Result<Option<BillingAccount>, BillingStoreError> {
        Ok(self
            .lock()?
            .accounts
            .get(id.as_str())
            .filter(|a| a.organization == *organization)
            .cloned())
    }

    fn billing_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BillingAccount>, BillingStoreError> {
        Ok(self
            .lock()?
            .accounts
            .values()
            .filter(|a| a.organization == *organization)
            .filter(|a| after.map(|c| a.id.as_str() > c).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn put_subscription(&self, subscription: &Subscription) -> Result<(), BillingStoreError> {
        self.lock()?.subscriptions.insert(
            subscription.organization.as_str().to_string(),
            subscription.clone(),
        );
        Ok(())
    }

    fn subscription(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Subscription>, BillingStoreError> {
        Ok(self
            .lock()?
            .subscriptions
            .get(organization.as_str())
            .cloned())
    }

    fn append_usage_event(&self, event: &UsageEvent) -> Result<UsageAppend, BillingStoreError> {
        let mut state = self.lock()?;
        let duplicate = state.usage.values().any(|row| {
            row.event.organization_id == event.organization_id
                && row.event.source_key == event.source_key
        });
        if duplicate {
            return Ok(UsageAppend::Duplicate);
        }
        let org = event.organization_id.as_str().to_string();
        let seq = state.usage_order.get(&org).map(|v| v.len()).unwrap_or(0) as i64 + 1;
        state.usage.insert(
            event.id.as_str().to_string(),
            StoredUsageEvent {
                event_seq: seq,
                event: event.clone(),
            },
        );
        state
            .usage_order
            .entry(org)
            .or_default()
            .push(event.id.as_str().to_string());
        Ok(UsageAppend::Appended)
    }

    fn usage_event(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Option<StoredUsageEvent>, BillingStoreError> {
        Ok(self
            .lock()?
            .usage
            .get(id)
            .filter(|row| row.event.organization_id == *organization)
            .cloned())
    }

    fn usage_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        let state = self.lock()?;
        let org = organization.as_str();
        let mut rows: Vec<StoredUsageEvent> = state
            .usage_order
            .get(org)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.usage.get(id).cloned())
                    .filter(|row| row.event_seq > after_seq)
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|row| row.event_seq);
        Ok(rows)
    }

    fn usage_events_of_task(
        &self,
        organization: &OrganizationId,
        task_id: u64,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<StoredUsageEvent> = state
            .usage
            .values()
            .filter(|row| {
                row.event.organization_id == *organization && row.event.task_id == task_id
            })
            .filter(|row| row.event_seq > after_seq)
            .cloned()
            .collect();
        rows.sort_by_key(|row| row.event_seq);
        rows.truncate(limit);
        Ok(rows)
    }

    fn correction_exists(
        &self,
        organization: &OrganizationId,
        base: &str,
    ) -> Result<bool, BillingStoreError> {
        Ok(self.lock()?.usage.values().any(|row| {
            row.event.organization_id == *organization
                && row.event.correction_of.as_ref().map(|c| c.as_str()) == Some(base)
        }))
    }

    fn append_credit_entry(&self, entry: &CreditEntry) -> Result<CreditAppend, BillingStoreError> {
        let mut state = self.lock()?;
        claim_credit(&mut state, entry).map_err(BillingStoreError::Credit)
    }

    fn credit_entries(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredCreditEntry>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<StoredCreditEntry> = state
            .credit_order
            .get(organization.as_str())
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.credits.get(id).cloned())
                    .filter(|row| row.entry_seq > after_seq)
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|row| row.entry_seq);
        Ok(rows)
    }

    fn credit_entry_by_idempotency_key(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<StoredCreditEntry>, BillingStoreError> {
        let state = self.lock()?;
        Ok(state
            .credits
            .values()
            .find(|row| {
                row.entry.organization == *organization
                    && row.entry.idempotency_key.as_deref() == Some(key)
            })
            .cloned())
    }

    fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, BillingStoreError> {
        let state = self.lock()?;
        let entries: Vec<CreditEntry> = state
            .credit_order
            .get(organization.as_str())
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| state.credits.get(id).map(|row| row.entry.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(fold_credits(&entries))
    }

    fn begin_in_flight(&self, txn: &InFlightTxn) -> Result<(), BillingStoreError> {
        self.lock()?
            .in_flight
            .insert(txn.id.as_str().to_string(), txn.clone());
        Ok(())
    }

    fn end_in_flight(
        &self,
        organization: &OrganizationId,
        id: &InFlightTxnId,
        at_ms: i64,
    ) -> Result<bool, BillingStoreError> {
        let mut state = self.lock()?;
        let Some(row) = state.in_flight.get_mut(id.as_str()) else {
            return Ok(false);
        };
        if row.organization != *organization {
            return Ok(false);
        }
        // Close exactly once: a second close changes nothing and reports
        // `false` (the same contract the SQLite store's guarded UPDATE has).
        if row.ended_ms.is_some() {
            return Ok(false);
        }
        row.ended_ms = Some(at_ms);
        Ok(true)
    }

    fn in_flight(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<InFlightTxn>, BillingStoreError> {
        Ok(self
            .lock()?
            .in_flight
            .values()
            .filter(|row| row.organization == *organization)
            .cloned()
            .collect())
    }

    fn put_report_period(&self, row: &ReportPeriodRow) -> Result<(), BillingStoreError> {
        let mut state = self.lock()?;
        let key = report_key(&row.organization_id, &row.period);
        state.report_periods.insert(key, row.clone());
        prune_report_periods(
            &mut state.report_periods,
            row.organization_id.as_str(),
            MAX_REPORT_PERIODS_PER_ORG,
        );
        Ok(())
    }

    fn report_period(
        &self,
        organization: &OrganizationId,
        period: &str,
    ) -> Result<Option<ReportPeriodRow>, BillingStoreError> {
        Ok(self
            .lock()?
            .report_periods
            .get(&report_key(organization.as_str(), period))
            .cloned())
    }

    fn report_periods(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<Vec<ReportPeriodRow>, BillingStoreError> {
        let state = self.lock()?;
        let mut rows: Vec<ReportPeriodRow> = state
            .report_periods
            .values()
            .filter(|row| row.organization_id == organization.as_str())
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            b.first_seen_ms
                .cmp(&a.first_seen_ms)
                .then_with(|| b.period.cmp(&a.period))
        });
        rows.truncate(limit);
        Ok(rows)
    }
}

fn report_key(organization: &str, period: &str) -> String {
    format!("{organization}\0{period}")
}

/// Prune one organization's report rows to the newest `bound` (by first
/// sighting, then period): the schedule stays bounded over an unbounded
/// lifetime. Terminal (reported/failed) rows are pruned first so an open
/// period is never silently dropped while an old terminal row survives.
fn prune_report_periods(
    rows: &mut BTreeMap<String, ReportPeriodRow>,
    organization: &str,
    bound: usize,
) {
    let mut mine: Vec<(String, bool, i64, String)> = rows
        .iter()
        .filter(|(_, row)| row.organization_id == organization)
        .map(|(key, row)| {
            (
                key.clone(),
                row.status == ReportPeriodStatus::Open,
                row.first_seen_ms,
                row.period.clone(),
            )
        })
        .collect();
    if mine.len() <= bound {
        return;
    }
    // Oldest first, terminal before open within the same age (an open
    // period is the more valuable row to keep).
    mine.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.3.cmp(&b.3))
    });
    let excess = mine.len().saturating_sub(bound);
    for (key, _, _, _) in mine.into_iter().take(excess) {
        rows.remove(&key);
    }
}

// ---------------------------------------------------------------- sqlite

fn backend(e: rusqlite::Error) -> BillingStoreError {
    BillingStoreError::Backend(e.to_string())
}

fn parse<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, BillingStoreError> {
    serde_json::from_str(payload)
        .map_err(|e| BillingStoreError::Malformed(format!("billing row payload: {e}")))
}

fn encode<T: Serialize>(value: &T) -> Result<String, BillingStoreError> {
    serde_json::to_string(value)
        .map_err(|e| BillingStoreError::Malformed(format!("billing row encode: {e}")))
}

impl SqliteControlPlaneStore {
    /// Run one credit append inside an IMMEDIATE transaction: the balance is
    /// re-derived from the durable rows and the guards applied before the
    /// insert, so concurrent writers can never overdraw or over-refund.
    fn credit_append_tx(
        conn: &mut Connection,
        entry: &CreditEntry,
    ) -> Result<CreditAppend, BillingStoreError> {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        if let Some(key) = &entry.idempotency_key {
            let existing: Option<(String, String, i64, Option<String>)> = tx
                .query_row(
                    "SELECT id, kind, amount_micro, reference FROM credit_entry
                     WHERE organization_id = ?1 AND idempotency_key = ?2",
                    params![entry.organization.as_str(), key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .map_err(backend)?;
            if let Some((_id, kind, amount, reference)) = existing {
                let same = kind == entry.kind.as_str()
                    && amount as u64 == entry.amount_micro
                    && reference.as_deref() == entry.reference.as_ref().map(|r| r.as_str());
                if same {
                    tx.commit().map_err(backend)?;
                    return Ok(CreditAppend::Duplicate);
                }
                return Err(BillingStoreError::Credit(
                    CreditAppendRefusal::IdempotencyConflict(key.clone()),
                ));
            }
        }
        let entries = read_credit_entries(&tx, &entry.organization)?;
        let balance = fold_credits(&entries);
        match entry.kind {
            CreditKind::Grant => {}
            CreditKind::Consume => {
                if entry.amount_micro > balance.balance_micro() {
                    return Err(BillingStoreError::Credit(
                        CreditAppendRefusal::InsufficientCredits {
                            available: balance.balance_micro(),
                            requested: entry.amount_micro,
                        },
                    ));
                }
            }
            CreditKind::Settle | CreditKind::Refund => {
                let reference = entry
                    .reference
                    .as_ref()
                    .expect("validated: settle/refund carry a reference");
                let target = entries.iter().find(|row| &row.id == reference);
                let Some(target) = target else {
                    return Err(BillingStoreError::Credit(
                        CreditAppendRefusal::UnknownReference(reference.as_str().to_string()),
                    ));
                };
                if target.kind != CreditKind::Consume {
                    return Err(BillingStoreError::Credit(
                        CreditAppendRefusal::NotAConsume {
                            reference: reference.as_str().to_string(),
                            kind: target.kind.as_str().to_string(),
                        },
                    ));
                }
                let settle = entries.iter().find(|row| {
                    row.kind == CreditKind::Settle && row.reference.as_ref() == Some(reference)
                });
                if entry.kind == CreditKind::Settle {
                    if settle.is_some() {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::AlreadySettled {
                                reference: reference.as_str().to_string(),
                            },
                        ));
                    }
                    let delta = entry.amount_micro.saturating_sub(target.amount_micro);
                    // The pending hold already reserves the target amount;
                    // only the delta above it must be covered.
                    let available = balance.balance_micro().saturating_add(target.amount_micro);
                    if delta > available {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::InsufficientCredits {
                                available,
                                requested: delta,
                            },
                        ));
                    }
                } else {
                    let spent = settle
                        .map(|s| s.amount_micro)
                        .unwrap_or(target.amount_micro);
                    let refunded: u64 = entries
                        .iter()
                        .filter(|row| {
                            row.kind == CreditKind::Refund
                                && row.reference.as_ref() == Some(reference)
                        })
                        .map(|row| row.amount_micro)
                        .sum();
                    let refundable = spent.saturating_sub(refunded);
                    if entry.amount_micro > refundable {
                        return Err(BillingStoreError::Credit(
                            CreditAppendRefusal::RefundExceedsConsumed {
                                reference: reference.as_str().to_string(),
                                refundable,
                                requested: entry.amount_micro,
                            },
                        ));
                    }
                }
            }
        }
        let seq: i64 = tx
            .query_row(
                "INSERT INTO billing_credit_seq (organization_id, next_seq) VALUES (?1, 1)
                 ON CONFLICT(organization_id) DO UPDATE SET next_seq = next_seq + 1
                 RETURNING next_seq",
                params![entry.organization.as_str()],
                |r| r.get(0),
            )
            .map_err(backend)?;
        tx.execute(
            "INSERT INTO credit_entry
                (id, organization_id, entry_seq, billing_account_id, kind, reference,
                 usage_event_id, idempotency_key, amount_micro, occurred_at_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                entry.id.as_str(),
                entry.organization.as_str(),
                seq,
                entry.billing_account_id.as_str(),
                entry.kind.as_str(),
                entry.reference.as_ref().map(|r| r.as_str()),
                entry.usage_event_id.as_ref().map(|u| u.as_str()),
                entry.idempotency_key.as_deref(),
                entry.amount_micro.min(i64::MAX as u64) as i64,
                entry.occurred_at_ms,
                encode(entry)?,
            ],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(CreditAppend::Appended)
    }
}

fn read_credit_entries(
    conn: &Connection,
    organization: &OrganizationId,
) -> Result<Vec<CreditEntry>, BillingStoreError> {
    let mut stmt = conn
        .prepare("SELECT payload FROM credit_entry WHERE organization_id = ?1 ORDER BY entry_seq")
        .map_err(backend)?;
    let rows = stmt
        .query_map(params![organization.as_str()], |r| r.get::<_, String>(0))
        .map_err(backend)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(backend)?;
    let mut out = Vec::with_capacity(rows.len());
    for payload in rows {
        out.push(parse(&payload)?);
    }
    Ok(out)
}

impl BillingStore for SqliteControlPlaneStore {
    fn put_billing_account(&self, account: &BillingAccount) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM billing_account WHERE organization_id = ?1 AND id = ?2",
                params![account.organization.as_str(), account.id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        match existing {
            Some(_) => {
                conn.execute(
                    "UPDATE billing_account SET payload = ?1 WHERE id = ?2",
                    params![encode(account)?, account.id.as_str()],
                )
                .map_err(backend)?;
            }
            None => {
                conn.execute(
                    "INSERT INTO billing_account (id, organization_id, payload) VALUES (?1, ?2, ?3)",
                    params![account.id.as_str(), account.organization.as_str(), encode(account)?],
                )
                .map_err(backend)?;
            }
        }
        Ok(())
    }

    fn billing_account(
        &self,
        organization: &OrganizationId,
        id: &BillingAccountId,
    ) -> Result<Option<BillingAccount>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM billing_account WHERE organization_id = ?1 AND id = ?2",
                params![organization.as_str(), id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn billing_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BillingAccount>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM billing_account
                 WHERE organization_id = ?1 AND (?2 IS NULL OR id > ?2)
                 ORDER BY id LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str(), after, limit as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn put_subscription(&self, subscription: &Subscription) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        conn.execute(
            "INSERT INTO billing_subscription (id, organization_id, payload) VALUES (?1, ?2, ?3)
             ON CONFLICT(organization_id) DO UPDATE SET
                id = excluded.id, payload = excluded.payload",
            params![
                subscription.id.as_str(),
                subscription.organization.as_str(),
                encode(subscription)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn subscription(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Subscription>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM billing_subscription WHERE organization_id = ?1",
                params![organization.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn append_usage_event(&self, event: &UsageEvent) -> Result<UsageAppend, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let duplicate: Option<String> = tx
            .query_row(
                "SELECT id FROM usage_event WHERE organization_id = ?1 AND source_key = ?2",
                params![event.organization_id.as_str(), event.source_key],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if duplicate.is_some() {
            tx.commit().map_err(backend)?;
            return Ok(UsageAppend::Duplicate);
        }
        let seq: i64 = tx
            .query_row(
                "INSERT INTO usage_event_seq (organization_id, next_seq) VALUES (?1, 1)
                 ON CONFLICT(organization_id) DO UPDATE SET next_seq = next_seq + 1
                 RETURNING next_seq",
                params![event.organization_id.as_str()],
                |r| r.get(0),
            )
            .map_err(backend)?;
        tx.execute(
            "INSERT INTO usage_event
                (id, organization_id, event_seq, source_key, billing_account_id, task_id,
                 run_id, attempt_id, category, correction_of, occurred_at_ms,
                 reconciliation_state, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                event.id.as_str(),
                event.organization_id.as_str(),
                seq,
                event.source_key,
                event.billing_account_id.as_str(),
                event.task_id.min(i64::MAX as u64) as i64,
                event.run_id,
                event.attempt_id,
                event.category.as_str(),
                event.correction_of.as_ref().map(|c| c.as_str()),
                event.occurred_at_ms,
                event.reconciliation_state.as_str(),
                encode(event)?,
            ],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(UsageAppend::Appended)
    }

    fn usage_event(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<Option<StoredUsageEvent>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT event_seq, payload FROM usage_event
                 WHERE organization_id = ?1 AND id = ?2",
                params![organization.as_str(), id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(backend)?;
        match row {
            Some((event_seq, payload)) => Ok(Some(StoredUsageEvent {
                event_seq,
                event: parse(&payload)?,
            })),
            None => Ok(None),
        }
    }

    fn usage_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        self.usage_page(organization, None, after_seq, limit)
    }

    fn usage_events_of_task(
        &self,
        organization: &OrganizationId,
        task_id: u64,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        self.usage_page(organization, Some(task_id), after_seq, limit)
    }

    fn correction_exists(
        &self,
        organization: &OrganizationId,
        base: &str,
    ) -> Result<bool, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_event
                 WHERE organization_id = ?1 AND correction_of = ?2",
                params![organization.as_str(), base],
                |r| r.get(0),
            )
            .map_err(backend)?;
        Ok(count > 0)
    }

    fn append_credit_entry(&self, entry: &CreditEntry) -> Result<CreditAppend, BillingStoreError> {
        let mut conn = self.lock_billing_conn()?;
        Self::credit_append_tx(&mut conn, entry)
    }

    fn credit_entries(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredCreditEntry>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT entry_seq, payload FROM credit_entry
                 WHERE organization_id = ?1 AND entry_seq > ?2
                 ORDER BY entry_seq LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(
                params![organization.as_str(), after_seq, limit as i64],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        let mut out = Vec::with_capacity(rows.len());
        for (entry_seq, payload) in rows {
            out.push(StoredCreditEntry {
                entry_seq,
                entry: parse(&payload)?,
            });
        }
        Ok(out)
    }

    fn credit_entry_by_idempotency_key(
        &self,
        organization: &OrganizationId,
        key: &str,
    ) -> Result<Option<StoredCreditEntry>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let row = conn
            .query_row(
                "SELECT entry_seq, payload FROM credit_entry
                 WHERE organization_id = ?1 AND idempotency_key = ?2",
                params![organization.as_str(), key],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(backend)?;
        match row {
            Some((entry_seq, payload)) => Ok(Some(StoredCreditEntry {
                entry_seq,
                entry: parse(&payload)?,
            })),
            None => Ok(None),
        }
    }

    fn credit_balance(
        &self,
        organization: &OrganizationId,
    ) -> Result<CreditBalance, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        Ok(fold_credits(&read_credit_entries(&conn, organization)?))
    }

    fn begin_in_flight(&self, txn: &InFlightTxn) -> Result<(), BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        conn.execute(
            "INSERT INTO billing_in_flight (id, organization_id, kind, reference, started_ms, ended_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
            params![
                txn.id.as_str(),
                txn.organization.as_str(),
                txn.kind.as_str(),
                txn.reference,
                txn.started_ms,
                txn.ended_ms,
                encode(txn)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn end_in_flight(
        &self,
        organization: &OrganizationId,
        id: &InFlightTxnId,
        at_ms: i64,
    ) -> Result<bool, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let updated = conn
            .execute(
                "UPDATE billing_in_flight SET ended_ms = ?1
                 WHERE id = ?2 AND organization_id = ?3 AND ended_ms IS NULL",
                params![at_ms, id.as_str(), organization.as_str()],
            )
            .map_err(backend)?;
        Ok(updated == 1)
    }

    fn in_flight(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<InFlightTxn>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM billing_in_flight
                 WHERE organization_id = ?1 ORDER BY started_ms, id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str()], |r| r.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn put_report_period(&self, row: &ReportPeriodRow) -> Result<(), BillingStoreError> {
        let payload = encode(row)?;
        let mut conn = self.lock_billing_conn()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        tx.execute(
            "INSERT INTO billing_report_period
                (organization_id, period, status, next_attempt_at_ms, first_seen_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(organization_id, period) DO UPDATE SET
                status = excluded.status,
                next_attempt_at_ms = excluded.next_attempt_at_ms,
                first_seen_ms = excluded.first_seen_ms,
                payload = excluded.payload",
            params![
                row.organization_id,
                row.period,
                row.status.as_str(),
                row.next_attempt_at_ms,
                row.first_seen_ms,
                payload,
            ],
        )
        .map_err(backend)?;
        // Bounded schedule: prune the organization's oldest rows beyond the
        // bound (terminal rows first at the same age).
        tx.execute(
            "DELETE FROM billing_report_period WHERE organization_id = ?1 AND period NOT IN (
                 SELECT period FROM billing_report_period WHERE organization_id = ?1
                 ORDER BY (status = 'open') DESC, first_seen_ms DESC, period DESC
                 LIMIT ?2)",
            params![row.organization_id, MAX_REPORT_PERIODS_PER_ORG as i64],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(())
    }

    fn report_period(
        &self,
        organization: &OrganizationId,
        period: &str,
    ) -> Result<Option<ReportPeriodRow>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM billing_report_period
                 WHERE organization_id = ?1 AND period = ?2",
                params![organization.as_str(), period],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn report_periods(
        &self,
        organization: &OrganizationId,
        limit: usize,
    ) -> Result<Vec<ReportPeriodRow>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM billing_report_period
                 WHERE organization_id = ?1
                 ORDER BY first_seen_ms DESC, period DESC LIMIT ?2",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization.as_str(), limit as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }
}

impl SqliteControlPlaneStore {
    fn lock_billing_conn(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Connection>, BillingStoreError> {
        self.lock().map_err(BillingStoreError::from)
    }

    fn usage_page(
        &self,
        organization: &OrganizationId,
        task_id: Option<u64>,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<StoredUsageEvent>, BillingStoreError> {
        let conn = self.lock_billing_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT event_seq, payload FROM usage_event
                 WHERE organization_id = ?1
                   AND event_seq > ?2
                   AND (?3 IS NULL OR task_id = ?3)
                 ORDER BY event_seq LIMIT ?4",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(
                params![
                    organization.as_str(),
                    after_seq,
                    task_id.map(|t| t.min(i64::MAX as u64) as i64),
                    limit as i64
                ],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        let mut out = Vec::with_capacity(rows.len());
        for (event_seq, payload) in rows {
            out.push(StoredUsageEvent {
                event_seq,
                event: parse(&payload)?,
            });
        }
        Ok(out)
    }
}
